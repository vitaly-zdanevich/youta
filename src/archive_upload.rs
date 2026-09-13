//! Explicit, review-first Internet Archive uploads through the IAS3 API.
//!
//! Credentials never enter a draft or a serializable view. Uploads target only
//! the fixed HTTPS IAS3 endpoint and never automatically retry or redirect.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, RustlsConnector, Transport,
};
use url::Url;

/// Maximum complete publication description; longer values are rejected.
pub const MAX_ARCHIVE_UPLOAD_DESCRIPTION_BYTES: usize = 64 * 1024;
/// A single reviewed upload is bounded independently of the media preparer.
pub const MAX_ARCHIVE_UPLOAD_BYTES: u64 = 20 * 1024 * 1024 * 1024;
const MAX_CREDENTIAL_FILE_BYTES: u64 = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(15);
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(20);
const UPLOAD_TIMEOUT: Duration = Duration::from_hours(2);
const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const IAS3_ENDPOINT: &str = "https://s3.us.archive.org/";

/// Editable publication metadata; defaults are deliberately not publishable.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArchiveUploadDraft {
    /// New Archive item identifier, generated once when review opens.
    pub identifier: String,
    /// Public item title.
    pub title: String,
    /// Complete public description, limited to 64 KiB without truncation.
    pub description: String,
    /// Optional public creator; an empty value omits this metadata field.
    pub creator: String,
    /// Immutable canonical `YouTube` watch URL supplied by the controller.
    pub source_url: String,
    /// Whether prepared media includes video instead of the default Opus audio.
    pub upload_video: bool,
}

impl ArchiveUploadDraft {
    /// Creates an editable draft with a fresh identifier without publishing anything.
    ///
    /// # Errors
    /// Rejects invalid metadata or an unavailable operating-system random source.
    pub fn new(
        source_url: Url,
        title: String,
        description: String,
        creator: Option<String>,
    ) -> Result<Self, String> {
        let source_url: String = source_url.into();
        let video_id = canonical_youtube_id(&source_url)?;
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random)
            .map_err(|_| "Could not generate a secure Archive item identifier".to_string())?;
        let mut suffix = String::with_capacity(32);
        for byte in random {
            let hex = b"0123456789abcdef";
            suffix.push(char::from(hex[usize::from(byte >> 4)]));
            suffix.push(char::from(hex[usize::from(byte & 15)]));
        }
        let draft = Self {
            identifier: format!("youtube-{video_id}-{suffix}"),
            title,
            description,
            creator: creator.unwrap_or_default(),
            source_url,
            ..Self::default()
        };
        draft.validate_metadata()?;
        Ok(draft)
    }

    /// Validates the editable fields and immutable source before confirmation.
    ///
    /// # Errors
    /// Rejects unsafe identifiers/source URLs, invalid XML characters or text bounds.
    pub fn validate_metadata(&self) -> Result<(), String> {
        if self.identifier.is_empty()
            || self.identifier.len() > 100
            || !self.identifier.as_bytes()[0].is_ascii_alphanumeric()
            || !self
                .identifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err("Archive identifier must start with a letter or digit and contain at most 100 letters, digits, dots, underscores or hyphens".into());
        }
        if self.title.trim().is_empty() || self.title.len() > 1024 {
            return Err("Archive title is required and must fit within 1 KiB".into());
        }
        if self.description.len() > MAX_ARCHIVE_UPLOAD_DESCRIPTION_BYTES {
            return Err(
                "Archive description exceeds 64 KiB; shorten it explicitly before publishing"
                    .into(),
            );
        }
        if self.creator.len() > 1024 {
            return Err("Archive creator exceeds 1 KiB".into());
        }
        if [&self.title, &self.description, &self.creator]
            .iter()
            .any(|value| !value.chars().all(valid_xml_character))
        {
            return Err("Archive metadata contains a character that XML cannot represent".into());
        }
        canonical_youtube_id(&self.source_url)?;
        Ok(())
    }

    /// Validates metadata before an explicitly requested upload performs I/O.
    ///
    /// # Errors
    /// Rejects unsafe identifiers, invalid sources or invalid metadata text.
    pub fn validate(&self) -> Result<(), String> {
        self.validate_metadata()
    }
}

/// Only exact, credential-free canonical watch URLs can identify the source.
fn canonical_youtube_id(source: &str) -> Result<&str, String> {
    let identifier = source
        .strip_prefix("https://www.youtube.com/watch?v=")
        .filter(|value| {
            value.len() == 11
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        });
    identifier
        .ok_or_else(|| "Archive upload requires the original canonical YouTube watch URL".into())
}

fn valid_xml_character(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}

/// Session-only IAS3 credentials with deliberately redacted debug formatting.
#[derive(Clone)]
pub struct ArchiveUploadCredentials {
    access: String,
    secret: String,
}

impl std::fmt::Debug for ArchiveUploadCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ArchiveUploadCredentials([REDACTED])")
    }
}

impl ArchiveUploadCredentials {
    /// Validates an access/secret pair without logging either value.
    ///
    /// # Errors
    /// Rejects empty, oversized or header-unsafe credentials without echoing them.
    pub fn new(access: String, secret: String) -> Result<Self, String> {
        if [&access, &secret].iter().any(|value| {
            value.is_empty()
                || value.len() > 512
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() && byte != b':')
        }) {
            return Err("Archive access and secret keys must be nonempty ASCII tokens without spaces or colons".into());
        }
        Ok(Self { access, secret })
    }

    /// Marks the authorization value sensitive so HTTP debug output redacts it.
    fn authorization(&self) -> Result<ureq::http::HeaderValue, String> {
        let mut value =
            ureq::http::HeaderValue::from_str(&format!("LOW {}:{}", self.access, self.secret))
                .map_err(|_| "Invalid Archive credentials".to_string())?;
        value.set_sensitive(true);
        Ok(value)
    }
}

