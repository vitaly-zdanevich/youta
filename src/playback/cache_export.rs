//! Best-effort exports of an already complete, finite mpv audio cache.
//!
//! A cache dump is a remux, not an original-file copy. Returned files remain
//! unvalidated until the caller checks their codec, packet timeline and length.
//! Nothing here seeks, pauses, extends the cache, or starts network buffering.

use std::path::{Path, PathBuf};
#[cfg(any(feature = "backend-mpv", test))]
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

/// Includes connection, bounded property checks and the experimental dump.
#[cfg(all(unix, feature = "backend-mpv"))]
const EXPORT_TIMEOUT: Duration = Duration::from_secs(3);

/// Encoded audio formats supported by the initial cache fast path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheAudioCodec {
    /// Opus packets, without decoding or re-encoding.
    Opus,
}

/// Opaque cache miss; diagnostics deliberately contain no source URLs or headers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheExportMiss {
    /// The cache is incomplete, unsupported or belongs to a different load.
    Unavailable,
    /// Export was cancelled or its loaded-media identity changed.
    Cancelled,
    /// IPC, temporary storage or mpv's experimental dump command failed.
    Failed,
}

impl std::fmt::Display for CacheExportMiss {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "Complete playback cache is unavailable",
            Self::Cancelled => "Playback cache export cancelled",
            Self::Failed => "Playback cache export failed",
        })
    }
}

impl std::error::Error for CacheExportMiss {}

/// A ticket bound to one already loaded input, player instance and load epoch.
#[derive(Clone)]
pub struct PlaybackCacheHandle {
    state: Arc<CacheState>,
    epoch: u64,
    #[cfg(all(unix, feature = "backend-mpv"))]
    location: Arc<str>,
    supervisor: Option<(Arc<AtomicU64>, u64)>,
}

impl std::fmt::Debug for PlaybackCacheHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PlaybackCacheHandle([REDACTED])")
    }
}

struct CacheState {
    #[cfg(all(unix, feature = "backend-mpv"))]
    pid: u32,
    #[cfg(all(unix, feature = "backend-mpv"))]
    endpoint: PathBuf,
    #[cfg(all(unix, feature = "backend-mpv"))]
    directory: PathBuf,
    epoch: AtomicU64,
    alive: AtomicBool,
    loaded: AtomicBool,
    #[cfg(any(feature = "backend-mpv", test))]
    input: Mutex<Arc<str>>,
    /// A timed-out dump may still be finishing inside mpv; do not start another.
    #[cfg(all(unix, feature = "backend-mpv"))]
    tainted_epoch: AtomicU64,
}

impl PlaybackCacheHandle {
    /// Whether this ticket still describes the same live, loaded player input.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.state.alive.load(Ordering::Acquire)
            && self.state.loaded.load(Ordering::Acquire)
            && self.state.epoch.load(Ordering::Acquire) == self.epoch
            && self
                .supervisor
                .as_ref()
                .is_none_or(|(epoch, expected)| epoch.load(Ordering::Acquire) == *expected)
    }

    /// Starts at most one export per player without IPC or waiting on the caller.
    /// A stale, busy or unsupported ticket returns `None` for normal download fallback.
    pub fn start(&self, cancellation: Arc<AtomicBool>) -> Option<CacheExportJob> {
        #[cfg(all(unix, feature = "backend-mpv"))]
        {
            worker::start(self, cancellation)
        }
        #[cfg(not(all(unix, feature = "backend-mpv")))]
        {
            let _ = cancellation;
            None
        }
    }

    pub(super) fn with_supervisor_epoch(mut self, epoch: Arc<AtomicU64>, expected: u64) -> Self {
        self.supervisor = Some((epoch, expected));
        self
    }
}

/// Load-state owner shared with mpv's ordered lifecycle event stream.
#[derive(Clone)]
#[cfg(any(feature = "backend-mpv", test))]
pub(super) struct CacheExportControl(Arc<CacheState>);

