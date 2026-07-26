//! Search adapters for public tracker-module archives.
//!
//! The adapters in this module deliberately use bounded, blocking requests.
//! They are intended to run on Youta's provider worker, never on the terminal
//! rendering thread. Each site's HTML parser is isolated because these public
//! archives do not all provide a stable machine-readable API.
//!
//! [Modland](https://ftp.modland.com/pub/modules/) is handled differently from
//! the search sites: a query only inspects an on-demand local catalogue.
//! [`TrackerArchiveHub::browse_modland_directory`] fetches exactly one
//! directory for a caller-controlled background indexing operation; it never
//! performs an unbounded recursive crawl.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ureq::ResponseExt;
use url::Url;

use super::{DEFAULT_REQUEST_TIMEOUT, ProviderError, provider_agent};

const MAX_QUERY_BYTES: usize = 256;
const MAX_PAGE: u32 = 10_000;
const MAX_RESULTS_PER_PAGE: usize = 100;
const MAX_MODLAND_LISTING_ENTRIES: usize = 10_000;
const DEFAULT_MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
const MAX_CONFIGURED_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MODLAND_PAGE_SIZE: usize = 50;
const MAX_MODLAND_CATALOGUE_ENTRIES: usize = 100_000;

const SCENE_BASE: &str = "https://files.scene.org/";
const AMINET_BASE: &str = "https://aminet.net/";
const MIRSOFT_BASE: &str = "http://www.mirsoft.info/";
const AMP_BASE: &str = "https://amp.dascene.net/";
const DEMOZOO_BASE: &str = "https://demozoo.org/";
const MODULES_PL_BASE: &str = "https://www.modules.pl/";
const MODLAND_BASE: &str = "https://ftp.modland.com/pub/modules/";

/// One archive shown in the `MOD/tracker music` source picker.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrackerArchiveSource {
    /// The scene.org public file archive.
    SceneOrg,
    /// The `mods` tree on Aminet.
    Aminet,
    /// Mirsoft's World of Game MODs catalogue.
    Mirsoft,
    /// The Modland HTTPS directory tree.
    Modland,
    /// Amiga Music Preservation.
    AmigaMusicPreservation,
    /// Demozoo's live metadata search.
    Demozoo,
    /// The modules.pl community archive.
    ModulesPl,
}

impl TrackerArchiveSource {
    /// Sources implemented by this module, in the default UI order.
    pub const ALL: [Self; 7] = [
        Self::SceneOrg,
        Self::Aminet,
        Self::Mirsoft,
        Self::Modland,
        Self::AmigaMusicPreservation,
        Self::Demozoo,
        Self::ModulesPl,
    ];

    /// Returns presentation and capability metadata for this source.
    #[must_use]
    pub const fn descriptor(self) -> TrackerSourceDescriptor {
        match self {
            Self::SceneOrg => TrackerSourceDescriptor {
                id: "scene-org",
                display_name: "scene.org",
                homepage: SCENE_BASE,
                enabled_by_default: true,
                insecure_http: false,
                pagination: true,
                search_mode: TrackerSearchMode::Remote,
            },
            Self::Aminet => TrackerSourceDescriptor {
                id: "aminet",
                display_name: "Aminet mods",
                homepage: "https://aminet.net/tree?path=mods",
                enabled_by_default: true,
                insecure_http: false,
                pagination: false,
                search_mode: TrackerSearchMode::Remote,
            },
            Self::Mirsoft => TrackerSourceDescriptor {
                id: "mirsoft",
                display_name: "Mirsoft Game MODs",
                homepage: "http://www.mirsoft.info/gamemods.php",
                enabled_by_default: true,
                insecure_http: true,
                pagination: true,
                search_mode: TrackerSearchMode::Remote,
            },
            Self::Modland => TrackerSourceDescriptor {
                id: "modland",
                display_name: "Modland",
                homepage: MODLAND_BASE,
                enabled_by_default: true,
                insecure_http: false,
                pagination: true,
                search_mode: TrackerSearchMode::LocalCatalogue,
            },
            Self::AmigaMusicPreservation => TrackerSourceDescriptor {
                id: "amiga-music-preservation",
                display_name: "Amiga Music Preservation",
                homepage: AMP_BASE,
                enabled_by_default: true,
                insecure_http: false,
                pagination: true,
                search_mode: TrackerSearchMode::Remote,
            },
            Self::Demozoo => TrackerSourceDescriptor {
                id: "demozoo",
                display_name: "Demozoo",
                homepage: "https://demozoo.org/music/?production_type=29",
                enabled_by_default: true,
                insecure_http: false,
                pagination: false,
                search_mode: TrackerSearchMode::MetadataOnly,
            },
            Self::ModulesPl => TrackerSourceDescriptor {
                id: "modules-pl",
                display_name: "modules.pl",
                homepage: MODULES_PL_BASE,
                enabled_by_default: true,
                insecure_http: false,
                pagination: false,
                search_mode: TrackerSearchMode::Remote,
            },
        }
    }
}

/// How a tracker source fulfils text searches.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrackerSearchMode {
    /// Send a bounded request to the source's public search endpoint.
    Remote,
    /// Query only a catalogue populated by explicit directory browsing.
    LocalCatalogue,
    /// Return links and descriptive metadata, but no playable payload.
    MetadataOnly,
}

/// Static metadata used to render a source selector.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrackerSourceDescriptor {
    /// Stable configuration identifier.
    pub id: &'static str,
    /// Human-readable UI label.
    pub display_name: &'static str,
    /// Public source homepage.
    pub homepage: &'static str,
    /// Whether a default build enables the source.
    pub enabled_by_default: bool,
    /// Whether the source itself requires unencrypted HTTP.
    pub insecure_http: bool,
    /// Whether a remote or local search can expose subsequent pages.
    pub pagination: bool,
    /// Search implementation used for this source.
    pub search_mode: TrackerSearchMode,
}

/// A validated text query and one-based result page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackerSearchRequest {
    /// User-entered title, author, game, or file-name text.
    pub query: String,
    /// One-based page number.
    pub page: u32,
}

impl TrackerSearchRequest {
    /// Creates a first-page search.
    #[must_use]
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            page: 1,
        }
    }

    fn validate(&self) -> Result<(), ProviderError> {
        let query = self.query.trim();
        if query.is_empty() {
            return Err(ProviderError::InvalidRequest(
                "tracker search query cannot be empty".to_owned(),
            ));
        }
        if query.len() > MAX_QUERY_BYTES {
            return Err(ProviderError::InvalidRequest(format!(
                "tracker search query cannot exceed {MAX_QUERY_BYTES} bytes"
            )));
        }
        if query.chars().any(char::is_control) {
            return Err(ProviderError::InvalidRequest(
                "tracker search query cannot contain control characters".to_owned(),
            ));
        }
        if !(1..=MAX_PAGE).contains(&self.page) {
            return Err(ProviderError::InvalidRequest(format!(
                "tracker search page must be between 1 and {MAX_PAGE}"
            )));
        }
        Ok(())
    }
}

/// What must happen before a returned item can be played.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrackerMediaAccess {
    /// The URL identifies a known tracker-module format.
    DirectModule,
    /// The payload is an archive that requires bounded inspection/extraction.
    ArchiveNeedsInspection,
    /// The result is descriptive metadata without a verified media payload.
    MetadataOnly,
    /// The result is a browsable Modland directory.
    Directory,
}

