//! Frontend-agnostic view model and action vocabulary.
//!
//! This module is the contract between the application reducer and a user
//! interface. It carries no rendering backend types, so a non-terminal
//! frontend can consume the same [`ViewModel`] and emit the same
//! [`UiAction`] values as the terminal renderer in [`crate::tui`].
//!
//! # Serialization and secrets
//!
//! Most of this vocabulary derives [`Serialize`] so a frontend outside this
//! process can consume it. Four popup views deliberately do not: they carry a
//! `YouTube` API key, a Yandex OAuth token, a private note body, and a feed URL
//! that may itself be a credential. Those values stay in this process. A
//! frontend that must render those editors needs a redacting projection first —
//! deriving [`Serialize`] on them would place secrets in another process's heap.
//!
//! Their [`ViewModel`] fields serialize as a bare `open` boolean rather than
//! being skipped outright, through `serialize_editor_presence`. One bit is
//! not a leak, and withholding it is worse than useless: these editors are
//! modal, so while one is open the keyboard map routes every key into it. A
//! frontend that cannot see that an editor exists renders an ordinary screen
//! that silently ignores input — and the `YouTube` setup editor opens by itself
//! the first time a search runs without credentials, so that is the state a new
//! user would meet first.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{BandcampAudioFormat, SubscriptionsLayout, YouTubeThumbnailSize};
use crate::domain::{Chapter, MediaId, MediaKind};
use crate::playback::PlaybackStatus;
#[cfg(feature = "qr")]
use crate::qr_code::QrMatrix;
use crate::subscriptions::SubscriptionKind;
#[cfg(feature = "local-browser")]
use crate::text_file_open::{TextFileOpenLifecycle, TextFileOpenPlan};
use crate::waveform::PeakPyramid;

/// Official Google instructions for creating and restricting a `YouTube` API key.
pub const YOUTUBE_API_KEY_GUIDE_URL: &str =
    "https://developers.google.com/youtube/registering_an_application";

/// Official overview of Yandex OAuth access tokens.
pub const YANDEX_OAUTH_GUIDE_URL: &str = "https://yandex.com/dev/id/doc/en/concepts/ya-oauth-intro";

/// Google Cloud page where the user creates and restricts API credentials.
pub const GOOGLE_CLOUD_CREDENTIALS_URL: &str = "https://console.cloud.google.com/apis/credentials";

/// Fixed-width placeholder rendered before playable Wikidata media values.
pub const WIKIDATA_MEDIA_PLAY_SYMBOL: &str = "▶";

/// Official Invidious documentation listing public instances.
pub const INVIDIOUS_INSTANCES_URL: &str = "https://docs.invidious.io/instances/";

/// Official yt-dlp source repository and release page.
pub const YT_DLP_PROJECT_URL: &str = "https://github.com/yt-dlp/yt-dlp";

/// Gentoo package metadata for yt-dlp.
pub const GENTOO_YT_DLP_PACKAGE_URL: &str = "https://packages.gentoo.org/packages/net-misc/yt-dlp";

/// Maximum clipboard payload reconstructed from one Details-panel drag.
pub(crate) const MAX_DETAILS_SELECTION_BYTES: usize = 64 * 1024;

/// Top-level Youta screen.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum Screen {
    /// Provider search and selected-item details.
    #[default]
    Search,
    /// Music-focused search through `music.youtube.com`.
    YouTubeMusic,
    /// Personalized recommendations and catalogue search through Yandex Music.
    YandexMusic,
    /// Artist, album, and track discovery through `Bandcamp`.
    Bandcamp,
    /// Podcast-show discovery through Apple's public catalogue.
    ApplePodcasts,
    /// Public-domain audiobook discovery through `LibriVox`.
    LibriVox,
    /// Curated public live-radio stations.
    Radio,
    /// Aggregated tracker-module search across dedicated archives.
    TrackerMusic,
    /// Locally subscribed channels and feeds.
    Subscriptions,
    /// Local folders and supported media files.
    Local,
    /// Media available without a network connection.
    Downloaded,
    /// Played and partially played media.
    History,
    /// Nested user playlists and folders.
    Playlists,
    /// Listening totals grouped by source.
    Statistics,
}

impl Screen {
    pub const ALL: [Self; 14] = [
        Self::Search,
        Self::YouTubeMusic,
        Self::YandexMusic,
        Self::Bandcamp,
        Self::ApplePodcasts,
        Self::LibriVox,
        Self::Radio,
        Self::TrackerMusic,
        Self::Subscriptions,
        Self::Local,
        Self::Playlists,
        Self::Downloaded,
        Self::History,
        Self::Statistics,
    ];

    /// Whether the active build exposes this provider-backed tab.
    pub const fn enabled(self) -> bool {
        match self {
            Self::YouTubeMusic => cfg!(feature = "youtube-music"),
            Self::YandexMusic => cfg!(feature = "yandex-music"),
            Self::Bandcamp => cfg!(feature = "bandcamp"),
            Self::ApplePodcasts => cfg!(feature = "apple-podcasts"),
            Self::LibriVox => cfg!(feature = "librivox"),
            Self::Radio => cfg!(feature = "radio"),
            _ => true,
        }
    }

    /// Returns the Details layout this screen's selection is rendered with.
    ///
    /// This lives beside the screen rather than inside a renderer because both
    /// front-ends have to agree on it: which facts a selected item exposes is a
    /// property of the source, not of the surface drawing it.
    #[must_use]
    pub const fn details_kind(self) -> InformationPanelKind {
        match self {
            Self::Local => InformationPanelKind::Local,
            Self::ApplePodcasts => InformationPanelKind::Podcast,
            Self::LibriVox => InformationPanelKind::Audiobook,
            Self::Radio => InformationPanelKind::Radio,
            Self::YandexMusic => InformationPanelKind::YandexMusic,
            Self::Bandcamp | Self::Playlists | Self::History => InformationPanelKind::Generic,
            _ => InformationPanelKind::Video,
        }
    }

    /// Returns the verb this screen's query editor is spelled with.
    ///
    /// `None` means the screen collects no query at all, and neither front-end
    /// should offer one. Radio filters its compiled catalogue as the user types;
    /// every other searchable screen submits to a provider. Both front-ends ask
    /// this exact question — the terminal to decide whether the result panel
    /// carries a title, the window to decide whether it draws a search field —
    /// so the answer lives beside the tab labels rather than in either renderer.
    #[must_use]
    pub const fn search_verb(self) -> Option<&'static str> {
        match self {
            Self::Search
            | Self::YouTubeMusic
            | Self::YandexMusic
            | Self::Bandcamp
            | Self::ApplePodcasts
            | Self::LibriVox
            | Self::TrackerMusic => Some("Search"),
            Self::Radio => Some("Filter"),
            Self::Subscriptions
            | Self::Local
            | Self::Downloaded
            | Self::History
            | Self::Playlists
            | Self::Statistics => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Search => "YT",
            Self::YouTubeMusic => "YT Music",
            Self::YandexMusic => "YandexMusic",
            Self::Bandcamp => "Bandcamp",
            Self::ApplePodcasts => "Apple Podcasts",
            Self::LibriVox => "LibriVox",
            Self::Radio => "Radio",
            Self::TrackerMusic => "MOD/tracker",
            Self::Local => "Local",
            Self::Subscriptions => "Subscriptions",
            Self::Playlists => "Playlists",
            Self::Downloaded => "Downloaded",
            Self::History => "History",
            Self::Statistics => "Stats",
        }
    }

    /// Returns the shortened tab label used where the full one does not fit.
    #[must_use]
    pub const fn compact_label(self) -> &'static str {
        match self {
            Self::Search => "YT",
            Self::YouTubeMusic => "YT Music",
            Self::YandexMusic => "Yandex",
            Self::Bandcamp => "Bandcamp",
            Self::ApplePodcasts => "Apple",
            Self::LibriVox => "LibriVox",
            Self::Radio => "Radio",
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
    pub(crate) fn next(self) -> Self {
        let Some(index) = Self::ALL.iter().position(|candidate| *candidate == self) else {
            return Self::ALL[0];
        };
        (1..=Self::ALL.len())
            .map(|offset| Self::ALL[(index + offset) % Self::ALL.len()])
            .find(|candidate| candidate.enabled())
            .unwrap_or(Self::Search)
    }

    /// Returns the previous enabled top-level tab, wrapping at the beginning.
    pub(crate) fn previous(self) -> Self {
        let Some(index) = Self::ALL.iter().position(|candidate| *candidate == self) else {
            return Self::ALL[Self::ALL.len() - 1];
        };
        (1..=Self::ALL.len())
            .map(|offset| Self::ALL[(index + Self::ALL.len() - offset) % Self::ALL.len()])
            .find(|candidate| candidate.enabled())
            .unwrap_or(Self::Search)
    }
}

/// Alternative content shown in the right-hand panel.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum RightPanelMode {
    /// Metadata and description.
    #[default]
    Details,
    /// Channel or feed information for the playing item.
    Channel,
}

/// Owner-aware state of the lazily generated local waveform.
///
/// Keeping the decoded peak pyramid in the immutable view lets terminal
/// resizes select a suitable resolution without decoding the local file again.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub enum WaveformView {
    /// No eligible local media currently owns the waveform track.
    #[default]
    Unavailable,
    /// The background worker is inspecting the exact local media item.
    Loading {
        /// Local media identity captured when generation started.
        media_id: MediaId,
    },
    /// A complete, reusable multiresolution peak envelope.
    Ready {
        /// Local media identity owning these peaks.
        media_id: MediaId,
        /// Controller generation identifying the exact file identity behind these peaks.
        generation: u64,
        /// Duration used for owner-aware mouse seeking.
        duration: Duration,
        /// Width-independent peak data shared without per-frame cloning.
        ///
        /// Skipped when serializing: an out-of-process frontend receives peaks
        /// over a binary channel, because a min/max envelope is disproportionately
        /// expensive as JSON and is sent once per media rather than per frame.
        #[serde(skip)]
        pyramid: Arc<PeakPyramid>,
    },
    /// Generation failed for the exact local media item.
    Failed {
        /// Local media identity owning this failure.
        media_id: MediaId,
        /// Short actionable failure text.
        message: String,
    },
}

/// Object type queried by the default YouTube search screen.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum SearchKind {
    /// Search for YouTube videos.
    #[default]
    Videos,
    /// Search for YouTube channels independently of videos.
    Channels,
}

