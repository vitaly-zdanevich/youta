//! Review-first audio transfers to Wikimedia Commons.
//!
//! This module owns credential discovery, Commons metadata construction, and
//! the blocking Action API boundary. Front-ends receive only redacted upload
//! state; account passwords and authenticated cookies stay inside the process.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::thread;

use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::config::{Config, WikimediaCommonsAuthMethod};
use crate::domain::{MediaItem, MediaLicense, SourceKind};
pub use crate::opus_export::{
    OpusAudioSource as CommonsAudioSource, prepare_provider_opus as prepare_commons_opus,
};

/// Commons Action API endpoint used for authentication and uploads.
pub const COMMONS_API_URL: &str = "https://commons.wikimedia.org/w/api.php";

/// Registration page for a narrowly scoped Wikimedia `BotPassword`.
pub const COMMONS_BOT_PASSWORD_URL: &str =
    "https://commons.wikimedia.org/wiki/Special:BotPasswords";

/// Registration page for a normal Wikimedia account.
pub const COMMONS_ACCOUNT_REGISTRATION_URL: &str =
    "https://commons.wikimedia.org/wiki/Special:CreateAccount";

const COMMONS_SITE_URL: &str = "https://commons.wikimedia.org/";
const COMMONS_WIKI_URL: &str = "https://commons.wikimedia.org/wiki/";
const COMMONS_UPLOAD_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const MAX_PYWIKIBOT_FILE_BYTES: u64 = 1024 * 1024;
const MAX_PYWIKIBOT_DIRECTORY_ENTRIES: usize = 64;

/// Maintenance category appended to every Youta upload.
pub const YOUTA_UPLOAD_CATEGORY: &str = "Uploaded by youta";

/// Supported license choices for one Commons transfer.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum CommonsLicense {
    /// No compatible license has been selected yet.
    #[default]
    Unspecified,
    /// Creative Commons Attribution 4.0 International.
    CcBy40,
    /// Creative Commons Attribution-ShareAlike 4.0 International.
    CcBySa40,
    /// Creative Commons Zero 1.0 Universal.
    Cc0,
}

/// Editable metadata shown before any network upload begins.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommonsUploadDraft {
    /// Commons filename, including the `.opus` suffix.
    pub title: String,
    /// Short human-facing caption.
    pub caption: String,
    /// Provider description retained as upload context.
    pub description: String,
    /// Canonical provider page for provenance.
    pub source: String,
    /// Wikitext attribution, normally a linked channel name.
    pub author: String,
    /// Free license selected for the upload.
    pub license: CommonsLicense,
    /// User-selected Commons categories without the `Category:` prefix.
    pub categories: Vec<String>,
}

impl CommonsUploadDraft {
    /// Builds a provider-aware review draft for one selected media item.
    #[must_use]
    pub fn from_media(media: &MediaItem, channel_url: Option<&url::Url>) -> Self {
        let title = commons_audio_filename(media);
        let caption = media.title.trim().to_owned();
        let description = media.description.clone().unwrap_or_default();
        let source = media.webpage_url.to_string();
        let creator = media.creator.as_deref().unwrap_or_default().trim();
        let author = match (channel_url, creator.is_empty()) {
            (Some(url), false) => format!("[{url} {creator}]"),
            (_, false) => creator.to_owned(),
            _ => String::new(),
        };
        Self {
            title,
            caption,
            description,
            source,
            author,
            license: commons_license(&media.license),
            categories: Vec::new(),
        }
    }

    /// Validates the fields needed to publish one audio file.
    ///
    /// # Errors
    ///
    /// Returns an explanation when the required title or optional category
    /// metadata is not safe enough for Commons.
    pub fn validate(&self) -> Result<(), String> {
        let title = self.title.trim();
        if title.is_empty() {
            return Err("The Commons title cannot be empty".to_owned());
        }
        if title.len() > 240
            || title.chars().any(|character| {
                character.is_control()
                    || matches!(
                        character,
                        '#' | '<' | '>' | '[' | ']' | '|' | '{' | '}' | ':' | '/' | '\\'
                    )
            })
        {
            return Err(
                "The Commons title contains a forbidden character or exceeds 240 bytes".to_owned(),
            );
        }
        if !title.to_ascii_lowercase().ends_with(".opus") {
            return Err("The Commons title must end with .opus".to_owned());
        }
        if self.categories.iter().any(|category| {
            let category = category.trim();
            category.is_empty()
                || category.len() > 255
                || category.chars().any(|character| {
                    character.is_control() || matches!(character, '[' | ']' | '{' | '}' | '|' | '#')
                })
        }) {
            return Err("A Commons category is empty or contains unsupported markup".to_owned());
        }
        Ok(())
    }

