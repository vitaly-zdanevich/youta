//! Search and download adapter for [The Mod Archive](https://modarchive.org/).
//!
//! The official metadata API returns XML and requires a personal API key.
//! Module downloads use the archive's public download endpoint. Files remain
//! tracker modules; Youta passes them to a libopenmpt-capable replay backend
//! instead of treating them as pre-rendered PCM recordings.

use std::time::Duration;

use serde::Deserialize;
use url::Url;

use super::{DEFAULT_REQUEST_TIMEOUT, ProviderError, provider_agent};

const API_BASE: &str = "https://api.modarchive.org/";
const WEBSITE_BASE: &str = "https://modarchive.org/";
const DEFAULT_MAX_XML_BYTES: usize = 2 * 1024 * 1024;
const MAX_CONFIGURED_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Field in which The Mod Archive should search.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModuleSearchField {
    /// Match the file name or embedded song title.
    #[default]
    FilenameOrTitle,
    /// Require a match in both the file name and song title.
    FilenameAndTitle,
    /// Match only the archive file name.
    Filename,
    /// Match only the embedded song title.
    SongTitle,
    /// Match instrument names stored inside module metadata.
    Instruments,
    /// Match comments stored inside the module.
    Comments,
}

impl ModuleSearchField {
    const fn api_value(self) -> &'static str {
        match self {
            Self::FilenameOrTitle => "filename_or_songtitle",
            Self::FilenameAndTitle => "filename_and_songtitle",
            Self::Filename => "filename",
            Self::SongTitle => "songtitle",
            Self::Instruments => "module_instruments",
            Self::Comments => "module_comments",
        }
    }
}

/// Format filter supported by The Mod Archive API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleFormat {
    /// Composer 669.
    Composer669,
    /// AHX.
    Ahx,
    /// Delusion Digital Music File.
    Dmf,
    /// HivelyTracker.
    Hvl,
    /// Impulse Tracker.
    It,
    /// MED or OctaMED.
    Med,
    /// MO3 compressed module.
    Mo3,
    /// ProTracker-compatible MOD.
    Mod,
    /// MultiTracker.
    Mtm,
    /// Oktalyzer OCT.
    Oct,
    /// Oktalyzer OKT.
    Okt,
    /// Scream Tracker 3.
    S3m,
    /// Scream Tracker 2.
    Stm,
    /// FastTracker 2.
    Xm,
}

impl ModuleFormat {
    const fn api_value(self) -> &'static str {
        match self {
            Self::Composer669 => "669",
            Self::Ahx => "AHX",
            Self::Dmf => "DMF",
            Self::Hvl => "HVL",
            Self::It => "IT",
            Self::Med => "MED",
            Self::Mo3 => "MO3",
            Self::Mod => "MOD",
            Self::Mtm => "MTM",
            Self::Oct => "OCT",
            Self::Okt => "OKT",
            Self::S3m => "S3M",
            Self::Stm => "STM",
            Self::Xm => "XM",
        }
    }
}

/// One page of module-search parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleSearchRequest {
    /// Text to find.
    pub query: String,
    /// Metadata field to search.
    pub field: ModuleSearchField,
    /// Optional tracker format.
    pub format: Option<ModuleFormat>,
    /// One-based result page.
    pub page: u32,
}

