//! Private, bounded original-byte caching for one Archive.org playback source.

#[cfg(test)]
mod native_tests;

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};
use url::Url;

const BLOCK_BYTES: u64 = 1024 * 1024;
const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ENTRIES: usize = 4;
const MAX_CLIENTS: usize = 4;
const MAX_FETCHES: usize = 2;
const MAX_HEADERS: usize = 8 * 1024;
const POLL: Duration = Duration::from_millis(5);
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const BLOCK_WAIT: Duration = Duration::from_secs(35);
const CLIENT_IDLE: Duration = Duration::from_secs(60);
const CLIENT_LIFETIME: Duration = Duration::from_secs(2 * 60 * 60);

/// Declines the optimization instead of bypassing a user-selected network route.
fn proxy_configured(mut value: impl FnMut(&str) -> Option<std::ffi::OsString>) -> bool {
    [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ]
    .into_iter()
    .any(|key| value(key).is_some_and(|value| !value.is_empty()))
}

/// Reservations include stopped entries still held by a worker or completed-file lease.
struct Budget {
    entries: AtomicUsize,
    bytes: AtomicU64,
    maximum: u64,
}

impl Budget {
    fn new(maximum: u64) -> Self {
        Self {
            entries: AtomicUsize::new(0),
            bytes: AtomicU64::new(0),
            maximum,
        }
    }

    fn reserve(self: &Arc<Self>, length: u64) -> io::Result<Reservation> {
        self.bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(length).filter(|sum| *sum <= self.maximum)
            })
            .map_err(|_| unavailable("original cache disk budget is occupied"))?;
        Ok(Reservation {
            budget: Arc::clone(self),
            length,
        })
    }
}

static BUDGET: LazyLock<Arc<Budget>> = LazyLock::new(|| Arc::new(Budget::new(MAX_TOTAL_BYTES)));

struct Reservation {
    budget: Arc<Budget>,
    length: u64,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.bytes.fetch_sub(self.length, Ordering::AcqRel);
    }
}

struct EntryPermit(Arc<Budget>);
impl Drop for EntryPermit {
    fn drop(&mut self) {
        self.0.entries.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Validators belong to this exact effective URL, never merely to an audio family.
#[derive(Clone)]
struct Identity {
    url: Url,
    etag: String,
    length: u64,
    content_type: String,
}

#[derive(Clone, Copy, PartialEq)]
enum BlockState {
    Missing,
    Fetching,
    Ready,
}

#[derive(Default)]
struct CacheState {
    identity: Option<Identity>,
    initializing: bool,
    disabled: bool,
    blocks: Vec<BlockState>,
    fetching: usize,
    reservation: Option<Reservation>,
}

/// Every file handle is local to an operation and closes before its shared owner drops.
struct Shared {
    source: Url,
    origin: Url,
    route: String,
    host: String,
    path: PathBuf,
    _directory: tempfile::TempDir,
    budget: Arc<Budget>,
    _permit: EntryPermit,
    state: Mutex<CacheState>,
    changed: Condvar,
    stop: AtomicBool,
    clients: AtomicUsize,
    waker: mio::Waker,
    agents: Mutex<Vec<ureq::Agent>>,
    /// Only bounded local block I/O is serialized, never an upstream or client wait.
    disk: Mutex<()>,
}

/// Owns one temporary playback route; dropping it stops new work without joining.
pub(crate) struct ArchivePlaybackCache {
    shared: Arc<Shared>,
    playback: String,
}

/// Leases complete original bytes while their exact playback owner remains current.
/// Owner retirement invalidates the lease and removes its path; callers must recheck
/// `is_current()` before publication, including after any asynchronous preparation.
#[derive(Clone)]
pub(crate) struct CompletedOriginal {
    shared: Arc<Shared>,
    length: u64,
}

impl ArchivePlaybackCache {
    /// Starts only the private local route; upstream reads remain demand-driven.
    pub(crate) fn start(source: Url) -> io::Result<Self> {
        if proxy_configured(|key| std::env::var_os(key)) {
            return Err(unavailable(
                "original cache defers to the configured proxy route",
            ));
        }
        Self::start_inner(source.clone(), source, Arc::clone(&BUDGET))
    }

    fn start_inner(source: Url, origin: Url, budget: Arc<Budget>) -> io::Result<Self> {
        if !canonical_source(&source) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a canonical Archive file URL",
            ));
        }
        budget
            .entries
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |entries| {
                (entries < MAX_ENTRIES).then_some(entries + 1)
            })
            .map_err(|_| unavailable("original cache entries are occupied"))?;
        let permit = EntryPermit(Arc::clone(&budget));
        let mut directory = tempfile::Builder::new();
        directory.prefix("youta-original-cache-").rand_bytes(24);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            directory.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = directory.tempdir()?;
        // Never expose the route as a filesystem name, even before Windows ACL setup.
        let token = private_route_token()?;
        let route = format!("/{token}/original");
        let path = directory.path().join("original");
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let host = listener.local_addr()?.to_string();
        let playback = format!("http://{host}{route}");
        let poll = mio::Poll::new()?;
        let mut listener = mio::net::TcpListener::from_std(listener);
        poll.registry()
            .register(&mut listener, mio::Token(0), mio::Interest::READABLE)?;
        let waker = mio::Waker::new(poll.registry(), mio::Token(1))?;
        let agents = (0..MAX_FETCHES).map(|_| http_agent(&origin)).collect();
        let shared = Arc::new(Shared {
            source,
            origin,
            route,
            host,
            path,
            _directory: directory,
            budget,
            _permit: permit,
            state: Mutex::new(CacheState::default()),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            waker,
            agents: Mutex::new(agents),
            disk: Mutex::new(()),
        });
        let worker = Arc::clone(&shared);
        thread::Builder::new()
            .name("youta-original-cache".into())
            .spawn(move || listen(poll, listener, worker))?;
        Ok(Self { shared, playback })
    }

    /// Returns the transient local playback URL, never a persistence identifier.
    pub(crate) fn playback_url(&self) -> &str {
        &self.playback
    }

    /// Returns the exact original canonical Archive file URL.
    pub(crate) fn source_url(&self) -> &Url {
        &self.shared.source
    }

    /// Returns a private lease only after every byte of the original is committed.
    pub(crate) fn completed(&self) -> Option<CompletedOriginal> {
        let state = self.shared.state.lock().ok()?;
        if self.shared.stop.load(Ordering::Acquire)
            || state.disabled
            || state.blocks.is_empty()
            || state.blocks.iter().any(|block| *block != BlockState::Ready)
        {
            return None;
        }
        Some(CompletedOriginal {
            shared: Arc::clone(&self.shared),
            length: state.identity.as_ref()?.length,
        })
    }

    /// Uses only a fixture-owned loopback origin while retaining canonical identity.
    #[cfg(test)]
    pub(crate) fn start_with_origin(source: Url, origin: Url) -> io::Result<Self> {
        if origin.scheme() != "http"
            || origin.host_str() != Some("127.0.0.1")
            || origin.port().is_none()
            || !origin.username().is_empty()
            || origin.password().is_some()
        {
            return Err(unavailable(
                "fixture origin must be an owned loopback listener",
            ));
        }
        Self::start_inner(source, origin, Arc::new(Budget::new(MAX_TOTAL_BYTES)))
    }
}

