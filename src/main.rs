//! Command-line entry point for Youta.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use youta::config::Config;
use youta::diagnostics::{ExternalHelper, ExternalHelperKind};
#[cfg(feature = "tui")]
use youta::persistence::StateStore;
use youta::persistence::{ANOTHER_INSTANCE_MESSAGE, PersistenceError};
#[cfg(feature = "yt-dlp")]
use youta::playback::ytdlp::{YtDlp, YtDlpConfig};
use youta::providers::{SearchItem, SearchRequest, SearchTarget, configured_youtube_provider};

/// Low-resource terminal `YouTube` audio player using `yt-dlp`.
#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Low-resource terminal YouTube audio player using yt-dlp",
    long_about = None
)]
struct Cli {
    /// Print the complete MIT license notice and exit.
    #[arg(long, global = true)]
    license: bool,

    /// Use this directory instead of the platform Youta configuration folder.
    #[arg(long, global = true, value_name = "PATH")]
    config_dir: Option<PathBuf>,

    /// Operation to run. With no command, Youta opens the terminal UI.
    #[command(subcommand)]
    command: Option<CliCommand>,
}

#[derive(Debug, Subcommand)]
enum CliCommand {
    /// Open the interactive terminal player.
    Tui,
    /// Search `YouTube` through the configured official API or Invidious.
    #[command(about = "Search YouTube through the configured official API or Invidious")]
    Search {
        /// Text to search for.
        query: String,
        /// Search channel names instead of videos.
        #[arg(long)]
        channels: bool,
    },
    /// Check helpers, configuration, storage, and tracker decoder support.
    Doctor,
    /// Print non-secret effective configuration and state paths.
    Config,
    /// List the site extractors reported by the installed yt-dlp.
    Extractors,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if is_another_instance_error(&error) {
                eprintln!("{ANOTHER_INSTANCE_MESSAGE}");
            } else {
                let report = youta::diagnostics::DiagnosticReport::capture_error(
                    error.as_ref(),
                    probe_diagnostic_helpers(
                        PathBuf::from("mpv"),
                        PathBuf::from("yt-dlp"),
                        PathBuf::from("fpcalc"),
                    ),
                );
                eprintln!("{}", report.render());
            }
            ExitCode::FAILURE
        }
    }
}

/// Identifies the expected conflict raised when another process owns the state lock.
///
/// The downcast remains valid through the context added by [`run_tui`], so
/// unrelated persistence and startup errors continue through full diagnostics.
fn is_another_instance_error(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<PersistenceError>(),
        Some(PersistenceError::FileStateAlreadyOpen)
    )
}

fn probe_diagnostic_helpers(
    mpv_executable: PathBuf,
    yt_dlp_executable: PathBuf,
    fpcalc_executable: PathBuf,
) -> Vec<ExternalHelper> {
    let helpers = vec![
        (ExternalHelperKind::Mpv, Some(mpv_executable)),
        (ExternalHelperKind::YtDlp, Some(yt_dlp_executable)),
        (ExternalHelperKind::Ffmpeg, Some(PathBuf::from("ffmpeg"))),
        (ExternalHelperKind::Ffprobe, Some(PathBuf::from("ffprobe"))),
    ];
    #[cfg(feature = "acoustid")]
    let helpers = {
        let mut helpers = helpers;
        helpers.push((ExternalHelperKind::Fpcalc, Some(fpcalc_executable)));
        helpers
    };
    #[cfg(not(feature = "acoustid"))]
    let _ = fpcalc_executable;
    ExternalHelper::probe_many(helpers)
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.license {
        print!("{}", youta::LICENSE_TEXT);
        return Ok(());
    }
    let config = if let Some(directory) = cli.config_dir {
        Config::load_from_dir(directory)
    } else {
        Config::load()
    }
    .context("cannot load Youta configuration")?;

    match cli.command.unwrap_or(CliCommand::Tui) {
        CliCommand::Tui => run_tui(config),
        CliCommand::Search { query, channels } => run_search(&config, &query, channels),
        CliCommand::Doctor => run_doctor(&config),
        CliCommand::Config => {
            print_config(&config);
            Ok(())
        }
        CliCommand::Extractors => list_extractors(&config),
    }
}

