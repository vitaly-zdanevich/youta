//! Layered application configuration and confined application paths.
//!
//! Configuration is loaded in this precedence order:
//!
//! 1. low-resource defaults;
//! 2. `<config-dir>/config.toml`, when it exists;
//! 3. environment variables prefixed with `YOUTA_`.
//!
//! Double underscores express nesting, so
//! `YOUTA_PLAYBACK__VOLUME_PERCENT=40` overrides
//! `playback.volume_percent`. `YOUTA_CONFIG_DIR` selects the directory before
//! the TOML file is read. Every path Youta writes is derived from that one
//! application directory.

use std::fmt;
use std::fs;
#[cfg(feature = "tui")]
use std::fs::OpenOptions;
#[cfg(feature = "tui")]
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use directories::BaseDirs;
use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};
#[cfg(feature = "tui")]
use toml_edit::{DocumentMut, Item, Table, value};
use url::Url;

use crate::domain::{DEFAULT_RESUME_REWIND_SECONDS, PLAYED_THRESHOLD_PERCENT};

/// Name of the environment variable that selects Youta's application folder.
pub const CONFIG_DIR_ENV: &str = "YOUTA_CONFIG_DIR";

/// Default maximum thumbnail height in terminal rows.
pub const DEFAULT_THUMBNAIL_HEIGHT: u16 = 20;

/// Smallest thumbnail height that the terminal renderer can use.
pub const MIN_THUMBNAIL_HEIGHT: u16 = 4;

#[cfg(feature = "tui")]
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// The root application configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Config {
    /// Playback behavior and output selection.
    pub playback: PlaybackConfig,
    /// Local subscription behavior.
    pub subscriptions: SubscriptionConfig,
    /// Terminal presentation preferences.
    pub ui: UiConfig,
    /// State persistence behavior.
    pub persistence: PersistenceConfig,
    /// Network provider endpoints, credentials, and helper executables.
    pub providers: ProviderConfig,
    /// Root for every file and directory written by Youta.
    ///
    /// This field is selected by [`Config::load`] or
    /// [`Config::load_from_dir`]. It is intentionally skipped in TOML and
    /// environment deserialization so a loaded file cannot redirect writes.
    #[serde(skip, default = "default_config_dir")]
    root_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self::for_dir(default_config_dir())
    }
}

impl Config {
    /// Creates default settings rooted at `config_dir`.
    #[must_use]
    pub fn for_dir(config_dir: impl Into<PathBuf>) -> Self {
        Self {
            playback: PlaybackConfig::default(),
            subscriptions: SubscriptionConfig::default(),
            ui: UiConfig::default(),
            persistence: PersistenceConfig::default(),
            providers: ProviderConfig::default(),
            root_dir: config_dir.into(),
        }
    }

    /// Loads configuration from the platform config directory.
    ///
    /// On Unix this is normally `~/.config/youta`; `XDG_CONFIG_HOME` is
    /// honored. `YOUTA_CONFIG_DIR` can select another root.
    ///
    /// # Errors
    ///
    /// Returns an error when TOML or environment values are invalid.
    pub fn load() -> Result<Self, ConfigError> {
        let config_dir =
            std::env::var_os(CONFIG_DIR_ENV).map_or_else(default_config_dir, PathBuf::from);
        Self::load_from_dir(config_dir)
    }

    /// Loads `config.toml` from an explicit application directory.
    ///
    /// `YOUTA_` variables still have higher precedence. The explicit directory
    /// remains the write root even when an unrelated environment key named
    /// `YOUTA_CONFIG_DIR` is present.
    ///
    /// # Errors
    ///
    /// Returns an error when TOML or environment values are invalid.
    pub fn load_from_dir(config_dir: impl Into<PathBuf>) -> Result<Self, ConfigError> {
        Self::load_from_dir_with_environment(config_dir.into(), true)
    }

