//! Local-only, bounded `FFprobe` fallback for already-fetched Web media headers.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{TryRecvError, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use crate::web_metadata::WebMediaMetadata;

/// Maximum already-fetched input made available to the local helper.
const MAX_PROBE_PREFIX_BYTES: usize = 256 * 1024;
/// Retained JSON is independently bounded, even for a misbehaving helper.
const MAX_PROBE_OUTPUT_BYTES: usize = 64 * 1024;
/// Total deadline for the helper and its pipe collection.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Poll interval keeps cancellation responsive without busy waiting.
const PROBE_POLL: Duration = Duration::from_millis(10);

/// Enriches Web metadata using only bytes already fetched by its bounded reader.
///
/// The filename is a display hint, never an input path. A fixed demuxer and
/// pipe-only protocol whitelist exclude playlists, network requests and local
/// file references. Disabling stream-info heuristics means durations come from
/// container headers, not estimates based on the truncated prefix. Bitrate and
/// file size are deliberately not copied from this incomplete input.
pub(super) fn enrich_web_metadata(
    metadata: &mut WebMediaMetadata,
    filename: &str,
    executable: &Path,
    cancelled: &AtomicBool,
) {
    let prefix = std::mem::take(&mut metadata.probe_prefix);
    if cancelled.load(Ordering::Relaxed)
        || prefix.is_empty()
        || prefix.len() > MAX_PROBE_PREFIX_BYTES
    {
        return;
    }
    let Some(demuxer) = prefix_demuxer(&prefix) else {
        return;
    };
    if let Some(output) = probe_prefix(executable, demuxer, prefix, cancelled, PROBE_TIMEOUT)
        && !cancelled.load(Ordering::Relaxed)
    {
        apply_probe_output(metadata, filename, &output);
    }
}

/// Recognizes only containers whose headers describe embedded audio streams.
fn prefix_demuxer(prefix: &[u8]) -> Option<&'static str> {
    if prefix.starts_with(b"\x1aE\xdf\xa3") {
        return Some("matroska");
    }
    if prefix.len() >= 12 && &prefix[4..8] == b"ftyp" {
        let size = u32::from_be_bytes(prefix[..4].try_into().ok()?);
        if size >= 12 && u64::from(size) <= prefix.len() as u64 {
            return Some("mov");
        }
    }
    None
}

/// Builds a shell-free command with no source URL, filename or filesystem access.
fn prefix_command(executable: &Path, demuxer: &str) -> Command {
    let mut command = Command::new(executable);
    command.args([
		"-v", "error", "-max_alloc", "4194304",
		"-protocol_whitelist", "pipe",
		"-format_whitelist", "matroska,webm,mov,mp4,m4a,3gp,3g2,mj2",
		"-f", demuxer, "-max_streams", "32", "-threads", "1",
		"-probesize", "262144", "-analyzeduration", "0",
		"-nofind_stream_info", "-select_streams", "a:0",
		"-show_entries",
		"stream=codec_name,codec_long_name,sample_rate,channels,duration:stream_tags=title,artist,album,genre,comment,description:format=format_name,duration:format_tags=title,artist,album,genre,comment,description",
		"-of", "json", "pipe:0",
	]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    crate::child_process::supervised(&mut command);
    command
}

/// Collects at most one bounded JSON document, closing the pipe on overflow.
fn capture_prefix_output(reader: impl Read) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    reader
        .take((MAX_PROBE_OUTPUT_BYTES + 1) as u64)
        .read_to_end(&mut output)
        .ok()?;
    (output.len() <= MAX_PROBE_OUTPUT_BYTES).then_some(output)
}