    /// Renders the Commons file-description page wikitext.
    #[must_use]
    pub fn wikitext(&self) -> String {
        let description = match (self.caption.trim(), self.description.trim()) {
            ("", "") => String::new(),
            ("", description) => description.to_owned(),
            (caption, "") => format!("{{{{en|1={caption}}}}}"),
            (caption, description) => format!("{{{{en|1={caption}}}}}\n\n{description}"),
        };
        let mut text = format!(
            "=={{{{int:filedesc}}}}==\n{{{{Information\n|description={description}\n|date=\n|source={}\n|author={}\n|permission=\n|other versions=\n}}}}\n",
            self.source.trim(),
            self.author.trim(),
        );
        if self.source.contains("youtube.com/") || self.source.contains("youtu.be/") {
            text.push_str("{{LicenseReview}}\n");
        }
        text.push_str(self.license.template());
        for category in &self.categories {
            let category = category.trim();
            if !category.is_empty() && !category.eq_ignore_ascii_case(YOUTA_UPLOAD_CATEGORY) {
                text.push_str("\n[[Category:");
                text.push_str(category);
                text.push_str("]]");
            }
        }
        text.push_str("\n\n[[Category:");
        text.push_str(YOUTA_UPLOAD_CATEGORY);
        text.push_str("]]");
        text
    }
}

impl CommonsLicense {
    /// Commons template emitted for this reviewed license choice.
    #[must_use]
    pub const fn template(self) -> &'static str {
        match self {
            Self::Unspecified => "",
            Self::CcBy40 => "{{Cc-by-4.0}}",
            Self::CcBySa40 => "{{Cc-by-sa-4.0}}",
            Self::Cc0 => "{{Cc-zero}}",
        }
    }

    /// Human-readable label used by upload editors.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Unspecified => "Not specified",
            Self::CcBy40 => "CC BY 4.0",
            Self::CcBySa40 => "CC BY-SA 4.0",
            Self::Cc0 => "CC0 1.0",
        }
    }

    /// Advances through every supported review choice.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Unspecified => Self::CcBy40,
            Self::CcBy40 => Self::CcBySa40,
            Self::CcBySa40 => Self::Cc0,
            Self::Cc0 => Self::Unspecified,
        }
    }
}

fn commons_license(license: &MediaLicense) -> CommonsLicense {
    let MediaLicense::CreativeCommons(label) = license else {
        return CommonsLicense::Unspecified;
    };
    let label = label.trim().to_ascii_lowercase();
    if label.contains("noncommercial")
        || label.contains("non-commercial")
        || label.contains("no derivatives")
        || label.contains("noderivatives")
        || label.contains("cc by-nc")
        || label.contains("cc by-nd")
    {
        CommonsLicense::Unspecified
    } else if label.contains("creativecommons.org/publicdomain/zero") || label.contains("cc0") {
        CommonsLicense::Cc0
    } else if label.contains("creativecommons.org/licenses/by-sa/4.0")
        || label.contains("cc by-sa 4.0")
        || label.contains("cc-by-sa-4.0")
        || label.contains("attribution-sharealike")
    {
        CommonsLicense::CcBySa40
    } else if label.contains("creative commons attribution")
        || label.contains("creativecommons.org/licenses/by/4.0")
        || label.contains("cc by 4.0")
        || label.contains("cc-by-4.0")
    {
        CommonsLicense::CcBy40
    } else {
        CommonsLicense::Unspecified
    }
}

fn commons_audio_filename(media: &MediaItem) -> String {
    let mut stem = media
        .title
        .trim()
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(
                    character,
                    '#' | '<' | '>' | '[' | ']' | '|' | '{' | '}' | ':' | '/' | '\\'
                )
            {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    stem = stem.trim_matches([' ', '.']).to_owned();
    truncate_utf8_bytes(&mut stem, 180);
    stem = stem.trim_end_matches([' ', '.']).to_owned();
    if stem.is_empty() {
        stem.push_str("Untitled audio");
    }
    if media.id.source == SourceKind::YouTube && !media.id.external_id.trim().is_empty() {
        format!("{stem} [{}].opus", media.id.external_id.trim())
    } else {
        format!("{stem}.opus")
    }
}

fn truncate_utf8_bytes(value: &mut String, maximum: usize) {
    if value.len() <= maximum {
        return;
    }
    let mut boundary = maximum;
    while !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }
    value.truncate(boundary);
}

/// Extracts Wikimedia cookies from a Pywikibot LWP cookie jar.
#[must_use]
pub fn lwp_cookie_header(contents: &str) -> Option<String> {
    let cookies = contents
        .lines()
        .filter_map(lwp_cookie_pair)
        .collect::<Vec<_>>();
    (!cookies.is_empty()).then(|| cookies.join("; "))
}

/// Parses the final supported credential entry in a Pywikibot password file.
#[must_use]
pub fn pywikibot_password_credentials(contents: &str) -> Option<(String, String)> {
    let mut selected = None;
    for line in contents.lines() {
        let line = line.trim();
        if !line.starts_with('(') || !line.ends_with(')') || line.starts_with('#') {
            continue;
        }
        let Some(values) = quoted_literals(line) else {
            continue;
        };
        if line.contains("BotPassword(") {
            if values.len() < 3 {
                continue;
            }
            let username = &values[values.len() - 3];
            let suffix = &values[values.len() - 2];
            let password = &values[values.len() - 1];
            if credential_component_is_valid(username)
                && credential_component_is_valid(suffix)
                && credential_component_is_valid(password)
                && !suffix.contains('@')
            {
                selected = Some((format!("{username}@{suffix}"), password.clone()));
            }
        } else if values.len() >= 2 {
            let username = &values[values.len() - 2];
            let password = &values[values.len() - 1];
            if credential_component_is_valid(username) && credential_component_is_valid(password) {
                selected = Some((username.clone(), password.clone()));
            }
        }
    }
    selected
}

