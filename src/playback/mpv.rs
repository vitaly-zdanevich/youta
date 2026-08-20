//! Invisible mpv process controlled through its JSON IPC protocol.
//!
//! The protocol is the same on every platform Youta runs on; only the channel
//! the lines travel through differs, and that lives in the private sibling
//! module `mpv_ipc`: a Unix socket there, a named pipe on Windows. Everything
//! here — request framing, event ordering, error redaction, the mpv command
//! line — is written once and compiled everywhere.

mod backend {
    use std::collections::VecDeque;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use super::super::mpv_ipc::{self, IpcLink};
    use super::super::{
        AudioOutputDriver, BufferedRange, PlaybackBackend, PlaybackEnd, PlaybackEndReason,
        PlaybackError, PlaybackEvent, PlaybackInput, PlaybackProfile, PlaybackStatus,
        PlayerCommand, ProcessPlaybackConfig, Result,
    };

    const IPC_TIMEOUT: Duration = Duration::from_secs(2);
    const MAX_MPV_SAMPLE_RATE_HZ: u32 = 768_000;
    const MAX_PENDING_EVENTS: usize = 32;
    const MAX_WARNING_LINES: usize = 12;
    const MAX_DIAGNOSTIC_CHARS: usize = 512;
    const MAX_STREAM_TITLE_BYTES: usize = 512;
    const MAX_RESOLVED_HTTP_HEADERS: usize = 32;
    const MAX_RESOLVED_HTTP_HEADER_BYTES: usize = 16 * 1024;
    const ICY_TITLE_OBSERVER_ID: u64 = 1;
    const ICY_TITLE_PROPERTY: &str = "metadata/by-key/icy-title";

    /// Audio-only selector used by every ordinary extractor-backed load.
    #[cfg(feature = "yt-dlp")]
    const YTDL_AUDIO_FORMAT: &str = "bestaudio[acodec^=opus]/bestaudio";

    /// Checked YouTube retry selector with a last-resort muxed stream.
    const YTDL_CHECKED_YOUTUBE_FORMAT: &str = "bestaudio[acodec^=opus]/bestaudio/best";

    struct MpvIpc {
        link: IpcLink,
        request_id: u64,
        events: VecDeque<PlaybackEvent>,
        warnings: VecDeque<String>,
        stream_title: Option<String>,
    }

    /// Headless mpv playback backend.
    ///
    /// mpv renders no terminal or video UI. Youta queries its JSON IPC socket
    /// and renders the seek bar, waveform, chapters, and controls itself.
    pub struct MpvBackend {
        child: Child,
        ipc: MpvIpc,
        socket_path: PathBuf,
        profile: PlaybackProfile,
        process_exit_reported: bool,
    }

