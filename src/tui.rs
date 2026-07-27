//! Ratatui user interface and input mapping.
//!
//! This module renders Youta's own controls. An external player backend never
//! writes to the terminal and does not create a second user interface.

use std::borrow::Cow;
use std::io::{self, IsTerminal, Stdout};
use std::path::PathBuf;
use std::time::Duration;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use ratatui::{Terminal, TerminalOptions, Viewport};
#[cfg(feature = "thumbnails")]
use ratatui_image::StatefulImage as TerminalImage;
use unicode_segmentation::UnicodeSegmentation;

use crate::config::{
    DEFAULT_THUMBNAIL_HEIGHT, MIN_THUMBNAIL_HEIGHT, SubscriptionsLayout, ThumbnailMode,
};
use crate::domain::{Chapter, MediaId};
#[cfg(all(feature = "gpm", target_os = "linux"))]
use crate::gpm::LinuxConsoleInput;
use crate::links::{chapter_title_for_display, is_advertisement_chapter_title};
use crate::playback::PlaybackStatus;
#[cfg(feature = "thumbnails")]
use crate::thumbnails::{ThumbnailManager, ThumbnailState};
use crate::waveform::Peak;

/// Official Google instructions for creating and restricting a `YouTube` API key.
pub const YOUTUBE_API_KEY_GUIDE_URL: &str =
    "https://developers.google.com/youtube/registering_an_application";

/// Google Cloud page where the user creates and restricts API credentials.
pub const GOOGLE_CLOUD_CREDENTIALS_URL: &str = "https://console.cloud.google.com/apis/credentials";

/// Official Invidious documentation listing public instances.
pub const INVIDIOUS_INSTANCES_URL: &str = "https://docs.invidious.io/instances/";

/// Most chapter-label rows Youta may reserve above the seek track.
const MAX_CHAPTER_LABEL_ROWS: u16 = 4;

/// Body rows retained even when a dense chapter timeline requests more space.
const MIN_BODY_ROWS: u16 = 8;

/// Top-level Youta screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    /// Provider search and selected-item details.
    #[default]
    Search,
    /// Music-focused search through `music.youtube.com`.
    YouTubeMusic,
    /// Local folders and supported media files.
    Local,
    /// Locally subscribed channels and feeds.
    Subscriptions,
    /// Media available without a network connection.
    Downloaded,
    /// Played and partially played media.
    History,
    /// Nested user playlists and folders.
    Playlists,
    /// Listening totals grouped by source.
    Statistics,
    /// Aggregated tracker-module search across dedicated archives.
    TrackerMusic,
}

impl Screen {
    #[cfg(feature = "youtube-music")]
    const ALL: [Self; 9] = [
        Self::Search,
        Self::YouTubeMusic,
        Self::TrackerMusic,
        Self::Subscriptions,
        Self::Local,
        Self::Playlists,
        Self::Downloaded,
        Self::History,
        Self::Statistics,
    ];

    #[cfg(not(feature = "youtube-music"))]
    const ALL: [Self; 8] = [
        Self::Search,
        Self::TrackerMusic,
        Self::Subscriptions,
        Self::Local,
        Self::Playlists,
        Self::Downloaded,
        Self::History,
        Self::Statistics,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::Search => "YouTube",
            Self::YouTubeMusic => "YouTube Music",
            Self::TrackerMusic => "MOD/tracker",
            Self::Local => "Local",
            Self::Subscriptions => "Subscriptions",
            Self::Playlists => "Playlists",
            Self::Downloaded => "Downloaded",
            Self::History => "History",
            Self::Statistics => "Stats",
        }
    }

    const fn compact_label(self) -> &'static str {
        match self {
            Self::Search => "YouTube",
            Self::YouTubeMusic => "YT Music",
            Self::TrackerMusic => "MOD",
            Self::Local => "Local",
            Self::Subscriptions => "Subs",
            Self::Playlists => "Lists",
            Self::Downloaded => "Offline",
            Self::History => "History",
            Self::Statistics => "Stats",
        }
    }

    /// Returns the next enabled top-level tab, wrapping at the end.
    fn next(self) -> Self {
        let Some(index) = Self::ALL.iter().position(|candidate| *candidate == self) else {
            return Self::ALL[0];
        };
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// Returns the previous enabled top-level tab, wrapping at the beginning.
    fn previous(self) -> Self {
        let Some(index) = Self::ALL.iter().position(|candidate| *candidate == self) else {
            return Self::ALL[Self::ALL.len() - 1];
        };
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// Alternative content shown in the right-hand panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RightPanelMode {
    /// Metadata and description.
    #[default]
    Details,
    /// Cached or progressively generated waveform.
    Waveform,
    /// Channel or feed information for the playing item.
    Channel,
}

/// Seek-bar visual style.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SeekBarStyle {
    /// A compact terminal gauge.
    #[default]
    Line,
    /// A small animated cat label on the progress marker.
    NyanCat,
}

/// Object type queried by the default YouTube search screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchKind {
    /// Search for YouTube videos.
    #[default]
    Videos,
    /// Search for YouTube channels independently of videos.
    Channels,
}

/// Ordering selected for YouTube video or channel searches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum YouTubeSearchSort {
    /// Let the configured provider rank results by relevance.
    #[default]
    Relevance,
    /// Put the newest available uploads first.
    Newest,
}

/// Ordering applied to entries in the Local file browser.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LocalSizeSort {
    /// Keep the normal directories-first, name ordering.
    #[default]
    Off,
    /// Put known smaller files or folders first.
    Ascending,
    /// Put known larger files or folders first.
    Descending,
}

impl LocalSizeSort {
    /// Returns the next state in the Local size-sort control.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Off => Self::Ascending,
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Off,
        }
    }

    /// Returns the compact label rendered beside the `Z` hotkey.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Size sort: off",
            Self::Ascending => "Size sort: ascending",
            Self::Descending => "Size sort: descending",
        }
    }
}

/// Submitted provider search whose progress is animated in the result panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchActivity {
    /// A video or channel search through the configured `YouTube` provider.
    YouTube,
    /// A music-focused search through `yt-dlp` and `music.youtube.com`.
    YouTubeMusic,
    /// An aggregate search through the enabled MOD/tracker archives.
    TrackerArchives,
}

impl SearchActivity {
    /// Returns the result screen that owns this submitted search.
    const fn screen(self) -> Screen {
        match self {
            Self::YouTube => Screen::Search,
            Self::YouTubeMusic => Screen::YouTubeMusic,
            Self::TrackerArchives => Screen::TrackerMusic,
        }
    }
}

/// UI color and visibility preferences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiSettings {
    /// Display shortcut labels inside buttons.
    pub show_hotkeys: bool,
    /// Use the humorous DOS-RPG-inspired border palette.
    pub funny_mode: bool,
    /// Seek-bar rendering mode.
    pub seek_bar_style: SeekBarStyle,
    /// Whether supported terminals may fetch and render thumbnails.
    pub thumbnails: ThumbnailMode,
    /// Maximum thumbnail height in terminal rows.
    pub thumbnail_height: u16,
    /// Prefetch artwork for all currently loaded global Search rows.
    pub prefetch_search_thumbnails: bool,
    /// Persistent thumbnail byte cache selected by the loaded configuration.
    pub thumbnail_cache_dir: Option<PathBuf>,
    /// Redraw period while playback is active.
    pub playing_tick: Duration,
    /// Redraw period while idle or paused.
    pub idle_tick: Duration,
}

/// Maximum event-loop sleep while one foreground Local listing is pending.
///
/// The tighter interval applies only for the short lifetime of a directory
/// request. Idle browsing retains [`UiSettings::idle_tick`], so an open Local
/// tab does not continuously poll or waste battery.
const LOCAL_BROWSE_RESPONSE_POLL_INTERVAL: Duration = Duration::from_millis(25);

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            show_hotkeys: true,
            funny_mode: false,
            seek_bar_style: SeekBarStyle::Line,
            thumbnails: ThumbnailMode::Auto,
            thumbnail_height: DEFAULT_THUMBNAIL_HEIGHT,
            prefetch_search_thumbnails: true,
            thumbnail_cache_dir: None,
            playing_tick: Duration::from_millis(250),
            idle_tick: Duration::from_secs(1),
        }
    }
}

/// One row shown in a list panel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowView {
    /// Stable media identity used to mark the authoritative playing item.
    pub media_id: Option<MediaId>,
    /// Primary row label.
    pub title: String,
    /// Source-specific secondary label.
    pub subtitle: String,
    /// Source name used for color distinction.
    pub source: String,
    /// Local watched percentage.
    pub watched_percent: u8,
    /// Whether the source is locally subscribed.
    pub subscribed: bool,
    /// Preferred artwork URL available for selected rendering or prefetch.
    pub thumbnail_url: Option<url::Url>,
    /// Whether provider metadata identifies this as a vertical video.
    pub vertical: bool,
    /// Omit generic source and marker padding on a source-specific screen.
    pub compact: bool,
}

/// Details for the selected media item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DetailView {
    /// Stable identity of the selected media used by description timecodes.
    pub media_id: Option<MediaId>,
    /// Display title.
    pub title: String,
    /// Source/provider label.
    pub source: String,
    /// Display name of the channel that published the selected media.
    pub channel_name: String,
    /// Stable provider channel identifier used by local subscriptions.
    pub channel_id: String,
    /// Exact public channel page opened by the channel browser action.
    pub channel_webpage_url: Option<url::Url>,
    /// Whether the channel is present in Youta's local OPML subscriptions.
    pub channel_subscribed: bool,
    /// Public channel subscriber count, when exposed by the provider.
    pub channel_subscriber_count: Option<u64>,
    /// Public channel video count, when exposed by the provider.
    pub channel_video_count: Option<u64>,
    /// Aggregate public channel view count, when exposed by the provider.
    pub channel_total_view_count: Option<u64>,
    /// Human-readable channel creation/joined date.
    pub channel_created: String,
    /// Provider-supplied channel country code or label.
    pub channel_country: String,
    /// Whether additional channel profile links exceeded a safety bound.
    pub channel_links_truncated: bool,
    /// Human-readable length.
    pub length: String,
    /// Description text.
    pub description: String,
    /// Parsed timecode spans that can seek or start the selected media.
    pub timecodes: Vec<DetailTimecodeView>,
    /// Parsed `YouTube` video URLs that may replace Details internally.
    pub video_links: Vec<DetailVideoLinkView>,
    /// Public like count, formatted by the provider.
    pub likes: String,
    /// Public view count, formatted by the provider.
    pub views: String,
    /// Publication date.
    pub published: String,
    /// Provider-reported license.
    pub license: String,
    /// Lazy Wikidata lookup state or item link.
    pub wikidata: String,
    /// Selectable external links associated with this media item or channel.
    pub links: Vec<DetailLinkView>,
    /// Wikidata item whose property spoiler is currently expanded.
    pub expanded_wikidata_item: Option<String>,
    /// Wikidata item whose property request is in flight.
    pub loading_wikidata_item: Option<String>,
    /// Bounded, already-formatted Wikidata property spoilers.
    pub wikidata_entities: Vec<DetailWikidataEntityView>,
    /// Remote thumbnail URL consumed by the optional image renderer.
    ///
    /// The URL is never rendered as text. Unsupported terminals omit the
    /// image without fetching it.
    pub thumbnail_url: Option<url::Url>,
    /// Whether the selected local entry can be renamed in place.
    pub local_renamable: bool,
    /// Whether the selected local entry can be moved to recoverable Trash.
    pub local_trashable: bool,
}

/// Current route inside the Subscriptions screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SubscriptionRoute {
    /// Selectable OPML sources.
    #[default]
    Sources,
    /// Videos belonging to the activated source.
    Items,
}

/// Pane receiving list navigation in split Subscriptions mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SubscriptionPane {
    /// The source list.
    #[default]
    Sources,
    /// The selected source's media list.
    Items,
}

/// Render-ready state owned by the Subscriptions screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubscriptionsView {
    /// Configured navigation model.
    pub layout: SubscriptionsLayout,
    /// Active drill-down route.
    pub route: SubscriptionRoute,
    /// Selectable OPML sources in stable folder order.
    pub sources: Vec<RowView>,
    /// Selected source index.
    pub selected_source: usize,
    /// Videos loaded for the selected source.
    pub items: Vec<RowView>,
    /// Selected video index.
    pub selected_item: usize,
    /// List pane receiving `j`, `k`, and Enter in split mode.
    pub focus: SubscriptionPane,
    /// Whether split mode temporarily shows the selected item's Details.
    pub description_expanded: bool,
    /// Whether the selected source has a provider request in flight.
    pub loading: bool,
    /// Human-readable source name included in the item-list heading.
    pub source_title: String,
    /// Public subscriber count for the selected source, when exposed.
    pub source_subscriber_count: Option<u64>,
    /// Human-readable channel creation date, when exposed.
    pub source_created: String,
}

impl Default for SubscriptionsView {
    fn default() -> Self {
        Self {
            layout: SubscriptionsLayout::default(),
            route: SubscriptionRoute::Sources,
            sources: Vec::new(),
            selected_source: 0,
            items: Vec::new(),
            selected_item: 0,
            focus: SubscriptionPane::Sources,
            description_expanded: false,
            loading: false,
            source_title: String::new(),
            source_subscriber_count: None,
            source_created: String::new(),
        }
    }
}

/// Focused in-app editor for preferences that are implemented at runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreferencesPopupView {
    /// Draft Subscriptions layout saved only when the user confirms.
    pub subscriptions_layout: SubscriptionsLayout,
    /// Draft advertisement-chapter behavior saved only on confirmation.
    pub skip_advertisement_chapters: bool,
    /// Draft selected-video `YouTube` prewarming saved only on confirmation.
    pub youtube_prewarm: bool,
    /// Draft lazy Local-folder size behavior saved only on confirmation.
    pub show_local_folder_sizes: bool,
    /// Exact private TOML file updated by the save action.
    pub config_path: String,
    /// Environment variable shadowing the TOML value, when present.
    pub environment_override: Option<String>,
    /// Save or validation failure kept inside the popup.
    pub validation_error: Option<String>,
}

/// Explicit local-file mutation awaiting text input or confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalFilePopupView {
    /// Same-directory rename with an editable basename.
    Rename {
        /// Draft basename.
        value: String,
        /// UTF-8 byte offset at a grapheme boundary where edits are applied.
        cursor_byte: usize,
        /// Validation or filesystem failure retained in the popup.
        error: Option<String>,
    },
    /// Recoverable move-to-Trash confirmation.
    Trash {
        /// Exact display basename being removed from the current folder.
        name: String,
        /// Full source path passed to the system Trash backend after confirmation.
        path: String,
        /// Filesystem failure retained in the popup.
        error: Option<String>,
    },
}

/// One selectable timecode span inside the original Details description.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DetailTimecodeView {
    /// Inclusive UTF-8 byte offset in [`DetailView::description`].
    pub start_byte: usize,
    /// Exclusive UTF-8 byte offset in [`DetailView::description`].
    pub end_byte: usize,
    /// Absolute playback destination in seconds.
    pub seconds: u64,
    /// Whether this timestamp starts a parsed line-leading chapter.
    pub is_chapter: bool,
}

/// One `YouTube` video URL followed by an internal-navigation action.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DetailVideoLinkView {
    /// Inclusive UTF-8 byte offset of the source URL.
    pub start_byte: usize,
    /// Exclusive UTF-8 byte offset of the source URL.
    pub end_byte: usize,
    /// Stable eleven-character `YouTube` video identifier.
    pub video_id: String,
    /// Optional initial position encoded in the URL.
    pub start_seconds: Option<u64>,
}

/// One selectable external link displayed in a details or channel panel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DetailLinkView {
    /// Human-readable link label, such as a Wikidata item name.
    pub label: String,
    /// Absolute URL passed to the controller when the link is activated.
    pub url: String,
    /// Exact Wikidata Q-ID when this link owns a lazy property spoiler.
    pub wikidata_item_id: Option<String>,
}

/// Bounded human-facing Wikidata properties cached for one Details page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DetailWikidataEntityView {
    /// Exact Wikidata Q-ID represented by this spoiler.
    pub item_id: String,
    /// Preformatted, scrollable property/value text.
    pub text: String,
    /// Item-valued statement spans that open canonical Wikidata pages.
    pub value_links: Vec<DetailWikidataValueLinkView>,
}

/// One clickable item-valued statement inside expanded Wikidata properties.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DetailWikidataValueLinkView {
    /// Inclusive UTF-8 byte offset in [`DetailWikidataEntityView::text`].
    pub start_byte: usize,
    /// Exclusive UTF-8 byte offset in [`DetailWikidataEntityView::text`].
    pub end_byte: usize,
    /// Validated Wikidata Q-ID used to construct the canonical target.
    pub item_id: String,
}

/// One terminal-cell position inside the visible, selectable Details text.
///
/// Rows are semantic selectable rows, not absolute terminal rows. This keeps
/// the selection confined to rendered metadata, links, and description text
/// while excluding the other pane, pane headings, controls, and thumbnail cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct DetailsTextPosition {
    /// Zero-based selectable row in the currently visible Details content.
    pub row: usize,
    /// Zero-based terminal-cell column inside that row.
    pub column: usize,
}

/// Current Youta-owned drag selection in the Details panel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DetailsTextSelection {
    /// Position where the current drag started.
    pub anchor: DetailsTextPosition,
    /// Most recent clipped pointer position.
    pub focus: DetailsTextPosition,
    /// Whether a left-button drag is still in progress.
    pub dragging: bool,
}

/// Diagnostic information shown above the normal interface after an error.
///
/// `report` contains the complete, copyable diagnostic report rather than a
/// shortened user-facing message. The controller owns `scroll_offset` so the
/// position survives terminal redraws and resize events.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ErrorPopupView {
    /// Short error title displayed in the popup border.
    pub title: String,
    /// Complete report, including the stack trace and environment information.
    pub report: String,
    /// Zero-based wrapped-line offset at the top of the viewport.
    pub scroll_offset: usize,
    /// Whether the GitHub CLI is available for pre-filling a new issue.
    pub gh_available: bool,
    /// Result of the most recent copy or issue-review action.
    pub action_status: Option<String>,
}

/// Editable setup shown when a YouTube search needs provider credentials.
///
/// The API key remains in controller-owned memory while the popup is open.
/// Rendering always masks it, including in test and alternate-screen buffers.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct YouTubeSetupPopupView {
    /// Input currently receiving keyboard characters.
    pub selected_field: YouTubeSetupField,
    /// YouTube Data API key. The renderer never displays this value directly.
    pub api_key: String,
    /// Base URL of a user-selected Invidious instance.
    pub invidious_url: String,
    /// Exact configuration path where a submitted value will be stored.
    pub config_path: String,
    /// Actionable validation or provider-construction failure, when present.
    pub validation_error: Option<String>,
}

impl std::fmt::Debug for YouTubeSetupPopupView {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("YouTubeSetupPopupView")
            .field("selected_field", &self.selected_field)
            .field("api_key", &"[REDACTED]")
            .field("invidious_url", &self.invidious_url)
            .field("config_path", &self.config_path)
            .field("validation_error", &self.validation_error)
            .finish()
    }
}

/// Selectable credential field in the YouTube provider setup popup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum YouTubeSetupField {
    /// Official YouTube Data API key.
    #[default]
    ApiKey,
    /// Invidious instance base URL.
    InvidiousUrl,
}

/// Progress and completion information for one supervised media download.
///
/// Only one download can be active at a time. A completed view remains visible
/// until another download starts so the destination path is easy to inspect.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DownloadView {
    /// Human-readable title of the selected remote media.
    pub title: String,
    /// Bytes written so far.
    pub downloaded_bytes: u64,
    /// Exact or extractor-estimated total byte count.
    pub total_bytes: Option<u64>,
    /// Current transfer speed, rounded to bytes per second.
    pub bytes_per_second: Option<u64>,
    /// Estimated seconds remaining.
    pub eta_seconds: Option<u64>,
    /// Whether the supervised child process is still running.
    pub active: bool,
    /// Confined final media path reported after post-processing.
    pub completed_path: Option<String>,
}

/// Complete immutable view rendered for one frame.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewModel {
    /// Active screen.
    pub screen: Screen,
    /// Whether text typed by the user edits the search query.
    pub search_editing: bool,
    /// Current search query.
    pub search_query: String,
    /// Canonical folder displayed by the Local screen.
    pub local_path: String,
    /// Whether the default YouTube search targets videos or channels.
    pub search_kind: SearchKind,
    /// Ordering applied to every page of the current YouTube search.
    pub youtube_search_sort: YouTubeSearchSort,
    /// Whether `YouTube` video searches require a Creative Commons licence.
    ///
    /// Channel searches retain this preference but do not send a video-only
    /// licence filter to the configured provider.
    pub youtube_creative_commons_only: bool,
    /// Submitted provider search that has not reached a terminal response.
    pub search_activity: Option<SearchActivity>,
    /// Whether a foreground Local directory listing is awaiting its response.
    pub local_browse_pending: bool,
    /// Whether selected Local artwork is awaiting background extraction.
    pub local_artwork_pending: bool,
    /// Monotonic frame counter for the active ASCII search animation.
    pub search_animation_frame: usize,
    /// Whether accepted media is waiting for authoritative playback start.
    pub playback_starting: bool,
    /// Monotonic frame counter for the ASCII playback-start animation.
    pub playback_start_animation_frame: usize,
    /// Media whose backend emitted `PlaybackStarted`, including while paused.
    pub playing_media_id: Option<MediaId>,
    /// Rows on the active screen.
    pub rows: Vec<RowView>,
    /// Selected row index.
    pub selected: usize,
    /// Selected item details.
    pub details: Option<DetailView>,
    /// Dedicated Subscriptions navigation and list state.
    pub subscriptions: SubscriptionsView,
    /// Whether Youta confines mouse-drag selection to visible Details text.
    pub text_selection_mode: bool,
    /// Active or most recently copied Details text range.
    pub details_text_selection: Option<DetailsTextSelection>,
    /// Whether the right-hand Details or Channel panel has explicit focus.
    pub details_focused: bool,
    /// Requested vertical scroll offset inside the focused details text.
    ///
    /// The renderer clamps this value to the current wrapped content and
    /// viewport, which can change after a resize.
    pub details_scroll: usize,
    /// Selected external-link index, or `None` before link navigation begins.
    pub selected_detail_link: Option<usize>,
    /// Selected right-panel mode.
    pub right_panel_mode: RightPanelMode,
    /// Waveform peaks chosen for the current terminal width.
    pub waveform: Vec<Peak>,
    /// Current player state.
    pub playback: PlaybackStatus,
    /// Chapters inferred for the authoritative playing media.
    pub playback_chapters: Vec<Chapter>,
    /// Whether chapter labels include their timestamps.
    pub show_chapter_timestamps: bool,
    /// Whether exact `Реклама` chapters are hidden from navigation and skipped.
    pub skip_advertisement_chapters: bool,
    /// Local file-browser ordering by known file and lazy folder sizes.
    pub local_size_sort: LocalSizeSort,
    /// Whether recursive Local-folder sizes and size ordering are available.
    pub local_folder_sizes_enabled: bool,
    /// Whether EOF continues with the next playable same-source list entry.
    pub autoplay: bool,
    /// Repeat-current-item state.
    pub repeating: bool,
    /// Status or error message.
    pub status_line: String,
    /// Whether the help overlay is open.
    pub help_open: bool,
    /// Scrollable diagnostic popup, when a recoverable error is being reported.
    pub error_popup: Option<ErrorPopupView>,
    /// Editable provider setup shown after an unavailable YouTube operation.
    pub youtube_setup_popup: Option<YouTubeSetupPopupView>,
    /// Focused runtime preferences editor.
    pub preferences_popup: Option<PreferencesPopupView>,
    /// Explicit rename or move-to-Trash confirmation for a local file.
    pub local_file_popup: Option<LocalFilePopupView>,
    /// Active or most recently completed supervised download.
    pub download: Option<DownloadView>,
    /// Whether the controller has requested application shutdown.
    pub quitting: bool,
}

impl Default for ViewModel {
    fn default() -> Self {
        Self {
            screen: Screen::Search,
            search_editing: false,
            search_query: String::new(),
            local_path: String::new(),
            search_kind: SearchKind::Videos,
            youtube_search_sort: YouTubeSearchSort::Relevance,
            youtube_creative_commons_only: false,
            search_activity: None,
            local_browse_pending: false,
            local_artwork_pending: false,
            search_animation_frame: 0,
            playback_starting: false,
            playback_start_animation_frame: 0,
            playing_media_id: None,
            rows: Vec::new(),
            selected: 0,
            details: None,
            subscriptions: SubscriptionsView::default(),
            text_selection_mode: false,
            details_text_selection: None,
            details_focused: false,
            details_scroll: 0,
            selected_detail_link: None,
            right_panel_mode: RightPanelMode::Details,
            waveform: Vec::new(),
            playback: PlaybackStatus::default(),
            playback_chapters: Vec::new(),
            show_chapter_timestamps: true,
            skip_advertisement_chapters: true,
            local_size_sort: LocalSizeSort::Off,
            local_folder_sizes_enabled: true,
            autoplay: false,
            repeating: false,
            status_line: "Press / to search or ? for help".to_owned(),
            help_open: false,
            error_popup: None,
            youtube_setup_popup: None,
            preferences_popup: None,
            local_file_popup: None,
            download: None,
            quitting: false,
        }
    }
}

/// Relative or absolute movement inside a diagnostic error report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorPopupScroll {
    /// Move by a signed number of wrapped text lines.
    Lines(i32),
    /// Move by a signed number of visible pages.
    Pages(i32),
    /// Jump to the beginning of the report.
    Home,
    /// Jump to the end of the report.
    End,
}

/// Relative or absolute movement inside the Details panel text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetailsScroll {
    /// Move by a signed number of wrapped text lines.
    Lines(i32),
    /// Move by a signed number of text pages.
    Pages(i32),
    /// Jump to the beginning of the text.
    Home,
    /// Jump to the end of the text.
    End,
}

/// Semantic action emitted by keyboard or mouse input.
#[derive(Clone, Debug, PartialEq)]
pub enum UiAction {
    /// Exit Youta after saving state.
    Quit,
    /// Open or close the help overlay.
    ToggleHelp,
    /// Switch to a top-level screen.
    ShowScreen(Screen),
    /// Enter search-query editing mode.
    BeginSearch,
    /// Cancel search-query editing.
    CancelSearch,
    /// Add one character to the query.
    AppendSearch(char),
    /// Remove the last query character.
    DeleteSearchCharacter,
    /// Submit the current query.
    SubmitSearch,
    /// Switch the default YouTube search between videos and channels.
    ToggleSearchKind,
    /// Switch YouTube search between relevance and newest-first ordering.
    ToggleYouTubeSearchSort,
    /// Restrict `YouTube` video search to Creative Commons-licensed results.
    ToggleYouTubeCreativeCommons,
    /// Move list selection by a signed row count.
    MoveSelection(i32),
    /// Select an exact row.
    SelectRow(usize),
    /// Activate the selected row.
    ActivateSelection,
    /// Select the active queue item and show its description.
    ShowNowPlaying,
    /// Move the external-link selection by a signed row count.
    MoveDetailLink(i32),
    /// Select an exact external link without opening it.
    SelectDetailLink(usize),
    /// Ask the controller to open an exact external link.
    ActivateDetailLink(usize),
    /// Expand or collapse lazy Wikidata properties for an external-link row.
    ToggleWikidataStatements(usize),
    /// Open one item-valued Wikidata statement through its validated Q-ID.
    OpenWikidataItem(String),
    /// Give or remove explicit keyboard focus from the Details panel.
    SetDetailsFocus(bool),
    /// Scroll the focused or pointer-targeted Details panel.
    ScrollDetails(DetailsScroll),
    /// Set the Details panel to an exact renderer-clamped wrapped-line offset.
    SetDetailsScroll(usize),
    /// Toggle Youta-owned mouse selection for text in the Details panel.
    ToggleTextSelectionMode,
    /// Start a text selection at an exact visible Details position.
    BeginDetailsTextSelection(DetailsTextPosition),
    /// Extend the current selection to a clipped visible Details position.
    UpdateDetailsTextSelection(DetailsTextPosition),
    /// Finish the selection and copy exactly the supplied visible text.
    FinishDetailsTextSelection {
        /// Final clipped position.
        focus: DetailsTextPosition,
        /// Bounded text reconstructed from selectable Details rows.
        text: String,
    },
    /// Subscribe to or unsubscribe from the displayed channel in local OPML.
    ToggleSubscription,
    /// Toggle pause in the invisible playback backend.
    TogglePause,
    /// Seek by a signed number of seconds.
    SeekRelative(i64),
    /// Seek to a percentage from `0.0` to `100.0`.
    SeekPercent(f64),
    /// Seek within, or start, the media owning a description timecode.
    ActivateTimecode {
        /// Media identity captured when the timecode was rendered.
        media_id: MediaId,
        /// Absolute playback destination in seconds.
        seconds: u64,
    },
    /// Replace Details with a `YouTube` video referenced by its description.
    ActivateDescriptionVideo {
        /// Stable eleven-character `YouTube` video identifier.
        video_id: String,
        /// Optional initial position encoded in the source URL.
        start_seconds: Option<u64>,
    },
    /// Change volume by a signed percentage.
    ChangeVolume(i8),
    /// Change playback speed by a signed multiplier step.
    ChangeSpeed(f64),
    /// Select the previous or next chapter.
    ChangeChapter(i32),
    /// Toggle timestamps inside chapter labels without changing seek targets.
    ToggleChapterTimestamps,
    /// Toggle repeat-current-item.
    ToggleRepeat,
    /// Toggle automatic continuation within the active source list.
    ToggleAutoplay,
    /// Cycle Local entry ordering through off, ascending, and descending size.
    ToggleLocalSizeSort,
    /// Toggle between details and waveform.
    ToggleWaveform,
    /// Show information about the playing channel.
    ShowChannel,
    /// Return to the previous internal Details page or seek position.
    GoBack,
    /// Move forward to a Details page previously left with [`Self::GoBack`].
    GoForward,
    /// Queue the selected item immediately after the current item.
    PlayNext,
    /// Add the selected item to the current queue.
    AddToQueue,
    /// Download the selected item.
    Download,
    /// Open the canonical item link in a browser.
    OpenInBrowser,
    /// Open the selected item's exact channel page in a browser.
    OpenChannelInBrowser,
    /// Copy the canonical item link.
    CopyLink,
    /// Edit a private local note.
    EditPrivateNote,
    /// Open equalizer controls.
    OpenEqualizer,
    /// Close the diagnostic error popup without changing the underlying screen.
    DismissErrorPopup,
    /// Scroll the diagnostic report.
    ScrollErrorPopup(ErrorPopupScroll),
    /// Copy the complete diagnostic report.
    CopyErrorReport,
    /// Ask the GitHub CLI to open a pre-filled issue without submitting it.
    FillGitHubIssue,
    /// Copy the report and open the repository's new-issue page.
    CopyAndOpenGitHubIssue,
    /// Select the credential field edited by the YouTube setup popup.
    SelectYouTubeSetupField(YouTubeSetupField),
    /// Add one printable character to the selected YouTube setup field.
    AppendYouTubeSetupCharacter(char),
    /// Remove the last character from the selected YouTube setup field.
    DeleteYouTubeSetupCharacter,
    /// Open Google's official `YouTube` API-key setup guide.
    OpenYouTubeApiKeyGuide,
    /// Open Google Cloud's API Credentials page.
    OpenGoogleCloudCredentials,
    /// Open the official Invidious public-instance list.
    OpenInvidiousInstances,
    /// Validate and save the selected YouTube provider configuration.
    SubmitYouTubeSetup,
    /// Close the YouTube setup popup without saving.
    DismissYouTubeSetup,
    /// Open the focused runtime preferences editor.
    OpenPreferences,
    /// Select one draft Subscriptions layout in the preferences editor.
    SetSubscriptionsLayout(SubscriptionsLayout),
    /// Toggle hiding and skipping exact `Реклама` chapters in the draft.
    ToggleSkipAdvertisementChapters,
    /// Toggle selected-video YouTube prewarming in the draft.
    ToggleYouTubePrewarm,
    /// Toggle lazy recursive Local-folder size measurement in the draft.
    ToggleLocalFolderSizes,
    /// Persist the draft preference and close the editor.
    SubmitPreferences,
    /// Close the preferences editor without saving.
    DismissPreferences,
    /// Open a basename editor for the selected regular local file.
    BeginLocalRename,
    /// Add one printable character to the local rename basename.
    AppendLocalRenameCharacter(char),
    /// Move the local rename cursor by one signed grapheme.
    MoveLocalRenameCursor(i8),
    /// Remove the grapheme immediately before the local rename cursor.
    DeleteLocalRenameCharacter,
    /// Validate and execute the local rename.
    SubmitLocalRename,
    /// Ask for confirmation before moving the selected local entry to Trash.
    RequestLocalTrash,
    /// Move the selected local entry to recoverable system Trash.
    ConfirmLocalTrash,
    /// Close the local-entry popup without changing the filesystem.
    DismissLocalFilePopup,
    /// Select an exact subscription source row.
    SelectSubscriptionSource(usize),
    /// Select an exact subscription item row.
    SelectSubscriptionItem(usize),
    /// Toggle the selected subscription item's expanded description.
    ToggleSubscriptionDescription,
    /// Refresh page one for the active subscribed channel.
    RefreshSubscriptionVideos,
}

/// Controller used by the generic terminal event loop.
pub trait UiController {
    /// Returns the view for the next frame.
    fn view(&self) -> &ViewModel;

    /// Applies one semantic user action.
    fn dispatch(&mut self, action: UiAction);

    /// Polls background workers and playback state.
    fn tick(&mut self);
}

trait ThumbnailRenderer {
    fn poll(&mut self) -> bool;
    fn is_enabled(&self) -> bool;
    fn is_pending(&self) -> bool {
        false
    }
    fn needs_immediate_redraw(&self) -> bool {
        false
    }
    fn synchronize(&mut self, source: Option<&url::Url>, area: Rect) -> bool;
    /// Replaces the cache-only backlog for artwork rows selected by the TUI.
    fn synchronize_prefetch(&mut self, _rows: &[RowView]) -> bool {
        false
    }
    fn clear(&mut self) -> bool;
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme);
}

#[cfg(feature = "thumbnails")]
struct TerminalThumbnailRenderer {
    manager: ThumbnailManager,
    clear_before_ready: bool,
    followup_frame_pending: bool,
    visible_source: Option<url::Url>,
    prefetched_visible_source: Option<url::Url>,
    prefetch_sources: Vec<url::Url>,
}

#[cfg(feature = "thumbnails")]
impl TerminalThumbnailRenderer {
    /// Wraps the asynchronous manager with terminal-frame transition state.
    fn new(manager: ThumbnailManager) -> Self {
        Self {
            manager,
            clear_before_ready: false,
            followup_frame_pending: false,
            visible_source: None,
            prefetched_visible_source: None,
            prefetch_sources: Vec::new(),
        }
    }
}

#[cfg(feature = "thumbnails")]
impl ThumbnailRenderer for TerminalThumbnailRenderer {
    fn poll(&mut self) -> bool {
        let changed = self.manager.poll();
        if changed {
            self.clear_before_ready = self.manager.state() == &ThumbnailState::Ready;
            self.followup_frame_pending = false;
        }
        changed
    }

    fn is_enabled(&self) -> bool {
        self.manager.is_enabled()
    }

    fn is_pending(&self) -> bool {
        self.manager.state() == &ThumbnailState::Loading
    }

    fn needs_immediate_redraw(&self) -> bool {
        self.followup_frame_pending
    }

    fn synchronize(&mut self, source: Option<&url::Url>, area: Rect) -> bool {
        self.visible_source = source.cloned();
        let changed = self.manager.synchronize(source, area);
        if changed {
            self.clear_before_ready = false;
            self.followup_frame_pending = false;
        }
        changed
    }

    fn synchronize_prefetch(&mut self, rows: &[RowView]) -> bool {
        let sources_unchanged = self
            .prefetch_sources
            .iter()
            .eq(rows.iter().filter_map(|row| row.thumbnail_url.as_ref()));
        if sources_unchanged && self.prefetched_visible_source == self.visible_source {
            return false;
        }
        self.prefetch_sources.clear();
        self.prefetch_sources.extend(
            rows.iter()
                .filter_map(|row| row.thumbnail_url.as_ref())
                .cloned(),
        );
        self.prefetched_visible_source
            .clone_from(&self.visible_source);
        self.manager.synchronize_prefetch(&self.prefetch_sources)
    }

    fn clear(&mut self) -> bool {
        self.visible_source = None;
        self.clear_before_ready = false;
        self.followup_frame_pending = false;
        self.manager.clear()
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        match self.manager.state().clone() {
            ThumbnailState::Loading => {
                frame.render_widget(
                    Paragraph::new("Loading thumbnail…")
                        .style(theme.muted)
                        .alignment(Alignment::Center),
                    area,
                );
            }
            ThumbnailState::Ready => {
                if self.clear_before_ready {
                    // Image protocols mark their cells as skipped so ratatui does
                    // not overwrite the pixels. A dedicated ordinary frame first
                    // erases the loading label that would otherwise remain in the
                    // terminal's previous buffer underneath those skipped cells.
                    self.clear_before_ready = false;
                    self.followup_frame_pending = true;
                } else if let Some(protocol) = self.manager.protocol_mut() {
                    frame.render_stateful_widget(TerminalImage::default(), area, protocol);
                    self.followup_frame_pending = false;
                }
            }
            ThumbnailState::Failed(error) => {
                self.followup_frame_pending = false;
                frame.render_widget(
                    Paragraph::new(format!("Thumbnail unavailable: {error}"))
                        .style(theme.muted)
                        .alignment(Alignment::Center)
                        .wrap(Wrap { trim: true }),
                    area,
                );
            }
            ThumbnailState::Disabled | ThumbnailState::Unsupported | ThumbnailState::Idle => {
                self.followup_frame_pending = false;
            }
        }
    }
}

#[cfg(feature = "thumbnails")]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the no-thumbnails build returns None through the same interface"
)]
fn create_thumbnail_renderer(settings: &UiSettings) -> Option<Box<dyn ThumbnailRenderer>> {
    let manager = settings.thumbnail_cache_dir.as_ref().map_or_else(
        || ThumbnailManager::from_current_terminal(settings.thumbnails),
        |cache_dir| {
            ThumbnailManager::from_current_terminal_with_cache(
                settings.thumbnails,
                cache_dir.clone(),
            )
        },
    );
    Some(Box::new(TerminalThumbnailRenderer::new(manager)))
}