/// Generates a 192-bit capability entirely in memory, independently of public paths.
fn private_route_token() -> io::Result<String> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut random = [0_u8; 24];
    getrandom::fill(&mut random).map_err(|_| unavailable("cannot create playback token"))?;
    let mut token = String::with_capacity(random.len() * 2);
    for byte in random {
        token.push(char::from(HEX[usize::from(byte >> 4)]));
        token.push(char::from(HEX[usize::from(byte & 0xf)]));
    }
    Ok(token)
}

impl Drop for ArchivePlaybackCache {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.changed.notify_all();
        let _ = self.shared.waker.wake();
        // A detached HTTP read can outlive normal process shutdown. Remove payloads
        // now, not in its eventual Arc destructor. No network wait holds this lock.
        if let Ok(_disk) = self.shared.disk.lock() {
            let _ = std::fs::remove_dir_all(self.shared._directory.path());
        }
    }
}

impl CompletedOriginal {
    /// Borrows the original path, which is removed when its playback owner retires.
    pub(crate) fn path(&self) -> &Path {
        &self.shared.path
    }
    /// Returns its verified complete byte length.
    pub(crate) fn len(&self) -> u64 {
        self.length
    }
    /// Returns the exact canonical source owning these original bytes.
    pub(crate) fn source_url(&self) -> &Url {
        &self.shared.source
    }
    /// Reports whether this exact cache owner still authorizes publication.
    pub(crate) fn is_current(&self) -> bool {
        !self.shared.stop.load(Ordering::Acquire)
            && self.shared.state.lock().is_ok_and(|state| !state.disabled)
    }
}

/// A short shared-state section elects fetchers; all network and disk work is outside it.
impl Shared {
    fn block(&self, index: usize) -> io::Result<Identity> {
        let deadline = Instant::now() + BLOCK_WAIT;
        loop {
            check_active(&self.stop, deadline)?;
            let mut state = self
                .state
                .lock()
                .map_err(|_| unavailable("cache state failed"))?;
            if state.disabled {
                return Err(unavailable("original cache is unavailable"));
            }
            let initialize = state.identity.is_none() && !state.initializing;
            let ready = state.blocks.get(index) == Some(&BlockState::Ready);
            if ready {
                return state
                    .identity
                    .clone()
                    .ok_or_else(|| unavailable("cache identity missing"));
            }
            let can_fetch = state.identity.is_some()
                && state.blocks.get(index) == Some(&BlockState::Missing)
                && state.fetching < MAX_FETCHES;
            if initialize || can_fetch {
                let expected = state.identity.clone();
                let target = if initialize { 0 } else { index };
                if initialize {
                    state.initializing = true;
                } else {
                    state.blocks[target] = BlockState::Fetching;
                }
                state.fetching += 1;
                drop(state);
                let result = self.fetch_and_write(target, expected.as_ref());
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| unavailable("cache state failed"))?;
                state.fetching -= 1;
                if initialize {
                    state.initializing = false;
                }
                match result {
                    Ok(identity) if !state.disabled && !self.stop.load(Ordering::Acquire) => {
                        if initialize {
                            state.blocks =
                                vec![
                                    BlockState::Missing;
                                    usize::try_from(identity.length.div_ceil(BLOCK_BYTES))
                                        .map_err(|_| unavailable("cache block count overflow"))?
                                ];
                            state.identity = Some(identity);
                        }
                        state.blocks[target] = BlockState::Ready;
                    }
                    _ => {
                        state.disabled = true;
                    }
                }
                self.changed.notify_all();
                continue;
            }
            if state.identity.is_some() && index >= state.blocks.len() {
                return Err(unavailable("cache block is outside the file"));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let _ = self
                .changed
                .wait_timeout(state, remaining.min(Duration::from_millis(100)))
                .map_err(|_| unavailable("cache wait failed"))?;
        }
    }

    /// A block becomes readable only after its exact response body is fully written.
    fn fetch_and_write(&self, index: usize, expected: Option<&Identity>) -> io::Result<Identity> {
        let start = (index as u64)
            .checked_mul(BLOCK_BYTES)
            .ok_or_else(|| unavailable("cache range overflow"))?;
        let (identity, bytes) = fetch_range(self, start, expected)?;
        if self.stop.load(Ordering::Acquire) {
            return Err(unavailable("playback cache stopped"));
        }
        if expected.is_none() {
            let reservation = self.budget.reserve(identity.length)?;
            self.state
                .lock()
                .map_err(|_| unavailable("cache state failed"))?
                .reservation = Some(reservation);
            // Windows ACL setup can spawn a helper; it belongs on this worker,
            // before creating payload bytes, never on the UI's start path.
            crate::private_files::set_private_directory_permissions(self._directory.path())?;
        }
        let _disk = self
            .disk
            .lock()
            .map_err(|_| unavailable("cache disk state failed"))?;
        check_active(&self.stop, Instant::now() + FETCH_TIMEOUT)?;
        let mut options = OpenOptions::new();
        options.write(true);
        if expected.is_none() {
            options.create_new(true);
        }
        let mut file = crate::private_files::open_privately(&mut options).open(&self.path)?;
        if expected.is_none() {
            file.set_len(identity.length)?;
        }
        file.seek(SeekFrom::Start(start))?;
        file.write_all(&bytes)?;
        Ok(identity)
    }