    impl MpvBackend {
        /// Starts a private mpv instance and connects to its IPC socket.
        ///
        /// # Errors
        ///
        /// Returns an error when the runtime directory is unsafe, tuning is
        /// invalid, mpv cannot start, or its private IPC socket is unavailable.
        pub fn spawn(config: &ProcessPlaybackConfig) -> Result<Self> {
            ensure_private_directory(&config.runtime_dir)?;
            let socket_path = endpoint_path(&config.runtime_dir);
            remove_stale_socket(&socket_path)?;

            let mut command = mpv_command(config, &socket_path)?;
            let mut child = command.spawn().map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    PlaybackError::ExecutableUnavailable(
                        config.mpv_executable.display().to_string(),
                    )
                } else {
                    PlaybackError::Io(error)
                }
            })?;

            let link = wait_for_socket(&mut child, &socket_path)?;

            let mut backend = Self {
                child,
                ipc: MpvIpc::new(link),
                socket_path,
                profile: config.profile,
                process_exit_reported: false,
            };
            configure_ipc(&mut backend.ipc)?;
            Ok(backend)
        }

        fn send(&mut self, command: &[Value]) -> Result<Value> {
            self.ipc.send(command)
        }

        fn process_exit_event(&mut self) -> Result<Option<PlaybackEvent>> {
            if self.process_exit_reported {
                return Ok(None);
            }
            let Some(status) = self.child.try_wait()? else {
                return Ok(None);
            };
            self.process_exit_reported = true;
            let context = self.ipc.diagnostic();
            let status = format!("mpv exited with {status}");
            let diagnostic = Some(match context {
                Some(context) => bounded_text(&format!("{status}\n{context}")),
                None => status,
            });
            Ok(Some(PlaybackEvent::ProcessExited { diagnostic }))
        }

        fn property(&mut self, name: &str) -> Result<Option<Value>> {
            match self.send(&[json!("get_property"), json!(name)]) {
                Ok(Value::Null) => Ok(None),
                Ok(value) => Ok(Some(value)),
                Err(PlaybackError::Protocol(error))
                    if error == "property unavailable" || error == "property not found" =>
                {
                    Ok(None)
                }
                Err(error) => Err(error),
            }
        }

        fn set_property(&mut self, name: &str, value: Value) -> Result<()> {
            self.send(&[json!("set_property"), json!(name), value])?;
            Ok(())
        }

        fn ensure_processing_allowed(&self, operation: &'static str) -> Result<()> {
            if self.profile == PlaybackProfile::Direct {
                return Err(PlaybackError::DirectProfileRestriction(operation));
            }
            Ok(())
        }
    }

    impl MpvIpc {
        fn new(link: IpcLink) -> Self {
            Self {
                link,
                request_id: 0,
                events: VecDeque::new(),
                warnings: VecDeque::new(),
                stream_title: None,
            }
        }

        fn send(&mut self, command: &[Value]) -> Result<Value> {
            self.request_id = self.request_id.wrapping_add(1);
            let request_id = self.request_id;
            let request = json!({
                "command": command,
                "request_id": request_id,
            });
            self.link.write_line(&serde_json::to_vec(&request)?)?;

            for _ in 0..128 {
                let mut line = String::new();
                if self.link.read_line(&mut line)? == 0 {
                    return Err(PlaybackError::ProcessExited(String::new()));
                }
                let response: Value = serde_json::from_str(&line)?;
                self.handle_event(&response);
                if response.get("request_id").and_then(Value::as_u64) != Some(request_id) {
                    continue;
                }
                let error = response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("malformed response");
                if error != "success" {
                    return Err(PlaybackError::Protocol(mpv_protocol_error(command, error)));
                }
                return Ok(response.get("data").cloned().unwrap_or(Value::Null));
            }

            Err(PlaybackError::Protocol(mpv_protocol_error(
                command,
                "too many unrelated IPC events",
            )))
        }

        fn handle_event(&mut self, message: &Value) {
            match message.get("event").and_then(Value::as_str) {
                Some("start-file") => {
                    // mpv may retain the previous file's metadata until the
                    // replacement stream publishes its first property event.
                    self.stream_title = None;
                }
                Some("file-loaded") => self.push_event(PlaybackEvent::MediaLoaded),
                Some("playback-restart") => self.push_event(PlaybackEvent::PlaybackStarted),
                Some("end-file") => {
                    let reason_text = message
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    if reason_text == "redirect" {
                        // mpv reports an intermediate `end-file` while it
                        // replaces an M3U or similar playlist entry with the
                        // resolved stream. A new `start-file` follows; this is
                        // not the logical end of Youta's queue item.
                        self.warnings.clear();
                        self.stream_title = None;
                        return;
                    }
                    let reason = match reason_text {
                        "eof" => PlaybackEndReason::Eof,
                        "stop" | "quit" => PlaybackEndReason::Stop,
                        "error" => PlaybackEndReason::Error,
                        other => PlaybackEndReason::Other(bounded_text(other)),
                    };
                    let error = event_text(message, "error");
                    let file_error = event_text(message, "file_error")
                        .or_else(|| event_text(message, "file-error"));
                    let diagnostic = self.diagnostic();
                    self.warnings.clear();
                    self.stream_title = None;
                    self.push_event(PlaybackEvent::Ended(PlaybackEnd {
                        reason,
                        error,
                        file_error,
                        diagnostic,
                    }));
                }
                Some("property-change")
                    if message.get("id").and_then(Value::as_u64) == Some(ICY_TITLE_OBSERVER_ID)
                        && message.get("name").and_then(Value::as_str)
                            == Some(ICY_TITLE_PROPERTY) =>
                {
                    self.stream_title = message
                        .get("data")
                        .and_then(Value::as_str)
                        .and_then(normalize_stream_title);
                }
                Some("log-message") => self.capture_warning(message),
                _ => {}
            }
        }

        fn capture_warning(&mut self, message: &Value) {
            let level = message
                .get("level")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !matches!(level, "warn" | "error" | "fatal") {
                return;
            }
            let prefix = message
                .get("prefix")
                .and_then(Value::as_str)
                .unwrap_or("mpv");
            let text = message
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let warning = sanitize_diagnostic(&format!("{prefix}: {}", text.trim()));
            if warning.is_empty() {
                return;
            }
            if self.warnings.len() == MAX_WARNING_LINES {
                self.warnings.pop_front();
            }
            self.warnings.push_back(warning);
        }

        fn push_event(&mut self, event: PlaybackEvent) {
            if self.events.len() == MAX_PENDING_EVENTS {
                self.events.pop_front();
            }
            self.events.push_back(event);
        }

        fn diagnostic(&self) -> Option<String> {
            if self.warnings.is_empty() {
                None
            } else {
                Some(bounded_text(
                    &self
                        .warnings
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join("\n"),
                ))
            }
        }
    }

    /// Enables the bounded mpv event streams consumed by Youta.
    fn configure_ipc(ipc: &mut MpvIpc) -> Result<()> {
        for command in ipc_configuration_commands() {
            ipc.send(&command)?;
        }
        Ok(())
    }

    /// Returns the deterministic subscriptions installed on every mpv process.
    fn ipc_configuration_commands() -> [Vec<Value>; 2] {
        // mpv's JSON IPC log stream contains the authoritative extractor,
        // decoder, and audio-output failure text that otherwise occurs after
        // `loadfile` has already been acknowledged.
        // ICY title changes arrive on the existing audio connection.
        // Observing the property avoids another stream or HTTP poll and keeps
        // status reads cheap on low-power systems.
        [
            vec![json!("request_log_messages"), json!("warn")],
            vec![
                json!("observe_property"),
                json!(ICY_TITLE_OBSERVER_ID),
                json!(ICY_TITLE_PROPERTY),
            ],
        ]
    }

    fn event_text(message: &Value, field: &str) -> Option<String> {
        message
            .get(field)
            .and_then(Value::as_str)
            .map(sanitize_diagnostic)
            .filter(|value| !value.is_empty())
    }

    /// Adds a bounded allowlisted operation name without exposing arguments.
    fn mpv_protocol_error(command: &[Value], error: &str) -> String {
        if matches!(error, "property unavailable" | "property not found") {
            return error.to_owned();
        }
        let command_name = command
            .first()
            .and_then(Value::as_str)
            .filter(|name| {
                matches!(
                    *name,
                    "loadfile"
                        | "seek"
                        | "get_property"
                        | "set_property"
                        | "cycle"
                        | "add"
                        | "stop"
                        | "quit"
                        | "request_log_messages"
                        | "observe_property"
                )
            })
            .unwrap_or("unknown");
        let error = sanitize_diagnostic(error);
        let error = if error.is_empty() {
            "unspecified backend error"
        } else {
            &error
        };
        bounded_text(&format!("mpv IPC command `{command_name}` failed: {error}"))
    }

    fn sanitize_diagnostic(message: &str) -> String {
        let mut words = message.split_whitespace().peekable();
        let mut output = Vec::new();
        while let Some(word) = words.next() {
            let lowercase = word.to_ascii_lowercase();
            if lowercase.contains("http://") || lowercase.contains("https://") {
                output.push("<redacted-url>");
                continue;
            }
            if lowercase.starts_with("authorization:") {
                output.push("<redacted-secret>");
                let _ = words.next();
                let _ = words.next();
                continue;
            }
            if let Some(value) = lowercase.strip_prefix("authorization=") {
                output.push("<redacted-secret>");
                if value.is_empty() || value == "bearer" {
                    let _ = words.next();
                }
                continue;
            }
            if ["api_key", "apikey", "access_token", "token"].contains(&lowercase.as_str()) {
                output.push("<redacted-secret>");
                if words.peek().copied() == Some("=") {
                    let _ = words.next();
                }
                let _ = words.next();
                continue;
            }
            if ["api_key=", "apikey=", "access_token=", "token="]
                .iter()
                .any(|marker| lowercase.contains(marker))
            {
                output.push("<redacted-secret>");
                continue;
            }
            output.push(word);
        }
        let sanitized = output.join(" ");
        bounded_text(&sanitized)
    }

    fn bounded_text(message: &str) -> String {
        if message.chars().count() <= MAX_DIAGNOSTIC_CHARS {
            return message.to_owned();
        }
        let mut bounded = message
            .chars()
            .take(MAX_DIAGNOSTIC_CHARS.saturating_sub(1))
            .collect::<String>();
        bounded.push('…');
        bounded
    }

    /// Normalizes untrusted ICY text for one terminal line.
    fn normalize_stream_title(message: &str) -> Option<String> {
        let mut normalized = String::new();
        let mut pending_space = false;
        let mut truncated = false;
        for character in message.chars() {
            if character.is_control() || character.is_whitespace() {
                pending_space = !normalized.is_empty();
                continue;
            }
            let separator_bytes = usize::from(pending_space && !normalized.is_empty());
            if normalized
                .len()
                .saturating_add(separator_bytes)
                .saturating_add(character.len_utf8())
                > MAX_STREAM_TITLE_BYTES
            {
                truncated = true;
                break;
            }
            if separator_bytes == 1 {
                normalized.push(' ');
            }
            normalized.push(character);
            pending_space = false;
        }
        if normalized.is_empty() || normalized.eq_ignore_ascii_case("(error)") {
            return None;
        }
        if truncated {
            while normalized.len().saturating_add('…'.len_utf8()) > MAX_STREAM_TITLE_BYTES {
                normalized.pop();
            }
            normalized.push('…');
        }
        Some(normalized)
    }

    fn parse_buffered_ranges(cache_state: &Value) -> Vec<BufferedRange> {
        let Some(ranges) = cache_state.get("seekable-ranges").and_then(Value::as_array) else {
            return Vec::new();
        };
        let mut normalized = ranges
            .iter()
            .filter_map(|range| {
                normalized_buffered_range(
                    range.get("start").and_then(Value::as_f64)?,
                    range.get("end").and_then(Value::as_f64)?,
                )
            })
            .collect::<Vec<_>>();
        normalized.sort_unstable_by_key(|range| (range.start, range.end));

        let mut merged: Vec<BufferedRange> = Vec::with_capacity(normalized.len());
        for range in normalized {
            if let Some(previous) = merged.last_mut()
                && range.start <= previous.end
            {
                previous.end = previous.end.max(range.end);
            } else {
                merged.push(range);
            }
        }
        merged
    }

    fn normalized_buffered_range(start: f64, end: f64) -> Option<BufferedRange> {
        if !start.is_finite() || !end.is_finite() {
            return None;
        }
        // Opus/WebM codec preroll can make mpv report a small negative cache
        // start for a valid range beginning at the media timeline origin.
        if start < -1.0 {
            return None;
        }
        let start = start.max(0.0);
        if end <= start {
            return None;
        }
        let start = Duration::try_from_secs_f64(start).ok()?;
        let end = Duration::try_from_secs_f64(end).ok()?;
        (start < end).then_some(BufferedRange { start, end })
    }

    fn loadfile_command(input: &PlaybackInput) -> Result<Vec<Value>> {
        if input.verify_remote_format && input.bypass_ytdl {
            return Err(PlaybackError::InvalidValue(
                "a resolved direct stream cannot request yt-dlp format verification".to_owned(),
            ));
        }
        if !input.bypass_ytdl && !input.http_headers.is_empty() {
            return Err(PlaybackError::InvalidValue(
                "resolved HTTP headers require yt-dlp bypass".to_owned(),
            ));
        }
        let mut command = vec![json!("loadfile"), json!(input.location), json!("replace")];
        let mut options = serde_json::Map::new();
        // mpv's global pause property survives `loadfile replace`. Make a new
        // application-owned selection start atomically even when the previous
        // item was paused.
        options.insert("pause".to_owned(), Value::String("no".to_owned()));
        if !input.start_at.is_zero() {
            // `loadfile` is asynchronous: a following `seek` can run before
            // mpv has loaded a seekable stream. The per-file `start` option
            // applies the resume position atomically when loading begins.
            options.insert(
                "start".to_owned(),
                Value::String(input.start_at.as_secs_f64().to_string()),
            );
        }
        if let Some(title) = input.title.as_ref() {
            // Direct CDN URLs have long, expiring query strings from which mpv
            // otherwise derives an unreadable `media-title`. Keep the
            // application-owned title authoritative for the OSD, MPRIS, and
            // status polling surfaces.
            options.insert("force-media-title".to_owned(), Value::String(title.clone()));
        }
        if input.verify_remote_format {
            options.insert(
                "ytdl-raw-options".to_owned(),
                Value::String("check-formats=".to_owned()),
            );
            options.insert(
                "ytdl-format".to_owned(),
                Value::String(YTDL_CHECKED_YOUTUBE_FORMAT.to_owned()),
            );
        }
        if input.bypass_ytdl {
            options.insert("ytdl".to_owned(), Value::String("no".to_owned()));
            if let Some(headers) = mpv_http_header_fields(input)? {
                options.insert("http-header-fields".to_owned(), Value::String(headers));
            }
        }
        if !options.is_empty() {
            // mpv 0.38 and newer reserve the third optional argument for a
            // playlist insertion index. Per-file options therefore occupy the
            // fourth optional argument after the unused -1 index.
            command.push(json!(-1));
            command.push(Value::Object(options));
        }
        Ok(command)
    }

    fn stream_recording_property_value(path: Option<PathBuf>) -> Result<Value> {
        match path {
            Some(path) => {
                if path.as_os_str().is_empty() {
                    return Err(PlaybackError::InvalidValue(
                        "stream recording path cannot be empty".to_owned(),
                    ));
                }
                // JSON IPC can transmit only Unicode strings. Rejecting an
                // invalid path avoids lossy conversion to a different output
                // filename.
                let path = path.to_str().ok_or_else(|| {
                    PlaybackError::InvalidValue(
                        "stream recording path must be valid UTF-8".to_owned(),
                    )
                })?;
                Ok(Value::String(path.to_owned()))
            }
            // mpv's `stream-record` option uses `no` to close the current
            // output file and disable further stream recording.
            None => Ok(Value::String("no".to_owned())),
        }
    }

    /// Validates extractor-provided fields and encodes mpv's comma-separated
    /// string-list syntax without exposing values in an error.
    fn mpv_http_header_fields(input: &PlaybackInput) -> Result<Option<String>> {
        if input.http_headers.is_empty() {
            return Ok(None);
        }
        if input.http_headers.iter().len() > MAX_RESOLVED_HTTP_HEADERS {
            return Err(PlaybackError::InvalidValue(
                "resolved media returned too many HTTP headers".to_owned(),
            ));
        }
        let mut total_bytes = 0_usize;
        let mut fields = Vec::with_capacity(input.http_headers.iter().len());
        for (name, value) in input.http_headers.iter() {
            if name.is_empty()
                || !name.bytes().all(is_http_token_byte)
                || value
                    .bytes()
                    .any(|byte| matches!(byte, b'\r' | b'\n' | b'\0'))
            {
                return Err(PlaybackError::InvalidValue(
                    "resolved media returned an invalid HTTP header".to_owned(),
                ));
            }
            total_bytes = total_bytes
                .saturating_add(name.len())
                .saturating_add(value.len())
                .saturating_add(2);
            if total_bytes > MAX_RESOLVED_HTTP_HEADER_BYTES {
                return Err(PlaybackError::InvalidValue(
                    "resolved media HTTP headers exceed the in-memory limit".to_owned(),
                ));
            }
            fields.push(escape_mpv_string_list_element(&format!("{name}: {value}")));
        }
        Ok(Some(fields.join(",")))
    }

    const fn is_http_token_byte(byte: u8) -> bool {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            )
    }

    fn escape_mpv_string_list_element(value: &str) -> String {
        let mut escaped = String::with_capacity(value.len());
        for character in value.chars() {
            if matches!(character, '\\' | ',') {
                escaped.push('\\');
            }
            escaped.push(character);
        }
        escaped
    }

    fn mpv_command(config: &ProcessPlaybackConfig, socket_path: &Path) -> Result<Command> {
        if config.profile == PlaybackProfile::Direct
            && let Some(sample_rate) = config.audiophile.output_sample_rate_hz
            && !(1..=MAX_MPV_SAMPLE_RATE_HZ).contains(&sample_rate)
        {
            return Err(PlaybackError::InvalidValue(format!(
                "output sample rate {sample_rate} is outside 1..={MAX_MPV_SAMPLE_RATE_HZ} Hz"
            )));
        }

        let mut command = Command::new(&config.mpv_executable);
        crate::child_process::quiet(&mut command);
        command
            .arg("--no-config")
            .arg("--idle=yes")
            .arg("--no-video")
            .arg("--audio-display=no")
            .arg("--terminal=no")
            .arg("--input-terminal=no")
            .arg("--really-quiet")
            .arg(format!("--input-ipc-server={}", socket_path.display()));

        #[cfg(feature = "yt-dlp")]
        command
            .arg(format!(
                "--script-opts=ytdl_hook-ytdl_path={}",
                config.yt_dlp_executable.display()
            ))
            .arg(format!("--ytdl-format={YTDL_AUDIO_FORMAT}"))
            .arg(format!(
                "--ytdl-raw-options=js-runtimes={}",
                super::super::ytdlp::ADDITIONAL_JS_RUNTIME
            ));

        if let Some(driver) = config.audio_output.mpv_name() {
            command.arg(format!("--ao={driver}"));
        }
        match config.profile {
            PlaybackProfile::Balanced => {}
            PlaybackProfile::Battery => {
                command.arg("--audio-buffer=0.5");
            }
            PlaybackProfile::Direct => {
                command
                    .arg("--audio-pitch-correction=no")
                    .arg("--volume=100");
                if config.audiophile.exclusive_device {
                    command.arg("--audio-exclusive=yes");
                }
                if let Some(sample_rate) = config.audiophile.output_sample_rate_hz {
                    command.arg(format!("--audio-samplerate={sample_rate}"));
                } else if config.audiophile.avoid_resampling {
                    command.arg("--audio-samplerate=0");
                }
                if config.audiophile.avoid_resampling
                    && config.audio_output == AudioOutputDriver::Alsa
                {
                    command.arg("--alsa-resample=no");
                }
            }
        }
        if let Some(device) = &config.audio_device {
            command.arg(format!("--audio-device={device}"));
        }

        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Ok(command)
    }

    impl PlaybackBackend for MpvBackend {
        fn play(&mut self, input: &PlaybackInput) -> Result<()> {
            if input.location.trim().is_empty() {
                return Err(PlaybackError::InvalidValue(
                    "media location cannot be empty".to_owned(),
                ));
            }
            // Do not show metadata retained from the previous stream while
            // mpv is loading a replacement.
            self.ipc.stream_title = None;
            self.send(&loadfile_command(input)?)?;
            Ok(())
        }

        fn command(&mut self, command: PlayerCommand) -> Result<()> {
            match command {
                PlayerCommand::TogglePause => {
                    self.send(&[json!("cycle"), json!("pause")])?;
                }
                PlayerCommand::SetPaused(paused) => self.set_property("pause", json!(paused))?,
                PlayerCommand::SeekRelative(seconds) => {
                    self.send(&[json!("seek"), json!(seconds), json!("relative")])?;
                }
                PlayerCommand::SeekAbsolute(position) => {
                    self.send(&[
                        json!("seek"),
                        json!(position.as_secs_f64()),
                        json!("absolute"),
                    ])?;
                }
                PlayerCommand::SeekPercent(percent) => {
                    if !(0.0..=100.0).contains(&percent) {
                        return Err(PlaybackError::InvalidValue(format!(
                            "seek percentage {percent} is outside 0..=100"
                        )));
                    }
                    self.send(&[json!("seek"), json!(percent), json!("absolute-percent")])?;
                }
                PlayerCommand::SetVolume(volume) => {
                    self.ensure_processing_allowed("software volume")?;
                    if volume > 100 {
                        return Err(PlaybackError::InvalidValue(format!(
                            "volume {volume} is outside 0..=100"
                        )));
                    }
                    self.set_property("volume", json!(volume))?;
                }
                PlayerCommand::SetSpeed(speed) => {
                    self.ensure_processing_allowed("playback speed")?;
                    if !(0.5..=3.0).contains(&speed) {
                        return Err(PlaybackError::InvalidValue(format!(
                            "speed {speed} is outside 0.5..=3.0"
                        )));
                    }
                    self.set_property("speed", json!(speed))?;
                }
                PlayerCommand::ChangeChapter(delta) => {
                    self.send(&[json!("add"), json!("chapter"), json!(delta)])?;
                }
                PlayerCommand::SetRepeat(enabled) => {
                    self.set_property("loop-file", json!(if enabled { "inf" } else { "no" }))?;
                }
                PlayerCommand::SetStreamRecording(path) => {
                    self.set_property("stream-record", stream_recording_property_value(path)?)?;
                }
                PlayerCommand::Stop => {
                    self.send(&[json!("stop")])?;
                }
            }
            Ok(())
        }

        fn status(&mut self) -> Result<PlaybackStatus> {
            let idle = self
                .property("idle-active")?
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let position = self
                .property("time-pos")?
                .and_then(|value| value.as_f64())
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map_or(Duration::ZERO, Duration::from_secs_f64);
            let duration = self
                .property("duration")?
                .and_then(|value| value.as_f64())
                .filter(|value| value.is_finite() && *value >= 0.0)
                .map(Duration::from_secs_f64);
            let paused = self
                .property("pause")?
                .and_then(|value| value.as_bool())
                .unwrap_or(true);
            let rounded_volume = self
                .property("volume")?
                .and_then(|value| value.as_f64())
                .filter(|value| value.is_finite())
                .unwrap_or(100.0)
                .clamp(0.0, 100.0)
                .round();
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "the finite value is rounded and clamped to the full u8 subset 0..=100"
            )]
            let volume = rounded_volume as u8;
            let speed = self
                .property("speed")?
                .and_then(|value| value.as_f64())
                .unwrap_or(1.0);
            let chapter = self.property("chapter")?.and_then(|value| value.as_i64());
            let buffering = self
                .property("paused-for-cache")?
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let buffered_ranges = self
                .property("demuxer-cache-state")?
                .as_ref()
                .map_or_else(Vec::new, parse_buffered_ranges);
            let title = self
                .property("media-title")?
                .and_then(|value| value.as_str().map(ToOwned::to_owned));
            let stream_title = self.ipc.stream_title.clone();

            Ok(PlaybackStatus {
                idle,
                live: false,
                live_seekable_range: None,
                position,
                duration,
                paused,
                volume,
                speed,
                chapter,
                buffering,
                buffered_ranges,
                title,
                stream_title,
            })
        }

        fn poll_event(&mut self) -> Result<Option<PlaybackEvent>> {
            if let Some(event) = self.ipc.events.pop_front() {
                return Ok(Some(event));
            }
            if self.process_exit_reported {
                return Ok(None);
            }
            if let Some(event) = self.process_exit_event()? {
                return Ok(Some(event));
            }

            // A cheap request causes mpv to flush lifecycle and log messages
            // already ahead of its response on the ordered IPC stream.
            let result = self.send(&[json!("get_property"), json!("idle-active")]);
            if let Some(event) = self.ipc.events.pop_front() {
                return Ok(Some(event));
            }
            match result {
                Ok(_) => Ok(None),
                Err(PlaybackError::ProcessExited(_)) => {
                    if let Some(event) = self.process_exit_event()? {
                        Ok(Some(event))
                    } else {
                        self.process_exit_reported = true;
                        Ok(Some(PlaybackEvent::ProcessExited {
                            diagnostic: self.ipc.diagnostic(),
                        }))
                    }
                }
                Err(error) => Err(error),
            }
        }

        fn shutdown(&mut self) -> Result<()> {
            let _ = self.send(&[json!("quit")]);
            if self.child.try_wait()?.is_none() {
                self.child.kill()?;
                let _ = self.child.wait();
            }
            remove_stale_socket(&self.socket_path)?;
            Ok(())
        }
    }

    impl Drop for MpvBackend {
        fn drop(&mut self) {
            let _ = self.shutdown();
        }
    }

    /// Names the private control channel this process asks mpv to publish.
    ///
    /// On Unix that is a socket inside Youta's own runtime directory, which the
    /// caller has already made private. On Windows a named pipe does not live
    /// in the filesystem at all: it lives in the kernel's `\\.\pipe\` namespace,
    /// so the runtime directory has nothing to do with it and the process
    /// identifier alone keeps two copies of Youta apart.
    fn endpoint_path(runtime_dir: &Path) -> PathBuf {
        let pid = std::process::id();
        #[cfg(unix)]
        {
            runtime_dir.join(format!("mpv-{pid}.sock"))
        }
        #[cfg(windows)]
        {
            let _ = runtime_dir;
            PathBuf::from(format!(r"\\.\pipe\youta-mpv-{pid}"))
        }
    }

    fn ensure_private_directory(path: &Path) -> Result<()> {
        fs::create_dir_all(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Clears a leftover endpoint, refusing to delete anything that is not one.
    ///
    /// Only the Unix endpoint can be left behind: a named pipe is owned by the
    /// process that created it and disappears when mpv exits, so there is
    /// nothing on Windows to clean up and nothing to refuse.
    fn remove_stale_socket(path: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;

            match fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_socket() => {
                    fs::remove_file(path)?;
                    Ok(())
                }
                Ok(_) => Err(PlaybackError::Protocol(format!(
                    "refusing to replace non-socket path {}",
                    path.display()
                ))),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            }
        }
        #[cfg(windows)]
        {
            let _ = path;
            Ok(())
        }
    }

    fn wait_for_socket(child: &mut Child, path: &Path) -> Result<IpcLink> {
        let deadline = Instant::now() + IPC_TIMEOUT;
        loop {
            match mpv_ipc::connect(path, IPC_TIMEOUT) {
                Ok(link) => return Ok(link),
                Err(error) if Instant::now() < deadline => {
                    if let Some(status) = child.try_wait()? {
                        return Err(PlaybackError::ProcessExited(format!(" ({status})")));
                    }
                    if !mpv_ipc::connection_is_pending(&error) {
                        return Err(error.into());
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error.into());
                }
            }
        }
    }

    // The mock IPC peer is a socket pair, which only Unix offers, so the
    // protocol suite runs where that pair can be built. What it covers —
    // framing, event order, redaction, the mpv command line — is the code both
    // platforms share; only the pipe underneath it differs, and that lives in
    // `mpv_ipc`.
    #[cfg(all(test, unix))]
    mod tests {
        use std::collections::BTreeMap;
        use std::ffi::{OsStr, OsString};
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixStream;
        use std::sync::mpsc;

        use super::*;
        use crate::playback::{AudiophilePlaybackOptions, PlaybackHttpHeaders};

        fn process_config(
            profile: PlaybackProfile,
            audio_output: AudioOutputDriver,
        ) -> ProcessPlaybackConfig {
            ProcessPlaybackConfig {
                mpv_executable: PathBuf::from("mock-mpv"),
                yt_dlp_executable: PathBuf::from("mock-yt-dlp"),
                runtime_dir: PathBuf::from("/tmp/youta-test-runtime"),
                audio_output,
                audio_device: None,
                profile,
                audiophile: AudiophilePlaybackOptions::default(),
            }
        }

        fn command_arguments(command: &Command) -> Vec<String> {
            command
                .get_args()
                .map(OsStr::to_string_lossy)
                .map(std::borrow::Cow::into_owned)
                .collect()
        }

        fn ipc_after_script(messages: Vec<Value>) -> MpvIpc {
            let (client, server) = UnixStream::pair().expect("mock IPC pair");
            let server_thread = thread::spawn(move || {
                let mut reader = BufReader::new(server);
                let mut request_line = String::new();
                reader
                    .read_line(&mut request_line)
                    .expect("read mock request");
                let request: Value =
                    serde_json::from_str(&request_line).expect("parse mock request");
                let request_id = request
                    .get("request_id")
                    .and_then(Value::as_u64)
                    .expect("request ID");

                for message in messages {
                    serde_json::to_writer(reader.get_mut(), &message).expect("write mock event");
                    reader.get_mut().write_all(b"\n").expect("event newline");
                }
                serde_json::to_writer(
                    reader.get_mut(),
                    &json!({"request_id": request_id, "error": "success"}),
                )
                .expect("write mock response");
                reader.get_mut().write_all(b"\n").expect("response newline");
                reader.get_mut().flush().expect("flush mock response");
            });

            let mut ipc = MpvIpc::new(IpcLink::over(client));
            ipc.send(&[json!("get_property"), json!("idle-active")])
                .expect("mock IPC request");
            server_thread.join().expect("mock IPC server");
            ipc
        }

        #[test]
        fn ipc_configuration_includes_the_icy_title_subscription() {
            assert_eq!(
                ipc_configuration_commands(),
                [
                    vec![json!("request_log_messages"), json!("warn")],
                    vec![
                        json!("observe_property"),
                        json!(ICY_TITLE_OBSERVER_ID),
                        json!(ICY_TITLE_PROPERTY),
                    ],
                ]
            );
        }

        fn backend_with_properties(
            properties: impl IntoIterator<Item = (&'static str, Value)>,
        ) -> (MpvBackend, thread::JoinHandle<()>) {
            let properties = properties
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect::<BTreeMap<_, _>>();
            let (client, server) = UnixStream::pair().expect("mock IPC pair");
            let server_thread = thread::spawn(move || {
                let mut reader = BufReader::new(server);
                loop {
                    let mut request_line = String::new();
                    if reader
                        .read_line(&mut request_line)
                        .expect("read mock status request")
                        == 0
                    {
                        break;
                    }
                    let request: Value =
                        serde_json::from_str(&request_line).expect("parse mock status request");
                    let request_id = request
                        .get("request_id")
                        .and_then(Value::as_u64)
                        .expect("request ID");
                    let command = request
                        .get("command")
                        .and_then(Value::as_array)
                        .expect("command array");
                    let name = command.first().and_then(Value::as_str).unwrap_or_default();
                    let data = if name == "get_property" {
                        command
                            .get(1)
                            .and_then(Value::as_str)
                            .and_then(|property| properties.get(property))
                            .cloned()
                            .unwrap_or(Value::Null)
                    } else {
                        Value::Null
                    };
                    serde_json::to_writer(
                        reader.get_mut(),
                        &json!({
                            "request_id": request_id,
                            "error": "success",
                            "data": data,
                        }),
                    )
                    .expect("write mock status response");
                    reader
                        .get_mut()
                        .write_all(b"\n")
                        .expect("status response newline");
                    reader
                        .get_mut()
                        .flush()
                        .expect("flush mock status response");
                    if name == "quit" {
                        break;
                    }
                }
            });
            let child = Command::new("sh")
                .args(["-c", "sleep 60"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("long-lived mock mpv");
            (
                MpvBackend {
                    child,
                    ipc: MpvIpc::new(IpcLink::over(client)),
                    socket_path: PathBuf::from("/tmp/youta-unused-buffer-status.sock"),
                    profile: PlaybackProfile::Balanced,
                    process_exit_reported: false,
                },
                server_thread,
            )
        }

        fn backend_with_command_recorder() -> (
            MpvBackend,
            mpsc::Receiver<Vec<Value>>,
            thread::JoinHandle<()>,
        ) {
            let (client, server) = UnixStream::pair().expect("mock IPC pair");
            let (command_sender, command_receiver) = mpsc::channel();
            let server_thread = thread::spawn(move || {
                let mut reader = BufReader::new(server);
                loop {
                    let mut request_line = String::new();
                    if reader
                        .read_line(&mut request_line)
                        .expect("read mock command request")
                        == 0
                    {
                        break;
                    }
                    let request: Value =
                        serde_json::from_str(&request_line).expect("parse mock command request");
                    let request_id = request
                        .get("request_id")
                        .and_then(Value::as_u64)
                        .expect("request ID");
                    let command = request
                        .get("command")
                        .and_then(Value::as_array)
                        .cloned()
                        .expect("command array");
                    let should_quit = command.first().and_then(Value::as_str) == Some("quit");
                    command_sender.send(command).expect("record mock command");
                    serde_json::to_writer(
                        reader.get_mut(),
                        &json!({"request_id": request_id, "error": "success"}),
                    )
                    .expect("write mock command response");
                    reader
                        .get_mut()
                        .write_all(b"\n")
                        .expect("mock command response newline");
                    reader
                        .get_mut()
                        .flush()
                        .expect("flush mock command response");
                    if should_quit {
                        break;
                    }
                }
            });
            let child = Command::new("sh")
                .args(["-c", "sleep 60"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("long-lived mock mpv");
            (
                MpvBackend {
                    child,
                    ipc: MpvIpc::new(IpcLink::over(client)),
                    socket_path: PathBuf::from("/tmp/youta-unused-command-recorder.sock"),
                    profile: PlaybackProfile::Balanced,
                    process_exit_reported: false,
                },
                command_receiver,
                server_thread,
            )
        }

        #[test]
        fn normal_loadfile_command_atomically_clears_global_pause() {
            let input = PlaybackInput::new("https://www.youtube.com/watch?v=fixture");

            assert_eq!(
                loadfile_command(&input).expect("normal loadfile command"),
                vec![
                    json!("loadfile"),
                    json!("https://www.youtube.com/watch?v=fixture"),
                    json!("replace"),
                    json!(-1),
                    json!({"pause": "no"}),
                ]
            );
        }

        #[test]
        fn stream_recording_value_serializes_unicode_paths_and_stop_marker() {
            assert_eq!(
                stream_recording_property_value(Some(PathBuf::from(
                    "/tmp/radio recordings/\u{65e5}\u{672c}\u{8a9e}.opus"
                )))
                .expect("Unicode recording path"),
                json!("/tmp/radio recordings/\u{65e5}\u{672c}\u{8a9e}.opus")
            );
            assert_eq!(
                stream_recording_property_value(None).expect("stop recording marker"),
                json!("no")
            );
        }

        #[test]
        fn stream_recording_value_rejects_empty_and_non_unicode_paths() {
            let empty = stream_recording_property_value(Some(PathBuf::new()))
                .expect_err("empty recording path must be rejected");
            assert!(matches!(empty, PlaybackError::InvalidValue(_)));

            let non_unicode = PathBuf::from(OsString::from_vec(vec![b'/', 0xFF]));
            let non_unicode = stream_recording_property_value(Some(non_unicode))
                .expect_err("non-Unicode recording path must be rejected");
            assert!(matches!(non_unicode, PlaybackError::InvalidValue(_)));
        }

        #[test]
        fn stream_recording_command_dispatches_start_and_stop_property_updates() {
            let (mut backend, command_receiver, server_thread) = backend_with_command_recorder();

            backend
                .command(PlayerCommand::SetStreamRecording(Some(PathBuf::from(
                    "/tmp/station recording.opus",
                ))))
                .expect("start stream recording");
            backend
                .command(PlayerCommand::SetStreamRecording(None))
                .expect("stop stream recording");
            backend.shutdown().expect("shut down mock backend");
            server_thread.join().expect("mock command server");

            assert_eq!(
                command_receiver.into_iter().collect::<Vec<_>>(),
                vec![
                    vec![
                        json!("set_property"),
                        json!("stream-record"),
                        json!("/tmp/station recording.opus"),
                    ],
                    vec![json!("set_property"), json!("stream-record"), json!("no")],
                    vec![json!("quit")],
                ]
            );
        }

        #[test]
        fn verified_loadfile_uses_a_per_file_ytdl_option_map() {
            let mut input = PlaybackInput::new("https://www.youtube.com/watch?v=fixture");
            input.verify_remote_format = true;

            assert_eq!(
                loadfile_command(&input).expect("verified loadfile command"),
                vec![
                    json!("loadfile"),
                    json!("https://www.youtube.com/watch?v=fixture"),
                    json!("replace"),
                    json!(-1),
                    json!({
                        "pause": "no",
                        "ytdl-raw-options": "check-formats=",
                        "ytdl-format": YTDL_CHECKED_YOUTUBE_FORMAT,
                    }),
                ]
            );
        }

        #[cfg(feature = "yt-dlp")]
        #[test]
        fn checked_load_allows_the_reported_muxed_hls_formats_as_a_last_resort() {
            let formats = json!([
                {"format_id": "91", "protocol": "m3u8_native", "acodec": "mp4a.40.5", "vcodec": "avc1.4d400b"},
                {"format_id": "92", "protocol": "m3u8_native", "acodec": "mp4a.40.5", "vcodec": "avc1.4d400c"},
                {"format_id": "93", "protocol": "m3u8_native", "acodec": "mp4a.40.2", "vcodec": "avc1.4d401e"},
                {"format_id": "94", "protocol": "m3u8_native", "acodec": "mp4a.40.2", "vcodec": "avc1.4d401f"},
                {"format_id": "95", "protocol": "m3u8_native", "acodec": "mp4a.40.2", "vcodec": "avc1.4d401f"},
                {"format_id": "96", "protocol": "m3u8_native", "acodec": "mp4a.40.2", "vcodec": "avc1.4d4028"},
            ]);
            let formats = formats.as_array().expect("format fixture array");

            assert_eq!(
                formats
                    .iter()
                    .filter_map(|format| format.get("format_id").and_then(Value::as_str))
                    .collect::<Vec<_>>(),
                ["91", "92", "93", "94", "95", "96"]
            );
            assert!(formats.iter().all(|format| {
                format.get("protocol").and_then(Value::as_str) == Some("m3u8_native")
                    && format.get("acodec").and_then(Value::as_str) != Some("none")
                    && format.get("vcodec").and_then(Value::as_str) != Some("none")
            }));
            assert_eq!(
                YTDL_CHECKED_YOUTUBE_FORMAT,
                format!("{YTDL_AUDIO_FORMAT}/best"),
                "the retry must preserve normal audio quality before allowing a muxed format"
            );

            let mut input = PlaybackInput::new("https://www.youtube.com/watch?v=fixture");
            input.verify_remote_format = true;
            let command = loadfile_command(&input).expect("checked HLS-only loadfile command");
            let options = command
                .get(4)
                .and_then(Value::as_object)
                .expect("per-file options");

            assert_eq!(
                options.get("ytdl-format").and_then(Value::as_str),
                Some(YTDL_CHECKED_YOUTUBE_FORMAT)
            );
            assert_eq!(
                options.get("ytdl-raw-options").and_then(Value::as_str),
                Some("check-formats=")
            );
        }

        #[test]
        fn resumed_loadfile_uses_an_atomic_per_file_start_option() {
            let mut input = PlaybackInput::new("https://www.youtube.com/watch?v=fixture");
            input.start_at = Duration::from_secs(30);

            assert_eq!(
                loadfile_command(&input).expect("resumed loadfile command"),
                vec![
                    json!("loadfile"),
                    json!("https://www.youtube.com/watch?v=fixture"),
                    json!("replace"),
                    json!(-1),
                    json!({
                        "pause": "no",
                        "start": "30",
                    }),
                ]
            );
        }

        #[test]
        fn resumed_verified_loadfile_combines_its_per_file_options() {
            let mut input = PlaybackInput::new("https://www.youtube.com/watch?v=fixture");
            input.start_at = Duration::from_millis(30_500);
            input.verify_remote_format = true;

            assert_eq!(
                loadfile_command(&input).expect("resumed verified loadfile command"),
                vec![
                    json!("loadfile"),
                    json!("https://www.youtube.com/watch?v=fixture"),
                    json!("replace"),
                    json!(-1),
                    json!({
                        "pause": "no",
                        "start": "30.5",
                        "ytdl-raw-options": "check-formats=",
                        "ytdl-format": YTDL_CHECKED_YOUTUBE_FORMAT,
                    }),
                ]
            );
        }

        #[test]
        fn resolved_loadfile_bypasses_ytdl_and_escapes_sensitive_headers() {
            let mut input = PlaybackInput::new(
                "https://cdn.example/videoplayback?mime=audio%2Fwebm&rqh=1&gir=yes&clen=101236984",
            );
            input.title = Some("Human-readable video name".to_owned());
            input.bypass_ytdl = true;
            input.http_headers = PlaybackHttpHeaders::new(std::collections::BTreeMap::from([
                ("Accept".to_owned(), "audio/webm,audio/ogg".to_owned()),
                ("X-Path".to_owned(), r"one\two".to_owned()),
            ]));

            assert_eq!(
                loadfile_command(&input).expect("resolved loadfile command"),
                vec![
                    json!("loadfile"),
                    json!(
                        "https://cdn.example/videoplayback?mime=audio%2Fwebm&rqh=1&gir=yes&clen=101236984"
                    ),
                    json!("replace"),
                    json!(-1),
                    json!({
                        "force-media-title": "Human-readable video name",
                        "http-header-fields": r"Accept: audio/webm\,audio/ogg,X-Path: one\\two",
                        "pause": "no",
                        "ytdl": "no",
                    }),
                ]
            );
            let debug = format!("{:?}", input.http_headers);
            assert!(debug.contains("Accept"));
            assert!(!debug.contains("audio/webm"));
        }

        #[test]
        fn resolved_header_validation_rejects_injection_without_echoing_values() {
            let secret = "secret-value\r\nX-Injected: yes";
            let mut input = PlaybackInput::new("https://cdn.example/audio");
            input.bypass_ytdl = true;
            input.http_headers = PlaybackHttpHeaders::new(std::collections::BTreeMap::from([(
                "Cookie".to_owned(),
                secret.to_owned(),
            )]));

            let error = loadfile_command(&input).expect_err("header injection must be rejected");

            assert!(!error.to_string().contains(secret));
            assert!(!format!("{input:?}").contains(secret));
        }

        #[test]
        fn backend_play_unpauses_after_one_atomic_resumed_loadfile_request() {
            let temporary = tempfile::tempdir().expect("temporary directory");
            let (client, server) = UnixStream::pair().expect("mock IPC pair");
            let (command_sender, command_receiver) = mpsc::channel();
            let server_thread = thread::spawn(move || {
                let mut reader = BufReader::new(server);
                loop {
                    let mut request_line = String::new();
                    if reader
                        .read_line(&mut request_line)
                        .expect("read mock play request")
                        == 0
                    {
                        break;
                    }
                    let request: Value =
                        serde_json::from_str(&request_line).expect("parse mock play request");
                    let request_id = request
                        .get("request_id")
                        .and_then(Value::as_u64)
                        .expect("request ID");
                    let command = request
                        .get("command")
                        .and_then(Value::as_array)
                        .cloned()
                        .expect("command array");
                    let should_quit = command.first().and_then(Value::as_str) == Some("quit");
                    command_sender
                        .send(command)
                        .expect("record mock play command");
                    serde_json::to_writer(
                        reader.get_mut(),
                        &json!({"request_id": request_id, "error": "success"}),
                    )
                    .expect("write mock play response");
                    reader
                        .get_mut()
                        .write_all(b"\n")
                        .expect("mock play response newline");
                    reader.get_mut().flush().expect("flush mock play response");
                    if should_quit {
                        break;
                    }
                }
            });
            let child = Command::new("sh")
                .args(["-c", "sleep 60"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("long-lived mock mpv");
            let mut backend = MpvBackend {
                child,
                ipc: MpvIpc::new(IpcLink::over(client)),
                socket_path: temporary.path().join("unused.sock"),
                profile: PlaybackProfile::Balanced,
                process_exit_reported: false,
            };
            let mut input = PlaybackInput::new("/tmp/fixture.opus");
            input.start_at = Duration::from_secs(30);

            backend.play(&input).expect("atomic resumed load");
            backend.shutdown().expect("shut down mock backend");
            server_thread.join().expect("mock play server");

            assert_eq!(
                command_receiver.into_iter().collect::<Vec<_>>(),
                vec![
                    vec![
                        json!("loadfile"),
                        json!("/tmp/fixture.opus"),
                        json!("replace"),
                        json!(-1),
                        json!({
                            "pause": "no",
                            "start": "30",
                        }),
                    ],
                    vec![json!("quit")],
                ],
                "resume and unpause options must share the atomic replacement request"
            );
        }

        #[test]
        fn protocol_errors_identify_only_allowlisted_commands_and_redact_context() {
            let secret = "do-not-report";
            let error = mpv_protocol_error(
                &[
                    json!("loadfile"),
                    json!(format!("https://example.invalid/audio?token={secret}")),
                ],
                &format!(
                    "error running command https://example.invalid/audio?token={secret} \
                     token={secret}"
                ),
            );

            assert!(error.contains("`loadfile`"));
            assert!(error.contains("error running command"));
            assert!(error.contains("<redacted-url>"));
            assert!(error.contains("<redacted-secret>"));
            assert!(!error.contains("example.invalid"));
            assert!(!error.contains(secret));

            let unknown = mpv_protocol_error(
                &[json!("secret-command"), json!(secret)],
                "permission denied",
            );
            assert!(unknown.contains("`unknown`"));
            assert!(!unknown.contains("secret-command"));
            assert!(!unknown.contains(secret));
        }

        #[test]
        fn protocol_errors_preserve_property_absence_and_bound_other_messages() {
            assert_eq!(
                mpv_protocol_error(
                    &[json!("get_property"), json!("missing")],
                    "property unavailable",
                ),
                "property unavailable"
            );

            let long_error = format!(
                "error running command https://example.invalid/private {}",
                "x".repeat(MAX_DIAGNOSTIC_CHARS * 2)
            );
            let error = mpv_protocol_error(&[json!("seek"), json!(30)], &long_error);
            assert!(error.starts_with("mpv IPC command `seek` failed:"));
            assert!(error.contains("<redacted-url>"));
            assert!(error.chars().count() <= MAX_DIAGNOSTIC_CHARS);
        }

        #[test]
        fn buffered_range_parser_preserves_discontinuous_ranges_in_time_order() {
            let ranges = parse_buffered_ranges(&json!({
                "seekable-ranges": [
                    {"start": 40.0, "end": 50.0},
                    {"start": 0.0, "end": 10.0},
                    {"start": 20.0, "end": 30.0},
                ],
            }));

            assert_eq!(
                ranges,
                vec![
                    BufferedRange {
                        start: Duration::ZERO,
                        end: Duration::from_secs(10),
                    },
                    BufferedRange {
                        start: Duration::from_secs(20),
                        end: Duration::from_secs(30),
                    },
                    BufferedRange {
                        start: Duration::from_secs(40),
                        end: Duration::from_secs(50),
                    },
                ]
            );
        }

        #[test]
        fn buffered_range_parser_merges_unordered_overlaps_and_touching_ranges() {
            let ranges = parse_buffered_ranges(&json!({
                "seekable-ranges": [
                    {"start": 12.0, "end": 18.0},
                    {"start": 0.0, "end": 5.0},
                    {"start": 18.0, "end": 25.0},
                    {"start": 4.0, "end": 15.0},
                    {"start": 30.0, "end": 31.0},
                ],
            }));

            assert_eq!(
                ranges,
                vec![
                    BufferedRange {
                        start: Duration::ZERO,
                        end: Duration::from_secs(25),
                    },
                    BufferedRange {
                        start: Duration::from_secs(30),
                        end: Duration::from_secs(31),
                    },
                ]
            );
        }

        #[test]
        fn buffered_range_parser_clamps_negative_codec_preroll_to_the_timeline_origin() {
            let ranges = parse_buffered_ranges(&json!({
                "seekable-ranges": [
                    {"start": -0.0065, "end": 634.5745},
                ],
            }));

            assert_eq!(
                ranges,
                vec![BufferedRange {
                    start: Duration::ZERO,
                    end: Duration::from_secs_f64(634.5745),
                }]
            );
        }

        #[test]
        fn buffered_range_parser_ignores_malformed_and_invalid_entries() {
            let ranges = parse_buffered_ranges(&json!({
                "seekable-ranges": [
                    {"start": -2.0, "end": -1.0},
                    {"start": -120.0, "end": 2.0},
                    {"start": 3.0, "end": 3.0},
                    {"start": 5.0, "end": 4.0},
                    {"start": "7", "end": 8.0},
                    {"start": 9.0},
                    null,
                    {"start": 1e300, "end": 1e301},
                    {"start": 10.5, "end": 11.25},
                ],
            }));

            assert_eq!(
                ranges,
                vec![BufferedRange {
                    start: Duration::from_secs_f64(10.5),
                    end: Duration::from_secs_f64(11.25),
                }]
            );
            assert!(parse_buffered_ranges(&json!({})).is_empty());
            assert!(parse_buffered_ranges(&json!({"seekable-ranges": "invalid"})).is_empty());
            for (start, end) in [
                (f64::NAN, 1.0),
                (0.0, f64::INFINITY),
                (f64::NEG_INFINITY, 1.0),
            ] {
                assert!(
                    normalized_buffered_range(start, end).is_none(),
                    "non-finite range must be discarded"
                );
            }
        }

        #[test]
        fn status_reads_and_normalizes_mpv_demuxer_cache_ranges() {
            let (mut backend, server_thread) = backend_with_properties([(
                "demuxer-cache-state",
                json!({
                    "seekable-ranges": [
                        {"start": 20.0, "end": 25.0},
                        {"start": 0.0, "end": 10.0},
                        {"start": 8.0, "end": 15.0},
                        {"start": null, "end": 90.0},
                    ],
                }),
            )]);
            backend.ipc.stream_title = Some("Artist — Work".to_owned());

            let status = backend.status().expect("mock mpv status");

            assert_eq!(
                status.buffered_ranges,
                vec![
                    BufferedRange {
                        start: Duration::ZERO,
                        end: Duration::from_secs(15),
                    },
                    BufferedRange {
                        start: Duration::from_secs(20),
                        end: Duration::from_secs(25),
                    },
                ]
            );
            assert_eq!(status.stream_title.as_deref(), Some("Artist — Work"));
            backend.shutdown().expect("shut down mock backend");
            server_thread.join().expect("mock status server");
        }

        #[test]
        fn icy_property_changes_are_normalized_bounded_and_clearable() {
            let (client, _server) = UnixStream::pair().expect("mock IPC pair");
            let mut ipc = MpvIpc::new(IpcLink::over(client));
            ipc.handle_event(&json!({
                "event": "property-change",
                "id": ICY_TITLE_OBSERVER_ID,
                "name": ICY_TITLE_PROPERTY,
                "data": "  Artist\u{0}\n  Work  ",
            }));
            assert_eq!(ipc.stream_title.as_deref(), Some("Artist Work"));

            let oversized = "é".repeat(MAX_STREAM_TITLE_BYTES);
            ipc.handle_event(&json!({
                "event": "property-change",
                "id": ICY_TITLE_OBSERVER_ID,
                "name": ICY_TITLE_PROPERTY,
                "data": oversized,
            }));
            let bounded = ipc
                .stream_title
                .as_deref()
                .expect("oversized title remains available in bounded form");
            assert!(bounded.len() <= MAX_STREAM_TITLE_BYTES);
            assert!(bounded.ends_with('…'));

            ipc.handle_event(&json!({
                "event": "property-change",
                "id": ICY_TITLE_OBSERVER_ID,
                "name": ICY_TITLE_PROPERTY,
                "data": null,
            }));
            assert_eq!(ipc.stream_title, None);

            ipc.handle_event(&json!({
                "event": "property-change",
                "id": ICY_TITLE_OBSERVER_ID,
                "name": ICY_TITLE_PROPERTY,
                "data": "(error)",
            }));
            assert_eq!(
                ipc.stream_title, None,
                "mpv's missing-property marker is not station metadata"
            );
        }

        #[test]
        fn unrelated_property_events_cannot_replace_the_icy_title() {
            let (client, _server) = UnixStream::pair().expect("mock IPC pair");
            let mut ipc = MpvIpc::new(IpcLink::over(client));
            ipc.stream_title = Some("retained".to_owned());

            for message in [
                json!({
                    "event": "property-change",
                    "id": ICY_TITLE_OBSERVER_ID + 1,
                    "name": ICY_TITLE_PROPERTY,
                    "data": "wrong observer",
                }),
                json!({
                    "event": "property-change",
                    "id": ICY_TITLE_OBSERVER_ID,
                    "name": "media-title",
                    "data": "wrong property",
                }),
            ] {
                ipc.handle_event(&message);
            }

            assert_eq!(ipc.stream_title.as_deref(), Some("retained"));
            ipc.handle_event(&json!({"event": "start-file"}));
            assert_eq!(ipc.stream_title, None);
        }

        #[test]
        fn command_is_headless_and_leaves_audio_output_automatic() {
            let config = process_config(PlaybackProfile::Balanced, AudioOutputDriver::Auto);
            let command =
                mpv_command(&config, Path::new("/tmp/youta-test.sock")).expect("mpv command");
            let arguments = command_arguments(&command);

            assert_eq!(command.get_program(), OsStr::new("mock-mpv"));
            assert!(arguments.contains(&"--no-video".to_owned()));
            assert!(arguments.contains(&"--terminal=no".to_owned()));
            assert!(arguments.contains(&"--input-terminal=no".to_owned()));
            assert!(arguments.contains(&"--audio-display=no".to_owned()));
            assert!(
                !arguments
                    .iter()
                    .any(|argument| argument.starts_with("--ao="))
            );
        }

        #[cfg(feature = "yt-dlp")]
        #[test]
        fn command_passes_compiled_ytdlp_helper_to_headless_mpv() {
            let config = process_config(PlaybackProfile::Balanced, AudioOutputDriver::Auto);
            let arguments = command_arguments(
                &mpv_command(&config, Path::new("/tmp/youta-test.sock")).expect("mpv command"),
            );

            assert!(
                arguments.contains(&"--script-opts=ytdl_hook-ytdl_path=mock-yt-dlp".to_owned())
            );
            assert!(
                arguments.contains(&"--ytdl-format=bestaudio[acodec^=opus]/bestaudio".to_owned())
            );
            assert!(arguments.contains(&"--ytdl-raw-options=js-runtimes=quickjs".to_owned()));
        }

        #[cfg(not(feature = "yt-dlp"))]
        #[test]
        fn command_omits_ytdlp_arguments_when_feature_is_not_compiled() {
            let config = process_config(PlaybackProfile::Balanced, AudioOutputDriver::Auto);
            let arguments = command_arguments(
                &mpv_command(&config, Path::new("/tmp/youta-test.sock")).expect("mpv command"),
            );

            assert!(!arguments.iter().any(|argument| argument.contains("ytdl")));
        }

        #[test]
        fn configured_audio_outputs_map_to_mpv_driver_names() {
            for (output, expected) in [
                (AudioOutputDriver::Null, "--ao=null"),
                (AudioOutputDriver::Alsa, "--ao=alsa"),
                (AudioOutputDriver::Jack, "--ao=jack"),
                (AudioOutputDriver::PulseAudio, "--ao=pulse"),
                (AudioOutputDriver::PipeWire, "--ao=pipewire"),
            ] {
                let config = process_config(PlaybackProfile::Balanced, output);
                let arguments = command_arguments(
                    &mpv_command(&config, Path::new("/tmp/youta-test.sock")).expect("mpv command"),
                );
                assert!(arguments.contains(&expected.to_owned()));
            }
        }

        #[test]
        fn direct_profile_honors_audiophile_options_without_forcing_alsa() {
            let mut config = process_config(PlaybackProfile::Direct, AudioOutputDriver::PipeWire);
            config.audio_device = Some("pipewire/speakers".to_owned());
            config.audiophile = AudiophilePlaybackOptions {
                exclusive_device: true,
                avoid_resampling: true,
                output_sample_rate_hz: None,
            };

            let arguments = command_arguments(
                &mpv_command(&config, Path::new("/tmp/youta-test.sock")).expect("mpv command"),
            );
            assert!(arguments.contains(&"--ao=pipewire".to_owned()));
            assert!(arguments.contains(&"--audio-exclusive=yes".to_owned()));
            assert!(arguments.contains(&"--audio-samplerate=0".to_owned()));
            assert!(arguments.contains(&"--audio-device=pipewire/speakers".to_owned()));
            assert!(!arguments.contains(&"--ao=alsa".to_owned()));
            assert!(!arguments.contains(&"--alsa-resample=no".to_owned()));
        }

        #[test]
        fn alsa_direct_profile_disables_alsa_resampling() {
            let mut config = process_config(PlaybackProfile::Direct, AudioOutputDriver::Alsa);
            config.audiophile.avoid_resampling = true;

            let arguments = command_arguments(
                &mpv_command(&config, Path::new("/tmp/youta-test.sock")).expect("mpv command"),
            );
            assert!(arguments.contains(&"--alsa-resample=no".to_owned()));
        }

        #[test]
        fn fixed_sample_rate_takes_precedence_over_automatic_rate() {
            let mut config = process_config(PlaybackProfile::Direct, AudioOutputDriver::Alsa);
            config.audiophile.avoid_resampling = true;
            config.audiophile.output_sample_rate_hz = Some(192_000);

            let arguments = command_arguments(
                &mpv_command(&config, Path::new("/tmp/youta-test.sock")).expect("mpv command"),
            );
            assert!(arguments.contains(&"--audio-samplerate=192000".to_owned()));
            assert!(!arguments.contains(&"--audio-samplerate=0".to_owned()));
        }

        #[test]
        fn invalid_fixed_sample_rate_is_rejected_before_spawning() {
            let mut config = process_config(PlaybackProfile::Direct, AudioOutputDriver::Auto);
            config.audiophile.output_sample_rate_hz = Some(MAX_MPV_SAMPLE_RATE_HZ + 1);

            let error = mpv_command(&config, Path::new("/tmp/youta-test.sock"))
                .expect_err("sample rate must be rejected");
            assert!(error.to_string().contains("output sample rate"));
        }

        #[test]
        fn private_runtime_directory_has_restricted_permissions() {
            let temporary = tempfile::tempdir().expect("temporary directory");
            let runtime = temporary.path().join("runtime");
            ensure_private_directory(&runtime).expect("private directory");

            let mode = fs::metadata(runtime)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }

        #[test]
        fn stale_cleanup_refuses_regular_files() {
            let temporary = tempfile::tempdir().expect("temporary directory");
            let path = temporary.path().join("not-a-socket");
            fs::write(&path, b"user data").expect("fixture");

            let error = remove_stale_socket(&path).expect_err("must refuse");
            assert!(error.to_string().contains("refusing to replace"));
            assert_eq!(fs::read(path).expect("preserved file"), b"user data");
        }

        #[test]
        fn asynchronous_load_error_keeps_bounded_redacted_diagnostics() {
            let secret = "secret-token-value";
            let mut ipc = ipc_after_script(vec![
                json!({
                    "event": "log-message",
                    "level": "error",
                    "prefix": "ytdl_hook",
                    "text": format!(
                        "request failed for https://example.test/watch?token={secret} \
                         api_key={secret}"
                    ),
                }),
                json!({
                    "event": "end-file",
                    "reason": "error",
                    "error": "loading failed",
                    "file_error": "HTTP 403 from https://example.test/private",
                }),
            ]);

            let event = ipc.events.pop_front().expect("end-file event");
            let PlaybackEvent::Ended(ended) = event else {
                panic!("expected an ended event");
            };
            assert_eq!(ended.reason, PlaybackEndReason::Error);
            assert_eq!(ended.error.as_deref(), Some("loading failed"));
            assert_eq!(
                ended.file_error.as_deref(),
                Some("HTTP 403 from <redacted-url>")
            );
            let diagnostic = ended.diagnostic.expect("warning diagnostic");
            assert!(diagnostic.contains("<redacted-url>"));
            assert!(diagnostic.contains("<redacted-secret>"));
            assert!(!diagnostic.contains(secret));
            assert!(diagnostic.chars().count() <= MAX_DIAGNOSTIC_CHARS);
        }

        #[test]
        fn command_acknowledgement_does_not_report_successful_playback() {
            let ipc = ipc_after_script(Vec::new());

            assert!(
                ipc.events.is_empty(),
                "an IPC acknowledgement is not a media lifecycle event"
            );
        }

        #[test]
        fn loaded_started_and_eof_events_preserve_protocol_order() {
            let mut ipc = ipc_after_script(vec![
                json!({"event": "file-loaded"}),
                json!({"event": "playback-restart"}),
                json!({"event": "end-file", "reason": "eof"}),
            ]);

            assert_eq!(ipc.events.pop_front(), Some(PlaybackEvent::MediaLoaded));
            assert_eq!(ipc.events.pop_front(), Some(PlaybackEvent::PlaybackStarted));
            assert_eq!(
                ipc.events.pop_front(),
                Some(PlaybackEvent::Ended(PlaybackEnd {
                    reason: PlaybackEndReason::Eof,
                    error: None,
                    file_error: None,
                    diagnostic: None,
                }))
            );
            assert!(ipc.events.is_empty());
        }

        #[test]
        fn playlist_redirect_is_not_reported_as_a_terminal_playback_event() {
            let mut ipc = ipc_after_script(vec![
                json!({"event": "start-file"}),
                json!({
                    "event": "log-message",
                    "level": "warn",
                    "prefix": "demux",
                    "text": "intermediate playlist warning"
                }),
                json!({
                    "event": "property-change",
                    "id": ICY_TITLE_OBSERVER_ID,
                    "name": ICY_TITLE_PROPERTY,
                    "data": "Playlist placeholder"
                }),
                json!({
                    "event": "end-file",
                    "reason": "redirect",
                    "playlist_insert_id": 2,
                    "playlist_insert_num_entries": 3
                }),
                json!({"event": "start-file"}),
                json!({"event": "file-loaded"}),
                json!({"event": "playback-restart"}),
            ]);

            assert_eq!(ipc.events.pop_front(), Some(PlaybackEvent::MediaLoaded));
            assert_eq!(ipc.events.pop_front(), Some(PlaybackEvent::PlaybackStarted));
            assert!(
                ipc.events.is_empty(),
                "mpv follows the inserted playlist stream without ending the queue item"
            );
            assert!(ipc.warnings.is_empty());
            assert_eq!(ipc.stream_title, None);
        }

        #[test]
        fn stop_is_distinct_from_natural_end_and_failure() {
            let mut ipc = ipc_after_script(vec![json!({"event": "end-file", "reason": "stop"})]);

            let PlaybackEvent::Ended(ended) = ipc.events.pop_front().expect("end-file event")
            else {
                panic!("expected an ended event");
            };
            assert_eq!(ended.reason, PlaybackEndReason::Stop);
            assert!(ended.error.is_none());
        }

        #[test]
        fn process_exit_is_reported_as_a_bounded_event() {
            let temporary = tempfile::tempdir().expect("temporary directory");
            let (client, server) = UnixStream::pair().expect("mock IPC pair");
            let (ready_sender, ready_receiver) = mpsc::channel();
            let server_thread = thread::spawn(move || {
                ready_sender.send(()).expect("signal mock server");
                let mut reader = BufReader::new(server);
                loop {
                    let mut request_line = String::new();
                    if reader
                        .read_line(&mut request_line)
                        .expect("read mock backend request")
                        == 0
                    {
                        break;
                    }
                    let request: Value =
                        serde_json::from_str(&request_line).expect("parse mock backend request");
                    let request_id = request
                        .get("request_id")
                        .and_then(Value::as_u64)
                        .expect("request ID");
                    let quit = request
                        .get("command")
                        .and_then(Value::as_array)
                        .and_then(|command| command.first())
                        .and_then(Value::as_str)
                        == Some("quit");
                    serde_json::to_writer(
                        reader.get_mut(),
                        &json!({"request_id": request_id, "error": "success"}),
                    )
                    .expect("write mock backend response");
                    reader.get_mut().write_all(b"\n").expect("response newline");
                    reader.get_mut().flush().expect("flush response");
                    if quit {
                        break;
                    }
                }
            });
            ready_receiver.recv().expect("mock server ready");

            let child = Command::new("sh")
                .args(["-c", "exit 23"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("short-lived mock mpv");
            let mut backend = MpvBackend {
                child,
                ipc: MpvIpc::new(IpcLink::over(client)),
                socket_path: temporary.path().join("unused.sock"),
                profile: PlaybackProfile::Balanced,
                process_exit_reported: false,
            };

            let deadline = Instant::now() + Duration::from_secs(2);
            let event = loop {
                if let Some(event) = backend.poll_event().expect("poll backend event") {
                    break event;
                }
                assert!(
                    Instant::now() < deadline,
                    "mock process did not exit before deadline"
                );
                thread::sleep(Duration::from_millis(10));
            };
            let PlaybackEvent::ProcessExited { diagnostic } = event else {
                panic!("expected process-exited event");
            };
            let diagnostic = diagnostic.expect("exit status diagnostic");
            assert!(diagnostic.contains("23"));
            assert!(diagnostic.chars().count() <= MAX_DIAGNOSTIC_CHARS);
            assert_eq!(
                backend.poll_event().expect("second event poll"),
                None,
                "process exit must be reported once"
            );

            backend.shutdown().expect("clean up mock backend");
            server_thread.join().expect("mock backend server");
        }
    }
}

pub use backend::MpvBackend;
