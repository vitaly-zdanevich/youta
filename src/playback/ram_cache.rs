//! Demand-filled encoded audio blocks retained in RAM across playback changes.
//!
//! One capability-protected loopback server serves seekable HTTP byte ranges.
//! Upstream bodies are never trusted without a finite length and strong
//! representation validator. This is an optional acceleration: unsupported
//! sources redirect to their original URL, while failures after response headers
//! close the connection instead of combining bytes from different versions.

#[cfg(test)]
mod native_tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};
use url::Url;

use super::PlaybackHttpHeaders;

const BLOCK_BYTES: u64 = 512 * 1024;
const SCRATCH_BYTES: u64 = 8192;
const BLOCK_OVERHEAD: u64 = 256;
const MAX_CLIENTS: usize = 4;
const MAX_FETCHES: usize = 2;
const MAX_ENTRIES: usize = 4096;
const MAX_HEADERS: usize = 16 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const BLOCK_WAIT: Duration = Duration::from_secs(35);
const CLIENT_IDLE: Duration = Duration::from_secs(60);
const CLIENT_LIFETIME: Duration = Duration::from_secs(2 * 60 * 60);
const POLL: Duration = Duration::from_millis(5);

type Allowance = dyn Fn(u64) -> Option<u64> + Send + Sync;

/// Counts payloads even after eviction while an active reader still holds them.
struct Budget {
    used: AtomicU64,
    /// Reservations not yet known to be resident must not manufacture headroom.
    pending: AtomicU64,
    allowance: Box<Allowance>,
}

/// One admitted allocation, including a fetcher's temporary read buffer.
struct Reservation {
    budget: Arc<Budget>,
    bytes: u64,
    pending: bool,
}

impl Reservation {
    /// Releases unused capacity only after the bounded HTTP transfer is complete.
    fn shrink(&mut self, bytes: u64) {
        if self.pending {
            self.budget.pending.fetch_sub(self.bytes, Ordering::AcqRel);
            self.pending = false;
        }
        self.budget
            .used
            .fetch_sub(self.bytes - bytes, Ordering::AcqRel);
        self.bytes = bytes;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.pending {
            self.budget.pending.fetch_sub(self.bytes, Ordering::AcqRel);
        }
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

struct Block {
    bytes: Vec<u8>,
    _reservation: Reservation,
}

struct CachedBlock {
    data: Arc<Block>,
    touched: u64,
}

/// Exact request context is private to one registration, never logged or persisted.
struct Origin {
    key: String,
    source: Url,
    headers: PlaybackHttpHeaders,
    route: String,
    invalidated: AtomicBool,
    /// Only a body which already advertised success can cause premature EOF.
    stream_failed: AtomicBool,
    _reservation: Reservation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Validator {
    Etag(String),
    Modified(String),
}

impl Validator {
    fn value(&self) -> &str {
        match self {
            Self::Etag(value) | Self::Modified(value) => value,
        }
    }
    fn header(&self) -> &'static str {
        match self {
            Self::Etag(_) => "ETag",
            Self::Modified(_) => "Last-Modified",
        }
    }
}

/// Ranges can be joined only for the exact effective URL, length and validator.
#[derive(Clone)]
struct Identity {
    effective: Url,
    validator: Validator,
    length: u64,
    content_type: String,
}

struct Entry {
    origin: Arc<Origin>,
    identity: Option<Identity>,
    blocks: BTreeMap<u64, CachedBlock>,
    touched: u64,
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    routes: HashMap<String, String>,
    fetching: HashSet<(String, u64)>,
    /// Ordered block handles make pressure eviction O(log n), not a full scan
    /// of every retained block for each byte range removed.
    block_lru: BTreeMap<u64, (Arc<Origin>, u64)>,
    clock: u64,
}

impl State {
    fn touch(&mut self) -> u64 {
        self.clock = self.clock.saturating_add(1);
        self.clock
    }

    /// Evicts one least-recently-read payload; leased bytes remain budgeted.
    fn evict_one(&mut self) -> bool {
        while let Some((touched, (origin, index))) = self.block_lru.pop_first() {
            if let Some(entry) = self.entries.get_mut(&origin.key)
                && Arc::ptr_eq(&entry.origin, &origin)
                && entry
                    .blocks
                    .get(&index)
                    .is_some_and(|block| block.touched == touched)
            {
                entry.blocks.remove(&index);
                return true;
            }
        }
        false
    }

    /// Removes payloads and their ordering records together without copying keys.
    fn clear_blocks(&mut self, key: &str) {
        if let Some(entry) = self.entries.get_mut(key) {
            for block in std::mem::take(&mut entry.blocks).into_values() {
                self.block_lru.remove(&block.touched);
            }
        }
    }

    /// Metadata has a separate safety bound even for permanently unsupported URLs.
    fn remove_oldest_entry(&mut self) {
        let key = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.touched)
            .map(|(key, _)| key.clone());
        if let Some(key) = key {
            self.remove(&key);
        }
    }

    fn remove(&mut self, key: &str) {
        self.clear_blocks(key);
        if let Some(entry) = self.entries.remove(key) {
            entry.origin.invalidated.store(true, Ordering::Release);
            self.routes.remove(&entry.origin.route);
        }
    }
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    budget: Arc<Budget>,
    stop: AtomicBool,
    clients: AtomicUsize,
    agents: Mutex<Vec<ureq::Agent>>,
    waker: mio::Waker,
    host: String,
    token: String,
    next_route: AtomicU64,
    allow_loopback: bool,
}

/// Owns lazy private HTTP routes and a shared pressure-sensitive RAM block LRU.
#[derive(Clone)]
pub struct RamPlaybackCache {
    shared: Arc<Shared>,
    _lifetime: Arc<CacheLifetime>,
}

/// Per-load failure state survives LRU eviction of the route's registry entry.
#[derive(Clone)]
pub(crate) struct RamCacheTicket {
    origin: Arc<Origin>,
}

impl RamCacheTicket {
    /// A successful response body was cut short, rather than safely redirected.
    pub(crate) fn is_failed(&self) -> bool {
        self.origin.stream_failed.load(Ordering::Acquire)
    }

    /// Allows an explicitly owned proxy to associate only its exact original input.
    pub(crate) fn matches_source(&self, source: &str) -> bool {
        self.origin.source.as_str() == source
    }
}

/// The listener also owns shared state, so only external handle ownership stops it.
struct CacheLifetime(Arc<Shared>);

/// An opaque owned route which another private proxy may explicitly trust.
/// Construction is restricted to a registered cache capability, never a supplied URL.
#[derive(Clone)]
#[cfg(any(feature = "archive-org", test))]
pub(crate) struct TrustedRamRoute {
    url: String,
    source: Url,
    ticket: RamCacheTicket,
    _owner: RamPlaybackCache,
}

#[cfg(any(feature = "archive-org", test))]
impl TrustedRamRoute {
    /// Exact capability URL for the owned loopback listener.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }
    /// Exact original URL whose bytes the RAM route is permitted to serve.
    pub(crate) fn source_url(&self) -> &Url {
        &self.source
    }

