//! Bounded private local inputs shared by the reviewed Archive and S3 exporters.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::StagingPlan;
use crate::config::Config;

/// Checks the shared cancellation and whole-preparation deadline.
pub(crate) fn check_work(
    cancellation: &AtomicBool,
    started: Instant,
    deadline: Duration,
) -> Result<(), String> {
    if cancellation.load(Ordering::Relaxed) {
        return Err("Media preparation canceled".to_owned());
    }
    if started.elapsed() >= deadline {
        return Err("Media preparation timed out".to_owned());
    }
    Ok(())
}

/// Pins an open regular input and copies it into private staging. The source
/// inode is never moved, truncated or passed to a helper after its owner drops.
fn copy_local_input(
    source: &Path,
    destination: &Path,
    cancellation: &AtomicBool,
    progress: &mut dyn FnMut(u64, Option<u64>),
    max_bytes: u64,
    started: Instant,
    deadline: Duration,
) -> Result<u64, String> {
    check_work(cancellation, started, deadline)?;
    let selected = std::fs::symlink_metadata(source)
        .map_err(|error| format!("Cannot resolve selected local media: {error}"))?;
    if !selected.file_type().is_file() {
        return Err("The selected media must be a nonempty regular file".to_owned());
    }
    let selected_snapshot = InputSnapshot::capture(source, &selected);
    let resolved = crate::fs_path::canonicalize(source)
        .map_err(|error| format!("Cannot resolve selected local media: {error}"))?;
    let inspected = std::fs::symlink_metadata(&resolved)
        .map_err(|error| format!("Cannot inspect selected local media: {error}"))?;
    let inspected_snapshot = InputSnapshot::capture(&resolved, &inspected);
    if !inspected.file_type().is_file() || selected_snapshot != inspected_snapshot {
        return Err("The selected local media changed before preparation; try again".to_owned());
    }
    // A reviewed path can be replaced before this worker opens it. Nonblocking
    // + no-follow prevents a FIFO/symlink swap from hanging or changing the input.
    #[cfg(unix)]
    let mut input = std::fs::File::from(
        rustix::fs::open(
            &resolved,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(|error| format!("Cannot open selected local media safely: {error}"))?,
    );
    #[cfg(not(unix))]
    let mut input = std::fs::File::open(&resolved)
        .map_err(|error| format!("Cannot open selected local media: {error}"))?;
    let before = input
        .metadata()
        .map_err(|error| format!("Cannot inspect selected local media: {error}"))?;
    if !before.is_file() || before.len() == 0 {
        return Err("The selected media must be a nonempty regular file".to_owned());
    }
    let before_snapshot = InputSnapshot::capture(&resolved, &before);
    if inspected_snapshot != before_snapshot {
        return Err("The selected local media changed before preparation; try again".to_owned());
    }
    if before.len() > max_bytes {
        return Err("Media preparation staging size limit exceeded".to_owned());
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options
        .open(destination)
        .map_err(|error| format!("Cannot create private input copy: {error}"))?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut copied = 0_u64;
    loop {
        check_work(cancellation, started, deadline)?;
        let count = input
            .read(&mut buffer)
            .map_err(|error| format!("Cannot read selected local media: {error}"))?;
        if count == 0 {
            break;
        }
        copied = copied
            .checked_add(count as u64)
            .ok_or("Media preparation staging size limit exceeded")?;
        if copied > max_bytes {
            return Err("Media preparation staging size limit exceeded".to_owned());
        }
        output
            .write_all(&buffer[..count])
            .map_err(|error| format!("Cannot stage selected local media: {error}"))?;
        progress(copied, Some(before.len()));
    }
    output
        .flush()
        .map_err(|error| format!("Cannot finish private input copy: {error}"))?;
    let after = input
        .metadata()
        .map_err(|error| format!("Cannot recheck selected local media: {error}"))?;
    let current = std::fs::symlink_metadata(source)
        .map_err(|_| "The selected local media changed during preparation; try again")?;
    if copied != before.len()
        || !current.file_type().is_file()
        || before_snapshot != InputSnapshot::capture(&resolved, &after)
        || selected_snapshot != InputSnapshot::capture(source, &current)
    {
        return Err("The selected local media changed during preparation; try again".to_owned());
    }
    check_work(cancellation, started, deadline)?;
    Ok(copied)
}

/// Captures identity immediately because Windows obtains it from a path lookup.
#[derive(Eq, PartialEq)]
struct InputSnapshot {
    bytes: u64,
    modified: Option<std::time::SystemTime>,
    identity: Option<crate::file_identity::FilesystemIdentity>,
}

impl InputSnapshot {
    /// Combines portable content metadata with the platform's file identity.
    fn capture(path: &Path, metadata: &std::fs::Metadata) -> Self {
        Self {
            bytes: metadata.len(),
            modified: metadata.modified().ok(),
            identity: crate::file_identity::filesystem_identity(path, metadata),
        }
    }
}

/// Copies a pinned regular file before choosing byte-copy Opus or a supervised helper.
pub(crate) fn prepare_local_plan(
    config: &Config,
    source: &Path,
    directory: &Path,
    upload_video: bool,
    cancellation: &AtomicBool,
    progress: &mut dyn FnMut(u64, Option<u64>),
    max_bytes: u64,
    started: Instant,
    deadline: Duration,
) -> Result<StagingPlan, String> {
    let extension = source
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("media")
        .to_ascii_lowercase();
    let extension =
        if extension.len() <= 12 && extension.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            extension.as_str()
        } else {
            "media"
        };
    let copy_only = !upload_video && extension == "opus";
    let input = directory.join(if copy_only {
        "media.opus".to_owned()
    } else {
        format!("source.{extension}")
    });
    copy_local_input(
        source,
        &input,
        cancellation,
        progress,
        max_bytes,
        started,
        deadline,
    )?;
    if copy_only {
        return Ok(StagingPlan {
            command: None,
            expected_output: Some(input),
        });
    }
    Ok(ffmpeg_plan(config, &input, directory, upload_video))
}

/// FFmpeg reads only a complete private file; video output always uses copy.
pub(crate) fn ffmpeg_plan(
    config: &Config,
    input: &Path,
    directory: &Path,
    upload_video: bool,
) -> StagingPlan {
    let output = directory.join(if upload_video {
        "media.mkv"
    } else {
        "media.opus"
    });
    let mut command = Command::new(&config.providers.ffmpeg_executable);
    command
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-n",
            "-progress",
            "pipe:1",
            "-protocol_whitelist",
            "file,pipe",
            "-i",
        ])
        .arg(input);
    if upload_video {
        command.args([
            "-map", "0:v:0", "-map", "0:a?", "-c", "copy", "-f", "matroska",
        ]);
    } else {
        command.args([
            "-map", "0:a:0", "-vn", "-sn", "-dn", "-c:a", "libopus", "-f", "opus",
        ]);
    }
    command
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    StagingPlan {
        command: Some(command),
        expected_output: Some(output),
    }
}
