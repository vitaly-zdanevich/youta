//! Bounded LibriVox catalog and public-page adapter.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::domain::remote_url_has_non_public_host;

use super::{DEFAULT_REQUEST_TIMEOUT, ProviderError};

const AUDIOBOOKS_ENDPOINT: &str = "https://librivox.org/api/feed/audiobooks/";
const AUTHORS_ENDPOINT: &str = "https://librivox.org/api/feed/authors/";
const DEFAULT_MAX_JSON_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
const MAX_CONFIGURED_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_SEARCH_RESULTS: usize = 512;
const MAX_QUERY_BYTES: usize = 512;
const MAX_TITLE_BYTES: usize = 4 * 1024;
const MAX_DESCRIPTION_BYTES: usize = 64 * 1024;
const MAX_NAME_BYTES: usize = 2 * 1024;
const MAX_LANGUAGE_BYTES: usize = 256;
const MAX_URL_BYTES: usize = 16 * 1024;
const MAX_AUTHORS: usize = 64;
const MAX_GENRES: usize = 128;
const MAX_KEYWORDS: usize = 128;
const MAX_SECTIONS: usize = 4_096;
const MAX_READERS: usize = 128;

/// One bounded LibriVox catalogue request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LibrivoxSearchRequest {
    /// Optional title fragment understood by the public API.
    pub title: Option<String>,
    /// Optional author surname fragment understood by the public API.
    pub author: Option<String>,
    /// Maximum number of API rows requested.
    pub limit: usize,
    /// Zero-based API offset.
    pub offset: usize,
}

impl LibrivoxSearchRequest {
    /// Creates a bounded catalogue request for the supplied text.
    #[must_use]
    pub fn for_text(query: impl Into<String>, limit: usize, offset: usize) -> Self {
        let query = query.into();
        Self {
            title: (!query.trim().is_empty()).then_some(query),
            author: None,
            limit,
            offset,
        }
    }

    /// Validates query sizes and pagination bounds.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] for empty explicit filters,
    /// oversized text, or a result limit outside `1..=512`.
    pub fn validate(&self) -> Result<(), ProviderError> {
        for (label, value) in [
            ("title", self.title.as_deref()),
            ("author", self.author.as_deref()),
        ] {
            if let Some(value) = value {
                if value.trim().is_empty() {
                    return Err(ProviderError::InvalidRequest(format!(
                        "LibriVox {label} filter cannot be empty"
                    )));
                }
                if value.len() > MAX_QUERY_BYTES {
                    return Err(ProviderError::InvalidRequest(format!(
                        "LibriVox {label} filter cannot exceed {MAX_QUERY_BYTES} bytes"
                    )));
                }
            }
        }
        if !(1..=MAX_SEARCH_RESULTS).contains(&self.limit) {
            return Err(ProviderError::InvalidRequest(format!(
                "LibriVox result limit must be between 1 and {MAX_SEARCH_RESULTS}"
            )));
        }
        Ok(())
    }
}

/// One normalized LibriVox author identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LibrivoxAuthor {
    /// Stable positive LibriVox author identifier.
    pub author_id: u64,
    /// Human-facing full name.
    pub display_name: String,
    /// Given names, when present.
    pub first_name: Option<String>,
    /// Family name, when present.
    pub last_name: Option<String>,
    /// Birth year, when supplied.
    pub birth_year: Option<u16>,
    /// Death year, when supplied.
    pub death_year: Option<u16>,
    /// Canonical public LibriVox author page.
    pub webpage_url: Url,
}

/// One LibriVox subject genre supplied by the public API.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LibrivoxGenre {
    /// Stable numeric genre identifier, when supplied.
    pub genre_id: Option<u64>,
    /// Human-readable genre name.
    pub name: String,
}

/// One public keyword link extracted from a canonical book page.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LibrivoxKeyword {
    /// Human-readable keyword.
    pub name: String,
    /// Canonical LibriVox keyword page.
    pub webpage_url: Url,
}

/// One volunteer reader credited for a section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LibrivoxReader {
    /// Stable reader identifier, when supplied.
    pub reader_id: Option<u64>,
    /// Human-readable reader name.
    pub display_name: String,
}

/// Artwork links supplied by LibriVox's extended audiobook response.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct LibrivoxCoverArt {
    /// Full JPEG cover.
    pub jpeg_url: Option<Url>,
    /// Small JPEG cover.
    pub thumbnail_url: Option<Url>,
    /// Printable PDF cover.
    pub pdf_url: Option<Url>,
}

/// One playable LibriVox audiobook section.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LibrivoxSection {
    /// Stable positive section identifier.
    pub section_id: u64,
    /// One-based order inside the audiobook.
    pub number: u32,
    /// Human-facing section title.
    pub title: String,
    /// Spoken language, when supplied.
    pub language: Option<String>,
    /// Section duration in seconds.
    pub duration_seconds: Option<u64>,
    /// Volunteer readers credited for this section.
    pub readers: Vec<LibrivoxReader>,
    /// Highest-quality validated Archive.org MP3 known to Youta.
    pub preferred_audio_url: Url,
    /// Lower-quality API URL retained if page enrichment found a better file.
    pub fallback_audio_url: Option<Url>,
}

/// One normalized LibriVox audiobook.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LibrivoxBook {
    /// Stable positive audiobook identifier.
    pub book_id: u64,
    /// Human-facing book title.
    pub title: String,
    /// Plain-text book description.
    pub description: Option<String>,
    /// Spoken language, when supplied.
    pub language: Option<String>,
    /// Publication or copyright year, when supplied by LibriVox.
    pub copyright_year: Option<u16>,
    /// Total runtime in seconds.
    pub duration_seconds: Option<u64>,
    /// Canonical public LibriVox book page.
    pub webpage_url: Url,
    /// Public source-text page, when supplied.
    pub text_source_url: Option<Url>,
    /// LibriVox RSS feed for this book.
    pub rss_url: Option<Url>,
    /// Archive.org MP3 ZIP, when supplied.
    pub zip_url: Option<Url>,
    /// Archive.org item page, when supplied.
    pub archive_url: Option<Url>,
    /// Full and thumbnail cover links.
    pub covers: LibrivoxCoverArt,
    /// All bounded authors supplied by the API.
    pub authors: Vec<LibrivoxAuthor>,
    /// API-native genres.
    pub genres: Vec<LibrivoxGenre>,
    /// Canonical-page keywords, populated by optional HTML enrichment.
    pub keywords: Vec<LibrivoxKeyword>,
    /// Playable audiobook sections.
    pub sections: Vec<LibrivoxSection>,
}

/// One bounded catalogue response.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LibrivoxSearchPage {
    /// Valid normalized books in provider order.
    pub books: Vec<LibrivoxBook>,
    /// Offset used for this response.
    pub offset: usize,
    /// Next offset when the API returned a full page.
    pub next_offset: Option<usize>,
}

/// One author biography plus an exact-ID-filtered bibliography.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LibrivoxAuthorDetails {
    /// Stable author identity.
    pub author: LibrivoxAuthor,
    /// Plain-text biography extracted from the public author page.
    pub description: Option<String>,
    /// Linked Wikipedia article, when the page supplies one.
    pub wikipedia_url: Option<Url>,
    /// Books whose embedded author list contains the exact author ID.
    pub books: Vec<LibrivoxBook>,
    /// Next surname-query offset if the configured hard bound was filled.
    pub next_offset: Option<usize>,
}

/// Fetches one already validated LibriVox API or public-page URL.
///
/// A transport trait keeps parsing and controller tests deterministic and
/// makes this module straightforward to extract into a standalone crate later.
pub trait LibrivoxTransport: Send + Sync {
    /// Returns at most `max_bytes` raw response bytes.
    ///
    /// # Errors
    ///
    /// Returns a provider error for invalid URLs, HTTP failures, transport
    /// failures, or oversized responses.
    fn fetch(
        &self,
        url: &Url,
        accept: &'static str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, ProviderError>;
}

#[derive(Clone)]
struct UreqLibrivoxTransport {
    agent: ureq::Agent,
}

impl LibrivoxTransport for UreqLibrivoxTransport {
    fn fetch(
        &self,
        url: &Url,
        accept: &'static str,
        max_bytes: usize,
    ) -> Result<Vec<u8>, ProviderError> {
        validate_librivox_fetch_url(url)?;
        let mut response = self
            .agent
            .get(url.as_str())
            .header("Accept", accept)
            .call()
            .map_err(map_ureq_error)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(ProviderError::HttpStatus(status));
        }
        if response
            .body()
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
        {
            return Err(ProviderError::ResponseTooLarge { limit: max_bytes });
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(u64::try_from(max_bytes.saturating_add(1)).unwrap_or(u64::MAX))
            .read_to_vec()
            .map_err(|error| match error {
                ureq::Error::BodyExceedsLimit(_) => {
                    ProviderError::ResponseTooLarge { limit: max_bytes }
                }
                other => ProviderError::Transport(other.to_string()),
            })?;
        if bytes.len() > max_bytes {
            return Err(ProviderError::ResponseTooLarge { limit: max_bytes });
        }
        Ok(bytes)
    }
}

/// Blocking, bounded client for the credential-free LibriVox catalogue.
#[derive(Clone)]
pub struct LibrivoxClient {
    transport: Arc<dyn LibrivoxTransport>,
    max_json_bytes: usize,
    max_html_bytes: usize,
}

impl fmt::Debug for LibrivoxClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LibrivoxClient")
            .field("max_json_bytes", &self.max_json_bytes)
            .field("max_html_bytes", &self.max_html_bytes)
            .finish_non_exhaustive()
    }
}

