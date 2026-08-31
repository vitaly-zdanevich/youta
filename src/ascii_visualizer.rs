//! CAVA-backed frequency-spectrum capture and fullscreen ASCII visualization.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use serde::Deserialize;
use thiserror::Error;

/// Number of logarithmically distributed FFT bands requested from CAVA.
pub const CAVA_SPECTRUM_BANDS: usize = 64;

/// Package-manager examples shown when the CAVA helper cannot be started.
pub const CAVA_INSTALL_GUIDANCE: &str = "Install CAVA: `emerge media-sound/cava` (Gentoo), `apt install cava` (Debian/Ubuntu), `dnf install cava` (Fedora), or `brew install cava` (macOS).";

/// Maximum number of columns rendered for any frontend request.
pub const MAX_RENDER_WIDTH: usize = 320;

/// Maximum number of rows rendered for any frontend request.
pub const MAX_RENDER_HEIGHT: usize = 120;

const CAVA_FRAME_QUEUE_CAPACITY: usize = 1;
const CAVA_CONFIG_ATTEMPTS: u64 = 16;
const MAX_PACTL_JSON_BYTES: usize = 1024 * 1024;
static CAVA_CONFIG_NONCE: AtomicU64 = AtomicU64::new(0);

/// One finite, normalized frame of logarithmically distributed frequency bands.
#[derive(Clone, Debug, PartialEq)]
pub struct AudioSpectrum {
    bands: Vec<f32>,
}

impl AudioSpectrum {
    /// Normalizes one complete unsigned 8-bit CAVA raw-output frame.
    #[must_use]
    pub fn from_cava_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != CAVA_SPECTRUM_BANDS {
            return None;
        }
        Self::from_normalized_bands(
            bytes
                .iter()
                .map(|value| f32::from(*value) / f32::from(u8::MAX))
                .collect(),
        )
    }

    /// Accepts exactly [`CAVA_SPECTRUM_BANDS`] finite values in `0.0..=1.0`.
    #[must_use]
    pub fn from_normalized_bands(bands: Vec<f32>) -> Option<Self> {
        (bands.len() == CAVA_SPECTRUM_BANDS
            && bands
                .iter()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value)))
        .then_some(Self { bands })
    }

    /// Returns the stable low-to-high-frequency band order supplied by CAVA.
    #[must_use]
    pub fn bands(&self) -> &[f32] {
        &self.bands
    }

    fn silence() -> Self {
        Self {
            bands: vec![0.0; CAVA_SPECTRUM_BANDS],
        }
    }
}

/// Stable name shown above the sole CAVA-backed spectrum visualization.
pub const ASCII_VISUALIZATION_LABEL: &str = "Spectrum";

/// Persistent spectrum and falling-peak state shared by responsive renders.
///
/// CAVA already performs logarithmic grouping, sensitivity correction, and
/// temporal smoothing. Youta retains only the state needed by peak caps.
#[derive(Clone, Debug, PartialEq)]
pub struct AsciiVisualizerRenderer {
    current: AudioSpectrum,
    peaks: Vec<f32>,
}

impl Default for AsciiVisualizerRenderer {
    fn default() -> Self {
        Self {
            current: AudioSpectrum::silence(),
            peaks: vec![0.0; CAVA_SPECTRUM_BANDS],
        }
    }
}

impl AsciiVisualizerRenderer {
    /// Accepts the newest CAVA frame and updates bounded falling-peak history.
    pub fn push_spectrum(&mut self, spectrum: AudioSpectrum) {
        for (peak, value) in self.peaks.iter_mut().zip(spectrum.bands()) {
            *peak = (*peak * 0.94).max(*value);
        }
        self.current = spectrum;
    }