/// Ordering selected for YouTube video or channel searches.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum YouTubeSearchSort {
    /// Let the configured provider rank results by relevance.
    #[default]
    Relevance,
    /// Put the newest available uploads first.
    Newest,
}

/// Exact catalogue category queried by the Yandex Music tab.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum YandexMusicSearchKind {
    /// Music, podcasts, and audiobooks in one catalogue query.
    #[default]
    All,
    /// Songs, artists, and music albums.
    Music,
    /// Podcast shows and episodes.
    Podcasts,
    /// Audiobooks identified by explicit provider metadata.
    Audiobooks,
}

impl YandexMusicSearchKind {
    /// Returns the next category in the Yandex Music search selector.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::All => Self::Music,
            Self::Music => Self::Podcasts,
            Self::Podcasts => Self::Audiobooks,
            Self::Audiobooks => Self::All,
        }
    }

    /// Returns the compact category label rendered in controls and status.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Music => "music",
            Self::Podcasts => "podcasts",
            Self::Audiobooks => "audiobooks",
        }
    }

    /// Returns the title-cased category label used in the search panel heading.
    #[must_use]
    pub const fn title_label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Music => "Music",
            Self::Podcasts => "Podcasts",
            Self::Audiobooks => "Audiobooks",
        }
    }
}

/// Active content route inside the authenticated Yandex Music tab.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum YandexMusicRouteView {
    /// The account's default My Wave recommendations.
    #[default]
    Recommendations,
    /// One explicit catalogue search result.
    Search,
    /// Tracks belonging to one opened album, podcast, or audiobook.
    Album,
    /// Popular tracks and albums belonging to one exact artist.
    Artist,
}

/// Reaction state shown for one selected Yandex Music track.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum YandexMusicReactionView {
    /// Neither explicit reaction is selected.
    #[default]
    Neutral,
    /// The track is liked, including an optimistic offline update.
    Liked,
    /// The track is disliked, including an optimistic offline update.
    Disliked,
}

/// Selection-sensitive controls exposed by the Yandex Music detail panel.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct YandexMusicActionsView {
    /// Selected row is a playable track, episode, or audiobook chapter.
    pub track_selected: bool,
    /// Selected row has an exact primary artist that can open inside Youta.
    pub artist_available: bool,
    /// Selected row can open an album, show, or audiobook.
    pub album_available: bool,
    /// Current route is an opened album rather than recommendations/search.
    pub album_open: bool,
    /// At least ten recommendation tracks can be downloaded as one batch.
    pub twenty_recommendations_available: bool,
    /// Optimistic current reaction for the selected track.
    pub reaction: YandexMusicReactionView,
}

/// Ordering applied to entries in the Local file browser.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
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

/// Ordering applied to the built-in Radio station catalogue.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum RadioSort {
    /// Sort station names alphabetically.
    #[default]
    Name,
    /// Put higher known bitrates first and unknown bitrates last.
    BitrateDescending,
    /// Put lower known bitrates first and unknown bitrates last.
    BitrateAscending,
}

impl RadioSort {
    /// Returns the next Radio ordering exposed by the `B` control.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Name => Self::BitrateDescending,
            Self::BitrateDescending => Self::BitrateAscending,
            Self::BitrateAscending => Self::Name,
        }
    }

    /// Returns the compact label displayed in the Radio control row.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Name => "Sort: name A–Z",
            Self::BitrateDescending => "Sort: bitrate high–low",
            Self::BitrateAscending => "Sort: bitrate low–high",
        }
    }
}

/// Submitted provider search whose progress is animated in the result panel.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum SearchActivity {
    /// A video or channel search through the configured `YouTube` provider.
    YouTube,
    /// A music-focused search through `yt-dlp` and `music.youtube.com`.
    YouTubeMusic,
    /// A Yandex Music catalogue search.
    YandexMusic,
    /// A public track and album search through `Bandcamp`.
    Bandcamp,
    /// A show search through Apple's public podcast catalogue.
    ApplePodcasts,
    /// A public-domain audiobook search through `LibriVox`.
    LibriVox,
    /// An aggregate search through the enabled MOD/tracker archives.
    TrackerArchives,
}

impl SearchActivity {
    /// Returns the result screen that owns this submitted search.
    #[must_use]
    pub const fn screen(self) -> Screen {
        match self {
            Self::YouTube => Screen::Search,
            Self::YouTubeMusic => Screen::YouTubeMusic,
            Self::YandexMusic => Screen::YandexMusic,
            Self::Bandcamp => Screen::Bandcamp,
            Self::ApplePodcasts => Screen::ApplePodcasts,
            Self::LibriVox => Screen::LibriVox,
            Self::TrackerArchives => Screen::TrackerMusic,
        }
    }
}

/// One row shown in a list panel.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
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
    /// Whether accepted playback has started, including before one percent.
    ///
    /// This is persisted independently from the rounded percentage so a
    /// successful zero-position start can render as partial without claiming
    /// that one percent has already been heard.
    pub playback_started: bool,
    /// Whether the source is locally subscribed.
    pub subscribed: bool,
    /// Preferred artwork URL available for selected rendering or prefetch.
    pub thumbnail_url: Option<url::Url>,
    /// Whether provider metadata identifies this as a vertical video.
    pub vertical: bool,
    /// Hide played-state markers while retaining identity for playing-row emphasis.
    pub hide_watched_marker: bool,
    /// Omit generic source and marker padding on a source-specific screen.
    pub compact: bool,
    /// Whether a Radio station is pinned by the persistent favorites action.
    pub radio_favorite: bool,
    /// Whether this Local row belongs to the current explicit move batch.
    pub local_marked: bool,
}

/// One selected local video frame rendered through the thumbnail worker.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LocalVideoThumbnailView {
    /// Exact local video path; the worker revalidates its filesystem identity.
    pub path: PathBuf,
    /// Midpoint seek position in milliseconds.
    pub midpoint_millis: u64,
}

/// Serializes a credential-bearing editor as the single bit that it is open.
///
/// The generic parameter is deliberately unconstrained: nothing about the
/// editor is read, so no future field can accidentally become serializable by
/// being added to one of them.
fn serialize_editor_presence<T, S: serde::Serializer>(
    editor: &Option<T>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_bool(editor.is_some())
}

/// Source-specific metadata layout used by a front-end's information panel.
///
/// The variants differ in which facts they present, not in how they look:
/// a Radio station has no like count and a local file has no publication date,
/// so a single layout would either invent fields or hide real ones.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum InformationPanelKind {
    /// Media details with duration, likes, and views.
    Video,
    /// Podcast show or episode details without video-only statistics.
    Podcast,
    /// Public-domain audiobook or section details without video-only statistics.
    Audiobook,
    /// Channel details with subscriber metadata.
    Channel,
    /// Public live-radio metadata without finite-media statistics.
    Radio,
    /// Authenticated Yandex Music catalogue details and source-specific actions.
    YandexMusic,
    /// Local folder, media, or image metadata without remote statistics.
    Local,
    /// Persisted or aggregate rows without source-specific remote statistics.
    Generic,
}

/// Details for the selected media item.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
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
    /// Full Last.fm artist biography discovered after local fingerprinting.
    ///
    /// This enrichment is kept separate from [`Self::description`] so a late
    /// network completion cannot duplicate or overwrite rebuilt local metadata.
    pub lastfm_artist_description: String,
    /// Parsed timecode spans that can seek or start the selected media.
    pub timecodes: Vec<DetailTimecodeView>,
    /// Parsed `YouTube` video URLs that may replace Details internally.
    pub video_links: Vec<DetailVideoLinkView>,
    /// Public like count, formatted by the provider.
    pub likes: String,
    /// Public view count, formatted by the provider.
    pub views: String,
    /// Public top-level comment count, formatted by the provider.
    pub comments: String,
    /// Publication date.
    pub published: String,
    /// Provider-reported license.
    pub license: String,
    /// Whether the selected Radio station is stored in persistent favorites.
    pub radio_favorite: bool,
    /// Local playlists that currently contain this media item.
    ///
    /// The controller supplies display names in stable presentation order.
    /// An empty list keeps the playlist metadata line out of Details.
    pub playlist_names: Vec<String>,
    /// Whether an exact private local note exists for this media or source.
    ///
    /// The note body deliberately remains outside `DetailView` so diagnostics
    /// and debug snapshots cannot expose user-authored private text.
    pub has_private_note: bool,
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
    /// Largest provider-advertised image reserved for full-terminal expansion.
    ///
    /// The normal Details preview continues to use [`Self::thumbnail_url`],
    /// which follows the configured YouTube size. Image-enabled renderers may
    /// warm this separate target before the user clicks the visible preview.
    pub expanded_thumbnail_url: Option<url::Url>,
    /// Source pixel dimensions used to reserve an aspect-correct preview area.
    pub thumbnail_dimensions: Option<(u32, u32)>,
    /// Lazy midpoint-frame target for a selected local video.
    pub local_video_thumbnail: Option<LocalVideoThumbnailView>,
    /// Whether artwork occupies all remaining rows in the Details panel.
    ///
    /// This interaction-only state is never persisted. Selecting another item
    /// constructs a fresh [`DetailView`] and restores the configured artwork
    /// height.
    pub thumbnail_expanded: bool,
    /// Whether the selected local entry can be renamed in place.
    pub local_renamable: bool,
    /// Whether the selected local file or folder can enter the move workflow.
    pub local_movable: bool,
    /// Whether the selected local entry can be moved to recoverable Trash.
    pub local_trashable: bool,
    /// Whether the selected local audio file should offer fingerprinting.
    ///
    /// A successful identity-bound lookup, including one with no matches,
    /// hides the action while its cached result remains projected in Details.
    pub local_fingerprint_available: bool,
    /// Whether the selected file owns an active fingerprint lookup.
    pub local_fingerprint_pending: bool,
    /// Whether the current Local selection or marked set can start quality analysis.
    ///
    /// Audio files are analyzed directly; directories are discovered
    /// recursively by the worker.
    pub local_audio_quality_available: bool,
    /// Whether an audio-quality batch is active for this Local view.
    pub local_audio_quality_pending: bool,
    /// Bounded, presentation-ready result of local audio-quality analysis.
    ///
    /// The controller keeps this separate from the provider description so
    /// front-ends can present the estimate as qualified analysis rather than
    /// as source metadata.
    pub local_audio_quality_description: String,
}

/// Current route inside the Subscriptions screen.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum SubscriptionRoute {
    /// Selectable OPML sources.
    #[default]
    Sources,
    /// Videos belonging to the activated source.
    Items,
}