impl Default for LibrivoxClient {
    fn default() -> Self {
        Self::new()
    }
}

impl LibrivoxClient {
    /// Creates a client with conservative timeout and body-size bounds.
    #[must_use]
    pub fn new() -> Self {
        Self::with_options(
            DEFAULT_REQUEST_TIMEOUT,
            DEFAULT_MAX_JSON_BYTES,
            DEFAULT_MAX_HTML_BYTES,
        )
        .expect("built-in LibriVox client limits must be valid")
    }

    /// Creates a client with explicit network and response limits.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] for a zero timeout or a body
    /// bound outside `1..=64 MiB`.
    pub fn with_options(
        timeout: Duration,
        max_json_bytes: usize,
        max_html_bytes: usize,
    ) -> Result<Self, ProviderError> {
        validate_client_options(timeout, max_json_bytes, max_html_bytes)?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(0)
            .user_agent(concat!(
                "youta/",
                env!("CARGO_PKG_VERSION"),
                " (+",
                env!("CARGO_PKG_REPOSITORY"),
                ")"
            ))
            .build()
            .into();
        Ok(Self {
            transport: Arc::new(UreqLibrivoxTransport { agent }),
            max_json_bytes,
            max_html_bytes,
        })
    }

    /// Creates a deterministic client around a caller-supplied transport.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::InvalidRequest`] for invalid body bounds.
    pub fn with_transport(
        transport: Arc<dyn LibrivoxTransport>,
        max_json_bytes: usize,
        max_html_bytes: usize,
    ) -> Result<Self, ProviderError> {
        validate_client_options(Duration::from_secs(1), max_json_bytes, max_html_bytes)?;
        Ok(Self {
            transport,
            max_json_bytes,
            max_html_bytes,
        })
    }

    /// Searches books by title and, for a general text search, by author too.
    ///
    /// The public API has separate title and author parameters. A normal text
    /// request therefore makes at most two bounded calls and deduplicates exact
    /// book IDs, while an explicit title+author request remains one call. A
    /// supplementary author-search outage cannot discard usable title matches.
    ///
    /// # Errors
    ///
    /// Returns a provider error for invalid input, a failed primary lookup, a
    /// failed supplementary lookup when the title lookup found nothing,
    /// oversized responses, or malformed top-level JSON.
    pub fn search(
        &self,
        request: &LibrivoxSearchRequest,
    ) -> Result<LibrivoxSearchPage, ProviderError> {
        let mut page = self.search_once(request)?;
        if request.title.is_some() && request.author.is_none() {
            let author_request = LibrivoxSearchRequest {
                title: None,
                author: request.title.clone(),
                limit: request.limit,
                offset: request.offset,
            };
            let author_page = match self.search_once(&author_request) {
                Ok(author_page) => author_page,
                Err(error) if page.books.is_empty() => return Err(error),
                Err(_) => return Ok(page),
            };
            for book in author_page.books {
                if page.books.len() == request.limit {
                    break;
                }
                if !page
                    .books
                    .iter()
                    .any(|existing| existing.book_id == book.book_id)
                {
                    page.books.push(book);
                }
            }
            if page.next_offset.is_none() {
                page.next_offset = author_page.next_offset;
            }
        }
        Ok(page)
    }

    /// Loads full API metadata and opportunistically enriches one book page.
    ///
    /// Failure of the optional HTML request never discards usable API genres,
    /// descriptions, covers, or 64 kbps chapter URLs.
    ///
    /// # Errors
    ///
    /// Returns a provider error when the ID is zero or the required API request
    /// fails or omits the exact book.
    pub fn book_details(&self, book_id: u64) -> Result<LibrivoxBook, ProviderError> {
        if book_id == 0 {
            return Err(ProviderError::InvalidRequest(
                "LibriVox book ID must be positive".to_owned(),
            ));
        }
        let url = build_book_url(book_id)?;
        let bytes = self
            .transport
            .fetch(&url, "application/json", self.max_json_bytes)?;
        let mut book = parse_books_json(&bytes)?
            .into_iter()
            .find(|book| book.book_id == book_id)
            .ok_or_else(|| {
                ProviderError::InvalidResponse(format!("LibriVox response omitted book {book_id}"))
            })?;
        if let Ok(bytes) = self.transport.fetch(
            &book.webpage_url,
            "text/html,application/xhtml+xml",
            self.max_html_bytes,
        ) && let Ok(html) = std::str::from_utf8(&bytes)
        {
            let _ = enrich_book_from_html(&mut book, html);
        }
        Ok(book)
    }

    /// Loads one author, their bounded biography, and an exact-ID bibliography.
    ///
    /// LibriVox's audiobook API filters authors by surname. Returned books are
    /// therefore filtered again by stable embedded author ID.
    ///
    /// # Errors
    ///
    /// Returns a provider error for a zero ID, invalid pagination, required API
    /// failures, or an API response that omits the exact author.
    pub fn author_details(
        &self,
        author_id: u64,
        limit: usize,
        offset: usize,
    ) -> Result<LibrivoxAuthorDetails, ProviderError> {
        if author_id == 0 {
            return Err(ProviderError::InvalidRequest(
                "LibriVox author ID must be positive".to_owned(),
            ));
        }
        if !(1..=MAX_SEARCH_RESULTS).contains(&limit) {
            return Err(ProviderError::InvalidRequest(format!(
                "LibriVox author book limit must be between 1 and {MAX_SEARCH_RESULTS}"
            )));
        }
        let url = build_author_url(author_id)?;
        let bytes = self
            .transport
            .fetch(&url, "application/json", self.max_json_bytes)?;
        let author = parse_authors_json(&bytes)?
            .into_iter()
            .find(|author| author.author_id == author_id)
            .ok_or_else(|| {
                ProviderError::InvalidResponse(format!(
                    "LibriVox response omitted author {author_id}"
                ))
            })?;
        let mut details = match self.transport.fetch(
            &author.webpage_url,
            "text/html,application/xhtml+xml",
            self.max_html_bytes,
        ) {
            Ok(bytes) => std::str::from_utf8(&bytes)
                .ok()
                .and_then(|html| parse_author_html(author.clone(), html).ok())
                .unwrap_or_else(|| empty_author_details(author.clone())),
            Err(_) => empty_author_details(author.clone()),
        };
        let page = self.author_books(&author, limit, offset)?;
        details.books = page.books;
        details.next_offset = page.next_offset;
        Ok(details)
    }

    /// Loads one bounded bibliography page for an already resolved author.
    ///
    /// LibriVox filters the catalogue by surname, so this method rechecks every
    /// returned book against the author's stable numeric ID. Unlike
    /// [`Self::author_details`], continuation pages do not refetch the author
    /// record or biography.
    ///
    /// # Errors
    ///
    /// Returns a provider error for an invalid author, invalid pagination, or
    /// a required catalogue request failure.
    pub fn author_books(
        &self,
        author: &LibrivoxAuthor,
        limit: usize,
        offset: usize,
    ) -> Result<LibrivoxSearchPage, ProviderError> {
        if author.author_id == 0 {
            return Err(ProviderError::InvalidRequest(
                "LibriVox author ID must be positive".to_owned(),
            ));
        }
        let request = LibrivoxSearchRequest {
            title: None,
            author: author
                .last_name
                .clone()
                .or_else(|| Some(author.display_name.clone())),
            limit,
            offset,
        };
        let mut page = self.author_books_once(&request)?;
        page.books.retain(|book| {
            book.authors
                .iter()
                .any(|candidate| candidate.author_id == author.author_id)
        });
        Ok(page)
    }