/// Supervises pipe I/O and the process under one deadline and cancellation flag.
fn probe_prefix(
    executable: &Path,
    demuxer: &str,
    prefix: Vec<u8>,
    cancelled: &AtomicBool,
    timeout: Duration,
) -> Option<Vec<u8>> {
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }
    let deadline = Instant::now().checked_add(timeout)?;
    let mut child = prefix_command(executable, demuxer).spawn().ok()?;
    let Some(mut stdin) = child.stdin.take() else {
        crate::child_process::terminate_tree(&mut child);
        return None;
    };
    let Some(stdout) = child.stdout.take() else {
        crate::child_process::terminate_tree(&mut child);
        return None;
    };
    let Ok(input_thread) = thread::Builder::new()
        .name("youta-web-probe-input".into())
        .spawn(move || {
            // Header-only demuxers may stop before consuming the entire prefix.
            let _ = stdin.write_all(&prefix);
        })
    else {
        crate::child_process::terminate_tree(&mut child);
        return None;
    };
    let (sender, receiver) = sync_channel(1);
    let Ok(output_thread) = thread::Builder::new()
        .name("youta-web-probe-output".into())
        .spawn(move || {
            let _ = sender.send(capture_prefix_output(stdout));
        })
    else {
        crate::child_process::terminate_tree(&mut child);
        if input_thread.is_finished() {
            let _ = input_thread.join();
        }
        return None;
    };
    let mut output = None;
    let mut success = false;
    let result = loop {
        if cancelled.load(Ordering::Relaxed) || Instant::now() >= deadline {
            break None;
        }
        if output.is_none() {
            match receiver.try_recv() {
                Ok(Some(bytes)) => output = Some(bytes),
                Ok(None) | Err(TryRecvError::Disconnected) => break None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if !success {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    // End descendants still holding a pipe after their parent exited.
                    crate::child_process::terminate_tree(&mut child);
                    success = true;
                }
                Ok(Some(_)) | Err(_) => break None,
                Ok(None) => {}
            }
        }
        if success && output.is_some() {
            break output;
        }
        thread::sleep(PROBE_POLL.min(deadline.saturating_duration_since(Instant::now())));
    };
    crate::child_process::terminate_tree(&mut child);
    // Termination closes the owned pipes. An unexpectedly inherited descriptor
    // must not turn a bounded worker into an unbounded thread join.
    if input_thread.is_finished() {
        let _ = input_thread.join();
    }
    if output_thread.is_finished() {
        let _ = output_thread.join();
    }
    result
}

/// Applies only bounded header facts; null and malformed tags remain absent.
fn apply_probe_output(metadata: &mut WebMediaMetadata, filename: &str, payload: &[u8]) {
    /// Looks up a nullable string without accepting coercions from other JSON types.
    fn string<'a>(value: Option<&'a serde_json::Value>, name: &str) -> Option<&'a str> {
        value
            .and_then(|value| value.get(name))
            .and_then(serde_json::Value::as_str)
    }
    if payload.len() > MAX_PROBE_OUTPUT_BYTES {
        return;
    }
    let Ok(output) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return;
    };
    let stream = output
        .get("streams")
        .and_then(serde_json::Value::as_array)
        .and_then(|streams| streams.first());
    let format = output.get("format");
    let tag = |name: &str, limit: usize| {
        [format, stream].into_iter().flatten().find_map(|value| {
            value
                .get("tags")?
                .as_object()?
                .iter()
                .find_map(|(key, value)| {
                    key.eq_ignore_ascii_case(name)
                        .then(|| clean_tag(value.as_str()?, limit))
                        .flatten()
                })
        })
    };
    metadata.title = metadata.title.take().or_else(|| tag("title", 1024));
    metadata.artist = metadata.artist.take().or_else(|| tag("artist", 1024));
    metadata.album = metadata.album.take().or_else(|| tag("album", 1024));
    metadata.genre = metadata.genre.take().or_else(|| tag("genre", 1024));
    metadata.comment = metadata
        .comment
        .take()
        .or_else(|| tag("comment", 8192).or_else(|| tag("description", 8192)));
    metadata.container = metadata.container.take().or_else(|| {
        string(format, "format_name").and_then(|name| probe_container_label(filename, name))
    });
    metadata.codec = metadata.codec.take().or_else(|| {
        super::local_probe_codec_label(
            string(stream, "codec_name"),
            string(stream, "codec_long_name"),
        )
    });
    metadata.duration = metadata.duration.or_else(|| {
        string(format, "duration")
            .and_then(super::parse_local_probe_duration)
            .or_else(|| string(stream, "duration").and_then(super::parse_local_probe_duration))
    });
    metadata.sample_rate_hz = metadata.sample_rate_hz.or_else(|| {
        string(stream, "sample_rate")
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
    });
    metadata.channels = metadata.channels.or_else(|| {
        stream
            .and_then(|stream| stream.get("channels"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u8::try_from(value).ok())
            .filter(|value| *value > 0)
    });
}

