//! Layered application configuration and confined application paths.
//!
//! Configuration is loaded in this precedence order:
//!
//! 1. low-resource defaults;
//! 2. `<config-dir>/config.toml`, when it exists;
//! 3. `<config-dir>/secrets/credentials.toml`, when it exists;
//! 4. environment variables prefixed with `YOUTA_`.
//!
//! Double underscores express nesting, so
//! `YOUTA_PLAYBACK__VOLUME_PERCENT=40` overrides
//! `playback.volume_percent`. `YOUTA_CONFIG_DIR` selects the directory before
//! the TOML file is read. Every path Youta writes is derived from that one
//! application directory.

use std::fmt;
use std::fs;
use std::fs::OpenOptions;
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

/// Environment variable that overrides the Subscriptions screen layout.
pub const SUBSCRIPTIONS_LAYOUT_ENV: &str = "YOUTA_UI__SUBSCRIPTIONS_LAYOUT";

/// Environment variable that overrides automatic advertisement-chapter skipping.
pub const SKIP_ADVERTISEMENT_CHAPTERS_ENV: &str = "YOUTA_PLAYBACK__SKIP_ADVERTISEMENT_CHAPTERS";

/// Environment variable that overrides automatic same-source queue continuation.
pub const AUTOPLAY_ENV: &str = "YOUTA_PLAYBACK__AUTOPLAY";

/// Environment variable that overrides selected and imminent-next `YouTube` prewarming.
pub const YOUTUBE_PREWARM_ENV: &str = "YOUTA_PLAYBACK__YOUTUBE_PREWARM";

/// Environment variable that overrides lazy Local-folder size measurement.
pub const LOCAL_FOLDER_SIZES_ENV: &str = "YOUTA_UI__SHOW_LOCAL_FOLDER_SIZES";

/// Environment variable that overrides artwork on a physical Linux TTY.
pub const TTY_IMAGES_ENV: &str = "YOUTA_UI__SHOW_IMAGES_IN_TTY";

/// Environment variable that overrides the preferred Bandcamp audio format.
pub const BANDCAMP_AUDIO_FORMAT_ENV: &str = "YOUTA_PROVIDERS__BANDCAMP_AUDIO_FORMAT";

/// Environment variable that overrides the selected `YouTube` metadata backend.
pub const YOUTUBE_BACKEND_ENV: &str = "YOUTA_PROVIDERS__YOUTUBE_BACKEND";

/// Environment variable that overrides the official `YouTube` Data API key.
pub const YOUTUBE_API_KEY_ENV: &str = "YOUTA_PROVIDERS__YOUTUBE_API_KEY";

/// Environment variable that overrides the configured Invidious base URL.
pub const INVIDIOUS_BASE_URL_ENV: &str = "YOUTA_PROVIDERS__INVIDIOUS_BASE_URL";

/// Default maximum thumbnail height in terminal rows.
pub const DEFAULT_THUMBNAIL_HEIGHT: u16 = 20;

/// Smallest thumbnail height that the terminal renderer can use.
pub const MIN_THUMBNAIL_HEIGHT: u16 = 4;

#[cfg(feature = "tui")]
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_CREDENTIALS_BYTES: u64 = 1024 * 1024;
const YOUTA_GITIGNORE_BEGIN: &str = "# BEGIN YOUTA PRIVATE AND GENERATED FILES";
const YOUTA_GITIGNORE_END: &str = "# END YOUTA PRIVATE AND GENERATED FILES";
const YOUTA_GITIGNORE_RULES: &[&str] = &[
    "/secrets/",
    "/cache/",
    "/runtime/",
    "/thumbnail-cache/",
    "/downloads/",
    "/state.sqlite3*",
    "/state/.lock",
    "/state/**/*.tmp",
];

/// Plain-text credentials accepted from the private credentials file.
///
/// This mirrors only secret-bearing provider fields. Keeping the schema
/// separate prevents `secrets/credentials.toml` from silently overriding
/// ordinary Git-friendly preferences.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CredentialsFile {
    providers: ProviderCredentials,
}

