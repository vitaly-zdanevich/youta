//! Authenticated, bounded VK website requests with explicit cookie-file opt-in.
//!
//! Catalogue endpoints and response shapes were checked against an authenticated
//! browser and the same native HTTP stack. Every request stays on a fixed VK
//! origin; redirects are not followed and session cookies never reach media CDNs.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use url::Url;

use super::bootstrap;
use super::catalog::{self, AlbumReference, CursorKind};
use super::{
    ProfileReference, VkAlbum, VkAlbumPage, VkCookieHeader, VkCookieJar, VkPage, VkPageCursor,
    VkPlaybackUrl, VkTrack, decode_audio_url,
};
use crate::providers::ProviderError;

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CACHED_ALBUMS: usize = 256;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MOBILE_AUDIO: &str = "https://m.vk.ru/audio";
const DESKTOP_BOOTSTRAP: &str = "https://vk.ru/audio";
const DESKTOP_AUDIO: &str = "https://vk.ru/al_audio.php";
const DESKTOP_ARTIST: &str = "https://vk.ru/al_artist.php";
const USER_AGENT: &str = concat!(
    "youta/",
    env!("CARGO_PKG_VERSION"),
    " (+",
    env!("CARGO_PKG_REPOSITORY"),
    ")"
);
const ARTIST_USER_AGENT: &str = concat!(
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) ",
    "Chrome/140.0.0.0 Safari/537.36 youta/",
    env!("CARGO_PKG_VERSION"),
    " (+",
    env!("CARGO_PKG_REPOSITORY"),
    ")"
);

/// Native client for one explicitly imported VK web session.
///
/// Clones share immutable cookies and a private result scope. Authenticate a
/// client before cloning it, or move it into a worker and return it afterward.
/// Changing the cookie file requires a new client; old cursors and lookup hashes
/// cannot be used by that replacement. No automatic browser discovery occurs.
#[derive(Clone)]
pub struct VkClient {
    cookies: Arc<VkCookieJar>,
    transport: Arc<dyn Transport>,
    scope: Arc<()>,
    account: Option<u64>,
    /// Playback uses only the route proven by this session's successful bootstrap.
    session_route: SessionRoute,
    page_size: usize,
    album_titles: HashMap<AlbumReference, Option<String>>,
}

/// A private, cloned session choice; it never changes the scope of selected tracks.
#[derive(Clone, Copy)]
enum SessionRoute {
    Mobile,
    Desktop,
}

/// Keeps authentication failures distinguishable without matching public error messages.
enum RequestOutcome {
    Body(String),
    AuthenticationRequired,
}

impl fmt::Debug for VkClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("VkClient([REDACTED])")
    }
}

impl VkClient {
    /// Imports one user-selected Netscape cookie file without persisting its data.
    ///
    /// # Errors
    /// Returns a sanitized import error for invalid, oversized or unreadable files.
    pub fn from_cookie_file(path: &Path) -> Result<Self, ProviderError> {
        VkCookieJar::from_file(path).map(Self::from_cookies)
    }

    /// Creates a client from already-imported cookies, without network activity.
    #[must_use]
    pub fn from_cookies(cookies: VkCookieJar) -> Self {
        Self::with_transport(cookies, Arc::new(HttpTransport::new()))
    }

    fn with_transport(cookies: VkCookieJar, transport: Arc<dyn Transport>) -> Self {
        Self {
            cookies: Arc::new(cookies),
            transport,
            scope: Arc::new(()),
            account: None,
            session_route: SessionRoute::Mobile,
            page_size: 20,
            album_titles: HashMap::new(),
        }
    }

    /// Requests one to one hundred rows, without fetching additional pages.
    ///
    /// This is advisory: VK currently ignores `count` for some catalogue routes.
    /// Returned surplus rows are retained, never discarded to fit a viewport.
    pub fn set_page_size(&mut self, page_size: usize) {
        self.page_size = page_size.clamp(1, 100);
    }

