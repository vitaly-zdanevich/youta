//! Session-scoped HTTP sharing for local files and podcast feeds.
//!
//! The server exposes an immutable, bounded manifest instead of translating
//! request paths back into filesystem paths. That keeps URL traversal and
//! post-start symlink changes outside the trust boundary. [`LanShareServer`]
//! owns its listener thread and stops it on drop, so sharing never survives a
//! Youta process that the user has closed.

use chrono::{DateTime, Datelike, Utc};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use sha2::{Digest, Sha256};
use url::Url;

use crate::local_browser::{LocalEntryKind, classify_local_file};
use crate::playback::youtube_prewarm::{
    PrewarmedYouTubeAudio, YouTubePlayerClientPolicy, YouTubePrewarmCancellation,
    YouTubePrewarmConfig, YouTubePrewarmRequest, YouTubePrewarmResolver,
};
use crate::playback::ytdlp::ExtractedCollection;
use crate::providers::validate_youtube_video_id;

const MAX_SHARED_FILES: usize = 10_000;
/// Includes active transfers, bounded per-class waiters, and header/control work.
const MAX_CONCURRENT_CONNECTIONS: usize = 32;
const MAX_MEDIA_CONNECTIONS: usize = 8;
const MAX_ARTWORK_CONNECTIONS: usize = 4;
const MAX_QUEUED_MEDIA_CONNECTIONS: usize = 8;
const MAX_QUEUED_ARTWORK_CONNECTIONS: usize = 4;
/// Absorbs short bursts without spending a podcast client's full read timeout.
const REQUEST_ADMISSION_TIMEOUT: Duration = Duration::from_secs(2);
/// Reserved feed/overload workers keep busy media workers from blocking accept.
const MAX_OVERLOAD_CONNECTIONS: usize = 4;
/// Caps draining overload request headers and sending the small retry response.
const OVERLOAD_RESPONSE_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_SCAN_DEPTH: usize = 64;
const MAX_REQUEST_LINE_BYTES: usize = 16 * 1024;
const MAX_REQUEST_HEADER_BYTES: usize = 64 * 1024;
const IO_POLL: Duration = Duration::from_millis(100);
/// Caps the complete request header while short socket polls permit shutdown.
const REQUEST_HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// Keeps a stalled feed or directory response from occupying a worker forever.
const CONTENT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const REMOTE_SETUP_TIMEOUT: Duration = Duration::from_secs(20);
/// Only failures received quickly are eligible for one initial GET retry.
const REMOTE_INITIAL_RETRY_WINDOW: Duration = Duration::from_secs(5);
/// Briefly spreads a retry without sleeping through server cancellation.
const REMOTE_INITIAL_RETRY_DELAY: Duration = Duration::from_millis(250);
/// Caps each setup phase of the retry, without limiting the streaming body.
const REMOTE_RETRY_PHASE_TIMEOUT: Duration = Duration::from_secs(2);
const REMOTE_RESOLUTION_CACHE_TTL: Duration = Duration::from_mins(30);
const MAX_REMOTE_RESUME_ATTEMPTS: usize = 4;
/// YouTube's ordinary HTTP downloader also uses bounded 10 MiB upstream ranges.
const YOUTUBE_REMOTE_CHUNK_SIZE: u64 = 10 * 1024 * 1024;
const YOUTUBE_PODCAST_AUDIO_FORMAT: &str = "bestaudio[ext=webm]";
const YOUTUBE_PODCAST_MIME: &str = "audio/webm";

/// What one immutable LAN server exposes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LanShareKind {
    /// A browser-friendly list of files, or one directly selected file.
    Files,
    /// An RSS podcast feed whose enclosures point back to this server.
    Podcast,
}

/// A prepared local share that has not opened a network listener yet.
#[derive(Debug)]
pub struct PreparedLocalShare {
    kind: LanShareKind,
    title: String,
    feed_artwork_route: Option<String>,
    files: Vec<SharedFile>,
    artwork: Vec<SharedArtwork>,
    remote_config: Option<YouTubePrewarmConfig>,
}

impl PreparedLocalShare {
    /// Returns the immutable number of files or podcast episodes to publish.
    #[must_use]
    pub fn item_count(&self) -> usize {
        self.files.len()
    }
}

/// One active server and the LAN URL suitable for a QR code.
#[derive(Debug)]
pub struct LanShareServer {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    remote_cancellation: Option<YouTubePrewarmCancellation>,
}

impl LanShareServer {
    /// Starts an immutable local share on an operating-system-selected port.
    ///
    /// # Errors
    ///
    /// Returns an error when Youta cannot bind a listener or determine its
    /// socket address.
    pub fn start(prepared: PreparedLocalShare) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let ip = discover_lan_ip().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let authority = match ip {
            IpAddr::V4(ip) => format!("{ip}:{port}"),
            IpAddr::V6(ip) => format!("[{ip}]:{port}"),
        };
        let base_url = format!("http://{authority}");
        let url = match prepared.kind {
            LanShareKind::Files if prepared.files.len() == 1 => {
                format!("{base_url}{}", prepared.files[0].route)
            }
            LanShareKind::Files => format!("{base_url}/"),
            LanShareKind::Podcast => format!("{base_url}/feed.xml"),
        };
        let state = Arc::new(ServerState::new(prepared, base_url));
        let remote_cancellation = state
            .remote
            .as_ref()
            .map(|remote| remote.cancellation.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("youta-lan-share".to_owned())
            .spawn(move || serve(listener, state, thread_stop))?;
        Ok(Self {
            url,
            stop,
            thread: Some(thread),
            remote_cancellation,
        })
    }

    /// Returns the LAN URL encoded into the UI QR code.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Stops the listener and waits for its bounded polling loop to exit.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(cancellation) = self.remote_cancellation.as_ref() {
            cancellation.cancel();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LanShareServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Builds a bounded browser share for one regular file or real directory.
///
/// # Errors
///
/// Returns an error for symlinks, unsupported target types, unreadable
/// entries, or a directory that exceeds the traversal bounds.
pub fn prepare_file_share(target: &Path) -> io::Result<PreparedLocalShare> {
    prepare_local_share(target, None, None)
}

/// Builds a bounded podcast feed from playable files under one target.
///
/// Embedded artwork is extracted into `artwork_cache`; a valid sidecar image
/// remains the fallback used by Youta's normal local-artwork policy.
/// Episode dates use each file's modification time; a filesystem does not
/// reliably expose the audio's original publication date.
///
/// # Errors
///
/// Returns an error for unsafe targets, traversal failures, an empty playable
/// selection, a missing or invalid modification date, or a directory that
/// exceeds the traversal bounds.
pub fn prepare_podcast_share(
    target: &Path,
    artwork_cache: &Path,
) -> io::Result<PreparedLocalShare> {
    prepare_local_share(target, Some(artwork_cache), None)
}

/// Builds a bounded podcast feed beginning with one selected file.
///
/// Files are ordered exactly as in a normal local podcast manifest: a
/// case-insensitive relative-path sort after the bounded recursive scan. The
/// selected file remains the first episode, while every earlier file is
/// omitted. This lets a visible Local row act as an inclusive resume boundary.
///
/// # Errors
///
/// Returns the same errors as [`prepare_podcast_share`], and also rejects a
/// symbolic-link boundary or a selected file absent from the playable
/// manifest below `target`.
pub fn prepare_podcast_share_from(
    target: &Path,
    artwork_cache: &Path,
    first_file: &Path,
) -> io::Result<PreparedLocalShare> {
    prepare_local_share(target, Some(artwork_cache), Some(first_file))
}

fn prepare_local_share(
    target: &Path,
    artwork_cache: Option<&Path>,
    first_file: Option<&Path>,
) -> io::Result<PreparedLocalShare> {
    let metadata = fs::symlink_metadata(target)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "symbolic-link sharing is disabled",
        ));
    }
    let canonical = fs::canonicalize(target)?;
    let title = canonical.file_name().map_or_else(
        || canonical.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let podcast = artwork_cache.is_some();
    let mut paths = Vec::new();
    if metadata.is_file() {
        if !podcast || is_playable(&canonical) {
            paths.push((
                canonical.clone(),
                canonical
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
    } else if metadata.is_dir() {
        collect_files(&canonical, &canonical, podcast, 0, &mut paths)?;
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only regular files and directories can be shared",
        ));
    }
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            if podcast {
                "no playable audio was found in the selected target"
            } else {
                "no regular files were found in the selected target"
            },
        ));
    }
    paths.sort_by_key(|(_, label)| label.to_lowercase());
    if let Some(first_file) = first_file {
        retain_podcast_files_from(&mut paths, first_file)?;
    }
    let mut artwork = Vec::new();
    let mut artwork_routes: HashMap<PathBuf, String> = HashMap::new();
    let mut files = Vec::with_capacity(paths.len());
    for (index, (path, label)) in paths.into_iter().enumerate() {
        let metadata = fs::metadata(&path)?;
        let route = format!(
            "/media/{index}/{}",
            utf8_percent_encode(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .as_ref(),
                NON_ALPHANUMERIC,
            )
        );
        let artwork_route = artwork_cache.and_then(|cache| {
            crate::local_artwork::local_media_artwork(&path, cache)
                .ok()
                .flatten()
                .and_then(|url| url.to_file_path().ok())
                .and_then(|artwork_path| {
                    let canonical_artwork = fs::canonicalize(artwork_path).ok()?;
                    if let Some(route) = artwork_routes.get(&canonical_artwork) {
                        return Some(route.clone());
                    }
                    let artwork_index = artwork.len();
                    let route = format!("/artwork/{artwork_index}");
                    artwork.push(SharedArtwork {
                        mime: mime_type(&canonical_artwork),
                        source: SharedArtworkSource::Local(canonical_artwork.clone()),
                    });
                    artwork_routes.insert(canonical_artwork, route.clone());
                    Some(route)
                })
        });
        let published_at = if podcast {
            Some(local_podcast_date(metadata.modified()?)?)
        } else {
            // Plain file serving does not depend on publication metadata.
            metadata
                .modified()
                .ok()
                .and_then(|time| local_podcast_date(time).ok())
        };
        files.push(SharedFile {
            guid: local_guid(&path, &metadata),
            label,
            published_at,
            length: metadata.len(),
            mime: mime_type(&path),
            source: SharedFileSource::Local(path),
            route,
            artwork_route,
        });
    }
    Ok(PreparedLocalShare {
        kind: if podcast {
            LanShareKind::Podcast
        } else {
            LanShareKind::Files
        },
        title,
        feed_artwork_route: if podcast {
            files.iter().find_map(|file| file.artwork_route.clone())
        } else {
            None
        },
        files,
        artwork,
        remote_config: None,
    })
}

/// Drops local podcast entries before one validated, inclusive file boundary.
fn retain_podcast_files_from(
    paths: &mut Vec<(PathBuf, String)>,
    first_file: &Path,
) -> io::Result<()> {
    let first_metadata = fs::symlink_metadata(first_file)?;
    if first_metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "symbolic-link podcast boundaries are disabled",
        ));
    }
    let canonical_first = fs::canonicalize(first_file)?;
    let Some(first_index) = paths.iter().position(|(path, _)| path == &canonical_first) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the selected file is absent from the playable podcast manifest",
        ));
    };
    paths.drain(..first_index);
    Ok(())
}

/// Builds a feed whose stable local enclosure routes resolve fresh `YouTube`
/// audio only when a podcast client requests an episode.
///
/// Metadata extraction downloads neither video nor audio. Each episode must
/// include its original publication date. The active LAN server supervises
/// each later `yt-dlp` resolver and proxies the resulting stream so required
/// request headers never leave Youta.
///
/// The input must use the unified newest-first upload order returned by
/// [`crate::playback::ytdlp::YtDlp::youtube_channel_collection`]. Episodes are
/// published oldest first so a podcast listener can follow channel chronology.
///
/// # Errors
///
/// When `skip_shorts` is set, entries with canonical `/shorts/` provider URLs
/// are omitted. Returns an error when the collection is empty, exceeds the
/// feed bound, contains no valid `YouTube` video identifiers, or a retained
/// episode has no valid publication date.
pub fn prepare_youtube_podcast_share(
    collection: ExtractedCollection,
    config: YouTubePrewarmConfig,
    skip_shorts: bool,
) -> io::Result<PreparedLocalShare> {
    prepare_youtube_podcast_share_with_boundary(collection, config, None, skip_shorts)
}

/// Builds a `YouTube` podcast feed beginning with one selected channel video.
///
/// The unified newest-first upload catalogue is reversed into chronological
/// order. The selected video is inclusive, and older uploads are omitted
/// before Youta's podcast-size bound is applied.
///
/// # Errors
///
/// The inclusive boundary is applied before the optional Shorts filter so a
/// selected Short can still delimit the retained chronology. Returns the
/// same errors as [`prepare_youtube_podcast_share`], and also rejects an invalid
/// or absent selected video identifier.
pub fn prepare_youtube_podcast_share_from(
    collection: ExtractedCollection,
    config: YouTubePrewarmConfig,
    first_video_id: &str,
    skip_shorts: bool,
) -> io::Result<PreparedLocalShare> {
    if validate_youtube_video_id(first_video_id).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the selected YouTube video ID is invalid",
        ));
    }
    prepare_youtube_podcast_share_with_boundary(
        collection,
        config,
        Some(first_video_id),
        skip_shorts,
    )
}

/// Returns retained input indices in oldest-first episode order.
///
/// Metadata lookups and manifest construction share this exact selection so
/// ignored older uploads and filtered Shorts need no publication-date lookup.
/// The selected Short remains an inclusive boundary even when Shorts are skipped.
/// The input is the unified newest-first uploads catalogue.
///
/// # Errors
///
/// Rejects an invalid or absent selected ID, an empty retained selection, or a
/// selection exceeding the podcast episode bound.
pub(crate) fn youtube_podcast_episode_indices(
    collection: &ExtractedCollection,
    first_video_id: Option<&str>,
    skip_shorts: bool,
) -> io::Result<Vec<usize>> {
    let end = if let Some(first_video_id) = first_video_id {
        if validate_youtube_video_id(first_video_id).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the selected YouTube video ID is invalid",
            ));
        }
        collection
            .entries
            .iter()
            .position(|entry| entry.id == first_video_id)
            .map(|index| index + 1)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "the selected video is absent from the enumerated YouTube channel",
                )
            })?
    } else {
        collection.entries.len()
    };
    let mut indices = Vec::new();
    for index in (0..end).rev() {
        let entry = &collection.entries[index];
        if validate_youtube_video_id(&entry.id).is_err()
            || (skip_shorts && youtube_collection_entry_is_short(entry))
        {
            continue;
        }
        if indices.len() >= MAX_SHARED_FILES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "YouTube channel exceeds Youta's podcast episode limit",
            ));
        }
        indices.push(index);
    }
    if indices.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "yt-dlp found no valid YouTube videos for the podcast feed",
        ));
    }
    Ok(indices)
}

/// Orders the unified catalogue before applying the selected boundary and filter.
fn prepare_youtube_podcast_share_with_boundary(
    mut collection: ExtractedCollection,
    mut config: YouTubePrewarmConfig,
    first_video_id: Option<&str>,
    skip_shorts: bool,
) -> io::Result<PreparedLocalShare> {
    let retained_indices =
        youtube_podcast_episode_indices(&collection, first_video_id, skip_shorts)?
            .into_iter()
            .collect::<HashSet<_>>();
    let title = if collection.title.trim().is_empty() {
        "YouTube channel".to_owned()
    } else {
        std::mem::take(&mut collection.title)
    };
    let mut files = Vec::new();
    let mut artwork = Vec::new();
    let mut feed_artwork_route = collection.thumbnail_url.take().map(|thumbnail_url| {
        let route = "/artwork/0".to_owned();
        artwork.push(SharedArtwork {
            mime: "image/jpeg",
            source: SharedArtworkSource::YouTube {
                media_index: 0,
                initial_url: Some(thumbnail_url),
            },
        });
        route
    });
    for (entry_index, entry) in collection.entries.into_iter().enumerate().rev() {
        if !retained_indices.contains(&entry_index) {
            continue;
        }
        let published_at = entry
            .published_at
            .and_then(rss_publication_date)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("YouTube episode {} has no valid publication date", entry.id),
                )
            })?;
        let source_url = Url::parse(&format!("https://www.youtube.com/watch?v={}", entry.id))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let index = files.len();
        let label = if entry.title.trim().is_empty() {
            entry.id.clone()
        } else {
            entry.title
        };
        let route = format!(
            "/media/{index}/{}.webm",
            utf8_percent_encode(&entry.id, NON_ALPHANUMERIC)
        );
        let artwork_route = format!("/artwork/{}", artwork.len());
        files.push(SharedFile {
            guid: format!("urn:youta:youtube:{}", entry.id),
            label,
            published_at: Some(published_at),
            length: 0,
            mime: YOUTUBE_PODCAST_MIME,
            source: SharedFileSource::YouTube {
                source_url,
                duration_seconds: entry.duration_seconds,
            },
            route,
            artwork_route: Some(artwork_route.clone()),
        });
        let thumbnail_url = entry.thumbnail_url.or_else(|| {
            Url::parse(&format!(
                "https://i.ytimg.com/vi/{}/hqdefault.jpg",
                entry.id
            ))
            .ok()
        });
        artwork.push(SharedArtwork {
            mime: "image/jpeg",
            source: SharedArtworkSource::YouTube {
                media_index: index,
                initial_url: thumbnail_url,
            },
        });
    }
    if feed_artwork_route.is_none() {
        feed_artwork_route = files.iter().find_map(|file| file.artwork_route.clone());
    }
    // A podcast enclosure must advertise a stable media type before the
    // signed stream exists. Prefer cookie-free embedded extraction and an
    // audio-only Opus/WebM representation rather than importing credentials.
    YOUTUBE_PODCAST_AUDIO_FORMAT.clone_into(&mut config.audio_format);
    config.player_client_policy = YouTubePlayerClientPolicy::EmbeddedThenDefault;
    Ok(PreparedLocalShare {
        kind: LanShareKind::Podcast,
        title,
        feed_artwork_route,
        files,
        artwork,
        remote_config: Some(config),
    })
}

/// Identifies a Short by yt-dlp's canonical entry URL rather than heuristics.
fn youtube_collection_entry_is_short(entry: &crate::playback::ytdlp::CollectionEntry) -> bool {
    entry.webpage_url.as_ref().is_some_and(|url| {
        url.domain()
            .is_some_and(|domain| domain == "youtube.com" || domain.ends_with(".youtube.com"))
            && url.path_segments().and_then(|mut segments| segments.next()) == Some("shorts")
    })
}

fn collect_files(
    root: &Path,
    directory: &Path,
    podcast: bool,
    depth: usize,
    files: &mut Vec<(PathBuf, String)>,
) -> io::Result<()> {
    if depth > MAX_SCAN_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shared directory exceeds Youta's recursion limit",
        ));
    }
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name().to_string_lossy().to_lowercase());
    for entry in entries {
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_files(root, &entry.path(), podcast, depth.saturating_add(1), files)?;
        } else if metadata.is_file() && (!podcast || is_playable(&entry.path())) {
            if files.len() >= MAX_SHARED_FILES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "shared directory exceeds Youta's file limit",
                ));
            }
            let entry_path = entry.path();
            let label = entry_path
                .strip_prefix(root)
                .unwrap_or(entry_path.as_path())
                .to_string_lossy()
                .into_owned();
            files.push((fs::canonicalize(entry_path)?, label));
        }
    }
    Ok(())
}