    fn load_from_dir_with_environment(
        config_dir: PathBuf,
        include_environment: bool,
    ) -> Result<Self, ConfigError> {
        let defaults = Self::for_dir(&config_dir);
        let config_path = config_dir.join("config.toml");
        let mut figment = Figment::from(Serialized::defaults(defaults));
        if config_path.is_file() {
            figment = figment.merge(Toml::file_exact(config_path));
        }
        if include_environment {
            figment = figment.merge(Env::prefixed("YOUTA_").ignore(&["config_dir"]).split("__"));
        }

        let mut config: Self = figment.extract()?;
        config.root_dir = config_dir;
        config.validate()?;
        Ok(config)
    }

    /// Creates the application and download directories with private Unix
    /// permissions.
    ///
    /// Existing Unix directories are tightened to mode `0700`. The operation
    /// does not create or modify the user's TOML file.
    ///
    /// # Errors
    ///
    /// Returns an error if settings are invalid or a directory cannot be
    /// created or secured.
    pub fn ensure_directories(&self) -> Result<(), ConfigError> {
        self.validate()?;
        create_private_directory(self.config_dir())?;
        create_private_directory(&self.downloads_dir())?;
        Ok(())
    }

    /// Validates ranges that cannot be expressed by `Serde`.
    ///
    /// # Errors
    ///
    /// Returns an error describing the first invalid value.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.root_dir.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "the Youta config directory cannot be empty".to_owned(),
            ));
        }
        if self.playback.volume_percent > 100 {
            return Err(ConfigError::Invalid(
                "playback.volume_percent must be between 0 and 100".to_owned(),
            ));
        }
        if !(50..=300).contains(&self.playback.speed_percent) {
            return Err(ConfigError::Invalid(
                "playback.speed_percent must be between 50 and 300".to_owned(),
            ));
        }
        if !(1..=100).contains(&self.persistence.played_threshold_percent) {
            return Err(ConfigError::Invalid(
                "persistence.played_threshold_percent must be between 1 and 100".to_owned(),
            ));
        }
        if self.persistence.position_save_interval_seconds == 0 {
            return Err(ConfigError::Invalid(
                "persistence.position_save_interval_seconds must be positive".to_owned(),
            ));
        }
        if self.ui.thumbnail_height < MIN_THUMBNAIL_HEIGHT {
            return Err(ConfigError::Invalid(format!(
                "ui.thumbnail_height must be at least {MIN_THUMBNAIL_HEIGHT}"
            )));
        }
        Ok(())
    }

    /// Returns the root directory used for all Youta writes.
    #[must_use]
    pub fn config_dir(&self) -> &Path {
        &self.root_dir
    }

    /// Returns the TOML configuration path.
    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.root_dir.join("config.toml")
    }

    /// Returns the `SQLite` state path.
    #[must_use]
    pub fn database_file(&self) -> PathBuf {
        self.root_dir.join("state.sqlite3")
    }

    /// Returns the portable OPML subscription path.
    #[must_use]
    pub fn subscriptions_file(&self) -> PathBuf {
        self.root_dir.join("subscriptions.opml")
    }

    /// Returns the directory used for media explicitly downloaded by Youta.
    #[must_use]
    pub fn downloads_dir(&self) -> PathBuf {
        self.root_dir.join("downloads")
    }

    /// Returns the private persistent cache for validated thumbnail bytes.
    #[must_use]
    pub fn thumbnail_cache_dir(&self) -> PathBuf {
        self.root_dir.join("thumbnail-cache")
    }

    /// Persists one selected `YouTube` metadata provider in `config.toml`.
    ///
    /// Existing unrelated keys and comments are preserved. The alternative
    /// provider credential is retained so the user can switch back later.
    /// Environment variables still take precedence when configuration is
    /// loaded again.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is invalid, the existing file is too
    /// large or malformed, or an atomic private-file update fails.
    #[cfg(feature = "tui")]
    pub fn save_youtube_provider(
        &mut self,
        setting: YouTubeProviderSetting,
    ) -> Result<(), ConfigError> {
        enum ValidatedSetting {
            OfficialApiKey(String),
            InvidiousUrl(Url),
        }

        let setting = match setting {
            YouTubeProviderSetting::OfficialApiKey(api_key) => {
                ValidatedSetting::OfficialApiKey(validate_youtube_api_key(&api_key)?)
            }
            YouTubeProviderSetting::InvidiousUrl(url) => {
                ValidatedSetting::InvidiousUrl(validate_provider_url(url)?)
            }
        };
        let (backend, field, stored_value) = match &setting {
            ValidatedSetting::OfficialApiKey(api_key) => {
                (YouTubeBackend::Official, "youtube_api_key", api_key.clone())
            }
            ValidatedSetting::InvidiousUrl(url) => (
                YouTubeBackend::Invidious,
                "invidious_base_url",
                url.to_string(),
            ),
        };

        self.ensure_directories()?;
        let path = self.config_file();
        let mut document = read_editable_config(&path)?;
        let providers = document
            .as_table_mut()
            .entry("providers")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| {
                ConfigError::Invalid(
                    "`providers` must be a TOML table before Youta can update it".to_owned(),
                )
            })?;
        providers["youtube_backend"] = value(backend.as_config_value());
        providers[field] = value(&stored_value);
        write_private_config(&path, document.to_string().as_bytes())?;

        self.providers.youtube_backend = backend;
        match setting {
            ValidatedSetting::OfficialApiKey(api_key) => {
                self.providers.youtube_api_key = Some(api_key);
            }
            ValidatedSetting::InvidiousUrl(url) => {
                self.providers.invidious_base_url = Some(url);
            }
        }
        Ok(())
    }
}