#[cfg(not(feature = "thumbnails"))]
fn create_thumbnail_renderer(_settings: &UiSettings) -> Option<Box<dyn ThumbnailRenderer>> {
    None
}

/// Runs Youta in the current terminal until the controller requests shutdown.
pub fn run(controller: &mut impl UiController, settings: &UiSettings) -> io::Result<()> {
    if !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "Youta's interactive UI requires a terminal",
        ));
    }

    let mut session = TerminalSession::enter()?;
    let mut input = TerminalInput::new();
    let mut thumbnail_renderer = create_thumbnail_renderer(settings);
    let mut hit_map = HitMap::default();
    let mut virtual_cursor = VirtualCursor::default();
    loop {
        let mut renderer = thumbnail_renderer.take();
        if let Some(renderer) = renderer.as_mut() {
            renderer.poll();
            session.terminal.draw(|frame| {
                render_frame(
                    frame,
                    controller.view(),
                    settings,
                    &mut hit_map,
                    Some(renderer.as_mut()),
                );
                virtual_cursor.render(frame);
            })?;
        } else {
            session.terminal.draw(|frame| {
                render_frame(frame, controller.view(), settings, &mut hit_map, None);
                virtual_cursor.render(frame);
            })?;
        }
        if let Some(renderer) = renderer.as_deref_mut() {
            synchronize_thumbnail_prefetch(controller.view(), settings, renderer);
        }
        if controller.view().quitting {
            break;
        }

        let wait = event_wait(controller.view(), settings);
        let wait_outcome = wait_for_event_or_thumbnail(wait, renderer.as_deref_mut(), |timeout| {
            input.poll(timeout)
        })?;
        if wait_outcome == WaitOutcome::TerminalEvent {
            match input.read()? {
                Event::Key(key) => match virtual_cursor.handle_key(key) {
                    VirtualCursorKey::PassThrough => {
                        if let Some(action) = key_action(key, controller.view()) {
                            controller.dispatch(action);
                        }
                    }
                    VirtualCursorKey::Click(mouse) => {
                        if let Some(action) = mouse_action(mouse, &hit_map, controller.view()) {
                            controller.dispatch(action);
                        }
                    }
                    VirtualCursorKey::Consumed => {}
                },
                Event::Mouse(mouse) => {
                    if let Some(action) = mouse_action(mouse, &hit_map, controller.view()) {
                        controller.dispatch(action);
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        controller.tick();
        thumbnail_renderer = renderer;
    }
    Ok(())
}

/// Input source that opportunistically adds GPM without changing PTY behavior.
struct TerminalInput {
    #[cfg(all(feature = "gpm", target_os = "linux"))]
    linux_console: Option<LinuxConsoleInput>,
}

impl TerminalInput {
    fn new() -> Self {
        Self {
            #[cfg(all(feature = "gpm", target_os = "linux"))]
            linux_console: LinuxConsoleInput::try_current(),
        }
    }

    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        #[cfg(all(feature = "gpm", target_os = "linux"))]
        if let Some(input) = self.linux_console.as_mut() {
            let Ok(ready) = input.poll(timeout) else {
                // GPM is optional and may stop while Youta is running. Drop
                // the failed socket and retain keyboard input without
                // presenting an application error.
                self.linux_console = None;
                return event::poll(Duration::ZERO);
            };
            return Ok(ready);
        }
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
        #[cfg(all(feature = "gpm", target_os = "linux"))]
        if let Some(input) = self.linux_console.as_mut() {
            return input.read();
        }
        event::read()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VirtualCursorKey {
    PassThrough,
    Consumed,
    Click(MouseEvent),
}

/// Keyboard-controlled pointer used when no physical mouse input is available.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct VirtualCursor {
    active: bool,
    column: u16,
    row: u16,
    bounds: Rect,
}

impl VirtualCursor {
    fn handle_key(&mut self, key: KeyEvent) -> VirtualCursorKey {
        if key.code == KeyCode::F(8) {
            if key.kind == KeyEventKind::Press {
                self.active = !self.active;
                if self.active {
                    self.column = self
                        .bounds
                        .x
                        .saturating_add(self.bounds.width.saturating_sub(1) / 2);
                    self.row = self
                        .bounds
                        .y
                        .saturating_add(self.bounds.height.saturating_sub(1) / 2);
                }
            }
            return VirtualCursorKey::Consumed;
        }
        if !self.active {
            return VirtualCursorKey::PassThrough;
        }
        if key.kind == KeyEventKind::Release
            && matches!(
                key.code,
                KeyCode::Esc
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Enter
            )
        {
            return VirtualCursorKey::Consumed;
        }
        match key.code {
            KeyCode::Esc => {
                self.active = false;
                VirtualCursorKey::Consumed
            }
            KeyCode::Left => {
                self.column = self.column.saturating_sub(1).max(self.bounds.x);
                VirtualCursorKey::Consumed
            }
            KeyCode::Right => {
                self.column = self
                    .column
                    .saturating_add(1)
                    .min(self.bounds.right().saturating_sub(1));
                VirtualCursorKey::Consumed
            }
            KeyCode::Up => {
                self.row = self.row.saturating_sub(1).max(self.bounds.y);
                VirtualCursorKey::Consumed
            }
            KeyCode::Down => {
                self.row = self
                    .row
                    .saturating_add(1)
                    .min(self.bounds.bottom().saturating_sub(1));
                VirtualCursorKey::Consumed
            }
            KeyCode::Enter => VirtualCursorKey::Click(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: self.column,
                row: self.row,
                modifiers: KeyModifiers::NONE,
            }),
            _ => VirtualCursorKey::PassThrough,
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        self.bounds = frame.area();
        if self.bounds.is_empty() {
            self.active = false;
            return;
        }
        self.column = self
            .column
            .clamp(self.bounds.x, self.bounds.right().saturating_sub(1));
        self.row = self
            .row
            .clamp(self.bounds.y, self.bounds.bottom().saturating_sub(1));
        if !self.active {
            return;
        }
        let cell = &mut frame.buffer_mut()[(self.column, self.row)];
        if cell.symbol().trim().is_empty() {
            cell.set_symbol("■");
        }
        cell.set_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .add_modifier(Modifier::REVERSED),
        );
    }
}

/// Synchronizes cache-only artwork work with the visible screen.
///
/// Global search warming follows its explicit preference. Subscription source
/// artwork is restored only from previously cached channel metadata and is
/// always warmed so moving between known channels does not wait for a network
/// request.
fn synchronize_thumbnail_prefetch(
    view: &ViewModel,
    settings: &UiSettings,
    renderer: &mut dyn ThumbnailRenderer,
) -> bool {
    let rows = match view.screen {
        Screen::Search | Screen::YouTubeMusic if settings.prefetch_search_thumbnails => {
            view.rows.as_slice()
        }
        Screen::Subscriptions => view.subscriptions.sources.as_slice(),
        _ => &[],
    };
    renderer.synchronize_prefetch(rows)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitOutcome {
    TerminalEvent,
    ThumbnailRedraw,
    Timeout,
}

/// Waits for terminal input while probing only an in-flight thumbnail worker.
///
/// Cached thumbnails receive two quick probes, followed by progressively
/// slower checks. Idle operation without thumbnail work still uses one full
/// blocking terminal wait, and long network loads settle at four probes per
/// second.
fn wait_for_event_or_thumbnail(
    wait: Duration,
    mut thumbnail_renderer: Option<&mut (dyn ThumbnailRenderer + '_)>,
    mut terminal_event_ready: impl FnMut(Duration) -> io::Result<bool>,
) -> io::Result<WaitOutcome> {
    let Some(renderer) = thumbnail_renderer.as_mut() else {
        return terminal_event_ready(wait).map(|ready| {
            if ready {
                WaitOutcome::TerminalEvent
            } else {
                WaitOutcome::Timeout
            }
        });
    };
    if renderer.needs_immediate_redraw() {
        return Ok(WaitOutcome::ThumbnailRedraw);
    }
    if !renderer.is_pending() {
        return terminal_event_ready(wait).map(|ready| {
            if ready {
                WaitOutcome::TerminalEvent
            } else {
                WaitOutcome::Timeout
            }
        });
    }
    if renderer.poll() || !renderer.is_pending() {
        return Ok(WaitOutcome::ThumbnailRedraw);
    }

    let mut remaining = wait;
    let mut waited = Duration::ZERO;
    while !remaining.is_zero() {
        let probe = thumbnail_probe_interval(waited).min(remaining);
        if terminal_event_ready(probe)? {
            return Ok(WaitOutcome::TerminalEvent);
        }
        remaining = remaining.saturating_sub(probe);
        waited = waited.saturating_add(probe);
        if renderer.poll() || !renderer.is_pending() {
            return Ok(WaitOutcome::ThumbnailRedraw);
        }
    }
    Ok(WaitOutcome::Timeout)
}

/// Chooses an early cache-friendly probe followed by low-power network checks.
fn thumbnail_probe_interval(waited: Duration) -> Duration {
    if waited < Duration::from_millis(50) {
        Duration::from_millis(25)
    } else if waited < Duration::from_millis(100) {
        Duration::from_millis(50)
    } else if waited < Duration::from_millis(200) {
        Duration::from_millis(100)
    } else {
        Duration::from_millis(250)
    }
}

/// Stable-width ASCII frames shared by background activity indicators.
const ASCII_ACTIVITY_FRAMES: [char; 4] = ['|', '/', '-', '\\'];

/// Chooses a bounded redraw wait without introducing another timer source.
fn event_wait(view: &ViewModel, settings: &UiSettings) -> Duration {
    let playback_wait = if view.playback.paused {
        settings.idle_tick
    } else {
        settings.playing_tick
    };
    let wait = if view.local_browse_pending || view.local_artwork_pending {
        playback_wait.min(LOCAL_BROWSE_RESPONSE_POLL_INTERVAL)
    } else if view.search_activity.is_some() || view.playback_starting {
        playback_wait.min(settings.playing_tick)
    } else {
        playback_wait
    };
    wait.max(Duration::from_millis(1))
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut setup_guard = TerminalSetupGuard { active: true };
        let mut stdout = io::stdout();
        write_terminal_startup(&mut stdout)?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fullscreen,
            },
        )?;
        setup_guard.active = false;
        Ok(Self { terminal })
    }
}

/// Writes the escape commands that initialize Youta's terminal session.
fn write_terminal_startup(writer: &mut impl io::Write) -> io::Result<()> {
    execute!(
        writer,
        SetTitle("Youta"),
        EnterAlternateScreen,
        EnableMouseCapture,
        Hide
    )
}

struct TerminalSetupGuard {
    active: bool,
}

impl Drop for TerminalSetupGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            Show,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            Show,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Clone, Debug, Default)]
struct HitMap {
    tabs: Vec<(Screen, Rect)>,
    rows: Rect,
    /// First model row represented by the visible main-list rectangle.
    rows_first_index: usize,
    /// Physical terminal rows occupied by one main-list model row.
    rows_row_height: u16,
    subscription_source_rows: Rect,
    /// First model source represented by the visible source-list rectangle.
    subscription_source_first_index: usize,
    subscription_item_rows: Rect,
    /// First model item represented by the visible subscription-item rectangle.
    subscription_item_first_index: usize,
    details_panel: Rect,
    /// Actual wrapped-line offset rendered in the Details description.
    details_scroll_offset: usize,
    /// Largest wrapped-line offset that can change the visible description.
    details_scroll_maximum: usize,
    detail_links: Vec<(usize, Rect)>,
    /// Exact one-cell actions injected after `YouTube` video URLs.
    description_video_actions: Vec<(UiAction, Rect)>,
    detail_buttons: Vec<(UiAction, Rect)>,
    detail_text_rows: Vec<SelectableDetailsRow>,
    seek_bar: Rect,
    seek_markers: Vec<(UiAction, Rect)>,
    buttons: Vec<(UiAction, Rect)>,
    now_playing: Option<Rect>,
    error_buttons: Vec<(UiAction, Rect)>,
    youtube_setup_fields: Vec<(YouTubeSetupField, Rect)>,
    youtube_setup_buttons: Vec<(UiAction, Rect)>,
    preferences_buttons: Vec<(UiAction, Rect)>,
    local_file_buttons: Vec<(UiAction, Rect)>,
}

/// Exact terminal cells belonging to one visible, selectable Details row.
#[derive(Clone, Debug, Default)]
struct SelectableDetailsRow {
    x: u16,
    y: u16,
    cells: Vec<String>,
}

#[cfg(test)]
fn render(frame: &mut Frame<'_>, view: &ViewModel, settings: &UiSettings, hit_map: &mut HitMap) {
    render_frame(frame, view, settings, hit_map, None);
}

/// Chooses a bounded chapter-label height without taking space from the
/// minimum usable body, seek track, playback status, or controls.
fn chapter_label_row_count(
    view: &ViewModel,
    terminal_width: u16,
    terminal_height: u16,
    has_download: bool,
) -> u16 {
    if view.playback_chapters.is_empty() {
        return 0;
    }

    let fixed_rows = 2_u16
        .saturating_add(MIN_BODY_ROWS)
        .saturating_add(if has_download { 2 } else { 0 })
        .saturating_add(2)
        .saturating_add(2);
    let available_rows = terminal_height
        .saturating_sub(fixed_rows)
        .min(MAX_CHAPTER_LABEL_ROWS);
    if available_rows == 0 {
        return 0;
    }

    let duration = view.playback.duration.unwrap_or(Duration::ZERO);
    if duration.is_zero() {
        return 1;
    }
    let duration_seconds = duration.as_secs();
    let visible_chapters = view
        .playback_chapters
        .iter()
        .filter(|chapter| {
            chapter.start_seconds < duration_seconds
                && (!view.skip_advertisement_chapters
                    || !is_advertisement_chapter_title(&chapter.title))
        })
        .count();
    if visible_chapters == 0 {
        return 0;
    }
    if visible_chapters == 1 {
        return 1;
    }

    required_chapter_label_rows(view, duration, terminal_width)
        .min(available_rows)
        .min(MAX_CHAPTER_LABEL_ROWS)
}

/// Simulates the renderer's interval placement to reserve enough label rows.
///
/// Using the real truncated label widths keeps timestamped and names-only
/// layouts consistent with the number of rows they receive.
fn required_chapter_label_rows(view: &ViewModel, duration: Duration, width: u16) -> u16 {
    if duration.is_zero() || width == 0 {
        return u16::from(!view.playback_chapters.is_empty() && width > 0);
    }
    let layout = chapter_label_layout(view, duration, width, MAX_CHAPTER_LABEL_ROWS);
    if layout.saturated {
        MAX_CHAPTER_LABEL_ROWS
    } else {
        layout
            .placements
            .iter()
            .map(|placement| placement.row.saturating_add(1))
            .max()
            .unwrap_or_default()
    }
}

/// One packed chapter label whose marker remains independently time-aligned.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ChapterLabelPlacement {
    index: usize,
    row: u16,
    start: u16,
    width: u16,
    text: String,
}

/// Result of bounded chapter-label packing.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct ChapterLabelLayout {
    placements: Vec<ChapterLabelPlacement>,
    saturated: bool,
}

/// Packs chapter labels into the nearest available horizontal intervals.
///
/// Exact marker-aligned positions are preferred across all rows. When those
/// collide, the closest free interval is used, preferring the right side on an
/// equal-distance tie. The current, first, and last visible chapters receive
/// priority so a dense early cluster cannot consume every label row.
fn chapter_label_layout(
    view: &ViewModel,
    duration: Duration,
    width: u16,
    row_limit: u16,
) -> ChapterLabelLayout {
    if duration.is_zero() || width == 0 || row_limit == 0 {
        return ChapterLabelLayout::default();
    }
    let visible = visible_chapter_indices(view, duration);
    let current = current_visible_chapter_index(view, &visible);
    let mut order = Vec::with_capacity(visible.len());
    for candidate in current
        .into_iter()
        .chain(visible.first().copied())
        .chain(visible.last().copied())
        .chain(visible.iter().copied())
    {
        if !order.contains(&candidate) {
            order.push(candidate);
        }
    }

    let mut occupied = vec![Vec::<(u16, u16)>::new(); usize::from(row_limit)];
    let mut layout = ChapterLabelLayout::default();
    let show_hours = duration.as_secs() >= 60 * 60;
    for index in order {
        let chapter = &view.playback_chapters[index];
        let marker = rounded_duration_column(
            Duration::from_secs(chapter.start_seconds),
            duration,
            width.saturating_sub(1),
        );
        let maximum_width = usize::from(width).min(if Some(index) == current { 36 } else { 20 });
        let text = truncate_terminal_text(
            &chapter_timeline_label(
                chapter,
                Some(index) == current,
                view.show_chapter_timestamps,
                show_hours,
            ),
            maximum_width,
        );
        let label_width = terminal_text_width(&text).min(width);
        if label_width == 0 {
            continue;
        }
        let preferred = marker.min(width.saturating_sub(label_width));
        let exact_row = occupied.iter().position(|ranges| {
            interval_is_free(ranges, preferred, preferred.saturating_add(label_width))
        });
        let (row, start) = if let Some(row) = exact_row {
            (row, preferred)
        } else if let Some(candidate) =
            nearest_free_label_slot(&occupied, width, label_width, preferred)
        {
            candidate
        } else {
            layout.saturated = true;
            continue;
        };
        occupied[row].push((start, start.saturating_add(label_width)));
        occupied[row].sort_unstable_by_key(|range| range.0);
        layout.placements.push(ChapterLabelPlacement {
            index,
            row: u16::try_from(row).unwrap_or(row_limit.saturating_sub(1)),
            start,
            width: label_width,
            text,
        });
    }
    expand_chapter_labels_into_free_space(&mut layout.placements, view, current, show_hours, width);
    layout
}

/// Expands packed labels to the next occupied interval on the same row.
///
/// Conservative widths still decide collision-free placement, including the
/// active chapter's priority. Once those positions are stable, labels may use
/// otherwise blank cells to their right without moving or overlapping a
/// neighbour.
fn expand_chapter_labels_into_free_space(
    placements: &mut [ChapterLabelPlacement],
    view: &ViewModel,
    current: Option<usize>,
    show_hours: bool,
    width: u16,
) {
    let row_count = placements
        .iter()
        .map(|placement| usize::from(placement.row).saturating_add(1))
        .max()
        .unwrap_or_default();
    let mut row_starts = vec![Vec::new(); row_count];
    for placement in placements.iter() {
        row_starts[usize::from(placement.row)].push(placement.start);
    }
    for starts in &mut row_starts {
        starts.sort_unstable();
    }

    for placement in placements {
        let starts = &row_starts[usize::from(placement.row)];
        let next_index = starts.partition_point(|start| *start <= placement.start);
        let free_end = starts.get(next_index).copied().unwrap_or(width);
        let available_width = free_end.saturating_sub(placement.start);
        let text = truncate_terminal_text(
            &chapter_timeline_label(
                &view.playback_chapters[placement.index],
                Some(placement.index) == current,
                view.show_chapter_timestamps,
                show_hours,
            ),
            usize::from(available_width),
        );
        placement.width = terminal_text_width(&text).min(available_width);
        placement.text = text;
    }
}

fn interval_is_free(ranges: &[(u16, u16)], start: u16, end: u16) -> bool {
    ranges
        .iter()
        .all(|(used_start, used_end)| start >= *used_end || end <= *used_start)
}

/// Finds the closest shifted label position among every free row interval.
fn nearest_free_label_slot(
    occupied: &[Vec<(u16, u16)>],
    width: u16,
    label_width: u16,
    preferred: u16,
) -> Option<(usize, u16)> {
    let mut best: Option<(u16, bool, usize, u16)> = None;
    for (row, ranges) in occupied.iter().enumerate() {
        let mut cursor = 0_u16;
        let mut consider_interval = |free_start: u16, free_end: u16| {
            if free_end.saturating_sub(free_start) >= label_width {
                let latest = free_end.saturating_sub(label_width);
                let start = preferred.clamp(free_start, latest);
                let distance = start.abs_diff(preferred);
                let left_of_preferred = start < preferred;
                let score = (distance, left_of_preferred, row, start);
                if best.is_none_or(|current| score < current) {
                    best = Some(score);
                }
            }
        };
        for (used_start, used_end) in ranges {
            consider_interval(cursor, *used_start);
            cursor = cursor.max(*used_end);
        }
        consider_interval(cursor, width);
    }
    best.map(|(_, _, row, start)| (row, start))
}

fn render_frame(
    frame: &mut Frame<'_>,
    view: &ViewModel,
    settings: &UiSettings,
    hit_map: &mut HitMap,
    mut thumbnail_renderer: Option<&mut dyn ThumbnailRenderer>,
) {
    let theme = Theme::new(settings.funny_mode);
    frame.render_widget(Block::default().style(theme.base), frame.area());

    let chapter_label_rows = chapter_label_row_count(
        view,
        frame.area().width,
        frame.area().height,
        view.download.is_some(),
    );
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(8),
            Constraint::Length(if view.download.is_some() { 2 } else { 0 }),
            Constraint::Length(2_u16.saturating_add(chapter_label_rows)),
            Constraint::Length(2),
        ])
        .split(frame.area());
    render_tabs(frame, sections[0], view, &theme, hit_map);
    let thumbnail_is_obscured = view.help_open
        || view.youtube_setup_popup.is_some()
        || view.preferences_popup.is_some()
        || view.local_file_popup.is_some()
        || view.error_popup.is_some();
    if thumbnail_is_obscured {
        if let Some(renderer) = thumbnail_renderer.as_mut() {
            renderer.clear();
        }
        render_body(
            frame,
            sections[1],
            view,
            settings.show_hotkeys,
            settings.thumbnail_height,
            &theme,
            hit_map,
            None,
        );
    } else {
        render_body(
            frame,
            sections[1],
            view,
            settings.show_hotkeys,
            settings.thumbnail_height,
            &theme,
            hit_map,
            thumbnail_renderer,
        );
    }
    if let Some(download) = view.download.as_ref() {
        render_download_bar(frame, sections[2], download, &theme);
    }
    render_seek_bar(frame, sections[3], view, settings, &theme, hit_map);
    let status_line = animated_status_line(view);
    render_buttons(
        frame,
        sections[4],
        settings,
        &theme,
        view.screen,
        view.youtube_search_sort,
        view.youtube_creative_commons_only,
        view.show_chapter_timestamps,
        view.autoplay,
        view.local_folder_sizes_enabled
            .then_some(view.local_size_sort),
        &status_line,
        !view.playback.idle,
        hit_map,
    );
    if view.help_open {
        render_help(frame, &theme);
    }
    hit_map.youtube_setup_fields.clear();
    hit_map.youtube_setup_buttons.clear();
    if let Some(setup) = view.youtube_setup_popup.as_ref() {
        render_youtube_setup_popup(frame, setup, &theme, hit_map);
    }
    hit_map.preferences_buttons.clear();
    if let Some(preferences) = view.preferences_popup.as_ref() {
        render_preferences_popup(frame, preferences, &theme, hit_map);
    }
    hit_map.local_file_buttons.clear();
    if let Some(popup) = view.local_file_popup.as_ref() {
        render_local_file_popup(frame, popup, &theme, hit_map);
    }
    hit_map.error_buttons.clear();
    if let Some(error) = view.error_popup.as_ref() {
        render_error_popup(frame, error, &theme, hit_map);
    }
}

fn render_download_bar(frame: &mut Frame<'_>, area: Rect, download: &DownloadView, theme: &Theme) {
    let ratio = if !download.active && download.completed_path.is_some() {
        1.0
    } else {
        download
            .total_bytes
            .filter(|total| *total > 0)
            .map_or(0.0, |total| {
                download_ratio(download.downloaded_bytes, total)
            })
    };
    let label = if let Some(path) = download.completed_path.as_deref() {
        format!("Downloaded: {path}")
    } else {
        let transferred = match download.total_bytes {
            Some(total) if total > 0 => format!(
                "{:.1}% · {} / {}",
                ratio * 100.0,
                human_bytes(download.downloaded_bytes),
                human_bytes(total)
            ),
            _ => format!("{} · size unknown", human_bytes(download.downloaded_bytes)),
        };
        let speed = download
            .bytes_per_second
            .map(|value| format!(" · {}/s", human_bytes(value)))
            .unwrap_or_default();
        let eta = download
            .eta_seconds
            .map(|value| format!(" · ETA {}", format_duration(Duration::from_secs(value))))
            .unwrap_or_default();
        format!(
            "{} {} · {transferred}{speed}{eta}",
            if download.active {
                "Downloading"
            } else {
                "Download stopped:"
            },
            download.title
        )
    };
    frame.render_widget(
        Gauge::default()
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(theme.border),
            )
            .gauge_style(theme.progress)
            .ratio(ratio)
            .label(label),
        area,
    );
}

fn render_tabs(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    hit_map.tabs.clear();
    if area.is_empty() {
        return;
    }
    const DIVIDER: &str = " │ ";
    let full_width = Screen::ALL
        .iter()
        .map(|screen| usize::from(terminal_text_width(screen.label())))
        .sum::<usize>()
        .saturating_add(
            usize::from(terminal_text_width(DIVIDER))
                .saturating_mul(Screen::ALL.len().saturating_sub(1)),
        );
    let compact = full_width > usize::from(area.width);
    let mut spans = Vec::with_capacity(Screen::ALL.len().saturating_mul(2));
    let mut x = area.x;
    for (index, screen) in Screen::ALL.into_iter().enumerate() {
        if index > 0 {
            let divider_width = terminal_text_width(DIVIDER);
            if x >= area.right() {
                break;
            }
            spans.push(Span::styled(DIVIDER, theme.muted));
            x = x.saturating_add(divider_width);
        }
        if x >= area.right() {
            break;
        }
        let label = if compact {
            screen.compact_label()
        } else {
            screen.label()
        };
        let width = terminal_text_width(label).min(area.right().saturating_sub(x));
        if width == 0 {
            break;
        }
        spans.push(Span::styled(
            label,
            if screen == view.screen {
                theme.selected
            } else {
                theme.base
            },
        ));
        hit_map.tabs.push((screen, Rect::new(x, area.y, width, 1)));
        x = x.saturating_add(width);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Builds the result-panel title, including a route-matched ASCII search frame.
fn search_panel_title(view: &ViewModel) -> String {
    let search_title = if view.search_editing {
        format!(" Search: {}▏ ", view.search_query)
    } else if view.search_query.is_empty() {
        match view.screen {
            Screen::Search => format!(
                " YouTube {} search ",
                match view.search_kind {
                    SearchKind::Videos => "video",
                    SearchKind::Channels => "channel",
                }
            ),
            Screen::YouTubeMusic => " YouTube Music search ".to_owned(),
            Screen::TrackerMusic => " MOD/tracker archive search ".to_owned(),
            Screen::Local if !view.local_path.is_empty() => {
                format!(" Local — {} ", view.local_path)
            }
            _ => format!(" {} ", view.screen.label()),
        }
    } else {
        format!(" {} — {} ", view.screen.label(), view.search_query)
    };
    if view
        .search_activity
        .is_some_and(|activity| activity.screen() == view.screen)
    {
        let frame =
            ASCII_ACTIVITY_FRAMES[view.search_animation_frame % ASCII_ACTIVITY_FRAMES.len()];
        format!(" {frame} {}", search_title.trim_start())
    } else {
        search_title
    }
}

/// Adds one stable-width ASCII frame while accepted media is starting.
fn animated_status_line(view: &ViewModel) -> Cow<'_, str> {
    if view.playback_starting {
        let frame = ASCII_ACTIVITY_FRAMES
            [view.playback_start_animation_frame % ASCII_ACTIVITY_FRAMES.len()];
        Cow::Owned(format!("{frame} {}", view.status_line))
    } else {
        Cow::Borrowed(&view.status_line)
    }
}

/// Returns the playback-progress marker displayed independently of subscription state.
fn watched_marker(watched_percent: u8) -> &'static str {
    match watched_percent {
        0 => "●",
        1..=90 => "◐",
        _ => "○",
    }
}

fn render_body(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    show_hotkeys: bool,
    thumbnail_height: u16,
    theme: &Theme,
    hit_map: &mut HitMap,
    thumbnail_renderer: Option<&mut dyn ThumbnailRenderer>,
) {
    hit_map.detail_links.clear();
    hit_map.detail_buttons.clear();
    hit_map.detail_text_rows.clear();
    hit_map.details_panel = Rect::default();
    hit_map.details_scroll_offset = 0;
    hit_map.details_scroll_maximum = 0;
    hit_map.rows = Rect::default();
    hit_map.rows_first_index = 0;
    hit_map.rows_row_height = 2;
    hit_map.subscription_source_rows = Rect::default();
    hit_map.subscription_source_first_index = 0;
    hit_map.subscription_item_rows = Rect::default();
    hit_map.subscription_item_first_index = 0;

    if view.screen == Screen::Subscriptions {
        render_subscriptions_body(
            frame,
            area,
            view,
            show_hotkeys,
            thumbnail_height,
            theme,
            hit_map,
            thumbnail_renderer,
        );
        return;
    }

    let horizontal = area.width >= 80;
    let panes = Layout::default()
        .direction(if horizontal {
            Direction::Horizontal
        } else {
            Direction::Vertical
        })
        .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
        .split(area);

    let search_title = search_panel_title(view);
    hit_map.rows_row_height = row_list_height(&view.rows);
    (hit_map.rows, hit_map.rows_first_index) = render_row_list(
        frame,
        panes[0],
        search_title.trim(),
        &view.rows,
        true,
        view.selected,
        view.playing_media_id.as_ref(),
        theme.heading,
        theme,
    );

    match view.right_panel_mode {
        RightPanelMode::Details => {
            render_details(
                frame,
                panes[1],
                view,
                show_hotkeys,
                thumbnail_height,
                theme,
                hit_map,
                thumbnail_renderer,
            );
        }
        RightPanelMode::Waveform => {
            if let Some(renderer) = thumbnail_renderer {
                renderer.clear();
            }
            render_waveform(frame, panes[1], view, theme);
        }
        RightPanelMode::Channel => {
            render_channel(
                frame,
                panes[1],
                view,
                show_hotkeys,
                thumbnail_height,
                theme,
                hit_map,
                thumbnail_renderer,
            );
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the renderer keeps list data and presentation state explicit"
)]
fn render_row_list(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    rows: &[RowView],
    show_source: bool,
    selected_index: usize,
    playing_media_id: Option<&MediaId>,
    heading_style: Style,
    theme: &Theme,
) -> (Rect, usize) {
    let rows_area = render_main_panel_heading(frame, area, title, heading_style);
    let row_height = row_list_height(rows);
    let visible_rows =
        usize::from(rows_area.height / row_height).max(usize::from(rows_area.height > 0));
    let selected_index = selected_index.min(rows.len().saturating_sub(1));
    let first_index = selected_index
        .saturating_sub(visible_rows.saturating_sub(1))
        .min(rows.len().saturating_sub(visible_rows));
    let items = rows
        .iter()
        .enumerate()
        .skip(first_index)
        .take(visible_rows)
        .map(|(index, row)| {
            let selected = index == selected_index;
            let playing =
                playing_media_id.is_some_and(|media_id| row.media_id.as_ref() == Some(media_id));
            let row_style = if selected {
                theme.selected.fg(Color::Black)
            } else if playing {
                theme.accent.add_modifier(Modifier::BOLD)
            } else {
                theme.base
            };
            let title_style = if !selected && row.vertical {
                let style = theme.vertical_video;
                if playing {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    style
                }
            } else {
                row_style
            };
            let marker = if row.subscribed { "◆" } else { " " };
            let has_playback_progress = row.media_id.is_some();
            let progress = if !has_playback_progress || row.watched_percent == 0 {
                String::new()
            } else {
                format!(" {:>3}%", row.watched_percent)
            };
            let source_style = if selected || playing {
                row_style
            } else {
                source_style(&row.source, theme)
            };
            let secondary_style = if selected || playing {
                row_style
            } else {
                theme.muted
            };
            let watched_style = if selected || playing {
                row_style
            } else if row.watched_percent == 0 {
                theme.muted
            } else {
                theme.accent
            };
            let watched_marker = if has_playback_progress {
                watched_marker(row.watched_percent)
            } else {
                " "
            };
            let mut title_spans = if row.compact {
                let mut spans = Vec::with_capacity(2);
                if playing {
                    spans.push(Span::styled("▶ ", row_style));
                }
                if has_playback_progress {
                    spans.push(Span::styled(format!("{watched_marker} "), watched_style));
                }
                spans
            } else if show_source {
                vec![
                    Span::styled(format!("{} ", if playing { "▶" } else { " " }), row_style),
                    Span::styled(format!("{marker} "), source_style),
                    Span::styled(format!("{watched_marker} "), watched_style),
                ]
            } else if playing {
                vec![
                    Span::styled("▶ ", row_style),
                    Span::styled(format!("{watched_marker} "), watched_style),
                ]
            } else {
                vec![Span::styled(format!("{watched_marker} "), watched_style)]
            };
            title_spans.push(Span::styled(&row.title, title_style));
            title_spans.push(Span::styled(progress, secondary_style));
            if row_height == 1 && !row.subtitle.is_empty() {
                title_spans.push(Span::styled(" · ", secondary_style));
                title_spans.push(Span::styled(&row.subtitle, secondary_style));
            }
            let line = Line::from(title_spans);
            if row_height == 1 {
                return ListItem::new(line).style(row_style);
            }
            let mut subtitle_spans = Vec::new();
            if !row.compact && (show_source || playing) {
                subtitle_spans.push(Span::styled("    ", row_style));
            }
            if !row.compact && show_source && !row.source.is_empty() {
                subtitle_spans.push(Span::styled(&row.source, source_style));
                if !row.subtitle.is_empty() {
                    subtitle_spans.push(Span::styled(" · ", row_style));
                }
            }
            if !row.subtitle.is_empty() {
                subtitle_spans.push(Span::styled(&row.subtitle, secondary_style));
            }
            let subtitle = Line::from(subtitle_spans);
            ListItem::new(vec![line, subtitle]).style(row_style)
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items), rows_area);
    (rows_area, first_index)
}

/// Returns the uniform physical height for a homogeneous compact list.
fn row_list_height(rows: &[RowView]) -> u16 {
    if !rows.is_empty() && rows.iter().all(|row| row.compact) {
        1
    } else {
        2
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the renderer receives the same explicit dependencies as the normal body"
)]
fn render_subscriptions_body(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    show_hotkeys: bool,
    thumbnail_height: u16,
    theme: &Theme,
    hit_map: &mut HitMap,
    thumbnail_renderer: Option<&mut dyn ThumbnailRenderer>,
) {
    let subscriptions = &view.subscriptions;
    match subscriptions.layout {
        SubscriptionsLayout::DrillDown if subscriptions.route == SubscriptionRoute::Items => {
            let panes = Layout::default()
                .direction(if area.width >= 80 {
                    Direction::Horizontal
                } else {
                    Direction::Vertical
                })
                .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
                .split(area);
            let list_sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(panes[0]);
            (
                hit_map.subscription_item_rows,
                hit_map.subscription_item_first_index,
            ) = render_row_list(
                frame,
                list_sections[0],
                &subscription_videos_heading(subscriptions),
                &subscriptions.items,
                false,
                subscriptions.selected_item,
                view.playing_media_id.as_ref(),
                theme.heading,
                theme,
            );
            render_subscription_item_buttons(
                frame,
                list_sections[1],
                !subscriptions.items.is_empty(),
                false,
                show_hotkeys,
                theme,
                hit_map,
            );
            render_details(
                frame,
                panes[1],
                view,
                show_hotkeys,
                thumbnail_height,
                theme,
                hit_map,
                thumbnail_renderer,
            );
        }
        SubscriptionsLayout::DrillDown => {
            let panes = Layout::default()
                .direction(if area.width >= 80 {
                    Direction::Horizontal
                } else {
                    Direction::Vertical
                })
                .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
                .split(area);
            (
                hit_map.subscription_source_rows,
                hit_map.subscription_source_first_index,
            ) = render_row_list(
                frame,
                panes[0],
                "Subscription sources",
                &subscriptions.sources,
                true,
                subscriptions.selected_source,
                view.playing_media_id.as_ref(),
                theme.heading,
                theme,
            );
            render_channel(
                frame,
                panes[1],
                view,
                show_hotkeys,
                thumbnail_height,
                theme,
                hit_map,
                thumbnail_renderer,
            );
        }
        SubscriptionsLayout::Split => {
            let panes = Layout::default()
                .direction(if area.width >= 80 {
                    Direction::Horizontal
                } else {
                    Direction::Vertical
                })
                .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
                .split(area);
            let source_heading = if subscriptions.focus == SubscriptionPane::Sources {
                theme
                    .accent
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                theme.heading
            };
            (
                hit_map.subscription_source_rows,
                hit_map.subscription_source_first_index,
            ) = render_row_list(
                frame,
                panes[0],
                "Subscription sources",
                &subscriptions.sources,
                true,
                subscriptions.selected_source,
                view.playing_media_id.as_ref(),
                source_heading,
                theme,
            );
            if subscriptions.description_expanded {
                render_details(
                    frame,
                    panes[1],
                    view,
                    show_hotkeys,
                    thumbnail_height,
                    theme,
                    hit_map,
                    thumbnail_renderer,
                );
                let footer = Rect::new(
                    panes[1].x,
                    panes[1].bottom().saturating_sub(1),
                    panes[1].width,
                    if panes[1].height > 0 { 1 } else { 0 },
                );
                render_subscription_item_buttons(
                    frame,
                    footer,
                    true,
                    true,
                    show_hotkeys,
                    theme,
                    hit_map,
                );
            } else {
                if let Some(renderer) = thumbnail_renderer {
                    renderer.clear();
                }
                let item_heading = if subscriptions.focus == SubscriptionPane::Items {
                    theme
                        .accent
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    theme.heading
                };
                let sections = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(1), Constraint::Length(1)])
                    .split(panes[1]);
                let heading = subscription_videos_heading(subscriptions);
                (
                    hit_map.subscription_item_rows,
                    hit_map.subscription_item_first_index,
                ) = render_row_list(
                    frame,
                    sections[0],
                    &heading,
                    &subscriptions.items,
                    false,
                    subscriptions.selected_item,
                    view.playing_media_id.as_ref(),
                    item_heading,
                    theme,
                );
                render_subscription_item_buttons(
                    frame,
                    sections[1],
                    !subscriptions.items.is_empty(),
                    false,
                    show_hotkeys,
                    theme,
                    hit_map,
                );
            }
        }
    }
}

