//! Durable manual download intent, independent of provider workers and credentials.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::domain::{MediaId, MediaKind, SourceKind};

/// Maximum retained pending and terminal download records.
pub const MAX_DOWNLOAD_QUEUE_ENTRIES: usize = 10_000;
/// Maximum encoded queue size accepted by either persistence backend.
pub const MAX_DOWNLOAD_QUEUE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 4 * 1024;
const MAX_URL_BYTES: usize = 16 * 1024;

/// Ordered durable work, including completed, failed, and cancelled records.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadQueue {
    /// Next never-reused positive job identifier.
    pub next_id: u64,
    /// User insertion order; terminal records stay until explicitly removed.
    pub entries: Vec<DownloadQueueEntry>,
}

impl Default for DownloadQueue {
    fn default() -> Self {
        Self {
            next_id: 1,
            entries: Vec::new(),
        }
    }
}

/// One captured source and its last durable execution outcome.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadQueueEntry {
    /// Stable identity, independent of row position and source identity.
    pub id: u64,
    /// Restart-safe source metadata, without resolved credentials or streams.
    pub source: DownloadSource,
    /// Confirmed output policy; absent while an explicit choice is required.
    pub format: Option<QueuedDownloadFormat>,
    /// Durable lifecycle state; transient byte progress is intentionally omitted.
    pub state: DownloadQueueState,
    /// Monotonic attempt owner, incremented before starting each worker.
    pub attempt: u64,
    /// Completed regular file relative to the current downloads directory.
    pub completed_path: Option<PathBuf>,
}

/// Compact replay metadata for every downloadable source, even when disabled.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadSource {
    /// Provider-qualified stable identity, never a signed resolved URL.
    pub media_id: MediaId,
    /// Playable media kind captured from the source.
    pub kind: MediaKind,
    /// Captured printable display title.
    pub title: String,
    /// Captured channel, author, or artist, when known.
    pub creator: Option<String>,
    /// Stable source page used for replay and provenance.
    pub webpage_url: Url,
    /// Stable downloader input, including an exact selected Archive file.
    pub download_url: Url,
    /// Known finite duration, in seconds.
    pub duration_seconds: Option<u64>,
}

/// Durable output policy, deliberately independent of compiled downloader features.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum QueuedDownloadFormat {
    /// Preserve the selected remote file unchanged.
    ExactFile,
    /// Merge the best video and audio without re-encoding.
    BestVideo,
    /// Extract audio without re-encoding.
    AudioOnlyWithoutReencoding,
    /// Prefer an existing Opus stream and remux it.
    OpusWithoutTranscoding,
    /// Keep the provider's best existing audio encoding.
    OriginalBestAudio,
    /// Explicitly allow lossy conversion to Opus.
    TranscodeToOpus,
    /// Resolve current authenticated original-quality Yandex media.
    YandexOriginal,
}

/// Durable state of one manual download.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum DownloadQueueState {
    /// Waiting for a choice or a free execution slot.
    Queued,
    /// An owned execution attempt started; restart requeues this state.
    Running,
    /// A validated output file was published successfully.
    Completed,
    /// An attempt or preparation failed and awaits explicit retry.
    Failed,
    /// The user cancelled this job; it does not resume automatically.
    Cancelled,
}

impl DownloadQueue {
    /// Checks bounds, stable identities, and lifecycle/path invariants.
    ///
    /// # Errors
    /// Returns a fixed diagnostic identifying the rejected invariant without
    /// including source URLs, credentials, or subprocess output.
    pub fn validate(&self) -> Result<(), String> {
        if self.next_id == 0 || self.entries.len() > MAX_DOWNLOAD_QUEUE_ENTRIES {
            return Err("download queue identity or row limit is invalid".to_owned());
        }
        let mut ids = HashSet::with_capacity(self.entries.len());
        for entry in &self.entries {
            if entry.id == 0 || entry.id >= self.next_id || !ids.insert(entry.id) {
                return Err("download job identifiers must be unique and below next_id".to_owned());
            }
            entry.source.validate()?;
            if let Some(format) = entry.format
                && (format == QueuedDownloadFormat::YandexOriginal)
                    != (entry.source.media_id.source == SourceKind::YandexMusic)
            {
                return Err("download format does not match its provider".to_owned());
            }
            if matches!(
                entry.state,
                DownloadQueueState::Running | DownloadQueueState::Completed
            ) && (entry.attempt == 0 || entry.format.is_none())
            {
                return Err("started downloads require an attempt and confirmed format".to_owned());
            }
            if (entry.state == DownloadQueueState::Completed) != entry.completed_path.is_some() {
                return Err("only completed downloads must retain an output path".to_owned());
            }
            if let Some(path) = &entry.completed_path {
                validate_completed_relative_path(path)?;
            }
        }
        serde_json::to_writer(QueueSizeLimit(0), self)
            .map_err(|_| "download queue exceeds its encoded byte limit".to_owned())?;
        Ok(())
    }