/// Player and audio-output settings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PlaybackConfig {
    /// Playback implementation selected at runtime.
    pub backend: PlaybackBackend,
    /// Audio system selected at runtime.
    pub output: AudioOutput,
    /// Optional backend-specific output device.
    pub device: Option<String>,
    /// Initial volume in the inclusive range `0..=100`.
    pub volume_percent: u8,
    /// Playback speed as an integer percentage in `50..=300`.
    pub speed_percent: u16,
    /// Context rewound when an interrupted stream is resumed.
    pub resume_rewind_seconds: u64,
    /// Settings that favor stable direct-device playback.
    pub audiophile: AudiophileConfig,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self {
            backend: PlaybackBackend::Mpv,
            output: AudioOutput::Auto,
            device: None,
            volume_percent: 80,
            speed_percent: 100,
            resume_rewind_seconds: DEFAULT_RESUME_REWIND_SECONDS,
            audiophile: AudiophileConfig::default(),
        }
    }
}

/// Playback engines compiled into different Youta builds.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlaybackBackend {
    /// Control an external `mpv` process through its IPC protocol.
    #[default]
    Mpv,
    /// Use Youta's optional in-process decoder and output pipeline.
    Native,
}

/// Audio output systems supported by playback backends.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AudioOutput {
    /// Let the playback backend select the platform default.
    #[default]
    Auto,
    /// Direct `ALSA` output without requiring `PulseAudio` or `PipeWire`.
    Alsa,
    /// `JACK` output.
    Jack,
    /// `PulseAudio` output.
    PulseAudio,
    /// `PipeWire` output.
    PipeWire,
}

/// Conservative options for direct, stable audio output.
///
/// Youta does not change CPU governors or kernel settings. Those are
/// system-wide administrative choices and are better applied explicitly
/// outside the media player.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct AudiophileConfig {
    /// Enables the selected backend's low-jitter buffer profile.
    pub enabled: bool,
    /// Requests exclusive access where the backend and audio system support it.
    pub exclusive_device: bool,
    /// Avoids software resampling when the device supports the source rate.
    pub avoid_resampling: bool,
    /// Optional fixed output sample rate when resampling is intentional.
    pub output_sample_rate_hz: Option<u32>,
}