/// Builds the shared `YouTube` source heading for both subscription layouts.
fn subscription_videos_heading(subscriptions: &SubscriptionsView) -> String {
    let mut heading = if subscriptions.source_title.is_empty() {
        "YouTube".to_owned()
    } else {
        format!("{} · YouTube", subscriptions.source_title)
    };
    if let Some(count) = subscriptions.source_subscriber_count {
        heading.push_str(" · ");
        heading.push_str(&format_count(count));
        heading.push_str(" subscribers");
    }
    if !subscriptions.source_created.is_empty() {
        heading.push_str(" · created ");
        heading.push_str(&subscriptions.source_created);
    }
    heading
}

fn render_subscription_item_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    description_available: bool,
    description_expanded: bool,
    show_hotkeys: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    if area.is_empty() {
        return;
    }
    let refresh = (
        button("R", "Refresh videos", show_hotkeys),
        UiAction::RefreshSubscriptionVideos,
    );
    let description = description_available.then(|| {
        (
            button(
                if description_expanded { "i/Esc" } else { "i" },
                if description_expanded {
                    "Back to videos"
                } else {
                    "Description"
                },
                show_hotkeys,
            ),
            UiAction::ToggleSubscriptionDescription,
        )
    });
    let mut buttons = Vec::with_capacity(2);
    if description_expanded && let Some(description) = description.clone() {
        buttons.push(description);
    }
    buttons.push(refresh);
    if !description_expanded && let Some(description) = description {
        buttons.push(description);
    }
    let mut x = area.x;
    for (label, action) in buttons {
        let available = area.right().saturating_sub(x);
        let width = terminal_text_width(&label).min(available);
        if width == 0 {
            break;
        }
        let target = Rect::new(x, area.y, width, 1);
        frame.render_widget(Paragraph::new(label).style(theme.accent), target);
        hit_map.detail_buttons.push((action, target));
        x = x.saturating_add(width).saturating_add(2);
    }
}

fn render_details(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    show_hotkeys: bool,
    thumbnail_height: u16,
    theme: &Theme,
    hit_map: &mut HitMap,
    thumbnail_renderer: Option<&mut dyn ThumbnailRenderer>,
) {
    render_information_panel(
        frame,
        area,
        view,
        show_hotkeys,
        theme,
        hit_map,
        " Details ",
        "Select an item to load details lazily.",
        match view.screen {
            Screen::Local => InformationPanelKind::Local,
            Screen::History => InformationPanelKind::Generic,
            _ => InformationPanelKind::Video,
        },
        true,
        thumbnail_height,
        thumbnail_renderer,
    );
}

/// Source-specific metadata layout used by the shared information renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InformationPanelKind {
    /// Media details with duration, likes, and views.
    Video,
    /// Channel details with subscriber metadata.
    Channel,
    /// Local folder, media, or image metadata without remote statistics.
    Local,
    /// Persisted or aggregate rows without source-specific remote statistics.
    Generic,
}

fn render_information_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    show_hotkeys: bool,
    theme: &Theme,
    hit_map: &mut HitMap,
    title: &str,
    empty_message: &str,
    kind: InformationPanelKind,
    show_text_selection: bool,
    thumbnail_height: u16,
    mut thumbnail_renderer: Option<&mut dyn ThumbnailRenderer>,
) {
    hit_map.details_panel = area;
    let heading_style = if view.details_focused {
        theme
            .accent
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        theme.heading
    };
    let inner = render_main_panel_heading(frame, area, title.trim(), heading_style);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let Some(details) = view.details.as_ref() else {
        frame.render_widget(
            Paragraph::new(empty_message)
                .style(theme.muted)
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    };

    let title_already_visible = if view.screen == Screen::Subscriptions {
        view.subscriptions.source_title == details.title
    } else {
        view.rows
            .get(view.selected)
            .is_some_and(|row| row.title == details.title)
    };
    let mut lines = if !show_text_selection && !title_already_visible {
        vec![Line::styled(&details.title, theme.heading)]
    } else {
        Vec::new()
    };
    let text_selection_button = show_text_selection.then(|| {
        let label = button(
            if view.text_selection_mode {
                "t/Esc"
            } else {
                "t"
            },
            if view.text_selection_mode {
                "End text selection"
            } else {
                "Select Details text"
            },
            show_hotkeys,
        );
        let line_index = lines.len();
        lines.push(Line::styled(
            label.clone(),
            if view.text_selection_mode {
                theme.selected
            } else {
                theme.accent
            },
        ));
        (line_index, label, UiAction::ToggleTextSelectionMode)
    });
    let subscription_button = (!details.channel_id.is_empty()).then(|| {
        let label = button(
            "s",
            if details.channel_subscribed {
                "Unsubscribe (locally)"
            } else {
                "Subscribe (locally)"
            },
            show_hotkeys,
        );
        let line_index = lines.len();
        lines.push(Line::styled(label.clone(), theme.accent));
        (line_index, label, UiAction::ToggleSubscription)
    });
    let open_button = (show_text_selection && kind == InformationPanelKind::Video).then(|| {
        let label = button("o", "xdg-open video", show_hotkeys);
        let line_index = lines.len();
        lines.push(Line::styled(label.clone(), theme.accent));
        (line_index, label, UiAction::OpenInBrowser)
    });
    let open_channel_button = details.channel_webpage_url.as_ref().map(|url| {
        let label = button("O", &format!("xdg-open channel · {url}"), show_hotkeys);
        let line_index = lines.len();
        lines.push(Line::styled(label.clone(), theme.accent));
        (line_index, label, UiAction::OpenChannelInBrowser)
    });
    let rename_button =
        (kind == InformationPanelKind::Local && details.local_renamable).then(|| {
            let label = button("r", "Rename", show_hotkeys);
            let line_index = lines.len();
            lines.push(Line::styled(label.clone(), theme.accent));
            (line_index, label, UiAction::BeginLocalRename)
        });
    let trash_button =
        (kind == InformationPanelKind::Local && details.local_trashable).then(|| {
            let label = button("Delete", "Move to Trash", show_hotkeys);
            let line_index = lines.len();
            lines.push(Line::styled(label.clone(), theme.accent));
            (line_index, label, UiAction::RequestLocalTrash)
        });
    match kind {
        InformationPanelKind::Video => lines.extend([
            Line::from(vec![
                Span::styled("Length: ", theme.muted),
                Span::raw(&details.length),
            ]),
            Line::from(vec![
                Span::styled("Likes: ", theme.muted),
                Span::raw(&details.likes),
                Span::styled("  Views: ", theme.muted),
                Span::raw(&details.views),
            ]),
        ]),
        InformationPanelKind::Channel => {
            if let Some(count) = details.channel_subscriber_count {
                lines.push(Line::from(vec![
                    Span::styled("Subscribers: ", theme.muted),
                    Span::raw(format_count(count)),
                ]));
            }
            if !details.channel_created.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("Joined: ", theme.muted),
                    Span::raw(&details.channel_created),
                ]));
            }
            if let Some(count) = details.channel_video_count {
                lines.push(Line::from(vec![
                    Span::styled("Videos: ", theme.muted),
                    Span::raw(format_count(count)),
                ]));
            }
            if let Some(count) = details.channel_total_view_count {
                lines.push(Line::from(vec![
                    Span::styled("Total views: ", theme.muted),
                    Span::raw(format_count(count)),
                ]));
            }
            if !details.channel_country.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("Country: ", theme.muted),
                    Span::raw(&details.channel_country),
                ]));
            }
            if details.channel_links_truncated {
                lines.push(Line::styled(
                    "Some channel links were omitted by safety limits.",
                    theme.muted,
                ));
            }
        }
        InformationPanelKind::Local => {
            if !details.length.is_empty() && details.length != "unknown" {
                lines.push(Line::from(vec![
                    Span::styled("Length: ", theme.muted),
                    Span::raw(&details.length),
                ]));
            }
        }
        InformationPanelKind::Generic => {}
    }
    if is_creative_commons_license(&details.license) {
        lines.push(Line::from(vec![
            Span::styled("License: ", theme.muted),
            Span::styled(display_license_label(&details.license), theme.accent),
        ]));
    }
    let metadata_height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .min(inner.height);
    let metadata_area = Rect::new(inner.x, inner.y, inner.width, metadata_height);
    // Metadata uses one terminal row per field so the subscription button's
    // mouse target remains stable even when a title or channel name is long.
    frame.render_widget(Paragraph::new(lines), metadata_area);
    if show_text_selection {
        let selection_button_row = text_selection_button
            .as_ref()
            .map(|(line_index, _, _)| *line_index);
        let subscription_button_row = subscription_button
            .as_ref()
            .map(|(line_index, _, _)| *line_index);
        let open_button_row = open_button.as_ref().map(|(line_index, _, _)| *line_index);
        let open_channel_button_row = open_channel_button
            .as_ref()
            .map(|(line_index, _, _)| *line_index);
        let rename_button_row = rename_button.as_ref().map(|(line_index, _, _)| *line_index);
        let trash_button_row = trash_button.as_ref().map(|(line_index, _, _)| *line_index);
        for line_index in 0..usize::from(metadata_height) {
            if selection_button_row == Some(line_index)
                || subscription_button_row == Some(line_index)
                || open_button_row == Some(line_index)
                || open_channel_button_row == Some(line_index)
                || rename_button_row == Some(line_index)
                || trash_button_row == Some(line_index)
            {
                continue;
            }
            capture_selectable_details_row(
                frame,
                hit_map,
                Rect::new(
                    metadata_area.x,
                    metadata_area.y.saturating_add(
                        u16::try_from(line_index).unwrap_or(metadata_height.saturating_sub(1)),
                    ),
                    metadata_area.width,
                    1,
                ),
            );
        }
    }
    for (line_index, label, action) in text_selection_button
        .into_iter()
        .chain(subscription_button)
        .chain(open_button)
        .chain(open_channel_button)
        .chain(rename_button)
        .chain(trash_button)
    {
        if line_index >= usize::from(metadata_height) {
            continue;
        }
        let width = terminal_text_width(&label).min(inner.width);
        if width > 0 {
            hit_map.detail_buttons.push((
                action,
                Rect::new(
                    inner.x,
                    inner.y.saturating_add(
                        u16::try_from(line_index).unwrap_or(metadata_height.saturating_sub(1)),
                    ),
                    width,
                    1,
                ),
            ));
        }
    }

    let mut cursor_y = metadata_area.bottom();
    let mut remaining_height = inner.bottom().saturating_sub(cursor_y);
    if let Some(renderer) = thumbnail_renderer.as_mut() {
        let text_reserve = u16::from(!details.description.is_empty())
            + u16::from(!details.links.is_empty()).saturating_mul(2);
        let preferred_thumbnail_height = if details.source == "Local image" {
            thumbnail_height.saturating_mul(2)
        } else {
            thumbnail_height
        };
        let rendered_thumbnail_height = remaining_height
            .saturating_sub(text_reserve)
            .min(preferred_thumbnail_height);
        if renderer.is_enabled()
            && details.thumbnail_url.is_some()
            && rendered_thumbnail_height >= MIN_THUMBNAIL_HEIGHT
        {
            let thumbnail_area =
                Rect::new(inner.x, cursor_y, inner.width, rendered_thumbnail_height);
            renderer.synchronize(details.thumbnail_url.as_ref(), thumbnail_area);
            renderer.render(frame, thumbnail_area, theme);
            cursor_y = cursor_y.saturating_add(rendered_thumbnail_height);
            remaining_height = inner.bottom().saturating_sub(cursor_y);
        } else {
            renderer.clear();
        }
    }
    if !details.links.is_empty() && remaining_height > 0 {
        let description_reserve = if details.description.is_empty() {
            0
        } else {
            remaining_height.min(1)
        };
        let desired_link_height = u16::try_from(details.links.len())
            .unwrap_or(u16::MAX)
            .saturating_add(1);
        let link_height =
            desired_link_height.min(remaining_height.saturating_sub(description_reserve));
        if link_height > 1 {
            let heading_area = Rect::new(inner.x, cursor_y, inner.width, 1);
            frame.render_widget(
                Paragraph::new("External links").style(theme.heading),
                heading_area,
            );
            if show_text_selection {
                capture_selectable_details_row(frame, hit_map, heading_area);
            }
            cursor_y = cursor_y.saturating_add(1);

            let visible_links = usize::from(link_height.saturating_sub(1));
            let selected_link = view
                .selected_detail_link
                .unwrap_or_default()
                .min(details.links.len().saturating_sub(1));
            let first_link = selected_link
                .saturating_add(1)
                .saturating_sub(visible_links);
            for (index, link) in details
                .links
                .iter()
                .enumerate()
                .skip(first_link)
                .take(visible_links)
            {
                let link_area = Rect::new(inner.x, cursor_y, inner.width, 1);
                let selected = view.selected_detail_link == Some(index);
                let marker = if selected { "▶ " } else { "  " };
                let mut spans = vec![Span::styled(
                    marker,
                    if selected { theme.accent } else { theme.base },
                )];
                if let Some(item_id) = link.wikidata_item_id.as_deref() {
                    let expanded = details.expanded_wikidata_item.as_deref() == Some(item_id);
                    let disclosure =
                        button("W", if expanded { "🧾▾" } else { "🧾▸" }, show_hotkeys);
                    let disclosure_width = terminal_text_width(&disclosure);
                    spans.push(Span::styled(
                        disclosure,
                        theme.accent.add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::raw(" "));
                    if disclosure_width > 0 {
                        hit_map.detail_buttons.push((
                            UiAction::ToggleWikidataStatements(index),
                            Rect::new(
                                link_area.x.saturating_add(terminal_text_width(marker)),
                                link_area.y,
                                disclosure_width.min(link_area.width),
                                1,
                            ),
                        ));
                    }
                }
                spans.extend([
                    Span::styled(
                        &link.label,
                        if selected {
                            theme.base.add_modifier(Modifier::BOLD)
                        } else {
                            theme.base
                        },
                    ),
                    Span::styled(" — ", theme.muted),
                    Span::styled(&link.url, theme.muted),
                ]);
                frame.render_widget(Paragraph::new(Line::from(spans)), link_area);
                if show_text_selection {
                    capture_selectable_details_row(frame, hit_map, link_area);
                }
                hit_map.detail_links.push((index, link_area));
                cursor_y = cursor_y.saturating_add(1);
            }
            remaining_height = inner.bottom().saturating_sub(cursor_y);
        }
    }

    let expanded_wikidata_entity = details
        .expanded_wikidata_item
        .as_deref()
        .and_then(|item_id| {
            details
                .wikidata_entities
                .iter()
                .find(|entity| entity.item_id == item_id)
        });
    let expanded_wikidata_text = details.expanded_wikidata_item.as_deref().map(|item_id| {
        expanded_wikidata_entity.map_or_else(
            || {
                if details.loading_wikidata_item.as_deref() == Some(item_id) {
                    "Loading Wikidata properties…"
                } else {
                    "Wikidata properties are unavailable."
                }
            },
            |entity| entity.text.as_str(),
        )
    });
    let body_is_wikidata = expanded_wikidata_text.is_some();
    let body_source = expanded_wikidata_text.unwrap_or(&details.description);
    let wikidata_value_links = expanded_wikidata_entity
        .map(|entity| entity.value_links.as_slice())
        .unwrap_or_default();
    if remaining_height > 0 && !body_source.is_empty() {
        let description_area = Rect::new(inner.x, cursor_y, inner.width, remaining_height);
        let (description_text_area, scrollbar_area) = if description_area.width > 1 {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(description_area);
            (columns[0], columns[1])
        } else {
            (description_area, Rect::default())
        };
        let description_lines = wrap_description_source(
            body_source,
            usize::from(description_text_area.width.max(1)),
            if body_is_wikidata {
                &[]
            } else {
                &details.video_links
            },
        );
        let visible_lines = usize::from(description_text_area.height);
        let maximum_offset = description_lines.len().saturating_sub(visible_lines);
        let offset = view.details_scroll.min(maximum_offset);
        hit_map.details_scroll_offset = offset;
        hit_map.details_scroll_maximum = maximum_offset;
        let visible = description_lines
            .iter()
            .skip(offset)
            .take(visible_lines)
            .enumerate();
        let active_chapter_line = (!body_is_wikidata)
            .then(|| active_description_chapter_line(view, details))
            .flatten();
        for (visible_index, source_line) in visible {
            let row = description_text_area.y.saturating_add(
                u16::try_from(visible_index).unwrap_or(description_text_area.height),
            );
            let mut spans = Vec::new();
            let mut cell_cursor = 0_u16;
            let active_line = active_chapter_line.as_ref().is_some_and(|active| {
                source_line.start_byte < active.end && source_line.end_byte > active.start
            });
            let line_style = if active_line {
                theme.active_chapter.add_modifier(Modifier::BOLD)
            } else {
                theme.base
            };
            for token in &source_line.tokens {
                match *token {
                    WrappedDescriptionToken::Source {
                        start_byte,
                        end_byte,
                    } => {
                        if body_is_wikidata {
                            append_wikidata_source_spans(
                                body_source,
                                wikidata_value_links,
                                start_byte,
                                end_byte,
                                description_text_area,
                                row,
                                theme,
                                hit_map,
                                &mut spans,
                                &mut cell_cursor,
                            );
                        } else {
                            append_description_source_spans(
                                details,
                                start_byte,
                                end_byte,
                                active_line,
                                description_text_area,
                                row,
                                theme,
                                hit_map,
                                &mut spans,
                                &mut cell_cursor,
                            );
                        }
                    }
                    WrappedDescriptionToken::VideoAction { link_index } => {
                        let Some(link) = details.video_links.get(link_index) else {
                            continue;
                        };
                        let action_width = terminal_text_width(DESCRIPTION_VIDEO_ACTION_SYMBOL);
                        spans.push(Span::styled(
                            DESCRIPTION_VIDEO_ACTION_SYMBOL,
                            theme
                                .accent
                                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                        ));
                        if action_width == 1 {
                            hit_map.description_video_actions.push((
                                UiAction::ActivateDescriptionVideo {
                                    video_id: link.video_id.clone(),
                                    start_seconds: link.start_seconds,
                                },
                                Rect::new(
                                    description_text_area.x.saturating_add(cell_cursor),
                                    row,
                                    1,
                                    1,
                                ),
                            ));
                        }
                        cell_cursor = cell_cursor.saturating_add(action_width);
                    }
                }
            }
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(line_style),
                Rect::new(description_text_area.x, row, description_text_area.width, 1),
            );
            if show_text_selection {
                capture_selectable_details_row(
                    frame,
                    hit_map,
                    Rect::new(description_text_area.x, row, description_text_area.width, 1),
                );
            }
        }
        if description_lines.len() > visible_lines
            && scrollbar_area.width > 0
            && scrollbar_area.height > 0
        {
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .track_style(theme.muted)
                .thumb_symbol("█")
                .thumb_style(theme.accent);
            let mut scrollbar_state = ScrollbarState::new(maximum_offset.saturating_add(1))
                .position(offset)
                .viewport_content_length(visible_lines);
            frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
        }
    }
    if show_text_selection {
        highlight_details_text_selection(frame, view, hit_map, theme);
    }
}

/// Appends one expanded-Wikidata source token while preserving item links
/// across terminal wrapping.
#[allow(
    clippy::too_many_arguments,
    reason = "render geometry and mutable frame products belong to one row operation"
)]
fn append_wikidata_source_spans<'a>(
    source: &'a str,
    value_links: &[DetailWikidataValueLinkView],
    start_byte: usize,
    end_byte: usize,
    description_area: Rect,
    row: u16,
    theme: &Theme,
    hit_map: &mut HitMap,
    spans: &mut Vec<Span<'a>>,
    cell_cursor: &mut u16,
) {
    let mut source_cursor = start_byte;
    for link in value_links
        .iter()
        .filter(|link| link.start_byte < end_byte && link.end_byte > start_byte)
    {
        let start = link.start_byte.max(start_byte);
        let end = link.end_byte.min(end_byte);
        if source_cursor < start {
            let plain = &source[source_cursor..start];
            *cell_cursor = cell_cursor.saturating_add(terminal_text_width(plain));
            spans.push(Span::raw(plain));
        }
        let linked = &source[start..end];
        let linked_width = terminal_text_width(linked);
        spans.push(Span::styled(
            linked,
            theme
                .accent
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        ));
        let available = description_area.width.saturating_sub(*cell_cursor);
        let target_width = linked_width.min(available);
        if target_width > 0 {
            hit_map.detail_buttons.push((
                UiAction::OpenWikidataItem(link.item_id.clone()),
                Rect::new(
                    description_area.x.saturating_add(*cell_cursor),
                    row,
                    target_width,
                    1,
                ),
            ));
        }
        *cell_cursor = cell_cursor.saturating_add(linked_width);
        source_cursor = end;
    }
    if source_cursor < end_byte {
        let plain = &source[source_cursor..end_byte];
        *cell_cursor = cell_cursor.saturating_add(terminal_text_width(plain));
        spans.push(Span::raw(plain));
    }
}

/// Appends one source-text token while retaining clickable timecode spans.
#[allow(
    clippy::too_many_arguments,
    reason = "render geometry and mutable frame products belong to one row operation"
)]
fn append_description_source_spans<'a>(
    details: &'a DetailView,
    start_byte: usize,
    end_byte: usize,
    active_line: bool,
    description_area: Rect,
    row: u16,
    theme: &Theme,
    hit_map: &mut HitMap,
    spans: &mut Vec<Span<'a>>,
    cell_cursor: &mut u16,
) {
    let mut source_cursor = start_byte;
    for timecode in details
        .timecodes
        .iter()
        .filter(|timecode| timecode.start_byte < end_byte && timecode.end_byte > start_byte)
    {
        let start = timecode.start_byte.max(start_byte);
        let end = timecode.end_byte.min(end_byte);
        if source_cursor < start {
            let plain = &details.description[source_cursor..start];
            *cell_cursor = cell_cursor.saturating_add(terminal_text_width(plain));
            spans.push(Span::raw(plain));
        }
        let linked = &details.description[start..end];
        let linked_width = terminal_text_width(linked);
        spans.push(Span::styled(
            linked,
            if active_line {
                theme
                    .active_chapter
                    .add_modifier(Modifier::UNDERLINED | Modifier::BOLD)
            } else {
                theme.accent.add_modifier(Modifier::UNDERLINED)
            },
        ));
        if let Some(media_id) = details.media_id.as_ref()
            && linked_width > 0
        {
            hit_map.detail_buttons.push((
                UiAction::ActivateTimecode {
                    media_id: media_id.clone(),
                    seconds: timecode.seconds,
                },
                Rect::new(
                    description_area.x.saturating_add(*cell_cursor),
                    row,
                    linked_width.min(description_area.width),
                    1,
                ),
            ));
        }
        *cell_cursor = cell_cursor.saturating_add(linked_width);
        source_cursor = end;
    }
    if source_cursor < end_byte {
        let plain = &details.description[source_cursor..end_byte];
        *cell_cursor = cell_cursor.saturating_add(terminal_text_width(plain));
        spans.push(Span::raw(plain));
    }
}

/// Finds the complete physical description line for the chapter that owns the
/// current playback position.
fn active_description_chapter_line(
    view: &ViewModel,
    details: &DetailView,
) -> Option<std::ops::Range<usize>> {
    if details.media_id.as_ref() != view.playing_media_id.as_ref() {
        return None;
    }
    let active = current_chapter_index(&view.playback_chapters, view.playback.position)?;
    let seconds = view.playback_chapters.get(active)?.start_seconds;
    let timecode = details
        .timecodes
        .iter()
        .find(|timecode| timecode.is_chapter && timecode.seconds == seconds)?;
    let start = details.description[..timecode.start_byte]
        .rfind('\n')
        .map_or(0, |newline| newline.saturating_add(1));
    let end = details.description[timecode.end_byte..]
        .find('\n')
        .map_or(details.description.len(), |offset| {
            timecode.end_byte.saturating_add(offset)
        });
    Some(start..end)
}

/// Maximum clipboard payload reconstructed from one Details-panel drag.
pub(crate) const MAX_DETAILS_SELECTION_BYTES: usize = 64 * 1024;

/// Records only non-padding cells from one explicitly selectable text row.
fn capture_selectable_details_row(frame: &mut Frame<'_>, hit_map: &mut HitMap, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut cells = (area.left()..area.right())
        .map(|x| frame.buffer_mut()[(x, area.y)].symbol().to_owned())
        .collect::<Vec<_>>();
    while cells
        .last()
        .is_some_and(|symbol| symbol.is_empty() || symbol.chars().all(char::is_whitespace))
    {
        cells.pop();
    }
    if !cells.is_empty() {
        hit_map.detail_text_rows.push(SelectableDetailsRow {
            x: area.x,
            y: area.y,
            cells,
        });
    }
}

impl HitMap {
    /// Maps a pointer to a semantic Details position.
    ///
    /// A press must land on an exact text cell. Drag and release events use
    /// clipping so leaving the selectable region extends only to its nearest
    /// visible boundary, never into the result panel or thumbnail.
    fn details_text_position(&self, x: u16, y: u16, clip: bool) -> Option<DetailsTextPosition> {
        if let Some((row, mapped)) = self.detail_text_rows.iter().enumerate().find(|(_, row)| {
            row.y == y && usize::from(x.saturating_sub(row.x)) < row.cells.len() && x >= row.x
        }) {
            return Some(DetailsTextPosition {
                row,
                column: usize::from(x.saturating_sub(mapped.x)),
            });
        }
        if !clip {
            return None;
        }

        let (row, mapped) = self
            .detail_text_rows
            .iter()
            .enumerate()
            .min_by_key(|(_, row)| row.y.abs_diff(y))?;
        let column = if x <= mapped.x {
            0
        } else {
            usize::from(x.saturating_sub(mapped.x)).min(mapped.cells.len().saturating_sub(1))
        };
        Some(DetailsTextPosition { row, column })
    }

    /// Reconstructs the selected visible cells with semantic row separators.
    fn selected_details_text(&self, selection: DetailsTextSelection) -> String {
        let (start, end) = ordered_details_positions(selection.anchor, selection.focus);
        let mut selected = String::new();
        for row_index in start.row..=end.row {
            let Some(row) = self.detail_text_rows.get(row_index) else {
                continue;
            };
            let first = if row_index == start.row {
                start.column
            } else {
                0
            };
            let last = if row_index == end.row {
                end.column
            } else {
                row.cells.len().saturating_sub(1)
            };
            if !selected.is_empty() {
                selected.push('\n');
            }
            if first <= last {
                for symbol in row
                    .cells
                    .iter()
                    .skip(first)
                    .take(last.saturating_sub(first).saturating_add(1))
                {
                    selected.push_str(symbol);
                }
            }
        }
        truncate_utf8(&mut selected, MAX_DETAILS_SELECTION_BYTES);
        selected
    }
}

fn ordered_details_positions(
    first: DetailsTextPosition,
    second: DetailsTextPosition,
) -> (DetailsTextPosition, DetailsTextPosition) {
    if first <= second {
        (first, second)
    } else {
        (second, first)
    }
}

fn truncate_utf8(value: &mut String, maximum_bytes: usize) {
    if value.len() <= maximum_bytes {
        return;
    }
    let mut boundary = maximum_bytes;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
}

/// Applies selection styling only to cells registered as Details text.
fn highlight_details_text_selection(
    frame: &mut Frame<'_>,
    view: &ViewModel,
    hit_map: &HitMap,
    theme: &Theme,
) {
    let Some(selection) = view.details_text_selection else {
        return;
    };
    let (start, end) = ordered_details_positions(selection.anchor, selection.focus);
    for row_index in start.row..=end.row {
        let Some(row) = hit_map.detail_text_rows.get(row_index) else {
            continue;
        };
        let first = if row_index == start.row {
            start.column
        } else {
            0
        };
        let last = if row_index == end.row {
            end.column
        } else {
            row.cells.len().saturating_sub(1)
        };
        for column in first..=last.min(row.cells.len().saturating_sub(1)) {
            let x = row
                .x
                .saturating_add(u16::try_from(column).unwrap_or(u16::MAX));
            frame.buffer_mut()[(x, row.y)].set_style(theme.selected);
        }
    }
}

fn is_creative_commons_license(label: &str) -> bool {
    let normalized = label.trim().to_ascii_lowercase();
    normalized.contains("creative commons") || normalized.contains("creativecommons.org")
}

/// Formats one non-negative count with comma-separated thousands groups.
fn format_count(count: u64) -> String {
    let digits = count.to_string();
    let mut formatted = String::with_capacity(digits.len().saturating_add(digits.len() / 3));
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    formatted
}

/// Canonicalizes `YouTube`'s localized spelling of its CC Attribution label.
///
/// Exact licence URLs and other provider terms remain unchanged because they
/// can carry information required by attribution or upload workflows.
fn display_license_label(label: &str) -> &str {
    let normalized = label.trim();
    if normalized.eq_ignore_ascii_case("Creative Commons Attribution")
        || normalized.eq_ignore_ascii_case("Creative Commons Attribution licence")
        || normalized.eq_ignore_ascii_case("Creative Commons Attribution license")
    {
        "Creative Commons Attribution"
    } else {
        label
    }
}

fn render_channel(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    show_hotkeys: bool,
    thumbnail_height: u16,
    theme: &Theme,
    hit_map: &mut HitMap,
    thumbnail_renderer: Option<&mut dyn ThumbnailRenderer>,
) {
    render_information_panel(
        frame,
        area,
        view,
        show_hotkeys,
        theme,
        hit_map,
        " Channel ",
        "No channel is selected.",
        InformationPanelKind::Channel,
        false,
        thumbnail_height,
        thumbnail_renderer,
    );
}

fn render_waveform(frame: &mut Frame<'_>, area: Rect, view: &ViewModel, theme: &Theme) {
    let inner = render_main_panel_heading(frame, area, "Waveform — click to seek", theme.heading);
    if view.waveform.is_empty() || inner.width == 0 || inner.height == 0 {
        frame.render_widget(
            Paragraph::new("Waveform is generated lazily for downloaded or cached audio.")
                .style(theme.muted)
                .wrap(Wrap { trim: true }),
            inner,
        );
        return;
    }

    let columns = usize::from(inner.width);
    let mut text = String::with_capacity(columns);
    for column in 0..columns {
        let index = column
            .saturating_mul(view.waveform.len())
            .checked_div(columns)
            .unwrap_or_default()
            .min(view.waveform.len().saturating_sub(1));
        let peak = view.waveform[index];
        let amplitude = i32::from(peak.maximum)
            .abs()
            .max(i32::from(peak.minimum).abs());
        let symbol = match amplitude {
            0..=4095 => '▁',
            4096..=8191 => '▂',
            8192..=12_287 => '▃',
            12_288..=16_383 => '▄',
            16_384..=20_479 => '▅',
            20_480..=24_575 => '▆',
            24_576..=28_671 => '▇',
            _ => '█',
        };
        text.push(symbol);
    }
    let vertical_padding = inner.height.saturating_sub(1) / 2;
    let waveform_area = Rect::new(
        inner.x,
        inner.y.saturating_add(vertical_padding),
        inner.width,
        1,
    );
    frame.render_widget(Paragraph::new(text).style(theme.accent), waveform_area);
}

fn render_seek_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &ViewModel,
    settings: &UiSettings,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    hit_map.seek_markers.clear();
    hit_map.now_playing = None;
    let duration = view.playback.duration.unwrap_or(Duration::ZERO);
    let ratio = if duration.is_zero() {
        0.0
    } else {
        (view.playback.position.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0)
    };
    let state = if view.playback.buffering {
        "buffering"
    } else if view.playback.paused {
        "paused"
    } else {
        "playing"
    };
    let marker = match settings.seek_bar_style {
        SeekBarStyle::Line => "",
        SeekBarStyle::NyanCat => " =^.^= ",
    };
    let status_prefix = format!(
        "{} / {}  {}×  vol {}%{} {state}{}",
        format_duration(view.playback.position),
        if duration.is_zero() {
            "--:--".to_owned()
        } else {
            format_duration(duration)
        },
        trim_speed(view.playback.speed),
        view.playback.volume,
        if view.repeating { "  repeat" } else { "" },
        if !view.playback.idle && view.playback.title.is_some() {
            " "
        } else {
            ""
        },
    );
    let title = (!view.playback.idle)
        .then_some(view.playback.title.as_deref())
        .flatten();
    let title_offset = title.map(|_| terminal_text_width(&status_prefix));
    let title_width = title.map(terminal_text_width).unwrap_or_default();
    let label = format!("{status_prefix}{}{marker}", title.unwrap_or_default());
    if !view.playback_chapters.is_empty() && area.height >= 3 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
        let label_area = rows[0];
        let track_area = rows[1];
        let status_area = rows[2];
        let gauge = Gauge::default()
            .gauge_style(theme.progress)
            .ratio(ratio)
            .label("");
        frame.render_widget(gauge, track_area);
        render_buffered_ranges(frame, track_area, &view.playback, duration, theme);
        render_chapter_timeline(
            frame, label_area, track_area, view, duration, theme, hit_map,
        );
        render_seek_status(
            frame,
            status_area,
            &label,
            title_offset,
            title_width,
            hit_map,
        );
        hit_map.seek_bar = track_area;
    } else if area.height >= 2 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        let track_area = rows[0];
        let status_area = rows[1];
        let gauge = Gauge::default()
            .gauge_style(theme.progress)
            .ratio(ratio)
            .label("");
        frame.render_widget(gauge, track_area);
        render_buffered_ranges(frame, track_area, &view.playback, duration, theme);
        if !view.playback_chapters.is_empty() {
            render_chapter_timeline(
                frame,
                Rect::new(track_area.x, track_area.y, track_area.width, 0),
                track_area,
                view,
                duration,
                theme,
                hit_map,
            );
        }
        render_seek_status(
            frame,
            status_area,
            &label,
            title_offset,
            title_width,
            hit_map,
        );
        hit_map.seek_bar = track_area;
    } else {
        let visible_label = truncate_terminal_text(&label, usize::from(area.width));
        let gauge = Gauge::default()
            .gauge_style(theme.progress)
            .ratio(ratio)
            .label(visible_label.clone());
        frame.render_widget(gauge, area);
        render_buffered_ranges(frame, area, &view.playback, duration, theme);
        restore_seek_label(frame, area, &visible_label);
        set_now_playing_target(area, &visible_label, title_offset, title_width, hit_map);
        hit_map.seek_bar = area;
    }
}

/// Renders the status beneath the seek track and records only the visible
/// now-playing title as a mouse target.
fn render_seek_status(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    title_offset: Option<u16>,
    title_width: u16,
    hit_map: &mut HitMap,
) {
    let visible_label = truncate_terminal_text(label, usize::from(area.width));
    frame.render_widget(
        Paragraph::new(visible_label.clone()).alignment(Alignment::Center),
        area,
    );
    set_now_playing_target(area, &visible_label, title_offset, title_width, hit_map);
}

/// Maps the visible title portion of a centered seek-status label.
fn set_now_playing_target(
    area: Rect,
    visible_label: &str,
    title_offset: Option<u16>,
    title_width: u16,
    hit_map: &mut HitMap,
) {
    let Some(title_offset) = title_offset else {
        return;
    };
    let visible_width = terminal_text_width(visible_label).min(area.width);
    if title_offset >= visible_width {
        return;
    }
    let label_x = centered_line_x(area, visible_width);
    let visible_title_width = title_width.min(visible_width.saturating_sub(title_offset));
    if visible_title_width > 0 {
        hit_map.now_playing = Some(Rect::new(
            label_x.saturating_add(title_offset),
            area.y,
            visible_title_width,
            1,
        ));
    }
}

fn render_chapter_timeline(
    frame: &mut Frame<'_>,
    label_area: Rect,
    track_area: Rect,
    view: &ViewModel,
    duration: Duration,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    if track_area.is_empty() {
        return;
    }
    let visible_indices = visible_chapter_indices(view, duration);
    let current = current_visible_chapter_index(view, &visible_indices);
    if duration.is_zero() {
        if !label_area.is_empty()
            && let Some(index) = current
        {
            let chapter = &view.playback_chapters[index];
            let label = truncate_terminal_text(
                &if view.show_chapter_timestamps {
                    format!(
                        "▶ {} {}",
                        format_duration(Duration::from_secs(chapter.start_seconds)),
                        chapter_title_for_display(&chapter.title)
                    )
                } else {
                    format!("▶ {}", chapter_title_for_display(&chapter.title))
                },
                usize::from(label_area.width),
            );
            frame.render_widget(Paragraph::new(label).style(theme.accent), label_area);
        }
        return;
    }

    let mut marker_columns = vec![Vec::new(); usize::from(track_area.width)];
    for index in visible_indices.iter().copied() {
        let chapter = &view.playback_chapters[index];
        if chapter.start_seconds >= duration.as_secs() {
            continue;
        }
        let column = rounded_duration_column(
            Duration::from_secs(chapter.start_seconds),
            duration,
            track_area.width.saturating_sub(1),
        );
        if let Some(indices) = marker_columns.get_mut(usize::from(column)) {
            indices.push(index);
        }
    }

    let marker_action = |seconds| {
        view.playing_media_id
            .as_ref()
            .map(|media_id| UiAction::ActivateTimecode {
                media_id: media_id.clone(),
                seconds,
            })
    };
    for (column, indices) in marker_columns
        .iter()
        .enumerate()
        .filter(|(_, indices)| !indices.is_empty())
    {
        let selected = current
            .filter(|current| indices.contains(current))
            .unwrap_or(indices[0]);
        let active = current == Some(selected);
        let glyph = if indices.len() > 1 {
            "┇"
        } else if active {
            "┃"
        } else {
            "│"
        };
        let x = track_area
            .x
            .saturating_add(u16::try_from(column).unwrap_or(track_area.width.saturating_sub(1)));
        frame.buffer_mut()[(x, track_area.y)]
            .set_symbol(glyph)
            .set_style(if active {
                theme.accent.add_modifier(Modifier::BOLD)
            } else {
                theme.muted
            });
        if let Some(action) = marker_action(view.playback_chapters[selected].start_seconds) {
            hit_map
                .seek_markers
                .push((action, Rect::new(x, track_area.y, 1, 1)));
        }
    }

    if label_area.is_empty() {
        return;
    }

    let layout = chapter_label_layout(
        view,
        duration,
        label_area.width,
        label_area.height.min(MAX_CHAPTER_LABEL_ROWS),
    );
    for placement in layout.placements {
        let chapter = &view.playback_chapters[placement.index];
        let label_rect = Rect::new(
            label_area.x.saturating_add(placement.start),
            label_area.y.saturating_add(placement.row),
            placement.width,
            1,
        );
        frame.render_widget(
            Paragraph::new(placement.text).style(if Some(placement.index) == current {
                theme.accent.add_modifier(Modifier::BOLD)
            } else {
                theme.muted
            }),
            label_rect,
        );
        if let Some(action) = marker_action(chapter.start_seconds) {
            hit_map.seek_markers.push((action, label_rect));
        }
    }
}