/// Pane receiving list navigation in split Subscriptions mode.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum SubscriptionPane {
    /// The source list.
    #[default]
    Sources,
    /// The selected source's media list.
    Items,
}

/// Render-ready state owned by the Subscriptions screen.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
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
    /// Whether YouTube subscription lists include provider-identified Shorts.
    pub show_youtube_shorts: bool,
    /// Human-readable source name included in the item-list heading.
    pub source_title: String,
    /// Provider family controlling source-specific headings and actions.
    pub source_kind: SubscriptionKind,
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
            show_youtube_shorts: true,
            source_title: String::new(),
            source_kind: SubscriptionKind::YouTube,
            source_subscriber_count: None,
            source_created: String::new(),
        }
    }
}

/// Focused editor for adding one portable audio or video podcast feed.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RssSubscriptionPopupView {
    /// Draft absolute HTTP(S) RSS or Atom feed URL, potentially with a private
    /// query token. Its custom [`std::fmt::Debug`] implementation redacts it.
    pub url: String,
    /// Validation or OPML-persistence failure retained inside the popup.
    pub validation_error: Option<String>,
    /// Exact private OPML file that receives the new subscription.
    pub config_path: String,
}

impl std::fmt::Debug for RssSubscriptionPopupView {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RssSubscriptionPopupView")
            .field("url", &"[REDACTED]")
            .field(
                "validation_error",
                &self.validation_error.as_ref().map(|_| "[REDACTED]"),
            )
            .field("config_path", &self.config_path)
            .finish()
    }
}

/// Focused in-app editor for preferences that are implemented at runtime.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PreferencesPopupView {
    /// Draft Subscriptions layout saved only when the user confirms.
    pub subscriptions_layout: SubscriptionsLayout,
    /// Draft advertisement-chapter behavior saved only on confirmation.
    pub skip_advertisement_chapters: bool,
    /// Draft selected-video `YouTube` prewarming saved only on confirmation.
    pub youtube_prewarm: bool,
    /// Draft exact `YouTube` thumbnail size saved only on confirmation.
    pub youtube_thumbnail_size: YouTubeThumbnailSize,
    /// Draft lazy Local-folder size behavior saved only on confirmation.
    pub show_local_folder_sizes: bool,
    /// Draft physical-Linux-TTY artwork behavior saved only on confirmation.
    pub show_images_in_tty: bool,
    /// Draft preferred Bandcamp playback encoding.
    pub bandcamp_audio_format: BandcampAudioFormat,
    /// Exact private TOML file updated by the save action.
    pub config_path: String,
    /// Environment variable shadowing the TOML value, when present.
    pub environment_override: Option<String>,
    /// Save or validation failure kept inside the popup.
    pub validation_error: Option<String>,
}

/// Playable selected item for which playlist actions are available.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlaylistItemView {
    /// Stable identity used to verify that Details actions target displayed media.
    pub media_id: MediaId,
    /// Human-readable item title shown in the playlist chooser.
    pub title: String,
    /// Whether the reserved `todo` playlist currently contains the item.
    pub in_todo: bool,
}

/// Focused multiline editor for one private local note.
///
/// The body has a custom redacted [`std::fmt::Debug`] representation because
/// `ViewModel` is routinely included in test failures and may later be sampled
/// while producing diagnostics.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct PrivateNotePopupView {
    /// Human-readable media, channel, or podcast-show label.
    pub target_label: String,
    /// User-authored multiline plain text.
    pub body: String,
    /// UTF-8 byte offset at a grapheme boundary where edits are applied.
    pub cursor_byte: usize,
    /// First wrapped visual line requested for the editor viewport.
    pub scroll_offset: usize,
    /// Whether rendering should keep the insertion cursor inside the viewport.
    pub follow_cursor: bool,
    /// Whether a note existed when this editor opened.
    pub existing: bool,
    /// Whether Delete now awaits one explicit confirmation.
    pub confirming_delete: bool,
    /// Exact local state path used by the selected persistence backend.
    pub storage_path: String,
    /// Validation or persistence failure retained inside the editor.
    pub validation_error: Option<String>,
}

impl std::fmt::Debug for PrivateNotePopupView {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PrivateNotePopupView")
            .field("target_label", &self.target_label)
            .field("body", &"[REDACTED]")
            .field("cursor_byte", &self.cursor_byte)
            .field("scroll_offset", &self.scroll_offset)
            .field("follow_cursor", &self.follow_cursor)
            .field("existing", &self.existing)
            .field("confirming_delete", &self.confirming_delete)
            .field("storage_path", &self.storage_path)
            .field(
                "validation_error",
                &self.validation_error.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

/// Cursor movement inside the multiline private-note editor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PrivateNoteCursorMotion {
    /// Move to the preceding grapheme cluster.
    Left,
    /// Move to the following grapheme cluster.
    Right,
    /// Keep the visual column while moving to the preceding line.
    Up,
    /// Keep the visual column while moving to the following line.
    Down,
    /// Move to the start of the current line.
    Home,
    /// Move to the end of the current line.
    End,
}

/// One entry of the playback queue, as a front-end should present it.
///
/// This is deliberately a projection rather than a borrow of
/// [`crate::domain::QueueItem`]: that type carries `playback_location`, which
/// for several providers is a signed, short-lived media URL. A signed URL must
/// not reach durable state, diagnostics, or — since the desktop window renders
/// this view — another process. Queue actions therefore address entries by
/// position, and the location never leaves the controller.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct QueueRowView {
    /// Stable media identity, for correlating with the playing item.
    pub media_id: MediaId,
    /// Primary label.
    pub title: String,
    /// Channel, artist, author, or station name.
    pub subtitle: String,
    /// Preformatted running time, or an empty string when the provider has none.
    ///
    /// The provider is named by [`Self::media_id`] rather than repeated here,
    /// so no front-end has to keep its own copy of the source-label mapping.
    pub length: String,
}

/// The playback queue and its cursor.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct QueuePopupView {
    /// Ordered entries, in play order.
    pub items: Vec<QueueRowView>,
    /// Entry the cursor is on, or `None` once the queue has been exhausted.
    pub current: Option<usize>,
    /// Highlighted row, which starts on the current entry.
    pub selected: usize,
    /// Whether the current entry repeats instead of advancing.
    pub repeat_one: bool,
}

/// What is playing right now, in the words a front-end should announce it with.
///
/// The player bar already shows what the backend reports, which is whatever the
/// engine parsed out of the stream. This is Youta's own answer instead, taken
/// from the queue entry that is playing, and it exists because three desktop
/// surfaces have to agree on it: the window title, the tray tooltip, and the
/// notification raised when a track changes while the window is behind
/// something else. Deriving it three times from `rows` would be three chances
/// to disagree, and `rows` does not even contain the playing item once the user
/// has browsed elsewhere.
///
/// It carries no location for the same reason [`QueueRowView`] carries none.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct NowPlayingView {
    /// Stable media identity, which is what marks a *change* of track.
    ///
    /// A title can repeat across entries, so the identity is what a front-end
    /// compares to decide that something new started.
    pub media_id: MediaId,
    /// Primary label.
    pub title: String,
    /// Channel, artist, author, or station name; empty when the provider has none.
    pub subtitle: String,
}

/// One local playlist shown in the membership chooser.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct PlaylistChoiceView {
    /// Stable controller-owned playlist identity.
    pub playlist_id: String,
    /// User-visible playlist name.
    pub name: String,
    /// Whether the selected playable item belongs to this playlist.
    pub contains_item: bool,
}

/// Active route inside the playlist chooser/editor popup.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum PlaylistPopupMode {
    /// Browse playlists and toggle the selected item's membership.
    #[default]
    Choose,
    /// Create a playlist and add the selected item to it.
    Create,
    /// Rename or describe an existing playlist without changing its identity.
    Edit,
}

/// Focused field inside the playlist create/edit form.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum PlaylistEditorField {
    /// Required playlist display name.
    #[default]
    Name,
    /// Optional playlist description.
    Description,
}

/// Controller-owned local-playlist chooser and create/edit form.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PlaylistPopupView {
    /// Selected playable item whose membership is being edited.
    pub item_title: String,
    /// Stable playlist rows with current membership exposed immediately.
    pub playlists: Vec<PlaylistChoiceView>,
    /// Selected playlist row in [`Self::playlists`].
    pub selected: usize,
    /// Chooser, creation, or editing route.
    pub mode: PlaylistPopupMode,
    /// Stable identity retained while editing an existing playlist.
    ///
    /// This remains `None` for the chooser and new-playlist form. In
    /// particular, editing the reserved `todo` playlist changes only its
    /// display fields, never the identity used by the quick action.
    pub editing_playlist_id: Option<String>,
    /// Editor field currently receiving printable characters.
    pub editor_field: PlaylistEditorField,
    /// Draft required display name.
    pub editor_name: String,
    /// Draft optional description; an empty value represents `None`.
    pub editor_description: String,
    /// Maximum UTF-8 bytes accepted in the name.
    pub name_limit: usize,
    /// Maximum user-visible characters accepted in the description.
    pub description_limit: usize,
    /// Validation or persistence failure retained inside the popup.
    pub validation_error: Option<String>,
}

impl Default for PlaylistPopupView {
    fn default() -> Self {
        Self {
            item_title: String::new(),
            playlists: Vec::new(),
            selected: 0,
            mode: PlaylistPopupMode::Choose,
            editing_playlist_id: None,
            editor_field: PlaylistEditorField::Name,
            editor_name: String::new(),
            editor_description: String::new(),
            name_limit: 256,
            description_limit: 1_000,
            validation_error: None,
        }
    }
}

/// Explicit local or downloaded-file mutation awaiting input or confirmation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
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
    /// Recoverable move-to-Trash confirmation for an offline download.
    DownloadedTrash {
        /// Exact display basename being removed from the downloads directory.
        name: String,
        /// Full source path passed to the system Trash backend after confirmation.
        path: String,
        /// Filesystem failure retained in the popup.
        error: Option<String>,
    },
    /// Destination browser for one or more explicitly selected Local entries.
    Move {
        /// Lossy display names of the source entries, bounded by the controller.
        source_names: Vec<String>,
        /// Canonical directory that would receive the selected sources.
        destination: String,
        /// Parent and real child directories available for navigation.
        directories: Vec<LocalMoveDestinationView>,
        /// Selected destination-browser row.
        selected: usize,
        /// Whether a background directory listing is in flight.
        pending: bool,
        /// Validation or filesystem failure retained in the popup.
        error: Option<String>,
    },
}