fn lwp_cookie_pair(line: &str) -> Option<String> {
    let payload = line.trim().strip_prefix("Set-Cookie3: ")?;
    let lowercase = payload.to_ascii_lowercase();
    let domain = lowercase.split("domain=\"").nth(1)?.split('"').next()?;
    if domain != "wikimedia.org" && !domain.ends_with(".wikimedia.org") {
        return None;
    }
    let (name, rest) = payload.split_once('=')?;
    let value = rest.split_once(';').map_or(rest, |(value, _)| value).trim();
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    Some(format!("{name}={}", unquote_lwp_value(value)))
}

fn unquote_lwp_value(value: &str) -> String {
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value);
    let mut output = String::with_capacity(value.len());
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            output.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else {
            output.push(character);
        }
    }
    if escaped {
        output.push('\\');
    }
    output
}

fn quoted_literals(line: &str) -> Option<Vec<String>> {
    let mut values = Vec::new();
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        if !matches!(character, '\'' | '"') {
            continue;
        }
        let quote = character;
        let mut value = String::new();
        let mut closed = false;
        while let Some(character) = characters.next() {
            if character == '\\' {
                let escaped = characters.next()?;
                value.push(match escaped {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    other => other,
                });
            } else if character == quote {
                closed = true;
                break;
            } else {
                value.push(character);
            }
        }
        if !closed {
            return None;
        }
        values.push(value);
    }
    Some(values)
}

fn credential_component_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && value == value.trim()
        && !value.chars().any(char::is_control)
}

/// Origin of credentials selected for a Commons session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommonsCredentialSource {
    /// Youta's private `secrets/credentials.toml` file.
    Youta,
    /// A Pywikibot password configuration file.
    PywikibotPassword,
    /// A Pywikibot LWP cookie jar.
    PywikibotCookies,
}

/// Authentication material retained only inside the controller process.
#[derive(Clone)]
pub enum CommonsAuthentication {
    /// Password-based `MediaWiki` authentication.
    Password {
        /// Account or `BotPassword` username.
        username: String,
        /// Account or `BotPassword` secret.
        password: String,
        /// `MediaWiki` login flow to use.
        method: WikimediaCommonsAuthMethod,
    },
    /// Cookie header reconstructed from a Pywikibot LWP jar.
    CookieHeader(String),
}

impl std::fmt::Debug for CommonsAuthentication {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Password {
                username, method, ..
            } => formatter
                .debug_struct("Password")
                .field("username", username)
                .field("password", &"[REDACTED]")
                .field("method", method)
                .finish(),
            Self::CookieHeader(_) => formatter.write_str("CookieHeader([REDACTED])"),
        }
    }
}

/// Discovered authentication and the private store it came from.
#[derive(Clone, Debug)]
pub struct DiscoveredCommonsAuthentication {
    /// Secret authentication material.
    pub authentication: CommonsAuthentication,
    /// Store selected by the bounded discovery process.
    pub source: CommonsCredentialSource,
}

/// One category returned by Commons prefix completion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommonsCategorySuggestion {
    /// Category title without the `Category:` namespace prefix.
    pub name: String,
    /// Browser-safe category page URL.
    pub url: Url,
}

impl CommonsCategorySuggestion {
    /// Label shown by text and graphical front-ends.
    #[must_use]
    pub fn label(&self) -> String {
        format!("📁 {}", self.name)
    }
}

/// Successful Commons upload identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommonsUploadResult {
    /// Canonical filename returned by Commons.
    pub filename: String,
    /// Public file-description page.
    pub url: Url,
}

/// Blocking Wikimedia Commons Action API client.
///
/// The client owns a cookie-aware agent. Callers should run its methods on a
/// worker thread so DNS, authentication, transcoding, and upload traffic never
/// block input rendering.
#[derive(Clone)]
pub struct CommonsClient {
    api_url: Url,
    agent: ureq::Agent,
}

/// Authenticated cookie session used for category lookup and upload.
#[derive(Clone)]
pub struct CommonsSession {
    client: CommonsClient,
    cookie_header: Option<String>,
}

impl CommonsClient {
    /// Creates a client for Wikimedia Commons' production Action API.
    ///
    /// # Panics
    ///
    /// Panics only if the compile-time constant [`COMMONS_API_URL`] stops being
    /// a valid absolute URL.
    #[must_use]
    pub fn new() -> Self {
        Self::with_api_url(Url::parse(COMMONS_API_URL).expect("static Commons API URL"))
    }