/// Provider credentials that may be loaded from the private credentials file.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProviderCredentials {
    youtube_api_key: Option<String>,
    mod_archive_api_key: Option<String>,
    jamendo_client_id: Option<String>,
}

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
    /// Network provider endpoints, loaded credentials, and helper executables.
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
        let credentials_path = config_dir.join("secrets").join("credentials.toml");
        let mut figment = Figment::from(Serialized::defaults(defaults));
        if config_path.is_file() {
            figment = figment.merge(Toml::file_exact(config_path));
        }
        if credentials_path.is_file() {
            validate_credentials_file(&credentials_path)?;
            figment = figment.merge(Toml::file_exact(credentials_path));
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
    /// Existing Unix directories are tightened to mode `0700`. A conservative
    /// default `.gitignore` is created only when none exists; an existing file
    /// remains entirely under user control. The operation does not create or
    /// modify the user's TOML configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if settings are invalid or a directory cannot be
    /// created or secured.
    pub fn ensure_directories(&self) -> Result<(), ConfigError> {
        self.validate()?;
        create_private_directory(self.config_dir())?;
        create_private_directory(&self.secrets_dir())?;
        create_private_directory(&self.downloads_dir())?;
        ensure_youta_gitignore(&self.gitignore_file())?;
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
        if let Some(api_key) = self.providers.youtube_api_key.as_deref() {
            validate_youtube_api_key("providers.youtube_api_key", api_key)?;
        }
        for (field, credential) in [
            (
                "providers.mod_archive_api_key",
                self.providers.mod_archive_api_key.as_deref(),
            ),
            (
                "providers.jamendo_client_id",
                self.providers.jamendo_client_id.as_deref(),
            ),
        ] {
            if let Some(credential) = credential {
                validate_generic_credential(field, credential)?;
            }
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

    /// Returns the directory containing plaintext credentials excluded by
    /// Youta's default Git ignore rules.
    #[must_use]
    pub fn secrets_dir(&self) -> PathBuf {
        self.root_dir.join("secrets")
    }

    /// Returns the private TOML credentials path.
    #[must_use]
    pub fn credentials_file(&self) -> PathBuf {
        self.secrets_dir().join("credentials.toml")
    }

    /// Returns the Git ignore file protecting generated and secret state by
    /// default.
    #[must_use]
    pub fn gitignore_file(&self) -> PathBuf {
        self.root_dir.join(".gitignore")
    }

    /// Returns the `SQLite` state path.
    #[must_use]
    pub fn database_file(&self) -> PathBuf {
        self.root_dir.join("state.sqlite3")
    }

    /// Returns the human-readable authoritative-state directory.
    #[must_use]
    pub fn state_dir(&self) -> PathBuf {
        self.root_dir.join("state")
    }

    /// Returns the restart-only runtime-state directory.
    #[must_use]
    pub fn runtime_dir(&self) -> PathBuf {
        self.root_dir.join("runtime")
    }

    /// Returns the regenerable provider-cache directory.
    #[must_use]
    pub fn cache_dir(&self) -> PathBuf {
        self.root_dir.join("cache")
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
    /// Youta refuses to write the selection while [`YOUTUBE_BACKEND_ENV`] or
    /// the selected provider's value-specific environment variable is present,
    /// because that override would shadow the saved setting on the next start.
    ///
    /// # Errors
    ///
    /// Returns an error when a relevant environment override is active, the
    /// value is invalid, the existing file is too large or malformed, or an
    /// atomic private-file update fails.
    #[cfg(feature = "tui")]
    pub fn save_youtube_provider(
        &mut self,
        setting: YouTubeProviderSetting,
    ) -> Result<(), ConfigError> {
        let value_override = match &setting {
            YouTubeProviderSetting::OfficialApiKey(_) => YOUTUBE_API_KEY_ENV,
            YouTubeProviderSetting::InvidiousUrl(_) => INVIDIOUS_BASE_URL_ENV,
        };
        for variable in [YOUTUBE_BACKEND_ENV, value_override] {
            if std::env::var_os(variable).is_some() {
                return Err(ConfigError::Invalid(format!(
                    "{variable} overrides the saved YouTube provider setting; change or remove it before saving"
                )));
            }
        }

        enum ValidatedSetting {
            OfficialApiKey(String),
            InvidiousUrl(Url),
        }

        let setting = match setting {
            YouTubeProviderSetting::OfficialApiKey(api_key) => ValidatedSetting::OfficialApiKey(
                validate_youtube_api_key("providers.youtube_api_key", &api_key)?,
            ),
            YouTubeProviderSetting::InvidiousUrl(url) => {
                ValidatedSetting::InvidiousUrl(validate_provider_url(url)?)
            }
        };
        let backend = match &setting {
            ValidatedSetting::OfficialApiKey(_) => YouTubeBackend::Official,
            ValidatedSetting::InvidiousUrl(_) => YouTubeBackend::Invidious,
        };

        self.ensure_directories()?;
        let config_path = self.config_file();
        let credentials_path = self.credentials_file();
        let mut config_document = read_editable_config(&config_path)?;
        let mut credentials_document = read_editable_credentials(&credentials_path)?;

        migrate_legacy_provider_credentials(&mut config_document, &mut credentials_document)?;

        {
            let providers = editable_table(&mut config_document, "providers")?;
            providers["youtube_backend"] = value(backend.as_config_value());
            if let ValidatedSetting::InvidiousUrl(url) = &setting {
                providers["invidious_base_url"] = value(url.to_string());
            }
            providers.remove("youtube_api_key");
        }
        if let ValidatedSetting::OfficialApiKey(api_key) = &setting {
            let providers = editable_table(&mut credentials_document, "providers")?;
            providers["youtube_api_key"] = value(api_key);
        }

        if !credentials_document.as_table().is_empty() {
            write_private_config(
                &credentials_path,
                credentials_document.to_string().as_bytes(),
            )?;
        }
        write_private_config(&config_path, config_document.to_string().as_bytes())?;

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

    /// Persists the selected Subscriptions screen layout in `config.toml`.
    ///
    /// Existing unrelated keys, comments, and credentials are preserved.
    /// Youta refuses to write a value while
    /// [`SUBSCRIPTIONS_LAYOUT_ENV`] is present because that environment value
    /// would shadow the saved preference on the next start.
    ///
    /// # Errors
    ///
    /// Returns an error when an environment override is active, the existing
    /// file is too large or malformed, or an atomic private-file update fails.
    #[cfg(feature = "tui")]
    pub fn save_subscriptions_layout(
        &mut self,
        layout: SubscriptionsLayout,
    ) -> Result<(), ConfigError> {
        if std::env::var_os(SUBSCRIPTIONS_LAYOUT_ENV).is_some() {
            return Err(ConfigError::Invalid(format!(
                "{SUBSCRIPTIONS_LAYOUT_ENV} overrides config.toml; change or remove it before saving this preference"
            )));
        }

        self.ensure_directories()?;
        let path = self.config_file();
        let mut document = read_editable_config(&path)?;
        let ui = document
            .as_table_mut()
            .entry("ui")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| {
                ConfigError::Invalid(
                    "`ui` must be a TOML table before Youta can update it".to_owned(),
                )
            })?;
        ui["subscriptions_layout"] = value(layout.as_config_value());
        write_private_config(&path, document.to_string().as_bytes())?;
        self.ui.subscriptions_layout = layout;
        Ok(())
    }

    /// Persists the preferences currently exposed by the in-app editor.
    ///
    /// The Subscriptions layout, advertisement-chapter behavior, selected
    /// YouTube-video prewarming, lazy Local-folder size preference, and
    /// physical-TTY image preference are written together so confirming the
    /// popup cannot save only part of the draft.
    /// Existing unrelated keys, comments, and credentials are preserved.
    /// [`SUBSCRIPTIONS_LAYOUT_ENV`] and
    /// [`SKIP_ADVERTISEMENT_CHAPTERS_ENV`] and
    /// [`YOUTUBE_PREWARM_ENV`] and
    /// [`LOCAL_FOLDER_SIZES_ENV`] and [`TTY_IMAGES_ENV`] retain precedence and
    /// therefore prevent this writer from storing a shadowed draft.
    ///
    /// The layout-only [`Self::save_subscriptions_layout`] method remains
    /// available for callers that do not edit the playback preference.
    ///
    /// # Errors
    ///
    /// Returns an error when any relevant environment override is active, the
    /// existing file is too large or malformed, or an atomic private-file
    /// update fails.
    #[cfg(feature = "tui")]
    pub fn save_tui_preferences(
        &mut self,
        layout: SubscriptionsLayout,
        skip_advertisement_chapters: bool,
        youtube_prewarm: bool,
        show_local_folder_sizes: bool,
        show_images_in_tty: bool,
    ) -> Result<(), ConfigError> {
        for variable in [
            SUBSCRIPTIONS_LAYOUT_ENV,
            SKIP_ADVERTISEMENT_CHAPTERS_ENV,
            YOUTUBE_PREWARM_ENV,
            LOCAL_FOLDER_SIZES_ENV,
            TTY_IMAGES_ENV,
        ]
        .into_iter()
        .filter(|variable| cfg!(feature = "images") || *variable != TTY_IMAGES_ENV)
        {
            if std::env::var_os(variable).is_some() {
                return Err(ConfigError::Invalid(format!(
                    "{variable} overrides config.toml; change or remove it before saving these preferences"
                )));
            }
        }

        self.ensure_directories()?;
        let path = self.config_file();
        let mut document = read_editable_config(&path)?;
        {
            let ui = document
                .as_table_mut()
                .entry("ui")
                .or_insert_with(|| Item::Table(Table::new()))
                .as_table_mut()
                .ok_or_else(|| {
                    ConfigError::Invalid(
                        "`ui` must be a TOML table before Youta can update it".to_owned(),
                    )
                })?;
            ui["subscriptions_layout"] = value(layout.as_config_value());
            ui["show_local_folder_sizes"] = value(show_local_folder_sizes);
            #[cfg(feature = "images")]
            {
                ui["show_images_in_tty"] = value(show_images_in_tty);
            }
        }
        {
            let playback = document
                .as_table_mut()
                .entry("playback")
                .or_insert_with(|| Item::Table(Table::new()))
                .as_table_mut()
                .ok_or_else(|| {
                    ConfigError::Invalid(
                        "`playback` must be a TOML table before Youta can update it".to_owned(),
                    )
                })?;
            playback["skip_advertisement_chapters"] = value(skip_advertisement_chapters);
            playback["youtube_prewarm"] = value(youtube_prewarm);
        }
        write_private_config(&path, document.to_string().as_bytes())?;

        self.ui.subscriptions_layout = layout;
        self.ui.show_local_folder_sizes = show_local_folder_sizes;
        #[cfg(feature = "images")]
        {
            self.ui.show_images_in_tty = show_images_in_tty;
        }
        #[cfg(not(feature = "images"))]
        let _ = show_images_in_tty;
        self.playback.skip_advertisement_chapters = skip_advertisement_chapters;
        self.playback.youtube_prewarm = youtube_prewarm;
        Ok(())
    }

    /// Persists automatic same-source playback in `config.toml`.
    ///
    /// Existing unrelated settings, comments, and credentials are preserved.
    /// [`AUTOPLAY_ENV`] retains precedence and therefore prevents this writer
    /// from storing a value that the environment would immediately shadow.
    ///
    /// # Errors
    ///
    /// Returns an error when the environment override is active, the existing
    /// file is too large or malformed, or the private atomic update fails.
    #[cfg(feature = "tui")]
    pub fn save_autoplay(&mut self, autoplay: bool) -> Result<(), ConfigError> {
        if std::env::var_os(AUTOPLAY_ENV).is_some() {
            return Err(ConfigError::Invalid(format!(
                "{AUTOPLAY_ENV} overrides config.toml; change or remove it before toggling autoplay"
            )));
        }

        self.ensure_directories()?;
        let path = self.config_file();
        let mut document = read_editable_config(&path)?;
        let playback = document
            .as_table_mut()
            .entry("playback")
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_mut()
            .ok_or_else(|| {
                ConfigError::Invalid(
                    "`playback` must be a TOML table before Youta can update it".to_owned(),
                )
            })?;
        playback["autoplay"] = value(autoplay);
        write_private_config(&path, document.to_string().as_bytes())?;
        self.playback.autoplay = autoplay;
        Ok(())
    }

    /// Persists the selected Bandcamp audio format in `config.toml`.
    ///
    /// Existing unrelated settings and comments are preserved.
    /// [`BANDCAMP_AUDIO_FORMAT_ENV`] retains precedence and therefore prevents
    /// this writer from storing a value that would immediately be shadowed.
    ///
    /// # Errors
    ///
    /// Returns an error when the environment override is active, the existing
    /// file is too large or malformed, or the private atomic update fails.
    #[cfg(feature = "tui")]
    pub fn save_bandcamp_audio_format(
        &mut self,
        format: BandcampAudioFormat,
    ) -> Result<(), ConfigError> {
        if std::env::var_os(BANDCAMP_AUDIO_FORMAT_ENV).is_some() {
            return Err(ConfigError::Invalid(format!(
                "{BANDCAMP_AUDIO_FORMAT_ENV} overrides config.toml; change or remove it before saving this preference"
            )));
        }

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
        providers["bandcamp_audio_format"] = value(format.as_config_value());
        write_private_config(&path, document.to_string().as_bytes())?;
        self.providers.bandcamp_audio_format = format;
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
    /// Continue with the next playable entry from the active source list.
    pub autoplay: bool,
    /// Resolve a selected or imminent-next `YouTube` video briefly in RAM.
    pub youtube_prewarm: bool,
    /// Hide and skip chapters whose normalized title is exactly `Реклама`.
    pub skip_advertisement_chapters: bool,
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
            autoplay: false,
            youtube_prewarm: true,
            skip_advertisement_chapters: true,
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
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent stable TOML switches are clearer as booleans"
)]
pub struct UiConfig {
    /// Show hotkey labels inside clickable controls.
    pub show_button_hotkeys: bool,
    /// Terminal theme selection.
    pub theme: ThemeMode,
    /// Preferred thumbnail behavior.
    pub thumbnails: ThumbnailMode,
    /// Render bounded half-block artwork on a confirmed physical Linux TTY.
    pub show_images_in_tty: bool,
    /// Maximum thumbnail height in terminal rows.
    pub thumbnail_height: u16,
    /// Prefetch currently loaded Search-result thumbnails into the disk cache.
    pub prefetch_search_thumbnails: bool,
    /// Measure visible Local folders lazily and show complete recursive sizes.
    pub show_local_folder_sizes: bool,
    /// Seek-bar foreground color name or terminal palette index.
    pub seekbar_color: String,
    /// Replace the standard seek indicator with Nyan Cat.
    pub nyan_cat_seekbar: bool,
    /// Enable the decorative DOS-RPG layout.
    pub dos_rpg_mode: bool,
    /// Navigation model used by the Subscriptions screen.
    pub subscriptions_layout: SubscriptionsLayout,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            show_button_hotkeys: true,
            theme: ThemeMode::Auto,
            thumbnails: ThumbnailMode::Auto,
            show_images_in_tty: true,
            thumbnail_height: DEFAULT_THUMBNAIL_HEIGHT,
            prefetch_search_thumbnails: true,
            show_local_folder_sizes: true,
            seekbar_color: "cyan".to_owned(),
            nyan_cat_seekbar: false,
            dos_rpg_mode: false,
            subscriptions_layout: SubscriptionsLayout::DrillDown,
        }
    }
}

/// Navigation model used by the Subscriptions screen.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubscriptionsLayout {
    /// Enter a source to reuse the normal media-list and Details layout.
    #[default]
    DrillDown,
    /// Keep sources and their recent media visible in adjacent panes.
    Split,
}