/// A normalized result from one tracker archive.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TrackerSearchResult {
    /// Source that produced the result.
    pub source: TrackerArchiveSource,
    /// Source-local stable identifier or path.
    pub source_id: String,
    /// Module, archive, production, or directory title.
    pub title: String,
    /// Composer or uploader, when exposed by the source.
    pub artist: Option<String>,
    /// Tracker or archive format, without a leading dot.
    pub format: Option<String>,
    /// Payload size when exposed.
    pub size_bytes: Option<u64>,
    /// Browser-facing information page.
    pub webpage_url: Url,
    /// Download locator, when the source exposes one.
    pub download_url: Option<Url>,
    /// Whether direct replay, archive inspection, or metadata enrichment is
    /// appropriate.
    pub access: TrackerMediaAccess,
    /// `true` when the result or payload crosses unencrypted HTTP.
    pub insecure_transport: bool,
}

impl TrackerSearchResult {
    /// Returns a URL suitable for direct replay only when its module format is
    /// known.
    #[must_use]
    pub fn direct_play_url(&self) -> Option<&Url> {
        (self.access == TrackerMediaAccess::DirectModule)
            .then_some(self.download_url.as_ref())
            .flatten()
    }
}

/// One normalized page of tracker search results.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackerSearchPage {
    /// Source searched.
    pub source: TrackerArchiveSource,
    /// One-based page number.
    pub page: u32,
    /// Bounded result set.
    pub items: Vec<TrackerSearchResult>,
    /// Page to load lazily, when the source can plausibly have more results.
    pub next_page: Option<u32>,
    /// Non-error status text, such as an empty Modland local catalogue.
    pub notice: Option<String>,
}

/// One non-recursive entry returned while browsing Modland.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModlandDirectoryEntry {
    /// Path relative to `https://ftp.modland.com/pub/modules/`.
    pub relative_path: String,
    /// Display name decoded from the directory listing.
    pub name: String,
    /// Whether this entry is another directory.
    pub directory: bool,
    /// File size when the listing supplies one.
    pub size_bytes: Option<u64>,
    /// Validated descendant URL.
    pub url: Url,
}

/// Bounded, on-demand Modland catalogue.
///
/// A caller populates this catalogue from explicit calls to
/// [`TrackerArchiveHub::browse_modland_directory`], then persists the entries
/// in Youta's config directory. Search never starts a recursive network crawl.
#[derive(Clone, Debug, Default)]
pub struct ModlandCatalogue {
    entries: BTreeMap<String, ModlandDirectoryEntry>,
}

impl ModlandCatalogue {
    /// Returns the number of cached directory entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Reports whether no Modland directory has been cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Adds one non-recursive listing, replacing duplicate paths.
    ///
    /// # Errors
    ///
    /// Returns an error if an entry is outside the Modland module tree or the
    /// catalogue would exceed its memory bound.
    pub fn update(
        &mut self,
        listing: impl IntoIterator<Item = ModlandDirectoryEntry>,
    ) -> Result<(), ProviderError> {
        for entry in listing {
            validate_modland_relative_path(&entry.relative_path)?;
            validate_modland_url(&entry.url)?;
            if !self.entries.contains_key(&entry.relative_path)
                && self.entries.len() >= MAX_MODLAND_CATALOGUE_ENTRIES
            {
                return Err(ProviderError::InvalidRequest(format!(
                    "Modland catalogue cannot exceed {MAX_MODLAND_CATALOGUE_ENTRIES} entries"
                )));
            }
            self.entries.insert(entry.relative_path.clone(), entry);
        }
        Ok(())
    }

    fn search(&self, request: &TrackerSearchRequest) -> Result<TrackerSearchPage, ProviderError> {
        request.validate()?;
        let query = request.query.trim();
        let skip = usize::try_from(request.page.saturating_sub(1))
            .unwrap_or(usize::MAX)
            .saturating_mul(MODLAND_PAGE_SIZE);
        let mut matched = 0;
        let mut items = Vec::with_capacity(MODLAND_PAGE_SIZE);
        for entry in self.entries.values().filter(|entry| {
            contains_ascii_case_insensitive(&entry.name, query)
                || contains_ascii_case_insensitive(&entry.relative_path, query)
        }) {
            if matched >= skip && items.len() < MODLAND_PAGE_SIZE {
                items.push(modland_result(entry));
            }
            matched += 1;
        }
        let next_page =
            (skip.saturating_add(items.len()) < matched).then(|| request.page.saturating_add(1));
        Ok(TrackerSearchPage {
            source: TrackerArchiveSource::Modland,
            page: request.page,
            items,
            next_page,
            notice: self.is_empty().then(|| {
                "Browse one or more Modland format directories to build the local catalogue"
                    .to_owned()
            }),
        })
    }
}

/// Blocking hub for tracker archives that do not need a private API key.
///
/// The Mod Archive's credentialed XML API remains in
/// `providers::modarchive`; callers can merge those results with this hub.
pub struct TrackerArchiveHub {
    agent: ureq::Agent,
    max_response_bytes: usize,
    allow_insecure_http: bool,
    modland: ModlandCatalogue,
}

impl TrackerArchiveHub {
    /// Creates a hub with conservative time and response bounds.
    ///
    /// Set `allow_insecure_http` from
    /// `Config::providers.allow_insecure_http`. It defaults to `true` in
    /// Youta so Mirsoft remains enabled, but this hub never accepts or sends
    /// credentials to Mirsoft.
    #[must_use]
    pub fn new(allow_insecure_http: bool) -> Self {
        Self {
            agent: provider_agent(DEFAULT_REQUEST_TIMEOUT),
            max_response_bytes: DEFAULT_MAX_HTML_BYTES,
            allow_insecure_http,
            modland: ModlandCatalogue::default(),
        }
    }