/// One exact destination-browser row inside the Local Move popup.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LocalMoveDestinationView {
    /// Human-readable basename, or `..` for the canonical parent.
    pub name: String,
    /// Canonical directory selected when this row is activated.
    pub path: String,
}

/// One selectable timecode span inside the original Details description.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
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
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
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
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DetailLinkView {
    /// Plain, non-clickable text rendered immediately before the link value.
    pub prefix: String,
    /// Human-readable link label, such as a Wikidata item name.
    pub label: String,
    /// Absolute URL passed to the controller when the link is activated.
    pub url: String,
    /// Exact Wikidata Q-ID when this link owns a lazy property spoiler.
    pub wikidata_item_id: Option<String>,
    /// Provider-selected text and spacing treatment for this link.
    pub presentation: DetailLinkPresentation,
    /// Optional exact destination opened inside Youta by the adjacent marker.
    pub internal_target: Option<DetailLinkInternalTarget>,
}

/// Exact provider destination exposed by one Details-row internal-link marker.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub enum DetailLinkInternalTarget {
    /// One stable Yandex Music artist identifier.
    YandexMusicArtist(String),
    /// One stable Yandex Music album, show, or audiobook identifier.
    YandexMusicAlbum(String),
    /// One stable numeric `LibriVox` author identifier.
    LibriVoxAuthor(String),
}

impl DetailLinkInternalTarget {
    /// Builds the semantic controller action dispatched by its one-cell marker.
    #[must_use]
    pub fn action(&self) -> UiAction {
        match self {
            Self::YandexMusicArtist(id) => UiAction::OpenYandexMusicArtistById(id.clone()),
            Self::YandexMusicAlbum(id) => UiAction::OpenYandexMusicAlbumById(id.clone()),
            Self::LibriVoxAuthor(id) => UiAction::OpenLibriVoxAuthorById(id.clone()),
        }
    }
}

/// Text and vertical-spacing treatment for one external Details link.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub enum DetailLinkPresentation {
    /// Render the human label followed by an em dash and the URL.
    #[default]
    LabelAndUrl,
    /// Render the human label and URL, then reserve one non-clickable row.
    LabelAndUrlSpaced,
    /// Render only the human-readable label.
    LabelOnly,
    /// Render only the label and reserve one non-clickable row after it.
    LabelOnlySpaced,
    /// Render only the URL.
    UrlOnly,
    /// Render only the URL and reserve one non-clickable row after it.
    UrlOnlySpaced,
}

/// Bounded human-facing Wikidata properties cached for one Details page.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DetailWikidataEntityView {
    /// Exact Wikidata Q-ID represented by this spoiler.
    pub item_id: String,
    /// Preformatted, scrollable property/value text.
    pub text: String,
    /// Item-valued statement spans that open canonical Wikidata pages.
    pub value_links: Vec<DetailWikidataValueLinkView>,
    /// Play/pause controls for supported Commons audio and video values.
    pub media_controls: Vec<DetailWikidataMediaView>,
    /// First Commons P18 preview retained for the expanded property spoiler.
    ///
    /// Keeping one preview bounds rendering and memory work. It is used only as
    /// a fallback when no primary provider artwork exists and never replaces
    /// YouTube or other source artwork. Every P18 value remains readable and
    /// clickable in [`Self::text`].
    pub image_url: Option<url::Url>,
}

/// One playable Commons value embedded in expanded Wikidata text.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DetailWikidataMediaView {
    /// Inclusive byte offset of the fixed-width play/pause marker.
    pub marker_start_byte: usize,
    /// Exclusive byte offset of the fixed-width play/pause marker.
    pub marker_end_byte: usize,
    /// Stable Commons identity used to match current playback.
    pub media_id: MediaId,
    /// Audio or video classification supplied by the Wikidata provider.
    pub kind: MediaKind,
    /// Human-facing Commons filename used as the player title.
    pub title: String,
    /// Canonical Commons file page retained for navigation and history.
    pub webpage_url: url::Url,
    /// Stable credential-free Commons file redirect passed to playback.
    pub playback_url: url::Url,
}

/// One clickable item, identifier, Commons page, or Wikipedia article inside
/// expanded Wikidata details.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DetailWikidataValueLinkView {
    /// Inclusive UTF-8 byte offset in [`DetailWikidataEntityView::text`].
    pub start_byte: usize,
    /// Exclusive UTF-8 byte offset in [`DetailWikidataEntityView::text`].
    pub end_byte: usize,
    /// Validated credential-free HTTP(S) target supplied by the provider.
    pub url: String,
}

/// One terminal-cell position inside the visible, selectable Details text.
///
/// Rows are semantic selectable rows, not absolute terminal rows. This keeps
/// the selection confined to rendered metadata, links, and description text
/// while excluding the other pane, pane headings, controls, and thumbnail cells.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct DetailsTextPosition {
    /// Zero-based selectable row in the currently visible Details content.
    pub row: usize,
    /// Zero-based terminal-cell column inside that row.
    pub column: usize,
}

/// Current Youta-owned drag selection in the Details panel.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct DetailsTextSelection {
    /// Position where the current drag started.
    pub anchor: DetailsTextPosition,
    /// Most recent clipped pointer position.
    pub focus: DetailsTextPosition,
    /// Whether a left-button drag is still in progress.
    pub dragging: bool,
}

/// Progressive result of one bounded yt-dlp version lookup.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub enum YtDlpVersionLookupView {
    /// The bounded local or remote lookup is still running.
    #[default]
    Loading,
    /// One version was found, with its release date when the source exposes one.
    Available {
        /// Version text exactly as reported by the trusted helper or provider.
        version: String,
        /// Human-readable release date, when known.
        released_on: Option<String>,
    },
    /// The bounded lookup finished without usable version metadata.
    Unavailable {
        /// Short, non-sensitive explanation suitable for display.
        reason: String,
    },
}

/// Gentoo's architecture-specific stable yt-dlp package metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct YtDlpGentooVersionView {
    /// Gentoo keyword architecture whose stable version is being reported.
    pub arch: String,
    /// Fixed Gentoo package page displayed by both front-ends.
    pub package_url: String,
    /// Independently loading latest stable version for this architecture.
    pub latest_stable: YtDlpVersionLookupView,
}

impl Default for YtDlpGentooVersionView {
    fn default() -> Self {
        Self {
            arch: String::new(),
            package_url: GENTOO_YT_DLP_PACKAGE_URL.to_owned(),
            latest_stable: YtDlpVersionLookupView::Loading,
        }
    }
}

/// Progressive version metadata shown for an HTTP 403 attributed to yt-dlp.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct YtDlpForbiddenView {
    /// Fixed official yt-dlp project URL displayed by both front-ends.
    pub project_url: String,
    /// Version of the configured yt-dlp executable Youta invoked.
    pub installed: YtDlpVersionLookupView,
    /// Latest release reported by the official GitHub repository.
    pub github_latest: YtDlpVersionLookupView,
    /// Gentoo package metadata, present only when Youta runs on Gentoo.
    pub gentoo: Option<YtDlpGentooVersionView>,
}

impl YtDlpForbiddenView {
    /// Creates the immediately visible state while bounded lookups run.
    #[must_use]
    pub fn loading(gentoo_arch: Option<String>) -> Self {
        Self {
            project_url: YT_DLP_PROJECT_URL.to_owned(),
            installed: YtDlpVersionLookupView::Loading,
            github_latest: YtDlpVersionLookupView::Loading,
            gentoo: gentoo_arch.map(|arch| YtDlpGentooVersionView {
                arch,
                package_url: GENTOO_YT_DLP_PACKAGE_URL.to_owned(),
                latest_stable: YtDlpVersionLookupView::Loading,
            }),
        }
    }
}

impl Default for YtDlpForbiddenView {
    fn default() -> Self {
        Self::loading(None)
    }
}

/// Lifecycle of one explicit diagnostic-report submission to GitHub.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub enum GitHubIssueSubmissionView {
    /// No submission has been requested for this diagnostic report.
    #[default]
    Idle,
    /// The user must confirm that the complete report will become public.
    Confirming,
    /// A background `gh issue create` process is still running.
    Submitting,
    /// GitHub accepted the report and returned its canonical issue URL.
    Submitted {
        /// Credential-free URL validated against Youta's issue tracker.
        url: String,
    },
    /// `gh` may have submitted the issue but did not return a definite result.
    OutcomeUnknown {
        /// Repository issue list where the user can check before retrying.
        issues_url: String,
    },
    /// GitHub definitely rejected the request, so an explicit retry is safe.
    Failed {
        /// Bounded, redacted failure returned by the GitHub CLI.
        message: String,
    },
}

/// Diagnostic or actionable setup information shown above the normal interface.
///
/// For reportable failures, `report` contains the complete, copyable diagnostic
/// report rather than a shortened user-facing message. Setup guidance instead
/// stores its concise instructions there. The controller owns `scroll_offset`
/// so the position survives terminal redraws and resize events.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ErrorPopupView {
    /// Short error title displayed in the popup border.
    pub title: String,
    /// Complete diagnostic report or concise local setup guidance.
    pub report: String,
    /// Zero-based wrapped-line offset at the top of the viewport.
    pub scroll_offset: usize,
    /// Whether direct GitHub CLI submission is available for this popup.
    pub gh_available: bool,
    /// Whether this popup describes a reportable failure rather than setup guidance.
    pub reportable: bool,
    /// Result of the most recent copy or issue-submission action.
    pub action_status: Option<String>,
    /// Short progressive body for a 403 attributed to yt-dlp.
    ///
    /// The complete diagnostic payload remains in `report` for copying even
    /// while front-ends render this structured body instead.
    pub yt_dlp_forbidden: Option<YtDlpForbiddenView>,
    /// Confirmation, progress, or result of direct GitHub issue submission.
    pub github_issue_submission: GitHubIssueSubmissionView,
}

/// Copyable progress and results for one local audio-quality batch.
///
/// A batch may contain one selected file, the explicitly marked Local rows, or
/// the recursively discovered audio files below one selected folder. The
/// controller bounds `report`; front-ends only wrap and scroll the text.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct AudioQualityPopupView {
    /// Short popup-border title.
    pub title: String,
    /// Current file or terminal batch summary.
    pub summary: String,
    /// Files that reached a terminal result.
    pub completed: usize,
    /// Files accepted into the bounded batch.
    pub total: usize,
    /// Plain-text, one-record-per-file report copied exactly by the controller.
    pub report: String,
    /// Result of the most recent report-copy request.
    pub action_status: Option<String>,
    /// Whether the worker may still add results.
    pub pending: bool,
    /// Zero-based wrapped-line offset at the top of the report viewport.
    pub scroll_offset: usize,
}