    /// Reports an outer proxy's truncated successful response, even after eviction.
    ///
    /// A nested request may safely redirect before its own response begins,
    /// while the outer original-file response has already promised more bytes.
    /// Its owner uses this only for an upstream read failure, not a client close.
    pub(crate) fn mark_stream_failed(&self) {
        self.ticket
            .origin
            .stream_failed
            .store(true, Ordering::Release);
    }
}

impl RamPlaybackCache {
    /// Whether this instance may intercept a source instead of opening a private service.
    pub(crate) fn accepts_source(&self, source: &str) -> bool {
        source.len() <= 8192
            && !source.chars().any(char::is_control)
            && Url::parse(source).is_ok_and(|url| eligible_url(&url, self.shared.allow_loopback))
    }

    /// Starts no upstream request and declines configured proxies or unknown RAM budgets.
    pub(crate) fn new() -> Option<Self> {
        if proxy_configured()
            || super::memory_budget::cache_allowance(0)? < BLOCK_BYTES + SCRATCH_BYTES
        {
            return None;
        }
        Self::start(Box::new(super::memory_budget::cache_allowance), false).ok()
    }

    /// Registers one exact source without performing extraction or downloading it.
    /// Changed URLs/headers under the same key never inherit existing payload bytes.
    #[cfg(test)]
    pub(crate) fn register(
        &self,
        key: &str,
        source: &str,
        headers: &PlaybackHttpHeaders,
    ) -> Option<String> {
        self.register_with_ticket(key, source, headers)
            .map(|(route, _)| route)
    }

    /// Atomically registers or reuses one source and captures its per-load failure state.
    pub(crate) fn register_with_ticket(
        &self,
        key: &str,
        source: &str,
        headers: &PlaybackHttpHeaders,
    ) -> Option<(String, RamCacheTicket)> {
        if key.is_empty()
            || key.len() > 8192
            || source.len() > 8192
            || source.chars().any(char::is_control)
            || !valid_headers(headers)
        {
            return None;
        }
        let source = Url::parse(source).ok()?;
        if !eligible_url(&source, self.shared.allow_loopback) {
            return None;
        }
        let mut state = self.shared.state.lock().ok()?;
        if !self.shared.make_room(&mut state, 0) {
            return None;
        }
        let touched = state.touch();
        if let Some(entry) = state.entries.get_mut(key) {
            if entry.origin.source == source && entry.origin.headers == *headers {
                entry.touched = touched;
                return (!entry.origin.invalidated.load(Ordering::Acquire)).then(|| {
                    (
                        self.shared.url(&entry.origin.route),
                        RamCacheTicket {
                            origin: Arc::clone(&entry.origin),
                        },
                    )
                });
            }
            state.remove(key);
        }
        if state.entries.len() >= MAX_ENTRIES {
            state.remove_oldest_entry()
        }
        // Account for copied keys, source/header strings, both lookup maps and
        // their fixed entry overhead. Payload reservations are separate.
        let metadata_bytes = (key.len() * 3
            + source.as_str().len()
            + headers
                .iter()
                .map(|(name, value)| name.len() + value.len())
                .sum::<usize>()
            + 2048) as u64;
        if !self.shared.make_room(
            &mut state,
            metadata_bytes + BLOCK_BYTES + SCRATCH_BYTES + BLOCK_OVERHEAD,
        ) {
            return None;
        }
        self.shared
            .budget
            .used
            .fetch_add(metadata_bytes, Ordering::AcqRel);
        let number = self.shared.next_route.fetch_add(1, Ordering::Relaxed);
        let route = format!("/{}/{number}", self.shared.token);
        let origin = Arc::new(Origin {
            key: key.into(),
            source,
            headers: headers.clone(),
            route: route.clone(),
            invalidated: AtomicBool::new(false),
            stream_failed: AtomicBool::new(false),
            _reservation: Reservation {
                budget: Arc::clone(&self.shared.budget),
                bytes: metadata_bytes,
                pending: false,
            },
        });
        state.routes.insert(route.clone(), key.into());
        state.entries.insert(
            key.into(),
            Entry {
                origin: Arc::clone(&origin),
                identity: None,
                blocks: BTreeMap::new(),
                touched,
            },
        );
        let _ = self.shared.waker.wake();
        Some((self.shared.url(&route), RamCacheTicket { origin }))
    }

    /// Returns a route only when encoded bytes remain available for this stable key.
    #[cfg(test)]
    pub(crate) fn cached_route(&self, key: &str) -> Option<String> {
        self.cached_route_with_ticket(key, None)
            .map(|(route, _)| route)
    }

    /// A direct input must match its request headers; extractor-backed lookups can
    /// pass `None` before resolving another expiring URL for the same stable key.
    pub(crate) fn cached_route_with_ticket(
        &self,
        key: &str,
        headers: Option<&PlaybackHttpHeaders>,
    ) -> Option<(String, RamCacheTicket)> {
        let mut state = self.shared.state.lock().ok()?;
        if !self.shared.make_room(&mut state, 0) {
            return None;
        }
        let touched = state.touch();
        let entry = state.entries.get_mut(key)?;
        if entry.origin.invalidated.load(Ordering::Acquire)
            || entry.blocks.is_empty()
            || headers.is_some_and(|headers| entry.origin.headers != *headers)
        {
            return None;
        }
        entry.touched = touched;
        Some((
            self.shared.url(&entry.origin.route),
            RamCacheTicket {
                origin: Arc::clone(&entry.origin),
            },
        ))
    }

    /// Obtains the exact owned registration when another private proxy starts a load.
    pub(crate) fn ticket(&self, key: &str) -> Option<RamCacheTicket> {
        let state = self.shared.state.lock().ok()?;
        let entry = state.entries.get(key)?;
        Some(RamCacheTicket {
            origin: Arc::clone(&entry.origin),
        })
    }

    /// Revokes an unsupported/failed representation without exposing its private context.
    pub(crate) fn invalidate(&self, key: &str) {
        if let Ok(mut state) = self.shared.state.lock() {
            if let Some(entry) = state.entries.get_mut(key) {
                entry.origin.invalidated.store(true, Ordering::Release);
            }
            state.clear_blocks(key);
            self.shared.changed.notify_all();
        }
    }

    /// Reports an interrupted successful body, not invalidation, LRU eviction or
    /// a harmless pre-body redirect to a source which mpv could play normally.
    #[cfg(test)]
    pub(crate) fn is_failed(&self, key: &str) -> bool {
        self.shared.state.lock().is_ok_and(|state| {
            state
                .entries
                .get(key)
                .is_some_and(|entry| entry.origin.stream_failed.load(Ordering::Acquire))
        })
    }

    /// Marks only the downstream failure edge for backend lifecycle fixtures.
    #[cfg(test)]
    pub(crate) fn mark_stream_failed(&self, key: &str) {
        if let Ok(state) = self.shared.state.lock()
            && let Some(entry) = state.entries.get(key)
        {
            entry.origin.stream_failed.store(true, Ordering::Release);
        }
    }