    /// Creates a hub with explicit request limits.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero timeout or an excessive response bound.
    pub fn with_options(
        allow_insecure_http: bool,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<Self, ProviderError> {
        if timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "tracker provider timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=MAX_CONFIGURED_RESPONSE_BYTES).contains(&max_response_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "tracker response limit must be between 1 and \
                 {MAX_CONFIGURED_RESPONSE_BYTES} bytes"
            )));
        }
        Ok(Self {
            agent: provider_agent(timeout),
            max_response_bytes,
            allow_insecure_http,
            modland: ModlandCatalogue::default(),
        })
    }

    /// Returns the on-demand Modland catalogue.
    #[must_use]
    pub const fn modland_catalogue(&self) -> &ModlandCatalogue {
        &self.modland
    }

    /// Returns the mutable on-demand Modland catalogue.
    pub const fn modland_catalogue_mut(&mut self) -> &mut ModlandCatalogue {
        &mut self.modland
    }

    /// Searches one source without starting parallel or recursive work.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input, a disabled HTTP-only Mirsoft
    /// request, network failure, an oversized response, or malformed source
    /// data.
    pub fn search(
        &self,
        source: TrackerArchiveSource,
        request: &TrackerSearchRequest,
    ) -> Result<TrackerSearchPage, ProviderError> {
        request.validate()?;
        match source {
            TrackerArchiveSource::SceneOrg => self.search_scene(request),
            TrackerArchiveSource::Aminet => self.search_aminet(request),
            TrackerArchiveSource::Mirsoft => self.search_mirsoft(request),
            TrackerArchiveSource::Modland => self.modland.search(request),
            TrackerArchiveSource::AmigaMusicPreservation => self.search_amp(request),
            TrackerArchiveSource::Demozoo => self.search_demozoo(request),
            TrackerArchiveSource::ModulesPl => self.search_modules_pl(request),
        }
    }

    /// Fetches one Modland directory without descending into child folders.
    ///
    /// The caller decides which directories to visit and when to persist the
    /// returned entries. An empty string browses the root module directory.
    ///
    /// # Errors
    ///
    /// Returns an error for traversal-like input, network failure, an
    /// oversized listing, or a link that escapes the Modland module tree.
    pub fn browse_modland_directory(
        &self,
        relative_directory: &str,
    ) -> Result<Vec<ModlandDirectoryEntry>, ProviderError> {
        let relative_directory = normalize_modland_directory(relative_directory)?;
        let base = parsed_base(MODLAND_BASE);
        let url = if relative_directory.is_empty() {
            base.clone()
        } else {
            base.join(&relative_directory).map_err(|error| {
                ProviderError::InvalidRequest(format!("invalid Modland directory path: {error}"))
            })?
        };
        validate_modland_url(&url)?;
        let html = self.fetch_text(&base, &url)?;
        parse_modland_listing(&html, &url)
    }

    fn search_scene(
        &self,
        request: &TrackerSearchRequest,
    ) -> Result<TrackerSearchPage, ProviderError> {
        let base = parsed_base(SCENE_BASE);
        let mut url = base.join("search/").expect("compile-time Scene path");
        url.query_pairs_mut()
            .append_pair("q", request.query.trim())
            .append_pair("page", &request.page.to_string());
        let html = self.fetch_text(&base, &url)?;
        Ok(page_with_lazy_next(
            TrackerArchiveSource::SceneOrg,
            request.page,
            parse_scene(&html, &base),
        ))
    }

    fn search_aminet(
        &self,
        request: &TrackerSearchRequest,
    ) -> Result<TrackerSearchPage, ProviderError> {
        require_first_page(TrackerArchiveSource::Aminet, request.page)?;
        let base = parsed_base(AMINET_BASE);
        let mut url = base.join("search").expect("compile-time Aminet path");
        url.query_pairs_mut()
            .append_pair("path[]", "mods")
            .append_pair("query", request.query.trim())
            .append_pair("type", "simple");
        let html = self.fetch_text(&base, &url)?;
        Ok(first_page(
            TrackerArchiveSource::Aminet,
            parse_aminet(&html, &base),
        ))
    }

    fn search_mirsoft(
        &self,
        request: &TrackerSearchRequest,
    ) -> Result<TrackerSearchPage, ProviderError> {
        if !self.allow_insecure_http {
            return Err(ProviderError::InvalidRequest(
                "Mirsoft requires providers.allow_insecure_http=true".to_owned(),
            ));
        }
        let base = parsed_base(MIRSOFT_BASE);
        let mut url = base
            .join("gamemods-archive.php")
            .expect("compile-time Mirsoft path");
        let offset = request
            .page
            .saturating_sub(1)
            .checked_mul(50)
            .ok_or_else(|| {
                ProviderError::InvalidRequest("Mirsoft page offset overflowed".to_owned())
            })?;
        url.query_pairs_mut()
            .append_pair("hladaj", request.query.trim())
            .append_pair("start", &offset.to_string())
            .append_pair("limit", "50")
            .append_pair("selection", "1");
        let html = self.fetch_text(&base, &url)?;
        let items = parse_mirsoft(&html, &base);
        Ok(page_with_counted_next(
            TrackerArchiveSource::Mirsoft,
            request.page,
            items,
            50,
        ))
    }

    fn search_amp(
        &self,
        request: &TrackerSearchRequest,
    ) -> Result<TrackerSearchPage, ProviderError> {
        let base = parsed_base(AMP_BASE);
        let mut url = base.join("newresult.php").expect("compile-time AMP path");
        let position = request
            .page
            .saturating_sub(1)
            .checked_mul(50)
            .ok_or_else(|| {
                ProviderError::InvalidRequest("AMP page offset overflowed".to_owned())
            })?;
        url.query_pairs_mut()
            .append_pair("request", "module")
            .append_pair("search", request.query.trim())
            .append_pair("position", &position.to_string());
        let html = self.fetch_text(&base, &url)?;
        let items = parse_amp(&html, &base);
        Ok(page_with_counted_next(
            TrackerArchiveSource::AmigaMusicPreservation,
            request.page,
            items,
            50,
        ))
    }

    fn search_demozoo(
        &self,
        request: &TrackerSearchRequest,
    ) -> Result<TrackerSearchPage, ProviderError> {
        require_first_page(TrackerArchiveSource::Demozoo, request.page)?;
        let base = parsed_base(DEMOZOO_BASE);
        let mut url = base
            .join("search/live/")
            .expect("compile-time Demozoo path");
        url.query_pairs_mut().append_pair("q", request.query.trim());
        let bytes = self.fetch_bytes(&base, &url, "application/json")?;
        let raw: Vec<RawDemozooResult> = serde_json::from_slice(&bytes)
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
        Ok(first_page(
            TrackerArchiveSource::Demozoo,
            parse_demozoo(raw, &base),
        ))
    }

    fn search_modules_pl(
        &self,
        request: &TrackerSearchRequest,
    ) -> Result<TrackerSearchPage, ProviderError> {
        require_first_page(TrackerArchiveSource::ModulesPl, request.page)?;
        let base = parsed_base(MODULES_PL_BASE);
        let mut url = base.clone();
        url.query_pairs_mut()
            .append_pair("id", "search")
            .append_pair("q", request.query.trim());
        let html = self.fetch_text(&base, &url)?;
        Ok(first_page(
            TrackerArchiveSource::ModulesPl,
            parse_modules_pl(&html, &base),
        ))
    }

    fn fetch_text(&self, origin: &Url, url: &Url) -> Result<String, ProviderError> {
        let bytes = self.fetch_bytes(origin, url, "text/html,application/xhtml+xml")?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn fetch_bytes(&self, origin: &Url, url: &Url, accept: &str) -> Result<Vec<u8>, ProviderError> {
        validate_same_origin(origin, url)?;
        let mut response = self
            .agent
            .get(url.as_str())
            .header("Accept", accept)
            .call()
            .map_err(map_ureq_error)?;
        let final_url = Url::parse(&response.get_uri().to_string())
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
        validate_same_origin(origin, &final_url)?;
        if response
            .body()
            .content_length()
            .is_some_and(|length| length > self.max_response_bytes as u64)
        {
            return Err(ProviderError::ResponseTooLarge {
                limit: self.max_response_bytes,
            });
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(u64::try_from(self.max_response_bytes.saturating_add(1)).unwrap_or(u64::MAX))
            .read_to_vec()
            .map_err(|error| match error {
                ureq::Error::BodyExceedsLimit(_) => ProviderError::ResponseTooLarge {
                    limit: self.max_response_bytes,
                },
                other => ProviderError::Transport(other.to_string()),
            })?;
        if bytes.len() > self.max_response_bytes {
            return Err(ProviderError::ResponseTooLarge {
                limit: self.max_response_bytes,
            });
        }
        Ok(bytes)
    }
}

fn parse_scene(html: &str, base: &Url) -> Vec<TrackerSearchResult> {
    let mut results = Vec::new();
    for block in element_blocks(html, "li", MAX_RESULTS_PER_PAGE.saturating_mul(4)) {
        let opening = opening_tag(block).unwrap_or_default();
        if !attribute_value(opening, "class")
            .is_some_and(|class| class.split_ascii_whitespace().any(|value| value == "file"))
        {
            continue;
        }
        let Some(anchor) = extract_anchors(block, 1).into_iter().next() else {
            continue;
        };
        let Some(webpage_url) = same_origin_url(base, &anchor.href) else {
            continue;
        };
        let title = file_name_from_url(&webpage_url)
            .map(decode_percent_for_display)
            .or_else(|| text_by_class(block, "span", "filename"))
            .unwrap_or(anchor.text)
            .trim()
            .to_owned();
        if title.is_empty() {
            continue;
        }
        let format = extension_from_url(&webpage_url)
            .or_else(|| extension_from_name(&title))
            .map(str::to_ascii_lowercase);
        let access = media_access(format.as_deref());
        if access == TrackerMediaAccess::MetadataOnly {
            continue;
        }
        let mut download_url = webpage_url.clone();
        if let Some(path) = webpage_url.path().strip_prefix("/view/") {
            download_url.set_path(&format!("/get/{path}"));
        }
        let size_bytes = text_by_class(block, "span", "filesize")
            .as_deref()
            .and_then(parse_human_size);
        results.push(TrackerSearchResult {
            source: TrackerArchiveSource::SceneOrg,
            source_id: webpage_url.path().to_owned(),
            title,
            artist: None,
            format,
            size_bytes,
            webpage_url,
            download_url: Some(download_url),
            access,
            insecure_transport: false,
        });
        if results.len() >= MAX_RESULTS_PER_PAGE {
            break;
        }
    }
    results
}

fn parse_aminet(html: &str, base: &Url) -> Vec<TrackerSearchResult> {
    let mut results = Vec::new();
    for block in elements_with_class(html, "tr", "pkg_row", MAX_RESULTS_PER_PAGE) {
        let anchors = extract_anchors(block, 16);
        let Some(download) = anchors.iter().find(|anchor| {
            let href = decode_html(&anchor.href);
            href.starts_with("/mods/")
                && media_access(extension_from_name(&href))
                    == TrackerMediaAccess::ArchiveNeedsInspection
        }) else {
            continue;
        };
        let Some(download_url) = same_origin_url(base, &download.href) else {
            continue;
        };
        let webpage_url = anchors
            .iter()
            .find(|anchor| decode_html(&anchor.href).starts_with("/package/mods/"))
            .and_then(|anchor| same_origin_url(base, &anchor.href))
            .unwrap_or_else(|| download_url.clone());
        let title = nonempty_text(&download.text)
            .unwrap_or_else(|| file_name_from_url(&download_url).unwrap_or("Aminet package"))
            .to_owned();
        let cells = extract_cell_text(block, 16);
        let size_bytes = cells.get(4).and_then(|value| parse_human_size(value));
        results.push(TrackerSearchResult {
            source: TrackerArchiveSource::Aminet,
            source_id: download_url.path().to_owned(),
            format: extension_from_url(&download_url).map(str::to_ascii_lowercase),
            title,
            artist: None,
            size_bytes,
            webpage_url,
            download_url: Some(download_url),
            access: TrackerMediaAccess::ArchiveNeedsInspection,
            insecure_transport: false,
        });
        if results.len() >= MAX_RESULTS_PER_PAGE {
            break;
        }
    }
    results
}

fn parse_mirsoft(html: &str, base: &Url) -> Vec<TrackerSearchResult> {
    let mut results = Vec::new();
    for block in blocks_starting_at(html, "<tr><td class=\"blocked", MAX_RESULTS_PER_PAGE) {
        if !contains_ascii_case_insensitive(block, "wogm_download.php?") {
            continue;
        }
        let anchors = extract_anchors(block, 32);
        let Some(title_anchor) = anchors
            .iter()
            .find(|anchor| contains_ascii_case_insensitive(&anchor.href, "gmb/music_info.php"))
        else {
            continue;
        };
        let Some(download_anchor) = anchors
            .iter()
            .find(|anchor| contains_ascii_case_insensitive(&anchor.href, "wogm_download.php?"))
        else {
            continue;
        };
        let Some(webpage_url) = same_origin_url(base, &title_anchor.href) else {
            continue;
        };
        let Some(download_url) = same_origin_url(base, &download_anchor.href) else {
            continue;
        };
        let artist = anchors
            .iter()
            .filter(|anchor| contains_ascii_case_insensitive(&anchor.href, "musician_info.php"))
            .filter_map(|anchor| nonempty_text(&anchor.text))
            .collect::<Vec<_>>()
            .join(", ");
        let cells = extract_cell_text(block, 16);
        results.push(TrackerSearchResult {
            source: TrackerArchiveSource::Mirsoft,
            source_id: webpage_url
                .query()
                .unwrap_or_else(|| webpage_url.path())
                .to_owned(),
            title: title_anchor.text.trim().to_owned(),
            artist: nonempty_text(&artist).map(str::to_owned),
            format: Some("archive".to_owned()),
            size_bytes: cells.get(2).and_then(|value| parse_human_size(value)),
            webpage_url,
            download_url: Some(download_url),
            access: TrackerMediaAccess::ArchiveNeedsInspection,
            insecure_transport: true,
        });
        if results.len() >= MAX_RESULTS_PER_PAGE {
            break;
        }
    }
    results
}

fn parse_amp(html: &str, base: &Url) -> Vec<TrackerSearchResult> {
    let mut results = Vec::new();
    for block in elements_with_any_class(html, "tr", &["tr0", "tr1"], MAX_RESULTS_PER_PAGE) {
        if !contains_ascii_case_insensitive(block, "downmod.php?") {
            continue;
        }
        let anchors = extract_anchors(block, 16);
        let Some(module) = anchors
            .iter()
            .find(|anchor| contains_ascii_case_insensitive(&anchor.href, "downmod.php?"))
        else {
            continue;
        };
        let Some(download_url) = same_origin_url(base, &module.href) else {
            continue;
        };
        let webpage_url = anchors
            .iter()
            .find(|anchor| contains_ascii_case_insensitive(&anchor.href, "analyzer2.php?"))
            .and_then(|anchor| same_origin_url(base, &anchor.href))
            .unwrap_or_else(|| download_url.clone());
        let artist = anchors
            .iter()
            .find(|anchor| contains_ascii_case_insensitive(&anchor.href, "detail.php?"))
            .and_then(|anchor| nonempty_text(&anchor.text))
            .map(str::to_owned);
        let cells = extract_cell_text(block, 8);
        let format = cells
            .get(2)
            .and_then(|value| nonempty_text(value))
            .map(str::to_ascii_lowercase);
        let access = if format.as_deref().is_some_and(is_known_module_extension) {
            TrackerMediaAccess::DirectModule
        } else {
            TrackerMediaAccess::MetadataOnly
        };
        results.push(TrackerSearchResult {
            source: TrackerArchiveSource::AmigaMusicPreservation,
            source_id: query_value(&download_url, "index")
                .unwrap_or_else(|| download_url.as_str().to_owned()),
            title: module.text.trim().to_owned(),
            artist,
            format,
            size_bytes: cells.get(3).and_then(|value| parse_human_size(value)),
            webpage_url,
            download_url: Some(download_url),
            access,
            insecure_transport: false,
        });
        if results.len() >= MAX_RESULTS_PER_PAGE {
            break;
        }
    }
    results
}

#[derive(Debug, Deserialize)]
struct RawDemozooResult {
    #[serde(rename = "type")]
    kind: String,
    url: String,
    value: String,
}

fn parse_demozoo(raw: Vec<RawDemozooResult>, base: &Url) -> Vec<TrackerSearchResult> {
    let mut results = Vec::new();
    for item in raw {
        if !matches!(item.kind.as_str(), "music" | "production") {
            continue;
        }
        let Some(webpage_url) = same_origin_url(base, &item.url) else {
            continue;
        };
        let title = item.value.trim().to_owned();
        if title.is_empty() {
            continue;
        }
        results.push(TrackerSearchResult {
            source: TrackerArchiveSource::Demozoo,
            source_id: webpage_url.path().trim_matches('/').to_owned(),
            title,
            artist: None,
            format: None,
            size_bytes: None,
            webpage_url,
            download_url: None,
            access: TrackerMediaAccess::MetadataOnly,
            insecure_transport: false,
        });
        if results.len() >= MAX_RESULTS_PER_PAGE {
            break;
        }
    }
    results
}

fn parse_modules_pl(html: &str, base: &Url) -> Vec<TrackerSearchResult> {
    let mut results = Vec::new();
    for block in blocks_starting_at(
        html,
        "<tr valign=\"middle\"",
        MAX_RESULTS_PER_PAGE.saturating_mul(2),
    ) {
        let anchors = extract_anchors(block, 32);
        let Some(module) = anchors.iter().find(|anchor| {
            contains_ascii_case_insensitive(&anchor.href, "id=module")
                && contains_ascii_case_insensitive(&anchor.href, "mod=")
        }) else {
            continue;
        };
        let module_id = query_value_from_raw_href(&module.href, "mod");
        let Some(download) = anchors.iter().find(|anchor| {
            contains_ascii_case_insensitive(&anchor.href, "dl.php?")
                && module_id.as_ref().is_none_or(|module_id| {
                    query_value_from_raw_href(&anchor.href, "mid").as_ref() == Some(module_id)
                })
        }) else {
            continue;
        };
        let Some(webpage_url) = same_origin_url(base, &module.href) else {
            continue;
        };
        let Some(download_url) = same_origin_url(base, &download.href) else {
            continue;
        };
        let artist = anchors
            .iter()
            .find(|anchor| contains_ascii_case_insensitive(&anchor.href, "id=modules&aid="))
            .and_then(|anchor| nonempty_text(&anchor.text))
            .map(str::to_owned);
        let format = text_by_class(block, "span", "format-small")
            .and_then(|value| nonempty_text(&value).map(str::to_ascii_lowercase));
        results.push(TrackerSearchResult {
            source: TrackerArchiveSource::ModulesPl,
            source_id: module_id.unwrap_or_else(|| webpage_url.as_str().to_owned()),
            title: module.text.trim().to_owned(),
            artist,
            format,
            size_bytes: None,
            webpage_url,
            download_url: Some(download_url),
            access: TrackerMediaAccess::ArchiveNeedsInspection,
            insecure_transport: false,
        });
        if results.len() >= MAX_RESULTS_PER_PAGE {
            break;
        }
    }
    results
}

fn parse_modland_listing(
    html: &str,
    directory_url: &Url,
) -> Result<Vec<ModlandDirectoryEntry>, ProviderError> {
    validate_modland_url(directory_url)?;
    let root = parsed_base(MODLAND_BASE);
    let mut entries = Vec::new();
    for block in element_blocks(html, "tr", MAX_MODLAND_LISTING_ENTRIES.saturating_mul(2)) {
        let anchors = extract_anchors(block, 4);
        let Some(anchor) = anchors.first() else {
            continue;
        };
        let raw_href = decode_html(&anchor.href);
        if raw_href == "../"
            || raw_href.starts_with('/')
            || raw_href.starts_with('?')
            || raw_href.contains('\\')
        {
            continue;
        }
        let Some(url) = same_origin_url(directory_url, &raw_href) else {
            continue;
        };
        if validate_modland_url(&url).is_err() || !url.path().starts_with(directory_url.path()) {
            continue;
        }
        let Some(relative_path) = url.path().strip_prefix(root.path()).map(ToOwned::to_owned)
        else {
            continue;
        };
        validate_modland_relative_path(&relative_path)?;
        let cells = extract_cell_text(block, 4);
        let directory = raw_href.ends_with('/');
        entries.push(ModlandDirectoryEntry {
            relative_path,
            name: anchor.text.trim_end_matches('/').trim().to_owned(),
            directory,
            size_bytes: (!directory)
                .then(|| cells.get(1).and_then(|value| parse_human_size(value)))
                .flatten(),
            url,
        });
        if entries.len() >= MAX_MODLAND_LISTING_ENTRIES {
            break;
        }
    }
    Ok(entries)
}

fn modland_result(entry: &ModlandDirectoryEntry) -> TrackerSearchResult {
    let format = (!entry.directory)
        .then(|| {
            extension_from_url(&entry.url)
                .or_else(|| extension_from_name(&entry.name))
                .map(str::to_ascii_lowercase)
        })
        .flatten();
    let access = if entry.directory {
        TrackerMediaAccess::Directory
    } else {
        media_access(format.as_deref())
    };
    TrackerSearchResult {
        source: TrackerArchiveSource::Modland,
        source_id: entry.relative_path.clone(),
        title: entry.name.clone(),
        artist: modland_artist(&entry.relative_path),
        format,
        size_bytes: entry.size_bytes,
        webpage_url: entry.url.clone(),
        download_url: (!entry.directory).then(|| entry.url.clone()),
        access,
        insecure_transport: false,
    }
}

fn modland_artist(relative_path: &str) -> Option<String> {
    let mut components = relative_path.trim_end_matches('/').split('/');
    let _format = components.next()?;
    let artist = components.next()?;
    nonempty_text(&decode_percent_for_display(artist)).map(str::to_owned)
}

fn page_with_lazy_next(
    source: TrackerArchiveSource,
    page: u32,
    items: Vec<TrackerSearchResult>,
) -> TrackerSearchPage {
    let next_page = (!items.is_empty()).then(|| page.saturating_add(1));
    TrackerSearchPage {
        source,
        page,
        items,
        next_page,
        notice: None,
    }
}

fn page_with_counted_next(
    source: TrackerArchiveSource,
    page: u32,
    items: Vec<TrackerSearchResult>,
    remote_page_size: usize,
) -> TrackerSearchPage {
    let next_page = (items.len() >= remote_page_size).then(|| page.saturating_add(1));
    TrackerSearchPage {
        source,
        page,
        items,
        next_page,
        notice: None,
    }
}

fn first_page(source: TrackerArchiveSource, items: Vec<TrackerSearchResult>) -> TrackerSearchPage {
    TrackerSearchPage {
        source,
        page: 1,
        items,
        next_page: None,
        notice: None,
    }
}

fn require_first_page(source: TrackerArchiveSource, page: u32) -> Result<(), ProviderError> {
    if page == 1 {
        return Ok(());
    }
    Err(ProviderError::InvalidRequest(format!(
        "{} search does not expose pagination",
        source.descriptor().display_name
    )))
}

fn parsed_base(raw: &str) -> Url {
    Url::parse(raw).expect("compile-time tracker archive URL must be valid")
}

fn validate_same_origin(origin: &Url, candidate: &Url) -> Result<(), ProviderError> {
    if !matches!(candidate.scheme(), "http" | "https")
        || candidate.host_str().is_none()
        || !candidate.username().is_empty()
        || candidate.password().is_some()
        || candidate.origin() != origin.origin()
        || candidate.scheme() != origin.scheme()
    {
        return Err(ProviderError::InvalidResponse(
            "tracker archive URL escaped its credential-free origin".to_owned(),
        ));
    }
    Ok(())
}

fn same_origin_url(base: &Url, raw: &str) -> Option<Url> {
    let raw = decode_html(raw);
    let mut url = base.join(raw.trim()).ok()?;
    validate_same_origin(base, &url).ok()?;
    url.set_fragment(None);
    Some(url)
}

fn validate_modland_url(url: &Url) -> Result<(), ProviderError> {
    let base = parsed_base(MODLAND_BASE);
    validate_same_origin(&base, url)?;
    if !url.path().starts_with(base.path()) {
        return Err(ProviderError::InvalidRequest(
            "Modland URL must stay inside /pub/modules/".to_owned(),
        ));
    }
    Ok(())
}

fn normalize_modland_directory(raw: &str) -> Result<String, ProviderError> {
    if raw.len() > 1_024 {
        return Err(ProviderError::InvalidRequest(
            "Modland directory path cannot exceed 1024 bytes".to_owned(),
        ));
    }
    let mut path = raw.trim_start_matches('/').to_owned();
    if !path.is_empty() && !path.ends_with('/') {
        path.push('/');
    }
    validate_modland_relative_path(&path)?;
    Ok(path)
}

fn validate_modland_relative_path(path: &str) -> Result<(), ProviderError> {
    if path.is_empty() {
        return Ok(());
    }
    let decoded = decode_percent_for_display(path);
    if path.len() > 2_048
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains(['?', '#'])
        || path.chars().any(char::is_control)
        || decoded.contains('\\')
        || decoded.chars().any(char::is_control)
        || decoded
            .trim_end_matches('/')
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(ProviderError::InvalidRequest(
            "invalid Modland relative path".to_owned(),
        ));
    }
    Ok(())
}