/// Discovers private Youta credentials, then standard Internet Archive configs.
///
/// Youta's `secrets/archive-org.toml` accepts `access_key` and `secret_key`
/// (`access`/`secret` are aliases). Standard `ia.ini` files use `[s3]` with
/// `access` and `secret`, matching Internet Archive's official Python client.
/// Existing malformed files stop discovery instead of selecting another account.
/// Files are read only; no credentials are persisted by this module.
///
/// # Errors
/// Returns a redacted error for inaccessible, malformed or oversized files.
pub fn discover_archive_upload_credentials(
    config_dir: &Path,
) -> Result<Option<ArchiveUploadCredentials>, String> {
    let home = directories::BaseDirs::new().map(|base| base.home_dir().to_path_buf());
    let paths = credential_paths(
        config_dir,
        std::env::var_os("IA_CONFIG_FILE").map(PathBuf::from),
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
        home.as_deref(),
    );
    discover_from_paths(&paths)
}

/// Builds documented precedence without reading global process state in tests.
fn credential_paths(
    config_dir: &Path,
    explicit: Option<PathBuf>,
    xdg: Option<PathBuf>,
    home: Option<&Path>,
) -> Vec<(PathBuf, bool)> {
    let mut paths = vec![(config_dir.join("secrets/archive-org.toml"), true)];
    if let Some(path) = explicit.filter(|path| !path.as_os_str().is_empty()) {
        paths.push((path, false));
    }
    let config_home = xdg
        .filter(|path| path.is_absolute())
        .or_else(|| home.map(|path| path.join(".config")));
    if let Some(path) = config_home {
        paths.push((path.join("internetarchive/ia.ini"), false));
    }
    if let Some(home) = home {
        paths.push((home.join(".config/ia.ini"), false));
        paths.push((home.join(".ia"), false));
    }
    paths
}

fn discover_from_paths(
    paths: &[(PathBuf, bool)],
) -> Result<Option<ArchiveUploadCredentials>, String> {
    for (path, is_toml) in paths {
        // Inspect before opening: opening a FIFO/device can itself block forever.
        let before =
            match fs::metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => return Err(
                    "Could not inspect an Archive credential file; check its access permissions"
                        .into(),
                ),
            };
        if !before.is_file() || before.len() > MAX_CREDENTIAL_FILE_BYTES {
            return Err("Archive credential files must be regular files of at most 64 KiB".into());
        }
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                return Err(
                    "Could not read an Archive credential file; check its access permissions"
                        .into(),
                );
            }
        };
        let metadata = file
            .metadata()
            .map_err(|_| "Could not inspect an Archive credential file".to_string())?;
        if !metadata.is_file() || metadata.len() > MAX_CREDENTIAL_FILE_BYTES {
            return Err("Archive credential files must be regular files of at most 64 KiB".into());
        }
        let mut text = String::new();
        Read::by_ref(&mut file)
            .take(MAX_CREDENTIAL_FILE_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(|_| "Could not read an Archive credential file as UTF-8".to_string())?;
        if text.len() as u64 > MAX_CREDENTIAL_FILE_BYTES {
            return Err("Archive credential file exceeds 64 KiB".into());
        }
        let credentials = if *is_toml {
            parse_toml_credentials(&text)
        } else {
            parse_ini_credentials(&text)
        }?;
        return Ok(Some(credentials));
    }
    Ok(None)
}

fn parse_toml_credentials(text: &str) -> Result<ArchiveUploadCredentials, String> {
    #[derive(Deserialize)]
    struct Keys {
        #[serde(alias = "access")]
        access_key: String,
        #[serde(alias = "secret")]
        secret_key: String,
    }
    let keys: Keys = toml::from_str(text).map_err(|_| {
        "Invalid Archive credential TOML; expected access_key and secret_key".to_string()
    })?;
    ArchiveUploadCredentials::new(keys.access_key, keys.secret_key)
}

/// Parses the small non-interpolating `[s3]` subset of the official INI schema.
fn parse_ini_credentials(text: &str) -> Result<ArchiveUploadCredentials, String> {
    let mut in_s3 = false;
    let (mut access, mut secret) = (None, None);
    for line in text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with(['#', ';']))
    {
        if line.starts_with('[') {
            in_s3 = line == "[s3]";
            continue;
        }
        if !in_s3 {
            continue;
        }
        let Some((key, value)) = line.split_once('=').or_else(|| line.split_once(':')) else {
            return Err("Invalid Archive [s3] credential settings".into());
        };
        let slot = match key.trim().to_ascii_lowercase().as_str() {
            "access" => &mut access,
            "secret" => &mut secret,
            _ => continue,
        };
        if slot.replace(value.trim().to_string()).is_some() {
            return Err("Duplicate Archive [s3] credential setting".into());
        }
    }
    ArchiveUploadCredentials::new(
        access.ok_or("Archive INI is missing [s3] access")?,
        secret.ok_or("Archive INI is missing [s3] secret")?,
    )
}

/// Bytes consumed by the streaming uploader, not proof of server publication.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ArchiveUploadProgress {
    /// Bytes consumed from the prepared file so far.
    pub sent_bytes: u64,
    /// Exact original file length.
    pub total_bytes: u64,
}

/// Successful IAS3 acceptance; Archive ingestion can finish asynchronously.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveUploadResult {
    /// Canonical public item page.
    pub item_url: Url,
}

/// Bounded, streaming uploader; production endpoints are not configurable.
#[derive(Clone, Debug)]
pub struct ArchiveUploadClient {
    endpoint: Url,
    io_timeout: Duration,
}

impl Default for ArchiveUploadClient {
    fn default() -> Self {
        Self {
            endpoint: Url::parse(IAS3_ENDPOINT).expect("fixed IAS3 URL"),
            io_timeout: IO_TIMEOUT,
        }
    }
}