fn is_playable(path: &Path) -> bool {
    classify_local_file(path).is_some_and(LocalEntryKind::is_playable)
}

/// Converts real filesystem modification times without fabricating a fallback date.
fn local_podcast_date(modified: SystemTime) -> io::Result<DateTime<Utc>> {
    let seconds = match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).ok(),
        Err(error) => {
            let duration = error.duration();
            i64::try_from(duration.as_secs())
                .ok()
                .and_then(i64::checked_neg)
                .and_then(|seconds| seconds.checked_sub(i64::from(duration.subsec_nanos() != 0)))
        }
    };
    seconds.and_then(rss_publication_date).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "local audio modification date cannot be represented as an RSS publication date",
        )
    })
}

/// Checks the RFC 2822 date domain before formatting can overflow or panic.
fn rss_publication_date(seconds: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(seconds, 0).filter(|date| (1900..=9999).contains(&date.year()))
}

#[derive(Debug)]
struct SharedFile {
    guid: String,
    label: String,
    /// Required by podcast manifests; optional only for plain file sharing.
    published_at: Option<DateTime<Utc>>,
    length: u64,
    mime: &'static str,
    source: SharedFileSource,
    route: String,
    artwork_route: Option<String>,
}

#[derive(Debug)]
enum SharedFileSource {
    Local(PathBuf),
    YouTube {
        source_url: Url,
        duration_seconds: Option<u64>,
    },
}

#[derive(Debug)]
struct SharedArtwork {
    mime: &'static str,
    source: SharedArtworkSource,
}

#[derive(Debug)]
enum SharedArtworkSource {
    Local(PathBuf),
    YouTube {
        media_index: usize,
        initial_url: Option<Url>,
    },
}

struct ServerState {
    kind: LanShareKind,
    title: String,
    feed_artwork_route: Option<String>,
    base_url: String,
    files: Vec<SharedFile>,
    artwork: Vec<SharedArtwork>,
    remote: Option<RemoteRuntime>,
}

struct RemoteRuntime {
    resolver: YouTubePrewarmResolver,
    cancellation: YouTubePrewarmCancellation,
    cache: Mutex<HashMap<usize, CachedYouTubeResolution>>,
    agent: ureq::Agent,
}

#[derive(Clone)]
struct CachedYouTubeResolution {
    resolved_at: Instant,
    audio: PrewarmedYouTubeAudio,
}

impl ServerState {
    fn new(prepared: PreparedLocalShare, base_url: String) -> Self {
        let remote = prepared.remote_config.map(|config| RemoteRuntime {
            resolver: YouTubePrewarmResolver::new(config),
            cancellation: YouTubePrewarmCancellation::new(),
            cache: Mutex::new(HashMap::new()),
            agent: remote_agent(),
        });
        Self {
            kind: prepared.kind,
            title: prepared.title,
            feed_artwork_route: prepared.feed_artwork_route,
            base_url,
            files: prepared.files,
            artwork: prepared.artwork,
            remote,
        }
    }

    fn rss(&self) -> String {
        let title = escape_xml(&self.title);
        let channel_link = escape_xml(&format!("{}/", self.base_url));
        let channel_artwork = self.feed_artwork_route.as_ref().map_or_else(
            String::new,
            |route| {
                let artwork_url = format!("{}{}", self.base_url, escape_xml(route));
                format!(
                    "\n<itunes:image href=\"{artwork_url}\"/>\n<image>\n<url>{artwork_url}</url>\n<title>{title}</title>\n<link>{channel_link}</link>\n</image>"
                )
            },
        );
        let channel_description = if self.remote.is_some() {
            "YouTube audio resolved and shared by Youta while the application is running."
        } else {
            "Local audio shared by Youta while the application is running."
        };
        let items = self
			.files
			.iter()
			.map(|file| {
				let episode_artwork = file.artwork_route.as_ref().map_or_else(String::new, |route| {
					format!(
						"\n<itunes:image href=\"{}{}\"/>",
						self.base_url,
						escape_xml(route)
					)
				});
				let (description, duration) = match &file.source {
					SharedFileSource::Local(_) => (
						format!("Local audio shared by Youta: {}", file.label),
						String::new(),
					),
					SharedFileSource::YouTube {
						duration_seconds,
						..
					} => (
						format!("YouTube audio shared by Youta: {}", file.label),
						duration_seconds.map_or_else(String::new, |seconds| {
							format!("\n<itunes:duration>{seconds}</itunes:duration>")
						}),
					),
				};
				// Podcast preparation validates dates: local modification time or
				// the YouTube upload's original publication time, always in UTC.
				let publication_date = file.published_at
					.expect("podcast manifests contain a validated episode date")
					.to_rfc2822();
				format!(
					"<item>\n<title>{}</title>\n<description>{}</description>\n<pubDate>{publication_date}</pubDate>\n<guid isPermaLink=\"false\">{}</guid>\n<enclosure url=\"{}{}\" length=\"{}\" type=\"{}\"/>{episode_artwork}{duration}\n</item>",
					escape_xml(&file.label),
					escape_xml(&description),
					file.guid,
					self.base_url,
					escape_xml(&file.route),
					file.length,
					file.mime,
				)
			})
			.collect::<Vec<_>>()
			.join("\n");
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rss version=\"2.0\" xmlns:itunes=\"http://www.itunes.com/dtds/podcast-1.0.dtd\">\n<channel>\n<title>{title}</title>\n<link>{channel_link}</link>\n<description>{channel_description}</description>\n<language>und</language>{channel_artwork}\n{items}\n</channel>\n</rss>\n",
        )
    }

    fn index_html(&self) -> String {
        let items = self
            .files
            .iter()
            .map(|file| {
                format!(
                    "<li><a href=\"{}\">{}</a> ({})</li>",
                    escape_html(&file.route),
                    escape_html(&file.label),
                    file.length,
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{}</title><h1>{}</h1><ul>{items}</ul></html>",
            escape_html(&self.title),
            escape_html(&self.title),
        )
    }
}

/// Independent transfer budgets prevent thumbnail bursts from rejecting audio.
struct RequestAdmission {
    media: AdmissionPool,
    artwork: AdmissionPool,
}

impl RequestAdmission {
    fn new() -> Self {
        Self {
            media: AdmissionPool::new(MAX_MEDIA_CONNECTIONS, MAX_QUEUED_MEDIA_CONNECTIONS),
            artwork: AdmissionPool::new(MAX_ARTWORK_CONNECTIONS, MAX_QUEUED_ARTWORK_CONNECTIONS),
        }
    }
}

/// A bounded FIFO with cancellable waits and RAII release of active transfers.
struct AdmissionPool {
    active_limit: usize,
    queue_limit: usize,
    state: Mutex<AdmissionPoolState>,
    changed: Condvar,
}

#[derive(Default)]
struct AdmissionPoolState {
    active: usize,
    next_ticket: u64,
    waiting: VecDeque<u64>,
}

impl AdmissionPool {
    fn new(active_limit: usize, queue_limit: usize) -> Self {
        Self {
            active_limit,
            queue_limit,
            state: Mutex::new(AdmissionPoolState::default()),
            changed: Condvar::new(),
        }
    }

    /// Returns `None` only when the bounded queue is full or its wait expires.
    fn acquire(&self, stop: &AtomicBool) -> io::Result<Option<AdmissionPermit<'_>>> {
        self.acquire_with_timeout(stop, REQUEST_ADMISSION_TIMEOUT)
    }

    /// Keeps the deadline policy injectable for deterministic short queue tests.
    fn acquire_with_timeout(
        &self,
        stop: &AtomicBool,
        timeout: Duration,
    ) -> io::Result<Option<AdmissionPermit<'_>>> {
        let deadline = Instant::now() + timeout;
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("LAN admission is unavailable"))?;
        if stop.load(Ordering::Acquire) {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        if state.active < self.active_limit && state.waiting.is_empty() {
            state.active += 1;
            return Ok(Some(AdmissionPermit { pool: self }));
        }
        if state.waiting.len() >= self.queue_limit {
            return Ok(None);
        }
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        state.waiting.push_back(ticket);
        loop {
            let cancelled = stop.load(Ordering::Acquire);
            let now = Instant::now();
            if cancelled || now >= deadline {
                state.waiting.retain(|queued| *queued != ticket);
                self.changed.notify_all();
                return if cancelled {
                    Err(io::Error::from(io::ErrorKind::Interrupted))
                } else {
                    Ok(None)
                };
            }
            if state.active < self.active_limit && state.waiting.front() == Some(&ticket) {
                state.waiting.pop_front();
                state.active += 1;
                self.changed.notify_all();
                return Ok(Some(AdmissionPermit { pool: self }));
            }
            let (next_state, _) = self
                .changed
                .wait_timeout(state, IO_POLL.min(deadline.saturating_duration_since(now)))
                .map_err(|_| io::Error::other("LAN admission is unavailable"))?;
            state = next_state;
        }
    }
}

/// Holds a transfer slot through resolution, headers, and the complete body.
struct AdmissionPermit<'a> {
    pool: &'a AdmissionPool,
}

impl Drop for AdmissionPermit<'_> {
    fn drop(&mut self) {
        let mut state = self
            .pool
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active -= 1;
        self.pool.changed.notify_all();
    }
}

/// Keeps accepting sockets while bounding both media and overload workers.
fn serve(listener: TcpListener, state: Arc<ServerState>, stop: Arc<AtomicBool>) {
    let mut connections = Vec::<JoinHandle<()>>::new();
    let mut overload_connections = Vec::<JoinHandle<()>>::new();
    let admission = Arc::new(RequestAdmission::new());
    let mut connection_id = 0_u64;
    while !stop.load(Ordering::Acquire) {
        reap_finished_connections(&mut connections);
        reap_finished_connections(&mut overload_connections);
        match listener.accept() {
            Ok((stream, _)) => {
                let overloaded = connections.len() >= MAX_CONCURRENT_CONNECTIONS;
                if overloaded && overload_connections.len() >= MAX_OVERLOAD_CONNECTIONS {
                    // Refuse excess load without accumulating sockets, threads,
                    // or an application queue behind stalled media transfers.
                    drop(stream);
                    continue;
                }
                let connection_state = Arc::clone(&state);
                let connection_stop = Arc::clone(&stop);
                let connection_admission = Arc::clone(&admission);
                let worker_kind = if overloaded { "overload" } else { "connection" };
                let thread = thread::Builder::new()
                    .name(format!("youta-lan-{worker_kind}-{connection_id}"))
                    .spawn(move || {
                        if overloaded {
                            let _ = handle_overloaded_connection(
                                stream,
                                &connection_state,
                                &connection_stop,
                            );
                        } else {
                            let _ = handle_connection_with_admission(
                                stream,
                                &connection_state,
                                &connection_stop,
                                Some(&connection_admission),
                            );
                        }
                    });
                connection_id = connection_id.wrapping_add(1);
                if let Ok(thread) = thread {
                    if overloaded {
                        overload_connections.push(thread);
                    } else {
                        connections.push(thread);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(IO_POLL);
            }
            Err(_) => break,
        }
    }
    for connection in connections.into_iter().chain(overload_connections) {
        let _ = connection.join();
    }
}

/// Reaps completed request workers so one slow media client cannot block feed
/// and artwork requests while the server retains a fixed concurrency bound.
fn reap_finished_connections(connections: &mut Vec<JoinHandle<()>>) {
    let mut index = 0;
    while index < connections.len() {
        if connections[index].is_finished() {
            let connection = connections.swap_remove(index);
            let _ = connection.join();
        } else {
            index += 1;
        }
    }
}

/// Uses blocking worker I/O with short polls independently of listener mode.
///
/// Winsock can preserve the listener's nonblocking mode on accepted sockets.
/// Socket timeouts alone do not clear that mode, so reads would otherwise spin
/// on `WouldBlock` instead of waiting for data or the next cancellation poll.
fn configure_http_stream(stream: &TcpStream) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_POLL))?;
    stream.set_write_timeout(Some(IO_POLL))
}

/// Keeps podcast feeds available while rejecting excess audio downloads.
///
/// Reading the headers avoids closing over unread GET/HEAD request bytes, which
/// can turn a useful HTTP response into a reset. Slow or oversized requests are
/// closed within the short header deadline; they never consume a media worker.
/// Only the immutable podcast feed uses these reserved workers for content,
/// with its own normal response deadline after the short header-drain deadline.
fn handle_overloaded_connection(
    mut stream: TcpStream,
    state: &ServerState,
    stop: &AtomicBool,
) -> io::Result<()> {
    configure_http_stream(&stream)?;
    let deadline = Instant::now() + OVERLOAD_RESPONSE_TIMEOUT;
    let mut reader = BufReader::new(stream.try_clone()?);
    let request_line = read_request_line(&mut reader, MAX_REQUEST_LINE_BYTES + 1, deadline, stop)?;
    if request_line.len() > MAX_REQUEST_LINE_BYTES || !request_line.ends_with('\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid overload request line",
        ));
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next();
    let raw_path = parts.next();
    let version = parts.next();
    let well_formed = matches!(version, Some("HTTP/1.0" | "HTTP/1.1")) && parts.next().is_none();
    let head = method == Some("HEAD");
    let mut header_bytes = 0_usize;
    loop {
        let remaining = MAX_REQUEST_HEADER_BYTES.saturating_sub(header_bytes);
        let line = read_request_line(&mut reader, remaining.saturating_add(1), deadline, stop)?;
        header_bytes = header_bytes.saturating_add(line.len());
        if header_bytes > MAX_REQUEST_HEADER_BYTES || !line.ends_with('\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid overload request headers",
            ));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    let feed_requested = raw_path.and_then(|path| path.split('?').next()) == Some("/feed.xml");
    if state.kind == LanShareKind::Podcast
        && well_formed
        && feed_requested
        && (method == Some("GET") || head)
    {
        return write_content_response(
            &mut stream,
            "application/rss+xml; charset=utf-8",
            state.rss().as_bytes(),
            head,
            stop,
        );
    }
    write_busy_response(&mut stream, head, stop)
}

/// Frames a bounded retry response only before any successful response begins.
fn write_busy_response(stream: &mut TcpStream, head: bool, stop: &AtomicBool) -> io::Result<()> {
    let deadline = Instant::now() + OVERLOAD_RESPONSE_TIMEOUT;
    let body = "Youta is busy; retry shortly";
    let headers = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nRetry-After: 1\r\nConnection: close\r\n\r\n",
        body.len(),
    );
    write_response_bytes(stream, headers.as_bytes(), stop, deadline)?;
    if !head {
        write_response_bytes(stream, body.as_bytes(), stop, deadline)?;
    }
    Ok(())
}

/// Reads one bounded HTTP line without losing fragments across socket timeouts.
///
/// The caller shares one absolute deadline across all request headers, so a
/// slow client cannot retain a connection indefinitely by sending small pieces.
/// Bytes remain buffered locally until a newline, EOF, or the size limit; UTF-8
/// sequences may therefore cross socket reads without being mistaken for errors.
fn read_request_line(
    reader: &mut impl BufRead,
    limit: usize,
    deadline: Instant,
    stop: &AtomicBool,
) -> io::Result<String> {
    let mut bytes = Vec::new();
    while bytes.len() < limit {
        check_http_transfer_active(stop, deadline)?;
        let consumed = match reader.fill_buf() {
            Ok([]) => break,
            Ok(buffer) => {
                let line_end = buffer
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(buffer.len(), |index| index + 1);
                let consumed = line_end.min(limit - bytes.len());
                bytes.extend_from_slice(&buffer[..consumed]);
                consumed
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        reader.consume(consumed);
        if bytes.last() == Some(&b'\n') {
            break;
        }
    }
    String::from_utf8(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "HTTP request line is not valid UTF-8",
        )
    })
}

/// Exercises request routing independently of the listener's admission policy.
#[cfg(test)]
fn handle_connection(stream: TcpStream, state: &ServerState, stop: &AtomicBool) -> io::Result<()> {
    handle_connection_with_admission(stream, state, stop, None)
}

/// Parses bounded headers before acquiring the matching transfer-class permit.
fn handle_connection_with_admission(
    mut stream: TcpStream,
    state: &ServerState,
    stop: &AtomicBool,
    admission: Option<&RequestAdmission>,
) -> io::Result<()> {
    configure_http_stream(&stream)?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let deadline = Instant::now() + REQUEST_HEADER_TIMEOUT;
    let request_line = read_request_line(&mut reader, MAX_REQUEST_LINE_BYTES + 1, deadline, stop)?;
    if request_line.len() > MAX_REQUEST_LINE_BYTES {
        return write_text_response(
            &mut stream,
            431,
            "Request Header Fields Too Large",
            "HTTP request line exceeded Youta's limit",
            false,
        );
    }
    let mut parts = request_line.split_whitespace();
    let Some(method) = parts.next() else {
        return write_text_response(
            &mut stream,
            400,
            "Bad Request",
            "Malformed HTTP request",
            false,
        );
    };
    let Some(raw_path) = parts.next() else {
        return write_text_response(
            &mut stream,
            400,
            "Bad Request",
            "Malformed HTTP request",
            false,
        );
    };
    let head = method == "HEAD";
    if method != "GET" && !head {
        return write_text_response(
            &mut stream,
            405,
            "Method Not Allowed",
            "Only GET and HEAD are supported",
            head,
        );
    }
    let mut range_header = None;
    let mut header_bytes = 0_usize;
    loop {
        let remaining = MAX_REQUEST_HEADER_BYTES.saturating_sub(header_bytes);
        let line = read_request_line(&mut reader, remaining.saturating_add(1), deadline, stop)?;
        header_bytes = header_bytes.saturating_add(line.len());
        if header_bytes > MAX_REQUEST_HEADER_BYTES {
            return write_text_response(
                &mut stream,
                431,
                "Request Header Fields Too Large",
                "HTTP request headers exceeded Youta's limit",
                head,
            );
        }
        if line.is_empty() || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("Range")
        {
            range_header = Some(value.trim().to_owned());
        }
    }
    let path = raw_path.split('?').next().unwrap_or(raw_path);
    if path == "/" && state.kind == LanShareKind::Files {
        return write_content_response(
            &mut stream,
            "text/html; charset=utf-8",
            state.index_html().as_bytes(),
            head,
            stop,
        );
    }
    if path == "/feed.xml" && state.kind == LanShareKind::Podcast {
        return write_content_response(
            &mut stream,
            "application/rss+xml; charset=utf-8",
            state.rss().as_bytes(),
            head,
            stop,
        );
    }
    if let Some(index) = route_index(path, "/media/")
        && let Some(file) = state.files.get(index)
    {
        let _permit = if let Some(admission) = admission {
            match admission.media.acquire(stop)? {
                Some(permit) => Some(permit),
                None => return write_busy_response(&mut stream, head, stop),
            }
        } else {
            None
        };
        return match &file.source {
            SharedFileSource::Local(path) => write_file_response(
                &mut stream,
                file.mime,
                path,
                file.length,
                range_header.as_deref(),
                head,
                stop,
            ),
            SharedFileSource::YouTube { source_url, .. } => proxy_youtube_audio(
                &mut stream,
                state,
                index,
                source_url,
                range_header.as_deref(),
                head,
                stop,
            ),
        };
    }
    if let Some(index) = route_index(path, "/artwork/")
        && let Some(artwork) = state.artwork.get(index)
    {
        let _permit = if let Some(admission) = admission {
            match admission.artwork.acquire(stop)? {
                Some(permit) => Some(permit),
                None => return write_busy_response(&mut stream, head, stop),
            }
        } else {
            None
        };
        return match &artwork.source {
            SharedArtworkSource::Local(path) => {
                let length = fs::metadata(path)?.len();
                write_file_response(
                    &mut stream,
                    artwork.mime,
                    path,
                    length,
                    range_header.as_deref(),
                    head,
                    stop,
                )
            }
            SharedArtworkSource::YouTube {
                media_index,
                initial_url,
            } => proxy_youtube_artwork(
                &mut stream,
                state,
                *media_index,
                initial_url.as_ref(),
                head,
                stop,
            ),
        };
    }
    write_text_response(&mut stream, 404, "Not Found", "Not found", head)
}