    /// Creates a client for an explicit API endpoint, including local test servers.
    #[must_use]
    pub fn with_api_url(api_url: Url) -> Self {
        let agent = ureq::Agent::config_builder()
            .user_agent(concat!("Youta/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self { api_url, agent }
    }

    /// Authenticates one configured account or validates a supplied cookie jar.
    ///
    /// # Errors
    ///
    /// Returns an explanation when the login request, cookie validation, or
    /// Commons response fails.
    pub fn authenticate(
        &self,
        authentication: &CommonsAuthentication,
    ) -> Result<CommonsSession, String> {
        let cookie_header = match authentication {
            CommonsAuthentication::Password {
                username,
                password,
                method,
            } => {
                self.login(username, password, *method)?;
                None
            }
            CommonsAuthentication::CookieHeader(header) => Some(header.clone()),
        };
        let session = CommonsSession {
            client: self.clone(),
            cookie_header,
        };
        session.validate_user()?;
        Ok(session)
    }

    /// Returns public Commons categories matching one user-entered prefix.
    ///
    /// # Errors
    ///
    /// Returns an explanation when Commons cannot be reached or responds with
    /// malformed category data.
    pub fn category_suggestions(
        &self,
        prefix: &str,
    ) -> Result<Vec<CommonsCategorySuggestion>, String> {
        self.category_suggestions_with_cookie(None, prefix)
    }

    fn category_suggestions_with_cookie(
        &self,
        cookie_header: Option<&str>,
        prefix: &str,
    ) -> Result<Vec<CommonsCategorySuggestion>, String> {
        let prefix = prefix.trim().trim_start_matches("Category:").trim();
        if prefix.is_empty() {
            return Ok(Vec::new());
        }
        let response = self.get_json(
            cookie_header,
            &[
                ("action", "query"),
                ("list", "allcategories"),
                ("acprefix", prefix),
                ("aclimit", "12"),
                ("format", "json"),
                ("formatversion", "2"),
            ],
        )?;
        let categories = response
            .pointer("/query/allcategories")
            .and_then(Value::as_array)
            .ok_or_else(|| api_failure(&response, "Commons did not return category suggestions"))?;
        Ok(categories
            .iter()
            .filter_map(|category| category.get("category").and_then(Value::as_str))
            .filter(|name| !name.trim().is_empty())
            .take(12)
            .map(|name| CommonsCategorySuggestion {
                name: name.to_owned(),
                url: commons_category_url(name),
            })
            .collect())
    }

    fn login(
        &self,
        username: &str,
        password: &str,
        method: WikimediaCommonsAuthMethod,
    ) -> Result<(), String> {
        let token = self.fetch_token(None, "login")?;
        let response = match method {
            WikimediaCommonsAuthMethod::BotPassword => self.post_form(
                None,
                &[
                    ("action", "login"),
                    ("lgname", username),
                    ("lgpassword", password),
                    ("lgtoken", &token),
                    ("format", "json"),
                    ("formatversion", "2"),
                ],
            )?,
            WikimediaCommonsAuthMethod::AccountPassword => self.post_form(
                None,
                &[
                    ("action", "clientlogin"),
                    ("username", username),
                    ("password", password),
                    ("logintoken", &token),
                    ("loginreturnurl", COMMONS_SITE_URL),
                    ("format", "json"),
                    ("formatversion", "2"),
                ],
            )?,
        };

        match method {
            WikimediaCommonsAuthMethod::BotPassword => {
                let result = response
                    .pointer("/login/result")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if result != "Success" {
                    return Err(api_failure(
                        &response,
                        "Wikimedia Commons rejected the BotPassword login",
                    ));
                }
            }
            WikimediaCommonsAuthMethod::AccountPassword => {
                let status = response
                    .pointer("/clientlogin/status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if status != "PASS" {
                    return Err(format!(
                        "{}; accounts requiring two-factor or interactive login should use a BotPassword",
                        api_failure(&response, "Wikimedia Commons rejected the account login")
                    ));
                }
            }
        }
        Ok(())
    }

    fn fetch_token(&self, cookie_header: Option<&str>, token_type: &str) -> Result<String, String> {
        let response = self.get_json(
            cookie_header,
            &[
                ("action", "query"),
                ("meta", "tokens"),
                ("type", token_type),
                ("format", "json"),
                ("formatversion", "2"),
            ],
        )?;
        let key = format!("{token_type}token");
        response
            .pointer(&format!("/query/tokens/{key}"))
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| api_failure(&response, "Commons did not return an authentication token"))
    }

    fn get_json(
        &self,
        cookie_header: Option<&str>,
        parameters: &[(&str, &str)],
    ) -> Result<Value, String> {
        let request = self
            .agent
            .get(self.api_url.as_str())
            .query_pairs(parameters.iter().copied());
        let mut response = match cookie_header {
            Some(header) => request.header("Cookie", header).call(),
            None => request.call(),
        }
        .map_err(|error| format!("Commons request failed: {error}"))?;
        response
            .body_mut()
            .read_json()
            .map_err(|error| format!("Commons returned invalid JSON: {error}"))
    }

    fn post_form(
        &self,
        cookie_header: Option<&str>,
        parameters: &[(&str, &str)],
    ) -> Result<Value, String> {
        let request = self.agent.post(self.api_url.as_str());
        let mut response = match cookie_header {
            Some(header) => request
                .header("Cookie", header)
                .send_form(parameters.iter().copied()),
            None => request.send_form(parameters.iter().copied()),
        }
        .map_err(|error| format!("Commons request failed: {error}"))?;
        response
            .body_mut()
            .read_json()
            .map_err(|error| format!("Commons returned invalid JSON: {error}"))
    }
}

impl Default for CommonsClient {
    fn default() -> Self {
        Self::new()
    }
}

impl CommonsSession {
    /// Returns matching Commons categories for one user-entered prefix.
    ///
    /// # Errors
    ///
    /// Returns an explanation when Commons cannot be reached or responds with
    /// malformed category data.
    pub fn category_suggestions(
        &self,
        prefix: &str,
    ) -> Result<Vec<CommonsCategorySuggestion>, String> {
        self.client
            .category_suggestions_with_cookie(self.cookie_header.as_deref(), prefix)
    }