impl SubscriptionsLayout {
    /// Returns the stable TOML representation used by the focused in-app
    /// preference writer.
    #[must_use]
    pub const fn as_config_value(self) -> &'static str {
        match self {
            Self::DrillDown => "drill-down",
            Self::Split => "split",
        }
    }

    /// Returns the alternative layout for a two-choice preference control.
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::DrillDown => Self::Split,
            Self::Split => Self::DrillDown,
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
    /// Storage backend used for state and caches.
    pub backend: PersistenceBackend,
    /// Interval between bounded crash-durable playback checkpoints.
    pub position_save_interval_seconds: u64,
    /// Completion threshold used by configurable UI and import code.
    pub played_threshold_percent: u8,
    /// On graceful shutdown, commit and push the application directory when it
    /// belongs to a Git worktree.
    pub git_commit_on_change: bool,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            backend: PersistenceBackend::Files,
            position_save_interval_seconds: 30,
            played_threshold_percent: PLAYED_THRESHOLD_PERCENT,
            git_commit_on_change: true,
        }
    }
}

/// State-storage implementation selected at runtime.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PersistenceBackend {
    /// Deterministic TOML files intended for inspection and version control.
    #[default]
    Files,
    /// One SQLite database, available when built with `sqlite-state`.
    Sqlite,
}

impl fmt::Display for PersistenceBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Files => "files",
            Self::Sqlite => "sqlite",
        })
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
    /// Preferred Bandcamp stream or free-download encoding.
    pub bandcamp_audio_format: BandcampAudioFormat,
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
            .field("bandcamp_audio_format", &self.bandcamp_audio_format)
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
            bandcamp_audio_format: BandcampAudioFormat::default(),
            yt_dlp_executable: PathBuf::from("yt-dlp"),
            mpv_executable: PathBuf::from("mpv"),
        }
    }
}

/// Closed set of Bandcamp encodings exposed by configuration and Preferences.
///
/// Each variant maps to a Youta-owned static `yt-dlp` selector. Configuration
/// deserialization therefore cannot introduce arbitrary format expressions.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum BandcampAudioFormat {
    /// Prefer FLAC, then other lossless encodings, lossy download encodings,
    /// and finally the public MP3-128 stream or extractor-selected best audio.
    #[default]
    #[serde(rename = "best-available")]
    BestAvailable,
    /// Prefer Bandcamp's FLAC download.
    #[serde(rename = "flac")]
    Flac,
    /// Prefer Bandcamp's Apple Lossless download.
    #[serde(rename = "alac")]
    Alac,
    /// Prefer Bandcamp's uncompressed WAV download.
    #[serde(rename = "wav")]
    Wav,
    /// Prefer Bandcamp's uncompressed AIFF download.
    #[serde(rename = "aiff")]
    Aiff,
    /// Prefer Bandcamp's 320 kbps MP3 download.
    #[serde(rename = "mp3-320")]
    Mp3Kbps320,
    /// Prefer Bandcamp's V0 variable-bitrate MP3 download.
    #[serde(rename = "mp3-v0")]
    Mp3V0,
    /// Prefer Bandcamp's high-quality AAC download.
    #[serde(rename = "aac")]
    Aac,
    /// Prefer Bandcamp's Ogg Vorbis download.
    #[serde(rename = "ogg-vorbis")]
    OggVorbis,
    /// Use the public MP3-128 stream, with `bestaudio` as a compatibility
    /// fallback for extractor naming changes.
    #[serde(rename = "public-stream-mp3-128")]
    PublicStreamMp3Kbps128,
}

