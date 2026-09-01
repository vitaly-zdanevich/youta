//! Session-scoped HTTP sharing for local files and podcast feeds.
//!
//! The server exposes an immutable, bounded manifest instead of translating
//! request paths back into filesystem paths. That keeps URL traversal and
//! post-start symlink changes outside the trust boundary. [`LanShareServer`]
//! owns its listener thread and stops it on drop, so sharing never survives a
//! Youta process that the user has closed.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, UNIX_EPOCH};

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use sha2::{Digest, Sha256};
use url::Url;

use crate::local_browser::{LocalEntryKind, classify_local_file};
use crate::playback::youtube_prewarm::{
    PrewarmedYouTubeAudio, YouTubePrewarmCancellation, YouTubePrewarmConfig, YouTubePrewarmRequest,
    YouTubePrewarmResolver,
};
use crate::playback::ytdlp::ExtractedCollection;
use crate::providers::validate_youtube_video_id;

const MAX_SHARED_FILES: usize = 10_000;
const MAX_CONCURRENT_CONNECTIONS: usize = 8;
const MAX_SCAN_DEPTH: usize = 64;
const MAX_REQUEST_LINE_BYTES: usize = 16 * 1024;
const MAX_REQUEST_HEADER_BYTES: usize = 64 * 1024;
const IO_POLL: Duration = Duration::from_millis(100);
const REMOTE_SETUP_TIMEOUT: Duration = Duration::from_secs(20);
const REMOTE_RESOLUTION_CACHE_TTL: Duration = Duration::from_mins(30);
const YOUTUBE_PODCAST_AUDIO_FORMAT: &str = "bestaudio[ext=m4a]";

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
    files: Vec<SharedFile>,
    artwork: Vec<SharedArtwork>,
    remote_config: Option<YouTubePrewarmConfig>,
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
    prepare_local_share(target, None)
}

/// Builds a bounded podcast feed from playable files under one target.
///
/// Embedded artwork is extracted into `artwork_cache`; a valid sidecar image
/// remains the fallback used by Youta's normal local-artwork policy.
///
/// # Errors
///
/// Returns an error for unsafe targets, traversal failures, an empty playable
/// selection, or a directory that exceeds the traversal bounds.
pub fn prepare_podcast_share(
    target: &Path,
    artwork_cache: &Path,
) -> io::Result<PreparedLocalShare> {
    prepare_local_share(target, Some(artwork_cache))
}

fn prepare_local_share(
    target: &Path,
    artwork_cache: Option<&Path>,
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
    paths.sort_by(|left, right| left.1.to_lowercase().cmp(&right.1.to_lowercase()));
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
        files.push(SharedFile {
            guid: local_guid(&path, &metadata),
            label,
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
        files,
        artwork,
        remote_config: None,
    })
}