    /// Gives fixtures an isolated, deterministic budget and owned loopback upstream access.
    #[cfg(test)]
    pub(crate) fn for_test(limit: u64) -> Option<Self> {
        Self::start(Box::new(move |_| Some(limit)), true).ok()
    }

    fn start(allowance: Box<Allowance>, allow_loopback: bool) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let host = listener.local_addr()?.to_string();
        let poll = mio::Poll::new()?;
        let mut listener = mio::net::TcpListener::from_std(listener);
        poll.registry()
            .register(&mut listener, mio::Token(0), mio::Interest::READABLE)?;
        let waker = mio::Waker::new(poll.registry(), mio::Token(1))?;
        let mut random = [0_u8; 24];
        getrandom::fill(&mut random)
            .map_err(|_| unavailable("cannot create RAM playback capability"))?;
        let token = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            budget: Arc::new(Budget {
                used: AtomicU64::new(0),
                pending: AtomicU64::new(0),
                allowance,
            }),
            stop: AtomicBool::new(false),
            clients: AtomicUsize::new(0),
            agents: Mutex::new(
                (0..MAX_FETCHES)
                    .map(|_| http_agent(allow_loopback))
                    .collect(),
            ),
            waker,
            host,
            token,
            next_route: AtomicU64::new(1),
            allow_loopback,
        });
        let worker = Arc::clone(&shared);
        thread::Builder::new()
            .name("youta-ram-cache".into())
            .spawn(move || listen(poll, listener, worker))?;
        let lifetime = Arc::new(CacheLifetime(Arc::clone(&shared)));
        Ok(Self {
            shared,
            _lifetime: lifetime,
        })
    }

    /// Registers an original source and gives a trusted private proxy an owned capability.
    #[cfg(any(feature = "archive-org", test))]
    pub(crate) fn register_trusted(
        &self,
        key: &str,
        source: &str,
        headers: &PlaybackHttpHeaders,
    ) -> Option<TrustedRamRoute> {
        let (url, ticket) = self.register_with_ticket(key, source, headers)?;
        Some(TrustedRamRoute {
            url,
            source: Url::parse(source).ok()?,
            ticket,
            _owner: self.clone(),
        })
    }
}

impl Drop for CacheLifetime {
    fn drop(&mut self) {
        self.0.stop.store(true, Ordering::Release);
        self.0.changed.notify_all();
        let _ = self.0.waker.wake();
        if let Ok(mut state) = self.0.state.lock() {
            state.entries.clear();
            state.routes.clear();
            state.block_lru.clear();
        }
    }
}

impl Shared {
    fn url(&self, route: &str) -> String {
        format!("http://{}{route}", self.host)
    }

    /// Reservations cover live leases as well as resident LRU payloads. Measurements
    /// are local bounded pseudo-file reads; no network operation holds this lock.
    fn make_room(&self, state: &mut State, required: u64) -> bool {
        let current = self.budget.used.load(Ordering::Acquire);
        let pending = self.budget.pending.load(Ordering::Acquire);
        let allowance = (self.budget.allowance)(current.saturating_sub(pending));
        let maximum = allowance.unwrap_or(0);
        while self
            .budget
            .used
            .load(Ordering::Acquire)
            .checked_add(required)
            .is_none_or(|sum| sum > maximum)
        {
            if !state.evict_one() {
                if state.entries.is_empty() {
                    return false;
                }
                state.remove_oldest_entry();
            }
        }
        allowance.is_some()
    }

    /// Fetches only one missing block, deduplicating concurrent readers and bounding workers.
    fn block(&self, origin: &Arc<Origin>, requested: u64) -> io::Result<(Identity, Arc<Block>)> {
        let deadline = Instant::now() + BLOCK_WAIT;
        loop {
            check_active(&self.stop, deadline)?;
            if origin.invalidated.load(Ordering::Acquire) {
                return Err(unavailable("RAM representation unavailable"));
            }
            let mut state = self
                .state
                .lock()
                .map_err(|_| unavailable("RAM state unavailable"))?;
            if !self.make_room(&mut state, 0) {
                return Err(unavailable("RAM cache pressure"));
            }
            let touched = state.touch();
            let entry = state
                .entries
                .get_mut(&origin.key)
                .filter(|entry| Arc::ptr_eq(&entry.origin, origin))
                .ok_or_else(|| unavailable("RAM registration expired"))?;
            entry.touched = touched;
            if let Some(block) = entry.blocks.get_mut(&requested) {
                let previous = block.touched;
                block.touched = touched;
                let result = (
                    entry
                        .identity
                        .clone()
                        .ok_or_else(|| unavailable("RAM identity unavailable"))?,
                    Arc::clone(&block.data),
                );
                state.block_lru.remove(&previous);
                state
                    .block_lru
                    .insert(touched, (Arc::clone(origin), requested));
                return Ok(result);
            }
            let expected = entry.identity.clone();
            if expected
                .as_ref()
                .is_some_and(|identity| requested > (identity.length - 1) / BLOCK_BYTES)
            {
                return Err(unavailable("RAM block beyond source"));
            }
            let target = if expected.is_none() { 0 } else { requested };
            let ticket = (origin.route.clone(), target);
            if state.fetching.len() < MAX_FETCHES && !state.fetching.contains(&ticket) {
                let reservation_bytes = BLOCK_BYTES + SCRATCH_BYTES + BLOCK_OVERHEAD;
                if !self.make_room(&mut state, reservation_bytes) {
                    return Err(unavailable("RAM fetch budget occupied"));
                }
                self.budget
                    .used
                    .fetch_add(reservation_bytes, Ordering::AcqRel);
                self.budget
                    .pending
                    .fetch_add(reservation_bytes, Ordering::AcqRel);
                let mut reservation = Reservation {
                    budget: Arc::clone(&self.budget),
                    bytes: reservation_bytes,
                    pending: true,
                };
                state.fetching.insert(ticket.clone());
                drop(state);
                let result = fetch_range(self, origin, target, expected.as_ref());
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| unavailable("RAM state unavailable"))?;
                state.fetching.remove(&ticket);
                let touched = state.touch();
                let mut inserted = false;
                let mut failed = false;
                let mut replaced_stamp = None;
                if let Some(entry) = state
                    .entries
                    .get_mut(&origin.key)
                    .filter(|entry| Arc::ptr_eq(&entry.origin, origin))
                {
                    match result {
                        Ok((identity, bytes))
                            if !origin.invalidated.load(Ordering::Acquire)
                                && !self.stop.load(Ordering::Acquire) =>
                        {
                            reservation.shrink(bytes.capacity() as u64 + BLOCK_OVERHEAD);
                            entry.identity = Some(identity);
                            replaced_stamp = entry
                                .blocks
                                .insert(
                                    target,
                                    CachedBlock {
                                        data: Arc::new(Block {
                                            bytes,
                                            _reservation: reservation,
                                        }),
                                        touched,
                                    },
                                )
                                .map(|block| block.touched);
                            inserted = true;
                        }
                        _ => {
                            origin.invalidated.store(true, Ordering::Release);
                            failed = true;
                        }
                    }
                }
                if let Some(previous) = replaced_stamp {
                    state.block_lru.remove(&previous);
                }
                if inserted {
                    state
                        .block_lru
                        .insert(touched, (Arc::clone(origin), target));
                }
                if failed {
                    state.clear_blocks(&origin.key);
                }
                self.changed.notify_all();
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            drop(
                self.changed
                    .wait_timeout(state, remaining)
                    .map_err(|_| unavailable("RAM cache wait failed"))?,
            );
        }
    }
}

