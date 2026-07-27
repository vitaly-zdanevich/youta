//! Provider-neutral data types used by Youta's user interface and storage.
//!
//! The types in this module deliberately contain no network or playback
//! implementation details. This keeps provider adapters, terminal rendering,
//! and the `SQLite` store independently testable.

use std::fmt;

use serde::{Deserialize, Serialize};
use url::Url;

/// The watched fraction at which an item is considered played.
pub const PLAYED_THRESHOLD: f64 = 0.90;

/// The watched percentage at which an item is considered played.
pub const PLAYED_THRESHOLD_PERCENT: u8 = 90;

/// The amount rewound when resuming an interrupted item.
pub const DEFAULT_RESUME_REWIND_SECONDS: u64 = 30;

/// A stable identifier for a source understood by Youta.
#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    /// `YouTube` through either its official API, Invidious, or `yt-dlp`.
    #[default]
    YouTube,
    /// An RSS or Atom podcast feed.
    Rss,
    /// `Apple Podcasts` discovery data.
    ApplePodcasts,
    /// A local file or directory.
    Local,
    /// Wikimedia Commons.
    WikimediaCommons,
    /// Internet Archive.
    ArchiveOrg,
    /// `LibriVox`.
    LibriVox,
    /// Yandex Music.
    YandexMusic,
    /// Bandcamp.
    Bandcamp,
    /// Odysee.
    Odysee,
    /// Rumble.
    Rumble,
    /// Bilibili.
    Bilibili,
    /// A federated `PeerTube` instance.
    PeerTube,
    /// A federated Funkwhale audio instance.
    Funkwhale,
    /// `Vimeo`.
    Vimeo,
    /// `RuTube`.
    RuTube,
    /// `SoundCloud`.
    SoundCloud,
    /// Jamendo's openly licensed music catalogue.
    Jamendo,
    /// `SoundStream`'s podcast catalogue.
    SoundStream,
    /// `LitRes` podcasts, limited to publicly accessible catalogues and media.
    LitRes,
    /// BBC radio and programme feeds.
    BbcRadio,
    /// The Mod Archive.
    ModArchive,
    /// A URL handled by `yt-dlp` without a first-class source adapter.
    GenericYtDlp,
    /// VK.
    Vk,
    /// Telegram.
    Telegram,
    /// An internet-radio directory or station.
    Radio,
    /// A remote file store such as `WebDAV`, `SSH`, or a cloud drive.
    RemoteFiles,
    /// A source supplied by a plugin or a future Youta version.
    Other(String),
}