fn map_ureq_error(error: ureq::Error) -> ProviderError {
    match error {
        ureq::Error::StatusCode(code) => ProviderError::HttpStatus(code),
        ureq::Error::BodyExceedsLimit(limit) => ProviderError::ResponseTooLarge {
            limit: usize::try_from(limit).unwrap_or(usize::MAX),
        },
        other => ProviderError::Transport(other.to_string()),
    }
}

fn media_access(extension: Option<&str>) -> TrackerMediaAccess {
    match extension.map(str::to_ascii_lowercase).as_deref() {
        Some(extension) if is_known_module_extension(extension) => TrackerMediaAccess::DirectModule,
        Some(extension) if is_archive_extension(extension) => {
            TrackerMediaAccess::ArchiveNeedsInspection
        }
        _ => TrackerMediaAccess::MetadataOnly,
    }
}

fn is_known_module_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "669"
            | "ahx"
            | "amf"
            | "ams"
            | "dbm"
            | "digi"
            | "dmf"
            | "dsm"
            | "far"
            | "hvl"
            | "imf"
            | "it"
            | "j2b"
            | "med"
            | "mdl"
            | "mo3"
            | "mod"
            | "mptm"
            | "mt2"
            | "mtm"
            | "okt"
            | "psm"
            | "ptm"
            | "s3m"
            | "stm"
            | "ult"
            | "umx"
            | "wow"
            | "xm"
    )
}