/// One public top-level comment rendered in the selected-video popup.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct VideoCommentView {
    /// Public author display name.
    pub author_name: String,
    /// Public like count attached to the comment.
    pub like_count: u64,
    /// Human-readable publication date, when exposed by the provider.
    pub published: Option<String>,
    /// Provider-supplied plain-text body.
    pub text: String,
}

/// Explicit loading state for the bounded public-comments popup.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub enum VideoCommentsPopupState {
    /// The provider worker is loading comments.
    #[default]
    Loading,
    /// One or more comments are ready for display.
    Ready,
    /// The provider returned a successful empty result.
    Empty,
    /// The provider request failed without closing the popup.
    Error(String),
}

/// Scrollable public comments for one exact selected YouTube video.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct VideoCommentsPopupView {
    /// Stable provider video identifier that owns this popup.
    pub video_id: String,
    /// Human-readable selected video title.
    pub video_title: String,
    /// Explicit request/result state.
    pub state: VideoCommentsPopupState,
    /// At most twenty bounded public top-level comments.
    pub comments: Vec<VideoCommentView>,
    /// Zero-based wrapped-line offset at the top of the viewport.
    pub scroll_offset: usize,
}

/// Offline QR representation of one exact selected YouTube video.
#[cfg(feature = "qr")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VideoQrPopupView {
    /// Stable provider video identifier that owns this popup.
    pub video_id: String,
    /// Human-readable selected video title retained for controller ownership checks.
    pub video_title: String,
    /// Full canonical YouTube watch URL encoded into [`Self::matrix`].
    pub url: String,
    /// Provider-independent QR modules generated once when the popup opens.
    pub matrix: QrMatrix,
}

/// One source-control commit rendered in the offline-first project-history popup.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ProjectCommitView {
    /// Complete hexadecimal object identifier.
    pub hash: String,
    /// ISO-8601 commit timestamp retained from the build or GitHub response.
    pub committed_at: String,
    /// Complete multiline commit message, including its body and trailers.
    pub message: String,
}

/// State of the one-per-process GitHub history check.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub enum ProjectHistoryRemoteState {
    /// Only deterministic build-time history is available.
    #[default]
    Embedded,
    /// A background comparison against the repository's main branch is active.
    Checking,
    /// GitHub confirmed that no newer commits exist.
    UpToDate,
    /// Newer commits were merged into the process-local view.
    Updated,
    /// The online check failed; embedded history remains usable.
    Unavailable(String),
}

/// Scrollable recent-project-history popup and runtime installation facts.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ProjectHistoryPopupView {
    /// Newest-first embedded and optionally fetched commits, bounded to ten.
    pub commits: Vec<ProjectCommitView>,
    /// Full commit hash that produced this binary, when known.
    pub current_hash: Option<String>,
    /// Human-readable package/build origin.
    pub installation: String,
    /// Absolute executable path resolved by the running process.
    pub executable_path: String,
    /// Directory from which this process was launched.
    pub started_in: String,
    /// Optional source directory retained only for local builds.
    pub build_source: Option<String>,
    /// Status of the lazy online comparison.
    pub remote_state: ProjectHistoryRemoteState,
    /// Zero-based wrapped-line offset at the top of the viewport.
    pub scroll_offset: usize,
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
    /// Exact private credentials path where an official API key is stored.
    pub api_key_path: String,
    /// Exact general configuration path where an Invidious URL is stored.
    pub invidious_path: String,
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
            .field("api_key_path", &self.api_key_path)
            .field("invidious_path", &self.invidious_path)
            .field("validation_error", &self.validation_error)
            .finish()
    }
}

/// Masked OAuth-token editor for the optional Yandex Music source.
///
/// The token remains controller-owned while the popup is open. Rendering and
/// debug output expose only a fixed redaction marker.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct YandexMusicSetupPopupView {
    /// Yandex OAuth access token. The renderer never displays this value.
    pub token: String,
    /// Exact private credentials path where the token will be stored.
    pub token_path: String,
    /// A candidate token is being validated before it can replace durable state.
    pub validating: bool,
    /// Actionable validation or provider-construction failure, when present.
    pub validation_error: Option<String>,
}

impl std::fmt::Debug for YandexMusicSetupPopupView {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("YandexMusicSetupPopupView")
            .field("token", &"[REDACTED]")
            .field("token_path", &self.token_path)
            .field("validating", &self.validating)
            .field("validation_error", &self.validation_error)
            .finish()
    }
}

/// Selectable credential field in the YouTube provider setup popup.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
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
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
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

/// A live Radio stream capture that remains private until it is finalized.
///
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RadioRecordingView {
    /// Stable curated-station identifier owning this capture.
    pub station_id: String,
    /// Human-readable station name shown beside the recording marker.
    pub station_name: String,
}

/// Complete immutable view rendered for one frame.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ViewModel {
    /// Active screen.
    pub screen: Screen,
    /// Whether text typed by the user edits the search query.
    pub search_editing: bool,
    /// Current search query.
    pub search_query: String,
    /// UTF-8 byte position of the search editor's insertion cursor.
    pub search_cursor_byte: usize,
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
    /// Exact category queried by the Yandex Music tab.
    pub yandex_music_search_kind: YandexMusicSearchKind,
    /// Active recommendations, search, or album route in the Yandex Music tab.
    pub yandex_music_route: YandexMusicRouteView,
    /// Submitted provider search that has not reached a terminal response.
    pub search_activity: Option<SearchActivity>,
    /// Whether a foreground Local directory listing is awaiting its response.
    pub local_browse_pending: bool,
    /// Whether selected Local artwork is awaiting background extraction.
    pub local_artwork_pending: bool,
    /// Monotonic frame counter for explicit local-audio fingerprinting.
    pub local_fingerprint_animation_frame: usize,
    /// Monotonic frame counter for the active ASCII search animation.
    pub search_animation_frame: usize,
    /// Whether accepted media is waiting for authoritative playback start.
    pub playback_starting: bool,
    /// Monotonic frame counter for the ASCII playback-start animation.
    pub playback_start_animation_frame: usize,
    /// Media whose backend emitted `PlaybackStarted`, including while paused.
    pub playing_media_id: Option<MediaId>,
    /// Title and creator of the queue entry playback is on.
    ///
    /// Present whenever the queue has a current entry, which includes the
    /// moment before the backend has started it. See [`NowPlayingView`].
    pub now_playing: Option<NowPlayingView>,
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
    /// Selected Commons media control inside the expanded Wikidata spoiler.
    pub selected_wikidata_media: Option<usize>,
    /// Selected right-panel mode.
    pub right_panel_mode: RightPanelMode,
    /// Whether the local waveform replaces the normal player seek bar.
    pub waveform_visible: bool,
    /// Owner-aware local waveform generation and peak state.
    pub waveform: WaveformView,
    /// Whether the active backend loaded the exact file identity behind the waveform.
    pub waveform_playback_matches: bool,
    /// Current player state.
    pub playback: PlaybackStatus,
    /// Best-effort current programme or track for the playing radio station.
    ///
    /// Fresh provider metadata is preferred; the player may supply ICY
    /// metadata as a fallback without replacing the stable station title.
    pub radio_now_playing: Option<String>,
    /// Active original-quality Radio capture, if one is being staged privately.
    pub radio_recording: Option<RadioRecordingView>,
    /// Chapters inferred for the authoritative playing media.
    pub playback_chapters: Vec<Chapter>,
    /// Whether chapter labels include their timestamps.
    pub show_chapter_timestamps: bool,
    /// Whether exact `Реклама` chapters are hidden from navigation and skipped.
    pub skip_advertisement_chapters: bool,
    /// Local file-browser ordering by known file and lazy folder sizes.
    pub local_size_sort: LocalSizeSort,
    /// Whether Local includes unsupported regular files alongside media.
    pub show_all_local_files: bool,
    /// Ordering applied to the built-in Radio station catalogue.
    pub radio_sort: RadioSort,
    /// Whether recursive Local-folder sizes and size ordering are available.
    pub local_folder_sizes_enabled: bool,
    /// Whether a physical Linux TTY may render half-block artwork.
    pub show_images_in_tty: bool,
    /// Selected playable item for which quick and general playlist actions apply.
    pub playlist_item: Option<PlaylistItemView>,
    /// Whether the selected Playlists-screen row can open the shared editor.
    pub playlist_edit_available: bool,
    /// Whether the Playlists screen is showing entries that can return to its index.
    pub playlist_back_available: bool,
    /// Whether EOF continues with the next playable same-source list entry.
    pub autoplay: bool,
    /// Repeat-current-item state.
    pub repeating: bool,
    /// Status or error message.
    pub status_line: String,
    /// One short-lived notice that temporarily reserves the bottom row.
    ///
    /// This is intentionally separate from [`Self::status_line`]: routine
    /// status changes must not accidentally keep or replace a timed notice.
    pub transient_footer_notice: Option<String>,
    /// Whether the help overlay is open.
    pub help_open: bool,
    /// Offline-first recent commit history and runtime provenance.
    pub project_history_popup: Option<ProjectHistoryPopupView>,
    /// Whether this terminal attachment can launch a graphical external opener.
    pub external_opener_available: bool,
    /// Whether output is attached directly to a Linux virtual console.
    ///
    /// Linux consoles simulate italic and dim text by changing palette colors,
    /// so renderers use this flag to avoid unstable text styling.
    pub physical_linux_console: bool,
    /// Scrollable diagnostic popup, when a recoverable error is being reported.
    pub error_popup: Option<ErrorPopupView>,
    /// Whether this build can analyze effective local audio quality.
    pub audio_quality_supported: bool,
    /// Immediate progress and copyable results for local quality analysis.
    pub audio_quality_popup: Option<AudioQualityPopupView>,
    /// Whether the selected YouTube video supports loading public comments.
    pub video_comments_available: bool,
    /// Scrollable bounded public-comments popup.
    pub video_comments_popup: Option<VideoCommentsPopupView>,
    /// Offline QR code for the exact selected YouTube video.
    #[cfg(feature = "qr")]
    pub video_qr_popup: Option<VideoQrPopupView>,
    /// Editable provider setup shown after an unavailable YouTube operation.
    // Redacted: this editor holds a credential or private text, so only the one
    // bit saying it is open crosses. See the module header.
    #[serde(
        rename = "youtube_setup_open",
        serialize_with = "serialize_editor_presence"
    )]
    pub youtube_setup_popup: Option<YouTubeSetupPopupView>,
    /// Editable OAuth-token setup for the optional Yandex Music source.
    // Redacted: this editor holds a credential or private text, so only the one
    // bit saying it is open crosses. See the module header.
    #[serde(
        rename = "yandex_music_setup_open",
        serialize_with = "serialize_editor_presence"
    )]
    pub yandex_music_setup_popup: Option<YandexMusicSetupPopupView>,
    /// Focused RSS/Atom podcast-subscription editor.
    // Redacted: this editor holds a credential or private text, so only the one
    // bit saying it is open crosses. See the module header.
    #[serde(
        rename = "rss_subscription_open",
        serialize_with = "serialize_editor_presence"
    )]
    pub rss_subscription_popup: Option<RssSubscriptionPopupView>,
    /// Focused runtime preferences editor.
    pub preferences_popup: Option<PreferencesPopupView>,
    /// Focused local-playlist chooser or create/edit form.
    pub playlist_popup: Option<PlaylistPopupView>,
    /// Focused playback-queue list.
    pub queue_popup: Option<QueuePopupView>,
    /// Focused private-note editor.
    // Redacted: this editor holds a credential or private text, so only the one
    // bit saying it is open crosses. See the module header.
    #[serde(
        rename = "private_note_open",
        serialize_with = "serialize_editor_presence"
    )]
    pub private_note_popup: Option<PrivateNotePopupView>,
    /// Whether the current selection resolves to a note-capable exact target.
    pub private_note_available: bool,
    /// Selection-sensitive actions for the Yandex Music tab.
    pub yandex_music_actions: YandexMusicActionsView,
    /// Explicit rename, move, or recoverable Trash confirmation for a local file.
    pub local_file_popup: Option<LocalFilePopupView>,
    /// Active or most recently completed supervised download.
    pub download: Option<DownloadView>,
    /// Whether the controller has requested application shutdown.
    pub quitting: bool,
}