impl ModuleSearchRequest {
    /// Creates a broad first-page module search.
    #[must_use]
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            field: ModuleSearchField::default(),
            format: None,
            page: 1,
        }
    }

    fn validate(&self) -> Result<(), ProviderError> {
        if self.query.trim().is_empty() {
            return Err(ProviderError::InvalidRequest(
                "module search query cannot be empty".to_owned(),
            ));
        }
        if self.query.len() > 512 {
            return Err(ProviderError::InvalidRequest(
                "module search query cannot exceed 512 bytes".to_owned(),
            ));
        }
        if !(1..=10_000).contains(&self.page) {
            return Err(ProviderError::InvalidRequest(
                "module search page must be between 1 and 10000".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Search result for one tracker module.
#[derive(Clone, Debug, PartialEq)]
pub struct ModuleSummary {
    /// Numeric Mod Archive identifier.
    pub id: u64,
    /// Downloaded file name.
    pub filename: String,
    /// Embedded song title.
    pub song_title: String,
    /// Tracker format reported by the archive.
    pub format: String,
    /// Pattern channel count.
    pub channels: Option<u16>,
    /// Module size in bytes.
    pub size_bytes: Option<u64>,
    /// Archive download count.
    pub hits: Option<u64>,
    /// Member rating when returned by the API.
    pub rating: Option<f64>,
    /// Instrument metadata stored inside the module.
    pub instruments: String,
    /// Comment text stored inside the module.
    pub comment: String,
    /// Canonical Mod Archive information page.
    pub webpage_url: Url,
    /// Public module download URL.
    pub download_url: Url,
}

/// Page returned by a module search.
#[derive(Clone, Debug, PartialEq)]
pub struct ModuleSearchPage {
    /// Current one-based page.
    pub page: u32,
    /// Total result pages reported by the API.
    pub total_pages: Option<u32>,
    /// Total matching modules reported by the API.
    pub total_results: Option<u64>,
    /// Module results.
    pub modules: Vec<ModuleSummary>,
}

/// Client for the official Mod Archive XML API.
#[derive(Clone)]
pub struct ModArchiveProvider {
    api_key: String,
    agent: ureq::Agent,
    max_response_bytes: usize,
}

impl ModArchiveProvider {
    /// Creates a provider with conservative timeout and response limits.
    pub fn new(api_key: impl Into<String>) -> Result<Self, ProviderError> {
        Self::with_options(api_key, DEFAULT_REQUEST_TIMEOUT, DEFAULT_MAX_XML_BYTES)
    }

    /// Creates a provider with explicit request limits.
    pub fn with_options(
        api_key: impl Into<String>,
        timeout: Duration,
        max_response_bytes: usize,
    ) -> Result<Self, ProviderError> {
        let api_key = api_key.into();
        if api_key.trim().is_empty() {
            return Err(ProviderError::InvalidRequest(
                "a Mod Archive API key is required for search".to_owned(),
            ));
        }
        if api_key.len() > 256
            || !api_key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ProviderError::InvalidRequest(
                "Mod Archive API key contains unexpected characters".to_owned(),
            ));
        }
        if timeout.is_zero() {
            return Err(ProviderError::InvalidRequest(
                "provider timeout must be greater than zero".to_owned(),
            ));
        }
        if !(1..=MAX_CONFIGURED_RESPONSE_BYTES).contains(&max_response_bytes) {
            return Err(ProviderError::InvalidRequest(format!(
                "response limit must be between 1 and {MAX_CONFIGURED_RESPONSE_BYTES} bytes"
            )));
        }
        Ok(Self {
            api_key,
            agent: provider_agent(timeout),
            max_response_bytes,
        })
    }

    /// Searches modules by file name, title, instruments, or comments.
    pub fn search(&self, request: &ModuleSearchRequest) -> Result<ModuleSearchPage, ProviderError> {
        request.validate()?;
        let mut url = Url::parse(API_BASE)
            .expect("the compile-time Mod Archive API URL must be valid")
            .join("xml-tools.php")
            .expect("the compile-time API path must join");
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("key", &self.api_key);
            query.append_pair("request", "search");
            query.append_pair("query", request.query.trim());
            query.append_pair("type", request.field.api_value());
            query.append_pair("page", &request.page.to_string());
            if let Some(format) = request.format {
                query.append_pair("format", format.api_value());
            }
        }

        let bytes = get_bounded_xml(&self.agent, &url, self.max_response_bytes)?;
        let raw: RawResponse = quick_xml::de::from_reader(bytes.as_slice())
            .map_err(|error| ProviderError::InvalidResponse(error.to_string()))?;
        if let Some(error) = nonempty(raw.error) {
            return Err(ProviderError::InvalidResponse(error));
        }
        let modules = raw
            .modules
            .into_iter()
            .map(ModuleSummary::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ModuleSearchPage {
            page: request.page,
            total_pages: parse_optional(&raw.total_pages),
            total_results: parse_optional(&raw.results),
            modules,
        })
    }

    /// Returns a bounded module payload ready for replay or private caching.
    ///
    /// The response is kept in memory because tracker modules are normally
    /// compact. The explicit bound prevents a malicious or broken endpoint from
    /// consuming unbounded memory.
    pub fn download(
        &self,
        module_id: u64,
        max_module_bytes: usize,
    ) -> Result<Vec<u8>, ProviderError> {
        if module_id == 0 {
            return Err(ProviderError::InvalidRequest(
                "module ID must be positive".to_owned(),
            ));
        }
        if max_module_bytes == 0 || max_module_bytes > MAX_CONFIGURED_RESPONSE_BYTES {
            return Err(ProviderError::InvalidRequest(format!(
                "module size limit must be between 1 and {MAX_CONFIGURED_RESPONSE_BYTES} bytes"
            )));
        }
        let url = module_download_url(module_id);
        get_bounded_bytes(
            &self.agent,
            &url,
            max_module_bytes,
            "application/octet-stream",
        )
    }
}

impl TryFrom<RawModule> for ModuleSummary {
    type Error = ProviderError;

    fn try_from(raw: RawModule) -> Result<Self, Self::Error> {
        let id = raw.id.parse::<u64>().map_err(|_| {
            ProviderError::InvalidResponse("module contains an invalid ID".to_owned())
        })?;
        if id == 0 {
            return Err(ProviderError::InvalidResponse(
                "module ID must be positive".to_owned(),
            ));
        }
        let filename = require_nonempty(raw.filename, "module filename")?;
        Ok(Self {
            id,
            filename,
            song_title: raw.song_title,
            format: raw.format,
            channels: parse_optional(&raw.channels),
            size_bytes: parse_optional(&raw.bytes),
            hits: parse_optional(&raw.hits),
            rating: parse_optional(&raw.rating),
            instruments: raw.instruments,
            comment: raw.comment,
            webpage_url: module_webpage_url(id),
            download_url: module_download_url(id),
        })
    }
}

#[derive(Debug, Default, Deserialize)]
struct RawResponse {
    #[serde(default, rename = "totalpages")]
    total_pages: String,
    #[serde(default)]
    results: String,
    #[serde(default)]
    error: String,
    #[serde(default, rename = "module")]
    modules: Vec<RawModule>,
}

#[derive(Debug, Default, Deserialize)]
struct RawModule {
    #[serde(default)]
    id: String,
    #[serde(default)]
    filename: String,
    #[serde(default, rename = "songtitle")]
    song_title: String,
    #[serde(default)]
    format: String,
    #[serde(default)]
    channels: String,
    #[serde(default)]
    bytes: String,
    #[serde(default)]
    hits: String,
    #[serde(default)]
    rating: String,
    #[serde(default)]
    instruments: String,
    #[serde(default, alias = "comments")]
    comment: String,
}

fn get_bounded_xml(agent: &ureq::Agent, url: &Url, limit: usize) -> Result<Vec<u8>, ProviderError> {
    get_bounded_bytes(agent, url, limit, "application/xml,text/xml")
}

fn get_bounded_bytes(
    agent: &ureq::Agent,
    url: &Url,
    limit: usize,
    accept: &str,
) -> Result<Vec<u8>, ProviderError> {
    let mut response = agent
        .get(url.as_str())
        .header("Accept", accept)
        .call()
        .map_err(|error| match error {
            ureq::Error::StatusCode(code) => ProviderError::HttpStatus(code),
            _ => ProviderError::Transport("Mod Archive request failed".to_owned()),
        })?;
    if response
        .body()
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(ProviderError::ResponseTooLarge { limit });
    }
    let bytes = response
        .body_mut()
        .with_config()
        .limit(u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX))
        .read_to_vec()
        .map_err(|error| match error {
            ureq::Error::BodyExceedsLimit(_) => ProviderError::ResponseTooLarge { limit },
            _ => ProviderError::Transport("Mod Archive response failed".to_owned()),
        })?;
    if bytes.len() > limit {
        return Err(ProviderError::ResponseTooLarge { limit });
    }
    Ok(bytes)
}