    /// Requeues interrupted workers while preserving choices and attempt owners.
    /// Returns whether a durable recovery write is needed before execution.
    pub fn recover_running(&mut self) -> bool {
        let mut changed = false;
        for entry in &mut self.entries {
            if entry.state == DownloadQueueState::Running {
                entry.state = DownloadQueueState::Queued;
                changed = true;
            }
        }
        changed
    }
}

impl DownloadSource {
    /// Validates credential-free source metadata without accessing the network.
    ///
    /// # Errors
    /// Returns a fixed diagnostic for unsafe URLs, identities, or display fields.
    pub fn validate(&self) -> Result<(), String> {
        validate_text(&self.title, MAX_TEXT_BYTES)?;
        if let Some(creator) = &self.creator {
            validate_text(creator, MAX_TEXT_BYTES)?;
        }
        validate_text(self.media_id.source.as_str(), 128)?;
        validate_text(&self.media_id.external_id, MAX_URL_BYTES)?;
        if !matches!(
            self.kind,
            MediaKind::Video | MediaKind::Audio | MediaKind::PodcastEpisode | MediaKind::LiveStream
        ) || self.media_id.source == SourceKind::Local
        {
            return Err("download source must be playable remote media".to_owned());
        }
        validate_remote_url(&self.webpage_url)?;
        validate_remote_url(&self.download_url)?;
        if let Ok(identity) = Url::parse(&self.media_id.external_id)
            && matches!(identity.scheme(), "http" | "https")
        {
            validate_remote_url(&identity)?;
        }
        match self.media_id.source {
            SourceKind::YouTube => {
                crate::providers::validate_youtube_video_id(&self.media_id.external_id)
                    .map_err(|_| "download YouTube identifier is invalid".to_owned())?;
                let expected = format!(
                    "https://www.youtube.com/watch?v={}",
                    self.media_id.external_id
                );
                if self.webpage_url.as_str() != expected || self.download_url.as_str() != expected {
                    return Err(
                        "download YouTube URLs must match the canonical video identity".to_owned(),
                    );
                }
            }
            SourceKind::ArchiveOrg => {
                if !crate::domain::is_canonical_archive_org_audio_url(&self.webpage_url)
                    || !crate::domain::is_canonical_archive_org_audio_url(&self.download_url)
                    || self.media_id.external_id != self.webpage_url.as_str()
                    || self
                        .webpage_url
                        .path_segments()
                        .and_then(|mut parts| parts.nth(1))
                        != self
                            .download_url
                            .path_segments()
                            .and_then(|mut parts| parts.nth(1))
                {
                    return Err(
                        "download Archive URLs must retain one canonical item identity".to_owned(),
                    );
                }
            }
            SourceKind::LibriVox => {
                if crate::domain::parse_librivox_section_external_id(&self.media_id.external_id)
                    .is_none()
                    || !crate::domain::is_canonical_librivox_audio_url(&self.download_url)
                    || !(crate::domain::is_canonical_librivox_book_url(&self.webpage_url)
                        || self.webpage_url == self.download_url)
                {
                    return Err(
                        "download LibriVox source must retain its stable chapter URL".to_owned(),
                    );
                }
            }
            SourceKind::YandexMusic => {
                let url = &self.webpage_url;
                let parts = url
                    .path_segments()
                    .map(Iterator::collect::<Vec<_>>)
                    .unwrap_or_default();
                if !self
                    .media_id
                    .external_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'-'))
                    || self.media_id.external_id.len() > 256
                    || url.scheme() != "https"
                    || url.query().is_some()
                    || url.port().is_some()
                    || !matches!(
                        url.host_str(),
                        Some(
                            "music.yandex.ru"
                                | "music.yandex.com"
                                | "music.yandex.by"
                                | "music.yandex.kz"
                                | "music.yandex.uz"
                        )
                    )
                    || parts.len() < 2
                    || parts[parts.len() - 2] != "track"
                    || parts.last().copied() != Some(self.media_id.external_id.as_str())
                    || self.download_url != self.webpage_url
                {
                    return Err(
                        "download Yandex source must retain a canonical track page".to_owned()
                    );
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn validate_text(value: &str, limit: usize) -> Result<(), String> {
    if value.is_empty()
        || value.len() > limit
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err("download metadata must be bounded printable text".to_owned());
    }
    Ok(())
}

/// Counts serialized bytes without allocating a second copy of oversized input.
struct QueueSizeLimit(usize);

impl std::io::Write for QueueSizeLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_DOWNLOAD_QUEUE_BYTES.saturating_sub(self.0) {
            return Err(std::io::Error::other("download queue byte limit exceeded"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Allows stable public or LAN URLs, but excludes common signed/authenticated queries.
fn validate_remote_url(url: &Url) -> Result<(), String> {
    if url.as_str().len() > MAX_URL_BYTES
        || !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "download URLs must be bounded HTTP(S) without credentials or fragments".to_owned(),
        );
    }
    for (key, value) in url.query_pairs() {
        let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
        if key.contains('%')
            || key.chars().any(char::is_control)
            || value.chars().any(char::is_control)
            || normalized.starts_with("xamz")
            || normalized.starts_with("xgoog")
            || matches!(
                normalized.as_str(),
                "token"
                    | "accesstoken"
                    | "refreshtoken"
                    | "authtoken"
                    | "authorization"
                    | "auth"
                    | "password"
                    | "passwd"
                    | "secret"
                    | "signature"
                    | "sig"
                    | "sign"
                    | "credential"
                    | "credentials"
                    | "apikey"
                    | "key"
                    | "awsaccesskeyid"
                    | "keypairid"
                    | "policy"
                    | "expires"
                    | "expiry"
                    | "jwt"
                    | "session"
                    | "sessionid"
                    | "hdnts"
                    | "hdnea"
            )
        {
            return Err(
                "download URLs cannot retain authentication or signed query fields".to_owned(),
            );
        }
    }
    Ok(())
}

/// Applies one portable relative-path policy on Unix and Windows alike.
fn validate_completed_relative_path(path: &Path) -> Result<(), String> {
    let text = path
        .to_str()
        .ok_or_else(|| "download completion path must be UTF-8".to_owned())?;
    if text.is_empty()
        || text.len() > MAX_URL_BYTES
        || text.chars().any(char::is_control)
        || text.contains(':')
        || text
            .split(['/', '\\'])
            .any(|part| matches!(part, "" | "." | ".."))
        || !path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        return Err("download completion path must be a normalized relative file path".to_owned());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Shared deterministic intent used by both persistence backend contracts.
    pub(crate) fn fixture() -> DownloadQueue {
        let url = Url::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap();
        DownloadQueue {
            next_id: 2,
            entries: vec![DownloadQueueEntry {
                id: 1,
                source: DownloadSource {
                    media_id: MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ"),
                    kind: MediaKind::Video,
                    title: "Fixture video".to_owned(),
                    creator: Some("Fixture creator".to_owned()),
                    webpage_url: url.clone(),
                    download_url: url,
                    duration_seconds: Some(60),
                },
                format: Some(QueuedDownloadFormat::BestVideo),
                state: DownloadQueueState::Queued,
                attempt: 0,
                completed_path: None,
            }],
        }
    }

    #[test]
    fn restart_requeues_only_running_intent_without_changing_its_choice() {
        let mut queue = fixture();
        queue.entries[0].state = DownloadQueueState::Running;
        queue.entries[0].attempt = 3;
        assert!(queue.recover_running());
        assert_eq!(queue.entries[0].state, DownloadQueueState::Queued);
        assert_eq!(queue.entries[0].attempt, 3);
        assert_eq!(
            queue.entries[0].format,
            Some(QueuedDownloadFormat::BestVideo)
        );
        assert!(!queue.recover_running());
    }

    #[test]
    fn queue_validation_rejects_identity_secrets_and_unsafe_completion_paths() {
        for url in [
            "https://user:password@example.org/file.mp3",
            "https://example.org/file.mp3?token=secret",
            "https://example.org/file.mp3?X-Amz-Signature=secret",
            "https://example.org/file.mp3?auth=secret",
            "https://example.org/file.mp3#secret",
            "https://example.org/file.mp3?%74oken=secret",
        ] {
            let mut queue = fixture();
            queue.entries[0].source.media_id = MediaId::new(SourceKind::RemoteFiles, url);
            assert!(queue.validate().is_err(), "unsafe identity: {url}");
        }
        for path in [
            "../escape.mp3",
            "/absolute.mp3",
            "nested/../escape.mp3",
            "C:\\escape.mp3",
            "nested\\..\\escape.mp3",
        ] {
            let mut queue = fixture();
            queue.entries[0].state = DownloadQueueState::Completed;
            queue.entries[0].attempt = 1;
            queue.entries[0].completed_path = Some(PathBuf::from(path));
            assert!(queue.validate().is_err(), "unsafe path: {path}");
        }
    }

    #[test]
    fn remote_files_retain_benign_queries_and_lan_http_without_feature_dependencies() {
        let mut queue = fixture();
        let url = Url::parse("http://192.168.1.3:8080/music?id=42&format=mp3").unwrap();
        queue.entries[0].source.media_id = MediaId::new(SourceKind::RemoteFiles, url.as_str());
        queue.entries[0].source.webpage_url = url.clone();
        queue.entries[0].source.download_url = url;
        queue.validate().unwrap();
        assert_eq!(
            serde_json::from_str::<DownloadQueue>(&serde_json::to_string(&queue).unwrap()).unwrap(),
            queue
        );
    }

    #[test]
    fn queue_validation_rejects_duplicate_ids_and_impossible_terminal_state() {
        let mut queue = fixture();
        queue.entries.push(queue.entries[0].clone());
        assert!(queue.validate().is_err());
        queue.entries.pop();
        queue.next_id = 1;
        assert!(queue.validate().is_err());
        queue.next_id = 2;
        queue.entries[0].state = DownloadQueueState::Completed;
        assert!(queue.validate().is_err());
    }

    #[test]
    fn provider_independent_records_round_trip_all_download_formats() {
        for format in [
            QueuedDownloadFormat::ExactFile,
            QueuedDownloadFormat::BestVideo,
            QueuedDownloadFormat::AudioOnlyWithoutReencoding,
            QueuedDownloadFormat::OpusWithoutTranscoding,
            QueuedDownloadFormat::OriginalBestAudio,
            QueuedDownloadFormat::TranscodeToOpus,
            QueuedDownloadFormat::YandexOriginal,
        ] {
            let mut queue = fixture();
            queue.entries[0].format = Some(format);
            if format == QueuedDownloadFormat::YandexOriginal {
                let source = &mut queue.entries[0].source;
                source.media_id = MediaId::new(SourceKind::YandexMusic, "303");
                source.webpage_url =
                    Url::parse("https://music.yandex.ru/album/101/track/303").unwrap();
                source.download_url = source.webpage_url.clone();
            }
            queue.validate().unwrap();
            let restored: DownloadQueue =
                serde_json::from_str(&serde_json::to_string(&queue).unwrap()).unwrap();
            assert_eq!(restored, queue);
        }
    }

    #[test]
    fn archive_choice_retains_exact_variant_and_rejects_another_item() {
        let mut queue = fixture();
        let source = &mut queue.entries[0].source;
        source.webpage_url =
            Url::parse("https://archive.org/download/fixture/playback.mp3").unwrap();
        source.media_id = MediaId::new(SourceKind::ArchiveOrg, source.webpage_url.as_str());
        source.download_url =
            Url::parse("https://archive.org/download/fixture/original.flac").unwrap();
        queue.entries[0].format = Some(QueuedDownloadFormat::ExactFile);
        queue.validate().unwrap();
        queue.entries[0].source.download_url =
            Url::parse("https://archive.org/download/unrelated/original.flac").unwrap();
        assert!(queue.validate().is_err());
    }

    #[test]
    fn completed_paths_support_native_separators_without_accepting_traversal() {
        for path in ["album/track.mp3", "album\\track.mp3"] {
            let mut queue = fixture();
            queue.entries[0].attempt = 1;
            queue.entries[0].state = DownloadQueueState::Completed;
            queue.entries[0].completed_path = Some(path.into());
            queue.validate().unwrap();
        }
    }

    #[test]
    fn queue_has_independent_row_and_serialized_byte_limits() {
        let mut queue = fixture();
        queue.entries = vec![queue.entries[0].clone(); MAX_DOWNLOAD_QUEUE_ENTRIES + 1];
        assert!(queue.validate().is_err());
        queue.entries.truncate(1000);
        queue.next_id = 1001;
        for (index, entry) in queue.entries.iter_mut().enumerate() {
            entry.id = index as u64 + 1;
            entry.source.title = "\"".repeat(MAX_TEXT_BYTES);
            entry.source.creator = Some("\"".repeat(MAX_TEXT_BYTES));
        }
        assert!(queue.validate().unwrap_err().contains("byte limit"));
    }
}