fn is_archive_extension(extension: &str) -> bool {
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "7z" | "bz2" | "gz" | "lha" | "lzh" | "lzx" | "rar" | "tar" | "xz" | "zip"
    )
}

fn extension_from_url(url: &Url) -> Option<&str> {
    extension_from_name(url.path())
}

fn extension_from_name(name: &str) -> Option<&str> {
    let file_name = name.rsplit('/').next()?;
    let extension = file_name.rsplit_once('.')?.1;
    (!extension.is_empty()).then_some(extension)
}

fn file_name_from_url(url: &Url) -> Option<&str> {
    nonempty_text(url.path().trim_end_matches('/').rsplit('/').next()?)
}

fn query_value(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find_map(|(name, value)| (name == key).then(|| value.into_owned()))
}

fn query_value_from_raw_href(raw: &str, key: &str) -> Option<String> {
    let base = parsed_base("https://example.invalid/");
    base.join(&decode_html(raw))
        .ok()
        .and_then(|url| query_value(&url, key))
}

fn nonempty_text(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn parse_human_size(raw: &str) -> Option<u64> {
    let normalized = raw
        .trim()
        .replace("&nbsp;", "")
        .replace(' ', "")
        .to_ascii_lowercase();
    if normalized.is_empty() || normalized == "-" {
        return None;
    }
    let split = normalized
        .find(|character: char| !character.is_ascii_digit() && character != '.')
        .unwrap_or(normalized.len());
    let unit = normalized[split..].trim_end_matches('b');
    let multiplier = match unit {
        "" => 1_u128,
        "k" | "ki" => 1024_u128,
        "m" | "mi" => 1024_u128.pow(2),
        "g" | "gi" => 1024_u128.pow(3),
        _ => return None,
    };
    let number = &normalized[..split];
    let (whole, fraction) = number
        .split_once('.')
        .map_or((number, ""), |(whole, fraction)| (whole, fraction));
    if whole.is_empty()
        || fraction.len() > 18
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.parse::<u128>().ok()?;
    let scale = 10_u128.checked_pow(u32::try_from(fraction.len()).ok()?)?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u128>().ok()?
    };
    let numerator = whole.checked_mul(scale)?.checked_add(fraction)?;
    let rounded = numerator.checked_mul(multiplier)?.checked_add(scale / 2)? / scale;
    u64::try_from(rounded).ok()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HtmlAnchor {
    href: String,
    text: String,
}

fn extract_anchors(html: &str, limit: usize) -> Vec<HtmlAnchor> {
    let mut anchors = Vec::new();
    let mut offset = 0;
    while anchors.len() < limit {
        let Some(start) = find_ascii_case_insensitive_from(html, "<a", offset) else {
            break;
        };
        let Some(open_end_relative) = html[start..].find('>') else {
            break;
        };
        let open_end = start + open_end_relative;
        let opening = &html[start..=open_end];
        let Some(close) = find_ascii_case_insensitive_from(html, "</a>", open_end + 1) else {
            break;
        };
        if let Some(href) = attribute_value(opening, "href") {
            anchors.push(HtmlAnchor {
                href,
                text: html_text(&html[open_end + 1..close]),
            });
        }
        offset = close.saturating_add(4);
    }
    anchors
}

fn extract_cell_text(html: &str, limit: usize) -> Vec<String> {
    element_blocks(html, "td", limit)
        .into_iter()
        .map(html_text)
        .collect()
}

fn text_by_class(html: &str, tag: &str, class_name: &str) -> Option<String> {
    element_blocks(html, tag, 256)
        .into_iter()
        .find_map(|block| {
            let opening = opening_tag(block)?;
            attribute_value(opening, "class")
                .is_some_and(|classes| {
                    classes
                        .split_ascii_whitespace()
                        .any(|class| class == class_name)
                })
                .then(|| html_text(block))
        })
}

fn element_blocks<'a>(html: &'a str, tag: &str, limit: usize) -> Vec<&'a str> {
    let start_needle = format!("<{tag}");
    let end_needle = format!("</{tag}>");
    let mut blocks = Vec::new();
    let mut offset = 0;
    while blocks.len() < limit {
        let Some(start) = find_ascii_case_insensitive_from(html, &start_needle, offset) else {
            break;
        };
        let Some(end) =
            matching_element_end(html, &start_needle, &end_needle, start + start_needle.len())
        else {
            break;
        };
        blocks.push(&html[start..end]);
        offset = end;
    }
    blocks
}