/// Automatic subscription download settings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct SubscriptionConfig {
    /// Download new items while Youta is open.
    pub auto_download: bool,
    /// Preferred downloaded audio container or codec.
    pub audio_format: String,
    /// Download thumbnails alongside audio.
    pub download_thumbnails: bool,
}

impl Default for SubscriptionConfig {
    fn default() -> Self {
        Self {
            auto_download: true,
            audio_format: "opus".to_owned(),
            download_thumbnails: true,
        }
    }
}

/// Terminal rendering preferences.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct UiConfig {
    /// Show hotkey labels inside clickable controls.
    pub show_button_hotkeys: bool,
    /// Terminal theme selection.
    pub theme: ThemeMode,
    /// Preferred thumbnail behavior.
    pub thumbnails: ThumbnailMode,
    /// Maximum thumbnail height in terminal rows.
    pub thumbnail_height: u16,
    /// Seek-bar foreground color name or terminal palette index.
    pub seekbar_color: String,
    /// Replace the standard seek indicator with Nyan Cat.
    pub nyan_cat_seekbar: bool,
    /// Enable the decorative DOS-RPG layout.
    pub dos_rpg_mode: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            show_button_hotkeys: true,
            theme: ThemeMode::Auto,
            thumbnails: ThumbnailMode::Auto,
            thumbnail_height: DEFAULT_THUMBNAIL_HEIGHT,
            seekbar_color: "cyan".to_owned(),
            nyan_cat_seekbar: false,
            dos_rpg_mode: false,
        }
    }
}

/// How the UI chooses colors for light and dark terminals.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeMode {
    /// Detect terminal color preferences when possible and use a neutral
    /// fallback otherwise.
    #[default]
    Auto,
    /// Colors designed for a dark background.
    Dark,
    /// Colors designed for a light background.
    Light,
}

/// Whether artwork is attempted in terminal protocols that support it.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThumbnailMode {
    /// Show artwork only when a graphics-capable terminal protocol is detected.
    #[default]
    Auto,
    /// Never request or render artwork, including on a plain TTY.
    Off,
    /// Attempt artwork and fall back cleanly when unsupported.
    On,
}

/// Restart-safe local state settings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct PersistenceConfig {
    /// Interval between durable position updates.
    pub position_save_interval_seconds: u64,
    /// Completion threshold used by configurable UI and import code.
    pub played_threshold_percent: u8,
    /// Automatically commit local state when the config directory is a Git
    /// working tree. Push behavior is implemented outside the state store.
    pub git_commit_on_change: bool,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            position_save_interval_seconds: 30,
            played_threshold_percent: PLAYED_THRESHOLD_PERCENT,
            git_commit_on_change: false,
        }
    }
}

/// Configurable provider instances and external helper paths.
///
/// Optional API keys are plain strings at this layer. A keyring adapter can
/// resolve references before constructing provider clients without changing
/// those clients' configuration.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Permit provider endpoints that use unencrypted HTTP.
    ///
    /// This defaults to `true` because Mirsoft currently requires HTTP. Disable
    /// it when that source is not used; credentials must never be sent to an
    /// unencrypted endpoint.
    pub allow_insecure_http: bool,
    /// Metadata backend used for `YouTube` search and public details.
    pub youtube_backend: YouTubeBackend,
    /// Optional Invidious instance base URL.
    pub invidious_base_url: Option<Url>,
    /// Optional default `PeerTube` instance base URL.
    pub peertube_instance_url: Option<Url>,
    /// Optional default `Funkwhale` instance base URL.
    pub funkwhale_instance_url: Option<Url>,
    /// Optional `YouTube` Data API credential.
    pub youtube_api_key: Option<String>,
    /// Optional Mod Archive API credential or externally resolved reference.
    pub mod_archive_api_key: Option<String>,
    /// Optional client ID issued for the user's Jamendo application.
    ///
    /// Youta does not bundle Jamendo's public documentation/testing client ID.
    pub jamendo_client_id: Option<String>,
    /// `yt-dlp` executable name or path.
    pub yt_dlp_executable: PathBuf,
    /// `mpv` executable name or path.
    pub mpv_executable: PathBuf,
}