    /// Produces a bounded ASCII frame for a frontend viewport.
    #[must_use]
    pub fn render(&self, width: u16, height: u16) -> Vec<String> {
        let width = usize::from(width).min(MAX_RENDER_WIDTH);
        let height = usize::from(height).min(MAX_RENDER_HEIGHT);
        if width == 0 || height == 0 {
            return Vec::new();
        }
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| self.spectrum_cell(x, y, width, height))
                    .collect()
            })
            .collect()
    }

    fn spectrum_cell(&self, x: usize, y: usize, width: usize, height: usize) -> char {
        let position = ratio(x, width);
        let value = interpolated_band(self.current.bands(), position);
        let peak = interpolated_band(&self.peaks, position);
        let from_bottom = 1.0 - ratio(y, height);
        let cell_height = 1.0 / bounded_coordinate(height).max(1.0);
        if value <= f32::EPSILON && peak <= f32::EPSILON {
            return ' ';
        }
        if (from_bottom - peak).abs() <= cell_height * 0.55 && peak > 0.04 {
            return '-';
        }
        if from_bottom > value {
            return ' ';
        }
        vertical_glyph(from_bottom)
    }
}

/// Errors emitted while starting or consuming the bounded CAVA raw stream.
#[derive(Debug, Error)]
pub enum CavaSpectrumError {
    /// Youta could not create its short-lived CAVA configuration file.
    #[error("could not create a temporary CAVA configuration: {0}")]
    CreateConfig(#[source] io::Error),
    /// Youta could not finish its short-lived CAVA configuration file.
    #[error("could not write the temporary CAVA configuration: {0}")]
    WriteConfig(#[source] io::Error),
    /// The configured executable could not be started.
    #[error("could not start CAVA; install it or set providers.cava_executable: {0}")]
    Spawn(#[source] io::Error),
    /// CAVA exited or stopped producing complete raw FFT frames.
    #[error("CAVA audio capture stopped; check its PipeWire, PulseAudio, or ALSA input")]
    StreamStopped,
}

/// Runtime boundary consumed by the application controller.
pub trait AudioSpectrumStream: Send {
    /// Drains obsolete frames and returns only the newest complete spectrum.
    ///
    /// # Errors
    ///
    /// Returns [`CavaSpectrumError::StreamStopped`] if CAVA exits while the
    /// visualization remains open.
    fn drain_latest(&mut self) -> Result<Option<AudioSpectrum>, CavaSpectrumError>;

    /// Stops and reaps the owned helper. Repeated calls are harmless.
    fn shutdown(&mut self);
}

/// Injectable constructor for system and deterministic test spectrum streams.
pub trait AudioSpectrumStreamFactory: Send {
    /// Starts one stream using the configured CAVA executable.
    ///
    /// # Errors
    ///
    /// Returns a bounded startup error when the configuration or helper cannot
    /// be created.
    fn start(
        &mut self,
        executable: &Path,
        playback_process_id: Option<u32>,
    ) -> Result<Box<dyn AudioSpectrumStream>, CavaSpectrumError>;
}

/// Production CAVA raw-output stream factory.
#[derive(Default)]
pub struct SystemCavaSpectrumStreamFactory;

impl AudioSpectrumStreamFactory for SystemCavaSpectrumStreamFactory {
    fn start(
        &mut self,
        executable: &Path,
        playback_process_id: Option<u32>,
    ) -> Result<Box<dyn AudioSpectrumStream>, CavaSpectrumError> {
        Ok(Box::new(CavaSpectrumStream::start(
            executable,
            playback_process_id,
        )?))
    }
}

struct CavaSpectrumStream {
    receiver: Receiver<CavaSpectrumEvent>,
    child: Arc<Mutex<Child>>,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    config_path: Option<PathBuf>,
    disconnected: bool,
}

enum CavaSpectrumEvent {
    Frame(AudioSpectrum),
    Stopped,
}

impl CavaSpectrumStream {
    fn start(
        executable: &Path,
        playback_process_id: Option<u32>,
    ) -> Result<Self, CavaSpectrumError> {
        let pulse_monitor = discover_pulse_monitor(playback_process_id);
        let config_path = write_cava_config(pulse_monitor.as_deref())?;
        let mut command = Command::new(executable);
        command
            .arg("-p")
            .arg(&config_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        crate::child_process::supervised(&mut command);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = fs::remove_file(&config_path);
                return Err(CavaSpectrumError::Spawn(error));
            }
        };
        let Some(mut stdout) = child.stdout.take() else {
            crate::child_process::terminate_tree(&mut child);
            let _ = fs::remove_file(&config_path);
            return Err(CavaSpectrumError::StreamStopped);
        };
        let child = Arc::new(Mutex::new(child));
        let stopping = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = bounded(CAVA_FRAME_QUEUE_CAPACITY);
        let stale_receiver = receiver.clone();
        let worker_stopping = Arc::clone(&stopping);
        let worker = thread::Builder::new()
            .name("youta-cava-spectrum".to_owned())
            .spawn(move || {
                let mut raw = [0_u8; CAVA_SPECTRUM_BANDS];
                while !worker_stopping.load(Ordering::Acquire) {
                    if stdout.read_exact(&mut raw).is_err() {
                        if !worker_stopping.load(Ordering::Acquire) {
                            send_latest(&sender, &stale_receiver, CavaSpectrumEvent::Stopped);
                        }
                        break;
                    }
                    if let Some(spectrum) = AudioSpectrum::from_cava_bytes(&raw) {
                        send_latest(&sender, &stale_receiver, CavaSpectrumEvent::Frame(spectrum));
                    }
                }
            })
            .map_err(|error| {
                if let Ok(mut child) = child.lock() {
                    crate::child_process::terminate_tree(&mut child);
                }
                let _ = fs::remove_file(&config_path);
                CavaSpectrumError::Spawn(error)
            })?;
        Ok(Self {
            receiver,
            child,
            stopping,
            worker: Some(worker),
            config_path: Some(config_path),
            disconnected: false,
        })
    }
}

fn send_latest(
    sender: &Sender<CavaSpectrumEvent>,
    stale_receiver: &Receiver<CavaSpectrumEvent>,
    event: CavaSpectrumEvent,
) {
    match sender.try_send(event) {
        Ok(()) | Err(TrySendError::Disconnected(_)) => {}
        Err(TrySendError::Full(event)) => {
            let _ = stale_receiver.try_recv();
            let _ = sender.try_send(event);
        }
    }
}

impl AudioSpectrumStream for CavaSpectrumStream {
    fn drain_latest(&mut self) -> Result<Option<AudioSpectrum>, CavaSpectrumError> {
        if self.disconnected {
            return Err(CavaSpectrumError::StreamStopped);
        }
        let mut latest = None;
        loop {
            match self.receiver.try_recv() {
                Ok(CavaSpectrumEvent::Frame(spectrum)) => latest = Some(spectrum),
                Ok(CavaSpectrumEvent::Stopped) => {
                    self.disconnected = true;
                    return Err(CavaSpectrumError::StreamStopped);
                }
                Err(TryRecvError::Empty) => return Ok(latest),
                Err(TryRecvError::Disconnected) => {
                    self.disconnected = true;
                    return Err(CavaSpectrumError::StreamStopped);
                }
            }
        }
    }

    fn shutdown(&mut self) {
        if self.stopping.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut child) = self.child.lock() {
            crate::child_process::terminate_tree(&mut child);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(path) = self.config_path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

impl Drop for CavaSpectrumStream {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn write_cava_config(pulse_monitor: Option<&str>) -> Result<PathBuf, CavaSpectrumError> {
    let directory = std::env::temp_dir();
    for _ in 0..CAVA_CONFIG_ATTEMPTS {
        let nonce = CAVA_CONFIG_NONCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!("youta-cava-{}-{nonce}.conf", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        match crate::private_files::open_privately(&mut options).open(&path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(cava_config(pulse_monitor).as_bytes()) {
                    let _ = fs::remove_file(&path);
                    return Err(CavaSpectrumError::WriteConfig(error));
                }
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(CavaSpectrumError::CreateConfig(error)),
        }
    }
    Err(CavaSpectrumError::CreateConfig(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "temporary CAVA configuration names are exhausted",
    )))
}

fn cava_config(pulse_monitor: Option<&str>) -> String {
    let input = pulse_monitor.map_or_else(String::new, |source| {
        format!("[input]\nmethod = pulse\nsource = {source}\n\n")
    });
    format!(
        "[general]\n\
         framerate = 30\n\
         bars = {CAVA_SPECTRUM_BANDS}\n\
         autosens = 1\n\
         sensitivity = 100\n\
         lower_cutoff_freq = 40\n\
         higher_cutoff_freq = 16000\n\
		 sleep_timer = 0\n\
		 \n\
		 {input}\
		 [output]\n\
         method = raw\n\
         raw_target = /dev/stdout\n\
         data_format = binary\n\
         bit_format = 8bit\n\
         channels = mono\n\
         mono_option = average\n\
         \n\
         [smoothing]\n\
         monstercat = 1\n\
         waves = 0\n\
         noise_reduction = 65\n"
    )
}

#[derive(Deserialize)]
struct PulseSinkInput {
    sink: u32,
    #[serde(default)]
    corked: bool,
    #[serde(default)]
    properties: HashMap<String, String>,
}

#[derive(Deserialize)]
struct PulseSink {
    index: u32,
    monitor_source: String,
}

/// Finds the monitor belonging to the sink that carries Youta's mpv process.
///
/// CAVA's PulseAudio `auto` source follows the server's default sink. Pulse
/// stream restore may route mpv to another sink, so the default monitor can be
/// silent while playback is audible. Discovery is optional: platforms without
/// `pactl`, non-Pulse outputs, and malformed local responses keep CAVA's own
/// backend selection.
fn discover_pulse_monitor(playback_process_id: Option<u32>) -> Option<String> {
    let process_id = playback_process_id?;
    let sink_inputs = pactl_json(&["--format=json", "list", "sink-inputs"])?;
    let sinks = pactl_json(&["--format=json", "list", "sinks"])?;
    pulse_monitor_for_process(&sink_inputs, &sinks, process_id)
}

fn pactl_json(arguments: &[&str]) -> Option<Vec<u8>> {
    let output = crate::child_process::quiet(&mut Command::new("pactl"))
        .args(arguments)
        .output()
        .ok()?;
    (output.status.success() && output.stdout.len() <= MAX_PACTL_JSON_BYTES)
        .then_some(output.stdout)
}

fn pulse_monitor_for_process(
    sink_inputs_json: &[u8],
    sinks_json: &[u8],
    process_id: u32,
) -> Option<String> {
    let sink_inputs: Vec<PulseSinkInput> = serde_json::from_slice(sink_inputs_json).ok()?;
    let sinks: Vec<PulseSink> = serde_json::from_slice(sinks_json).ok()?;
    let process_id = process_id.to_string();
    let matching = sink_inputs.iter().filter(|input| {
        input
            .properties
            .get("application.process.id")
            .is_some_and(|candidate| candidate == &process_id)
    });
    let sink = matching
        .clone()
        .find(|input| !input.corked)
        .or_else(|| matching.into_iter().next())?
        .sink;
    let monitor = sinks
        .into_iter()
        .find(|candidate| candidate.index == sink)?
        .monitor_source;
    is_safe_cava_source(&monitor).then_some(monitor)
}

fn is_safe_cava_source(source: &str) -> bool {
    !source.is_empty()
        && source.len() <= 512
        && source
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
}

fn interpolated_band(bands: &[f32], position: f32) -> f32 {
    if bands.is_empty() {
        return 0.0;
    }
    let last = bands.len().saturating_sub(1);
    let scaled = position.clamp(0.0, 1.0) * bounded_coordinate(last);
    let lower = scaled.floor();
    let lower_index = usize::from(u16::try_from(lower as u64).unwrap_or_default()).min(last);
    let upper_index = lower_index.saturating_add(1).min(last);
    let fraction = scaled - lower;
    bands[lower_index] * (1.0 - fraction) + bands[upper_index] * fraction
}

fn ratio(position: usize, extent: usize) -> f32 {
    if extent <= 1 {
        0.0
    } else {
        bounded_coordinate(position) / bounded_coordinate(extent - 1)
    }
}

fn bounded_coordinate(value: usize) -> f32 {
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}

fn vertical_glyph(position: f32) -> char {
    if position < 0.25 {
        '#'
    } else if position < 0.5 {
        '='
    } else if position < 0.75 {
        '*'
    } else {
        ':'
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    use super::*;

    fn fixture_spectrum() -> AudioSpectrum {
        AudioSpectrum::from_normalized_bands(
            (0..CAVA_SPECTRUM_BANDS)
                .map(|band| {
                    let position =
                        bounded_coordinate(band) / bounded_coordinate(CAVA_SPECTRUM_BANDS - 1);
                    ((position * std::f32::consts::TAU * 2.5).sin() * 0.35 + 0.52).clamp(0.0, 1.0)
                })
                .collect(),
        )
        .expect("finite fixture")
    }

    #[test]
    fn exposes_only_the_cava_spectrum_visualization() {
        assert_eq!(ASCII_VISUALIZATION_LABEL, "Spectrum");
    }

    #[test]
    fn spectrum_produces_one_bounded_ascii_cell_per_requested_position() {
        let mut renderer = AsciiVisualizerRenderer::default();
        renderer.push_spectrum(fixture_spectrum());
        let frame = renderer.render(47, 13);
        assert_eq!(frame.len(), 13);
        assert!(frame.iter().all(|line| line.chars().count() == 47));
        assert!(
            frame
                .iter()
                .flat_map(|line| line.chars())
                .all(|character| character.is_ascii())
        );
    }

    #[test]
    fn live_spectrum_changes_the_rendered_frame() {
        let quiet = AsciiVisualizerRenderer::default();
        let mut active = AsciiVisualizerRenderer::default();
        active.push_spectrum(fixture_spectrum());
        assert_ne!(quiet.render(40, 12), active.render(40, 12));
    }

    #[test]
    fn silence_does_not_create_decorative_motion() {
        let renderer = AsciiVisualizerRenderer::default();
        assert!(
            renderer
                .render(40, 12)
                .iter()
                .all(|line| line.trim().is_empty())
        );
    }

    #[test]
    fn renderer_caps_untrusted_frontend_dimensions() {
        let mut renderer = AsciiVisualizerRenderer::default();
        renderer.push_spectrum(fixture_spectrum());
        let frame = renderer.render(u16::MAX, u16::MAX);
        assert_eq!(frame.len(), MAX_RENDER_HEIGHT);
        assert!(frame.iter().all(|line| line.len() == MAX_RENDER_WIDTH));
    }

    #[test]
    fn spectrum_distinguishes_bass_from_treble_with_equal_overall_level() {
        let bass = AudioSpectrum::from_normalized_bands(
            (0..CAVA_SPECTRUM_BANDS)
                .map(|band| if band < 12 { 0.82 } else { 0.08 })
                .collect(),
        )
        .expect("bounded bass spectrum");
        let treble = AudioSpectrum::from_normalized_bands(
            (0..CAVA_SPECTRUM_BANDS)
                .map(|band| if band >= 52 { 0.82 } else { 0.08 })
                .collect(),
        )
        .expect("bounded treble spectrum");

        let mut bass_renderer = AsciiVisualizerRenderer::default();
        bass_renderer.push_spectrum(bass);
        let mut treble_renderer = AsciiVisualizerRenderer::default();
        treble_renderer.push_spectrum(treble);
        assert_ne!(
            bass_renderer.render(64, 20),
            treble_renderer.render(64, 20),
            "the spectrum must use frequency bands, not aggregate loudness",
        );
    }

    #[test]
    fn cava_frames_are_exactly_sized_and_normalized() {
        let mut raw = vec![0_u8; CAVA_SPECTRUM_BANDS];
        raw[1] = 128;
        raw[CAVA_SPECTRUM_BANDS - 1] = u8::MAX;
        let spectrum = AudioSpectrum::from_cava_bytes(&raw).expect("complete CAVA frame");

        assert_eq!(spectrum.bands().len(), CAVA_SPECTRUM_BANDS);
        assert_eq!(spectrum.bands()[0], 0.0);
        assert!((spectrum.bands()[1] - 128.0 / 255.0).abs() < f32::EPSILON);
        assert_eq!(spectrum.bands()[CAVA_SPECTRUM_BANDS - 1], 1.0);
        assert!(AudioSpectrum::from_cava_bytes(&raw[..raw.len() - 1]).is_none());
    }

    #[test]
    fn cava_config_requests_smoothed_mono_binary_fft_frames() {
        let config = cava_config(None);
        for expected in [
            "bars = 64",
            "method = raw",
            "raw_target = /dev/stdout",
            "data_format = binary",
            "bit_format = 8bit",
            "channels = mono",
            "monstercat = 1",
            "noise_reduction = 65",
        ] {
            assert!(config.contains(expected), "missing {expected:?}");
        }
        assert!(!config.contains("[input]"));
    }

    #[test]
    fn active_mpv_sink_monitor_overrides_a_different_default_sink() {
        let sink_inputs = br#"[
			{"sink":0,"corked":false,"properties":{"application.process.id":"7"}},
			{"sink":4,"corked":false,"properties":{"application.process.id":"42"}}
		]"#;
        let sinks = br#"[
			{"index":0,"monitor_source":"alsa_output.Focusrite.monitor"},
			{"index":4,"monitor_source":"alsa_output.Jabra.monitor"}
		]"#;

        let monitor =
            pulse_monitor_for_process(sink_inputs, sinks, 42).expect("active mpv monitor");
        assert_eq!(monitor, "alsa_output.Jabra.monitor");
        let config = cava_config(Some(&monitor));
        assert!(config.contains("[input]\nmethod = pulse\nsource = alsa_output.Jabra.monitor\n"));
    }

    #[test]
    fn pulse_monitor_rejects_an_ini_injection() {
        let sink_inputs =
            br#"[{"sink":4,"corked":false,"properties":{"application.process.id":"42"}}]"#;
        let sinks = br#"[{"index":4,"monitor_source":"safe.monitor\n[output]"}]"#;

        assert!(pulse_monitor_for_process(sink_inputs, sinks, 42).is_none());
    }

    #[test]
    fn bounded_cava_queue_replaces_stale_frames() {
        let (sender, receiver) = bounded(CAVA_FRAME_QUEUE_CAPACITY);
        let stale_receiver = receiver.clone();
        let quiet = AudioSpectrum::from_cava_bytes(&[0; CAVA_SPECTRUM_BANDS]).expect("quiet frame");
        let loud =
            AudioSpectrum::from_cava_bytes(&[u8::MAX; CAVA_SPECTRUM_BANDS]).expect("loud frame");

        send_latest(&sender, &stale_receiver, CavaSpectrumEvent::Frame(quiet));
        send_latest(
            &sender,
            &stale_receiver,
            CavaSpectrumEvent::Frame(loud.clone()),
        );

        let CavaSpectrumEvent::Frame(latest) = receiver.try_recv().expect("latest frame") else {
            panic!("expected a spectrum frame");
        };
        assert_eq!(latest, loud);
    }

    #[cfg(unix)]
    #[test]
    fn cava_process_streams_one_exact_mock_fft_frame_and_is_reaped() {
        let directory = tempfile::tempdir().expect("CAVA mock directory");
        let executable = directory.path().join("cava-mock");
        fs::write(
            &executable,
            b"#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 64 ]; do printf '\\377'; i=$((i + 1)); done\nsleep 60\n",
        )
        .expect("write CAVA mock");
        let mut permissions = fs::metadata(&executable)
            .expect("CAVA mock metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&executable, permissions).expect("make CAVA mock executable");

        let mut stream = CavaSpectrumStream::start(&executable, None).expect("start CAVA mock");
        let config_path = stream
            .config_path
            .clone()
            .expect("temporary CAVA configuration");
        let deadline = Instant::now() + Duration::from_secs(2);
        let spectrum = loop {
            if let Some(spectrum) = stream.drain_latest().expect("read CAVA mock") {
                break spectrum;
            }
            assert!(Instant::now() < deadline, "CAVA mock produced no frame");
            thread::sleep(Duration::from_millis(10));
        };
        assert!(spectrum.bands().iter().all(|value| *value == 1.0));

        stream.shutdown();
        assert!(!config_path.exists());
        assert!(stream.worker.is_none());
    }
}