impl BandcampAudioFormat {
    /// Formats in the stable order shown by the Preferences selector.
    pub const ALL: [Self; 10] = [
        Self::BestAvailable,
        Self::Flac,
        Self::Alac,
        Self::Wav,
        Self::Aiff,
        Self::Mp3Kbps320,
        Self::Mp3V0,
        Self::Aac,
        Self::OggVorbis,
        Self::PublicStreamMp3Kbps128,
    ];

    /// Returns the human-readable Preferences label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::BestAvailable => "Best available",
            Self::Flac => "FLAC",
            Self::Alac => "ALAC",
            Self::Wav => "WAV",
            Self::Aiff => "AIFF",
            Self::Mp3Kbps320 => "MP3 320",
            Self::Mp3V0 => "MP3 V0",
            Self::Aac => "AAC",
            Self::OggVorbis => "Ogg Vorbis",
            Self::PublicStreamMp3Kbps128 => "Public stream/MP3 128",
        }
    }

    /// Returns the stable TOML and environment representation.
    #[must_use]
    pub const fn as_config_value(self) -> &'static str {
        match self {
            Self::BestAvailable => "best-available",
            Self::Flac => "flac",
            Self::Alac => "alac",
            Self::Wav => "wav",
            Self::Aiff => "aiff",
            Self::Mp3Kbps320 => "mp3-320",
            Self::Mp3V0 => "mp3-v0",
            Self::Aac => "aac",
            Self::OggVorbis => "ogg-vorbis",
            Self::PublicStreamMp3Kbps128 => "public-stream-mp3-128",
        }
    }

    /// Returns Youta's static `yt-dlp` selector for this preference.
    ///
    /// Every named download encoding falls back to Bandcamp's public MP3-128
    /// stream and then `bestaudio`. `Best available` orders FLAC first,
    /// followed by the other lossless choices before lossy encodings.
    #[must_use]
    pub const fn yt_dlp_selector(self) -> &'static str {
        match self {
            Self::BestAvailable => {
                "flac/wav/aiff-lossless/falac/[acodec^=alac]/mp3-320/mp3-v0/aac-hi/vorbis/mp3-128/bestaudio"
            }
            Self::Flac => "flac/mp3-128/bestaudio",
            Self::Alac => "falac/[acodec^=alac]/mp3-128/bestaudio",
            Self::Wav => "wav/mp3-128/bestaudio",
            Self::Aiff => "aiff-lossless/mp3-128/bestaudio",
            Self::Mp3Kbps320 => "mp3-320/mp3-128/bestaudio",
            Self::Mp3V0 => "mp3-v0/mp3-128/bestaudio",
            Self::Aac => "aac-hi/mp3-128/bestaudio",
            Self::OggVorbis => "vorbis/mp3-128/bestaudio",
            Self::PublicStreamMp3Kbps128 => "mp3-128/bestaudio",
        }
    }
}

impl fmt::Display for BandcampAudioFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
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

fn validate_credentials_file(path: &Path) -> Result<(), ConfigError> {
    let contents = read_limited_utf8_file(path, MAX_CREDENTIALS_BYTES, "credentials.toml")?;
    let credentials: CredentialsFile = toml::from_str(&contents).map_err(|_| {
        ConfigError::Invalid(format!(
            "{} is not a valid Youta credentials file; check its TOML syntax and supported fields",
            path.display()
        ))
    })?;
    if let Some(api_key) = credentials.providers.youtube_api_key.as_deref() {
        validate_youtube_api_key("providers.youtube_api_key", api_key)
            .map_err(|error| credential_file_error(path, error))?;
    }
    for (field, credential) in [
        (
            "providers.mod_archive_api_key",
            credentials.providers.mod_archive_api_key.as_deref(),
        ),
        (
            "providers.jamendo_client_id",
            credentials.providers.jamendo_client_id.as_deref(),
        ),
    ] {
        if let Some(credential) = credential {
            validate_generic_credential(field, credential)
                .map_err(|error| credential_file_error(path, error))?;
        }
    }
    Ok(())
}

fn credential_file_error(path: &Path, error: ConfigError) -> ConfigError {
    match error {
        ConfigError::Invalid(message) => {
            ConfigError::Invalid(format!("{}: {message}", path.display()))
        }
        other => other,
    }
}

fn read_limited_utf8_file(path: &Path, limit: u64, label: &str) -> Result<String, ConfigError> {
    let file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    if length > limit {
        return Err(ConfigError::Invalid(format!(
            "{label} exceeds the {limit}-byte limit"
        )));
    }
    let capacity = usize::try_from(length).unwrap_or_default();
    let mut contents = String::with_capacity(capacity);
    file.take(limit.saturating_add(1))
        .read_to_string(&mut contents)?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > limit {
        return Err(ConfigError::Invalid(format!(
            "{label} exceeds the {limit}-byte limit"
        )));
    }
    Ok(contents)
}

fn validate_generic_credential(field: &str, credential: &str) -> Result<(), ConfigError> {
    let trimmed = credential.trim();
    if credential != trimmed {
        return Err(ConfigError::Invalid(format!(
            "{field} must not contain surrounding whitespace"
        )));
    }
    if trimmed.is_empty() || trimmed.len() > 4096 || trimmed.chars().any(char::is_control) {
        return Err(ConfigError::Invalid(format!(
            "{field} must contain 1 to 4096 printable characters"
        )));
    }
    Ok(())
}

fn validate_youtube_api_key(field: &str, api_key: &str) -> Result<String, ConfigError> {
    if api_key != api_key.trim() {
        return Err(ConfigError::Invalid(format!(
            "{field} must not contain surrounding whitespace"
        )));
    }
    if !(16..=256).contains(&api_key.len())
        || !api_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ConfigError::Invalid(format!(
            "{field} must contain 16 to 256 URL-safe characters"
        )));
    }
    Ok(api_key.to_owned())
}

fn ensure_youta_gitignore(path: &Path) -> Result<(), ConfigError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => return Ok(()),
        Ok(_) => {
            return Err(ConfigError::Invalid(format!(
                "{} must be a regular file",
                path.display()
            )));
        }
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        Err(_) => {}
    }

    let mut contents = String::new();
    contents.push_str(YOUTA_GITIGNORE_BEGIN);
    contents.push('\n');
    for rule in YOUTA_GITIGNORE_RULES {
        contents.push_str(rule);
        contents.push('\n');
    }
    contents.push_str(YOUTA_GITIGNORE_END);
    contents.push('\n');
    write_private_config(path, contents.as_bytes())
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
    read_editable_toml(path, MAX_CONFIG_BYTES, "config.toml")
}

#[cfg(feature = "tui")]
fn read_editable_credentials(path: &Path) -> Result<DocumentMut, ConfigError> {
    if path.is_file() {
        validate_credentials_file(path)?;
    }
    read_editable_toml(path, MAX_CREDENTIALS_BYTES, "credentials.toml")
}