/// Uses a filename hint only within the container family identified from bytes.
fn probe_container_label(filename: &str, format: &str) -> Option<String> {
    let path = Path::new(filename);
    let extension = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    let compatible = format.split(',').any(|name| match name {
        "matroska" | "webm" => ["mkv", "webm"]
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known)),
        "mov" | "mp4" | "m4a" => ["mov", "mp4", "m4a", "m4v", "3gp", "3g2", "mj2"]
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known)),
        _ => false,
    });
    super::local_probe_container_label(if compatible { path } else { Path::new("") }, format)
}

/// Normalizes display text without retaining controls or oversized strings.
fn clean_tag(value: &str, limit: usize) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > limit {
        return None;
    }
    let text: String = value
        .chars()
        .filter(|character| {
            (!character.is_control() || *character == '\n')
                && !matches!(*character, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect();
    (!text.trim().is_empty()).then(|| text.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_probe_never_accepts_playlists_or_uses_a_remote_name_as_input() {
        assert_eq!(prefix_demuxer(b"#EXTM3U\nhttps://example.org/secret"), None);
        assert_eq!(prefix_demuxer(b"\x1aE\xdf\xa3header"), Some("matroska"));
        assert_eq!(prefix_demuxer(b"\0\0\0\x10ftypisom\0\0\0\0"), Some("mov"));
        let command = prefix_command(Path::new("ffprobe"), "matroska");
        let arguments: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy())
            .collect();
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["-protocol_whitelist", "pipe"])
        );
        assert!(arguments.windows(2).any(|pair| pair == ["-f", "matroska"]));
        assert!(arguments.iter().any(|arg| arg == "-nofind_stream_info"));
        assert_eq!(arguments.last().unwrap(), "pipe:0");
        assert!(
            !arguments
                .iter()
                .any(|arg| arg.starts_with("http") || arg.starts_with("file:"))
        );
    }

    #[test]
    fn prefix_metadata_accepts_null_tags_and_preserves_real_http_size() {
        let mut metadata = WebMediaMetadata {
            size_bytes: Some(90_000_000),
            ..WebMediaMetadata::default()
        };
        apply_probe_output(&mut metadata, "episode.webm", br#"{
			"streams":[{"codec_name":"opus","sample_rate":"48000","channels":2,"bit_rate":"9000000","tags":{"title":null,"ARTIST":"Narrator"}}],
			"format":{"format_name":"matroska,webm","duration":"600.5","size":"2048","bit_rate":"9000000","tags":{"title":"Story","album":null,"comment":"Recorded at home"}}
		}"#);
        assert_eq!(metadata.title.as_deref(), Some("Story"));
        assert_eq!(metadata.artist.as_deref(), Some("Narrator"));
        assert_eq!(metadata.album, None);
        assert_eq!(metadata.codec.as_deref(), Some("Opus"));
        assert_eq!(metadata.container.as_deref(), Some("WebM"));
        assert_eq!(metadata.sample_rate_hz, Some(48_000));
        assert_eq!(metadata.channels, Some(2));
        assert_eq!(metadata.duration, Some(Duration::from_millis(600_500)));
        assert_eq!(metadata.bitrate_kbps, None);
        assert_eq!(metadata.size_bytes, Some(90_000_000));
    }

    #[test]
    fn detected_container_family_wins_over_a_misleading_filename() {
        assert_eq!(
            probe_container_label("audio.mp3", "matroska,webm").as_deref(),
            Some("WebM")
        );
        assert_eq!(
            probe_container_label("audio.webm", "mov,mp4,m4a,3gp,3g2,mj2").as_deref(),
            Some("MP4")
        );
        assert_eq!(
            probe_container_label("audio.MKV", "matroska,webm").as_deref(),
            Some("Matroska")
        );
        assert_eq!(
            probe_container_label("audio.MOV", "mov,mp4,m4a,3gp,3g2,mj2").as_deref(),
            Some("QuickTime")
        );
    }

    #[test]
    fn mp4_uses_header_stream_duration_without_format_estimates() {
        let mut metadata = WebMediaMetadata::default();
        apply_probe_output(&mut metadata, "episode.mp4", br#"{
            "streams":[{"codec_name":"aac","sample_rate":"44100","channels":1,"duration":"60.000000"}],
            "format":{"format_name":"mov,mp4,m4a,3gp,3g2,mj2"}
        }"#);
        assert_eq!(metadata.duration, Some(Duration::from_mins(1)));
        assert_eq!(metadata.codec.as_deref(), Some("AAC"));
        assert_eq!(metadata.container.as_deref(), Some("MP4"));
        assert_eq!(metadata.bitrate_kbps, None);
        assert_eq!(metadata.size_bytes, None);
    }

    #[test]
    fn header_probe_does_not_invent_missing_duration() {
        let mut metadata = WebMediaMetadata::default();
        apply_probe_output(
            &mut metadata,
            "live.webm",
            br#"{
            "streams":[{"codec_name":"opus","duration":"N/A"}],
            "format":{"format_name":"matroska,webm","duration":null}
        }"#,
        );
        assert_eq!(metadata.duration, None);
        assert_eq!(metadata.bitrate_kbps, None);
    }

    #[test]
    fn malformed_probe_output_keeps_existing_metadata() {
        let mut metadata = WebMediaMetadata {
            title: Some("Original".into()),
            duration: Some(Duration::from_secs(40)),
            ..WebMediaMetadata::default()
        };
        apply_probe_output(
            &mut metadata,
            "sound.mp4",
            br#"{"streams":null,"format":{"duration":"NaN","tags":{"title":null,"artist":123}}}"#,
        );
        apply_probe_output(&mut metadata, "sound.mp4", b"not JSON");
        assert_eq!(metadata.title.as_deref(), Some("Original"));
        assert_eq!(metadata.duration, Some(Duration::from_secs(40)));
        assert_eq!(metadata.artist, None);
    }

    #[test]
    fn output_capture_stops_at_the_fixed_limit() {
        assert_eq!(capture_prefix_output(&b"{}"[..]), Some(b"{}".to_vec()));
        assert!(capture_prefix_output(&vec![b' '; MAX_PROBE_OUTPUT_BYTES + 1][..]).is_none());
    }

    #[test]
    fn absent_or_cancelled_prefix_never_starts_a_helper() {
        let mut metadata = WebMediaMetadata::default();
        enrich_web_metadata(
            &mut metadata,
            "empty.webm",
            Path::new("nonexistent-web-probe"),
            &AtomicBool::new(false),
        );
        metadata.probe_prefix = b"\x1aE\xdf\xa3header".to_vec();
        enrich_web_metadata(
            &mut metadata,
            "empty.webm",
            Path::new("nonexistent-web-probe"),
            &AtomicBool::new(true),
        );
        assert!(metadata.probe_prefix.is_empty());
        assert_eq!(metadata.title, None);
    }

    /// Installs a Unix fixture helper without relying on a real media decoder.
    #[cfg(unix)]
    fn mock_helper(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        const READY_ARGUMENT: &str = "__youta_web_probe_fixture_ready__";
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ffprobe-fixture");
        let staging = directory.path().join("ffprobe-fixture.writing");
        {
            let mut file = std::fs::File::create(&staging).unwrap();
            file.write_all(format!(
                "#!/bin/sh\nif [ \"${{1-}}\" = '{READY_ARGUMENT}' ]; then exit 0; fi\nprintf '%s' \"$$\" > \"$0.started\"\n{body}\n"
            ).as_bytes()).unwrap();
            file.sync_all().unwrap();
        }
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::rename(&staging, &path).unwrap();
        // Instrumented parallel tests may briefly observe ETXTBSY on a newly
        // published executable. Prove fixture readiness without executing its
        // behavior or mistaking a spawn failure for successful cancellation.
        for attempt in 0..50 {
            match Command::new(&path).arg(READY_ARGUMENT).status() {
                Ok(status) => {
                    assert!(status.success(), "fixture readiness failed: {status}");
                    return (directory, path);
                }
                Err(error) if attempt < 49 && error.raw_os_error() == Some(26) => {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("fixture did not become executable: {error}"),
            }
        }
        unreachable!("bounded readiness attempts always return or panic");
    }

    /// Proves the production invocation actually started, not just its readiness check.
    #[cfg(unix)]
    fn started_helper_pid(path: &Path) -> u32 {
        std::fs::read_to_string(path.with_extension("started"))
            .expect("the helper must execute before a lifecycle assertion")
            .parse()
            .expect("the helper records its process ID")
    }

    #[cfg(unix)]
    #[test]
    fn prefix_helper_collects_success_without_source_paths_or_network() {
        let (_directory, path) = mock_helper(
            "printf '%s' '{\"format\":{\"format_name\":\"matroska,webm\",\"tags\":{\"title\":\"Header title\"}}}'",
        );
        let mut metadata = WebMediaMetadata {
            probe_prefix: b"\x1aE\xdf\xa3header".to_vec(),
            ..WebMediaMetadata::default()
        };
        enrich_web_metadata(
            &mut metadata,
            "https://example.org/signed.webm?secret=unshared",
            &path,
            &AtomicBool::new(false),
        );
        assert_eq!(metadata.title.as_deref(), Some("Header title"));
        assert!(metadata.probe_prefix.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn timeout_terminates_helpers_that_never_read_the_prefix() {
        let (_directory, path) = mock_helper("sleep 30");
        let started = Instant::now();
        assert!(
            probe_prefix(
                &path,
                "matroska",
                vec![0; MAX_PROBE_PREFIX_BYTES],
                &AtomicBool::new(false),
                Duration::from_millis(100)
            )
            .is_none()
        );
        assert!(started.elapsed() >= Duration::from_millis(75));
        assert!(started.elapsed() < Duration::from_secs(3));
        let pid = started_helper_pid(&path);
        #[cfg(target_os = "linux")]
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "timed-out helper must be reaped"
        );
        #[cfg(not(target_os = "linux"))]
        let _ = pid;
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_terminates_an_active_helper() {
        let (_directory, path) = mock_helper("sleep 30");
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let stop = std::sync::Arc::clone(&cancelled);
        let marker = path.with_extension("started");
        let notifier = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !marker.exists() {
                assert!(
                    Instant::now() < deadline,
                    "helper did not start before cancellation"
                );
                thread::sleep(Duration::from_millis(1));
            }
            stop.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        assert!(
            probe_prefix(
                &path,
                "matroska",
                vec![0; MAX_PROBE_PREFIX_BYTES],
                &cancelled,
                PROBE_TIMEOUT
            )
            .is_none()
        );
        notifier.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(started_helper_pid(&path) > 0);
    }

    #[cfg(unix)]
    #[test]
    fn output_overflow_terminates_the_helper_before_timeout() {
        let (_directory, path) = mock_helper("while :; do printf '%01024d' 0; done");
        let started = Instant::now();
        assert!(
            probe_prefix(
                &path,
                "matroska",
                vec![0; MAX_PROBE_PREFIX_BYTES],
                &AtomicBool::new(false),
                PROBE_TIMEOUT
            )
            .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(started_helper_pid(&path) > 0);
    }

    #[test]
    fn display_tags_are_bounded_and_terminal_safe() {
        assert_eq!(
            clean_tag(" \u{1b}Author\u{0} ", 100).as_deref(),
            Some("Author")
        );
        assert_eq!(clean_tag("\u{0}", 100), None);
        assert_eq!(clean_tag("A\u{202e}uthor", 100).as_deref(), Some("Author"));
        assert_eq!(clean_tag(&"x".repeat(1025), 1024), None);
    }
}