    /// File handles never span an upstream read or a potentially slow socket write.
    fn read_block(&self, start: u64, end: u64) -> io::Result<Vec<u8>> {
        self.block(
            usize::try_from(start / BLOCK_BYTES).map_err(|_| unavailable("range overflow"))?,
        )?;
        let _disk = self
            .disk
            .lock()
            .map_err(|_| unavailable("cache disk state failed"))?;
        check_active(&self.stop, Instant::now() + FETCH_TIMEOUT)?;
        let length = usize::try_from(end - start + 1).map_err(|_| unavailable("range overflow"))?;
        if length > BLOCK_BYTES as usize {
            return Err(unavailable("cache block read exceeds limit"));
        }
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0; length];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }
}

/// Each exclusive agent retains its own bounded connection pool and no ambient cookies.
fn http_agent(origin: &Url) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .max_redirects(0)
        .max_response_header_size(MAX_HEADERS)
        .max_idle_connections(2)
        .max_idle_connections_per_host(1)
        .http_status_as_error(false)
        // Public start declines when a proxy is configured; fixture origins stay local.
        .proxy(None)
        .user_agent(concat!("youta/", env!("CARGO_PKG_VERSION")))
        .build();
    let _ = origin;
    ureq::Agent::with_parts(
        config,
        DefaultConnector::default(),
        PublicResolver {
            resolver: DefaultResolver::default(),
            #[cfg(test)]
            loopback: origin.scheme() == "http",
        },
    )
}

/// Exclusive checkout avoids cookie-jar races between the two bounded fetchers.
struct AgentLease<'a> {
    shared: &'a Shared,
    agent: Option<ureq::Agent>,
}

impl Drop for AgentLease<'_> {
    fn drop(&mut self) {
        if let Some(agent) = self.agent.take() {
            #[cfg(feature = "commons-upload")]
            agent.cookie_jar_lock().clear();
            if let Ok(mut agents) = self.shared.agents.lock() {
                agents.push(agent);
            }
        }
    }
}

/// Public DNS addresses are pinned by the connection resolver, not merely prechecked.
#[derive(Debug, Default)]
struct PublicResolver {
    resolver: DefaultResolver,
    #[cfg(test)]
    loopback: bool,
}

impl Resolver for PublicResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let resolved = self.resolver.resolve(uri, config, timeout)?;
        let mut public = self.empty();
        for address in &resolved {
            #[cfg(test)]
            if self.loopback && address.ip().is_loopback() {
                public.push(*address);
                continue;
            }
            if !crate::domain::ip_address_is_non_public(address.ip()) {
                public.push(*address);
            }
        }
        if public.is_empty() {
            Err(ureq::Error::HostNotFound)
        } else {
            Ok(public)
        }
    }
}

/// Combining HTTP ranges requires a strong validator for the same representation.
/// See <https://www.rfc-editor.org/rfc/rfc9110.html#section-15.3.7.3>.
fn fetch_range(
    shared: &Shared,
    start: u64,
    expected: Option<&Identity>,
) -> io::Result<(Identity, Vec<u8>)> {
    let deadline = Instant::now() + FETCH_TIMEOUT;
    let lease = AgentLease {
        shared,
        agent: Some(
            shared
                .agents
                .lock()
                .map_err(|_| unavailable("cache HTTP pool failed"))?
                .pop()
                .ok_or_else(|| unavailable("cache HTTP pool occupied"))?,
        ),
    };
    let agent = lease.agent.as_ref().expect("checked out HTTP agent");
    let mut current =
        expected.map_or_else(|| shared.origin.clone(), |identity| identity.url.clone());
    let requested_end = start
        .checked_add(BLOCK_BYTES - 1)
        .ok_or_else(|| unavailable("range overflow"))?;
    for redirects in 0..=3 {
        check_active(&shared.stop, deadline)?;
        if !endpoint_allowed(shared, &current) {
            return Err(unavailable("unsafe Archive media redirect"));
        }
        #[cfg(feature = "commons-upload")]
        agent.cookie_jar_lock().clear();
        let mut request = agent
            .get(current.as_str())
            .header("Accept-Encoding", "identity")
            .header("Range", format!("bytes={start}-{requested_end}"));
        if let Some(identity) = expected {
            request = request.header("If-Range", identity.etag.as_str());
        }
        let response = request
            .config()
            .timeout_global(Some(deadline.saturating_duration_since(Instant::now())))
            .build()
            .call();
        #[cfg(feature = "commons-upload")]
        agent.cookie_jar_lock().clear();
        let mut response = response.map_err(|_| unavailable("Archive media request failed"))?;
        let status = response.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            if redirects == 3 {
                return Err(unavailable("too many Archive media redirects"));
            }
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .filter(|value| value.len() <= 6400)
                .ok_or_else(|| unavailable("invalid Archive redirect"))?;
            current = current
                .join(location)
                .map_err(|_| unavailable("invalid Archive redirect"))?;
            continue;
        }
        if status != 206
            || response
                .headers()
                .get("content-encoding")
                .is_some_and(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
        {
            return Err(unavailable(
                "Archive response is not an original byte range",
            ));
        }
        let etag = response
            .headers()
            .get("etag")
            .and_then(|value| value.to_str().ok())
            .filter(|value| {
                value.len() >= 2
                    && value.len() <= 512
                    && value.starts_with('"')
                    && value.ends_with('"')
                    && value[1..value.len() - 1]
                        .bytes()
                        .all(|byte| byte >= 0x21 && byte != b'"' && byte < 0x7f)
            })
            .ok_or_else(|| unavailable("Archive file has no strong validator"))?
            .to_owned();
        let range = response
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok())
            .and_then(content_range)
            .ok_or_else(|| unavailable("invalid Archive content range"))?;
        let (actual_start, actual_end, length) = range;
        if length == 0
            || length > MAX_FILE_BYTES
            || actual_start != start
            || actual_end != requested_end.min(length - 1)
            || expected
                .is_some_and(|old| old.length != length || old.etag != etag || old.url != current)
        {
            return Err(unavailable(
                "Archive file changed or exceeds the cache limit",
            ));
        }
        let wanted = usize::try_from(actual_end - actual_start + 1)
            .map_err(|_| unavailable("invalid range length"))?;
        if response.body().content_length() != Some(wanted as u64) {
            return Err(unavailable("Archive range length mismatch"));
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .filter(|value| {
                value.len() <= 128 && value.bytes().all(|byte| (0x20..0x7f).contains(&byte))
            })
            .unwrap_or("application/octet-stream")
            .to_owned();
        let mut bytes = Vec::with_capacity(wanted);
        let mut reader = response.body_mut().as_reader();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            check_active(&shared.stop, deadline)?;
            let limit = (wanted + 1 - bytes.len()).min(buffer.len());
            let read = reader.read(&mut buffer[..limit])?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if bytes.len() > wanted {
                return Err(unavailable("Archive range body exceeded its length"));
            }
        }
        if bytes.len() != wanted {
            return Err(unavailable("Archive range body was incomplete"));
        }
        return Ok((
            Identity {
                url: current,
                etag,
                length,
                content_type,
            },
            bytes,
        ));
    }
    Err(unavailable("Archive media request exhausted redirects"))
}