#[cfg(feature = "tui")]
fn read_editable_toml(path: &Path, limit: u64, label: &str) -> Result<DocumentMut, ConfigError> {
    let contents = match fs::metadata(path) {
        Ok(_) => read_limited_utf8_file(path, limit, label)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DocumentMut::new());
        }
        Err(error) => return Err(error.into()),
    };
    contents.parse().map_err(|_| {
        ConfigError::Invalid(format!(
            "{label} is not valid TOML; fix it before saving from Youta"
        ))
    })
}

#[cfg(feature = "tui")]
fn editable_table<'a>(
    document: &'a mut DocumentMut,
    name: &str,
) -> Result<&'a mut Table, ConfigError> {
    document
        .as_table_mut()
        .entry(name)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| {
            ConfigError::Invalid(format!(
                "`{name}` must be a TOML table before Youta can update it"
            ))
        })
}

#[cfg(feature = "tui")]
fn migrate_legacy_provider_credentials(
    config: &mut DocumentMut,
    credentials: &mut DocumentMut,
) -> Result<(), ConfigError> {
    let mut legacy_credentials = Vec::new();
    if let Some(providers) = config
        .as_table_mut()
        .get_mut("providers")
        .and_then(Item::as_table_mut)
    {
        for field in [
            "youtube_api_key",
            "mod_archive_api_key",
            "jamendo_client_id",
        ] {
            if let Some(item) = providers.remove(field) {
                let credential = item.as_str().ok_or_else(|| {
                    ConfigError::Invalid(format!(
                        "`providers.{field}` must be a string before Youta can migrate it"
                    ))
                })?;
                legacy_credentials.push((field, credential.to_owned()));
            }
        }
    }
    if legacy_credentials.is_empty() {
        return Ok(());
    }

    let providers = editable_table(credentials, "providers")?;
    for (field, credential) in legacy_credentials {
        if !providers.contains_key(field) {
            providers[field] = value(credential);
        }
    }
    Ok(())
}

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
        assert!(!config.playback.autoplay);
        assert!(config.playback.youtube_prewarm);
        assert!(config.playback.skip_advertisement_chapters);
        assert_eq!(config.persistence.backend, PersistenceBackend::Files);
        assert_eq!(config.persistence.position_save_interval_seconds, 30);
        assert!(config.persistence.git_commit_on_change);
        assert_eq!(config.ui.thumbnail_height, DEFAULT_THUMBNAIL_HEIGHT);
        assert!(config.ui.prefetch_search_thumbnails);
        assert!(config.ui.show_images_in_tty);
        assert!(config.ui.show_local_folder_sizes);
        assert_eq!(
            config.ui.subscriptions_layout,
            SubscriptionsLayout::DrillDown
        );
        assert!(config.providers.allow_insecure_http);
        assert_eq!(config.providers.youtube_backend, YouTubeBackend::Auto);
        assert!(config.providers.jamendo_client_id.is_none());
        assert_eq!(
            config.providers.bandcamp_audio_format,
            BandcampAudioFormat::BestAvailable
        );
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
autoplay = true
youtube_prewarm = false
skip_advertisement_chapters = false

[subscriptions]
auto_download = false

[ui]
theme = "light"
thumbnail_height = 14
prefetch_search_thumbnails = false
show_images_in_tty = false
show_local_folder_sizes = false
subscriptions_layout = "split"

[providers]
bandcamp_audio_format = "alac"
"#,
        )
        .expect("write test TOML");

        let config = Config::load_from_dir_with_environment(directory.path().to_owned(), false)
            .expect("load TOML");
        assert_eq!(config.playback.volume_percent, 35);
        assert_eq!(config.playback.speed_percent, 120);
        assert!(config.playback.autoplay);
        assert!(!config.playback.youtube_prewarm);
        assert!(!config.playback.skip_advertisement_chapters);
        assert!(!config.subscriptions.auto_download);
        assert_eq!(config.ui.theme, ThemeMode::Light);
        assert_eq!(config.ui.thumbnail_height, 14);
        assert!(!config.ui.prefetch_search_thumbnails);
        assert!(!config.ui.show_images_in_tty);
        assert!(!config.ui.show_local_folder_sizes);
        assert_eq!(config.ui.subscriptions_layout, SubscriptionsLayout::Split);
        assert_eq!(
            config.providers.bandcamp_audio_format,
            BandcampAudioFormat::Alac
        );
        assert_eq!(config.config_dir(), directory.path());
    }

    #[test]
    fn documented_config_example_remains_loadable() {
        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("config.toml"),
            include_str!("../config.example.toml"),
        )
        .expect("write documented configuration");

        let config = Config::load_from_dir_with_environment(directory.path().to_owned(), false)
            .expect("load documented configuration");

        assert!(config.ui.show_images_in_tty);
        assert_eq!(config.ui.thumbnails, ThumbnailMode::Auto);
    }

    #[test]
    fn environment_overrides_toml_in_child_process() {
        const CHILD_MARKER: &str = "YOUTA_CONFIG_TEST_CHILD";
        if std::env::var_os(CHILD_MARKER).is_some() {
            let config = Config::load().expect("load child configuration");
            assert_eq!(config.playback.volume_percent, 62);
            assert!(config.playback.autoplay);
            assert!(!config.playback.youtube_prewarm);
            assert!(!config.playback.skip_advertisement_chapters);
            assert!(config.ui.nyan_cat_seekbar);
            assert_eq!(config.ui.thumbnail_height, 16);
            assert!(!config.ui.prefetch_search_thumbnails);
            assert!(!config.ui.show_images_in_tty);
            assert!(!config.ui.show_local_folder_sizes);
            assert_eq!(config.ui.subscriptions_layout, SubscriptionsLayout::Split);
            assert_eq!(
                config.providers.bandcamp_audio_format,
                BandcampAudioFormat::OggVorbis
            );
            assert_eq!(
                config.providers.youtube_api_key.as_deref(),
                Some("AIzaEnvironment_key_123456789012345")
            );
            return;
        }

        let directory = tempdir().expect("temporary directory");
        fs::create_dir(directory.path().join("secrets")).expect("secrets directory");
        fs::write(
            directory.path().join("config.toml"),
            "[playback]\nvolume_percent = 20\n\n[providers]\nyoutube_api_key = \"AIzaConfig_key_123456789012345678\"\n",
        )
        .expect("write test TOML");
        fs::write(
            directory.path().join("secrets/credentials.toml"),
            "[providers]\nyoutube_api_key = \"AIzaPrivate_key_12345678901234567\"\n",
        )
        .expect("write private credentials");
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "config::tests::environment_overrides_toml_in_child_process",
                "--nocapture",
            ])
            .env(CHILD_MARKER, "1")
            .env(CONFIG_DIR_ENV, directory.path())
            .env("YOUTA_PLAYBACK__VOLUME_PERCENT", "62")
            .env(AUTOPLAY_ENV, "true")
            .env(YOUTUBE_PREWARM_ENV, "false")
            .env(SKIP_ADVERTISEMENT_CHAPTERS_ENV, "false")
            .env("YOUTA_UI__NYAN_CAT_SEEKBAR", "true")
            .env("YOUTA_UI__THUMBNAIL_HEIGHT", "16")
            .env("YOUTA_UI__PREFETCH_SEARCH_THUMBNAILS", "false")
            .env(TTY_IMAGES_ENV, "false")
            .env(LOCAL_FOLDER_SIZES_ENV, "false")
            .env(SUBSCRIPTIONS_LAYOUT_ENV, "split")
            .env(BANDCAMP_AUDIO_FORMAT_ENV, "ogg-vorbis")
            .env(
                "YOUTA_PROVIDERS__YOUTUBE_API_KEY",
                "AIzaEnvironment_key_123456789012345",
            )
            .output()
            .expect("run environment test child");
        assert!(
            output.status.success(),
            "child test failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(feature = "tui")]
    #[test]
    fn subscriptions_layout_save_preserves_unrelated_toml_and_comments() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"# keep this comment
[ui]
theme = "dark"
subscriptions_layout = "drill-down"

[providers]
youtube_api_key = "keep-this-existing-secret"
"#,
        )
        .expect("write configuration");
        let mut config = Config::load_from_dir_with_environment(directory.path().to_owned(), false)
            .expect("load configuration");

        config
            .save_subscriptions_layout(SubscriptionsLayout::Split)
            .expect("save subscriptions layout");

        let contents = fs::read_to_string(&path).expect("read updated configuration");
        assert!(contents.contains("# keep this comment"));
        assert!(contents.contains("theme = \"dark\""));
        assert!(contents.contains("youtube_api_key = \"keep-this-existing-secret\""));
        assert!(contents.contains("subscriptions_layout = \"split\""));
        let reloaded = Config::load_from_dir_with_environment(directory.path().to_owned(), false)
            .expect("reload configuration");
        assert_eq!(reloaded.ui.subscriptions_layout, SubscriptionsLayout::Split);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn tui_preferences_save_both_tables_atomically_and_preserve_unrelated_content() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"# keep this comment