    /// Uploads one local Opus file through the chunked stash protocol.
    ///
    /// `progress` runs after Commons acknowledges each chunk, so the reported
    /// byte count represents server-accepted data rather than bytes merely read
    /// from disk.
    ///
    /// # Errors
    ///
    /// Returns an explanation when the draft or staged file is invalid, a
    /// Commons request fails, a chunk is not acknowledged, or Commons declines
    /// to publish the stashed upload.
    #[allow(clippy::too_many_lines)]
    pub fn upload_opus(
        &self,
        path: &Path,
        draft: &CommonsUploadDraft,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<CommonsUploadResult, String> {
        draft.validate()?;
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("Cannot inspect the staged Opus file: {error}"))?;
        if !metadata.file_type().is_file() || metadata.len() == 0 {
            return Err("The staged Commons upload must be a non-empty regular file".to_owned());
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("opus") {
            return Err("Wikimedia Commons uploads from Youta must use Opus".to_owned());
        }
        let total = metadata.len();
        let csrf_token = self
            .client
            .fetch_token(self.cookie_header.as_deref(), "csrf")?;
        let mut file = fs::File::open(path)
            .map_err(|error| format!("Cannot open the staged Opus file: {error}"))?;
        let mut accepted = 0_u64;
        let mut file_key: Option<String> = None;
        let mut buffer = vec![0_u8; COMMONS_UPLOAD_CHUNK_BYTES];

        while accepted < total {
            file.seek(SeekFrom::Start(accepted))
                .map_err(|error| format!("Cannot seek in the staged Opus file: {error}"))?;
            let remaining = total.saturating_sub(accepted);
            let requested = usize::try_from(remaining.min(COMMONS_UPLOAD_CHUNK_BYTES as u64))
                .unwrap_or(COMMONS_UPLOAD_CHUNK_BYTES);
            file.read_exact(&mut buffer[..requested])
                .map_err(|error| format!("Cannot read the staged Opus file: {error}"))?;
            let accepted_text = accepted.to_string();
            let total_text = total.to_string();
            let mut form = ureq::unversioned::multipart::Form::new()
                .text("action", "upload")
                .text("filename", draft.title.trim())
                .text("stash", "1")
                .text("filesize", &total_text)
                .text("offset", &accepted_text)
                .text("token", &csrf_token)
                .text("format", "json")
                .text("formatversion", "2")
                .part(
                    "chunk",
                    ureq::unversioned::multipart::Part::bytes(&buffer[..requested])
                        .file_name("audio.opus")
                        .mime_str("audio/ogg")
                        .map_err(|error| format!("Cannot prepare the Opus upload: {error}"))?,
                );
            if let Some(key) = file_key.as_deref() {
                form = form.text("filekey", key);
            }
            let request = self.client.agent.post(self.client.api_url.as_str());
            let mut response = match self.cookie_header.as_deref() {
                Some(header) => request.header("Cookie", header).send(form),
                None => request.send(form),
            }
            .map_err(|error| format!("Commons chunk upload failed: {error}"))?;
            let response: Value = response
                .body_mut()
                .read_json()
                .map_err(|error| format!("Commons returned invalid upload JSON: {error}"))?;
            let upload = response
                .get("upload")
                .ok_or_else(|| api_failure(&response, "Commons rejected an upload chunk"))?;
            file_key = upload
                .get("filekey")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or(file_key);
            let next = upload
                .get("offset")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| accepted.saturating_add(requested as u64));
            if next <= accepted || next > total {
                return Err("Commons returned an invalid accepted-byte offset".to_owned());
            }
            accepted = next;
            progress(accepted, total);
        }

        let file_key = file_key.ok_or_else(|| {
            "Commons accepted the audio chunks but did not return a stash key".to_owned()
        })?;
        let wikitext = draft.wikitext();
        let response = self.client.post_form(
            self.cookie_header.as_deref(),
            &[
                ("action", "upload"),
                ("filename", draft.title.trim()),
                ("filekey", &file_key),
                ("text", &wikitext),
                ("comment", "Upload audio with Youta"),
                ("token", &csrf_token),
                ("format", "json"),
                ("formatversion", "2"),
            ],
        )?;
        let upload = response
            .get("upload")
            .ok_or_else(|| api_failure(&response, "Commons rejected the stashed upload"))?;
        if upload.get("warnings").is_some() {
            return Err(api_failure(
                &response,
                "Commons returned an upload warning; review the filename and source before retrying",
            ));
        }
        if upload.get("result").and_then(Value::as_str) != Some("Success") {
            return Err(api_failure(&response, "Commons did not publish the upload"));
        }
        let filename = upload
            .get("filename")
            .and_then(Value::as_str)
            .unwrap_or_else(|| draft.title.trim())
            .to_owned();
        let url = upload
            .pointer("/imageinfo/descriptionurl")
            .and_then(Value::as_str)
            .and_then(|value| Url::parse(value).ok())
            .unwrap_or_else(|| commons_file_url(&filename));
        Ok(CommonsUploadResult { filename, url })
    }