    fn search_once(
        &self,
        request: &LibrivoxSearchRequest,
    ) -> Result<LibrivoxSearchPage, ProviderError> {
        let url = build_search_url(request)?;
        let bytes = match self
            .transport
            .fetch(&url, "application/json", self.max_json_bytes)
        {
            Ok(bytes) => bytes,
            // The public API represents an empty catalogue filter as 404
            // instead of a successful response containing an empty array.
            Err(ProviderError::HttpStatus(404)) => {
                return Ok(empty_search_page(request));
            }
            Err(error) => return Err(error),
        };
        let books = parse_books_json(&bytes)?;
        let next_offset =
            (books.len() == request.limit).then(|| request.offset.saturating_add(request.limit));
        Ok(LibrivoxSearchPage {
            books,
            offset: request.offset,
            next_offset,
        })
    }

    /// Loads author rows without the chapter arrays that are needed only
    /// after the user opens a book.
    fn author_books_once(
        &self,
        request: &LibrivoxSearchRequest,
    ) -> Result<LibrivoxSearchPage, ProviderError> {
        let url = build_author_books_url(request)?;
        let bytes = match self
            .transport
            .fetch(&url, "application/json", self.max_json_bytes)
        {
            Ok(bytes) => bytes,
            Err(ProviderError::HttpStatus(404)) => {
                return Ok(empty_search_page(request));
            }
            Err(error) => return Err(error),
        };
        let books = parse_books_json(&bytes)?;
        let next_offset =
            (books.len() == request.limit).then(|| request.offset.saturating_add(request.limit));
        Ok(LibrivoxSearchPage {
            books,
            offset: request.offset,
            next_offset,
        })
    }
}

fn empty_search_page(request: &LibrivoxSearchRequest) -> LibrivoxSearchPage {
    LibrivoxSearchPage {
        books: Vec::new(),
        offset: request.offset,
        next_offset: None,
    }
}

fn validate_client_options(
    timeout: Duration,
    max_json_bytes: usize,
    max_html_bytes: usize,
) -> Result<(), ProviderError> {
    if timeout.is_zero() {
        return Err(ProviderError::InvalidRequest(
            "LibriVox timeout must be greater than zero".to_owned(),
        ));
    }
    for (label, value) in [("JSON", max_json_bytes), ("HTML", max_html_bytes)] {
        if !(1..=MAX_CONFIGURED_RESPONSE_BYTES).contains(&value) {
            return Err(ProviderError::InvalidRequest(format!(
                "LibriVox {label} response limit must be between 1 byte and {MAX_CONFIGURED_RESPONSE_BYTES} bytes"
            )));
        }
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

fn build_search_url(request: &LibrivoxSearchRequest) -> Result<Url, ProviderError> {
    request.validate()?;
    let mut url = Url::parse(AUDIOBOOKS_ENDPOINT)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("format", "json");
        query.append_pair("extended", "1");
        query.append_pair("coverart", "1");
        if let Some(title) = request.title.as_deref() {
            query.append_pair("title", title.trim());
        }
        if let Some(author) = request.author.as_deref() {
            query.append_pair("author", author.trim());
        }
        query.append_pair("limit", &request.limit.to_string());
        query.append_pair("offset", &request.offset.to_string());
    }
    Ok(url)
}

fn build_book_url(book_id: u64) -> Result<Url, ProviderError> {
    let mut url = Url::parse(AUDIOBOOKS_ENDPOINT)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    url.query_pairs_mut()
        .append_pair("format", "json")
        .append_pair("extended", "1")
        .append_pair("coverart", "1")
        .append_pair("id", &book_id.to_string());
    Ok(url)
}

fn build_author_books_url(request: &LibrivoxSearchRequest) -> Result<Url, ProviderError> {
    request.validate()?;
    if request.title.is_some() || request.author.is_none() {
        return Err(ProviderError::InvalidRequest(
            "LibriVox author bibliography requires one author filter".to_owned(),
        ));
    }
    let mut url = Url::parse(AUDIOBOOKS_ENDPOINT)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("format", "json");
        query.append_pair("coverart", "1");
        query.append_pair(
            "author",
            request.author.as_deref().expect("validated author filter"),
        );
        query.append_pair("limit", &request.limit.to_string());
        query.append_pair("offset", &request.offset.to_string());
    }
    Ok(url)
}

fn build_author_url(author_id: u64) -> Result<Url, ProviderError> {
    let mut url = Url::parse(AUTHORS_ENDPOINT)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    url.query_pairs_mut()
        .append_pair("format", "json")
        .append_pair("id", &author_id.to_string());
    Ok(url)
}