#[cfg(feature = "tui")]
fn run_tui(config: Config) -> Result<()> {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{Arc, Mutex};

    use youta::app::AppController;
    use youta::diagnostics::{DiagnosticReport, format_panic};
    use youta::tui::{SeekBarStyle, UiSettings};

    let diagnostic_mpv = config.providers.mpv_executable.clone();
    let diagnostic_yt_dlp = config.providers.yt_dlp_executable.clone();
    let diagnostic_fpcalc = config.providers.fpcalc_executable.clone();
    let (provider, provider_startup_error) = match configured_youtube_provider(&config.providers) {
        Ok(provider) => (provider, None),
        Err(error) => (None, Some(error)),
    };
    let playback_factory = youta::playback::configured_playback_factory(&config);
    let store = StateStore::open(&config).context("cannot open restart-safe state")?;
    let settings = UiSettings {
        show_hotkeys: config.ui.show_button_hotkeys,
        funny_mode: config.ui.dos_rpg_mode,
        seek_bar_style: if config.ui.nyan_cat_seekbar {
            SeekBarStyle::NyanCat
        } else {
            SeekBarStyle::Line
        },
        thumbnails: config.ui.thumbnails,
        thumbnail_height: config.ui.thumbnail_height,
        prefetch_search_thumbnails: config.ui.prefetch_search_thumbnails,
        thumbnail_cache_dir: Some(config.thumbnail_cache_dir()),
        ffmpeg_executable: config.providers.ffmpeg_executable.clone(),
        ..UiSettings::default()
    };
    let shutdown_git_sync = config
        .persistence
        .git_commit_on_change
        .then(|| config.config_dir().to_path_buf());
    let mut controller = AppController::new(config, store, provider, playback_factory);
    if let Some(error) = provider_startup_error {
        let report = DiagnosticReport::capture_error(
            &error,
            probe_diagnostic_helpers(
                diagnostic_mpv.clone(),
                diagnostic_yt_dlp.clone(),
                diagnostic_fpcalc.clone(),
            ),
        )
        .render();
        controller.show_diagnostic_report("YouTube provider configuration failed", report);
    }
    let panic_report = Arc::new(Mutex::new(None::<DiagnosticReport>));
    let hook_report = Arc::clone(&panic_report);
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let message = format_panic(panic_info);
        let report = DiagnosticReport::capture_message(message, Vec::<ExternalHelper>::new());
        if let Ok(mut slot) = hook_report.lock() {
            *slot = Some(report);
        }
    }));
    let run_result = catch_unwind(AssertUnwindSafe(|| {
        youta::tui::run(&mut controller, &settings)
    }));
    std::panic::set_hook(previous_hook);

    match run_result {
        Ok(Ok(())) => {
            let synchronization =
                finalize_before_sync(controller, AppController::shutdown_for_exit, || {
                    shutdown_git_sync
                        .as_deref()
                        .map(youta::git_sync::sync_config_root)
                        .transpose()
                });
            match synchronization {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => eprintln!("Youta Git synchronization failed: {error}"),
                Err(error) => {
                    eprintln!(
                        "Youta state persistence failed; automatic Git synchronization was skipped: {error}"
                    );
                }
            }
            Ok(())
        }
        Ok(Err(error)) => {
            let helpers = probe_diagnostic_helpers(
                diagnostic_mpv.clone(),
                diagnostic_yt_dlp.clone(),
                diagnostic_fpcalc.clone(),
            );
            let report = DiagnosticReport::capture_error(&error, helpers).render();
            controller.enter_fatal_diagnostic_mode("Terminal UI failed", report);
            let _ = catch_unwind(AssertUnwindSafe(|| {
                youta::tui::run(&mut controller, &settings)
            }));
            Err(error).context("terminal UI failed")
        }
        Err(payload) => {
            let report = panic_report
                .lock()
                .ok()
                .and_then(|mut report| report.take())
                .unwrap_or_else(|| {
                    DiagnosticReport::capture_message(
                        panic_payload_message(payload.as_ref()),
                        Vec::<ExternalHelper>::new(),
                    )
                })
                .with_external_helpers(probe_diagnostic_helpers(
                    diagnostic_mpv,
                    diagnostic_yt_dlp,
                    diagnostic_fpcalc,
                ))
                .render();
            controller.enter_fatal_diagnostic_mode("Youta panicked", report);
            let _ = catch_unwind(AssertUnwindSafe(|| {
                youta::tui::run(&mut controller, &settings)
            }));
            bail!("terminal UI panicked; Youta displayed a diagnostic report")
        }
    }
}