/// Ambient routing preferences must not be bypassed by a direct connection pool.
fn proxy_configured() -> bool {
    [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ]
    .into_iter()
    .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
}

fn valid_headers(headers: &PlaybackHttpHeaders) -> bool {
    headers.iter().len() <= 32
        && headers
            .iter()
            .try_fold(0_usize, |size, (name, value)| {
                if name.is_empty()
                    || !name.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
                    })
                    || value.bytes().any(|byte| byte < 0x20 || byte == 0x7f)
                    || ["host", "content-length", "transfer-encoding", "connection"]
                        .iter()
                        .any(|reserved| name.eq_ignore_ascii_case(reserved))
                {
                    return None;
                }
                size.checked_add(name.len())?
                    .checked_add(value.len())
                    .filter(|size| *size <= MAX_HEADERS)
            })
            .is_some()
}

fn eligible_url(url: &Url, allow_loopback: bool) -> bool {
    if !matches!(url.scheme(), "http" | "https")
        || url.as_str().len() > 8192
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    let path = url.path().to_ascii_lowercase();
    // Archive's ZIP member endpoint decompresses on demand and ignores Range.
    // Do not fetch it speculatively only to redirect and decompress it again.
    if url
        .host_str()
        .is_some_and(|host| host == "archive.org" || host.ends_with(".archive.org"))
        && path.contains(".zip/")
    {
        return false;
    }
    if [".m3u", ".m3u8", ".mpd", ".ism", ".isml"]
        .iter()
        .any(|extension| path.ends_with(extension))
    {
        return false;
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) => {
            allow_loopback && ip.is_loopback()
                || !crate::domain::ip_address_is_non_public(ip.into())
        }
        Some(url::Host::Ipv6(ip)) => {
            allow_loopback && ip.is_loopback()
                || !crate::domain::ip_address_is_non_public(ip.into())
        }
        Some(url::Host::Domain(host)) => {
            !(host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost"))
        }
        None => false,
    }
}

/// DNS filtering is applied to the actual connecting resolver, including redirects.
#[derive(Debug)]
struct PublicResolver {
    inner: DefaultResolver,
    allow_loopback: bool,
}

impl Resolver for PublicResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        let addresses = self.inner.resolve(uri, config, timeout)?;
        let mut accepted = self.empty();
        for address in &addresses {
            if self.allow_loopback && address.ip().is_loopback()
                || !crate::domain::ip_address_is_non_public(address.ip())
            {
                accepted.push(*address);
            }
        }
        if accepted.is_empty() {
            Err(ureq::Error::HostNotFound)
        } else {
            Ok(accepted)
        }
    }
}

fn http_agent(allow_loopback: bool) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .max_redirects(0)
        .max_response_header_size(MAX_HEADERS)
        .max_idle_connections(2)
        .max_idle_connections_per_host(1)
        .http_status_as_error(false)
        .proxy(None)
        .user_agent(concat!("youta/", env!("CARGO_PKG_VERSION")))
        .build();
    ureq::Agent::with_parts(
        config,
        DefaultConnector::default(),
        PublicResolver {
            inner: DefaultResolver::default(),
            allow_loopback,
        },
    )
}

/// Exclusive leases prevent optional cookie-jar state from crossing source requests.
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
                agents.push(agent)
            }
        }
    }
}

/// Accepts strong ETags or the RFC9110 client-side strong Last-Modified test.
fn validator(etag: Option<&str>, modified: Option<&str>, date: Option<&str>) -> Option<Validator> {
    if let Some(etag) = etag.filter(|value| {
        value.len() >= 2
            && value.len() <= 512
            && value.starts_with('"')
            && value.ends_with('"')
            && value[1..value.len() - 1]
                .bytes()
                .all(|byte| (0x21..0x7f).contains(&byte) && byte != b'"')
    }) {
        return Some(Validator::Etag(etag.into()));
    }
    let modified = modified.filter(|value| {
        value.len() <= 128 && value.bytes().all(|byte| (0x20..0x7f).contains(&byte))
    })?;
    let modified_time = chrono::DateTime::parse_from_rfc2822(modified)
        .ok()?
        .timestamp();
    let date_time = chrono::DateTime::parse_from_rfc2822(date?)
        .ok()?
        .timestamp();
    (date_time.checked_sub(modified_time)? >= 60).then(|| Validator::Modified(modified.into()))
}