impl ArchiveUploadClient {
    /// Creates an uploader without reading credentials or making requests.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn for_test(endpoint: Url) -> Self {
        Self {
            endpoint,
            io_timeout: Duration::from_millis(250),
        }
    }

    /// Uploads one prepared file after validation and a fail-closed existence check.
    ///
    /// A cancellation or error after PUT begins may leave a staged public upload.
    /// Callers must show the error and must not silently retry the same draft.
    /// Cancellation is checked between 64 KiB reads and every socket operation;
    /// an already-blocked socket operation times out after at most 15 seconds.
    /// The whole PUT is limited to two hours, and the file to 20 GiB. Progress
    /// counts bytes consumed locally, not acknowledgements from Archive.
    ///
    /// Fresh 128-bit identifiers, bucket preflight, and `If-None-Match: *` avoid
    /// ordinary accidental replacement. IAS3 does not document atomic conditional
    /// PUT support, so this is not an atomic create-only guarantee against races.
    ///
    /// # Errors
    /// Rejects invalid/unconfirmed drafts, existing or ambiguous item identifiers,
    /// unsafe/changed files, cancellation and unconfirmed network outcomes. Once
    /// PUT starts, errors warn that publication may already have happened.
    pub fn upload_file(
        &self,
        credentials: &ArchiveUploadCredentials,
        draft: &ArchiveUploadDraft,
        path: &Path,
        filename: &str,
        cancellation: &Arc<AtomicBool>,
        progress: impl FnMut(ArchiveUploadProgress),
    ) -> Result<ArchiveUploadResult, String> {
        draft.validate()?;
        let content_type = validate_filename(filename, draft.upload_video)?;
        check_cancelled(cancellation)
            .map_err(|_| "Archive upload cancelled before publication".to_string())?;
        let (mut file, snapshot) = open_prepared_file(path)?;
        let mut bucket_url = self.endpoint.clone();
        bucket_url
            .path_segments_mut()
            .map_err(|_| "Invalid fixed IAS3 endpoint".to_string())?
            .push(&draft.identifier);
        let mut file_url = bucket_url.clone();
        file_url
            .path_segments_mut()
            .map_err(|_| "Invalid fixed IAS3 endpoint".to_string())?
            .push(filename);
        let item_url = Url::parse(&format!("https://archive.org/details/{}", draft.identifier))
            .map_err(|_| "Invalid Archive item identifier".to_string())?;
        let agent = upload_agent(Arc::clone(cancellation), self.io_timeout, PREFLIGHT_TIMEOUT);
        let response = agent
            .head(bucket_url.as_str())
            .config()
            .timeout_global(Some(PREFLIGHT_TIMEOUT))
            .build()
            .call()
            .map_err(|_| {
                "Could not verify that the Archive item is absent; nothing was uploaded".to_string()
            })?;
        match response.status().as_u16() {
			404 => {}
			200..=399 => return Err("Archive item already exists or redirects; choose a new identifier. Nothing was uploaded".into()),
			_ => return Err("Could not verify that the Archive item is absent; nothing was uploaded".into()),
		}
        drop(response);
        check_cancelled(cancellation)
            .map_err(|_| "Archive upload cancelled before publication".to_string())?;
        if !snapshot.matches(path, &file) {
            return Err("Prepared media changed before upload; prepare it again".into());
        }
        let agent = upload_agent(Arc::clone(cancellation), self.io_timeout, UPLOAD_TIMEOUT);
        let mut request = agent
            .put(file_url.as_str())
            .header("Authorization", credentials.authorization()?)
            .header("Content-Length", snapshot.len.to_string())
            .header("Content-Type", content_type)
            .header("If-None-Match", "*")
            .header("x-archive-auto-make-bucket", "1")
            .header("x-archive-ignore-preexisting-bucket", "0")
            .header("x-archive-keep-old-version", "1")
            .header("x-archive-size-hint", snapshot.len.to_string())
            .header(
                "x-archive-meta-mediatype",
                if draft.upload_video {
                    "movies"
                } else {
                    "audio"
                },
            )
            .header(
                "x-archive-meta-collection",
                if draft.upload_video {
                    "opensource_movies"
                } else {
                    "opensource_audio"
                },
            )
            .header("x-archive-meta-title", metadata_header(&draft.title))
            .header(
                "x-archive-meta-description",
                metadata_header(&draft.description),
            )
            .header("x-archive-meta-source", metadata_header(&draft.source_url));
        if !draft.creator.trim().is_empty() {
            request = request.header("x-archive-meta-creator", metadata_header(&draft.creator));
        }
        let mut reader = UploadReader {
            file: &mut file,
            cancellation,
            progress,
            sent: 0,
            total: snapshot.len,
        };
        (reader.progress)(ArchiveUploadProgress {
            sent_bytes: 0,
            total_bytes: snapshot.len,
        });
        check_cancelled(cancellation)
            .map_err(|_| "Archive upload cancelled before publication".to_string())?;
        let response = request.send(ureq::SendBody::from_reader(&mut reader));
        let sent = reader.sent;
        let response = response
            .map_err(|_| uncertain_error(&item_url, cancellation.load(Ordering::Acquire)))?;
        let status = response.status().as_u16();
        if !(200..=299).contains(&status) {
            let reason = match status {
                301..=399 => "Archive requested a redirect; credentials were not forwarded",
                401 | 403 => "Archive rejected the credentials or publication permissions",
                409 | 412 => "Archive rejected a conflicting or existing upload",
                503 => "Archive is busy and did not confirm the upload",
                _ => "Archive did not confirm the upload",
            };
            return Err(format!("{reason}. {}", uncertain_error(&item_url, false)));
        }
        if sent != snapshot.len || !snapshot.matches(path, &file) {
            return Err(format!(
                "Prepared media changed during upload. {}",
                uncertain_error(&item_url, false)
            ));
        }
        Ok(ArchiveUploadResult { item_url })
    }
}

fn uncertain_error(item_url: &Url, cancelled: bool) -> String {
    format!(
        "Archive upload {} after publication may have started. Check {item_url} before retrying; a partial or complete public upload may exist",
        if cancelled { "cancelled" } else { "failed" }
    )
}

fn metadata_header(value: &str) -> String {
    format!("uri({})", utf8_percent_encode(value, NON_ALPHANUMERIC))
}

