//! Bounded projections of authenticated catalogue responses into safe metadata.
//!
//! Reload hashes and pagination tokens are runtime-only and scoped to the exact
//! client that obtained them. They have no serializable or printable form.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use html5gum::{DefaultEmitter, Token, Tokenizer};
use serde_json::Value;
use url::Url;

use super::VkAudioId;
use crate::providers::ProviderError;

const MAX_TRACKS: usize = 2_000;
const MAX_PLAYLISTS: usize = 128;
const MAX_LABEL_BYTES: usize = 4 * 1024;
const MAX_TOKEN_BYTES: usize = 2 * 1024;
const MAX_DURATION_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Safe track metadata with a private, session-scoped playback lookup key.
#[derive(Clone)]
pub struct VkTrack {
    /// Stable signed owner and positive audio identity, suitable for history.
    pub id: VkAudioId,
    /// Entity-decoded, terminal-safe title.
    pub title: String,
    /// Entity-decoded, terminal-safe artist name.
    pub artist: String,
    /// Optional release-album title, populated by a separate metadata request.
    pub album_title: Option<String>,
    /// Advertised full duration in seconds, when valid.
    pub duration_seconds: Option<u64>,
    /// Public HTTPS VK artwork, fetched separately without session cookies.
    pub artwork_url: Option<Url>,
    /// Bounded pixel dimensions advertised by the selected cover URL, when present.
    pub artwork_dimensions: Option<(u32, u32)>,
    reload: ReloadKey,
    album: Option<AlbumReference>,
    main_artist: Option<String>,
    scope: Arc<()>,
}

/// Runtime-only release identity; its access hash must never be persisted or logged.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) struct AlbumReference {
    pub(super) owner: i64,
    pub(super) id: u64,
    pub(super) access_hash: String,
}

impl fmt::Debug for AlbumReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AlbumReference([REDACTED])")
    }
}

#[derive(Clone)]
struct ReloadKey {
    action: String,
    locator: String,
}

impl fmt::Debug for VkTrack {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VkTrack")
            .field("id", &self.id)
            .field("title", &self.title)
            .field("artist", &self.artist)
            .field("album_title", &self.album_title)
            .field("duration_seconds", &self.duration_seconds)
            .field("private_session_data", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl VkTrack {
    /// Reports whether this row provides a validated release-album reference.
    #[must_use]
    pub fn has_album(&self) -> bool {
        self.album.is_some()
    }

    /// Returns the first validated main artist's public page, without private tokens.
    #[must_use]
    pub fn artist_webpage_url(&self) -> Option<Url> {
        let mut url = self.webpage_url();
        url.set_path(&format!("/artist/{}", self.main_artist.as_ref()?));
        Some(url)
    }

    /// Keeps artist navigation tied to the authenticated client that supplied it.
    pub(super) fn artist_reference(&self, scope: &Arc<()>) -> Result<Option<&str>, ProviderError> {
        if !Arc::ptr_eq(&self.scope, scope) {
            return Err(stale_session());
        }
        Ok(self.main_artist.as_deref())
    }

    /// Returns the public release page, without its private access hash.
    #[must_use]
    pub fn album_webpage_url(&self) -> Option<Url> {
        let album = self.album.as_ref()?;
        let mut url = self.webpage_url();
        url.set_path(&format!("/music/album/{}_{}", album.owner, album.id));
        Some(url)
    }

    /// Exposes private album metadata only to the client that obtained the track.
    pub(super) fn album_reference(
        &self,
        scope: &Arc<()>,
    ) -> Result<Option<&AlbumReference>, ProviderError> {
        if !Arc::ptr_eq(&self.scope, scope) {
            return Err(stale_session());
        }
        Ok(self.album.as_ref())
    }

    /// Returns the canonical original track page without private lookup hashes.
    #[must_use]
    pub fn webpage_url(&self) -> Url {
        self.id.webpage_url()
    }

    /// Produces a request key only for the same authenticated client instance.
    pub(super) fn reload_key(&self, scope: &Arc<()>) -> Result<String, ProviderError> {
        if !Arc::ptr_eq(&self.scope, scope) {
            return Err(stale_session());
        }
        Ok(format!(
            "{}_{}_{}",
            self.id, self.reload.action, self.reload.locator
        ))
    }

    /// Builds deterministic display-only rows for controller boundary tests.
    #[cfg(test)]
    pub(crate) fn fixture(id: VkAudioId, title: &str, artist: &str) -> Self {
        Self {
            id,
            title: title.to_owned(),
            artist: artist.to_owned(),
            album_title: None,
            duration_seconds: Some(120),
            artwork_url: None,
            artwork_dimensions: None,
            reload: ReloadKey {
                action: "fixture-action".to_owned(),
                locator: "fixture-locator".to_owned(),
            },
            album: None,
            main_artist: None,
            scope: Arc::new(()),
        }
    }

    /// Builds a synthetic album-bearing row for controller scheduling tests.
    #[cfg(test)]
    pub(crate) fn fixture_with_album(id: VkAudioId, title: &str, artist: &str) -> Self {
        let mut track = Self::fixture(id, title, artist);
        track.album = Some(AlbumReference {
            owner: -2000123,
            id: 456,
            access_hash: "fixture-album-hash".to_owned(),
        });
        track
    }

    /// Builds a synthetic exact-artist row for internal-navigation boundary tests.
    #[cfg(test)]
    pub(crate) fn fixture_with_artist(id: VkAudioId, title: &str, artist: &str) -> Self {
        let mut track = Self::fixture(id, title, artist);
        track.main_artist = Some("fixture_artist".to_owned());
        track
    }

    /// Shares a synthetic row's owner only with offline authenticated-client fixtures.
    #[cfg(test)]
    pub(super) fn fixture_scope(&self) -> Arc<()> {
        Arc::clone(&self.scope)
    }
}

/// A single upstream page; VK currently chooses its own page size.
#[derive(Clone, Debug)]
pub struct VkPage {
    /// Valid playable candidates in upstream order, without duplicate identities.
    pub tracks: Vec<VkTrack>,
    /// Runtime-only continuation, absent when the provider reports no more rows.
    pub next: Option<VkPageCursor>,
}

/// An opaque, non-persistable page locator owned by one client session.
#[derive(Clone)]
pub struct VkPageCursor {
    pub(super) kind: CursorKind,
    scope: Arc<()>,
}

#[derive(Clone)]
pub(super) enum CursorKind {
    Search {
        owner: i64,
        playlist: String,
        offset: String,
    },
    Profile {
        owner: u64,
        offset: u64,
    },
    Album {
        album: AlbumReference,
        offset: u64,
    },
    Artist {
        section: String,
        offset: String,
    },
}

impl fmt::Debug for VkPageCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("VkPageCursor([REDACTED])")
    }
}