impl SourceKind {
    /// Returns the stable lowercase name used in persistence and logs.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::YouTube => "youtube",
            Self::Rss => "rss",
            Self::ApplePodcasts => "apple-podcasts",
            Self::Local => "local",
            Self::WikimediaCommons => "wikimedia-commons",
            Self::ArchiveOrg => "archive-org",
            Self::LibriVox => "librivox",
            Self::YandexMusic => "yandex-music",
            Self::Bandcamp => "bandcamp",
            Self::Odysee => "odysee",
            Self::Rumble => "rumble",
            Self::Bilibili => "bilibili",
            Self::PeerTube => "peertube",
            Self::Funkwhale => "funkwhale",
            Self::Vimeo => "vimeo",
            Self::RuTube => "rutube",
            Self::SoundCloud => "soundcloud",
            Self::Jamendo => "jamendo",
            Self::SoundStream => "soundstream",
            Self::LitRes => "litres",
            Self::BbcRadio => "bbc-radio",
            Self::ModArchive => "mod-archive",
            Self::GenericYtDlp => "generic-yt-dlp",
            Self::Vk => "vk",
            Self::Telegram => "telegram",
            Self::Radio => "radio",
            Self::RemoteFiles => "remote-files",
            Self::Other(name) => name,
        }
    }

    /// Returns Youta's built-in capability matrix for this source.
    ///
    /// Runtime provider probes may narrow these capabilities, for example when
    /// an Invidious instance disables comments.
    #[must_use]
    pub fn capabilities(&self) -> SourceCapabilities {
        match self {
            Self::YouTube => SourceCapabilities {
                search: true,
                video_details: true,
                pagination: true,
                filters: true,
                sorting: true,
                subscriptions: true,
                playlists: true,
                chapters: true,
                // Comments, captions, playback, downloads, and remote writes
                // depend on the chosen official/Invidious/yt-dlp adapter.
                // They remain false until that adapter reports them.
                ..SourceCapabilities::default()
            },
            Self::Rss | Self::ApplePodcasts | Self::SoundStream | Self::LitRes => {
                SourceCapabilities {
                    search: !matches!(self, Self::Rss),
                    subscriptions: true,
                    chapters: true,
                    download: true,
                    stream: true,
                    ..SourceCapabilities::default()
                }
            }
            Self::Jamendo => SourceCapabilities {
                search: true,
                subscriptions: true,
                playlists: true,
                download: true,
                stream: true,
                ..SourceCapabilities::default()
            },
            Self::Local | Self::RemoteFiles => SourceCapabilities {
                search: true,
                video_details: true,
                filters: true,
                sorting: true,
                playlists: true,
                chapters: true,
                stream: true,
                ..SourceCapabilities::default()
            },
            Self::Radio => SourceCapabilities {
                search: true,
                subscriptions: true,
                stream: true,
                ..SourceCapabilities::default()
            },
            Self::Funkwhale => SourceCapabilities {
                search: true,
                video_details: true,
                pagination: true,
                filters: true,
                subscriptions: true,
                playlists: true,
                download: true,
                stream: true,
                ..SourceCapabilities::default()
            },
            Self::PeerTube
            | Self::WikimediaCommons
            | Self::ArchiveOrg
            | Self::LibriVox
            | Self::YandexMusic
            | Self::Bandcamp
            | Self::Odysee
            | Self::Rumble
            | Self::Bilibili
            | Self::Vk
            | Self::Telegram => SourceCapabilities {
                search: true,
                video_details: true,
                pagination: true,
                filters: true,
                sorting: true,
                subscriptions: true,
                playlists: true,
                download: true,
                stream: true,
                ..SourceCapabilities::default()
            },
            Self::Vimeo | Self::RuTube | Self::SoundCloud | Self::BbcRadio | Self::GenericYtDlp => {
                SourceCapabilities {
                    video_details: true,
                    download: true,
                    stream: true,
                    ..SourceCapabilities::default()
                }
            }
            Self::ModArchive => SourceCapabilities {
                search: true,
                video_details: true,
                pagination: true,
                filters: true,
                download: true,
                stream: true,
                ..SourceCapabilities::default()
            },
            Self::Other(_) => SourceCapabilities::default(),
        }
    }
}

impl fmt::Display for SourceKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<&str> for SourceKind {
    fn from(value: &str) -> Self {
        match value {
            "youtube" => Self::YouTube,
            "rss" => Self::Rss,
            "apple-podcasts" => Self::ApplePodcasts,
            "local" => Self::Local,
            "wikimedia-commons" => Self::WikimediaCommons,
            "archive-org" => Self::ArchiveOrg,
            "librivox" => Self::LibriVox,
            "yandex-music" => Self::YandexMusic,
            "bandcamp" => Self::Bandcamp,
            "odysee" => Self::Odysee,
            "rumble" => Self::Rumble,
            "bilibili" => Self::Bilibili,
            "peertube" => Self::PeerTube,
            "funkwhale" => Self::Funkwhale,
            "vimeo" => Self::Vimeo,
            "rutube" => Self::RuTube,
            "soundcloud" => Self::SoundCloud,
            "jamendo" => Self::Jamendo,
            "soundstream" => Self::SoundStream,
            "litres" => Self::LitRes,
            "bbc-radio" => Self::BbcRadio,
            "mod-archive" => Self::ModArchive,
            "generic-yt-dlp" => Self::GenericYtDlp,
            "vk" => Self::Vk,
            "telegram" => Self::Telegram,
            "radio" => Self::Radio,
            "remote-files" => Self::RemoteFiles,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// Operations a source can expose to the rest of the application.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct SourceCapabilities {
    /// Full-text discovery is available.
    pub search: bool,
    /// Detailed metadata can be fetched for a single item.
    pub video_details: bool,
    /// Search or listing results can be paginated.
    pub pagination: bool,
    /// Server-side search filters are available.
    pub filters: bool,
    /// Server-side result sorting is available.
    pub sorting: bool,
    /// Sources can be subscribed to.
    pub subscriptions: bool,
    /// Playlists can be listed.
    pub playlists: bool,
    /// Comments can be read.
    pub comments_read: bool,
    /// Comments can be posted.
    pub comments_write: bool,
    /// Played state can be sent back to the service.
    pub played_state_write: bool,
    /// Captions or transcripts can be fetched.
    pub captions: bool,
    /// Chapters can be fetched.
    pub chapters: bool,
    /// Media can be downloaded.
    pub download: bool,
    /// Media can be streamed.
    pub stream: bool,
}

/// An identifier that remains unique across all configured sources.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct MediaId {
    /// The service or storage provider that owns the identifier.
    pub source: SourceKind,
    /// The provider's stable identifier, URL, or canonical local path.
    pub external_id: String,
}

impl MediaId {
    /// Creates a provider-qualified media identifier.
    #[must_use]
    pub fn new(source: SourceKind, external_id: impl Into<String>) -> Self {
        Self {
            source,
            external_id: external_id.into(),
        }
    }
}

/// The kind of an item returned by discovery or stored in a playlist.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MediaKind {
    /// A video whose audio, video, or both may be played.
    #[default]
    Video,
    /// An audio-first item such as a song or audiobook track.
    Audio,
    /// A podcast episode.
    PodcastEpisode,
    /// A live radio or event stream.
    LiveStream,
    /// A channel, artist, author, station, or podcast.
    Channel,
    /// A provider-owned playlist.
    Playlist,
    /// A directory used to group local or remote files.
    Folder,
}