fn endpoint_allowed(shared: &Shared, url: &Url) -> bool {
    if url.as_str().len() > 6400
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    #[cfg(test)]
    if shared.origin.scheme() == "http" {
        return url.origin() == shared.origin.origin();
    }
    let _ = shared;
    url.scheme() == "https"
        && url.port().is_none()
        && url
            .host_str()
            .is_some_and(|host| host == "archive.org" || host.ends_with(".archive.org"))
}

fn content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, length) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, length) = (
        start.parse::<u64>().ok()?,
        end.parse::<u64>().ok()?,
        length.parse::<u64>().ok()?,
    );
    (start <= end && end < length).then_some((start, end, length))
}

/// Only provider-canonical segment spelling is accepted; decoded separators never escape it.
fn canonical_source(url: &Url) -> bool {
    if url.as_str().len() > 6400 || !crate::domain::is_canonical_archive_org_audio_url(url) {
        return false;
    }
    let Some(mut segments) = url.path_segments() else {
        return false;
    };
    if segments.next() != Some("download") {
        return false;
    }
    let Some(identifier) = segments.next() else {
        return false;
    };
    if identifier.is_empty()
        || identifier.len() > 100
        || !(identifier.as_bytes()[0].is_ascii_alphanumeric() || identifier.starts_with('@'))
        || !identifier
            .bytes()
            .skip(1)
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return false;
    }
    let mut filenames = Vec::new();
    let mut length = 0_usize;
    for segment in segments {
        let Some(decoded) = decode_segment(segment) else {
            return false;
        };
        if matches!(decoded.as_str(), "" | "." | "..")
            || decoded
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\'))
        {
            return false;
        }
        length = length.saturating_add(decoded.len() + usize::from(!filenames.is_empty()));
        filenames.push(decoded);
        if filenames.len() > 32 || length > 2048 {
            return false;
        }
    }
    if filenames.is_empty() {
        return false;
    }
    let Ok(mut canonical) = Url::parse("https://archive.org/") else {
        return false;
    };
    canonical
        .path_segments_mut()
        .expect("fixed HTTPS base")
        .clear()
        .extend(["download", identifier])
        .extend(&filenames);
    canonical == *url
}

fn decode_segment(segment: &str) -> Option<String> {
    let mut decoded = Vec::with_capacity(segment.len());
    let mut bytes = segment.bytes();
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'%' {
            let high = char::from(bytes.next()?).to_digit(16)?;
            let low = char::from(bytes.next()?).to_digit(16)?;
            u8::try_from(high * 16 + low).ok()?
        } else {
            byte
        });
    }
    String::from_utf8(decoded).ok()
}

struct ClientPermit(Arc<Shared>);
impl Drop for ClientPermit {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Readiness and the owner's wakeup replace any idle/paused polling loop.
fn listen(mut poll: mio::Poll, listener: mio::net::TcpListener, shared: Arc<Shared>) {
    let mut events = mio::Events::with_capacity(4);
    while !shared.stop.load(Ordering::Acquire) {
        if let Err(error) = poll.poll(&mut events, None) {
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        while !shared.stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    if shared
                        .clients
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                            (count < MAX_CLIENTS).then_some(count + 1)
                        })
                        .is_err()
                    {
                        continue;
                    }
                    let permit = ClientPermit(Arc::clone(&shared));
                    let _ = thread::Builder::new()
                        .name("youta-cache-reader".into())
                        .spawn(move || {
                            let permit = permit;
                            let _ = serve(stream.into(), &permit.0);
                        });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }
}

