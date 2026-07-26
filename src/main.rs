//! Command-line entry point for Youta.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use youta::config::Config;
use youta::diagnostics::{ExternalHelper, ExternalHelperKind};
#[cfg(feature = "tui")]
use youta::persistence::StateStore;
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
            let report = youta::diagnostics::DiagnosticReport::capture_error(
                error.as_ref(),
                probe_diagnostic_helpers(PathBuf::from("mpv"), PathBuf::from("yt-dlp")),
            );
            eprintln!("{}", report.render());
            ExitCode::FAILURE
        }
    }
}

fn probe_diagnostic_helpers(
    mpv_executable: PathBuf,
    yt_dlp_executable: PathBuf,
) -> Vec<ExternalHelper> {
    ExternalHelper::probe_many([
        (ExternalHelperKind::Mpv, Some(mpv_executable)),
        (ExternalHelperKind::YtDlp, Some(yt_dlp_executable)),
        (ExternalHelperKind::Ffmpeg, Some(PathBuf::from("ffmpeg"))),
        (ExternalHelperKind::Ffprobe, Some(PathBuf::from("ffprobe"))),
    ])
}

fn run() -> Result<()> {
    let cli = Cli::parse();
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
    #[cfg(feature = "backend-mpv")]
    use youta::app::PlaybackFactory;
    use youta::config::PlaybackBackend as ConfiguredBackend;
    use youta::diagnostics::{DiagnosticReport, format_panic};
    use youta::tui::{SeekBarStyle, UiSettings};

    let diagnostic_mpv = config.providers.mpv_executable.clone();
    let diagnostic_yt_dlp = config.providers.yt_dlp_executable.clone();
    let (provider, provider_startup_error) = match configured_youtube_provider(&config.providers) {
        Ok(provider) => (provider, None),
        Err(error) => (None, Some(error)),
    };
    let playback_factory = match config.playback.backend {
        ConfiguredBackend::Mpv => {
            #[cfg(feature = "backend-mpv")]
            {
                use youta::playback::mpv::MpvBackend;

                let process_config = process_playback_config(&config);
                let factory: PlaybackFactory = Box::new(move || {
                    let player = MpvBackend::spawn(&process_config)?;
                    Ok(Box::new(player))
                });
                Some(factory)
            }
            #[cfg(not(feature = "backend-mpv"))]
            {
                None
            }
        }
        ConfiguredBackend::Native => None,
    };
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
        ..UiSettings::default()
    };
    let mut controller = AppController::new(config, store, provider, playback_factory);
    if let Some(error) = provider_startup_error {
        let report = DiagnosticReport::capture_error(
            &error,
            probe_diagnostic_helpers(diagnostic_mpv.clone(), diagnostic_yt_dlp.clone()),
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
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => {
            let helpers =
                probe_diagnostic_helpers(diagnostic_mpv.clone(), diagnostic_yt_dlp.clone());
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
                .with_external_helpers(probe_diagnostic_helpers(diagnostic_mpv, diagnostic_yt_dlp))
                .render();
            controller.enter_fatal_diagnostic_mode("Youta panicked", report);
            let _ = catch_unwind(AssertUnwindSafe(|| {
                youta::tui::run(&mut controller, &settings)
            }));
            bail!("terminal UI panicked; Youta displayed a diagnostic report")
        }
    }
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

#[cfg(all(feature = "tui", feature = "backend-mpv"))]
fn process_playback_config(config: &Config) -> youta::playback::ProcessPlaybackConfig {
    use youta::config::AudioOutput as ConfiguredAudioOutput;
    use youta::playback::{
        AudioOutputDriver, AudiophilePlaybackOptions, PlaybackProfile, ProcessPlaybackConfig,
    };

    let audio_output = match config.playback.output {
        ConfiguredAudioOutput::Auto => AudioOutputDriver::Auto,
        ConfiguredAudioOutput::Alsa => AudioOutputDriver::Alsa,
        ConfiguredAudioOutput::Jack => AudioOutputDriver::Jack,
        ConfiguredAudioOutput::PulseAudio => AudioOutputDriver::PulseAudio,
        ConfiguredAudioOutput::PipeWire => AudioOutputDriver::PipeWire,
    };
    ProcessPlaybackConfig {
        mpv_executable: config.providers.mpv_executable.clone(),
        yt_dlp_executable: config.providers.yt_dlp_executable.clone(),
        runtime_dir: config.config_dir().join("runtime"),
        audio_output,
        audio_device: config.playback.device.clone(),
        profile: if config.playback.audiophile.enabled {
            PlaybackProfile::Direct
        } else {
            PlaybackProfile::Balanced
        },
        audiophile: AudiophilePlaybackOptions {
            exclusive_device: config.playback.audiophile.exclusive_device,
            avoid_resampling: config.playback.audiophile.avoid_resampling,
            output_sample_rate_hz: config.playback.audiophile.output_sample_rate_hz,
        },
    }
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

fn doctor_helper_checks(config: &Config) -> Vec<HelperCheck<'_>> {
    use youta::config::PlaybackBackend;

    let mut checks = Vec::with_capacity(4);
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
    checks.extend([
        HelperCheck {
            name: "ffmpeg",
            executable: Path::new("ffmpeg"),
            arguments: &["-version"],
            required: false,
        },
        HelperCheck {
            name: "ffprobe",
            executable: Path::new("ffprobe"),
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
    println!("state database: {}", config.database_file().display());
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

    let mut missing_required = false;
    for check in doctor_helper_checks(config) {
        match helper_version(check.executable, check.arguments) {
            Ok(version) => println!("{}: {version}", check.name),
            Err(error) => {
                println!("{}: unavailable ({error})", check.name);
                missing_required |= check.required;
            }
        }
    }

    #[cfg(feature = "tracker-music")]
    {
        let tracker_decoder = Command::new("ffmpeg")
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
    let output = Command::new(executable)
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
    println!("database_file = {}", config.database_file().display());
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
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;
    #[cfg(all(feature = "tui", feature = "backend-mpv"))]
    use youta::config::AudioOutput;
    use youta::config::{Config, PlaybackBackend};

    use super::doctor_helper_checks;

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

    #[cfg(all(feature = "tui", feature = "backend-mpv"))]
    #[test]
    fn process_config_maps_every_audio_output_driver() {
        use youta::playback::AudioOutputDriver;

        let temporary = tempdir().expect("temporary directory");
        let mut config = Config::for_dir(temporary.path());
        for (configured, expected) in [
            (AudioOutput::Auto, AudioOutputDriver::Auto),
            (AudioOutput::Alsa, AudioOutputDriver::Alsa),
            (AudioOutput::Jack, AudioOutputDriver::Jack),
            (AudioOutput::PulseAudio, AudioOutputDriver::PulseAudio),
            (AudioOutput::PipeWire, AudioOutputDriver::PipeWire),
        ] {
            config.playback.output = configured;
            assert_eq!(
                super::process_playback_config(&config).audio_output,
                expected
            );
        }
    }

    #[cfg(all(feature = "tui", feature = "backend-mpv"))]
    #[test]
    fn process_config_preserves_audiophile_tuning() {
        use youta::playback::PlaybackProfile;

        let temporary = tempdir().expect("temporary directory");
        let mut config = Config::for_dir(temporary.path());
        config.playback.audiophile.enabled = true;
        config.playback.audiophile.exclusive_device = true;
        config.playback.audiophile.avoid_resampling = true;
        config.playback.audiophile.output_sample_rate_hz = Some(96_000);

        let process = super::process_playback_config(&config);
        assert_eq!(process.profile, PlaybackProfile::Direct);
        assert!(process.audiophile.exclusive_device);
        assert!(process.audiophile.avoid_resampling);
        assert_eq!(process.audiophile.output_sample_rate_hz, Some(96_000));
    }
}