/// Formats one chapter label while keeping its seek position independent from
/// the user's timestamp-visibility preference.
fn chapter_timeline_label(
    chapter: &Chapter,
    current: bool,
    show_timestamp: bool,
    show_hours: bool,
) -> String {
    let prefix = if current { "▶ " } else { "" };
    if show_timestamp {
        format!(
            "{prefix}{} {}",
            format_timeline_timestamp(chapter.start_seconds, show_hours),
            chapter_title_for_display(&chapter.title)
        )
    } else {
        format!("{prefix}{}", chapter_title_for_display(&chapter.title))
    }
}

fn current_chapter_index(chapters: &[Chapter], position: Duration) -> Option<usize> {
    chapters
        .iter()
        .rposition(|chapter| chapter.start_seconds <= position.as_secs())
}

/// Returns chapters that remain visible and navigable for the current
/// advertisement preference and known media duration.
fn visible_chapter_indices(view: &ViewModel, duration: Duration) -> Vec<usize> {
    view.playback_chapters
        .iter()
        .enumerate()
        .filter(|(_, chapter)| {
            (duration.is_zero() || chapter.start_seconds < duration.as_secs())
                && (!view.skip_advertisement_chapters
                    || !is_advertisement_chapter_title(&chapter.title))
        })
        .map(|(index, _)| index)
        .collect()
}

fn current_visible_chapter_index(view: &ViewModel, visible: &[usize]) -> Option<usize> {
    visible.iter().copied().rfind(|index| {
        view.playback_chapters[*index].start_seconds <= view.playback.position.as_secs()
    })
}

fn format_timeline_timestamp(seconds: u64, show_hours: bool) -> String {
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    if show_hours {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn truncate_terminal_text(value: &str, maximum_width: usize) -> String {
    if Span::raw(value).width() <= maximum_width {
        return value.to_owned();
    }
    if maximum_width == 0 {
        return String::new();
    }
    if maximum_width == 1 {
        return "…".to_owned();
    }
    let mut output = String::new();
    let mut width = 0_usize;
    for character in value.chars() {
        let character_width = Span::raw(character.to_string()).width();
        if width.saturating_add(character_width) >= maximum_width {
            break;
        }
        output.push(character);
        width = width.saturating_add(character_width);
    }
    output.push('…');
    output
}

fn render_buffered_ranges(
    frame: &mut Frame<'_>,
    area: Rect,
    playback: &PlaybackStatus,
    duration: Duration,
    theme: &Theme,
) {
    if area.is_empty() || duration.is_zero() {
        return;
    }
    let played_end = rounded_duration_column(playback.position, duration, area.width);
    for range in &playback.buffered_ranges {
        let start = range.start.min(duration);
        let end = range.end.min(duration);
        if start >= end {
            continue;
        }
        let start = floored_duration_column(start, duration, area.width).max(played_end);
        let end = ceiled_duration_column(end, duration, area.width);
        if start >= end {
            continue;
        }
        for y in area.top()..area.bottom() {
            for offset in start..end {
                frame.buffer_mut()[(area.x.saturating_add(offset), y)]
                    .set_symbol(ratatui::symbols::block::FULL)
                    .set_style(theme.cached);
            }
        }
    }
}

fn floored_duration_column(value: Duration, duration: Duration, width: u16) -> u16 {
    let total = duration.as_nanos();
    if total == 0 {
        return 0;
    }
    let column = value
        .min(duration)
        .as_nanos()
        .saturating_mul(u128::from(width))
        / total;
    u16::try_from(column).unwrap_or(width).min(width)
}

fn ceiled_duration_column(value: Duration, duration: Duration, width: u16) -> u16 {
    let total = duration.as_nanos();
    if total == 0 {
        return 0;
    }
    let numerator = value
        .min(duration)
        .as_nanos()
        .saturating_mul(u128::from(width));
    let column = numerator.saturating_add(total.saturating_sub(1)) / total;
    u16::try_from(column).unwrap_or(width).min(width)
}

fn rounded_duration_column(value: Duration, duration: Duration, width: u16) -> u16 {
    let total = duration.as_nanos();
    if total == 0 {
        return 0;
    }
    let numerator = value
        .min(duration)
        .as_nanos()
        .saturating_mul(u128::from(width));
    let column = numerator.saturating_add(total / 2) / total;
    u16::try_from(column).unwrap_or(width).min(width)
}

fn restore_seek_label(frame: &mut Frame<'_>, area: Rect, label: &str) {
    if area.is_empty() {
        return;
    }
    let label = Span::raw(label);
    let width = u16::try_from(label.width())
        .unwrap_or(u16::MAX)
        .min(area.width);
    let x = area.x.saturating_add(area.width.saturating_sub(width) / 2);
    let y = area.y.saturating_add(area.height / 2);
    // A raw span has no foreground or background fields, so `set_span`
    // replaces the glyphs while preserving the played/cached cell styles.
    frame.buffer_mut().set_span(x, y, &label, width);
}

fn render_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    settings: &UiSettings,
    theme: &Theme,
    screen: Screen,
    youtube_search_sort: YouTubeSearchSort,
    youtube_creative_commons_only: bool,
    show_chapter_timestamps: bool,
    autoplay: bool,
    local_size_sort: Option<LocalSizeSort>,
    status: &str,
    playback_active: bool,
    hit_map: &mut HitMap,
) {
    let compact_search_leading_labels = screen == Screen::Search
        && search_primary_prefix_needs_compaction(
            area.width,
            youtube_creative_commons_only,
            autoplay,
            settings.show_hotkeys,
        );
    let mut primary_buttons = Vec::with_capacity(12);
    primary_buttons.push((
        button("/", "Search", settings.show_hotkeys),
        UiAction::BeginSearch,
    ));
    if screen == Screen::Search {
        primary_buttons.push((
            button(
                "C",
                if compact_search_leading_labels && youtube_creative_commons_only {
                    "CC:on"
                } else if compact_search_leading_labels {
                    "CC:off"
                } else if youtube_creative_commons_only {
                    "CC only: on"
                } else {
                    "CC only: off"
                },
                settings.show_hotkeys,
            ),
            UiAction::ToggleYouTubeCreativeCommons,
        ));
    }
    if screen == Screen::Local
        && let Some(local_size_sort) = local_size_sort
    {
        primary_buttons.push((
            button("Z", local_size_sort.label(), settings.show_hotkeys),
            UiAction::ToggleLocalSizeSort,
        ));
    }
    primary_buttons.push((
        button(
            "Tab",
            if compact_search_leading_labels {
                "Next"
            } else {
                "Next tab"
            },
            settings.show_hotkeys,
        ),
        UiAction::ShowScreen(screen.next()),
    ));
    primary_buttons.push((
        button(
            "S",
            if compact_search_leading_labels {
                "Subs"
            } else {
                "Subscriptions"
            },
            settings.show_hotkeys,
        ),
        UiAction::ShowScreen(Screen::Subscriptions),
    ));
    primary_buttons.push((
        button("Space", "Pause", settings.show_hotkeys),
        UiAction::TogglePause,
    ));
    primary_buttons.push((
        button(
            "A",
            if autoplay {
                "Autoplay: on"
            } else {
                "Autoplay: off"
            },
            settings.show_hotkeys,
        ),
        UiAction::ToggleAutoplay,
    ));
    primary_buttons.push((
        button("p", "Preferences", settings.show_hotkeys),
        UiAction::OpenPreferences,
    ));
    if screen == Screen::Search {
        primary_buttons.push((
            button(
                "N",
                match youtube_search_sort {
                    YouTubeSearchSort::Relevance => "Sort: relevance",
                    YouTubeSearchSort::Newest => "Sort: newest",
                },
                settings.show_hotkeys,
            ),
            UiAction::ToggleYouTubeSearchSort,
        ));
    }
    primary_buttons.push((
        button("?", "Help", settings.show_hotkeys),
        UiAction::ToggleHelp,
    ));
    primary_buttons.push((
        button("M", "MOD/tracker music", settings.show_hotkeys),
        UiAction::ShowScreen(Screen::TrackerMusic),
    ));
    if screen == Screen::Search {
        primary_buttons.push((
            button("v", "Videos/channels", settings.show_hotkeys),
            UiAction::ToggleSearchKind,
        ));
    }
    primary_buttons.push((
        button("d", "Download", settings.show_hotkeys),
        UiAction::Download,
    ));
    primary_buttons.push((
        button("w", "Waveform", settings.show_hotkeys),
        UiAction::ToggleWaveform,
    ));
    let full_navigation_buttons = [
        (
            button("k", "Move up", settings.show_hotkeys),
            UiAction::MoveSelection(-1),
        ),
        (
            button("j", "Move down", settings.show_hotkeys),
            UiAction::MoveSelection(1),
        ),
        (
            button("↑", "Volume up", settings.show_hotkeys),
            UiAction::ChangeVolume(5),
        ),
        (
            button("↓", "Volume down", settings.show_hotkeys),
            UiAction::ChangeVolume(-5),
        ),
        (
            button("Enter", "Start", settings.show_hotkeys),
            UiAction::ActivateSelection,
        ),
        (
            button(
                "T",
                if show_chapter_timestamps {
                    "Chapter times: on"
                } else {
                    "Chapter times: off"
                },
                settings.show_hotkeys,
            ),
            UiAction::ToggleChapterTimestamps,
        ),
    ];
    let full_navigation_width = full_navigation_buttons
        .iter()
        .map(|(label, _)| usize::from(terminal_text_width(label)))
        .sum::<usize>()
        .saturating_add((full_navigation_buttons.len() - 1) * 2);
    let navigation_buttons = if full_navigation_width <= usize::from(area.width) {
        full_navigation_buttons
    } else {
        [
            (
                button("k", "Up", settings.show_hotkeys),
                UiAction::MoveSelection(-1),
            ),
            (
                button("j", "Down", settings.show_hotkeys),
                UiAction::MoveSelection(1),
            ),
            (
                button("↑", "Vol+", settings.show_hotkeys),
                UiAction::ChangeVolume(5),
            ),
            (
                button("↓", "Vol-", settings.show_hotkeys),
                UiAction::ChangeVolume(-5),
            ),
            (
                button("Enter", "Start", settings.show_hotkeys),
                UiAction::ActivateSelection,
            ),
            (
                button(
                    "T",
                    if show_chapter_timestamps {
                        "Time:on"
                    } else {
                        "Time:off"
                    },
                    settings.show_hotkeys,
                ),
                UiAction::ToggleChapterTimestamps,
            ),
        ]
    };
    let primary_controls = primary_buttons
        .iter()
        .map(|(label, _)| label.as_str())
        .collect::<Vec<_>>()
        .join("  ");
    let navigation_controls = navigation_buttons
        .iter()
        .map(|(label, _)| label.as_str())
        .collect::<Vec<_>>()
        .join("  ");
    let available = usize::from(area.width);
    let displayed_status = if playback_active && status.trim_start().starts_with("Playing ") {
        ""
    } else {
        status
    };
    let primary_controls_width = Span::raw(primary_controls.as_str()).width();
    let status_width = Span::raw(displayed_status).width();
    let status_is_appended = primary_controls_width + status_width + 3 <= available;
    let primary_line = if status_is_appended && !displayed_status.is_empty() {
        format!("{primary_controls} │ {displayed_status}")
    } else {
        primary_controls
    };
    let lines = if area.height > 1 {
        vec![
            Line::raw(primary_line.clone()),
            Line::raw(navigation_controls.clone()),
        ]
    } else {
        vec![Line::raw(navigation_controls.clone())]
    };
    let button_rows = if area.height > 1 {
        vec![
            (primary_buttons.as_slice(), primary_line.as_str(), area.y),
            (
                navigation_buttons.as_slice(),
                navigation_controls.as_str(),
                area.y.saturating_add(1),
            ),
        ]
    } else {
        vec![(
            navigation_buttons.as_slice(),
            navigation_controls.as_str(),
            area.y,
        )]
    };
    hit_map.buttons.clear();
    for (buttons, line, y) in button_rows {
        let line_width = terminal_text_width(line);
        let mut button_x = centered_line_x(area, line_width);
        for (label, action) in buttons {
            let width = terminal_text_width(label);
            let visible_width = area.right().saturating_sub(button_x).min(width);
            if visible_width > 0 {
                hit_map
                    .buttons
                    .push((action.clone(), Rect::new(button_x, y, visible_width, 1)));
            }
            button_x = button_x.saturating_add(width).saturating_add(2);
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Center)
            .style(theme.base),
        area,
    );
}

/// Detects when Search's leading labels must contract to keep Autoplay visible.
///
/// The compact labels preserve the same actions and leave the frequently used
/// Search, filter, tab, subscription, and pause controls ahead of Autoplay.
fn search_primary_prefix_needs_compaction(
    available_width: u16,
    youtube_creative_commons_only: bool,
    autoplay: bool,
    show_hotkeys: bool,
) -> bool {
    let controls = [
        button("/", "Search", show_hotkeys),
        button(
            "C",
            if youtube_creative_commons_only {
                "CC only: on"
            } else {
                "CC only: off"
            },
            show_hotkeys,
        ),
        button("Tab", "Next tab", show_hotkeys),
        button("S", "Subscriptions", show_hotkeys),
        button("Space", "Pause", show_hotkeys),
        button(
            "A",
            if autoplay {
                "Autoplay: on"
            } else {
                "Autoplay: off"
            },
            show_hotkeys,
        ),
    ];
    terminal_text_width(&controls.join("  ")) > available_width
}

fn centered_line_x(area: Rect, line_width: u16) -> u16 {
    area.x
        .saturating_add((area.width / 2).saturating_sub(line_width.min(area.width) / 2))
}

fn render_help(frame: &mut Frame<'_>, theme: &Theme) {
    let area = centered_rect(76, 92, frame.area());
    frame.render_widget(Clear, area);
    let help = [
        "Navigation",
        "  / search     Tab next tab     Shift+Tab previous tab     S subscriptions",
        "  Ctrl+Tab/Ctrl+Shift+Tab are aliases when the terminal distinguishes them.",
        "  F2 offline     F3 history",
        "  F4 playlists     F5 stats     M/F6 MOD/tracker music     p preferences",
        "  v video/channel search     N relevance/newest     C CC-only videos",
        "  j/k select     Enter open/play",
        "  Local: Z size order     r rename     Delete move to Trash",
        "  Subscriptions channel: R refresh videos     i description",
        "  F8 pointer: arrows move, Enter clicks, Esc/F8 exits.",
        "  Linux /dev/ttyN: GPM mouse input is detected automatically.",
        "",
        "Playback",
        "  Space pause     ←/→ 5 s     0–9 seek by 10%",
        "  ↑/↓ volume     </> speed 10%     [/] chapter     T chapter times",
        "  r repeat     A autoplay next item from the same source list",
        "  w waveform     Alt+←/→ Details back/forward     Backspace Details back",
        "",
        "Actions",
        "  n play next     a add to queue     d download     o video page",
        "  O channel page     i subscription description     p preferences",
        "  y copy link     c channel info     s local subscribe/unsubscribe",
        "  m private note     e equalizer     t Details-only text selection",
        "  Alt+j/k select external link     Alt+Enter open selected link",
        "  ↪ internal video: click after a YouTube URL to open it in Details",
        "",
        "Mouse",
        "  Click Details; wheel/PageUp/PageDown scroll.",
        "  Press t, then drag visible Details text to copy it; t/Esc exits selection.",
        "  The result list, borders, buttons, scrollbar, and thumbnail are never selected.",
        "  Click tabs, rows, links, buttons, seek; wheel elsewhere selects rows.",
        "",
        "Press ? or Esc to close help. Press q or Ctrl+C to quit.",
    ];
    frame.render_widget(
        Paragraph::new(help.join("\n"))
            .block(panel_block(" Youta help ", theme))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_error_popup(
    frame: &mut Frame<'_>,
    error: &ErrorPopupView,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let area = centered_rect(92, 88, frame.area());
    frame.render_widget(Clear, area);
    let title = if error.title.trim().is_empty() {
        " Youta error ".to_owned()
    } else {
        format!(" {} ", error.title.trim())
    };
    frame.render_widget(panel_block(&title, theme), area);

    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let report_area = sections[0];
    let position_area = sections[1];
    let buttons_area = sections[2];

    let (report_text_area, scrollbar_area) = if report_area.width > 1 {
        let report_columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(report_area);
        (report_columns[0], report_columns[1])
    } else {
        (report_area, Rect::default())
    };
    let report_lines =
        wrap_diagnostic_report(&error.report, usize::from(report_text_area.width.max(1)));
    let visible_lines = usize::from(report_text_area.height);
    let maximum_offset = report_lines.len().saturating_sub(visible_lines);
    let offset = error.scroll_offset.min(maximum_offset);
    let visible = report_lines
        .iter()
        .skip(offset)
        .take(visible_lines)
        .cloned()
        .map(Line::raw)
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(visible)
            .style(theme.base)
            .wrap(Wrap { trim: false }),
        report_text_area,
    );
    if report_lines.len() > visible_lines && scrollbar_area.width > 0 && scrollbar_area.height > 0 {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .track_style(theme.muted)
            .thumb_symbol("█")
            .thumb_style(theme.accent);
        let mut scrollbar_state = ScrollbarState::new(report_lines.len())
            .position(offset)
            .viewport_content_length(visible_lines);
        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
    }

    let first_line = if report_lines.is_empty() || visible_lines == 0 {
        0
    } else {
        offset.saturating_add(1)
    };
    let last_line = offset.saturating_add(visible_lines).min(report_lines.len());
    let position = format!("Lines {first_line}–{last_line} of {}", report_lines.len());
    let position = if let Some(status) = &error.action_status {
        format!("{status} | {position}")
    } else {
        position
    };
    frame.render_widget(
        Paragraph::new(position)
            .alignment(Alignment::Right)
            .style(theme.muted),
        position_area,
    );

    let mut buttons = vec![
        ("[c] Copy", UiAction::CopyErrorReport),
        ("[i] Copy + open issue", UiAction::CopyAndOpenGitHubIssue),
    ];
    if error.gh_available {
        buttons.push(("[g] Fill GitHub issue", UiAction::FillGitHubIssue));
    }
    buttons.push(("[Esc] Close", UiAction::DismissErrorPopup));
    let labels_width = buttons
        .iter()
        .map(|(label, _)| label.chars().count())
        .sum::<usize>()
        .saturating_add(buttons.len().saturating_sub(1) * 3);
    let labels_width = u16::try_from(labels_width).unwrap_or(u16::MAX);
    let mut button_x = buttons_area
        .x
        .saturating_add(buttons_area.width.saturating_sub(labels_width) / 2);
    let controls = buttons
        .iter()
        .map(|(label, _)| *label)
        .collect::<Vec<_>>()
        .join("   ");
    frame.render_widget(
        Paragraph::new(controls.as_str())
            .alignment(Alignment::Center)
            .style(theme.accent),
        buttons_area,
    );
    for (label, action) in buttons {
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        hit_map
            .error_buttons
            .push((action, Rect::new(button_x, buttons_area.y, width, 1)));
        button_x = button_x.saturating_add(width).saturating_add(3);
    }
}

fn render_youtube_setup_popup(
    frame: &mut Frame<'_>,
    setup: &YouTubeSetupPopupView,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let height_percent = if frame.area().height < 30 { 100 } else { 88 };
    let width_percent = if frame.area().width < 90 { 100 } else { 94 };
    let area = centered_rect(width_percent, height_percent, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(" Configure YouTube metadata ", theme), area);

    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(9),
            Constraint::Min(if setup.validation_error.is_some() {
                4
            } else {
                3
            }),
            Constraint::Length(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new("Choose one metadata provider. Tab/↑/↓ switches; Enter saves and retries.")
            .style(theme.base)
            .wrap(Wrap { trim: false }),
        sections[0],
    );

    let api_selected = setup.selected_field == YouTubeSetupField::ApiKey;
    let api_key = masked_setup_value(
        &setup.api_key,
        usize::from(sections[1].width.saturating_sub(2)),
    );
    let api_key = if api_key.is_empty() {
        "enter an API key".to_owned()
    } else {
        api_key
    };
    frame.render_widget(
        Paragraph::new(api_key)
            .style(if api_selected {
                theme.accent
            } else {
                theme.muted
            })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(if api_selected {
                        theme.accent
                    } else {
                        theme.border
                    })
                    .title(if api_selected {
                        " ▶ YouTube API key (masked) "
                    } else {
                        " YouTube API key (masked) "
                    }),
            ),
        sections[1],
    );
    hit_map
        .youtube_setup_fields
        .push((YouTubeSetupField::ApiKey, sections[1]));

    let invidious_selected = setup.selected_field == YouTubeSetupField::InvidiousUrl;
    let invidious = if setup.invidious_url.is_empty() {
        "https://your-invidious-instance.example".to_owned()
    } else {
        truncate_setup_value(
            &setup.invidious_url,
            usize::from(sections[2].width.saturating_sub(2)),
        )
    };
    frame.render_widget(
        Paragraph::new(invidious)
            .style(if invidious_selected {
                theme.accent
            } else {
                theme.muted
            })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(if invidious_selected {
                        theme.accent
                    } else {
                        theme.border
                    })
                    .title(if invidious_selected {
                        " ▶ Invidious instance URL "
                    } else {
                        " Invidious instance URL "
                    }),
            ),
        sections[2],
    );
    hit_map
        .youtube_setup_fields
        .push((YouTubeSetupField::InvidiousUrl, sections[2]));

    let guide_sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(4),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(2),
            Constraint::Length(1),
        ])
        .split(sections[3]);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("YouTube API-key steps: ", theme.heading),
            Span::raw(
                "(1) create/select a Google Cloud project; (2) enable YouTube Data API v3; \
                 (3) Credentials > Create credentials > API key; (4) edit key > API \
                 restrictions > Restrict key > YouTube Data API v3 > Save; (5) paste it above. \
                 Restriction blocks other Google APIs.",
            ),
        ]))
        .style(theme.base)
        .wrap(Wrap { trim: false }),
        guide_sections[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("[F1] Google guide: ", theme.accent),
            Span::styled(
                YOUTUBE_API_KEY_GUIDE_URL.trim_start_matches("https://"),
                theme.accent.add_modifier(Modifier::UNDERLINED),
            ),
        ]))
        .wrap(Wrap { trim: false }),
        guide_sections[1],
    );
    hit_map
        .youtube_setup_buttons
        .push((UiAction::OpenYouTubeApiKeyGuide, guide_sections[1]));

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("[F2] Google Cloud: ", theme.accent),
            Span::styled(
                GOOGLE_CLOUD_CREDENTIALS_URL.trim_start_matches("https://"),
                theme.accent.add_modifier(Modifier::UNDERLINED),
            ),
        ]))
        .wrap(Wrap { trim: false }),
        guide_sections[2],
    );
    hit_map
        .youtube_setup_buttons
        .push((UiAction::OpenGoogleCloudCredentials, guide_sections[2]));

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Invidious: ", theme.heading),
            Span::raw(
                "choose a public instance from the official list, or paste your self-hosted base \
                 URL above.",
            ),
        ]))
        .style(theme.base)
        .wrap(Wrap { trim: false }),
        guide_sections[3],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("[F3] Instance list: ", theme.accent),
            Span::styled(
                INVIDIOUS_INSTANCES_URL.trim_start_matches("https://"),
                theme.accent.add_modifier(Modifier::UNDERLINED),
            ),
        ]))
        .wrap(Wrap { trim: false }),
        guide_sections[4],
    );
    hit_map
        .youtube_setup_buttons
        .push((UiAction::OpenInvidiousInstances, guide_sections[4]));

    let storage = format!(
        "Will save to: {}\nAPI keys are plaintext; Unix permissions: directory 0700, file 0600.\nEnvironment variables override saved values.{}",
        setup.config_path,
        setup
            .validation_error
            .as_ref()
            .map_or_else(String::new, |error| format!("\nError: {error}"))
    );
    frame.render_widget(
        Paragraph::new(storage)
            .style(if setup.validation_error.is_some() {
                Style::default().fg(Color::Red)
            } else {
                theme.muted
            })
            .wrap(Wrap { trim: false }),
        sections[4],
    );

    let buttons = [
        ("[Enter] Save and retry", UiAction::SubmitYouTubeSetup),
        ("[Esc] Cancel", UiAction::DismissYouTubeSetup),
    ];
    let controls = buttons
        .iter()
        .map(|(label, _)| *label)
        .collect::<Vec<_>>()
        .join("   ");
    frame.render_widget(
        Paragraph::new(controls)
            .alignment(Alignment::Center)
            .style(theme.accent),
        sections[5],
    );
    let labels_width = buttons
        .iter()
        .map(|(label, _)| label.chars().count())
        .sum::<usize>()
        .saturating_add(3);
    let labels_width = u16::try_from(labels_width).unwrap_or(u16::MAX);
    let mut button_x = sections[5]
        .x
        .saturating_add(sections[5].width.saturating_sub(labels_width) / 2);
    for (label, action) in buttons {
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        hit_map.youtube_setup_buttons.push((
            action,
            Rect::new(button_x, sections[5].y, width, sections[5].height),
        ));
        button_x = button_x.saturating_add(width).saturating_add(3);
    }
}

fn render_preferences_popup(
    frame: &mut Frame<'_>,
    preferences: &PreferencesPopupView,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let area = centered_rect(76, 64, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(" Youta preferences ", theme), area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.is_empty() {
        return;
    }
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new("Subscriptions layout. Choose with d/s or ←/→, then press Enter to save.")
            .style(theme.base)
            .wrap(Wrap { trim: false }),
        sections[0],
    );

    let choices = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(sections[1]);
    let options = [
        (
            SubscriptionsLayout::DrillDown,
            "[d] Drill-down",
            "sources → videos + Details",
        ),
        (
            SubscriptionsLayout::Split,
            "[s] Split",
            "sources and videos together",
        ),
    ];
    for ((layout, label, description), choice_area) in options.into_iter().zip(choices.iter()) {
        let selected = layout == preferences.subscriptions_layout;
        frame.render_widget(
            Paragraph::new(format!("{label}\n{description}"))
                .style(if selected { theme.selected } else { theme.base })
                .alignment(Alignment::Center)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(if selected { theme.accent } else { theme.border }),
                ),
            *choice_area,
        );
        hit_map
            .preferences_buttons
            .push((UiAction::SetSubscriptionsLayout(layout), *choice_area));
    }

    let advertisement_label = format!(
        "[a] Skip sections named Реклама: {}",
        if preferences.skip_advertisement_chapters {
            "on"
        } else {
            "off"
        }
    );
    frame.render_widget(
        Paragraph::new(advertisement_label.clone())
            .style(if preferences.skip_advertisement_chapters {
                theme.selected
            } else {
                theme.base
            })
            .alignment(Alignment::Center),
        sections[2],
    );
    hit_map.preferences_buttons.push((
        UiAction::ToggleSkipAdvertisementChapters,
        Rect::new(
            centered_line_x(sections[2], terminal_text_width(&advertisement_label)),
            sections[2].y,
            terminal_text_width(&advertisement_label).min(sections[2].width),
            1,
        ),
    ));

    let youtube_prewarm_label = format!(
        "[y] Prepare selected YouTube audio: {}",
        if preferences.youtube_prewarm {
            "on"
        } else {
            "off"
        }
    );
    frame.render_widget(
        Paragraph::new(youtube_prewarm_label.clone())
            .style(if preferences.youtube_prewarm {
                theme.selected
            } else {
                theme.base
            })
            .alignment(Alignment::Center),
        sections[3],
    );
    hit_map.preferences_buttons.push((
        UiAction::ToggleYouTubePrewarm,
        Rect::new(
            centered_line_x(sections[3], terminal_text_width(&youtube_prewarm_label)),
            sections[3].y,
            terminal_text_width(&youtube_prewarm_label).min(sections[3].width),
            1,
        ),
    ));

    let folder_size_label = format!(
        "[f] Show Local folder sizes: {}",
        if preferences.show_local_folder_sizes {
            "on"
        } else {
            "off"
        }
    );
    frame.render_widget(
        Paragraph::new(folder_size_label.clone())
            .style(if preferences.show_local_folder_sizes {
                theme.selected
            } else {
                theme.base
            })
            .alignment(Alignment::Center),
        sections[4],
    );
    hit_map.preferences_buttons.push((
        UiAction::ToggleLocalFolderSizes,
        Rect::new(
            centered_line_x(sections[4], terminal_text_width(&folder_size_label)),
            sections[4].y,
            terminal_text_width(&folder_size_label).min(sections[4].width),
            1,
        ),
    ));

    let mut notes = format!(
        "Drill-down is the low-width default. Split is useful on wide terminals.\nYouTube preparation keeps one short-lived result in RAM; folder sizes are measured lazily.\nWill save UI and playback preferences in:\n{}",
        preferences.config_path
    );
    if let Some(variable) = preferences.environment_override.as_deref() {
        notes.push_str(&format!(
            "\n\nLocked by environment: {variable}\nChange or remove it before saving."
        ));
    }
    if let Some(error) = preferences.validation_error.as_deref() {
        notes.push_str(&format!("\n\nError: {error}"));
    }
    frame.render_widget(
        Paragraph::new(notes)
            .style(
                if preferences.validation_error.is_some()
                    || preferences.environment_override.is_some()
                {
                    Style::default().fg(Color::Red)
                } else {
                    theme.muted
                },
            )
            .wrap(Wrap { trim: false }),
        sections[5],
    );

    let buttons = [
        ("[Enter] Save", UiAction::SubmitPreferences),
        ("[Esc] Cancel", UiAction::DismissPreferences),
    ];
    let controls = buttons
        .iter()
        .map(|(label, _)| *label)
        .collect::<Vec<_>>()
        .join("   ");
    frame.render_widget(
        Paragraph::new(controls.as_str())
            .alignment(Alignment::Center)
            .style(theme.accent),
        sections[6],
    );
    let total_width = u16::try_from(Span::raw(&controls).width()).unwrap_or(u16::MAX);
    let mut x = centered_line_x(sections[6], total_width);
    for (label, action) in buttons {
        let width = terminal_text_width(label);
        hit_map
            .preferences_buttons
            .push((action, Rect::new(x, sections[6].y, width, 1)));
        x = x.saturating_add(width).saturating_add(3);
    }
}