fn serve(mut stream: TcpStream, shared: &Shared) -> io::Result<()> {
    // Blocking Winsock write timeouts have ambiguous progress; retry only explicit WouldBlock.
    stream.set_nonblocking(true)?;
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    let request = read_headers(&mut stream, &shared.stop)?;
    let mut lines = request.lines();
    let mut request_line = lines.next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default();
    let path = request_line.next().unwrap_or_default();
    let version = request_line.next().unwrap_or_default();
    if !matches!(method, "GET" | "HEAD")
        || path != shared.route
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || request_line.next().is_some()
    {
        return response(&mut stream, "404 Not Found", "", &shared.stop);
    }
    let mut host = None;
    let mut range = None;
    let mut if_range = None;
    for line in lines.take_while(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else {
            return response(&mut stream, "400 Bad Request", "", &shared.stop);
        };
        if name.eq_ignore_ascii_case("host") {
            if host.replace(value.trim()).is_some() {
                return response(&mut stream, "400 Bad Request", "", &shared.stop);
            }
        } else if name.eq_ignore_ascii_case("range") && range.replace(value.trim()).is_some() {
            return response(&mut stream, "400 Bad Request", "", &shared.stop);
        } else if name.eq_ignore_ascii_case("if-range") && if_range.replace(value.trim()).is_some()
        {
            return response(&mut stream, "400 Bad Request", "", &shared.stop);
        }
    }
    if host != Some(shared.host.as_str()) {
        return response(&mut stream, "404 Not Found", "", &shared.stop);
    }
    let Ok(identity) = shared.block(0) else {
        return redirect(&mut stream, shared);
    };
    // Only the exact strong ETag proves the client's range belongs to these bytes.
    // HTTP dates/weak or changed validators safely receive the complete representation.
    let range = range.filter(|_| if_range.is_none_or(|value| value == identity.etag));
    let bounds = if let Some(range) = range {
        let Some(bounds) = single_range(range, identity.length) else {
            return response(
                &mut stream,
                "416 Range Not Satisfiable",
                &format!("Content-Range: bytes */{}\r\n", identity.length),
                &shared.stop,
            );
        };
        bounds
    } else {
        (0, identity.length - 1)
    };
    let (start, end) = bounds;
    if shared
        .block(usize::try_from(start / BLOCK_BYTES).map_err(|_| unavailable("range overflow"))?)
        .is_err()
    {
        return redirect(&mut stream, shared);
    }
    let status = if range.is_some() {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let content_range = if range.is_some() {
        format!("Content-Range: bytes {start}-{end}/{}\r\n", identity.length)
    } else {
        String::new()
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: {}\r\n{content_range}Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        identity.content_type,
        end - start + 1,
        identity.etag
    );
    let lifetime = Instant::now() + CLIENT_LIFETIME;
    write_bytes(&mut stream, header.as_bytes(), &shared.stop, lifetime)?;
    if method != "HEAD" {
        let mut position = start;
        while position <= end {
            check_active(&shared.stop, lifetime)?;
            let block_end = ((position / BLOCK_BYTES + 1) * BLOCK_BYTES - 1).min(end);
            // Once headers were emitted a later cache failure closes this response.
            // The next request redirects; never splice 307 headers into audio bytes.
            let bytes = shared.read_block(position, block_end)?;
            write_bytes(&mut stream, &bytes, &shared.stop, lifetime)?;
            position = block_end + 1;
        }
    }
    stream.shutdown(Shutdown::Write)
}

fn single_range(value: &str, length: u64) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    if value.contains(',') || length == 0 {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        let count = end.parse::<u64>().ok()?.min(length);
        return (count > 0).then_some((length - count, length - 1));
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        length - 1
    } else {
        end.parse::<u64>().ok()?.min(length - 1)
    };
    (start <= end && start < length).then_some((start, end))
}