fn validate_librivox_fetch_url(url: &Url) -> Result<(), ProviderError> {
    if url.scheme() != "https"
        || url.host_str() != Some("librivox.org")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ProviderError::InvalidRequest(
            "LibriVox fetch URL left the fixed HTTPS origin".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct RawBooksEnvelope {
    #[serde(default)]
    books: Vec<RawBook>,
}

#[derive(Debug, Deserialize)]
struct RawAuthorsEnvelope {
    #[serde(default)]
    authors: Vec<RawAuthor>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum RawScalar {
    String(String),
    Unsigned(u64),
    Signed(i64),
}

impl RawScalar {
    fn text(&self) -> String {
        match self {
            Self::String(value) => value.clone(),
            Self::Unsigned(value) => value.to_string(),
            Self::Signed(value) => value.to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawBook {
    #[serde(default)]
    id: Option<RawScalar>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    url_text_source: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    copyright_year: Option<RawScalar>,
    #[serde(default)]
    url_rss: Option<String>,
    #[serde(default)]
    url_zip_file: Option<String>,
    #[serde(default)]
    url_librivox: Option<String>,
    #[serde(default)]
    url_iarchive: Option<String>,
    #[serde(default)]
    totaltimesecs: Option<RawScalar>,
    #[serde(default)]
    authors: Vec<RawAuthor>,
    #[serde(default)]
    coverart_jpg: Option<String>,
    #[serde(default)]
    coverart_pdf: Option<String>,
    #[serde(default)]
    coverart_thumbnail: Option<String>,
    #[serde(default)]
    sections: Vec<RawSection>,
    #[serde(default)]
    genres: Vec<RawGenre>,
}

#[derive(Clone, Debug, Deserialize)]
struct RawAuthor {
    #[serde(default)]
    id: Option<RawScalar>,
    #[serde(default)]
    first_name: Option<String>,
    #[serde(default)]
    last_name: Option<String>,
    #[serde(default)]
    dob: Option<RawScalar>,
    #[serde(default)]
    dod: Option<RawScalar>,
}

#[derive(Debug, Deserialize)]
struct RawGenre {
    #[serde(default)]
    id: Option<RawScalar>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawSection {
    #[serde(default)]
    id: Option<RawScalar>,
    #[serde(default)]
    section_number: Option<RawScalar>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    listen_url: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    playtime: Option<RawScalar>,
    #[serde(default)]
    readers: Vec<RawReader>,
}

#[derive(Debug, Deserialize)]
struct RawReader {
    #[serde(default)]
    reader_id: Option<RawScalar>,
    #[serde(default)]
    display_name: Option<String>,
}

fn parse_books_json(bytes: &[u8]) -> Result<Vec<LibrivoxBook>, ProviderError> {
    let envelope: RawBooksEnvelope = serde_json::from_slice(bytes)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    Ok(envelope
        .books
        .into_iter()
        .take(MAX_SEARCH_RESULTS)
        .filter_map(|book| normalize_book(book).ok())
        .collect())
}

fn parse_authors_json(bytes: &[u8]) -> Result<Vec<LibrivoxAuthor>, ProviderError> {
    let envelope: RawAuthorsEnvelope = serde_json::from_slice(bytes)
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    Ok(envelope
        .authors
        .into_iter()
        .take(MAX_AUTHORS)
        .filter_map(|author| normalize_author(author).ok())
        .collect())
}

fn normalize_book(raw: RawBook) -> Result<LibrivoxBook, ProviderError> {
    let book_id = required_positive_u64(raw.id.as_ref(), "book ID")?;
    let title = required_text(raw.title.as_deref(), MAX_TITLE_BYTES, "book title")?;
    let webpage_url = parse_librivox_page_url(raw.url_librivox.as_deref(), "book webpage URL")?;
    let authors = raw
        .authors
        .into_iter()
        .take(MAX_AUTHORS)
        .filter_map(|author| normalize_author(author).ok())
        .collect::<Vec<_>>();
    let genres = raw
        .genres
        .into_iter()
        .take(MAX_GENRES)
        .filter_map(|genre| {
            Some(LibrivoxGenre {
                genre_id: optional_positive_u64(genre.id.as_ref()),
                name: bounded_nonempty_text(genre.name.as_deref()?, MAX_NAME_BYTES)?,
            })
        })
        .collect();
    let sections = raw
        .sections
        .into_iter()
        .take(MAX_SECTIONS)
        .filter_map(|section| normalize_section(section).ok())
        .collect();
    Ok(LibrivoxBook {
        book_id,
        title,
        description: raw
            .description
            .as_deref()
            .map(|value| normalize_html_text(value, MAX_DESCRIPTION_BYTES))
            .filter(|value| !value.is_empty()),
        language: raw
            .language
            .as_deref()
            .and_then(|value| bounded_nonempty_text(value, MAX_LANGUAGE_BYTES)),
        copyright_year: optional_u16(raw.copyright_year.as_ref()),
        duration_seconds: optional_positive_u64(raw.totaltimesecs.as_ref()),
        webpage_url,
        text_source_url: parse_optional_public_url(raw.url_text_source.as_deref()),
        rss_url: parse_optional_librivox_url(raw.url_rss.as_deref()),
        zip_url: parse_optional_archive_url(raw.url_zip_file.as_deref(), ArchiveKind::Zip),
        archive_url: parse_optional_archive_url(raw.url_iarchive.as_deref(), ArchiveKind::Page),
        covers: LibrivoxCoverArt {
            jpeg_url: parse_optional_archive_url(raw.coverart_jpg.as_deref(), ArchiveKind::Image),
            thumbnail_url: parse_optional_archive_url(
                raw.coverart_thumbnail.as_deref(),
                ArchiveKind::Image,
            ),
            pdf_url: parse_optional_archive_url(raw.coverart_pdf.as_deref(), ArchiveKind::Pdf),
        },
        authors,
        genres,
        keywords: Vec::new(),
        sections,
    })
}

fn normalize_author(raw: RawAuthor) -> Result<LibrivoxAuthor, ProviderError> {
    let author_id = required_positive_u64(raw.id.as_ref(), "author ID")?;
    let first_name = raw
        .first_name
        .as_deref()
        .and_then(|value| bounded_nonempty_text(value, MAX_NAME_BYTES));
    let last_name = raw
        .last_name
        .as_deref()
        .and_then(|value| bounded_nonempty_text(value, MAX_NAME_BYTES));
    let display_name = [first_name.as_deref(), last_name.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
    if display_name.is_empty() {
        return Err(ProviderError::InvalidResponse(
            "LibriVox author omitted a name".to_owned(),
        ));
    }
    let webpage_url = Url::parse(&format!("https://librivox.org/author/{author_id}"))
        .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
    Ok(LibrivoxAuthor {
        author_id,
        display_name,
        first_name,
        last_name,
        birth_year: optional_u16(raw.dob.as_ref()),
        death_year: optional_u16(raw.dod.as_ref()),
        webpage_url,
    })
}

fn normalize_section(raw: RawSection) -> Result<LibrivoxSection, ProviderError> {
    let section_id = required_positive_u64(raw.id.as_ref(), "section ID")?;
    let number = required_positive_u64(raw.section_number.as_ref(), "section number")?
        .try_into()
        .map_err(|_| {
            ProviderError::InvalidResponse("LibriVox section number is too large".to_owned())
        })?;
    let title = required_text(raw.title.as_deref(), MAX_TITLE_BYTES, "section title")?;
    let preferred_audio_url = parse_archive_url(
        raw.listen_url.as_deref(),
        ArchiveKind::Mp3,
        "section audio URL",
    )?;
    let readers = raw
        .readers
        .into_iter()
        .take(MAX_READERS)
        .filter_map(|reader| {
            Some(LibrivoxReader {
                reader_id: optional_positive_u64(reader.reader_id.as_ref()),
                display_name: bounded_nonempty_text(
                    reader.display_name.as_deref()?,
                    MAX_NAME_BYTES,
                )?,
            })
        })
        .collect();
    Ok(LibrivoxSection {
        section_id,
        number,
        title,
        language: raw
            .language
            .as_deref()
            .and_then(|value| bounded_nonempty_text(value, MAX_LANGUAGE_BYTES)),
        duration_seconds: optional_positive_u64(raw.playtime.as_ref()),
        readers,
        preferred_audio_url,
        fallback_audio_url: None,
    })
}

fn required_positive_u64(value: Option<&RawScalar>, field: &str) -> Result<u64, ProviderError> {
    optional_positive_u64(value).ok_or_else(|| {
        ProviderError::InvalidResponse(format!("LibriVox {field} is missing or invalid"))
    })
}

fn optional_positive_u64(value: Option<&RawScalar>) -> Option<u64> {
    value?
        .text()
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
}

fn optional_u16(value: Option<&RawScalar>) -> Option<u16> {
    value?.text().trim().parse::<u16>().ok()
}

fn required_text(
    value: Option<&str>,
    max_bytes: usize,
    field: &str,
) -> Result<String, ProviderError> {
    value
        .and_then(|value| bounded_nonempty_text(value, max_bytes))
        .ok_or_else(|| {
            ProviderError::InvalidResponse(format!("LibriVox {field} is missing or invalid"))
        })
}

fn bounded_nonempty_text(value: &str, max_bytes: usize) -> Option<String> {
    let safe = sanitize_terminal_text(value, false);
    let normalized = safe.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty() && normalized.len() <= max_bytes).then_some(normalized)
}

/// Removes terminal instructions and unsafe control characters from remote text.
///
/// ANSI/ECMA-48 escape sequences are discarded as complete units, including
/// their seven-bit `ESC` and eight-bit C1 forms. Tabs become ordinary spaces;
/// every other C0/C1 control is removed. Multiline descriptions may retain
/// normalized line feeds when `preserve_line_breaks` is true, while display
/// names and titles are always reduced to terminal-safe single-line text.
fn sanitize_terminal_text(value: &str, preserve_line_breaks: bool) -> String {
    #[derive(Clone, Copy)]
    enum State {
        Text,
        Escape,
        EscapeIntermediate,
        Csi,
        Osc,
        OscEscape,
        ControlString,
        ControlStringEscape,
    }

    fn is_escape_intermediate(character: char) -> bool {
        ('\u{20}'..='\u{2f}').contains(&character)
    }

    fn is_escape_final(character: char) -> bool {
        ('\u{30}'..='\u{7e}').contains(&character)
    }

    fn is_csi_final(character: char) -> bool {
        ('\u{40}'..='\u{7e}').contains(&character)
    }

    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    let mut state = State::Text;
    while let Some(character) = characters.next() {
        state = match state {
            State::Text => match character {
                '\u{1b}' => State::Escape,
                '\u{9b}' => State::Csi,
                '\u{9d}' => State::Osc,
                '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => State::ControlString,
                '\r' => {
                    if characters.peek() == Some(&'\n') {
                        characters.next();
                    }
                    if preserve_line_breaks {
                        output.push('\n');
                    } else {
                        output.push(' ');
                    }
                    State::Text
                }
                '\n' => {
                    output.push(if preserve_line_breaks { '\n' } else { ' ' });
                    State::Text
                }
                '\t' => {
                    output.push(' ');
                    State::Text
                }
                character if character.is_control() => State::Text,
                character => {
                    output.push(character);
                    State::Text
                }
            },
            State::Escape => match character {
                '[' => State::Csi,
                ']' => State::Osc,
                'P' | 'X' | '^' | '_' => State::ControlString,
                character if is_escape_intermediate(character) => State::EscapeIntermediate,
                _ => State::Text,
            },
            State::EscapeIntermediate => {
                if character == '\u{1b}' {
                    State::Escape
                } else if is_escape_final(character) {
                    State::Text
                } else {
                    State::EscapeIntermediate
                }
            }
            State::Csi => {
                if character == '\u{1b}' {
                    State::Escape
                } else if character == '\u{9c}' || is_csi_final(character) {
                    State::Text
                } else {
                    State::Csi
                }
            }
            State::Osc => match character {
                '\u{7}' | '\u{9c}' => State::Text,
                '\u{1b}' => State::OscEscape,
                _ => State::Osc,
            },
            State::OscEscape => {
                if character == '\\' {
                    State::Text
                } else if character == '\u{1b}' {
                    State::OscEscape
                } else {
                    State::Osc
                }
            }
            State::ControlString => match character {
                '\u{7}' | '\u{9c}' => State::Text,
                '\u{1b}' => State::ControlStringEscape,
                _ => State::ControlString,
            },
            State::ControlStringEscape => {
                if character == '\\' {
                    State::Text
                } else if character == '\u{1b}' {
                    State::ControlStringEscape
                } else {
                    State::ControlString
                }
            }
        };
    }
    output
}

#[derive(Clone, Copy)]
enum ArchiveKind {
    Page,
    Mp3,
    Zip,
    Image,
    Pdf,
}

fn parse_librivox_page_url(raw: Option<&str>, field: &str) -> Result<Url, ProviderError> {
    let url =
        Url::parse(raw.ok_or_else(|| {
            ProviderError::InvalidResponse(format!("LibriVox {field} is missing"))
        })?)
        .map_err(|error| ProviderError::InvalidResponse(format!("invalid {field}: {error}")))?;
    if !safe_librivox_url(&url) {
        return Err(ProviderError::InvalidResponse(format!(
            "LibriVox {field} left the fixed HTTPS origin"
        )));
    }
    Ok(url)
}

fn parse_optional_librivox_url(raw: Option<&str>) -> Option<Url> {
    let url = Url::parse(raw?.trim()).ok()?;
    safe_librivox_url(&url).then_some(url)
}

fn safe_librivox_url(url: &Url) -> bool {
    url.as_str().len() <= MAX_URL_BYTES
        && url.scheme() == "https"
        && url.host_str() == Some("librivox.org")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn parse_optional_public_url(raw: Option<&str>) -> Option<Url> {
    let url = Url::parse(raw?.trim()).ok()?;
    (url.as_str().len() <= MAX_URL_BYTES
        && matches!(url.scheme(), "http" | "https")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && !remote_url_has_non_public_host(&url))
    .then_some(url)
}

fn parse_archive_url(
    raw: Option<&str>,
    kind: ArchiveKind,
    field: &str,
) -> Result<Url, ProviderError> {
    parse_optional_archive_url(raw, kind).ok_or_else(|| {
        ProviderError::InvalidResponse(format!("LibriVox {field} is missing or unsafe"))
    })
}

fn parse_optional_archive_url(raw: Option<&str>, kind: ArchiveKind) -> Option<Url> {
    let url = Url::parse(raw?.trim()).ok()?;
    let host = url.host_str()?;
    let path = url.path().to_ascii_lowercase();
    let path_matches = match kind {
        ArchiveKind::Page => path.starts_with("/details/"),
        ArchiveKind::Mp3 => path.starts_with("/download/") && path.ends_with(".mp3"),
        ArchiveKind::Zip => path.starts_with("/download/") && path.ends_with(".zip"),
        ArchiveKind::Image => {
            path.starts_with("/download/")
                && [".jpg", ".jpeg", ".png", ".webp"]
                    .iter()
                    .any(|suffix| path.ends_with(suffix))
        }
        ArchiveKind::Pdf => path.starts_with("/download/") && path.ends_with(".pdf"),
    };
    (url.as_str().len() <= MAX_URL_BYTES
        && url.scheme() == "https"
        && matches!(host, "archive.org" | "www.archive.org")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && path_matches)
        .then_some(url)
}

fn enrich_book_from_html(book: &mut LibrivoxBook, html: &str) -> Result<(), ProviderError> {
    if html.len() > DEFAULT_MAX_HTML_BYTES {
        return Err(ProviderError::ResponseTooLarge {
            limit: DEFAULT_MAX_HTML_BYTES,
        });
    }
    let anchors = html_anchors(html);
    let mut keywords = Vec::new();
    for anchor in &anchors {
        if keywords.len() == MAX_KEYWORDS {
            break;
        }
        let Some(url) = parse_optional_librivox_url(Some(&anchor.href)) else {
            continue;
        };
        let Some(id) = url.path().strip_prefix("/keywords/") else {
            continue;
        };
        if id.is_empty()
            || !id
                .trim_matches('/')
                .bytes()
                .all(|byte| byte.is_ascii_digit())
        {
            continue;
        }
        let Some(name) = bounded_nonempty_text(&anchor.text, MAX_NAME_BYTES) else {
            continue;
        };
        if !keywords
            .iter()
            .any(|existing: &LibrivoxKeyword| existing.webpage_url == url)
        {
            keywords.push(LibrivoxKeyword {
                name,
                webpage_url: url,
            });
        }
    }
    book.keywords = keywords;
    let full_quality = anchors
        .iter()
        .filter(|anchor| {
            anchor
                .classes
                .split_ascii_whitespace()
                .any(|class| class == "chapter-name")
        })
        // Preserve one position for every chapter anchor. Dropping an unsafe
        // URL here would shift every later URL onto the preceding API section.
        .map(|anchor| parse_optional_archive_url(Some(&anchor.href), ArchiveKind::Mp3))
        .collect::<Vec<_>>();
    for section in &mut book.sections {
        // API normalization may have rejected an earlier malformed section.
        // Address the page row by the explicit one-based section number so a
        // surviving chapter can never inherit another chapter's audio URL.
        let Some(anchor_index) = section
            .number
            .checked_sub(1)
            .and_then(|number| usize::try_from(number).ok())
        else {
            continue;
        };
        let Some(full_quality_url) = full_quality.get(anchor_index).and_then(Option::as_ref) else {
            continue;
        };
        if section.preferred_audio_url != *full_quality_url {
            section.fallback_audio_url = Some(section.preferred_audio_url.clone());
            section.preferred_audio_url = full_quality_url.clone();
        }
    }
    Ok(())
}

fn parse_author_html(
    author: LibrivoxAuthor,
    html: &str,
) -> Result<LibrivoxAuthorDetails, ProviderError> {
    if html.len() > DEFAULT_MAX_HTML_BYTES {
        return Err(ProviderError::ResponseTooLarge {
            limit: DEFAULT_MAX_HTML_BYTES,
        });
    }
    let description = class_inner_html(html, "description")
        .map(|value| normalize_html_text(value, MAX_DESCRIPTION_BYTES))
        .filter(|value| !value.is_empty());
    let wikipedia_url = html_anchors(html).into_iter().find_map(|anchor| {
        let url = Url::parse(&anchor.href).ok()?;
        let host = url.host_str()?;
        (url.scheme() == "https"
            && (host == "wikipedia.org" || host.ends_with(".wikipedia.org"))
            && url.username().is_empty()
            && url.password().is_none()
            && url.path().starts_with("/wiki/"))
        .then_some(url)
    });
    Ok(LibrivoxAuthorDetails {
        author,
        description,
        wikipedia_url,
        books: Vec::new(),
        next_offset: None,
    })
}

fn empty_author_details(author: LibrivoxAuthor) -> LibrivoxAuthorDetails {
    LibrivoxAuthorDetails {
        author,
        description: None,
        wikipedia_url: None,
        books: Vec::new(),
        next_offset: None,
    }
}

#[derive(Debug)]
struct HtmlAnchor {
    href: String,
    classes: String,
    text: String,
}

fn html_anchors(html: &str) -> Vec<HtmlAnchor> {
    let lower = html.to_ascii_lowercase();
    let mut anchors = Vec::new();
    let mut cursor = 0;
    while anchors.len() < MAX_SECTIONS.saturating_add(MAX_KEYWORDS)
        && let Some(relative_start) = lower[cursor..].find("<a")
    {
        let start = cursor.saturating_add(relative_start);
        let Some(relative_tag_end) = lower[start..].find('>') else {
            break;
        };
        let tag_end = start.saturating_add(relative_tag_end);
        let tag = &html[start..=tag_end];
        let content_start = tag_end.saturating_add(1);
        let Some(relative_close) = lower[content_start..].find("</a>") else {
            break;
        };
        let content_end = content_start.saturating_add(relative_close);
        if let Some(href) = html_attribute(tag, "href") {
            anchors.push(HtmlAnchor {
                href,
                classes: html_attribute(tag, "class").unwrap_or_default(),
                text: normalize_html_text(&html[content_start..content_end], MAX_NAME_BYTES),
            });
        }
        cursor = content_end.saturating_add("</a>".len());
    }
    anchors
}

fn html_attribute(tag: &str, attribute: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{attribute}=");
    let at = lower.find(&needle)?.saturating_add(needle.len());
    let quote = tag[at..].chars().next()?;
    if !matches!(quote, '\'' | '"') {
        return None;
    }
    let start = at.saturating_add(quote.len_utf8());
    let end = start.saturating_add(tag[start..].find(quote)?);
    Some(decode_html_entities(&tag[start..end]))
}

fn class_inner_html<'a>(html: &'a str, class_name: &str) -> Option<&'a str> {
    let lower = html.to_ascii_lowercase();
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find("class=") {
        let class_at = cursor.saturating_add(relative);
        let tag_start = lower[..class_at].rfind('<')?;
        let tag_end = class_at.saturating_add(lower[class_at..].find('>')?);
        let tag = &html[tag_start..=tag_end];
        if html_attribute(tag, "class").is_some_and(|classes| {
            classes
                .split_ascii_whitespace()
                .any(|class| class == class_name)
        }) {
            let tag_name = lower[tag_start.saturating_add(1)..]
                .split(|character: char| character.is_ascii_whitespace() || character == '>')
                .next()?;
            let closing = format!("</{tag_name}>");
            let content_start = tag_end.saturating_add(1);
            let content_end = lower[content_start..]
                .find(&closing)
                .map_or(html.len(), |relative| {
                    content_start.saturating_add(relative)
                });
            return Some(&html[content_start..content_end]);
        }
        cursor = tag_end.saturating_add(1);
    }
    None
}

fn normalize_html_text(html: &str, max_bytes: usize) -> String {
    let mut plain = String::with_capacity(html.len().min(max_bytes));
    let mut cursor = 0;
    while cursor < html.len() {
        let Some(relative_start) = html[cursor..].find('<') else {
            plain.push_str(&html[cursor..]);
            break;
        };
        let start = cursor.saturating_add(relative_start);
        plain.push_str(&html[cursor..start]);
        let Some(relative_end) = html[start..].find('>') else {
            break;
        };
        let end = start.saturating_add(relative_end);
        let tag = html[start.saturating_add(1)..end]
            .trim()
            .to_ascii_lowercase();
        if tag.starts_with("br")
            || tag.starts_with("/p")
            || tag.starts_with("/div")
            || tag.starts_with("/li")
        {
            plain.push('\n');
        }
        cursor = end.saturating_add(1);
    }
    let decoded = decode_html_entities(&plain);
    let safe = sanitize_terminal_text(&decoded, true);
    let mut lines = Vec::new();
    for line in safe.lines() {
        let normalized = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !normalized.is_empty() {
            lines.push(normalized);
        }
    }
    truncate_utf8(&lines.join("\n"), max_bytes)
}

fn decode_html_entities(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(ampersand) = remaining.find('&') {
        output.push_str(&remaining[..ampersand]);
        remaining = &remaining[ampersand..];
        let Some(semicolon) = remaining.find(';').filter(|index| *index <= 12) else {
            output.push('&');
            remaining = &remaining[1..];
            continue;
        };
        let entity = &remaining[1..semicolon];
        let decoded = match entity {
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "nbsp" => Some(' '),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| {
                    entity
                        .strip_prefix('#')
                        .and_then(|decimal| decimal.parse().ok())
                })
                .and_then(char::from_u32),
        };
        if let Some(character) = decoded {
            output.push(character);
        } else {
            output.push_str(&remaining[..=semicolon]);
        }
        remaining = &remaining[semicolon.saturating_add(1)..];
    }
    output.push_str(remaining);
    output
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut boundary = max_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value[..boundary].to_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct MockTransport {
        replies: Mutex<VecDeque<Result<Vec<u8>, ProviderError>>>,
        requests: Mutex<Vec<Url>>,
    }

    impl MockTransport {
        fn from_replies(replies: impl IntoIterator<Item = Result<Vec<u8>, ProviderError>>) -> Self {
            Self {
                replies: Mutex::new(replies.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn request_urls(&self) -> Vec<Url> {
            self.requests.lock().expect("request log").clone()
        }
    }

    impl LibrivoxTransport for MockTransport {
        fn fetch(
            &self,
            url: &Url,
            _accept: &'static str,
            max_bytes: usize,
        ) -> Result<Vec<u8>, ProviderError> {
            self.requests.lock().expect("request log").push(url.clone());
            let reply = self
                .replies
                .lock()
                .expect("reply queue")
                .pop_front()
                .expect("unexpected request")?;
            if reply.len() > max_bytes {
                return Err(ProviderError::ResponseTooLarge { limit: max_bytes });
            }
            Ok(reply)
        }
    }

    const BOOKS_JSON: &str = r#"{
		"books": [{
			"id": "5936",
			"title": "With the Turks in Palestine",
			"description": "A first line.<br /><br />A second &amp; final line.",
			"url_text_source": "https://www.gutenberg.org/ebooks/10338",
			"language": "English",
			"copyright_year": "1916",
			"num_sections": "1",
			"url_rss": "https://librivox.org/rss/5936",
			"url_zip_file": "https://archive.org/download/withturkspalestine_1301_librivox/withturkspalestine_1301_librivox_64kb_mp3.zip",
			"url_librivox": "https://librivox.org/with-the-turks-in-palestine-by-alexander-aaronsohn/",
			"url_iarchive": "https://archive.org/details/withturkspalestine_1301_librivox",
			"totaltime": "1:54:49",
			"totaltimesecs": 6889,
			"authors": [{
				"id": "3595",
				"first_name": "Alexander",
				"last_name": "Aaronsohn",
				"dob": "1888",
				"dod": "1948"
			}],
			"coverart_jpg": "https://archive.org/download/LibrivoxCdCoverArt27/withturkspalestine_1301.jpg",
			"coverart_pdf": "https://archive.org/download/LibrivoxCdCoverArt27/withturkspalestine_1301.pdf",
			"coverart_thumbnail": "https://archive.org/download/LibrivoxCdCoverArt27/withturkspalestine_1301_thumb.jpg",
			"sections": [{
				"id": "77736",
				"section_number": "1",
				"title": "Introduction and Ch. I: Zicron-Jacob",
				"listen_url": "https://www.archive.org/download/withturkspalestine_1301_librivox/withtheturksinpalestine_01_aaronsohn_64kb.mp3",
				"language": "English",
				"playtime": "667",
				"readers": [{"reader_id": "6798", "display_name": "Aesthete's Readings"}]
			}],
			"genres": [
				{"id": "73", "name": "War & Military"},
				{"id": "111", "name": "Memoirs"},
				{"id": "117", "name": "Modern (20th C)"}
			]
		}]
	}"#;

    const BOOK_HTML: &str = r#"<!doctype html>
	<html><body>
		<p class="book-page-genre primary_keywords_string"><span>Keyword(s):</span>
			<a href="https://librivox.org/keywords/49">history</a>,
			<a href="https://librivox.org/keywords/631">World War I</a>
		</p>
		<table class="chapter-download"><tbody><tr>
			<td><a class="play-btn" href="https://www.archive.org/download/withturkspalestine_1301_librivox/withtheturksinpalestine_01_aaronsohn_64kb.mp3">Play</a></td>
			<td><a class="chapter-name" href="https://www.archive.org/download/withturkspalestine_1301_librivox/withtheturksinpalestine_01_aaronsohn.mp3">Introduction and Ch. I</a></td>
		</tr></tbody></table>
	</body></html>"#;

    const AUTHOR_JSON: &str = r#"{
		"authors": [{
			"id": "3595",
			"first_name": "Alexander",
			"last_name": "Aaronsohn",
			"dob": "1888",
			"dod": "1948"
		}]
	}"#;

    const AUTHOR_HTML: &str = r#"<!doctype html><html><body>
		<div class="page author-page">
			<h1>Alexander Aaronsohn <span class="dod-dob">(1888 - 1948)</span></h1>
			<p class="description">Alexander Aaronsohn was an author &amp; activist.<br>He wrote about Palestine.</p>
			<p><a href="https://en.wikipedia.org/wiki/Alexander_Aaronsohn">Wiki</a></p>
		</div>
	</body></html>"#;

    #[test]
    fn parses_book_metadata_genres_and_safe_chapter_fallback() {
        let mut books = parse_books_json(BOOKS_JSON.as_bytes()).expect("valid fixture");
        assert_eq!(books.len(), 1);
        let mut book = books.remove(0);
        assert_eq!(
            book.genres
                .iter()
                .map(|genre| genre.name.as_str())
                .collect::<Vec<_>>(),
            ["War & Military", "Memoirs", "Modern (20th C)"]
        );
        assert_eq!(
            book.description.as_deref(),
            Some("A first line.\nA second & final line.")
        );
        assert_eq!(book.sections.len(), 1);
        assert!(book.sections[0].fallback_audio_url.is_none());
        assert!(
            book.sections[0]
                .preferred_audio_url
                .as_str()
                .ends_with("_64kb.mp3")
        );

        enrich_book_from_html(&mut book, BOOK_HTML).expect("valid public page");
        assert_eq!(
            book.keywords
                .iter()
                .map(|keyword| keyword.name.as_str())
                .collect::<Vec<_>>(),
            ["history", "World War I"]
        );
        assert!(
            book.sections[0]
                .preferred_audio_url
                .as_str()
                .ends_with("aaronsohn.mp3")
        );
        assert!(
            book.sections[0]
                .fallback_audio_url
                .as_ref()
                .expect("64 kbps fallback")
                .as_str()
                .ends_with("_64kb.mp3")
        );
    }

    #[test]
    fn rejects_an_untrusted_full_quality_chapter_and_keeps_the_api_fallback() {
        let mut book = parse_books_json(BOOKS_JSON.as_bytes())
            .expect("valid fixture")
            .remove(0);
        let html = BOOK_HTML.replace(
			"https://www.archive.org/download/withturkspalestine_1301_librivox/withtheturksinpalestine_01_aaronsohn.mp3",
			"https://archive.org.example/download/withturkspalestine_1301_librivox/withtheturksinpalestine_01_aaronsohn.mp3",
		);
        enrich_book_from_html(&mut book, &html).expect("unsafe optional link is ignored");
        assert!(
            book.sections[0]
                .preferred_audio_url
                .as_str()
                .ends_with("_64kb.mp3")
        );
        assert!(book.sections[0].fallback_audio_url.is_none());
    }

    #[test]
    fn rejected_chapter_anchor_cannot_shift_a_later_url_onto_the_wrong_section() {
        let mut fixture: serde_json::Value =
            serde_json::from_str(BOOKS_JSON).expect("book JSON fixture");
        fixture["books"][0]["sections"]
            .as_array_mut()
            .expect("section array")
            .push(serde_json::json!({
                "id": "77737",
                "section_number": "2",
                "title": "Chapter II",
                "listen_url": "https://www.archive.org/download/withturkspalestine_1301_librivox/withtheturksinpalestine_02_aaronsohn_64kb.mp3",
                "language": "English",
                "playtime": "668",
                "readers": [{"reader_id": "6798", "display_name": "Aesthete's Readings"}]
            }));
        let mut book = parse_books_json(&serde_json::to_vec(&fixture).expect("serialized fixture"))
            .expect("valid two-section fixture")
            .remove(0);
        let html = r#"<!doctype html><html><body><table class="chapter-download"><tbody>
			<tr><td><a class="chapter-name" href="https://archive.org.example/download/withturkspalestine_1301_librivox/withtheturksinpalestine_01_aaronsohn.mp3">Chapter I</a></td></tr>
			<tr><td><a class="chapter-name" href="https://www.archive.org/download/withturkspalestine_1301_librivox/withtheturksinpalestine_02_aaronsohn.mp3">Chapter II</a></td></tr>
		</tbody></table></body></html>"#;

        enrich_book_from_html(&mut book, html).expect("unsafe optional link is ignored in place");

        assert!(
            book.sections[0]
                .preferred_audio_url
                .as_str()
                .ends_with("_01_aaronsohn_64kb.mp3"),
            "the second full-quality URL must not be assigned to section one"
        );
        assert!(book.sections[0].fallback_audio_url.is_none());
        assert!(
            book.sections[1]
                .preferred_audio_url
                .as_str()
                .ends_with("_02_aaronsohn.mp3")
        );
        assert!(
            book.sections[1]
                .fallback_audio_url
                .as_ref()
                .expect("section-two API fallback")
                .as_str()
                .ends_with("_02_aaronsohn_64kb.mp3")
        );
    }

    #[test]
    fn rejected_api_section_cannot_shift_an_earlier_url_onto_a_later_section() {
        let mut fixture: serde_json::Value =
            serde_json::from_str(BOOKS_JSON).expect("book JSON fixture");
        fixture["books"][0]["sections"][0]["listen_url"] =
            serde_json::json!("https://archive.org.example/download/unsafe/chapter-01.mp3");
        fixture["books"][0]["sections"]
            .as_array_mut()
            .expect("section array")
            .push(serde_json::json!({
                "id": "77737",
                "section_number": "2",
                "title": "Chapter II",
                "listen_url": "https://www.archive.org/download/withturkspalestine_1301_librivox/withtheturksinpalestine_02_aaronsohn_64kb.mp3",
                "language": "English",
                "playtime": "668",
                "readers": [{"reader_id": "6798", "display_name": "Aesthete's Readings"}]
            }));
        let mut book = parse_books_json(&serde_json::to_vec(&fixture).expect("serialized fixture"))
            .expect("fixture retains its valid second section")
            .remove(0);
        assert_eq!(book.sections.len(), 1);
        assert_eq!(book.sections[0].number, 2);
        let html = r#"<!doctype html><html><body><table class="chapter-download"><tbody>
			<tr><td><a class="chapter-name" href="https://www.archive.org/download/withturkspalestine_1301_librivox/withtheturksinpalestine_01_aaronsohn.mp3">Chapter I</a></td></tr>
			<tr><td><a class="chapter-name" href="https://www.archive.org/download/withturkspalestine_1301_librivox/withtheturksinpalestine_02_aaronsohn.mp3">Chapter II</a></td></tr>
		</tbody></table></body></html>"#;

        enrich_book_from_html(&mut book, html).expect("valid page enrichment");

        assert!(
            book.sections[0]
                .preferred_audio_url
                .as_str()
                .ends_with("_02_aaronsohn.mp3"),
            "section two must receive the second chapter URL"
        );
        assert!(
            book.sections[0]
                .fallback_audio_url
                .as_ref()
                .expect("section-two API fallback")
                .as_str()
                .ends_with("_02_aaronsohn_64kb.mp3")
        );
    }

    #[test]
    fn parses_bounded_author_description_and_wikipedia_link() {
        let author = parse_authors_json(AUTHOR_JSON.as_bytes())
            .expect("valid author fixture")
            .remove(0);
        let details = parse_author_html(author, AUTHOR_HTML).expect("valid author HTML");
        assert_eq!(
            details.description.as_deref(),
            Some("Alexander Aaronsohn was an author & activist.\nHe wrote about Palestine.")
        );
        assert_eq!(
            details.wikipedia_url.as_ref().map(Url::as_str),
            Some("https://en.wikipedia.org/wiki/Alexander_Aaronsohn")
        );
    }

    #[test]
    fn remote_api_text_fields_cannot_retain_terminal_controls() {
        let mut fixture: serde_json::Value =
            serde_json::from_str(BOOKS_JSON).expect("book JSON fixture");
        let book = &mut fixture["books"][0];
        book["title"] = serde_json::Value::String(
            "\u{1b}[31mWith the Turks\u{1b}[0m in Palestine\u{7}".to_owned(),
        );
        book["description"] = serde_json::Value::String(
            "First\u{1b}]0;forged title\u{7} line.<br>Second\u{0} line.".to_owned(),
        );
        book["language"] = serde_json::Value::String("Eng\u{9b}31mlish".to_owned());
        book["authors"][0]["first_name"] =
            serde_json::Value::String("Alex\u{1b}[2Kander".to_owned());
        book["authors"][0]["last_name"] = serde_json::Value::String("Aaron\u{9b}0msohn".to_owned());
        book["genres"][0]["name"] =
            serde_json::Value::String("War\u{1b}]2;spoof\u{7} & Military".to_owned());
        book["sections"][0]["title"] =
            serde_json::Value::String("Intro\u{1b}[2Jduction".to_owned());
        book["sections"][0]["language"] = serde_json::Value::String("Eng\u{9b}31mlish".to_owned());
        book["sections"][0]["readers"][0]["display_name"] =
            serde_json::Value::String("Aesthete\u{1b}[31m's Readings".to_owned());

        let book = parse_books_json(&serde_json::to_vec(&fixture).expect("serialized fixture"))
            .expect("terminal controls are sanitized")
            .remove(0);

        assert_eq!(book.title, "With the Turks in Palestine");
        assert_eq!(
            book.description.as_deref(),
            Some("First line.\nSecond line.")
        );
        assert_eq!(book.language.as_deref(), Some("English"));
        assert_eq!(book.authors[0].display_name, "Alexander Aaronsohn");
        assert_eq!(book.genres[0].name, "War & Military");
        assert_eq!(book.sections[0].title, "Introduction");
        assert_eq!(book.sections[0].language.as_deref(), Some("English"));
        assert_eq!(
            book.sections[0].readers[0].display_name,
            "Aesthete's Readings"
        );
        for value in [
            book.title.as_str(),
            book.description.as_deref().expect("description"),
            book.language.as_deref().expect("language"),
            book.authors[0].display_name.as_str(),
            book.genres[0].name.as_str(),
            book.sections[0].title.as_str(),
            book.sections[0]
                .language
                .as_deref()
                .expect("section language"),
            book.sections[0].readers[0].display_name.as_str(),
        ] {
            assert!(
                value
                    .chars()
                    .all(|character| character == '\n' || !character.is_control()),
                "remote text retained a terminal control: {value:?}"
            );
        }
    }

    #[test]
    fn html_numeric_control_entities_are_removed_without_losing_line_breaks() {
        let author = parse_authors_json(AUTHOR_JSON.as_bytes())
            .expect("valid author fixture")
            .remove(0);
        let html = r#"<!doctype html><html><body>
		<div class="page author-page">
			<p class="description">First&#10;line&#x1b;[31m red&#27;[0m&#x9b;2m text&#0;.<br>Second&#13;&#10;line.</p>
		</div>
	</body></html>"#;
        let details = parse_author_html(author, html).expect("valid author HTML");
        assert_eq!(
            details.description.as_deref(),
            Some("First\nline red text.\nSecond\nline.")
        );

        let mut book = parse_books_json(BOOKS_JSON.as_bytes())
            .expect("valid book fixture")
            .remove(0);
        let keyword_html =
            r#"<a href="https://librivox.org/keywords/49">his&#x1b;[31mtory&#x9b;0m</a>"#;
        enrich_book_from_html(&mut book, keyword_html).expect("valid keyword HTML");
        assert_eq!(book.keywords[0].name, "history");
        assert!(
            details
                .description
                .as_deref()
                .expect("description")
                .chars()
                .all(|character| character == '\n' || !character.is_control())
        );
    }

    #[test]
    fn builds_one_bounded_search_for_title_and_author() {
        let request = LibrivoxSearchRequest {
            title: Some("Palestine & memoir".to_owned()),
            author: Some("Aaronsohn".to_owned()),
            limit: 20,
            offset: 40,
        };
        let url = build_search_url(&request).expect("valid request");
        let pairs = url.query_pairs().collect::<Vec<_>>();
        assert!(pairs.contains(&("title".into(), "Palestine & memoir".into())));
        assert!(pairs.contains(&("author".into(), "Aaronsohn".into())));
        assert!(pairs.contains(&("extended".into(), "1".into())));
        assert!(pairs.contains(&("coverart".into(), "1".into())));
        assert!(pairs.contains(&("limit".into(), "20".into())));
        assert!(pairs.contains(&("offset".into(), "40".into())));
    }

    #[test]
    fn text_search_queries_title_and_author_then_deduplicates_book_ids() {
        let transport = Arc::new(MockTransport::from_replies([
            Ok(BOOKS_JSON.as_bytes().to_vec()),
            Ok(BOOKS_JSON.as_bytes().to_vec()),
        ]));
        let client = LibrivoxClient::with_transport(transport.clone(), 256 * 1024, 256 * 1024)
            .expect("mock client");
        let page = client
            .search(&LibrivoxSearchRequest::for_text("Aaronsohn", 20, 0))
            .expect("combined search");
        assert_eq!(page.books.len(), 1, "same book ID must not appear twice");
        let urls = transport.request_urls();
        assert_eq!(urls.len(), 2);
        assert!(urls[0].query_pairs().any(|(key, _)| key == "title"));
        assert!(urls[1].query_pairs().any(|(key, _)| key == "author"));
    }

    #[test]
    fn empty_supplementary_author_search_does_not_discard_title_matches() {
        let transport = Arc::new(MockTransport::from_replies([
            Ok(BOOKS_JSON.as_bytes().to_vec()),
            Err(ProviderError::HttpStatus(404)),
        ]));
        let client =
            LibrivoxClient::with_transport(transport, 256 * 1024, 256 * 1024).expect("mock client");

        let page = client
            .search(&LibrivoxSearchRequest::for_text(
                "With the Turks in Palestine",
                20,
                0,
            ))
            .expect("LibriVox 404 means no supplementary matches");

        assert_eq!(page.books.len(), 1);
        assert_eq!(page.books[0].book_id, 5936);
        assert_eq!(page.next_offset, None);
    }

    #[test]
    fn failed_supplementary_author_search_keeps_successful_title_matches() {
        let transport = Arc::new(MockTransport::from_replies([
            Ok(BOOKS_JSON.as_bytes().to_vec()),
            Err(ProviderError::Transport(
                "mock supplementary outage".to_owned(),
            )),
        ]));
        let client =
            LibrivoxClient::with_transport(transport, 256 * 1024, 256 * 1024).expect("mock client");

        let page = client
            .search(&LibrivoxSearchRequest::for_text(
                "With the Turks in Palestine",
                20,
                0,
            ))
            .expect("successful title matches survive a supplementary outage");

        assert_eq!(page.books.len(), 1);
        assert_eq!(page.books[0].book_id, 5936);
        assert_eq!(page.next_offset, None);
    }

    #[test]
    fn failed_supplementary_author_search_is_reported_when_title_found_nothing() {
        let transport = Arc::new(MockTransport::from_replies([
            Ok(br#"{"books": []}"#.to_vec()),
            Err(ProviderError::Transport(
                "mock supplementary outage".to_owned(),
            )),
        ]));
        let client =
            LibrivoxClient::with_transport(transport, 256 * 1024, 256 * 1024).expect("mock client");

        assert!(matches!(
            client.search(&LibrivoxSearchRequest::for_text("Aaronsohn", 20, 0)),
            Err(ProviderError::Transport(message)) if message == "mock supplementary outage"
        ));
    }

    #[test]
    fn optional_book_page_failure_keeps_api_description_genres_and_audio() {
        let transport = Arc::new(MockTransport::from_replies([
            Ok(BOOKS_JSON.as_bytes().to_vec()),
            Err(ProviderError::Transport("mock page outage".to_owned())),
        ]));
        let client = LibrivoxClient::with_transport(
            Arc::clone(&transport) as Arc<dyn LibrivoxTransport>,
            256 * 1024,
            256 * 1024,
        )
        .expect("mock client");
        let book = client
            .book_details(5936)
            .expect("API metadata remains usable");
        assert_eq!(book.genres.len(), 3);
        assert_eq!(
            book.description.as_deref(),
            Some("A first line.\nA second & final line.")
        );
        assert!(book.keywords.is_empty());
        assert!(
            book.sections[0]
                .preferred_audio_url
                .as_str()
                .ends_with("_64kb.mp3")
        );
    }

    #[test]
    fn author_books_are_filtered_by_exact_embedded_author_id() {
        let mut books: serde_json::Value =
            serde_json::from_str(BOOKS_JSON).expect("book JSON fixture");
        let unrelated = books["books"][0].clone();
        books["books"]
            .as_array_mut()
            .expect("books array")
            .push(unrelated);
        books["books"][1]["id"] = serde_json::Value::String("7000".to_owned());
        books["books"][1]["title"] =
            serde_json::Value::String("Unrelated surname match".to_owned());
        books["books"][1]["url_librivox"] =
            serde_json::Value::String("https://librivox.org/unrelated-surname-match/".to_owned());
        books["books"][1]["authors"][0]["id"] = serde_json::Value::String("9999".to_owned());
        let transport = Arc::new(MockTransport::from_replies([
            Ok(AUTHOR_JSON.as_bytes().to_vec()),
            Ok(AUTHOR_HTML.as_bytes().to_vec()),
            Ok(serde_json::to_vec(&books).expect("serialized books")),
        ]));
        let client = LibrivoxClient::with_transport(
            Arc::clone(&transport) as Arc<dyn LibrivoxTransport>,
            256 * 1024,
            256 * 1024,
        )
        .expect("mock client");
        let details = client.author_details(3595, 20, 0).expect("author details");
        assert_eq!(details.books.len(), 1);
        assert_eq!(details.books[0].book_id, 5936);
        assert_eq!(
            details.wikipedia_url.as_ref().map(Url::as_str),
            Some("https://en.wikipedia.org/wiki/Alexander_Aaronsohn")
        );
        let urls = transport.request_urls();
        assert!(urls[2].query_pairs().any(|(key, _)| key == "coverart"));
        assert!(
            !urls[2].query_pairs().any(|(key, _)| key == "extended"),
            "author rows must not download every chapter list"
        );
    }

    #[test]
    fn author_continuation_fetches_only_one_bounded_book_page() {
        let author = parse_authors_json(AUTHOR_JSON.as_bytes())
            .expect("author fixture")
            .remove(0);
        let transport = Arc::new(MockTransport::from_replies([Ok(BOOKS_JSON
            .as_bytes()
            .to_vec())]));
        let client = LibrivoxClient::with_transport(
            Arc::clone(&transport) as Arc<dyn LibrivoxTransport>,
            256 * 1024,
            256 * 1024,
        )
        .expect("mock client");

        let page = client
            .author_books(&author, 20, 40)
            .expect("author continuation");

        assert_eq!(page.offset, 40);
        assert_eq!(page.books.len(), 1);
        let urls = transport.request_urls();
        assert_eq!(urls.len(), 1);
        assert!(
            urls[0]
                .query_pairs()
                .any(|(key, value)| { key == "offset" && value == "40" })
        );
        assert!(
            urls[0]
                .query_pairs()
                .any(|(key, value)| { key == "limit" && value == "20" })
        );
    }

    #[test]
    fn response_limits_are_validated_before_constructing_a_client() {
        assert!(matches!(
            LibrivoxClient::with_options(Duration::ZERO, 1, 1),
            Err(ProviderError::InvalidRequest(_))
        ));
        assert!(matches!(
            LibrivoxClient::with_options(Duration::from_secs(1), 0, 1),
            Err(ProviderError::InvalidRequest(_))
        ));
        assert!(matches!(
            LibrivoxClient::with_options(Duration::from_secs(1), 1, 0),
            Err(ProviderError::InvalidRequest(_))
        ));
    }
}
