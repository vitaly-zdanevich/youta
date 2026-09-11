//! Selected-only Web metadata scheduling, caching, and Local-style presentation.

use super::*;
use crate::web_metadata::{WebMediaMetadata, WebMetadataClient};

const SELECTION_DELAY: Duration = Duration::from_millis(200);
const CACHE_AGE: Duration = Duration::from_mins(5);
const CACHE_ENTRIES: usize = 128;

/// Injectable worker boundary, including optional artwork publication off the UI thread.
type MetadataLoader =
    dyn Fn(&url::Url, &AtomicBool, &Path, &Path) -> LoadedWebMetadata + Send + Sync;

/// Metadata and a private artwork URL; raw media bytes never enter the controller cache.
#[derive(Default)]
pub(super) struct LoadedWebMetadata {
    pub(super) metadata: WebMediaMetadata,
    pub(super) artwork: Option<url::Url>,
}

/// A cache result, including failed probes, with a finite session-only lifetime.
pub(super) struct CachedWebMetadata {
    pub(super) result: LoadedWebMetadata,
    stored: Instant,
}

/// One coalesced selection and at most one network/probe worker.
pub(super) struct WebMetadataState {
    pub(super) request: Option<(url::Url, Instant)>,
    pub(super) worker: Option<WebMetadataWorker>,
    pub(super) cache: HashMap<url::Url, CachedWebMetadata>,
    pub(super) loader: Arc<MetadataLoader>,
    cache_order: VecDeque<url::Url>,
}

impl Default for WebMetadataState {
    fn default() -> Self {
        Self {
            request: None,
            worker: None,
            cache: HashMap::new(),
            loader: Arc::new(load_web_metadata),
            cache_order: VecDeque::new(),
        }
    }
}

/// Cancellation is cooperative; the reader and helper each also enforce deadlines.
pub(super) struct WebMetadataWorker {
    url: url::Url,
    cancelled: Arc<AtomicBool>,
    response: Receiver<LoadedWebMetadata>,
    thread: JoinHandle<()>,
}

impl Drop for WebMetadataState {
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            worker.cancelled.store(true, AtomicOrdering::Relaxed);
        }
    }
}

impl AppController {
    /// Replaces speculative work on selection changes without waiting for a socket.
    pub(super) fn sync_web_metadata_selection(&mut self) {
        let selected = self
            .selected_web_entry()
            .filter(|entry| entry.media_kind().is_some())
            .map(|entry| entry.url.clone());
        let state = &mut self.web.metadata;
        if let Some(worker) = &state.worker
            && selected.as_ref() != Some(&worker.url)
        {
            worker.cancelled.store(true, AtomicOrdering::Relaxed);
        }
        let Some(url) = selected else {
            state.request = None;
            return;
        };
        if state
            .cache
            .get(&url)
            .is_some_and(|cached| cached.stored.elapsed() < CACHE_AGE)
        {
            state.request = None;
            return;
        }
        state.cache.remove(&url);
        state.cache_order.retain(|cached| cached != &url);
        if state.worker.as_ref().is_some_and(|worker| {
            worker.url == url && !worker.cancelled.load(AtomicOrdering::Relaxed)
        }) {
            state.request = None;
            return;
        }
        if state
            .request
            .as_ref()
            .is_none_or(|(pending, _)| *pending != url)
        {
            state.request = Some((url, Instant::now() + SELECTION_DELAY));
        }
    }

    /// Retries metadata on explicit Refresh, including previously unavailable fields.
    pub(super) fn invalidate_web_metadata(&mut self) {
        let state = &mut self.web.metadata;
        state.request = None;
        state.cache.clear();
        state.cache_order.clear();
        if let Some(worker) = &state.worker {
            worker.cancelled.store(true, AtomicOrdering::Relaxed);
        }
    }

    /// Caches bounded display fields only; HTTP response bodies and pictures are discarded.
    pub(super) fn cache_web_metadata(
        &mut self,
        url: url::Url,
        mut metadata: WebMediaMetadata,
        artwork: Option<url::Url>,
    ) {
        metadata.probe_prefix.clear();
        metadata.probe_prefix.shrink_to_fit();
        metadata.artwork = None;
        let state = &mut self.web.metadata;
        state.cache.insert(
            url.clone(),
            CachedWebMetadata {
                result: LoadedWebMetadata { metadata, artwork },
                stored: Instant::now(),
            },
        );
        state.cache_order.retain(|cached| cached != &url);
        state.cache_order.push_back(url);
        while state.cache.len() > CACHE_ENTRIES {
            if let Some(oldest) = state.cache_order.pop_front() {
                state.cache.remove(&oldest);
            }
        }
    }

    /// Projects only the current URL's cached metadata, never local filesystem actions.
    pub(super) fn apply_web_metadata_detail(&mut self) {
        self.sync_web_metadata_selection();
        let Some(entry) = self
            .selected_web_entry()
            .filter(|entry| entry.media_kind().is_some())
        else {
            return;
        };
        let cached = self.web.metadata.cache.get(&entry.url);
        let description =
            web_metadata_description(&entry.name, cached.map(|cached| &cached.result.metadata));
        let Some(details) = self.view.details.as_mut() else {
            return;
        };
        details.description = description;
        if let Some(cached) = cached {
            let metadata = &cached.result.metadata;
            if let Some(title) = &metadata.title {
                details.title.clone_from(title);
            }
            details.length = metadata
                .duration
                .map_or_else(String::new, |duration| format_seconds(duration.as_secs()));
            details.thumbnail_url.clone_from(&cached.result.artwork);
        }
    }

