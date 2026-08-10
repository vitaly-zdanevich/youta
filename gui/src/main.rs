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
mod desktop;
mod media_keys;
mod reducer;

use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;
use tauri::DragDropEvent;
use tauri::http::{Request, Response};
use tauri::{Manager, WindowEvent};
use url::Url;

use youta::artwork::{local_artwork, remote_artwork};
use youta::config::{Config, PlaybackBackend as ConfiguredBackend};
use youta::keymap::{KeyPress, PopupGeometry};
use youta::playback::{AudioOutputDriver, ProcessPlaybackConfig};
use youta::view::{InformationPanelKind, Screen, UiAction, ViewModel};

use desktop::WindowFocus;
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
///
/// Two kinds of source arrive here and they are trusted differently. A remote
/// one is fetched through the guarded agent, which needs no permission because
/// it applies its own rules to any URL. A local one names a real file — the
/// cover extracted out of a download, or the image `yt-dlp` left beside it — and
/// is served only when `published_local_artwork` recognizes it as something the
/// reducer put in a snapshot. Without that question this endpoint would read any
/// path a provider could name; see `reducer::PublishedArtwork`.
fn serve_artwork(
    cache_directory: &Path,
    published_local_artwork: &dyn Fn(&Url) -> bool,
    request: &Request<Vec<u8>>,
) -> Response<Vec<u8>> {
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

    let served = if source.scheme() == "file" {
        if !published_local_artwork(&source) {
            return refuse(404);
        }
        local_artwork(&source)
    } else {
        remote_artwork(cache_directory, &source)
    };

    match served {
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
    let focus = WindowFocus::default();
    let window_focus = focus.clone();

    tauri::Builder::default()
        // Both are used from the reducer thread rather than from the web view,
        // so neither adds a permission to the window's capability list.
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .menu(desktop::menu)
        .on_menu_event(|app, event| desktop::on_menu_event(app, &event))
        .on_window_event(move |window, event| match event {
            // Whether a track change is worth a notification depends on this
            // and on nothing else in the snapshot.
            WindowEvent::Focused(focused) => window_focus.set(*focused),
            // A drop is the desktop's own way of naming a file, and it means
            // what typing that path into the input box means. The reducer is
            // absent only if a drop lands during startup.
            WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }) => {
                if let Some(reducer) = window.try_state::<ReducerHandle>()
                    && let Err(error) = reducer.dispatch(UiAction::OpenDroppedPaths(paths.clone()))
                {
                    eprintln!("the Youta window could not open the dropped paths: {error}");
                }
            }
            _ => {}
        })
        .register_uri_scheme_protocol("youta", move |app, request| {
            // The reducer is managed by `setup`, which has run long before a
            // page can ask for an image; an absent handle simply means no local
            // artwork has been published, so none may be read.
            let reducer = app.app_handle().try_state::<ReducerHandle>();
            let published = |source: &Url| {
                reducer
                    .as_ref()
                    .is_some_and(|reducer| reducer.published_local_artwork(source))
            };
            serve_artwork(&artwork_cache, &published, &request)
        })
        .setup(move |app| {
            app.manage(AudioOutputView::from_config(&config));
            desktop::install_tray(app.handle());
            match reducer::start(app.handle().clone(), config.clone(), focus.clone()) {
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

    use tauri::http::{Request, Response};
    use url::Url;

    /// Builds a protocol request the way a web view would.
    fn request(uri: &str) -> Request<Vec<u8>> {
        Request::builder()
            .uri(uri)
            .body(Vec::new())
            .expect("build protocol request")
    }

    /// Serves a request against a window that has published no local artwork.
    fn serve(cache: &Path, request: &Request<Vec<u8>>) -> Response<Vec<u8>> {
        serve_artwork(cache, &|_| false, request)
    }

    /// Builds the request a window makes for one local artwork file.
    fn artwork_request(path: &Path) -> (Url, Request<Vec<u8>>) {
        let url = Url::from_file_path(path).expect("absolute artwork path");
        let encoded =
            percent_encoding::utf8_percent_encode(url.as_str(), percent_encoding::NON_ALPHANUMERIC)
                .to_string();
        (url, request(&format!("youta://artwork/{encoded}")))
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
            assert_eq!(serve(cache, &request(uri)).status(), 400, "{uri}");
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
            assert_eq!(serve(cache, &request(uri)).status(), expected, "{uri}");
        }
    }

    /// Refusals carry no body, so the endpoint cannot be used to probe.
    #[test]
    fn a_refusal_reveals_nothing() {
        let response = serve(
            Path::new("/nonexistent"),
            &request("youta://artwork/file%3A%2F%2F%2Fetc%2Fpasswd"),
        );
        assert_eq!(response.status(), 404);
        assert!(response.body().is_empty());
    }

    /// A local file is served only because a snapshot named it.
    ///
    /// This is the whole boundary: the same request against a window that never
    /// published that URL must be refused, and it must be refused for a readable
    /// image rather than only for a missing one — otherwise the test would pass
    /// on an unrelated failure.
    #[test]
    fn local_artwork_is_served_only_when_the_reducer_published_it() {
        let directory = tempfile::tempdir().expect("temporary artwork directory");
        let cover = directory.path().join("Track [id].webp");
        std::fs::write(&cover, b"RIFF\0\0\0\0WEBPVP8 body").expect("write sidecar cover");
        let (published_url, request) = artwork_request(&cover);

        let refused = serve(directory.path(), &request);
        assert_eq!(refused.status(), 404);
        assert!(refused.body().is_empty());

        let served = serve_artwork(
            directory.path(),
            &|source: &Url| source == &published_url,
            &request,
        );
        assert_eq!(served.status(), 200);
        assert_eq!(
            served
                .headers()
                .get("Content-Type")
                .map(|value| value.to_str().expect("ASCII media type")),
            Some("image/webp"),
            "the media type comes from the bytes, not from the extension"
        );
        assert_eq!(served.body(), b"RIFF\0\0\0\0WEBPVP8 body");
    }

    /// Publishing one local file must not make its neighbours readable.
    #[test]
    fn a_published_cover_does_not_open_the_directory_around_it() {
        let directory = tempfile::tempdir().expect("temporary artwork directory");
        let cover = directory.path().join("cover.png");
        let private = directory.path().join("notes.png");
        std::fs::write(&cover, b"\x89PNG\r\n\x1a\ncover").expect("write published cover");
        std::fs::write(&private, b"\x89PNG\r\n\x1a\nprivate").expect("write neighbour file");
        let (published_url, _) = artwork_request(&cover);
        let (_, neighbour) = artwork_request(&private);

        assert_eq!(
            serve_artwork(
                directory.path(),
                &|source: &Url| source == &published_url,
                &neighbour
            )
            .status(),
            404
        );
    }
}