/// Builds a feed whose stable local enclosure routes resolve fresh YouTube
/// audio only when a podcast client requests an episode.
///
/// Flat extraction keeps feed creation fast: it downloads neither video nor
/// audio. The active LAN server supervises each later `yt-dlp` resolver and
/// proxies the resulting stream so required request headers never leave Youta.
///
/// # Errors
///
/// Returns an error when the collection is empty, exceeds the feed bound, or
/// contains no valid YouTube video identifiers.
pub fn prepare_youtube_podcast_share(
    collection: ExtractedCollection,
    mut config: YouTubePrewarmConfig,
) -> io::Result<PreparedLocalShare> {
    let title = if collection.title.trim().is_empty() {
        "YouTube channel".to_owned()
    } else {
        collection.title
    };
    let mut files = Vec::new();
    let mut artwork = Vec::new();
    for entry in collection.entries {
        if validate_youtube_video_id(&entry.id).is_err() {
            continue;
        }
        if files.len() >= MAX_SHARED_FILES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "YouTube channel exceeds Youta's podcast episode limit",
            ));
        }
        let source_url = Url::parse(&format!("https://www.youtube.com/watch?v={}", entry.id))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let index = files.len();
        let label = if entry.title.trim().is_empty() {
            entry.id.clone()
        } else {
            entry.title
        };
        let route = format!(
            "/media/{index}/{}.m4a",
            utf8_percent_encode(&entry.id, NON_ALPHANUMERIC)
        );
        let artwork_route = format!("/artwork/{index}");
        files.push(SharedFile {
            guid: format!("urn:youta:youtube:{}", entry.id),
            label,
            length: 0,
            mime: "audio/mp4",
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
    if files.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "yt-dlp found no valid YouTube videos for the podcast feed",
        ));
    }
    // A podcast enclosure must advertise a stable media type before the
    // signed stream exists, so request YouTube's podcast-compatible AAC/M4A
    // representation instead of allowing yt-dlp to choose another container.
    config.audio_format = YOUTUBE_PODCAST_AUDIO_FORMAT.to_owned();
    Ok(PreparedLocalShare {
        kind: LanShareKind::Podcast,
        title,
        files,
        artwork,
        remote_config: Some(config),
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

#[derive(Debug)]
struct SharedFile {
    guid: String,
    label: String,
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
            base_url,
            files: prepared.files,
            artwork: prepared.artwork,
            remote,
        }
    }

    fn rss(&self) -> String {
        let title = escape_xml(&self.title);
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
				format!(
					"<item>\n<title>{}</title>\n<description>{}</description>\n<guid isPermaLink=\"false\">{}</guid>\n<enclosure url=\"{}{}\" length=\"{}\" type=\"{}\"/>{episode_artwork}{duration}\n</item>",
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
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rss version=\"2.0\" xmlns:itunes=\"http://www.itunes.com/dtds/podcast-1.0.dtd\">\n<channel>\n<title>{title}</title>\n<link>{}</link>\n<description>{channel_description}</description>\n<language>und</language>\n{items}\n</channel>\n</rss>\n",
            escape_xml(&format!("{}/", self.base_url)),
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

fn serve(listener: TcpListener, state: Arc<ServerState>, stop: Arc<AtomicBool>) {
    let mut connections = Vec::<JoinHandle<()>>::new();
    let mut connection_id = 0_u64;
    while !stop.load(Ordering::Acquire) {
        reap_finished_connections(&mut connections);
        if connections.len() >= MAX_CONCURRENT_CONNECTIONS {
            thread::sleep(IO_POLL);
            continue;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let connection_state = Arc::clone(&state);
                let connection_stop = Arc::clone(&stop);
                let thread = thread::Builder::new()
                    .name(format!("youta-lan-connection-{connection_id}"))
                    .spawn(move || {
                        let _ = handle_connection(stream, &connection_state, &connection_stop);
                    });
                connection_id = connection_id.wrapping_add(1);
                if let Ok(thread) = thread {
                    connections.push(thread);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(IO_POLL);
            }
            Err(_) => break,
        }
    }
    for connection in connections {
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

fn handle_connection(
    mut stream: TcpStream,
    state: &ServerState,
    stop: &AtomicBool,
) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_POLL))?;
    stream.set_write_timeout(Some(IO_POLL))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader
        .by_ref()
        .take(u64::try_from(MAX_REQUEST_LINE_BYTES).unwrap_or(u64::MAX))
        .read_line(&mut request_line)?;
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
        let mut line = String::new();
        let remaining = MAX_REQUEST_HEADER_BYTES.saturating_sub(header_bytes);
        let read = reader
            .by_ref()
            .take(
                u64::try_from(remaining)
                    .unwrap_or(u64::MAX)
                    .saturating_add(1),
            )
            .read_line(&mut line)?;
        header_bytes = header_bytes.saturating_add(read);
        if header_bytes > MAX_REQUEST_HEADER_BYTES {
            return write_text_response(
                &mut stream,
                431,
                "Request Header Fields Too Large",
                "HTTP request headers exceeded Youta's limit",
                head,
            );
        }
        if read == 0 || line == "\r\n" || line == "\n" {
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
        );
    }
    if path == "/feed.xml" && state.kind == LanShareKind::Podcast {
        return write_content_response(
            &mut stream,
            "application/rss+xml; charset=utf-8",
            state.rss().as_bytes(),
            head,
        );
    }
    if let Some(index) = route_index(path, "/media/")
        && let Some(file) = state.files.get(index)
    {
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
    let resolved = match state.resolve_youtube_audio(index, source_url) {
        Ok(resolved) => resolved,
        Err(_) => {
            return write_text_response(
                stream,
                502,
                "Bad Gateway",
                "Could not resolve fresh YouTube audio",
                head,
            );
        }
    };
    let headers = resolved.http_headers().iter().collect::<Vec<_>>();
    proxy_remote_response(
        stream,
        state,
        resolved.media_url(),
        &headers,
        range,
        head,
        stop,
        "audio/mp4",
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
    fn resolve_youtube_audio(
        &self,
        index: usize,
        source_url: &Url,
    ) -> io::Result<PrewarmedYouTubeAudio> {
        let remote = self.remote.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "YouTube resolver is unavailable")
        })?;
        if let Some(cached) = remote
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
        let result = remote.resolver.resolve(
            YouTubePrewarmRequest::new(generation, source_url.clone()),
            &remote.cancellation,
        );
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
    let response = match request.call() {
        Ok(response) => response,
        Err(_) => {
            return write_text_response(
                stream,
                502,
                "Bad Gateway",
                "Remote media request failed",
                head,
            );
        }
    };
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
    if let Some(content_range) = content_range {
        write!(stream, "Content-Range: {content_range}\r\n")?;
    }
    write!(stream, "Connection: close\r\n\r\n")?;
    if head {
        return Ok(());
    }
    let (_, body) = response.into_parts();
    let mut reader = body.into_reader();
    let mut buffer = [0_u8; 64 * 1024];
    while !stop.load(Ordering::Acquire) {
        let read = reader.read(&mut buffer).map_err(|_| {
            io::Error::new(io::ErrorKind::ConnectionAborted, "remote media read failed")
        })?;
        if read == 0 {
            break;
        }
        stream.write_all(&buffer[..read])?;
    }
    Ok(())
}

fn proxy_request_header_allowed(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "user-agent" | "referer" | "origin" | "accept" | "accept-language" | "cookie"
    )
}