impl fmt::Debug for ProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderConfig")
            .field("allow_insecure_http", &self.allow_insecure_http)
            .field("youtube_backend", &self.youtube_backend)
            .field("invidious_base_url", &self.invidious_base_url)
            .field("peertube_instance_url", &self.peertube_instance_url)
            .field("funkwhale_instance_url", &self.funkwhale_instance_url)
            .field(
                "youtube_api_key",
                &self.youtube_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "mod_archive_api_key",
                &self.mod_archive_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "jamendo_client_id",
                &self.jamendo_client_id.as_ref().map(|_| "[CONFIGURED]"),
            )
            .field("yt_dlp_executable", &self.yt_dlp_executable)
            .field("mpv_executable", &self.mpv_executable)
            .finish()
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            allow_insecure_http: true,
            youtube_backend: YouTubeBackend::Auto,
            invidious_base_url: None,
            peertube_instance_url: None,
            funkwhale_instance_url: None,
            youtube_api_key: None,
            mod_archive_api_key: None,
            jamendo_client_id: None,
            yt_dlp_executable: PathBuf::from("yt-dlp"),
            mpv_executable: PathBuf::from("mpv"),
        }
    }
}

/// Metadata backend used for public `YouTube` discovery.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum YouTubeBackend {
    /// Prefer an API key when configured, then fall back to Invidious.
    #[default]
    Auto,
    /// Use the official `YouTube` Data API v3.
    Official,
    /// Use the configured Invidious instance.
    Invidious,
}

impl YouTubeBackend {
    const fn as_config_value(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Official => "official",
            Self::Invidious => "invidious",
        }
    }
}

impl fmt::Display for YouTubeBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_config_value())
    }
}

/// A validated-on-save `YouTube` provider choice entered in the TUI.
///
/// This type intentionally omits `Debug` so an API key cannot be printed by a
/// derived formatter.
pub enum YouTubeProviderSetting {
    /// Select the official API and store its key in the private TOML file.
    OfficialApiKey(String),
    /// Select and store a credential-free Invidious base URL.
    InvidiousUrl(Url),
}

/// Errors produced while loading or preparing configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A TOML or environment value did not deserialize.
    #[error("invalid Youta configuration: {0}")]
    Figment(#[source] Box<figment::Error>),
    /// A configuration directory could not be created or secured.
    #[error("cannot prepare the Youta application directory: {0}")]
    Io(#[from] std::io::Error),
    /// A value is syntactically valid but outside Youta's accepted range.
    #[error("invalid Youta configuration: {0}")]
    Invalid(String),
}

impl From<figment::Error> for ConfigError {
    fn from(error: figment::Error) -> Self {
        Self::Figment(Box::new(error))
    }
}

fn default_config_dir() -> PathBuf {
    BaseDirs::new().map_or_else(
        || PathBuf::from(".config").join("youta"),
        |directories| directories.config_dir().join("youta"),
    )
}

fn create_private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(feature = "tui")]
fn validate_youtube_api_key(api_key: &str) -> Result<String, ConfigError> {
    let api_key = api_key.trim();
    if !(16..=256).contains(&api_key.len())
        || !api_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ConfigError::Invalid(
            "the YouTube API key must contain 16 to 256 URL-safe characters".to_owned(),
        ));
    }
    Ok(api_key.to_owned())
}