impl VkPageCursor {
    pub(super) fn validate_scope(&self, scope: &Arc<()>) -> Result<(), ProviderError> {
        if Arc::ptr_eq(&self.scope, scope) {
            Ok(())
        } else {
            Err(stale_session())
        }
    }
}

/// Selects global results by the website's locale-independent block identity.
pub(super) fn search_page(value: &Value, scope: &Arc<()>) -> Result<VkPage, ProviderError> {
    let metadata = value
        .pointer("/payload/1/1")
        .and_then(Value::as_object)
        .ok_or_else(invalid_catalogue)?;
    let playlists = metadata
        .get("playlists")
        .and_then(Value::as_array)
        .ok_or_else(invalid_catalogue)?;
    if playlists.len() > MAX_PLAYLISTS {
        return Err(invalid_catalogue());
    }
    if playlists.is_empty() {
        return Ok(VkPage {
            tracks: Vec::new(),
            next: None,
        });
    }
    let html = value
        .pointer("/payload/1/0")
        .and_then(Value::as_str)
        .ok_or_else(invalid_catalogue)?;
    let block_id = global_search_block(html)?;
    let mut matches = playlists
        .iter()
        .filter(|playlist| playlist.get("id").and_then(Value::as_str) == Some(block_id.as_str()));
    let playlist = matches.next().ok_or_else(invalid_catalogue)?;
    if matches.next().is_some() || playlist.get("type").and_then(Value::as_str) != Some("search") {
        return Err(invalid_catalogue());
    }
    let owner = integer(playlist.get("ownerId").ok_or_else(invalid_catalogue)?)
        .ok_or_else(invalid_catalogue)?;
    search_playlist(playlist, owner, &block_id, None, scope)
}

/// Parses the exact desktop continuation route without selecting another block.
pub(super) fn next_search_page(
    value: &Value,
    cursor: &VkPageCursor,
    scope: &Arc<()>,
) -> Result<VkPage, ProviderError> {
    let CursorKind::Search {
        owner,
        playlist,
        offset,
    } = &cursor.kind
    else {
        return Err(invalid_catalogue());
    };
    let response = value
        .pointer("/payload/1/0")
        .filter(|value| value.is_object())
        .ok_or_else(invalid_catalogue)?;
    if response
        .get("id")
        .is_some_and(|id| id.as_str() != Some(playlist))
        || response
            .get("ownerId")
            .is_some_and(|id| integer(id) != Some(*owner))
    {
        return Err(invalid_catalogue());
    }
    search_playlist(response, *owner, playlist, Some(offset), scope)
}

fn search_playlist(
    value: &Value,
    owner: i64,
    playlist: &str,
    previous: Option<&str>,
    scope: &Arc<()>,
) -> Result<VkPage, ProviderError> {
    let tracks = parse_tracks(value.get("list").ok_or_else(invalid_catalogue)?, scope)?;
    let next = if has_more(value)? {
        let offset = bounded_token(
            value
                .get("nextOffset")
                .and_then(Value::as_str)
                .ok_or_else(invalid_catalogue)?,
        )?;
        if previous == Some(offset.as_str()) {
            return Err(invalid_catalogue());
        }
        Some(VkPageCursor {
            kind: CursorKind::Search {
                owner,
                playlist: bounded_token(playlist)?,
                offset,
            },
            scope: Arc::clone(scope),
        })
    } else {
        None
    };
    Ok(VkPage { tracks, next })
}

/// Parses one user's mobile catalogue with strictly advancing numeric offsets.
pub(super) fn profile_page(
    value: &Value,
    owner: u64,
    previous: u64,
    scope: &Arc<()>,
) -> Result<VkPage, ProviderError> {
    let data = value
        .pointer("/data/0")
        .filter(|value| value.is_object())
        .ok_or_else(invalid_catalogue)?;
    profile_playlist(data, owner, previous, scope)
}

/// Reads only the requested owner's personal playlist from a successful desktop response.
pub(super) fn desktop_profile_page(
    value: &Value,
    owner: u64,
    previous: u64,
    scope: &Arc<()>,
) -> Result<VkPage, ProviderError> {
    if owner == 0
        || i64::try_from(owner).is_err()
        || value.pointer("/payload/0").and_then(Value::as_i64) != Some(0)
    {
        return Err(invalid_catalogue());
    }
    let data = value
        .pointer("/payload/1/0")
        .filter(|value| value.is_object())
        .ok_or_else(invalid_catalogue)?;
    if data.get("ownerId").and_then(Value::as_u64) != Some(owner)
        || data.get("id").and_then(Value::as_i64) != Some(-1)
    {
        return Err(invalid_catalogue());
    }
    profile_playlist(data, owner, previous, scope)
}

/// Shares bounded row parsing and strictly advancing profile cursors across verified envelopes.
fn profile_playlist(
    data: &Value,
    owner: u64,
    previous: u64,
    scope: &Arc<()>,
) -> Result<VkPage, ProviderError> {
    let tracks = parse_tracks(data.get("list").ok_or_else(invalid_catalogue)?, scope)?;
    let next = if has_more(data)? {
        let offset = data
            .get("nextOffset")
            .and_then(Value::as_u64)
            .filter(|offset| *offset > previous)
            .ok_or_else(invalid_catalogue)?;
        Some(VkPageCursor {
            kind: CursorKind::Profile { owner, offset },
            scope: Arc::clone(scope),
        })
    } else {
        None
    };
    Ok(VkPage { tracks, next })
}