    /// Reports whether this client has completed the account bootstrap.
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.account.is_some()
    }

    /// Confirms the imported session and reads only its numeric account identity.
    ///
    /// Cached success avoids repeated bootstrap requests. The webpage is parsed
    /// as data, not executed, and all other account configuration is discarded.
    /// If mobile authentication is unavailable, one fixed desktop page is checked;
    /// redirects are never followed and transport/server failures are not retried.
    ///
    /// # Errors
    /// Returns a sanitized authentication, HTTP, timeout or schema error.
    pub fn authenticate(&mut self) -> Result<(), ProviderError> {
        if self.account.is_some() {
            return Ok(());
        }
        for (address, route) in [
            (MOBILE_AUDIO, SessionRoute::Mobile),
            (DESKTOP_BOOTSTRAP, SessionRoute::Desktop),
        ] {
            if let RequestOutcome::Body(html) = self.request_outcome(address, None)?
                && let Some(account) = bootstrap::account(&html)
            {
                self.account = Some(account);
                self.session_route = route;
                return Ok(());
            }
        }
        Err(authentication_required())
    }

    /// Searches the global music block, independent of page language or ordering.
    ///
    /// # Errors
    /// Rejects invalid queries, unauthenticated clients and unsupported responses.
    pub fn search(&self, query: &str) -> Result<VkPage, ProviderError> {
        catalog::search_page(&self.search_response(query)?, &self.scope)
    }

    /// Searches VK's advertised Albums section, independently from user-created playlists.
    ///
    /// The mixed search discovers a private section token; one fixed catalogue
    /// request retrieves releases. No JavaScript or returned navigation URL is executed.
    ///
    /// # Errors
    /// Rejects invalid queries, authentication failures and ambiguous catalogue metadata.
    pub fn search_albums(&self, query: &str) -> Result<VkAlbumPage, ProviderError> {
        let response = self.search_response(query)?;
        let Some(section) = catalog::album_search_section(&response)? else {
            return Ok(VkAlbumPage {
                albums: Vec::new(),
                next: None,
            });
        };
        self.album_search_page(&section, None)
    }

    /// Loads one album-search continuation from the same authenticated client session.
    ///
    /// # Errors
    /// Rejects stale or non-album cursors before HTTP, and nonadvancing provider responses.
    pub fn next_album_search_page(
        &self,
        cursor: &VkPageCursor,
    ) -> Result<VkAlbumPage, ProviderError> {
        self.account()?;
        cursor.validate_scope(&self.scope)?;
        let CursorKind::SearchAlbums { section, offset } = &cursor.kind else {
            return Err(catalog::invalid_catalogue());
        };
        self.album_search_page(section, Some(offset))
    }

    /// Opens the exact selected release using only its private session-scoped reference.
    ///
    /// # Errors
    /// Rejects another client's result before HTTP, and mismatched release responses.
    pub fn album_tracks_from_result(&self, album: &VkAlbum) -> Result<VkPage, ProviderError> {
        self.account()?;
        let reference = album.reference(&self.scope)?;
        catalog::album_page(
            &self.album_response(reference, 0)?,
            reference,
            0,
            &self.scope,
        )
    }

    /// Keeps the existing track-search request identical while sharing section discovery.
    fn search_response(&self, query: &str) -> Result<Value, ProviderError> {
        let account = self.account()?;
        if query.trim().is_empty() || query.len() > 2 * 1024 || query.chars().any(char::is_control)
        {
            return Err(ProviderError::InvalidRequest(
                "VK search must be nonempty text within the 2 KiB limit".to_owned(),
            ));
        }
        let response = self.post(
            DESKTOP_AUDIO,
            &[
                ("al", "1".to_owned()),
                ("act", "section".to_owned()),
                ("claim", "0".to_owned()),
                ("is_layer", "0".to_owned()),
                ("owner_id", account.to_string()),
                ("section", "search".to_owned()),
                ("q", query.trim().to_owned()),
                ("count", self.page_size.to_string()),
            ],
        )?;
        Ok(response)
    }

    /// Follows only an already-validated opaque section and continuation on the fixed origin.
    fn album_search_page(
        &self,
        section: &str,
        previous: Option<&str>,
    ) -> Result<VkAlbumPage, ProviderError> {
        let response = self.post(
            DESKTOP_AUDIO,
            &[
                ("al", "1".to_owned()),
                ("act", "load_catalog_section".to_owned()),
                ("section_id", section.to_owned()),
                ("start_from", previous.unwrap_or_default().to_owned()),
            ],
        )?;
        catalog::album_search_page(&response, section, previous, &self.scope)
    }

    /// Lists music visible to the authenticated account on one exact user profile.
    ///
    /// Handles are resolved through the page's exact-name prefetch metadata, not
    /// through a broad search. Only user profiles are admitted, not group aliases.
    ///
    /// # Errors
    /// Returns authentication, unavailable-profile, network or schema errors.
    pub fn profile_tracks(&self, reference: &ProfileReference) -> Result<VkPage, ProviderError> {
        self.account()?;
        let owner = if let Some(id) = reference.user_id() {
            id
        } else {
            let handle = reference.handle().ok_or_else(catalog::invalid_catalogue)?;
            let mut url = reference.webpage_url();
            url.set_host(Some("m.vk.ru"))
                .map_err(|_| catalog::invalid_catalogue())?;
            let html = self.get(url.as_str())?;
            profile_owner(&html, handle).ok_or_else(|| {
                ProviderError::InvalidResponse("VK could not resolve that user profile".to_owned())
            })?
        };
        self.profile_page(owner, 0)
    }

    /// Loads one continuation from the same client, without automatic page crawling.
    ///
    /// # Errors
    /// Rejects another session's cursor and nonadvancing or malformed responses.
    pub fn next_page(&self, cursor: &VkPageCursor) -> Result<VkPage, ProviderError> {
        self.account()?;
        cursor.validate_scope(&self.scope)?;
        match &cursor.kind {
            CursorKind::Search {
                owner,
                playlist,
                offset,
            } => {
                let response = self.post(
                    DESKTOP_AUDIO,
                    &[
                        ("al", "1".to_owned()),
                        ("act", "load_section".to_owned()),
                        ("owner_id", owner.to_string()),
                        ("playlist_id", playlist.clone()),
                        ("offset", offset.clone()),
                        ("type", "search".to_owned()),
                        ("count", self.page_size.to_string()),
                    ],
                )?;
                catalog::next_search_page(&response, cursor, &self.scope)
            }
            CursorKind::Profile { owner, offset } => self.profile_page(*owner, *offset),
            CursorKind::Album { album, offset } => catalog::album_page(
                &self.album_response(album, *offset)?,
                album,
                *offset,
                &self.scope,
            ),
            CursorKind::Artist { section, offset } => {
                let response = self.post(
                    DESKTOP_AUDIO,
                    &[
                        ("al", "1".to_owned()),
                        ("act", "load_catalog_section".to_owned()),
                        ("section_id", section.clone()),
                        ("start_from", offset.clone()),
                    ],
                )?;
                catalog::next_artist_page(&response, cursor, &self.scope)
            }
            CursorKind::SearchAlbums { .. } => Err(catalog::invalid_catalogue()),
        }
    }

    /// Resolves only the selected track to a transient, vetted audio CDN locator.
    ///
    /// The returned signed URL must stay out of persisted track metadata. Private
    /// lookup keys belong to this client session and are never sent to the CDN.
    ///
    /// # Errors
    /// Rejects stale-session rows, unavailable tracks, unexpected identities and
    /// unsupported URL encoding without printing hashes or signed URLs.
    pub fn resolve(&self, track: &VkTrack) -> Result<VkPlaybackUrl, ProviderError> {
        let account = self.account()?;
        let mut fields = vec![
            ("act", "reload_audio".to_owned()),
            ("ids", track.reload_key(&self.scope)?),
        ];
        let address = match self.session_route {
            SessionRoute::Mobile => MOBILE_AUDIO,
            SessionRoute::Desktop => {
                fields.push(("al", "1".to_owned()));
                DESKTOP_AUDIO
            }
        };
        let response = self.post(address, &fields)?;
        let raw = match self.session_route {
            SessionRoute::Mobile => catalog::reloaded_url(&response, track.id)?,
            SessionRoute::Desktop => catalog::reloaded_desktop_url(&response, track.id)?,
        };
        decode_audio_url(raw, account)
    }

    /// Reads only the selected track's release title and caches successful lookups.
    ///
    /// The bounded cache exists only in memory and includes the private access hash
    /// in its key. Missing or unusable titles are cached as absent; failed requests
    /// remain retryable. Search and catalogue parsing perform no album requests.
    ///
    /// # Errors
    /// Rejects another client's row, unauthenticated sessions, failed requests and
    /// responses whose album identity does not match the requested release.
    pub fn album_title(&mut self, track: &VkTrack) -> Result<Option<String>, ProviderError> {
        self.account()?;
        let Some(album) = track.album_reference(&self.scope)? else {
            return Ok(None);
        };
        if let Some(title) = self.album_titles.get(album) {
            return Ok(title.clone());
        }
        let response = self.album_response(album, 0)?;
        let title = catalog::album_title(&response, album)?;
        if self.album_titles.len() >= MAX_CACHED_ALBUMS {
            self.album_titles.clear();
        }
        self.album_titles.insert(album.clone(), title.clone());
        Ok(title)
    }

    /// Browses the selected track's exact release using its private session reference.
    ///
    /// # Errors
    /// Rejects missing albums, rows from another session, unavailable releases and
    /// malformed or mismatched responses. Continuations use `next_page`.
    pub fn album_tracks(&self, track: &VkTrack) -> Result<VkPage, ProviderError> {
        self.account()?;
        let album = track.album_reference(&self.scope)?.ok_or_else(|| {
            ProviderError::InvalidRequest("VK did not provide an album for this track".to_owned())
        })?;
        catalog::album_page(&self.album_response(album, 0)?, album, 0, &self.scope)
    }

    /// Browses the first identified main artist's own top-tracks catalogue.
    ///
    /// The desktop route may canonicalize a numeric artist to a public slug.
    /// At most three artist-page requests and one catalogue request are admitted.
    /// Page scripts are parsed only for literal configuration, never evaluated.
    ///
    /// # Errors
    /// Rejects stale-session tracks, absent artist identity, unavailable pages and
    /// ambiguous initialization data. Artist display names are never searched.
    pub fn artist_tracks(&self, track: &VkTrack) -> Result<VkPage, ProviderError> {
        self.account()?;
        let mut artist = track
            .artist_reference(&self.scope)?
            .ok_or_else(|| {
                ProviderError::InvalidRequest(
                    "VK did not provide an artist page for this track".to_owned(),
                )
            })?
            .to_owned();
        let mut visited = HashSet::new();
        for _ in 0..3 {
            if !visited.insert(artist.clone()) {
                return Err(catalog::invalid_catalogue());
            }
            let path = format!("artist/{artist}/top_audios");
            let response = self.post(
                DESKTOP_ARTIST,
                &[
                    ("al", "1".to_owned()),
                    ("__query", path.clone()),
                    ("_ref", format!("/{path}")),
                ],
            )?;
            if let Some(canonical) = artist_redirect(&response)? {
                artist = canonical;
                continue;
            }
            let section = catalog::artist_section(&response)?;
            let response = self.post(
                DESKTOP_AUDIO,
                &[
                    ("al", "1".to_owned()),
                    ("act", "load_catalog_section".to_owned()),
                    ("section_id", section.clone()),
                ],
            )?;
            return catalog::artist_catalogue_page(&response, &section, None, &self.scope);
        }
        Err(catalog::invalid_catalogue())
    }

    /// Keeps release secrets confined to the authenticated desktop catalogue route.
    fn album_response(&self, album: &AlbumReference, offset: u64) -> Result<Value, ProviderError> {
        self.post(
            DESKTOP_AUDIO,
            &[
                ("al", "1".to_owned()),
                ("act", "load_section".to_owned()),
                ("claim", "0".to_owned()),
                ("owner_id", album.owner.to_string()),
                ("playlist_id", album.id.to_string()),
                ("access_hash", album.access_hash.clone()),
                ("offset", offset.to_string()),
                ("type", "playlist".to_owned()),
                ("is_loading_all", "0".to_owned()),
                ("is_preload", "0".to_owned()),
            ],
        )
    }

    fn profile_page(&self, owner: u64, offset: u64) -> Result<VkPage, ProviderError> {
        let mut fields = vec![
            ("act", "load_section".to_owned()),
            ("owner_id", owner.to_string()),
            ("playlist_id", "-1".to_owned()),
            ("offset", offset.to_string()),
            ("type", "playlist".to_owned()),
            ("is_loading_all", "0".to_owned()),
            ("count", self.page_size.to_string()),
        ];
        match self.session_route {
            SessionRoute::Mobile => {
                let response = self.post(MOBILE_AUDIO, &fields)?;
                catalog::profile_page(&response, owner, offset, &self.scope)
            }
            SessionRoute::Desktop => {
                fields.push(("al", "1".to_owned()));
                let response = self.post(DESKTOP_AUDIO, &fields)?;
                catalog::desktop_profile_page(&response, owner, offset, &self.scope)
            }
        }
    }

    fn account(&self) -> Result<u64, ProviderError> {
        self.account.ok_or_else(authentication_required)
    }

    fn get(&self, address: &str) -> Result<String, ProviderError> {
        self.request(address, None)
    }

    fn post(&self, address: &str, fields: &[(&str, String)]) -> Result<Value, ProviderError> {
        let body = self.request(address, Some(fields))?;
        let body = body.trim_start();
        serde_json::from_str(body.strip_prefix("<!--").unwrap_or(body))
            .map_err(|_| catalog::invalid_catalogue())
    }

    fn request(
        &self,
        address: &str,
        fields: Option<&[(&str, String)]>,
    ) -> Result<String, ProviderError> {
        match self.request_outcome(address, fields)? {
            RequestOutcome::Body(body) => Ok(body),
            RequestOutcome::AuthenticationRequired => Err(authentication_required()),
        }
    }

    /// Preserves a typed authentication outcome only for the bounded bootstrap decision.
    fn request_outcome(
        &self,
        address: &str,
        fields: Option<&[(&str, String)]>,
    ) -> Result<RequestOutcome, ProviderError> {
        let url = Url::parse(address).map_err(|_| catalog::invalid_catalogue())?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| {
                ProviderError::Transport("VK request requires a valid system clock".to_owned())
            })?
            .as_secs();
        let cookie = self
            .cookies
            .request_header(&url, now)?
            .ok_or_else(authentication_required)?;
        let response = self.transport.request(&url, &cookie, fields)?;
        if response.login_redirect || matches!(response.status, 401 | 403) {
            return Ok(RequestOutcome::AuthenticationRequired);
        }
        if response.status != 200 {
            let mut instance = url;
            instance.set_path("/");
            instance.set_query(None);
            instance.set_fragment(None);
            return Err(ProviderError::HttpStatusAt {
                provider: "VK",
                instance,
                status: response.status,
            });
        }
        decode_body(response).map(RequestOutcome::Body)
    }
}