fn validate_filename(filename: &str, video: bool) -> Result<&'static str, String> {
    if filename.is_empty()
        || filename.len() > 200
        || filename.starts_with('.')
        || !filename
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("Prepared Archive filename must be a simple safe media filename".into());
    }
    match (video, filename.rsplit('.').next()) {
        (false, Some("opus")) => Ok("audio/ogg"),
        (true, Some("mkv")) => Ok("video/x-matroska"),
        (true, Some("mp4")) => Ok("video/mp4"),
        (true, Some("webm")) => Ok("video/webm"),
        _ => Err("Prepared media format does not match the reviewed Archive upload choice".into()),
    }
}

/// Portable immutable-file snapshot; the opened descriptor is used throughout.
struct FileSnapshot {
    len: u64,
    modified: Option<std::time::SystemTime>,
    identity: Option<crate::file_identity::FilesystemIdentity>,
}

impl FileSnapshot {
    fn from_metadata(path: &Path, metadata: &fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            identity: crate::file_identity::filesystem_identity(path, metadata),
        }
    }

    fn same_metadata(&self, path: &Path, metadata: &fs::Metadata) -> bool {
        metadata.is_file()
            && metadata.len() == self.len
            && metadata.modified().ok() == self.modified
            && crate::file_identity::filesystem_identity(path, metadata) == self.identity
    }

    fn matches(&self, path: &Path, file: &File) -> bool {
        file.metadata()
            .is_ok_and(|metadata| self.same_metadata(path, &metadata))
            && fs::symlink_metadata(path).is_ok_and(|metadata| self.same_metadata(path, &metadata))
    }
}

fn open_prepared_file(path: &Path) -> Result<(File, FileSnapshot), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| "Prepared Archive media is unavailable".to_string())?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_ARCHIVE_UPLOAD_BYTES {
        return Err(
            "Prepared Archive media must be a nonempty regular file of at most 20 GiB".into(),
        );
    }
    let snapshot = FileSnapshot::from_metadata(path, &metadata);
    let file = File::open(path).map_err(|_| "Could not open prepared Archive media".to_string())?;
    if !snapshot.matches(path, &file) {
        return Err("Prepared Archive media changed while opening it".into());
    }
    Ok((file, snapshot))
}

/// Streams at most one fixed-size chunk per read and never buffers the file.
struct UploadReader<'a, Progress> {
    file: &'a mut File,
    cancellation: &'a Arc<AtomicBool>,
    progress: Progress,
    sent: u64,
    total: u64,
}

impl<Progress: FnMut(ArchiveUploadProgress)> Read for UploadReader<'_, Progress> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        check_cancelled(self.cancellation)?;
        let length = buffer
            .len()
            .min(STREAM_CHUNK_BYTES)
            .min(usize::try_from(self.total.saturating_sub(self.sent)).unwrap_or(usize::MAX));
        if length == 0 {
            return Ok(0);
        }
        let count = self.file.read(&mut buffer[..length])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Prepared upload shortened",
            ));
        }
        self.sent += count as u64;
        (self.progress)(ArchiveUploadProgress {
            sent_bytes: self.sent,
            total_bytes: self.total,
        });
        check_cancelled(self.cancellation)?;
        Ok(count)
    }
}

fn check_cancelled(cancellation: &AtomicBool) -> io::Result<()> {
    if cancellation.load(Ordering::Acquire) {
        Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "Archive upload cancelled",
        ))
    } else {
        Ok(())
    }
}

/// Uses an absolute deadline for partial writes, not a restarting write_all
/// timeout. The callback sets the underlying socket timeout to the remaining
/// budget before each write; cancellation also interrupts between partial writes.
fn write_with_budget(
    bytes: &[u8],
    cancellation: &AtomicBool,
    budget: Duration,
    mut write: impl FnMut(&[u8], Duration) -> io::Result<usize>,
) -> io::Result<()> {
    let started = std::time::Instant::now();
    let mut sent = 0;
    while sent < bytes.len() {
        check_cancelled(cancellation)?;
        let remaining = budget
            .checked_sub(started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::TimedOut, "Archive upload write deadline")
            })?;
        match write(&bytes[sent..], remaining) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "Archive upload socket closed",
                ));
            }
            Ok(count) if count <= bytes.len() - sent => sent += count,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Invalid socket write count",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Only the TCP boundary is specialized; ureq still owns HTTP and rustls owns
/// TLS verification. No proxy, redirect, reconnect or upload retry is provided.
#[derive(Debug)]
struct UploadConnector {
    cancellation: Arc<AtomicBool>,
    io_timeout: Duration,
    deadline: std::time::Instant,
}

impl Connector<()> for UploadConnector {
    type Out = UploadTransport;
    fn connect(
        &self,
        details: &ConnectionDetails<'_>,
        _: Option<()>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        let started = std::time::Instant::now();
        let connect_budget = details
            .timeout
            .not_zero()
            .map(|value| *value)
            .unwrap_or(self.io_timeout)
            .min(self.deadline.saturating_duration_since(started));
        let mut last_error = io::Error::new(
            io::ErrorKind::NotConnected,
            "Archive host has no usable address",
        );
        for (index, address) in details.addrs.iter().enumerate() {
            check_cancelled(&self.cancellation)?;
            let remaining = connect_budget.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(
                    io::Error::new(io::ErrorKind::TimedOut, "Archive connection deadline").into(),
                );
            }
            let share = remaining / u32::try_from(details.addrs.len() - index).unwrap_or(u32::MAX);
            match std::net::TcpStream::connect_timeout(address, share.max(Duration::from_millis(1)))
            {
                Ok(stream) => {
                    stream.set_nodelay(true)?;
                    return Ok(Some(UploadTransport {
                        stream,
                        buffers: LazyBuffers::new(
                            details.config.input_buffer_size(),
                            details.config.output_buffer_size(),
                        ),
                        cancellation: Arc::clone(&self.cancellation),
                        io_timeout: self.io_timeout,
                        deadline: self.deadline,
                    }));
                }
                Err(error) => last_error = error,
            }
        }
        Err(last_error.into())
    }
}

#[derive(Debug)]
struct UploadTransport {
    stream: std::net::TcpStream,
    buffers: LazyBuffers,
    cancellation: Arc<AtomicBool>,
    io_timeout: Duration,
    deadline: std::time::Instant,
}