/// Licensing metadata exposed by a source.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "name")]
pub enum MediaLicense {
    /// No licence information was supplied.
    #[default]
    Unknown,
    /// `YouTube`'s standard, non-free licence.
    YouTubeStandard,
    /// A Creative Commons licence, identified by its supplied name or URL.
    CreativeCommons(String),
    /// Public-domain material.
    PublicDomain,
    /// A provider-specific licence not known to this Youta build.
    Other(String),
}

impl MediaLicense {
    /// Whether the licence may be eligible for upload to Wikimedia Commons.
    ///
    /// This is only an initial UI gate. Upload code must still validate the
    /// exact licence and source attribution requirements.
    #[must_use]
    pub fn is_potentially_commons_compatible(&self) -> bool {
        matches!(self, Self::CreativeCommons(_) | Self::PublicDomain)
    }
}

/// View and reaction counts associated with a media item.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MediaStatistics {
    /// Number of provider-recorded views or listens.
    pub views: Option<u64>,
    /// Number of positive reactions.
    pub likes: Option<u64>,
}

/// A chapter or navigable timecode.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Chapter {
    /// Chapter title.
    pub title: String,
    /// Inclusive start offset in seconds.
    pub start_seconds: u64,
    /// Exclusive end offset, if known.
    pub end_seconds: Option<u64>,
}

/// A caption or transcript representation advertised by a provider.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CaptionTrack {
    /// BCP 47 language tag where supplied.
    pub language: String,
    /// Human-readable track name.
    pub label: Option<String>,
    /// URL used to retrieve the captions.
    pub url: Url,
    /// Whether captions were generated automatically.
    pub auto_generated: bool,
}

/// Provider-neutral metadata displayed by Youta.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MediaItem {
    /// Provider-qualified identifier.
    pub id: MediaId,
    /// Type of item.
    pub kind: MediaKind,
    /// Display title.
    pub title: String,
    /// Channel, artist, author, podcast, or station name.
    pub creator: Option<String>,
    /// Provider description, without interpreting embedded links.
    pub description: Option<String>,
    /// Canonical page that a browser should open.
    pub webpage_url: Url,
    /// Artwork suitable for a preview when the terminal supports it.
    pub thumbnail_url: Option<Url>,
    /// Total duration, when known.
    pub duration_seconds: Option<u64>,
    /// Publication time as seconds since the Unix epoch.
    pub published_at: Option<i64>,
    /// Provider view and reaction counts.
    pub statistics: MediaStatistics,
    /// Provider-supplied licensing information.
    pub license: MediaLicense,
    /// Navigable chapters.
    pub chapters: Vec<Chapter>,
    /// Available caption tracks.
    pub captions: Vec<CaptionTrack>,
}

/// A Wikidata item linked to an external media identifier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WikidataLink {
    /// Stable Wikidata entity identifier such as `Q60231842`.
    pub item_id: String,
    /// Best available localized label.
    pub label: String,
    /// Optional localized description.
    pub description: Option<String>,
    /// Canonical HTTPS Wikidata entity page.
    pub url: Url,
}