/// Reads the same already-validated row shape from a selected-only reload response.
pub(super) fn reloaded_url(value: &Value, id: VkAudioId) -> Result<&str, ProviderError> {
    let row = value
        .pointer("/data/0/0")
        .and_then(Value::as_array)
        .ok_or_else(invalid_catalogue)?;
    if row_id(row) != Some(id) {
        return Err(invalid_catalogue());
    }
    row.get(2)
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty() && url.len() <= 16 * 1024)
        .ok_or_else(invalid_catalogue)
}

/// Reads the selected row only from the successful desktop reload envelope.
///
/// This is deliberately separate from the mobile parser: a mismatched first row
/// must not cause a search through other response fields for a plausible URL.
pub(super) fn reloaded_desktop_url(value: &Value, id: VkAudioId) -> Result<&str, ProviderError> {
    if value.pointer("/payload/0").and_then(Value::as_i64) != Some(0) {
        return Err(invalid_catalogue());
    }
    let row = value
        .pointer("/payload/1/0/0")
        .and_then(Value::as_array)
        .ok_or_else(invalid_catalogue)?;
    if row_id(row) != Some(id) {
        return Err(invalid_catalogue());
    }
    row.get(2)
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty() && url.len() <= 16 * 1024)
        .ok_or_else(invalid_catalogue)
}

/// Projects a matching release response into bounded, terminal-safe public text.
pub(super) fn album_title(
    value: &Value,
    album: &AlbumReference,
) -> Result<Option<String>, ProviderError> {
    let metadata = value
        .pointer("/payload/1/0")
        .ok_or_else(invalid_catalogue)?;
    if metadata.get("ownerId").and_then(integer) != Some(album.owner)
        || metadata.get("id").and_then(integer) != i64::try_from(album.id).ok()
    {
        return Err(invalid_catalogue());
    }
    Ok(metadata
        .get("title")
        .and_then(Value::as_str)
        .and_then(plain_label)
        .filter(|title| !title.is_empty()))
}

/// Reads one exact release page and keeps its private access hash in the cursor.
pub(super) fn album_page(
    value: &Value,
    album: &AlbumReference,
    previous: u64,
    scope: &Arc<()>,
) -> Result<VkPage, ProviderError> {
    let title = album_title(value, album)?;
    let metadata = value
        .pointer("/payload/1/0")
        .ok_or_else(invalid_catalogue)?;
    let mut tracks = parse_tracks(metadata.get("list").ok_or_else(invalid_catalogue)?, scope)?;
    for track in &mut tracks {
        if track.album.as_ref() == Some(album) {
            track.album_title = title.clone();
        }
    }
    let next = if has_more(metadata)? {
        let offset = metadata
            .get("nextOffset")
            .and_then(Value::as_u64)
            .filter(|offset| *offset > previous)
            .ok_or_else(invalid_catalogue)?;
        Some(VkPageCursor {
            kind: CursorKind::Album {
                album: album.clone(),
                offset,
            },
            scope: Arc::clone(scope),
        })
    } else {
        None
    };
    Ok(VkPage { tracks, next })
}

/// Reads the artist block from the unique constructor's literal JSON configuration.
///
/// The deployed page initializes `AudioPage` with a DOM expression followed by
/// JSON. Only that second argument is deserialized; no JavaScript is evaluated.
pub(super) fn artist_section(value: &Value) -> Result<String, ProviderError> {
    let script = value
        .pointer("/payload/1/2")
        .and_then(Value::as_str)
        .filter(|script| script.len() <= 2 * 1024 * 1024)
        .ok_or_else(invalid_catalogue)?;
    let marker = "new AudioPage(";
    let mut occurrences = script.match_indices(marker);
    let (position, _) = occurrences.next().ok_or_else(invalid_catalogue)?;
    if occurrences.next().is_some() {
        return Err(invalid_catalogue());
    }
    let arguments = script[position + marker.len()..].trim_start();
    if !arguments.starts_with("geByClass1(") {
        return Err(invalid_catalogue());
    }
    let literal = second_constructor_argument(arguments).ok_or_else(invalid_catalogue)?;
    if !literal.starts_with('{') {
        return Err(invalid_catalogue());
    }
    let mut values = serde_json::Deserializer::from_str(literal).into_iter::<Value>();
    let configuration = values
        .next()
        .ok_or_else(invalid_catalogue)?
        .map_err(|_| invalid_catalogue())?;
    if !literal[values.byte_offset()..]
        .trim_start()
        .starts_with(')')
        || configuration.get("initSection").and_then(Value::as_str) != Some("artist_block")
    {
        return Err(invalid_catalogue());
    }
    configuration
        .pointer("/sectionData/artist_block/sectionId")
        .and_then(Value::as_str)
        .ok_or_else(invalid_catalogue)
        .and_then(bounded_token)
}

/// Skips a bounded DOM argument while respecting its quoted strings and parentheses.
fn second_constructor_argument(arguments: &str) -> Option<&str> {
    let mut depth = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    for (position, byte) in arguments.bytes().take(4 * 1024).enumerate() {
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == delimiter {
                quote = None;
            }
            continue;
        }
        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'(' => depth = depth.checked_add(1)?,
            b')' => depth = depth.checked_sub(1)?,
            b',' if depth == 0 => return Some(arguments[position + 1..].trim_start()),
            b'{' | b'}' | b'[' | b']' | b';' | b'`' => return None,
            _ => {}
        }
    }
    None
}