    fn validate_user(&self) -> Result<(), String> {
        let response = self.client.get_json(
            self.cookie_header.as_deref(),
            &[
                ("action", "query"),
                ("meta", "userinfo"),
                ("format", "json"),
                ("formatversion", "2"),
            ],
        )?;
        let user = response
            .pointer("/query/userinfo")
            .ok_or_else(|| api_failure(&response, "Commons did not return account information"))?;
        if user.get("anon").is_some()
            || user.get("id").and_then(Value::as_u64).unwrap_or_default() == 0
        {
            return Err("The Wikimedia Commons session is not logged in".to_owned());
        }
        Ok(())
    }
}

/// Finds credentials in Youta and then the user's normal Pywikibot directory.
#[must_use]
pub fn discover_commons_authentication(config: &Config) -> Option<DiscoveredCommonsAuthentication> {
    if let (Some(username), Some(password)) = (
        config.providers.wikimedia_commons_username.as_ref(),
        config.providers.wikimedia_commons_password.as_ref(),
    ) {
        return Some(DiscoveredCommonsAuthentication {
            authentication: CommonsAuthentication::Password {
                username: username.clone(),
                password: password.clone(),
                method: config.providers.wikimedia_commons_auth_method,
            },
            source: CommonsCredentialSource::Youta,
        });
    }
    let directory = std::env::var_os("PYWIKIBOT_DIR")
        .map(PathBuf::from)
        .or_else(|| BaseDirs::new().map(|dirs| dirs.home_dir().join(".pywikibot")))?;
    discover_pywikibot_authentication(&directory)
}

/// Finds one supported password file or LWP session without executing Python.
#[must_use]
pub fn discover_pywikibot_authentication(
    directory: &Path,
) -> Option<DiscoveredCommonsAuthentication> {
    for filename in ["user-password.cfg", "user-password.py"] {
        let Some(contents) = read_bounded_regular_file(&directory.join(filename)) else {
            continue;
        };
        if let Some((username, password)) = pywikibot_password_credentials(&contents) {
            let method = if username.contains('@') {
                WikimediaCommonsAuthMethod::BotPassword
            } else {
                WikimediaCommonsAuthMethod::AccountPassword
            };
            return Some(DiscoveredCommonsAuthentication {
                authentication: CommonsAuthentication::Password {
                    username,
                    password,
                    method,
                },
                source: CommonsCredentialSource::PywikibotPassword,
            });
        }
    }

    let mut jars = fs::read_dir(directory)
        .ok()?
        .take(MAX_PYWIKIBOT_DIRECTORY_ENTRIES)
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("lwp"))
        .collect::<Vec<_>>();
    jars.sort();
    for jar in jars.into_iter().rev() {
        let Some(contents) = read_bounded_regular_file(&jar) else {
            continue;
        };
        if let Some(header) = lwp_cookie_header(&contents) {
            return Some(DiscoveredCommonsAuthentication {
                authentication: CommonsAuthentication::CookieHeader(header),
                source: CommonsCredentialSource::PywikibotCookies,
            });
        }
    }
    None
}

fn read_bounded_regular_file(path: &Path) -> Option<String> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_PYWIKIBOT_FILE_BYTES {
        return None;
    }
    fs::read_to_string(path).ok()
}

fn commons_category_url(name: &str) -> Url {
    commons_wiki_page_url(&format!("Category:{}", name.replace(' ', "_")))
}

fn commons_file_url(name: &str) -> Url {
    commons_wiki_page_url(&format!("File:{}", name.replace(' ', "_")))
}

fn commons_wiki_page_url(page: &str) -> Url {
    let mut url = Url::parse(COMMONS_WIKI_URL).expect("static Commons wiki URL");
    url.path_segments_mut()
        .expect("Commons wiki URL can accept path segments")
        .pop_if_empty()
        .push(page);
    url
}