fn remote_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(None)
        .timeout_per_call(None)
        .timeout_resolve(Some(REMOTE_SETUP_TIMEOUT))
        .timeout_connect(Some(REMOTE_SETUP_TIMEOUT))
        .timeout_send_request(Some(REMOTE_SETUP_TIMEOUT))
        .timeout_recv_response(Some(REMOTE_SETUP_TIMEOUT))
        .timeout_recv_body(Some(REMOTE_SETUP_TIMEOUT))
        .https_only(true)
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

fn write_content_response(
    stream: &mut TcpStream,
    content_type: &str,
    body: &[u8],
    head: bool,
) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len(),
    )?;
    if !head {
        stream.write_all(body)?;
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
        stream.write_all(&buffer[..read])?;
        remaining = remaining.saturating_sub(read as u64);
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
    fn youtube_feed_uses_stable_proxy_routes_and_artwork_for_every_episode() {
        let collection = ExtractedCollection {
            id: "UCfixture".to_owned(),
            title: "Fixture channel".to_owned(),
            extractor: Some("YoutubeTab".to_owned()),
            entries: vec![
                crate::playback::ytdlp::CollectionEntry {
                    id: "dQw4w9WgXcQ".to_owned(),
                    title: "First & episode".to_owned(),
                    webpage_url: None,
                    duration_seconds: Some(42),
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
                    thumbnail_url: None,
                },
            ],
        };
        let prepared = prepare_youtube_podcast_share(collection, YouTubePrewarmConfig::default())
            .expect("prepare YouTube feed");
        assert_eq!(prepared.files.len(), 2);
        assert_eq!(prepared.artwork.len(), 2);
        assert!(prepared.files.iter().all(|file| file.length == 0));
        assert!(matches!(
            &prepared.artwork[1].source,
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
            Some(YOUTUBE_PODCAST_AUDIO_FORMAT)
        );
        let state = ServerState::new(prepared, "http://192.0.2.10:8123".to_owned());

        let rss = state.rss();
        assert!(rss.contains("<title>First &amp; episode</title>"));
        assert_eq!(rss.matches("<itunes:image href=").count(), 2);
        assert!(rss.contains("<itunes:duration>42</itunes:duration>"));
        assert!(rss.contains("length=\"0\" type=\"audio/mp4\""));
        assert!(rss.contains("http://192.0.2.10:8123/media/0/"));
        assert!(!rss.contains("googlevideo.com"));
        assert!(!rss.contains("i.ytimg.com"));
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
    fn dropping_server_closes_its_listener() {
        let directory = canonical_tempdir("lan-drop");
        let audio = directory.path().join("episode.opus");
        fs::write(&audio, b"audio").expect("write audio");
        let server = LanShareServer::start(prepare_file_share(&audio).expect("prepare file"))
            .expect("start server");
        let address = server
            .url()
            .strip_prefix("http://")
            .and_then(|url| url.split('/').next())
            .expect("server authority")
            .to_owned();
        drop(server);

        assert!(TcpStream::connect(address).is_err());
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