/// Projects only structured rows belonging to the exact artist catalogue section.
pub(super) fn artist_catalogue_page(
    value: &Value,
    section: &str,
    previous: Option<&str>,
    scope: &Arc<()>,
) -> Result<VkPage, ProviderError> {
    let metadata = value
        .pointer("/payload/1/1")
        .ok_or_else(invalid_catalogue)?;
    if metadata.get("sectionId").and_then(Value::as_str) != Some(section) {
        return Err(invalid_catalogue());
    }
    let token = |key: &str| -> Result<Option<String>, ProviderError> {
        match metadata.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) if value.is_empty() => Ok(None),
            Some(Value::String(value)) => bounded_token(value).map(Some),
            _ => Err(invalid_catalogue()),
        }
    };
    let snake = token("next_from")?;
    let camel = token("nextFrom")?;
    if snake
        .as_ref()
        .zip(camel.as_ref())
        .is_some_and(|(left, right)| left != right)
    {
        return Err(invalid_catalogue());
    }
    let offset = snake.or(camel);
    if offset
        .as_deref()
        .is_some_and(|offset| Some(offset) == previous)
    {
        return Err(invalid_catalogue());
    }
    let tracks = parse_tracks(
        metadata
            .pointer("/playlist/list")
            .ok_or_else(invalid_catalogue)?,
        scope,
    )?;
    let next = offset.map(|offset| VkPageCursor {
        kind: CursorKind::Artist {
            section: section.to_owned(),
            offset,
        },
        scope: Arc::clone(scope),
    });
    Ok(VkPage { tracks, next })
}

/// Reads only the continuation of the artist catalogue selected by the website.
pub(super) fn next_artist_page(
    value: &Value,
    cursor: &VkPageCursor,
    scope: &Arc<()>,
) -> Result<VkPage, ProviderError> {
    let CursorKind::Artist {
        section,
        offset: previous,
    } = &cursor.kind
    else {
        return Err(invalid_catalogue());
    };
    artist_catalogue_page(value, section, Some(previous), scope)
}

fn parse_tracks(value: &Value, scope: &Arc<()>) -> Result<Vec<VkTrack>, ProviderError> {
    let rows = value
        .as_array()
        .filter(|rows| rows.len() <= MAX_TRACKS)
        .ok_or_else(invalid_catalogue)?;
    let mut seen = HashSet::new();
    Ok(rows
        .iter()
        .filter_map(|value| parse_track(value, scope))
        .filter(|track| seen.insert(track.id))
        .collect())
}

fn parse_track(value: &Value, scope: &Arc<()>) -> Option<VkTrack> {
    let row = value.as_array()?;
    let id = row_id(row)?;
    let hashes = row.get(13)?.as_str()?;
    if hashes.len() > MAX_TOKEN_BYTES {
        return None;
    }
    let mut parts = hashes.split('/');
    let action = parts.nth(2)?;
    let locator = parts.nth(2)?;
    if [action, locator].iter().any(|hash| {
        hash.is_empty()
            || hash.len() > 256
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    }) {
        return None;
    }
    let title = plain_label(row.get(3)?.as_str()?)?;
    if title.is_empty() {
        return None;
    }
    let artist = plain_label(row.get(4)?.as_str()?)?;
    let duration_seconds = row
        .get(5)
        .and_then(Value::as_u64)
        .filter(|seconds| *seconds <= MAX_DURATION_SECONDS);
    let artwork_url = row
        .get(14)
        .and_then(Value::as_str)
        .filter(|value| value.len() <= 16 * 1024)
        .and_then(|value| {
            value
                .split(',')
                .take(4)
                .filter_map(artwork_url)
                .max_by_key(|url| {
                    artwork_dimensions(url)
                        .map_or(0, |(width, height)| u64::from(width) * u64::from(height))
                })
        });
    let artwork_dimensions = artwork_url.as_ref().and_then(artwork_dimensions);
    Some(VkTrack {
        id,
        title,
        artist,
        album_title: None,
        duration_seconds,
        artwork_url,
        artwork_dimensions,
        reload: ReloadKey {
            action: action.to_owned(),
            locator: locator.to_owned(),
        },
        album: row.get(19).and_then(parse_album_reference),
        main_artist: row.get(17).and_then(parse_main_artist),
        scope: Arc::clone(scope),
    })
}

/// Selects an upstream artist identity; performer display text is never a lookup key.
fn parse_main_artist(value: &Value) -> Option<String> {
    let artists = value.as_array().filter(|artists| artists.len() <= 32)?;
    artists.iter().find_map(|artist| {
        if let Some(domain) = artist
            .get("domain")
            .and_then(Value::as_str)
            .filter(|domain| {
                !domain.is_empty()
                    && domain.len() <= 128
                    && domain
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            })
        {
            return Some(domain.to_owned());
        }
        let id = artist.get("id")?;
        let numeric = id.as_u64().or_else(|| id.as_str()?.parse::<u64>().ok())?;
        (numeric > 0).then(|| numeric.to_string())
    })
}

/// Validates VK's `[owner_id, playlist_id, access_hash]` release tuple at slot 19.
///
/// The web tuple and separate playlist-title lookup are also documented by
/// `vk-music-saver`'s `getAlbumId` and `getAudioPlaylistById` adapters; both
/// were checked against a live catalogue before adopting this projection.
fn parse_album_reference(value: &Value) -> Option<AlbumReference> {
    let fields = value.as_array().filter(|fields| fields.len() == 3)?;
    let owner = integer(&fields[0])?;
    let id = fields[1].as_u64()?;
    VkAudioId::new(owner, id).ok()?;
    let access_hash = fields[2].as_str()?;
    if access_hash.is_empty()
        || access_hash.len() > 256
        || !access_hash
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    Some(AlbumReference {
        owner,
        id,
        access_hash: access_hash.to_owned(),
    })
}

fn row_id(row: &[Value]) -> Option<VkAudioId> {
    VkAudioId::new(integer(row.get(1)?)?, row.first()?.as_u64()?).ok()
}