fn require_nonempty(value: String, field: &str) -> Result<String, ProviderError> {
    if value.trim().is_empty() {
        return Err(ProviderError::InvalidResponse(format!(
            "{field} cannot be empty"
        )));
    }
    Ok(value)
}

fn nonempty(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn parse_optional<T: std::str::FromStr>(value: &str) -> Option<T> {
    value.trim().parse().ok()
}

fn module_download_url(module_id: u64) -> Url {
    let mut url = Url::parse(API_BASE)
        .expect("the compile-time Mod Archive API URL must be valid")
        .join("downloads.php")
        .expect("the compile-time download path must join");
    url.query_pairs_mut()
        .append_pair("moduleid", &module_id.to_string());
    url
}

fn module_webpage_url(module_id: u64) -> Url {
    let mut url =
        Url::parse(WEBSITE_BASE).expect("the compile-time Mod Archive website URL must be valid");
    url.query_pairs_mut()
        .append_pair("request", "view_by_moduleid")
        .append_pair("query", &module_id.to_string());
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH_FIXTURE: &str = r#"
		<modarchive>
			<totalpages>3</totalpages>
			<results>41</results>
			<module>
				<id>12345</id>
				<filename>example.mod</filename>
				<songtitle>Example Tune</songtitle>
				<format>MOD</format>
				<channels>4</channels>
				<bytes>102400</bytes>
				<hits>77</hits>
				<rating>4.5</rating>
				<instruments>Piano; Bass</instruments>
				<comment>Made in ProTracker</comment>
			</module>
		</modarchive>
	"#;

    #[test]
    fn parses_official_xml_shape() {
        let raw: RawResponse = quick_xml::de::from_str(SEARCH_FIXTURE).expect("XML fixture");
        let modules = raw
            .modules
            .into_iter()
            .map(ModuleSummary::try_from)
            .collect::<Result<Vec<_>, _>>()
            .expect("module conversion");

        assert_eq!(parse_optional::<u32>(&raw.total_pages), Some(3));
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].id, 12_345);
        assert_eq!(modules[0].format, "MOD");
        assert_eq!(modules[0].channels, Some(4));
        assert_eq!(modules[0].rating, Some(4.5));
        assert_eq!(
            modules[0].download_url.as_str(),
            "https://api.modarchive.org/downloads.php?moduleid=12345"
        );
    }

    #[test]
    fn search_validation_rejects_empty_and_excessive_queries() {
        assert!(ModuleSearchRequest::new("").validate().is_err());
        assert!(
            ModuleSearchRequest::new("x".repeat(513))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn api_key_validation_rejects_url_delimiters() {
        assert!(ModArchiveProvider::new("secret&request=random").is_err());
        assert!(ModArchiveProvider::new("normal_Key-123").is_ok());
    }
}