impl UploadTransport {
    fn budget(&self, timeout: NextTimeout) -> Result<Duration, ureq::Error> {
        check_cancelled(&self.cancellation)?;
        let budget = (*timeout.after).min(self.io_timeout).min(
            self.deadline
                .saturating_duration_since(std::time::Instant::now()),
        );
        if budget.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "Archive upload deadline").into());
        }
        Ok(budget)
    }
}

impl Transport for UploadTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }
    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        let budget = self.budget(timeout)?;
        let stream = &mut self.stream;
        write_with_budget(
            &self.buffers.output()[..amount],
            &self.cancellation,
            budget,
            |bytes, remaining| {
                stream.set_write_timeout(Some(remaining))?;
                stream.write(bytes)
            },
        )?;
        Ok(())
    }
    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        let budget = self.budget(timeout)?;
        self.stream.set_read_timeout(Some(budget))?;
        let count = self.stream.read(self.buffers.input_append_buf())?;
        self.buffers.input_appended(count);
        Ok(count > 0)
    }
    fn is_open(&mut self) -> bool {
        // Pooling is disabled; a remotely closed socket fails on its next I/O.
        !self.cancellation.load(Ordering::Acquire) && std::time::Instant::now() < self.deadline
    }
}

fn upload_agent(
    cancellation: Arc<AtomicBool>,
    io_timeout: Duration,
    global_timeout: Duration,
) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .max_idle_connections(0)
        .timeout_global(Some(global_timeout))
        .timeout_resolve(Some(Duration::from_secs(10)))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_send_request(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .max_response_header_size(32 * 1024)
        .input_buffer_size(STREAM_CHUNK_BYTES)
        .output_buffer_size(STREAM_CHUNK_BYTES)
        .user_agent(concat!("youta/", env!("CARGO_PKG_VERSION")))
        .build();
    let connector = UploadConnector {
        cancellation,
        io_timeout,
        deadline: std::time::Instant::now() + global_timeout,
    }
    .chain(RustlsConnector::default());
    ureq::Agent::with_parts(config, connector, DefaultResolver::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_writes_do_not_restart_the_operation_deadline() {
        let started = std::time::Instant::now();
        let result = write_with_budget(
            &[0; 30],
            &AtomicBool::new(false),
            Duration::from_millis(15),
            |_, _| {
                std::thread::sleep(Duration::from_millis(2));
                Ok(1)
            },
        );
        assert!(result.is_err(), "partial writes restarted their timeout");
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn cancellation_is_checked_between_partial_socket_writes() {
        let cancellation = AtomicBool::new(false);
        let mut writes = 0;
        let result = write_with_budget(&[0; 30], &cancellation, Duration::from_secs(1), |_, _| {
            writes += 1;
            cancellation.store(true, Ordering::Release);
            Ok(1)
        });
        assert!(result.is_err());
        assert_eq!(writes, 1);
    }

    fn source() -> Url {
        Url::parse("https://www.youtube.com/watch?v=abcdefghijk").expect("fixture source")
    }

    #[test]
    fn drafts_generate_distinct_identifiers_without_a_rights_checkbox() {
        let first = ArchiveUploadDraft::new(
            source(),
            "Title".into(),
            "Full description".into(),
            Some("Creator".into()),
        )
        .expect("first draft");
        let second = ArchiveUploadDraft::new(source(), "Title".into(), String::new(), None)
            .expect("second draft");
        assert_ne!(first.identifier, second.identifier);
        assert!(first.identifier.starts_with("youtube-abcdefghijk-"));
        assert_eq!(first.identifier.len(), "youtube-abcdefghijk-".len() + 32);
        assert!(!first.upload_video);
        assert_eq!(first.creator, "Creator");
        assert!(first.validate().is_ok());
        assert!(
            serde_json::to_value(&first)
                .unwrap()
                .get("rights_confirmed")
                .is_none()
        );
    }

    #[test]
    fn credential_debug_never_contains_fake_keys() {
        let credentials = ArchiveUploadCredentials::new("FAKE_ACCESS".into(), "FAKE_SECRET".into())
            .expect("fixture credentials");
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("FAKE_ACCESS"));
        assert!(!debug.contains("FAKE_SECRET"));
        assert!(debug.contains("REDACTED"));
    }

    /// Loopback-only HTTP fixture; its credentials and bytes are synthetic.
    struct Server {
        url: Url,
        requests: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    /// Windows inherits the listener's nonblocking mode; parsing needs blocking reads.
    fn configure_fixture_stream(stream: &std::net::TcpStream) {
        stream
            .set_nonblocking(false)
            .expect("blocking fixture stream");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("fixture timeout");
    }

    /// Reproduces inherited Windows socket mode without timing-dependent reads.
    #[cfg(unix)]
    #[test]
    fn fixture_stream_configuration_clears_inherited_nonblocking_mode() {
        use rustix::fs::{OFlags, fcntl_getfl};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback listener");
        let _client = std::net::TcpStream::connect_timeout(
            &listener.local_addr().expect("fixture address"),
            std::time::Duration::from_secs(2),
        )
        .expect("fixture client");
        let (stream, _) = listener.accept().expect("accepted fixture stream");
        stream.set_nonblocking(true).expect("inherited socket mode");
        assert!(fcntl_getfl(&stream).unwrap().contains(OFlags::NONBLOCK));

        configure_fixture_stream(&stream);

        assert!(
            !fcntl_getfl(&stream).unwrap().contains(OFlags::NONBLOCK),
            "fixture request parsing requires a blocking stream on every platform"
        );
        assert!(stream.read_timeout().unwrap().is_some());
    }

    impl Server {
        fn new(responses: Vec<(u16, Option<String>)>) -> Self {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback listener");
            listener
                .set_nonblocking(true)
                .expect("nonblocking listener");
            let url = Url::parse(&format!(
                "http://{}/",
                listener.local_addr().expect("fixture address")
            ))
            .expect("fixture URL");
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let captured = Arc::clone(&requests);
            let stop = Arc::new(AtomicBool::new(false));
            let worker_stop = Arc::clone(&stop);
            let thread = std::thread::spawn(move || {
                let mut responses = responses.into_iter();
                while !worker_stop.load(std::sync::atomic::Ordering::Acquire) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                            continue;
                        }
                        Err(error) => panic!("fixture listener failed: {error}"),
                    };
                    configure_fixture_stream(&stream);
                    let mut request = Vec::new();
                    let mut byte = [0];
                    while !request.ends_with(b"\r\n\r\n") && request.len() < 256 * 1024 {
                        if stream.read(&mut byte).unwrap_or(0) == 0 {
                            break;
                        }
                        request.push(byte[0]);
                    }
                    let headers = String::from_utf8_lossy(&request);
                    let size = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    assert!(size <= 2 * 1024 * 1024, "bounded fixture body");
                    let mut body = Vec::new();
                    let _ = std::io::Read::by_ref(&mut stream)
                        .take(size as u64)
                        .read_to_end(&mut body);
                    request.extend(body);
                    captured.lock().expect("fixture requests").push(request);
                    let (status, location) = responses.next().unwrap_or((500, None));
                    if status == 0 {
                        std::thread::sleep(std::time::Duration::from_millis(500));
                        continue;
                    }
                    let redirect =
                        location.map_or_else(String::new, |value| format!("Location: {value}\r\n"));
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} Fixture\r\n{redirect}Content-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                }
            });
            Self {
                url,
                requests,
                stop,
                thread: Some(thread),
            }
        }

        fn client(&self) -> ArchiveUploadClient {
            ArchiveUploadClient::for_test(self.url.clone())
        }

        fn requests(&self) -> Vec<Vec<u8>> {
            self.requests.lock().expect("requests").clone()
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Release);
            if let Some(thread) = self.thread.take() {
                thread.join().expect("fixture worker");
            }
        }
    }

    fn confirmed_draft() -> ArchiveUploadDraft {
        ArchiveUploadDraft {
            identifier: "youtube-abcdefghijk-0123456789abcdef0123456789abcdef".into(),
            title: "Title ქართული".into(),
            description: "Complete\nUnicode 😀 & <end>".into(),
            creator: "Creator".into(),
            source_url: source().to_string(),
            ..ArchiveUploadDraft::default()
        }
    }

    fn fake_credentials() -> ArchiveUploadCredentials {
        ArchiveUploadCredentials::new("FAKE_ACCESS".into(), "FAKE_SECRET".into())
            .expect("fake keys")
    }

    fn prepared_file(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new().expect("prepared fixture");
        file.write_all(bytes).expect("fixture bytes");
        file
    }

    #[test]
    fn existing_item_or_ambiguous_preflight_never_sends_put() {
        for status in [200, 301, 307, 403, 500] {
            let server = Server::new(vec![(status, Some("https://foreign.example/steal".into()))]);
            let file = prepared_file(b"audio");
            let error = server
                .client()
                .upload_file(
                    &fake_credentials(),
                    &confirmed_draft(),
                    file.path(),
                    "audio.opus",
                    &Arc::new(AtomicBool::new(false)),
                    |_| {},
                )
                .expect_err("must refuse");
            assert!(!error.contains("FAKE_"));
            let requests = server.requests();
            assert_eq!(requests.len(), 1);
            assert!(requests[0].starts_with(b"HEAD "));
            assert!(
                !String::from_utf8_lossy(&requests[0])
                    .to_lowercase()
                    .contains("authorization:")
            );
        }
    }

    #[test]
    fn streams_exact_bytes_known_length_and_complete_unicode_metadata() {
        let server = Server::new(vec![(404, None), (200, None)]);
        let bytes = vec![0xa5; 200_001];
        let file = prepared_file(&bytes);
        let draft = confirmed_draft();
        let mut progress = Vec::new();
        let result = server
            .client()
            .upload_file(
                &fake_credentials(),
                &draft,
                file.path(),
                "audio.opus",
                &Arc::new(AtomicBool::new(false)),
                |event| progress.push(event),
            )
            .expect("upload fixture");
        assert_eq!(
            result.item_url.as_str(),
            format!("https://archive.org/details/{}", draft.identifier)
        );
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        let put = &requests[1];
        let end = put
            .windows(4)
            .position(|value| value == b"\r\n\r\n")
            .expect("headers")
            + 4;
        let headers = String::from_utf8_lossy(&put[..end]).to_ascii_lowercase();
        assert_eq!(&put[end..], bytes);
        assert!(headers.contains("content-length: 200001\r\n"));
        assert!(!headers.contains("transfer-encoding:"));
        assert!(headers.contains("if-none-match: *\r\n"));
        assert!(headers.contains("x-archive-meta-mediatype: audio\r\n"));
        assert!(headers.contains(
            "x-archive-meta-description: uri(complete%0aunicode%20%f0%9f%98%80%20%26%20%3cend%3e)"
        ));
        assert_eq!(progress.first().expect("initial").sent_bytes, 0);
        assert_eq!(
            progress.last().expect("final").sent_bytes,
            bytes.len() as u64
        );
        assert!(
            progress
                .windows(2)
                .all(|pair| pair[0].sent_bytes <= pair[1].sent_bytes)
        );
    }

    #[test]
    fn draft_validation_rejects_unsafe_sources_and_unrepresentable_or_oversized_text() {
        for source in [
            "https://www.youtube.com/watch?v=abcdefghijk&list=abc",
            "http://www.youtube.com/watch?v=abcdefghijk",
            "https://user@www.youtube.com/watch?v=abcdefghijk",
            "https://www.youtube.com:443/watch?v=abcdefghijk",
            "https://www.youtube.com/watch?v=abcdefghijk#fragment",
            "https://foreign.example/watch?v=abcdefghijk",
            "https://youtu.be/abcdefghijk",
        ] {
            let mut draft = confirmed_draft();
            draft.source_url = source.into();
            assert!(draft.validate().is_err(), "noncanonical source accepted");
        }
        for identifier in [
            "",
            "../existing",
            ".hidden",
            "x/y",
            "x%2fy",
            "x?other",
            "x\\y",
            "x\r\nheader: injected",
        ] {
            let mut draft = confirmed_draft();
            draft.identifier = identifier.into();
            assert!(draft.validate().is_err(), "unsafe identifier accepted");
        }
        let mut draft = confirmed_draft();
        draft.description = "😀".repeat(MAX_ARCHIVE_UPLOAD_DESCRIPTION_BYTES / 4);
        assert!(draft.validate().is_ok());
        draft.description.push('x');
        assert!(draft.validate().expect_err("byte limit").contains("64 KiB"));
        draft.description = "line\tline\nline\r終".into();
        assert!(draft.validate().is_ok());
        draft.description.push('\0');
        assert!(draft.validate().expect_err("XML NUL").contains("XML"));
        draft.description.clear();
        draft.title.clear();
        assert!(draft.validate().is_err());
        assert!(ArchiveUploadDraft::default().validate_metadata().is_err());
    }

    #[test]
    fn credential_validation_and_http_debug_redact_all_keys() {
        for invalid in [
            "",
            "FAKE_ACCESS\nInjected",
            "FAKE:ACCESS",
            "FAKE ACCESS",
            "nonASCII😀",
        ] {
            let error = ArchiveUploadCredentials::new(invalid.into(), "FAKE_SECRET".into())
                .expect_err("invalid key");
            assert!(!error.contains("FAKE_"));
        }
        let authorization = fake_credentials().authorization().expect("auth value");
        assert!(authorization.is_sensitive());
        assert!(!format!("{authorization:?}").contains("FAKE_"));
    }

    #[test]
    fn standard_ini_and_private_toml_keep_raw_keys_without_interpolation() {
        let ini = "[cookies]\nlogged-in-sig=not-a-key\n[s3]\naccess = FAKE_ACCESS\nsecret = FAKE%SECRET+/=\n";
        let keys = parse_ini_credentials(ini).expect("standard ia.ini");
        assert_eq!(keys.access, "FAKE_ACCESS");
        assert_eq!(keys.secret, "FAKE%SECRET+/=");
        for text in [
            "access_key = 'FAKE_ACCESS'\nsecret_key = 'FAKE_SECRET'",
            "access = 'FAKE_ACCESS'\nsecret = 'FAKE_SECRET'",
        ] {
            assert_eq!(
                parse_toml_credentials(text).expect("private TOML").secret,
                "FAKE_SECRET"
            );
        }
        for invalid in [
            "[s3]\naccess=FAKE_ACCESS",
            "[s3]\naccess=FAKE_ACCESS\nsecret=FAKE_SECRET\nsecret=OTHER",
            "[other]\naccess=FAKE_ACCESS\nsecret=FAKE_SECRET",
        ] {
            let error = parse_ini_credentials(invalid).expect_err("invalid settings");
            assert!(!error.contains("FAKE_"));
        }
        let error = parse_toml_credentials("access_key='FAKE_ACCESS'\nsecret_key='FAKE_SECRET")
            .expect_err("invalid TOML");
        assert!(!error.contains("FAKE_"));
    }

    #[test]
    fn credential_discovery_uses_documented_precedence_without_reading_real_configs() {
        // A slash-rooted Unix spelling lacks the drive prefix that Windows
        // requires for `is_absolute`; construct every fixture from a native root.
        let directory = tempfile::tempdir().expect("fake configs");
        let config_dir = directory.path().join("youta");
        let explicit = directory.path().join("override.ini");
        let xdg = directory.path().join("xdg");
        let home = directory.path().join("home");
        assert!(xdg.is_absolute(), "fixture XDG path must be absolute");
        let paths = credential_paths(
            &config_dir,
            Some(explicit.clone()),
            Some(xdg.clone()),
            Some(&home),
        );
        assert_eq!(
            paths,
            vec![
                (config_dir.join("secrets/archive-org.toml"), true),
                (explicit, false),
                (xdg.join("internetarchive/ia.ini"), false),
                (home.join(".config/ia.ini"), false),
                (home.join(".ia"), false)
            ]
        );
        let fallback = credential_paths(
            &config_dir,
            None,
            Some(PathBuf::from("relative-xdg")),
            Some(&home),
        );
        assert_eq!(fallback[1].0, home.join(".config/internetarchive/ia.ini"));
        let first = directory.path().join("first.toml");
        let second = directory.path().join("second.ini");
        fs::write(&first, "access_key='FAKE_FIRST'\nsecret_key='FAKE_SECRET'").expect("fake TOML");
        fs::write(&second, "[s3]\naccess=FAKE_SECOND\nsecret=FAKE_SECRET").expect("fake INI");
        let candidates = vec![(first.clone(), true), (second, false)];
        assert_eq!(
            discover_from_paths(&candidates)
                .expect("discover")
                .expect("found")
                .access,
            "FAKE_FIRST"
        );
        fs::write(&first, "malformed='FAKE_SECRET").expect("malformed fake config");
        assert!(
            discover_from_paths(&candidates)
                .expect_err("do not select another account")
                .contains("Invalid")
        );
        assert!(
            discover_from_paths(&[(directory.path().join("missing"), true)])
                .expect("no credentials")
                .is_none()
        );
    }

    #[test]
    fn credential_files_are_bounded_before_parsing() {
        let directory = tempfile::tempdir().expect("fake config directory");
        let path = directory.path().join("oversized.ini");
        fs::write(&path, vec![b'x'; MAX_CREDENTIAL_FILE_BYTES as usize + 1])
            .expect("bounded fake data");
        assert!(
            discover_from_paths(&[(path, false)])
                .expect_err("oversized credentials")
                .contains("64 KiB")
        );
        assert!(discover_from_paths(&[(directory.path().to_path_buf(), false)]).is_err());
    }

    #[test]
    fn authenticated_redirects_are_never_followed_or_retried() {
        let trap = Server::new(vec![(200, None)]);
        let server = Server::new(vec![(404, None), (307, Some(trap.url.to_string()))]);
        let file = prepared_file(b"synthetic opus");
        let error = server
            .client()
            .upload_file(
                &fake_credentials(),
                &confirmed_draft(),
                file.path(),
                "audio.opus",
                &Arc::new(AtomicBool::new(false)),
                |_| {},
            )
            .expect_err("redirect blocked");
        assert!(error.contains("credentials were not forwarded"));
        assert!(error.contains("before retrying"));
        assert!(!error.contains("FAKE_"));
        assert_eq!(server.requests().len(), 2);
        assert!(trap.requests().is_empty());
    }

    #[test]
    fn upload_failure_never_retries_and_warns_about_uncertain_publication() {
        for status in [400, 401, 403, 409, 412, 500, 503] {
            let server = Server::new(vec![(404, None), (status, None)]);
            let file = prepared_file(b"audio");
            let draft = confirmed_draft();
            let error = server
                .client()
                .upload_file(
                    &fake_credentials(),
                    &draft,
                    file.path(),
                    "audio.opus",
                    &Arc::new(AtomicBool::new(false)),
                    |_| {},
                )
                .expect_err("upload failure");
            assert!(error.contains("before retrying"));
            assert!(error.contains(&format!("https://archive.org/details/{}", draft.identifier)));
            assert!(!error.contains("FAKE_"));
            assert_eq!(server.requests().len(), 2);
        }
    }

    #[test]
    fn cancelled_before_start_or_initial_progress_never_publishes() {
        for initially_cancelled in [true, false] {
            let server = Server::new(vec![(404, None)]);
            let file = prepared_file(b"audio");
            let cancellation = Arc::new(AtomicBool::new(initially_cancelled));
            let error = server
                .client()
                .upload_file(
                    &fake_credentials(),
                    &confirmed_draft(),
                    file.path(),
                    "audio.opus",
                    &cancellation,
                    |_| cancellation.store(true, Ordering::Release),
                )
                .expect_err("cancelled");
            assert!(error.contains("before publication"));
            assert_eq!(server.requests().len(), usize::from(!initially_cancelled));
        }
    }

    #[test]
    fn cancellation_during_streaming_stops_and_reports_possible_partial_publication() {
        let server = Server::new(vec![(404, None), (200, None)]);
        let file = prepared_file(&vec![0xa5; 1_000_000]);
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut last_progress = 0;
        let error = server
            .client()
            .upload_file(
                &fake_credentials(),
                &confirmed_draft(),
                file.path(),
                "audio.opus",
                &cancellation,
                |event| {
                    last_progress = event.sent_bytes;
                    if event.sent_bytes > 0 {
                        cancellation.store(true, Ordering::Release);
                    }
                },
            )
            .expect_err("cancelled in body");
        assert!(error.contains("cancelled"));
        assert!(error.contains("partial or complete public upload may exist"));
        assert!(last_progress <= STREAM_CHUNK_BYTES as u64);
    }

    #[test]
    fn stalled_server_is_bounded_and_does_not_receive_a_retry() {
        let server = Server::new(vec![(404, None), (0, None)]);
        let file = prepared_file(b"audio");
        let started = std::time::Instant::now();
        let error = server
            .client()
            .upload_file(
                &fake_credentials(),
                &confirmed_draft(),
                file.path(),
                "audio.opus",
                &Arc::new(AtomicBool::new(false)),
                |_| {},
            )
            .expect_err("bounded response wait");
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(error.contains("before retrying"));
        assert_eq!(server.requests().len(), 2);
    }

    #[test]
    fn video_mode_uses_movie_metadata_and_unsafe_filenames_never_request_network() {
        let server = Server::new(vec![(404, None), (200, None)]);
        let file = prepared_file(b"synthetic video");
        let mut draft = confirmed_draft();
        draft.upload_video = true;
        draft.creator.clear();
        server
            .client()
            .upload_file(
                &fake_credentials(),
                &draft,
                file.path(),
                "video.mkv",
                &Arc::new(AtomicBool::new(false)),
                |_| {},
            )
            .expect("video fixture");
        let requests = server.requests();
        let put = String::from_utf8_lossy(&requests[1]).to_ascii_lowercase();
        assert!(put.contains("x-archive-meta-mediatype: movies\r\n"));
        assert!(put.contains("x-archive-meta-collection: opensource_movies\r\n"));
        assert!(!put.contains("x-archive-meta-creator:"));
        for filename in [
            "../audio.opus",
            "x/y.opus",
            "x%2fy.opus",
            ".hidden.opus",
            "file.mp3",
            "x\r\nheader.opus",
        ] {
            assert!(
                server
                    .client()
                    .upload_file(
                        &fake_credentials(),
                        &confirmed_draft(),
                        file.path(),
                        filename,
                        &Arc::new(AtomicBool::new(false)),
                        |_| {}
                    )
                    .is_err()
            );
        }
        assert_eq!(server.requests().len(), 2);
    }

    #[test]
    fn changed_prepared_file_is_not_reported_as_success() {
        let server = Server::new(vec![(404, None), (200, None)]);
        let file = prepared_file(b"audio");
        let error = server
            .client()
            .upload_file(
                &fake_credentials(),
                &confirmed_draft(),
                file.path(),
                "audio.opus",
                &Arc::new(AtomicBool::new(false)),
                |event| {
                    if event.sent_bytes > 0 {
                        file.as_file()
                            .set_len(6)
                            .expect("grow fixture during upload");
                    }
                },
            )
            .expect_err("changed source");
        assert!(error.contains("changed during upload"));
        assert!(error.contains("before retrying"));
    }

    #[test]
    fn prepared_files_must_be_regular_nonempty_and_within_the_size_cap() {
        let directory = tempfile::tempdir().expect("fixture directory");
        assert!(open_prepared_file(directory.path()).is_err());
        let file = prepared_file(b"");
        assert!(open_prepared_file(file.path()).is_err());
        file.as_file()
            .set_len(MAX_ARCHIVE_UPLOAD_BYTES + 1)
            .expect("sparse oversized fixture");
        assert!(open_prepared_file(file.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn prepared_media_symlinks_are_not_followed() {
        let directory = tempfile::tempdir().expect("fixture directory");
        let file = prepared_file(b"audio");
        let symlink = directory.path().join("link.opus");
        std::os::unix::fs::symlink(file.path(), &symlink).expect("fixture symlink");
        assert!(open_prepared_file(&symlink).is_err());
    }
}