fn render_local_file_popup(
    frame: &mut Frame<'_>,
    popup: &LocalFilePopupView,
    theme: &Theme,
    hit_map: &mut HitMap,
) {
    let (message, error, confirm_label, confirm_action) = match popup {
        LocalFilePopupView::Rename {
            value,
            cursor_byte,
            error,
        } => {
            let cursor_byte = rename_cursor_boundary(value, *cursor_byte);
            let (before, after) = value.split_at(cursor_byte);
            (
                format!("New basename:\n{before}▏{after}"),
                error.as_deref(),
                "[Enter] Rename",
                UiAction::SubmitLocalRename,
            )
        }
        LocalFilePopupView::Trash { name, path, error } => (
            format!("Move “{name}” to recoverable system Trash?\nFrom: {path}"),
            error.as_deref(),
            "[Enter] Move to Trash",
            UiAction::ConfirmLocalTrash,
        ),
    };
    let (title, area, message) = match popup {
        LocalFilePopupView::Rename { .. } => (
            " Local entry ",
            centered_rect(66, 28, frame.area()),
            message,
        ),
        LocalFilePopupView::Trash { .. } => {
            let width = frame.area().width.saturating_sub(4).clamp(1, 96);
            let message_width = width.saturating_sub(6).max(1);
            let wrapped_message = wrap_text_lines(&message, message_width);
            let message_height = u16::try_from(wrapped_message.len()).unwrap_or(u16::MAX);
            let height = message_height
                .saturating_add(6)
                .clamp(1, frame.area().height.saturating_sub(2).max(1));
            (
                " Move to trash? ",
                centered_sized_rect(width, height, frame.area()),
                wrapped_message.join("\n"),
            )
        }
    };
    frame.render_widget(Clear, area);
    frame.render_widget(panel_block(title, theme), area);
    let inner = area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.is_empty() {
        return;
    }
    let sections = Layout::vertical([
        Constraint::Min(2),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(message)
            .style(theme.base)
            .wrap(Wrap { trim: false }),
        sections[0],
    );
    if let Some(error) = error {
        frame.render_widget(
            Paragraph::new(error).style(Style::default().fg(Color::Red)),
            sections[1],
        );
    }
    let cancel_label = "[Esc] Cancel";
    let controls = format!("{confirm_label}   {cancel_label}");
    frame.render_widget(
        Paragraph::new(controls.as_str())
            .alignment(Alignment::Center)
            .style(theme.accent),
        sections[2],
    );
    let controls_width = terminal_text_width(&controls);
    let start = centered_line_x(sections[2], controls_width);
    hit_map.local_file_buttons.push((
        confirm_action,
        Rect::new(start, sections[2].y, terminal_text_width(confirm_label), 1),
    ));
    hit_map.local_file_buttons.push((
        UiAction::DismissLocalFilePopup,
        Rect::new(
            start
                .saturating_add(terminal_text_width(confirm_label))
                .saturating_add(3),
            sections[2].y,
            terminal_text_width(cancel_label),
            1,
        ),
    ));
}

/// Clamps a possibly stale rename cursor to the nearest preceding grapheme
/// boundary so rendering never splits UTF-8 or a visible character cluster.
fn rename_cursor_boundary(value: &str, requested: usize) -> usize {
    let requested = requested.min(value.len());
    if requested == value.len() {
        return requested;
    }
    value
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .take_while(|index| *index <= requested)
        .last()
        .unwrap_or_default()
}

fn masked_setup_value(value: &str, width: usize) -> String {
    let length = value.chars().count();
    if length <= width {
        return "*".repeat(length);
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    format!("{}…", "*".repeat(width - 1))
}

fn truncate_setup_value(value: &str, width: usize) -> String {
    let mut characters = value.chars();
    let prefix = characters.by_ref().take(width).collect::<String>();
    if characters.next().is_none() {
        return prefix;
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    format!("{}…", prefix.chars().take(width - 1).collect::<String>())
}

/// Compact action rendered immediately after an internally navigable video URL.
const DESCRIPTION_VIDEO_ACTION_SYMBOL: &str = "↪";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WrappedDescriptionToken {
    /// A contiguous UTF-8 source slice.
    Source { start_byte: usize, end_byte: usize },
    /// An injected action referring to [`DetailView::video_links`].
    VideoAction { link_index: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WrappedSourceLine {
    start_byte: usize,
    end_byte: usize,
    tokens: Vec<WrappedDescriptionToken>,
}

#[derive(Debug)]
struct WrappedSourceLineBuilder {
    fallback_byte: usize,
    width: usize,
    tokens: Vec<WrappedDescriptionToken>,
}

impl WrappedSourceLineBuilder {
    fn new(fallback_byte: usize) -> Self {
        Self {
            fallback_byte,
            width: 0,
            tokens: Vec::new(),
        }
    }

    fn push_source(&mut self, start_byte: usize, end_byte: usize, width: usize) {
        if let Some(WrappedDescriptionToken::Source {
            end_byte: previous_end,
            ..
        }) = self.tokens.last_mut()
            && *previous_end == start_byte
        {
            *previous_end = end_byte;
        } else {
            self.tokens.push(WrappedDescriptionToken::Source {
                start_byte,
                end_byte,
            });
        }
        self.width = self.width.saturating_add(width);
    }

    fn push_video_action(&mut self, link_index: usize, width: usize) {
        self.tokens
            .push(WrappedDescriptionToken::VideoAction { link_index });
        self.width = self.width.saturating_add(width);
    }

    fn finish(self) -> WrappedSourceLine {
        let source_range = self.tokens.iter().filter_map(|token| match token {
            WrappedDescriptionToken::Source {
                start_byte,
                end_byte,
            } => Some((*start_byte, *end_byte)),
            WrappedDescriptionToken::VideoAction { .. } => None,
        });
        let mut start_byte = None;
        let mut end_byte = None;
        for (start, end) in source_range {
            start_byte.get_or_insert(start);
            end_byte = Some(end);
        }
        WrappedSourceLine {
            start_byte: start_byte.unwrap_or(self.fallback_byte),
            end_byte: end_byte.unwrap_or(self.fallback_byte),
            tokens: self.tokens,
        }
    }
}

/// Wraps source text and injected URL actions without losing source byte ranges.
fn wrap_description_source(
    description: &str,
    width: usize,
    video_links: &[DetailVideoLinkView],
) -> Vec<WrappedSourceLine> {
    let width = width.max(1);
    let action_width = usize::from(terminal_text_width(DESCRIPTION_VIDEO_ACTION_SYMBOL));
    let mut action_indexes = video_links
        .iter()
        .enumerate()
        .filter(|(_, link)| {
            link.start_byte < link.end_byte
                && link.end_byte <= description.len()
                && description.is_char_boundary(link.start_byte)
                && description.is_char_boundary(link.end_byte)
        })
        .map(|(index, link)| (link.end_byte, index))
        .collect::<Vec<_>>();
    action_indexes.sort_unstable();

    let mut wrapped = Vec::new();
    let mut source_line_start = 0_usize;
    let mut next_action = 0_usize;
    for raw_line in description.split('\n') {
        let visible_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let source_line_end = source_line_start.saturating_add(visible_line.len());
        while next_action < action_indexes.len()
            && action_indexes[next_action].0 < source_line_start
        {
            next_action = next_action.saturating_add(1);
        }

        let mut builder = WrappedSourceLineBuilder::new(source_line_start);
        for (byte_offset, character) in visible_line.char_indices() {
            let absolute_byte = source_line_start.saturating_add(byte_offset);
            while next_action < action_indexes.len()
                && action_indexes[next_action].0 == absolute_byte
            {
                let (_, link_index) = action_indexes[next_action];
                if builder.width > 0 && builder.width.saturating_add(action_width) > width {
                    wrapped.push(builder.finish());
                    builder = WrappedSourceLineBuilder::new(absolute_byte);
                }
                builder.push_video_action(link_index, action_width);
                next_action = next_action.saturating_add(1);
            }

            let character_end = absolute_byte.saturating_add(character.len_utf8());
            let character_width = Span::raw(character.to_string()).width();
            if builder.width > 0 && builder.width.saturating_add(character_width) > width {
                wrapped.push(builder.finish());
                builder = WrappedSourceLineBuilder::new(absolute_byte);
            }
            builder.push_source(absolute_byte, character_end, character_width);
        }
        while next_action < action_indexes.len() && action_indexes[next_action].0 == source_line_end
        {
            let (_, link_index) = action_indexes[next_action];
            if builder.width > 0 && builder.width.saturating_add(action_width) > width {
                wrapped.push(builder.finish());
                builder = WrappedSourceLineBuilder::new(source_line_end);
            }
            builder.push_video_action(link_index, action_width);
            next_action = next_action.saturating_add(1);
        }
        wrapped.push(builder.finish());
        source_line_start = source_line_start
            .saturating_add(raw_line.len())
            .saturating_add(1);
    }
    if wrapped.is_empty() {
        wrapped.push(WrappedSourceLine {
            start_byte: 0,
            end_byte: 0,
            tokens: Vec::new(),
        });
    }
    wrapped
}

fn terminal_text_width(value: &str) -> u16 {
    u16::try_from(Span::raw(value).width()).unwrap_or(u16::MAX)
}

fn wrap_diagnostic_report(report: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut wrapped = Vec::new();
    for raw_line in report.split('\n') {
        let raw_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if raw_line.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let characters = raw_line.chars().collect::<Vec<_>>();
        for chunk in characters.chunks(width) {
            wrapped.push(chunk.iter().collect());
        }
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

fn key_action(key: KeyEvent, view: &ViewModel) -> Option<UiAction> {
    if view.error_popup.is_some() {
        return match key.code {
            KeyCode::Esc => Some(UiAction::DismissErrorPopup),
            KeyCode::Char('c' | 'C') => Some(UiAction::CopyErrorReport),
            KeyCode::Char('g' | 'G')
                if view
                    .error_popup
                    .as_ref()
                    .is_some_and(|error| error.gh_available) =>
            {
                Some(UiAction::FillGitHubIssue)
            }
            KeyCode::Char('i' | 'I') => Some(UiAction::CopyAndOpenGitHubIssue),
            KeyCode::Up | KeyCode::Left => {
                Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(-1)))
            }
            KeyCode::Down | KeyCode::Right => {
                Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(1)))
            }
            KeyCode::PageUp => Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Pages(-1))),
            KeyCode::PageDown => Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Pages(1))),
            KeyCode::Home => Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Home)),
            KeyCode::End => Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::End)),
            _ => None,
        };
    }
    if let Some(popup) = view.local_file_popup.as_ref() {
        return match (popup, key.code) {
            (_, KeyCode::Esc) => Some(UiAction::DismissLocalFilePopup),
            (LocalFilePopupView::Rename { .. }, KeyCode::Enter) => {
                Some(UiAction::SubmitLocalRename)
            }
            (LocalFilePopupView::Rename { .. }, KeyCode::Backspace) => {
                Some(UiAction::DeleteLocalRenameCharacter)
            }
            (LocalFilePopupView::Rename { .. }, KeyCode::Left) => {
                Some(UiAction::MoveLocalRenameCursor(-1))
            }
            (LocalFilePopupView::Rename { .. }, KeyCode::Right) => {
                Some(UiAction::MoveLocalRenameCursor(1))
            }
            (LocalFilePopupView::Rename { .. }, KeyCode::Char(character))
                if !character.is_control()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                Some(UiAction::AppendLocalRenameCharacter(character))
            }
            (LocalFilePopupView::Trash { .. }, KeyCode::Enter) => Some(UiAction::ConfirmLocalTrash),
            _ => None,
        };
    }
    if let Some(preferences) = view.preferences_popup.as_ref() {
        let alternative = preferences.subscriptions_layout.toggled();
        return match key.code {
            KeyCode::Esc | KeyCode::Char('p') => Some(UiAction::DismissPreferences),
            KeyCode::Enter => Some(UiAction::SubmitPreferences),
            KeyCode::Char('a') => Some(UiAction::ToggleSkipAdvertisementChapters),
            KeyCode::Char('y') => Some(UiAction::ToggleYouTubePrewarm),
            KeyCode::Char('f') => Some(UiAction::ToggleLocalFolderSizes),
            KeyCode::Char('d') => Some(UiAction::SetSubscriptionsLayout(
                SubscriptionsLayout::DrillDown,
            )),
            KeyCode::Char('s') => {
                Some(UiAction::SetSubscriptionsLayout(SubscriptionsLayout::Split))
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down | KeyCode::Char(' ') => {
                Some(UiAction::SetSubscriptionsLayout(alternative))
            }
            _ => None,
        };
    }
    if view.text_selection_mode {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if control && matches!(key.code, KeyCode::Char('c' | 'C')) {
            // Terminals normally consume Ctrl+Shift+C as their Copy command.
            // If one forwards it, do not reinterpret that copy chord as Quit.
            return (!shift).then_some(UiAction::Quit);
        }
        return match key.code {
            KeyCode::Esc | KeyCode::Char('t') => Some(UiAction::ToggleTextSelectionMode),
            KeyCode::Char('T') => Some(UiAction::ToggleChapterTimestamps),
            _ => None,
        };
    }
    if let Some(setup) = view.youtube_setup_popup.as_ref() {
        let other_field = match setup.selected_field {
            YouTubeSetupField::ApiKey => YouTubeSetupField::InvidiousUrl,
            YouTubeSetupField::InvidiousUrl => YouTubeSetupField::ApiKey,
        };
        return match key.code {
            KeyCode::Esc => Some(UiAction::DismissYouTubeSetup),
            KeyCode::Enter => Some(UiAction::SubmitYouTubeSetup),
            KeyCode::F(1) => Some(UiAction::OpenYouTubeApiKeyGuide),
            KeyCode::F(2) => Some(UiAction::OpenGoogleCloudCredentials),
            KeyCode::F(3) => Some(UiAction::OpenInvidiousInstances),
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                Some(UiAction::SelectYouTubeSetupField(other_field))
            }
            KeyCode::Backspace => Some(UiAction::DeleteYouTubeSetupCharacter),
            KeyCode::Char(character)
                if !character.is_control()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                Some(UiAction::AppendYouTubeSetupCharacter(character))
            }
            _ => None,
        };
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(UiAction::Quit);
    }
    if view.help_open {
        return match key.code {
            KeyCode::Char('?') | KeyCode::Esc => Some(UiAction::ToggleHelp),
            KeyCode::Char('q') => Some(UiAction::Quit),
            _ => None,
        };
    }
    if view.search_editing {
        return match key.code {
            KeyCode::Esc => Some(UiAction::CancelSearch),
            KeyCode::Enter => Some(UiAction::SubmitSearch),
            KeyCode::Backspace => Some(UiAction::DeleteSearchCharacter),
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                Some(UiAction::AppendSearch(character))
            }
            _ => None,
        };
    }

    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let detail_link_count = view
        .details
        .as_ref()
        .map_or(0, |details| details.links.len());
    match key.code {
        KeyCode::Char('q') => Some(UiAction::Quit),
        KeyCode::Char('?') => Some(UiAction::ToggleHelp),
        KeyCode::Char('/') => Some(UiAction::BeginSearch),
        KeyCode::Char('p') | KeyCode::F(7) => Some(UiAction::OpenPreferences),
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
            Some(UiAction::ShowScreen(view.screen.previous()))
        }
        KeyCode::Tab => Some(UiAction::ShowScreen(view.screen.next())),
        KeyCode::BackTab => Some(UiAction::ShowScreen(view.screen.previous())),
        KeyCode::Char('S') => Some(UiAction::ShowScreen(Screen::Subscriptions)),
        KeyCode::F(2) => Some(UiAction::ShowScreen(Screen::Downloaded)),
        KeyCode::F(3) => Some(UiAction::ShowScreen(Screen::History)),
        KeyCode::F(4) => Some(UiAction::ShowScreen(Screen::Playlists)),
        KeyCode::F(5) => Some(UiAction::ShowScreen(Screen::Statistics)),
        KeyCode::Char('M') | KeyCode::F(6) => Some(UiAction::ShowScreen(Screen::TrackerMusic)),
        KeyCode::Char('v') => Some(UiAction::ToggleSearchKind),
        KeyCode::Char('N') => Some(UiAction::ToggleYouTubeSearchSort),
        KeyCode::Char('C') => Some(UiAction::ToggleYouTubeCreativeCommons),
        KeyCode::Char('A') => Some(UiAction::ToggleAutoplay),
        KeyCode::Char('Z') if view.screen == Screen::Local && view.local_folder_sizes_enabled => {
            Some(UiAction::ToggleLocalSizeSort)
        }
        KeyCode::Char('T') => Some(UiAction::ToggleChapterTimestamps),
        KeyCode::Char('r') if view.screen == Screen::Local => Some(UiAction::BeginLocalRename),
        KeyCode::Delete if view.screen == Screen::Local => Some(UiAction::RequestLocalTrash),
        KeyCode::Char('i')
            if view.screen == Screen::Subscriptions && !view.subscriptions.items.is_empty() =>
        {
            Some(UiAction::ToggleSubscriptionDescription)
        }
        KeyCode::Char('W')
            if view
                .selected_detail_link
                .and_then(|index| {
                    view.details
                        .as_ref()
                        .and_then(|details| details.links.get(index))
                })
                .is_some_and(|link| link.wikidata_item_id.is_some()) =>
        {
            Some(UiAction::ToggleWikidataStatements(
                view.selected_detail_link.unwrap_or_default(),
            ))
        }
        KeyCode::Char('R')
            if view.screen == Screen::Subscriptions
                && (view.subscriptions.route == SubscriptionRoute::Items
                    || (view.subscriptions.layout == SubscriptionsLayout::Split
                        && view.subscriptions.focus == SubscriptionPane::Items)) =>
        {
            Some(UiAction::RefreshSubscriptionVideos)
        }
        KeyCode::Char('t')
            if view.details.is_some() && view.right_panel_mode == RightPanelMode::Details =>
        {
            Some(UiAction::ToggleTextSelectionMode)
        }
        KeyCode::Char('j') if alt && detail_link_count > 0 => Some(UiAction::MoveDetailLink(1)),
        KeyCode::Char('k') if alt && detail_link_count > 0 => Some(UiAction::MoveDetailLink(-1)),
        KeyCode::Home if alt && detail_link_count > 0 => Some(UiAction::SelectDetailLink(0)),
        KeyCode::End if alt && detail_link_count > 0 => {
            Some(UiAction::SelectDetailLink(detail_link_count - 1))
        }
        KeyCode::Esc
            if view.screen == Screen::Subscriptions
                && (view.subscriptions.description_expanded
                    || view.subscriptions.focus == SubscriptionPane::Items) =>
        {
            Some(UiAction::GoBack)
        }
        KeyCode::Esc if view.details_focused => Some(UiAction::SetDetailsFocus(false)),
        KeyCode::PageUp if view.details_focused => {
            Some(UiAction::ScrollDetails(DetailsScroll::Pages(-1)))
        }
        KeyCode::PageDown if view.details_focused => {
            Some(UiAction::ScrollDetails(DetailsScroll::Pages(1)))
        }
        KeyCode::Home if view.details_focused => Some(UiAction::ScrollDetails(DetailsScroll::Home)),
        KeyCode::End if view.details_focused => Some(UiAction::ScrollDetails(DetailsScroll::End)),
        KeyCode::Char('j') => Some(UiAction::MoveSelection(1)),
        KeyCode::Char('k') => Some(UiAction::MoveSelection(-1)),
        KeyCode::Enter if alt && detail_link_count > 0 => {
            let selected = view
                .selected_detail_link
                .unwrap_or_default()
                .min(detail_link_count - 1);
            Some(UiAction::ActivateDetailLink(selected))
        }
        KeyCode::Enter => Some(UiAction::ActivateSelection),
        KeyCode::Char(' ') => Some(UiAction::TogglePause),
        KeyCode::Left if alt => Some(UiAction::GoBack),
        KeyCode::Right if alt => Some(UiAction::GoForward),
        KeyCode::Left => Some(UiAction::SeekRelative(-5)),
        KeyCode::Right => Some(UiAction::SeekRelative(5)),
        KeyCode::Up => Some(UiAction::ChangeVolume(5)),
        KeyCode::Down => Some(UiAction::ChangeVolume(-5)),
        KeyCode::Char('<') | KeyCode::Char(',') => Some(UiAction::ChangeSpeed(-0.1)),
        KeyCode::Char('>') | KeyCode::Char('.') => Some(UiAction::ChangeSpeed(0.1)),
        KeyCode::Char('[') => Some(UiAction::ChangeChapter(-1)),
        KeyCode::Char(']') => Some(UiAction::ChangeChapter(1)),
        KeyCode::Char('r') => Some(UiAction::ToggleRepeat),
        KeyCode::Char('w') => Some(UiAction::ToggleWaveform),
        KeyCode::Char('c') => Some(UiAction::ShowChannel),
        KeyCode::Char('s') => Some(UiAction::ToggleSubscription),
        KeyCode::Backspace => Some(UiAction::GoBack),
        KeyCode::Char('n') => Some(UiAction::PlayNext),
        KeyCode::Char('a') => Some(UiAction::AddToQueue),
        KeyCode::Char('d') => Some(UiAction::Download),
        KeyCode::Char('o') => Some(UiAction::OpenInBrowser),
        KeyCode::Char('O') => Some(UiAction::OpenChannelInBrowser),
        KeyCode::Char('y') => Some(UiAction::CopyLink),
        KeyCode::Char('m') => Some(UiAction::EditPrivateNote),
        KeyCode::Char('e') => Some(UiAction::OpenEqualizer),
        KeyCode::Char(digit @ '0'..='9') => {
            let percentage = f64::from(digit.to_digit(10).unwrap_or_default()) * 10.0;
            Some(UiAction::SeekPercent(percentage))
        }
        _ => None,
    }
}

fn mouse_action(mouse: MouseEvent, hit_map: &HitMap, view: &ViewModel) -> Option<UiAction> {
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        // Terminals conventionally reserve Shift-drag for native text
        // selection even while mouse reporting is enabled. Never turn the
        // corresponding events into Youta actions if a terminal forwards them.
        return None;
    }
    if view.error_popup.is_some() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => hit_map
                .error_buttons
                .iter()
                .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                .map(|(action, _)| action.clone()),
            MouseEventKind::ScrollDown => {
                Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(3)))
            }
            MouseEventKind::ScrollUp => {
                Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(-3)))
            }
            _ => None,
        };
    }
    if view.local_file_popup.is_some() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => hit_map
                .local_file_buttons
                .iter()
                .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                .map(|(action, _)| action.clone()),
            _ => None,
        };
    }
    if view.youtube_setup_popup.is_some() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((field, _)) = hit_map
                    .youtube_setup_fields
                    .iter()
                    .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                {
                    Some(UiAction::SelectYouTubeSetupField(*field))
                } else {
                    hit_map
                        .youtube_setup_buttons
                        .iter()
                        .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                        .map(|(action, _)| action.clone())
                }
            }
            _ => None,
        };
    }
    if view.preferences_popup.is_some() {
        return match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => hit_map
                .preferences_buttons
                .iter()
                .find(|(_, area)| contains(*area, mouse.column, mouse.row))
                .map(|(action, _)| action.clone()),
            _ => None,
        };
    }
    if view.text_selection_mode {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(position) =
                    hit_map.details_text_position(mouse.column, mouse.row, false)
                {
                    return Some(UiAction::BeginDetailsTextSelection(position));
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if view
                    .details_text_selection
                    .is_some_and(|selection| selection.dragging)
                {
                    return hit_map
                        .details_text_position(mouse.column, mouse.row, true)
                        .map(UiAction::UpdateDetailsTextSelection);
                }
                return None;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let selection = view
                    .details_text_selection
                    .filter(|selection| selection.dragging)?;
                let focus = hit_map.details_text_position(mouse.column, mouse.row, true)?;
                let finished = DetailsTextSelection {
                    focus,
                    dragging: false,
                    ..selection
                };
                return Some(UiAction::FinishDetailsTextSelection {
                    focus,
                    text: hit_map.selected_details_text(finished),
                });
            }
            _ => {}
        }
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            for (screen, area) in &hit_map.tabs {
                if contains(*area, mouse.column, mouse.row) {
                    return Some(UiAction::ShowScreen(*screen));
                }
            }
            for (action, area) in &hit_map.buttons {
                if contains(*area, mouse.column, mouse.row) {
                    return Some(action.clone());
                }
            }
            if hit_map
                .now_playing
                .is_some_and(|area| contains(area, mouse.column, mouse.row))
            {
                return Some(UiAction::ShowNowPlaying);
            }
            for (action, area) in &hit_map.description_video_actions {
                if contains(*area, mouse.column, mouse.row) {
                    return Some(action.clone());
                }
            }
            for (action, area) in &hit_map.detail_buttons {
                if contains(*area, mouse.column, mouse.row) {
                    return Some(action.clone());
                }
            }
            for (index, area) in &hit_map.detail_links {
                if contains(*area, mouse.column, mouse.row) {
                    return Some(UiAction::ActivateDetailLink(*index));
                }
            }
            if contains(hit_map.details_panel, mouse.column, mouse.row) {
                return Some(UiAction::SetDetailsFocus(true));
            }
            for (action, area) in &hit_map.seek_markers {
                if contains(*area, mouse.column, mouse.row) {
                    return Some(action.clone());
                }
            }
            if contains(hit_map.seek_bar, mouse.column, mouse.row) && hit_map.seek_bar.width > 1 {
                let offset = mouse.column.saturating_sub(hit_map.seek_bar.x);
                let percent =
                    f64::from(offset) / f64::from(hit_map.seek_bar.width.saturating_sub(1)) * 100.0;
                return Some(UiAction::SeekPercent(percent.clamp(0.0, 100.0)));
            }
            if contains(hit_map.rows, mouse.column, mouse.row) {
                let relative_row = mouse.row.saturating_sub(hit_map.rows.y);
                let row_height = if hit_map.rows_row_height == 0 {
                    2
                } else {
                    hit_map.rows_row_height
                };
                let index = hit_map
                    .rows_first_index
                    .saturating_add(usize::from(relative_row / row_height));
                if index < view.rows.len() {
                    return Some(UiAction::SelectRow(index));
                }
            }
            if contains(hit_map.subscription_source_rows, mouse.column, mouse.row) {
                let relative_row = mouse.row.saturating_sub(hit_map.subscription_source_rows.y);
                let index = hit_map
                    .subscription_source_first_index
                    .saturating_add(usize::from(relative_row / 2));
                if index < view.subscriptions.sources.len() {
                    return Some(UiAction::SelectSubscriptionSource(index));
                }
            }
            if contains(hit_map.subscription_item_rows, mouse.column, mouse.row) {
                let relative_row = mouse.row.saturating_sub(hit_map.subscription_item_rows.y);
                let index = hit_map
                    .subscription_item_first_index
                    .saturating_add(usize::from(relative_row / 2));
                if index < view.subscriptions.items.len() {
                    return Some(UiAction::SelectSubscriptionItem(index));
                }
            }
            None
        }
        MouseEventKind::ScrollDown => {
            if contains(hit_map.details_panel, mouse.column, mouse.row) {
                Some(UiAction::SetDetailsScroll(
                    hit_map
                        .details_scroll_offset
                        .saturating_add(3)
                        .min(hit_map.details_scroll_maximum),
                ))
            } else if hit_map
                .detail_links
                .iter()
                .any(|(_, area)| contains(*area, mouse.column, mouse.row))
            {
                Some(UiAction::MoveDetailLink(1))
            } else {
                Some(UiAction::MoveSelection(1))
            }
        }
        MouseEventKind::ScrollUp => {
            if contains(hit_map.details_panel, mouse.column, mouse.row) {
                Some(UiAction::SetDetailsScroll(
                    hit_map.details_scroll_offset.saturating_sub(3),
                ))
            } else if hit_map
                .detail_links
                .iter()
                .any(|(_, area)| contains(*area, mouse.column, mouse.row))
            {
                Some(UiAction::MoveDetailLink(-1))
            } else {
                Some(UiAction::MoveSelection(-1))
            }
        }
        _ => None,
    }
}

fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x && x < area.right() && y >= area.y && y < area.bottom()
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// Centers a popup with explicit dimensions, clamped to the terminal area.
fn centered_sized_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    )
}

/// Wraps text at an exact terminal-cell width, including unbroken paths.
fn wrap_text_lines(value: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut wrapped = Vec::new();
    for source_line in value.split('\n') {
        let mut line = String::new();
        let mut line_width = 0_usize;
        for character in source_line.chars() {
            let character_width = Span::raw(character.to_string()).width();
            if !line.is_empty() && line_width.saturating_add(character_width) > width {
                wrapped.push(std::mem::take(&mut line));
                line_width = 0;
            }
            line.push(character);
            line_width = line_width.saturating_add(character_width);
        }
        wrapped.push(line);
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

/// Renders one compact, borderless main-pane heading and returns its content area.
///
/// Popup panels intentionally continue to use [`panel_block`] so diagnostics and
/// configuration dialogs remain visually distinct from the primary workspace.
fn render_main_panel_heading(frame: &mut Frame<'_>, area: Rect, title: &str, style: Style) -> Rect {
    let heading_height = if area.height > 0 { 1 } else { 0 };
    if area.width > 0 && heading_height > 0 {
        frame.render_widget(
            Paragraph::new(title).style(style),
            Rect::new(area.x, area.y, area.width, heading_height),
        );
    }
    Rect::new(
        area.x,
        area.y.saturating_add(heading_height),
        area.width,
        area.height.saturating_sub(heading_height),
    )
}

fn panel_block<'a>(title: &'a str, theme: &Theme) -> Block<'a> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border)
        .title(title)
}

fn button(key: &str, label: &str, show_hotkey: bool) -> String {
    if show_hotkey {
        format!("[{key}] {label}")
    } else {
        label.to_owned()
    }
}

fn source_style(source: &str, theme: &Theme) -> Style {
    let lower = source.to_ascii_lowercase();
    if lower.contains("youtube") || lower.contains("invidious") {
        Style::default().fg(Color::Red)
    } else if lower.contains("podcast") || lower.contains("rss") {
        Style::default().fg(Color::Green)
    } else if lower.contains("local") {
        Style::default().fg(Color::Cyan)
    } else if lower.contains("peertube") {
        Style::default().fg(Color::Magenta)
    } else if lower.contains("radio") {
        Style::default().fg(Color::Yellow)
    } else if lower.contains("mod")
        || lower.contains("tracker")
        || lower.contains("scene.org")
        || lower.contains("aminet")
        || lower.contains("demozoo")
    {
        Style::default().fg(Color::LightBlue)
    } else {
        theme.accent
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn download_ratio(downloaded_bytes: u64, total_bytes: u64) -> f64 {
    const RATIO_SCALE: u32 = 10_000;

    if total_bytes == 0 {
        return 0.0;
    }
    let scaled = u128::from(downloaded_bytes.min(total_bytes))
        .saturating_mul(u128::from(RATIO_SCALE))
        / u128::from(total_bytes);
    f64::from(u32::try_from(scaled).unwrap_or(RATIO_SCALE)) / f64::from(RATIO_SCALE)
}

fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format_binary_unit(bytes, GIB, "GiB")
    } else if bytes >= MIB {
        format_binary_unit(bytes, MIB, "MiB")
    } else if bytes >= KIB {
        format_binary_unit(bytes, KIB, "KiB")
    } else {
        format!("{bytes} B")
    }
}

fn format_binary_unit(bytes: u64, unit: u64, suffix: &str) -> String {
    let tenths = (u128::from(bytes) * 10 + u128::from(unit / 2)) / u128::from(unit);
    format!("{}.{} {suffix}", tenths / 10, tenths % 10)
}

fn trim_speed(speed: f64) -> String {
    let mut result = format!("{speed:.1}");
    if result.ends_with(".0") {
        result.truncate(result.len() - 2);
    }
    result
}

struct Theme {
    base: Style,
    border: Style,
    heading: Style,
    selected: Style,
    accent: Style,
    /// Restrained terminal-palette pink used by the playing description chapter.
    active_chapter: Style,
    vertical_video: Style,
    muted: Style,
    cached: Style,
    progress: Style,
}