/// Durably shuts down and drops an owner before invoking a synchronization callback.
///
/// Dropping the owner before `synchronize` releases resources such as the
/// human-readable state lock. A shutdown error drops the owner but returns
/// without invoking `synchronize`.
#[cfg(feature = "tui")]
fn finalize_before_sync<T, Shutdown, Synchronize, Output, Error>(
    mut owner: T,
    shutdown: Shutdown,
    synchronize: Synchronize,
) -> Result<Output, Error>
where
    Shutdown: FnOnce(&mut T) -> Result<(), Error>,
    Synchronize: FnOnce() -> Output,
{
    shutdown(&mut owner)?;
    drop(owner);
    Ok(synchronize())
}

#[cfg(feature = "tui")]
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload (contents omitted)".to_owned()
    }
}

#[cfg(not(feature = "tui"))]
fn run_tui(_config: Config) -> Result<()> {
    bail!("this build does not include the `tui` feature")
}

fn run_search(config: &Config, query: &str, channels: bool) -> Result<()> {
    let provider = configured_youtube_provider(&config.providers)?.context(
        "configure providers.youtube_api_key or providers.invidious_base_url before searching YouTube",
    )?;
    let target = if channels {
        SearchTarget::Channels
    } else {
        SearchTarget::Videos
    };
    let page = provider
        .search(&SearchRequest::new(query, target))
        .context("YouTube search failed")?;
    for item in page.items {
        match item {
            SearchItem::Video(video) => {
                println!(
                    "{}\t{}\t{}",
                    video.video_id,
                    video.title,
                    video
                        .webpage_url
                        .map_or_else(String::new, |url| url.to_string())
                );
            }
            SearchItem::Channel(channel) => {
                println!(
                    "{}\t{}\t{}",
                    channel.channel_id,
                    channel.name,
                    channel
                        .webpage_url
                        .map_or_else(String::new, |url| url.to_string())
                );
            }
            SearchItem::PodcastEpisode(_) => {
                bail!("the configured YouTube provider returned an RSS podcast episode");
            }
        }
    }
    Ok(())
}

fn list_extractors(config: &Config) -> Result<()> {
    #[cfg(feature = "yt-dlp")]
    {
        let client = YtDlp::new(YtDlpConfig {
            executable: config.providers.yt_dlp_executable.clone(),
            ..YtDlpConfig::default()
        });
        for extractor in client
            .extractors()
            .context("cannot list yt-dlp extractors")?
        {
            println!("{extractor}");
        }
        Ok(())
    }
    #[cfg(not(feature = "yt-dlp"))]
    {
        let _ = config;
        bail!("this build does not include the `yt-dlp` feature")
    }
}

#[derive(Clone, Copy, Debug)]
struct HelperCheck<'a> {
    name: &'static str,
    executable: &'a Path,
    arguments: &'static [&'static str],
    required: bool,
}

/// Returns actionable package guidance for an unavailable optional helper.
#[cfg(feature = "acoustid")]
fn helper_install_guidance(name: &str) -> Option<&'static str> {
    (name == "fpcalc").then_some(youta::audio_identification::FPCALC_INSTALL_GUIDANCE)
}