#[cfg(feature = "tui")]
fn validate_provider_url(mut url: Url) -> Result<Url, ConfigError> {
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.cannot_be_a_base()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigError::Invalid(
            "the Invidious instance must be a credential-free HTTP(S) base URL".to_owned(),
        ));
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

#[cfg(feature = "tui")]
fn read_editable_config(path: &Path) -> Result<DocumentMut, ConfigError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DocumentMut::new());
        }
        Err(error) => return Err(error.into()),
    };
    let length = file.metadata()?.len();
    if length > MAX_CONFIG_BYTES {
        return Err(ConfigError::Invalid(format!(
            "config.toml exceeds the {MAX_CONFIG_BYTES}-byte update limit"
        )));
    }
    let capacity = usize::try_from(length).unwrap_or_default();
    let mut contents = String::with_capacity(capacity);
    file.take(MAX_CONFIG_BYTES.saturating_add(1))
        .read_to_string(&mut contents)?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_CONFIG_BYTES {
        return Err(ConfigError::Invalid(format!(
            "config.toml exceeds the {MAX_CONFIG_BYTES}-byte update limit"
        )));
    }
    contents.parse().map_err(|_| {
        ConfigError::Invalid(
            "config.toml is not valid TOML; fix it before saving a provider from Youta".to_owned(),
        )
    })
}