fn elements_with_class<'a>(
    html: &'a str,
    tag: &str,
    class_name: &str,
    limit: usize,
) -> Vec<&'a str> {
    elements_with_any_class(html, tag, &[class_name], limit)
}

fn elements_with_any_class<'a>(
    html: &'a str,
    tag: &str,
    class_names: &[&str],
    limit: usize,
) -> Vec<&'a str> {
    let start_needle = format!("<{tag}");
    let end_needle = format!("</{tag}>");
    let mut blocks = Vec::new();
    let mut offset = 0;
    while blocks.len() < limit {
        let Some(start) = find_ascii_case_insensitive_from(html, &start_needle, offset) else {
            break;
        };
        offset = start.saturating_add(start_needle.len());
        let Some(open_end_relative) = html[start..].find('>') else {
            break;
        };
        let open_end = start + open_end_relative;
        let opening = &html[start..=open_end];
        let matches = attribute_value(opening, "class").is_some_and(|classes| {
            classes
                .split_ascii_whitespace()
                .any(|class| class_names.contains(&class))
        });
        if !matches {
            continue;
        }
        let Some(end) =
            matching_element_end(html, &start_needle, &end_needle, open_end.saturating_add(1))
        else {
            break;
        };
        blocks.push(&html[start..end]);
        offset = end;
    }
    blocks
}

fn matching_element_end(
    html: &str,
    start_needle: &str,
    end_needle: &str,
    mut cursor: usize,
) -> Option<usize> {
    let mut depth = 1_u32;
    loop {
        let next_start = find_ascii_case_insensitive_from(html, start_needle, cursor);
        let next_end = find_ascii_case_insensitive_from(html, end_needle, cursor)?;
        if next_start.is_some_and(|start| start < next_end) {
            depth = depth.saturating_add(1);
            cursor = next_start?.saturating_add(start_needle.len());
        } else {
            depth = depth.saturating_sub(1);
            cursor = next_end.saturating_add(end_needle.len());
            if depth == 0 {
                return Some(cursor);
            }
        }
    }
}