fn integer(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

fn has_more(value: &Value) -> Result<bool, ProviderError> {
    match value.get("hasMore") {
        Some(Value::Bool(value)) => Ok(*value),
        Some(Value::Number(value)) if value.as_u64() == Some(0) => Ok(false),
        Some(Value::Number(value)) if value.as_u64() == Some(1) => Ok(true),
        _ => Err(invalid_catalogue()),
    }
}

fn bounded_token(value: &str) -> Result<String, ProviderError> {
    if value.is_empty()
        || value.len() > MAX_TOKEN_BYTES
        || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return Err(invalid_catalogue());
    }
    Ok(value.to_owned())
}

fn global_search_block(html: &str) -> Result<String, ProviderError> {
    if html.len() > 2 * 1024 * 1024 {
        return Err(invalid_catalogue());
    }
    let mut selected = None;
    for token in Tokenizer::new(html) {
        let Ok(Token::StartTag(tag)) = token else {
            continue;
        };
        let Some(class) = tag.attributes.get(b"class".as_slice()) else {
            continue;
        };
        if !class
            .split(u8::is_ascii_whitespace)
            .any(|class| class == b"CatalogBlock__search_global_audios")
        {
            continue;
        }
        let id = tag
            .attributes
            .get(b"data-id".as_slice())
            .and_then(|value| std::str::from_utf8(value).ok())
            .ok_or_else(invalid_catalogue)?;
        if selected.is_some() {
            return Err(invalid_catalogue());
        }
        selected = Some(bounded_token(id)?);
    }
    selected.ok_or_else(invalid_catalogue)
}

fn plain_label(value: &str) -> Option<String> {
    if value.len() > MAX_LABEL_BYTES {
        return None;
    }
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    let mut output = String::new();
    for token in Tokenizer::new_with_emitter(value, emitter) {
        if let Ok(Token::String(text)) = token {
            output.extend(
                String::from_utf8_lossy(text.value.as_ref())
                    .chars()
                    .filter(|character| !character.is_control()),
            );
        }
    }
    let output = output.split_whitespace().collect::<Vec<_>>().join(" ");
    (output.len() <= MAX_LABEL_BYTES).then_some(output)
}

/// Reads the rendition VK actually advertised without rewriting signed CDN URLs.
///
/// Dimensionless URLs retain their previous last-valid fallback ordering. A
/// duplicate, malformed or excessive size is unknown, not an allocation hint.
fn artwork_dimensions(url: &Url) -> Option<(u32, u32)> {
    let mut sizes = url.query_pairs().filter(|(key, _)| key == "size");
    let (_, size) = sizes.next()?;
    if sizes.next().is_some() {
        return None;
    }
    let (width, height) = size.split_once('x')?;
    let parse = |value: &str| {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        value
            .parse::<u32>()
            .ok()
            .filter(|size| (1..=4096).contains(size))
    };
    Some((parse(width)?, parse(height)?))
}