fn route_index(path: &str, prefix: &str) -> Option<usize> {
    path.strip_prefix(prefix)?
        .split('/')
        .next()?
        .parse::<usize>()
        .ok()
}

fn proxy_youtube_audio(
    stream: &mut TcpStream,
    state: &ServerState,
    index: usize,
    source_url: &Url,
    range: Option<&str>,
    head: bool,
    stop: &AtomicBool,
) -> io::Result<()> {
    let Some(remote) = state.remote.as_ref() else {
        return write_text_response(stream, 502, "Bad Gateway", "Remote proxy unavailable", head);
    };
    let mut resolved = match state.resolve_youtube_audio(index, source_url, false) {
        Ok(resolved) => resolved,
        Err(_) => {
            return write_text_response(
                stream,
                502,
                "Bad Gateway",
                "Could not resolve cookie-free YouTube audio; embedded playback may be unavailable",
                head,
            );
        }
    };
    let chunk_plan = YouTubeChunkPlan::for_range(range);
    let initial_chunk_range = chunk_plan.map(|plan| plan.initial_range(YOUTUBE_REMOTE_CHUNK_SIZE));
    let upstream_range = initial_chunk_range.as_deref().or(range);
    let response = request_with_resolution_refresh(|refresh| {
        ensure_remote_request_active(stop, &remote.cancellation)?;
        if refresh {
            resolved = state
                .resolve_youtube_audio(index, source_url, true)
                .map_err(|_| RemoteRequestFailure::Resolution)?;
        }
        let headers = resolved.http_headers().iter().collect::<Vec<_>>();
        let result = request_initial_remote_response(
            remote,
            resolved.media_url(),
            &headers,
            upstream_range,
            stop,
        );
        if matches!(
            result,
            Err(RemoteRequestFailure::HttpStatus(403 | 410)
                | RemoteRequestFailure::DeferredHttpStatus(403 | 410))
        ) {
            // Evict even a rejected replacement, but never discard a newer
            // resolution installed by another request while this one ran.
            state
                .invalidate_youtube_resolution(index, resolved.media_url())
                .map_err(|_| RemoteRequestFailure::Resolution)?;
        }
        result
    });
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return write_text_response(stream, 502, "Bad Gateway", &error.message(), head);
        }
    };
    let headers = resolved.http_headers().iter().collect::<Vec<_>>();
    if let Some(plan) = chunk_plan {
        return write_chunked_youtube_response(
            stream,
            remote,
            resolved.media_url(),
            &headers,
            response,
            head,
            stop,
            plan,
            YOUTUBE_REMOTE_CHUNK_SIZE,
        );
    }
    write_remote_response(
        stream,
        remote,
        resolved.media_url(),
        &headers,
        response,
        head,
        stop,
        YOUTUBE_PODCAST_MIME,
    )
}

fn proxy_youtube_artwork(
    stream: &mut TcpStream,
    state: &ServerState,
    _media_index: usize,
    initial_url: Option<&Url>,
    head: bool,
    stop: &AtomicBool,
) -> io::Result<()> {
    let Some(url) = initial_url else {
        return write_text_response(stream, 404, "Not Found", "Artwork unavailable", head);
    };
    proxy_remote_response(stream, state, url, &[], None, head, stop, "image/jpeg")
}

impl ServerState {
    /// Drops only the rejected URL, preserving a newer concurrent replacement.
    fn invalidate_youtube_resolution(&self, index: usize, rejected_url: &Url) -> io::Result<()> {
        let remote = self.remote.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "YouTube resolver is unavailable")
        })?;
        let mut cache = remote
            .cache
            .lock()
            .map_err(|_| io::Error::other("YouTube resolver cache is unavailable"))?;
        if cache
            .get(&index)
            .is_some_and(|cached| cached.audio.media_url() == rejected_url)
        {
            cache.remove(&index);
        }
        Ok(())
    }

    /// Uses ordinary cached resolutions until a rejected URL needs yt-dlp's
    /// format availability check. Checked refreshes bypass unverified cache
    /// entries, including those installed by another in-flight request.
    fn resolve_youtube_audio(
        &self,
        index: usize,
        source_url: &Url,
        check_formats: bool,
    ) -> io::Result<PrewarmedYouTubeAudio> {
        let remote = self.remote.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "YouTube resolver is unavailable")
        })?;
        if !check_formats
            && let Some(cached) = remote
                .cache
                .lock()
                .map_err(|_| io::Error::other("YouTube resolver cache is unavailable"))?
                .get(&index)
                .filter(|cached| cached.is_fresh())
                .cloned()
        {
            return Ok(cached.audio);
        }
        let generation = u64::try_from(index).unwrap_or(u64::MAX);
        let request = YouTubePrewarmRequest::new(generation, source_url.clone());
        let result = if check_formats {
            remote
                .resolver
                .resolve_checked(request, &remote.cancellation)
        } else {
            remote.resolver.resolve(request, &remote.cancellation)
        };
        let audio = result.into_outcome().map_err(|_| {
            io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "YouTube audio resolution failed",
            )
        })?;
        remote
            .cache
            .lock()
            .map_err(|_| io::Error::other("YouTube resolver cache is unavailable"))?
            .insert(
                index,
                CachedYouTubeResolution {
                    resolved_at: Instant::now(),
                    audio: audio.clone(),
                },
            );
        Ok(audio)
    }
}

impl CachedYouTubeResolution {
    fn is_fresh(&self) -> bool {
        if self.resolved_at.elapsed() >= REMOTE_RESOLUTION_CACHE_TTL {
            return false;
        }
        self.audio
            .expires_at_unix()
            .is_none_or(|expires_at| expires_at > unix_seconds().saturating_add(60))
    }
}

fn proxy_remote_response(
    stream: &mut TcpStream,
    state: &ServerState,
    url: &Url,
    headers: &[(&str, &str)],
    range: Option<&str>,
    head: bool,
    stop: &AtomicBool,
    fallback_content_type: &str,
) -> io::Result<()> {
    let Some(remote) = state.remote.as_ref() else {
        return write_text_response(stream, 502, "Bad Gateway", "Remote proxy unavailable", head);
    };
    let response = match request_initial_remote_response(remote, url, headers, range, stop) {
        Ok(response) => response,
        Err(error) => {
            return write_text_response(stream, 502, "Bad Gateway", &error.message(), head);
        }
    };
    write_remote_response(
        stream,
        remote,
        url,
        headers,
        response,
        head,
        stop,
        fallback_content_type,
    )
}

/// Writes only an accepted upstream response, after any setup retries complete.
#[allow(clippy::too_many_arguments)]
fn write_remote_response(
    stream: &mut TcpStream,
    remote: &RemoteRuntime,
    url: &Url,
    headers: &[(&str, &str)],
    response: ureq::http::Response<ureq::Body>,
    head: bool,
    stop: &AtomicBool,
    fallback_content_type: &str,
) -> io::Result<()> {
    let status = response.status().as_u16();
    let reason = if status == 206 {
        "Partial Content"
    } else {
        "OK"
    };
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or(fallback_content_type)
        .to_owned();
    let content_range = response
        .headers()
        .get("content-range")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let accept_ranges = response
        .headers()
        .get("accept-ranges")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("bytes")
        .to_owned();
    let content_length = response.body().content_length();
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nAccept-Ranges: {accept_ranges}\r\n"
    )?;
    if let Some(content_length) = content_length {
        write!(stream, "Content-Length: {content_length}\r\n")?;
    }
    if let Some(content_range) = &content_range {
        write!(stream, "Content-Range: {content_range}\r\n")?;
    }
    write!(stream, "Connection: close\r\n\r\n")?;
    if head {
        return Ok(());
    }
    proxy_remote_body(
        stream,
        remote,
        url,
        headers,
        response,
        status,
        content_range.as_deref(),
        content_length,
        None,
        stop,
    )
}

/// Client requests whose full remaining entity can use bounded upstream chunks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum YouTubeChunkPlan {
    /// A fresh download without a client Range header remains HTTP 200.
    Full,
    /// An open-ended client Range remains HTTP 206 from its original offset.
    From(u64),
}

impl YouTubeChunkPlan {
    /// Leaves bounded, suffix, multiple, and malformed ranges on the old path.
    fn for_range(range: Option<&str>) -> Option<Self> {
        let Some(range) = range else {
            return Some(Self::Full);
        };
        let start = range.strip_prefix("bytes=")?.strip_suffix('-')?;
        if start.is_empty() || !start.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        start.parse().ok().map(Self::From)
    }

    /// Returns the first absolute byte requested by the downstream client.
    fn start(self) -> u64 {
        match self {
            Self::Full => 0,
            Self::From(start) => start,
        }
    }

    /// Bounds the initial CDN request even for an open-ended client range.
    fn initial_range(self, chunk_size: u64) -> String {
        let start = self.start();
        format!(
            "bytes={start}-{}",
            start.saturating_add(chunk_size.saturating_sub(1))
        )
    }
}

/// Reassembles bounded upstream ranges into a full download or client suffix.
#[allow(clippy::too_many_arguments)]
fn write_chunked_youtube_response(
    stream: &mut TcpStream,
    remote: &RemoteRuntime,
    url: &Url,
    headers: &[(&str, &str)],
    response: ureq::http::Response<ureq::Body>,
    head: bool,
    stop: &AtomicBool,
    plan: YouTubeChunkPlan,
    chunk_size: u64,
) -> io::Result<()> {
    if response.status().as_u16() == 200 {
        return write_remote_response(
            stream,
            remote,
            url,
            headers,
            response,
            head,
            stop,
            YOUTUBE_PODCAST_MIME,
        );
    }
    let Some(mut bounds) = remote_chunk_bounds(&response).filter(|bounds| {
        chunk_size > 0
            && bounds.start == plan.start()
            && bounds.end
                == plan
                    .start()
                    .saturating_add(chunk_size - 1)
                    .min(bounds.total - 1)
    }) else {
        return write_text_response(
            stream,
            502,
            "Bad Gateway",
            "Remote media request failed: invalid upstream byte range",
            head,
        );
    };
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or(YOUTUBE_PODCAST_MIME);
    let (status, content_range) = match plan {
        YouTubeChunkPlan::Full => ("200 OK", String::new()),
        YouTubeChunkPlan::From(start) => (
            "206 Partial Content",
            format!(
                "Content-Range: bytes {start}-{}/{}\r\n",
                bounds.total - 1,
                bounds.total,
            ),
        ),
    };
    let downstream_headers = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n{content_range}Connection: close\r\n\r\n",
        bounds.total - plan.start()
    );
    write_media_bytes(stream, downstream_headers.as_bytes(), stop)?;
    if head {
        return Ok(());
    }
    let mut response = response;
    loop {
        ensure_remote_request_active(stop, &remote.cancellation)
            .map_err(|_| io::Error::new(io::ErrorKind::Interrupted, "LAN media share stopped"))?;
        let content_range = format!("bytes {}-{}/{}", bounds.start, bounds.end, bounds.total);
        // An interrupted chunk has its own bounded resume budget. A normal
        // chunk boundary is progress, not a failed whole-episode download.
        proxy_remote_body(
            stream,
            remote,
            url,
            headers,
            response,
            206,
            Some(&content_range),
            Some(bounds.end - bounds.start + 1),
            Some(bounds.total),
            stop,
        )?;
        if bounds.end == bounds.total - 1 {
            return Ok(());
        }
        ensure_remote_request_active(stop, &remote.cancellation)
            .map_err(|_| io::Error::new(io::ErrorKind::Interrupted, "LAN media share stopped"))?;
        let next_start = bounds.end + 1;
        let next_end = next_start
            .saturating_add(chunk_size - 1)
            .min(bounds.total - 1);
        let range = format!("bytes={next_start}-{next_end}");
        response = request_initial_remote_response(remote, url, headers, Some(&range), stop)
            .map_err(|error| io::Error::new(io::ErrorKind::ConnectionAborted, error.message()))?;
        let Some(next_bounds) = remote_chunk_bounds(&response).filter(|candidate| {
            candidate.start == next_start
                && candidate.end == next_end
                && candidate.total == bounds.total
        }) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "remote media chunk has an inconsistent byte range",
            ));
        };
        bounds = next_bounds;
    }
}

/// Exact framing of one partial response belonging to a known whole entity.
#[derive(Clone, Copy)]
struct RemoteChunkBounds {
    start: u64,
    end: u64,
    total: u64,
}

/// Requires a numeric whole size and a body length matching the inclusive span.
fn remote_chunk_bounds(response: &ureq::http::Response<ureq::Body>) -> Option<RemoteChunkBounds> {
    if response.status().as_u16() != 206 {
        return None;
    }
    let range = response.headers().get("content-range")?.to_str().ok()?;
    let (span, total) = range.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = span.split_once('-')?;
    let bounds = RemoteChunkBounds {
        start: start.parse().ok()?,
        end: end.parse().ok()?,
        total: total.parse().ok()?,
    };
    (bounds.start <= bounds.end
        && bounds.end < bounds.total
        && response.body().content_length() == Some(bounds.end - bounds.start + 1))
    .then_some(bounds)
}

/// Safe upstream error classes exclude signed URLs, headers, and remote text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteRequestFailure {
    HttpStatus(u16),
    DeferredHttpStatus(u16),
    Timeout,
    Transport,
    Protocol,
    Resolution,
    Cancelled,
}

impl RemoteRequestFailure {
    /// Retains only enough information to choose a retry and explain a failure.
    fn from_error(error: &ureq::Error) -> Self {
        match error {
            ureq::Error::StatusCode(status) => Self::HttpStatus(*status),
            ureq::Error::Timeout(_) => Self::Timeout,
            ureq::Error::Io(_) | ureq::Error::HostNotFound | ureq::Error::ConnectionFailed => {
                Self::Transport
            }
            _ => Self::Protocol,
        }
    }

    /// Rate limits and permanent client errors must not provoke repeat requests.
    fn is_transient(self) -> bool {
        matches!(
            self,
            Self::HttpStatus(500 | 502 | 503 | 504) | Self::Timeout | Self::Transport
        )
    }

    /// Returns a bounded local explanation, never an upstream error's Display.
    fn message(self) -> String {
        let detail = match self {
            Self::HttpStatus(status) | Self::DeferredHttpStatus(status) => {
                format!("upstream HTTP {status}")
            }
            Self::Timeout => "upstream timeout".to_owned(),
            Self::Transport => "upstream connection failure".to_owned(),
            Self::Protocol => "upstream protocol failure".to_owned(),
            Self::Resolution => "YouTube audio resolution failed".to_owned(),
            Self::Cancelled => "request cancelled".to_owned(),
        };
        format!("Remote media request failed: {detail}")
    }
}

/// Re-resolves one rejected signed URL once, never looping on permanent errors.
///
/// The callback evicts only its rejected cache value and checks cancellation.
/// Retry-After responses remain deferred even when their status is 403 or 410.
fn request_with_resolution_refresh<T>(
    mut request: impl FnMut(bool) -> Result<T, RemoteRequestFailure>,
) -> Result<T, RemoteRequestFailure> {
    match request(false) {
        Err(RemoteRequestFailure::HttpStatus(403 | 410)) => request(true),
        result => result,
    }
}

/// Runs at most two safe GET attempts, retrying only an early transient failure.
///
/// The five-second eligibility window is not a whole-request deadline: existing
/// first-attempt phase limits remain in force. A retry has shorter setup phases
/// without introducing an end-to-end deadline on a successful audio response.
/// HTTP 429 is never retried here, avoiding requests before Retry-After permits.
fn retry_initial_remote_request<T>(
    stop: &AtomicBool,
    cancellation: &YouTubePrewarmCancellation,
    mut request: impl FnMut(bool) -> Result<T, RemoteRequestFailure>,
) -> Result<T, RemoteRequestFailure> {
    let started = Instant::now();
    for retry in [false, true] {
        ensure_remote_request_active(stop, cancellation)?;
        let result = request(retry);
        ensure_remote_request_active(stop, cancellation)?;
        match result {
            Ok(response) => return Ok(response),
            Err(failure) => {
                if retry
                    || !failure.is_transient()
                    || started.elapsed().saturating_add(REMOTE_INITIAL_RETRY_DELAY)
                        >= REMOTE_INITIAL_RETRY_WINDOW
                {
                    return Err(failure);
                }
                let wake_at = Instant::now() + REMOTE_INITIAL_RETRY_DELAY;
                while Instant::now() < wake_at {
                    ensure_remote_request_active(stop, cancellation)?;
                    thread::sleep(IO_POLL.min(wake_at.saturating_duration_since(Instant::now())));
                }
                if started.elapsed() >= REMOTE_INITIAL_RETRY_WINDOW {
                    return Err(failure);
                }
            }
        }
    }
    unreachable!("the second attempt always returns")
}

/// Checks both LAN shutdown and supervised resolver cancellation before retries.
fn ensure_remote_request_active(
    stop: &AtomicBool,
    cancellation: &YouTubePrewarmCancellation,
) -> Result<(), RemoteRequestFailure> {
    if stop.load(Ordering::Acquire) || cancellation.is_cancelled() {
        Err(RemoteRequestFailure::Cancelled)
    } else {
        Ok(())
    }
}

/// Obtains response headers before sending any status or bytes downstream.
fn request_initial_remote_response(
    remote: &RemoteRuntime,
    url: &Url,
    headers: &[(&str, &str)],
    range: Option<&str>,
    stop: &AtomicBool,
) -> Result<ureq::http::Response<ureq::Body>, RemoteRequestFailure> {
    retry_initial_remote_request(stop, &remote.cancellation, |retry| {
        let response =
            request_remote_response_with_setup_policy(remote, url, headers, range, retry, true)
                .map_err(|error| RemoteRequestFailure::from_error(&error))?;
        let status = response.status().as_u16();
        if status >= 400 {
            // Defer nonzero or unrecognized Retry-After to the podcast client;
            // never hold a worker for a long server delay or hammer an upstream.
            let deferred = response
                .headers()
                .get("retry-after")
                .is_some_and(|value| value.to_str().ok().is_none_or(|value| value.trim() != "0"));
            return Err(if deferred {
                RemoteRequestFailure::DeferredHttpStatus(status)
            } else {
                RemoteRequestFailure::HttpStatus(status)
            });
        }
        Ok(response)
    })
}

/// Sends one upstream request while retaining only headers that yt-dlp may
/// require for the signed media URL.
fn request_remote_response(
    remote: &RemoteRuntime,
    url: &Url,
    headers: &[(&str, &str)],
    range: Option<&str>,
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    request_remote_response_with_setup_policy(remote, url, headers, range, false, false)
}