/// Private injectable transport prevents fixtures from contacting real accounts.
trait Transport: Send + Sync {
    fn request(
        &self,
        url: &Url,
        cookie: &VkCookieHeader,
        fields: Option<&[(&str, String)]>,
    ) -> Result<HttpResponse, ProviderError>;
}

struct HttpResponse {
    status: u16,
    /// A recognized login redirect, without retaining its potentially private URL.
    login_redirect: bool,
    content_type: Option<String>,
    body: Vec<u8>,
}

struct HttpTransport {
    agent: ureq::Agent,
}

impl HttpTransport {
    fn new() -> Self {
        Self {
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(REQUEST_TIMEOUT))
                .max_redirects(0)
                .http_status_as_error(false)
                .user_agent(USER_AGENT)
                .build()
                .into(),
        }
    }
}

impl Transport for HttpTransport {
    fn request(
        &self,
        url: &Url,
        cookie: &VkCookieHeader,
        fields: Option<&[(&str, String)]>,
    ) -> Result<HttpResponse, ProviderError> {
        let response = if let Some(fields) = fields {
            self.agent
                .post(url.as_str())
                .header("User-Agent", request_user_agent(url, false))
                .header("Cookie", cookie.as_str())
                .header(
                    "Referer",
                    if url.host_str() == Some("m.vk.ru") {
                        MOBILE_AUDIO
                    } else {
                        "https://vk.ru/audio"
                    },
                )
                .header("Accept", "application/json, text/javascript, */*; q=0.01")
                .header("X-Requested-With", "XMLHttpRequest")
                .send_form(fields.iter().map(|(key, value)| (*key, value.as_str())))
        } else {
            self.agent
                .get(url.as_str())
                .header("User-Agent", request_user_agent(url, true))
                .header("Cookie", cookie.as_str())
                .header(
                    "Accept",
                    "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
                )
                .call()
        };
        let mut response = response
            .map_err(|_| ProviderError::Transport("VK request failed or timed out".to_owned()))?;
        let status = response.status().as_u16();
        let login_redirect = is_login_redirect(
            status,
            response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok()),
        );
        if login_redirect || matches!(status, 401 | 403) {
            return Ok(HttpResponse {
                status,
                login_redirect,
                content_type: None,
                body: Vec::new(),
            });
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = response
            .body_mut()
            .with_config()
            .limit((MAX_RESPONSE_BYTES + 1) as u64)
            .read_to_vec()
            .map_err(|_| {
                ProviderError::Transport(
                    "VK response could not be read within its limit".to_owned(),
                )
            })?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(ProviderError::ResponseTooLarge {
                limit: MAX_RESPONSE_BYTES,
            });
        }
        Ok(HttpResponse {
            status,
            login_redirect,
            content_type,
            body,
        })
    }
}

/// Classifies only explicit VK login redirects, never retaining or following their URLs.
fn is_login_redirect(status: u16, location: Option<&str>) -> bool {
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return false;
    }
    let Some(location) =
        location.filter(|value| value.len() <= 8192 && !value.chars().any(char::is_control))
    else {
        return false;
    };
    let Ok(url) = Url::parse(location) else {
        return false;
    };
    url.scheme() == "https"
        && matches!(url.host_str(), Some("login.vk.ru" | "login.vk.com"))
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && url.port_or_known_default() == Some(443)
}

/// Selects the desktop shell only for artist requests and the exact bootstrap GET.
fn request_user_agent(url: &Url, is_get: bool) -> &'static str {
    if url.scheme() == "https"
        && url.host_str() == Some("vk.ru")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && (url.path() == "/al_artist.php" || is_get && url.path() == "/audio")
    {
        ARTIST_USER_AGENT
    } else {
        USER_AGENT
    }
}