    /// Drains completed work and starts only the latest settled selection.
    pub(super) fn poll_web_metadata(&mut self) {
        self.sync_web_metadata_selection();
        if self
            .web
            .metadata
            .worker
            .as_ref()
            .is_some_and(|worker| worker.thread.is_finished())
        {
            let worker = self
                .web
                .metadata
                .worker
                .take()
                .expect("finished metadata worker");
            let _ = worker.thread.join();
            if !worker.cancelled.load(AtomicOrdering::Relaxed) {
                let result = worker.response.try_recv().unwrap_or_default();
                let current = self
                    .selected_web_entry()
                    .is_some_and(|entry| entry.url == worker.url);
                self.cache_web_metadata(worker.url, result.metadata, result.artwork);
                if current {
                    self.update_web_detail();
                    self.refresh_selected_playlist_state();
                }
            }
        }
        if self.web.metadata.worker.is_some() {
            return;
        }
        self.sync_web_metadata_selection();
        if self
            .web
            .metadata
            .request
            .as_ref()
            .is_none_or(|(_, due)| Instant::now() < *due)
        {
            return;
        }
        let (url, _) = self
            .web
            .metadata
            .request
            .take()
            .expect("due metadata request");
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&cancelled);
        let worker_url = url.clone();
        let loader = Arc::clone(&self.web.metadata.loader);
        let executable = self.config.providers.ffprobe_executable.clone();
        let cache_directory = self.config.thumbnail_cache_dir();
        let (sender, response) = bounded(1);
        if let Ok(thread) = thread::Builder::new()
            .name("youta-web-metadata".to_owned())
            .spawn(move || {
                let result = loader(&worker_url, &cancel, &executable, &cache_directory);
                if !cancel.load(AtomicOrdering::Relaxed) {
                    let _ = sender.send(result);
                }
            })
        {
            self.web.metadata.worker = Some(WebMetadataWorker {
                url,
                cancelled,
                response,
                thread,
            });
        } else {
            self.cache_web_metadata(url, WebMediaMetadata::default(), None);
            self.update_web_detail();
        }
    }
}

/// Reads native tags first; unsupported video headers use an isolated, pipe-only `FFprobe`.
fn load_web_metadata(
    url: &url::Url,
    cancelled: &AtomicBool,
    executable: &Path,
    cache_directory: &Path,
) -> LoadedWebMetadata {
    let mut metadata = WebMetadataClient::default().read_cancellable(url, cancelled);
    if !cancelled.load(AtomicOrdering::Relaxed) {
        super::web_probe::enrich_web_metadata(&mut metadata, url.path(), executable, cancelled);
    }
    #[cfg(feature = "local-artwork")]
    let artwork = (!cancelled.load(AtomicOrdering::Relaxed))
        .then(|| cache_web_embedded_artwork(metadata.artwork.take(), cache_directory))
        .flatten();
    #[cfg(not(feature = "local-artwork"))]
    let artwork = {
        let _ = cache_directory;
        None
    };
    metadata.probe_prefix = Vec::new();
    LoadedWebMetadata { metadata, artwork }
}

/// Publishes supported image bytes through the existing bounded, private artwork cache.
#[cfg(feature = "local-artwork")]
fn cache_web_embedded_artwork(
    artwork: Option<crate::web_metadata::WebEmbeddedArtwork>,
    directory: &Path,
) -> Option<url::Url> {
    use crate::artwork::{ArtworkFormat, MAX_DOWNLOAD_BYTES, ThumbnailCache};
    use sha2::{Digest, Sha256};
    let artwork = artwork?;
    if artwork.bytes.len() > MAX_DOWNLOAD_BYTES || ArtworkFormat::sniff(&artwork.bytes).is_none() {
        return None;
    }
    // Content addressing exposes neither signed media URLs nor media titles on disk.
    let key = format!("youta-web-art-v1:{:x}", Sha256::digest(&artwork.bytes));
    let cache = ThumbnailCache::new(directory.to_path_buf());
    cache.store_key(key.as_bytes(), &artwork.bytes).ok()?;
    let path = crate::fs_path::canonicalize(cache.entry_path_for_key(key.as_bytes())).ok()?;
    url::Url::from_file_path(path).ok()
}

/// Uses Local's field labels and units, with no made-up values for unavailable fields.
fn web_metadata_description(filename: &str, metadata: Option<&WebMediaMetadata>) -> String {
    let mut lines = vec![format!("File: {filename}")];
    if let Some(metadata) = metadata {
        for (label, value) in [
            ("Artists", &metadata.artist),
            ("Album", &metadata.album),
            ("Genre", &metadata.genre),
            ("Comment", &metadata.comment),
        ] {
            if let Some(value) = value {
                lines.push(format!("{label}: {value}"));
            }
        }
        if let Some(duration) = metadata.duration {
            lines.push(format!("Length: {}", format_seconds(duration.as_secs())));
        }
        for (label, value) in [
            ("Container", &metadata.container),
            ("Codec", &metadata.codec),
        ] {
            if let Some(value) = value {
                lines.push(format!("{label}: {value}"));
            }
        }
        if let Some(size) = metadata.size_bytes {
            lines.push(format!("Size: {}", human_bytes(size)));
        }
        if let Some(value) = metadata.bitrate_kbps {
            lines.push(format!("Bitrate: {value} kb/s"));
        }
        if let Some(value) = metadata.sample_rate_hz {
            lines.push(format!("Sample rate: {value} Hz"));
        }
        if let Some(value) = metadata.channels {
            lines.push(format!("Channels: {value}"));
        }
    } else {
        lines.push("Loading metadata…".to_owned());
    }
    lines.push(String::new());
    lines.push("Audio only. With Autoplay enabled, playback continues through this folder in the displayed order.".to_owned());
    lines.join("\n")
}
