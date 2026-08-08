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

#[cfg(test)]
mod contract_tests;
mod reducer;

use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;
use tauri::Manager;
use tauri::http::{Request, Response};
use url::Url;

use youta::artwork::remote_artwork;
use youta::config::{Config, PlaybackBackend as ConfiguredBackend};
use youta::keymap::{KeyPress, PopupGeometry};
use youta::playback::{AudioOutputDriver, ProcessPlaybackConfig};
use youta::view::{InformationPanelKind, Screen, UiAction, ViewModel};

use reducer::ReducerHandle;

/// One selectable source, as the window should label it.
#[derive(serde::Serialize)]
struct ScreenEntry {
    /// Identifier matching `ViewModel::screen`.
    id: Screen,
    /// Human-readable tab label.
    label: &'static str,
    /// Which set of facts this source's Details panel presents.
    details_kind: InformationPanelKind,
    /// Verb the search field is labelled with, or `None` where the screen
    /// collects no query and the window must not offer one.
    search_verb: Option<&'static str>,
}

/// Lists the sources compiled into this build.
///
/// The catalogue stays in Rust so a feature-trimmed build never offers a tab it
/// cannot serve, and so the window does not restate the source list. The Details
/// layout and the search verb travel with it for the same reason: a copy of
/// either mapping in TypeScript is a copy that can drift.
#[tauri::command]
fn screens() -> Vec<ScreenEntry> {
    Screen::ALL
        .into_iter()
        .filter(|screen| screen.enabled())
        .map(|screen| ScreenEntry {
            id: screen,
            label: screen.label(),
            details_kind: screen.details_kind(),
            search_verb: screen.search_verb(),
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
const MAX_WINDOW_FAILURE_CHARS: usize = 512;

/// Copies a window-side failure into the process log.
///
/// A web view swallows its own errors: without this, a window that failed to
/// wire itself up is indistinguishable from one that started cleanly, because
/// the process keeps running with empty stderr either way. The same applies to
/// every later rejection — an action name this window spells wrong is refused
/// by `dispatch`'s deserializer and would otherwise look exactly like a control
/// that does nothing.
#[tauri::command]
fn report_window_failure(context: String, message: String) {
    let bound = |text: String| -> String { text.chars().take(MAX_WINDOW_FAILURE_CHARS).collect() };
    eprintln!(
        "the Youta window failed: {} — {}",
        bound(context),
        bound(message)
    );
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

/// Serves the local waveform envelope for one exact generation.
///
/// This is the only part of the view that does not travel as JSON. A
/// window-wide envelope is several thousand sixteen-bit numbers, which JSON
/// inflates by roughly an order of magnitude, and it changes once per media
/// rather than once per frame — so it is pulled as bytes when the window needs
/// it instead of being pushed with every snapshot.
///
/// The reduction happens in Rust, using the same `reduced_for_width` the
/// terminal draws with. Reducing in the web view instead would put a second
/// copy of that arithmetic behind the same pixels.
#[tauri::command]
fn waveform_peaks(
    generation: u64,
    columns: usize,
    reducer: tauri::State<'_, ReducerHandle>,
) -> tauri::ipc::Response {
    tauri::ipc::Response::new(reducer.waveform_peaks(generation, columns))
}

/// Sends one key press to the shared keyboard map.
///
/// The window reports the press and how much it rendered, and learns nothing
/// about modal precedence: the same map that serves the terminal decides what
/// the key means. See [`youta::keymap`].
#[tauri::command]
fn key(
    press: KeyPress,
    page_rows: Option<usize>,
    popups: Option<PopupGeometry>,
    reducer: tauri::State<'_, ReducerHandle>,
) -> Result<(), String> {
    reducer.key(press, page_rows, popups)
}

/// Serves one artwork request arriving as `youta://artwork/<encoded-url>`.
///
/// Artwork bypasses the JSON channel entirely: the window asks for it with an
/// ordinary `<img src>`, and the bytes never enter a snapshot. That also keeps
/// the network in this process, so the provider sees Youta's guarded agent
/// rather than a request from the web view, and every protection in
/// [`youta::artwork`] still applies.
fn serve_artwork(cache_directory: &Path, request: &Request<Vec<u8>>) -> Response<Vec<u8>> {
    /// Refusals are deliberately bodyless and identical, so a caller cannot use
    /// this endpoint to probe what exists.
    fn refuse(status: u16) -> Response<Vec<u8>> {
        Response::builder()
            .status(status)
            .body(Vec::new())
            .unwrap_or_else(|_| Response::new(Vec::new()))
    }

    // Platforms shape a custom-scheme URI differently. Where the scheme is real
    // (`youta://artwork/x`) the first segment lands in the authority; where it
    // is emulated over HTTP (`http://youta.localhost/artwork/x`) it stays in the
    // path. Both must resolve to the same request.
    let uri = request.uri();
    let path = uri.path().trim_start_matches('/');
    let encoded = if uri.host() == Some("artwork") {
        path
    } else if let Some(rest) = path.strip_prefix("artwork/") {
        rest
    } else {
        return refuse(404);
    };
    if encoded.is_empty() {
        return refuse(400);
    }
    let Ok(decoded) = percent_decode_str(encoded).decode_utf8() else {
        return refuse(400);
    };
    let Ok(source) = Url::parse(&decoded) else {
        return refuse(400);
    };

    match remote_artwork(cache_directory, &source) {
        Ok(artwork) => Response::builder()
            .status(200)
            .header("Content-Type", artwork.format.media_type())
            // The bytes are already in Youta's own cache, so the web view keeps
            // only a short-lived copy.
            .header("Cache-Control", "max-age=300")
            .body(artwork.bytes)
            .unwrap_or_else(|_| refuse(500)),
        Err(_) => refuse(404),
    }
}

fn main() {
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("cannot load Youta configuration: {error}");
            std::process::exit(1);
        }
    };

    let artwork_cache: PathBuf = config.thumbnail_cache_dir();

    tauri::Builder::default()
        .register_uri_scheme_protocol("youta", move |_app, request| {
            serve_artwork(&artwork_cache, &request)
        })
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
            key,
            audio_output,
            waveform_peaks,
            report_window_failure
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

#[cfg(test)]
mod tests {
    use super::serve_artwork;

    use std::path::Path;

    use tauri::http::Request;

    /// Builds a protocol request the way a web view would.
    fn request(uri: &str) -> Request<Vec<u8>> {
        Request::builder()
            .uri(uri)
            .body(Vec::new())
            .expect("build protocol request")
    }

    /// Platforms shape the custom-scheme URI differently, and both forms must
    /// resolve to the same request.
    ///
    /// The malformed payload is what makes this test meaningful: a 400 can only
    /// come from the decoder, so it proves the payload was actually extracted.
    /// An unreachable-source 404 would also be produced by falling through.
    #[test]
    fn both_platform_uri_shapes_reach_the_same_handler() {
        let cache = Path::new("/nonexistent");
        for uri in [
            "youta://artwork/not%20a%20url",
            "http://youta.localhost/artwork/not%20a%20url",
        ] {
            assert_eq!(serve_artwork(cache, &request(uri)).status(), 400, "{uri}");
        }
    }

    /// The endpoint serves artwork and nothing else.
    #[test]
    fn unrelated_paths_and_malformed_sources_are_refused() {
        let cache = Path::new("/nonexistent");
        for (uri, expected) in [
            ("youta://localhost/", 404),
            ("http://youta.localhost/", 404),
            ("youta://secrets/credentials.toml", 404),
            ("http://youta.localhost/secrets/credentials.toml", 404),
            ("youta://artwork/", 400),
            ("youta://artwork/not%20a%20url", 400),
        ] {
            assert_eq!(
                serve_artwork(cache, &request(uri)).status(),
                expected,
                "{uri}"
            );
        }
    }

    /// Refusals carry no body, so the endpoint cannot be used to probe.
    #[test]
    fn a_refusal_reveals_nothing() {
        let response = serve_artwork(
            Path::new("/nonexistent"),
            &request("youta://artwork/file%3A%2F%2F%2Fetc%2Fpasswd"),
        );
        assert_eq!(response.status(), 404);
        assert!(response.body().is_empty());
    }
}