impl ViewModel {
    /// Reports whether expanded Details owns a renderable artwork source.
    ///
    /// Both front-ends need this to decide whether the expanded-artwork key is
    /// live, so it belongs to the view rather than to either renderer.
    #[must_use]
    pub fn expanded_thumbnail_available(&self) -> bool {
        let Some(details) = self
            .details
            .as_ref()
            .filter(|details| details.thumbnail_expanded)
        else {
            return false;
        };
        details.expanded_thumbnail_url.is_some()
            || details.thumbnail_url.is_some()
            || details.local_video_thumbnail.is_some()
            || details
                .expanded_wikidata_item
                .as_deref()
                .and_then(|item_id| {
                    details
                        .wikidata_entities
                        .iter()
                        .find(|entity| entity.item_id == item_id)
                })
                .is_some_and(|entity| entity.image_url.is_some())
    }
}

impl Default for ViewModel {
    fn default() -> Self {
        Self {
            screen: Screen::Search,
            search_editing: false,
            search_query: String::new(),
            search_cursor_byte: 0,
            local_path: String::new(),
            search_kind: SearchKind::Videos,
            youtube_search_sort: YouTubeSearchSort::Relevance,
            youtube_creative_commons_only: false,
            yandex_music_search_kind: YandexMusicSearchKind::All,
            yandex_music_route: YandexMusicRouteView::Recommendations,
            search_activity: None,
            local_browse_pending: false,
            local_artwork_pending: false,
            local_fingerprint_animation_frame: 0,
            search_animation_frame: 0,
            playback_starting: false,
            playback_start_animation_frame: 0,
            playing_media_id: None,
            now_playing: None,
            rows: Vec::new(),
            selected: 0,
            details: None,
            subscriptions: SubscriptionsView::default(),
            text_selection_mode: false,
            details_text_selection: None,
            details_focused: false,
            details_scroll: 0,
            selected_detail_link: None,
            selected_wikidata_media: None,
            right_panel_mode: RightPanelMode::Details,
            waveform_visible: false,
            waveform: WaveformView::Unavailable,
            waveform_playback_matches: false,
            playback: PlaybackStatus::default(),
            radio_now_playing: None,
            radio_recording: None,
            playback_chapters: Vec::new(),
            show_chapter_timestamps: false,
            skip_advertisement_chapters: true,
            local_size_sort: LocalSizeSort::Off,
            show_all_local_files: false,
            radio_sort: RadioSort::Name,
            local_folder_sizes_enabled: true,
            show_images_in_tty: true,
            playlist_item: None,
            playlist_edit_available: false,
            playlist_back_available: false,
            autoplay: false,
            repeating: false,
            status_line: "Press / to search or ? for help".to_owned(),
            transient_footer_notice: None,
            help_open: false,
            project_history_popup: None,
            external_opener_available: true,
            physical_linux_console: false,
            error_popup: None,
            audio_quality_supported: cfg!(feature = "audio-quality"),
            audio_quality_popup: None,
            video_comments_available: false,
            video_comments_popup: None,
            #[cfg(feature = "qr")]
            video_qr_popup: None,
            youtube_setup_popup: None,
            yandex_music_setup_popup: None,
            rss_subscription_popup: None,
            preferences_popup: None,
            playlist_popup: None,
            queue_popup: None,
            private_note_popup: None,
            private_note_available: false,
            yandex_music_actions: YandexMusicActionsView::default(),
            local_file_popup: None,
            download: None,
            quitting: false,
        }
    }
}