/// Shortens retry setup without changing the existing media body timeout.
fn request_remote_response_with_setup_policy(
    remote: &RemoteRuntime,
    url: &Url,
    headers: &[(&str, &str)],
    range: Option<&str>,
    retry: bool,
    inspect_error_status: bool,
) -> Result<ureq::http::Response<ureq::Body>, ureq::Error> {
    let mut request = remote
        .agent
        .get(url.as_str())
        .header("Accept-Encoding", "identity");
    for (name, value) in headers {
        if proxy_request_header_allowed(name) {
            request = request.header(*name, *value);
        }
    }
    if let Some(range) = range {
        request = request.header("Range", range);
    }
    if inspect_error_status {
        request = request.config().http_status_as_error(false).build();
    }
    if retry {
        request = request
            .config()
            .timeout_resolve(Some(REMOTE_RETRY_PHASE_TIMEOUT))
            .timeout_connect(Some(REMOTE_RETRY_PHASE_TIMEOUT))
            .timeout_send_request(Some(REMOTE_RETRY_PHASE_TIMEOUT))
            // ureq 3 also applies the response-header timeout to body reads.
            // The preceding send-request timer already bounds header receipt;
            // clearing this timer avoids turning a short setup retry into a
            // two-second deadline for an otherwise successful audio body.
            .timeout_recv_response(None)
            .build();
    }
    request.call()
}

/// Streams a remote body and resumes an interrupted response from its first
/// missing byte before the advertised downstream length can be truncated.
#[allow(clippy::too_many_arguments)]
fn proxy_remote_body(
    stream: &mut TcpStream,
    remote: &RemoteRuntime,
    url: &Url,
    headers: &[(&str, &str)],
    response: ureq::http::Response<ureq::Body>,
    initial_status: u16,
    initial_content_range: Option<&str>,
    expected_length: Option<u64>,
    expected_total: Option<u64>,
    stop: &AtomicBool,
) -> io::Result<()> {
    let absolute_start = if initial_status == 206 {
        initial_content_range
            .and_then(parse_content_range)
            .map(|(start, _)| start)
    } else {
        Some(0)
    };
    let absolute_end = absolute_start
        .zip(expected_length)
        .and_then(|(start, length)| {
            (length > 0).then(|| start.saturating_add(length.saturating_sub(1)))
        });
    let mut response = response;
    let mut forwarded = 0_u64;
    let mut resume_attempts = 0_usize;

    loop {
        let (_, body) = response.into_parts();
        let mut reader = body.into_reader();
        let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
        let mut read_error = None;
        loop {
            if stop.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "LAN media share stopped",
                ));
            }
            let wanted = expected_length.map_or(buffer.len(), |length| {
                usize::try_from(length.saturating_sub(forwarded).min(buffer.len() as u64))
                    .unwrap_or(buffer.len())
            });
            if wanted == 0 {
                return Ok(());
            }
            let read = match reader.read(&mut buffer[..wanted]) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    read_error = Some(error);
                    break;
                }
            };
            write_media_bytes(stream, &buffer[..read], stop)?;
            forwarded = forwarded.saturating_add(read as u64);
        }

        let Some(expected_length) = expected_length else {
            return read_error.map_or(Ok(()), |error| {
                Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    format!("remote media read failed: {error}"),
                ))
            });
        };
        if forwarded >= expected_length {
            return Ok(());
        }
        let Some((absolute_start, absolute_end)) = absolute_start.zip(absolute_end) else {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "remote partial response ended before its advertised length",
            ));
        };
        let resume_start = absolute_start.saturating_add(forwarded);
        let resume_range = format!("bytes={resume_start}-{absolute_end}");
        response = loop {
            if resume_attempts >= MAX_REMOTE_RESUME_ATTEMPTS {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "remote media ended after {forwarded} of {expected_length} advertised bytes"
                    ),
                ));
            }
            resume_attempts += 1;
            let Ok(candidate) =
                request_remote_response(remote, url, headers, Some(resume_range.as_str()))
            else {
                continue;
            };
            let starts_at_missing_byte = candidate.status().as_u16() == 206
                && candidate
                    .headers()
                    .get("content-range")
                    .and_then(|value| value.to_str().ok())
                    .and_then(parse_content_range)
                    .is_some_and(|(start, _)| start == resume_start);
            let matches_chunk = expected_total.is_none_or(|total| {
                remote_chunk_bounds(&candidate).is_some_and(|bounds| {
                    bounds.start == resume_start
                        && bounds.end == absolute_end
                        && bounds.total == total
                })
            });
            if starts_at_missing_byte && matches_chunk {
                break candidate;
            }
        };
    }
}

/// Parses the inclusive byte bounds from an HTTP `Content-Range` value.
fn parse_content_range(value: &str) -> Option<(u64, u64)> {
    let bounds = value.strip_prefix("bytes ")?.split_once('/')?.0;
    let (start, end) = bounds.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    (start <= end).then_some((start, end))
}

fn proxy_request_header_allowed(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "user-agent" | "referer" | "origin" | "accept" | "accept-language" | "cookie"
    )
}

/// Bounds remote setup and each stalled body read, not a whole audio download.
fn remote_agent() -> ureq::Agent {
    remote_agent_with_policy(REMOTE_SETUP_TIMEOUT, true)
}

/// Keeps production and scaled loopback tests on the same remote HTTP policy.
///
/// ureq also checks the preceding phase's deadline while reading a body. Its
/// response timer must remain unset: the send-request timer still bounds header
/// receipt, while the body timer limits each individual stalled read.
fn remote_agent_with_policy(phase_timeout: Duration, https_only: bool) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(None)
        .timeout_per_call(None)
        .timeout_resolve(Some(phase_timeout))
        .timeout_connect(Some(phase_timeout))
        .timeout_send_request(Some(phase_timeout))
        .timeout_recv_response(None)
        .timeout_recv_body(Some(phase_timeout))
        .https_only(https_only)
        .max_redirects(5)
        .user_agent(concat!(
            "youta/",
            env!("CARGO_PKG_VERSION"),
            " (+",
            env!("CARGO_PKG_REPOSITORY"),
            ")"
        ))
        .build()
        .into()
}

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// Sends a complete feed or directory page despite temporary client backpressure.
///
/// Headers and body share an absolute response deadline. Each partial write
/// advances the remaining slice before retrying, preserving Content-Length even
/// when a podcast app pauses its reads for longer than the socket poll interval.
fn write_content_response(
    stream: &mut TcpStream,
    content_type: &str,
    body: &[u8],
    head: bool,
    stop: &AtomicBool,
) -> io::Result<()> {
    let deadline = Instant::now() + CONTENT_RESPONSE_TIMEOUT;
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    );
    write_response_bytes(stream, headers.as_bytes(), stop, deadline)?;
    if !head {
        write_response_bytes(stream, body, stop, deadline)?;
    }
    Ok(())
}

/// Checks cancellation and the whole-operation deadline between socket polls.
fn check_http_transfer_active(stop: &AtomicBool, deadline: Instant) -> io::Result<()> {
    if stop.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "LAN share stopped",
        ));
    }
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "LAN HTTP transfer timed out",
        ));
    }
    Ok(())
}

/// Retries transient writes while accounting for progress and bounding total time.
fn write_response_bytes(
    writer: &mut impl Write,
    mut bytes: &[u8],
    stop: &AtomicBool,
    deadline: Instant,
) -> io::Result<()> {
    while !bytes.is_empty() {
        check_http_transfer_active(stop, deadline)?;
        match writer.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "LAN response write failed",
                ));
            }
            Ok(written) => bytes = &bytes[written..],
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn write_text_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    body: &str,
    head: bool,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    )?;
    if !head {
        stream.write_all(body.as_bytes())?;
    }
    Ok(())
}

fn write_file_response(
    stream: &mut TcpStream,
    content_type: &str,
    path: &Path,
    length: u64,
    range_header: Option<&str>,
    head: bool,
    stop: &AtomicBool,
) -> io::Result<()> {
    let range = match range_header {
        Some(value) => match parse_single_range(value, length) {
            Some(range) => Some(range),
            None => {
                write!(
                    stream,
                    "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{length}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )?;
                return Ok(());
            }
        },
        None => None,
    };
    let (start, end, status) = range
        .map_or((0, length.saturating_sub(1), "200 OK"), |(start, end)| {
            (start, end, "206 Partial Content")
        });
    let content_length = if length == 0 {
        0
    } else {
        end.saturating_sub(start).saturating_add(1)
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nAccept-Ranges: bytes\r\nContent-Length: {content_length}\r\n"
    )?;
    if range.is_some() {
        write!(stream, "Content-Range: bytes {start}-{end}/{length}\r\n")?;
    }
    write!(stream, "Connection: close\r\n\r\n")?;
    if head || content_length == 0 {
        return Ok(());
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut remaining = content_length;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 && !stop.load(Ordering::Acquire) {
        let wanted = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let read = file.read(&mut buffer[..wanted])?;
        if read == 0 {
            break;
        }
        write_media_bytes(stream, &buffer[..read], stop)?;
        remaining = remaining.saturating_sub(read as u64);
    }
    Ok(())
}

/// Maximum time without a successful media write before releasing its worker.
const MEDIA_WRITE_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Writes one media chunk without treating temporary client backpressure as a
/// truncated download.
///
/// Accepted sockets retain short write polls so shutdown remains responsive.
/// Transient timeouts and interruptions retry the exact unsent suffix, but a
/// client that makes no write progress for 60 seconds releases its worker.
/// Successful writes restart the idle timer; total media duration is unlimited.
fn write_media_bytes(stream: &mut TcpStream, bytes: &[u8], stop: &AtomicBool) -> io::Result<()> {
    write_media_bytes_with_policy(stream, bytes, stop, MEDIA_WRITE_IDLE_TIMEOUT, Instant::now)
}

/// Applies media-write retry policy with an injectable clock for deterministic
/// cancellation, partial-progress, and idle-timeout regression tests.
fn write_media_bytes_with_policy(
    writer: &mut impl Write,
    mut bytes: &[u8],
    stop: &AtomicBool,
    idle_timeout: Duration,
    mut now: impl FnMut() -> Instant,
) -> io::Result<()> {
    let mut last_progress = now();
    while !bytes.is_empty() {
        if stop.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "LAN media share stopped",
            ));
        }
        if now().saturating_duration_since(last_progress) >= idle_timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "LAN media response stalled without write progress",
            ));
        }
        match writer.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "could not write LAN media response",
                ));
            }
            Ok(written) => {
                bytes = &bytes[written..];
                last_progress = now();
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn parse_single_range(value: &str, length: u64) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    if value.contains(',') || length == 0 {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?.min(length);
        return (suffix > 0).then(|| (length.saturating_sub(suffix), length.saturating_sub(1)));
    }
    let start = start.parse::<u64>().ok()?;
    if start >= length {
        return None;
    }
    let end = if end.is_empty() {
        length.saturating_sub(1)
    } else {
        end.parse::<u64>().ok()?.min(length.saturating_sub(1))
    };
    (start <= end).then_some((start, end))
}

fn discover_lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect(SocketAddr::from(([192, 0, 2, 1], 9))).ok()?;
    Some(socket.local_addr().ok()?.ip())
}

fn local_guid(path: &Path, metadata: &fs::Metadata) -> String {
    let mut digest = Sha256::new();
    digest.update(path.as_os_str().as_encoded_bytes());
    digest.update(metadata.len().to_le_bytes());
    if let Ok(modified) = metadata.modified()
        && let Ok(duration) = modified.duration_since(UNIX_EPOCH)
    {
        digest.update(duration.as_nanos().to_le_bytes());
    }
    format!("urn:youta:local:{:x}", digest.finalize())
}