fn blocks_starting_at<'a>(html: &'a str, marker: &str, limit: usize) -> Vec<&'a str> {
    let mut starts = Vec::new();
    let mut offset = 0;
    while starts.len() <= limit {
        let Some(start) = find_ascii_case_insensitive_from(html, marker, offset) else {
            break;
        };
        starts.push(start);
        offset = start.saturating_add(marker.len());
    }
    starts
        .iter()
        .take(limit)
        .enumerate()
        .map(|(index, start)| {
            let end = starts.get(index + 1).copied().unwrap_or(html.len());
            &html[*start..end]
        })
        .collect()
}

fn opening_tag(block: &str) -> Option<&str> {
    let end = block.find('>')?;
    Some(&block[..=end])
}

fn attribute_value(tag: &str, wanted: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let mut cursor = 1;
    while cursor < bytes.len() {
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_whitespace() || matches!(bytes[cursor], b'<' | b'/' | b'>'))
        {
            cursor += 1;
        }
        let name_start = cursor;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric()
                || matches!(bytes[cursor], b'-' | b'_' | b':'))
        {
            cursor += 1;
        }
        if cursor == name_start {
            cursor += 1;
            continue;
        }
        let name = &tag[name_start..cursor];
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'=' {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            break;
        }
        let (value_start, value_end) = if matches!(bytes[cursor], b'\'' | b'"') {
            let quote = bytes[cursor];
            cursor += 1;
            let value_start = cursor;
            while cursor < bytes.len() && bytes[cursor] != quote {
                cursor += 1;
            }
            (value_start, cursor)
        } else {
            let value_start = cursor;
            while cursor < bytes.len()
                && !bytes[cursor].is_ascii_whitespace()
                && bytes[cursor] != b'>'
            {
                cursor += 1;
            }
            (value_start, cursor)
        };
        if name.eq_ignore_ascii_case(wanted) {
            return Some(decode_html(&tag[value_start..value_end]));
        }
    }
    None
}

fn html_text(html: &str) -> String {
    let mut without_tags = String::with_capacity(html.len().min(256));
    let mut in_tag = false;
    for character in html.chars() {
        match character {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                without_tags.push(' ');
            }
            _ if !in_tag => without_tags.push(character),
            _ => {}
        }
    }
    collapse_whitespace(&decode_html(&without_tags))
}

fn decode_html(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(ampersand) = rest.find('&') {
        output.push_str(&rest[..ampersand]);
        let entity_start = ampersand + 1;
        let Some(relative_end) = rest[entity_start..].find(';') else {
            output.push_str(&rest[ampersand..]);
            return output;
        };
        let entity_end = entity_start + relative_end;
        let entity = &rest[entity_start..entity_end];
        let decoded = match entity {
            "amp" => Some('&'),
            "apos" | "#39" => Some('\''),
            "gt" => Some('>'),
            "lt" => Some('<'),
            "nbsp" => Some(' '),
            "quot" => Some('"'),
            _ => decode_numeric_entity(entity),
        };
        if let Some(character) = decoded {
            output.push(character);
        } else {
            output.push_str(&rest[ampersand..=entity_end]);
        }
        rest = &rest[entity_end + 1..];
    }
    output.push_str(rest);
    output
}