/// Relative or absolute movement inside a diagnostic error report.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
pub enum UiAction {
    /// Exit Youta after saving state.
    Quit,
    /// Record graphical-opener and physical-Linux-console capabilities.
    SetExternalOpenerAvailable(bool),
    /// Record the attached terminal window's current pixel dimensions.
    SetTerminalWindowPixels {
        /// Independently reported pixel width, when nonzero.
        width: Option<u16>,
        /// Independently reported pixel height, when nonzero.
        height: Option<u16>,
    },
    /// Report that F8 retained its keyboard pointer without physical GPM input.
    ReportGpmUnavailable {
        /// Whether this binary was compiled with the GPM input adapter.
        gpm_supported: bool,
        /// Whether OpenRC manages the system.
        openrc_managed: bool,
    },
    /// Open or close the help overlay.
    ToggleHelp,
    /// Open the offline-first recent project-history popup.
    OpenProjectHistory,
    /// Set the exact renderer-clamped project-history line offset.
    SetProjectHistoryScroll(usize),
    /// Close the project-history popup without changing the active screen.
    DismissProjectHistory,
    /// Switch to a top-level screen.
    ShowScreen(Screen),
    /// Enter search-query editing mode.
    BeginSearch,
    /// Cancel search-query editing.
    CancelSearch,
    /// Insert one character at the query cursor.
    AppendSearch(char),
    /// Move the query insertion cursor by one displayed grapheme.
    MoveSearchCursor(i8),
    /// Remove the query grapheme immediately before the insertion cursor.
    DeleteSearchCharacter,
    /// Delete the Vim-style word before the search cursor.
    DeleteSearchWord,
    /// Submit the current query.
    SubmitSearch,
    /// Switch the default YouTube search between videos and channels.
    ToggleSearchKind,
    /// Switch YouTube search between relevance and newest-first ordering.
    ToggleYouTubeSearchSort,
    /// Restrict `YouTube` video search to Creative Commons-licensed results.
    ToggleYouTubeCreativeCommons,
    /// Cycle Yandex Music search through music, podcasts, and audiobooks.
    CycleYandexMusicSearchKind,
    /// Toggle the selected Yandex Music track's liked state.
    ToggleYandexMusicLike,
    /// Toggle the selected Yandex Music track's disliked state.
    ToggleYandexMusicDislike,
    /// Open the selected row's primary artist inside Youta.
    OpenYandexMusicArtist,
    /// Open the selected track's album or selected album row.
    OpenYandexMusicAlbum,
    /// Open one exact Yandex Music artist selected from a Details link.
    OpenYandexMusicArtistById(String),
    /// Open one exact Yandex Music album selected from a Details link.
    OpenYandexMusicAlbumById(String),
    /// Open one exact `LibriVox` author selected from a Details link.
    OpenLibriVoxAuthorById(String),
    /// Download every track in the currently opened or selected album.
    DownloadYandexMusicAlbum,
    /// Download the first twenty current My Wave recommendations.
    DownloadTwentyYandexMusicRecommendations,
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
    /// Open one validated Wikidata item, identifier, Commons page, or
    /// Wikipedia article.
    OpenWikidataValue(String),
    /// Move the selected Commons media control inside expanded Wikidata.
    MoveWikidataMedia(i32),
    /// Start or pause one indexed Commons media value in expanded Wikidata.
    ActivateWikidataMedia(usize),
    /// Give or remove explicit keyboard focus from the Details panel.
    SetDetailsFocus(bool),
    /// Toggle artwork between its configured size and the remaining Details area.
    ToggleThumbnailExpansion,
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
    /// Seek within, or start, the exact local file generation owning a waveform.
    ActivateWaveformTimecode {
        /// Media identity captured when the waveform was rendered.
        media_id: MediaId,
        /// Controller generation identifying the waveform's exact file identity.
        generation: u64,
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
    /// Open or close the selected playable local file's waveform.
    ToggleWaveform,
    /// Fingerprint the selected local audio file and query AcoustID.
    FingerprintLocalAudio,
    /// Analyze the selected local audio file, marked entries, or selected folder.
    AnalyzeLocalAudioQuality,
    /// Cancel the active local audio-quality batch without discarding completed rows.
    CancelAudioQualityAnalysis,
    /// Copy the complete local audio-quality report through the front-end clipboard seam.
    CopyAudioQualityReport,
    /// Close a terminal local audio-quality result popup.
    DismissAudioQualityPopup,
    /// Set the renderer-clamped wrapped-line offset of the audio-quality report.
    SetAudioQualityPopupScroll(usize),
    /// Toggle repeat-current-item.
    ToggleRepeat,
    /// Toggle automatic continuation within the active source list.
    ToggleAutoplay,
    /// Cycle Local entry ordering through off, ascending, and descending size.
    ToggleLocalSizeSort,
    /// Toggle unsupported regular files in the Local listing.
    ToggleLocalAllFiles,
    /// Cycle Radio stations through name and known-bitrate orderings.
    CycleRadioSort,
    /// Toggle the selected Radio station in persistent favorites.
    ToggleRadioFavorite,
    /// Start or stop original-quality capture of the currently playing Radio station.
    ToggleRadioRecording,
    /// Show information about the playing channel.
    ShowChannel,
    /// Open the parent of the currently displayed Local directory.
    OpenLocalParent,
    /// Return to the previous internal Details page or seek position.
    GoBack,
    /// Move forward to a Details page previously left with [`Self::GoBack`].
    GoForward,
    /// Queue the selected item immediately after the current item.
    PlayNext,
    /// Add the selected item to the current queue.
    AddToQueue,
    /// Start the queue entry a signed number of steps from the current one.
    ///
    /// This is the transport "next track", and it is about the queue rather
    /// than about the list on screen: [`Self::PlayNext`] queues whatever the
    /// cursor is on, which is a different act and has no meaning at all on a
    /// surface that shows no list — a tray menu, or a keyboard's media keys.
    PlayQueueNeighbour(i32),
    /// Toggle the selected playable item in the reserved `todo` playlist.
    ToggleTodoPlaylist,
    /// Open the general playlist-membership chooser for the selected item.
    OpenPlaylistPopup,
    /// Move the playlist chooser selection by a signed row count.
    MovePlaylistPopupSelection(i32),
    /// Select one exact playlist-membership row.
    SelectPlaylistPopupRow(usize),
    /// Toggle membership in the selected chooser row without closing it.
    ToggleSelectedPlaylistMembership,
    /// Replace the chooser with an empty new-playlist editor.
    BeginNewPlaylist,
    /// Open the shared editor for the selected Playlists-screen row.
    EditSelectedPlaylist,
    /// Give one create/edit field keyboard focus.
    SelectPlaylistEditorField(PlaylistEditorField),
    /// Append one printable character to the focused playlist editor field.
    AppendPlaylistEditorCharacter(char),
    /// Remove the final character from the focused playlist editor field.
    DeletePlaylistEditorCharacter,
    /// Delete the Vim-style word before the focused playlist editor cursor.
    DeletePlaylistEditorWord,
    /// Create a playlist and add the selected playable item to it.
    CreatePlaylistAndAdd,
    /// Save display-name and optional-description changes to one stable playlist.
    UpdatePlaylist,
    /// Return from the editor to its chooser, or close the playlist popup.
    DismissPlaylistPopup,
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
    /// Insert one printable character into the private-note editor.
    AppendPrivateNoteCharacter(char),
    /// Insert a line break into the private-note editor.
    InsertPrivateNoteNewline,
    /// Remove the grapheme immediately before the private-note cursor.
    DeletePrivateNoteCharacter,
    /// Delete the Vim-style word immediately before the private-note cursor.
    DeletePrivateNoteWord,
    /// Set the first wrapped visual line shown by the private-note editor.
    SetPrivateNoteScroll(usize),
    /// Move the private-note cursor without modifying its body.
    MovePrivateNoteCursor(PrivateNoteCursorMotion),
    /// Persist the private-note draft for its exact target.
    SavePrivateNote,
    /// Enter or complete private-note deletion confirmation.
    RequestPrivateNoteDelete,
    /// Close the private-note editor without saving.
    DismissPrivateNotePopup,
    /// Open the playback queue.
    OpenQueuePopup,
    /// Move the queue selection by a signed row count.
    MoveQueuePopupSelection(i32),
    /// Select one exact queue row without playing it.
    SelectQueuePopupRow(usize),
    /// Move the queue cursor to one exact row and start playing it.
    ActivateQueuePopupRow(usize),
    /// Drop one exact queue row.
    RemoveQueuePopupRow(usize),
    /// Drop every queue entry that has not started playing.
    ClearQueue,
    /// Close the playback queue.
    DismissQueuePopup,
    /// Close the diagnostic error popup without changing the underlying screen.
    DismissErrorPopup,
    /// Open the official yt-dlp project page from its specialized 403 popup.
    OpenYtDlpProject,
    /// Open Gentoo's yt-dlp package page from its specialized 403 popup.
    OpenGentooYtDlpPackage,
    /// Open public top-level comments for the selected YouTube video.
    OpenVideoComments,
    /// Set the exact wrapped-line offset in the public-comments popup.
    SetVideoCommentsScroll(usize),
    /// Close the public-comments popup without changing Details.
    DismissVideoComments,
    /// Generate and show a QR code for the selected YouTube video.
    #[cfg(feature = "qr")]
    OpenVideoQr,
    /// Close the selected-video QR popup without changing Details.
    #[cfg(feature = "qr")]
    DismissVideoQr,
    /// Scroll the diagnostic report.
    ScrollErrorPopup(ErrorPopupScroll),
    /// Copy the complete diagnostic report.
    CopyErrorReport,
    /// Ask for confirmation before publishing the complete diagnostic report.
    RequestGitHubIssueSubmission,
    /// Submit the confirmed diagnostic report through the GitHub CLI.
    ConfirmGitHubIssueSubmission,
    /// Return to the diagnostic report without submitting it.
    CancelGitHubIssueSubmission,
    /// Open the created issue or the issue list for an uncertain submission.
    OpenGitHubIssueSubmissionTarget,
    /// Copy the report and open the repository's new-issue page.
    CopyAndOpenGitHubIssue,
    /// Select the credential field edited by the YouTube setup popup.
    SelectYouTubeSetupField(YouTubeSetupField),
    /// Add one printable character to the selected YouTube setup field.
    AppendYouTubeSetupCharacter(char),
    /// Remove the last character from the selected YouTube setup field.
    DeleteYouTubeSetupCharacter,
    /// Delete the Vim-style word before the selected setup-field cursor.
    DeleteYouTubeSetupWord,
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
    /// Add one printable character to the masked Yandex Music OAuth token.
    AppendYandexMusicTokenCharacter(char),
    /// Remove the final token character.
    DeleteYandexMusicTokenCharacter,
    /// Delete the Vim-style word before the token cursor.
    DeleteYandexMusicTokenWord,
    /// Open Yandex's official OAuth overview.
    OpenYandexOAuthGuide,
    /// Validate and save the Yandex Music OAuth token.
    SubmitYandexMusicSetup,
    /// Close the Yandex Music setup popup without saving.
    DismissYandexMusicSetup,
    /// Open or focus the RSS/Atom podcast-feed editor.
    OpenRssSubscriptionPopup,
    /// Add one printable character to the draft RSS feed URL.
    AppendRssSubscriptionCharacter(char),
    /// Remove the last character from the draft RSS feed URL.
    DeleteRssSubscriptionCharacter,
    /// Delete the Vim-style word before the draft RSS feed cursor.
    DeleteRssSubscriptionWord,
    /// Validate and persist the draft RSS subscription.
    SubmitRssSubscription,
    /// Close the RSS subscription popup without saving.
    DismissRssSubscriptionPopup,
    /// Open the focused runtime preferences editor.
    OpenPreferences,
    /// Select one draft Subscriptions layout in the preferences editor.
    SetSubscriptionsLayout(SubscriptionsLayout),
    /// Toggle hiding and skipping exact `Реклама` chapters in the draft.
    ToggleSkipAdvertisementChapters,
    /// Toggle selected-video YouTube prewarming in the draft.
    ToggleYouTubePrewarm,
    /// Cycle the exact YouTube thumbnail size in the draft.
    CycleYouTubeThumbnailSize,
    /// Toggle lazy recursive Local-folder size measurement in the draft.
    ToggleLocalFolderSizes,
    /// Toggle half-block artwork on a physical Linux TTY in the draft.
    ToggleTtyImages,
    /// Cycle the preferred Bandcamp playback encoding in the draft.
    CycleBandcampAudioFormat,
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
    /// Delete the Vim-style word immediately before the rename cursor.
    DeleteLocalRenameWord,
    /// Validate and execute the local rename.
    SubmitLocalRename,
    /// Ask for confirmation before moving the selected local entry to Trash.
    RequestLocalTrash,
    /// Move the selected local entry to recoverable system Trash.
    ConfirmLocalTrash,
    /// Ask for confirmation before moving the selected download to Trash.
    RequestDownloadedTrash,
    /// Move the selected download to recoverable system Trash.
    ConfirmDownloadedTrash,
    /// Open the destination chooser for marked entries or the current row.
    BeginLocalMove,
    /// Toggle one Local batch mark and move the selection by one signed row.
    ExtendLocalMoveSelection(i32),
    /// Select one exact row inside the Local Move destination browser.
    SelectLocalMoveDestination(usize),
    /// Move destination-browser selection by a signed row count.
    MoveLocalMoveDestination(i32),
    /// Open the selected parent or child directory in the Move popup.
    ActivateLocalMoveDestination,
    /// Move the validated source batch into the displayed destination.
    ConfirmLocalMoveHere,
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
    /// Include or exclude YouTube Shorts in the active subscription list.
    ToggleSubscriptionShorts,
    /// Show paths a system drag-and-drop delivered, in the Local browser.
    ///
    /// Only a windowed front-end can emit this: a terminal is not a drop
    /// target. The paths are the user's own selection from a file manager, so
    /// they are trusted exactly as far as a path typed into the input box is —
    /// and no further. They name a directory to *list*, which the Local browser
    /// would let the same user reach by walking there with the arrow keys, so
    /// this opens no door that Local does not already open. The controller
    /// still bounds the batch and reads nothing but directory entries.
    OpenDroppedPaths(Vec<PathBuf>),
}

impl UiAction {
    /// Returns whether this action exists only to launch an external URL.
    pub(crate) fn requires_external_opener(&self) -> bool {
        matches!(
            self,
            Self::ActivateDetailLink(_)
                | Self::OpenWikidataValue(_)
                | Self::OpenInBrowser
                | Self::OpenChannelInBrowser
                | Self::CopyAndOpenGitHubIssue
                | Self::OpenGitHubIssueSubmissionTarget
                | Self::OpenYtDlpProject
                | Self::OpenGentooYtDlpPackage
                | Self::OpenYouTubeApiKeyGuide
                | Self::OpenGoogleCloudCredentials
                | Self::OpenInvidiousInstances
                | Self::OpenYandexOAuthGuide
        )
    }
}

/// What one clipboard copy was about, so the controller can report it.
///
/// The wording stays in the controller because both front-ends show the same
/// status line; only the transport differs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardSubject {
    /// The canonical link of the selected item.
    Link,
    /// A range of Details text, measured in Unicode scalar values.
    DetailsText(usize),
    /// A local audio-quality report, measured in Unicode scalar values.
    AudioQualityReport(usize),
}