fn api_failure(response: &Value, fallback: &str) -> String {
    let detail = response
        .pointer("/error/info")
        .or_else(|| response.pointer("/login/reason"))
        .or_else(|| response.pointer("/clientlogin/message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| response.pointer("/upload/warnings").map(Value::to_string));
    detail.map_or_else(
        || fallback.to_owned(),
        |detail| format!("{fallback}: {detail}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{MediaId, MediaKind, MediaStatistics};
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};
    use std::thread::JoinHandle;
    use tempfile::tempdir;

    fn read_http_request(mut stream: &TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut body_start = None;
        let mut content_length = 0_usize;
        loop {
            let mut chunk = [0_u8; 8 * 1024];
            let read = stream.read(&mut chunk).expect("read mock request");
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if body_start.is_none()
                && let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let start = index + 4;
                let headers = String::from_utf8_lossy(&bytes[..index]);
                content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or_default();
                body_start = Some(start);
            }
            if body_start.is_some_and(|start| bytes.len() >= start + content_length) {
                break;
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn scripted_api(responses: Vec<String>) -> (Url, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Commons API");
        let address = listener.local_addr().expect("mock Commons address");
        let handle = thread::spawn(move || {
            let mut requests = Vec::with_capacity(responses.len());
            for body in responses {
                let (mut stream, _) = listener.accept().expect("accept mock Commons request");
                requests.push(read_http_request(&stream));
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write mock Commons response");
            }
            requests
        });
        (
            Url::parse(&format!("http://{address}/w/api.php")).expect("mock Commons URL"),
            handle,
        )
    }

    fn youtube_media(license: MediaLicense) -> MediaItem {
        MediaItem {
            id: MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ"),
            kind: MediaKind::Video,
            title: "Fixture video".to_owned(),
            creator: Some("Fixture channel".to_owned()),
            description: Some("Provider description".to_owned()),
            webpage_url: url::Url::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
                .expect("fixture URL"),
            thumbnail_url: None,
            duration_seconds: Some(42),
            published_at: None,
            statistics: MediaStatistics::default(),
            license,
            chapters: Vec::new(),
            captions: Vec::new(),
        }
    }

    #[test]
    fn youtube_draft_matches_ytdlp_filename_and_prefills_provenance() {
        let channel =
            url::Url::parse("https://www.youtube.com/channel/UCfixture").expect("channel URL");
        let draft = CommonsUploadDraft::from_media(
            &youtube_media(MediaLicense::CreativeCommons(
                "Creative Commons Attribution".to_owned(),
            )),
            Some(&channel),
        );

        assert_eq!(draft.title, "Fixture video [dQw4w9WgXcQ].opus");
        assert_eq!(draft.caption, "Fixture video");
        assert_eq!(draft.description, "Provider description");
        assert_eq!(draft.source, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert_eq!(
            draft.author,
            "[https://www.youtube.com/channel/UCfixture Fixture channel]"
        );
        assert_eq!(draft.license, CommonsLicense::CcBy40);
    }

    #[test]
    fn unknown_provider_license_remains_optional() {
        let mut media = youtube_media(MediaLicense::Unknown);
        media.id.source = SourceKind::ApplePodcasts;
        let draft = CommonsUploadDraft::from_media(&media, None);

        assert_eq!(draft.license, CommonsLicense::Unspecified);
        assert_eq!(draft.validate(), Ok(()));
    }

    #[test]
    fn title_cannot_be_empty() {
        let mut draft = CommonsUploadDraft::from_media(
            &youtube_media(MediaLicense::CreativeCommons("CC BY 4.0".to_owned())),
            None,
        );
        draft.title.clear();

        assert_eq!(
            draft.validate(),
            Err("The Commons title cannot be empty".to_owned())
        );
    }

    #[test]
    fn title_must_remain_an_opus_filename() {
        let mut draft = CommonsUploadDraft::from_media(
            &youtube_media(MediaLicense::CreativeCommons("CC BY 4.0".to_owned())),
            None,
        );
        draft.title = "Fixture video.ogg".to_owned();

        assert_eq!(
            draft.validate(),
            Err("The Commons title must end with .opus".to_owned())
        );
    }

    #[test]
    fn noncommercial_and_no_derivatives_licenses_are_not_auto_selected() {
        for label in ["CC BY-NC 4.0", "Creative Commons NoDerivatives"] {
            let draft = CommonsUploadDraft::from_media(
                &youtube_media(MediaLicense::CreativeCommons(label.to_owned())),
                None,
            );
            assert_eq!(
                draft.license,
                CommonsLicense::Unspecified,
                "{label} is not Commons-compatible"
            );
        }
    }

    #[test]
    fn wikitext_separates_the_maintenance_category_with_a_blank_line() {
        let mut draft = CommonsUploadDraft::from_media(
            &youtube_media(MediaLicense::CreativeCommons("CC BY 4.0".to_owned())),
            None,
        );
        draft.categories = vec!["Spoken word".to_owned(), "History podcasts".to_owned()];
        let wikitext = draft.wikitext();

        assert!(wikitext.contains("{{Information"));
        assert!(wikitext.contains("{{Cc-by-4.0}}"));
        assert!(wikitext.contains("[[Category:Spoken word]]"));
        assert!(
            wikitext.ends_with("[[Category:History podcasts]]\n\n[[Category:Uploaded by youta]]")
        );
    }

    #[test]
    fn pywikibot_lwp_parser_keeps_only_wikimedia_cookie_pairs() {
        let contents = r#"#LWP-Cookies-2.0
Set-Cookie3: centralauth_User="Fixture"; path="/"; domain=".wikimedia.org"; path_spec; secure; version=0
Set-Cookie3: session="secret\"value"; path="/"; domain="commons.wikimedia.org"; path_spec; secure; version=0
Set-Cookie3: unrelated="nope"; path="/"; domain="example.org"; path_spec; version=0
"#;

        assert_eq!(
            lwp_cookie_header(contents).as_deref(),
            Some("centralauth_User=Fixture; session=secret\"value")
        );
    }

    #[test]
    fn pywikibot_bot_password_is_parsed_without_executing_python() {
        let contents = r#"
('Old user', 'old regular password')
('Fixture user', BotPassword('youta', 'scoped secret'))
"#;

        assert_eq!(
            pywikibot_password_credentials(contents),
            Some(("Fixture user@youta".to_owned(), "scoped secret".to_owned()))
        );
    }

    #[test]
    fn pywikibot_discovery_does_not_execute_password_configuration() {
        let directory = tempdir().expect("temporary Pywikibot directory");
        let marker = directory.path().join("must-not-exist");
        fs::write(
			directory.path().join("user-password.py"),
			format!(
				"__import__('pathlib').Path({marker:?}).touch()\n('Fixture', BotPassword('youta', 'secret'))\n"
			),
		)
		.expect("password fixture");

        let discovered = discover_pywikibot_authentication(directory.path())
            .expect("discover static credential tuple");

        assert_eq!(
            discovered.source,
            CommonsCredentialSource::PywikibotPassword
        );
        assert!(!marker.exists(), "password configuration was executed");
        assert!(!format!("{:?}", discovered.authentication).contains("secret"));
    }

    #[test]
    fn category_suggestion_has_emoji_and_encoded_commons_link() {
        let suggestion = CommonsCategorySuggestion {
            name: "Audio files from Tbilisi".to_owned(),
            url: commons_category_url("Audio files from Tbilisi"),
        };

        assert_eq!(suggestion.label(), "📁 Audio files from Tbilisi");
        assert_eq!(
            suggestion.url.as_str(),
            "https://commons.wikimedia.org/wiki/Category:Audio_files_from_Tbilisi"
        );
    }

    #[test]
    fn category_completion_uses_the_bounded_commons_api_shape() {
        let (api_url, server) = scripted_api(vec![
            r#"{"query":{"allcategories":[{"category":"Audio files from Tbilisi"}]}}"#.to_owned(),
        ]);
        let suggestions = CommonsClient::with_api_url(api_url)
            .category_suggestions("Audio files")
            .expect("mock Commons category completion");

        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].label(), "📁 Audio files from Tbilisi");
        let requests = server.join().expect("mock Commons server");
        assert!(requests[0].starts_with("GET /w/api.php?"));
        assert!(requests[0].contains("list=allcategories"));
        assert!(requests[0].contains("aclimit=12"));
    }

    #[test]
    fn upload_progress_advances_only_after_commons_accepts_a_chunk() {
        let directory = tempdir().expect("upload staging directory");
        let path = directory.path().join("audio.opus");
        fs::write(&path, b"OggSfixture").expect("write staged Opus fixture");
        let total = fs::metadata(&path).expect("fixture metadata").len();
        let final_url = "https://commons.wikimedia.org/wiki/File:Fixture_audio.opus";
        let (api_url, server) = scripted_api(vec![
            r#"{"query":{"userinfo":{"id":42,"name":"Fixture"}}}"#.to_owned(),
            r#"{"query":{"tokens":{"csrftoken":"csrf-token"}}}"#.to_owned(),
            format!(
                r#"{{"upload":{{"result":"Continue","offset":{total},"filekey":"stash-key"}}}}"#
            ),
            format!(
                r#"{{"upload":{{"result":"Success","filename":"Fixture audio.opus","imageinfo":{{"descriptionurl":"{final_url}"}}}}}}"#
            ),
        ]);
        let session = CommonsClient::with_api_url(api_url)
            .authenticate(&CommonsAuthentication::CookieHeader(
                "commons_session=fixture".to_owned(),
            ))
            .expect("authenticate mock Commons session");
        let mut draft = CommonsUploadDraft::from_media(
            &youtube_media(MediaLicense::CreativeCommons("CC BY 4.0".to_owned())),
            None,
        );
        draft.title = "Fixture audio.opus".to_owned();
        let mut progress = Vec::new();
        let uploaded = session
            .upload_opus(&path, &draft, |accepted, total| {
                progress.push((accepted, total));
            })
            .expect("upload through mock Commons API");

        assert_eq!(progress, vec![(total, total)]);
        assert_eq!(uploaded.url.as_str(), final_url);
        let requests = server.join().expect("mock Commons server");
        assert_eq!(requests.len(), 4);
        assert!(requests.iter().all(|request| {
            request
                .to_ascii_lowercase()
                .contains("cookie: commons_session=fixture")
        }));
        assert!(requests[2].contains("name=\"stash\""));
        assert!(requests[2].contains("name=\"chunk\""));
        assert!(requests[3].contains("filekey=stash-key"));
        assert!(requests[3].contains("Uploaded+by+youta"));
    }
}