fn mime_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("opus" | "ogg" | "oga") => "audio/ogg",
        Some("mp3") => "audio/mpeg",
        Some("m4a" | "mp4" | "m4b") => "audio/mp4",
        Some("aac") => "audio/aac",
        Some("flac") => "audio/flac",
        Some("wav") => "audio/wav",
        Some("webm") => "audio/webm",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn escape_html(value: &str) -> String {
    escape_xml(value)
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    use super::*;
    use crate::test_support::canonical_tempdir;

    #[test]
    fn podcast_feed_escapes_titles_and_exposes_unique_enclosures() {
        let directory = canonical_tempdir("lan-podcast");
        let first = directory.path().join("one & two.opus");
        let second = directory.path().join("three.opus");
        fs::write(&first, b"first").expect("write first audio");
        fs::write(&second, b"second").expect("write second audio");
        let cache = directory.path().join("artwork");
        let prepared = prepare_podcast_share(directory.path(), &cache).expect("prepare feed");
        let state = ServerState::new(prepared, "http://192.0.2.10:8123".to_owned());

        let rss = state.rss();
        assert!(rss.contains("<title>one &amp; two.opus</title>"));
        assert!(rss.contains("url=\"http://192.0.2.10:8123/media/0/"));
        assert!(rss.contains("url=\"http://192.0.2.10:8123/media/1/"));
        assert_eq!(rss.matches("<guid isPermaLink=\"false\">").count(), 2);
    }

    #[test]
    fn local_podcast_episodes_publish_their_own_modification_dates() {
        let directory = canonical_tempdir("lan-podcast-dates");
        for (name, seconds) in [("01.opus", 1_704_164_645), ("02.opus", 1_706_933_106)] {
            let file = File::create(directory.path().join(name)).expect("create episode");
            file.set_modified(UNIX_EPOCH + Duration::from_secs(seconds))
                .expect("set episode modification time");
        }
        let prepared = prepare_podcast_share(directory.path(), &directory.path().join("cache"))
            .expect("prepare dated feed");
        let rss = ServerState::new(prepared, "http://192.0.2.10:8123".to_owned()).rss();
        let dates = rss
            .split("<pubDate>")
            .skip(1)
            .map(|item| item.split_once("</pubDate>").expect("closed date").0)
            .collect::<Vec<_>>();

        assert_eq!(
            dates,
            [
                "Tue, 2 Jan 2024 03:04:05 +0000",
                "Sat, 3 Feb 2024 04:05:06 +0000"
            ]
        );
        for (date, expected) in dates.into_iter().zip([1_704_164_645, 1_706_933_106]) {
            assert_eq!(
                chrono::DateTime::parse_from_rfc2822(date)
                    .expect("RSS date")
                    .timestamp(),
                expected
            );
        }
    }

    #[test]
    fn local_podcast_feed_publishes_item_artwork_as_the_channel_cover() {
        let directory = canonical_tempdir("lan-podcast-cover");
        fs::write(directory.path().join("episode.opus"), b"audio").expect("write audio");
        fs::write(
            directory.path().join("episode.png"),
            b"\x89PNG\r\n\x1a\nfixture",
        )
        .expect("write sidecar cover");
        let prepared = prepare_podcast_share(directory.path(), &directory.path().join("cache"))
            .expect("prepare feed");
        let mut state = ServerState::new(prepared, "http://192.0.2.10:8123".to_owned());
        state.title = "Local fixture".to_owned();

        let rss = state.rss();
        assert!(rss.contains("<itunes:image href=\"http://192.0.2.10:8123/artwork/0\"/>"));
        assert!(rss.contains(
            "<image>\n<url>http://192.0.2.10:8123/artwork/0</url>\n<title>Local fixture</title>\n<link>http://192.0.2.10:8123/</link>\n</image>"
        ));
        assert_eq!(rss.matches("<itunes:image href=").count(), 2);
    }

    #[test]
    fn podcast_share_ignores_non_media_and_symlinks() {
        let directory = canonical_tempdir("lan-safe-scan");
        fs::write(directory.path().join("episode.opus"), b"audio").expect("write audio");
        fs::write(directory.path().join("notes.txt"), b"private").expect("write notes");
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            directory.path().join("notes.txt"),
            directory.path().join("alias.opus"),
        )
        .expect("create symlink");

        let prepared = prepare_podcast_share(directory.path(), &directory.path().join("cache"))
            .expect("prepare podcast");
        assert_eq!(prepared.files.len(), 1);
        assert_eq!(prepared.files[0].label, "episode.opus");
    }

    #[test]
    fn local_podcast_boundary_starts_with_the_selected_file_in_feed_order() {
        let directory = canonical_tempdir("lan-podcast-boundary");
        let first = directory.path().join("01-introduction.opus");
        let selected = directory.path().join("02-selected.opus");
        let last = directory.path().join("03-finale.opus");
        fs::write(&first, b"first").expect("write first audio");
        fs::write(&selected, b"selected").expect("write selected audio");
        fs::write(&last, b"last").expect("write last audio");

        let prepared = prepare_podcast_share_from(
            directory.path(),
            &directory.path().join("cache"),
            &selected,
        )
        .expect("prepare feed from selected file");

        assert_eq!(
            prepared
                .files
                .iter()
                .map(|file| file.label.as_str())
                .collect::<Vec<_>>(),
            ["02-selected.opus", "03-finale.opus"]
        );
    }

    #[test]
    fn youtube_feed_uses_stable_proxy_routes_and_artwork_for_every_episode() {
        let collection = ExtractedCollection {
            id: "UCfixture".to_owned(),
            title: "Fixture channel".to_owned(),
            extractor: Some("YoutubeTab".to_owned()),
            thumbnail_url: Some(
                Url::parse("https://yt3.example/fixture-channel-avatar.jpg")
                    .expect("channel avatar"),
            ),
            entries: vec![
                crate::playback::ytdlp::CollectionEntry {
                    id: "dQw4w9WgXcQ".to_owned(),
                    title: "First & episode".to_owned(),
                    webpage_url: None,
                    duration_seconds: Some(42),
                    published_at: Some(1_706_933_106),
                    thumbnail_url: Some(
                        Url::parse("https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg")
                            .expect("thumbnail"),
                    ),
                },
                crate::playback::ytdlp::CollectionEntry {
                    id: "M7lc1UVf-VE".to_owned(),
                    title: "Second episode".to_owned(),
                    webpage_url: None,
                    duration_seconds: None,
                    published_at: Some(1_704_164_645),
                    thumbnail_url: None,
                },
            ],
        };
        let prepared =
            prepare_youtube_podcast_share(collection, YouTubePrewarmConfig::default(), false)
                .expect("prepare YouTube feed");
        assert_eq!(prepared.files.len(), 2);
        assert_eq!(prepared.artwork.len(), 3);
        assert!(prepared.files.iter().all(|file| file.length == 0));
        assert!(matches!(
            &prepared.artwork[2].source,
            SharedArtworkSource::YouTube {
                initial_url: Some(_),
                ..
            }
        ));
        assert_eq!(
            prepared
                .remote_config
                .as_ref()
                .map(|config| config.audio_format.as_str()),
            Some("bestaudio[ext=webm]")
        );
        assert_eq!(
            prepared
                .remote_config
                .as_ref()
                .map(|config| config.player_client_policy),
            Some(YouTubePlayerClientPolicy::EmbeddedThenDefault)
        );
        let state = ServerState::new(prepared, "http://192.0.2.10:8123".to_owned());

        let rss = state.rss();
        assert!(rss.contains("<title>First &amp; episode</title>"));
        assert_eq!(rss.matches("<itunes:image href=").count(), 3);
        assert!(rss.contains(
            "<image>\n<url>http://192.0.2.10:8123/artwork/0</url>\n<title>Fixture channel</title>"
        ));
        assert!(rss.contains("<itunes:duration>42</itunes:duration>"));
        assert!(rss.contains("length=\"0\" type=\"audio/webm\""));
        assert!(rss.contains("http://192.0.2.10:8123/media/0/"));
        assert!(!rss.contains("googlevideo.com"));
        assert!(!rss.contains("i.ytimg.com"));
    }

    #[test]
    fn youtube_podcast_boundary_keeps_the_selected_video_and_newer_uploads_oldest_first() {
        let entry = |id: &str, title: &str| crate::playback::ytdlp::CollectionEntry {
            id: id.to_owned(),
            title: title.to_owned(),
            webpage_url: None,
            duration_seconds: None,
            published_at: Some(1_704_164_645),
            thumbnail_url: None,
        };
        let collection = ExtractedCollection {
            id: "UCfixture".to_owned(),
            title: "Fixture channel".to_owned(),
            extractor: Some("YoutubeTab".to_owned()),
            thumbnail_url: None,
            entries: vec![
                entry("aqz-KE-bpKQ", "After selection"),
                entry("M7lc1UVf-VE", "Selected episode"),
                entry("dQw4w9WgXcQ", "Before selection"),
            ],
        };

        let prepared = prepare_youtube_podcast_share_from(
            collection,
            YouTubePrewarmConfig::default(),
            "M7lc1UVf-VE",
            false,
        )
        .expect("prepare YouTube feed from selected video");

        assert_eq!(
            prepared
                .files
                .iter()
                .map(|file| file.label.as_str())
                .collect::<Vec<_>>(),
            ["Selected episode", "After selection"]
        );
        assert_eq!(prepared.files[0].route, "/media/0/M7lc1UVf%2DVE.webm");
        let state = ServerState::new(prepared, "http://192.0.2.10:8123".to_owned());
        assert!(state.rss().contains(
            "<image>\n<url>http://192.0.2.10:8123/artwork/0</url>\n<title>Fixture channel</title>"
        ));
    }

    #[test]
    fn youtube_podcast_skip_shorts_uses_provider_urls_after_the_selected_boundary() {
        let entry = |id: &str, title: &str, path: &str| crate::playback::ytdlp::CollectionEntry {
            id: id.to_owned(),
            title: title.to_owned(),
            webpage_url: Some(
                Url::parse(&format!("https://www.youtube.com/{path}/{id}"))
                    .expect("YouTube fixture URL"),
            ),
            duration_seconds: None,
            published_at: Some(1_704_164_645),
            thumbnail_url: None,
        };
        let collection = ExtractedCollection {
            id: "UCfixture".to_owned(),
            title: "Fixture channel".to_owned(),
            extractor: Some("YoutubeTab".to_owned()),
            thumbnail_url: None,
            entries: vec![
                entry("aqz-KE-bpKQ", "Retained video", "watch?v="),
                entry("M7lc1UVf-VE", "Selected Short", "shorts"),
                entry("dQw4w9WgXcQ", "Before selection", "watch?v="),
            ],
        };

        let prepared = prepare_youtube_podcast_share_from(
            collection,
            YouTubePrewarmConfig::default(),
            "M7lc1UVf-VE",
            true,
        )
        .expect("prepare filtered YouTube feed");

        assert_eq!(prepared.item_count(), 1);
        assert_eq!(prepared.files[0].label, "Retained video");
    }

    #[test]
    fn youtube_podcast_global_upload_order_and_short_boundary_do_not_retain_older_tabs() {
        let entry = |id: &str, title: &str, short: bool| crate::playback::ytdlp::CollectionEntry {
            id: id.to_owned(),
            title: title.to_owned(),
            webpage_url: Some(
                Url::parse(&if short {
                    format!("https://www.youtube.com/shorts/{id}")
                } else {
                    format!("https://www.youtube.com/watch?v={id}")
                })
                .expect("video URL"),
            ),
            duration_seconds: None,
            published_at: Some(1_704_164_645),
            thumbnail_url: None,
        };
        let collection = ExtractedCollection {
            id: "UCfixture".to_owned(),
            title: "Fixture channel".to_owned(),
            extractor: Some("YoutubeTab".to_owned()),
            thumbnail_url: None,
            // Unified uploads are newest first, with regular videos and Shorts interleaved.
            entries: vec![
                entry("aaaaaaaaaaa", "Newest Short", true),
                entry("bbbbbbbbbbb", "Newer video", false),
                entry("ccccccccccc", "Selected Short", true),
                entry("ddddddddddd", "Older video", false),
                entry("eeeeeeeeeee", "Oldest Short", true),
            ],
        };
        let labels = |prepared: PreparedLocalShare| {
            prepared
                .files
                .into_iter()
                .map(|file| file.label)
                .collect::<Vec<_>>()
        };
        let all = prepare_youtube_podcast_share(
            collection.clone(),
            YouTubePrewarmConfig::default(),
            false,
        )
        .expect("whole feed");
        assert_eq!(
            labels(all),
            [
                "Oldest Short",
                "Older video",
                "Selected Short",
                "Newer video",
                "Newest Short"
            ]
        );

        let selected = prepare_youtube_podcast_share_from(
            collection.clone(),
            YouTubePrewarmConfig::default(),
            "ccccccccccc",
            false,
        )
        .expect("selected and newer feed");
        let state = ServerState::new(selected, "http://192.0.2.10:8123".to_owned());
        assert_eq!(
            state
                .files
                .iter()
                .map(|file| file.label.as_str())
                .collect::<Vec<_>>(),
            ["Selected Short", "Newer video", "Newest Short"]
        );
        let rss = state.rss();
        assert!(
            rss.find("Selected Short").expect("selected") < rss.find("Newer video").expect("newer")
        );
        assert!(
            rss.find("Newer video").expect("newer") < rss.find("Newest Short").expect("newest")
        );
        assert!(!rss.contains("Older video"));

        let filtered = prepare_youtube_podcast_share_from(
            collection,
            YouTubePrewarmConfig::default(),
            "ccccccccccc",
            true,
        )
        .expect("selected Short remains a valid cutoff");
        assert_eq!(labels(filtered), ["Newer video"]);
    }

    /// Makes a newest-first upload catalogue with distinct dated episodes.
    fn dated_youtube_collection() -> ExtractedCollection {
        let entry = |id: &str, published_at, short: bool| crate::playback::ytdlp::CollectionEntry {
            id: id.to_owned(),
            title: id.to_owned(),
            webpage_url: short.then(|| {
                Url::parse(&format!("https://www.youtube.com/shorts/{id}")).expect("Short URL")
            }),
            duration_seconds: None,
            published_at,
            thumbnail_url: None,
        };
        ExtractedCollection {
            id: "UCfixture".to_owned(),
            title: "Dated uploads".to_owned(),
            extractor: Some("YoutubeTab".to_owned()),
            thumbnail_url: None,
            entries: vec![
                entry("aaaaaaaaaaa", Some(1_706_933_106), false),
                entry("bbbbbbbbbbb", None, true),
                entry("ccccccccccc", Some(1_704_164_645), false),
                entry("ddddddddddd", None, false),
            ],
        }
    }

    #[test]
    fn youtube_podcast_episodes_preserve_dates_after_cutoff_and_short_filter() {
        let collection = dated_youtube_collection();
        assert_eq!(
            youtube_podcast_episode_indices(&collection, Some("ccccccccccc"), true)
                .expect("retained episode indices"),
            [2, 0],
        );
        assert_eq!(
            youtube_podcast_episode_indices(&collection, Some("bbbbbbbbbbb"), true)
                .expect("selected Short still defines the cutoff"),
            [0],
        );
        let prepared = prepare_youtube_podcast_share_from(
            collection,
            YouTubePrewarmConfig::default(),
            "ccccccccccc",
            true,
        )
        .expect("prepare dated YouTube feed");
        let rss = ServerState::new(prepared, "http://192.0.2.10:8123".to_owned()).rss();
        let episodes = rss.split("<item>").skip(1).collect::<Vec<_>>();
        assert_eq!(episodes.len(), 2);
        for (episode, id, date) in [
            (episodes[0], "ccccccccccc", "Tue, 2 Jan 2024 03:04:05 +0000"),
            (episodes[1], "aaaaaaaaaaa", "Sat, 3 Feb 2024 04:05:06 +0000"),
        ] {
            assert!(episode.contains(&format!("<title>{id}</title>")));
            assert!(episode.contains(&format!("<pubDate>{date}</pubDate>")));
            assert!(chrono::DateTime::parse_from_rfc2822(date).is_ok());
        }
    }

    #[test]
    fn youtube_podcast_rejects_retained_episodes_without_valid_dates() {
        for published_at in [None, Some(i64::MAX), Some(253_402_300_800)] {
            let mut collection = dated_youtube_collection();
            collection.entries[0].published_at = published_at;
            let error = prepare_youtube_podcast_share_from(
                collection,
                YouTubePrewarmConfig::default(),
                "aaaaaaaaaaa",
                false,
            )
            .expect_err("retained episode must have an RSS date");
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("aaaaaaaaaaa"));
            assert!(error.to_string().contains("publication date"));
        }
    }

    #[test]
    fn local_podcast_dates_accept_pre_epoch_times_and_reject_unrepresentable_dates() {
        let date = local_podcast_date(UNIX_EPOCH - Duration::from_millis(1))
            .expect("valid pre-epoch time");
        assert_eq!(date.to_rfc2822(), "Wed, 31 Dec 1969 23:59:59 +0000");
        let invalid = UNIX_EPOCH + Duration::from_secs(253_402_300_800);
        assert!(local_podcast_date(invalid).is_err());
    }

    #[test]
    fn server_supports_head_and_single_byte_ranges() {
        let directory = canonical_tempdir("lan-range");
        let audio = directory.path().join("episode.opus");
        fs::write(&audio, b"0123456789").expect("write audio");
        let mut server = LanShareServer::start(prepare_file_share(&audio).expect("prepare file"))
            .expect("start server");
        let address = server
            .url()
            .strip_prefix("http://")
            .and_then(|url| url.split('/').next())
            .expect("server authority");

        let head = request(
            address,
            "HEAD /media/0/episode.opus HTTP/1.1\r\nHost: test\r\n\r\n",
        );
        assert!(head.starts_with("HTTP/1.1 200 OK"));
        assert!(head.contains("Accept-Ranges: bytes"));
        assert!(head.ends_with("\r\n\r\n"));
        let partial = request(
            address,
            "GET /media/0/episode.opus HTTP/1.1\r\nHost: test\r\nRange: bytes=2-5\r\n\r\n",
        );
        assert!(partial.starts_with("HTTP/1.1 206 Partial Content"));
        assert!(partial.contains("Content-Range: bytes 2-5/10"));
        assert!(partial.ends_with("\r\n\r\n2345"));
        let oversized = request(
            address,
            &format!(
                "GET / HTTP/1.1\r\nX-Oversized: {}\r\n\r\n",
                "x".repeat(MAX_REQUEST_HEADER_BYTES)
            ),
        );
        assert!(oversized.starts_with("HTTP/1.1 431 Request Header Fields Too Large"));
        server.stop();
    }

    #[test]
    fn http_transfer_deadline_and_cancellation_stop_before_io() {
        for (cancelled, kind) in [
            (false, io::ErrorKind::TimedOut),
            (true, io::ErrorKind::Interrupted),
        ] {
            let stop = AtomicBool::new(cancelled);
            let deadline = if cancelled {
                Instant::now() + Duration::from_secs(5)
            } else {
                Instant::now()
            };
            let mut input = io::Cursor::new(b"GET /feed.xml HTTP/1.1\r\n");
            assert_eq!(
                read_request_line(&mut input, 1024, deadline, &stop)
                    .expect_err("inactive request must stop")
                    .kind(),
                kind,
            );
            assert_eq!(input.position(), 0);
            let mut output = Vec::new();
            assert_eq!(
                write_response_bytes(&mut output, b"response", &stop, deadline)
                    .expect_err("inactive response must stop")
                    .kind(),
                kind,
            );
            assert!(output.is_empty());
        }
    }

    /// Advances a fake clock for each bounded write and records only accepted bytes.
    struct ScriptedMediaWriter<'a> {
        clock: &'a std::cell::Cell<Instant>,
        steps: std::collections::VecDeque<(Duration, io::Result<usize>)>,
        calls: usize,
        output: Vec<u8>,
        cancel_after_write: Option<&'a AtomicBool>,
    }

    impl Write for ScriptedMediaWriter<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            let (elapsed, result) = self
                .steps
                .pop_front()
                .unwrap_or_else(|| (Duration::ZERO, Err(io::ErrorKind::BrokenPipe.into())));
            self.clock.set(self.clock.get() + elapsed);
            if let Some(stop) = self.cancel_after_write {
                stop.store(true, Ordering::Release);
            }
            let written = result?.min(bytes.len());
            self.output.extend_from_slice(&bytes[..written]);
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Makes a finite writer script whose exhausted fallback cannot hang a test.
    fn scripted_media_writer(
        clock: &std::cell::Cell<Instant>,
        steps: impl IntoIterator<Item = (Duration, io::Result<usize>)>,
    ) -> ScriptedMediaWriter<'_> {
        ScriptedMediaWriter {
            clock,
            steps: steps.into_iter().collect(),
            calls: 0,
            output: Vec::new(),
            cancel_after_write: None,
        }
    }

    #[test]
    fn media_write_stalls_timeout_without_resetting_on_transient_errors() {
        for kind in [
            io::ErrorKind::WouldBlock,
            io::ErrorKind::TimedOut,
            io::ErrorKind::Interrupted,
        ] {
            let clock = std::cell::Cell::new(Instant::now());
            let mut writer = scripted_media_writer(
                &clock,
                (0..3).map(|_| (Duration::from_secs(4), Err(kind.into()))),
            );
            let error = write_media_bytes_with_policy(
                &mut writer,
                b"audio",
                &AtomicBool::new(false),
                Duration::from_secs(10),
                || clock.get(),
            )
            .expect_err("stalled writes must expire before the finite script fallback");
            assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{kind:?}");
            assert_eq!(writer.calls, 3);
            assert!(writer.output.is_empty());
        }
    }

    #[test]
    fn media_write_progress_resets_only_idle_timeout_and_preserves_exact_bytes() {
        let started = Instant::now();
        let clock = std::cell::Cell::new(started);
        let mut writer = scripted_media_writer(
            &clock,
            [
                (Duration::from_secs(4), Ok(2)),
                (Duration::from_secs(2), Err(io::ErrorKind::TimedOut.into())),
                (
                    Duration::from_secs(2),
                    Err(io::ErrorKind::WouldBlock.into()),
                ),
                (
                    Duration::from_secs(2),
                    Err(io::ErrorKind::Interrupted.into()),
                ),
                (Duration::from_secs(2), Ok(2)),
                (Duration::from_secs(4), Ok(2)),
            ],
        );
        let idle_timeout = Duration::from_secs(10);
        write_media_bytes_with_policy(
            &mut writer,
            b"abcdef",
            &AtomicBool::new(false),
            idle_timeout,
            || clock.get(),
        )
        .expect("positive writes keep a slow transfer alive across interrupted polls");
        assert!(clock.get().duration_since(started) > idle_timeout);
        assert_eq!(writer.calls, 6);
        assert_eq!(writer.output, b"abcdef");
    }

    #[test]
    fn media_write_cancellation_zero_and_terminal_errors_remain_bounded() {
        for cancelled_before_io in [false, true] {
            let clock = std::cell::Cell::new(Instant::now());
            let stop = AtomicBool::new(cancelled_before_io);
            let mut writer = scripted_media_writer(&clock, [(Duration::ZERO, Ok(2))]);
            writer.cancel_after_write = Some(&stop);
            let error = write_media_bytes_with_policy(
                &mut writer,
                b"audio",
                &stop,
                Duration::from_secs(10),
                || clock.get(),
            )
            .expect_err("cancellation must stop before the next write");
            assert_eq!(error.kind(), io::ErrorKind::Interrupted);
            assert_eq!(writer.calls, usize::from(!cancelled_before_io));
            assert_eq!(
                writer.output,
                if cancelled_before_io {
                    b"".as_slice()
                } else {
                    b"au".as_slice()
                }
            );
        }
        for (result, expected_kind) in [
            (Ok(0), io::ErrorKind::WriteZero),
            (
                Err(io::ErrorKind::BrokenPipe.into()),
                io::ErrorKind::BrokenPipe,
            ),
            (
                Err(io::ErrorKind::ConnectionReset.into()),
                io::ErrorKind::ConnectionReset,
            ),
            (
                Err(io::ErrorKind::PermissionDenied.into()),
                io::ErrorKind::PermissionDenied,
            ),
        ] {
            let clock = std::cell::Cell::new(Instant::now());
            let mut writer = scripted_media_writer(&clock, [(Duration::ZERO, result)]);
            let error = write_media_bytes_with_policy(
                &mut writer,
                b"audio",
                &AtomicBool::new(false),
                Duration::from_secs(10),
                || clock.get(),
            )
            .expect_err("non-transient write failures must stay terminal");
            assert_eq!(error.kind(), expected_kind);
            assert_eq!(writer.calls, 1);
            assert!(writer.output.is_empty());
        }
    }

    #[test]
    fn http_response_retries_preserve_partial_write_progress() {
        /// Delivers a prefix, then transient errors before accepting the suffix.
        #[derive(Default)]
        struct InterruptedWriter {
            calls: usize,
            output: Vec<u8>,
        }

        impl Write for InterruptedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.calls += 1;
                let written = match self.calls {
                    1 => bytes.len().min(2),
                    2 => return Err(io::Error::from(io::ErrorKind::TimedOut)),
                    3 => return Err(io::Error::from(io::ErrorKind::WouldBlock)),
                    4 => return Err(io::Error::from(io::ErrorKind::Interrupted)),
                    _ => bytes.len(),
                };
                self.output.extend_from_slice(&bytes[..written]);
                Ok(written)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut writer = InterruptedWriter::default();
        write_response_bytes(
            &mut writer,
            b"complete XML response",
            &AtomicBool::new(false),
            Instant::now() + Duration::from_secs(5),
        )
        .expect("transient errors should resume from the exact unsent byte");
        assert_eq!(writer.calls, 5);
        assert_eq!(writer.output, b"complete XML response");
    }

    /// Bounds admission fixtures' connect, write, and response reads.
    fn admission_request(address: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream
            .take(2 * 1024 * 1024)
            .read_to_string(&mut response)
            .expect("bounded admission response");
        response
    }

    /// Minimal immutable state for direct admission-handler deadline tests.
    fn admission_test_state(kind: LanShareKind) -> ServerState {
        ServerState {
            kind,
            title: "Fixture".to_owned(),
            feed_artwork_route: None,
            base_url: "http://127.0.0.1".to_owned(),
            files: Vec::new(),
            artwork: Vec::new(),
            remote: None,
        }
    }

    /// Reproduces inherited Winsock nonblocking mode on Unix and inspects it
    /// directly, without relying on scheduling delays or timeout measurements.
    #[cfg(unix)]
    fn assert_http_handler_restores_blocking_mode(overloaded: bool) {
        use rustix::fs::{OFlags, fcntl_getfl};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(true).unwrap();
        // Kernels may round timeout values to their timer resolution. Capture
        // that normalization, then ensure the handler replaces longer polls.
        stream.set_read_timeout(Some(IO_POLL)).unwrap();
        stream.set_write_timeout(Some(IO_POLL)).unwrap();
        let expected_read_timeout = stream.read_timeout().unwrap();
        let expected_write_timeout = stream.write_timeout().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let observer = stream.try_clone().unwrap();
        assert!(fcntl_getfl(&observer).unwrap().contains(OFlags::NONBLOCK));
        client
            .write_all(b"GET /feed.xml HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .unwrap();
        let state = admission_test_state(LanShareKind::Podcast);
        let stop = AtomicBool::new(false);
        if overloaded {
            handle_overloaded_connection(stream, &state, &stop).unwrap();
        } else {
            handle_connection_with_admission(stream, &state, &stop, None).unwrap();
        }
        assert!(
            !fcntl_getfl(&observer).unwrap().contains(OFlags::NONBLOCK),
            "HTTP workers must use blocking I/O with short socket timeouts"
        );
        assert_eq!(observer.read_timeout().unwrap(), expected_read_timeout);
        assert_eq!(observer.write_timeout().unwrap(), expected_write_timeout);
        drop(observer);

        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert_eq!(body, state.rss());
    }

    #[cfg(unix)]
    #[test]
    fn server_admission_normal_handler_restores_blocking_mode() {
        assert_http_handler_restores_blocking_mode(false);
    }

    #[cfg(unix)]
    #[test]
    fn server_admission_overload_handler_restores_blocking_mode() {
        assert_http_handler_restores_blocking_mode(true);
    }

    /// Waits for an observed queue transition instead of guessing thread timing.
    fn wait_for_admission_queue(pool: &AdmissionPool, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if pool.state.lock().unwrap().waiting.len() == expected {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(pool.state.lock().unwrap().waiting.len(), expected);
    }

    #[test]
    fn server_admission_queue_is_bounded_fifo_and_releases_after_work() {
        let pool = Arc::new(AdmissionPool::new(1, 2));
        let stop = AtomicBool::new(false);
        let held = pool.acquire(&stop).unwrap().unwrap();
        let (completed, received) = std::sync::mpsc::channel();
        let (release_first, released) = std::sync::mpsc::channel();
        let first_pool = Arc::clone(&pool);
        let first_completed = completed.clone();
        let first = thread::spawn(move || {
            let _permit = first_pool
                .acquire(&AtomicBool::new(false))
                .unwrap()
                .unwrap();
            first_completed.send(1).unwrap();
            released.recv_timeout(Duration::from_secs(2)).unwrap();
        });
        wait_for_admission_queue(&pool, 1);
        let second_pool = Arc::clone(&pool);
        let second = thread::spawn(move || {
            let _permit = second_pool
                .acquire(&AtomicBool::new(false))
                .unwrap()
                .unwrap();
            completed.send(2).unwrap();
        });
        wait_for_admission_queue(&pool, 2);
        let rejected_at = Instant::now();
        assert!(pool.acquire(&stop).unwrap().is_none());
        assert!(rejected_at.elapsed() < Duration::from_millis(200));
        assert_eq!(pool.state.lock().unwrap().waiting.len(), 2);
        drop(held);
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
        assert!(received.try_recv().is_err());
        release_first.send(()).unwrap();
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), 2);
        first.join().unwrap();
        second.join().unwrap();
        let state = pool.state.lock().unwrap();
        assert_eq!(state.active, 0);
        assert!(state.waiting.is_empty());
    }

    #[test]
    fn server_admission_expired_and_cancelled_front_waiters_do_not_block_followers() {
        for cancel in [false, true] {
            let pool = Arc::new(AdmissionPool::new(1, 2));
            let first_stop = Arc::new(AtomicBool::new(false));
            let stop = AtomicBool::new(false);
            let held = pool.acquire(&stop).unwrap().unwrap();
            let first_pool = Arc::clone(&pool);
            let worker_stop = Arc::clone(&first_stop);
            let first = thread::spawn(move || {
                match first_pool.acquire_with_timeout(
                    &worker_stop,
                    if cancel {
                        Duration::from_secs(10)
                    } else {
                        Duration::from_millis(300)
                    },
                ) {
                    Ok(permit) => {
                        assert!(permit.is_none());
                        None
                    }
                    Err(error) => Some(error.kind()),
                }
            });
            wait_for_admission_queue(&pool, 1);
            let second_pool = Arc::clone(&pool);
            let second = thread::spawn(move || {
                let _permit = second_pool
                    .acquire(&AtomicBool::new(false))
                    .unwrap()
                    .unwrap();
            });
            wait_for_admission_queue(&pool, 2);
            let stopped_at = Instant::now();
            if cancel {
                first_stop.store(true, Ordering::Release);
            }
            assert_eq!(
                first.join().unwrap(),
                cancel.then_some(io::ErrorKind::Interrupted)
            );
            assert!(stopped_at.elapsed() < Duration::from_secs(1));
            wait_for_admission_queue(&pool, 1);
            drop(held);
            second.join().unwrap();
            let state = pool.state.lock().unwrap();
            assert_eq!(state.active, 0);
            assert!(state.waiting.is_empty());
        }
    }

    #[test]
    fn server_admission_handler_error_releases_its_transfer_permit() {
        let directory = canonical_tempdir("lan-admission-error");
        let mut state = admission_test_state(LanShareKind::Files);
        state.files.push(SharedFile {
            guid: "missing".to_owned(),
            label: "Missing".to_owned(),
            published_at: None,
            length: 4,
            mime: "audio/opus",
            source: SharedFileSource::Local(directory.path().join("missing.opus")),
            route: "/media/0/missing.opus".to_owned(),
            artwork_route: None,
        });
        let admission = Arc::new(RequestAdmission::new());
        let worker_admission = Arc::clone(&admission);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection_with_admission(
                stream,
                &state,
                &AtomicBool::new(false),
                Some(&worker_admission),
            )
        });
        let _response = admission_request(
            address,
            "GET /media/0/missing.opus HTTP/1.1\r\nHost: fixture\r\n\r\n",
        );
        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let state = admission.media.state.lock().unwrap();
        assert_eq!(state.active, 0);
        assert!(state.waiting.is_empty());
    }

    /// Holds every media permit with an admitted large response whose client
    /// reads the headers but deliberately stops consuming its audio body.
    fn occupy_media_workers(
        kind: LanShareKind,
    ) -> (
        tempfile::TempDir,
        LanShareServer,
        SocketAddr,
        Vec<TcpStream>,
        String,
    ) {
        occupy_request_workers(kind, 0)
    }

    /// Mixes slow artwork and media readers to reproduce shared-slot starvation.
    fn occupy_request_workers(
        kind: LanShareKind,
        artwork_count: usize,
    ) -> (
        tempfile::TempDir,
        LanShareServer,
        SocketAddr,
        Vec<TcpStream>,
        String,
    ) {
        let directory = canonical_tempdir("lan-busy-workers");
        let audio = directory.path().join("episode.opus");
        File::create(&audio)
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
        let mut prepared = if kind == LanShareKind::Podcast {
            prepare_podcast_share(&audio, &directory.path().join("artwork")).unwrap()
        } else {
            prepare_file_share(&audio).unwrap()
        };
        prepared.artwork.push(SharedArtwork {
            mime: "image/jpeg",
            source: SharedArtworkSource::Local(audio),
        });
        let server = LanShareServer::start(prepared).unwrap();
        let address: SocketAddr = server
            .url()
            .strip_prefix("http://")
            .unwrap()
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let expected_feed = if kind == LanShareKind::Podcast {
            admission_request(address, "GET /feed.xml HTTP/1.1\r\nHost: fixture\r\n\r\n")
                .split_once("\r\n\r\n")
                .unwrap()
                .1
                .to_owned()
        } else {
            String::new()
        };
        let mut clients = Vec::new();
        for index in 0..MAX_MEDIA_CONNECTIONS {
            let mut client = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            client
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let path = if index < artwork_count {
                "/artwork/0"
            } else {
                "/media/0/episode.opus"
            };
            client
                .write_all(format!("GET {path} HTTP/1.1\r\nHost: fixture\r\n\r\n").as_bytes())
                .unwrap();
            let mut reader = BufReader::new(client.try_clone().unwrap());
            let mut status = String::new();
            reader.read_line(&mut status).unwrap();
            assert!(status.starts_with("HTTP/1.1 200 OK"));
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
            }
            clients.push(client);
        }
        (directory, server, address, clients, expected_feed)
    }

    #[test]
    fn server_admission_artwork_cannot_consume_audio_capacity() {
        let (_directory, mut server, address, clients, _) =
            occupy_request_workers(LanShareKind::Podcast, 4);
        let response = admission_request(
            address,
            "GET /media/0/episode.opus HTTP/1.1\r\nHost: fixture\r\nRange: bytes=0-3\r\n\r\n",
        );
        drop(clients);
        server.stop();
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(
            headers.starts_with("HTTP/1.1 206 Partial Content"),
            "{headers}"
        );
        assert_eq!(body.as_bytes(), &[0; 4]);
    }

    #[test]
    fn server_admission_audio_burst_waits_for_a_released_slot() {
        let (_directory, mut server, address, mut clients, _) =
            occupy_media_workers(LanShareKind::Podcast);
        let request = thread::spawn(move || {
            admission_request(
                address,
                "GET /media/0/episode.opus HTTP/1.1\r\nHost: fixture\r\nRange: bytes=7-10\r\n\r\n",
            )
        });
        thread::sleep(Duration::from_millis(300));
        drop(clients.pop());
        let response = request.join().unwrap();
        drop(clients);
        server.stop();
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(
            headers.starts_with("HTTP/1.1 206 Partial Content"),
            "{headers}"
        );
        assert!(headers.contains("Content-Range: bytes 7-10/67108864"));
        assert_eq!(body.as_bytes(), &[0; 4]);
    }

    #[test]
    fn server_admission_responds_when_all_media_workers_are_busy() {
        let (_directory, mut server, address, clients, _) =
            occupy_media_workers(LanShareKind::Files);
        let mut outcomes = Vec::new();
        for method in ["GET", "HEAD", "GET", "HEAD"] {
            let mut extra = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
            extra
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            extra
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            extra
                .write_all(
                    format!("{method} /media/0/episode.opus HTTP/1.1\r\nHost: fixture\r\n\r\n")
                        .as_bytes(),
                )
                .unwrap();
            let mut response = String::new();
            let read = extra.read_to_string(&mut response);
            outcomes.push((method, response, read));
        }
        drop(clients);
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut recovered = false;
        while Instant::now() < deadline {
            let response = admission_request(
                address,
                "HEAD /media/0/episode.opus HTTP/1.1\r\nHost: fixture\r\n\r\n",
            );
            if response.starts_with("HTTP/1.1 200 OK") {
                recovered = true;
                break;
            }
            thread::sleep(IO_POLL);
        }
        server.stop();
        for (method, response, read) in outcomes {
            read.expect("busy listener must accept and answer instead of leaving the request in its backlog");
            let (headers, body) = response.split_once("\r\n\r\n").unwrap();
            assert!(
                headers.starts_with("HTTP/1.1 503 Service Unavailable"),
                "{response}"
            );
            assert!(headers.contains("Retry-After: 1\r\n"));
            assert!(headers.contains("Connection: close"));
            assert!(headers.contains(&format!(
                "Content-Length: {}\r\n",
                "Youta is busy; retry shortly".len()
            )));
            assert_eq!(
                body,
                if method == "HEAD" {
                    ""
                } else {
                    "Youta is busy; retry shortly"
                }
            );
        }
        assert!(
            recovered,
            "normal admission must resume after busy workers exit"
        );
    }

    #[test]
    fn server_admission_serves_podcast_feed_while_all_download_workers_are_busy() {
        let (_directory, mut server, address, mut clients, expected_feed) =
            occupy_media_workers(LanShareKind::Podcast);
        // Partial headers occupy the remaining contexts without spending any
        // media or artwork permits; wrong-route503 below confirms overflow.
        for _ in MAX_MEDIA_CONNECTIONS..MAX_CONCURRENT_CONNECTIONS {
            let mut client = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
            client
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            client
                .write_all(b"GET /feed.xml HTTP/1.1\r\nHost: incomplete")
                .unwrap();
            clients.push(client);
        }
        thread::sleep(IO_POLL * 2);
        let mut responses = Vec::new();
        for (method, path) in [
            ("GET", "/feed.xml"),
            ("HEAD", "/feed.xml"),
            ("GET", "/feed.xml?refresh=1"),
            ("HEAD", "/feed.xml?refresh=2"),
        ] {
            responses.push((
                method,
                admission_request(
                    address,
                    &format!("{method} {path} HTTP/1.1\r\nHost: fixture\r\n\r\n"),
                ),
            ));
        }
        let busy_media = admission_request(
            address,
            "GET /media/0/episode.opus HTTP/1.1\r\nHost: fixture\r\n\r\n",
        );
        let wrong_route = admission_request(
            address,
            "GET /feed.xml/extra HTTP/1.1\r\nHost: fixture\r\n\r\n",
        );
        let post_feed = admission_request(
            address,
            "POST /feed.xml HTTP/1.1\r\nHost: fixture\r\nContent-Length: 0\r\n\r\n",
        );
        let invalid_feed =
            admission_request(address, "GET /feed.xml INVALID\r\nHost: fixture\r\n\r\n");
        let busy_artwork =
            admission_request(address, "GET /artwork/0 HTTP/1.1\r\nHost: fixture\r\n\r\n");
        drop(clients);
        let stopped_at = Instant::now();
        server.stop();
        assert!(stopped_at.elapsed() < Duration::from_secs(2));
        assert!(expected_feed.contains("<rss "));
        for (method, response) in responses {
            let (headers, body) = response.split_once("\r\n\r\n").unwrap();
            assert!(headers.starts_with("HTTP/1.1 200 OK"), "{response}");
            assert!(headers.contains("Content-Type: application/rss+xml; charset=utf-8\r\n"));
            assert!(headers.contains(&format!("Content-Length: {}\r\n", expected_feed.len())));
            assert!(!headers.contains("Retry-After:"));
            assert_eq!(
                body,
                if method == "HEAD" {
                    ""
                } else {
                    expected_feed.as_str()
                }
            );
        }
        for response in [
            busy_media,
            wrong_route,
            post_feed,
            invalid_feed,
            busy_artwork,
        ] {
            assert!(
                response.starts_with("HTTP/1.1 503 Service Unavailable"),
                "{response}"
            );
            assert!(response.contains("Retry-After: 1\r\n"));
        }
    }

    #[test]
    fn server_admission_feed_body_outlasts_its_short_header_deadline() {
        let mut state = admission_test_state(LanShareKind::Podcast);
        state.title = "x".repeat(16 * 1024 * 1024);
        let expected_feed = state.rss();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_overloaded_connection(stream, &state, &AtomicBool::new(false))
        });
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut client = BufReader::new(stream);
        client
            .get_mut()
            .write_all(
                b"GET /feed.xml?refresh=1 HTTP/1.1\r\nHost: fixture\r\nRange: bytes=0-0\r\n\r\n",
            )
            .unwrap();
        let mut headers = String::new();
        loop {
            let mut line = String::new();
            assert!(client.read_line(&mut line).unwrap() > 0);
            headers.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        thread::sleep(OVERLOAD_RESPONSE_TIMEOUT + Duration::from_millis(200));
        let mut body = String::new();
        let read = client.read_to_string(&mut body);
        drop(client);
        worker
            .join()
            .unwrap()
            .expect("feed content keeps its separate normal deadline");
        read.expect("complete paused feed response");
        assert!(headers.starts_with("HTTP/1.1 200 OK"));
        assert!(headers.contains(&format!("Content-Length: {}\r\n", expected_feed.len())));
        assert_eq!(body.len(), expected_feed.len());
        assert!(
            body == expected_feed,
            "paused feed bytes must match the complete manifest"
        );
    }

    #[test]
    fn server_admission_overload_headers_have_a_deadline_and_cancel_on_shutdown() {
        for cancelled in [false, true] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let worker_stop = Arc::clone(&stop);
            let (ready, accepted) = std::sync::mpsc::channel();
            let worker = thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                ready.send(()).unwrap();
                handle_overloaded_connection(
                    stream,
                    &admission_test_state(LanShareKind::Files),
                    &worker_stop,
                )
            });
            let mut client = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            client
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            client
                .write_all(b"GET /feed.xml HTTP/1.1\r\nHost: incomplete")
                .unwrap();
            accepted.recv_timeout(Duration::from_secs(2)).unwrap();
            let started = Instant::now();
            if cancelled {
                stop.store(true, Ordering::Release);
            }
            let mut response = String::new();
            let _ = client.read_to_string(&mut response);
            let error = worker
                .join()
                .unwrap()
                .expect_err("incomplete overload headers must not hold a worker forever");
            assert_eq!(
                error.kind(),
                if cancelled {
                    io::ErrorKind::Interrupted
                } else {
                    io::ErrorKind::TimedOut
                }
            );
            assert!(started.elapsed() < Duration::from_secs(2));
            assert!(response.is_empty());
        }
    }

    #[test]
    fn server_admission_overload_rejects_oversized_headers() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_overloaded_connection(
                stream,
                &admission_test_state(LanShareKind::Files),
                &AtomicBool::new(false),
            )
        });
        let mut client = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let request = format!(
            "GET /feed.xml HTTP/1.1\r\nX-Large: {}\r\n\r\n",
            "x".repeat(MAX_REQUEST_HEADER_BYTES)
        );
        let _ = client.write_all(request.as_bytes());
        let mut response = String::new();
        let _ = client.read_to_string(&mut response);
        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(response.is_empty());
    }

    #[test]
    fn server_accepts_slow_and_fragmented_request_headers() {
        let directory = canonical_tempdir("lan-slow-request");
        fs::write(directory.path().join("episode.opus"), b"audio").expect("audio fixture");
        let prepared = prepare_podcast_share(directory.path(), &directory.path().join("artwork"))
            .expect("podcast fixture");
        let state = Arc::new(ServerState::new(prepared, "http://127.0.0.1".to_owned()));
        for (first, remaining) in [
            ("", "GET /feed.xml HTTP/1.1\r\nHost: test\r\n\r\n"),
            ("GET /feed", ".xml HTTP/1.1\r\nHost: test\r\n\r\n"),
            ("GET /feed.xml HTTP/1.1\r\nHost: te", "st\r\n\r\n"),
        ] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("test listener");
            let address = listener.local_addr().expect("test address");
            let connection_state = Arc::clone(&state);
            let (ready, accepted) = std::sync::mpsc::channel();
            let worker = thread::spawn(move || {
                let (stream, _) = listener.accept().expect("accept test request");
                ready.send(()).expect("signal accepted request");
                handle_connection(stream, &connection_state, &AtomicBool::new(false))
            });
            let mut client = TcpStream::connect(address).expect("connect test request");
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("bounded read");
            accepted
                .recv_timeout(Duration::from_secs(5))
                .expect("accepted request");
            client
                .write_all(first.as_bytes())
                .expect("initial request fragment");
            thread::sleep(Duration::from_millis(350));
            let write_result = client.write_all(remaining.as_bytes());
            let mut response = String::new();
            let read_result = client.read_to_string(&mut response);
            let handled = worker.join().expect("join request worker");
            assert!(
                handled.is_ok(),
                "request with prefix {first:?} failed: {handled:?}"
            );
            write_result.expect("remaining request fragment");
            read_result.expect("complete feed response");
            assert!(response.starts_with("HTTP/1.1 200 OK"));
            assert!(response.contains("<rss "));
            assert!(response.ends_with("</rss>\n"));
        }
    }

    #[test]
    fn server_does_not_truncate_feed_when_client_temporarily_stops_reading() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("test listener");
        let address = listener.local_addr().expect("test address");
        let body = format!(
            "<?xml version=\"1.0\"?><rss version=\"2.0\"><channel><description>{}</description></channel></rss>",
            "x".repeat(16 * 1024 * 1024),
        );
        let expected_length = body.len();
        let (ready, accepted) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept feed client");
            stream
                .set_write_timeout(Some(IO_POLL))
                .expect("short cancellation poll");
            ready.send(()).expect("signal response start");
            write_content_response(
                &mut stream,
                "application/rss+xml",
                body.as_bytes(),
                false,
                &AtomicBool::new(false),
            )
        });
        let mut client = TcpStream::connect(address).expect("connect feed client");
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("bounded read");
        accepted
            .recv_timeout(Duration::from_secs(5))
            .expect("response start");
        thread::sleep(Duration::from_millis(350));
        let mut response = Vec::new();
        let read_result = client.read_to_end(&mut response);
        let written = worker.join().expect("join response worker");
        assert!(
            written.is_ok(),
            "feed was truncated by client backpressure: {written:?}"
        );
        read_result.expect("read complete feed");
        let header_end = response
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .expect("response headers")
            + 4;
        assert_eq!(response.len() - header_end, expected_length);
        assert!(
            String::from_utf8_lossy(&response[..header_end])
                .contains(&format!("Content-Length: {expected_length}\r\n"))
        );
        assert!(response.ends_with(b"</rss>"));
    }

    #[test]
    fn server_does_not_truncate_media_when_client_temporarily_stops_reading() {
        let directory = canonical_tempdir("lan-slow-client");
        let audio = directory.path().join("episode.opus");
        let audio_length = 16 * 1024 * 1024;
        File::create(&audio)
            .and_then(|file| file.set_len(audio_length))
            .expect("create large sparse audio fixture");
        let mut server = LanShareServer::start(prepare_file_share(&audio).expect("prepare file"))
            .expect("start server");
        let address = server
            .url()
            .strip_prefix("http://")
            .and_then(|url| url.split('/').next())
            .expect("server authority");
        let mut stream = TcpStream::connect(address).expect("connect to LAN server");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("bound regression-test read");
        stream
            .write_all(b"GET /media/0/episode.opus HTTP/1.1\r\nHost: test\r\n\r\n")
            .expect("request media");

        // Podcast clients can briefly stop draining their socket while moving
        // a download between buffers or storage. The server must retain the
        // connection across a pause longer than its listener poll interval.
        thread::sleep(Duration::from_millis(350));
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .expect("read complete media response");
        let header_end = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
            .expect("HTTP response headers");

        assert_eq!(
            response.len() - header_end,
            usize::try_from(audio_length).expect("fixture length fits usize")
        );
        server.stop();
    }

    #[test]
    fn youtube_proxy_initial_retry_is_bounded_and_cancellable() {
        let cancellation = YouTubePrewarmCancellation::new();
        let mut attempts = Vec::new();
        let result: Result<(), _> =
            retry_initial_remote_request(&AtomicBool::new(false), &cancellation, |retry| {
                attempts.push(retry);
                Err(RemoteRequestFailure::HttpStatus(503))
            });
        assert_eq!(result, Err(RemoteRequestFailure::HttpStatus(503)));
        assert_eq!(attempts, [false, true]);

        let (started, wait_for_attempt) = std::sync::mpsc::channel();
        let worker_cancellation = cancellation.clone();
        let canceller = thread::spawn(move || {
            wait_for_attempt
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            thread::sleep(Duration::from_millis(50));
            worker_cancellation.cancel();
        });
        let mut attempts = 0;
        let result: Result<(), _> =
            retry_initial_remote_request(&AtomicBool::new(false), &cancellation, |_| {
                attempts += 1;
                started.send(()).unwrap();
                Err(RemoteRequestFailure::Transport)
            });
        canceller.join().unwrap();
        assert_eq!(result, Err(RemoteRequestFailure::Cancelled));
        assert_eq!(
            attempts, 1,
            "cancellation during backoff must prevent the retry"
        );
        let result: Result<(), _> = retry_initial_remote_request(
            &AtomicBool::new(true),
            &YouTubePrewarmCancellation::new(),
            |_| panic!("stopped LAN server must not send a request"),
        );
        assert_eq!(result, Err(RemoteRequestFailure::Cancelled));
    }

    #[test]
    fn youtube_proxy_errors_never_expose_transport_secrets() {
        for error in [
            ureq::Error::BadUri("https://cdn.test/audio?secret=fixture-token".to_owned()),
            ureq::Error::Io(io::Error::other("Cookie: fixture-token")),
            ureq::Error::StatusCode(403),
        ] {
            let message = RemoteRequestFailure::from_error(&error).message();
            assert!(!message.contains("fixture-token"));
            assert!(!message.contains("cdn.test"));
            assert!(!message.contains("Cookie"));
            assert!(message.len() < 100);
        }
    }

    #[test]
    fn youtube_proxy_refreshes_rejected_resolutions_at_most_once() {
        for status in [403, 410] {
            let mut calls = Vec::new();
            let result = request_with_resolution_refresh(|refresh| {
                calls.push(refresh);
                if refresh {
                    Ok(17)
                } else {
                    Err(RemoteRequestFailure::HttpStatus(status))
                }
            });
            assert_eq!(result, Ok(17));
            assert_eq!(calls, [false, true]);
            let mut calls = 0;
            let result: Result<(), _> = request_with_resolution_refresh(|_| {
                calls += 1;
                Err(RemoteRequestFailure::HttpStatus(status))
            });
            assert_eq!(result, Err(RemoteRequestFailure::HttpStatus(status)));
            assert_eq!(calls, 2);
        }
        for failure in [
            RemoteRequestFailure::HttpStatus(404),
            RemoteRequestFailure::HttpStatus(429),
            RemoteRequestFailure::DeferredHttpStatus(403),
            RemoteRequestFailure::Cancelled,
        ] {
            let mut calls = 0;
            let result: Result<(), _> = request_with_resolution_refresh(|_| {
                calls += 1;
                Err(failure)
            });
            assert_eq!(result, Err(failure));
            assert_eq!(calls, 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn youtube_proxy_checks_formats_only_on_rejected_url_refresh() {
        use std::os::unix::fs::PermissionsExt;
        let directory = canonical_tempdir("lan-checked-resolution");
        let helper = directory.path().join("yt-dlp");
        fs::write(
            &helper,
            r#"#!/bin/sh
checked=false
for argument in "$@"; do
    if [ "$argument" = '--check-formats' ]; then checked=true; fi
done
printf '%s\n' "$checked" >> "$0.calls"
printf '{"url":"https://cdn.example.test/%s.webm","acodec":"opus","vcodec":"none"}\n' "$checked"
"#,
        )
        .unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let calls = helper.with_extension("calls");
        let state = ServerState {
            kind: LanShareKind::Podcast,
            title: "Fixture".to_owned(),
            feed_artwork_route: None,
            base_url: String::new(),
            files: Vec::new(),
            artwork: Vec::new(),
            remote: Some(RemoteRuntime {
                resolver: YouTubePrewarmResolver::new(YouTubePrewarmConfig {
                    executable: helper,
                    timeout: Duration::from_secs(2),
                    ..YouTubePrewarmConfig::default()
                }),
                cancellation: YouTubePrewarmCancellation::new(),
                cache: Mutex::new(HashMap::new()),
                agent: remote_agent(),
            }),
        };
        let source = Url::parse("https://www.youtube.com/watch?v=jNQXAC9IVRw").unwrap();
        let initial = state.resolve_youtube_audio(0, &source, false).unwrap();
        assert_eq!(initial.media_url().path(), "/false.webm");
        let checked = request_with_resolution_refresh(|refresh| {
            // Keep the unverified cache entry to model a concurrent request
            // inserting its ordinary resolution just before the refresh.
            let audio = state.resolve_youtube_audio(0, &source, refresh).unwrap();
            if refresh {
                Ok(audio)
            } else {
                Err(RemoteRequestFailure::HttpStatus(403))
            }
        })
        .unwrap();
        assert_eq!(checked.media_url().path(), "/true.webm");
        let cached = state.resolve_youtube_audio(0, &source, false).unwrap();
        assert_eq!(cached.media_url(), checked.media_url());
        assert_eq!(fs::read_to_string(calls).unwrap(), "false\ntrue\n");
    }

    #[cfg(unix)]
    #[test]
    fn youtube_proxy_invalidates_only_the_rejected_cached_url() {
        use std::os::unix::fs::PermissionsExt;
        let directory = canonical_tempdir("lan-rejected-resolution");
        let helper = directory.path().join("yt-dlp");
        fs::write(&helper, "#!/bin/sh\nprintf '%s\\n' '{\"url\":\"https://cdn.example.test/audio.webm\",\"acodec\":\"opus\",\"vcodec\":\"none\"}'\n").unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
        let resolver = YouTubePrewarmResolver::new(YouTubePrewarmConfig {
            executable: helper,
            timeout: Duration::from_secs(2),
            ..YouTubePrewarmConfig::default()
        });
        let audio = resolver
            .resolve(
                YouTubePrewarmRequest::new(
                    0,
                    Url::parse("https://www.youtube.com/watch?v=jNQXAC9IVRw").unwrap(),
                ),
                &YouTubePrewarmCancellation::new(),
            )
            .into_outcome()
            .expect("offline resolver fixture");
        let rejected = audio.media_url().clone();
        let remote = RemoteRuntime {
            resolver,
            cancellation: YouTubePrewarmCancellation::new(),
            cache: Mutex::new(HashMap::from([(
                0,
                CachedYouTubeResolution {
                    resolved_at: Instant::now(),
                    audio,
                },
            )])),
            agent: remote_agent(),
        };
        let state = ServerState {
            kind: LanShareKind::Podcast,
            title: "Fixture".to_owned(),
            feed_artwork_route: None,
            base_url: String::new(),
            files: Vec::new(),
            artwork: Vec::new(),
            remote: Some(remote),
        };
        state
            .invalidate_youtube_resolution(
                0,
                &Url::parse("https://cdn.example.test/old.webm").unwrap(),
            )
            .expect("a newer parallel cache entry survives");
        assert_eq!(
            state.remote.as_ref().unwrap().cache.lock().unwrap().len(),
            1
        );
        state
            .invalidate_youtube_resolution(0, &rejected)
            .expect("evict rejected URL");
        assert!(
            state
                .remote
                .as_ref()
                .unwrap()
                .cache
                .lock()
                .unwrap()
                .is_empty()
        );
    }

    /// Serves bounded mock CDN replies and captures only fixture requests.
    fn mock_initial_proxy(responses: Vec<String>) -> (String, Vec<String>) {
        let (response, requests, handled) = mock_proxy_responses(responses, None);
        handled.expect("proxy response");
        (response, requests)
    }

    /// Optionally exercises full-entity range assembly with tiny test chunks.
    fn mock_proxy_responses(
        responses: Vec<String>,
        chunked: Option<(u64, bool)>,
    ) -> (String, Vec<String>, io::Result<()>) {
        mock_proxy_responses_from(responses, chunked, None)
    }

    /// Exercises a client's open-ended resume request against the same fixture.
    fn mock_proxy_responses_from(
        responses: Vec<String>,
        chunked: Option<(u64, bool)>,
        resume_from: Option<u64>,
    ) -> (String, Vec<String>, io::Result<()>) {
        let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("mock CDN");
        upstream.set_nonblocking(true).expect("nonblocking CDN");
        let upstream_url = Url::parse(&format!(
            "http://{}/audio.webm?secret=fixture-token",
            upstream.local_addr().unwrap(),
        ))
        .unwrap();
        let upstream_stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&upstream_stop);
        let upstream_worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut requests = Vec::new();
            while !worker_stop.load(Ordering::Acquire) && Instant::now() < deadline {
                let Ok((mut stream, _)) = upstream.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                // Accepted Winsock streams retain the listener's mode.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    let read = reader.read_line(&mut line).unwrap();
                    request.push_str(&line);
                    if read == 0 || line == "\r\n" {
                        break;
                    }
                }
                let response = responses
                    .get(requests.len())
                    .unwrap_or_else(|| responses.last().unwrap());
                requests.push(request);
                let _ = stream.write_all(response.as_bytes());
            }
            requests
        });
        let state = ServerState {
            kind: LanShareKind::Podcast,
            title: "Fixture".to_owned(),
            feed_artwork_route: None,
            base_url: "http://127.0.0.1".to_owned(),
            files: Vec::new(),
            artwork: Vec::new(),
            remote: Some(RemoteRuntime {
                resolver: YouTubePrewarmResolver::new(YouTubePrewarmConfig::default()),
                cancellation: YouTubePrewarmCancellation::new(),
                cache: Mutex::new(HashMap::new()),
                agent: ureq::Agent::config_builder()
                    .https_only(false)
                    .timeout_global(Some(Duration::from_secs(2)))
                    .build()
                    .into(),
            }),
        };
        let downstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = downstream.local_addr().unwrap();
        let proxy = thread::spawn(move || {
            let (mut stream, _) = downstream.accept().unwrap();
            if let Some((chunk_size, head)) = chunked {
                let remote = state.remote.as_ref().unwrap();
                let plan = resume_from.map_or(YouTubeChunkPlan::Full, YouTubeChunkPlan::From);
                let range = plan.initial_range(chunk_size);
                let headers = [
                    ("User-Agent", "fixture-client"),
                    ("Referer", "https://www.youtube.com/"),
                ];
                let response = request_initial_remote_response(
                    remote,
                    &upstream_url,
                    &headers,
                    Some(&range),
                    &AtomicBool::new(false),
                )
                .unwrap();
                return write_chunked_youtube_response(
                    &mut stream,
                    remote,
                    &upstream_url,
                    &headers,
                    response,
                    head,
                    &AtomicBool::new(false),
                    plan,
                    chunk_size,
                );
            }
            proxy_remote_response(
                &mut stream,
                &state,
                &upstream_url,
                &[("User-Agent", "fixture-client")],
                Some("bytes=2-5"),
                false,
                &AtomicBool::new(false),
                "audio/webm",
            )
        });
        let mut client = TcpStream::connect(address).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut response = String::new();
        let read_result = client.read_to_string(&mut response);
        let proxy_result = proxy.join().unwrap();
        upstream_stop.store(true, Ordering::Release);
        let requests = upstream_worker.join().unwrap();
        read_result.expect("bounded proxy read");
        (response, requests, proxy_result)
    }

    /// Builds an exact fixture range without involving remote media metadata.
    fn mock_chunk_response(start: u64, end: u64, total: u64, body: &str) -> String {
        format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/webm\r\nContent-Length: {}\r\nContent-Range: bytes {start}-{end}/{total}\r\nConnection: close\r\n\r\n{body}",
            end - start + 1
        )
    }

    #[test]
    fn youtube_proxy_open_ended_range_assembles_zero_and_nonzero_offsets() {
        let body = "abcdefghijklmnopqrstuvwxyz0123456789";
        for start in [0_usize, 2] {
            let responses = body.as_bytes()[start..]
                .chunks(4)
                .enumerate()
                .map(|(index, bytes)| {
                    let offset = start as u64 + index as u64 * 4;
                    mock_chunk_response(
                        offset,
                        offset + bytes.len() as u64 - 1,
                        body.len() as u64,
                        std::str::from_utf8(bytes).unwrap(),
                    )
                })
                .collect();
            let (response, requests, handled) =
                mock_proxy_responses_from(responses, Some((4, false)), Some(start as u64));
            handled.unwrap();
            let (headers, downloaded) = response.split_once("\r\n\r\n").unwrap();
            assert!(headers.starts_with("HTTP/1.1 206 Partial Content"));
            assert!(
                headers.contains(&format!("Content-Length: {}\r\n", body.len() - start)),
                "{response}"
            );
            assert!(headers.contains(&format!(
                "Content-Range: bytes {start}-{}/{}\r\n",
                body.len() - 1,
                body.len()
            )));
            assert_eq!(downloaded, &body[start..]);
            assert_eq!(requests.len(), (body.len() - start).div_ceil(4));
            for (index, request) in requests.iter().enumerate() {
                let offset = start + index * 4;
                assert!(request.to_ascii_lowercase().contains(&format!(
                    "range: bytes={offset}-{}\r\n",
                    (offset + 3).min(body.len() - 1)
                )));
            }
        }
    }

    #[test]
    fn youtube_proxy_open_ended_range_plan_leaves_other_ranges_unchanged() {
        assert_eq!(
            YouTubeChunkPlan::for_range(None),
            Some(YouTubeChunkPlan::Full)
        );
        for (range, offset) in [("bytes=0-", 0), ("bytes=2-", 2), ("bytes=0002-", 2)] {
            assert_eq!(
                YouTubeChunkPlan::for_range(Some(range)),
                Some(YouTubeChunkPlan::From(offset))
            );
        }
        for range in [
            "bytes=0-3",
            "bytes=-4",
            "bytes=0-,4-",
            "bytes=",
            "bytes=-",
            "bytes=+2-",
            "bytes= 2-",
            "items=2-",
            "bytes=18446744073709551616-",
        ] {
            assert_eq!(YouTubeChunkPlan::for_range(Some(range)), None, "{range}");
        }
        assert_eq!(
            YouTubeChunkPlan::From(u64::MAX - 1).initial_range(4),
            format!("bytes={}-{}", u64::MAX - 1, u64::MAX)
        );
    }

    #[test]
    fn youtube_proxy_open_ended_range_handles_a_short_tail_and_head() {
        for head in [false, true] {
            let (response, requests, handled) = mock_proxy_responses_from(
                vec![mock_chunk_response(8, 9, 10, "ij")],
                Some((4, head)),
                Some(8),
            );
            handled.unwrap();
            let (headers, body) = response.split_once("\r\n\r\n").unwrap();
            assert!(headers.starts_with("HTTP/1.1 206 Partial Content"));
            assert!(headers.contains("Content-Length: 2\r\n"));
            assert!(headers.contains("Content-Range: bytes 8-9/10\r\n"));
            assert_eq!(body, if head { "" } else { "ij" });
            assert_eq!(requests.len(), 1);
            assert!(
                requests[0]
                    .to_ascii_lowercase()
                    .contains("range: bytes=8-11\r\n")
            );
        }
    }

    #[test]
    fn youtube_proxy_open_ended_range_preserves_whole_response_when_ignored() {
        for head in [false, true] {
            let (response, requests, handled) = mock_proxy_responses_from(
                vec![
                    "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabcdefghij"
                        .to_owned(),
                ],
                Some((4, head)),
                Some(2),
            );
            handled.unwrap();
            let (headers, body) = response.split_once("\r\n\r\n").unwrap();
            assert!(headers.starts_with("HTTP/1.1 200 OK"));
            assert!(headers.contains("Content-Length: 10\r\n"));
            assert!(!headers.contains("Content-Range:"));
            assert_eq!(body, if head { "" } else { "abcdefghij" });
            assert_eq!(requests.len(), 1);
        }
    }

    #[test]
    fn youtube_proxy_open_ended_range_resumes_inside_the_first_chunk() {
        let (response, requests, handled) = mock_proxy_responses_from(
            vec![
                mock_chunk_response(2, 5, 10, "cd"),
                mock_chunk_response(4, 5, 10, "ef"),
                mock_chunk_response(6, 9, 10, "ghij"),
            ],
            Some((4, false)),
            Some(2),
        );
        handled.unwrap();
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(headers.contains("Content-Length: 8\r\n"));
        assert!(headers.contains("Content-Range: bytes 2-9/10\r\n"));
        assert_eq!(body, "cdefghij");
        assert_eq!(requests.len(), 3);
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("range: bytes=4-5\r\n")
        );
        for request in requests {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("user-agent: fixture-client\r\n")
            );
        }
    }

    #[test]
    fn youtube_proxy_open_ended_range_rejects_the_wrong_first_offset() {
        let (response, requests, handled) = mock_proxy_responses_from(
            vec![mock_chunk_response(0, 3, 10, "BAD!")],
            Some((4, false)),
            Some(2),
        );
        handled.unwrap();
        assert!(response.starts_with("HTTP/1.1 502 Bad Gateway"));
        assert!(!response.contains("BAD!"));
        assert_eq!(requests.len(), 1);
    }

    #[test]
    fn youtube_proxy_full_audio_assembles_more_than_five_chunks() {
        let body = "abcdefghijklmnopqrstuvwxyz";
        let responses = body
            .as_bytes()
            .chunks(4)
            .enumerate()
            .map(|(index, bytes)| {
                let start = index as u64 * 4;
                mock_chunk_response(
                    start,
                    start + bytes.len() as u64 - 1,
                    26,
                    std::str::from_utf8(bytes).unwrap(),
                )
            })
            .collect();
        let (response, requests, handled) = mock_proxy_responses(responses, Some((4, false)));
        handled.unwrap();
        let (headers, actual_body) = response.split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(headers.contains("Content-Length: 26"));
        assert!(!headers.contains("Content-Range:"));
        assert_eq!(actual_body, body);
        assert_eq!(
            requests.len(),
            7,
            "ordinary chunks do not spend the resume budget"
        );
        for (index, request) in requests.iter().enumerate() {
            let request = request.to_ascii_lowercase();
            assert!(request.contains("user-agent: fixture-client\r\n"));
            assert!(request.contains("referer: https://www.youtube.com/\r\n"));
            let end = (index as u64 * 4 + 3).min(25);
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains(&format!("range: bytes={}-{end}\r\n", index * 4))
            );
        }
    }

    #[test]
    fn youtube_proxy_full_audio_resumes_within_one_chunk() {
        let (response, requests, handled) = mock_proxy_responses(
            vec![
                mock_chunk_response(0, 3, 10, "ab"),
                mock_chunk_response(2, 3, 10, "cd"),
                mock_chunk_response(4, 7, 10, "efgh"),
                mock_chunk_response(8, 9, 10, "ij"),
            ],
            Some((4, false)),
        );
        handled.unwrap();
        assert!(response.ends_with("\r\n\r\nabcdefghij"));
        assert_eq!(requests.len(), 4);
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("range: bytes=2-3\r\n")
        );
    }

    #[test]
    fn youtube_proxy_full_audio_does_not_append_later_http_errors() {
        for status in [403, 429, 503] {
            let (response, requests, handled) = mock_proxy_responses(
                vec![
                    mock_chunk_response(0, 3, 10, "abcd"),
                    format!(
                        "HTTP/1.1 {status} Error\r\nContent-Length: 0\r\nRetry-After: 3600\r\nConnection: close\r\n\r\n"
                    ),
                ],
                Some((4, false)),
            );
            assert!(handled.is_err());
            assert!(response.starts_with("HTTP/1.1 200 OK"));
            assert_eq!(response.matches("HTTP/1.1").count(), 1);
            assert!(response.ends_with("\r\n\r\nabcd"));
            assert_eq!(requests.len(), 2);
        }
    }

    #[test]
    fn youtube_proxy_full_audio_rejects_mismatched_next_chunks_before_appending() {
        for malformed in [
            mock_chunk_response(3, 7, 10, "WRONG"),
            mock_chunk_response(4, 6, 10, "BAD"),
            mock_chunk_response(4, 7, 11, "BAD!"),
            mock_chunk_response(4, 7, 10, "BAD!").replace("Content-Length: 4", "Content-Length: 3"),
            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nBAD!".to_owned(),
        ] {
            let (response, requests, handled) = mock_proxy_responses(
                vec![mock_chunk_response(0, 3, 10, "abcd"), malformed],
                Some((4, false)),
            );
            assert!(handled.is_err(), "mismatched next chunk must abort");
            assert!(response.ends_with("\r\n\r\nabcd"), "{response}");
            assert_eq!(requests.len(), 2);
        }
    }

    #[test]
    fn youtube_proxy_full_audio_validates_the_first_chunk_before_headers() {
        for malformed in [
            mock_chunk_response(1, 4, 10, "BAD!"),
            mock_chunk_response(0, 2, 10, "BAD"),
            mock_chunk_response(0, 3, 3, "BAD!"),
            mock_chunk_response(0, 3, 10, "BAD!").replace("/10", "/*"),
            mock_chunk_response(0, 3, 10, "BAD!").replace("Content-Length: 4", "Content-Length: 3"),
            mock_chunk_response(0, 3, 10, "BAD!").replace("Content-Length: 4\r\n", ""),
        ] {
            let (response, requests, handled) =
                mock_proxy_responses(vec![malformed], Some((4, false)));
            handled.unwrap();
            assert!(response.starts_with("HTTP/1.1 502 Bad Gateway"));
            assert!(!response.contains("BAD!"));
            assert_eq!(requests.len(), 1);
        }
    }

    #[test]
    fn youtube_proxy_full_audio_rejects_invalid_interrupted_chunk_resumes() {
        for malformed in [
            mock_chunk_response(2, 3, 11, "XX"),
            mock_chunk_response(2, 4, 10, "XXX"),
            mock_chunk_response(2, 3, 10, "XX").replace("Content-Length: 2", "Content-Length: 1"),
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nXX".to_owned(),
        ] {
            let (response, requests, handled) = mock_proxy_responses(
                vec![mock_chunk_response(0, 3, 10, "ab"), malformed],
                Some((4, false)),
            );
            assert!(handled.is_err());
            assert!(
                response.ends_with("\r\n\r\nab"),
                "never append mismatched resume bytes: {response}"
            );
            assert_eq!(requests.len(), MAX_REMOTE_RESUME_ATTEMPTS + 1);
        }
    }

    #[test]
    fn youtube_proxy_full_audio_handles_short_and_exact_chunk_boundaries() {
        for total in [1, 3, 4, 8] {
            let body = "abcdefgh"[..total].to_owned();
            let responses = body
                .as_bytes()
                .chunks(4)
                .enumerate()
                .map(|(index, bytes)| {
                    let start = index as u64 * 4;
                    mock_chunk_response(
                        start,
                        start + bytes.len() as u64 - 1,
                        total as u64,
                        std::str::from_utf8(bytes).unwrap(),
                    )
                })
                .collect();
            let (response, requests, handled) = mock_proxy_responses(responses, Some((4, false)));
            handled.unwrap();
            assert_eq!(response.split_once("\r\n\r\n").unwrap().1, body);
            assert!(response.contains(&format!("Content-Length: {total}\r\n")));
            assert_eq!(requests.len(), total.div_ceil(4));
        }
    }

    #[test]
    fn youtube_proxy_full_audio_head_reports_total_without_fetching_next_chunk() {
        let (response, requests, handled) =
            mock_proxy_responses(vec![mock_chunk_response(0, 3, 10, "abcd")], Some((4, true)));
        handled.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("Content-Length: 10\r\n"));
        assert!(!response.contains("Content-Range:"));
        assert!(response.ends_with("\r\n\r\n"));
        assert_eq!(requests.len(), 1);
    }

    #[test]
    fn youtube_proxy_full_audio_accepts_upstream_ignoring_initial_range() {
        let (response, requests, handled) = mock_proxy_responses(
            vec![
                "HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabcdefghij"
                    .to_owned(),
            ],
            Some((4, false)),
        );
        handled.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.ends_with("\r\n\r\nabcdefghij"));
        assert_eq!(requests.len(), 1);
    }

    #[test]
    fn youtube_proxy_retries_an_initial_transient_cdn_failure() {
        let (response, requests) = mock_initial_proxy(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
            "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/webm\r\nContent-Length: 4\r\nContent-Range: bytes 2-5/10\r\nConnection: close\r\n\r\n2345".to_owned(),
        ]);
        assert!(
            response.starts_with("HTTP/1.1 206 Partial Content"),
            "{response}"
        );
        assert_eq!(response.matches("HTTP/1.1").count(), 1);
        assert!(response.contains("Content-Length: 4\r\n"));
        assert!(response.contains("Content-Range: bytes 2-5/10\r\n"));
        assert!(response.ends_with("\r\n\r\n2345"));
        assert_eq!(requests.len(), 2);
        for request in requests {
            let request = request.to_ascii_lowercase();
            assert!(request.contains("range: bytes=2-5\r\n"));
            assert!(request.contains("user-agent: fixture-client\r\n"));
        }
    }

    #[test]
    fn youtube_proxy_initial_and_resumed_bodies_can_outlast_setup_timeout() {
        let phase_timeout = Duration::from_millis(300);
        let mut outcomes = Vec::new();
        for resumed in [false, true] {
            let (headers, body): (&[u8], &[u8]) = if resumed {
                (b"HTTP/1.1 206 Partial Content\r\nContent-Length: 6\r\nContent-Range: bytes 4-9/10\r\nConnection: close\r\n\r\n", b"456789")
            } else {
                (
                    b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\n",
                    b"012345",
                )
            };
            let (url, upstream) = mock_timed_remote_response(
                headers,
                body.iter()
                    .map(|byte| (Duration::from_millis(175), vec![*byte]))
                    .collect(),
            );
            let remote = timeout_test_remote(phase_timeout);
            let response = if resumed {
                request_remote_response(&remote, &url, &[], Some("bytes=4-9"))
                    .map_err(|error| RemoteRequestFailure::from_error(&error))
            } else {
                request_initial_remote_response(&remote, &url, &[], None, &AtomicBool::new(false))
            };
            let outcome =
                response
                    .map_err(|error| format!("{error:?}"))
                    .and_then(|mut response| {
                        response
                            .body_mut()
                            .read_to_string()
                            .map_err(|error| format!("{error:?}"))
                    });
            upstream.join().unwrap();
            outcomes.push((resumed, outcome, String::from_utf8(body.to_vec()).unwrap()));
        }
        for (resumed, outcome, expected) in &outcomes {
            assert_eq!(
                outcome.as_ref(),
                Ok(expected),
                "resumed={resumed}; all outcomes: {outcomes:?}"
            );
        }
    }

    #[test]
    fn youtube_proxy_timeouts_still_bound_stalled_headers_and_body() {
        let phase_timeout = Duration::from_millis(300);
        for stalled_body in [false, true] {
            let headers: &[u8] = if stalled_body {
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n0"
            } else {
                b""
            };
            let (url, upstream) = mock_timed_remote_response(
                headers,
                vec![(Duration::from_millis(800), b"1".to_vec())],
            );
            let remote = timeout_test_remote(phase_timeout);
            let started = Instant::now();
            let result = request_remote_response(&remote, &url, &[], None)
                .and_then(|mut response| response.body_mut().read_to_string());
            let elapsed = started.elapsed();
            upstream.join().unwrap();
            assert!(
                matches!(result, Err(ureq::Error::Timeout(_))),
                "stalled_body={stalled_body}: {result:?}"
            );
            assert!(
                elapsed < Duration::from_secs(2),
                "stalled operation was not bounded: {elapsed:?}"
            );
        }
    }

    /// Builds a loopback runtime using the exact production timeout policy.
    fn timeout_test_remote(phase_timeout: Duration) -> RemoteRuntime {
        RemoteRuntime {
            resolver: YouTubePrewarmResolver::new(YouTubePrewarmConfig::default()),
            cancellation: YouTubePrewarmCancellation::new(),
            cache: Mutex::new(HashMap::new()),
            agent: remote_agent_with_policy(phase_timeout, false),
        }
    }

    /// Sends one bounded mock response with controlled gaps between body bytes.
    fn mock_timed_remote_response(
        headers: &[u8],
        chunks: Vec<(Duration, Vec<u8>)>,
    ) -> (Url, JoinHandle<()>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = Url::parse(&format!(
            "http://{}/audio.webm",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let headers = headers.to_vec();
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut stream = loop {
                if let Ok((stream, _)) = listener.accept() {
                    break stream;
                }
                assert!(
                    Instant::now() < deadline,
                    "mock upstream connection timeout"
                );
                thread::sleep(Duration::from_millis(5));
            };
            // Accepted Winsock streams retain the listener's mode.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
            }
            let _ = stream.write_all(&headers);
            for (delay, bytes) in chunks {
                thread::sleep(delay);
                let _ = stream.write_all(&bytes);
            }
        });
        (url, worker)
    }

    #[test]
    fn youtube_proxy_retry_does_not_shorten_the_body_read_timeout() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = Url::parse(&format!(
            "http://{}/audio.webm",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let upstream = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut attempts = 0;
            while attempts < 2 && Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                // Accepted Winsock streams retain the listener's mode.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                attempts += 1;
                if attempts == 1 {
                    stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    stream.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 2-5/10\r\nConnection: close\r\n\r\n23").unwrap();
                    thread::sleep(REMOTE_RETRY_PHASE_TIMEOUT + Duration::from_millis(200));
                    let _ = stream.write_all(b"45");
                }
            }
            attempts
        });
        let remote = RemoteRuntime {
            resolver: YouTubePrewarmResolver::new(YouTubePrewarmConfig::default()),
            cancellation: YouTubePrewarmCancellation::new(),
            cache: Mutex::new(HashMap::new()),
            agent: ureq::Agent::config_builder()
                .https_only(false)
                .timeout_global(None)
                .timeout_recv_body(Some(Duration::from_secs(5)))
                .build()
                .into(),
        };
        let response = request_initial_remote_response(
            &remote,
            &url,
            &[],
            Some("bytes=2-5"),
            &AtomicBool::new(false),
        );
        let body = response.map(|mut response| response.body_mut().read_to_string());
        assert_eq!(upstream.join().unwrap(), 2);
        assert_eq!(
            body.expect("successful retry")
                .expect("body retains its longer read timeout"),
            "2345"
        );
    }

    #[test]
    fn youtube_proxy_defers_retry_after_and_preserves_safe_status() {
        let (response, requests) = mock_initial_proxy(vec![
            "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 3600\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
        ]);
        assert_eq!(requests.len(), 1, "do not retry before Retry-After");
        assert!(response.starts_with("HTTP/1.1 502 Bad Gateway"));
        assert!(response.ends_with("Remote media request failed: upstream HTTP 503"));
        assert!(!response.contains("fixture-token"));
    }

    #[test]
    fn youtube_proxy_does_not_retry_permanent_errors_or_rate_limits() {
        for status in [400, 401, 403, 404, 410, 429] {
            let (response, requests) = mock_initial_proxy(vec![format!(
                "HTTP/1.1 {status} Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )]);
            assert_eq!(requests.len(), 1, "upstream HTTP {status}");
            assert!(response.starts_with("HTTP/1.1 502 Bad Gateway"));
            assert!(response.ends_with(&format!(
                "Remote media request failed: upstream HTTP {status}"
            )));
            assert!(!response.contains("fixture-token"));
        }
    }

    #[test]
    fn youtube_proxy_resumes_an_incomplete_upstream_body() {
        let upstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind mock upstream");
        upstream
            .set_nonblocking(true)
            .expect("make mock upstream nonblocking");
        let upstream_url = Url::parse(&format!(
            "http://{}/audio.webm",
            upstream.local_addr().expect("mock upstream address")
        ))
        .expect("mock upstream URL");
        let upstream_thread = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut request_count = 0;
            while request_count < 2 && Instant::now() < deadline {
                let Ok((mut stream, _)) = upstream.accept() else {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                };
                // Keep inherited nonblocking mode out of this bounded fixture.
                stream
                    .set_nonblocking(false)
                    .expect("make mock request blocking");
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("bound mock request reads");
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .expect("bound mock response writes");
                let mut request = [0_u8; 2_048];
                let read = stream.read(&mut request).expect("read proxy request");
                let request = String::from_utf8_lossy(&request[..read]);
                if request_count == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: audio/webm\r\nAccept-Ranges: bytes\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123",
                        )
                        .expect("write truncated upstream response");
                } else {
                    assert!(request.to_ascii_lowercase().contains("range: bytes=4-9"));
                    stream
                        .write_all(
                            b"HTTP/1.1 206 Partial Content\r\nContent-Type: audio/webm\r\nAccept-Ranges: bytes\r\nContent-Range: bytes 4-9/10\r\nContent-Length: 6\r\nConnection: close\r\n\r\n456789",
                        )
                        .expect("write resumed upstream response");
                }
                request_count += 1;
            }
            request_count
        });

        let downstream = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind proxy client");
        let downstream_address = downstream.local_addr().expect("proxy client address");
        let state = ServerState {
            kind: LanShareKind::Podcast,
            title: "Fixture".to_owned(),
            feed_artwork_route: None,
            base_url: "http://127.0.0.1".to_owned(),
            files: Vec::new(),
            artwork: Vec::new(),
            remote: Some(RemoteRuntime {
                resolver: YouTubePrewarmResolver::new(YouTubePrewarmConfig::default()),
                cancellation: YouTubePrewarmCancellation::new(),
                cache: Mutex::new(HashMap::new()),
                agent: ureq::Agent::config_builder()
                    .https_only(false)
                    .build()
                    .into(),
            }),
        };
        let proxy_thread = thread::spawn(move || {
            let (mut stream, _) = downstream.accept().expect("accept proxy client");
            proxy_remote_response(
                &mut stream,
                &state,
                &upstream_url,
                &[],
                None,
                false,
                &AtomicBool::new(false),
                "audio/webm",
            )
        });
        let mut client = TcpStream::connect(downstream_address).expect("connect proxy client");
        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .expect("read proxy response");
        let proxy_result = proxy_thread.join().expect("join proxy thread");
        let upstream_requests = upstream_thread.join().expect("join mock upstream");
        let header_end = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
            .expect("proxy response headers");

        assert!(proxy_result.is_ok(), "proxy failed: {proxy_result:?}");
        assert_eq!(upstream_requests, 2);
        assert_eq!(&response[header_end..], b"0123456789");
    }

    #[test]
    fn dropping_server_signals_and_joins_its_listener_thread() {
        let directory = canonical_tempdir("lan-drop");
        let audio = directory.path().join("episode.opus");
        fs::write(&audio, b"audio").expect("write audio");
        let server = LanShareServer::start(prepare_file_share(&audio).expect("prepare file"))
            .expect("start server");
        let listener_stop = Arc::clone(&server.stop);
        drop(server);

        assert!(listener_stop.load(Ordering::Acquire));
        assert_eq!(Arc::strong_count(&listener_stop), 1);
    }

    fn request(address: &str, request: &str) -> String {
        let mut stream = TcpStream::connect(address).expect("connect to LAN server");
        stream.write_all(request.as_bytes()).expect("write request");
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("finish request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");
        response
    }
}