/// Which result types a search should return.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SearchScope {
    /// Return every supported result type.
    #[default]
    All,
    /// Return videos and audio items.
    Video,
    /// Return channels, artists, authors, stations, or podcasts.
    Channel,
    /// Return provider playlists.
    Playlist,
    /// Return podcast episodes and feeds.
    Podcast,
    /// Return audiobooks and audiobook authors.
    Audiobook,
}

/// Supported server-side result ordering.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SearchSort {
    /// Provider-defined relevance order.
    #[default]
    Relevance,
    /// Most viewed first.
    Views,
    /// Most recently published first.
    UploadDate,
    /// Highest provider rating first.
    Rating,
    /// Shortest duration first.
    DurationAscending,
    /// Longest duration first.
    DurationDescending,
}

/// Optional features a search result must have.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SearchFeature {
    /// Captions or a transcript are available.
    Captions,
    /// High-definition video is available.
    HighDefinition,
    /// The item is currently live.
    Live,
    /// A Creative Commons licence is declared.
    CreativeCommons,
    /// The item can be downloaded by the active provider.
    Downloadable,
}

/// Search restrictions shared by provider adapters.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SearchFilters {
    /// Earliest publication time, inclusive, as Unix seconds.
    pub published_after: Option<i64>,
    /// Latest publication time, inclusive, as Unix seconds.
    pub published_before: Option<i64>,
    /// Minimum duration in seconds.
    pub minimum_duration_seconds: Option<u64>,
    /// Maximum duration in seconds.
    pub maximum_duration_seconds: Option<u64>,
    /// Required item features.
    pub features: Vec<SearchFeature>,
    /// ISO 3166 region code used by providers that support regional search.
    pub region: Option<String>,
    /// Restrict results to these sources; an empty list means all sources.
    pub sources: Vec<SourceKind>,
    /// Restrict results to items Youta considers played.
    pub played: Option<bool>,
}

/// A normalized search request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SearchQuery {
    /// User-entered query text.
    pub text: String,
    /// Requested result type.
    pub scope: SearchScope,
    /// Optional restrictions.
    pub filters: SearchFilters,
    /// Requested ordering.
    pub sort: SearchSort,
    /// Maximum number of results requested from one provider page.
    pub limit: usize,
}

impl SearchQuery {
    /// Creates an unrestricted relevance search with a conservative page size.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            scope: SearchScope::All,
            filters: SearchFilters::default(),
            sort: SearchSort::Relevance,
            limit: 25,
        }
    }
}

/// Local playback progress for one media item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlaybackProgress {
    /// Item whose progress is recorded.
    pub media_id: MediaId,
    /// Last observed playback position.
    pub position_seconds: u64,
    /// Duration observed by the player or provider.
    pub duration_seconds: Option<u64>,
    /// Explicit user override of the derived played state.
    pub played_override: Option<bool>,
    /// Last update time as seconds since the Unix epoch.
    pub updated_at: i64,
}

impl PlaybackProgress {
    /// Creates progress at the beginning of an item.
    #[must_use]
    pub fn new(media_id: MediaId, duration_seconds: Option<u64>, updated_at: i64) -> Self {
        Self {
            media_id,
            position_seconds: 0,
            duration_seconds,
            played_override: None,
            updated_at,
        }
    }

    /// Updates and clamps the position to a known non-zero duration.
    pub fn record_position(&mut self, position_seconds: u64, updated_at: i64) {
        self.position_seconds = self
            .duration_seconds
            .filter(|duration| *duration > 0)
            .map_or(position_seconds, |duration| position_seconds.min(duration));
        self.updated_at = updated_at;
    }