#[cfg(feature = "tui")]
fn write_private_config(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    let temporary = path.with_extension(format!("toml.{}.tmp", std::process::id()));
    let result = (|| -> Result<(), ConfigError> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        set_private_file_permissions(&temporary)?;
        fs::rename(&temporary, path)?;
        set_private_file_permissions(path)?;
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(feature = "tui")]
fn set_private_file_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn defaults_are_low_resource_and_paths_are_confined() {
        let root = PathBuf::from("/tmp/youta-config-test");
        let config = Config::for_dir(&root);
        assert!(config.subscriptions.auto_download);
        assert_eq!(config.subscriptions.audio_format, "opus");
        assert_eq!(config.playback.resume_rewind_seconds, 30);
        assert_eq!(config.persistence.position_save_interval_seconds, 30);
        assert_eq!(config.ui.thumbnail_height, DEFAULT_THUMBNAIL_HEIGHT);
        assert!(config.providers.allow_insecure_http);
        assert_eq!(config.providers.youtube_backend, YouTubeBackend::Auto);
        assert!(config.providers.jamendo_client_id.is_none());
        assert_eq!(config.database_file(), root.join("state.sqlite3"));
        assert_eq!(config.downloads_dir(), root.join("downloads"));
        assert_eq!(config.thumbnail_cache_dir(), root.join("thumbnail-cache"));
        assert!(config.database_file().starts_with(config.config_dir()));
        assert!(config.subscriptions_file().starts_with(config.config_dir()));
    }

    #[test]
    fn toml_overrides_defaults() {
        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("config.toml"),
            r#"
[playback]
volume_percent = 35
speed_percent = 120

[subscriptions]
auto_download = false

[ui]
theme = "light"
thumbnail_height = 14
"#,
        )
        .expect("write test TOML");

        let config = Config::load_from_dir_with_environment(directory.path().to_owned(), false)
            .expect("load TOML");
        assert_eq!(config.playback.volume_percent, 35);
        assert_eq!(config.playback.speed_percent, 120);
        assert!(!config.subscriptions.auto_download);
        assert_eq!(config.ui.theme, ThemeMode::Light);
        assert_eq!(config.ui.thumbnail_height, 14);
        assert_eq!(config.config_dir(), directory.path());
    }

    #[test]
    fn environment_overrides_toml_in_child_process() {
        const CHILD_MARKER: &str = "YOUTA_CONFIG_TEST_CHILD";
        if std::env::var_os(CHILD_MARKER).is_some() {
            let config = Config::load().expect("load child configuration");
            assert_eq!(config.playback.volume_percent, 62);
            assert!(config.ui.nyan_cat_seekbar);
            assert_eq!(config.ui.thumbnail_height, 16);
            return;
        }

        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("config.toml"),
            "[playback]\nvolume_percent = 20\n",
        )
        .expect("write test TOML");
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "config::tests::environment_overrides_toml_in_child_process",
                "--nocapture",
            ])
            .env(CHILD_MARKER, "1")
            .env(CONFIG_DIR_ENV, directory.path())
            .env("YOUTA_PLAYBACK__VOLUME_PERCENT", "62")
            .env("YOUTA_UI__NYAN_CAT_SEEKBAR", "true")
            .env("YOUTA_UI__THUMBNAIL_HEIGHT", "16")
            .output()
            .expect("run environment test child");
        assert!(
            output.status.success(),
            "child test failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn invalid_ranges_are_rejected() {
        let mut config = Config::default();
        config.playback.speed_percent = 40;
        assert!(matches!(config.validate(), Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn thumbnail_height_below_renderer_minimum_is_rejected() {
        let mut config = Config::default();
        config.ui.thumbnail_height = MIN_THUMBNAIL_HEIGHT.saturating_sub(1);

        assert!(matches!(
            config.validate(),
            Err(ConfigError::Invalid(message))
                if message == "ui.thumbnail_height must be at least 4"
        ));
    }

    #[test]
    fn provider_debug_output_redacts_all_configured_credentials() {
        let mut providers = ProviderConfig {
            youtube_api_key: Some("youtube-secret-canary".to_owned()),
            mod_archive_api_key: Some("mod-secret-canary".to_owned()),
            jamendo_client_id: Some("jamendo-client-canary".to_owned()),
            ..ProviderConfig::default()
        };
        let rendered = format!("{providers:?}");
        for secret in [
            "youtube-secret-canary",
            "mod-secret-canary",
            "jamendo-client-canary",
        ] {
            assert!(!rendered.contains(secret));
        }
        assert!(rendered.contains("[REDACTED]"));
        assert!(rendered.contains("[CONFIGURED]"));

        providers.youtube_api_key = None;
        assert!(!format!("{providers:?}").contains("youtube-secret-canary"));
    }

    #[test]
    fn directories_are_private_and_nested() {
        let directory = tempdir().expect("temporary directory");
        let root = directory.path().join("youta");
        let config = Config::for_dir(&root);
        config
            .ensure_directories()
            .expect("create application folders");
        assert!(config.downloads_dir().is_dir());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(&root)
                .expect("root metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
            let mode = fs::metadata(config.downloads_dir())
                .expect("downloads metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[cfg(feature = "tui")]
    #[test]
    fn youtube_provider_save_preserves_unrelated_toml_and_switchable_credentials() {
        let directory = tempdir().expect("temporary directory");
        let root = directory.path().join("youta");
        fs::create_dir(&root).expect("config directory");
        fs::write(
            root.join("config.toml"),
            "# keep this comment\n[playback]\nvolume_percent = 35\n",
        )
        .expect("initial config");
        let mut config = Config::load_from_dir_with_environment(root.clone(), false)
            .expect("load initial config");
        let api_key = "AIzaSyFixture_key_123456789012345678";

        config
            .save_youtube_provider(YouTubeProviderSetting::OfficialApiKey(api_key.to_owned()))
            .expect("save API key");
        let after_key = fs::read_to_string(config.config_file()).expect("saved config");
        assert!(after_key.contains("# keep this comment"));
        assert!(after_key.contains("volume_percent = 35"));
        assert!(after_key.contains("youtube_backend = \"official\""));
        assert!(after_key.contains(api_key));
        assert_eq!(config.providers.youtube_backend, YouTubeBackend::Official);
        assert_eq!(config.providers.youtube_api_key.as_deref(), Some(api_key));

        let invidious = Url::parse("https://inv.example.test/api").expect("fixture URL");
        config
            .save_youtube_provider(YouTubeProviderSetting::InvidiousUrl(invidious))
            .expect("save Invidious URL");
        let after_url = fs::read_to_string(config.config_file()).expect("updated config");
        assert!(after_url.contains("# keep this comment"));
        assert!(
            after_url.contains(api_key),
            "switching must retain the API key"
        );
        assert!(after_url.contains("youtube_backend = \"invidious\""));
        assert!(after_url.contains("https://inv.example.test/api/"));

        let restored =
            Config::load_from_dir_with_environment(root, false).expect("reload updated config");
        assert_eq!(
            restored.providers.youtube_backend,
            YouTubeBackend::Invidious
        );
        assert_eq!(
            restored
                .providers
                .invidious_base_url
                .expect("saved URL")
                .as_str(),
            "https://inv.example.test/api/"
        );
        assert_eq!(restored.playback.volume_percent, 35);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn youtube_provider_save_rejects_unsafe_values_without_creating_a_file() {
        let directory = tempdir().expect("temporary directory");
        let root = directory.path().join("youta");
        let mut config = Config::for_dir(&root);

        assert!(matches!(
            config.save_youtube_provider(YouTubeProviderSetting::OfficialApiKey(
                "short key".to_owned()
            )),
            Err(ConfigError::Invalid(_))
        ));
        assert!(!config.config_file().exists());

        let credentialed =
            Url::parse("https://user:secret@inv.example.test/").expect("fixture URL");
        assert!(matches!(
            config.save_youtube_provider(YouTubeProviderSetting::InvidiousUrl(credentialed)),
            Err(ConfigError::Invalid(_))
        ));
        assert!(!config.config_file().exists());

        let queried = Url::parse("https://inv.example.test/?token=secret").expect("fixture URL");
        assert!(matches!(
            config.save_youtube_provider(YouTubeProviderSetting::InvidiousUrl(queried)),
            Err(ConfigError::Invalid(_))
        ));
        assert!(!config.config_file().exists());
    }

    #[cfg(feature = "tui")]
    #[test]
    fn youtube_provider_save_refuses_malformed_or_oversized_existing_config() {
        let directory = tempdir().expect("temporary directory");
        let root = directory.path().join("youta");
        fs::create_dir(&root).expect("config directory");
        let path = root.join("config.toml");
        fs::write(&path, "[providers\nbroken = true\n").expect("malformed config");
        let original = fs::read(&path).expect("original config");
        let mut config = Config::for_dir(&root);

        assert!(matches!(
            config.save_youtube_provider(YouTubeProviderSetting::OfficialApiKey(
                "AIzaSyFixture_key_123456789012345678".to_owned()
            )),
            Err(ConfigError::Invalid(_))
        ));
        assert_eq!(fs::read(&path).expect("unchanged config"), original);

        fs::write(
            &path,
            vec![b'#'; usize::try_from(MAX_CONFIG_BYTES + 1).expect("test size")],
        )
        .expect("oversized config");
        assert!(matches!(
            config.save_youtube_provider(YouTubeProviderSetting::OfficialApiKey(
                "AIzaSyFixture_key_123456789012345678".to_owned()
            )),
            Err(ConfigError::Invalid(_))
        ));
    }

    #[cfg(all(unix, feature = "tui"))]
    #[test]
    fn youtube_provider_save_uses_private_file_and_directory_modes() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempdir().expect("temporary directory");
        let root = directory.path().join("youta");
        let mut config = Config::for_dir(&root);
        config
            .save_youtube_provider(YouTubeProviderSetting::OfficialApiKey(
                "AIzaSyFixture_key_123456789012345678".to_owned(),
            ))
            .expect("save provider");

        let directory_mode = fs::metadata(&root)
            .expect("config directory metadata")
            .permissions()
            .mode()
            & 0o777;
        let file_mode = fs::metadata(config.config_file())
            .expect("config file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }
}