impl Theme {
    fn new(funny_mode: bool) -> Self {
        if funny_mode {
            Self {
                base: Style::default().fg(Color::LightGreen),
                border: Style::default().fg(Color::Yellow),
                heading: Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
                selected: Style::default()
                    .fg(Color::Black)
                    .bg(Color::LightGreen)
                    .add_modifier(Modifier::BOLD),
                accent: Style::default().fg(Color::LightMagenta),
                active_chapter: Style::default().fg(Color::LightMagenta),
                vertical_video: Style::default().fg(Color::LightCyan),
                muted: Style::default().fg(Color::DarkGray),
                cached: Style::default().fg(Color::DarkGray).bg(Color::Reset),
                progress: Style::default().fg(Color::LightMagenta).bg(Color::Black),
            }
        } else {
            Self {
                base: Style::default(),
                border: Style::default().fg(Color::DarkGray),
                heading: Style::default().add_modifier(Modifier::BOLD),
                selected: Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                accent: Style::default().fg(Color::Cyan),
                // ANSI magenta follows the terminal's configured palette and
                // remains available without true-color support.
                active_chapter: Style::default().fg(Color::Magenta),
                vertical_video: Style::default().fg(Color::Rgb(255, 105, 180)),
                muted: Style::default().fg(Color::DarkGray),
                cached: Style::default().fg(Color::DarkGray).bg(Color::Reset),
                progress: Style::default().fg(Color::Cyan),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::domain::SourceKind;

    use super::*;

    #[derive(Default)]
    struct MockThumbnailRenderer {
        enabled: bool,
        synchronized: Vec<(Option<url::Url>, Rect)>,
        prefetch_batches: Vec<Vec<url::Url>>,
        clear_count: usize,
        pending: bool,
        immediate_redraw: bool,
        poll_results: VecDeque<bool>,
        poll_count: usize,
    }

    impl ThumbnailRenderer for MockThumbnailRenderer {
        fn poll(&mut self) -> bool {
            self.poll_count = self.poll_count.saturating_add(1);
            let changed = self.poll_results.pop_front().unwrap_or(false);
            if changed {
                self.pending = false;
            }
            changed
        }

        fn is_enabled(&self) -> bool {
            self.enabled
        }

        fn is_pending(&self) -> bool {
            self.pending
        }

        fn needs_immediate_redraw(&self) -> bool {
            self.immediate_redraw
        }

        fn synchronize(&mut self, source: Option<&url::Url>, area: Rect) -> bool {
            self.synchronized.push((source.cloned(), area));
            true
        }

        fn synchronize_prefetch(&mut self, rows: &[RowView]) -> bool {
            self.prefetch_batches.push(
                rows.iter()
                    .filter_map(|row| row.thumbnail_url.clone())
                    .collect(),
            );
            true
        }

        fn clear(&mut self) -> bool {
            self.clear_count = self.clear_count.saturating_add(1);
            true
        }

        fn render(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
            frame.render_widget(
                Paragraph::new("THUMBNAIL IMAGE")
                    .style(theme.accent)
                    .alignment(Alignment::Center),
                area,
            );
        }
    }

    #[test]
    fn f8_virtual_cursor_moves_clamps_and_clicks_existing_hitboxes() {
        let mut cursor = VirtualCursor {
            bounds: Rect::new(2, 1, 5, 3),
            ..VirtualCursor::default()
        };
        assert_eq!(
            cursor.handle_key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
            VirtualCursorKey::Consumed
        );
        assert_eq!((cursor.column, cursor.row), (4, 2));
        assert_eq!(
            cursor.handle_key(KeyEvent::new_with_kind(
                KeyCode::F(8),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
            VirtualCursorKey::Consumed
        );
        assert!(cursor.active, "an F8 key release must not toggle twice");

        for _ in 0..10 {
            assert_eq!(
                cursor.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
                VirtualCursorKey::Consumed
            );
            assert_eq!(
                cursor.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
                VirtualCursorKey::Consumed
            );
        }
        assert_eq!((cursor.column, cursor.row), (2, 1));

        let click = match cursor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)) {
            VirtualCursorKey::Click(mouse) => mouse,
            outcome => panic!("expected a virtual click, got {outcome:?}"),
        };
        let hit_map = HitMap {
            buttons: vec![(UiAction::ToggleHelp, Rect::new(2, 1, 1, 1))],
            ..HitMap::default()
        };
        assert_eq!(
            mouse_action(click, &hit_map, &ViewModel::default()),
            Some(UiAction::ToggleHelp)
        );

        assert_eq!(
            cursor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            VirtualCursorKey::Consumed
        );
        assert!(!cursor.active);
        assert_eq!(
            cursor.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)),
            VirtualCursorKey::PassThrough
        );
    }

    #[test]
    fn virtual_cursor_renders_a_reversed_square_over_an_empty_cell() {
        let backend = TestBackend::new(7, 3);
        let mut terminal = Terminal::new(backend).expect("create virtual-cursor terminal");
        let mut cursor = VirtualCursor {
            active: true,
            column: 3,
            row: 1,
            bounds: Rect::new(0, 0, 7, 3),
        };

        terminal
            .draw(|frame| cursor.render(frame))
            .expect("render virtual cursor");

        let cell = &terminal.backend().buffer()[(3, 1)];
        assert_eq!(cell.symbol(), "■");
        assert!(cell.modifier.contains(Modifier::BOLD));
        assert!(cell.modifier.contains(Modifier::REVERSED));
    }

    /// Collects the test backend's current cells for whole-frame assertions.
    fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn terminal_startup_sets_youta_title_before_entering_the_ui() {
        let mut output = Vec::new();

        write_terminal_startup(&mut output).expect("write terminal startup sequence");

        assert!(
            output.starts_with(b"\x1b]0;Youta\x07"),
            "terminal title must be the first startup command: {output:?}"
        );
        assert!(
            output
                .windows(b"\x1b[?1049h".len())
                .any(|window| window == b"\x1b[?1049h"),
            "startup must still enter the alternate screen: {output:?}"
        );
        assert!(
            output
                .windows(b"\x1b[?1006l".len())
                .all(|window| window != b"\x1b[?1006l"),
            "Details selection must keep SGR mouse reporting enabled: {output:?}"
        );
    }

    #[test]
    fn active_search_uses_the_existing_playing_redraw_cadence() {
        let settings = UiSettings {
            idle_tick: Duration::from_secs(2),
            playing_tick: Duration::from_millis(250),
            ..UiSettings::default()
        };
        let mut view = ViewModel::default();

        assert_eq!(event_wait(&view, &settings), Duration::from_secs(2));
        view.search_activity = Some(SearchActivity::YouTube);
        assert_eq!(event_wait(&view, &settings), Duration::from_millis(250));
        view.search_activity = None;
        view.playback_starting = true;
        assert_eq!(event_wait(&view, &settings), Duration::from_millis(250));
        view.playback_starting = false;
        view.playback.paused = false;
        assert_eq!(event_wait(&view, &settings), Duration::from_millis(250));

        let zero_settings = UiSettings {
            idle_tick: Duration::ZERO,
            playing_tick: Duration::ZERO,
            ..UiSettings::default()
        };
        assert_eq!(
            event_wait(&ViewModel::default(), &zero_settings),
            Duration::from_millis(1)
        );
    }

    #[test]
    fn pending_local_folder_open_uses_the_interactive_response_budget() {
        let settings = UiSettings {
            idle_tick: Duration::from_secs(2),
            playing_tick: Duration::from_millis(250),
            ..UiSettings::default()
        };
        let mut view = ViewModel {
            local_browse_pending: true,
            ..ViewModel::default()
        };

        assert_eq!(
            event_wait(&view, &settings),
            LOCAL_BROWSE_RESPONSE_POLL_INTERVAL
        );

        view.local_browse_pending = false;
        assert_eq!(event_wait(&view, &settings), Duration::from_secs(2));
    }

    #[test]
    fn pending_local_artwork_uses_the_interactive_response_budget() {
        let settings = UiSettings {
            idle_tick: Duration::from_secs(2),
            playing_tick: Duration::from_millis(250),
            ..UiSettings::default()
        };
        let view = ViewModel {
            local_artwork_pending: true,
            ..ViewModel::default()
        };

        assert_eq!(
            event_wait(&view, &settings),
            LOCAL_BROWSE_RESPONSE_POLL_INTERVAL
        );
    }

    #[test]
    fn thumbnail_wait_probes_cache_hits_early_and_redraws_on_completion() {
        let mut thumbnails = MockThumbnailRenderer {
            pending: true,
            poll_results: VecDeque::from([false, true]),
            ..MockThumbnailRenderer::default()
        };
        let mut waits = Vec::new();

        let outcome =
            wait_for_event_or_thumbnail(Duration::from_secs(2), Some(&mut thumbnails), |wait| {
                waits.push(wait);
                Ok(false)
            })
            .expect("wait for cached thumbnail");

        assert_eq!(outcome, WaitOutcome::ThumbnailRedraw);
        assert_eq!(waits, [Duration::from_millis(25)]);
        assert_eq!(thumbnails.poll_count, 2);
    }

    #[test]
    fn thumbnail_wait_keeps_idle_and_long_network_work_low_frequency() {
        let mut idle = MockThumbnailRenderer::default();
        let mut idle_waits = Vec::new();
        assert_eq!(
            wait_for_event_or_thumbnail(Duration::from_secs(2), Some(&mut idle), |wait| {
                idle_waits.push(wait);
                Ok(false)
            })
            .expect("idle terminal wait"),
            WaitOutcome::Timeout
        );
        assert_eq!(idle_waits, [Duration::from_secs(2)]);
        assert_eq!(idle.poll_count, 0);

        let mut loading = MockThumbnailRenderer {
            pending: true,
            ..MockThumbnailRenderer::default()
        };
        let mut loading_waits = Vec::new();
        assert_eq!(
            wait_for_event_or_thumbnail(Duration::from_secs(1), Some(&mut loading), |wait| {
                loading_waits.push(wait);
                Ok(false)
            },)
            .expect("network thumbnail wait"),
            WaitOutcome::Timeout
        );
        assert_eq!(
            loading_waits,
            [
                Duration::from_millis(25),
                Duration::from_millis(25),
                Duration::from_millis(50),
                Duration::from_millis(100),
                Duration::from_millis(250),
                Duration::from_millis(250),
                Duration::from_millis(250),
                Duration::from_millis(50),
            ]
        );
    }

    #[test]
    fn thumbnail_wait_prioritizes_terminal_input_and_clear_followup_frames() {
        let mut loading = MockThumbnailRenderer {
            pending: true,
            ..MockThumbnailRenderer::default()
        };
        let mut event_waits = Vec::new();
        assert_eq!(
            wait_for_event_or_thumbnail(Duration::from_secs(1), Some(&mut loading), |wait| {
                event_waits.push(wait);
                Ok(true)
            },)
            .expect("terminal event wait"),
            WaitOutcome::TerminalEvent
        );
        assert_eq!(event_waits, [Duration::from_millis(25)]);

        let mut followup = MockThumbnailRenderer {
            immediate_redraw: true,
            ..MockThumbnailRenderer::default()
        };
        assert_eq!(
            wait_for_event_or_thumbnail(Duration::from_secs(1), Some(&mut followup), |_| panic!(
                "a required followup frame must not block"
            ),)
            .expect("immediate thumbnail followup"),
            WaitOutcome::ThumbnailRedraw
        );
    }

    #[test]
    fn search_panel_title_cycles_ascii_frames_only_on_the_matching_screen() {
        let mut view = ViewModel {
            search_query: "ambient".to_owned(),
            search_activity: Some(SearchActivity::YouTube),
            ..ViewModel::default()
        };

        for (frame, symbol) in ['|', '/', '-', '\\'].into_iter().enumerate() {
            view.search_animation_frame = frame;
            assert_eq!(
                search_panel_title(&view),
                format!(" {symbol} YouTube — ambient ")
            );
        }

        view.screen = Screen::TrackerMusic;
        assert_eq!(search_panel_title(&view), " MOD/tracker — ambient ");
        view.search_activity = Some(SearchActivity::TrackerArchives);
        view.search_animation_frame = 0;
        assert_eq!(search_panel_title(&view), " | MOD/tracker — ambient ");
        view.search_activity = None;
        assert_eq!(search_panel_title(&view), " MOD/tracker — ambient ");
    }

    #[test]
    fn playback_start_status_cycles_ascii_frames_and_restores_plain_status() {
        let mut view = ViewModel {
            playback_starting: true,
            status_line: "Loading Fixture audio…".to_owned(),
            ..ViewModel::default()
        };

        for (frame, symbol) in ASCII_ACTIVITY_FRAMES.into_iter().enumerate() {
            view.playback_start_animation_frame = frame;
            assert_eq!(
                animated_status_line(&view),
                format!("{symbol} Loading Fixture audio…")
            );
        }

        view.playback_starting = false;
        assert_eq!(animated_status_line(&view), "Loading Fixture audio…");
    }

    #[test]
    fn play_marker_follows_playing_media_instead_of_selection_while_paused() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let playing = MediaId::new(crate::domain::SourceKind::YouTube, "playing");
        let selected = MediaId::new(crate::domain::SourceKind::YouTube, "selected");
        let view = ViewModel {
            rows: vec![
                RowView {
                    media_id: Some(playing.clone()),
                    title: "Playing row".to_owned(),
                    source: "YouTube".to_owned(),
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(selected),
                    title: "Selected row".to_owned(),
                    source: "YouTube".to_owned(),
                    ..RowView::default()
                },
            ],
            selected: 1,
            playing_media_id: Some(playing),
            playback: PlaybackStatus {
                idle: false,
                paused: true,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw independent playing and selected rows");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 1)].symbol(), "▶");
        assert_eq!(buffer[(0, 1)].fg, Color::Cyan);
        assert_eq!(buffer[(0, 3)].symbol(), " ");
        assert_eq!(buffer[(0, 3)].bg, Color::Cyan);
        assert_eq!(
            buffer
                .content()
                .iter()
                .filter(|cell| cell.symbol() == "▶")
                .count(),
            1
        );
    }

    #[test]
    fn vertical_video_titles_keep_playing_and_selection_precedence() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let playing = MediaId::new(SourceKind::YouTube, "playing-vertical");
        let view = ViewModel {
            rows: vec![
                RowView {
                    media_id: Some(playing.clone()),
                    title: "Playing vertical".to_owned(),
                    source: "YouTube".to_owned(),
                    vertical: true,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "idle-vertical")),
                    title: "Vertical idle".to_owned(),
                    source: "YouTube".to_owned(),
                    vertical: true,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "selected-vertical")),
                    title: "Selected vertical".to_owned(),
                    source: "YouTube".to_owned(),
                    vertical: true,
                    ..RowView::default()
                },
            ],
            selected: 2,
            playing_media_id: Some(playing),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw vertical-video colors");
        let buffer = terminal.backend().buffer();
        let playing_title = &buffer[(6, 1)];
        assert_eq!(playing_title.symbol(), "P");
        assert_eq!(playing_title.fg, Color::Rgb(255, 105, 180));
        assert!(playing_title.modifier.contains(Modifier::BOLD));
        let idle_title = &buffer[(6, 3)];
        assert_eq!(idle_title.symbol(), "V");
        assert_eq!(idle_title.fg, Color::Rgb(255, 105, 180));
        assert!(!idle_title.modifier.contains(Modifier::BOLD));
        let selected_title = &buffer[(6, 5)];
        assert_eq!(selected_title.symbol(), "S");
        assert_eq!(selected_title.fg, Color::Black);
        assert_eq!(selected_title.bg, Color::Cyan);
        assert_eq!(buffer[(0, 1)].symbol(), "▶");
    }

    #[test]
    fn active_chapter_colors_use_terminal_palette_pinks() {
        assert_eq!(
            Theme::new(false).active_chapter.fg,
            Some(Color::Magenta),
            "the normal theme must use adaptable ANSI magenta, not fixed RGB"
        );
        assert_eq!(
            Theme::new(true).active_chapter.fg,
            Some(Color::LightMagenta),
            "the DOS theme keeps its brighter terminal-palette accent"
        );
    }

    #[test]
    fn selected_row_has_no_play_marker_when_nothing_is_playing() {
        let backend = TestBackend::new(100, 10);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            rows: vec![RowView {
                media_id: Some(MediaId::new(crate::domain::SourceKind::YouTube, "selected")),
                title: "Selected row".to_owned(),
                source: "YouTube".to_owned(),
                ..RowView::default()
            }],
            selected: 0,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw selected idle row");
        let buffer = terminal.backend().buffer();

        assert_eq!(buffer[(0, 1)].symbol(), " ");
        assert_eq!(buffer[(0, 1)].bg, Color::Cyan);
        assert!(buffer.content().iter().all(|cell| cell.symbol() != "▶"));
    }

    #[test]
    fn list_rows_show_subscription_and_watched_state_independently() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            rows: vec![
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "unwatched")),
                    title: "Unsubscribed unwatched row".to_owned(),
                    source: "YouTube".to_owned(),
                    subscribed: false,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "partial")),
                    title: "Unsubscribed partial row".to_owned(),
                    source: "YouTube".to_owned(),
                    watched_percent: 90,
                    subscribed: false,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::YouTube, "watched")),
                    title: "Subscribed watched row".to_owned(),
                    source: "YouTube".to_owned(),
                    watched_percent: 91,
                    subscribed: true,
                    ..RowView::default()
                },
                RowView {
                    title: "Subscribed channel source".to_owned(),
                    source: "YouTube channel".to_owned(),
                    subscribed: true,
                    ..RowView::default()
                },
            ],
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw subscription markers");
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(
            !rendered.contains('◇'),
            "unsubscribed rows must not show a hollow subscription marker"
        );
        assert_eq!(
            buffer[(2, 1)].symbol(),
            " ",
            "the unsubscribed row keeps alignment without a visible marker"
        );
        assert_eq!(
            buffer[(4, 1)].symbol(),
            "●",
            "an unwatched row has a playback marker regardless of subscription"
        );
        assert_eq!(
            buffer[(4, 3)].symbol(),
            "◐",
            "a partially watched row has a playback marker regardless of subscription"
        );
        assert_eq!(
            buffer[(2, 5)].symbol(),
            "◆",
            "a locally subscribed row keeps the solid subscription marker"
        );
        assert_eq!(
            buffer[(4, 5)].symbol(),
            "○",
            "more than 90 percent watched uses the completed marker"
        );
        assert_eq!(
            buffer[(4, 7)].symbol(),
            " ",
            "a non-playable channel source has no watched-state marker"
        );
        assert!(
            rendered.contains(" 90%"),
            "exactly 90 percent remains partial and visible"
        );
        assert!(
            rendered.contains(" 91%"),
            "completed rows retain their exact watched percentage"
        );
    }

    #[test]
    fn main_panels_are_borderless_and_keep_headings_focus_and_mouse_regions() {
        let backend = TestBackend::new(100, 12);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            rows: vec![RowView {
                title: "Borderless result".to_owned(),
                source: "YouTube".to_owned(),
                ..RowView::default()
            }],
            details: Some(DetailView {
                description: "Borderless details content".to_owned(),
                ..DetailView::default()
            }),
            details_focused: true,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_body(
                    frame,
                    frame.area(),
                    &view,
                    true,
                    DEFAULT_THUMBNAIL_HEIGHT,
                    &Theme::new(false),
                    &mut hit_map,
                    None,
                );
            })
            .expect("draw borderless main panels");
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        for border in ['┌', '┐', '└', '┘', '─', '│'] {
            assert!(
                !rendered.contains(border),
                "main panes must not render the {border} box-border glyph"
            );
        }
        assert!(rendered.contains("YouTube video search"));
        assert!(rendered.contains("Borderless result"));
        assert!(rendered.contains("Borderless details content"));
        let details_heading = &buffer[(hit_map.details_panel.x, hit_map.details_panel.y)];
        assert_eq!(details_heading.symbol(), "D");
        assert_eq!(details_heading.fg, Color::Cyan);
        assert!(
            details_heading
                .modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED)
        );
        assert_eq!(hit_map.rows.y, 1);
        assert_eq!(hit_map.rows.bottom(), buffer.area.bottom());
        assert_eq!(hit_map.rows.right(), hit_map.details_panel.x);
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: hit_map.rows.x,
                    row: hit_map.rows.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SelectRow(0))
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: hit_map.details_panel.x,
                    row: hit_map.details_panel.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SetDetailsFocus(true))
        );
    }

    #[test]
    fn popup_panels_keep_their_box_borders() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");

        terminal
            .draw(|frame| render_help(frame, &Theme::new(false)))
            .expect("draw bordered help popup");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("Youta help"));
        assert!(rendered.contains("Alt+←/→ Details back/forward"));
        assert!(rendered.contains("Backspace Details back"));
        assert!(rendered.contains("Local: Z size order"));
        assert!(rendered.contains("↪ internal video"));
        assert!(rendered.contains("F8 pointer"));
        assert!(rendered.contains("GPM mouse input is detected automatically"));
        for border in ['┌', '┐', '└', '┘'] {
            assert!(
                rendered.contains(border),
                "popup panels must retain the {border} border glyph"
            );
        }
    }

    #[test]
    fn key_map_separates_seek_controls_from_details_history() {
        let view = ViewModel::default();
        let alt_left = KeyEvent::new(KeyCode::Left, KeyModifiers::ALT);
        let alt_right = KeyEvent::new(KeyCode::Right, KeyModifiers::ALT);
        assert_eq!(key_action(alt_left, &view), Some(UiAction::GoBack));
        assert_eq!(key_action(alt_right, &view), Some(UiAction::GoForward));
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &view,),
            Some(UiAction::GoBack)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &view),
            Some(UiAction::SeekRelative(-5))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &view),
            Some(UiAction::SeekRelative(5))
        );
        let five = KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE);
        assert_eq!(key_action(five, &view), Some(UiAction::SeekPercent(50.0)));
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::ToggleAutoplay)
        );
    }

    #[test]
    fn focused_details_use_page_keys_without_stealing_volume_or_row_keys() {
        let focused = ViewModel {
            details_focused: true,
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &focused),
            Some(UiAction::ScrollDetails(DetailsScroll::Pages(-1)))
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                &focused
            ),
            Some(UiAction::ScrollDetails(DetailsScroll::Pages(1)))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &focused),
            Some(UiAction::ChangeVolume(5))
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
                &focused
            ),
            Some(UiAction::MoveSelection(1))
        );

        let unfocused = ViewModel::default();
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                &unfocused
            ),
            None
        );
    }

    #[test]
    fn text_selection_mode_reserves_keys_for_copying_and_returning_to_controls() {
        let available = ViewModel {
            details: Some(DetailView::default()),
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
                &available
            ),
            Some(UiAction::ToggleTextSelectionMode)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
                &ViewModel::default()
            ),
            None,
            "text selection cannot start without Details content"
        );

        let active = ViewModel {
            text_selection_mode: true,
            ..available
        };
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &active),
            Some(UiAction::ToggleTextSelectionMode)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT),
                &active
            ),
            Some(UiAction::ToggleChapterTimestamps)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &active
            ),
            Some(UiAction::Quit)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
                &active,
            ),
            None,
            "a forwarded terminal Copy chord must never quit Youta"
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
                &active
            ),
            None,
            "copy mode must not dispatch unrelated Youta controls"
        );
    }

    #[test]
    fn diagnostic_popup_captures_keyboard_and_maps_all_report_controls() {
        let view = ViewModel {
            search_editing: true,
            help_open: true,
            error_popup: Some(ErrorPopupView {
                title: "Playback failed".to_owned(),
                report: "complete report".to_owned(),
                gh_available: true,
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::DismissErrorPopup)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &view
            ),
            Some(UiAction::CopyErrorReport)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &view),
            Some(UiAction::FillGitHubIssue)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &view),
            Some(UiAction::CopyAndOpenGitHubIssue)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &view),
            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(1)))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE), &view),
            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Pages(-1)))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), &view),
            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Home))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &view),
            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::End))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &view),
            None,
            "the popup must not leak the normal quit action"
        );
    }

    #[test]
    fn diagnostic_popup_uses_browser_fallback_when_github_cli_is_unavailable() {
        let view = ViewModel {
            error_popup: Some(ErrorPopupView {
                title: "Error".to_owned(),
                report: "report".to_owned(),
                gh_available: false,
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &view),
            Some(UiAction::CopyAndOpenGitHubIssue)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE), &view),
            None
        );
    }

    #[test]
    fn rename_popup_maps_arrows_to_cursor_movement_without_seeking() {
        let view = ViewModel {
            local_file_popup: Some(LocalFilePopupView::Rename {
                value: "трек.flac".to_owned(),
                cursor_byte: "трек".len(),
                error: None,
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &view),
            Some(UiAction::MoveLocalRenameCursor(-1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &view),
            Some(UiAction::MoveLocalRenameCursor(1))
        );
    }

    #[test]
    fn rename_popup_renders_the_caret_at_its_utf8_cursor() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            local_file_popup: Some(LocalFilePopupView::Rename {
                value: "трек.flac".to_owned(),
                cursor_byte: "трек".len(),
                error: None,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw rename popup");

        assert!(rendered_text(&terminal).contains("трек▏.flac"));
    }

    #[test]
    fn youtube_setup_popup_captures_keyboard_and_edits_only_through_setup_actions() {
        let mut view = ViewModel {
            search_editing: true,
            help_open: true,
            youtube_setup_popup: Some(YouTubeSetupPopupView {
                selected_field: YouTubeSetupField::ApiKey,
                ..YouTubeSetupPopupView::default()
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE), &view),
            Some(UiAction::AppendYouTubeSetupCharacter('A'))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &view),
            Some(UiAction::DeleteYouTubeSetupCharacter)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &view),
            Some(UiAction::SelectYouTubeSetupField(
                YouTubeSetupField::InvidiousUrl
            ))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::SubmitYouTubeSetup)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), &view),
            Some(UiAction::OpenYouTubeApiKeyGuide)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE), &view),
            Some(UiAction::OpenGoogleCloudCredentials)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE), &view),
            Some(UiAction::OpenInvidiousInstances)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::DismissYouTubeSetup)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &view
            ),
            None,
            "the modal must not leak the normal quit action"
        );

        view.youtube_setup_popup
            .as_mut()
            .expect("setup popup")
            .selected_field = YouTubeSetupField::InvidiousUrl;
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &view),
            Some(UiAction::SelectYouTubeSetupField(YouTubeSetupField::ApiKey))
        );
    }

    #[test]
    fn keyboard_moves_selects_and_activates_detail_links_without_replacing_list_controls() {
        let view = ViewModel {
            details: Some(DetailView {
                links: vec![
                    DetailLinkView {
                        label: "First".to_owned(),
                        url: "https://example.com/first".to_owned(),
                        ..DetailLinkView::default()
                    },
                    DetailLinkView {
                        label: "Second".to_owned(),
                        url: "https://example.com/second".to_owned(),
                        wikidata_item_id: Some("Q42".to_owned()),
                    },
                ],
                ..DetailView::default()
            }),
            selected_detail_link: Some(1),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT), &view,),
            Some(UiAction::MoveDetailLink(1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT), &view,),
            Some(UiAction::MoveDetailLink(-1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Home, KeyModifiers::ALT), &view),
            Some(UiAction::SelectDetailLink(0))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::End, KeyModifiers::ALT), &view),
            Some(UiAction::SelectDetailLink(1))
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::ToggleWikidataStatements(1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT), &view),
            Some(UiAction::ActivateDetailLink(1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &view,),
            Some(UiAction::MoveSelection(1))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::ActivateSelection)
        );
    }

    #[test]
    fn tab_shortcuts_cycle_every_enabled_screen_and_wrap() {
        assert_eq!(Screen::TrackerMusic.next(), Screen::Subscriptions);
        assert_eq!(Screen::Subscriptions.next(), Screen::Local);
        assert_eq!(Screen::Local.previous(), Screen::Subscriptions);

        for (index, screen) in Screen::ALL.into_iter().enumerate() {
            let next = Screen::ALL[(index + 1) % Screen::ALL.len()];
            let previous = Screen::ALL[(index + Screen::ALL.len() - 1) % Screen::ALL.len()];
            let mut view = ViewModel {
                screen,
                ..ViewModel::default()
            };
            view.subscriptions.route = SubscriptionRoute::Items;
            for modifiers in [KeyModifiers::NONE, KeyModifiers::CONTROL] {
                assert_eq!(
                    key_action(KeyEvent::new(KeyCode::Tab, modifiers), &view),
                    Some(UiAction::ShowScreen(next)),
                    "Tab with {modifiers:?} must advance from {screen:?}"
                );
            }
            for key in [
                KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                KeyEvent::new(
                    KeyCode::BackTab,
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                ),
                KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
            ] {
                assert_eq!(
                    key_action(key, &view),
                    Some(UiAction::ShowScreen(previous)),
                    "{key:?} must move backward from {screen:?}"
                );
            }
        }

        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('S'), KeyModifiers::SHIFT),
                &ViewModel::default()
            ),
            Some(UiAction::ShowScreen(Screen::Subscriptions))
        );
    }

    #[test]
    fn preferences_and_channel_page_have_distinct_global_shortcuts() {
        let view = ViewModel::default();
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &view),
            Some(UiAction::OpenPreferences)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE), &view),
            Some(UiAction::OpenPreferences)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::OpenChannelInBrowser)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &view),
            Some(UiAction::OpenInBrowser)
        );
    }

    #[test]
    fn uppercase_m_opens_tracker_music_screen() {
        let view = ViewModel::default();
        let tracker = KeyEvent::new(KeyCode::Char('M'), KeyModifiers::SHIFT);
        assert_eq!(
            key_action(tracker, &view),
            Some(UiAction::ShowScreen(Screen::TrackerMusic))
        );
    }

    #[test]
    fn uppercase_n_toggles_youtube_search_order() {
        let view = ViewModel::default();
        let newest = KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT);

        assert_eq!(
            key_action(newest, &view),
            Some(UiAction::ToggleYouTubeSearchSort)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &view),
            Some(UiAction::PlayNext),
            "lowercase play-next must retain its existing action"
        );
    }

    #[test]
    fn uppercase_c_toggles_creative_commons_filter_without_replacing_channel_info() {
        let view = ViewModel::default();
        let creative_commons = KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SHIFT);

        assert_eq!(
            key_action(creative_commons, &view),
            Some(UiAction::ToggleYouTubeCreativeCommons)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &view),
            Some(UiAction::ShowChannel),
            "lowercase channel-info must retain its existing action"
        );
    }

    #[test]
    fn uppercase_t_toggles_chapter_timestamps_without_replacing_text_selection() {
        let view = ViewModel {
            details: Some(DetailView::default()),
            text_selection_mode: true,
            ..ViewModel::default()
        };
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::ToggleChapterTimestamps)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleTextSelectionMode)
        );
    }

    #[test]
    fn render_contains_player_and_hotkey_controls() {
        let backend = TestBackend::new(240, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            rows: vec![RowView {
                title: "Mock video".to_owned(),
                source: "YouTube / Invidious".to_owned(),
                subtitle: "Mock channel".to_owned(),
                ..RowView::default()
            }],
            details: Some(DetailView {
                title: "Mock video".to_owned(),
                description: "Description with #example".to_owned(),
                license: "Creative Commons".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        view.playback.duration = Some(Duration::from_secs(120));
        view.playback.position = Duration::from_secs(30);
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hit_map))
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("Mock video"));
        assert!(rendered.contains("Creative Commons"));
        assert!(rendered.contains("License:"));
        assert!(rendered.contains("[Space] Pause"));
        assert!(rendered.contains("[M] MOD/tracker music"));
        assert!(rendered.contains("[C] CC only: off"));
        assert!(rendered.contains("[k] Move up"));
        assert!(rendered.contains("[j] Move down"));
        assert!(rendered.contains("[↑] Volume up"));
        assert!(rendered.contains("[↓] Volume down"));
        assert!(rendered.contains("[Enter] Start"));
        assert!(rendered.contains("[T] Chapter times: on"));
        assert!(rendered.contains("0:30 / 2:00"));
        for expected in [
            UiAction::MoveSelection(-1),
            UiAction::MoveSelection(1),
            UiAction::ChangeVolume(5),
            UiAction::ChangeVolume(-5),
            UiAction::ActivateSelection,
        ] {
            assert!(
                hit_map
                    .buttons
                    .iter()
                    .any(|(action, _)| action == &expected),
                "missing clickable bottom-line action {expected:?}"
            );
        }
    }

    fn subscription_row(title: &str, video: bool) -> RowView {
        RowView {
            media_id: video.then(|| MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ")),
            title: title.to_owned(),
            subtitle: if video {
                "2026 July 25 (yesterday) · 3:32".to_owned()
            } else {
                "https://www.youtube.com/feeds/videos.xml?channel_id=UCfixture".to_owned()
            },
            source: if video {
                "YouTube".to_owned()
            } else {
                "YouTube channel".to_owned()
            },
            subscribed: !video,
            ..RowView::default()
        }
    }

    #[test]
    fn compact_local_rows_drop_redundant_source_labels_and_folder_padding() {
        let backend = TestBackend::new(140, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            screen: Screen::Local,
            local_path: "/music".to_owned(),
            rows: vec![
                RowView {
                    title: "A long album folder".to_owned(),
                    subtitle: "/music/A long album folder".to_owned(),
                    source: "Local folder".to_owned(),
                    compact: true,
                    ..RowView::default()
                },
                RowView {
                    media_id: Some(MediaId::new(SourceKind::Local, "/music/song.flac")),
                    title: "song.flac".to_owned(),
                    subtitle: "42.00 MiB".to_owned(),
                    source: "Local audio".to_owned(),
                    compact: true,
                    ..RowView::default()
                },
            ],
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw compact Local rows");
        let rendered = rendered_text(&terminal);
        let buffer = terminal.backend().buffer();

        assert!(!rendered.contains("Local folder"));
        assert!(!rendered.contains("Local audio"));
        assert_eq!(
            buffer[(hit_map.rows.x, hit_map.rows.y)].symbol(),
            "A",
            "a non-media folder title must start at the first list cell"
        );
        assert_eq!(
            buffer[(hit_map.rows.x, hit_map.rows.y.saturating_add(1))].symbol(),
            "●",
            "the next Local item must occupy the immediately following row"
        );
        assert!(rendered.contains("A long album folder · /music/A long album folder"));
        assert!(rendered.contains("● song.flac · 42.00 MiB"));
        assert_eq!(hit_map.rows_row_height, 1);
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: hit_map.rows.x,
                    row: hit_map.rows.y.saturating_add(1),
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SelectRow(1))
        );
    }

    #[test]
    fn subscriptions_render_both_drill_down_and_split_navigation_models() {
        let backend = TestBackend::new(160, 34);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let details = DetailView {
            media_id: Some(MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ")),
            title: "Fixture channel".to_owned(),
            description: "Expanded fixture description".to_owned(),
            channel_id: "UCfixture".to_owned(),
            channel_webpage_url: Some(
                url::Url::parse("https://www.youtube.com/@fixture").expect("fixture channel URL"),
            ),
            ..DetailView::default()
        };
        let mut view = ViewModel {
            screen: Screen::Subscriptions,
            details: Some(details),
            ..ViewModel::default()
        };
        // Leading spaces represent one nested OPML folder level in source rows.
        view.subscriptions.sources = vec![subscription_row("  Fixture channel", false)];
        view.subscriptions.items = vec![subscription_row("Fixture video", true)];
        view.subscriptions.source_title = "Fixture channel".to_owned();
        view.subscriptions.source_subscriber_count = Some(13_045);
        view.playing_media_id = view.subscriptions.items[0].media_id.clone();
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw drill-down sources");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Subscription sources"));
        assert!(rendered.contains("Fixture channel"));
        assert_eq!(
            rendered.matches("Fixture channel").count(),
            1,
            "the channel title already visible in the source row must not repeat"
        );
        assert!(rendered.contains("[O] xdg-open"));
        assert!(!rendered.contains("Refresh videos"));
        assert!(hit_map.subscription_source_rows.width > 0);
        assert_eq!(hit_map.subscription_item_rows, Rect::default());

        view.subscriptions.route = SubscriptionRoute::Items;
        view.subscriptions.focus = SubscriptionPane::Items;
        view.right_panel_mode = RightPanelMode::Details;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw drill-down items");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Fixture channel · YouTube · 13,045 subscribers"));
        assert!(rendered.contains("Fixture video"));
        assert!(
            !rendered.contains("YouTube · 2026 July 25"),
            "the subscription heading already identifies the video source"
        );
        assert!(
            !rendered.contains('◆'),
            "subscription videos must omit the redundant channel marker"
        );
        assert!(
            rendered.contains("▶ ● Fixture video"),
            "playing subscription videos keep one compact separator before the watched marker"
        );
        assert!(rendered.contains("Expanded fixture description"));
        assert!(rendered.contains("[R] Refresh videos"));
        assert!(hit_map.subscription_item_rows.width > 0);

        view.subscriptions.layout = SubscriptionsLayout::Split;
        view.subscriptions.route = SubscriptionRoute::Sources;
        view.subscriptions.focus = SubscriptionPane::Items;
        view.subscriptions.description_expanded = false;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw split lists");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Subscription sources"));
        assert!(rendered.contains("Fixture channel · YouTube · 13,045 subscribers"));
        assert!(
            !rendered.contains("YouTube · 2026 July 25"),
            "split subscription rows must also omit the repeated source"
        );
        assert!(
            rendered.contains("▶ ● Fixture video"),
            "split subscription rows keep the same compact marker spacing"
        );
        assert!(rendered.contains("[R] Refresh videos"));
        assert!(rendered.contains("[i] Description"));
        assert!(hit_map.subscription_source_rows.width > 0);
        assert!(hit_map.subscription_item_rows.width > 0);
        let (_, description_target) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleSubscriptionDescription)
            .expect("description button target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: description_target.x,
                    row: description_target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ToggleSubscriptionDescription)
        );
        let (_, refresh_target) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::RefreshSubscriptionVideos)
            .expect("subscription refresh button target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: refresh_target.x,
                    row: refresh_target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::RefreshSubscriptionVideos)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::NONE), &view),
            Some(UiAction::RefreshSubscriptionVideos)
        );

        view.subscriptions.description_expanded = true;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw split description");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Expanded fixture description"));
        assert!(rendered.contains("[R] Refresh videos"));

        view.subscriptions.description_expanded = false;
        view.subscriptions.source_subscriber_count = None;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw split list without public subscriber count");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Fixture channel · YouTube"));
        assert!(!rendered.contains("Fixture channel · YouTube · "));
        assert!(rendered.contains("[i] Description"));
    }

    #[test]
    fn subscription_mouse_rows_target_their_independent_lists() {
        let view = ViewModel {
            screen: Screen::Subscriptions,
            subscriptions: SubscriptionsView {
                layout: SubscriptionsLayout::Split,
                sources: vec![subscription_row("Source", false)],
                items: vec![subscription_row("Video", true)],
                ..SubscriptionsView::default()
            },
            ..ViewModel::default()
        };
        let hit_map = HitMap {
            subscription_source_rows: Rect::new(1, 2, 20, 4),
            subscription_item_rows: Rect::new(30, 2, 20, 4),
            ..HitMap::default()
        };
        let click = |column| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            mouse_action(click(1), &hit_map, &view),
            Some(UiAction::SelectSubscriptionSource(0))
        );
        assert_eq!(
            mouse_action(click(30), &hit_map, &view),
            Some(UiAction::SelectSubscriptionItem(0))
        );
    }

    #[test]
    fn narrow_expanded_subscription_buttons_never_overlap_mouse_targets() {
        let backend = TestBackend::new(38, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            screen: Screen::Subscriptions,
            details: Some(DetailView {
                description: "Expanded fixture description".to_owned(),
                ..DetailView::default()
            }),
            subscriptions: SubscriptionsView {
                layout: SubscriptionsLayout::Split,
                focus: SubscriptionPane::Items,
                description_expanded: true,
                sources: vec![subscription_row("Source", false)],
                items: vec![subscription_row("Video", true)],
                ..SubscriptionsView::default()
            },
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw narrow expanded subscription");

        let back = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleSubscriptionDescription)
            .map(|(_, target)| *target)
            .expect("back target");
        let refresh = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::RefreshSubscriptionVideos)
            .map(|(_, target)| *target)
            .expect("refresh target");
        assert!(
            back.right() <= refresh.x || refresh.right() <= back.x,
            "visible buttons must own disjoint mouse regions"
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: refresh.x,
                    row: refresh.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::RefreshSubscriptionVideos)
        );
    }

    #[test]
    fn subscription_lists_keep_late_selection_visible_and_mouse_indexes_aligned() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            screen: Screen::Subscriptions,
            ..ViewModel::default()
        };
        view.subscriptions.sources = (0..20)
            .map(|index| subscription_row(&format!("Source {index:02}"), false))
            .collect();
        view.subscriptions.selected_source = 17;
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw scrolled subscriptions");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Source 17"));
        assert!(!rendered.contains("Source 00"));
        assert!(hit_map.subscription_source_first_index > 0);
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: hit_map.subscription_source_rows.x,
                    row: hit_map.subscription_source_rows.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SelectSubscriptionSource(
                hit_map.subscription_source_first_index
            ))
        );
    }

    #[test]
    fn preferences_popup_is_modal_selectable_and_clickable() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            preferences_popup: Some(PreferencesPopupView {
                subscriptions_layout: SubscriptionsLayout::DrillDown,
                skip_advertisement_chapters: true,
                youtube_prewarm: true,
                show_local_folder_sizes: true,
                config_path: "/tmp/youta/config.toml".to_owned(),
                environment_override: None,
                validation_error: None,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw preferences");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Youta preferences"));
        assert!(rendered.contains("[d] Drill-down"));
        assert!(rendered.contains("[s] Split"));
        assert!(rendered.contains("[y] Prepare selected YouTube audio: on"));
        assert!(rendered.contains("[f] Show Local folder sizes: on"));
        assert!(rendered.contains("/tmp/youta/config.toml"));
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &view),
            Some(UiAction::SetSubscriptionsLayout(SubscriptionsLayout::Split))
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &view),
            Some(UiAction::SubmitPreferences)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleYouTubePrewarm)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleLocalFolderSizes)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &view),
            None,
            "the modal must not leak top-level tab cycling"
        );
        let (_, split_target) = hit_map
            .preferences_buttons
            .iter()
            .find(|(action, _)| {
                action == &UiAction::SetSubscriptionsLayout(SubscriptionsLayout::Split)
            })
            .expect("split choice target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: split_target.x,
                    row: split_target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SetSubscriptionsLayout(SubscriptionsLayout::Split))
        );
        let (_, youtube_prewarm_target) = hit_map
            .preferences_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleYouTubePrewarm)
            .expect("YouTube-prewarm target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: youtube_prewarm_target.x,
                    row: youtube_prewarm_target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ToggleYouTubePrewarm)
        );
        let (_, folder_size_target) = hit_map
            .preferences_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleLocalFolderSizes)
            .expect("Local folder-size target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: folder_size_target.x,
                    row: folder_size_target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ToggleLocalFolderSizes)
        );
    }

    #[test]
    fn diagnostic_keyboard_routing_precedes_stacked_preferences_and_text_selection() {
        let view = ViewModel {
            text_selection_mode: true,
            preferences_popup: Some(PreferencesPopupView {
                subscriptions_layout: SubscriptionsLayout::DrillDown,
                skip_advertisement_chapters: true,
                youtube_prewarm: true,
                show_local_folder_sizes: true,
                config_path: "/tmp/youta/config.toml".to_owned(),
                environment_override: None,
                validation_error: Some("save failed".to_owned()),
            }),
            error_popup: Some(ErrorPopupView {
                title: "Preferences failed".to_owned(),
                report: "complete report".to_owned(),
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &view),
            Some(UiAction::DismissErrorPopup)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &view),
            Some(UiAction::CopyErrorReport)
        );
        let target = Rect::new(2, 3, 6, 1);
        let hit_map = HitMap {
            error_buttons: vec![(UiAction::CopyErrorReport, target)],
            preferences_buttons: vec![(
                UiAction::SetSubscriptionsLayout(SubscriptionsLayout::Split),
                target,
            )],
            detail_text_rows: vec![SelectableDetailsRow {
                x: target.x,
                y: target.y,
                cells: vec!["x".to_owned(); usize::from(target.width)],
            }],
            ..HitMap::default()
        };
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: target.x,
                    row: target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::CopyErrorReport)
        );
    }

    #[test]
    fn bottom_button_shows_and_clicks_the_current_youtube_search_order() {
        let backend = TestBackend::new(240, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            youtube_search_sort: YouTubeSearchSort::Newest,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw newest ordering");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("[N] Sort: newest"));
        let (_, target) = hit_map
            .buttons
            .iter()
            .find(|(action, _)| *action == UiAction::ToggleYouTubeSearchSort)
            .expect("sort button hit target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: target.x,
                    row: target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ToggleYouTubeSearchSort)
        );

        view.youtube_search_sort = YouTubeSearchSort::Relevance;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw relevance ordering");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("[N] Sort: relevance"));
    }

    #[test]
    fn bottom_button_shows_and_clicks_the_creative_commons_filter() {
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            youtube_creative_commons_only: true,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw enabled CC filter");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("[C] CC:on"));
        let (_, target) = hit_map
            .buttons
            .iter()
            .find(|(action, _)| *action == UiAction::ToggleYouTubeCreativeCommons)
            .expect("Creative Commons button hit target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: target.x,
                    row: target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ToggleYouTubeCreativeCommons)
        );

        view.youtube_creative_commons_only = false;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw disabled CC filter");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("[C] CC:off"));
    }

    #[test]
    fn video_row_shows_publication_date_while_details_omit_published_line() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            rows: vec![RowView {
                title: "Fixture video".to_owned(),
                subtitle: "Fixture channel · 2026 July 25 · 4:05".to_owned(),
                source: "YouTube".to_owned(),
                ..RowView::default()
            }],
            details: Some(DetailView {
                title: "Fixture video".to_owned(),
                source: "YouTube".to_owned(),
                published: "2026 July 25".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw publication metadata");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(
            rendered.contains("Fixture channel · 2026 July 25 · 4:05"),
            "{rendered}"
        );
        assert!(!rendered.contains("Published:"));
    }

    #[test]
    fn focused_details_render_scrolled_description_and_visible_scrollbar() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let description = (0..=60)
            .map(|line| format!("DETAIL_LINE_{line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Fixture video".to_owned(),
                source: "YouTube".to_owned(),
                description,
                ..DetailView::default()
            }),
            details_focused: true,
            details_scroll: 10,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw focused details");
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(!rendered.contains("DETAIL_LINE_00"));
        assert!(rendered.contains("DETAIL_LINE_10"), "{rendered}");
        assert!(
            buffer.content().iter().any(|cell| cell.symbol() == "█"),
            "overflowing details must render a scrollbar thumb"
        );
        let details_heading = &buffer[(hit_map.details_panel.x, hit_map.details_panel.y)];
        assert_eq!(details_heading.symbol(), "D");
        assert_eq!(details_heading.fg, Color::Cyan);
        assert!(
            details_heading
                .modifier
                .contains(Modifier::BOLD | Modifier::UNDERLINED),
            "focused Details must use a distinct accent heading"
        );
        assert!(hit_map.details_panel.width > 0);
        assert!(hit_map.details_panel.height > 0);
    }

    #[test]
    fn details_end_scroll_reaches_last_wrapped_row_and_scrollbar_bottom() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let description = (0..=60)
            .map(|line| format!("BOTTOM_SCROLL_LINE_{line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Long description".to_owned(),
                source: "YouTube".to_owned(),
                description,
                ..DetailView::default()
            }),
            details_focused: true,
            details_scroll: usize::MAX,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw description end");

        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("BOTTOM_SCROLL_LINE_60"), "{rendered}");
        assert!(!rendered.contains("BOTTOM_SCROLL_LINE_00"), "{rendered}");
        assert!(
            !rendered.contains("Description"),
            "the Details panel must not add a Description heading"
        );
        let scrollbar_bottom = (
            hit_map.details_panel.right().saturating_sub(1),
            hit_map.details_panel.bottom().saturating_sub(1),
        );
        assert_eq!(
            buffer[scrollbar_bottom].symbol(),
            "█",
            "the scrollbar thumb must reach the final track cell"
        );
        assert_eq!(
            hit_map.details_scroll_offset,
            hit_map.details_scroll_maximum
        );
        assert!(hit_map.details_scroll_maximum >= 3);

        let rendered_at_end = rendered;
        let upward = mouse_action(
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: hit_map.details_panel.x,
                row: hit_map.details_panel.y,
                modifiers: KeyModifiers::NONE,
            },
            &hit_map,
            &view,
        );
        let expected_offset = hit_map.details_scroll_maximum - 3;
        assert_eq!(
            upward,
            Some(UiAction::SetDetailsScroll(expected_offset)),
            "the first upward wheel notch must use the visible clamped offset"
        );

        view.details_scroll = expected_offset;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw one wheel notch above description end");
        let rendered_after_wheel = rendered_text(&terminal);
        assert_eq!(hit_map.details_scroll_offset, expected_offset);
        assert_ne!(
            rendered_after_wheel, rendered_at_end,
            "one upward wheel notch must visibly change the wrapped text window"
        );
    }

    #[test]
    fn description_timecodes_are_unicode_safe_click_targets_and_selection_takes_priority() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
        let description =
            "Вступление\n00:01:35 Многоэтажное великолепие Батуми\n01:02:51 Relocation";
        let first_start = description.find("00:01:35").expect("first timestamp");
        let second_start = description.find("01:02:51").expect("second timestamp");
        let mut view = ViewModel {
            details: Some(DetailView {
                media_id: Some(media_id.clone()),
                description: description.to_owned(),
                timecodes: vec![
                    DetailTimecodeView {
                        start_byte: first_start,
                        end_byte: first_start + "00:01:35".len(),
                        seconds: 95,
                        is_chapter: true,
                    },
                    DetailTimecodeView {
                        start_byte: second_start,
                        end_byte: second_start + "01:02:51".len(),
                        seconds: 3_771,
                        is_chapter: true,
                    },
                ],
                ..DetailView::default()
            }),
            playing_media_id: Some(media_id.clone()),
            playback: PlaybackStatus {
                position: Duration::from_secs(100),
                ..PlaybackStatus::default()
            },
            playback_chapters: vec![
                Chapter {
                    title: "Многоэтажное великолепие Батуми".to_owned(),
                    start_seconds: 95,
                    end_seconds: Some(3_771),
                },
                Chapter {
                    title: "Relocation".to_owned(),
                    start_seconds: 3_771,
                    end_seconds: None,
                },
            ],
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw clickable timecodes");
        let (_, target) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| {
                action
                    == &UiAction::ActivateTimecode {
                        media_id: media_id.clone(),
                        seconds: 95,
                    }
            })
            .expect("first timecode hit target");
        let target = *target;
        let inactive_target = hit_map
            .detail_buttons
            .iter()
            .find_map(|(action, target)| {
                (action
                    == &UiAction::ActivateTimecode {
                        media_id: media_id.clone(),
                        seconds: 3_771,
                    })
                    .then_some(*target)
            })
            .expect("second timecode hit target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: target.x,
                    row: target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ActivateTimecode {
                media_id: media_id.clone(),
                seconds: 95,
            })
        );
        assert_eq!(
            terminal.backend().buffer()[(target.x, target.y)].fg,
            Color::Magenta,
            "the active chapter timestamp must use restrained terminal-palette pink"
        );
        assert!(
            terminal.backend().buffer()[(target.x, target.y)]
                .modifier
                .contains(Modifier::UNDERLINED),
            "a clickable timestamp must be visibly distinct"
        );
        assert!(
            terminal.backend().buffer()[(target.x, target.y)]
                .modifier
                .contains(Modifier::BOLD),
            "the active chapter timestamp must be bold"
        );
        let active_title_cell = (
            target.x.saturating_add(target.width).saturating_add(1),
            target.y,
        );
        assert_eq!(
            terminal.backend().buffer()[active_title_cell].fg,
            Color::Magenta,
            "the active chapter title must share the timestamp's pink foreground"
        );
        assert!(
            terminal.backend().buffer()[active_title_cell]
                .modifier
                .contains(Modifier::BOLD),
            "the active chapter description line must be bold"
        );
        assert_eq!(
            terminal.backend().buffer()[(inactive_target.x, inactive_target.y)].fg,
            Color::Cyan,
            "inactive clickable timecodes must retain the normal accent"
        );
        assert!(
            !terminal.backend().buffer()[(inactive_target.x, inactive_target.y)]
                .modifier
                .contains(Modifier::BOLD),
            "inactive chapter timestamps must not inherit the active style"
        );

        let funny_settings = UiSettings {
            funny_mode: true,
            ..UiSettings::default()
        };
        terminal
            .draw(|frame| render(frame, &view, &funny_settings, &mut hit_map))
            .expect("draw active timecode in DOS theme");
        assert_eq!(
            terminal.backend().buffer()[(target.x, target.y)].fg,
            Color::LightMagenta,
            "the active chapter must follow the DOS theme's palette pink"
        );

        view.text_selection_mode = true;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw selection mode");
        assert!(matches!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: target.x,
                    row: target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::BeginDetailsTextSelection(_))
        ));
    }

    #[test]
    fn wrapped_description_keeps_video_action_immediately_after_its_url() {
        let url = "https://youtu.be/dQw4w9WgXcQ";
        let description = format!("prefix {url} suffix");
        let start_byte = description.find(url).expect("fixture URL");
        let links = [DetailVideoLinkView {
            start_byte,
            end_byte: start_byte + url.len(),
            video_id: "dQw4w9WgXcQ".to_owned(),
            start_seconds: None,
        }];

        let wrapped = wrap_description_source(&description, 11, &links);
        let logical = wrapped
            .iter()
            .flat_map(|line| line.tokens.iter())
            .map(|token| match token {
                WrappedDescriptionToken::Source {
                    start_byte,
                    end_byte,
                } => description[*start_byte..*end_byte].to_owned(),
                WrappedDescriptionToken::VideoAction { .. } => {
                    DESCRIPTION_VIDEO_ACTION_SYMBOL.to_owned()
                }
            })
            .collect::<String>();

        assert_eq!(
            terminal_text_width(DESCRIPTION_VIDEO_ACTION_SYMBOL),
            1,
            "the inline action must occupy exactly one terminal cell"
        );
        assert_eq!(logical, format!("prefix {url}↪ suffix"));
        assert!(wrapped.iter().all(|line| {
            line.tokens
                .iter()
                .map(|token| match token {
                    WrappedDescriptionToken::Source {
                        start_byte,
                        end_byte,
                    } => usize::from(terminal_text_width(&description[*start_byte..*end_byte])),
                    WrappedDescriptionToken::VideoAction { .. } => 1,
                })
                .sum::<usize>()
                <= 11
        }));
    }

    #[test]
    fn rendered_video_action_has_an_exact_one_cell_internal_hitbox() {
        let backend = TestBackend::new(92, 26);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let url = "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=90";
        let description =
            format!("A deliberately long prefix makes this URL wrap: {url} then continue.");
        let start_byte = description.find(url).expect("fixture URL");
        let expected = UiAction::ActivateDescriptionVideo {
            video_id: "dQw4w9WgXcQ".to_owned(),
            start_seconds: Some(90),
        };
        let view = ViewModel {
            details: Some(DetailView {
                description,
                video_links: vec![DetailVideoLinkView {
                    start_byte,
                    end_byte: start_byte + url.len(),
                    video_id: "dQw4w9WgXcQ".to_owned(),
                    start_seconds: Some(90),
                }],
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw wrapped internal-video action");

        let [(action, area)] = hit_map.description_video_actions.as_slice() else {
            panic!("expected exactly one internal-video action");
        };
        assert_eq!(action, &expected);
        assert_eq!(area.width, 1);
        assert_eq!(
            terminal.backend().buffer()[(area.x, area.y)].symbol(),
            DESCRIPTION_VIDEO_ACTION_SYMBOL
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: area.x,
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(expected.clone())
        );
        assert_ne!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: area.x.saturating_add(1),
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(expected),
            "the neighboring cell must not activate the video"
        );
    }

    #[test]
    fn details_render_clickable_local_subscription_without_duplicate_channel_name() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Mock video".to_owned(),
                source: "YouTube".to_owned(),
                channel_name: "Fixture channel".to_owned(),
                channel_id: "UCfixture".to_owned(),
                channel_webpage_url: Some(
                    url::Url::parse("https://www.youtube.com/@fixture")
                        .expect("fixture channel URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hit_map))
            .expect("draw unsubscribed details");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(
            !rendered.contains("Channel: Fixture channel"),
            "the selected channel name is already visible in the left panel"
        );
        assert!(
            !rendered.contains("Mock video"),
            "the selected title is already visible in the left panel"
        );
        assert!(
            !rendered.contains("Source:"),
            "the selected source is already visible in the left panel"
        );
        assert!(rendered.contains("[s] Subscribe (locally)"));
        assert!(rendered.contains("[o] xdg-open video"));
        assert!(rendered.contains("[O] xdg-open channel · https://www.youtube.com/@fixture"));
        let (_, subscribe_area) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleSubscription)
            .expect("local subscribe hit target");
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: subscribe_area.x,
            row: subscribe_area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click, &hit_map, &view),
            Some(UiAction::ToggleSubscription)
        );
        let (_, open_area) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::OpenInBrowser)
            .expect("xdg-open video hit target");
        let open_click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: open_area.x,
            row: open_area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(open_click, &hit_map, &view),
            Some(UiAction::OpenInBrowser)
        );
        let (_, open_channel_area) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::OpenChannelInBrowser)
            .expect("xdg-open channel hit target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: open_channel_area.x,
                    row: open_channel_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::OpenChannelInBrowser)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleSubscription)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::OpenChannelInBrowser)
        );

        view.details
            .as_mut()
            .expect("fixture details")
            .channel_subscribed = true;
        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hit_map))
            .expect("draw subscribed details");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("[s] Unsubscribe (locally)"));
    }

    #[test]
    fn history_details_omit_video_only_controls_and_statistics() {
        let backend = TestBackend::new(100, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            screen: Screen::History,
            rows: vec![RowView {
                title: "Local fixture".to_owned(),
                subtitle: "stopped at 0:42".to_owned(),
                source: "local".to_owned(),
                ..RowView::default()
            }],
            details: Some(DetailView {
                title: "Local fixture".to_owned(),
                source: "local".to_owned(),
                description: "stopped at 0:42".to_owned(),
                length: "must not render".to_owned(),
                likes: "must not render".to_owned(),
                views: "must not render".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw History details");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("stopped at 0:42"));
        assert!(!rendered.contains("xdg-open video"));
        assert!(!rendered.contains("Length:"));
        assert!(!rendered.contains("Likes:"));
        assert!(!rendered.contains("Views:"));
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::OpenInBrowser)
        );
    }

    #[test]
    fn details_render_confined_text_selection_control_and_highlight() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Selectable fixture".to_owned(),
                description: "Drag across this text".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw text-selection control");
        assert!(rendered_text(&terminal).contains("[t] Select Details text"));
        let selection_area = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleTextSelectionMode)
            .map(|(_, area)| *area)
            .expect("Details text-selection hit target");
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: selection_area.x,
            row: selection_area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click, &hit_map, &view),
            Some(UiAction::ToggleTextSelectionMode)
        );

        view.text_selection_mode = true;
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw active text-selection mode");
        assert!(rendered_text(&terminal).contains("[t/Esc] End text selection"));
        assert_eq!(
            mouse_action(click, &hit_map, &view),
            Some(UiAction::ToggleTextSelectionMode),
            "the in-app exit button remains clickable"
        );
        let (selectable_x, selectable_y) = hit_map
            .detail_text_rows
            .first()
            .map(|row| (row.x, row.y))
            .expect("selectable title row");
        let anchor = DetailsTextPosition { row: 0, column: 0 };
        view.details_text_selection = Some(DetailsTextSelection {
            anchor,
            focus: DetailsTextPosition { row: 0, column: 5 },
            dragging: true,
        });
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw highlighted selection");
        assert_eq!(
            terminal.backend().buffer()[(selectable_x, selectable_y)].bg,
            Color::Cyan
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Drag(MouseButton::Left),
                    column: selectable_x.saturating_add(4),
                    row: selectable_y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::UpdateDetailsTextSelection(DetailsTextPosition {
                row: 0,
                column: 4
            }))
        );
    }

    #[test]
    fn selected_channel_name_uses_black_foreground_on_selected_background() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            search_kind: SearchKind::Channels,
            rows: vec![RowView {
                title: "Zebra channel".to_owned(),
                subtitle: "42 subscribers".to_owned(),
                source: "YouTube channel".to_owned(),
                ..RowView::default()
            }],
            selected: 0,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw selected channel");

        let title_cell = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .find(|cell| cell.symbol() == "Z")
            .expect("selected channel title cell");
        assert_eq!(title_cell.fg, Color::Black);
        assert_eq!(title_cell.bg, Color::Cyan);
    }

    #[test]
    fn details_subscription_button_obeys_hidden_hotkey_setting() {
        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Mock video".to_owned(),
                channel_name: "Fixture channel".to_owned(),
                channel_id: "UCfixture".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings {
            show_hotkeys: false,
            ..UiSettings::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hit_map))
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("Subscribe (locally)"));
        assert!(!rendered.contains("[s] Subscribe (locally)"));
        assert!(rendered.contains("Select Details text"));
        assert!(!rendered.contains("[t] Select Details text"));
        assert!(rendered.contains("xdg-open video"));
        assert!(!rendered.contains("[o] xdg-open video"));
        assert!(!rendered.contains("[O] xdg-open"));
        assert_eq!(hit_map.detail_buttons.len(), 3);
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::ToggleTextSelectionMode)
        );
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::OpenInBrowser)
        );
    }

    #[test]
    fn details_render_license_row_only_for_recognized_creative_commons_labels() {
        for hidden_license in [
            "",
            "unknown",
            "not applicable",
            "Standard YouTube License",
            "youtube",
        ] {
            let backend = TestBackend::new(120, 28);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let view = ViewModel {
                details: Some(DetailView {
                    title: "Standard-license fixture".to_owned(),
                    source: "YouTube".to_owned(),
                    license: hidden_license.to_owned(),
                    wikidata: "not loaded (lazy)".to_owned(),
                    ..DetailView::default()
                }),
                ..ViewModel::default()
            };
            let mut hit_map = HitMap::default();
            terminal
                .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
                .expect("draw");
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(
                !rendered.contains("License:"),
                "unexpected license row for {hidden_license:?}"
            );
        }

        for (creative_commons_license, expected_display) in [
            (
                "Creative Commons Attribution licence",
                "Creative Commons Attribution",
            ),
            (
                "creative commons attribution LICENSE",
                "Creative Commons Attribution",
            ),
            (
                "https://creativecommons.org/licenses/by/4.0/",
                "https://creativecommons.org/licenses/by/4.0/",
            ),
        ] {
            let backend = TestBackend::new(120, 28);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let view = ViewModel {
                details: Some(DetailView {
                    title: "Creative Commons fixture".to_owned(),
                    source: "YouTube".to_owned(),
                    license: creative_commons_license.to_owned(),
                    wikidata: "not loaded (lazy)".to_owned(),
                    ..DetailView::default()
                }),
                ..ViewModel::default()
            };
            let mut hit_map = HitMap::default();
            terminal
                .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
                .expect("draw");
            let rendered = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(rendered.contains("License:"));
            assert!(rendered.contains(expected_display));
            if creative_commons_license != expected_display {
                assert!(
                    !rendered.contains(creative_commons_license),
                    "localized spelling must not leak into Details"
                );
            }
        }
    }

    #[test]
    fn details_render_wikidata_only_once_as_an_external_link() {
        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Wikidata fixture".to_owned(),
                source: "YouTube".to_owned(),
                wikidata: "no linked Wikidata item found".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw empty result");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(!rendered.contains("Wikidata:"));
        assert!(!rendered.contains("no linked Wikidata item found"));

        let details = view.details.as_mut().expect("details");
        details.wikidata = "Douglas Adams (Q42): https://www.wikidata.org/wiki/Q42".to_owned();
        details.links.push(DetailLinkView {
            label: "Douglas Adams (Q42)".to_owned(),
            url: "https://www.wikidata.org/wiki/Q42".to_owned(),
            wikidata_item_id: Some("Q42".to_owned()),
        });
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw linked result");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(!rendered.contains("Wikidata:"));
        assert!(rendered.contains("Douglas Adams (Q42)"));
        assert_eq!(rendered.matches("Douglas Adams (Q42)").count(), 1);
    }

    #[test]
    fn details_never_render_the_thumbnail_url_as_text() {
        let backend = TestBackend::new(120, 28);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Thumbnail fixture".to_owned(),
                source: "YouTube".to_owned(),
                thumbnail_url: Some(
                    url::Url::parse("https://i.ytimg.com/vi/fixture/maxresdefault.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(!rendered.contains("Thumbnail:"));
        assert!(!rendered.contains("i.ytimg.com"));
        assert!(!rendered.contains("maxresdefault.jpg"));
    }

    #[test]
    fn supported_thumbnail_renderer_receives_only_the_selected_url_and_reserved_area() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let thumbnail_url = url::Url::parse("https://i.ytimg.com/vi/fixture/mqdefault.jpg")
            .expect("fixture thumbnail URL");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Thumbnail fixture".to_owned(),
                source: "YouTube".to_owned(),
                description: "Description remains below the image.".to_owned(),
                thumbnail_url: Some(thumbnail_url.clone()),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("THUMBNAIL IMAGE"));
        assert!(rendered.contains("Description remains below"));
        assert!(!rendered.contains(thumbnail_url.as_str()));
        assert_eq!(thumbnails.synchronized.len(), 1);
        assert_eq!(thumbnails.synchronized[0].0.as_ref(), Some(&thumbnail_url));
        assert_eq!(
            thumbnails.synchronized[0].1.height,
            DEFAULT_THUMBNAIL_HEIGHT
        );
        assert_eq!(thumbnails.clear_count, 0);
    }

    #[test]
    fn thumbnail_prefetch_uses_search_rows_and_cached_subscription_source_artwork() {
        let first = url::Url::parse("https://i.ytimg.com/vi/first/mqdefault.jpg")
            .expect("first thumbnail URL");
        let second = url::Url::parse("https://i.ytimg.com/vi/second/mqdefault.jpg")
            .expect("second thumbnail URL");
        let channel = url::Url::parse("https://yt3.ggpht.com/cached-channel=s800")
            .expect("channel artwork URL");
        let mut view = ViewModel {
            screen: Screen::Search,
            rows: vec![
                RowView {
                    thumbnail_url: Some(first.clone()),
                    ..RowView::default()
                },
                RowView::default(),
                RowView {
                    thumbnail_url: Some(second.clone()),
                    ..RowView::default()
                },
            ],
            ..ViewModel::default()
        };
        let mut renderer = MockThumbnailRenderer::default();

        assert!(synchronize_thumbnail_prefetch(
            &view,
            &UiSettings::default(),
            &mut renderer,
        ));
        assert_eq!(renderer.prefetch_batches, [vec![first, second]]);

        view.screen = Screen::Subscriptions;
        view.subscriptions.sources = vec![
            RowView {
                thumbnail_url: Some(channel.clone()),
                ..RowView::default()
            },
            RowView::default(),
        ];
        assert!(synchronize_thumbnail_prefetch(
            &view,
            &UiSettings::default(),
            &mut renderer,
        ));
        assert_eq!(
            renderer.prefetch_batches.last(),
            Some(&vec![channel.clone()]),
            "persisted channel artwork must warm before another source is selected"
        );

        let disabled = UiSettings {
            prefetch_search_thumbnails: false,
            ..UiSettings::default()
        };
        assert!(synchronize_thumbnail_prefetch(
            &view,
            &disabled,
            &mut renderer,
        ));
        assert_eq!(
            renderer.prefetch_batches.last(),
            Some(&vec![channel]),
            "the Search-only preference must not reintroduce channel-switch latency"
        );

        view.screen = Screen::Search;
        assert!(synchronize_thumbnail_prefetch(
            &view,
            &disabled,
            &mut renderer,
        ));
        assert!(renderer.prefetch_batches.last().is_some_and(Vec::is_empty));
    }

    #[test]
    fn configured_thumbnail_height_controls_the_reserved_area() {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Configured thumbnail fixture".to_owned(),
                source: "YouTube".to_owned(),
                description: "Description remains below the image.".to_owned(),
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/configured.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings {
            thumbnail_height: 7,
            ..UiSettings::default()
        };
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw");

        assert_eq!(thumbnails.synchronized.len(), 1);
        assert_eq!(thumbnails.synchronized[0].1.height, 7);
        assert!(rendered_text(&terminal).contains("Description remains below"));
    }

    #[test]
    fn configured_thumbnail_height_is_bounded_by_the_available_details_area() {
        let backend = TestBackend::new(120, 26);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Bounded thumbnail fixture".to_owned(),
                source: "YouTube".to_owned(),
                description: "Reserved description row.".to_owned(),
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/bounded.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings {
            thumbnail_height: 100,
            ..UiSettings::default()
        };
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw");

        assert_eq!(thumbnails.synchronized.len(), 1);
        let thumbnail_area = thumbnails.synchronized[0].1;
        assert!(thumbnail_area.height < settings.thumbnail_height);
        assert!(
            thumbnail_area.bottom() <= hit_map.details_panel.bottom().saturating_sub(1),
            "thumbnail must leave one description row"
        );
        assert!(rendered_text(&terminal).contains("Reserved description row"));
    }

    #[cfg(feature = "thumbnails")]
    #[test]
    fn repeated_tui_frames_replace_loading_with_the_real_thumbnail_protocol() {
        use std::collections::BTreeSet;
        use std::time::{Duration, Instant};

        use crate::thumbnails::{ThumbnailFailure, ThumbnailState, tests as thumbnail_tests};

        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let thumbnail_url =
            url::Url::parse("https://images.example/fixture.png").expect("fixture thumbnail URL");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Asynchronous thumbnail fixture".to_owned(),
                source: "YouTube".to_owned(),
                description: "Description remains visible after the image loads.".to_owned(),
                thumbnail_url: Some(thumbnail_url.clone()),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();
        let (manager, replies, observed) = thumbnail_tests::manager_with_mock_transport();
        let mut thumbnails = TerminalThumbnailRenderer::new(manager);

        // Match the production loop ordering: poll any completed work, then
        // synchronize and render the selected target during the frame.
        thumbnails.poll();
        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw loading frame");
        assert_eq!(thumbnails.manager.state(), &ThumbnailState::Loading);
        assert!(rendered_text(&terminal).contains("Loading thumbnail…"));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("worker must receive the TUI-selected thumbnail"),
            thumbnail_url
        );

        replies
            .send(Ok(thumbnail_tests::fixture_thumbnail_png()))
            .expect("release successful mock thumbnail");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            thumbnails.poll();
            terminal
                .draw(|frame| {
                    render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
                })
                .expect("draw subsequent thumbnail frame");
            if thumbnails.manager.state() != &ThumbnailState::Loading {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "repeated TUI frames left the completed thumbnail in Loading"
            );
            std::thread::yield_now();
        }

        assert_eq!(thumbnails.manager.state(), &ThumbnailState::Ready);
        let rendered = rendered_text(&terminal);
        assert!(
            !rendered.contains("Loading thumbnail…"),
            "the ready image must replace the loading label"
        );
        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw protocol frame after the clearing transition");
        let rendered = rendered_text(&terminal);
        assert!(
            rendered.contains('\u{10EEEE}') || rendered.contains("\u{1b}_G"),
            "the real ratatui-image protocol must occupy the reserved area"
        );
        let buffer = terminal.backend().buffer();
        let placeholder_rows = buffer
            .content()
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.symbol().contains('\u{10EEEE}'))
            .map(|(index, _)| buffer.pos_of(index).1)
            .collect::<BTreeSet<_>>();
        assert!(
            placeholder_rows.len() > 1,
            "Kitty protocol rows were collapsed by terminal-buffer diffing: {placeholder_rows:?}"
        );
        assert!(rendered.contains("Description remains visible"));

        let failed_url = url::Url::parse("https://images.example/failed.png")
            .expect("failed fixture thumbnail URL");
        view.details
            .as_mut()
            .expect("fixture details")
            .thumbnail_url = Some(failed_url.clone());
        terminal
            .draw(|frame| {
                render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
            })
            .expect("draw replacement loading frame");
        assert_eq!(thumbnails.manager.state(), &ThumbnailState::Loading);
        assert!(rendered_text(&terminal).contains("Loading thumbnail…"));
        assert_eq!(
            observed
                .recv_timeout(Duration::from_secs(1))
                .expect("worker must receive the replacement thumbnail"),
            failed_url
        );
        replies
            .send(Err(ThumbnailFailure::DownloadFailed))
            .expect("release failed mock thumbnail");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            thumbnails.poll();
            terminal
                .draw(|frame| {
                    render_frame(frame, &view, &settings, &mut hit_map, Some(&mut thumbnails));
                })
                .expect("draw failed thumbnail frame");
            if thumbnails.manager.state() != &ThumbnailState::Loading {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "replacement thumbnail remained Loading after a failed response"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            thumbnails.manager.state(),
            &ThumbnailState::Failed(ThumbnailFailure::DownloadFailed)
        );
        let rendered = rendered_text(&terminal);
        assert!(!rendered.contains("Loading thumbnail…"));
        assert!(rendered.contains("Thumbnail unavailable: thumbnail download failed"));
    }

    #[test]
    fn thumbnail_is_not_requested_when_the_details_panel_is_too_short() {
        let backend = TestBackend::new(60, 8);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            details: Some(DetailView {
                title: "Small panel fixture".to_owned(),
                source: "YouTube".to_owned(),
                description: "Text takes priority.".to_owned(),
                thumbnail_url: Some(
                    url::Url::parse("https://images.example/small.jpg")
                        .expect("fixture thumbnail URL"),
                ),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();
        let mut thumbnails = MockThumbnailRenderer {
            enabled: true,
            ..MockThumbnailRenderer::default()
        };

        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &view,
                    &UiSettings::default(),
                    &mut hit_map,
                    Some(&mut thumbnails),
                );
            })
            .expect("draw");

        assert!(thumbnails.synchronized.is_empty());
        assert!(thumbnails.clear_count > 0);
    }

    #[test]
    fn bottom_controls_hide_hotkey_values_without_hiding_action_labels() {
        let backend = TestBackend::new(240, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let settings = UiSettings {
            show_hotkeys: false,
            ..UiSettings::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_buttons(
                    frame,
                    frame.area(),
                    &settings,
                    &Theme::new(false),
                    Screen::Search,
                    YouTubeSearchSort::Relevance,
                    false,
                    true,
                    false,
                    Some(LocalSizeSort::Off),
                    "",
                    false,
                    &mut hit_map,
                );
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("Move up"));
        assert!(rendered.contains("Move down"));
        assert!(rendered.contains("Volume up"));
        assert!(rendered.contains("Volume down"));
        assert!(rendered.contains("Start"));
        assert!(rendered.contains("Sort: relevance"));
        assert!(rendered.contains("Preferences"));
        assert!(rendered.contains("Autoplay: off"));
        for hidden in [
            "[N]", "[C]", "[p]", "[k]", "[j]", "[↑]", "[↓]", "[Enter]", "[T]",
        ] {
            assert!(!rendered.contains(hidden));
        }
        assert_eq!(hit_map.buttons.len(), 19);
    }

    #[test]
    fn one_line_bottom_controls_keep_navigation_click_targets_aligned() {
        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_buttons(
                    frame,
                    frame.area(),
                    &UiSettings::default(),
                    &Theme::new(false),
                    Screen::Search,
                    YouTubeSearchSort::Relevance,
                    false,
                    true,
                    false,
                    Some(LocalSizeSort::Off),
                    "",
                    false,
                    &mut hit_map,
                );
            })
            .expect("draw");

        assert_eq!(hit_map.buttons.len(), 6);
        assert!(rendered_text(&terminal).contains("[T] Time:on"));
        assert!(
            hit_map
                .buttons
                .iter()
                .all(|(_, target)| target.y == terminal.backend().buffer().area.y)
        );
        assert!(hit_map.buttons.iter().all(|(_, target)| {
            target.x >= terminal.backend().buffer().area.x
                && target.right() <= terminal.backend().buffer().area.right()
        }));
        assert!(
            hit_map
                .buttons
                .iter()
                .any(|(action, _)| action == &UiAction::ActivateSelection)
        );
    }

    #[test]
    fn eighty_column_search_footer_keeps_autoplay_visible_and_clickable() {
        let backend = TestBackend::new(80, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let view = ViewModel::default();

        terminal
            .draw(|frame| {
                render_buttons(
                    frame,
                    frame.area(),
                    &UiSettings::default(),
                    &Theme::new(false),
                    Screen::Search,
                    YouTubeSearchSort::Relevance,
                    false,
                    true,
                    false,
                    None,
                    "",
                    false,
                    &mut hit_map,
                );
            })
            .expect("draw compact Search controls");

        let rendered = rendered_text(&terminal);
        for label in [
            "[/] Search",
            "[C] CC:off",
            "[Tab] Next",
            "[S] Subs",
            "[Space] Pause",
            "[A] Autoplay: off",
        ] {
            assert!(
                rendered.contains(label),
                "missing compact Search label {label}"
            );
        }
        for expected in [
            UiAction::BeginSearch,
            UiAction::ToggleYouTubeCreativeCommons,
            UiAction::ShowScreen(Screen::Search.next()),
            UiAction::ShowScreen(Screen::Subscriptions),
            UiAction::TogglePause,
            UiAction::ToggleAutoplay,
        ] {
            assert!(
                hit_map
                    .buttons
                    .iter()
                    .any(|(action, target)| action == &expected && target.width > 0),
                "missing visible compact Search action {expected:?}"
            );
        }
        let (_, target) = hit_map
            .buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleAutoplay)
            .expect("visible Autoplay click target");
        assert!(target.width > 0);
        assert!(target.right() <= terminal.backend().buffer().area.right());
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: target.x,
                    row: target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ToggleAutoplay)
        );
    }

    #[test]
    fn uppercase_a_toggles_autoplay_without_replacing_lowercase_queue_action() {
        let view = ViewModel::default();

        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleAutoplay)
        );
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT),
                &view
            ),
            Some(UiAction::ToggleAutoplay)
        );
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &view),
            Some(UiAction::AddToQueue)
        );
    }

    #[test]
    fn local_size_sort_is_three_state_clickable_and_hidden_when_sizes_are_disabled() {
        let backend = TestBackend::new(240, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let view = ViewModel {
            screen: Screen::Local,
            local_size_sort: LocalSizeSort::Descending,
            local_folder_sizes_enabled: true,
            ..ViewModel::default()
        };

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Local controls");
        assert!(rendered_text(&terminal).contains("[Z] Size sort: descending"));
        assert_eq!(
            key_action(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::NONE), &view),
            Some(UiAction::ToggleLocalSizeSort)
        );
        let (_, target) = hit_map
            .buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleLocalSizeSort)
            .expect("size-sort click target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: target.x,
                    row: target.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ToggleLocalSizeSort)
        );
        assert_eq!(LocalSizeSort::Off.next(), LocalSizeSort::Ascending);
        assert_eq!(LocalSizeSort::Ascending.next(), LocalSizeSort::Descending);
        assert_eq!(LocalSizeSort::Descending.next(), LocalSizeSort::Off);

        let disabled = ViewModel {
            local_folder_sizes_enabled: false,
            ..view
        };
        let mut disabled_hit_map = HitMap::default();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    &disabled,
                    &UiSettings::default(),
                    &mut disabled_hit_map,
                );
            })
            .expect("draw disabled Local controls");
        assert!(!rendered_text(&terminal).contains("Size sort:"));
        assert_eq!(
            key_action(
                KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::NONE),
                &disabled,
            ),
            None
        );
        assert!(
            disabled_hit_map
                .buttons
                .iter()
                .all(|(action, _)| action != &UiAction::ToggleLocalSizeSort)
        );
    }

    #[cfg(feature = "youtube-music")]
    #[test]
    fn youtube_music_bottom_controls_omit_youtube_video_filters() {
        let backend = TestBackend::new(240, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| {
                render_buttons(
                    frame,
                    frame.area(),
                    &UiSettings::default(),
                    &Theme::new(false),
                    Screen::YouTubeMusic,
                    YouTubeSearchSort::Newest,
                    true,
                    true,
                    false,
                    Some(LocalSizeSort::Off),
                    "",
                    false,
                    &mut hit_map,
                );
            })
            .expect("draw Music controls");
        let rendered = rendered_text(&terminal);

        assert!(!rendered.contains("CC only"));
        assert!(!rendered.contains("Sort:"));
        assert!(!rendered.contains("Videos/channels"));
        assert!(rendered.contains("[/] Search"));
        assert!(rendered.contains("[d] Download"));
    }

    #[test]
    fn seek_bar_has_no_separator_border_above_its_status() {
        let backend = TestBackend::new(80, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel::default();
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("0:00 / --:--"));
        assert!(!rendered.contains("────────"));
        assert_eq!(hit_map.seek_bar, Rect::new(0, 0, 80, 1));
        let status_row = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect::<String>();
        assert!(
            status_row.contains("0:00 / --:--"),
            "playback status must render below, rather than over, the seek track"
        );
    }

    #[test]
    fn chapter_label_height_adapts_to_density_mode_and_terminal_space() {
        let chapters = (0..29)
            .map(|index| Chapter {
                title: format!("Chapter {index}"),
                start_seconds: index * 10,
                end_seconds: None,
            })
            .collect();
        let mut view = ViewModel {
            playback: PlaybackStatus {
                duration: Some(Duration::from_secs(1_000)),
                ..PlaybackStatus::default()
            },
            playback_chapters: chapters,
            ..ViewModel::default()
        };

        assert_eq!(chapter_label_row_count(&view, 240, 32, false), 4);
        assert_eq!(chapter_label_row_count(&view, 80, 32, false), 4);
        assert_eq!(chapter_label_row_count(&view, 240, 15, false), 1);
        assert_eq!(chapter_label_row_count(&view, 240, 14, false), 0);
        assert_eq!(chapter_label_row_count(&view, 240, 16, true), 0);
        view.show_chapter_timestamps = false;
        assert_eq!(chapter_label_row_count(&view, 240, 32, false), 4);
        view.playback.duration = None;
        assert_eq!(chapter_label_row_count(&view, 240, 32, false), 1);
        view.playback_chapters.clear();
        assert_eq!(chapter_label_row_count(&view, 240, 32, false), 0);
    }

    #[test]
    fn chapter_label_sizing_uses_fractional_duration_like_rendering() {
        let view = ViewModel {
            playback: PlaybackStatus {
                duration: Some(Duration::from_millis(31_900)),
                ..PlaybackStatus::default()
            },
            playback_chapters: vec![
                Chapter {
                    title: "First boundary chapter title".to_owned(),
                    start_seconds: 14,
                    end_seconds: None,
                },
                Chapter {
                    title: "Second boundary chapter title".to_owned(),
                    start_seconds: 22,
                    end_seconds: None,
                },
            ],
            ..ViewModel::default()
        };

        assert_eq!(
            chapter_label_row_count(&view, 80, 32, false),
            2,
            "the sizing pass must preserve the renderer's fractional column geometry"
        );
    }

    #[test]
    fn inactive_unicode_chapter_label_borrows_free_lane_space_before_activation() {
        let target_title = "Возникновение “Скелы” и криминализация украинской армии";
        let mut view = ViewModel {
            playback: PlaybackStatus {
                position: Duration::ZERO,
                duration: Some(Duration::from_secs(1_000)),
                ..PlaybackStatus::default()
            },
            playback_chapters: vec![
                Chapter {
                    title: "Вступление".to_owned(),
                    start_seconds: 0,
                    end_seconds: Some(400),
                },
                Chapter {
                    title: target_title.to_owned(),
                    start_seconds: 400,
                    end_seconds: Some(900),
                },
                Chapter {
                    title: "Заключение".to_owned(),
                    start_seconds: 900,
                    end_seconds: Some(1_000),
                },
            ],
            ..ViewModel::default()
        };

        let inactive_layout = chapter_label_layout(&view, Duration::from_secs(1_000), 100, 1);
        let inactive = inactive_layout
            .placements
            .iter()
            .find(|placement| placement.index == 1)
            .expect("inactive Unicode chapter placement");
        let following = inactive_layout
            .placements
            .iter()
            .filter(|placement| placement.row == inactive.row && placement.start > inactive.start)
            .min_by_key(|placement| placement.start)
            .expect("following occupied interval");

        assert!(
            inactive.width > 20,
            "an inactive label must not retain the conservative placement width"
        );
        assert_eq!(
            inactive.start + inactive.width,
            following.start,
            "the long inactive label should use every free cell before its neighbour"
        );
        assert!(
            inactive.text.contains("Возникновение “Скелы”"),
            "Unicode text beyond the former 20-cell cap must remain visible: {}",
            inactive.text
        );

        view.playback.position = Duration::from_secs(400);
        let active_layout = chapter_label_layout(&view, Duration::from_secs(1_000), 100, 1);
        let active = active_layout
            .placements
            .iter()
            .find(|placement| placement.index == 1)
            .expect("active Unicode chapter placement");
        let active_following = active_layout
            .placements
            .iter()
            .filter(|placement| placement.row == active.row && placement.start > active.start)
            .min_by_key(|placement| placement.start)
            .expect("occupied interval after active chapter");

        assert_eq!(inactive.start, active.start);
        assert!(
            active.start.saturating_add(active.width) <= active_following.start,
            "the priority active label must not overlap its following interval"
        );
        assert!(active.text.starts_with("▶ 06:40 Возникновение"));
    }

    #[test]
    fn seek_bar_chapter_markers_and_labels_seek_to_exact_timecodes() {
        let backend = TestBackend::new(80, 4);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
        let view = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                position: Duration::from_secs(35),
                duration: Some(Duration::from_secs(100)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(media_id.clone()),
            playback_chapters: vec![
                Chapter {
                    title: "Intro".to_owned(),
                    start_seconds: 0,
                    end_seconds: Some(30),
                },
                Chapter {
                    title: "Middle".to_owned(),
                    start_seconds: 30,
                    end_seconds: Some(60),
                },
                Chapter {
                    title: "Finish".to_owned(),
                    start_seconds: 60,
                    end_seconds: Some(100),
                },
            ],
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw chapter timeline");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("▶ 00:30 Middle"), "{rendered}");
        assert!(
            rendered.contains('┃'),
            "current chapter needs a strong split"
        );
        assert!(
            rendered.matches('│').count() >= 2,
            "all noncurrent chapter splits must remain visible"
        );

        let (_, marker) = hit_map
            .seek_markers
            .iter()
            .find(|(action, area)| {
                area.y == hit_map.seek_bar.y
                    && action
                        == &UiAction::ActivateTimecode {
                            media_id: media_id.clone(),
                            seconds: 60,
                        }
            })
            .expect("exact chapter marker");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: marker.x,
                    row: marker.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ActivateTimecode {
                media_id,
                seconds: 60,
            }),
            "an exact split must win over the generic percentage seek target"
        );
    }

    #[test]
    fn list_prefixed_description_timecodes_render_on_the_seek_bar() {
        let description = "\
➤ 00:00 Introduction
➤ 05:45 Дело не в фамилиях, а в системе
prose 07:25 remains clickable but is not a chapter";
        let chapters = crate::links::parse_description_chapters(description, Some(600))
            .into_iter()
            .map(|chapter| Chapter {
                title: chapter.title,
                start_seconds: chapter.start_seconds,
                end_seconds: chapter.end_seconds,
            })
            .collect();
        let backend = TestBackend::new(100, 4);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                position: Duration::from_secs(350),
                duration: Some(Duration::from_mins(10)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(MediaId::new(SourceKind::YouTube, "abcdefghijk")),
            playback_chapters: chapters,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw chapters parsed from a real description");

        let rendered = rendered_text(&terminal);
        assert!(
            rendered.contains("▶ 05:45 Дело не в фамилиях"),
            "the active description chapter must be visible above the seek bar: {rendered}"
        );
        assert_eq!(
            hit_map
                .seek_markers
                .iter()
                .filter(|(action, area)| {
                    area.y == hit_map.seek_bar.y
                        && matches!(action, UiAction::ActivateTimecode { .. })
                })
                .count(),
            2,
            "only the two list-prefixed description timecodes must become exact seek markers"
        );
    }

    #[test]
    fn seek_bar_omits_exact_advertisement_chapters_when_enabled() {
        let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
        let backend = TestBackend::new(120, 5);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            playback: PlaybackStatus {
                position: Duration::from_secs(10),
                duration: Some(Duration::from_secs(90)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(media_id),
            playback_chapters: vec![
                Chapter {
                    title: "Introduction".to_owned(),
                    start_seconds: 0,
                    end_seconds: Some(30),
                },
                Chapter {
                    title: "Реклама".to_owned(),
                    start_seconds: 30,
                    end_seconds: Some(45),
                },
                Chapter {
                    title: "Main section".to_owned(),
                    start_seconds: 45,
                    end_seconds: None,
                },
            ],
            skip_advertisement_chapters: true,
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw advertisement-filtered seek bar");

        assert!(!rendered_text(&terminal).contains("Реклама"));
        assert_eq!(
            hit_map
                .seek_markers
                .iter()
                .filter_map(|(action, _)| match action {
                    UiAction::ActivateTimecode { seconds, .. } => Some(*seconds),
                    _ => None,
                })
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([0, 45])
        );
    }

    #[test]
    fn chapter_labels_use_two_rows_before_hiding_collisions() {
        let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
        let view = ViewModel {
            playback: PlaybackStatus {
                position: Duration::from_secs(10),
                duration: Some(Duration::from_secs(100)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(media_id),
            playback_chapters: vec![
                Chapter {
                    title: "First nearby chapter".to_owned(),
                    start_seconds: 10,
                    end_seconds: Some(11),
                },
                Chapter {
                    title: "Second nearby chapter".to_owned(),
                    start_seconds: 11,
                    end_seconds: Some(100),
                },
            ],
            ..ViewModel::default()
        };
        let backend = TestBackend::new(80, 4);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw two-line chapter labels");

        let mut label_rows = hit_map
            .seek_markers
            .iter()
            .filter_map(|(_, area)| (area.y < hit_map.seek_bar.y).then_some(area.y))
            .collect::<Vec<_>>();
        label_rows.sort_unstable();
        label_rows.dedup();
        assert_eq!(
            label_rows,
            [0, 1],
            "overlapping chapter labels should use both rows before one is hidden"
        );
        assert_eq!(hit_map.seek_bar, Rect::new(0, 2, 80, 1));
        let status_row = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 3)].symbol())
            .collect::<String>();
        assert!(status_row.contains("0:10 / 1:40"));
    }

    #[test]
    fn dense_chapter_labels_use_four_clickable_rows_when_space_allows() {
        let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
        let view = ViewModel {
            playback: PlaybackStatus {
                position: Duration::from_secs(17),
                duration: Some(Duration::from_secs(100)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(media_id),
            playback_chapters: (10..18)
                .map(|seconds| Chapter {
                    title: format!("Clustered chapter {seconds}"),
                    start_seconds: seconds,
                    end_seconds: None,
                })
                .collect(),
            ..ViewModel::default()
        };
        let backend = TestBackend::new(80, 6);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw adaptive chapter labels");

        let mut label_rows = hit_map
            .seek_markers
            .iter()
            .filter_map(|(_, area)| (area.y < hit_map.seek_bar.y).then_some(area.y))
            .collect::<Vec<_>>();
        label_rows.sort_unstable();
        label_rows.dedup();
        assert_eq!(label_rows, [0, 1, 2, 3]);
        assert_eq!(hit_map.seek_bar, Rect::new(0, 4, 80, 1));
        let status_row = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 5)].symbol())
            .collect::<String>();
        assert!(status_row.contains("0:17 / 1:40"));
    }

    #[test]
    fn chapter_timestamp_toggle_changes_labels_but_not_exact_seek_targets() {
        let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
        let mut view = ViewModel {
            playback: PlaybackStatus {
                duration: Some(Duration::from_secs(100)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(media_id.clone()),
            playback_chapters: vec![Chapter {
                title: "Introduction".to_owned(),
                start_seconds: 10,
                end_seconds: Some(100),
            }],
            ..ViewModel::default()
        };
        let backend = TestBackend::new(80, 4);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw timestamped chapter");
        assert!(rendered_text(&terminal).contains("00:10 Introduction"));

        view.show_chapter_timestamps = false;
        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw names-only chapter");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Introduction"));
        assert!(!rendered.contains("00:10 Introduction"));
        assert!(hit_map.seek_markers.iter().any(|(action, _)| {
            action
                == &UiAction::ActivateTimecode {
                    media_id: media_id.clone(),
                    seconds: 10,
                }
        }));
    }

    #[test]
    fn chapter_labels_hide_one_final_period_but_preserve_ellipses() {
        let view = ViewModel {
            playback: PlaybackStatus {
                position: Duration::from_secs(10),
                duration: Some(Duration::from_secs(100)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(MediaId::new(SourceKind::YouTube, "abcdefghijk")),
            playback_chapters: vec![
                Chapter {
                    title: "Sentence title.".to_owned(),
                    start_seconds: 10,
                    end_seconds: Some(70),
                },
                Chapter {
                    title: "Wait...".to_owned(),
                    start_seconds: 70,
                    end_seconds: Some(100),
                },
            ],
            ..ViewModel::default()
        };
        let backend = TestBackend::new(100, 4);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw normalized chapter labels");

        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Sentence title"));
        assert!(!rendered.contains("Sentence title."));
        assert!(rendered.contains("Wait..."));
    }

    #[test]
    fn short_chapter_seekbar_keeps_markers_and_places_exact_status_below_track() {
        let media_id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
        let view = ViewModel {
            playback: PlaybackStatus {
                position: Duration::from_secs(18 * 60 + 28),
                duration: Some(Duration::from_secs(60 * 60 + 33 * 60 + 6)),
                paused: true,
                volume: 80,
                speed: 1.0,
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(media_id.clone()),
            playback_chapters: vec![
                Chapter {
                    title: "Introduction".to_owned(),
                    start_seconds: 0,
                    end_seconds: Some(16 * 60),
                },
                Chapter {
                    title: "Current chapter".to_owned(),
                    start_seconds: 16 * 60,
                    end_seconds: None,
                },
            ],
            ..ViewModel::default()
        };
        let backend = TestBackend::new(120, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw short chapter seek bar");

        assert_eq!(hit_map.seek_bar, Rect::new(0, 0, 120, 1));
        assert!(
            hit_map.seek_markers.iter().any(|(action, area)| {
                area.y == hit_map.seek_bar.y
                    && action
                        == &UiAction::ActivateTimecode {
                            media_id: media_id.clone(),
                            seconds: 16 * 60,
                        }
            }),
            "short terminals must retain exact chapter split click targets"
        );
        let track_row = (0..120)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        let status_row = (0..120)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect::<String>();
        let expected = "18:28 / 1:33:06  1×  vol 80% paused";
        assert!(!track_row.contains(expected));
        assert!(status_row.contains(expected));
    }

    #[test]
    fn colliding_chapter_splits_remain_visible_without_inventing_unknown_positions() {
        let chapters = vec![
            Chapter {
                title: "First".to_owned(),
                start_seconds: 10,
                end_seconds: Some(11),
            },
            Chapter {
                title: "Second".to_owned(),
                start_seconds: 11,
                end_seconds: None,
            },
        ];
        let backend = TestBackend::new(8, 3);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let mut view = ViewModel {
            playback: PlaybackStatus {
                position: Duration::from_secs(11),
                duration: Some(Duration::from_secs(100)),
                ..PlaybackStatus::default()
            },
            playing_media_id: Some(MediaId::new(SourceKind::YouTube, "abcdefghijk")),
            playback_chapters: chapters.clone(),
            ..ViewModel::default()
        };

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw colliding markers");
        assert!(rendered_text(&terminal).contains('┇'));

        view.playback.duration = None;
        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &view,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw unknown-duration chapter");
        let rendered = rendered_text(&terminal);
        assert!(
            rendered.contains("▶ 0:11"),
            "a narrow unknown-duration timeline must retain the active timestamp: {rendered}"
        );
        assert!(
            rendered.contains('…'),
            "the active chapter title must be truncated explicitly: {rendered}"
        );
        assert!(hit_map.seek_markers.is_empty());
    }

    #[test]
    fn seek_bar_layers_discontinuous_cached_ranges_behind_progress_for_both_styles() {
        for seek_bar_style in [SeekBarStyle::Line, SeekBarStyle::NyanCat] {
            let backend = TestBackend::new(80, 2);
            let mut terminal = Terminal::new(backend).expect("terminal");
            let view = ViewModel {
                playback: PlaybackStatus {
                    idle: false,
                    position: Duration::from_secs(10),
                    duration: Some(Duration::from_secs(100)),
                    buffered_ranges: vec![
                        crate::playback::BufferedRange {
                            start: Duration::ZERO,
                            end: Duration::from_secs(15),
                        },
                        crate::playback::BufferedRange {
                            start: Duration::from_secs(30),
                            end: Duration::from_secs(40),
                        },
                        crate::playback::BufferedRange {
                            start: Duration::from_secs(60),
                            end: Duration::from_secs(80),
                        },
                        crate::playback::BufferedRange {
                            start: Duration::from_secs(90),
                            end: Duration::from_secs(120),
                        },
                    ],
                    ..PlaybackStatus::default()
                },
                ..ViewModel::default()
            };
            let settings = UiSettings {
                seek_bar_style,
                ..UiSettings::default()
            };
            let mut hit_map = HitMap::default();

            terminal
                .draw(|frame| {
                    render_seek_bar(
                        frame,
                        frame.area(),
                        &view,
                        &settings,
                        &Theme::new(false),
                        &mut hit_map,
                    );
                })
                .expect("draw");
            let buffer = terminal.backend().buffer();

            assert_eq!(buffer[(4, 0)].fg, Color::Cyan);
            assert_eq!(
                buffer[(4, 0)].symbol(),
                ratatui::symbols::block::FULL,
                "played progress must cover an overlapping cached range"
            );
            for x in [24, 31, 48, 63, 72, 79] {
                assert_eq!(buffer[(x, 0)].fg, Color::DarkGray);
                assert_eq!(buffer[(x, 0)].symbol(), ratatui::symbols::block::FULL);
            }
            for x in [20, 40, 68] {
                assert_eq!(
                    buffer[(x, 0)].symbol(),
                    " ",
                    "gaps between cached intervals must remain visible"
                );
            }

            let status_row = (0..80).map(|x| buffer[(x, 1)].symbol()).collect::<String>();
            assert!(
                status_row.contains("0:10 / 1:40"),
                "the status must remain readable below cached seek ranges"
            );
            assert_ne!(
                buffer[(24, 1)].symbol(),
                ratatui::symbols::block::FULL,
                "cached seek styling must not bleed into the status row"
            );
            let rendered = buffer
                .content()
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            if seek_bar_style == SeekBarStyle::NyanCat {
                assert!(rendered.contains("=^.^="));
            }
        }
    }

    #[test]
    fn playing_status_has_an_exact_click_target_only_while_playback_is_active() {
        let backend = TestBackend::new(220, 2);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut hit_map = HitMap::default();
        let active = ViewModel {
            playback: PlaybackStatus {
                idle: false,
                duration: Some(Duration::from_secs(120)),
                title: Some("Fixture title".to_owned()),
                volume: 80,
                speed: 1.0,
                ..PlaybackStatus::default()
            },
            ..ViewModel::default()
        };

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &active,
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw");

        let target = hit_map.now_playing.expect("now-playing click target");
        assert_eq!(target.height, 1);
        assert_eq!(target.width, "Fixture title".chars().count() as u16);
        let rendered_status = (target.x..target.right())
            .map(|x| terminal.backend().buffer()[(x, target.y)].symbol())
            .collect::<String>();
        assert_eq!(rendered_status, "Fixture title");

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: target.x,
            row: target.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click, &hit_map, &active),
            Some(UiAction::ShowNowPlaying)
        );

        terminal
            .draw(|frame| {
                render_seek_bar(
                    frame,
                    frame.area(),
                    &ViewModel::default(),
                    &UiSettings::default(),
                    &Theme::new(false),
                    &mut hit_map,
                );
            })
            .expect("draw inactive state");
        assert!(hit_map.now_playing.is_none());
    }

    #[test]
    fn render_shows_known_and_unknown_download_progress_without_hiding_seekbar() {
        let backend = TestBackend::new(160, 34);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            download: Some(DownloadView {
                title: "Fixture video".to_owned(),
                downloaded_bytes: 2048,
                total_bytes: Some(4096),
                bytes_per_second: Some(512),
                eta_seconds: Some(4),
                active: true,
                completed_path: None,
            }),
            ..ViewModel::default()
        };
        view.playback.duration = Some(Duration::from_secs(120));
        view.playback.position = Duration::from_secs(30);
        let settings = UiSettings::default();
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hit_map))
            .expect("draw known progress");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("Downloading Fixture video"));
        assert!(rendered.contains("50.0%"));
        assert!(rendered.contains("2.0 KiB / 4.0 KiB"));
        assert!(rendered.contains("0:30 / 2:00"));

        view.download = Some(DownloadView {
            title: "Unknown-size stream".to_owned(),
            downloaded_bytes: 1024,
            active: true,
            ..DownloadView::default()
        });
        terminal
            .draw(|frame| render(frame, &view, &settings, &mut hit_map))
            .expect("draw unknown progress");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("Unknown-size stream"));
        assert!(rendered.contains("1.0 KiB · size unknown"));
    }

    #[test]
    fn download_ratio_and_binary_units_are_bounded_without_float_casts() {
        assert!((download_ratio(2, 4) - 0.5).abs() < f64::EPSILON);
        assert!((download_ratio(5, 4) - 1.0).abs() < f64::EPSILON);
        assert!(download_ratio(1, 0).abs() < f64::EPSILON);
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn render_keeps_the_confined_completed_download_path_visible() {
        let backend = TestBackend::new(140, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            download: Some(DownloadView {
                title: "Fixture".to_owned(),
                downloaded_bytes: 4096,
                total_bytes: Some(4096),
                active: false,
                completed_path: Some(
                    "/home/listener/.config/youta/downloads/fixture.opus".to_owned(),
                ),
                ..DownloadView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw completed path");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("Downloaded:"));
        assert!(rendered.contains("downloads/fixture.opus"));
    }

    #[test]
    fn diagnostic_popup_renders_scrolled_report_position_and_github_buttons() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut report_lines = vec!["FIRST_MARKER".to_owned()];
        report_lines.extend((1..=60).map(|line| format!("filler diagnostic line {line:02}")));
        report_lines.push("SCROLLED_MARKER".to_owned());
        let view = ViewModel {
            error_popup: Some(ErrorPopupView {
                title: "Playback failed".to_owned(),
                report: report_lines.join("\n"),
                scroll_offset: 10,
                gh_available: true,
                action_status: None,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render(frame, &view, &UiSettings::default(), &mut hit_map);
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("Playback failed"));
        assert!(!rendered.contains("FIRST_MARKER"));
        assert!(rendered.contains("filler diagnostic line 10"));
        assert!(rendered.contains("Lines 11–32 of 62"), "{rendered}");
        let scrollbar_thumb_cells = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.symbol() == "█")
            .count();
        assert!(
            scrollbar_thumb_cells > 0,
            "a scrollable report must render a visible thumb"
        );
        assert!(rendered.contains("[c] Copy"));
        assert!(rendered.contains("[i] Copy + open issue"));
        assert!(rendered.contains("[g] Fill GitHub issue"));
        assert!(rendered.contains("[Esc] Close"));
        assert_eq!(hit_map.error_buttons.len(), 4);
    }

    #[test]
    fn diagnostic_scrollbar_tracks_the_clamped_final_viewport() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let report = (1..=62)
            .map(|line| format!("diagnostic line {line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let view = ViewModel {
            error_popup: Some(ErrorPopupView {
                title: "Playback failed".to_owned(),
                report,
                scroll_offset: usize::MAX,
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw final report viewport");
        let buffer = terminal.backend().buffer();
        let rendered = buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(rendered.contains("Lines 41–62 of 62"), "{rendered}");

        let width = usize::from(buffer.area().width);
        let thumb_positions = buffer
            .content()
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.symbol() == "█")
            .map(|(index, _)| (index % width, index / width))
            .collect::<Vec<_>>();
        let &(scrollbar_x, _) = thumb_positions.first().expect("scrollbar thumb");
        let thumb_top = thumb_positions
            .iter()
            .map(|(_, y)| *y)
            .min()
            .expect("scrollbar thumb top");
        let track_top = buffer
            .content()
            .iter()
            .enumerate()
            .filter(|(index, cell)| {
                index % width == scrollbar_x && matches!(cell.symbol(), "█" | "│")
            })
            .map(|(index, _)| index / width)
            .min()
            .expect("scrollbar track top");
        assert!(
            thumb_top > track_top,
            "the clamped final viewport must move the thumb below the start"
        );
    }

    #[test]
    fn diagnostic_popup_renders_copy_and_open_fallback_button() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            error_popup: Some(ErrorPopupView {
                title: "Network error".to_owned(),
                report: "full report".to_owned(),
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render(frame, &view, &UiSettings::default(), &mut hit_map);
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("[i] Copy + open issue"));
        assert!(!rendered.contains("[g] Fill GitHub issue"));
    }

    #[test]
    fn youtube_setup_popup_masks_secret_and_renders_storage_notice_and_controls() {
        let backend = TestBackend::new(140, 34);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let secret = "AIzaActualSecretNeverRender";
        let view = ViewModel {
            youtube_setup_popup: Some(YouTubeSetupPopupView {
                selected_field: YouTubeSetupField::ApiKey,
                api_key: secret.to_owned(),
                invidious_url: "https://invidious.example".to_owned(),
                config_path: "/home/listener/.config/youta/config.toml".to_owned(),
                validation_error: Some("API key was rejected".to_owned()),
            }),
            ..ViewModel::default()
        };
        let debug_view = format!("{view:?}");
        assert!(!debug_view.contains(secret));
        assert!(debug_view.contains("[REDACTED]"));
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render(frame, &view, &UiSettings::default(), &mut hit_map);
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        let printable = rendered
            .chars()
            .map(
                |character| {
                    if character.is_ascii() { character } else { ' ' }
                },
            )
            .collect::<String>();
        let normalized = printable.split_whitespace().collect::<Vec<_>>().join(" ");

        assert!(rendered.contains("Configure YouTube metadata"));
        assert!(rendered.contains("YouTube API key (masked)"));
        assert!(!rendered.contains(secret));
        assert!(rendered.contains(&"*".repeat(secret.len())));
        assert!(rendered.contains("https://invidious.example"));
        assert!(normalized.contains("create/select a Google Cloud project"));
        assert!(normalized.contains("enable YouTube Data API v3"));
        assert!(normalized.contains("Create credentials > API key"));
        assert!(normalized.contains("API restrictions > Restrict key"));
        assert!(normalized.contains("Restriction blocks other Google APIs"));
        assert!(normalized.contains("[F1] Google guide"));
        assert!(normalized.contains(YOUTUBE_API_KEY_GUIDE_URL.trim_start_matches("https://")));
        assert!(normalized.contains("[F2] Google Cloud"));
        assert!(normalized.contains(GOOGLE_CLOUD_CREDENTIALS_URL.trim_start_matches("https://")));
        assert!(normalized.contains("choose a public instance from the official list"));
        assert!(normalized.contains("[F3] Instance list"));
        assert!(normalized.contains(INVIDIOUS_INSTANCES_URL.trim_start_matches("https://")));
        assert!(normalized.contains("/home/listener/.config/youta/config.toml"));
        assert!(normalized.contains("Will save to:"));
        assert!(normalized.contains("API keys are plaintext"));
        assert!(normalized.contains("directory 0700, file 0600"));
        assert!(normalized.contains("Environment variables override"));
        assert!(normalized.contains("Error: API key was rejected"));
        assert!(normalized.contains("[Enter] Save and retry"));
        assert!(normalized.contains("[Esc] Cancel"));
        assert_eq!(hit_map.youtube_setup_fields.len(), 2);
        assert_eq!(hit_map.youtube_setup_buttons.len(), 5);
    }

    #[test]
    fn youtube_setup_popup_keeps_provider_instructions_on_an_80_by_24_terminal() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            youtube_setup_popup: Some(YouTubeSetupPopupView {
                config_path: "/home/listener/.config/youta/config.toml".to_owned(),
                ..YouTubeSetupPopupView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render(frame, &view, &UiSettings::default(), &mut hit_map);
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        let printable = rendered
            .chars()
            .map(
                |character| {
                    if character.is_ascii() { character } else { ' ' }
                },
            )
            .collect::<String>();
        let normalized = printable.split_whitespace().collect::<Vec<_>>().join(" ");

        for expected in [
            "create/select a Google Cloud project",
            "enable YouTube Data API v3",
            "Create credentials > API key",
            "API restrictions > Restrict key > YouTube Data API v3 > Save",
            "Restriction blocks other Google APIs",
            "[F1] Google guide",
            "developers.google.com/youtube/registering_an_application",
            "[F2] Google Cloud",
            "console.cloud.google.com/apis/credentials",
            "choose a public instance from the official list",
            "[F3] Instance list",
            "docs.invidious.io/instances/",
            "Will save to: /home/listener/.config/youta/config.toml",
            "API keys are plaintext",
            "[Enter] Save and retry",
            "[Esc] Cancel",
        ] {
            assert!(
                normalized.contains(expected),
                "80x24 popup omitted `{expected}`:\n{normalized}"
            );
        }
        assert_eq!(hit_map.youtube_setup_buttons.len(), 5);
    }

    #[test]
    fn render_shows_selectable_external_links_and_records_hit_areas() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let mut view = ViewModel {
            details: Some(DetailView {
                title: "Mock video".to_owned(),
                description: "Mock description".to_owned(),
                wikidata: "loaded".to_owned(),
                links: vec![DetailLinkView {
                    label: "Douglas Adams (Q42)".to_owned(),
                    url: "https://www.wikidata.org/wiki/Q42".to_owned(),
                    wikidata_item_id: Some("Q42".to_owned()),
                }],
                ..DetailView::default()
            }),
            selected_detail_link: Some(0),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render(frame, &view, &UiSettings::default(), &mut hit_map);
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("External links"));
        assert!(rendered.contains("Douglas Adams (Q42)"));
        assert!(rendered.contains("[W]"));
        assert!(!rendered.contains("instance of (P31)"));
        assert_eq!(hit_map.detail_links.len(), 1);
        assert_eq!(hit_map.detail_links[0].0, 0);
        let (_, disclosure_area) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::ToggleWikidataStatements(0))
            .expect("Wikidata disclosure hit target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: disclosure_area.x,
                    row: disclosure_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::ToggleWikidataStatements(0))
        );

        let details = view.details.as_mut().expect("fixture details");
        details.expanded_wikidata_item = Some("Q42".to_owned());
        let text = "Wikidata properties for Q42\ninstance of (P31): human (Q5)".to_owned();
        let start_byte = text.find("human (Q5)").expect("linked value");
        details.wikidata_entities.push(DetailWikidataEntityView {
            item_id: "Q42".to_owned(),
            text,
            value_links: vec![DetailWikidataValueLinkView {
                start_byte,
                end_byte: start_byte + "human (Q5)".len(),
                item_id: "Q5".to_owned(),
            }],
        });
        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw expanded Wikidata properties");
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("instance of (P31): human (Q5)"));
        let (_, value_area) = hit_map
            .detail_buttons
            .iter()
            .find(|(action, _)| action == &UiAction::OpenWikidataItem("Q5".to_owned()))
            .expect("Wikidata value hit target");
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: value_area.x,
                    row: value_area.y,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::OpenWikidataItem("Q5".to_owned()))
        );
    }

    #[test]
    fn wrapped_unicode_wikidata_value_keeps_every_fragment_clickable() {
        let backend = TestBackend::new(56, 36);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let value = "Україна з довгою тестовою назвою (Q212)";
        let text = format!("Wikidata properties for Q61113\ncountry of citizenship (P27): {value}");
        let start_byte = text.find(value).expect("linked Unicode value");
        let view = ViewModel {
            details: Some(DetailView {
                links: vec![DetailLinkView {
                    label: "Fixture creator (Q61113)".to_owned(),
                    url: "https://www.wikidata.org/wiki/Q61113".to_owned(),
                    wikidata_item_id: Some("Q61113".to_owned()),
                }],
                expanded_wikidata_item: Some("Q61113".to_owned()),
                wikidata_entities: vec![DetailWikidataEntityView {
                    item_id: "Q61113".to_owned(),
                    text,
                    value_links: vec![DetailWikidataValueLinkView {
                        start_byte,
                        end_byte: start_byte + value.len(),
                        item_id: "Q212".to_owned(),
                    }],
                }],
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw wrapped Wikidata value");

        let value_areas = hit_map
            .detail_buttons
            .iter()
            .filter_map(|(action, area)| {
                (action == &UiAction::OpenWikidataItem("Q212".to_owned())).then_some(*area)
            })
            .collect::<Vec<_>>();
        assert!(
            value_areas.len() >= 2,
            "the fixture must wrap one Unicode value across clickable rows"
        );
        assert!(
            value_areas.windows(2).any(|areas| areas[0].y != areas[1].y),
            "wrapped fragments must occupy distinct terminal rows"
        );
        for area in value_areas {
            assert_eq!(
                mouse_action(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: area.x,
                        row: area.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hit_map,
                    &view,
                ),
                Some(UiAction::OpenWikidataItem("Q212".to_owned()))
            );
        }
    }

    #[test]
    fn trash_confirmation_names_the_action_and_full_source_path() {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            local_file_popup: Some(LocalFilePopupView::Trash {
                name: "06 - 500 рублей.flac".to_owned(),
                path: "/home/listener/Music/06 - 500 рублей.flac".to_owned(),
                error: None,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw Trash confirmation");
        let rendered = rendered_text(&terminal);

        assert!(rendered.contains("Move to trash?"));
        assert!(rendered.contains("Move “06 - 500 рублей.flac”"));
        assert!(rendered.contains("From: /home/listener/Music/06 - 500 рублей.flac"));
        assert!(
            hit_map
                .local_file_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::ConfirmLocalTrash)
        );
    }

    #[test]
    fn trash_confirmation_wraps_a_long_path_on_a_narrow_terminal() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let path = "/home/listener/Music/a-long-album-directory/another-long-directory/06-500-rubles-final-master.flac";
        let view = ViewModel {
            local_file_popup: Some(LocalFilePopupView::Trash {
                name: "06-500-rubles-final-master.flac".to_owned(),
                path: path.to_owned(),
                error: None,
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw narrow Trash confirmation");
        let rendered = rendered_text(&terminal);
        let message = format!(
            "Move “06-500-rubles-final-master.flac” to recoverable system Trash?\nFrom: {path}"
        );
        let expected_lines = wrap_text_lines(&message, 70);

        assert!(rendered.contains("Move to trash?"));
        for line in expected_lines {
            assert!(
                rendered.contains(&line),
                "wrapped popup line must remain visible: {line:?}"
            );
        }
        assert!(
            hit_map
                .local_file_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::ConfirmLocalTrash)
        );
    }

    #[test]
    fn channel_panel_renders_description_wikidata_and_external_links() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            right_panel_mode: RightPanelMode::Channel,
            details: Some(DetailView {
                title: "Mock channel".to_owned(),
                source: "Bilibili channel".to_owned(),
                channel_subscriber_count: Some(13_045),
                channel_video_count: Some(412),
                channel_total_view_count: Some(987_654_321),
                channel_created: "2018 May 4".to_owned(),
                channel_country: "Georgia".to_owned(),
                channel_webpage_url: Some(
                    url::Url::parse("https://www.youtube.com/channel/UCfixture")
                        .expect("fixture channel URL"),
                ),
                length: "must not render".to_owned(),
                likes: "must not render".to_owned(),
                views: "must not render".to_owned(),
                description: "Full channel description".to_owned(),
                wikidata: "Douglas Adams (Q42): https://www.wikidata.org/wiki/Q42".to_owned(),
                links: vec![
                    DetailLinkView {
                        label: "Telegram: Fixture".to_owned(),
                        url: "https://t.me/fixture".to_owned(),
                        wikidata_item_id: None,
                    },
                    DetailLinkView {
                        label: "Douglas Adams (Q42)".to_owned(),
                        url: "https://www.wikidata.org/wiki/Q42".to_owned(),
                        wikidata_item_id: Some("Q42".to_owned()),
                    },
                ],
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| {
                render(frame, &view, &UiSettings::default(), &mut hit_map);
            })
            .expect("draw");
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();

        assert!(rendered.contains("Mock channel"));
        assert!(rendered.contains("Subscribers: 13,045"));
        assert!(rendered.contains("Joined: 2018 May 4"));
        assert!(rendered.contains("Videos: 412"));
        assert!(rendered.contains("Total views: 987,654,321"));
        assert!(rendered.contains("Country: Georgia"));
        assert!(!rendered.contains("Length:"));
        assert!(!rendered.contains("Likes:"));
        assert!(!rendered.contains("Views:"));
        assert!(
            rendered.contains("[O] xdg-open channel · https://www.youtube.com/channel/UCfixture")
        );
        assert!(!rendered.contains("Load channel info"));
        assert!(rendered.contains("Full channel description"));
        assert!(rendered.contains("Douglas Adams (Q42)"));
        assert_eq!(rendered.matches("Douglas Adams (Q42)").count(), 1);
        assert!(!rendered.contains("Wikidata:"));
        assert!(rendered.contains("Telegram: Fixture"));
        assert_eq!(hit_map.detail_links.len(), 2);
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .all(|(action, _)| action != &UiAction::ShowChannel)
        );
    }

    #[test]
    fn channel_panel_omits_unavailable_subscriber_count() {
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            right_panel_mode: RightPanelMode::Channel,
            details: Some(DetailView {
                title: "Hidden statistics".to_owned(),
                description: "Channel description".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw channel without public subscribers");

        assert!(!rendered_text(&terminal).contains("Subscribers:"));
    }

    #[test]
    fn video_details_keep_length_likes_and_views() {
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            right_panel_mode: RightPanelMode::Details,
            details: Some(DetailView {
                title: "Mock video".to_owned(),
                length: "4:05".to_owned(),
                likes: "13,045".to_owned(),
                views: "887,263".to_owned(),
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw video details");
        let rendered = rendered_text(&terminal);

        assert!(rendered.contains("Length: 4:05"));
        assert!(rendered.contains("Likes: 13,045"));
        assert!(rendered.contains("Views: 887,263"));
        assert!(!rendered.contains("Load channel info"));
    }

    #[test]
    fn local_folder_details_offer_only_safe_entry_actions() {
        let backend = TestBackend::new(100, 18);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel {
            screen: Screen::Local,
            details: Some(DetailView {
                title: "Album".to_owned(),
                source: "Local folder".to_owned(),
                description: "Full path: /music/Album".to_owned(),
                local_trashable: true,
                ..DetailView::default()
            }),
            ..ViewModel::default()
        };
        let mut hit_map = HitMap::default();

        terminal
            .draw(|frame| render(frame, &view, &UiSettings::default(), &mut hit_map))
            .expect("draw local folder details");
        let rendered = rendered_text(&terminal);

        assert!(rendered.contains("[Delete] Move to Trash"));
        assert!(!rendered.contains("[r] Rename"));
        assert!(
            hit_map
                .detail_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::RequestLocalTrash)
        );
        assert!(
            !hit_map
                .detail_buttons
                .iter()
                .any(|(action, _)| action == &UiAction::BeginLocalRename)
        );
    }

    #[test]
    fn mouse_seek_maps_horizontal_position_to_percent() {
        let view = ViewModel::default();
        let hit_map = HitMap {
            seek_bar: Rect::new(10, 20, 101, 2),
            now_playing: Some(Rect::new(10, 21, 20, 1)),
            ..HitMap::default()
        };
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 60,
            row: 20,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(mouse, &hit_map, &view),
            Some(UiAction::SeekPercent(50.0))
        );
    }

    #[test]
    fn mouse_click_on_tracker_button_opens_tracker_screen() {
        let view = ViewModel::default();
        let hit_map = HitMap {
            buttons: vec![(
                UiAction::ShowScreen(Screen::TrackerMusic),
                Rect::new(20, 30, 21, 1),
            )],
            now_playing: Some(Rect::new(20, 30, 21, 1)),
            ..HitMap::default()
        };
        let mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 25,
            row: 30,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(mouse, &hit_map, &view),
            Some(UiAction::ShowScreen(Screen::TrackerMusic)),
            "button targets must retain priority over the now-playing target"
        );
    }

    #[test]
    fn top_tabs_follow_expected_order_with_exact_click_targets() {
        let backend = TestBackend::new(180, 1);
        let mut terminal = Terminal::new(backend).expect("terminal");
        let view = ViewModel::default();
        let mut hit_map = HitMap::default();
        terminal
            .draw(|frame| {
                render_tabs(frame, frame.area(), &view, &Theme::new(false), &mut hit_map);
            })
            .expect("draw tabs");

        let screens = hit_map
            .tabs
            .iter()
            .map(|(screen, _)| *screen)
            .collect::<Vec<_>>();
        assert_eq!(
            screens,
            vec![
                Screen::Search,
                #[cfg(feature = "youtube-music")]
                Screen::YouTubeMusic,
                Screen::TrackerMusic,
                Screen::Subscriptions,
                Screen::Local,
                Screen::Playlists,
                Screen::Downloaded,
                Screen::History,
                Screen::Statistics,
            ]
        );
        for (screen, area) in &hit_map.tabs {
            for column in [area.x, area.right().saturating_sub(1)] {
                let click = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                };
                assert_eq!(
                    mouse_action(click, &hit_map, &view),
                    Some(UiAction::ShowScreen(*screen))
                );
            }
        }
        for adjacent in hit_map.tabs.windows(2) {
            assert!(
                adjacent[0].1.right() <= adjacent[1].1.x,
                "tab hit targets must never overlap"
            );
            for column in adjacent[0].1.right()..adjacent[1].1.x {
                let divider_click = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row: adjacent[0].1.y,
                    modifiers: KeyModifiers::NONE,
                };
                assert_eq!(mouse_action(divider_click, &hit_map, &view), None);
            }
        }

        let backend = TestBackend::new(80, 1);
        let mut terminal = Terminal::new(backend).expect("compact terminal");
        let mut compact_hit_map = HitMap::default();
        terminal
            .draw(|frame| {
                render_tabs(
                    frame,
                    frame.area(),
                    &view,
                    &Theme::new(false),
                    &mut compact_hit_map,
                );
            })
            .expect("draw compact tabs");
        #[cfg(feature = "youtube-music")]
        assert!(rendered_text(&terminal).contains("YT Music"));
        assert_eq!(compact_hit_map.tabs.len(), Screen::ALL.len());
        for (screen, area) in &compact_hit_map.tabs {
            for column in [area.x, area.right().saturating_sub(1)] {
                let click = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                };
                assert_eq!(
                    mouse_action(click, &compact_hit_map, &view),
                    Some(UiAction::ShowScreen(*screen))
                );
            }
        }
        for adjacent in compact_hit_map.tabs.windows(2) {
            for column in adjacent[0].1.right()..adjacent[1].1.x {
                let divider_click = MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column,
                    row: adjacent[0].1.y,
                    modifiers: KeyModifiers::NONE,
                };
                assert_eq!(mouse_action(divider_click, &compact_hit_map, &view), None);
            }
        }
    }

    #[test]
    fn diagnostic_popup_captures_mouse_buttons_and_wheel() {
        let view = ViewModel {
            error_popup: Some(ErrorPopupView {
                title: "Error".to_owned(),
                report: "report".to_owned(),
                gh_available: true,
                ..ErrorPopupView::default()
            }),
            ..ViewModel::default()
        };
        let hit_map = HitMap {
            tabs: vec![(Screen::History, Rect::new(0, 0, 20, 2))],
            error_buttons: vec![
                (UiAction::CopyErrorReport, Rect::new(20, 20, 8, 1)),
                (UiAction::FillGitHubIssue, Rect::new(31, 20, 21, 1)),
            ],
            ..HitMap::default()
        };
        let click_copy = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 22,
            row: 20,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click_copy, &hit_map, &view),
            Some(UiAction::CopyErrorReport)
        );

        let click_underlying_tab = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(mouse_action(click_underlying_tab, &hit_map, &view), None);

        let scroll = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(scroll, &hit_map, &view),
            Some(UiAction::ScrollErrorPopup(ErrorPopupScroll::Lines(3)))
        );
    }

    #[test]
    fn youtube_setup_popup_captures_mouse_fields_and_buttons() {
        let view = ViewModel {
            youtube_setup_popup: Some(YouTubeSetupPopupView::default()),
            ..ViewModel::default()
        };
        let hit_map = HitMap {
            tabs: vec![(Screen::History, Rect::new(0, 0, 20, 2))],
            youtube_setup_fields: vec![(YouTubeSetupField::InvidiousUrl, Rect::new(20, 8, 60, 3))],
            youtube_setup_buttons: vec![
                (UiAction::OpenYouTubeApiKeyGuide, Rect::new(20, 14, 60, 2)),
                (
                    UiAction::OpenGoogleCloudCredentials,
                    Rect::new(20, 16, 60, 2),
                ),
                (UiAction::OpenInvidiousInstances, Rect::new(20, 18, 60, 2)),
                (UiAction::SubmitYouTubeSetup, Rect::new(30, 22, 22, 1)),
            ],
            ..HitMap::default()
        };
        let click_field = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 25,
            row: 9,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click_field, &hit_map, &view),
            Some(UiAction::SelectYouTubeSetupField(
                YouTubeSetupField::InvidiousUrl
            ))
        );

        let click_guide = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 25,
            row: 14,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click_guide, &hit_map, &view),
            Some(UiAction::OpenYouTubeApiKeyGuide)
        );

        let click_cloud = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 25,
            row: 16,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click_cloud, &hit_map, &view),
            Some(UiAction::OpenGoogleCloudCredentials)
        );

        let click_instances = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 25,
            row: 18,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click_instances, &hit_map, &view),
            Some(UiAction::OpenInvidiousInstances)
        );

        let click_submit = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 35,
            row: 22,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click_submit, &hit_map, &view),
            Some(UiAction::SubmitYouTubeSetup)
        );

        let click_underlying_tab = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(mouse_action(click_underlying_tab, &hit_map, &view), None);
    }

    #[test]
    fn details_mouse_focus_and_scroll_preserve_link_targets_and_shift_selection() {
        let view = ViewModel::default();
        let hit_map = HitMap {
            details_panel: Rect::new(70, 5, 40, 12),
            details_scroll_offset: 4,
            details_scroll_maximum: 10,
            detail_links: vec![(2, Rect::new(70, 8, 30, 1))],
            ..HitMap::default()
        };
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 75,
            row: 8,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click, &hit_map, &view),
            Some(UiAction::ActivateDetailLink(2))
        );

        let scroll = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 75,
            row: 8,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(scroll, &hit_map, &view),
            Some(UiAction::SetDetailsScroll(7))
        );

        let click_panel = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 105,
            row: 12,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            mouse_action(click_panel, &hit_map, &view),
            Some(UiAction::SetDetailsFocus(true))
        );

        let shift_drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 105,
            row: 12,
            modifiers: KeyModifiers::SHIFT,
        };
        assert_eq!(
            mouse_action(shift_drag, &hit_map, &view),
            None,
            "Shift-drag remains reserved for terminal-native text selection"
        );
    }

    #[test]
    fn details_selection_clips_reverse_drag_and_copies_multiple_visible_rows() {
        let hit_map = HitMap {
            detail_text_rows: vec![
                SelectableDetailsRow {
                    x: 70,
                    y: 5,
                    cells: "alpha"
                        .chars()
                        .map(|character| character.to_string())
                        .collect(),
                },
                SelectableDetailsRow {
                    x: 70,
                    y: 6,
                    cells: "beta"
                        .chars()
                        .map(|character| character.to_string())
                        .collect(),
                },
            ],
            ..HitMap::default()
        };
        let view = ViewModel {
            text_selection_mode: true,
            details_text_selection: Some(DetailsTextSelection {
                anchor: DetailsTextPosition { row: 1, column: 3 },
                focus: DetailsTextPosition { row: 1, column: 3 },
                dragging: true,
            }),
            ..ViewModel::default()
        };
        let release_beyond_top_left = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            mouse_action(release_beyond_top_left, &hit_map, &view),
            Some(UiAction::FinishDetailsTextSelection {
                focus: DetailsTextPosition { row: 0, column: 0 },
                text: "alpha\nbeta".to_owned(),
            })
        );
    }

    #[test]
    fn text_selection_mode_keeps_left_result_panel_mouse_controls_normal() {
        let view = ViewModel {
            text_selection_mode: true,
            rows: vec![RowView::default(), RowView::default()],
            ..ViewModel::default()
        };
        let hit_map = HitMap {
            rows: Rect::new(1, 4, 30, 6),
            details_panel: Rect::new(50, 3, 40, 10),
            detail_text_rows: vec![SelectableDetailsRow {
                x: 51,
                y: 4,
                cells: vec!["D".to_owned()],
            }],
            ..HitMap::default()
        };

        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 2,
                    row: 6,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::SelectRow(1))
        );
        assert_eq!(
            mouse_action(
                MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: 2,
                    row: 6,
                    modifiers: KeyModifiers::NONE,
                },
                &hit_map,
                &view,
            ),
            Some(UiAction::MoveSelection(1))
        );
    }
}