fn artwork_url(raw: &str) -> Option<Url> {
    if raw.len() > 4 * 1024 || raw.chars().any(char::is_control) {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    let host = url.host_str()?;
    let trusted = [
        "userapi.com",
        "vkuserphoto.ru",
        "vkuserphoto.net",
        "vkuserphoto.com",
    ]
    .iter()
    .any(|root| {
        host == *root
            || host
                .strip_suffix(root)
                .is_some_and(|prefix| prefix.ends_with('.'))
    });
    (trusted
        && url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
        && url.port_or_known_default() == Some(443))
    .then_some(url)
}

pub(super) fn invalid_catalogue() -> ProviderError {
    ProviderError::InvalidResponse("VK returned an unsupported catalogue response".to_owned())
}

fn stale_session() -> ProviderError {
    ProviderError::InvalidRequest(
        "VK results belong to another session; search or browse again".to_owned(),
    )
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use serde_json::json;

    pub(in crate::providers::vk) fn row(id: u64) -> Value {
        json!([
            id,
            -123,
            "",
            "Title &amp; &quot;fixture&quot;",
            "Artist &amp; band",
            120,
            null,
            null,
            null,
            null,
            null,
            null,
            null,
            "a/b/privateaction/d/e/privatelocator",
            "https://sun1.userapi.com/small.jpg,https://sun1.userapi.com/large.jpg"
        ])
    }

    /// Extends a synthetic track with VK's private release-album reference tuple.
    pub(in crate::providers::vk) fn album_row(id: u64, reference: Value) -> Value {
        let mut row = row(id);
        let fields = row.as_array_mut().unwrap();
        fields.resize(20, Value::Null);
        fields[19] = reference;
        row
    }

    /// Adds synthetic main-artist metadata without any account-specific values.
    pub(in crate::providers::vk) fn artist_row(id: u64, artist: Value) -> Value {
        let mut row = album_row(id, Value::Null);
        row[17] = json!([artist]);
        row
    }

    /// Models only the verified artist constructor's literal configuration.
    pub(in crate::providers::vk) fn artist_initialization() -> Value {
        let configuration = json!({
            "initSection":"artist_block",
            "sectionData":{"artist_block":{"sectionId":"private-artist-section"}},
            "readyAudio":null
        });
        json!({"payload":[0,["","<div class='CatalogSection'></div>",format!(
            "new AudioPage(geByClass1('_audio_page_layout',document.getElementById('content')), {configuration}); trailingCode();"
        ),{}]]})
    }

    /// Keeps all structured rows and continuation metadata outside display HTML.
    pub(in crate::providers::vk) fn artist_catalogue(id: u64, next: &str) -> Value {
        json!({"payload":[0,[["<div>Unrelated display HTML</div>"],{
            "sectionId":"private-artist-section",
            "nextFrom":next,"next_from":next,
            "playlist":{"list":[row(id)]}
        }]]})
    }

    #[test]
    fn artist_links_use_validated_exact_metadata_without_name_searches() {
        let scope = Arc::new(());
        let track = parse_track(
            &artist_row(
                1,
                json!({"id":"1234567890123456","domain":"fixture_artist","name":"Artist"}),
            ),
            &scope,
        )
        .unwrap();
        assert_eq!(
            track.artist_webpage_url().unwrap().as_str(),
            "https://vk.ru/artist/fixture_artist"
        );
        let numeric = parse_track(
            &artist_row(1, json!({"id":"1234567890123456","name":"Artist"})),
            &scope,
        )
        .unwrap();
        assert_eq!(
            numeric.artist_webpage_url().unwrap().as_str(),
            "https://vk.ru/artist/1234567890123456"
        );
        for artist in [
            Value::Null,
            json!({"name":"Artist"}),
            json!({"id":"0"}),
            json!({"id":"../wrong"}),
            json!({"domain":"https://evil.test"}),
            json!({"domain":"bad?secret=hash"}),
            json!({"domain":"x".repeat(129)}),
        ] {
            let track = parse_track(&artist_row(1, artist), &scope).unwrap();
            assert!(track.artist_webpage_url().is_none());
        }
        assert!(
            VkTrack::fixture_with_artist(VkAudioId::new(-1, 2).unwrap(), "T", "A")
                .artist_webpage_url()
                .is_some()
        );
    }

    #[test]
    fn artist_pages_scope_and_bound_opaque_pagination_tokens() {
        let scope = Arc::new(());
        let section = artist_section(&artist_initialization()).unwrap();
        let page = artist_catalogue_page(
            &artist_catalogue(1, "private-next-token"),
            &section,
            None,
            &scope,
        )
        .unwrap();
        assert_eq!(page.tracks.len(), 1);
        let cursor = page.next.unwrap();
        assert!(cursor.validate_scope(&Arc::new(())).is_err());
        assert!(!format!("{cursor:?}").contains("private-artist-section"));
        let response = artist_catalogue(2, "private-next-token");
        assert!(next_artist_page(&response, &cursor, &scope).is_err());
        let response = artist_catalogue(2, "");
        let last = next_artist_page(&response, &cursor, &scope).unwrap();
        assert_eq!(last.tracks[0].id.audio_id(), 2);
        assert!(last.next.is_none());
        assert!(
            artist_catalogue_page(
                &artist_catalogue(1, &"x".repeat(MAX_TOKEN_BYTES + 1)),
                &section,
                None,
                &scope
            )
            .is_err()
        );
        let mut wrong = artist_catalogue(2, "");
        wrong["payload"][1][1]["sectionId"] = json!("other-private-section");
        assert!(next_artist_page(&wrong, &cursor, &scope).is_err());
        let mut ambiguous = artist_catalogue(2, "next-one");
        ambiguous["payload"][1][1]["nextFrom"] = json!("next-two");
        assert!(next_artist_page(&ambiguous, &cursor, &scope).is_err());
    }

    /// Artist initialization is parsed as one literal, never executed or guessed from text.
    #[test]
    fn artist_initialization_requires_one_matching_literal_configuration() {
        let response = artist_initialization();
        let original = response.pointer("/payload/1/2").unwrap().as_str().unwrap();
        for script in [
            "new AudioPage(geByClass1('x',document.getElementById('y')), evaluateThis());"
                .to_owned(),
            original.replace(
                "\"initSection\":\"artist_block\"",
                "\"initSection\":\"other\"",
            ),
            original.replace("\"artist_block\":{", "\"other\":{"),
            format!("{original}{original}"),
            "const unrelated = {\"sectionId\":\"private-artist-section\"};".to_owned(),
            original.replace("private-artist-section", &"x".repeat(MAX_TOKEN_BYTES + 1)),
            original.replace("); trailingCode();", ", evaluateThis());"),
        ] {
            let mut value = response.clone();
            value["payload"][1][2] = json!(script);
            assert!(artist_section(&value).is_err());
        }
        let mut absent = response;
        absent["payload"][1][2] = Value::Null;
        assert!(artist_section(&absent).is_err());
    }

    #[test]
    fn album_navigation_exposes_only_public_identity_and_scoped_advancing_pages() {
        let scope = Arc::new(());
        let track = parse_track(
            &album_row(1, json!([-2000123, 456, "private-album-hash"])),
            &scope,
        )
        .unwrap();
        assert_eq!(
            track.album_webpage_url().unwrap().as_str(),
            "https://vk.ru/music/album/-2000123_456"
        );
        assert!(
            parse_track(&row(2), &scope)
                .unwrap()
                .album_webpage_url()
                .is_none()
        );
        let reference = track.album_reference(&scope).unwrap().unwrap();
        let response = json!({"payload":[0,[{"ownerId":-2000123,"id":456,"title":"Album &amp; title","list":[album_row(2,json!([-2000123,456,"private-album-hash"]))],"hasMore":true,"nextOffset":100}]]});
        let page = album_page(&response, reference, 0, &scope).unwrap();
        assert_eq!(page.tracks[0].album_title.as_deref(), Some("Album & title"));
        let cursor = page.next.unwrap();
        assert!(cursor.validate_scope(&Arc::new(())).is_err());
        assert!(!format!("{cursor:?}").contains("private-album-hash"));
        assert!(album_page(&response, reference, 100, &scope).is_err());
        let mut response = response;
        response["payload"][1][0]["ownerId"] = json!(-999);
        assert!(album_page(&response, reference, 0, &scope).is_err());
    }

    #[test]
    fn album_references_are_optional_validated_and_never_printed() {
        let scope = Arc::new(());
        let row = album_row(1, json!([-2000123, 456, "private-album-hash"]));
        let track = parse_track(&row, &scope).unwrap();
        assert!(track.has_album());
        assert!(track.album_title.is_none());
        assert!(!format!("{track:?}").contains("private-album-hash"));
        assert!(track.album_reference(&Arc::new(())).is_err());
        let album = track.album_reference(&scope).unwrap().unwrap();
        assert_eq!(album.owner, -2000123);
        assert_eq!(album.id, 456);
        assert_eq!(album.access_hash, "private-album-hash");
        assert!(!format!("{album:?}").contains("private-album-hash"));
    }

    #[test]
    fn invalid_album_references_do_not_discard_playable_tracks() {
        let scope = Arc::new(());
        for reference in [
            Value::Null,
            json!("Album title"),
            json!([]),
            json!([-2000123, 456]),
            json!([0, 456, "hash"]),
            json!([i64::MIN, 456, "hash"]),
            json!([-2000123, 0, "hash"]),
            json!([-2000123, -1, "hash"]),
            json!([-2000123, u64::MAX, "hash"]),
            json!([-2000123, 456, ""]),
            json!([-2000123, 456, "hash\nheader"]),
            json!([-2000123, 456, "x".repeat(257)]),
            json!([-2000123, 456, "https://other.test"]),
            json!([-2000123, 456, "hash", "unexpected"]),
        ] {
            let track = parse_track(&album_row(1, reference), &scope).unwrap();
            assert!(!track.has_album());
            assert!(track.album_title.is_none());
        }
        assert!(!parse_track(&row(1), &scope).unwrap().has_album());
        assert!(
            VkTrack::fixture(VkAudioId::new(-1, 2).unwrap(), "T", "A")
                .album_title
                .is_none()
        );
        let fixture = VkTrack::fixture_with_album(VkAudioId::new(-1, 2).unwrap(), "T", "A");
        assert!(fixture.has_album());
        assert!(fixture.album_title.is_none());
    }

    pub(in crate::providers::vk) fn search_fixture() -> Value {
        json!({"payload":[0,["<div class='CatalogBlock CatalogBlock__search_global_audios' data-id='global-fixture'></div>", {
            "playlists":[
                {"id":"mine", "type":"playlist", "list":[row(1)], "hasMore":false},
                {"id":"global-fixture", "ownerId":123, "type":"search", "list":[row(2)], "hasMore":true, "nextOffset":"private:offset+token"},
                {"id":"found-in-text", "type":"search", "list":[row(3)], "hasMore":false}
            ], "playlist":{"id":"found-in-text", "list":[row(3)]}
        }]]})
    }

    /// VK can advertise a large cover before its compact rendition.
    #[test]
    fn artwork_selects_largest_advertised_size_independent_of_url_order() {
        let scope = Arc::new(());
        let large = "https://sun1.userapi.com/cover.jpg?size=300x300&token=fixture";
        let small = "https://sun1.userapi.com/cover.jpg?size=160x160&token=fixture";
        for urls in [format!("{large},{small}"), format!("{small},{large}")] {
            let mut item = row(1);
            item[14] = json!(urls);
            let track = parse_track(&item, &scope).unwrap();
            assert_eq!(track.artwork_url.as_ref().unwrap().as_str(), large);
            assert_eq!(track.artwork_dimensions, Some((300, 300)));
        }
    }

    /// Unknown dimensions must not invent sizes or admit untrusted artwork hosts.
    #[test]
    fn artwork_size_validation_preserves_safe_fallbacks() {
        for size in [
            "0x300",
            "300x0",
            "-1x300",
            "+300x300",
            "300",
            "300x300x300",
            "999999999999x1",
            "4097x300",
            "300x300&size=1200x1200",
        ] {
            let url =
                Url::parse(&format!("https://sun1.userapi.com/cover.jpg?size={size}")).unwrap();
            assert_eq!(artwork_dimensions(&url), None);
        }
        let mut item = row(1);
        item[14] = json!(
            "https://sun1.userapi.com/cover.jpg?size=300x300,https://evil.test/larger.jpg?size=1200x1200,https://sun1.userapi.com/unknown.jpg"
        );
        let track = parse_track(&item, &Arc::new(())).unwrap();
        assert_eq!(track.artwork_url.as_ref().unwrap().path(), "/cover.jpg");
        assert_eq!(track.artwork_dimensions, Some((300, 300)));
        let unknown = parse_track(&row(1), &Arc::new(())).unwrap();
        assert_eq!(unknown.artwork_url.as_ref().unwrap().path(), "/large.jpg");
        assert_eq!(unknown.artwork_dimensions, None);
        item[14] = json!(
            "https://sun1.userapi.com/full.jpg?size=1200x1200,https://sun1.userapi.com/preview.jpg?size=300x300"
        );
        let full = parse_track(&item, &Arc::new(())).unwrap();
        assert_eq!(full.artwork_url.as_ref().unwrap().path(), "/full.jpg");
        assert_eq!(full.artwork_dimensions, Some((1200, 1200)));
    }

    #[test]
    fn selects_global_search_block_not_first_or_last_playlist() {
        let scope = Arc::new(());
        let page = search_page(&search_fixture(), &scope).unwrap();
        assert_eq!(page.tracks.len(), 1);
        let track = &page.tracks[0];
        assert_eq!(track.id.audio_id(), 2);
        assert_eq!(track.title, "Title & \"fixture\"");
        assert_eq!(track.artist, "Artist & band");
        assert_eq!(track.artwork_url.as_ref().unwrap().path(), "/large.jpg");
        let debug = format!("{page:?}");
        for private in ["privateaction", "privatelocator", "private:offset+token"] {
            assert!(!debug.contains(private));
        }
        assert!(track.reload_key(&Arc::new(())).is_err());
        assert!(page.next.unwrap().validate_scope(&Arc::new(())).is_err());
    }

    #[test]
    fn preserves_opaque_search_offsets_and_rejects_nonadvancing_pages() {
        let scope = Arc::new(());
        let cursor = search_page(&search_fixture(), &scope)
            .unwrap()
            .next
            .unwrap();
        let mut response = json!({"payload":[0,[{"list":[row(4)], "hasMore":true, "nextOffset":"private:offset+token"}]]});
        assert!(next_search_page(&response, &cursor, &scope).is_err());
        response["payload"][1][0]["nextOffset"] = json!("private:next/offset");
        assert!(
            next_search_page(&response, &cursor, &scope)
                .unwrap()
                .next
                .is_some()
        );
        response["payload"][1][0]["hasMore"] = json!(false);
        assert!(
            next_search_page(&response, &cursor, &scope)
                .unwrap()
                .next
                .is_none()
        );
    }

    #[test]
    fn profile_pages_skip_restricted_or_invalid_rows_and_deduplicate() {
        let scope = Arc::new(());
        let mut restricted = row(2);
        restricted[13] = json!("");
        let response = json!({"data":[{"list":[row(1),row(1),restricted,null], "hasMore":1,"nextOffset":100}]});
        let page = profile_page(&response, 123, 0, &scope).unwrap();
        assert_eq!(page.tracks.len(), 1);
        assert!(page.next.is_some());
        assert!(profile_page(&response, 123, 100, &scope).is_err());
    }

    /// Desktop profile pages require a successful envelope and the requested personal playlist.
    #[test]
    fn desktop_profile_pages_validate_identity_and_reuse_numeric_pagination() {
        let scope = Arc::new(());
        let response = json!({"payload":[0,[{"ownerId":123,"id":-1,"list":[row(1),row(1)],"hasMore":1,"nextOffset":100}]]});
        let page = desktop_profile_page(&response, 123, 0, &scope).unwrap();
        assert_eq!(page.tracks.len(), 1);
        let cursor = page.next.unwrap();
        assert!(cursor.validate_scope(&scope).is_ok());
        assert!(cursor.validate_scope(&Arc::new(())).is_err());
        assert!(matches!(
            cursor.kind,
            CursorKind::Profile {
                owner: 123,
                offset: 100
            }
        ));
        assert!(desktop_profile_page(&response, 123, 100, &scope).is_err());
        assert!(desktop_profile_page(&response, 456, 0, &scope).is_err());
        assert!(desktop_profile_page(&response, 0, 0, &scope).is_err());
        for (field, invalid) in [
            ("ownerId", json!(456)),
            ("ownerId", json!("123")),
            ("ownerId", Value::Null),
            ("id", json!(0)),
            ("id", json!("-1")),
            ("id", Value::Null),
        ] {
            let mut changed = response.clone();
            changed["payload"][1][0][field] = invalid;
            assert!(desktop_profile_page(&changed, 123, 0, &scope).is_err());
        }
        for status in [json!(1), json!("0"), Value::Null] {
            let mut changed = response.clone();
            changed["payload"][0] = status;
            assert!(desktop_profile_page(&changed, 123, 0, &scope).is_err());
        }
        let mut terminal = response;
        terminal["payload"][1][0]["hasMore"] = json!(0);
        terminal["payload"][1][0]["nextOffset"] = json!(0);
        assert!(
            desktop_profile_page(&terminal, 123, 100, &scope)
                .unwrap()
                .next
                .is_none()
        );
        assert!(
            desktop_profile_page(
                &json!({"data":[{"list":[row(1)],"hasMore":false}]}),
                123,
                0,
                &scope
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_ambiguous_or_missing_global_identity_instead_of_guessing() {
        let scope = Arc::new(());
        let mut value = search_fixture();
        value["payload"][1][0] = json!("<div class='other' data-id='global-fixture'></div>");
        assert!(search_page(&value, &scope).is_err());
        let mut value = search_fixture();
        value["payload"][1][1]["playlists"][2]["id"] = json!("global-fixture");
        assert!(search_page(&value, &scope).is_err());
        assert!(
            search_page(&json!({"payload":[0,["",{"playlists":[]}]]}), &scope)
                .unwrap()
                .tracks
                .is_empty()
        );
    }

    #[test]
    fn desktop_reload_requires_success_and_the_exact_first_row() {
        let id = VkAudioId::new(-123, 2).unwrap();
        let mut selected = row(2);
        selected[2] = json!("https://psv4.vkuseraudio.ru/fixture/index.m3u8");
        let response = json!({"payload":[0,[[selected.clone()]]]});
        assert_eq!(
            reloaded_desktop_url(&response, id).unwrap(),
            "https://psv4.vkuseraudio.ru/fixture/index.m3u8"
        );
        assert!(reloaded_url(&response, id).is_err());
        for response in [
            json!({"payload":[1,[[selected.clone()]]]}),
            json!({"payload":["0",[[selected.clone()]]]}),
            json!({"payload":[null,[[selected.clone()]]]}),
            json!({"payload":[0,[[row(1), selected.clone()]]]}),
            json!({"payload":[0,[selected.clone()]]}),
            json!({"data":[[selected.clone()]]}),
        ] {
            assert!(reloaded_desktop_url(&response, id).is_err());
        }
        selected[2] = json!("x".repeat(16 * 1024 + 1));
        assert!(reloaded_desktop_url(&json!({"payload":[0,[[selected]]]}), id).is_err());
    }

    #[test]
    fn bounds_remote_text_and_rejects_wrong_reloaded_identity() {
        let scope = Arc::new(());
        let mut value = row(1);
        value[3] = json!("x".repeat(MAX_LABEL_BYTES + 1));
        assert!(parse_track(&value, &scope).is_none());
        value[3] = json!("<b>title</b>&#27;[31m");
        assert!(
            !parse_track(&value, &scope)
                .unwrap()
                .title
                .chars()
                .any(char::is_control)
        );
        let response = json!({"data":[[row(1)]]});
        assert!(reloaded_url(&response, VkAudioId::new(-123, 2).unwrap()).is_err());
        assert!(artwork_url("https://sun1.userapi.com.evil.test/fixture").is_none());
    }

    #[test]
    fn display_fixture_exposes_only_the_canonical_track_page() {
        let track = VkTrack::fixture(VkAudioId::new(-123, 456).unwrap(), "Title", "Artist");
        assert_eq!(track.webpage_url().as_str(), "https://vk.ru/audio-123_456");
        assert!(!format!("{track:?}").contains("fixture-action"));
    }
}