[playback]
volume_percent = 35

[ui]
theme = "dark"
subscriptions_layout = "drill-down"

[providers]
youtube_api_key = "keep-this-existing-secret"
"#,
        )
        .expect("write configuration");
        let mut config = Config::load_from_dir_with_environment(directory.path().to_owned(), false)
            .expect("load configuration");

        config
            .save_tui_preferences(SubscriptionsLayout::Split, false, false, false, false)
            .expect("save TUI preferences");

        let contents = fs::read_to_string(&path).expect("read updated configuration");
        assert!(contents.contains("# keep this comment"));
        assert!(contents.contains("volume_percent = 35"));
        assert!(contents.contains("theme = \"dark\""));
        assert!(contents.contains("youtube_api_key = \"keep-this-existing-secret\""));
        assert!(contents.contains("subscriptions_layout = \"split\""));
        assert!(contents.contains("skip_advertisement_chapters = false"));
        assert!(contents.contains("youtube_prewarm = false"));
        assert!(contents.contains("show_local_folder_sizes = false"));
        #[cfg(feature = "images")]
        assert!(contents.contains("show_images_in_tty = false"));
        #[cfg(not(feature = "images"))]
        assert!(!contents.contains("show_images_in_tty"));
        assert_eq!(config.ui.subscriptions_layout, SubscriptionsLayout::Split);
        assert!(!config.ui.show_local_folder_sizes);
        #[cfg(feature = "images")]
        assert!(!config.ui.show_images_in_tty);
        #[cfg(not(feature = "images"))]
        assert!(config.ui.show_images_in_tty);
        assert!(!config.playback.youtube_prewarm);
        assert!(!config.playback.skip_advertisement_chapters);

        let reloaded = Config::load_from_dir_with_environment(directory.path().to_owned(), false)
            .expect("reload configuration");
        assert_eq!(reloaded.ui.subscriptions_layout, SubscriptionsLayout::Split);
        assert!(!reloaded.ui.show_local_folder_sizes);
        #[cfg(feature = "images")]
        assert!(!reloaded.ui.show_images_in_tty);
        #[cfg(not(feature = "images"))]
        assert!(reloaded.ui.show_images_in_tty);
        assert!(!reloaded.playback.youtube_prewarm);
        assert!(!reloaded.playback.skip_advertisement_chapters);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn autoplay_save_preserves_unrelated_configuration() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            r#"# keep this comment
[playback]
volume_percent = 35