    /// Returns a watched fraction in the inclusive range `0.0..=1.0`.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn watched_fraction(&self) -> f64 {
        self.duration_seconds
            .filter(|duration| *duration > 0)
            .map_or(0.0, |duration| {
                (self.position_seconds as f64 / duration as f64).clamp(0.0, 1.0)
            })
    }

    /// Returns an integer percentage suitable for compact terminal display.
    #[must_use]
    pub fn watched_percent(&self) -> u8 {
        let Some(duration) = self.duration_seconds.filter(|duration| *duration > 0) else {
            return 0;
        };
        let position = self.position_seconds.min(duration);
        let rounded =
            (u128::from(position) * 100 + u128::from(duration / 2)) / u128::from(duration);
        u8::try_from(rounded).unwrap_or(100)
    }

    /// Whether the item is played, honoring an override before the >90% rule.
    #[must_use]
    pub fn is_played(&self) -> bool {
        self.played_override.unwrap_or_else(|| {
            self.duration_seconds
                .filter(|duration| *duration > 0)
                .is_some_and(|duration| {
                    u128::from(self.position_seconds) * 100
                        > u128::from(duration) * u128::from(PLAYED_THRESHOLD_PERCENT)
                })
        })
    }

    /// Explicitly marks the item played or unplayed.
    pub fn set_played(&mut self, played: bool) {
        self.played_override = Some(played);
    }

    /// Restores automatic played-state calculation.
    pub fn clear_played_override(&mut self) {
        self.played_override = None;
    }

    /// Returns the resume point with Youta's default 30-second context rewind.
    #[must_use]
    pub fn resume_position(&self) -> u64 {
        self.resume_position_with_rewind(DEFAULT_RESUME_REWIND_SECONDS)
    }

    /// Returns a resume point rewound by `rewind_seconds`, saturating at zero.
    #[must_use]
    pub fn resume_position_with_rewind(&self, rewind_seconds: u64) -> u64 {
        self.position_seconds.saturating_sub(rewind_seconds)
    }
}

/// One appearance of an item in cross-source playback history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HistoryEntry {
    /// Database-assigned identifier. Use zero before insertion.
    pub id: i64,
    /// Played media.
    pub media_id: MediaId,
    /// Title captured at playback time for offline history display.
    pub title: String,
    /// First playback time for this entry, as Unix seconds.
    pub started_at: i64,
    /// Most recent playback time for this entry, as Unix seconds.
    pub last_played_at: i64,
    /// Last observed playback position.
    pub position_seconds: u64,
    /// Duration observed at playback time.
    pub duration_seconds: Option<u64>,
    /// Whether playback finished according to the configured completion rule.
    pub finished: bool,
}

/// An item in the ephemeral playback queue.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueueItem {
    /// Media to play.
    pub media: MediaItem,
    /// Local path or remote URL passed to the selected playback backend.
    ///
    /// This is separate from [`MediaItem::webpage_url`] because a provider's
    /// canonical browser page and its explicitly playable media URL may differ.
    pub playback_location: String,
    /// Explicit initial position supplied by an input link, when present.
    ///
    /// Persistent resume state is used when this field is `None`.
    pub start_at_seconds: Option<u64>,
    /// Time it was enqueued, as Unix seconds.
    pub added_at: i64,
}

/// The current playback queue and cursor.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlaybackQueue {
    /// Ordered queue entries.
    pub items: Vec<QueueItem>,
    /// Index of the active item, or `None` for an empty queue.
    pub current_index: Option<usize>,
    /// Repeat the current item indefinitely.
    pub repeat_one: bool,
}

impl PlaybackQueue {
    /// Adds an item to the end of the queue.
    pub fn push(&mut self, item: QueueItem) {
        let appended_index = self.items.len();
        self.items.push(item);
        if self.current_index.is_none() {
            // A non-empty queue with no cursor has already been exhausted.
            // Point at the newly appended entry instead of replaying old ones.
            self.current_index = Some(appended_index);
        }
    }

    /// Inserts an item directly after the active item.
    pub fn play_next(&mut self, item: QueueItem) {
        let index = self.current_index.map_or(self.items.len(), |current| {
            current.saturating_add(1).min(self.items.len())
        });
        self.items.insert(index, item);
        if self.current_index.is_none() {
            self.current_index = Some(index);
        }
    }

    /// Records an item that has started playing now.
    ///
    /// When `replace_active` is true, the active slot is replaced while
    /// preserving entries queued after it. When it is false, the new item is
    /// inserted before the first queued item. The latter is useful when items
    /// were enqueued before a player was started.
    pub fn begin_now(&mut self, item: QueueItem, replace_active: bool) {
        if replace_active
            && let Some(index) = self.current_index
            && let Some(slot) = self.items.get_mut(index)
        {
            *slot = item;
            return;
        }

        let index = self.current_index.unwrap_or(self.items.len());
        self.items.insert(index, item);
        self.current_index = Some(index);
    }

    /// Returns the active queue entry.
    #[must_use]
    pub fn current(&self) -> Option<&QueueItem> {
        self.current_index.and_then(|index| self.items.get(index))
    }