fn doctor_helper_checks(config: &Config) -> Vec<HelperCheck<'_>> {
    use youta::config::PlaybackBackend;

    let mut checks = Vec::with_capacity(5);
    if cfg!(feature = "backend-mpv") && config.playback.backend == PlaybackBackend::Mpv {
        checks.push(HelperCheck {
            name: "mpv",
            executable: config.providers.mpv_executable.as_path(),
            arguments: &["--version"],
            required: true,
        });
    }
    if cfg!(feature = "yt-dlp") {
        checks.push(HelperCheck {
            name: "yt-dlp",
            executable: config.providers.yt_dlp_executable.as_path(),
            arguments: &["--version"],
            required: true,
        });
    }
    if cfg!(feature = "acoustid") {
        checks.push(HelperCheck {
            name: "fpcalc",
            executable: config.providers.fpcalc_executable.as_path(),
            arguments: &["-version"],
            required: config.providers.acoustid_client_key.is_some(),
        });
    }
    checks.extend([
        HelperCheck {
            name: "ffmpeg",
            executable: &config.providers.ffmpeg_executable,
            arguments: &["-version"],
            required: false,
        },
        HelperCheck {
            name: "ffprobe",
            executable: &config.providers.ffprobe_executable,
            arguments: &["-version"],
            required: false,
        },
    ]);
    checks
}