#[cfg(any(feature = "backend-mpv", test))]
impl CacheExportControl {
    /// Binds tickets to one process and its exclusively owned IPC/runtime paths.
    pub(super) fn new(pid: u32, endpoint: PathBuf, directory: PathBuf) -> Self {
        #[cfg(not(all(unix, feature = "backend-mpv")))]
        let _ = (pid, endpoint, directory);
        Self(Arc::new(CacheState {
            #[cfg(all(unix, feature = "backend-mpv"))]
            pid,
            #[cfg(all(unix, feature = "backend-mpv"))]
            endpoint,
            #[cfg(all(unix, feature = "backend-mpv"))]
            directory,
            epoch: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            loaded: AtomicBool::new(false),
            input: Mutex::new(Arc::from("")),
            #[cfg(all(unix, feature = "backend-mpv"))]
            tainted_epoch: AtomicU64::new(0),
        }))
    }
    /// Revokes the previous input before sending mpv its replacement command.
    pub(super) fn begin_load(&self, location: &str) {
        self.invalidate();
        if let Ok(mut input) = self.0.input.lock() {
            *input = Arc::from(location);
        }
    }
    /// Revokes completed-load tickets before stop and on ordered lifecycle events.
    pub(super) fn invalidate(&self) {
        self.0.loaded.store(false, Ordering::Release);
        self.0.epoch.fetch_add(1, Ordering::AcqRel);
    }
    /// Marks readiness only after mpv's ordered `file-loaded` event.
    pub(super) fn loaded(&self) {
        self.0.loaded.store(true, Ordering::Release);
    }
    /// Permanently revokes this process instance, even if its socket is reused.
    pub(super) fn shutdown(&self) {
        self.0.alive.store(false, Ordering::Release);
        self.invalidate();
    }
    /// Reads a current ticket without waiting for a lifecycle-state lock.
    pub(super) fn handle(&self) -> Option<PlaybackCacheHandle> {
        let epoch = self.0.epoch.load(Ordering::Acquire);
        let location = Arc::clone(&*self.0.input.try_lock().ok()?);
        if location.is_empty() {
            return None;
        }
        let handle = PlaybackCacheHandle {
            state: Arc::clone(&self.0),
            epoch,
            #[cfg(all(unix, feature = "backend-mpv"))]
            location,
            supervisor: None,
        };
        handle.is_current().then_some(handle)
    }
}

#[cfg(any(all(unix, feature = "backend-mpv"), test))]
fn complete_cache(cache: &serde_json::Value) -> Option<(f64, f64)> {
    if !cache.get("bof-cached")?.as_bool()? || !cache.get("eof-cached")?.as_bool()? {
        return None;
    }
    let bytes = cache.get("total-bytes")?.as_u64()?;
    if bytes == 0 || bytes > 32 * 1024 * 1024 {
        return None;
    }
    let ranges = cache.get("seekable-ranges")?.as_array()?;
    if ranges.len() != 1 {
        return None;
    }
    let start = ranges[0].get("start")?.as_f64()?;
    let end = ranges[0].get("end")?.as_f64()?;
    (start.is_finite() && end.is_finite() && (-1.0..=0.1).contains(&start) && end > start)
        .then_some((start, end))
}

/// One asynchronous cache export; dropping it cancels without joining a worker.
pub struct CacheExportJob {
    receiver: Receiver<Result<CacheExportedFile, CacheExportMiss>>,
    cancelled: Arc<AtomicBool>,
    caller_cancelled: Arc<AtomicBool>,
    deadline: Instant,
    completed: bool,
}