    /// Advances and returns the new current item, respecting repeat-one mode.
    pub fn advance(&mut self) -> Option<&QueueItem> {
        if self.repeat_one {
            return self.current();
        }
        let next = self.current_index?.saturating_add(1);
        if next < self.items.len() {
            self.current_index = Some(next);
            self.items.get(next)
        } else {
            self.current_index = None;
            None
        }
    }
}

/// Stable identifier for a local playlist.
pub type PlaylistId = String;

/// A user-selected lossless cut within an item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Segment {
    /// Stable local identifier.
    pub id: i64,
    /// Source media.
    pub media_id: MediaId,
    /// Inclusive start offset.
    pub start_seconds: u64,
    /// Exclusive end offset.
    pub end_seconds: u64,
    /// Optional display name.
    pub label: Option<String>,
    /// Time the segment was created, as Unix seconds.
    pub created_at: i64,
}

impl Segment {
    /// Creates a segment when the range is non-empty.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::EmptySegment`] when the end is not after the
    /// start.
    pub fn new(
        id: i64,
        media_id: MediaId,
        start_seconds: u64,
        end_seconds: u64,
        label: Option<String>,
        created_at: i64,
    ) -> Result<Self, DomainError> {
        if end_seconds <= start_seconds {
            return Err(DomainError::EmptySegment);
        }
        Ok(Self {
            id,
            media_id,
            start_seconds,
            end_seconds,
            label,
            created_at,
        })
    }

    /// Segment duration in seconds.
    #[must_use]
    pub fn duration_seconds(&self) -> u64 {
        self.end_seconds - self.start_seconds
    }
}

/// A playlist entry, optionally restricted to a saved segment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlaylistEntry {
    /// Source media.
    pub media_id: MediaId,
    /// Saved cut to play instead of the complete item.
    pub segment: Option<Segment>,
    /// Time the entry was added, as Unix seconds.
    pub added_at: i64,
}

/// A named local playlist.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Playlist {
    /// Stable local identifier.
    pub id: PlaylistId,
    /// User-visible name.
    pub name: String,
    /// Optional user description.
    pub description: Option<String>,
    /// Ordered entries.
    pub entries: Vec<PlaylistEntry>,
}

/// A recursively nested folder containing playlists.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PlaylistFolder {
    /// User-visible folder name.
    pub name: String,
    /// Playlists directly in this folder.
    pub playlists: Vec<Playlist>,
    /// Nested folders.
    pub folders: Vec<Self>,
}

/// A position bookmark on a media item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Bookmark {
    /// Database-assigned identifier. Use zero before insertion.
    pub id: i64,
    /// Bookmarked media.
    pub media_id: MediaId,
    /// Position in seconds.
    pub position_seconds: u64,
    /// Optional user label.
    pub label: Option<String>,
    /// Creation time as Unix seconds.
    pub created_at: i64,
}

/// A target for a private local comment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum CommentTarget {
    /// A complete media item.
    Media {
        /// Commented media.
        media_id: MediaId,
    },
    /// A position within a media item.
    Position {
        /// Commented media.
        media_id: MediaId,
        /// Commented position.
        position_seconds: u64,
    },
    /// A saved segment.
    Segment {
        /// Segment database identifier.
        segment_id: i64,
    },
    /// A subscribed channel, podcast, author, station, or folder.
    Subscription {
        /// Stable subscription identifier.
        subscription_id: String,
    },
}

/// A private comment stored only in Youta's application directory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PrivateComment {
    /// Database-assigned identifier. Use zero before insertion.
    pub id: i64,
    /// Object the comment annotates.
    pub target: CommentTarget,
    /// User-authored plain text.
    pub body: String,
    /// Creation time as Unix seconds.
    pub created_at: i64,
    /// Last edit time as Unix seconds.
    pub updated_at: i64,
}

/// Top-level terminal screens whose state can survive a restart.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "screen", content = "id")]
pub enum Screen {
    /// Search results and details.
    #[default]
    Search,
    /// Music-focused `YouTube Music` search results and details.
    YouTubeMusic,
    /// Local folders and supported media files.
    Local,
    /// Local subscription tree.
    Subscriptions,
    /// Offline media.
    Downloaded,
    /// Cross-source playback history.
    History,
    /// Nested user playlists and folders.
    Playlists,
    /// Current playback queue.
    Queue,
    /// Listening statistics.
    Statistics,
    /// Aggregated search across tracker-module archives.
    TrackerMusic,
    /// A named local playlist.
    Playlist(PlaylistId),
    /// A channel or equivalent source page.
    Channel(MediaId),
    /// Full-screen waveform navigation.
    Waveform,
}