fn run_doctor(config: &Config) -> Result<()> {
    use youta::config::PlaybackBackend;

    config
        .ensure_directories()
        .context("cannot prepare private Youta directories")?;
    println!("config directory: {}", config.config_dir().display());
    println!("state backend: {}", config.persistence.backend);
    println!("state directory: {}", config.state_dir().display());
    if config.persistence.backend == youta::config::PersistenceBackend::Sqlite {
        println!("SQLite database: {}", config.database_file().display());
    }
    println!("credentials file: {}", config.credentials_file().display());
    println!(
        "Invidious: {}",
        config
            .providers
            .invidious_base_url
            .as_ref()
            .map_or("not configured", |_| "configured")
    );
    println!(
        "YouTube Data API: {} (backend: {})",
        if config.providers.youtube_api_key.is_some() {
            "configured"
        } else {
            "not configured"
        },
        config.providers.youtube_backend
    );

    if !cfg!(feature = "backend-mpv") {
        println!("mpv: skipped (backend omitted at build time)");
    } else if config.playback.backend != PlaybackBackend::Mpv {
        println!("mpv: skipped (backend not selected)");
    }
    if !cfg!(feature = "yt-dlp") {
        println!("yt-dlp: skipped (feature omitted at build time)");
    }
    if !cfg!(feature = "acoustid") {
        println!("fpcalc: skipped (AcoustID feature omitted at build time)");
    }

    let mut missing_required = false;
    for check in doctor_helper_checks(config) {
        match helper_version(check.executable, check.arguments) {
            Ok(version) => println!("{}: {version}", check.name),
            Err(error) => {
                println!("{}: unavailable ({error})", check.name);
                #[cfg(feature = "acoustid")]
                if let Some(guidance) = helper_install_guidance(check.name) {
                    println!("{guidance}");
                }
                missing_required |= check.required;
            }
        }
    }

    #[cfg(feature = "tracker-music")]
    {
        let tracker_decoder =
            youta::child_process::quiet(&mut Command::new(&config.providers.ffmpeg_executable))
                .args(["-hide_banner", "-demuxers"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .is_some_and(|output| {
                    String::from_utf8_lossy(&output.stdout)
                        .to_ascii_lowercase()
                        .contains("libopenmpt")
                });
        println!(
            "tracker replay through FFmpeg/libopenmpt: {}",
            if tracker_decoder {
                "available"
            } else {
                "not detected"
            }
        );
    }
    #[cfg(not(feature = "tracker-music"))]
    println!("tracker replay: skipped (feature omitted at build time)");
    println!(
        "Mirsoft plaintext HTTP: {} (user-selected default)",
        if config.providers.allow_insecure_http {
            "enabled"
        } else {
            "disabled"
        }
    );

    if missing_required {
        bail!("one or more required playback helpers are unavailable")
    }
    Ok(())
}

fn helper_version(executable: &Path, arguments: &[&str]) -> Result<String> {
    let output = youta::child_process::quiet(&mut Command::new(executable))
        .args(arguments)
        .output()
        .with_context(|| format!("cannot start {}", executable.display()))?;
    if !output.status.success() {
        bail!("exited with {}", output.status);
    }
    let text = if output.stdout.is_empty() {
        &output.stderr
    } else {
        &output.stdout
    };
    Ok(String::from_utf8_lossy(text)
        .lines()
        .next()
        .unwrap_or("version not reported")
        .trim()
        .to_owned())
}

fn print_config(config: &Config) {
    println!("config_dir = {}", config.config_dir().display());
    println!("config_file = {}", config.config_file().display());
    println!("credentials_file = {}", config.credentials_file().display());
    println!("persistence_backend = {}", config.persistence.backend);
    println!("state_dir = {}", config.state_dir().display());
    println!("runtime_dir = {}", config.runtime_dir().display());
    println!("cache_dir = {}", config.cache_dir().display());
    if config.persistence.backend == youta::config::PersistenceBackend::Sqlite {
        println!("database_file = {}", config.database_file().display());
    }
    println!(
        "subscriptions_file = {}",
        config.subscriptions_file().display()
    );
    println!("downloads_dir = {}", config.downloads_dir().display());
    println!("youtube_backend = {}", config.providers.youtube_backend);
    println!(
        "invidious_base_url = {}",
        config
            .providers
            .invidious_base_url
            .as_ref()
            .map_or("unset", url::Url::as_str)
    );
    println!(
        "peertube_instance_url = {}",
        config
            .providers
            .peertube_instance_url
            .as_ref()
            .map_or("unset", url::Url::as_str)
    );
    println!(
        "funkwhale_instance_url = {}",
        config
            .providers
            .funkwhale_instance_url
            .as_ref()
            .map_or("unset", url::Url::as_str)
    );
    println!(
        "allow_insecure_http = {}",
        config.providers.allow_insecure_http
    );
    println!(
        "yt_dlp_executable = {}",
        config.providers.yt_dlp_executable.display()
    );
    println!(
        "mpv_executable = {}",
        config.providers.mpv_executable.display()
    );
    println!(
        "fpcalc_executable = {}",
        config.providers.fpcalc_executable.display()
    );
    println!(
        "youtube_api_key = {}",
        if config.providers.youtube_api_key.is_some() {
            "configured (redacted)"
        } else {
            "unset"
        }
    );
    println!(
        "mod_archive_api_key = {}",
        if config.providers.mod_archive_api_key.is_some() {
            "configured (redacted)"
        } else {
            "unset"
        }
    );
    println!(
        "acoustid_client_key = {}",
        if config.providers.acoustid_client_key.is_some() {
            "configured (redacted)"
        } else {
            "unset"
        }
    );
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    use youta::config::{Config, PlaybackBackend};

    use super::doctor_helper_checks;
    #[cfg(feature = "acoustid")]
    use super::helper_install_guidance;

    #[test]
    fn doctor_checks_only_helpers_required_by_compiled_features() {
        let temporary = tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path());
        let checks = doctor_helper_checks(&config);

        assert_eq!(
            checks.iter().any(|check| check.name == "mpv"),
            cfg!(feature = "backend-mpv")
        );
        assert_eq!(
            checks.iter().any(|check| check.name == "yt-dlp"),
            cfg!(feature = "yt-dlp")
        );
        assert_eq!(
            checks.iter().any(|check| check.name == "fpcalc"),
            cfg!(feature = "acoustid")
        );
        assert!(
            checks
                .iter()
                .filter(|check| check.required)
                .all(|check| check.name == "mpv" || check.name == "yt-dlp")
        );
    }

    #[test]
    fn doctor_does_not_require_mpv_when_native_backend_is_selected() {
        let temporary = tempdir().expect("temporary directory");
        let mut config = Config::for_dir(temporary.path());
        config.playback.backend = PlaybackBackend::Native;

        assert!(
            doctor_helper_checks(&config)
                .iter()
                .all(|check| check.name != "mpv")
        );
    }

    #[cfg(feature = "acoustid")]
    #[test]
    fn doctor_requires_configured_fpcalc_only_when_fingerprinting_is_enabled() {
        let temporary = tempdir().expect("temporary directory");
        let mut config = Config::for_dir(temporary.path());
        config.providers.fpcalc_executable = "custom-fpcalc".into();

        let optional = doctor_helper_checks(&config)
            .into_iter()
            .find(|check| check.name == "fpcalc")
            .expect("AcoustID build must probe fpcalc");
        assert!(!optional.required);
        assert_eq!(optional.executable, std::path::Path::new("custom-fpcalc"));
        assert_eq!(optional.arguments, ["-version"]);

        config.providers.acoustid_client_key = Some("fixture-client-key".to_owned());
        let required = doctor_helper_checks(&config)
            .into_iter()
            .find(|check| check.name == "fpcalc")
            .expect("configured AcoustID must retain fpcalc check");
        assert!(required.required);
    }

    #[cfg(feature = "acoustid")]
    #[test]
    fn doctor_install_guidance_is_scoped_to_fpcalc() {
        let guidance = helper_install_guidance("fpcalc").expect("fpcalc install guidance");

        for command in [
            "USE=tools emerge media-libs/chromaprint",
            "apt install libchromaprint-tools",
            "dnf install chromaprint-tools",
            "brew install chromaprint",
        ] {
            assert!(guidance.contains(command));
        }
        assert!(helper_install_guidance("mpv").is_none());
    }

    #[cfg(feature = "tui")]
    #[test]
    fn panic_payloads_are_bounded_to_string_data() {
        let text = "panic fixture".to_owned();
        assert_eq!(
            super::panic_payload_message(&text),
            "panic fixture".to_owned()
        );
        let opaque = 42_u64;
        assert_eq!(
            super::panic_payload_message(&opaque),
            "non-string panic payload (contents omitted)"
        );
    }

    #[cfg(feature = "tui")]
    #[test]
    fn graceful_exit_flushes_then_drops_lock_before_sync() {
        use std::cell::Cell;
        use std::cell::RefCell;
        use std::rc::Rc;

        struct LockedOwner {
            events: Rc<RefCell<Vec<&'static str>>>,
            lock_held: Rc<Cell<bool>>,
        }

        impl Drop for LockedOwner {
            fn drop(&mut self) {
                self.events.borrow_mut().push("drop");
                self.lock_held.set(false);
            }
        }

        let events = Rc::new(RefCell::new(Vec::new()));
        let lock_held = Rc::new(Cell::new(true));
        let owner = LockedOwner {
            events: Rc::clone(&events),
            lock_held: Rc::clone(&lock_held),
        };

        let outcome = super::finalize_before_sync(
            owner,
            |owner| {
                assert!(owner.lock_held.get(), "state lock disappeared before flush");
                owner.events.borrow_mut().push("flush");
                Ok::<_, &'static str>(())
            },
            || {
                assert!(!lock_held.get(), "Git ran while the state lock was held");
                events.borrow_mut().push("sync");
                youta::git_sync::GitSyncOutcome::NotRepository
            },
        )
        .expect("durable shutdown");

        assert_eq!(outcome, youta::git_sync::GitSyncOutcome::NotRepository);
        assert_eq!(*events.borrow(), ["flush", "drop", "sync"]);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn graceful_exit_skips_sync_when_persistence_fails() {
        use std::cell::Cell;
        use std::rc::Rc;

        struct DropProbe(Rc<Cell<bool>>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }

        let dropped = Rc::new(Cell::new(false));
        let synchronized = Rc::new(Cell::new(false));
        let synchronized_by_callback = Rc::clone(&synchronized);
        let error = super::finalize_before_sync(
            DropProbe(Rc::clone(&dropped)),
            |_| Err::<(), _>("fixture persistence failure"),
            move || synchronized_by_callback.set(true),
        )
        .expect_err("persistence failure");

        assert_eq!(error, "fixture persistence failure");
        assert!(
            dropped.get(),
            "failed owner must still release its state lock"
        );
        assert!(
            !synchronized.get(),
            "Git callback must not run after persistence failure"
        );
    }

    #[cfg(feature = "tui")]
    #[test]
    fn graceful_exit_releases_real_state_lock_before_callback() {
        use youta::app::AppController;
        use youta::persistence::StateStore;

        let temporary = tempdir().expect("temporary directory");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = StateStore::open(&config).expect("initial state lock");
        let controller = AppController::new(config.clone(), store, None, None);

        let reopened =
            super::finalize_before_sync(controller, AppController::shutdown_for_exit, || {
                StateStore::open(&config)
            })
            .expect("durable controller shutdown")
            .expect("state lock released before callback");

        drop(reopened);
    }
}