impl CacheExportJob {
    /// Polls once without blocking the reducer or playback supervisor.
    pub fn poll(&mut self) -> Option<Result<CacheExportedFile, CacheExportMiss>> {
        if self.completed {
            return None;
        }
        let result = if self.cancelled.load(Ordering::Acquire)
            || self.caller_cancelled.load(Ordering::Acquire)
        {
            Some(Err(CacheExportMiss::Cancelled))
        } else {
            match self.receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => Some(Err(CacheExportMiss::Failed)),
                Err(TryRecvError::Empty) if Instant::now() >= self.deadline => {
                    Some(Err(CacheExportMiss::Failed))
                }
                Err(TryRecvError::Empty) => None,
            }
        };
        if result.is_some() {
            self.completed = true;
            self.cancel();
        }
        result
    }

    /// Waits only on a preparation worker, while observing caller cancellation.
    ///
    /// # Errors
    /// Returns a redacted cache miss, never a reason to skip normal downloading.
    pub fn wait(
        mut self,
        cancellation: &Arc<AtomicBool>,
    ) -> Result<CacheExportedFile, CacheExportMiss> {
        if self.completed {
            return Err(CacheExportMiss::Unavailable);
        }
        loop {
            if cancellation.load(Ordering::Acquire) {
                self.cancel();
            }
            if let Some(result) = self.poll() {
                return result;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Requests cancellation without sending any playback-changing command.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl Drop for CacheExportJob {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Owns only a directory created exclusively for this dump, never caller paths.
struct PrivateDump {
    path: PathBuf,
    directory: PathBuf,
}

impl Drop for PrivateDump {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

/// Private raw cache dump whose lifetime is owned by this guard.
///
/// This is not proof of a complete usable file. The independent media validator
/// must inspect it before publication, and recheck [`Self::is_current`] afterward.
pub struct CacheExportedFile {
    storage: PrivateDump,
    duration: Duration,
    handle: PlaybackCacheHandle,
}

impl std::fmt::Debug for CacheExportedFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CacheExportedFile([PRIVATE])")
    }
}

impl CacheExportedFile {
    /// Borrows the private Ogg `.opus` or `.webm` dump while this guard keeps it alive.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.storage.path
    }

    /// Precise expected media duration reported by the pinned mpv load.
    #[must_use]
    pub const fn duration(&self) -> Duration {
        self.duration
    }

    /// Expected encoded audio codec, to be independently checked by the validator.
    #[must_use]
    pub const fn codec(&self) -> CacheAudioCodec {
        CacheAudioCodec::Opus
    }

    /// Whether the same loaded input still owns the result before publication.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.handle.is_current()
    }
}

/// The separate nonblocking IPC connection never consumes the player's normal
/// event stream. Windows currently falls back rather than using an uncancellable
/// named-pipe reader. mpv itself can briefly stall while dumping its demuxer;
/// the 32 MiB input cap limits that experimental fast path conservatively.
#[cfg(all(unix, feature = "backend-mpv"))]
mod worker {
    use super::{
        Arc, AtomicBool, AtomicU64, CacheExportJob, CacheExportMiss, CacheExportedFile, Duration,
        EXPORT_TIMEOUT, Instant, Ordering, PlaybackCacheHandle, PrivateDump, complete_cache,
    };
    use mio::net::UnixStream;
    use serde_json::{Value, json};
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::mpsc::sync_channel;
    use std::thread;

    /// One dump at a time across this application, including replacement players.
    static EXPORT_BUSY: AtomicBool = AtomicBool::new(false);
    static DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(1);
    const MAX_LINE_BYTES: usize = 256 * 1024;
    const MAX_IPC_BYTES: usize = 1024 * 1024;
    const MAX_MESSAGES: usize = 128;

    /// Match the source container: native Matroska loses Opus discard padding
    /// when mpv writes directly to Ogg. Independent validation remains mandatory.
    #[derive(Clone, Copy, Eq, PartialEq)]
    enum Container {
        Ogg,
        WebM,
    }

    /// Stable stream identity surrounding the dump, not merely the same codec.
    struct Snapshot {
        duration: f64,
        audio_id: u64,
        container: Container,
    }

    /// Releases the process-wide permit even when the caller has gone away.
    struct Permit;
    impl Drop for Permit {
        fn drop(&mut self) {
            EXPORT_BUSY.store(false, Ordering::Release);
        }
    }

    pub(super) fn start(
        handle: &PlaybackCacheHandle,
        cancellation: Arc<AtomicBool>,
    ) -> Option<CacheExportJob> {
        if !handle.is_current()
            || cancellation.load(Ordering::Acquire)
            || handle.state.tainted_epoch.load(Ordering::Acquire) == handle.epoch
            || EXPORT_BUSY
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return None;
        }
        let permit = Permit;
        let (sender, receiver) = sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let deadline = Instant::now() + EXPORT_TIMEOUT;
        let mut connection = Connection {
            handle: handle.clone(),
            caller_cancelled: Arc::clone(&cancellation),
            cancelled: Arc::clone(&cancelled),
            deadline,
            stream: None,
            input: Vec::new(),
            request_id: 0,
            messages: 0,
            received_bytes: 0,
            dump_started: false,
        };
        thread::Builder::new()
            .name("youta-cache-export".to_owned())
            .spawn(move || {
                let _permit = permit;
                let result = connection.export();
                if result.is_err() && connection.dump_started {
                    connection
                        .handle
                        .state
                        .tainted_epoch
                        .store(connection.handle.epoch, Ordering::Release);
                }
                // Close only this client's connection; do not cancel another client's
                // dump or issue playback commands. A lost reply is a normal cache miss.
                connection.stream.take();
                let _ = sender.send(result);
            })
            .ok()?;
        Some(CacheExportJob {
            receiver,
            cancelled,
            caller_cancelled: cancellation,
            deadline,
            completed: false,
        })
    }

    /// A single deadline and bounded JSON framing cover the entire IPC exchange.
    struct Connection {
        handle: PlaybackCacheHandle,
        caller_cancelled: Arc<AtomicBool>,
        cancelled: Arc<AtomicBool>,
        deadline: Instant,
        stream: Option<UnixStream>,
        input: Vec<u8>,
        request_id: u64,
        messages: usize,
        received_bytes: usize,
        dump_started: bool,
    }

    impl Connection {
        fn check(&self) -> Result<(), CacheExportMiss> {
            if !self.handle.is_current()
                || self.cancelled.load(Ordering::Acquire)
                || self.caller_cancelled.load(Ordering::Acquire)
            {
                return Err(CacheExportMiss::Cancelled);
            }
            if Instant::now() >= self.deadline {
                return Err(CacheExportMiss::Failed);
            }
            Ok(())
        }

        fn export(&mut self) -> Result<CacheExportedFile, CacheExportMiss> {
            self.check()?;
            // mio performs a nonblocking connect; a full listen backlog is a miss.
            self.stream = Some(
                UnixStream::connect(&self.handle.state.endpoint)
                    .map_err(|_| CacheExportMiss::Unavailable)?,
            );
            let before = self.snapshot()?;
            let duration = before.duration;
            let storage = self.private_dump(before.container)?;
            self.check()?;
            self.dump_started = true;
            self.command(&json!([
                "dump-cache",
                0.0,
                duration + 1.0,
                storage.path.to_str().ok_or(CacheExportMiss::Failed)?
            ]))?;
            self.check()?;
            let after = self.snapshot()?;
            if (after.duration - duration).abs() > 0.001
                || after.audio_id != before.audio_id
                || after.container != before.container
            {
                return Err(CacheExportMiss::Unavailable);
            }
            let metadata =
                std::fs::symlink_metadata(&storage.path).map_err(|_| CacheExportMiss::Failed)?;
            if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 64 * 1024 * 1024 {
                return Err(CacheExportMiss::Failed);
            }
            self.check()?;
            Ok(CacheExportedFile {
                storage,
                duration: Duration::from_secs_f64(duration),
                handle: self.handle.clone(),
            })
        }

        /// Verify raw completeness and main-demuxer identity, never merged UI ranges.
        fn snapshot(&mut self) -> Result<Snapshot, CacheExportMiss> {
            if self.property("pid")?.as_u64() != Some(u64::from(self.handle.state.pid))
                || self.property("path")?.as_str() != Some(self.handle.location.as_ref())
                || self.property("idle-active")?.as_bool() != Some(false)
            {
                return Err(CacheExportMiss::Unavailable);
            }
            let duration = self
                .property("duration")?
                .as_f64()
                .filter(|value| value.is_finite() && *value > 0.0 && *value <= 24.0 * 60.0 * 60.0)
                .ok_or(CacheExportMiss::Unavailable)?;
            let container = match self.property("file-format")?.as_str() {
                Some("ogg") => Container::Ogg,
                Some("mkv" | "matroska" | "webm" | "matroska,webm") => Container::WebM,
                _ => return Err(CacheExportMiss::Unavailable),
            };
            if !matches!(
                self.property("current-demuxer")?.as_str(),
                Some("lavf" | "mkv")
            ) {
                return Err(CacheExportMiss::Unavailable);
            }
            let tracks = self.property("track-list")?;
            let tracks = tracks
                .as_array()
                .filter(|tracks| !tracks.is_empty() && tracks.len() <= 64)
                .ok_or(CacheExportMiss::Unavailable)?;
            let mut audio = 0;
            let mut audio_id = None;
            for track in tracks {
                let kind = track["type"].as_str().ok_or(CacheExportMiss::Unavailable)?;
                let selected = track["selected"]
                    .as_bool()
                    .ok_or(CacheExportMiss::Unavailable)?;
                if matches!(kind, "audio" | "video") && track["external"].as_bool() != Some(false) {
                    return Err(CacheExportMiss::Unavailable);
                }
                if selected {
                    if kind != "audio" || track["codec"].as_str() != Some("opus") {
                        return Err(CacheExportMiss::Unavailable);
                    }
                    audio += 1;
                    audio_id = track["id"].as_u64().filter(|id| *id > 0);
                }
            }
            if audio != 1 {
                return Err(CacheExportMiss::Unavailable);
            }
            let (_, end) = complete_cache(&self.property("demuxer-cache-state")?)
                .ok_or(CacheExportMiss::Unavailable)?;
            if (end - duration).abs() > 0.25 {
                return Err(CacheExportMiss::Unavailable);
            }
            Ok(Snapshot {
                duration,
                audio_id: audio_id.ok_or(CacheExportMiss::Unavailable)?,
                container,
            })
        }

        /// Exclusively create a mode-0700 directory below mpv's private runtime.
        fn private_dump(&self, container: Container) -> Result<PrivateDump, CacheExportMiss> {
            for _ in 0..16 {
                self.check()?;
                let sequence = DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let directory = self
                    .handle
                    .state
                    .directory
                    .join(format!("cache-export-{}-{sequence}", std::process::id()));
                match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
                    Ok(()) => {
                        return Ok(PrivateDump {
                            path: directory.join(match container {
                                Container::Ogg => "cached.opus",
                                Container::WebM => "cached.webm",
                            }),
                            directory,
                        });
                    }
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(_) => return Err(CacheExportMiss::Failed),
                }
            }
            Err(CacheExportMiss::Failed)
        }

        fn property(&mut self, property: &str) -> Result<Value, CacheExportMiss> {
            self.command(&json!(["get_property", property]))
        }

        fn command(&mut self, command: &Value) -> Result<Value, CacheExportMiss> {
            self.check()?;
            self.request_id += 1;
            let request = json!({"command":command,"request_id":self.request_id,"async":true});
            let mut bytes = serde_json::to_vec(&request).map_err(|_| CacheExportMiss::Failed)?;
            bytes.push(b'\n');
            let mut written = 0;
            while written < bytes.len() {
                self.check()?;
                match self
                    .stream
                    .as_mut()
                    .ok_or(CacheExportMiss::Failed)?
                    .write(&bytes[written..])
                {
                    Ok(0) => return Err(CacheExportMiss::Failed),
                    Ok(count) => written += count,
                    Err(error)
                        if matches!(
                            error.kind(),
                            ErrorKind::WouldBlock | ErrorKind::Interrupted
                        ) =>
                    {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => return Err(CacheExportMiss::Failed),
                }
            }
            loop {
                self.check()?;
                if let Some(end) = self.input.iter().position(|byte| *byte == b'\n') {
                    if end > MAX_LINE_BYTES || self.messages >= MAX_MESSAGES {
                        return Err(CacheExportMiss::Failed);
                    }
                    self.messages += 1;
                    let response: Value = serde_json::from_slice(&self.input[..end])
                        .map_err(|_| CacheExportMiss::Failed)?;
                    self.input.drain(..=end);
                    if response.get("request_id").and_then(Value::as_u64) == Some(self.request_id) {
                        if response.get("error").and_then(Value::as_str) != Some("success") {
                            return Err(CacheExportMiss::Unavailable);
                        }
                        return Ok(response.get("data").cloned().unwrap_or(Value::Null));
                    }
                    continue;
                }
                if self.input.len() > MAX_LINE_BYTES {
                    return Err(CacheExportMiss::Failed);
                }
                let mut buffer = [0; 4096];
                match self
                    .stream
                    .as_mut()
                    .ok_or(CacheExportMiss::Failed)?
                    .read(&mut buffer)
                {
                    Ok(0) => return Err(CacheExportMiss::Failed),
                    Ok(count) => {
                        self.received_bytes += count;
                        if self.received_bytes > MAX_IPC_BYTES {
                            return Err(CacheExportMiss::Failed);
                        }
                        self.input.extend_from_slice(&buffer[..count]);
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            ErrorKind::WouldBlock | ErrorKind::Interrupted
                        ) =>
                    {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => return Err(CacheExportMiss::Failed),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::Ordering;

    fn complete() -> serde_json::Value {
        json!({"bof-cached":true,"eof-cached":true,"total-bytes":1024,"seekable-ranges":[{"start":0.0,"end":59.99}]})
    }

    #[test]
    fn completeness_requires_raw_bof_eof_and_exactly_one_range() {
        assert_eq!(complete_cache(&complete()), Some((0.0, 59.99)));
        for field in ["bof-cached", "eof-cached"] {
            let mut cache = complete();
            cache[field] = json!(false);
            assert!(complete_cache(&cache).is_none());
        }
        let mut merged_display_would_look_complete = complete();
        merged_display_would_look_complete["seekable-ranges"] =
            json!([{"start":0,"end":40},{"start":30,"end":60}]);
        assert!(complete_cache(&merged_display_would_look_complete).is_none());
        for ranges in [
            json!([]),
            json!([{"start":10,"end":1}]),
            json!([{"start":-10,"end":60}]),
        ] {
            let mut cache = complete();
            cache["seekable-ranges"] = ranges;
            assert!(complete_cache(&cache).is_none());
        }
    }

    #[test]
    fn cached_byte_bound_is_not_derived_from_forward_bytes_or_duration() {
        for bytes in [json!(0), json!(-1), json!(33 * 1024 * 1024), json!(null)] {
            let mut cache = complete();
            cache["total-bytes"] = bytes;
            cache["fw-bytes"] = json!(1);
            assert!(complete_cache(&cache).is_none());
        }
        let mut cache = complete();
        cache["total-bytes"] = json!(32 * 1024 * 1024);
        assert!(complete_cache(&cache).is_some());
    }

    #[test]
    fn tickets_require_completed_load_and_revoke_on_replace_stop_or_shutdown() {
        let control = CacheExportControl::new(
            123,
            "private.sock".into(),
            "/tmp/unused-cache-fixture".into(),
        );
        assert!(control.handle().is_none());
        control.begin_load("https://example.invalid/first.opus");
        assert!(control.handle().is_none());
        control.loaded();
        let first = control.handle().expect("completed load ticket");
        assert!(first.is_current());
        control.begin_load("https://example.invalid/second.opus");
        assert!(!first.is_current());
        assert!(control.handle().is_none());
        control.loaded();
        let second = control.handle().expect("replacement ticket");
        control.invalidate();
        assert!(!second.is_current());
        control.loaded();
        let third = control.handle().expect("loaded ticket");
        control.shutdown();
        assert!(!third.is_current());
        assert!(control.handle().is_none());
    }

    #[test]
    fn queued_supervisor_load_invalidates_old_ticket_before_backend_handles_job() {
        let control = CacheExportControl::new(
            123,
            "private.sock".into(),
            "/tmp/unused-cache-fixture".into(),
        );
        control.begin_load("https://example.invalid/first.opus");
        control.loaded();
        let epoch = Arc::new(AtomicU64::new(4));
        let handle = control
            .handle()
            .expect("loaded ticket")
            .with_supervisor_epoch(Arc::clone(&epoch), 4);
        assert!(handle.is_current());
        epoch.store(5, Ordering::Release);
        assert!(!handle.is_current());
        assert!(handle.start(Arc::new(AtomicBool::new(false))).is_none());
    }

    #[test]
    fn handles_never_debug_print_loaded_signed_urls() {
        let control = CacheExportControl::new(
            123,
            "private.sock".into(),
            "/tmp/unused-cache-fixture".into(),
        );
        control.begin_load("https://example.invalid/first.opus?secret=fixture-secret");
        control.loaded();
        let handle = control.handle().expect("loaded ticket");
        assert!(!format!("{handle:?}").contains("fixture-secret"));
    }

    #[cfg(all(unix, feature = "backend-mpv"))]
    mod ipc {
        use super::*;
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::sync::MutexGuard;
        use std::thread::{self, JoinHandle};
        use std::time::Instant;

        static FIXTURE_LOCK: Mutex<()> = Mutex::new(());

        /// Private scripted IPC server; no mpv process, media or network access.
        struct Fixture {
            _lock: MutexGuard<'static, ()>,
            directory: tempfile::TempDir,
            control: CacheExportControl,
            commands: Arc<Mutex<Vec<serde_json::Value>>>,
            stop: Arc<AtomicBool>,
            worker: Option<JoinHandle<()>>,
        }

        impl Fixture {
            fn new(
                overrides: serde_json::Value,
                on_dump: impl Fn(&Path, &CacheExportControl) + Send + 'static,
            ) -> Self {
                let lock = FIXTURE_LOCK
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let directory = tempfile::tempdir().expect("private fixture");
                let endpoint = directory.path().join("mpv.sock");
                let listener = UnixListener::bind(&endpoint).expect("fixture socket");
                listener.set_nonblocking(true).expect("nonblocking fixture");
                let control = CacheExportControl::new(123, endpoint, directory.path().to_owned());
                control.begin_load("https://example.invalid/fixture.opus?secret=fixture-secret");
                control.loaded();
                let commands = Arc::new(Mutex::new(Vec::new()));
                let stop = Arc::new(AtomicBool::new(false));
                let worker_control = control.clone();
                let worker_commands = Arc::clone(&commands);
                let worker_stop = Arc::clone(&stop);
                let worker = thread::spawn(move || {
                    let mut properties = json!({"pid":123,"path":"https://example.invalid/fixture.opus?secret=fixture-secret","idle-active":false,"duration":60.0,"file-format":"ogg","current-demuxer":"lavf","track-list":[{"id":1,"type":"audio","selected":true,"external":false,"codec":"opus"}],"demuxer-cache-state":complete()});
                    for (key, value) in overrides.as_object().expect("override object") {
                        properties[key] = value.clone();
                    }
                    let stream = loop {
                        if worker_stop.load(Ordering::Acquire) {
                            return;
                        }
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(2))
                            }
                            Err(_) => return,
                        }
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_millis(20)))
                        .expect("fixture read bound");
                    let mut reader = BufReader::new(stream);
                    loop {
                        if worker_stop.load(Ordering::Acquire) {
                            return;
                        }
                        let mut line = String::new();
                        match reader.read_line(&mut line) {
                            Ok(0) => return,
                            Ok(_) => {}
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) =>
                            {
                                continue;
                            }
                            Err(_) => return,
                        }
                        let request: serde_json::Value =
                            serde_json::from_str(&line).expect("request JSON");
                        worker_commands
                            .lock()
                            .expect("commands")
                            .push(request.clone());
                        let command = request["command"][0].as_str().expect("command");
                        if properties["fixture-event-flood"] == true {
                            for _ in 0..129 {
                                if writeln!(reader.get_mut(), "{{\"event\":\"fixture\"}}").is_err()
                                {
                                    return;
                                }
                            }
                        }
                        let data = if command == "get_property" {
                            properties[request["command"][1].as_str().expect("property")].clone()
                        } else if command == "dump-cache" {
                            on_dump(
                                Path::new(request["command"][3].as_str().expect("private output")),
                                &worker_control,
                            );
                            if properties["fixture-track-switch"] == true {
                                properties["track-list"][0]["id"] = json!(2);
                            }
                            json!(null)
                        } else {
                            panic!("cache export must not alter playback: {command}")
                        };
                        let reply = json!({"request_id":request["request_id"],"error":"success","data":data});
                        if writeln!(reader.get_mut(), "{reply}").is_err() {
                            return;
                        }
                    }
                });
                Self {
                    _lock: lock,
                    directory,
                    control,
                    commands,
                    stop,
                    worker: Some(worker),
                }
            }

            fn handle(&self) -> PlaybackCacheHandle {
                self.control.handle().expect("loaded fixture")
            }

            fn exported(&self) -> Result<CacheExportedFile, CacheExportMiss> {
                let cancel = Arc::new(AtomicBool::new(false));
                self.handle()
                    .start(Arc::clone(&cancel))
                    .expect("asynchronous cache job")
                    .wait(&cancel)
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::Release);
                if let Some(worker) = self.worker.take() {
                    worker.join().expect("fixture worker");
                }
            }
        }

        #[test]
        fn complete_opus_export_is_private_async_and_never_changes_playback() {
            let fixture = Fixture::new(json!({}), |path, _| {
                std::fs::write(path, b"fixture packets").expect("fixture dump")
            });
            let exported = fixture.exported().expect("complete cache");
            let path = exported.path().to_owned();
            assert_eq!(exported.duration(), Duration::from_mins(1));
            assert_eq!(exported.codec(), CacheAudioCodec::Opus);
            assert!(exported.is_current());
            assert!(path.is_file());
            let commands = fixture.commands.lock().expect("commands");
            let dump = commands
                .iter()
                .find(|request| request["command"][0] == "dump-cache")
                .expect("dump command");
            assert_eq!(dump["async"], true);
            assert_eq!(dump["command"][1], 0.0);
            assert!(
                dump["command"][2]
                    .as_f64()
                    .expect("finite ending")
                    .is_finite()
            );
            assert!(commands.iter().all(|request| matches!(
                request["command"][0].as_str(),
                Some("get_property" | "dump-cache")
            )));
            drop(exported);
            assert!(!path.exists());
            assert!(!path.parent().expect("private directory").exists());
        }

        #[test]
        fn matching_container_preserves_native_matroska_opus_padding() {
            let fixture = Fixture::new(
                json!({"file-format":"mkv", "current-demuxer":"mkv"}),
                |path, _| std::fs::write(path, b"fixture packets").expect("fixture dump"),
            );
            let exported = fixture.exported().expect("native Matroska cache");
            assert_eq!(
                exported
                    .path()
                    .extension()
                    .and_then(std::ffi::OsStr::to_str),
                Some("webm")
            );
        }

        #[test]
        fn selected_audio_stream_change_discards_same_codec_dump() {
            let fixture = Fixture::new(json!({"fixture-track-switch":true}), |path, _| {
                std::fs::write(path, b"different selected audio").expect("fixture dump")
            });
            assert_eq!(
                fixture.exported().expect_err("selected audio changed"),
                CacheExportMiss::Unavailable
            );
        }

        #[test]
        fn cache_export_rejects_wrong_identity_live_external_and_non_opus_inputs() {
            for overrides in [
                json!({"pid":456}),
                json!({"path":"https://example.invalid/other.opus"}),
                json!({"duration":null}),
                json!({"duration":0}),
                json!({"idle-active":true}),
                json!({"current-demuxer":"edl"}),
                json!({"file-format":"hls"}),
                json!({"track-list":[{"type":"audio","selected":true,"external":true,"codec":"opus"}]}),
                json!({"track-list":[{"type":"audio","selected":true,"external":false,"codec":"aac"}]}),
                json!({"track-list":[{"type":"audio","selected":true,"external":false,"codec":"opus"},{"type":"video","selected":true,"external":false,"codec":"vp9"}]}),
                json!({"demuxer-cache-state":{"bof-cached":true,"eof-cached":false,"total-bytes":100,"seekable-ranges":[{"start":0,"end":60}]}}),
            ] {
                let fixture =
                    Fixture::new(overrides, |_, _| panic!("ineligible cache must not dump"));
                assert_eq!(
                    fixture.exported().expect_err("cache miss"),
                    CacheExportMiss::Unavailable
                );
            }
        }

        #[test]
        fn load_change_during_export_discards_private_output() {
            let fixture = Fixture::new(json!({}), |path, control| {
                std::fs::write(path, b"stale fixture packets").expect("fixture dump");
                control.begin_load("https://example.invalid/replacement.opus");
            });
            assert_eq!(
                fixture.exported().expect_err("stale cache"),
                CacheExportMiss::Cancelled
            );
            assert_eq!(
                std::fs::read_dir(fixture.directory.path())
                    .expect("fixture directory")
                    .count(),
                1,
                "only the private socket remains"
            );
        }

        #[test]
        fn malformed_empty_or_oversized_dump_is_never_returned() {
            for size in [0, 65 * 1024 * 1024] {
                let fixture = Fixture::new(json!({}), move |path, _| {
                    std::fs::File::create(path)
                        .expect("fixture dump")
                        .set_len(size)
                        .expect("sparse fixture length");
                });
                assert_eq!(
                    fixture.exported().expect_err("invalid raw dump"),
                    CacheExportMiss::Failed
                );
            }
        }

        #[test]
        fn cancellation_is_bounded_single_job_and_never_cancels_normal_fallback() {
            let entered = Arc::new(AtomicBool::new(false));
            let worker_entered = Arc::clone(&entered);
            let fixture = Fixture::new(json!({}), move |_, _| {
                worker_entered.store(true, Ordering::Release);
                thread::sleep(Duration::from_millis(300));
            });
            let cancellation = Arc::new(AtomicBool::new(false));
            let handle = fixture.handle();
            let start = Instant::now();
            let job = handle.start(Arc::clone(&cancellation)).expect("cache job");
            assert!(start.elapsed() < Duration::from_millis(100));
            assert!(
                handle.start(Arc::clone(&cancellation)).is_none(),
                "only one active export"
            );
            let deadline = Instant::now() + Duration::from_secs(1);
            while !entered.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            assert!(entered.load(Ordering::Acquire));
            let start = Instant::now();
            job.cancel();
            assert_eq!(
                job.wait(&cancellation).expect_err("cancelled export"),
                CacheExportMiss::Cancelled
            );
            assert!(start.elapsed() < Duration::from_millis(150));
            assert!(
                !cancellation.load(Ordering::Acquire),
                "cache cancellation must leave normal fallback enabled"
            );
        }

        #[test]
        fn oversized_json_and_unrelated_event_flood_are_bounded_cache_misses() {
            for overrides in [
                json!({"path":"x".repeat(300 * 1024)}),
                json!({"fixture-event-flood":true}),
            ] {
                let fixture = Fixture::new(overrides, |_, _| panic!("invalid IPC must not dump"));
                let started = Instant::now();
                assert_eq!(
                    fixture.exported().expect_err("bounded IPC"),
                    CacheExportMiss::Failed
                );
                assert!(started.elapsed() < Duration::from_secs(1));
            }
        }

        #[test]
        fn symlink_output_is_rejected_without_removing_its_target() {
            use std::os::unix::fs::symlink;
            let fixture = Fixture::new(json!({}), |path, _| {
                let target = path
                    .parent()
                    .expect("private output directory")
                    .parent()
                    .expect("fixture root")
                    .join("unrelated.opus");
                std::fs::write(&target, b"unrelated file").expect("sentinel");
                symlink(target, path).expect("malformed dump");
            });
            assert_eq!(
                fixture.exported().expect_err("symlink rejected"),
                CacheExportMiss::Failed
            );
            assert_eq!(
                std::fs::read(fixture.directory.path().join("unrelated.opus"))
                    .expect("sentinel retained"),
                b"unrelated file"
            );
        }

        #[test]
        fn unresponsive_dump_has_one_overall_deadline_and_no_retry_for_that_load() {
            let fixture =
                Fixture::new(json!({}), |_, _| thread::sleep(Duration::from_millis(3250)));
            let cancellation = Arc::new(AtomicBool::new(false));
            let handle = fixture.handle();
            let started = Instant::now();
            let mut job = handle.start(Arc::clone(&cancellation)).expect("cache job");
            assert!(job.poll().is_none());
            assert_eq!(
                job.wait(&cancellation).expect_err("bounded dump wait"),
                CacheExportMiss::Failed
            );
            assert!(started.elapsed() < Duration::from_millis(3200));
            assert!(
                handle.start(Arc::clone(&cancellation)).is_none(),
                "do not race a timed-out mpv command with a second dump"
            );
            assert!(!cancellation.load(Ordering::Acquire));
        }
    }
}