[providers]
youtube_api_key = "keep-this-existing-secret"
"#,
        )
        .expect("write configuration");
        let mut config = Config::load_from_dir_with_environment(directory.path().to_owned(), false)
            .expect("load configuration");

        config.save_autoplay(true).expect("save autoplay");

        let contents = fs::read_to_string(&path).expect("read updated configuration");
        assert!(contents.contains("# keep this comment"));
        assert!(contents.contains("volume_percent = 35"));
        assert!(contents.contains("youtube_api_key = \"keep-this-existing-secret\""));
        assert!(contents.contains("autoplay = true"));
        assert!(config.playback.autoplay);
        let reloaded = Config::load_from_dir_with_environment(directory.path().to_owned(), false)
            .expect("reload configuration");
        assert!(reloaded.playback.autoplay);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn bandcamp_audio_format_save_preserves_unrelated_configuration() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("config.toml");
        fs::write(
            &path,
            "# keep this comment\n[providers]\nyoutube_api_key = \"keep-this-secret\"\n",
        )
        .expect("write configuration");
        let mut config = Config::load_from_dir_with_environment(directory.path().to_owned(), false)
            .expect("load configuration");

        config
            .save_bandcamp_audio_format(BandcampAudioFormat::Flac)
            .expect("save Bandcamp format");

        let contents = fs::read_to_string(&path).expect("read updated configuration");
        assert!(contents.contains("# keep this comment"));
        assert!(contents.contains("youtube_api_key = \"keep-this-secret\""));
        assert!(contents.contains("bandcamp_audio_format = \"flac\""));
        assert_eq!(
            config.providers.bandcamp_audio_format,
            BandcampAudioFormat::Flac
        );
    }

    #[cfg(feature = "tui")]
    #[test]
    fn autoplay_environment_override_prevents_file_and_memory_mutation() {
        const CHILD_MARKER: &str = "YOUTA_AUTOPLAY_SAVE_TEST_CHILD";
        const TEST_DIRECTORY: &str = "YOUTA_AUTOPLAY_SAVE_TEST_DIR";
        if std::env::var_os(CHILD_MARKER).is_some() {
            let directory =
                PathBuf::from(std::env::var(TEST_DIRECTORY).expect("child test directory"));
            let mut config =
                Config::load_from_dir(directory.clone()).expect("load overridden configuration");
            assert!(config.playback.autoplay);

            let error = config
                .save_autoplay(false)
                .expect_err("the environment override must lock autoplay");

            assert!(error.to_string().contains(AUTOPLAY_ENV));
            assert!(config.playback.autoplay);
            assert!(!directory.join("config.toml").exists());
            return;
        }

        let directory = tempdir().expect("temporary directory");
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "config::tests::autoplay_environment_override_prevents_file_and_memory_mutation",
                "--nocapture",
            ])
            .env(CHILD_MARKER, "1")
            .env(TEST_DIRECTORY, directory.path())
            .env(CONFIG_DIR_ENV, directory.path())
            .env(AUTOPLAY_ENV, "true")
            .output()
            .expect("run autoplay environment-lock child");
        assert!(
            output.status.success(),
            "child test failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(feature = "tui")]
    #[test]
    fn tui_preferences_environment_overrides_prevent_any_partial_write() {
        const CHILD_MARKER: &str = "YOUTA_PREFERENCES_SAVE_TEST_CHILD";
        const OVERRIDE_NAME: &str = "YOUTA_PREFERENCES_SAVE_TEST_OVERRIDE";
        if std::env::var_os(CHILD_MARKER).is_some() {
            let directory = PathBuf::from(
                std::env::var("YOUTA_PREFERENCES_SAVE_TEST_DIR").expect("child test directory"),
            );
            let override_name = std::env::var(OVERRIDE_NAME).expect("override name");
            let mut config =
                Config::load_from_dir(directory.clone()).expect("load overridden configuration");
            let error = config
                .save_tui_preferences(SubscriptionsLayout::Split, false, true, true, true)
                .expect_err("an environment override must lock the atomic writer");
            assert!(error.to_string().contains(&override_name));
            assert!(!directory.join("config.toml").exists());
            return;
        }

        let overrides = [
            (SUBSCRIPTIONS_LAYOUT_ENV, "split"),
            (SKIP_ADVERTISEMENT_CHAPTERS_ENV, "false"),
            (YOUTUBE_PREWARM_ENV, "false"),
            (LOCAL_FOLDER_SIZES_ENV, "false"),
            (TTY_IMAGES_ENV, "false"),
        ];
        for (override_name, override_value) in overrides
            .into_iter()
            .filter(|(name, _)| cfg!(feature = "images") || *name != TTY_IMAGES_ENV)
        {
            let directory = tempdir().expect("temporary directory");
            let output = Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    "config::tests::tui_preferences_environment_overrides_prevent_any_partial_write",
                    "--nocapture",
                ])
                .env(CHILD_MARKER, "1")
                .env(OVERRIDE_NAME, override_name)
                .env("YOUTA_PREFERENCES_SAVE_TEST_DIR", directory.path())
                .env(CONFIG_DIR_ENV, directory.path())
                .env(override_name, override_value)
                .output()
                .expect("run environment-lock child");
            assert!(
                output.status.success(),
                "child test failed for {override_name}:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[cfg(feature = "tui")]
    #[test]
    fn subscriptions_layout_environment_override_locks_in_app_save() {
        const CHILD_MARKER: &str = "YOUTA_LAYOUT_SAVE_TEST_CHILD";
        if std::env::var_os(CHILD_MARKER).is_some() {
            let directory = PathBuf::from(
                std::env::var("YOUTA_LAYOUT_SAVE_TEST_DIR").expect("child test directory"),
            );
            let mut config =
                Config::load_from_dir(directory.clone()).expect("load overridden configuration");
            assert_eq!(config.ui.subscriptions_layout, SubscriptionsLayout::Split);
            let error = config
                .save_subscriptions_layout(SubscriptionsLayout::DrillDown)
                .expect_err("environment override must lock the writer");
            assert!(error.to_string().contains(SUBSCRIPTIONS_LAYOUT_ENV));
            assert!(!directory.join("config.toml").exists());
            return;
        }

        let directory = tempdir().expect("temporary directory");
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "config::tests::subscriptions_layout_environment_override_locks_in_app_save",
                "--nocapture",
            ])
            .env(CHILD_MARKER, "1")
            .env("YOUTA_LAYOUT_SAVE_TEST_DIR", directory.path())
            .env(CONFIG_DIR_ENV, directory.path())
            .env(SUBSCRIPTIONS_LAYOUT_ENV, "split")
            .output()
            .expect("run environment-lock child");
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
    fn bandcamp_audio_format_serde_is_closed_and_stable() {
        let expected = [
            (BandcampAudioFormat::BestAvailable, "best-available"),
            (BandcampAudioFormat::Flac, "flac"),
            (BandcampAudioFormat::Alac, "alac"),
            (BandcampAudioFormat::Wav, "wav"),
            (BandcampAudioFormat::Aiff, "aiff"),
            (BandcampAudioFormat::Mp3Kbps320, "mp3-320"),
            (BandcampAudioFormat::Mp3V0, "mp3-v0"),
            (BandcampAudioFormat::Aac, "aac"),
            (BandcampAudioFormat::OggVorbis, "ogg-vorbis"),
            (
                BandcampAudioFormat::PublicStreamMp3Kbps128,
                "public-stream-mp3-128",
            ),
        ];
        for (format, value) in expected {
            let encoded = serde_json::to_string(&format).expect("serialize format");
            assert_eq!(encoded, format!("\"{value}\""));
            assert_eq!(
                serde_json::from_str::<BandcampAudioFormat>(&encoded)
                    .expect("deserialize known format"),
                format
            );
            assert_eq!(format.as_config_value(), value);
        }
        assert!(
            serde_json::from_str::<BandcampAudioFormat>("\"flac/bestaudio[protocol=https]\"")
                .is_err(),
            "arbitrary yt-dlp expressions must not deserialize"
        );
    }

    #[test]
    fn invalid_bandcamp_audio_format_is_rejected_from_toml() {
        let directory = tempdir().expect("temporary directory");
        fs::write(
            directory.path().join("config.toml"),
            "[providers]\nbandcamp_audio_format = \"flac/bestaudio\"\n",
        )
        .expect("write invalid configuration");

        assert!(
            Config::load_from_dir_with_environment(directory.path().to_owned(), false).is_err(),
            "an arbitrary yt-dlp selector must not deserialize as a Bandcamp preference"
        );
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
        assert!(config.secrets_dir().is_dir());
        let gitignore = fs::read_to_string(config.gitignore_file()).expect("default .gitignore");
        assert!(gitignore.contains("/secrets/"));
        assert!(gitignore.contains("/runtime/"));
        assert!(gitignore.contains("/cache/"));

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
            let mode = fs::metadata(config.secrets_dir())
                .expect("secrets metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[test]
    fn existing_gitignore_remains_under_user_control() {
        let directory = tempdir().expect("temporary directory");
        let root = directory.path().join("youta");
        fs::create_dir(&root).expect("config directory");
        let custom = "# I intentionally track credentials in a private repository.\n";
        fs::write(root.join(".gitignore"), custom).expect("custom .gitignore");

        Config::for_dir(&root)
            .ensure_directories()
            .expect("prepare application directory");

        assert_eq!(
            fs::read_to_string(root.join(".gitignore")).expect("preserved .gitignore"),
            custom
        );
    }

    #[test]
    fn private_credentials_override_legacy_config_values() {
        let directory = tempdir().expect("temporary directory");
        fs::create_dir(directory.path().join("secrets")).expect("secrets directory");
        fs::write(
            directory.path().join("config.toml"),
            "[providers]\nyoutube_api_key = \"AIzaLegacy_key_123456789012345678\"\n",
        )
        .expect("legacy config");
        fs::write(
            directory.path().join("secrets/credentials.toml"),
            "[providers]\nyoutube_api_key = \"AIzaPrivate_key_12345678901234567\"\n",
        )
        .expect("private credentials");

        let config = Config::load_from_dir_with_environment(directory.path().to_owned(), false)
            .expect("load layered credentials");

        assert_eq!(
            config.providers.youtube_api_key.as_deref(),
            Some("AIzaPrivate_key_12345678901234567")
        );
    }

    #[test]
    fn malformed_or_unknown_private_credentials_are_rejected() {
        let directory = tempdir().expect("temporary directory");
        fs::create_dir(directory.path().join("secrets")).expect("secrets directory");
        let path = directory.path().join("secrets/credentials.toml");
        fs::write(
            &path,
            "[providers]\nyoutube_api_key = \"AIzaValid_key_123456789012345678\"\nunknown = true\n",
        )
        .expect("invalid credentials");

        let error = Config::load_from_dir_with_environment(directory.path().to_owned(), false)
            .expect_err("unknown credential fields must fail");
        let rendered = error.to_string();
        assert!(!rendered.contains("AIzaValid_key_123456789012345678"));
        assert!(rendered.contains("credentials file"));
    }

    #[test]
    fn credentials_reject_surrounding_whitespace_without_loading_trimmed_values() {
        for (field, value) in [
            ("youtube_api_key", " AIzaValid_key_123456789012345678"),
            ("mod_archive_api_key", "mod-archive-key "),
            ("jamendo_client_id", "\tjamendo-client-id"),
        ] {
            let directory = tempdir().expect("temporary directory");
            let secrets = directory.path().join("secrets");
            fs::create_dir(&secrets).expect("secrets directory");
            fs::write(
                secrets.join("credentials.toml"),
                format!("[providers]\n{field} = {value:?}\n"),
            )
            .expect("credential fixture");

            let error = Config::load_from_dir_with_environment(directory.path().to_owned(), false)
                .expect_err("surrounding whitespace must be rejected");
            let rendered = error.to_string();
            assert!(
                rendered.contains(&format!("providers.{field}")),
                "missing field path in: {rendered}"
            );
            assert!(
                rendered.contains("credentials.toml"),
                "missing credentials-file path in: {rendered}"
            );
            assert!(
                rendered.contains("surrounding whitespace"),
                "missing whitespace guidance in: {rendered}"
            );
            assert!(
                !rendered.contains(value),
                "credential value leaked in: {rendered}"
            );
        }
    }

    #[test]
    fn legacy_provider_credentials_use_the_same_exact_validation() {
        for (field, value) in [
            ("youtube_api_key", "AIzaValid_key_123456789012345678 "),
            ("mod_archive_api_key", " mod-archive-key"),
            ("jamendo_client_id", "jamendo-client-id\n"),
        ] {
            let directory = tempdir().expect("temporary directory");
            fs::write(
                directory.path().join("config.toml"),
                format!("[providers]\n{field} = {value:?}\n"),
            )
            .expect("legacy provider fixture");

            let error = Config::load_from_dir_with_environment(directory.path().to_owned(), false)
                .expect_err("legacy credentials must not be normalized implicitly");
            assert!(error.to_string().contains(&format!("providers.{field}")));
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
        assert!(!after_key.contains(api_key));
        let credentials = fs::read_to_string(config.credentials_file()).expect("saved credentials");
        assert!(credentials.contains(api_key));
        assert_eq!(config.providers.youtube_backend, YouTubeBackend::Official);
        assert_eq!(config.providers.youtube_api_key.as_deref(), Some(api_key));

        let invidious = Url::parse("https://inv.example.test/api").expect("fixture URL");
        config
            .save_youtube_provider(YouTubeProviderSetting::InvidiousUrl(invidious))
            .expect("save Invidious URL");
        let after_url = fs::read_to_string(config.config_file()).expect("updated config");
        assert!(after_url.contains("# keep this comment"));
        assert!(
            !after_url.contains(api_key),
            "the Git-friendly configuration must not contain the API key"
        );
        assert!(
            fs::read_to_string(config.credentials_file())
                .expect("retained credentials")
                .contains(api_key),
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
        assert_eq!(restored.providers.youtube_api_key.as_deref(), Some(api_key));
        assert_eq!(restored.playback.volume_percent, 35);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn youtube_provider_environment_overrides_prevent_file_and_memory_mutation() {
        const CHILD_MARKER: &str = "YOUTA_PROVIDER_SAVE_TEST_CHILD";
        const TEST_DIRECTORY: &str = "YOUTA_PROVIDER_SAVE_TEST_DIR";
        const TEST_CASE: &str = "YOUTA_PROVIDER_SAVE_TEST_CASE";

        if std::env::var_os(CHILD_MARKER).is_some() {
            let directory =
                PathBuf::from(std::env::var(TEST_DIRECTORY).expect("child test directory"));
            let test_case = std::env::var(TEST_CASE).expect("child test case");
            let mut config =
                Config::load_from_dir(directory.clone()).expect("load overridden configuration");
            let before = config.clone();
            let (setting, expected_override) = match test_case.as_str() {
                "backend-official" => (
                    YouTubeProviderSetting::OfficialApiKey(
                        "AIzaSave_key_12345678901234567890".to_owned(),
                    ),
                    YOUTUBE_BACKEND_ENV,
                ),
                "backend-invidious" => (
                    YouTubeProviderSetting::InvidiousUrl(
                        Url::parse("https://save.example.test/").expect("fixture URL"),
                    ),
                    YOUTUBE_BACKEND_ENV,
                ),
                "api-key" => (
                    YouTubeProviderSetting::OfficialApiKey(
                        "AIzaSave_key_12345678901234567890".to_owned(),
                    ),
                    YOUTUBE_API_KEY_ENV,
                ),
                "invidious" => (
                    YouTubeProviderSetting::InvidiousUrl(
                        Url::parse("https://save.example.test/").expect("fixture URL"),
                    ),
                    INVIDIOUS_BASE_URL_ENV,
                ),
                other => panic!("unknown child test case: {other}"),
            };

            let error = config
                .save_youtube_provider(setting)
                .expect_err("environment override must lock the provider writer");

            assert!(error.to_string().contains(expected_override));
            assert_eq!(config, before, "failed save mutated in-memory settings");
            assert!(!config.config_file().exists());
            assert!(!config.credentials_file().exists());
            assert!(!config.gitignore_file().exists());
            assert!(!config.downloads_dir().exists());
            return;
        }

        for (test_case, override_name, override_value) in [
            ("backend-official", YOUTUBE_BACKEND_ENV, "invidious"),
            ("backend-invidious", YOUTUBE_BACKEND_ENV, "official"),
            (
                "api-key",
                YOUTUBE_API_KEY_ENV,
                "AIzaEnvironment_key_123456789012345",
            ),
            (
                "invidious",
                INVIDIOUS_BASE_URL_ENV,
                "https://environment.example.test/",
            ),
        ] {
            let directory = tempdir().expect("temporary directory");
            let output = Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    "config::tests::youtube_provider_environment_overrides_prevent_file_and_memory_mutation",
                    "--nocapture",
                ])
                .env_clear()
                .env(CHILD_MARKER, "1")
                .env(TEST_CASE, test_case)
                .env(TEST_DIRECTORY, directory.path())
                .env(CONFIG_DIR_ENV, directory.path())
                .env(override_name, override_value)
                .output()
                .expect("run provider environment-lock child");
            assert!(
                output.status.success(),
                "child test failed for {override_name}:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[cfg(feature = "tui")]
    #[test]
    fn youtube_provider_save_migrates_legacy_provider_credentials() {
        let directory = tempdir().expect("temporary directory");
        let root = directory.path().join("youta");
        fs::create_dir(&root).expect("config directory");
        fs::write(
            root.join("config.toml"),
            r#"[providers]
youtube_api_key = "AIzaLegacy_key_123456789012345678"
mod_archive_api_key = "legacy-mod-key"
jamendo_client_id = "legacy-jamendo-id"
"#,
        )
        .expect("legacy config");
        let mut config =
            Config::load_from_dir_with_environment(root, false).expect("load legacy config");

        config
            .save_youtube_provider(YouTubeProviderSetting::InvidiousUrl(
                Url::parse("https://inv.example.test/").expect("fixture URL"),
            ))
            .expect("save and migrate");

        let config_contents = fs::read_to_string(config.config_file()).expect("migrated config");
        for secret in [
            "AIzaLegacy_key_123456789012345678",
            "legacy-mod-key",
            "legacy-jamendo-id",
        ] {
            assert!(!config_contents.contains(secret));
        }
        let credentials =
            fs::read_to_string(config.credentials_file()).expect("migrated credentials");
        for secret in [
            "AIzaLegacy_key_123456789012345678",
            "legacy-mod-key",
            "legacy-jamendo-id",
        ] {
            assert!(credentials.contains(secret));
        }
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
        let credentials_mode = fs::metadata(config.credentials_file())
            .expect("credentials file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        assert_eq!(credentials_mode, 0o600);
    }
}