/// Reads only an exact public artist route from VK's nested JSON-string redirect.
///
/// The returned identity is used to construct another fixed endpoint request;
/// the redirect URL itself is never followed, and queries or other routes fail closed.
fn artist_redirect(value: &Value) -> Result<Option<String>, ProviderError> {
    let redirect = value
        .pointer("/payload/1/0")
        .and_then(Value::as_str)
        .filter(|value| value.len() <= 2 * 1024)
        .ok_or_else(catalog::invalid_catalogue)?;
    if redirect.is_empty() {
        return Ok(None);
    }
    let destination =
        serde_json::from_str::<String>(redirect).map_err(|_| catalog::invalid_catalogue())?;
    let path = destination
        .strip_prefix("https://vk.ru")
        .unwrap_or(&destination);
    let artist = path
        .strip_prefix("/artist/")
        .and_then(|path| path.strip_suffix("/top_audios"))
        .filter(|artist| {
            !artist.is_empty()
                && artist.len() <= 128
                && artist
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
        .ok_or_else(catalog::invalid_catalogue)?;
    Ok(Some(artist.to_owned()))
}

/// Honors the deployed legacy charset without exposing body text in decoder errors.
fn decode_body(response: HttpResponse) -> Result<String, ProviderError> {
    if response.body.len() > MAX_RESPONSE_BYTES {
        return Err(ProviderError::ResponseTooLarge {
            limit: MAX_RESPONSE_BYTES,
        });
    }
    if response
        .content_type
        .as_deref()
        .is_some_and(|value| value.to_ascii_lowercase().contains("windows-1251"))
    {
        let (text, had_errors) =
            encoding_rs::WINDOWS_1251.decode_without_bom_handling(&response.body);
        if had_errors {
            return Err(catalog::invalid_catalogue());
        }
        Ok(text.into_owned())
    } else {
        String::from_utf8(response.body).map_err(|_| catalog::invalid_catalogue())
    }
}

/// Extracts only a literal JSON value after the named assignment; never evaluates JS.
fn assigned_json(html: &str, assignment: &str) -> Option<Value> {
    if html.len() > MAX_RESPONSE_BYTES {
        return None;
    }
    for (position, _) in html.match_indices(assignment) {
        let after = html.get(position + assignment.len()..)?.trim_start();
        let Some(after) = after.strip_prefix('=') else {
            continue;
        };
        let after = after.trim_start();
        if !after.starts_with(['{', '[']) {
            continue;
        }
        if let Some(Ok(value)) = serde_json::Deserializer::from_str(after)
            .into_iter::<Value>()
            .next()
        {
            return Some(value);
        }
    }
    None
}

fn profile_owner(html: &str, handle: &str) -> Option<u64> {
    let cache = assigned_json(html, "window.cur.apiPrefetchCache")?;
    let entries = cache.as_array().filter(|entries| entries.len() <= 128)?;
    let mut matches = entries
        .iter()
        .filter_map(|value| profile_owner_record(value, handle));
    let owner = matches.next()?;
    matches.next().is_none().then_some(owner)
}

/// Uses only an exact user-profile resolution record from the bounded cache.
fn profile_owner_record(value: &Value, handle: &str) -> Option<u64> {
    if value.get("method")?.as_str()? != "utils.resolveScreenName"
        || value.pointer("/request/screen_name")?.as_str()? != handle
        || value.pointer("/response/type")?.as_str()? != "user"
    {
        return None;
    }
    value
        .pointer("/response/object_id")?
        .as_u64()
        .filter(|id| *id > 0 && i64::try_from(*id).is_ok())
}

fn authentication_required() -> ProviderError {
    ProviderError::InvalidRequest(
        "VK could not confirm a signed-in session; sign in at vk.ru, complete any security check, export cookies again, and restart Youta".to_owned(),
    )
}

#[cfg(test)]
mod tests {
    /// Album search follows only the website-advertised section and exact scoped release.
    #[test]
    fn album_search_pages_and_open_use_fixed_routes_and_private_scoped_tokens() {
        let (mut client, transport) = client(vec![
            html("<script>window.vk={id:123};</script>"),
            json_response(&catalog::tests::album_search_fixture()),
            json_response(&catalog::tests::album_catalogue_fixture("private-next")),
            json_response(&catalog::tests::album_catalogue_fixture("")),
            json_response(
                &json!({"payload":[0,[{"ownerId":-123,"id":456,"title":"Album","list":[catalog::tests::row(2)],"hasMore":false}]]}),
            ),
        ]);
        client.authenticate().unwrap();
        let page = client.search_albums("minsk").unwrap();
        let next = client
            .next_album_search_page(page.next.as_ref().unwrap())
            .unwrap();
        assert_eq!(next.albums.len(), 1);
        assert!(next.next.is_none());
        assert_eq!(
            client
                .album_tracks_from_result(&page.albums[0])
                .unwrap()
                .tracks
                .len(),
            1
        );
        assert!(client.next_page(page.next.as_ref().unwrap()).is_err());
        assert!(!format!("{page:?} {client:?}").contains("private-"));
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 5);
        assert!(
            requests
                .iter()
                .skip(1)
                .all(|request| request.0 == DESKTOP_AUDIO)
        );
        assert!(
            requests[2]
                .1
                .contains(&("act".into(), "load_catalog_section".into()))
        );
        assert!(
            requests[2]
                .1
                .contains(&("section_id".into(), "private-album-section".into()))
        );
        assert!(requests[2].1.contains(&("start_from".into(), "".into())));
        assert!(
            requests[3]
                .1
                .contains(&("start_from".into(), "private-next".into()))
        );
        assert!(
            requests[4]
                .1
                .contains(&("access_hash".into(), "private-album-hash".into()))
        );
    }

    #[test]
    fn album_search_rejects_foreign_objects_and_invalid_queries_before_http() {
        let (mut first, _) = client(vec![
            html("<script>window.vk={id:123};</script>"),
            json_response(&catalog::tests::album_search_fixture()),
            json_response(&catalog::tests::album_catalogue_fixture("private-next")),
        ]);
        first.authenticate().unwrap();
        let page = first.search_albums("minsk").unwrap();
        let (mut other, transport) = client(vec![html("<script>window.vk={id:123};</script>")]);
        other.authenticate().unwrap();
        assert!(other.album_tracks_from_result(&page.albums[0]).is_err());
        assert!(
            other
                .next_album_search_page(page.next.as_ref().unwrap())
                .is_err()
        );
        for query in ["", " ", "private\nquery"] {
            assert!(other.search_albums(query).is_err());
        }
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn absent_album_search_header_is_empty_without_catalogue_request() {
        let (mut client, transport) = client(vec![
            html("<script>window.vk={id:123};</script>"),
            json_response(&json!({"payload":[0,["<div>No album results</div>",{}]]})),
        ]);
        client.authenticate().unwrap();
        let page = client.search_albums("minsk").unwrap();
        assert!(page.albums.is_empty() && page.next.is_none());
        assert_eq!(transport.requests.lock().unwrap().len(), 2);
    }
    use super::*;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Captured synthetic requests contain no real credentials or account data.
    type RecordedRequest = (String, Vec<(String, String)>);

    struct MockTransport {
        responses: Mutex<VecDeque<Result<HttpResponse, ProviderError>>>,
        requests: Mutex<Vec<RecordedRequest>>,
        cookie_headers: HashMap<String, &'static str>,
    }

    impl MockTransport {
        fn new(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().map(Ok).collect()),
                requests: Mutex::new(Vec::new()),
                cookie_headers: HashMap::new(),
            }
        }
    }

    impl Transport for MockTransport {
        fn request(
            &self,
            url: &Url,
            cookie: &VkCookieHeader,
            fields: Option<&[(&str, String)]>,
        ) -> Result<HttpResponse, ProviderError> {
            assert_eq!(
                cookie.as_str(),
                self.cookie_headers
                    .get(url.as_str())
                    .copied()
                    .unwrap_or("sid=fixture")
            );
            self.requests.lock().unwrap().push((
                url.to_string(),
                fields
                    .unwrap_or_default()
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), value.clone()))
                    .collect(),
            ));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("mock response")
        }
    }

    fn html(text: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            login_redirect: false,
            content_type: Some("text/html; charset=utf-8".to_owned()),
            body: text.as_bytes().to_vec(),
        }
    }
    fn json_response(value: &Value) -> HttpResponse {
        html(&value.to_string())
    }
    fn client(responses: Vec<HttpResponse>) -> (VkClient, Arc<MockTransport>) {
        let transport = Arc::new(MockTransport::new(responses));
        let jar = VkCookieJar::parse(b".vk.ru\tTRUE\t/\tTRUE\t0\tsid\tfixture\n").unwrap();
        (VkClient::with_transport(jar, transport.clone()), transport)
    }

    /// A desktop-only web session must authenticate once and resolve through the same route.
    #[test]
    fn desktop_fallback_authenticates_searches_and_resolves_from_a_cached_clone() {
        let mut loaded = catalog::tests::row(2);
        loaded[2] = json!("https://psv4.vkuseraudio.ru/fixture/index.m3u8?token=private-fixture");
        let (mut client, transport) = client(vec![
            HttpResponse {
                status: 302,
                login_redirect: true,
                content_type: None,
                body: vec![],
            },
            html(
                "<html><script>window.vk = {language: 0, id: 123, extra: {id: 456}};</script></html>",
            ),
            json_response(&catalog::tests::search_fixture()),
            json_response(&json!({"payload":[0,[[loaded]]]})),
        ]);
        client.authenticate().unwrap();
        client.authenticate().unwrap();
        let mut clone = client.clone();
        clone.authenticate().unwrap();
        let page = client.search("minsk").unwrap();
        let foreign = VkTrack::fixture(page.tracks[0].id, "Fixture", "Artist");
        assert!(clone.resolve(&foreign).is_err());
        assert_eq!(transport.requests.lock().unwrap().len(), 3);
        let locator = clone.resolve(&page.tracks[0]).unwrap();
        assert_eq!(locator.as_url().path(), "/fixture/index.m3u8");
        assert!(!format!("{client:?} {clone:?} {page:?} {locator:?}").contains("private-fixture"));
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].0, MOBILE_AUDIO);
        assert_eq!(requests[1], ("https://vk.ru/audio".to_owned(), vec![]));
        assert_eq!(requests[2].0, DESKTOP_AUDIO);
        assert_eq!(requests[3].0, DESKTOP_AUDIO);
        assert!(requests[3].1.contains(&("al".to_owned(), "1".to_owned())));
        assert!(
            requests[3]
                .1
                .contains(&("act".to_owned(), "reload_audio".to_owned()))
        );
    }

    /// Only known authentication failures or an unidentified successful page admit one fallback.
    #[test]
    fn desktop_fallback_is_bounded_and_requires_a_valid_account() {
        for status in [200, 301, 302, 303, 307, 308, 401, 403] {
            let (mut client, transport) = client(vec![
                HttpResponse {
                    status,
                    login_redirect: matches!(status, 301 | 302 | 303 | 307 | 308),
                    content_type: None,
                    body: b"<script>window.vk={id:0};</script>".to_vec(),
                },
                html("<script>window.vk={id:123};</script>"),
            ]);
            client.authenticate().unwrap();
            assert_eq!(transport.requests.lock().unwrap().len(), 2);
        }
        for body in [
            "<script>window.vk={id:0};</script>",
            "<script>window.vk={id:'123'};</script>",
            "<p>private logged-out fixture</p>",
            "<script>window.vk=execute();</script>",
        ] {
            let (mut client, transport) = client(vec![html("<p>signed out</p>"), html(body)]);
            let error = client.authenticate().unwrap_err();
            assert!(!client.is_authenticated());
            assert!(!error.to_string().contains("private"));
            assert_eq!(transport.requests.lock().unwrap().len(), 2);
        }
    }

    /// Transport/server/decoding/size failures are not retried on another origin.
    #[test]
    fn desktop_fallback_does_not_hide_non_authentication_failures() {
        for response in [
            Err(ProviderError::Transport(
                "fixture transport failure".to_owned(),
            )),
            Ok(HttpResponse {
                status: 500,
                login_redirect: false,
                content_type: None,
                body: vec![],
            }),
            Ok(HttpResponse {
                status: 302,
                login_redirect: false,
                content_type: None,
                body: vec![],
            }),
            Ok(HttpResponse {
                status: 200,
                login_redirect: false,
                content_type: None,
                body: vec![0xff],
            }),
            Ok(HttpResponse {
                status: 200,
                login_redirect: false,
                content_type: None,
                body: vec![b'x'; MAX_RESPONSE_BYTES + 1],
            }),
        ] {
            let (mut client, transport) = client(vec![]);
            transport.responses.lock().unwrap().push_back(response);
            assert!(client.authenticate().is_err());
            assert!(!client.is_authenticated());
            assert_eq!(transport.requests.lock().unwrap().len(), 1);
        }
    }

    /// Falling back never copies a mobile host-only cookie into a desktop request.
    #[test]
    fn desktop_fallback_preserves_host_only_cookie_isolation() {
        let mut transport = MockTransport::new(vec![
            html("<p>signed out</p>"),
            html("<script>window.vk={id:123};</script>"),
        ]);
        transport
            .cookie_headers
            .insert(MOBILE_AUDIO.to_owned(), "mobile=fixture-mobile");
        transport
            .cookie_headers
            .insert("https://vk.ru/audio".to_owned(), "desktop=fixture-desktop");
        let transport = Arc::new(transport);
        let cookies = VkCookieJar::parse(b"m.vk.ru\tFALSE\t/\tTRUE\t0\tmobile\tfixture-mobile\nvk.ru\tFALSE\t/\tTRUE\t0\tdesktop\tfixture-desktop\n").unwrap();
        let mut client = VkClient::with_transport(cookies, transport.clone());
        client.authenticate().unwrap();
        assert_eq!(transport.requests.lock().unwrap().len(), 2);
        assert!(!format!("{client:?}").contains("fixture-mobile"));
    }

    /// Numeric profiles keep their exact owner and desktop route across scoped pagination.
    #[test]
    fn desktop_profile_pages_use_the_authenticated_route_and_exact_owner() {
        let response = |id, more, next| {
            json_response(&json!({"payload":[0,[{
                "ownerId":456,"id":-1,"list":[catalog::tests::row(id)],"hasMore":more,"nextOffset":next
            }]]}))
        };
        let (mut client, transport) = client(vec![
            html("<p>signed out</p>"),
            html("<script>window.vk={id:123};</script>"),
            response(1, 1, 100),
            response(2, 0, 0),
        ]);
        client.authenticate().unwrap();
        let page = client
            .profile_tracks(&ProfileReference::parse("456").unwrap())
            .unwrap();
        let cursor = page.next.as_ref().unwrap();
        let (mut other, other_transport) = self::client(vec![]);
        other.account = Some(123);
        assert!(other.next_page(cursor).is_err());
        assert!(other_transport.requests.lock().unwrap().is_empty());
        let next = client.clone().next_page(cursor).unwrap();
        assert_eq!(next.tracks[0].id.audio_id(), 2);
        assert!(next.next.is_none());
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        for (request, offset) in requests[2..].iter().zip(["0", "100"]) {
            assert_eq!(request.0, DESKTOP_AUDIO);
            for (key, value) in [
                ("al", "1"),
                ("act", "load_section"),
                ("owner_id", "456"),
                ("playlist_id", "-1"),
                ("offset", offset),
                ("type", "playlist"),
                ("is_loading_all", "0"),
                ("count", "20"),
            ] {
                assert!(request.1.contains(&(key.to_owned(), value.to_owned())));
            }
        }
    }

    impl VkClient {
        /// Creates an authenticated, same-scope fixture that panics on any HTTP request.
        pub(crate) fn fixture_for_track(track: &VkTrack) -> Self {
            let (mut client, _) = client(Vec::new());
            client.scope = track.fixture_scope();
            client.account = Some(123);
            client
        }
    }

    /// Controller fixtures share lookup ownership without ever reaching a live transport.
    #[test]
    fn scoped_client_fixture_is_authenticated_and_rejects_unexpected_http() {
        let track = VkTrack::fixture(super::super::VkAudioId::new(1, 2).unwrap(), "T", "A");
        let client = VkClient::fixture_for_track(&track);
        assert!(client.is_authenticated());
        assert!(track.reload_key(&client.scope).is_ok());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| client.resolve(&track)))
                .is_err()
        );
    }

    /// Projects synthetic rows into the exact client scope without network traffic.
    fn album_track(client: &VkClient, track_id: u64, reference: Value) -> VkTrack {
        catalog::profile_page(
			&json!({"data":[{"list":[catalog::tests::album_row(track_id, reference)],"hasMore":false}]}),
			123,
			0,
			&client.scope,
		)
		.unwrap()
		.tracks
		.remove(0)
    }

    /// Builds the verified desktop load-section response shape using mock metadata.
    fn album_response(owner: i64, id: u64, title: Value) -> HttpResponse {
        json_response(&json!({"payload":[0,[{"ownerId":owner,"id":id,"title":title}]]}))
    }

    #[test]
    fn album_browsing_preserves_private_reference_only_on_fixed_vk_requests() {
        let response = |id, more, offset| {
            json_response(
                &json!({"payload":[0,[{"ownerId":-2000123,"id":456,"title":"Album","list":[catalog::tests::row(id)],"hasMore":more,"nextOffset":offset}]]}),
            )
        };
        let (mut client, transport) = client(vec![response(1, true, 100), response(2, false, 0)]);
        client.account = Some(123);
        let track = album_track(&client, 99, json!([-2000123, 456, "private-album-hash"]));
        let first = client.album_tracks(&track).unwrap();
        let next = client.next_page(first.next.as_ref().unwrap()).unwrap();
        assert_eq!(next.tracks[0].id.audio_id(), 2);
        assert!(next.next.is_none());
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert_eq!(request.0, DESKTOP_AUDIO);
            assert!(
                request
                    .1
                    .contains(&("access_hash".to_owned(), "private-album-hash".to_owned()))
            );
            assert!(
                request
                    .1
                    .contains(&("type".to_owned(), "playlist".to_owned()))
            );
        }
        assert!(
            requests[1]
                .1
                .contains(&("offset".to_owned(), "100".to_owned()))
        );
        let (mut other, other_transport) = self::client(vec![]);
        other.account = Some(123);
        assert!(other.album_tracks(&track).is_err());
        assert!(other.next_page(first.next.as_ref().unwrap()).is_err());
        assert!(other_transport.requests.lock().unwrap().is_empty());
        let no_album = album_track(&client, 99, Value::Null);
        assert!(client.album_tracks(&no_album).is_err());
    }

    #[test]
    fn artist_redirects_only_admit_canonical_same_origin_artist_routes() {
        let response = |destination: &str| json!({"payload":[0,[serde_json::to_string(destination).unwrap(),"0"]]});
        for destination in [
            "/artist/fixture_artist/top_audios",
            "https://vk.ru/artist/fixture_artist/top_audios",
        ] {
            assert_eq!(
                artist_redirect(&response(destination)).unwrap().as_deref(),
                Some("fixture_artist")
            );
        }
        assert!(
            artist_redirect(&json!({"payload":[0,["","<div></div>","",{}]]}))
                .unwrap()
                .is_none()
        );
        for destination in [
            "https://evil.test/artist/fixture_artist/top_audios",
            "https://vk.ru.evil.test/artist/fixture_artist/top_audios",
            "https://m.vk.ru/artist/fixture_artist/top_audios",
            "http://vk.ru/artist/fixture_artist/top_audios",
            "https://user:secret@vk.ru/artist/fixture_artist/top_audios",
            "https://vk.ru:444/artist/fixture_artist/top_audios",
            "/artist/fixture_artist/top_audios?access_hash=private",
            "/artist/fixture_artist/top_audios#private",
            "/artist/fixture_artist/top_audios/",
            "/artist/fixture_artist/albums",
            "/artist/../fixture_artist/top_audios",
            "/artist/bad%2Fidentity/top_audios",
            "//vk.ru/artist/fixture_artist/top_audios",
            "/audio-1_2",
        ] {
            assert!(
                artist_redirect(&response(destination)).is_err(),
                "{destination}"
            );
        }
        assert!(
            artist_redirect(&response(&format!(
                "/artist/{}/top_audios",
                "x".repeat(129)
            )))
            .is_err()
        );
        assert!(artist_redirect(&json!({"payload":[0,["evaluateThis()","0"]]})).is_err());
    }

    /// Only artist navigation and the fixed desktop bootstrap GET need the desktop shell.
    #[test]
    fn desktop_user_agent_is_confined_to_the_exact_artist_endpoint() {
        let artist = Url::parse("https://vk.ru/al_artist.php").unwrap();
        assert!(request_user_agent(&artist, false).contains("Mozilla/5.0"));
        assert!(request_user_agent(&artist, false).contains(USER_AGENT));
        assert!(
            request_user_agent(&Url::parse(DESKTOP_BOOTSTRAP).unwrap(), true)
                .contains("Mozilla/5.0")
        );
        assert_eq!(
            request_user_agent(&Url::parse(DESKTOP_BOOTSTRAP).unwrap(), false),
            USER_AGENT
        );
        assert_eq!(
            request_user_agent(&Url::parse(DESKTOP_AUDIO).unwrap(), false),
            USER_AGENT
        );
        for address in [
            MOBILE_AUDIO,
            DESKTOP_AUDIO,
            "https://vk.ru/artist/fixture_artist",
            "https://m.vk.ru/al_artist.php",
            "https://evil.test/al_artist.php",
            "https://vk.ru/al_artist.php/",
            "https://vk.ru/audio?private=fixture",
            "https://vk.ru/audio#fixture",
            "https://vk.ru/audio/",
            "https://vk.com/audio",
        ] {
            assert_eq!(
                request_user_agent(&Url::parse(address).unwrap(), true),
                USER_AGENT
            );
        }
    }

    #[test]
    fn artist_browsing_uses_exact_track_identity_and_scoped_catalogue_pagination() {
        let (mut client, transport) = client(vec![
            json_response(&json!({"payload":[0,["\"/artist/canonical_fixture/top_audios\"","0"]]})),
            json_response(&catalog::tests::artist_initialization()),
            json_response(&catalog::tests::artist_catalogue(1, "private-next-token")),
            json_response(&catalog::tests::artist_catalogue(2, "")),
        ]);
        client.account = Some(123);
        let track = catalog::profile_page(&json!({"data":[{"list":[catalog::tests::artist_row(1,json!({"id":"12345","domain":"fixture_artist","name":"Artist"}))],"hasMore":false}]}),123,0,&client.scope).unwrap().tracks.remove(0);
        let first = client.artist_tracks(&track).unwrap();
        let last = client.next_page(first.next.as_ref().unwrap()).unwrap();
        assert_eq!(last.tracks[0].id.audio_id(), 2);
        assert!(last.next.is_none());
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        for (index, artist) in [(0, "fixture_artist"), (1, "canonical_fixture")] {
            assert_eq!(requests[index].0, DESKTOP_ARTIST);
            assert!(
                requests[index]
                    .1
                    .contains(&("al".to_owned(), "1".to_owned()))
            );
            assert!(
                requests[index]
                    .1
                    .contains(&("__query".to_owned(), format!("artist/{artist}/top_audios")))
            );
        }
        assert_eq!(requests[2].0, DESKTOP_AUDIO);
        assert!(!requests[2].1.iter().any(|(key, _)| key == "start_from"));
        assert_eq!(requests[3].0, DESKTOP_AUDIO);
        assert!(
            requests[3]
                .1
                .contains(&("act".to_owned(), "load_catalog_section".to_owned()))
        );
        assert!(
            requests[3]
                .1
                .contains(&("section_id".to_owned(), "private-artist-section".to_owned()))
        );
        assert!(
            requests[3]
                .1
                .contains(&("start_from".to_owned(), "private-next-token".to_owned()))
        );
        let (mut other, other_transport) = self::client(vec![]);
        other.account = Some(123);
        assert!(other.artist_tracks(&track).is_err());
        assert!(other.next_page(first.next.as_ref().unwrap()).is_err());
        assert!(other_transport.requests.lock().unwrap().is_empty());
        let no_artist = album_track(&client, 99, Value::Null);
        assert!(client.artist_tracks(&no_artist).is_err());
    }

    /// A canonicalization cycle or foreign redirect never starts a catalogue request.
    #[test]
    fn artist_browsing_bounds_redirects_before_following_an_untrusted_destination() {
        let redirect = |target: &str| {
            json_response(&json!({"payload":[0,[serde_json::to_string(target).unwrap(),"0"]]}))
        };
        for responses in [
            vec![redirect("https://evil.test/artist/other/top_audios")],
            vec![
                redirect("/artist/alias/top_audios"),
                redirect("/artist/fixture_artist/top_audios"),
            ],
            vec![
                redirect("/artist/alias1/top_audios"),
                redirect("/artist/alias2/top_audios"),
                redirect("/artist/alias3/top_audios"),
            ],
        ] {
            let expected = responses.len();
            let (mut client, transport) = client(responses);
            client.account = Some(123);
            let track = catalog::profile_page(&json!({"data":[{"list":[catalog::tests::artist_row(1,json!({"domain":"fixture_artist"}))],"hasMore":false}]}),123,0,&client.scope).unwrap().tracks.remove(0);
            let error = client.artist_tracks(&track).unwrap_err().to_string();
            assert!(!error.contains("evil.test"));
            let requests = transport.requests.lock().unwrap();
            assert_eq!(requests.len(), expected);
            assert!(requests.iter().all(|request| request.0 == DESKTOP_ARTIST));
        }
    }

    #[test]
    fn album_title_fetches_once_for_tracks_with_the_same_exact_release() {
        let (mut client, transport) = client(vec![album_response(
            -2000123,
            456,
            json!("<b>Album</b> &amp; &#27;\n Band"),
        )]);
        client.account = Some(123);
        let reference = json!([-2000123, 456, "private-album-hash"]);
        let first = album_track(&client, 1, reference.clone());
        let second = album_track(&client, 2, reference);
        assert_eq!(
            client.album_title(&first).unwrap().as_deref(),
            Some("Album & Band")
        );
        assert_eq!(
            client.album_title(&second).unwrap().as_deref(),
            Some("Album & Band")
        );
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, DESKTOP_AUDIO);
        for (key, value) in [
            ("act", "load_section"),
            ("al", "1"),
            ("claim", "0"),
            ("owner_id", "-2000123"),
            ("playlist_id", "456"),
            ("access_hash", "private-album-hash"),
            ("offset", "0"),
            ("type", "playlist"),
            ("is_loading_all", "0"),
            ("is_preload", "0"),
        ] {
            assert!(requests[0].1.contains(&(key.to_owned(), value.to_owned())));
        }
        assert!(!format!("{client:?} {first:?}").contains("private-album-hash"));
        assert!(!format!("{client:?} {first:?} {requests:?}").contains("sid=fixture"));
    }

    #[test]
    fn album_title_rejects_another_client_before_any_http_request() {
        let (mut first, _) = client(vec![album_response(-2000123, 456, json!("Album"))]);
        first.account = Some(123);
        let track = album_track(&first, 1, json!([-2000123, 456, "private-album-hash"]));
        first.album_title(&track).unwrap();
        let (mut other, transport) = client(vec![]);
        other.account = Some(123);
        let error = other.album_title(&track).unwrap_err();
        assert!(matches!(error, ProviderError::InvalidRequest(_)));
        assert!(!error.to_string().contains("private-album-hash"));
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn tracks_without_valid_albums_need_no_metadata_requests() {
        let (mut client, transport) = client(vec![]);
        client.account = Some(123);
        for reference in [Value::Null, json!([-2000123, 456, "invalid\nhash"])] {
            let track = album_track(&client, 1, reference);
            assert!(client.album_title(&track).unwrap().is_none());
        }
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn missing_or_unusable_album_titles_are_cached_as_absent() {
        for title in [
            Value::Null,
            json!(false),
            json!({"name":"Album"}),
            json!(""),
            json!(" \t<b></b>&#27;"),
            json!("x".repeat(4097)),
        ] {
            let (mut client, transport) = client(vec![album_response(-2000123, 456, title)]);
            client.account = Some(123);
            let track = album_track(&client, 1, json!([-2000123, 456, "hash"]));
            assert!(client.album_title(&track).unwrap().is_none());
            assert!(client.album_title(&track).unwrap().is_none());
            assert_eq!(transport.requests.lock().unwrap().len(), 1);
        }
        let (mut client, transport) = client(vec![json_response(
            &json!({"payload":[0,[{"ownerId":-2000123,"id":456}]]}),
        )]);
        client.account = Some(123);
        let track = album_track(&client, 1, json!([-2000123, 456, "hash"]));
        assert!(client.album_title(&track).unwrap().is_none());
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn album_title_rejects_wrong_or_missing_response_identity_without_caching() {
        for metadata in [
            json!({"ownerId":-2000999,"id":456,"title":"Wrong"}),
            json!({"ownerId":-2000123,"id":999,"title":"Wrong"}),
            json!({"id":456,"title":"Wrong"}),
            json!({"ownerId":-2000123,"title":"Wrong"}),
            json!("Wrong"),
        ] {
            let (mut client, transport) = client(vec![
                json_response(&json!({"payload":[0,[metadata]]})),
                album_response(-2000123, 456, json!("Correct")),
            ]);
            client.account = Some(123);
            let track = album_track(&client, 1, json!([-2000123, 456, "private-album-hash"]));
            let error = client.album_title(&track).unwrap_err();
            assert!(matches!(error, ProviderError::InvalidResponse(_)));
            assert!(!error.to_string().contains("private-album-hash"));
            assert_eq!(
                client.album_title(&track).unwrap().as_deref(),
                Some("Correct")
            );
            assert_eq!(transport.requests.lock().unwrap().len(), 2);
        }
    }

    #[test]
    fn album_cache_keeps_access_hashes_distinct_and_retries_transport_errors() {
        let (mut client, transport) = client(vec![
            album_response(-2000123, 456, json!("First")),
            album_response(-2000123, 456, json!("Second")),
        ]);
        client.account = Some(123);
        transport
            .responses
            .lock()
            .unwrap()
            .push_front(Err(ProviderError::Transport("Fixture failure".to_owned())));
        let first = album_track(&client, 1, json!([-2000123, 456, "first-hash"]));
        let second = album_track(&client, 2, json!([-2000123, 456, "second-hash"]));
        assert!(client.album_title(&first).is_err());
        assert_eq!(
            client.album_title(&first).unwrap().as_deref(),
            Some("First")
        );
        assert_eq!(
            client.album_title(&second).unwrap().as_deref(),
            Some("Second")
        );
        assert_eq!(
            client.album_title(&first).unwrap().as_deref(),
            Some("First")
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 3);
    }

    #[test]
    fn album_title_requires_authentication_without_sending_a_request() {
        let (mut client, transport) = client(vec![]);
        let track = album_track(&client, 1, json!([-2000123, 456, "private-album-hash"]));
        let error = client.album_title(&track).unwrap_err();
        assert!(matches!(error, ProviderError::InvalidRequest(_)));
        assert!(!error.to_string().contains("private-album-hash"));
        assert!(transport.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn album_title_cache_is_bounded_and_distinguishes_release_identities() {
        let count = MAX_CACHED_ALBUMS as u64;
        let responses = (1..=count + 1)
            .map(|id| album_response(-2000123, id, json!(format!("Album {id}"))))
            .chain([album_response(-2000124, count + 1, json!("Other owner"))])
            .collect();
        let (mut client, transport) = client(responses);
        client.account = Some(123);
        for id in 1..=count + 1 {
            let track = album_track(&client, id, json!([-2000123, id, "same-hash"]));
            assert_eq!(
                client.album_title(&track).unwrap(),
                Some(format!("Album {id}"))
            );
            assert!(client.album_titles.len() <= MAX_CACHED_ALBUMS);
        }
        assert_eq!(client.album_titles.len(), 1);
        let other = album_track(
            &client,
            count + 2,
            json!([-2000124, count + 1, "same-hash"]),
        );
        assert_eq!(
            client.album_title(&other).unwrap().as_deref(),
            Some("Other owner")
        );
        assert_eq!(
            transport.requests.lock().unwrap().len(),
            MAX_CACHED_ALBUMS + 2
        );
    }

    #[test]
    fn authenticates_once_and_scopes_global_search_pagination_requests() {
        let (mut client, transport) = client(vec![
            html(
                "<script>window.vk = {\"id\":123,\"private\":\"ignored\"}; doNotExecute()</script>",
            ),
            json_response(&catalog::tests::search_fixture()),
            json_response(
                &json!({"payload":[0,[{"list":[catalog::tests::row(4)],"hasMore":false}]]}),
            ),
        ]);
        assert!(client.search("minsk").is_err());
        client.authenticate().unwrap();
        client.authenticate().unwrap();
        assert!(client.is_authenticated());
        client.set_page_size(81);
        let page = client.search(" minsk ").unwrap();
        let next = client.next_page(page.next.as_ref().unwrap()).unwrap();
        assert_eq!(next.tracks[0].id.audio_id(), 4);
        assert!(next.next.is_none());
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].0, MOBILE_AUDIO);
        assert!(
            requests[1]
                .1
                .contains(&("q".to_owned(), "minsk".to_owned()))
        );
        assert!(
            requests[1]
                .1
                .contains(&("count".to_owned(), "81".to_owned()))
        );
        assert!(
            requests[2]
                .1
                .contains(&("offset".to_owned(), "private:offset+token".to_owned()))
        );
    }

    #[test]
    fn resolves_selected_track_without_persisting_or_logging_its_locator() {
        let mut loaded = catalog::tests::row(2);
        loaded[2] = json!("https://psv4.vkuseraudio.ru/fixture/index.m3u8?token=private-fixture");
        let (mut client, transport) = client(vec![
            html("<script>window.vk={\"id\":123};</script>"),
            json_response(&catalog::tests::search_fixture()),
            json_response(&json!({"data":[[loaded]]})),
        ]);
        client.authenticate().unwrap();
        let page = client.search("minsk").unwrap();
        let locator = client.resolve(&page.tracks[0]).unwrap();
        assert_eq!(locator.as_url().path(), "/fixture/index.m3u8");
        assert!(!format!("{client:?} {page:?} {locator:?}").contains("private-fixture"));
        assert_eq!(transport.requests.lock().unwrap().len(), 3);
        let (mut other, other_transport) =
            self::client(vec![html("<script>window.vk={\"id\":123};</script>")]);
        other.authenticate().unwrap();
        assert!(other.resolve(&page.tracks[0]).is_err());
        assert!(other.next_page(page.next.as_ref().unwrap()).is_err());
        assert_eq!(other_transport.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn resolves_exact_handle_then_fetches_the_numeric_profile_once() {
        let (mut client, transport) = client(vec![
            html("<script>window.vk={\"id\":123};</script>"),
            html(
                "window.cur.apiPrefetchCache = [{\"method\":\"utils.resolveScreenName\",\"request\":{\"screen_name\":\"fixture\"},\"response\":{\"object_id\":456,\"type\":\"user\"}}];",
            ),
            json_response(&json!({"data":[{"list":[catalog::tests::row(1)],"hasMore":0}]})),
        ]);
        client.authenticate().unwrap();
        let page = client
            .profile_tracks(&ProfileReference::parse("fixture").unwrap())
            .unwrap();
        assert_eq!(page.tracks.len(), 1);
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests[1].0, "https://m.vk.ru/fixture");
        assert!(
            requests[2]
                .1
                .contains(&("owner_id".to_owned(), "456".to_owned()))
        );
    }

    #[test]
    fn resolves_exact_user_from_bounded_prefetch_array_without_guessing() {
        let html = r#"window.cur.apiPrefetchCache = [
            {"method":"utils.resolveScreenName","request":{"screen_name":"other"},"response":{"object_id":99,"type":"user"}},
            {"method":"utils.resolveScreenName","request":{"screen_name":"fixture"},"response":{"object_id":456,"type":"user"}}
        ];"#;
        assert_eq!(profile_owner(html, "fixture"), Some(456));
        assert_eq!(profile_owner(html, "missing"), None);
        let record = json!({"method":"utils.resolveScreenName","request":{"screen_name":"fixture"},"response":{"object_id":456,"type":"user"}});
        let duplicate = format!("window.cur.apiPrefetchCache = {};", json!([record, record]));
        assert_eq!(profile_owner(&duplicate, "fixture"), None);
        let oversized = format!(
            "window.cur.apiPrefetchCache = {};",
            json!(vec![record; 129])
        );
        assert_eq!(profile_owner(&oversized, "fixture"), None);
    }

    #[test]
    fn decodes_declared_windows_1251_and_rejects_mismatched_profile_metadata() {
        let (bytes, _, errors) = encoding_rs::WINDOWS_1251.encode("Только для ознакомления");
        assert!(!errors);
        assert_eq!(
            decode_body(HttpResponse {
                status: 200,
                login_redirect: false,
                content_type: Some("application/json; charset=windows-1251".to_owned()),
                body: bytes.into_owned()
            })
            .unwrap(),
            "Только для ознакомления"
        );
        assert!(profile_owner("window.cur.apiPrefetchCache={\"method\":\"utils.resolveScreenName\",\"request\":{\"screen_name\":\"other\"},\"response\":{\"object_id\":456,\"type\":\"user\"}};", "fixture").is_none());
        assert!(bootstrap::account("<script>window.vk = evaluateThis();</script>").is_none());
        assert!(bootstrap::account("<script>window.vk={\"id\":0};</script>").is_none());
    }

    #[test]
    fn login_redirect_classification_requires_an_exact_first_party_https_destination() {
        for status in [301, 302, 303, 307, 308] {
            for destination in [
                "https://login.vk.ru/?role=fast&_origin=private-origin&to=private-token",
                "https://login.vk.com/?role=fast&to=private-token",
            ] {
                assert!(is_login_redirect(status, Some(destination)));
            }
        }
        for status in [200, 201, 300, 304, 401, 403, 500] {
            assert!(!is_login_redirect(status, Some("https://login.vk.ru/")));
        }
        for destination in [
            "https://login.vk.ru.evil.test/",
            "https://evil.test/?next=https://login.vk.ru/",
            "https://login.vk.ru@evil.test/",
            "https://secret@login.vk.ru/",
            "https://login.vk.ru:444/",
            "http://login.vk.ru/",
            "https://vk.ru/login",
            "https://m.vk.ru/audio",
            "https://id.vk.ru/",
            "https://login.vk.ru/#private",
            "//login.vk.ru/",
            "/login",
            "https://login.vk.ru/\n",
            "not a URL",
        ] {
            assert!(!is_login_redirect(302, Some(destination)), "{destination}");
        }
        assert!(!is_login_redirect(302, None));
        assert!(!is_login_redirect(
            302,
            Some(&format!("https://login.vk.ru/?to={}", "x".repeat(8192)))
        ));
    }

    #[test]
    fn login_redirect_requests_session_recovery_without_following_or_disclosing_it() {
        for status in [301, 302, 303, 307, 308] {
            let (mut client, transport) = client(vec![
                HttpResponse {
                    status,
                    login_redirect: true,
                    content_type: None,
                    body: b"private-login-body sid=fixture".to_vec(),
                },
                HttpResponse {
                    status,
                    login_redirect: true,
                    content_type: None,
                    body: vec![],
                },
            ]);
            let error = client.authenticate().unwrap_err();
            assert!(matches!(error, ProviderError::InvalidRequest(_)));
            let message = error.to_string();
            assert!(message.contains("sign in"));
            assert!(message.contains("cookie"));
            assert!(message.contains("security check"));
            assert!(message.contains("restart Youta"));
            assert!(!message.contains("private-login-body"));
            assert!(!message.contains("sid=fixture"));
            assert!(!client.is_authenticated());
            let requests = transport.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[1].0, DESKTOP_BOOTSTRAP);
        }
    }

    #[test]
    fn redirects_and_http_errors_never_expose_body_or_cookie_data() {
        for status in [302, 500] {
            let (mut client, transport) = client(vec![HttpResponse {
                status,
                login_redirect: false,
                content_type: None,
                body: b"private-fixture".to_vec(),
            }]);
            let error = client.authenticate().unwrap_err();
            assert!(!error.to_string().contains("private-fixture"));
            if status == 302 {
                assert!(matches!(
                    error,
                    ProviderError::HttpStatusAt { status: 302, .. }
                ));
            }
            assert_eq!(transport.requests.lock().unwrap().len(), 1);
        }
        assert!(
            decode_body(HttpResponse {
                status: 200,
                login_redirect: false,
                content_type: None,
                body: vec![b'x'; MAX_RESPONSE_BYTES + 1]
            })
            .is_err()
        );
    }
}