fn fetch_range(
    shared: &Shared,
    origin: &Origin,
    index: u64,
    expected: Option<&Identity>,
) -> io::Result<(Identity, Vec<u8>)> {
    let deadline = Instant::now() + FETCH_TIMEOUT;
    let lease = AgentLease {
        shared,
        agent: Some(
            shared
                .agents
                .lock()
                .map_err(|_| unavailable("RAM HTTP pool failed"))?
                .pop()
                .ok_or_else(|| unavailable("RAM HTTP pool occupied"))?,
        ),
    };
    let agent = lease.agent.as_ref().expect("leased agent");
    let start = index
        .checked_mul(BLOCK_BYTES)
        .ok_or_else(|| unavailable("RAM range overflow"))?;
    let end = start
        .checked_add(BLOCK_BYTES - 1)
        .ok_or_else(|| unavailable("RAM range overflow"))?;
    let mut current = expected.map_or_else(
        || origin.source.clone(),
        |identity| identity.effective.clone(),
    );
    for redirects in 0..=3 {
        check_active(&shared.stop, deadline)?;
        if origin.invalidated.load(Ordering::Acquire)
            || !eligible_url(&current, shared.allow_loopback)
            || (!origin.headers.is_empty() && current.origin() != origin.source.origin())
        {
            return Err(unavailable("RAM redirect or representation unavailable"));
        }
        #[cfg(feature = "commons-upload")]
        agent.cookie_jar_lock().clear();
        let mut request = agent.get(current.as_str());
        for (name, value) in origin.headers.iter() {
            if !["range", "if-range", "accept-encoding"]
                .iter()
                .any(|reserved| name.eq_ignore_ascii_case(reserved))
            {
                request = request.header(name, value);
            }
        }
        request = request
            .header("Accept-Encoding", "identity")
            .header("Range", format!("bytes={start}-{end}"));
        if let Some(identity) = expected {
            request = request.header("If-Range", identity.validator.value())
        }
        let response = request
            .config()
            .timeout_global(Some(deadline.saturating_duration_since(Instant::now())))
            .build()
            .call();
        #[cfg(feature = "commons-upload")]
        agent.cookie_jar_lock().clear();
        let mut response = response.map_err(|_| unavailable("RAM upstream request failed"))?;
        let status = response.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            if redirects == 3 {
                return Err(unavailable("RAM redirect limit"));
            }
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .filter(|value| value.len() <= 8192)
                .ok_or_else(|| unavailable("invalid RAM redirect"))?;
            current = current
                .join(location)
                .map_err(|_| unavailable("invalid RAM redirect"))?;
            continue;
        }
        if status != 206
            || response
                .headers()
                .get("content-encoding")
                .is_some_and(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
        {
            return Err(unavailable("RAM source does not provide original ranges"));
        }
        let header = |name| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
        };
        let valid = validator(header("etag"), header("last-modified"), header("date"))
            .ok_or_else(|| unavailable("RAM source has no strong validator"))?;
        let (actual_start, actual_end, length) = header("content-range")
            .and_then(content_range)
            .ok_or_else(|| unavailable("invalid RAM content range"))?;
        if length == 0
            || actual_start != start
            || actual_end != end.min(length - 1)
            || expected.is_some_and(|old| {
                old.length != length || old.validator != valid || old.effective != current
            })
        {
            return Err(unavailable("RAM representation changed"));
        }
        let content_type = header("content-type")
            .filter(|value| {
                value.len() <= 128 && value.bytes().all(|byte| (0x20..0x7f).contains(&byte))
            })
            .unwrap_or("application/octet-stream")
            .to_owned();
        let lower_type = content_type.to_ascii_lowercase();
        if lower_type.contains("mpegurl")
            || lower_type.contains("dash+xml")
            || lower_type.starts_with("text/")
            || lower_type.contains("json")
        {
            return Err(unavailable("RAM source is not progressive media"));
        }
        let wanted = usize::try_from(actual_end - actual_start + 1)
            .map_err(|_| unavailable("RAM range too large"))?;
        if response.body().content_length() != Some(wanted as u64) {
            return Err(unavailable("RAM body length mismatch"));
        }
        let mut bytes = Vec::with_capacity(wanted);
        let mut reader = response.body_mut().as_reader();
        let mut scratch = [0_u8; SCRATCH_BYTES as usize];
        loop {
            check_active(&shared.stop, deadline)?;
            let count = reader
                .read(&mut scratch[..(wanted + 1 - bytes.len()).min(SCRATCH_BYTES as usize)])?;
            if count == 0 {
                break;
            }
            if bytes.len() + count > wanted {
                return Err(unavailable("RAM body exceeds range"));
            }
            bytes.extend_from_slice(&scratch[..count]);
        }
        if bytes.len() != wanted {
            return Err(unavailable("RAM range incomplete"));
        }
        return Ok((
            Identity {
                effective: current,
                validator: valid,
                length,
                content_type,
            },
            bytes,
        ));
    }
    Err(unavailable("RAM redirects exhausted"))
}

fn decimal(value: &str) -> Option<u64> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

fn content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, length) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, length) = (decimal(start)?, decimal(end)?, decimal(length)?);
    (start <= end && end < length).then_some((start, end, length))
}

fn single_range(value: &str, length: u64) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    if value.contains(',') || length == 0 {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        let count = decimal(end)?.min(length);
        return (count > 0).then_some((length - count, length - 1));
    }
    let start = decimal(start)?;
    let end = if end.is_empty() {
        length - 1
    } else {
        decimal(end)?.min(length - 1)
    };
    (start <= end && start < length).then_some((start, end))
}

