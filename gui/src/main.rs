//! Youta's desktop window.
//!
//! The window is a second front-end to the same reducer the terminal UI drives.
//! It renders [`ViewModel`] and emits [`UiAction`]; no provider, persistence, or
//! playback logic lives here.
//!
//! Untrusted provider text — video descriptions, comments, Wikidata values —
//! reaches a web view here rather than a terminal cell buffer, so the window
//! ships a restrictive CSP, an empty capability set, and never receives the
//! credential-bearing editors. See the serialization notes in `src/view.rs` of
//! the shared crate.

// A GUI binary must not open a console window on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod reducer;

use tauri::Manager;

use youta::config::{Config, PlaybackBackend as ConfiguredBackend};
use youta::playback::{AudioOutputDriver, ProcessPlaybackConfig};
use youta::view::{Screen, UiAction, ViewModel};

use reducer::ReducerHandle;

/// One selectable source, as the window should label it.
#[derive(serde::Serialize)]
struct ScreenEntry {
    /// Identifier matching `ViewModel::screen`.
    id: Screen,
    /// Human-readable tab label.
    label: &'static str,
}

/// Lists the sources compiled into this build.
///
/// The catalogue stays in Rust so a feature-trimmed build never offers a tab it
/// cannot serve, and so the window does not restate the source list.
#[tauri::command]
fn screens() -> Vec<ScreenEntry> {
    Screen::ALL
        .into_iter()
        .filter(|screen| screen.enabled())
        .map(|screen| ScreenEntry {
            id: screen,
            label: screen.label(),
        })
        .collect()
}

/// Where this build will send audio, as the player bar should label it.
///
/// Output selection is configuration rather than interactive state, so it is
/// absent from [`ViewModel`] and read once instead of published every frame.
/// Changing it belongs to Preferences, which the reducer already owns.
#[derive(Clone, serde::Serialize)]
struct AudioOutputView {
    /// Playback engine the configuration selects.
    engine: &'static str,
    /// Audio driver requested from that engine.
    driver: &'static str,
    /// Explicit device, when the user pinned one.
    device: Option<String>,
}

impl AudioOutputView {
    /// Reads the configured output without starting an engine.
    fn from_config(config: &Config) -> Self {
        let process = ProcessPlaybackConfig::from_config(config);
        Self {
            engine: match config.playback.backend {
                ConfiguredBackend::Mpv => "mpv",
                ConfiguredBackend::Native => "no engine",
            },
            driver: match process.audio_output {
                AudioOutputDriver::Auto => "system default",
                AudioOutputDriver::Null => "no output",
                AudioOutputDriver::Alsa => "ALSA",
                AudioOutputDriver::Jack => "JACK",
                AudioOutputDriver::PulseAudio => "PulseAudio",
                AudioOutputDriver::PipeWire => "PipeWire",
            },
            device: process.audio_device,
        }
    }
}

/// Returns the configured audio engine, driver, and device.
#[tauri::command]
fn audio_output(output: tauri::State<'_, AudioOutputView>) -> AudioOutputView {
    output.inner().clone()
}

/// Longest window-side failure text copied into the process log.
///
/// The message may quote provider text, so it is bounded like every other
/// external string Youta reports.
const MAX_STARTUP_FAILURE_CHARS: usize = 512;

/// Copies a window-side startup failure into the process log.
///
/// A web view swallows its own errors: without this, a window that failed to
/// wire itself up is indistinguishable from one that started cleanly, because
/// the process keeps running with empty stderr either way.
#[tauri::command]
fn report_startup_failure(message: String) {
    let bounded: String = message.chars().take(MAX_STARTUP_FAILURE_CHARS).collect();
    eprintln!("the Youta window failed to start: {bounded}");
}

/// Returns the snapshot the window should render right now.
#[tauri::command]
fn snapshot(reducer: tauri::State<'_, ReducerHandle>) -> ViewModel {
    reducer.snapshot()
}

/// Applies one semantic action to the shared reducer.
#[tauri::command]
fn dispatch(action: UiAction, reducer: tauri::State<'_, ReducerHandle>) -> Result<(), String> {
    reducer.dispatch(action)
}

fn main() {
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("cannot load Youta configuration: {error}");
            std::process::exit(1);
        }
    };

    tauri::Builder::default()
        .setup(move |app| {
            app.manage(AudioOutputView::from_config(&config));
            match reducer::start(app.handle().clone(), config.clone()) {
                Ok(handle) => {
                    app.manage(handle);
                    Ok(())
                }
                // Tauri turns a setup error into a panic. A second Youta, or a
                // state directory Youta may not touch, is a condition to report
                // rather than a crash to display.
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            screens,
            snapshot,
            dispatch,
            audio_output,
            report_startup_failure
        ])
        .build(tauri::generate_context!())
        .expect("the Youta window must start")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                // Closing the window is not a quit action inside the reducer, so
                // without this the player process would outlive the window and
                // the durable state lock would be released only by process death.
                app.state::<ReducerHandle>().shutdown();
            }
        });
}