/// Which part of a split screen receives keyboard input.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PanelFocus {
    /// Result list or navigation tree.
    #[default]
    Left,
    /// Metadata, description, or artwork panel.
    Right,
    /// Bottom player controls.
    Player,
}

/// Restart-safe terminal navigation and selection state.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionState {
    /// Currently visible screen.
    pub screen: Screen,
    /// Screens used by the back action.
    pub back_stack: Vec<Screen>,
    /// Focused panel.
    pub focus: PanelFocus,
    /// Selected media, when applicable.
    pub selected_media: Option<MediaId>,
    /// Selected zero-based row in the active list.
    pub selected_row: usize,
    /// Last selected row in the independent `YouTube` result list.
    #[serde(default)]
    pub youtube_selected_row: Option<usize>,
    /// Last selected row in the independent `YouTube Music` result list.
    #[serde(default)]
    pub youtube_music_selected_row: Option<usize>,
    /// Vertical scroll offset in the details panel.
    pub details_scroll: u64,
    /// Last search text.
    pub search_text: String,
    /// Last search text entered on the independent `YouTube Music` tab.
    #[serde(default)]
    pub youtube_music_search_text: String,
    /// Last canonical folder shown by the Local screen.
    #[serde(default)]
    pub local_path: Option<String>,
    /// Whether the waveform replaces the normal seek bar.
    pub waveform_visible: bool,
    /// Whether chapter timestamps are omitted from seek-bar labels.
    ///
    /// The stored value is the inverse of the visible UI setting so older
    /// session documents naturally retain the timestamped default.
    #[serde(default)]
    pub chapter_timestamps_hidden: bool,
}

impl SessionState {
    /// Navigates to a screen while recording the previous screen.
    pub fn navigate_to(&mut self, screen: Screen) {
        if self.screen != screen {
            self.back_stack
                .push(std::mem::replace(&mut self.screen, screen));
            self.selected_row = 0;
            self.details_scroll = 0;
        }
    }

    /// Navigates back and returns whether a previous screen existed.
    pub fn navigate_back(&mut self) -> bool {
        let Some(previous) = self.back_stack.pop() else {
            return false;
        };
        self.screen = previous;
        self.selected_row = 0;
        self.details_scroll = 0;
        true
    }
}