fn decode_numeric_entity(entity: &str) -> Option<char> {
    let digits = entity.strip_prefix('#')?;
    let value = if let Some(hexadecimal) = digits
        .strip_prefix('x')
        .or_else(|| digits.strip_prefix('X'))
    {
        u32::from_str_radix(hexadecimal, 16).ok()?
    } else {
        digits.parse().ok()?
    };
    char::from_u32(value)
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn contains_ascii_case_insensitive(haystack: &str, needle: &str) -> bool {
    find_ascii_case_insensitive_from(haystack, needle, 0).is_some()
}

fn find_ascii_case_insensitive_from(haystack: &str, needle: &str, start: usize) -> Option<usize> {
    if needle.is_empty() || start > haystack.len() {
        return None;
    }
    haystack.as_bytes()[start..]
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
        .map(|relative| start + relative)
}

fn decode_percent_for_display(value: &str) -> String {
    let mut output = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'%'
            && cursor + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[cursor + 1]), hex_value(bytes[cursor + 2]))
        {
            output.push((high << 4) | low);
            cursor += 3;
        } else {
            output.push(bytes[cursor]);
            cursor += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

const fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCENE_FIXTURE: &str = r"
        <ul class='filelist'>
          <li class='file'><a href='https://files.scene.org/view/music/demo.xm'>
            <span class='filename'><span class='path'>/music/</span>demo.xm</span>
            <span class='filesize'>1.25M</span></a></li>
          <li class='file archive'><a href='/view/music/pack.zip'>
            <span class='filename'>pack.zip</span> <span class='filesize'>20K</span></a></li>
          <li class='file image'><a href='/view/music/cover.png'>
            <span class='filename'>cover.png</span></a></li>
        </ul>
    ";

    const AMINET_FIXTURE: &str = r#"
        <tr class="lightrow pkg_row">
          <td class="name_col"><a href="/mods/tranc/example.lha">example.lha</a></td>
          <td></td><td><a href="/mods/tranc">mods/tranc</a></td><td>907</td>
          <td class="size_col">11M</td><td>2026-01-01</td><td></td>
          <td><a href="/package/mods/tranc/example">Example archive</a></td>
        </tr>
    "#;

    const MIRSOFT_FIXTURE: &str = r#"
        <TR><TD class="blocked">
          <a href="./gmb/music_info.php?id_ele=NjQ5">Lotus 3: Game rip</a>
        </TD><TD><a href="./gmb/musician_info.php?id_ele=NDE0">Patrick Phelan</a></TD>
        <TD>695 KB</TD><TD>17284</TD><TD>23</TD><TD>9.09</TD><TD>2002-08-23</TD>
        <TD><a href="./wogm_download.php?data=safe-token">Download</a></TD></TR>
    "#;

    const AMP_FIXTURE: &str = r#"
        <tr class="tr0">
          <td><a href="downmod.php?index=68102&amp;application=AMP">Lotus title</a></td>
          <td><a href="detail.php?view=6584">Shaun Southern</a></td>
          <td>MOD</td><td>53Kb</td><td>163</td>
          <td><a href="analyzer2.php?idx=68102">Info</a></td>
        </tr>
    "#;

    const MODULES_PL_FIXTURE: &str = r#"
        <tr valign="middle">
          <td><span class="format-small">xm</span></td>
          <td><a href="?id=modules&amp;aid=458">MichU</a></td>
          <td><table><tr><td><a class="module" href="?id=module&amp;mod=3678">
            Lotus 3</a></td></tr></table></td>
          <td><table><tr><td><a class="module" href="dl.php?mid=3678">Download</a>
          </td></tr></table></td>
        </tr>
    "#;

    const MODLAND_FIXTURE: &str = r#"
        <table><tbody>
          <tr><td class="link"><a href="../">Parent directory/</a></td><td>-</td></tr>
          <tr><td class="link"><a href="Artist/" title="Artist">Artist/</a></td>
              <td class="size">-</td></tr>
          <tr><td class="link"><a href="demo.mod" title="demo.mod">demo.mod</a></td>
              <td class="size">128K</td></tr>
          <tr><td class="link"><a href="pack.zip">pack.zip</a></td>
              <td class="size">1.5M</td></tr>
          <tr><td><a href="https://attacker.invalid/file.mod">outside.mod</a></td></tr>
        </tbody></table>
    "#;

    #[test]
    fn all_requested_sources_are_enabled_by_default() {
        let descriptors = TrackerArchiveSource::ALL.map(TrackerArchiveSource::descriptor);
        assert!(descriptors.iter().all(|source| source.enabled_by_default));
        let mirsoft = TrackerArchiveSource::Mirsoft.descriptor();
        assert!(mirsoft.insecure_http);
        assert_eq!(mirsoft.homepage, "http://www.mirsoft.info/gamemods.php");
        assert_eq!(
            TrackerArchiveSource::Modland.descriptor().search_mode,
            TrackerSearchMode::LocalCatalogue
        );
    }

    #[test]
    fn request_limits_reject_empty_large_and_invalid_pages() {
        assert!(TrackerSearchRequest::new("").validate().is_err());
        assert!(
            TrackerSearchRequest::new("x".repeat(MAX_QUERY_BYTES + 1))
                .validate()
                .is_err()
        );
        let mut request = TrackerSearchRequest::new("music");
        request.page = 0;
        assert!(request.validate().is_err());
        request.page = MAX_PAGE + 1;
        assert!(request.validate().is_err());
        assert!(TrackerSearchRequest::new("line\nbreak").validate().is_err());
    }

    #[test]
    fn scene_parser_keeps_modules_and_archives_only() {
        let results = parse_scene(SCENE_FIXTURE, &parsed_base(SCENE_BASE));
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "demo.xm");
        assert_eq!(results[0].format.as_deref(), Some("xm"));
        assert_eq!(results[0].access, TrackerMediaAccess::DirectModule);
        assert_eq!(results[0].size_bytes, Some(1_310_720));
        assert_eq!(
            results[0].download_url.as_ref().map(Url::as_str),
            Some("https://files.scene.org/get/music/demo.xm")
        );
        assert_eq!(
            results[1].access,
            TrackerMediaAccess::ArchiveNeedsInspection
        );
    }

    #[test]
    fn aminet_parser_marks_package_for_safe_extraction() {
        let results = parse_aminet(AMINET_FIXTURE, &parsed_base(AMINET_BASE));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "example.lha");
        assert_eq!(results[0].size_bytes, Some(11 * 1024 * 1024));
        assert_eq!(
            results[0].webpage_url.as_str(),
            "https://aminet.net/package/mods/tranc/example"
        );
        assert_eq!(
            results[0].access,
            TrackerMediaAccess::ArchiveNeedsInspection
        );
    }

    #[test]
    fn mirsoft_parser_retains_insecure_transport_marker() {
        let results = parse_mirsoft(MIRSOFT_FIXTURE, &parsed_base(MIRSOFT_BASE));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Lotus 3: Game rip");
        assert_eq!(results[0].artist.as_deref(), Some("Patrick Phelan"));
        assert_eq!(results[0].size_bytes, Some(695 * 1024));
        assert!(results[0].insecure_transport);
        assert!(
            results[0]
                .download_url
                .as_ref()
                .is_some_and(|url| url.scheme() == "http")
        );
    }

    #[test]
    fn amp_parser_exposes_known_module_as_direct() {
        let results = parse_amp(AMP_FIXTURE, &parsed_base(AMP_BASE));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source_id, "68102");
        assert_eq!(results[0].artist.as_deref(), Some("Shaun Southern"));
        assert_eq!(results[0].format.as_deref(), Some("mod"));
        assert_eq!(results[0].access, TrackerMediaAccess::DirectModule);
        assert!(results[0].direct_play_url().is_some());
    }

    #[test]
    fn marked_row_parsers_work_inside_layout_tables() {
        let nested_amp =
            format!("<table><tr><td><table>{AMP_FIXTURE}{AMP_FIXTURE}</table></td></tr></table>");
        let amp_results = parse_amp(&nested_amp, &parsed_base(AMP_BASE));
        assert_eq!(amp_results.len(), 2);

        let nested_aminet =
            format!("<table><tr><td><table>{AMINET_FIXTURE}</table></td></tr></table>");
        let aminet_results = parse_aminet(&nested_aminet, &parsed_base(AMINET_BASE));
        assert_eq!(aminet_results.len(), 1);
    }

    #[test]
    fn demozoo_parser_is_metadata_only_and_filters_graphics() {
        let raw: Vec<RawDemozooResult> = serde_json::from_str(
            r#"[
              {"type":"music","url":"/music/322150/","value":"Lotus - Maxus"},
              {"type":"graphics","url":"/graphics/1/","value":"Lotus image"}
            ]"#,
        )
        .expect("Demozoo fixture");
        let results = parse_demozoo(raw, &parsed_base(DEMOZOO_BASE));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source_id, "music/322150");
        assert_eq!(results[0].access, TrackerMediaAccess::MetadataOnly);
        assert!(results[0].download_url.is_none());
    }

    #[test]
    fn modules_pl_parser_pairs_title_and_download_ids() {
        let results = parse_modules_pl(MODULES_PL_FIXTURE, &parsed_base(MODULES_PL_BASE));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source_id, "3678");
        assert_eq!(results[0].artist.as_deref(), Some("MichU"));
        assert_eq!(results[0].format.as_deref(), Some("xm"));
        assert_eq!(
            results[0].access,
            TrackerMediaAccess::ArchiveNeedsInspection
        );
        assert_eq!(
            results[0].download_url.as_ref().map(Url::as_str),
            Some("https://www.modules.pl/dl.php?mid=3678")
        );
    }

    #[test]
    fn modland_listing_is_non_recursive_and_rejects_external_links() {
        let directory = parsed_base("https://ftp.modland.com/pub/modules/Fasttracker%202/");
        let entries = parse_modland_listing(MODLAND_FIXTURE, &directory).expect("Modland fixture");
        assert_eq!(entries.len(), 3);
        assert!(entries[0].directory);
        assert_eq!(entries[1].name, "demo.mod");
        assert_eq!(entries[1].size_bytes, Some(128 * 1024));
        assert_eq!(
            entries[1].url.as_str(),
            "https://ftp.modland.com/pub/modules/Fasttracker%202/demo.mod"
        );
    }

    #[test]
    fn modland_catalogue_searches_locally_with_paging() {
        let root = parsed_base(MODLAND_BASE);
        let mut catalogue = ModlandCatalogue::default();
        let listing = (0..55)
            .map(|index| {
                let relative_path = format!("Protracker/Artist/lotus-{index:02}.mod");
                ModlandDirectoryEntry {
                    name: format!("lotus-{index:02}.mod"),
                    relative_path: relative_path.clone(),
                    directory: false,
                    size_bytes: Some(1024),
                    url: root.join(&relative_path).expect("fixture URL"),
                }
            })
            .collect::<Vec<_>>();
        catalogue.update(listing).expect("catalogue update");
        let first = catalogue
            .search(&TrackerSearchRequest::new("LOTUS"))
            .expect("first page");
        assert_eq!(first.items.len(), MODLAND_PAGE_SIZE);
        assert_eq!(first.next_page, Some(2));
        assert_eq!(first.items[0].artist.as_deref(), Some("Artist"));
        let mut request = TrackerSearchRequest::new("lotus");
        request.page = 2;
        let second = catalogue.search(&request).expect("second page");
        assert_eq!(second.items.len(), 5);
        assert_eq!(second.next_page, None);
    }

    #[test]
    fn helpers_decode_entities_sizes_and_reject_traversal() {
        assert_eq!(decode_html("A&amp;B&nbsp;&#x266b;"), "A&B ♫");
        assert_eq!(parse_human_size("1.5M"), Some(1_572_864));
        assert!(normalize_modland_directory("").is_ok());
        assert!(normalize_modland_directory("../private").is_err());
        assert!(normalize_modland_directory("%2e%2e/private").is_err());
        assert!(normalize_modland_directory("Format/?secret").is_err());
        assert!(
            same_origin_url(
                &parsed_base(SCENE_BASE),
                "https://attacker.invalid/file.mod"
            )
            .is_none()
        );
    }
}