struct ClientPermit(Arc<Shared>);
impl Drop for ClientPermit {
    fn drop(&mut self) {
        self.0.clients.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Empty caches sleep on socket readiness. Retained bytes get a five-second
/// pressure check without waking or repainting the frontend.
fn listen(mut poll: mio::Poll, listener: mio::net::TcpListener, shared: Arc<Shared>) {
    let mut events = mio::Events::with_capacity(4);
    while !shared.stop.load(Ordering::Acquire) {
        let timeout =
            (shared.budget.used.load(Ordering::Acquire) != 0).then_some(Duration::from_secs(5));
        if let Err(error) = poll.poll(&mut events, timeout) {
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if let Ok(mut state) = shared.state.lock() {
            shared.make_room(&mut state, 0);
            shared.changed.notify_all();
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
                        .name("youta-ram-reader".into())
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
    stream.set_nonblocking(true)?;
    let request = read_headers(&mut stream, &shared.stop)?;
    let mut lines = request.lines();
    let mut first = lines.next().unwrap_or_default().split_whitespace();
    let method = first.next().unwrap_or_default();
    let path = first.next().unwrap_or_default();
    let version = first.next().unwrap_or_default();
    if !matches!(method, "GET" | "HEAD")
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || first.next().is_some()
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
        let target = if name.eq_ignore_ascii_case("host") {
            &mut host
        } else if name.eq_ignore_ascii_case("range") {
            &mut range
        } else if name.eq_ignore_ascii_case("if-range") {
            &mut if_range
        } else {
            continue;
        };
        if target.replace(value.trim()).is_some() {
            return response(&mut stream, "400 Bad Request", "", &shared.stop);
        }
    }
    if host != Some(shared.host.as_str()) {
        return response(&mut stream, "404 Not Found", "", &shared.stop);
    }
    let origin = {
        let state = shared
            .state
            .lock()
            .map_err(|_| unavailable("RAM state unavailable"))?;
        state
            .routes
            .get(path)
            .and_then(|key| state.entries.get(key))
            .map(|entry| Arc::clone(&entry.origin))
    };
    let Some(origin) = origin else {
        return response(&mut stream, "404 Not Found", "", &shared.stop);
    };
    let identity = shared.state.lock().ok().and_then(|state| {
        state
            .entries
            .get(&origin.key)
            .and_then(|entry| entry.identity.clone())
    });
    let identity = match identity {
        Some(identity) => identity,
        None => match shared.block(&origin, 0) {
            Ok((identity, _)) => identity,
            Err(_) => return redirect(&mut stream, shared, &origin),
        },
    };
    let range = range.filter(|_| if_range.is_none_or(|value| value == identity.validator.value()));
    let (start, end) = if let Some(range) = range {
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
    // Fetch the first requested block before emitting success headers, so a miss
    // can still redirect instead of turning an optimization into a playback error.
    let Ok((_, first_block)) = shared.block(&origin, start / BLOCK_BYTES) else {
        return redirect(&mut stream, shared, &origin);
    };
    let status = if range.is_some() {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let range_header = if range.is_some() {
        format!("Content-Range: bytes {start}-{end}/{}\r\n", identity.length)
    } else {
        String::new()
    };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n{}: {}\r\n{range_header}Cache-Control: no-store\r\nConnection: close\r\n\r\n",
        identity.content_type,
        end - start + 1,
        identity.validator.header(),
        identity.validator.value()
    );
    let lifetime = Instant::now() + CLIENT_LIFETIME;
    write_bytes(&mut stream, header.as_bytes(), &shared.stop, lifetime)?;
    if method == "GET" {
        let mut position = start;
        let mut first_block = Some(first_block);
        while position <= end {
            if origin.invalidated.load(Ordering::Acquire) {
                origin.stream_failed.store(true, Ordering::Release);
                return Err(unavailable("RAM representation revoked"));
            }
            let index = position / BLOCK_BYTES;
            let block = match first_block.take() {
                Some(block) => block,
                None => match shared.block(&origin, index) {
                    Ok((_, block)) => block,
                    Err(error) => {
                        origin.invalidated.store(true, Ordering::Release);
                        origin.stream_failed.store(true, Ordering::Release);
                        if let Ok(mut state) = shared.state.lock() {
                            if state
                                .entries
                                .get(&origin.key)
                                .is_some_and(|entry| Arc::ptr_eq(&entry.origin, &origin))
                            {
                                state.clear_blocks(&origin.key);
                            }
                        }
                        return Err(error);
                    }
                },
            };
            let offset = usize::try_from(position % BLOCK_BYTES)
                .map_err(|_| unavailable("RAM block offset"))?;
            let remaining = usize::try_from((end - position + 1).min(BLOCK_BYTES))
                .map_err(|_| unavailable("RAM block length"))?;
            let count = block.bytes.len().saturating_sub(offset).min(remaining);
            if count == 0 {
                origin.stream_failed.store(true, Ordering::Release);
                return Err(unavailable("RAM block empty"));
            }
            write_bytes(
                &mut stream,
                &block.bytes[offset..offset + count],
                &shared.stop,
                lifetime,
            )?;
            position += count as u64;
        }
    }
    stream.shutdown(Shutdown::Write)
}

fn read_headers(stream: &mut TcpStream, stop: &AtomicBool) -> io::Result<String> {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut bytes = Vec::new();
    let mut scratch = [0_u8; 1024];
    loop {
        check_active(stop, deadline)?;
        match stream.read(&mut scratch) {
            Ok(0) => return Err(unavailable("incomplete RAM request")),
            Ok(count) => {
                bytes.extend_from_slice(&scratch[..count]);
                if bytes.len() > MAX_HEADERS {
                    return Err(unavailable("RAM request too large"));
                }
                if let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    bytes.truncate(end + 4);
                    return String::from_utf8(bytes)
                        .map_err(|_| unavailable("invalid RAM request"));
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
            Ok(0) => return Err(unavailable("RAM response ended")),
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

fn redirect(stream: &mut TcpStream, shared: &Shared, origin: &Origin) -> io::Result<()> {
    response(
        stream,
        "307 Temporary Redirect",
        &format!("Location: {}\r\nCache-Control: no-store\r\n", origin.source),
        &shared.stop,
    )
}

fn check_active(stop: &AtomicBool, deadline: Instant) -> io::Result<()> {
    if stop.load(Ordering::Acquire) {
        return Err(io::ErrorKind::Interrupted.into());
    }
    if Instant::now() >= deadline {
        return Err(io::ErrorKind::TimedOut.into());
    }
    Ok(())
}

fn unavailable(message: &'static str) -> io::Error {
    io::Error::other(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owned loopback fixture; no real source, credentials, environment or DNS involved.
    struct Source {
        url: String,
        requests: Arc<AtomicUsize>,
        mode: Arc<AtomicUsize>,
        seen: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl Source {
        fn new(length: usize) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}/audio.mp3", listener.local_addr().unwrap());
            let requests = Arc::new(AtomicUsize::new(0));
            let mode = Arc::new(AtomicUsize::new(0));
            let seen = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let worker_requests = Arc::clone(&requests);
            let worker_mode = Arc::clone(&mode);
            let worker_seen = Arc::clone(&seen);
            let worker_stop = Arc::clone(&stop);
            let worker = thread::spawn(move || {
                let bytes = vec![b'a'; length];
                while !worker_stop.load(Ordering::Acquire) {
                    let mut stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") && request.len() < MAX_HEADERS {
                        let mut byte = [0];
                        if stream.read_exact(&mut byte).is_err() {
                            break;
                        }
                        request.push(byte[0]);
                    }
                    let request = String::from_utf8(request).unwrap();
                    worker_seen.lock().unwrap().push(request.clone());
                    worker_requests.fetch_add(1, Ordering::AcqRel);
                    let mode = worker_mode.load(Ordering::Acquire);
                    let headers = request
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
                        .collect::<HashMap<_, _>>();
                    let etag = if mode == 4 { "\"new\"" } else { "\"fixture\"" };
                    let range = headers
                        .get("range")
                        .and_then(|range| single_range(range, length as u64));
                    let changed =
                        mode == 4 && headers.get("if-range").is_some_and(|value| value != etag);
                    let fail_later = mode == 5 && headers.contains_key("if-range");
                    let result = if mode == 1 || changed || fail_later || range.is_none() {
                        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: {length}\r\nETag: {etag}\r\nConnection: close\r\n\r\n").and_then(|()| stream.write_all(&bytes))
                    } else {
                        let (start, end) = range.unwrap();
                        let validation = match mode {
                            2 => "ETag: W/\"weak\"\r\n".to_owned(),
                            3 => "Last-Modified: Wed, 21 Oct 2015 07:28:00 GMT\r\nDate: Wed, 21 Oct 2015 07:29:00 GMT\r\n".to_owned(),
                            _ => format!("ETag: {etag}\r\n"),
                        };
                        write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Type: audio/mpeg\r\nContent-Range: bytes {start}-{end}/{length}\r\nContent-Length: {}\r\n{validation}Connection: close\r\n\r\n", end - start + 1).and_then(|()| stream.write_all(&bytes[start as usize..=end as usize]))
                    };
                    let _ = result;
                }
            });
            Self {
                url,
                requests,
                mode,
                seen,
                stop,
                worker: Some(worker),
            }
        }
    }

    impl Drop for Source {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap()
            }
        }
    }

    fn request(route: &str, range: &str) -> (String, Vec<u8>) {
        let url = Url::parse(route).unwrap();
        let host = format!("{}:{}", url.host_str().unwrap(), url.port().unwrap());
        let mut stream = TcpStream::connect(&host).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET {} HTTP/1.1\r\nHost: {host}\r\nRange: {range}\r\nConnection: close\r\n\r\n",
            url.path()
        )
        .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let split = response
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap()
            + 4;
        (
            String::from_utf8(response[..split].to_owned()).unwrap(),
            response[split..].to_owned(),
        )
    }

    fn wait_for_clients(cache: &RamPlaybackCache) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while cache.shared.clients.load(Ordering::Acquire) != 0 {
            assert!(Instant::now() < deadline, "fixture readers did not finish");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// A capability route is local, lazy, stable for one input, and not its public identity.
    #[test]
    fn registration_is_lazy_and_does_not_expose_the_upstream_url() {
        let cache = RamPlaybackCache::for_test(4 * BLOCK_BYTES).unwrap();
        let source = "https://media.example/audio.mp3?token=private";
        let headers = PlaybackHttpHeaders::default();
        let route = cache.register("stable-track", source, &headers).unwrap();
        assert!(route.starts_with("http://127.0.0.1:"));
        assert!(!route.contains("private"));
        assert_eq!(
            cache.register("stable-track", source, &headers),
            Some(route)
        );
        assert_eq!(cache.cached_route("stable-track"), None);
        assert!(
            cache.shared.budget.used.load(Ordering::Acquire) < 8192,
            "registration retains only bounded identity metadata, no audio"
        );
    }

    /// Explicitly unsupported inputs are passed to mpv without speculative requests.
    #[test]
    fn rejects_private_targets_manifests_and_injected_headers() {
        for source in [
            "file:///etc/passwd",
            "https://user:pass@example.test/a.mp3",
            "https://127.0.0.1/a.mp3",
            "http://[::1]/a.mp3",
            "https://example.test/a.m3u8?token=x",
            "https://example.test/a.MPD",
            "https://archive.org/download/item/songs.zip/track.mp3",
        ] {
            assert!(
                !eligible_url(&Url::parse(source).unwrap(), false),
                "accepted {source}"
            );
        }
        let cache = RamPlaybackCache::for_test(4 * BLOCK_BYTES).unwrap();
        let headers = PlaybackHttpHeaders::new(std::collections::BTreeMap::from([(
            "Authorization".into(),
            "secret\r\nInjected: yes".into(),
        )]));
        assert!(
            cache
                .register("id", "https://example.test/a.mp3", &headers)
                .is_none()
        );
    }

    /// HTTP's date validator is strong only when the response Date is at least 60s newer.
    #[test]
    fn accepts_only_strong_representation_validators() {
        let date = "Wed, 21 Oct 2015 07:29:00 GMT";
        let modified = "Wed, 21 Oct 2015 07:28:00 GMT";
        assert_eq!(
            validator(Some("\"one\""), None, None),
            Some(Validator::Etag("\"one\"".into()))
        );
        assert_eq!(
            validator(Some("W/\"one\""), Some(modified), Some(date)),
            Some(Validator::Modified(modified.into()))
        );
        assert_eq!(
            validator(None, Some(modified), Some("Wed, 21 Oct 2015 07:28:59 GMT")),
            None
        );
        assert_eq!(validator(Some("\"bad\r\n\""), None, None), None);
        assert_eq!(validator(None, Some(modified), None), None);
    }

    /// Strict single-range handling cannot merge different representations.
    #[test]
    fn validates_range_and_length_boundaries() {
        assert_eq!(content_range("bytes 0-511/1024"), Some((0, 511, 1024)));
        assert_eq!(content_range("bytes 0-1024/1024"), None);
        assert_eq!(content_range("bytes +0-1/2"), None);
        assert_eq!(single_range("bytes=8-", 10), Some((8, 9)));
        assert_eq!(single_range("bytes=-2", 10), Some((8, 9)));
        assert_eq!(single_range("bytes=0-99", 10), Some((0, 9)));
        assert_eq!(single_range("bytes=0-1,3-4", 10), None);
        assert_eq!(single_range("bytes=10-", 10), None);
    }

    /// A -> B -> A reuses encoded blocks and does not contact the first source again.
    #[test]
    fn retains_multiple_tracks_and_fetches_only_demanded_blocks() {
        let source = Source::new(BLOCK_BYTES as usize * 3);
        let cache = RamPlaybackCache::for_test(8 * BLOCK_BYTES).unwrap();
        let a = cache
            .register("A", &source.url, &PlaybackHttpHeaders::default())
            .unwrap();
        let b = cache
            .register(
                "B",
                &format!("{}?other", source.url),
                &PlaybackHttpHeaders::default(),
            )
            .unwrap();
        for route in [&a, &b, &a] {
            let (head, bytes) = request(route, "bytes=0-15");
            assert!(head.starts_with("HTTP/1.1 206"), "{head}");
            assert_eq!(bytes, vec![b'a'; 16]);
        }
        assert_eq!(source.requests.load(Ordering::Acquire), 2);
        assert_eq!(cache.cached_route("A"), Some(a.clone()));
        let offset = BLOCK_BYTES * 2 + 7;
        assert_eq!(
            request(&a, &format!("bytes={offset}-{}", offset + 15)).1,
            vec![b'a'; 16]
        );
        assert_eq!(source.requests.load(Ordering::Acquire), 3);
        let state = cache.shared.state.lock().unwrap();
        assert_eq!(
            state.entries["A"]
                .blocks
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
    }

    /// Absent range support and weak validators safely fall back before media headers.
    #[test]
    fn unsupported_responses_redirect_to_original() {
        for mode in [1, 2] {
            let source = Source::new(1024);
            source.mode.store(mode, Ordering::Release);
            let cache = RamPlaybackCache::for_test(4 * BLOCK_BYTES).unwrap();
            let route = cache
                .register("A", &source.url, &PlaybackHttpHeaders::default())
                .unwrap();
            let (head, bytes) = request(&route, "bytes=0-15");
            assert!(head.starts_with("HTTP/1.1 307"), "{head}");
            assert!(head.contains(&format!("Location: {}\r\n", source.url)));
            assert!(bytes.is_empty());
            assert!(cache.cached_route("A").is_none());
            assert!(!cache.is_failed("A"));
        }
    }

    /// If-Range prevents blocks from a changed file being joined to an older version.
    #[test]
    fn changed_representation_invalidates_all_cached_blocks() {
        let source = Source::new(BLOCK_BYTES as usize * 2);
        let cache = RamPlaybackCache::for_test(4 * BLOCK_BYTES).unwrap();
        let route = cache
            .register("A", &source.url, &PlaybackHttpHeaders::default())
            .unwrap();
        assert_eq!(request(&route, "bytes=0-15").1, vec![b'a'; 16]);
        source.mode.store(4, Ordering::Release);
        let (head, bytes) = request(&route, &format!("bytes={BLOCK_BYTES}-{}", BLOCK_BYTES + 15));
        assert!(head.starts_with("HTTP/1.1 307"), "{head}");
        assert!(bytes.is_empty());
        assert!(cache.cached_route("A").is_none());
        assert!(!cache.is_failed("A"));
        assert!(
            source.seen.lock().unwrap()[1]
                .to_ascii_lowercase()
                .contains("if-range: \"fixture\"")
        );
    }

    /// A truncated body needs backend recovery; an earlier redirect does not.
    #[test]
    fn distinguishes_prebody_redirects_from_truncated_successful_bodies() {
        let source = Source::new(BLOCK_BYTES as usize * 2);
        source.mode.store(5, Ordering::Release);
        let cache = RamPlaybackCache::for_test(4 * BLOCK_BYTES).unwrap();
        let (route, ticket) = cache
            .register_with_ticket("A", &source.url, &PlaybackHttpHeaders::default())
            .unwrap();
        let (head, bytes) = request(&route, &format!("bytes=0-{}", 2 * BLOCK_BYTES - 1));
        assert!(head.starts_with("HTTP/1.1 206"), "{head}");
        assert_eq!(bytes.len(), BLOCK_BYTES as usize);
        assert!(ticket.is_failed());
        assert!(ticket.matches_source(&source.url));
        assert!(!ticket.matches_source("https://example.org/different.mp3"));
        let mut state = cache.shared.state.lock().unwrap();
        assert!(state.block_lru.is_empty());
        state.remove("A");
        drop(state);
        assert!(
            ticket.is_failed(),
            "eviction must retain the per-load signal"
        );
    }

    /// Revoking a route alone must not restart a player which followed a safe redirect.
    #[test]
    fn ordinary_invalidation_does_not_mark_a_stream_failed() {
        let cache = RamPlaybackCache::for_test(4 * BLOCK_BYTES).unwrap();
        let (_, ticket) = cache
            .register_with_ticket(
                "A",
                "https://example.org/audio.mp3",
                &PlaybackHttpHeaders::default(),
            )
            .unwrap();
        cache.invalidate("A");
        assert!(!ticket.is_failed());
        assert!(!cache.is_failed("A"));
        cache.mark_stream_failed("A");
        assert!(ticket.is_failed());
    }

    /// A direct replay cannot reuse encoded bytes from another authentication context.
    #[test]
    fn warm_direct_routes_require_matching_headers() {
        let source = Source::new(1024);
        let cache = RamPlaybackCache::for_test(4 * BLOCK_BYTES).unwrap();
        let first = PlaybackHttpHeaders::new(BTreeMap::from([(
            "Authorization".into(),
            "Bearer first".into(),
        )]));
        let second = PlaybackHttpHeaders::new(BTreeMap::from([(
            "Authorization".into(),
            "Bearer second".into(),
        )]));
        let route = cache.register("A", &source.url, &first).unwrap();
        assert_eq!(request(&route, "bytes=0-15").1, vec![b'a'; 16]);
        assert!(cache.cached_route_with_ticket("A", Some(&second)).is_none());
        assert_eq!(
            cache
                .cached_route_with_ticket("A", Some(&first))
                .map(|(route, _)| route),
            Some(route.clone())
        );
        assert_eq!(
            cache
                .cached_route_with_ticket("A", None)
                .map(|(route, _)| route),
            Some(route)
        );
    }

    /// Strong Last-Modified works for progressive sources without an ETag.
    #[test]
    fn conditional_ranges_use_the_strong_modification_date() {
        let source = Source::new(BLOCK_BYTES as usize * 2);
        source.mode.store(3, Ordering::Release);
        let cache = RamPlaybackCache::for_test(4 * BLOCK_BYTES).unwrap();
        let route = cache
            .register("A", &source.url, &PlaybackHttpHeaders::default())
            .unwrap();
        let (head, _) = request(&route, "bytes=0-15");
        assert!(
            head.contains("Last-Modified: Wed, 21 Oct 2015 07:28:00 GMT"),
            "{head}"
        );
        assert_eq!(
            request(&route, &format!("bytes={BLOCK_BYTES}-{}", BLOCK_BYTES + 15)).1,
            vec![b'a'; 16]
        );
        assert!(
            source.seen.lock().unwrap()[1]
                .to_ascii_lowercase()
                .contains("if-range: wed, 21 oct 2015 07:28:00 gmt")
        );
    }

    /// LRU eviction counts pinned readers; dropping a lease eventually releases RAM.
    #[test]
    fn pressure_evicts_least_recent_blocks_and_accounts_for_live_leases() {
        let limit = Arc::new(AtomicU64::new(8 * BLOCK_BYTES));
        let budget = Arc::clone(&limit);
        let cache = RamPlaybackCache::start(
            Box::new(move |_| Some(budget.load(Ordering::Acquire))),
            true,
        )
        .unwrap();
        let source = Source::new(BLOCK_BYTES as usize * 3);
        let route = cache
            .register("A", &source.url, &PlaybackHttpHeaders::default())
            .unwrap();
        for index in [0, 1, 0, 2] {
            let start = index * BLOCK_BYTES;
            assert_eq!(
                request(&route, &format!("bytes={start}-{}", start + 15))
                    .1
                    .len(),
                16
            );
        }
        wait_for_clients(&cache);
        let previous = cache.shared.budget.used.load(Ordering::Acquire);
        limit.store(previous - BLOCK_BYTES - BLOCK_OVERHEAD, Ordering::Release);
        assert_eq!(cache.cached_route("A"), Some(route.clone()));
        let lease = {
            let state = cache.shared.state.lock().unwrap();
            let blocks = &state.entries["A"].blocks;
            assert_eq!(blocks.keys().copied().collect::<Vec<_>>(), vec![0, 2]);
            Arc::clone(&blocks[&0].data)
        };
        // No refetch of evicted block zero is necessary to read another retained range.
        let hits = source.requests.load(Ordering::Acquire);
        assert_eq!(
            request(
                &route,
                &format!("bytes={}-{}", BLOCK_BYTES * 2, BLOCK_BYTES * 2 + 15)
            )
            .1
            .len(),
            16
        );
        assert_eq!(source.requests.load(Ordering::Acquire), hits);
        wait_for_clients(&cache);
        limit.store(0, Ordering::Release);
        assert!(cache.cached_route("A").is_none());
        assert_eq!(
            cache.shared.budget.used.load(Ordering::Acquire),
            BLOCK_BYTES + BLOCK_OVERHEAD
        );
        drop(lease);
        assert_eq!(cache.shared.budget.used.load(Ordering::Acquire), 0);
    }

    /// Unallocated reservations cannot be treated as already-resident reclaimable cache.
    #[test]
    fn concurrent_transfer_reservations_do_not_create_extra_headroom() {
        let cache =
            RamPlaybackCache::start(Box::new(|current| Some(current + BLOCK_BYTES)), true).unwrap();
        cache
            .shared
            .budget
            .used
            .store(BLOCK_BYTES, Ordering::Release);
        cache
            .shared
            .budget
            .pending
            .store(BLOCK_BYTES, Ordering::Release);
        let pending = Reservation {
            budget: Arc::clone(&cache.shared.budget),
            bytes: BLOCK_BYTES,
            pending: true,
        };
        assert!(
            !cache
                .shared
                .make_room(&mut cache.shared.state.lock().unwrap(), 1)
        );
        drop(pending);
        assert_eq!(cache.shared.budget.used.load(Ordering::Acquire), 0);
        assert_eq!(cache.shared.budget.pending.load(Ordering::Acquire), 0);
    }

    /// Typed route ownership keeps its listener and bytes alive after the original handle drops.
    #[test]
    fn trusted_route_owns_listener_and_exact_source_identity() {
        let source = Source::new(1024);
        let cache = RamPlaybackCache::for_test(4 * BLOCK_BYTES).unwrap();
        let trusted = cache
            .register_trusted("A", &source.url, &PlaybackHttpHeaders::default())
            .unwrap();
        assert_eq!(trusted.source_url().as_str(), source.url);
        let ticket = cache.ticket("A").unwrap();
        cache.shared.state.lock().unwrap().remove("A");
        trusted.mark_stream_failed();
        assert!(ticket.is_failed());
    }

    /// Trusted proxies also keep their listener available after the application handle drops.
    #[test]
    fn trusted_route_keeps_listener_alive() {
        let source = Source::new(1024);
        let cache = RamPlaybackCache::for_test(4 * BLOCK_BYTES).unwrap();
        let trusted = cache
            .register_trusted("A", &source.url, &PlaybackHttpHeaders::default())
            .unwrap();
        drop(cache);
        assert_eq!(request(trusted.url(), "bytes=0-15").1, vec![b'a'; 16]);
    }
}