/// Invalid domain values rejected before reaching providers or persistence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DomainError {
    /// A segment's end must be after its start.
    #[error("a segment must end after it starts")]
    EmptySegment,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: &str) -> MediaId {
        MediaId::new(SourceKind::YouTube, value)
    }

    #[test]
    fn played_threshold_is_strict_and_can_be_overridden() {
        let mut progress = PlaybackProgress::new(id("video"), Some(100), 1);
        progress.record_position(89, 2);
        assert!(!progress.is_played());
        progress.record_position(90, 3);
        assert!(!progress.is_played());
        progress.record_position(91, 4);
        assert!(progress.is_played());

        progress.set_played(false);
        assert!(!progress.is_played());
        progress.clear_played_override();
        assert!(progress.is_played());
    }

    #[test]
    fn progress_clamps_and_resume_rewind_saturates() {
        let mut progress = PlaybackProgress::new(id("video"), Some(100), 1);
        progress.record_position(150, 2);
        assert_eq!(progress.position_seconds, 100);
        assert_eq!(progress.watched_percent(), 100);
        assert_eq!(progress.resume_position(), 70);

        progress.record_position(12, 3);
        assert_eq!(progress.resume_position(), 0);
    }

    #[test]
    fn queue_supports_play_next_and_repeat() {
        let media = |external_id: &str| QueueItem {
            media: MediaItem {
                id: id(external_id),
                kind: MediaKind::Video,
                title: external_id.to_owned(),
                creator: None,
                description: None,
                webpage_url: Url::parse("https://youtu.be/example").expect("valid URL"),
                thumbnail_url: None,
                duration_seconds: None,
                published_at: None,
                statistics: MediaStatistics::default(),
                license: MediaLicense::Unknown,
                chapters: Vec::new(),
                captions: Vec::new(),
            },
            playback_location: format!("https://youtu.be/{external_id}"),
            start_at_seconds: None,
            added_at: 1,
        };

        let mut queue = PlaybackQueue::default();
        queue.push(media("first"));
        queue.push(media("third"));
        queue.play_next(media("second"));
        assert_eq!(
            queue.advance().map(|item| item.media.title.as_str()),
            Some("second")
        );

        queue.repeat_one = true;
        assert_eq!(
            queue.advance().map(|item| item.media.title.as_str()),
            Some("second")
        );
    }

    #[test]
    fn queue_orders_append_and_play_next_around_the_current_item() {
        let item = |external_id: &str| QueueItem {
            media: MediaItem {
                id: id(external_id),
                kind: MediaKind::Audio,
                title: external_id.to_owned(),
                creator: None,
                description: None,
                webpage_url: Url::parse("https://media.example/item").expect("valid URL"),
                thumbnail_url: None,
                duration_seconds: None,
                published_at: None,
                statistics: MediaStatistics::default(),
                license: MediaLicense::Unknown,
                chapters: Vec::new(),
                captions: Vec::new(),
            },
            playback_location: format!("https://media.example/{external_id}.opus"),
            start_at_seconds: None,
            added_at: 1,
        };

        let mut queue = PlaybackQueue::default();
        queue.push(item("first"));
        queue.push(item("last"));
        queue.play_next(item("next-b"));
        queue.play_next(item("next-a"));

        assert_eq!(
            queue
                .items
                .iter()
                .map(|entry| entry.media.title.as_str())
                .collect::<Vec<_>>(),
            ["first", "next-a", "next-b", "last"]
        );
        assert_eq!(queue.current_index, Some(0));
    }

    #[test]
    fn queue_handles_items_added_before_playback_and_after_exhaustion() {
        let item = |external_id: &str| QueueItem {
            media: MediaItem {
                id: id(external_id),
                kind: MediaKind::Audio,
                title: external_id.to_owned(),
                creator: None,
                description: None,
                webpage_url: Url::parse("https://media.example/item").expect("valid URL"),
                thumbnail_url: None,
                duration_seconds: None,
                published_at: None,
                statistics: MediaStatistics::default(),
                license: MediaLicense::Unknown,
                chapters: Vec::new(),
                captions: Vec::new(),
            },
            playback_location: format!("https://media.example/{external_id}.opus"),
            start_at_seconds: None,
            added_at: 1,
        };

        let mut queue = PlaybackQueue::default();
        queue.push(item("queued"));
        queue.begin_now(item("manual"), false);
        assert_eq!(queue.current().unwrap().media.title, "manual");
        assert_eq!(queue.items[1].media.title, "queued");

        assert_eq!(queue.advance().unwrap().media.title, "queued");
        assert!(queue.advance().is_none());
        queue.push(item("after-end"));
        assert_eq!(queue.current().unwrap().media.title, "after-end");
    }

    #[test]
    fn segment_rejects_empty_ranges() {
        assert_eq!(
            Segment::new(1, id("video"), 20, 20, None, 1),
            Err(DomainError::EmptySegment)
        );
        let segment = Segment::new(1, id("video"), 20, 30, None, 1).expect("valid segment range");
        assert_eq!(segment.duration_seconds(), 10);
    }

    #[test]
    fn session_navigation_round_trips() {
        let mut state = SessionState::default();
        state.navigate_to(Screen::History);
        assert_eq!(state.screen, Screen::History);
        assert!(state.navigate_back());
        assert_eq!(state.screen, Screen::Search);
        assert!(!state.navigate_back());
    }

    #[test]
    fn older_sessions_default_to_visible_chapter_timestamps() {
        let mut encoded =
            serde_json::to_value(SessionState::default()).expect("encode session fixture");
        encoded
            .as_object_mut()
            .expect("session must encode as an object")
            .remove("chapter_timestamps_hidden");

        let restored: SessionState =
            serde_json::from_value(encoded).expect("decode pre-toggle session");

        assert!(!restored.chapter_timestamps_hidden);
    }

    #[test]
    fn unknown_source_names_round_trip() {
        let source = SourceKind::from("future-service");
        assert_eq!(source.as_str(), "future-service");
        assert_eq!(source.capabilities(), SourceCapabilities::default());
    }
}