fn read_headers(stream: &mut TcpStream, stop: &AtomicBool) -> io::Result<String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        check_active(stop, deadline)?;
        match stream.read(&mut buffer) {
            Ok(0) => return Err(unavailable("incomplete cache request")),
            Ok(count) => {
                bytes.extend_from_slice(&buffer[..count]);
                if bytes.len() > MAX_HEADERS {
                    return Err(unavailable("cache request headers too large"));
                }
                if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    bytes.truncate(end + 4);
                    return String::from_utf8(bytes)
                        .map_err(|_| unavailable("invalid cache request"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

fn write_bytes(
    stream: &mut TcpStream,
    mut bytes: &[u8],
    stop: &AtomicBool,
    lifetime: Instant,
) -> io::Result<()> {
    let mut deadline = (Instant::now() + CLIENT_IDLE).min(lifetime);
    while !bytes.is_empty() {
        check_active(stop, deadline)?;
        match stream.write(bytes) {
            Ok(0) => return Err(unavailable("cache response write returned zero")),
            Ok(count) => {
                bytes = &bytes[count..];
                deadline = (Instant::now() + CLIENT_IDLE).min(lifetime);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn response(
    stream: &mut TcpStream,
    status: &str,
    headers: &str,
    stop: &AtomicBool,
) -> io::Result<()> {
    write_bytes(
        stream,
        format!("HTTP/1.1 {status}\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n")
            .as_bytes(),
        stop,
        Instant::now() + Duration::from_secs(3),
    )?;
    stream.shutdown(Shutdown::Write)
}

fn redirect(stream: &mut TcpStream, shared: &Shared) -> io::Result<()> {
    response(
        stream,
        "307 Temporary Redirect",
        &format!("Location: {}\r\nCache-Control: no-store\r\n", shared.source),
        &shared.stop,
    )
}

fn check_active(stop: &AtomicBool, deadline: Instant) -> io::Result<()> {
    if stop.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "playback cache stopped",
        ));
    }
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "playback cache wait timed out",
        ));
    }
    Ok(())
}

fn unavailable(message: &'static str) -> io::Error {
    io::Error::other(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    const BLOCK: usize = 1024 * 1024;

    #[derive(Clone, Copy)]
    enum ResponseKind {
        Valid,
        WrongRange,
        WeakTag,
        Encoded,
        Truncated,
        Oversized,
        IgnoreRange,
        ChangedTag,
        StallAfterFirst,
    }

    /// A finite origin owns its listener/thread and never uses public media.
    struct Origin {
        url: Url,
        bytes: Arc<Vec<u8>>,
        calls: Arc<AtomicUsize>,
        ranges: Arc<Mutex<Vec<(usize, usize)>>>,
        stop: Arc<AtomicBool>,
        stalled: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Origin {
        fn new(length: usize, kind: ResponseKind) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = Url::parse(&format!(
                "http://{}/original",
                listener.local_addr().unwrap()
            ))
            .unwrap();
            let bytes = Arc::new(
                (0..length)
                    .map(|index| (index % 251) as u8)
                    .collect::<Vec<_>>(),
            );
            let calls = Arc::new(AtomicUsize::new(0));
            let ranges = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let stalled = Arc::new(AtomicBool::new(false));
            let worker_bytes = Arc::clone(&bytes);
            let worker_calls = Arc::clone(&calls);
            let worker_ranges = Arc::clone(&ranges);
            let worker_stop = Arc::clone(&stop);
            let worker_stalled = Arc::clone(&stalled);
            let thread = thread::spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            stream
                                .set_read_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            stream
                                .set_write_timeout(Some(Duration::from_secs(2)))
                                .unwrap();
                            let request = request_headers(&mut stream);
                            worker_calls.fetch_add(1, Ordering::AcqRel);
                            let range = request
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("range").then_some(value.trim())
                                })
                                .unwrap_or("bytes=0-0");
                            let (start, end) = range
                                .strip_prefix("bytes=")
                                .unwrap()
                                .split_once('-')
                                .unwrap();
                            let start = start.parse::<usize>().unwrap();
                            let end = end.parse::<usize>().unwrap().min(worker_bytes.len() - 1);
                            worker_ranges.lock().unwrap().push((start, end));
                            let mut body = worker_bytes[start..=end].to_vec();
                            let reported_length = if matches!(kind, ResponseKind::Oversized) {
                                256 * BLOCK + 1
                            } else {
                                worker_bytes.len()
                            };
                            let reported_start =
                                start + usize::from(matches!(kind, ResponseKind::WrongRange));
                            let etag = if matches!(kind, ResponseKind::ChangedTag) && start > 0 {
                                "\"changed\""
                            } else if matches!(kind, ResponseKind::WeakTag) {
                                "W/\"fixture\""
                            } else {
                                "\"fixture\""
                            };
                            let encoding = if matches!(kind, ResponseKind::Encoded) {
                                "Content-Encoding: gzip\r\n"
                            } else {
                                ""
                            };
                            let status = if matches!(kind, ResponseKind::IgnoreRange) {
                                "200 OK"
                            } else {
                                "206 Partial Content"
                            };
                            let headers = format!(
                                "HTTP/1.1 {status}\r\nContent-Type: audio/flac\r\nETag: {etag}\r\nContent-Range: bytes {reported_start}-{end}/{reported_length}\r\nContent-Length: {}\r\n{encoding}Connection: close\r\n\r\n",
                                body.len()
                            );
                            if matches!(kind, ResponseKind::Truncated) {
                                body.truncate(body.len() / 2);
                            }
                            let _ = stream.write_all(headers.as_bytes());
                            if matches!(kind, ResponseKind::StallAfterFirst) && start > 0 {
                                worker_stalled.store(true, Ordering::Release);
                                let deadline = Instant::now() + Duration::from_secs(10);
                                while !worker_stop.load(Ordering::Acquire)
                                    && Instant::now() < deadline
                                {
                                    thread::sleep(POLL);
                                }
                            }
                            let _ = stream.write_all(&body);
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                url,
                bytes,
                calls,
                ranges,
                stop,
                stalled,
                thread: Some(thread),
            }
        }
    }

    impl Drop for Origin {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.thread.take() {
                worker.join().unwrap();
            }
        }
    }

    fn source() -> Url {
        Url::parse("https://archive.org/download/fixture/original.flac").unwrap()
    }

    fn request_headers(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") && bytes.len() < 8192 {
            let mut byte = [0];
            if stream.read(&mut byte).unwrap_or(0) == 0 {
                break;
            }
            bytes.push(byte[0]);
        }
        String::from_utf8(bytes).unwrap()
    }

    fn get(address: &str, range: Option<&str>) -> (String, Vec<u8>) {
        let range = range.map_or_else(String::new, |value| format!("Range: {value}\r\n"));
        request(address, "GET", &range)
    }

    fn request(address: &str, method: &str, headers: &str) -> (String, Vec<u8>) {
        let url = Url::parse(address).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", url.port().unwrap())).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "{method} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{headers}Connection: close\r\n\r\n",
            url.path(),
            url.port().unwrap()
        )
        .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let end = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        (
            String::from_utf8(response[..end].to_vec()).unwrap(),
            response[end..].to_vec(),
        )
    }

    #[test]
    fn playback_cache_is_lazy_reuses_blocks_and_finishes_byte_exactly_offline() {
        let origin = Origin::new(BLOCK * 2 + 137, ResponseKind::Valid);
        let expected = Arc::clone(&origin.bytes);
        let cache = ArchivePlaybackCache::start_with_origin(source(), origin.url.clone()).unwrap();
        assert_eq!(origin.calls.load(Ordering::Acquire), 0);
        assert_eq!(cache.source_url(), &source());
        for _ in 0..2 {
            let (headers, body) = get(cache.playback_url(), Some("bytes=10-31"));
            assert!(headers.starts_with("HTTP/1.1 206"));
            assert_eq!(body, expected[10..32]);
        }
        assert_eq!(origin.calls.load(Ordering::Acquire), 1);
        assert!(cache.completed().is_none());
        assert_eq!(get(cache.playback_url(), None).1, *expected);
        assert_eq!(origin.calls.load(Ordering::Acquire), 3);
        assert_eq!(origin.ranges.lock().unwrap()[0], (0, BLOCK - 1));
        let completed = cache.completed().expect("every block committed");
        assert_eq!(completed.len(), expected.len() as u64);
        assert_eq!(completed.source_url(), &source());
        assert_eq!(std::fs::read(completed.path()).unwrap(), *expected);
        drop(origin);
        assert_eq!(
            get(cache.playback_url(), Some("bytes=-137")).1,
            expected[BLOCK * 2..]
        );
    }

    #[test]
    fn playback_cache_rejects_untrusted_or_incomplete_responses_without_completion() {
        for kind in [
            ResponseKind::WrongRange,
            ResponseKind::WeakTag,
            ResponseKind::Encoded,
            ResponseKind::Truncated,
            ResponseKind::Oversized,
            ResponseKind::IgnoreRange,
        ] {
            let origin = Origin::new(2048, kind);
            let cache =
                ArchivePlaybackCache::start_with_origin(source(), origin.url.clone()).unwrap();
            let (headers, _) = get(cache.playback_url(), None);
            assert!(headers.starts_with("HTTP/1.1 307"), "{headers}");
            assert!(headers.contains(&format!("Location: {}\r\n", source())));
            assert!(cache.completed().is_none());
        }
    }

    #[test]
    fn playback_cache_owner_stop_invalidates_leases_and_retires_storage_immediately() {
        let origin = Origin::new(2048, ResponseKind::Valid);
        let cache = ArchivePlaybackCache::start_with_origin(source(), origin.url.clone()).unwrap();
        assert_eq!(get(cache.playback_url(), None).1, *origin.bytes);
        let completed = cache.completed().unwrap();
        assert!(completed.is_current());
        let second = completed.clone();
        let path = completed.path().to_owned();
        let started = Instant::now();
        drop(cache);
        assert!(!completed.is_current());
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(!path.exists());
        drop(completed);
        drop(second);
        let deadline = Instant::now() + Duration::from_secs(2);
        while path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!path.exists());
    }

    #[test]
    fn playback_cache_rejects_noncanonical_sources_before_binding() {
        for address in [
            "http://archive.org/download/fixture/a.flac",
            "https://evil.example/a.flac",
            "https://archive.org/download/fixture/a.flac?token=secret",
            "https://user@archive.org/download/fixture/a.flac",
        ] {
            assert!(ArchivePlaybackCache::start(Url::parse(address).unwrap()).is_err());
        }
    }

    #[test]
    fn playback_capability_is_not_the_public_temporary_directory_name() {
        let origin = Origin::new(2048, ResponseKind::Valid);
        let cache = ArchivePlaybackCache::start_with_origin(source(), origin.url.clone()).unwrap();
        let public_name = cache
            .shared
            ._directory
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(!cache.playback_url().contains(public_name));
        assert_eq!(origin.calls.load(Ordering::Acquire), 0);
    }

    /// The route is memory-only, and Unix privacy precedes all worker activity.
    #[test]
    fn playback_capability_is_memory_only_and_directory_starts_private() {
        let origin = Origin::new(2048, ResponseKind::Valid);
        let cache = ArchivePlaybackCache::start_with_origin(source(), origin.url.clone()).unwrap();
        let token = cache
            .shared
            .route
            .strip_prefix('/')
            .unwrap()
            .strip_suffix("/original")
            .unwrap();
        assert_eq!(
            token.len(),
            48,
            "route must contain 24 independent random bytes"
        );
        assert!(token.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(
            std::fs::read_dir(cache.shared._directory.path())
                .unwrap()
                .count(),
            0
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(cache.shared._directory.path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700,
                "directory must be private before its first upstream request"
            );
        }
        assert_eq!(origin.calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn configured_proxy_disables_optimization_without_interpreting_or_logging_values() {
        assert!(!proxy_configured(|_| None));
        assert!(!proxy_configured(|_| Some("".into())));
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            assert!(proxy_configured(
                |key| (key == name).then(|| "http://user:private@example.invalid:8080".into())
            ));
        }
        assert!(!proxy_configured(
            |key| (key == "NO_PROXY").then(|| "*".into())
        ));
    }

    #[test]
    fn sparse_tail_and_changed_validators_never_produce_completed_files() {
        let origin = Origin::new(3 * BLOCK + 13, ResponseKind::Valid);
        let cache = ArchivePlaybackCache::start_with_origin(source(), origin.url.clone()).unwrap();
        assert_eq!(
            get(cache.playback_url(), Some("bytes=-13")).1,
            origin.bytes[3 * BLOCK..]
        );
        assert_eq!(
            std::fs::metadata(&cache.shared.path).unwrap().len(),
            origin.bytes.len() as u64
        );
        assert!(cache.completed().is_none(), "sparse length is not coverage");
        assert_eq!(origin.calls.load(Ordering::Acquire), 2);

        let changed = Origin::new(2 * BLOCK, ResponseKind::ChangedTag);
        let cache = ArchivePlaybackCache::start_with_origin(source(), changed.url.clone()).unwrap();
        assert_eq!(get(cache.playback_url(), Some("bytes=0-0")).1, [0]);
        assert!(
            get(
                cache.playback_url(),
                Some(&format!("bytes={BLOCK}-{BLOCK}"))
            )
            .0
            .starts_with("HTTP/1.1 307")
        );
        assert!(cache.completed().is_none());
        assert!(
            get(cache.playback_url(), Some("bytes=0-0"))
                .0
                .starts_with("HTTP/1.1 307")
        );
        assert_eq!(changed.calls.load(Ordering::Acquire), 2);
    }

    #[test]
    fn retiring_cache_removes_payload_while_an_upstream_body_is_still_blocked() {
        let origin = Origin::new(2 * BLOCK, ResponseKind::StallAfterFirst);
        let cache = ArchivePlaybackCache::start_with_origin(source(), origin.url.clone()).unwrap();
        assert_eq!(get(cache.playback_url(), Some("bytes=0-0")).1, [0]);
        let path = cache.shared.path.clone();
        assert!(path.exists());
        let url = cache.playback_url().to_owned();
        let client = thread::spawn(move || get(&url, None));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !origin.stalled.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(POLL);
        }
        assert!(origin.stalled.load(Ordering::Acquire));
        let started = Instant::now();
        drop(cache);
        assert!(
            started.elapsed() < Duration::from_millis(100),
            "retirement must not wait for remote I/O"
        );
        assert!(
            !path.exists(),
            "normal exit must not depend on detached HTTP worker destructors"
        );
        drop(origin);
        let (_, partial) = client.join().unwrap();
        assert_eq!(partial.len(), BLOCK);
        assert!(
            !path.exists(),
            "the retired fetch must not recreate the payload"
        );
    }

    #[test]
    fn retired_leases_keep_disk_and_entry_reservations_until_cleanup() {
        let budget = Arc::new(Budget::new(2048));
        let origin = Origin::new(2048, ResponseKind::Valid);
        let cache =
            ArchivePlaybackCache::start_inner(source(), origin.url.clone(), Arc::clone(&budget))
                .unwrap();
        assert_eq!(get(cache.playback_url(), None).1, *origin.bytes);
        let completed = cache.completed().unwrap();
        drop(cache);
        let other =
            ArchivePlaybackCache::start_inner(source(), origin.url.clone(), Arc::clone(&budget))
                .unwrap();
        assert!(
            get(other.playback_url(), None)
                .0
                .starts_with("HTTP/1.1 307")
        );
        assert_eq!(budget.bytes.load(Ordering::Acquire), 2048);
        drop(other);
        drop(completed);
        let deadline = Instant::now() + Duration::from_secs(2);
        while budget.entries.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            thread::sleep(POLL);
        }
        assert_eq!(budget.entries.load(Ordering::Acquire), 0);
        assert_eq!(budget.bytes.load(Ordering::Acquire), 0);

        let mut entries = Vec::new();
        for _ in 0..MAX_ENTRIES {
            entries.push(
                ArchivePlaybackCache::start_inner(
                    source(),
                    origin.url.clone(),
                    Arc::clone(&budget),
                )
                .unwrap(),
            );
        }
        assert!(
            ArchivePlaybackCache::start_inner(source(), origin.url.clone(), Arc::clone(&budget))
                .is_err()
        );
        drop(entries);
        while budget.entries.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            thread::sleep(POLL);
        }
        assert_eq!(budget.entries.load(Ordering::Acquire), 0);
    }

    #[test]
    fn disk_budget_checked_add_bounds_and_failed_file_creation_are_accounted() {
        let budget = Arc::new(Budget::new(MAX_TOTAL_BYTES));
        let first = budget.reserve(MAX_FILE_BYTES).unwrap();
        let second = budget.reserve(MAX_FILE_BYTES).unwrap();
        assert!(budget.reserve(1).is_err());
        assert!(budget.reserve(u64::MAX).is_err());
        drop(first);
        drop(second);
        assert_eq!(budget.bytes.load(Ordering::Acquire), 0);
        let origin = Origin::new(2048, ResponseKind::Valid);
        let cache =
            ArchivePlaybackCache::start_inner(source(), origin.url.clone(), Arc::clone(&budget))
                .unwrap();
        std::fs::create_dir(&cache.shared.path).unwrap();
        assert!(
            get(cache.playback_url(), None)
                .0
                .starts_with("HTTP/1.1 307")
        );
        assert!(cache.completed().is_none());
        assert_eq!(budget.bytes.load(Ordering::Acquire), 2048);
        drop(cache);
        let deadline = Instant::now() + Duration::from_secs(2);
        while budget.entries.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
            thread::sleep(POLL);
        }
        assert_eq!(budget.bytes.load(Ordering::Acquire), 0);
    }

    #[test]
    fn simultaneous_requests_for_one_block_share_the_same_upstream_read() {
        let origin = Origin::new(2 * BLOCK, ResponseKind::Valid);
        let cache = ArchivePlaybackCache::start_with_origin(source(), origin.url.clone()).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(MAX_CLIENTS));
        let workers = (0..MAX_CLIENTS)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let url = cache.playback_url().to_owned();
                thread::spawn(move || {
                    barrier.wait();
                    get(&url, Some("bytes=2-5"))
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            let (headers, bytes) = worker.join().unwrap();
            assert!(headers.starts_with("HTTP/1.1 206"));
            assert_eq!(bytes, origin.bytes[2..6]);
        }
        assert_eq!(origin.calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn request_routing_head_and_range_validation_remain_local_and_bounded() {
        let origin = Origin::new(2048, ResponseKind::Valid);
        let cache = ArchivePlaybackCache::start_with_origin(source(), origin.url.clone()).unwrap();
        let mut invalid = Url::parse(cache.playback_url()).unwrap();
        invalid.set_path("/not-the-capability/original");
        assert!(get(invalid.as_str(), None).0.starts_with("HTTP/1.1 404"));
        assert!(
            request(cache.playback_url(), "GET", "Host: attacker.example\r\n")
                .0
                .starts_with("HTTP/1.1 400")
        );
        assert_eq!(origin.calls.load(Ordering::Acquire), 0);
        let (headers, body) = request(cache.playback_url(), "HEAD", "");
        assert!(headers.starts_with("HTTP/1.1 200"));
        assert!(headers.contains("Content-Length: 2048\r\n"));
        assert!(body.is_empty());
        for range in [
            "bytes=2048-",
            "bytes=3-2",
            "bytes=0-1,3-4",
            "bytes=-0",
            "bytes=a-b",
        ] {
            assert!(
                get(cache.playback_url(), Some(range))
                    .0
                    .starts_with("HTTP/1.1 416")
            );
        }
        assert_eq!(origin.calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn local_if_range_uses_only_a_matching_strong_validator() {
        let origin = Origin::new(2048, ResponseKind::Valid);
        let cache = ArchivePlaybackCache::start_with_origin(source(), origin.url.clone()).unwrap();
        let (headers, body) = request(
            cache.playback_url(),
            "GET",
            "Range: bytes=7-11\r\nIf-Range: \"fixture\"\r\n",
        );
        assert!(headers.starts_with("HTTP/1.1 206"));
        assert_eq!(body, origin.bytes[7..12]);
        for validator in [
            "\"other\"",
            "W/\"fixture\"",
            "Wed, 21 Oct 2015 07:28:00 GMT",
        ] {
            let (headers, body) = request(
                cache.playback_url(),
                "GET",
                &format!("Range: bytes=7-11\r\nIf-Range: {validator}\r\n"),
            );
            assert!(headers.starts_with("HTTP/1.1 200"), "{headers}");
            assert_eq!(body, *origin.bytes);
        }
        assert_eq!(origin.calls.load(Ordering::Acquire), 1);
    }

    /// A keep-alive origin proves TCP reuse and checks that response cookies are not replayed.
    #[test]
    fn sequential_blocks_reuse_connections_without_replaying_cookies() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = Url::parse(&format!(
            "http://{}/original",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let seen_cookie = Arc::new(AtomicBool::new(false));
        let worker_connections = Arc::clone(&connections);
        let worker_cookie = Arc::clone(&seen_cookie);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut served = 0;
            while served < 2 && Instant::now() < deadline {
                let Ok((mut stream, _)) = listener.accept() else {
                    thread::sleep(POLL);
                    continue;
                };
                worker_connections.fetch_add(1, Ordering::AcqRel);
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                while served < 2 {
                    let request = request_headers(&mut stream);
                    if request.is_empty() {
                        break;
                    }
                    if request
                        .lines()
                        .any(|line| line.to_ascii_lowercase().starts_with("cookie:"))
                    {
                        worker_cookie.store(true, Ordering::Release);
                    }
                    let start = served * BLOCK;
                    let end = start + BLOCK - 1;
                    write!(stream, "HTTP/1.1 206 Partial Content\r\nETag: \"fixture\"\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {BLOCK}\r\nSet-Cookie: secret=never-forward; Path=/\r\nConnection: keep-alive\r\n\r\n", 2 * BLOCK).unwrap();
                    stream.write_all(&vec![served as u8; BLOCK]).unwrap();
                    served += 1;
                }
            }
            assert_eq!(served, 2);
        });
        let cache = ArchivePlaybackCache::start_with_origin(source(), origin).unwrap();
        assert_eq!(get(cache.playback_url(), Some("bytes=0-0")).1, [0]);
        assert_eq!(
            get(
                cache.playback_url(),
                Some(&format!("bytes={BLOCK}-{BLOCK}"))
            )
            .1,
            [1]
        );
        worker.join().unwrap();
        assert_eq!(connections.load(Ordering::Acquire), 1);
        assert!(!seen_cookie.load(Ordering::Acquire));
    }
}