/// Text the controller decided to copy but deliberately does not copy itself.
///
/// The clipboard belongs to the front-end. A terminal reaches it through a
/// native helper or an OSC 52 escape written to its own tty; a desktop window
/// has the platform clipboard directly and has no tty to write an escape into,
/// so an escape there would be written into nothing and reported as success.
/// Routing every copy through this seam puts the choice where the knowledge is
/// and keeps the reducer free of platform code — the same split
/// [`TextFileOpenPlan`] already uses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClipboardRequest {
    /// Exact text to place on the clipboard.
    pub text: String,
    /// What was copied, used only to compose the status line.
    pub subject: ClipboardSubject,
}

/// Controller used by the generic terminal event loop.
pub trait UiController {
    /// Returns the view for the next frame.
    fn view(&self) -> &ViewModel;

    /// Applies one semantic user action.
    fn dispatch(&mut self, action: UiAction);

    /// Polls background workers and playback state.
    fn tick(&mut self);

    /// Takes one copy the front-end must place on the platform clipboard.
    fn take_clipboard_request(&mut self) -> Option<ClipboardRequest> {
        None
    }

    /// Reports the transport that accepted the copy, or why none did.
    fn report_clipboard_result(&mut self, _result: Result<String, String>) {}

    /// Takes one text-file command after an activation action planned it.
    #[cfg(feature = "local-browser")]
    fn take_text_file_open_plan(&mut self) -> Option<TextFileOpenPlan> {
        None
    }

    /// Reports the result after the event loop safely handled terminal state.
    #[cfg(feature = "local-browser")]
    fn report_text_file_open_result(&mut self, _result: Result<TextFileOpenLifecycle, String>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_shorts_are_visible_by_default() {
        assert!(SubscriptionsView::default().show_youtube_shorts);
    }

    /// Fills the four editors whose values must never leave this process.
    fn view_holding_every_secret() -> ViewModel {
        ViewModel {
            youtube_setup_popup: Some(YouTubeSetupPopupView {
                selected_field: YouTubeSetupField::ApiKey,
                api_key: "AIzaSyTOTALLY_SECRET_API_KEY_000000000".to_owned(),
                invidious_url: "https://inv.example.org/".to_owned(),
                api_key_path: "/config/secrets/credentials.toml".to_owned(),
                invidious_path: "/config/config.toml".to_owned(),
                validation_error: None,
            }),
            yandex_music_setup_popup: Some(YandexMusicSetupPopupView {
                token: "y0_TOTALLY_SECRET_OAUTH_TOKEN_111111".to_owned(),
                token_path: "/config/secrets/credentials.toml".to_owned(),
                validating: false,
                validation_error: None,
            }),
            rss_subscription_popup: Some(RssSubscriptionPopupView {
                url: "https://feeds.example.org/private/SECRET_FEED_GUID".to_owned(),
                validation_error: None,
                config_path: "/config/subscriptions.opml".to_owned(),
            }),
            private_note_popup: Some(PrivateNotePopupView {
                target_label: "Nocturne".to_owned(),
                body: "TOTALLY_SECRET_PRIVATE_NOTE_BODY".to_owned(),
                cursor_byte: 0,
                scroll_offset: 0,
                follow_cursor: true,
                existing: false,
                confirming_delete: false,
                storage_path: "/config/state/notes.toml".to_owned(),
                validation_error: None,
            }),
            ..ViewModel::default()
        }
    }

    /// Measures what the whole-snapshot IPC protocol actually costs.
    ///
    /// The plan proposes a sectioned diff with per-section generations, and
    /// requires this measurement before paying for that complexity. The numbers
    /// are printed so the decision rests on them, and bounded so the contract
    /// cannot quietly grow past the point where the answer changes.
    #[test]
    fn a_published_snapshot_stays_small_enough_to_send_whole() {
        let mut view = ViewModel::default();
        let empty = serde_json::to_vec(&view).expect("encode empty view").len();

        // A full screen of YouTube results, with the string lengths providers
        // actually return.
        view.rows = (0..50)
            .map(|index| RowView {
                title: format!("A reasonably long provider video title, number {index}"),
                subtitle: "Some Channel Name · 3 weeks ago · 1.2M views".to_owned(),
                source: "YouTube".to_owned(),
                thumbnail_url: url::Url::parse(&format!(
                    "https://i.ytimg.com/vi/dQw4w9WgXcQ{index:03}/hqdefault.jpg"
                ))
                .ok(),
                watched_percent: 42,
                ..RowView::default()
            })
            .collect();
        let listed = serde_json::to_vec(&view).expect("encode listed view").len();

        // Playback republishes on every position tick, four times a second, and
        // that is the protocol's worst realistic steady state.
        let per_second = listed * 4;
        println!(
            "snapshot: empty {empty} B, 50 rows {listed} B ({} B/row); \
             playback steady state ~{} KiB/s",
            (listed - empty) / 50,
            per_second / 1024
        );

        assert!(
            listed < 64 * 1024,
            "a 50-row snapshot grew to {listed} B; past roughly 64 KiB the \
             whole-snapshot protocol stops being reasonable and the sectioned \
             diff in the plan becomes necessary"
        );
    }

    #[test]
    fn serializing_the_view_never_carries_a_credential_or_private_note() {
        let serialized = serde_json::to_string(&view_holding_every_secret())
            .expect("the view model must serialize");

        for secret in [
            "AIzaSyTOTALLY_SECRET_API_KEY_000000000",
            "y0_TOTALLY_SECRET_OAUTH_TOKEN_111111",
            "SECRET_FEED_GUID",
            "TOTALLY_SECRET_PRIVATE_NOTE_BODY",
            // Paths are not credentials, but they name the file that holds one,
            // and an out-of-process frontend has no reason to learn either.
            "/config/secrets/credentials.toml",
            "/config/subscriptions.opml",
            "/config/state/notes.toml",
            "Nocturne",
        ] {
            assert!(
                !serialized.contains(secret),
                "a secret reached the serialized view model: {secret}"
            );
        }
    }

    /// The one bit a frontend needs is the one bit it gets.
    ///
    /// These editors are modal: while one is open the keyboard map sends every
    /// key into it. A frontend that cannot see that would render an ordinary
    /// screen that ignores input, so withholding this bit produces a worse
    /// failure than publishing it — and it is a boolean, not a projection, so
    /// no field of those editors can ever ride along with it.
    #[test]
    fn an_open_credential_editor_crosses_as_one_bit_and_nothing_else() {
        let encoded = serde_json::to_value(view_holding_every_secret())
            .expect("the view model must serialize");
        let closed =
            serde_json::to_value(ViewModel::default()).expect("the view model must serialize");

        for marker in [
            "youtube_setup_open",
            "yandex_music_setup_open",
            "rss_subscription_open",
            "private_note_open",
        ] {
            assert_eq!(
                encoded.get(marker),
                Some(&serde_json::Value::Bool(true)),
                "{marker} must report an open editor as a bare boolean"
            );
            assert_eq!(
                closed.get(marker),
                Some(&serde_json::Value::Bool(false)),
                "{marker} must report a closed editor as a bare boolean"
            );
        }
    }

    #[test]
    fn serializing_the_view_still_carries_ordinary_state() {
        let view = ViewModel {
            search_query: "nocturne op 9".to_owned(),
            status_line: "Playing".to_owned(),
            ..ViewModel::default()
        };
        let serialized = serde_json::to_string(&view).expect("the view model must serialize");

        assert!(serialized.contains("nocturne op 9"));
        assert!(serialized.contains("Playing"));
    }

    #[test]
    fn yt_dlp_forbidden_loading_state_uses_only_fixed_project_links() {
        let popup = YtDlpForbiddenView::loading(Some("amd64".to_owned()));

        assert_eq!(popup.project_url, YT_DLP_PROJECT_URL);
        assert_eq!(popup.installed, YtDlpVersionLookupView::Loading);
        assert_eq!(popup.github_latest, YtDlpVersionLookupView::Loading);
        let gentoo = popup.gentoo.expect("Gentoo metadata row");
        assert_eq!(gentoo.arch, "amd64");
        assert_eq!(gentoo.package_url, GENTOO_YT_DLP_PACKAGE_URL);
        assert_eq!(gentoo.latest_stable, YtDlpVersionLookupView::Loading);
    }

    #[test]
    fn yt_dlp_popup_links_require_a_graphical_external_opener() {
        assert!(UiAction::OpenYtDlpProject.requires_external_opener());
        assert!(UiAction::OpenGentooYtDlpPackage.requires_external_opener());
        assert!(!UiAction::CopyErrorReport.requires_external_opener());
    }

    #[test]
    fn every_action_survives_a_round_trip_to_json() {
        let actions = [
            UiAction::TogglePause,
            UiAction::SeekRelative(-5),
            UiAction::MoveSelection(1),
            UiAction::SubmitSearch,
            UiAction::SeekPercent(42.5),
            UiAction::AnalyzeLocalAudioQuality,
            UiAction::CancelAudioQualityAnalysis,
            UiAction::CopyAudioQualityReport,
            UiAction::DismissAudioQualityPopup,
            UiAction::SetAudioQualityPopupScroll(3),
        ];
        for action in actions {
            let encoded = serde_json::to_string(&action).expect("actions must serialize");
            assert!(!encoded.is_empty(), "{action:?} produced no payload");
        }
    }

    #[test]
    fn audio_quality_popup_serializes_copyable_batch_progress() {
        let view = ViewModel {
            audio_quality_supported: true,
            audio_quality_popup: Some(AudioQualityPopupView {
                title: "Audio quality analysis".to_owned(),
                summary: "Analyzing 2 audio files".to_owned(),
                completed: 1,
                total: 2,
                report: "one.flac\tVerdict: band-limited audio".to_owned(),
                action_status: Some("Copied with OSC 52".to_owned()),
                pending: true,
                scroll_offset: 4,
            }),
            ..ViewModel::default()
        };

        let json = serde_json::to_value(view).expect("serialize audio-quality popup");
        assert_eq!(json["audio_quality_supported"], true);
        assert_eq!(json["audio_quality_popup"]["completed"], 1);
        assert_eq!(json["audio_quality_popup"]["total"], 2);
        assert_eq!(json["audio_quality_popup"]["pending"], true);
        assert_eq!(json["audio_quality_popup"]["scroll_offset"], 4);
        assert_eq!(
            json["audio_quality_popup"]["action_status"],
            "Copied with OSC 52"
        );
        assert_eq!(
            json["audio_quality_popup"]["report"],
            "one.flac\tVerdict: band-limited audio"
        );
    }
}
