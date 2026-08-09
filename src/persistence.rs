//! Backend-neutral restart-safe state.
//!
//! [`StateStore`] selects deterministic TOML documents by default. Builds with
//! `sqlite-state` may instead select `SQLite` through
//! [`crate::config::PersistenceConfig`].

use std::collections::{HashMap, HashSet};
use std::ops::Deref;
#[cfg(any(
    feature = "sqlite-state",
    feature = "local-rename",
    feature = "local-move"
))]
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "sqlite-state")]
use std::time::Duration;

#[cfg(feature = "sqlite-state")]
use rusqlite::types::Type;
#[cfg(feature = "sqlite-state")]
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::{Config, ConfigError, PersistenceBackend};
#[cfg(all(test, feature = "sqlite-state"))]
use crate::domain::librivox_section_external_id;
use crate::domain::{
    BandcampReleaseKind, BandcampSearchSummary, Bookmark, CommentTarget, HistoryEntry, MediaId,
    MediaItem, MediaKind, PlaybackProgress, Playlist, PlaylistEntry, PlaylistId,
    PlaylistMediaSnapshot, PlaylistMembership, PlaylistSummary, PodcastShowSummary, PrivateComment,
    RADIO_FAVORITES_PLAYLIST_ID, RADIO_FAVORITES_PLAYLIST_NAME, SessionState, SourceKind,
    TODO_PLAYLIST_ID, TODO_PLAYLIST_NAME, WikidataLink, is_canonical_librivox_audio_url,
    is_canonical_librivox_book_url, parse_librivox_section_external_id,
    remote_url_has_non_public_host,
};
#[cfg(feature = "yandex-music")]
use crate::domain::{
    PendingYandexMusicReaction, YandexMusicReaction, YandexMusicReactionLedgerEntry,
};
#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
use crate::local_move::LocalIdentityRemapError;
#[cfg(any(feature = "local-rename", feature = "local-move"))]
use crate::local_move::{
    LocalMoveMapping, remap_local_media_id, remap_local_path_prefix, remap_local_replay_locator,
};
use crate::providers::{
    ChannelSummary, SearchItem, SearchRequest, SearchTarget, VideoOrientation,
    validate_youtube_video_id,
};

const MAX_SAVED_SEARCH_REQUEST_BYTES: usize = 16 * 1024;
const MAX_SAVED_SEARCH_RESULTS_BYTES: usize = 4 * 1024 * 1024;
const MAX_SAVED_MUSIC_QUERY_BYTES: usize = 512;
const MAX_SAVED_BANDCAMP_QUERY_BYTES: usize = 256;
const MAX_SAVED_BANDCAMP_RESULTS_BYTES: usize = 512 * 1024;
const MAX_SAVED_BANDCAMP_CANONICAL_URL_BYTES: usize = 512;
const MAX_SAVED_BANDCAMP_ARTWORK_URL_BYTES: usize = 4 * 1024;
const MAX_SAVED_BANDCAMP_TITLE_BYTES: usize = 512;
const MAX_SAVED_BANDCAMP_ARTIST_BYTES: usize = 256;
const MAX_SAVED_BANDCAMP_PAGE: u16 = 100;
const MAX_SAVED_APPLE_QUERY_BYTES: usize = 512;
const MAX_SAVED_APPLE_RESULTS_BYTES: usize = 2 * 1024 * 1024;
const MAX_SAVED_APPLE_URL_BYTES: usize = 16 * 1024;
const MAX_SAVED_APPLE_TEXT_BYTES: usize = 4 * 1024;
const MAX_SAVED_APPLE_GENRE_BYTES: usize = 512;
const MAX_SAVED_APPLE_GENRES: usize = 64;
const MAX_SAVED_APPLE_RESULTS: usize = 200;
const MAX_SAVED_SUBSCRIPTION_ITEMS_BYTES: usize = 512 * 1024;
const MAX_SAVED_SUBSCRIPTION_SOURCE_BYTES: usize = 128;
const MAX_SAVED_SUBSCRIPTION_SOURCE_ID_BYTES: usize = 2 * 1024;
const MAX_HISTORY_REPLAY_LOCATOR_BYTES: usize = 16 * 1024;
const MAX_PLAYLIST_ID_BYTES: usize = 128;
const MAX_PLAYLIST_EXTERNAL_ID_BYTES: usize = 16 * 1024;
const MAX_PLAYLIST_SNAPSHOT_BYTES: usize = 64 * 1024;
const MAX_PLAYLIST_SEGMENT_BYTES: usize = 16 * 1024;
const MAX_PLAYLIST_TITLE_BYTES: usize = 4 * 1024;
const MAX_PLAYLIST_CREATOR_BYTES: usize = 4 * 1024;
const MAX_PLAYLIST_URL_BYTES: usize = 16 * 1024;
/// Maximum UTF-8 byte length of a local playlist name.
pub const MAX_PLAYLIST_NAME_BYTES: usize = 256;
/// Maximum UTF-8 byte length of a local playlist description.
pub const MAX_PLAYLIST_DESCRIPTION_BYTES: usize = 16 * 1024;
/// Maximum number of local playlists accepted by the bounded list API.
pub const MAX_PLAYLISTS: usize = 1_024;
/// Maximum number of entries accepted in one local playlist.
pub const MAX_PLAYLIST_ENTRIES: usize = 10_000;
#[cfg(any(feature = "local-rename", feature = "local-move"))]
const MAX_LOCAL_MOVE_MAPPINGS: usize = 10_000;
#[cfg(feature = "yandex-music")]
const MAX_YANDEX_MUSIC_ACCOUNT_UID_BYTES: usize = 128;
#[cfg(feature = "yandex-music")]
const MAX_YANDEX_MUSIC_TRACK_ID_BYTES: usize = 256;
#[cfg(feature = "yandex-music")]
const MAX_YANDEX_MUSIC_REACTION_ROWS: usize = 10_000;
#[cfg(feature = "yandex-music")]
const YANDEX_MUSIC_REACTIONS_DOCUMENT: &str = "Yandex Music reactions";

/// Maximum number of `YouTube` summaries retained in one restart snapshot.
///
/// The application also uses this as its accumulated lazy-search limit so the
/// visible list and its durable representation cannot diverge.
pub const MAX_SAVED_YOUTUBE_SEARCH_RESULTS: usize = 500;

/// Maximum number of Bandcamp track/album summaries retained in one page.
pub const MAX_SAVED_BANDCAMP_SEARCH_RESULTS: usize = 64;

/// Maximum number of first-page items retained for one subscribed source.
pub const MAX_SAVED_SUBSCRIPTION_ITEMS: usize = 50;

/// Maximum number of recently refreshed subscribed sources retained on disk.
pub const MAX_SAVED_SUBSCRIPTION_SOURCES: usize = 32;

/// Indexed hot-path query for the playlists containing one complete media item.
///
/// Keeping this projection separate from playlist summaries avoids counting
/// every entry merely to render selection-dependent membership names.
#[cfg(feature = "sqlite-state")]
const PLAYLIST_MEMBERSHIPS_WITH_NAMES_SQL: &str = r"
	SELECT p.id, p.name
	FROM playlist_entries AS e INDEXED BY playlist_entries_media
	INNER JOIN playlists AS p ON p.id = e.playlist_id
	WHERE e.source = ?1 AND e.external_id = ?2 AND e.segment_json IS NULL
	ORDER BY p.name_key, p.id
	LIMIT ?3
	";

#[cfg(feature = "sqlite-state")]
const MIGRATIONS: &[&str] = &[
    r"
	CREATE TABLE schema_migrations (
		version INTEGER PRIMARY KEY,
		applied_at INTEGER NOT NULL
	);

	CREATE TABLE playback_progress (
		source TEXT NOT NULL,
		external_id TEXT NOT NULL,
		position_seconds INTEGER NOT NULL CHECK (position_seconds >= 0),
		duration_seconds INTEGER CHECK (duration_seconds IS NULL OR duration_seconds >= 0),
		played_override INTEGER CHECK (played_override IS NULL OR played_override IN (0, 1)),
		updated_at INTEGER NOT NULL,
		PRIMARY KEY (source, external_id)
	) WITHOUT ROWID;

	CREATE TABLE playback_history (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		source TEXT NOT NULL,
		external_id TEXT NOT NULL,
		title TEXT NOT NULL,
		started_at INTEGER NOT NULL,
		last_played_at INTEGER NOT NULL,
		position_seconds INTEGER NOT NULL CHECK (position_seconds >= 0),
		duration_seconds INTEGER CHECK (duration_seconds IS NULL OR duration_seconds >= 0),
		finished INTEGER NOT NULL CHECK (finished IN (0, 1))
	);
	CREATE INDEX playback_history_recent
		ON playback_history(last_played_at DESC, id DESC);
	CREATE INDEX playback_history_finished
		ON playback_history(finished, last_played_at DESC);

	CREATE TABLE private_comments (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		target_json TEXT NOT NULL,
		body TEXT NOT NULL,
		created_at INTEGER NOT NULL,
		updated_at INTEGER NOT NULL
	);
	CREATE INDEX private_comments_target ON private_comments(target_json);

	CREATE TABLE bookmarks (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		source TEXT NOT NULL,
		external_id TEXT NOT NULL,
		position_seconds INTEGER NOT NULL CHECK (position_seconds >= 0),
		label TEXT,
		created_at INTEGER NOT NULL
	);
	CREATE INDEX bookmarks_media
		ON bookmarks(source, external_id, position_seconds);

	CREATE TABLE session_state (
		slot TEXT PRIMARY KEY,
		state_json TEXT NOT NULL,
		updated_at INTEGER NOT NULL
	) WITHOUT ROWID;

	CREATE TABLE listen_totals (
		source TEXT PRIMARY KEY,
		total_seconds INTEGER NOT NULL CHECK (total_seconds >= 0)
	) WITHOUT ROWID;
	",
    r"
	CREATE TABLE metadata_cache (
		source TEXT NOT NULL,
		external_id TEXT NOT NULL,
		media_json TEXT NOT NULL,
		provider TEXT NOT NULL,
		source_url TEXT,
		fetched_at INTEGER NOT NULL,
		expires_at INTEGER,
		PRIMARY KEY (source, external_id)
	) WITHOUT ROWID;
    CREATE INDEX metadata_cache_expiry ON metadata_cache(expires_at);
	",
    r"
	CREATE TABLE wikidata_cache (
		property_id TEXT NOT NULL,
		external_id TEXT NOT NULL,
		items_json TEXT NOT NULL,
		fetched_at INTEGER NOT NULL,
		expires_at INTEGER NOT NULL,
		PRIMARY KEY (property_id, external_id)
	) WITHOUT ROWID;
	CREATE INDEX wikidata_cache_expiry ON wikidata_cache(expires_at);
	",
    r"
	CREATE TABLE youtube_search_state (
		slot INTEGER PRIMARY KEY CHECK (slot = 1),
		request_json TEXT NOT NULL,
		results_json TEXT NOT NULL,
		next_page INTEGER CHECK (
			next_page IS NULL OR next_page BETWEEN 1 AND 10000
		),
		updated_at INTEGER NOT NULL
	) WITHOUT ROWID;
	",
    r"
	CREATE TABLE channel_summary_cache (
		channel_id TEXT PRIMARY KEY,
		summary_json TEXT NOT NULL,
		fetched_at INTEGER NOT NULL,
		expires_at INTEGER NOT NULL
	) WITHOUT ROWID;
	CREATE INDEX channel_summary_cache_expiry
		ON channel_summary_cache(expires_at);
	",
    r"
	CREATE TABLE youtube_music_search_state (
		slot INTEGER PRIMARY KEY CHECK (slot = 1),
		query TEXT NOT NULL,
		results_json TEXT NOT NULL,
		updated_at INTEGER NOT NULL
	) WITHOUT ROWID;
	",
    r"
	CREATE TABLE subscription_items_cache (
		source TEXT NOT NULL CHECK (
			length(CAST(source AS BLOB)) BETWEEN 1 AND 128
		),
		source_id TEXT NOT NULL CHECK (
			length(CAST(source_id AS BLOB)) BETWEEN 1 AND 2048
		),
		items_json TEXT NOT NULL CHECK (
			length(CAST(items_json AS BLOB)) <= 524288
		),
		fetched_at INTEGER NOT NULL CHECK (fetched_at >= 0),
		PRIMARY KEY (source, source_id)
	) WITHOUT ROWID;
	CREATE INDEX subscription_items_cache_recency
		ON subscription_items_cache(fetched_at DESC, source, source_id);
	",
    r"
	ALTER TABLE playback_history ADD COLUMN replay_locator TEXT CHECK (
		replay_locator IS NULL OR length(CAST(replay_locator AS BLOB)) BETWEEN 1 AND 16384
	);
	",
    r"
	CREATE TABLE apple_podcasts_search_state (
		slot INTEGER PRIMARY KEY CHECK (slot = 1),
		query TEXT NOT NULL CHECK (
			length(CAST(query AS BLOB)) BETWEEN 1 AND 512
		),
		storefront TEXT NOT NULL CHECK (
			length(CAST(storefront AS BLOB)) = 2
			AND storefront GLOB '[a-z][a-z]'
		),
		results_json TEXT NOT NULL CHECK (
			length(CAST(results_json AS BLOB)) <= 2097152
		),
		updated_at INTEGER NOT NULL
	) WITHOUT ROWID;
	",
    r"
	CREATE TABLE bandcamp_search_state (
		slot INTEGER PRIMARY KEY CHECK (slot = 1),
		query TEXT NOT NULL CHECK (
			length(CAST(query AS BLOB)) BETWEEN 1 AND 256
		),
		page INTEGER NOT NULL CHECK (page BETWEEN 1 AND 100),
		results_json TEXT NOT NULL CHECK (
			length(CAST(results_json AS BLOB)) <= 524288
		),
		next_page INTEGER CHECK (
			next_page IS NULL
			OR (next_page = page + 1 AND next_page BETWEEN 2 AND 100)
		),
		updated_at INTEGER NOT NULL
	) WITHOUT ROWID;
	",
    r"
	CREATE TABLE local_move_journal (
		source_path TEXT PRIMARY KEY CHECK (
			length(CAST(source_path AS BLOB)) BETWEEN 1 AND 16384
		),
		target_path TEXT NOT NULL UNIQUE CHECK (
			length(CAST(target_path AS BLOB)) BETWEEN 1 AND 16384
		),
		created_at INTEGER NOT NULL
	) WITHOUT ROWID;
	",
    r"
	CREATE TABLE playlists (
		id TEXT PRIMARY KEY CHECK (
			length(CAST(id AS BLOB)) BETWEEN 1 AND 128
		),
		name TEXT NOT NULL CHECK (
			length(CAST(name AS BLOB)) BETWEEN 1 AND 256
		),
		name_key TEXT NOT NULL UNIQUE CHECK (
			length(CAST(name_key AS BLOB)) BETWEEN 1 AND 256
		),
		description TEXT CHECK (
			description IS NULL
			OR length(CAST(description AS BLOB)) <= 16384
		),
		created_at INTEGER NOT NULL CHECK (created_at >= 0),
		updated_at INTEGER NOT NULL CHECK (updated_at >= 0)
	) WITHOUT ROWID;

	CREATE TABLE playlist_entries (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		playlist_id TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
		position INTEGER NOT NULL CHECK (position >= 0),
		source TEXT NOT NULL CHECK (
			length(CAST(source AS BLOB)) BETWEEN 1 AND 128
		),
		external_id TEXT NOT NULL CHECK (
			length(CAST(external_id AS BLOB)) BETWEEN 1 AND 16384
		),
		snapshot_json TEXT NOT NULL CHECK (
			length(CAST(snapshot_json AS BLOB)) BETWEEN 2 AND 65536
		),
		segment_json TEXT CHECK (
			segment_json IS NULL
			OR length(CAST(segment_json AS BLOB)) BETWEEN 2 AND 16384
		),
		added_at INTEGER NOT NULL CHECK (added_at >= 0),
		UNIQUE (playlist_id, position)
	);
	CREATE UNIQUE INDEX playlist_whole_media_unique
		ON playlist_entries(playlist_id, source, external_id)
		WHERE segment_json IS NULL;
	CREATE INDEX playlist_entries_media
		ON playlist_entries(source, external_id, playlist_id);
	",
    r"
	CREATE TABLE yandex_music_reaction_outbox (
		account_uid TEXT NOT NULL CHECK (
			length(CAST(account_uid AS BLOB)) BETWEEN 1 AND 128
			AND account_uid NOT GLOB '*[^0-9A-Za-z._:-]*'
		),
		track_id TEXT NOT NULL CHECK (
			length(CAST(track_id AS BLOB)) BETWEEN 1 AND 256
			AND track_id NOT GLOB '*[^0-9A-Za-z._:-]*'
		),
		reaction TEXT NOT NULL CHECK (
			reaction IN ('neutral', 'liked', 'disliked')
		),
		generation INTEGER NOT NULL CHECK (generation > 0),
		updated_at INTEGER NOT NULL CHECK (updated_at >= 0),
		PRIMARY KEY (account_uid, track_id)
	) WITHOUT ROWID;
	",
    r"
	ALTER TABLE yandex_music_reaction_outbox
	ADD COLUMN acknowledged_generation INTEGER NOT NULL DEFAULT 0 CHECK (
		acknowledged_generation >= 0
		AND acknowledged_generation <= generation
	);
	",
];

/// Current on-disk schema version.
pub const SCHEMA_VERSION: u32 = 14;

/// One bounded `YouTube` search snapshot retained across application restarts.
///
/// Search summaries intentionally exclude lazily fetched video details,
/// subscriber enrichment, and Wikidata data. Those retain their independent
/// cache and request policies after restoration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SavedYouTubeSearch {
    /// Exact request that produced the most recently accepted page.
    pub request: SearchRequest,
    /// Accumulated video or channel summaries shown in the search list.
    pub results: Vec<SearchItem>,
    /// Next provider page to request when the user reaches the list boundary.
    pub next_page: Option<u32>,
}

/// One bounded `YouTube Music` search snapshot retained across restarts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SavedYouTubeMusicSearch {
    /// Exact trimmed query sent to the music-search adapter.
    pub query: String,
    /// Playable video summaries shown in the music result list.
    pub results: Vec<SearchItem>,
}

/// One bounded public Bandcamp search page retained across restarts.
///
/// Only canonical page identities and compact display metadata are stored.
/// Resolved or signed media URLs are intentionally excluded so restoring this
/// snapshot cannot consume a free-download allocation or replay stale media.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SavedBandcampSearch {
    /// Exact trimmed query sent to public Bandcamp search.
    pub query: String,
    /// One-based search page represented by `results`.
    pub page: u16,
    /// Canonical track and album summaries in provider order.
    pub results: Vec<BandcampSearchSummary>,
    /// Next sequential public page, when advertised.
    pub next_page: Option<u16>,
}

/// One bounded `Apple Podcasts` show-search snapshot retained across restarts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SavedApplePodcastsSearch {
    /// Exact trimmed query sent to Apple's public Search API.
    pub query: String,
    /// Lowercase two-letter Apple storefront used for the search.
    pub storefront: String,
    /// Compact show summaries in Apple's ranked order.
    pub results: Vec<PodcastShowSummary>,
}

/// A bounded first-page snapshot for one subscribed source.
///
/// Snapshots are intentionally stale-while-revalidate: callers may render
/// [`Self::items`] immediately, then refresh the source in the background and
/// replace the row. Only compact [`SearchItem`] summaries are stored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CachedSubscriptionItems {
    /// Provider family that owns the subscription.
    pub source: SourceKind,
    /// Stable channel, feed, or provider-specific subscription identifier.
    pub source_id: String,
    /// First-page playable item summaries in provider order.
    pub items: Vec<SearchItem>,
    /// Successful fetch completion time as seconds since the Unix epoch.
    pub fetched_at: i64,
}

/// Result of idempotently creating a case-insensitively named playlist.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaylistCreateOutcome {
    /// A new empty playlist was created.
    Created(PlaylistSummary),
    /// A playlist with the same case-insensitive name already existed.
    Existing(PlaylistSummary),
}

/// Result of idempotently adding a complete media item to one playlist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistAddOutcome {
    /// The item was appended to the end of the playlist.
    Added,
    /// The same provider-qualified media identity was already present.
    AlreadyPresent,
}

/// Result of toggling a complete media item's membership in one playlist.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaylistToggleOutcome {
    /// The absent item was appended.
    Added,
    /// The existing item was removed.
    Removed,
}

/// A provenance record attached to cached provider metadata.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MetadataProvenance {
    /// Adapter or endpoint that supplied the metadata, such as an Invidious
    /// instance hostname.
    pub provider: String,
    /// Exact request or canonical source URL when it is safe to retain.
    pub source_url: Option<Url>,
    /// Fetch completion time as seconds since the Unix epoch.
    pub fetched_at: i64,
    /// Optional expiration time as seconds since the Unix epoch.
    pub expires_at: Option<i64>,
}

/// Provider metadata cached together with where and when it came from.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CachedMetadata {
    /// Normalized media metadata.
    pub media: MediaItem,
    /// Origin and freshness metadata.
    pub provenance: MetadataProvenance,
}

/// Cached positive or empty Wikidata external-ID lookup.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CachedWikidataLookup {
    /// Wikidata property queried, such as `P1651` or `P2397`.
    pub property_id: String,
    /// Exact external identifier supplied to the query.
    pub external_id: String,
    /// Matching Wikidata items. An empty vector is a cacheable result.
    pub items: Vec<WikidataLink>,
    /// Fetch completion time as Unix seconds.
    pub fetched_at: i64,
    /// Expiration time as Unix seconds.
    pub expires_at: i64,
}

/// Restart-safe provider metadata for one `YouTube` channel.
///
/// The summary remains readable after it expires so the terminal can restore
/// channel artwork and headings without waiting for the network. Callers should
/// use [`Self::is_fresh_at`] to decide whether to schedule a background refresh.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CachedChannelSummary {
    /// Compact channel metadata shared by official-API and Invidious adapters.
    pub summary: ChannelSummary,
    /// Fetch completion time as seconds since the Unix epoch.
    pub fetched_at: i64,
    /// Expiration time as seconds since the Unix epoch.
    pub expires_at: i64,
}

impl CachedChannelSummary {
    /// Returns whether the cached summary may be reused without refreshing at
    /// `now`.
    #[must_use]
    pub const fn is_fresh_at(&self, now: i64) -> bool {
        self.expires_at > now
    }
}

impl CachedWikidataLookup {
    /// Returns whether the lookup may be reused at `now`.
    #[must_use]
    pub const fn is_fresh_at(&self, now: i64) -> bool {
        self.expires_at > now
    }
}

impl CachedMetadata {
    /// Whether the entry has not expired at `now`.
    #[must_use]
    pub fn is_fresh_at(&self, now: i64) -> bool {
        self.provenance
            .expires_at
            .is_none_or(|expires_at| expires_at > now)
    }
}

/// Listening time aggregated for one source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ListenTotal {
    /// Source whose playback time was counted.
    pub source: SourceKind,
    /// Exact accumulated value. The UI can display whole or fractional hours.
    pub total_seconds: u64,
}

/// Rows changed by one atomic durable Local move remap.
///
/// A successful report means every supplied mapping was applied to all
/// supported path-bearing state in one transaction. Callers may then discard
/// the mappings. On error the transaction is rolled back, so callers must keep
/// and retry the complete mapping slice rather than only a suffix.
#[cfg(any(feature = "local-rename", feature = "local-move"))]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LocalMoveStateRemap {
    /// Provider-qualified playback-progress identities changed.
    pub playback_progress: usize,
    /// History rows whose identity or stable replay path changed.
    pub playback_history: usize,
    /// Private media or position comments retargeted.
    pub private_comments: usize,
    /// Bookmark identities changed.
    pub bookmarks: usize,
    /// Saved terminal-session documents changed.
    pub sessions: usize,
    /// Cached Local metadata identities or file URLs changed.
    pub metadata_cache: usize,
    /// Cached Local folder identities or embedded item paths changed.
    pub subscription_items_cache: usize,
    /// Local playlist snapshots and their stable replay paths changed.
    pub playlist_entries: usize,
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
impl LocalMoveStateRemap {
    /// Returns the number of persistent rows changed across every table.
    #[must_use]
    pub const fn total(self) -> usize {
        self.playback_progress
            + self.playback_history
            + self.private_comments
            + self.bookmarks
            + self.sessions
            + self.metadata_cache
            + self.subscription_items_cache
            + self.playlist_entries
    }
}

/// A migrated `SQLite` connection for Youta state.
#[cfg(feature = "sqlite-state")]
struct SqliteStateStore {
    connection: Connection,
}

#[cfg(feature = "sqlite-state")]
impl SqliteStateStore {
    /// Opens the database derived from `config`, creating private directories as
    /// needed and enabling WAL mode.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory or database cannot be prepared,
    /// migrated, or switched to WAL mode.
    pub fn open(config: &Config) -> Result<Self, PersistenceError> {
        config.ensure_directories()?;
        Self::open_path(&config.database_file(), true)
    }

    /// Opens an in-memory migrated store for ephemeral sessions and tests.
    ///
    /// `SQLite` does not support WAL for a purely in-memory database, so this
    /// constructor uses the default memory journal.
    ///
    /// # Errors
    ///
    /// Returns an error if the in-memory schema cannot be migrated.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, PersistenceError> {
        Self::open_path(Path::new(":memory:"), false)
    }

    fn open_path(path: &Path, enable_wal: bool) -> Result<Self, PersistenceError> {
        let connection = Connection::open(path)?;
        connection.set_prepared_statement_cache_capacity(32);
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        if enable_wal {
            set_private_file_permissions(path)?;
            let mode: String =
                connection
                    .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
            if !mode.eq_ignore_ascii_case("wal") {
                return Err(PersistenceError::WalUnavailable(mode));
            }
            connection.pragma_update(None, "synchronous", "NORMAL")?;
        }

        run_migrations(&connection)?;
        Ok(Self { connection })
    }

    /// Returns the active `SQLite` journal mode.
    ///
    /// # Errors
    ///
    /// Returns an error if the journal-mode pragma cannot be queried.
    #[cfg(test)]
    pub fn journal_mode(&self) -> Result<String, PersistenceError> {
        Ok(self
            .connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))?)
    }

    /// Returns the database's applied schema version.
    ///
    /// # Errors
    ///
    /// Returns an error if the schema-version pragma cannot be queried.
    pub fn schema_version(&self) -> Result<u32, PersistenceError> {
        Ok(self
            .connection
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    /// Idempotently creates an empty local playlist.
    ///
    /// Names are compared through a Unicode-lowercase key while their original
    /// spelling is retained for display. Repeating the request with different
    /// casing returns [`PlaylistCreateOutcome::Existing`] and does not replace
    /// the stored description. The reserved `todo` name always addresses
    /// [`TODO_PLAYLIST_ID`], even after that playlist has been renamed.
    ///
    /// # Errors
    ///
    /// Returns an error when the name, description, timestamp, or playlist
    /// count violates a fixed bound, or when the transaction fails.
    pub fn create_playlist(
        &self,
        name: &str,
        description: Option<&str>,
        created_at: i64,
    ) -> Result<PlaylistCreateOutcome, PersistenceError> {
        let (name, name_key, description) =
            validated_playlist_fields(name, description, created_at)?;
        if is_radio_favorites_name_key(&name_key) {
            return Err(invalid_playlist(
                "the case-insensitive `Favorite radio stations` name is reserved for Radio favorites",
            ));
        }
        let transaction = self.connection.unchecked_transaction()?;
        if name_key == TODO_PLAYLIST_NAME {
            if let Some(summary) = playlist_summary_by_id(&transaction, TODO_PLAYLIST_ID)? {
                transaction.commit()?;
                return Ok(PlaylistCreateOutcome::Existing(summary));
            }
            if let Some(summary) = playlist_summary_by_name_key(&transaction, &name_key)? {
                transaction.commit()?;
                return Ok(PlaylistCreateOutcome::Existing(summary));
            }
            ensure_playlist_capacity(&transaction)?;
            insert_playlist(
                &transaction,
                TODO_PLAYLIST_ID,
                name,
                &name_key,
                description,
                created_at,
            )?;
            let summary = playlist_summary_by_id(&transaction, TODO_PLAYLIST_ID)?
                .ok_or_else(|| invalid_playlist("created todo playlist could not be read back"))?;
            transaction.commit()?;
            return Ok(PlaylistCreateOutcome::Created(summary));
        }
        if let Some(summary) = playlist_summary_by_name_key(&transaction, &name_key)? {
            transaction.commit()?;
            return Ok(PlaylistCreateOutcome::Existing(summary));
        }
        ensure_playlist_capacity(&transaction)?;
        let id = next_playlist_id(&transaction)?;
        transaction
            .prepare_cached(
                r"
				INSERT INTO playlists (
					id, name, name_key, description, created_at, updated_at
				) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
				",
            )?
            .execute(params![id, name, name_key, description, created_at])?;
        let summary = playlist_summary_by_id(&transaction, &id)?
            .ok_or_else(|| invalid_playlist("created playlist could not be read back"))?;
        transaction.commit()?;
        Ok(PlaylistCreateOutcome::Created(summary))
    }

    /// Atomically creates a playlist and adds its first complete media item.
    ///
    /// An existing case-insensitive name returns
    /// [`PlaylistCreateOutcome::Existing`] without changing that playlist.
    /// Invalid snapshot data, capacity failures, and insertion errors roll back
    /// the playlist row, so the popup workflow cannot leave an empty artifact.
    ///
    /// # Errors
    ///
    /// Returns an error under the combined validation and transaction
    /// conditions of [`Self::create_playlist`] and
    /// [`Self::add_playlist_entry`].
    pub fn create_playlist_with_entry(
        &self,
        name: &str,
        description: Option<&str>,
        media: &PlaylistMediaSnapshot,
        created_at: i64,
    ) -> Result<PlaylistCreateOutcome, PersistenceError> {
        let (name, name_key, description) =
            validated_playlist_fields(name, description, created_at)?;
        if is_radio_favorites_name_key(&name_key) {
            return Err(invalid_playlist(
                "the case-insensitive `Favorite radio stations` name is reserved for Radio favorites",
            ));
        }
        let snapshot_json = encoded_playlist_snapshot(media)?;
        let transaction = self.connection.unchecked_transaction()?;
        if name_key == TODO_PLAYLIST_NAME {
            if let Some(summary) = playlist_summary_by_id(&transaction, TODO_PLAYLIST_ID)? {
                transaction.commit()?;
                return Ok(PlaylistCreateOutcome::Existing(summary));
            }
            if let Some(summary) = playlist_summary_by_name_key(&transaction, &name_key)? {
                transaction.commit()?;
                return Ok(PlaylistCreateOutcome::Existing(summary));
            }
            ensure_playlist_capacity(&transaction)?;
            insert_playlist(
                &transaction,
                TODO_PLAYLIST_ID,
                name,
                &name_key,
                description,
                created_at,
            )?;
            let outcome = add_playlist_entry_in_transaction(
                &transaction,
                TODO_PLAYLIST_ID,
                media,
                &snapshot_json,
                created_at,
            )?;
            debug_assert_eq!(outcome, PlaylistAddOutcome::Added);
            let summary = playlist_summary_by_id(&transaction, TODO_PLAYLIST_ID)?
                .ok_or_else(|| invalid_playlist("created todo playlist could not be read back"))?;
            transaction.commit()?;
            return Ok(PlaylistCreateOutcome::Created(summary));
        }
        if let Some(summary) = playlist_summary_by_name_key(&transaction, &name_key)? {
            transaction.commit()?;
            return Ok(PlaylistCreateOutcome::Existing(summary));
        }
        ensure_playlist_capacity(&transaction)?;
        let id = next_playlist_id(&transaction)?;
        insert_playlist(&transaction, &id, name, &name_key, description, created_at)?;
        let outcome = add_playlist_entry_in_transaction(
            &transaction,
            &id,
            media,
            &snapshot_json,
            created_at,
        )?;
        debug_assert_eq!(outcome, PlaylistAddOutcome::Added);
        let summary = playlist_summary_by_id(&transaction, &id)?
            .ok_or_else(|| invalid_playlist("created playlist could not be read back"))?;
        transaction.commit()?;
        Ok(PlaylistCreateOutcome::Created(summary))
    }

    /// Updates one playlist's display name and optional description.
    ///
    /// The stable playlist ID is never changed. In particular, renaming the
    /// built-in todo playlist does not redirect the quick-add action.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid or duplicate fields, an unsafe identifier,
    /// an invalid timestamp, or a failed transaction.
    pub fn update_playlist(
        &self,
        playlist_id: &str,
        name: &str,
        description: Option<&str>,
        updated_at: i64,
    ) -> Result<Option<PlaylistSummary>, PersistenceError> {
        validate_playlist_id(playlist_id)?;
        let (name, name_key, description) =
            validated_playlist_fields(name, description, updated_at)?;
        if playlist_id == RADIO_FAVORITES_PLAYLIST_ID {
            return Err(invalid_playlist(
                "the hidden Radio favorites playlist cannot be edited",
            ));
        }
        if playlist_id != TODO_PLAYLIST_ID && name_key == TODO_PLAYLIST_NAME {
            return Err(invalid_playlist(
                "the case-insensitive `todo` name is reserved for the built-in todo playlist",
            ));
        }
        if is_radio_favorites_name_key(&name_key) {
            return Err(invalid_playlist(
                "the case-insensitive `Favorite radio stations` name is reserved for Radio favorites",
            ));
        }
        let transaction = self.connection.unchecked_transaction()?;
        let Some(existing) = playlist_summary_by_id(&transaction, playlist_id)? else {
            transaction.commit()?;
            return Ok(None);
        };
        if let Some(owner) = playlist_summary_by_name_key(&transaction, &name_key)?
            && owner.id != playlist_id
        {
            return Err(invalid_playlist(format!(
                "playlist name conflicts with existing playlist `{}`",
                owner.name
            )));
        }
        transaction
            .prepare_cached(
                r"
				UPDATE playlists
				SET name = ?1, name_key = ?2, description = ?3, updated_at = ?4
				WHERE id = ?5
				",
            )?
            .execute(params![
                name,
                name_key,
                description,
                updated_at,
                existing.id
            ])?;
        let summary = playlist_summary_by_id(&transaction, playlist_id)?
            .ok_or_else(|| invalid_playlist("updated playlist could not be read back"))?;
        transaction.commit()?;
        Ok(Some(summary))
    }

    /// Lists compact playlist metadata in case-insensitive name order.
    ///
    /// # Errors
    ///
    /// Returns an error when stored state exceeds [`MAX_PLAYLISTS`], contains
    /// invalid fields, or cannot be queried.
    pub fn playlists(&self) -> Result<Vec<PlaylistSummary>, PersistenceError> {
        playlist_summaries(&self.connection)
    }

    /// Loads one playlist and its entries in their explicit stored order.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe identifier, an oversized playlist,
    /// invalid stored snapshots, or a database failure.
    pub fn playlist(&self, playlist_id: &str) -> Result<Option<Playlist>, PersistenceError> {
        validate_playlist_id(playlist_id)?;
        let Some(summary) = playlist_summary_by_id(&self.connection, playlist_id)? else {
            return Ok(None);
        };
        let mut statement = self.connection.prepare_cached(
            r"
			SELECT source, external_id, snapshot_json, segment_json, added_at
			FROM playlist_entries
			WHERE playlist_id = ?1
			ORDER BY position, id
			LIMIT ?2
			",
        )?;
        let limit = i64::try_from(MAX_PLAYLIST_ENTRIES.saturating_add(1)).map_err(|_| {
            PersistenceError::IntegerOutOfRange {
                field: "playlist entry limit",
            }
        })?;
        let rows = statement
            .query_map(params![playlist_id, limit], playlist_entry_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        if rows.len() > MAX_PLAYLIST_ENTRIES {
            return Err(invalid_playlist(format!(
                "playlist `{playlist_id}` exceeds the {MAX_PLAYLIST_ENTRIES}-entry limit"
            )));
        }
        if rows.len() != summary.entry_count {
            return Err(invalid_playlist(format!(
                "playlist `{playlist_id}` entry count changed while it was being read"
            )));
        }
        Ok(Some(Playlist {
            id: summary.id,
            name: summary.name,
            description: summary.description,
            entries: rows,
        }))
    }

    /// Returns whether an exact provider-qualified item belongs to a playlist.
    ///
    /// A missing playlist is treated as empty, making membership checks
    /// idempotent during lazy todo creation.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identifiers or a failed query.
    pub fn playlist_contains(
        &self,
        playlist_id: &str,
        media_id: &MediaId,
    ) -> Result<bool, PersistenceError> {
        validate_playlist_id(playlist_id)?;
        validate_playlist_media_id(media_id)?;
        Ok(self
            .connection
            .prepare_cached(
                r"
				SELECT 1
				FROM playlist_entries
				WHERE playlist_id = ?1 AND source = ?2 AND external_id = ?3
				      AND segment_json IS NULL
				LIMIT 1
				",
            )?
            .query_row(
                params![playlist_id, media_id.source.as_str(), media_id.external_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Lists playlist IDs containing an exact complete media item.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid media identity, excessive stored
    /// playlist state, or a failed query.
    pub fn playlist_memberships(
        &self,
        media_id: &MediaId,
    ) -> Result<Vec<PlaylistId>, PersistenceError> {
        validate_playlist_media_id(media_id)?;
        let mut statement = self.connection.prepare_cached(
            r"
			SELECT playlist_id
			FROM playlist_entries
			WHERE source = ?1 AND external_id = ?2 AND segment_json IS NULL
			ORDER BY playlist_id
			LIMIT ?3
			",
        )?;
        let limit = i64::try_from(MAX_PLAYLISTS.saturating_add(1)).map_err(|_| {
            PersistenceError::IntegerOutOfRange {
                field: "playlist membership limit",
            }
        })?;
        let mut ids = statement
            .query_map(
                params![media_id.source.as_str(), media_id.external_id, limit],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        ids.retain(|playlist_id| !is_hidden_builtin_playlist_id(playlist_id));
        if ids.len() > MAX_PLAYLISTS {
            return Err(invalid_playlist(format!(
                "media membership exceeds the {MAX_PLAYLISTS}-playlist limit"
            )));
        }
        for id in &ids {
            validate_playlist_id(id)?;
        }
        Ok(ids)
    }

    /// Lists the stable IDs and current display names of playlists containing
    /// an exact complete media item.
    ///
    /// The query starts from the `playlist_entries_media` index and joins only matching
    /// playlist rows. It deliberately does not load descriptions or aggregate
    /// entry counts, making it suitable for selection-driven UI refreshes.
    /// Callers determine todo membership by comparing `playlist_id` with
    /// [`TODO_PLAYLIST_ID`], which remains correct after a user renames Todo.
    ///
    /// Saved segments do not make their parent media a complete-item member.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid media identity, excessive or invalid
    /// stored playlist membership state, or a failed query.
    pub fn playlist_memberships_with_names(
        &self,
        media_id: &MediaId,
    ) -> Result<Vec<PlaylistMembership>, PersistenceError> {
        validate_playlist_media_id(media_id)?;
        let mut statement = self
            .connection
            .prepare_cached(PLAYLIST_MEMBERSHIPS_WITH_NAMES_SQL)?;
        let limit = i64::try_from(MAX_PLAYLISTS.saturating_add(1)).map_err(|_| {
            PersistenceError::IntegerOutOfRange {
                field: "playlist membership limit",
            }
        })?;
        let mut memberships = statement
            .query_map(
                params![media_id.source.as_str(), media_id.external_id, limit],
                |row| {
                    Ok(PlaylistMembership {
                        playlist_id: row.get(0)?,
                        playlist_name: row.get(1)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        memberships.retain(|membership| !is_hidden_builtin_playlist_id(&membership.playlist_id));
        if memberships.len() > MAX_PLAYLISTS {
            return Err(invalid_playlist(format!(
                "media membership exceeds the {MAX_PLAYLISTS}-playlist limit"
            )));
        }
        for membership in &memberships {
            validate_playlist_id(&membership.playlist_id)?;
            validated_playlist_fields(&membership.playlist_name, None, 0)?;
        }
        Ok(memberships)
    }

    /// Idempotently appends one complete item to an existing playlist.
    ///
    /// Duplicate provider-qualified media identities keep their original
    /// position, timestamp, and captured metadata.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing playlist, invalid or oversized snapshot,
    /// full playlist, invalid timestamp, or failed transaction.
    pub fn add_playlist_entry(
        &self,
        playlist_id: &str,
        media: &PlaylistMediaSnapshot,
        added_at: i64,
    ) -> Result<PlaylistAddOutcome, PersistenceError> {
        validate_playlist_id(playlist_id)?;
        let snapshot_json = encoded_playlist_snapshot(media)?;
        validate_playlist_timestamp(added_at)?;
        let transaction = self.connection.unchecked_transaction()?;
        let outcome = add_playlist_entry_in_transaction(
            &transaction,
            playlist_id,
            media,
            &snapshot_json,
            added_at,
        )?;
        transaction.commit()?;
        Ok(outcome)
    }

    /// Idempotently removes one complete item from a playlist.
    ///
    /// Remaining positions are intentionally not renumbered; their order is
    /// stable and a later re-add appends after the greatest retained position.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identifiers, an invalid timestamp, or a
    /// failed transaction.
    pub fn remove_playlist_entry(
        &self,
        playlist_id: &str,
        media_id: &MediaId,
        updated_at: i64,
    ) -> Result<bool, PersistenceError> {
        validate_playlist_id(playlist_id)?;
        validate_playlist_media_id(media_id)?;
        validate_playlist_timestamp(updated_at)?;
        let transaction = self.connection.unchecked_transaction()?;
        let removed =
            remove_playlist_entry_in_transaction(&transaction, playlist_id, media_id, updated_at)?;
        transaction.commit()?;
        Ok(removed)
    }

    /// Atomically toggles one complete item's membership in a playlist.
    ///
    /// # Errors
    ///
    /// Returns an error under the same conditions as
    /// [`Self::add_playlist_entry`] or [`Self::remove_playlist_entry`].
    pub fn toggle_playlist_entry(
        &self,
        playlist_id: &str,
        media: &PlaylistMediaSnapshot,
        updated_at: i64,
    ) -> Result<PlaylistToggleOutcome, PersistenceError> {
        validate_playlist_id(playlist_id)?;
        let snapshot_json = encoded_playlist_snapshot(media)?;
        validate_playlist_timestamp(updated_at)?;
        let transaction = self.connection.unchecked_transaction()?;
        if playlist_contains_connection(&transaction, playlist_id, &media.id)? {
            remove_playlist_entry_in_transaction(&transaction, playlist_id, &media.id, updated_at)?;
            transaction.commit()?;
            return Ok(PlaylistToggleOutcome::Removed);
        }
        let outcome = add_playlist_entry_in_transaction(
            &transaction,
            playlist_id,
            media,
            &snapshot_json,
            updated_at,
        )?;
        debug_assert_eq!(outcome, PlaylistAddOutcome::Added);
        transaction.commit()?;
        Ok(PlaylistToggleOutcome::Added)
    }

    /// Lazily creates the fixed todo playlist and idempotently appends an item.
    ///
    /// Renaming the todo playlist does not change this method's target.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid snapshot data, a full store or playlist,
    /// invalid timestamp, or failed transaction.
    pub fn add_to_todo(
        &self,
        media: &PlaylistMediaSnapshot,
        added_at: i64,
    ) -> Result<PlaylistAddOutcome, PersistenceError> {
        let snapshot_json = encoded_playlist_snapshot(media)?;
        validate_playlist_timestamp(added_at)?;
        let transaction = self.connection.unchecked_transaction()?;
        ensure_todo_playlist(&transaction, added_at)?;
        let outcome = add_playlist_entry_in_transaction(
            &transaction,
            TODO_PLAYLIST_ID,
            media,
            &snapshot_json,
            added_at,
        )?;
        transaction.commit()?;
        Ok(outcome)
    }

    /// Atomically toggles membership in the fixed todo playlist.
    ///
    /// The playlist is created only when the operation needs to add an item.
    ///
    /// # Errors
    ///
    /// Returns an error under the same conditions as [`Self::add_to_todo`].
    pub fn toggle_todo(
        &self,
        media: &PlaylistMediaSnapshot,
        updated_at: i64,
    ) -> Result<PlaylistToggleOutcome, PersistenceError> {
        let snapshot_json = encoded_playlist_snapshot(media)?;
        validate_playlist_timestamp(updated_at)?;
        let transaction = self.connection.unchecked_transaction()?;
        if playlist_contains_connection(&transaction, TODO_PLAYLIST_ID, &media.id)? {
            remove_playlist_entry_in_transaction(
                &transaction,
                TODO_PLAYLIST_ID,
                &media.id,
                updated_at,
            )?;
            transaction.commit()?;
            return Ok(PlaylistToggleOutcome::Removed);
        }
        ensure_todo_playlist(&transaction, updated_at)?;
        let outcome = add_playlist_entry_in_transaction(
            &transaction,
            TODO_PLAYLIST_ID,
            media,
            &snapshot_json,
            updated_at,
        )?;
        debug_assert_eq!(outcome, PlaylistAddOutcome::Added);
        transaction.commit()?;
        Ok(PlaylistToggleOutcome::Added)
    }

    /// Returns whether an item belongs to the fixed todo playlist.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid media identity or failed query.
    pub fn todo_contains(&self, media_id: &MediaId) -> Result<bool, PersistenceError> {
        self.playlist_contains(TODO_PLAYLIST_ID, media_id)
    }

    /// Atomically toggles a station in the hidden Radio favorites playlist.
    ///
    /// The playlist is created only when the operation adds its first station.
    /// Its entries use normal playlist snapshots so file persistence stays
    /// human-readable and optional `SQLite` persistence requires no migration.
    ///
    /// # Errors
    ///
    /// Returns an error for non-Radio snapshots, invalid bounded data, a full
    /// store or playlist, an invalid timestamp, or a failed transaction.
    pub fn toggle_radio_favorite(
        &self,
        media: &PlaylistMediaSnapshot,
        updated_at: i64,
    ) -> Result<PlaylistToggleOutcome, PersistenceError> {
        validate_radio_favorite_snapshot(media)?;
        let snapshot_json = encoded_playlist_snapshot(media)?;
        validate_playlist_timestamp(updated_at)?;
        let transaction = self.connection.unchecked_transaction()?;
        if playlist_contains_connection(&transaction, RADIO_FAVORITES_PLAYLIST_ID, &media.id)? {
            remove_playlist_entry_in_transaction(
                &transaction,
                RADIO_FAVORITES_PLAYLIST_ID,
                &media.id,
                updated_at,
            )?;
            transaction.commit()?;
            return Ok(PlaylistToggleOutcome::Removed);
        }
        ensure_radio_favorites_playlist(&transaction, updated_at)?;
        let outcome = add_playlist_entry_in_transaction(
            &transaction,
            RADIO_FAVORITES_PLAYLIST_ID,
            media,
            &snapshot_json,
            updated_at,
        )?;
        debug_assert_eq!(outcome, PlaylistAddOutcome::Added);
        transaction.commit()?;
        Ok(PlaylistToggleOutcome::Added)
    }

    /// Returns whether a station belongs to the hidden Radio favorites playlist.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid media identity or failed query.
    pub fn radio_favorite_contains(&self, media_id: &MediaId) -> Result<bool, PersistenceError> {
        if media_id.source != SourceKind::Radio {
            return Ok(false);
        }
        self.playlist_contains(RADIO_FAVORITES_PLAYLIST_ID, media_id)
    }

    /// Lists unacknowledged Yandex Music reactions in canonical identity order.
    ///
    /// # Errors
    ///
    /// Returns an error when stored state is invalid, exceeds the fixed row
    /// bound, or cannot be queried.
    #[cfg(feature = "yandex-music")]
    pub fn pending_yandex_music_reactions(
        &self,
    ) -> Result<Vec<PendingYandexMusicReaction>, PersistenceError> {
        type ReactionColumns = (String, String, String, i64, i64, i64);

        let limit =
            i64::try_from(MAX_YANDEX_MUSIC_REACTION_ROWS.saturating_add(1)).map_err(|_| {
                PersistenceError::IntegerOutOfRange {
                    field: "Yandex Music reaction row limit",
                }
            })?;
        let mut statement = self.connection.prepare_cached(
            r"
				SELECT
					account_uid,
					track_id,
					reaction,
					generation,
					acknowledged_generation,
					updated_at
				FROM yandex_music_reaction_outbox
				WHERE acknowledged_generation < generation
				ORDER BY account_uid, track_id
			LIMIT ?1
			",
        )?;
        let rows = statement
            .query_map([limit], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })?
            .collect::<Result<Vec<ReactionColumns>, _>>()?;
        drop(statement);
        if rows.len() > MAX_YANDEX_MUSIC_REACTION_ROWS {
            return Err(PersistenceError::StateRowLimitReached {
                document: YANDEX_MUSIC_REACTIONS_DOCUMENT,
                maximum_rows: MAX_YANDEX_MUSIC_REACTION_ROWS,
            });
        }

        rows.into_iter()
            .map(
                |(
                    account_uid,
                    track_id,
                    reaction,
                    generation,
                    acknowledged_generation,
                    updated_at,
                )| {
                    let generation = u64::try_from(generation).map_err(|_| {
                        invalid_yandex_music_reaction(
                            "stored generation must be a positive persistent integer",
                        )
                    })?;
                    let acknowledged_generation =
                        u64::try_from(acknowledged_generation).map_err(|_| {
                            invalid_yandex_music_reaction(
                                "stored acknowledged generation cannot be negative",
                            )
                        })?;
                    let entry = YandexMusicReactionLedgerEntry {
                        account_uid,
                        track_id,
                        reaction: parse_yandex_music_reaction(&reaction)?,
                        generation,
                        acknowledged_generation,
                        updated_at,
                    };
                    validate_yandex_music_reaction_ledger_entry(&entry)?;
                    entry.pending_intent().ok_or_else(|| {
                        invalid_yandex_music_reaction(
                            "pending query returned an acknowledged ledger entry",
                        )
                    })
                },
            )
            .collect()
    }

    /// Queues one desired reaction with an atomic per-track next generation.
    ///
    /// Acknowledged rows remain in the ledger. The next generation therefore
    /// never reuses a revision that an older in-flight response may still carry.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid bounded data, a full ledger, generation
    /// exhaustion, or a failed transaction.
    #[cfg(feature = "yandex-music")]
    pub fn queue_yandex_music_reaction(
        &self,
        account_uid: &str,
        track_id: &str,
        reaction: YandexMusicReaction,
        updated_at: i64,
    ) -> Result<PendingYandexMusicReaction, PersistenceError> {
        validate_yandex_music_reaction_identity(account_uid, track_id)?;
        validate_yandex_music_reaction_timestamp(updated_at)?;
        let transaction = self.connection.unchecked_transaction()?;
        let current_generation = transaction
            .prepare_cached(
                r"
				SELECT generation
				FROM yandex_music_reaction_outbox
					WHERE account_uid = ?1 AND track_id = ?2
					",
            )?
            .query_row(params![account_uid, track_id], |row| row.get::<_, i64>(0))
            .optional()?;
        let generation = match current_generation {
            Some(current) => {
                let current = u64::try_from(current).map_err(|_| {
                    invalid_yandex_music_reaction(
                        "stored generation must be a positive persistent integer",
                    )
                })?;
                next_yandex_music_reaction_generation(current)?
            }
            None => 1,
        };
        let generation_sql = to_sql_u64(generation, "Yandex Music reaction generation")?;
        if current_generation.is_none() {
            let count: i64 = transaction.query_row(
                "SELECT count(*) FROM yandex_music_reaction_outbox",
                [],
                |row| row.get(0),
            )?;
            let maximum = i64::try_from(MAX_YANDEX_MUSIC_REACTION_ROWS).map_err(|_| {
                PersistenceError::IntegerOutOfRange {
                    field: "Yandex Music reaction row limit",
                }
            })?;
            if count >= maximum {
                return Err(PersistenceError::StateRowLimitReached {
                    document: YANDEX_MUSIC_REACTIONS_DOCUMENT,
                    maximum_rows: MAX_YANDEX_MUSIC_REACTION_ROWS,
                });
            }
        }
        if current_generation.is_some() {
            transaction
                .prepare_cached(
                    r"
					UPDATE yandex_music_reaction_outbox
					SET reaction = ?3, generation = ?4, updated_at = ?5
					WHERE account_uid = ?1 AND track_id = ?2
					",
                )?
                .execute(params![
                    account_uid,
                    track_id,
                    yandex_music_reaction_name(reaction),
                    generation_sql,
                    updated_at,
                ])?;
        } else {
            transaction
                .prepare_cached(
                    r"
					INSERT INTO yandex_music_reaction_outbox (
						account_uid,
						track_id,
						reaction,
						generation,
						acknowledged_generation,
						updated_at
					) VALUES (?1, ?2, ?3, ?4, 0, ?5)
					",
                )?
                .execute(params![
                    account_uid,
                    track_id,
                    yandex_music_reaction_name(reaction),
                    generation_sql,
                    updated_at,
                ])?;
        }
        transaction.commit()?;
        Ok(PendingYandexMusicReaction {
            account_uid: account_uid.to_owned(),
            track_id: track_id.to_owned(),
            reaction,
            generation,
            updated_at,
        })
    }

    /// Acknowledges only the current desired generation without deleting it.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid identity or generation, or when the
    /// compare-and-update query fails.
    #[cfg(feature = "yandex-music")]
    pub fn acknowledge_yandex_music_reaction(
        &self,
        account_uid: &str,
        track_id: &str,
        generation: u64,
    ) -> Result<bool, PersistenceError> {
        validate_yandex_music_reaction_identity(account_uid, track_id)?;
        validate_yandex_music_reaction_generation(generation)?;
        let generation = to_sql_u64(generation, "Yandex Music reaction generation")?;
        Ok(self
            .connection
            .prepare_cached(
                r"
					UPDATE yandex_music_reaction_outbox
					SET acknowledged_generation = generation
					WHERE account_uid = ?1
						AND track_id = ?2
						AND generation = ?3
						AND acknowledged_generation < generation
					",
            )?
            .execute(params![account_uid, track_id, generation])?
            == 1)
    }

    /// Saves one periodic playback checkpoint in a single transaction.
    ///
    /// # Errors
    ///
    /// Returns an error on integer overflow or when either transactional row
    /// update cannot be committed.
    pub fn checkpoint_playback(
        &self,
        progress: &PlaybackProgress,
        listen_source: &SourceKind,
        listened_seconds: u64,
    ) -> Result<(), PersistenceError> {
        let position = to_sql_u64(progress.position_seconds, "position_seconds")?;
        let duration = progress
            .duration_seconds
            .map(|value| to_sql_u64(value, "duration_seconds"))
            .transpose()?;
        let played_override = progress.played_override.map(i64::from);
        let listened_seconds = to_sql_u64(listened_seconds, "listen seconds")?;
        let transaction = self.connection.unchecked_transaction()?;
        transaction
            .prepare_cached(
                r"
				INSERT INTO playback_progress (
					source, external_id, position_seconds, duration_seconds,
					played_override, updated_at
				) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
				ON CONFLICT(source, external_id) DO UPDATE SET
					position_seconds = excluded.position_seconds,
					duration_seconds = excluded.duration_seconds,
					played_override = excluded.played_override,
					updated_at = excluded.updated_at
				",
            )?
            .execute(params![
                progress.media_id.source.as_str(),
                progress.media_id.external_id,
                position,
                duration,
                played_override,
                progress.updated_at,
            ])?;
        if listened_seconds > 0 {
            add_sqlite_listen_seconds(&transaction, listen_source, listened_seconds)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Saves listened time without creating playback progress.
    ///
    /// This is the periodic checkpoint for live or otherwise non-seekable
    /// streams. A zero-second delta is a no-op.
    ///
    /// # Errors
    ///
    /// Returns an error on integer overflow or when the statistics transaction
    /// cannot be committed.
    pub fn checkpoint_listening(
        &self,
        source: &SourceKind,
        listened_seconds: u64,
    ) -> Result<(), PersistenceError> {
        let listened_seconds = to_sql_u64(listened_seconds, "listen seconds")?;
        if listened_seconds == 0 {
            return Ok(());
        }
        let transaction = self.connection.unchecked_transaction()?;
        add_sqlite_listen_seconds(&transaction, source, listened_seconds)?;
        transaction.commit()?;
        Ok(())
    }

    /// Inserts or replaces the latest progress for an item.
    ///
    /// # Errors
    ///
    /// Returns an error on integer overflow or a database write failure.
    pub fn upsert_progress(&self, progress: &PlaybackProgress) -> Result<(), PersistenceError> {
        let position = to_sql_u64(progress.position_seconds, "position_seconds")?;
        let duration = progress
            .duration_seconds
            .map(|value| to_sql_u64(value, "duration_seconds"))
            .transpose()?;
        let played_override = progress.played_override.map(i64::from);
        self.connection
            .prepare_cached(
                r"
				INSERT INTO playback_progress (
					source, external_id, position_seconds, duration_seconds,
					played_override, updated_at
				) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
				ON CONFLICT(source, external_id) DO UPDATE SET
					position_seconds = excluded.position_seconds,
					duration_seconds = excluded.duration_seconds,
					played_override = excluded.played_override,
					updated_at = excluded.updated_at
				",
            )?
            .execute(params![
                progress.media_id.source.as_str(),
                progress.media_id.external_id,
                position,
                duration,
                played_override,
                progress.updated_at,
            ])?;
        Ok(())
    }

    /// Loads the latest progress for an item.
    ///
    /// # Errors
    ///
    /// Returns an error when the database row cannot be read.
    pub fn progress(
        &self,
        media_id: &MediaId,
    ) -> Result<Option<PlaybackProgress>, PersistenceError> {
        let result = self
            .connection
            .prepare_cached(
                r"
				SELECT source, external_id, position_seconds, duration_seconds,
					played_override, updated_at
				FROM playback_progress
				WHERE source = ?1 AND external_id = ?2
				",
            )?
            .query_row(
                params![media_id.source.as_str(), media_id.external_id],
                playback_progress_from_row,
            )
            .optional()?;
        Ok(result)
    }

    /// Loads progress for a bounded set of provider-qualified media IDs.
    ///
    /// Requests are chunked below `SQLite`'s conservative bind-variable limit,
    /// allowing directory views to hydrate all visible progress with a small
    /// number of queries instead of one query per row. Missing IDs are omitted
    /// from the returned map.
    ///
    /// # Errors
    ///
    /// Returns an error when a database row cannot be read.
    pub fn progress_for_media_ids(
        &self,
        media_ids: &[MediaId],
    ) -> Result<HashMap<MediaId, PlaybackProgress>, PersistenceError> {
        const IDS_PER_QUERY: usize = 400;

        let mut progress_by_id = HashMap::with_capacity(media_ids.len());
        for chunk in media_ids.chunks(IDS_PER_QUERY) {
            if chunk.is_empty() {
                continue;
            }
            let predicates = std::iter::repeat_n("(source = ? AND external_id = ?)", chunk.len())
                .collect::<Vec<_>>()
                .join(" OR ");
            let statement = format!(
                "SELECT source, external_id, position_seconds, duration_seconds, \
				 played_override, updated_at FROM playback_progress WHERE {predicates}"
            );
            let parameters = chunk.iter().flat_map(|media_id| {
                [
                    media_id.source.as_str().to_owned(),
                    media_id.external_id.clone(),
                ]
            });
            let mut query = self.connection.prepare(&statement)?;
            let rows = query.query_map(params_from_iter(parameters), playback_progress_from_row)?;
            for row in rows {
                let progress = row?;
                progress_by_id.insert(progress.media_id.clone(), progress);
            }
        }
        Ok(progress_by_id)
    }

    /// Removes stored progress and returns whether a row existed.
    ///
    /// # Errors
    ///
    /// Returns an error when the database row cannot be removed.
    pub fn delete_progress(&self, media_id: &MediaId) -> Result<bool, PersistenceError> {
        let changed = self
            .connection
            .prepare_cached("DELETE FROM playback_progress WHERE source = ?1 AND external_id = ?2")?
            .execute(params![media_id.source.as_str(), media_id.external_id])?;
        Ok(changed > 0)
    }

    /// Appends a history entry and returns its database identifier.
    ///
    /// # Errors
    ///
    /// Returns an error on integer overflow or a database write failure.
    pub fn insert_history(&self, entry: &HistoryEntry) -> Result<i64, PersistenceError> {
        let position = to_sql_u64(entry.position_seconds, "position_seconds")?;
        let duration = entry
            .duration_seconds
            .map(|value| to_sql_u64(value, "duration_seconds"))
            .transpose()?;
        let replay_locator = bounded_history_replay_locator(
            &entry.media_id.source,
            entry.replay_locator.as_deref(),
        )?;
        self.connection
            .prepare_cached(
                r"
				INSERT INTO playback_history (
					source, external_id, title, started_at, last_played_at,
					position_seconds, duration_seconds, finished, replay_locator
				) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
				",
            )?
            .execute(params![
                entry.media_id.source.as_str(),
                entry.media_id.external_id,
                entry.title,
                entry.started_at,
                entry.last_played_at,
                position,
                duration,
                entry.finished,
                replay_locator,
            ])?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Updates an existing history entry and returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns an error on integer overflow or a database write failure.
    pub fn update_history(&self, entry: &HistoryEntry) -> Result<bool, PersistenceError> {
        let position = to_sql_u64(entry.position_seconds, "position_seconds")?;
        let duration = entry
            .duration_seconds
            .map(|value| to_sql_u64(value, "duration_seconds"))
            .transpose()?;
        let replay_locator = bounded_history_replay_locator(
            &entry.media_id.source,
            entry.replay_locator.as_deref(),
        )?;
        let changed = self
            .connection
            .prepare_cached(
                r"
				UPDATE playback_history SET
					source = ?2,
					external_id = ?3,
					title = ?4,
					started_at = ?5,
					last_played_at = ?6,
					position_seconds = ?7,
					duration_seconds = ?8,
					finished = ?9,
					replay_locator = ?10
				WHERE id = ?1
				",
            )?
            .execute(params![
                entry.id,
                entry.media_id.source.as_str(),
                entry.media_id.external_id,
                entry.title,
                entry.started_at,
                entry.last_played_at,
                position,
                duration,
                entry.finished,
                replay_locator,
            ])?;
        Ok(changed > 0)
    }

    /// Lists recent history, optionally returning finished items only.
    ///
    /// # Errors
    ///
    /// Returns an error if the limit is too large or rows cannot be read.
    pub fn history(
        &self,
        finished_only: bool,
        limit: usize,
    ) -> Result<Vec<HistoryEntry>, PersistenceError> {
        let limit = to_sql_u64(limit as u64, "history limit")?;
        let sql = if finished_only {
            r"
			SELECT id, source, external_id, title, started_at, last_played_at,
				position_seconds, duration_seconds, finished, replay_locator
			FROM playback_history
			WHERE finished = 1
			ORDER BY last_played_at DESC, id DESC
			LIMIT ?1
			"
        } else {
            r"
			SELECT id, source, external_id, title, started_at, last_played_at,
				position_seconds, duration_seconds, finished, replay_locator
			FROM playback_history
			ORDER BY last_played_at DESC, id DESC
			LIMIT ?1
			"
        };
        let mut statement = self.connection.prepare_cached(sql)?;
        let entries = statement
            .query_map([limit], history_entry_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(entries)
    }

    /// Removes a history row and returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns an error when the database row cannot be removed.
    pub fn delete_history(&self, id: i64) -> Result<bool, PersistenceError> {
        let changed = self
            .connection
            .prepare_cached("DELETE FROM playback_history WHERE id = ?1")?
            .execute([id])?;
        Ok(changed > 0)
    }

    /// Inserts a private comment and returns its database identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if the target cannot be encoded or the row cannot be
    /// written.
    pub fn insert_private_comment(
        &self,
        comment: &PrivateComment,
    ) -> Result<i64, PersistenceError> {
        let target_json = serde_json::to_string(&comment.target)?;
        self.connection
            .prepare_cached(
                r"
				INSERT INTO private_comments (target_json, body, created_at, updated_at)
				VALUES (?1, ?2, ?3, ?4)
				",
            )?
            .execute(params![
                target_json,
                comment.body,
                comment.created_at,
                comment.updated_at
            ])?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Updates an existing private comment and returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns an error if the target cannot be encoded or the row cannot be
    /// updated.
    pub fn update_private_comment(
        &self,
        comment: &PrivateComment,
    ) -> Result<bool, PersistenceError> {
        let target_json = serde_json::to_string(&comment.target)?;
        let changed = self
            .connection
            .prepare_cached(
                r"
				UPDATE private_comments SET
					target_json = ?2, body = ?3, created_at = ?4, updated_at = ?5
				WHERE id = ?1
				",
            )?
            .execute(params![
                comment.id,
                target_json,
                comment.body,
                comment.created_at,
                comment.updated_at
            ])?;
        Ok(changed > 0)
    }

    /// Lists private comments attached to an exact target.
    ///
    /// # Errors
    ///
    /// Returns an error if the target cannot be encoded or stored rows cannot
    /// be decoded.
    pub fn private_comments(
        &self,
        target: &CommentTarget,
    ) -> Result<Vec<PrivateComment>, PersistenceError> {
        let target_json = serde_json::to_string(target)?;
        let mut statement = self.connection.prepare_cached(
            r"
			SELECT id, target_json, body, created_at, updated_at
			FROM private_comments
			WHERE target_json = ?1
			ORDER BY created_at, id
			",
        )?;
        let comments = statement
            .query_map([target_json], private_comment_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(comments)
    }

    /// Loads the sole user-facing private note for an exact target.
    ///
    /// Legacy databases can contain multiple comment rows for one target. In
    /// that case this returns the most recently updated row without exposing
    /// duplicate implementation records to the editor.
    ///
    /// # Errors
    ///
    /// Returns an error if the target cannot be encoded or the row cannot be
    /// decoded.
    pub fn private_note(
        &self,
        target: &CommentTarget,
    ) -> Result<Option<PrivateComment>, PersistenceError> {
        let target_json = serde_json::to_string(target)?;
        self.connection
            .query_row(
                r"
				SELECT id, target_json, body, created_at, updated_at
				FROM private_comments
				WHERE target_json = ?1
				ORDER BY updated_at DESC, id DESC
				LIMIT 1
				",
                [target_json],
                private_comment_from_row,
            )
            .optional()
            .map_err(PersistenceError::from)
    }

    /// Atomically creates or replaces the sole private note for a target.
    ///
    /// The first note's creation timestamp is retained across edits. Any
    /// duplicate rows left by the older multi-comment API are collapsed in the
    /// same transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the target cannot be encoded or the transaction
    /// cannot be completed.
    pub fn upsert_private_note(
        &self,
        target: &CommentTarget,
        body: &str,
        updated_at: i64,
    ) -> Result<PrivateComment, PersistenceError> {
        let target_json = serde_json::to_string(target)?;
        let transaction = self.connection.unchecked_transaction()?;
        let existing = transaction
            .query_row(
                r"
				SELECT id, target_json, body, created_at, updated_at
				FROM private_comments
				WHERE target_json = ?1
				ORDER BY updated_at DESC, id DESC
				LIMIT 1
				",
                [&target_json],
                private_comment_from_row,
            )
            .optional()?;
        let stored = if let Some(existing) = existing {
            transaction.execute(
                r"
				UPDATE private_comments
				SET body = ?2, updated_at = ?3
				WHERE id = ?1
				",
                params![existing.id, body, updated_at],
            )?;
            transaction.execute(
                "DELETE FROM private_comments WHERE target_json = ?1 AND id != ?2",
                params![target_json, existing.id],
            )?;
            PrivateComment {
                body: body.to_owned(),
                updated_at,
                ..existing
            }
        } else {
            transaction.execute(
                r"
				INSERT INTO private_comments (target_json, body, created_at, updated_at)
				VALUES (?1, ?2, ?3, ?3)
				",
                params![target_json, body, updated_at],
            )?;
            PrivateComment {
                id: transaction.last_insert_rowid(),
                target: target.clone(),
                body: body.to_owned(),
                created_at: updated_at,
                updated_at,
            }
        };
        transaction.commit()?;
        Ok(stored)
    }

    /// Atomically removes the private note and any legacy duplicates.
    ///
    /// # Errors
    ///
    /// Returns an error if the target cannot be encoded or rows cannot be
    /// removed.
    pub fn delete_private_note(&self, target: &CommentTarget) -> Result<bool, PersistenceError> {
        let target_json = serde_json::to_string(target)?;
        Ok(self
            .connection
            .prepare_cached("DELETE FROM private_comments WHERE target_json = ?1")?
            .execute([target_json])?
            > 0)
    }

    /// Removes a private comment and returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns an error when the database row cannot be removed.
    pub fn delete_private_comment(&self, id: i64) -> Result<bool, PersistenceError> {
        let changed = self
            .connection
            .prepare_cached("DELETE FROM private_comments WHERE id = ?1")?
            .execute([id])?;
        Ok(changed > 0)
    }

    /// Inserts a bookmark and returns its database identifier.
    ///
    /// # Errors
    ///
    /// Returns an error on integer overflow or a database write failure.
    pub fn insert_bookmark(&self, bookmark: &Bookmark) -> Result<i64, PersistenceError> {
        let position = to_sql_u64(bookmark.position_seconds, "position_seconds")?;
        self.connection
            .prepare_cached(
                r"
				INSERT INTO bookmarks (
					source, external_id, position_seconds, label, created_at
				) VALUES (?1, ?2, ?3, ?4, ?5)
				",
            )?
            .execute(params![
                bookmark.media_id.source.as_str(),
                bookmark.media_id.external_id,
                position,
                bookmark.label,
                bookmark.created_at,
            ])?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Updates a bookmark and returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns an error on integer overflow or a database write failure.
    pub fn update_bookmark(&self, bookmark: &Bookmark) -> Result<bool, PersistenceError> {
        let position = to_sql_u64(bookmark.position_seconds, "position_seconds")?;
        let changed = self
            .connection
            .prepare_cached(
                r"
				UPDATE bookmarks SET
					source = ?2, external_id = ?3, position_seconds = ?4,
					label = ?5, created_at = ?6
				WHERE id = ?1
				",
            )?
            .execute(params![
                bookmark.id,
                bookmark.media_id.source.as_str(),
                bookmark.media_id.external_id,
                position,
                bookmark.label,
                bookmark.created_at,
            ])?;
        Ok(changed > 0)
    }

    /// Lists bookmarks for one item in playback order.
    ///
    /// # Errors
    ///
    /// Returns an error when database rows cannot be read.
    pub fn bookmarks(&self, media_id: &MediaId) -> Result<Vec<Bookmark>, PersistenceError> {
        let mut statement = self.connection.prepare_cached(
            r"
			SELECT id, source, external_id, position_seconds, label, created_at
			FROM bookmarks
			WHERE source = ?1 AND external_id = ?2
			ORDER BY position_seconds, id
			",
        )?;
        let bookmarks = statement
            .query_map(
                params![media_id.source.as_str(), media_id.external_id],
                bookmark_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(bookmarks)
    }

    /// Removes a bookmark and returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns an error when the database row cannot be removed.
    pub fn delete_bookmark(&self, id: i64) -> Result<bool, PersistenceError> {
        let changed = self
            .connection
            .prepare_cached("DELETE FROM bookmarks WHERE id = ?1")?
            .execute([id])?;
        Ok(changed > 0)
    }

    /// Saves the single active terminal session.
    ///
    /// # Errors
    ///
    /// Returns an error when session state cannot be encoded or written.
    pub fn save_session(
        &self,
        state: &SessionState,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        let state_json = serde_json::to_string(state)?;
        self.connection
            .prepare_cached(
                r"
				INSERT INTO session_state (slot, state_json, updated_at)
				VALUES ('active', ?1, ?2)
				ON CONFLICT(slot) DO UPDATE SET
					state_json = excluded.state_json,
					updated_at = excluded.updated_at
				",
            )?
            .execute(params![state_json, updated_at])?;
        Ok(())
    }

    /// Loads the active terminal session, if it has been saved.
    ///
    /// # Errors
    ///
    /// Returns an error when stored session state cannot be read or decoded.
    pub fn session(&self) -> Result<Option<SessionState>, PersistenceError> {
        let json: Option<String> = self
            .connection
            .prepare_cached("SELECT state_json FROM session_state WHERE slot = 'active'")?
            .query_row([], |row| row.get(0))
            .optional()?;
        json.map(|value| serde_json::from_str(&value).map_err(PersistenceError::from))
            .transpose()
    }

    /// Atomically remaps durable Local identities after filesystem moves.
    ///
    /// `mappings` must be the authoritative completed mappings returned by the
    /// Local move worker. Sources and targets must be distinct, normalized,
    /// absolute UTF-8 paths. Overlapping, duplicate, chained, or colliding
    /// mappings are rejected before any row is updated.
    ///
    /// Playback progress, history identities and replay locators, private
    /// media/position comments, bookmarks, session selections/navigation, and
    /// cached Local metadata are changed together. Local subscription snapshots
    /// remap their folder key, descendant video and channel identities, and
    /// embedded `file:` URLs in the same transaction. Playlist snapshots,
    /// stable replay paths, artwork URLs, and saved-segment media identities
    /// are also remapped atomically.
    ///
    /// A successful return means callers may consume the complete mapping
    /// slice. On error every database change is rolled back; callers must keep
    /// and retry the same complete slice.
    ///
    /// # Errors
    ///
    /// Returns an error when mappings are structurally unsafe, a destination
    /// identity conflicts with unrelated state, stored JSON cannot be decoded
    /// within its existing bound, remapped JSON or a replay locator violates
    /// its bound, or the transaction cannot be completed.
    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    pub fn remap_local_move_state(
        &self,
        mappings: &[LocalMoveMapping],
    ) -> Result<LocalMoveStateRemap, PersistenceError> {
        validate_local_move_mappings(mappings)?;
        if mappings.is_empty() {
            return Ok(LocalMoveStateRemap::default());
        }

        let transaction = self.connection.unchecked_transaction()?;
        let plan = prepare_local_move_state_remap(&transaction, mappings)?;
        apply_local_move_state_remap(&transaction, &plan)?;
        for mapping in mappings {
            transaction
                .prepare_cached(
                    "DELETE FROM local_move_journal \
					 WHERE source_path = ?1 AND target_path = ?2",
                )?
                .execute(params![
                    local_move_path_text(&mapping.source, "source")?,
                    local_move_path_text(&mapping.target, "target")?,
                ])?;
        }
        transaction.commit()?;
        Ok(plan.report())
    }

    /// Durably records an exact validated move plan before filesystem mutation.
    ///
    /// The complete slice is inserted in one transaction after the normal
    /// remap planner proves that every current durable destination identity and
    /// bounded cache key can accept it. Existing source or target journal
    /// identities reject the new plan, keeping recovery decisions unambiguous
    /// across process restarts.
    ///
    /// # Errors
    ///
    /// Returns an error when mappings are unsafe, durable state would collide
    /// or exceed an existing bound, the journal bound would be exceeded, an
    /// identity is already pending, or the transaction fails.
    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    pub fn journal_local_move_intent(
        &self,
        mappings: &[LocalMoveMapping],
        created_at: i64,
    ) -> Result<(), PersistenceError> {
        validate_local_move_mappings(mappings)?;
        if mappings.is_empty() {
            return Ok(());
        }
        let transaction = self.connection.unchecked_transaction()?;
        // Reuse the same collision-complete transaction planner that will
        // later apply the remap. A durable target conflict must be rejected
        // before the worker receives permission to mutate the filesystem.
        let _ = prepare_local_move_state_remap(&transaction, mappings)?;
        let pending: i64 =
            transaction.query_row("SELECT COUNT(*) FROM local_move_journal", [], |row| {
                row.get(0)
            })?;
        let pending = usize::try_from(pending)
            .map_err(|_| invalid_local_move_state("move journal row count is out of range"))?;
        if pending.saturating_add(mappings.len()) > MAX_LOCAL_MOVE_MAPPINGS {
            return Err(invalid_local_move_state(format!(
                "pending move journal would exceed the {MAX_LOCAL_MOVE_MAPPINGS}-mapping limit"
            )));
        }
        for mapping in mappings {
            transaction
                .prepare_cached(
                    r"
					INSERT INTO local_move_journal (
						source_path, target_path, created_at
					) VALUES (?1, ?2, ?3)
					",
                )?
                .execute(params![
                    local_move_path_text(&mapping.source, "source")?,
                    local_move_path_text(&mapping.target, "target")?,
                    created_at,
                ])?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Loads exact move intents that still require startup reconciliation.
    ///
    /// Rows are returned in source-path order so diagnostics and recovery tests
    /// remain deterministic.
    ///
    /// # Errors
    ///
    /// Returns an error when the journal cannot be queried.
    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    pub fn local_move_intents(&self) -> Result<Vec<LocalMoveMapping>, PersistenceError> {
        let mut statement = self.connection.prepare_cached(
            r"
			SELECT source_path, target_path
			FROM local_move_journal
			ORDER BY source_path, target_path
			",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(LocalMoveMapping {
                source: Path::new(row.get_ref(0)?.as_str()?).to_owned(),
                target: Path::new(row.get_ref(1)?.as_str()?).to_owned(),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(PersistenceError::from)
    }

    /// Removes journal mappings proven not to have mutated the filesystem.
    ///
    /// Every supplied source/target pair must still exist in the journal. The
    /// all-or-nothing check prevents a stale caller from discarding a different
    /// recovery record that reused one side of an identity.
    ///
    /// # Errors
    ///
    /// Returns an error when mappings are unsafe, no longer match the journal,
    /// or the transaction fails.
    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    pub fn discard_local_move_intents(
        &self,
        mappings: &[LocalMoveMapping],
    ) -> Result<(), PersistenceError> {
        validate_local_move_mappings(mappings)?;
        if mappings.is_empty() {
            return Ok(());
        }
        let transaction = self.connection.unchecked_transaction()?;
        for mapping in mappings {
            let changed = transaction
                .prepare_cached(
                    "DELETE FROM local_move_journal \
					 WHERE source_path = ?1 AND target_path = ?2",
                )?
                .execute(params![
                    local_move_path_text(&mapping.source, "source")?,
                    local_move_path_text(&mapping.target, "target")?,
                ])?;
            if changed != 1 {
                return Err(invalid_local_move_state(format!(
                    "move journal no longer contains `{}` -> `{}`",
                    mapping.source.display(),
                    mapping.target.display()
                )));
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Replaces the restart snapshot for the active `YouTube` search.
    ///
    /// The request, accumulated summaries, and continuation page are validated
    /// and byte-bounded before `SQLite` receives them. Full enriched details
    /// are not part of this snapshot, though compact summary fields such as
    /// provider-confirmed orientation can be updated separately.
    ///
    /// # Errors
    ///
    /// Returns an error when the snapshot is inconsistent, exceeds its
    /// resource limits, cannot be encoded, or cannot be written.
    pub fn save_youtube_search(
        &self,
        search: &SavedYouTubeSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        validate_saved_youtube_search(search)?;
        let request_json = serde_json::to_string(&search.request)?;
        ensure_saved_search_json_bound(
            "request",
            request_json.len(),
            MAX_SAVED_SEARCH_REQUEST_BYTES,
        )?;
        let results_json = serde_json::to_string(&search.results)?;
        ensure_saved_search_json_bound(
            "results",
            results_json.len(),
            MAX_SAVED_SEARCH_RESULTS_BYTES,
        )?;
        self.connection
            .prepare_cached(
                r"
				INSERT INTO youtube_search_state (
					slot, request_json, results_json, next_page, updated_at
				) VALUES (1, ?1, ?2, ?3, ?4)
				ON CONFLICT(slot) DO UPDATE SET
					request_json = excluded.request_json,
					results_json = excluded.results_json,
					next_page = excluded.next_page,
					updated_at = excluded.updated_at
				",
            )?
            .execute(params![
                request_json,
                results_json,
                search.next_page.map(i64::from),
                updated_at,
            ])?;
        Ok(())
    }

    /// Loads the bounded restart snapshot for the latest `YouTube` search.
    ///
    /// The JSON byte lengths are checked before either payload is copied out of
    /// `SQLite`. The decoded request and summaries are then validated again so a
    /// manually modified database cannot bypass current invariants.
    ///
    /// # Errors
    ///
    /// Returns an error when the row exceeds its resource limits, contains
    /// inconsistent data, cannot be decoded, or cannot be read.
    pub fn youtube_search(&self) -> Result<Option<SavedYouTubeSearch>, PersistenceError> {
        let lengths: Option<(i64, i64)> = self
            .connection
            .prepare_cached(
                r"
				SELECT
					length(CAST(request_json AS BLOB)),
					length(CAST(results_json AS BLOB))
				FROM youtube_search_state
				WHERE slot = 1
				",
            )?
            .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        let Some((request_bytes, results_bytes)) = lengths else {
            return Ok(None);
        };
        ensure_saved_search_json_bound(
            "request",
            usize::try_from(request_bytes).unwrap_or(usize::MAX),
            MAX_SAVED_SEARCH_REQUEST_BYTES,
        )?;
        ensure_saved_search_json_bound(
            "results",
            usize::try_from(results_bytes).unwrap_or(usize::MAX),
            MAX_SAVED_SEARCH_RESULTS_BYTES,
        )?;

        let (request_json, results_json, next_page): (String, String, Option<i64>) = self
            .connection
            .prepare_cached(
                r"
				SELECT request_json, results_json, next_page
				FROM youtube_search_state
				WHERE slot = 1
				",
            )?
            .query_row([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        let next_page = next_page
            .map(|value| {
                u32::try_from(value).map_err(|_| PersistenceError::InvalidSavedSearch {
                    reason: "next page is outside the supported range".to_owned(),
                })
            })
            .transpose()?;
        let search = SavedYouTubeSearch {
            request: serde_json::from_str(&request_json)?,
            results: serde_json::from_str(&results_json)?,
            next_page,
        };
        validate_saved_youtube_search(&search)?;
        Ok(Some(search))
    }

    /// Updates one provider-confirmed video orientation in the restart-safe
    /// search snapshot.
    ///
    /// This reads and rewrites only the existing bounded summary snapshot; it
    /// never copies a full video description into persistent search state.
    ///
    /// # Errors
    ///
    /// Returns an error when the saved snapshot cannot be loaded, validated,
    /// encoded, or written.
    pub fn update_saved_youtube_video_orientation(
        &self,
        video_id: &str,
        orientation: VideoOrientation,
        updated_at: i64,
    ) -> Result<bool, PersistenceError> {
        let Some(mut search) = self.youtube_search()? else {
            return Ok(false);
        };
        let mut changed = false;
        for item in &mut search.results {
            if let SearchItem::Video(video) = item
                && video.video_id == video_id
                && video.orientation != orientation
            {
                video.orientation = orientation;
                changed = true;
            }
        }
        if changed {
            self.save_youtube_search(&search, updated_at)?;
        }
        Ok(changed)
    }

    /// Removes the saved `YouTube` search and reports whether one existed.
    ///
    /// # Errors
    ///
    /// Returns an error when the snapshot cannot be removed.
    pub fn clear_youtube_search(&self) -> Result<bool, PersistenceError> {
        Ok(self
            .connection
            .prepare_cached("DELETE FROM youtube_search_state WHERE slot = 1")?
            .execute([])?
            > 0)
    }

    /// Replaces the restart snapshot for the latest `YouTube Music` search.
    ///
    /// # Errors
    ///
    /// Returns an error when the query or results violate resource bounds,
    /// contain a non-video item, cannot be encoded, or cannot be written.
    pub fn save_youtube_music_search(
        &self,
        search: &SavedYouTubeMusicSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        validate_saved_youtube_music_search(search)?;
        let results_json = serde_json::to_string(&search.results)?;
        ensure_saved_search_json_bound(
            "results",
            results_json.len(),
            MAX_SAVED_SEARCH_RESULTS_BYTES,
        )?;
        self.connection
            .prepare_cached(
                r"
				INSERT INTO youtube_music_search_state (
					slot, query, results_json, updated_at
				) VALUES (1, ?1, ?2, ?3)
				ON CONFLICT(slot) DO UPDATE SET
					query = excluded.query,
					results_json = excluded.results_json,
					updated_at = excluded.updated_at
				",
            )?
            .execute(params![search.query, results_json, updated_at])?;
        Ok(())
    }

    /// Loads the bounded restart snapshot for the latest `YouTube Music` search.
    ///
    /// # Errors
    ///
    /// Returns an error when persisted fields exceed their limits, cannot be
    /// decoded, contain non-video results, or cannot be read.
    pub fn youtube_music_search(
        &self,
    ) -> Result<Option<SavedYouTubeMusicSearch>, PersistenceError> {
        let lengths: Option<(i64, i64)> = self
            .connection
            .prepare_cached(
                r"
				SELECT
					length(CAST(query AS BLOB)),
					length(CAST(results_json AS BLOB))
				FROM youtube_music_search_state
				WHERE slot = 1
				",
            )?
            .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        let Some((query_bytes, results_bytes)) = lengths else {
            return Ok(None);
        };
        ensure_saved_search_json_bound(
            "query",
            usize::try_from(query_bytes).unwrap_or(usize::MAX),
            MAX_SAVED_MUSIC_QUERY_BYTES,
        )?;
        ensure_saved_search_json_bound(
            "results",
            usize::try_from(results_bytes).unwrap_or(usize::MAX),
            MAX_SAVED_SEARCH_RESULTS_BYTES,
        )?;
        let (query, results_json): (String, String) = self
            .connection
            .prepare_cached(
                r"
				SELECT query, results_json
				FROM youtube_music_search_state
				WHERE slot = 1
				",
            )?
            .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        let search = SavedYouTubeMusicSearch {
            query,
            results: serde_json::from_str(&results_json)?,
        };
        validate_saved_youtube_music_search(&search)?;
        Ok(Some(search))
    }

    /// Removes the restart snapshot for `YouTube Music`.
    ///
    /// # Errors
    ///
    /// Returns an error when the row cannot be removed.
    pub fn clear_youtube_music_search(&self) -> Result<bool, PersistenceError> {
        Ok(self
            .connection
            .prepare_cached("DELETE FROM youtube_music_search_state WHERE slot = 1")?
            .execute([])?
            > 0)
    }

    /// Replaces the restart snapshot for the latest public Bandcamp search.
    ///
    /// Only the bounded query, page cursor, canonical track/album identities,
    /// and compact display metadata are retained. This method never resolves
    /// media or stores signed download URLs.
    ///
    /// # Errors
    ///
    /// Returns an error when the page or any summary violates fixed identity,
    /// URL, text, count, or encoded-size bounds.
    pub fn save_bandcamp_search(
        &self,
        search: &SavedBandcampSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        validate_saved_bandcamp_search(search)?;
        let results_json = serde_json::to_string(&search.results)?;
        ensure_saved_search_json_bound(
            "Bandcamp results",
            results_json.len(),
            MAX_SAVED_BANDCAMP_RESULTS_BYTES,
        )?;
        self.connection
            .prepare_cached(
                r"
				INSERT INTO bandcamp_search_state (
					slot, query, page, results_json, next_page, updated_at
				) VALUES (1, ?1, ?2, ?3, ?4, ?5)
				ON CONFLICT(slot) DO UPDATE SET
					query = excluded.query,
					page = excluded.page,
					results_json = excluded.results_json,
					next_page = excluded.next_page,
					updated_at = excluded.updated_at
				",
            )?
            .execute(params![
                search.query,
                i64::from(search.page),
                results_json,
                search.next_page.map(i64::from),
                updated_at,
            ])?;
        Ok(())
    }

    /// Loads the bounded restart snapshot for the latest public Bandcamp search.
    ///
    /// Text and JSON byte lengths are checked before their values are copied
    /// out of `SQLite`. Decoded summaries are then fully revalidated so a
    /// manually modified database cannot introduce a noncanonical page, signed
    /// media URL, duplicate identity, or excessive field.
    ///
    /// # Errors
    ///
    /// Returns an error when persisted fields exceed their limits, cannot be
    /// decoded, contain inconsistent or unsafe summaries, or cannot be read.
    pub fn bandcamp_search(&self) -> Result<Option<SavedBandcampSearch>, PersistenceError> {
        let lengths: Option<(i64, i64)> = self
            .connection
            .prepare_cached(
                r"
				SELECT
					length(CAST(query AS BLOB)),
					length(CAST(results_json AS BLOB))
				FROM bandcamp_search_state
				WHERE slot = 1
				",
            )?
            .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        let Some((query_bytes, results_bytes)) = lengths else {
            return Ok(None);
        };
        ensure_saved_search_json_bound(
            "Bandcamp query",
            usize::try_from(query_bytes).unwrap_or(usize::MAX),
            MAX_SAVED_BANDCAMP_QUERY_BYTES,
        )?;
        ensure_saved_search_json_bound(
            "Bandcamp results",
            usize::try_from(results_bytes).unwrap_or(usize::MAX),
            MAX_SAVED_BANDCAMP_RESULTS_BYTES,
        )?;
        let (query, page, results_json, next_page): (String, i64, String, Option<i64>) = self
            .connection
            .prepare_cached(
                r"
				SELECT query, page, results_json, next_page
				FROM bandcamp_search_state
				WHERE slot = 1
				",
            )?
            .query_row([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?;
        let page = u16::try_from(page).map_err(|_| PersistenceError::InvalidSavedSearch {
            reason: "Bandcamp page is outside the supported range".to_owned(),
        })?;
        let next_page = next_page
            .map(|value| {
                u16::try_from(value).map_err(|_| PersistenceError::InvalidSavedSearch {
                    reason: "Bandcamp next page is outside the supported range".to_owned(),
                })
            })
            .transpose()?;
        let search = SavedBandcampSearch {
            query,
            page,
            results: serde_json::from_str(&results_json)?,
            next_page,
        };
        validate_saved_bandcamp_search(&search)?;
        Ok(Some(search))
    }

    /// Removes the saved public Bandcamp search page.
    ///
    /// # Errors
    ///
    /// Returns an error when the snapshot cannot be removed.
    pub fn clear_bandcamp_search(&self) -> Result<bool, PersistenceError> {
        Ok(self
            .connection
            .prepare_cached("DELETE FROM bandcamp_search_state WHERE slot = 1")?
            .execute([])?
            > 0)
    }

    /// Replaces the restart snapshot for the latest `Apple Podcasts` search.
    ///
    /// Only compact provider-neutral show summaries are retained. Episode
    /// metadata and feed contents remain lazy so they can be refreshed after
    /// selection.
    ///
    /// # Errors
    ///
    /// Returns an error when the query, storefront, summaries, or encoded
    /// result payload violate their fixed resource and safety bounds.
    pub fn save_apple_podcasts_search(
        &self,
        search: &SavedApplePodcastsSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        validate_saved_apple_podcasts_search(search)?;
        let results_json = serde_json::to_string(&search.results)?;
        ensure_saved_search_json_bound(
            "Apple Podcasts results",
            results_json.len(),
            MAX_SAVED_APPLE_RESULTS_BYTES,
        )?;
        self.connection
            .prepare_cached(
                r"
				INSERT INTO apple_podcasts_search_state (
					slot, query, storefront, results_json, updated_at
				) VALUES (1, ?1, ?2, ?3, ?4)
				ON CONFLICT(slot) DO UPDATE SET
					query = excluded.query,
					storefront = excluded.storefront,
					results_json = excluded.results_json,
					updated_at = excluded.updated_at
				",
            )?
            .execute(params![
                search.query,
                search.storefront,
                results_json,
                updated_at,
            ])?;
        Ok(())
    }

    /// Loads the bounded restart snapshot for the latest `Apple Podcasts`
    /// search.
    ///
    /// Column byte lengths are checked before payloads are copied out of
    /// `SQLite`; decoded summaries are then revalidated so a manually modified
    /// database cannot bypass current invariants.
    ///
    /// # Errors
    ///
    /// Returns an error when stored fields exceed their limits, contain unsafe
    /// or inconsistent summaries, cannot be decoded, or cannot be read.
    pub fn apple_podcasts_search(
        &self,
    ) -> Result<Option<SavedApplePodcastsSearch>, PersistenceError> {
        let lengths: Option<(i64, i64, i64)> = self
            .connection
            .prepare_cached(
                r"
				SELECT
					length(CAST(query AS BLOB)),
					length(CAST(storefront AS BLOB)),
					length(CAST(results_json AS BLOB))
				FROM apple_podcasts_search_state
				WHERE slot = 1
				",
            )?
            .query_row([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .optional()?;
        let Some((query_bytes, storefront_bytes, results_bytes)) = lengths else {
            return Ok(None);
        };
        ensure_saved_search_json_bound(
            "Apple Podcasts query",
            usize::try_from(query_bytes).unwrap_or(usize::MAX),
            MAX_SAVED_APPLE_QUERY_BYTES,
        )?;
        ensure_saved_search_json_bound(
            "Apple Podcasts storefront",
            usize::try_from(storefront_bytes).unwrap_or(usize::MAX),
            2,
        )?;
        ensure_saved_search_json_bound(
            "Apple Podcasts results",
            usize::try_from(results_bytes).unwrap_or(usize::MAX),
            MAX_SAVED_APPLE_RESULTS_BYTES,
        )?;
        let (query, storefront, results_json): (String, String, String) = self
            .connection
            .prepare_cached(
                r"
				SELECT query, storefront, results_json
				FROM apple_podcasts_search_state
				WHERE slot = 1
				",
            )?
            .query_row([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        let search = SavedApplePodcastsSearch {
            query,
            storefront,
            results: serde_json::from_str(&results_json)?,
        };
        validate_saved_apple_podcasts_search(&search)?;
        Ok(Some(search))
    }

    /// Removes the restart snapshot for `Apple Podcasts`.
    ///
    /// # Errors
    ///
    /// Returns an error when the row cannot be removed.
    pub fn clear_apple_podcasts_search(&self) -> Result<bool, PersistenceError> {
        Ok(self
            .connection
            .prepare_cached("DELETE FROM apple_podcasts_search_state WHERE slot = 1")?
            .execute([])?
            > 0)
    }

    /// Replaces the first-page cache for one subscribed source.
    ///
    /// The item count is validated before writing. When the complete encoded
    /// page would exceed the byte bound, the longest whole-item prefix that
    /// fits is stored; this keeps provider order deterministic without
    /// rejecting an otherwise valid refresh. After the upsert, the least
    /// recently fetched rows are removed so databases cannot grow with every
    /// source ever opened. The row being written wins deterministic timestamp
    /// ties.
    ///
    /// # Errors
    ///
    /// Returns an error when the source identity or item count violates its
    /// bounds, contains a channel result, cannot be encoded, or cannot be
    /// written.
    pub fn put_cached_subscription_items(
        &self,
        cached: &CachedSubscriptionItems,
    ) -> Result<(), PersistenceError> {
        validate_cached_subscription_items(cached)?;
        let items_json = encode_bounded_subscription_items(&cached.items)?;
        let source_limit = i64::try_from(MAX_SAVED_SUBSCRIPTION_SOURCES).map_err(|_| {
            PersistenceError::IntegerOutOfRange {
                field: "subscription source cache limit",
            }
        })?;
        self.connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<(), PersistenceError> {
            self.connection
                .prepare_cached(
                    r"
					INSERT INTO subscription_items_cache (
						source, source_id, items_json, fetched_at
					) VALUES (?1, ?2, ?3, ?4)
					ON CONFLICT(source, source_id) DO UPDATE SET
						items_json = excluded.items_json,
						fetched_at = excluded.fetched_at
					",
                )?
                .execute(params![
                    cached.source.as_str(),
                    cached.source_id,
                    items_json,
                    cached.fetched_at,
                ])?;
            self.connection
                .prepare_cached(
                    r"
					DELETE FROM subscription_items_cache
					WHERE (source, source_id) IN (
						SELECT source, source_id
						FROM subscription_items_cache
						ORDER BY
							fetched_at DESC,
							(source = ?1 AND source_id = ?2) DESC,
							source,
							source_id
						LIMIT -1 OFFSET ?3
					)
					",
                )?
                .execute(params![
                    cached.source.as_str(),
                    cached.source_id,
                    source_limit,
                ])?;
            self.connection.execute_batch("COMMIT")?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = self.connection.execute_batch("ROLLBACK");
            return Err(error);
        }
        Ok(())
    }

    /// Loads the bounded first-page cache for one subscribed source.
    ///
    /// The encoded byte length is checked before copying JSON out of `SQLite`,
    /// then all current invariants are revalidated. Stale rows are returned:
    /// callers decide when to refresh by comparing [`CachedSubscriptionItems::fetched_at`].
    ///
    /// # Errors
    ///
    /// Returns an error when the requested identity is invalid, a stored row
    /// exceeds current limits, cannot be decoded, or cannot be read.
    pub fn cached_subscription_items(
        &self,
        source: &SourceKind,
        source_id: &str,
    ) -> Result<Option<CachedSubscriptionItems>, PersistenceError> {
        validate_subscription_source_identity(source, source_id)?;
        let items_bytes: Option<i64> = self
            .connection
            .prepare_cached(
                r"
				SELECT length(CAST(items_json AS BLOB))
				FROM subscription_items_cache
				WHERE source = ?1 AND source_id = ?2
				",
            )?
            .query_row(params![source.as_str(), source_id], |row| row.get(0))
            .optional()?;
        let Some(items_bytes) = items_bytes else {
            return Ok(None);
        };
        ensure_subscription_snapshot_json_bound(
            usize::try_from(items_bytes).unwrap_or(usize::MAX),
        )?;
        let (items_json, fetched_at): (String, i64) = self
            .connection
            .prepare_cached(
                r"
				SELECT items_json, fetched_at
				FROM subscription_items_cache
				WHERE source = ?1 AND source_id = ?2
				",
            )?
            .query_row(params![source.as_str(), source_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        let cached = CachedSubscriptionItems {
            source: source.clone(),
            source_id: source_id.to_owned(),
            items: serde_json::from_str(&items_json)?,
            fetched_at,
        };
        validate_cached_subscription_items(&cached)?;
        Ok(Some(cached))
    }

    /// Removes one subscribed-source snapshot and reports whether it existed.
    ///
    /// # Errors
    ///
    /// Returns an error when the source identity is invalid or the row cannot
    /// be removed.
    pub fn delete_cached_subscription_items(
        &self,
        source: &SourceKind,
        source_id: &str,
    ) -> Result<bool, PersistenceError> {
        validate_subscription_source_identity(source, source_id)?;
        Ok(self
            .connection
            .prepare_cached(
                r"
				DELETE FROM subscription_items_cache
				WHERE source = ?1 AND source_id = ?2
				",
            )?
            .execute(params![source.as_str(), source_id])?
            > 0)
    }

    /// Adds listened seconds to a source using an atomic upsert.
    ///
    /// # Errors
    ///
    /// Returns an error on integer overflow or a database write failure.
    pub fn add_listen_seconds(
        &self,
        source: &SourceKind,
        seconds: u64,
    ) -> Result<(), PersistenceError> {
        let seconds = to_sql_u64(seconds, "listen seconds")?;
        add_sqlite_listen_seconds(&self.connection, source, seconds)
    }

    /// Returns exact listened seconds for a source.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored total cannot be read.
    pub fn listened_seconds(&self, source: &SourceKind) -> Result<u64, PersistenceError> {
        let value: Option<i64> = self
            .connection
            .prepare_cached("SELECT total_seconds FROM listen_totals WHERE source = ?1")?
            .query_row([source.as_str()], |row| row.get(0))
            .optional()?;
        from_sql_u64_value(value.unwrap_or(0), 0).map_err(PersistenceError::from)
    }

    /// Lists listening totals in descending duration order.
    ///
    /// # Errors
    ///
    /// Returns an error when stored totals cannot be read.
    pub fn listen_totals(&self) -> Result<Vec<ListenTotal>, PersistenceError> {
        let mut statement = self.connection.prepare_cached(
            "SELECT source, total_seconds FROM listen_totals ORDER BY total_seconds DESC, source",
        )?;
        let totals = statement
            .query_map([], |row| {
                Ok(ListenTotal {
                    source: SourceKind::from(row.get::<_, String>(0)?.as_str()),
                    total_seconds: from_sql_u64(row, 1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(totals)
    }

    /// Removes a source's listening total and returns whether it existed.
    ///
    /// # Errors
    ///
    /// Returns an error when the database row cannot be removed.
    pub fn reset_listen_seconds(&self, source: &SourceKind) -> Result<bool, PersistenceError> {
        let changed = self
            .connection
            .prepare_cached("DELETE FROM listen_totals WHERE source = ?1")?
            .execute([source.as_str()])?;
        Ok(changed > 0)
    }

    /// Inserts or replaces provider metadata and its provenance.
    ///
    /// # Errors
    ///
    /// Returns an error when metadata cannot be encoded or written.
    pub fn put_cached_metadata(&self, cached: &CachedMetadata) -> Result<(), PersistenceError> {
        let media_json = serde_json::to_string(&cached.media)?;
        let source_url = cached.provenance.source_url.as_ref().map(Url::as_str);
        self.connection
            .prepare_cached(
                r"
				INSERT INTO metadata_cache (
					source, external_id, media_json, provider, source_url,
					fetched_at, expires_at
				) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
				ON CONFLICT(source, external_id) DO UPDATE SET
					media_json = excluded.media_json,
					provider = excluded.provider,
					source_url = excluded.source_url,
					fetched_at = excluded.fetched_at,
					expires_at = excluded.expires_at
				",
            )?
            .execute(params![
                cached.media.id.source.as_str(),
                cached.media.id.external_id,
                media_json,
                cached.provenance.provider,
                source_url,
                cached.provenance.fetched_at,
                cached.provenance.expires_at,
            ])?;
        Ok(())
    }

    /// Loads cached metadata, including provenance, without applying expiry.
    ///
    /// # Errors
    ///
    /// Returns an error when metadata or provenance cannot be read and decoded.
    pub fn cached_metadata(
        &self,
        media_id: &MediaId,
    ) -> Result<Option<CachedMetadata>, PersistenceError> {
        type CacheColumns = (String, String, Option<String>, i64, Option<i64>);
        let columns: Option<CacheColumns> = self
            .connection
            .prepare_cached(
                r"
				SELECT media_json, provider, source_url, fetched_at, expires_at
				FROM metadata_cache
				WHERE source = ?1 AND external_id = ?2
				",
            )?
            .query_row(
                params![media_id.source.as_str(), media_id.external_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((media_json, provider, source_url, fetched_at, expires_at)) = columns else {
            return Ok(None);
        };
        let media = serde_json::from_str(&media_json)?;
        let source_url = source_url
            .map(|value| Url::parse(&value))
            .transpose()
            .map_err(PersistenceError::InvalidCachedUrl)?;
        Ok(Some(CachedMetadata {
            media,
            provenance: MetadataProvenance {
                provider,
                source_url,
                fetched_at,
                expires_at,
            },
        }))
    }

    /// Deletes expired cache entries and returns the number removed.
    ///
    /// # Errors
    ///
    /// Returns an error when expired rows cannot be removed.
    pub fn delete_expired_metadata(&self, now: i64) -> Result<usize, PersistenceError> {
        Ok(self
            .connection
            .prepare_cached(
                "DELETE FROM metadata_cache WHERE expires_at IS NOT NULL AND expires_at <= ?1",
            )?
            .execute([now])?)
    }

    /// Inserts or replaces one restart-safe channel summary.
    ///
    /// Official-API and Invidious adapters use the same stable `YouTube`
    /// channel identifier, so either adapter can refresh an existing row.
    ///
    /// # Errors
    ///
    /// Returns an error when the summary cannot be encoded or written.
    pub fn put_cached_channel_summary(
        &self,
        cached: &CachedChannelSummary,
    ) -> Result<(), PersistenceError> {
        let summary_json = serde_json::to_string(&cached.summary)?;
        self.connection
            .prepare_cached(
                r"
				INSERT INTO channel_summary_cache (
					channel_id, summary_json, fetched_at, expires_at
				) VALUES (?1, ?2, ?3, ?4)
				ON CONFLICT(channel_id) DO UPDATE SET
					summary_json = excluded.summary_json,
					fetched_at = excluded.fetched_at,
					expires_at = excluded.expires_at
				",
            )?
            .execute(params![
                cached.summary.channel_id,
                summary_json,
                cached.fetched_at,
                cached.expires_at,
            ])?;
        Ok(())
    }

    /// Loads a cached channel summary without applying its expiry policy.
    ///
    /// Returning stale rows is intentional: the UI can render their artwork
    /// and headings immediately, then use [`CachedChannelSummary::is_fresh_at`]
    /// to decide whether to refresh the entry in the background.
    ///
    /// # Errors
    ///
    /// Returns an error when the row or summary JSON cannot be read and
    /// decoded.
    pub fn cached_channel_summary(
        &self,
        channel_id: &str,
    ) -> Result<Option<CachedChannelSummary>, PersistenceError> {
        type CacheColumns = (String, i64, i64);
        let columns: Option<CacheColumns> = self
            .connection
            .prepare_cached(
                r"
				SELECT summary_json, fetched_at, expires_at
				FROM channel_summary_cache
				WHERE channel_id = ?1
				",
            )?
            .query_row([channel_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .optional()?;
        let Some((summary_json, fetched_at, expires_at)) = columns else {
            return Ok(None);
        };
        Ok(Some(CachedChannelSummary {
            summary: serde_json::from_str(&summary_json)?,
            fetched_at,
            expires_at,
        }))
    }

    /// Deletes expired channel-summary rows and returns the number removed.
    ///
    /// Normal startup should not call this before hydrating visible channels,
    /// because stale rows are useful while a background refresh is pending.
    ///
    /// # Errors
    ///
    /// Returns an error when expired rows cannot be removed.
    pub fn delete_expired_channel_summaries(&self, now: i64) -> Result<usize, PersistenceError> {
        Ok(self
            .connection
            .prepare_cached("DELETE FROM channel_summary_cache WHERE expires_at <= ?1")?
            .execute([now])?)
    }

    /// Inserts or replaces a bounded Wikidata lookup, including an empty
    /// result.
    ///
    /// # Errors
    ///
    /// Returns an error when the item list cannot be encoded or written.
    pub fn put_cached_wikidata(
        &self,
        cached: &CachedWikidataLookup,
    ) -> Result<(), PersistenceError> {
        let items_json = serde_json::to_string(&cached.items)?;
        self.connection
            .prepare_cached(
                r"
				INSERT INTO wikidata_cache (
					property_id, external_id, items_json, fetched_at, expires_at
				) VALUES (?1, ?2, ?3, ?4, ?5)
				ON CONFLICT(property_id, external_id) DO UPDATE SET
					items_json = excluded.items_json,
					fetched_at = excluded.fetched_at,
					expires_at = excluded.expires_at
				",
            )?
            .execute(params![
                cached.property_id,
                cached.external_id,
                items_json,
                cached.fetched_at,
                cached.expires_at,
            ])?;
        Ok(())
    }

    /// Loads a Wikidata lookup without applying its expiry policy.
    ///
    /// # Errors
    ///
    /// Returns an error when the row or item JSON cannot be decoded.
    pub fn cached_wikidata(
        &self,
        property_id: &str,
        external_id: &str,
    ) -> Result<Option<CachedWikidataLookup>, PersistenceError> {
        type CacheColumns = (String, i64, i64);
        let columns: Option<CacheColumns> = self
            .connection
            .prepare_cached(
                r"
				SELECT items_json, fetched_at, expires_at
				FROM wikidata_cache
				WHERE property_id = ?1 AND external_id = ?2
				",
            )?
            .query_row(params![property_id, external_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .optional()?;
        let Some((items_json, fetched_at, expires_at)) = columns else {
            return Ok(None);
        };
        Ok(Some(CachedWikidataLookup {
            property_id: property_id.to_owned(),
            external_id: external_id.to_owned(),
            items: serde_json::from_str(&items_json)?,
            fetched_at,
            expires_at,
        }))
    }

    /// Deletes expired Wikidata lookup rows.
    ///
    /// # Errors
    ///
    /// Returns an error when expired rows cannot be removed.
    pub fn delete_expired_wikidata(&self, now: i64) -> Result<usize, PersistenceError> {
        Ok(self
            .connection
            .prepare_cached("DELETE FROM wikidata_cache WHERE expires_at <= ?1")?
            .execute([now])?)
    }
}

#[path = "persistence_files.rs"]
mod files;

use files::FileStateStore;

/// Operations supplied by every restart-state backend.
///
/// The trait is public only so [`StateStore`] can expose normal method syntax
/// through [`Deref`]. Applications should construct [`StateStore`] rather than
/// depending on a concrete backend.
pub trait StateBackend {
    /// Returns the stable backend name used in diagnostics.
    fn backend_name(&self) -> &'static str;
    /// Returns the backend document or schema version.
    fn format_version(&self) -> Result<u32, PersistenceError>;
    /// Creates an empty playlist idempotently.
    fn create_playlist(
        &self,
        name: &str,
        description: Option<&str>,
        created_at: i64,
    ) -> Result<PlaylistCreateOutcome, PersistenceError>;
    /// Creates a playlist and its first entry atomically.
    fn create_playlist_with_entry(
        &self,
        name: &str,
        description: Option<&str>,
        media: &PlaylistMediaSnapshot,
        created_at: i64,
    ) -> Result<PlaylistCreateOutcome, PersistenceError>;
    /// Updates playlist metadata.
    fn update_playlist(
        &self,
        playlist_id: &str,
        name: &str,
        description: Option<&str>,
        updated_at: i64,
    ) -> Result<Option<PlaylistSummary>, PersistenceError>;
    /// Lists playlists.
    fn playlists(&self) -> Result<Vec<PlaylistSummary>, PersistenceError>;
    /// Loads one playlist.
    fn playlist(&self, playlist_id: &str) -> Result<Option<Playlist>, PersistenceError>;
    /// Tests membership in one playlist.
    fn playlist_contains(
        &self,
        playlist_id: &str,
        media_id: &MediaId,
    ) -> Result<bool, PersistenceError>;
    /// Lists playlist IDs containing media.
    fn playlist_memberships(&self, media_id: &MediaId)
    -> Result<Vec<PlaylistId>, PersistenceError>;
    /// Lists playlist membership names.
    fn playlist_memberships_with_names(
        &self,
        media_id: &MediaId,
    ) -> Result<Vec<PlaylistMembership>, PersistenceError>;
    /// Adds one playlist entry.
    fn add_playlist_entry(
        &self,
        playlist_id: &str,
        media: &PlaylistMediaSnapshot,
        added_at: i64,
    ) -> Result<PlaylistAddOutcome, PersistenceError>;
    /// Removes one playlist entry.
    fn remove_playlist_entry(
        &self,
        playlist_id: &str,
        media_id: &MediaId,
        updated_at: i64,
    ) -> Result<bool, PersistenceError>;
    /// Toggles one playlist entry.
    fn toggle_playlist_entry(
        &self,
        playlist_id: &str,
        media: &PlaylistMediaSnapshot,
        updated_at: i64,
    ) -> Result<PlaylistToggleOutcome, PersistenceError>;
    /// Adds media to the built-in todo playlist.
    fn add_to_todo(
        &self,
        media: &PlaylistMediaSnapshot,
        added_at: i64,
    ) -> Result<PlaylistAddOutcome, PersistenceError>;
    /// Toggles media in the built-in todo playlist.
    fn toggle_todo(
        &self,
        media: &PlaylistMediaSnapshot,
        updated_at: i64,
    ) -> Result<PlaylistToggleOutcome, PersistenceError>;
    /// Tests built-in todo membership.
    fn todo_contains(&self, media_id: &MediaId) -> Result<bool, PersistenceError>;
    /// Toggles a Radio station in the hidden persistent favorites playlist.
    fn toggle_radio_favorite(
        &self,
        media: &PlaylistMediaSnapshot,
        updated_at: i64,
    ) -> Result<PlaylistToggleOutcome, PersistenceError>;
    /// Tests membership in the hidden persistent Radio favorites playlist.
    fn radio_favorite_contains(&self, media_id: &MediaId) -> Result<bool, PersistenceError>;
    /// Lists canonical pending Yandex Music desired reactions.
    #[cfg(feature = "yandex-music")]
    fn pending_yandex_music_reactions(
        &self,
    ) -> Result<Vec<PendingYandexMusicReaction>, PersistenceError>;
    /// Queues one Yandex Music desired reaction with an atomic next revision.
    ///
    /// The returned intent contains the revision that must be sent through the
    /// serialized provider dispatcher. Acknowledged ledger entries remain
    /// durable, so a later choice cannot reuse an in-flight revision.
    #[cfg(feature = "yandex-music")]
    fn queue_yandex_music_reaction(
        &self,
        account_uid: &str,
        track_id: &str,
        reaction: YandexMusicReaction,
        updated_at: i64,
    ) -> Result<PendingYandexMusicReaction, PersistenceError>;
    /// Acknowledges a Yandex Music desired reaction only for its current revision.
    ///
    /// The desired ledger row remains durable after acknowledgement. A delayed
    /// response therefore cannot acknowledge or erase a later user choice.
    #[cfg(feature = "yandex-music")]
    fn acknowledge_yandex_music_reaction(
        &self,
        account_uid: &str,
        track_id: &str,
        generation: u64,
    ) -> Result<bool, PersistenceError>;
    /// Saves a bounded periodic playback checkpoint.
    ///
    /// File persistence atomically replaces only its small runtime checkpoint
    /// and exposes the pending values through normal reads. `SQLite` applies the
    /// progress row and `listened_seconds` delta in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend cannot durably publish the checkpoint.
    fn checkpoint_playback(
        &self,
        progress: &PlaybackProgress,
        listen_source: &SourceKind,
        listened_seconds: u64,
    ) -> Result<(), PersistenceError>;
    /// Saves a bounded listening-only checkpoint for non-seekable media.
    ///
    /// This method never creates playback progress. File persistence atomically
    /// replaces the small runtime checkpoint with an absolute listening target,
    /// making startup replay idempotent. `SQLite` increments statistics in one
    /// transaction. A zero-second delta is a no-op on every backend.
    ///
    /// # Errors
    ///
    /// Returns an error when the delta or cumulative total overflows the
    /// persistent integer range, or when the backend cannot durably record it.
    fn checkpoint_listening(
        &self,
        source: &SourceKind,
        listened_seconds: u64,
    ) -> Result<(), PersistenceError>;
    /// Publishes a pending playback checkpoint into canonical state.
    ///
    /// File persistence clears the recovery record only after both canonical
    /// documents are durable, making retry and startup replay idempotent.
    /// `SQLite` checkpoints are already canonical, so this is a no-op there.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend cannot merge or clear the checkpoint.
    fn flush_playback_checkpoint(&self) -> Result<(), PersistenceError>;
    /// Inserts or replaces playback progress.
    fn upsert_progress(&self, progress: &PlaybackProgress) -> Result<(), PersistenceError>;
    /// Loads one progress row.
    fn progress(&self, media_id: &MediaId) -> Result<Option<PlaybackProgress>, PersistenceError>;
    /// Loads progress for a bounded identity list.
    fn progress_for_media_ids(
        &self,
        media_ids: &[MediaId],
    ) -> Result<HashMap<MediaId, PlaybackProgress>, PersistenceError>;
    /// Deletes one progress row.
    fn delete_progress(&self, media_id: &MediaId) -> Result<bool, PersistenceError>;
    /// Inserts one history row.
    fn insert_history(&self, entry: &HistoryEntry) -> Result<i64, PersistenceError>;
    /// Updates one history row.
    fn update_history(&self, entry: &HistoryEntry) -> Result<bool, PersistenceError>;
    /// Lists recent history.
    fn history(
        &self,
        finished_only: bool,
        limit: usize,
    ) -> Result<Vec<HistoryEntry>, PersistenceError>;
    /// Deletes one history row.
    fn delete_history(&self, id: i64) -> Result<bool, PersistenceError>;
    /// Inserts one private comment.
    fn insert_private_comment(&self, comment: &PrivateComment) -> Result<i64, PersistenceError>;
    /// Updates one private comment.
    fn update_private_comment(&self, comment: &PrivateComment) -> Result<bool, PersistenceError>;
    /// Lists private comments for a target.
    fn private_comments(
        &self,
        target: &CommentTarget,
    ) -> Result<Vec<PrivateComment>, PersistenceError>;
    /// Loads the sole user-facing private note for an exact target.
    fn private_note(
        &self,
        target: &CommentTarget,
    ) -> Result<Option<PrivateComment>, PersistenceError>;
    /// Creates or replaces the sole private note for an exact target.
    fn upsert_private_note(
        &self,
        target: &CommentTarget,
        body: &str,
        updated_at: i64,
    ) -> Result<PrivateComment, PersistenceError>;
    /// Deletes the private note and any legacy duplicates at an exact target.
    fn delete_private_note(&self, target: &CommentTarget) -> Result<bool, PersistenceError>;
    /// Deletes one private comment.
    fn delete_private_comment(&self, id: i64) -> Result<bool, PersistenceError>;
    /// Inserts one bookmark.
    fn insert_bookmark(&self, bookmark: &Bookmark) -> Result<i64, PersistenceError>;
    /// Updates one bookmark.
    fn update_bookmark(&self, bookmark: &Bookmark) -> Result<bool, PersistenceError>;
    /// Lists bookmarks for media.
    fn bookmarks(&self, media_id: &MediaId) -> Result<Vec<Bookmark>, PersistenceError>;
    /// Deletes one bookmark.
    fn delete_bookmark(&self, id: i64) -> Result<bool, PersistenceError>;
    /// Saves the active session.
    fn save_session(&self, state: &SessionState, updated_at: i64) -> Result<(), PersistenceError>;
    /// Loads the active session.
    fn session(&self) -> Result<Option<SessionState>, PersistenceError>;
    /// Atomically remaps state after Local moves.
    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn remap_local_move_state(
        &self,
        mappings: &[LocalMoveMapping],
    ) -> Result<LocalMoveStateRemap, PersistenceError>;
    /// Journals Local move intentions.
    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn journal_local_move_intent(
        &self,
        mappings: &[LocalMoveMapping],
        created_at: i64,
    ) -> Result<(), PersistenceError>;
    /// Loads Local move intentions.
    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn local_move_intents(&self) -> Result<Vec<LocalMoveMapping>, PersistenceError>;
    /// Discards completed Local move intentions.
    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn discard_local_move_intents(
        &self,
        mappings: &[LocalMoveMapping],
    ) -> Result<(), PersistenceError>;
    /// Saves the latest YouTube search.
    fn save_youtube_search(
        &self,
        search: &SavedYouTubeSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError>;
    /// Loads the latest YouTube search.
    fn youtube_search(&self) -> Result<Option<SavedYouTubeSearch>, PersistenceError>;
    /// Updates one saved YouTube orientation.
    fn update_saved_youtube_video_orientation(
        &self,
        video_id: &str,
        orientation: VideoOrientation,
        updated_at: i64,
    ) -> Result<bool, PersistenceError>;
    /// Clears the latest YouTube search.
    fn clear_youtube_search(&self) -> Result<bool, PersistenceError>;
    /// Saves the latest YouTube Music search.
    fn save_youtube_music_search(
        &self,
        search: &SavedYouTubeMusicSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError>;
    /// Loads the latest YouTube Music search.
    fn youtube_music_search(&self) -> Result<Option<SavedYouTubeMusicSearch>, PersistenceError>;
    /// Clears the latest YouTube Music search.
    fn clear_youtube_music_search(&self) -> Result<bool, PersistenceError>;
    /// Saves the latest Bandcamp search.
    fn save_bandcamp_search(
        &self,
        search: &SavedBandcampSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError>;
    /// Loads the latest Bandcamp search.
    fn bandcamp_search(&self) -> Result<Option<SavedBandcampSearch>, PersistenceError>;
    /// Clears the latest Bandcamp search.
    fn clear_bandcamp_search(&self) -> Result<bool, PersistenceError>;
    /// Saves the latest Apple Podcasts search.
    fn save_apple_podcasts_search(
        &self,
        search: &SavedApplePodcastsSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError>;
    /// Loads the latest Apple Podcasts search.
    fn apple_podcasts_search(&self) -> Result<Option<SavedApplePodcastsSearch>, PersistenceError>;
    /// Clears the latest Apple Podcasts search.
    fn clear_apple_podcasts_search(&self) -> Result<bool, PersistenceError>;
    /// Saves one subscription item cache.
    fn put_cached_subscription_items(
        &self,
        cached: &CachedSubscriptionItems,
    ) -> Result<(), PersistenceError>;
    /// Loads one subscription item cache.
    fn cached_subscription_items(
        &self,
        source: &SourceKind,
        source_id: &str,
    ) -> Result<Option<CachedSubscriptionItems>, PersistenceError>;
    /// Deletes one subscription item cache.
    fn delete_cached_subscription_items(
        &self,
        source: &SourceKind,
        source_id: &str,
    ) -> Result<bool, PersistenceError>;
    /// Adds listened seconds.
    fn add_listen_seconds(&self, source: &SourceKind, seconds: u64)
    -> Result<(), PersistenceError>;
    /// Loads listened seconds for a source.
    fn listened_seconds(&self, source: &SourceKind) -> Result<u64, PersistenceError>;
    /// Lists all listening totals.
    fn listen_totals(&self) -> Result<Vec<ListenTotal>, PersistenceError>;
    /// Deletes one listening total.
    fn reset_listen_seconds(&self, source: &SourceKind) -> Result<bool, PersistenceError>;
    /// Saves provider metadata.
    fn put_cached_metadata(&self, cached: &CachedMetadata) -> Result<(), PersistenceError>;
    /// Loads provider metadata.
    fn cached_metadata(
        &self,
        media_id: &MediaId,
    ) -> Result<Option<CachedMetadata>, PersistenceError>;
    /// Deletes expired provider metadata.
    fn delete_expired_metadata(&self, now: i64) -> Result<usize, PersistenceError>;
    /// Saves a channel summary.
    fn put_cached_channel_summary(
        &self,
        cached: &CachedChannelSummary,
    ) -> Result<(), PersistenceError>;
    /// Loads a channel summary.
    fn cached_channel_summary(
        &self,
        channel_id: &str,
    ) -> Result<Option<CachedChannelSummary>, PersistenceError>;
    /// Deletes expired channel summaries.
    fn delete_expired_channel_summaries(&self, now: i64) -> Result<usize, PersistenceError>;
    /// Saves a Wikidata lookup.
    fn put_cached_wikidata(&self, cached: &CachedWikidataLookup) -> Result<(), PersistenceError>;
    /// Loads a Wikidata lookup.
    fn cached_wikidata(
        &self,
        property_id: &str,
        external_id: &str,
    ) -> Result<Option<CachedWikidataLookup>, PersistenceError>;
    /// Deletes expired Wikidata lookups.
    fn delete_expired_wikidata(&self, now: i64) -> Result<usize, PersistenceError>;
}

/// Backend-selected restart-safe application state.
pub struct StateStore {
    backend: Box<dyn StateBackend>,
}

impl StateStore {
    /// Opens the backend selected by `persistence.backend`.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected backend is unavailable or its state
    /// cannot be opened. Selecting file persistence never modifies an existing
    /// SQLite database.
    pub fn open(config: &Config) -> Result<Self, PersistenceError> {
        config.ensure_directories()?;
        let backend: Box<dyn StateBackend> = match config.persistence.backend {
            PersistenceBackend::Files => Box::new(FileStateStore::open(config)?),
            PersistenceBackend::Sqlite => {
                #[cfg(feature = "sqlite-state")]
                {
                    Box::new(SqliteStateStore::open(config)?)
                }
                #[cfg(not(feature = "sqlite-state"))]
                {
                    return Err(PersistenceError::BackendUnavailable {
                        backend: "sqlite",
                        required_feature: "sqlite-state",
                    });
                }
            }
        };
        Ok(Self { backend })
    }

    /// Opens an ephemeral human-readable-backend store for tests and transient
    /// application sessions.
    ///
    /// # Errors
    ///
    /// Returns an error if the initial documents cannot be validated.
    pub fn open_in_memory() -> Result<Self, PersistenceError> {
        Ok(Self {
            backend: Box::new(FileStateStore::open_in_memory()?),
        })
    }
}

impl Deref for StateStore {
    type Target = dyn StateBackend;

    fn deref(&self) -> &Self::Target {
        self.backend.as_ref()
    }
}

#[cfg(feature = "sqlite-state")]
impl StateBackend for SqliteStateStore {
    fn backend_name(&self) -> &'static str {
        "sqlite"
    }

    fn format_version(&self) -> Result<u32, PersistenceError> {
        self.schema_version()
    }

    fn create_playlist(
        &self,
        name: &str,
        description: Option<&str>,
        created_at: i64,
    ) -> Result<PlaylistCreateOutcome, PersistenceError> {
        SqliteStateStore::create_playlist(self, name, description, created_at)
    }

    fn create_playlist_with_entry(
        &self,
        name: &str,
        description: Option<&str>,
        media: &PlaylistMediaSnapshot,
        created_at: i64,
    ) -> Result<PlaylistCreateOutcome, PersistenceError> {
        SqliteStateStore::create_playlist_with_entry(self, name, description, media, created_at)
    }

    fn update_playlist(
        &self,
        playlist_id: &str,
        name: &str,
        description: Option<&str>,
        updated_at: i64,
    ) -> Result<Option<PlaylistSummary>, PersistenceError> {
        SqliteStateStore::update_playlist(self, playlist_id, name, description, updated_at)
    }

    fn playlists(&self) -> Result<Vec<PlaylistSummary>, PersistenceError> {
        SqliteStateStore::playlists(self)
    }

    fn playlist(&self, playlist_id: &str) -> Result<Option<Playlist>, PersistenceError> {
        SqliteStateStore::playlist(self, playlist_id)
    }

    fn playlist_contains(
        &self,
        playlist_id: &str,
        media_id: &MediaId,
    ) -> Result<bool, PersistenceError> {
        SqliteStateStore::playlist_contains(self, playlist_id, media_id)
    }

    fn playlist_memberships(
        &self,
        media_id: &MediaId,
    ) -> Result<Vec<PlaylistId>, PersistenceError> {
        SqliteStateStore::playlist_memberships(self, media_id)
    }

    fn playlist_memberships_with_names(
        &self,
        media_id: &MediaId,
    ) -> Result<Vec<PlaylistMembership>, PersistenceError> {
        SqliteStateStore::playlist_memberships_with_names(self, media_id)
    }

    fn add_playlist_entry(
        &self,
        playlist_id: &str,
        media: &PlaylistMediaSnapshot,
        added_at: i64,
    ) -> Result<PlaylistAddOutcome, PersistenceError> {
        SqliteStateStore::add_playlist_entry(self, playlist_id, media, added_at)
    }

    fn remove_playlist_entry(
        &self,
        playlist_id: &str,
        media_id: &MediaId,
        updated_at: i64,
    ) -> Result<bool, PersistenceError> {
        SqliteStateStore::remove_playlist_entry(self, playlist_id, media_id, updated_at)
    }

    fn toggle_playlist_entry(
        &self,
        playlist_id: &str,
        media: &PlaylistMediaSnapshot,
        updated_at: i64,
    ) -> Result<PlaylistToggleOutcome, PersistenceError> {
        SqliteStateStore::toggle_playlist_entry(self, playlist_id, media, updated_at)
    }

    fn add_to_todo(
        &self,
        media: &PlaylistMediaSnapshot,
        added_at: i64,
    ) -> Result<PlaylistAddOutcome, PersistenceError> {
        SqliteStateStore::add_to_todo(self, media, added_at)
    }

    fn toggle_todo(
        &self,
        media: &PlaylistMediaSnapshot,
        updated_at: i64,
    ) -> Result<PlaylistToggleOutcome, PersistenceError> {
        SqliteStateStore::toggle_todo(self, media, updated_at)
    }

    fn todo_contains(&self, media_id: &MediaId) -> Result<bool, PersistenceError> {
        SqliteStateStore::todo_contains(self, media_id)
    }

    fn toggle_radio_favorite(
        &self,
        media: &PlaylistMediaSnapshot,
        updated_at: i64,
    ) -> Result<PlaylistToggleOutcome, PersistenceError> {
        SqliteStateStore::toggle_radio_favorite(self, media, updated_at)
    }

    fn radio_favorite_contains(&self, media_id: &MediaId) -> Result<bool, PersistenceError> {
        SqliteStateStore::radio_favorite_contains(self, media_id)
    }

    #[cfg(feature = "yandex-music")]
    fn pending_yandex_music_reactions(
        &self,
    ) -> Result<Vec<PendingYandexMusicReaction>, PersistenceError> {
        SqliteStateStore::pending_yandex_music_reactions(self)
    }

    #[cfg(feature = "yandex-music")]
    fn queue_yandex_music_reaction(
        &self,
        account_uid: &str,
        track_id: &str,
        reaction: YandexMusicReaction,
        updated_at: i64,
    ) -> Result<PendingYandexMusicReaction, PersistenceError> {
        SqliteStateStore::queue_yandex_music_reaction(
            self,
            account_uid,
            track_id,
            reaction,
            updated_at,
        )
    }

    #[cfg(feature = "yandex-music")]
    fn acknowledge_yandex_music_reaction(
        &self,
        account_uid: &str,
        track_id: &str,
        generation: u64,
    ) -> Result<bool, PersistenceError> {
        SqliteStateStore::acknowledge_yandex_music_reaction(self, account_uid, track_id, generation)
    }

    fn checkpoint_playback(
        &self,
        progress: &PlaybackProgress,
        listen_source: &SourceKind,
        listened_seconds: u64,
    ) -> Result<(), PersistenceError> {
        SqliteStateStore::checkpoint_playback(self, progress, listen_source, listened_seconds)
    }

    fn checkpoint_listening(
        &self,
        source: &SourceKind,
        listened_seconds: u64,
    ) -> Result<(), PersistenceError> {
        SqliteStateStore::checkpoint_listening(self, source, listened_seconds)
    }

    fn flush_playback_checkpoint(&self) -> Result<(), PersistenceError> {
        Ok(())
    }

    fn upsert_progress(&self, progress: &PlaybackProgress) -> Result<(), PersistenceError> {
        SqliteStateStore::upsert_progress(self, progress)
    }

    fn progress(&self, media_id: &MediaId) -> Result<Option<PlaybackProgress>, PersistenceError> {
        SqliteStateStore::progress(self, media_id)
    }

    fn progress_for_media_ids(
        &self,
        media_ids: &[MediaId],
    ) -> Result<HashMap<MediaId, PlaybackProgress>, PersistenceError> {
        SqliteStateStore::progress_for_media_ids(self, media_ids)
    }

    fn delete_progress(&self, media_id: &MediaId) -> Result<bool, PersistenceError> {
        SqliteStateStore::delete_progress(self, media_id)
    }

    fn insert_history(&self, entry: &HistoryEntry) -> Result<i64, PersistenceError> {
        SqliteStateStore::insert_history(self, entry)
    }

    fn update_history(&self, entry: &HistoryEntry) -> Result<bool, PersistenceError> {
        SqliteStateStore::update_history(self, entry)
    }

    fn history(
        &self,
        finished_only: bool,
        limit: usize,
    ) -> Result<Vec<HistoryEntry>, PersistenceError> {
        SqliteStateStore::history(self, finished_only, limit)
    }

    fn delete_history(&self, id: i64) -> Result<bool, PersistenceError> {
        SqliteStateStore::delete_history(self, id)
    }

    fn insert_private_comment(&self, comment: &PrivateComment) -> Result<i64, PersistenceError> {
        SqliteStateStore::insert_private_comment(self, comment)
    }

    fn update_private_comment(&self, comment: &PrivateComment) -> Result<bool, PersistenceError> {
        SqliteStateStore::update_private_comment(self, comment)
    }

    fn private_comments(
        &self,
        target: &CommentTarget,
    ) -> Result<Vec<PrivateComment>, PersistenceError> {
        SqliteStateStore::private_comments(self, target)
    }

    fn private_note(
        &self,
        target: &CommentTarget,
    ) -> Result<Option<PrivateComment>, PersistenceError> {
        SqliteStateStore::private_note(self, target)
    }

    fn upsert_private_note(
        &self,
        target: &CommentTarget,
        body: &str,
        updated_at: i64,
    ) -> Result<PrivateComment, PersistenceError> {
        SqliteStateStore::upsert_private_note(self, target, body, updated_at)
    }

    fn delete_private_note(&self, target: &CommentTarget) -> Result<bool, PersistenceError> {
        SqliteStateStore::delete_private_note(self, target)
    }

    fn delete_private_comment(&self, id: i64) -> Result<bool, PersistenceError> {
        SqliteStateStore::delete_private_comment(self, id)
    }

    fn insert_bookmark(&self, bookmark: &Bookmark) -> Result<i64, PersistenceError> {
        SqliteStateStore::insert_bookmark(self, bookmark)
    }

    fn update_bookmark(&self, bookmark: &Bookmark) -> Result<bool, PersistenceError> {
        SqliteStateStore::update_bookmark(self, bookmark)
    }

    fn bookmarks(&self, media_id: &MediaId) -> Result<Vec<Bookmark>, PersistenceError> {
        SqliteStateStore::bookmarks(self, media_id)
    }

    fn delete_bookmark(&self, id: i64) -> Result<bool, PersistenceError> {
        SqliteStateStore::delete_bookmark(self, id)
    }

    fn save_session(&self, state: &SessionState, updated_at: i64) -> Result<(), PersistenceError> {
        SqliteStateStore::save_session(self, state, updated_at)
    }

    fn session(&self) -> Result<Option<SessionState>, PersistenceError> {
        SqliteStateStore::session(self)
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn remap_local_move_state(
        &self,
        mappings: &[LocalMoveMapping],
    ) -> Result<LocalMoveStateRemap, PersistenceError> {
        SqliteStateStore::remap_local_move_state(self, mappings)
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn journal_local_move_intent(
        &self,
        mappings: &[LocalMoveMapping],
        created_at: i64,
    ) -> Result<(), PersistenceError> {
        SqliteStateStore::journal_local_move_intent(self, mappings, created_at)
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn local_move_intents(&self) -> Result<Vec<LocalMoveMapping>, PersistenceError> {
        SqliteStateStore::local_move_intents(self)
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn discard_local_move_intents(
        &self,
        mappings: &[LocalMoveMapping],
    ) -> Result<(), PersistenceError> {
        SqliteStateStore::discard_local_move_intents(self, mappings)
    }

    fn save_youtube_search(
        &self,
        search: &SavedYouTubeSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        SqliteStateStore::save_youtube_search(self, search, updated_at)
    }

    fn youtube_search(&self) -> Result<Option<SavedYouTubeSearch>, PersistenceError> {
        SqliteStateStore::youtube_search(self)
    }

    fn update_saved_youtube_video_orientation(
        &self,
        video_id: &str,
        orientation: VideoOrientation,
        updated_at: i64,
    ) -> Result<bool, PersistenceError> {
        SqliteStateStore::update_saved_youtube_video_orientation(
            self,
            video_id,
            orientation,
            updated_at,
        )
    }

    fn clear_youtube_search(&self) -> Result<bool, PersistenceError> {
        SqliteStateStore::clear_youtube_search(self)
    }

    fn save_youtube_music_search(
        &self,
        search: &SavedYouTubeMusicSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        SqliteStateStore::save_youtube_music_search(self, search, updated_at)
    }

    fn youtube_music_search(&self) -> Result<Option<SavedYouTubeMusicSearch>, PersistenceError> {
        SqliteStateStore::youtube_music_search(self)
    }

    fn clear_youtube_music_search(&self) -> Result<bool, PersistenceError> {
        SqliteStateStore::clear_youtube_music_search(self)
    }

    fn save_bandcamp_search(
        &self,
        search: &SavedBandcampSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        SqliteStateStore::save_bandcamp_search(self, search, updated_at)
    }

    fn bandcamp_search(&self) -> Result<Option<SavedBandcampSearch>, PersistenceError> {
        SqliteStateStore::bandcamp_search(self)
    }

    fn clear_bandcamp_search(&self) -> Result<bool, PersistenceError> {
        SqliteStateStore::clear_bandcamp_search(self)
    }

    fn save_apple_podcasts_search(
        &self,
        search: &SavedApplePodcastsSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        SqliteStateStore::save_apple_podcasts_search(self, search, updated_at)
    }

    fn apple_podcasts_search(&self) -> Result<Option<SavedApplePodcastsSearch>, PersistenceError> {
        SqliteStateStore::apple_podcasts_search(self)
    }

    fn clear_apple_podcasts_search(&self) -> Result<bool, PersistenceError> {
        SqliteStateStore::clear_apple_podcasts_search(self)
    }

    fn put_cached_subscription_items(
        &self,
        cached: &CachedSubscriptionItems,
    ) -> Result<(), PersistenceError> {
        SqliteStateStore::put_cached_subscription_items(self, cached)
    }

    fn cached_subscription_items(
        &self,
        source: &SourceKind,
        source_id: &str,
    ) -> Result<Option<CachedSubscriptionItems>, PersistenceError> {
        SqliteStateStore::cached_subscription_items(self, source, source_id)
    }

    fn delete_cached_subscription_items(
        &self,
        source: &SourceKind,
        source_id: &str,
    ) -> Result<bool, PersistenceError> {
        SqliteStateStore::delete_cached_subscription_items(self, source, source_id)
    }

    fn add_listen_seconds(
        &self,
        source: &SourceKind,
        seconds: u64,
    ) -> Result<(), PersistenceError> {
        SqliteStateStore::add_listen_seconds(self, source, seconds)
    }

    fn listened_seconds(&self, source: &SourceKind) -> Result<u64, PersistenceError> {
        SqliteStateStore::listened_seconds(self, source)
    }

    fn listen_totals(&self) -> Result<Vec<ListenTotal>, PersistenceError> {
        SqliteStateStore::listen_totals(self)
    }

    fn reset_listen_seconds(&self, source: &SourceKind) -> Result<bool, PersistenceError> {
        SqliteStateStore::reset_listen_seconds(self, source)
    }

    fn put_cached_metadata(&self, cached: &CachedMetadata) -> Result<(), PersistenceError> {
        SqliteStateStore::put_cached_metadata(self, cached)
    }

    fn cached_metadata(
        &self,
        media_id: &MediaId,
    ) -> Result<Option<CachedMetadata>, PersistenceError> {
        SqliteStateStore::cached_metadata(self, media_id)
    }

    fn delete_expired_metadata(&self, now: i64) -> Result<usize, PersistenceError> {
        SqliteStateStore::delete_expired_metadata(self, now)
    }

    fn put_cached_channel_summary(
        &self,
        cached: &CachedChannelSummary,
    ) -> Result<(), PersistenceError> {
        SqliteStateStore::put_cached_channel_summary(self, cached)
    }

    fn cached_channel_summary(
        &self,
        channel_id: &str,
    ) -> Result<Option<CachedChannelSummary>, PersistenceError> {
        SqliteStateStore::cached_channel_summary(self, channel_id)
    }

    fn delete_expired_channel_summaries(&self, now: i64) -> Result<usize, PersistenceError> {
        SqliteStateStore::delete_expired_channel_summaries(self, now)
    }

    fn put_cached_wikidata(&self, cached: &CachedWikidataLookup) -> Result<(), PersistenceError> {
        SqliteStateStore::put_cached_wikidata(self, cached)
    }

    fn cached_wikidata(
        &self,
        property_id: &str,
        external_id: &str,
    ) -> Result<Option<CachedWikidataLookup>, PersistenceError> {
        SqliteStateStore::cached_wikidata(self, property_id, external_id)
    }

    fn delete_expired_wikidata(&self, now: i64) -> Result<usize, PersistenceError> {
        SqliteStateStore::delete_expired_wikidata(self, now)
    }
}

/// Alias emphasizing that [`StateStore`] is the persistence boundary.
pub type Store = StateStore;

/// Errors raised by the local state store.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    /// Application-directory preparation failed.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A database file could not be secured.
    #[error("database file operation failed: {0}")]
    Io(#[from] std::io::Error),
    /// A `SQLite` operation failed.
    #[cfg(feature = "sqlite-state")]
    #[error("SQLite state operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A human-readable TOML document is malformed.
    #[error("human-readable state document is invalid: {0}")]
    TomlDecode(#[from] toml::de::Error),
    /// A human-readable TOML document cannot be encoded.
    #[error("human-readable state document cannot be encoded: {0}")]
    TomlEncode(#[from] toml::ser::Error),
    /// A human-readable document exceeds its fixed read/write budget.
    #[error("{document} state document exceeds the {maximum_bytes}-byte limit")]
    StateDocumentTooLarge {
        /// Stable document label that does not expose a user path.
        document: &'static str,
        /// Maximum accepted UTF-8 size.
        maximum_bytes: usize,
    },
    /// A human-readable document violates an identity, ordering, or row invariant.
    #[error("{document} state document is invalid: {reason}")]
    InvalidFileDocument {
        /// Stable document label that does not expose a user path.
        document: &'static str,
        /// Rejected invariant.
        reason: String,
    },
    /// A bounded human-readable document cannot accept another durable row.
    #[error("{document} state document has reached its {maximum_rows}-row limit")]
    StateRowLimitReached {
        /// Stable document label that does not expose a user path.
        document: &'static str,
        /// Maximum number of retained rows.
        maximum_rows: usize,
    },
    /// A state file changed after this process loaded it.
    #[error(
        "{document} state document changed outside Youta; restart before saving to avoid \
         overwriting the external edit"
    )]
    StateDocumentChangedExternally {
        /// Stable document label that does not expose a user path.
        document: &'static str,
    },
    /// A multi-document publication stopped after at least one replacement.
    #[error(
        "a Local move state publication stopped after partially updating files; restart Youta \
         to reconcile the retained move journal before saving more state"
    )]
    PartialFilePublicationRequiresRestart,
    /// Another process already holds the human-readable state lock.
    #[error("human-readable state is already open by another Youta process")]
    FileStateAlreadyOpen,
    /// A JSON payload could not be encoded or decoded.
    #[error("stored JSON state is invalid: {0}")]
    Json(#[from] serde_json::Error),
    /// A cached provenance URL is malformed.
    #[error("stored metadata provenance URL is invalid: {0}")]
    InvalidCachedUrl(url::ParseError),
    /// A saved search JSON field exceeds its fixed persistence limit.
    #[error("saved search {field} exceeds the {maximum_bytes}-byte limit")]
    SavedSearchTooLarge {
        /// Snapshot field that exceeded its bound.
        field: &'static str,
        /// Maximum accepted encoded size.
        maximum_bytes: usize,
    },
    /// A saved search is internally inconsistent or outside provider limits.
    #[error("saved search is invalid: {reason}")]
    InvalidSavedSearch {
        /// Invariant rejected while saving or restoring.
        reason: String,
    },
    /// A subscribed-source first-page JSON payload exceeds its fixed limit.
    #[error("cached subscription items exceed the {maximum_bytes}-byte encoded limit")]
    SubscriptionSnapshotTooLarge {
        /// Maximum accepted encoded size.
        maximum_bytes: usize,
    },
    /// A subscribed-source snapshot violates an identity or item invariant.
    #[error("cached subscription items are invalid: {reason}")]
    InvalidSubscriptionSnapshot {
        /// Invariant rejected while saving, restoring, or deleting.
        reason: String,
    },
    /// A durable Yandex Music desired reaction violates a fixed invariant.
    #[cfg(feature = "yandex-music")]
    #[error("Yandex Music pending reaction is invalid: {reason}")]
    InvalidYandexMusicReaction {
        /// Rejected identity, ordering, timestamp, or reaction invariant.
        reason: String,
    },
    /// A replay locator is empty or exceeds its fixed persistence limit.
    #[error("history replay locator is invalid: {reason}")]
    InvalidHistoryReplayLocator {
        /// Invariant rejected before the locator reached `SQLite`.
        reason: String,
    },
    /// Playlist metadata, identity, ordering, or snapshots violate a bound.
    #[error("playlist state is invalid: {reason}")]
    InvalidPlaylist {
        /// Invariant rejected before use or while reading stored state.
        reason: String,
    },
    /// Completed Local move mappings or their durable destinations conflict.
    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[error("local move state remap is invalid: {reason}")]
    InvalidLocalMoveStateRemap {
        /// Mapping, path, row, or destination invariant that was rejected.
        reason: String,
    },
    /// The configured persistence backend was not compiled into this binary.
    #[error(
        "persistence backend {backend:?} is unavailable; rebuild Youta with feature \
         {required_feature:?}"
    )]
    BackendUnavailable {
        /// Configuration value that selected the unavailable backend.
        backend: &'static str,
        /// Cargo feature that enables it.
        required_feature: &'static str,
    },
    /// A state document uses a format newer than this binary.
    #[error("state document format {found} is newer than supported format {supported}")]
    UnsupportedFileFormat {
        /// Version read from TOML.
        found: u32,
        /// Newest supported TOML version.
        supported: u32,
    },
    /// An in-process state lock was poisoned by a prior panic.
    #[error("state lock is poisoned")]
    StateLockPoisoned,
    /// A domain value is outside the supported persistent integer range.
    #[error("{field} is outside the supported persistent integer range")]
    IntegerOutOfRange {
        /// Name of the field that overflowed.
        field: &'static str,
    },
    /// An on-disk database was unable to enter WAL mode.
    #[cfg(feature = "sqlite-state")]
    #[error("SQLite refused WAL journal mode and selected {0:?}")]
    WalUnavailable(String),
    /// The database was written by a newer Youta schema.
    #[cfg(feature = "sqlite-state")]
    #[error("database schema {found} is newer than supported schema {supported}")]
    UnsupportedSchema {
        /// Version stored in `SQLite`.
        found: u32,
        /// Newest version this binary understands.
        supported: u32,
    },
}

#[cfg(feature = "yandex-music")]
fn invalid_yandex_music_reaction(reason: impl Into<String>) -> PersistenceError {
    PersistenceError::InvalidYandexMusicReaction {
        reason: reason.into(),
    }
}

#[cfg(feature = "yandex-music")]
fn validate_yandex_music_reaction_identity(
    account_uid: &str,
    track_id: &str,
) -> Result<(), PersistenceError> {
    let valid_component = |value: &str, maximum_bytes: usize| {
        !value.is_empty()
            && value.len() <= maximum_bytes
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-')
            })
    };
    if !valid_component(account_uid, MAX_YANDEX_MUSIC_ACCOUNT_UID_BYTES) {
        return Err(invalid_yandex_music_reaction(format!(
            "account UID must contain only ASCII letters, digits, `.`, `_`, `:`, or `-` and be at most {MAX_YANDEX_MUSIC_ACCOUNT_UID_BYTES} bytes"
        )));
    }
    if !valid_component(track_id, MAX_YANDEX_MUSIC_TRACK_ID_BYTES) {
        return Err(invalid_yandex_music_reaction(format!(
            "track ID must contain only ASCII letters, digits, `.`, `_`, `:`, or `-` and be at most {MAX_YANDEX_MUSIC_TRACK_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

#[cfg(feature = "yandex-music")]
fn validate_yandex_music_reaction_generation(generation: u64) -> Result<(), PersistenceError> {
    if generation == 0 {
        return Err(invalid_yandex_music_reaction("generation must be positive"));
    }
    i64::try_from(generation)
        .map(|_| ())
        .map_err(|_| PersistenceError::IntegerOutOfRange {
            field: "Yandex Music reaction generation",
        })
}

#[cfg(feature = "yandex-music")]
fn next_yandex_music_reaction_generation(current: u64) -> Result<u64, PersistenceError> {
    let next = current
        .checked_add(1)
        .ok_or(PersistenceError::IntegerOutOfRange {
            field: "Yandex Music reaction generation",
        })?;
    validate_yandex_music_reaction_generation(next)?;
    Ok(next)
}

#[cfg(feature = "yandex-music")]
fn validate_yandex_music_reaction_timestamp(updated_at: i64) -> Result<(), PersistenceError> {
    if updated_at < 0 {
        return Err(invalid_yandex_music_reaction(
            "update timestamp cannot be negative",
        ));
    }
    Ok(())
}

#[cfg(feature = "yandex-music")]
fn validate_yandex_music_reaction_ledger_entry(
    entry: &YandexMusicReactionLedgerEntry,
) -> Result<(), PersistenceError> {
    validate_yandex_music_reaction_identity(&entry.account_uid, &entry.track_id)?;
    validate_yandex_music_reaction_generation(entry.generation)?;
    if entry.acknowledged_generation > entry.generation {
        return Err(invalid_yandex_music_reaction(
            "acknowledged generation cannot exceed the desired generation",
        ));
    }
    validate_yandex_music_reaction_timestamp(entry.updated_at)
}

#[cfg(all(feature = "yandex-music", feature = "sqlite-state"))]
const fn yandex_music_reaction_name(reaction: YandexMusicReaction) -> &'static str {
    match reaction {
        YandexMusicReaction::Neutral => "neutral",
        YandexMusicReaction::Liked => "liked",
        YandexMusicReaction::Disliked => "disliked",
    }
}

#[cfg(all(feature = "yandex-music", feature = "sqlite-state"))]
fn parse_yandex_music_reaction(value: &str) -> Result<YandexMusicReaction, PersistenceError> {
    match value {
        "neutral" => Ok(YandexMusicReaction::Neutral),
        "liked" => Ok(YandexMusicReaction::Liked),
        "disliked" => Ok(YandexMusicReaction::Disliked),
        _ => Err(invalid_yandex_music_reaction(format!(
            "stored reaction {value:?} is unknown"
        ))),
    }
}

fn invalid_playlist(reason: impl Into<String>) -> PersistenceError {
    PersistenceError::InvalidPlaylist {
        reason: reason.into(),
    }
}

fn validate_playlist_timestamp(timestamp: i64) -> Result<(), PersistenceError> {
    if timestamp < 0 {
        return Err(invalid_playlist("playlist timestamps cannot be negative"));
    }
    Ok(())
}

fn validate_playlist_id(playlist_id: &str) -> Result<(), PersistenceError> {
    if playlist_id.is_empty()
        || playlist_id.len() > MAX_PLAYLIST_ID_BYTES
        || playlist_id.trim() != playlist_id
        || playlist_id.chars().any(char::is_control)
    {
        return Err(invalid_playlist(format!(
            "playlist ID must be trimmed, printable, and at most {MAX_PLAYLIST_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

fn playlist_name_key(name: &str) -> String {
    name.chars().flat_map(char::to_lowercase).collect()
}

fn is_radio_favorites_name_key(name_key: &str) -> bool {
    name_key == playlist_name_key(RADIO_FAVORITES_PLAYLIST_NAME)
}

fn is_hidden_builtin_playlist_id(playlist_id: &str) -> bool {
    playlist_id == RADIO_FAVORITES_PLAYLIST_ID
}

fn validated_playlist_fields<'a>(
    name: &'a str,
    description: Option<&'a str>,
    timestamp: i64,
) -> Result<(&'a str, String, Option<&'a str>), PersistenceError> {
    validate_playlist_timestamp(timestamp)?;
    if name.is_empty()
        || name.len() > MAX_PLAYLIST_NAME_BYTES
        || name.trim() != name
        || name.chars().any(char::is_control)
    {
        return Err(invalid_playlist(format!(
            "playlist name must be trimmed, printable, and at most \
             {MAX_PLAYLIST_NAME_BYTES} bytes"
        )));
    }
    let name_key = playlist_name_key(name);
    if name_key.is_empty() || name_key.len() > MAX_PLAYLIST_NAME_BYTES {
        return Err(invalid_playlist(format!(
            "case-insensitive playlist name exceeds {MAX_PLAYLIST_NAME_BYTES} bytes"
        )));
    }
    if let Some(description) = description {
        if description.is_empty() {
            return Err(invalid_playlist(
                "an empty playlist description must be represented as None",
            ));
        }
        if description.len() > MAX_PLAYLIST_DESCRIPTION_BYTES {
            return Err(invalid_playlist(format!(
                "playlist description exceeds {MAX_PLAYLIST_DESCRIPTION_BYTES} bytes"
            )));
        }
        if description
            .chars()
            .any(|character| character == '\0' || character == '\r')
        {
            return Err(invalid_playlist(
                "playlist description cannot contain NUL or carriage-return characters",
            ));
        }
    }
    Ok((name, name_key, description))
}

fn validate_playlist_media_id(media_id: &MediaId) -> Result<(), PersistenceError> {
    let source = media_id.source.as_str();
    if source.is_empty()
        || source.len() > MAX_SAVED_SUBSCRIPTION_SOURCE_BYTES
        || source.trim() != source
        || source.chars().any(char::is_control)
    {
        return Err(invalid_playlist("playlist media source is invalid"));
    }
    if media_id.external_id.is_empty()
        || media_id.external_id.len() > MAX_PLAYLIST_EXTERNAL_ID_BYTES
        || media_id.external_id.chars().any(char::is_control)
    {
        return Err(invalid_playlist(format!(
            "playlist media identity must be printable and at most \
             {MAX_PLAYLIST_EXTERNAL_ID_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_radio_favorite_snapshot(media: &PlaylistMediaSnapshot) -> Result<(), PersistenceError> {
    if media.id.source != SourceKind::Radio || media.kind != MediaKind::LiveStream {
        return Err(invalid_playlist(
            "Radio favorites accept only live Radio station snapshots",
        ));
    }
    validate_playlist_snapshot(media)
}

fn validate_playlist_snapshot(media: &PlaylistMediaSnapshot) -> Result<(), PersistenceError> {
    validate_playlist_media_id(&media.id)?;
    if !matches!(
        media.kind,
        MediaKind::Video | MediaKind::Audio | MediaKind::PodcastEpisode | MediaKind::LiveStream
    ) {
        return Err(invalid_playlist(
            "playlist snapshots must represent playable media",
        ));
    }
    if media.title.is_empty()
        || media.title.len() > MAX_PLAYLIST_TITLE_BYTES
        || media.title.trim() != media.title
        || media.title.chars().any(char::is_control)
    {
        return Err(invalid_playlist(format!(
            "playlist item title must be trimmed, printable, and at most \
             {MAX_PLAYLIST_TITLE_BYTES} bytes"
        )));
    }
    if let Some(creator) = &media.creator
        && (creator.is_empty()
            || creator.len() > MAX_PLAYLIST_CREATOR_BYTES
            || creator.trim() != creator
            || creator.chars().any(char::is_control))
    {
        return Err(invalid_playlist(format!(
            "playlist item creator must be trimmed, printable, and at most \
             {MAX_PLAYLIST_CREATOR_BYTES} bytes"
        )));
    }
    validate_playlist_webpage(media)?;
    if let Some(thumbnail_url) = &media.thumbnail_url {
        validate_playlist_thumbnail_url(&media.id.source, thumbnail_url)?;
    }
    validate_playlist_replay_locator(media)?;
    Ok(())
}

fn validate_playlist_webpage(media: &PlaylistMediaSnapshot) -> Result<(), PersistenceError> {
    let url = &media.webpage_url;
    if url.as_str().len() > MAX_PLAYLIST_URL_BYTES
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid_playlist(
            "playlist webpage must be bounded and contain no credentials or fragment",
        ));
    }
    if media.id.source == SourceKind::Local {
        if url.query().is_some() {
            return Err(invalid_playlist(
                "Local playlist webpage must not contain a query",
            ));
        }
        let path = url.to_file_path().map_err(|()| {
            invalid_playlist("Local playlist webpage must be an absolute file URL")
        })?;
        if persisted_local_locator_path(&media.id.external_id).as_ref() != Some(&path)
            || !path.is_absolute()
        {
            return Err(invalid_playlist(
                "Local playlist webpage does not match its media identity",
            ));
        }
        return Ok(());
    }
    let public_scheme =
        url.scheme() == "https" || (media.id.source == SourceKind::Radio && url.scheme() == "http");
    if !public_scheme || url.host_str().is_none() || remote_url_has_non_public_host(url) {
        return Err(invalid_playlist(
            "remote playlist webpages must use public absolute HTTPS, except curated Radio pages may use public HTTP",
        ));
    }
    Ok(())
}

fn validate_playlist_thumbnail_url(source: &SourceKind, url: &Url) -> Result<(), PersistenceError> {
    if url.as_str().len() > MAX_PLAYLIST_URL_BYTES
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.query().is_some()
    {
        return Err(invalid_playlist(
            "playlist artwork must be bounded and contain no credentials, query, or fragment",
        ));
    }
    if source == &SourceKind::Local {
        let path = url
            .to_file_path()
            .map_err(|()| invalid_playlist("Local playlist artwork must use a file URL"))?;
        if !path.is_absolute() {
            return Err(invalid_playlist(
                "Local playlist artwork must use an absolute path",
            ));
        }
        return Ok(());
    }
    if url.scheme() != "https" || url.host_str().is_none() || remote_url_has_non_public_host(url) {
        return Err(invalid_playlist(
            "remote playlist artwork must use a public absolute HTTPS URL",
        ));
    }
    Ok(())
}

fn validate_playlist_replay_locator(media: &PlaylistMediaSnapshot) -> Result<(), PersistenceError> {
    if media.replay_locator.is_empty()
        || media.replay_locator.len() > MAX_PLAYLIST_URL_BYTES
        || media.replay_locator.chars().any(char::is_control)
    {
        return Err(invalid_playlist(format!(
            "playlist replay locator must be printable and at most \
             {MAX_PLAYLIST_URL_BYTES} bytes"
        )));
    }
    match media.id.source {
        SourceKind::Local => {
            let Some(path) = persisted_local_locator_path(&media.replay_locator) else {
                return Err(invalid_playlist(
                    "Local playlist replay locator must be an absolute path or file URL",
                ));
            };
            if !path.is_absolute()
                || path.file_name().is_none()
                || path.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::CurDir | std::path::Component::ParentDir
                    )
                })
                || persisted_local_locator_path(&media.id.external_id).as_ref() != Some(&path)
            {
                return Err(invalid_playlist(
                    "Local playlist replay locator must identify the same normalized absolute media path",
                ));
            }
        }
        SourceKind::YouTube => validate_youtube_playlist_snapshot(media)?,
        SourceKind::ApplePodcasts => validate_apple_playlist_snapshot(media)?,
        SourceKind::Bandcamp => validate_bandcamp_playlist_snapshot(media)?,
        SourceKind::LibriVox => validate_librivox_playlist_snapshot(media)?,
        _ => {
            if media.replay_locator != media.webpage_url.as_str()
                || media.webpage_url.query().is_some()
            {
                return Err(invalid_playlist(
                    "remote playlist replay must equal a canonical queryless webpage",
                ));
            }
        }
    }
    Ok(())
}

fn validate_librivox_playlist_snapshot(
    media: &PlaylistMediaSnapshot,
) -> Result<(), PersistenceError> {
    if media.kind != MediaKind::Audio
        || parse_librivox_section_external_id(&media.id.external_id).is_none()
    {
        return Err(invalid_playlist(
            "LibriVox playlist items must identify one book section",
        ));
    }
    let replay_url = Url::parse(&media.replay_locator)
        .map_err(|_| invalid_playlist("LibriVox playlist audio URL is malformed"))?;
    if !is_canonical_librivox_audio_url(&replay_url) {
        return Err(invalid_playlist(
            "LibriVox playlist replay must use a stable Archive.org MP3 download",
        ));
    }
    if !is_canonical_librivox_book_url(&media.webpage_url) && media.webpage_url != replay_url {
        return Err(invalid_playlist(
            "LibriVox playlist webpages must use the book page or matching stable chapter audio",
        ));
    }
    Ok(())
}

fn validate_youtube_playlist_snapshot(
    media: &PlaylistMediaSnapshot,
) -> Result<(), PersistenceError> {
    validate_youtube_video_id(&media.id.external_id)
        .map_err(|error| invalid_playlist(format!("invalid YouTube playlist item: {error}")))?;
    let expected = format!("https://www.youtube.com/watch?v={}", media.id.external_id);
    if media.webpage_url.as_str() != expected || media.replay_locator != expected {
        return Err(invalid_playlist(
            "YouTube playlist replay must use its canonical watch URL",
        ));
    }
    Ok(())
}

fn validate_apple_playlist_snapshot(media: &PlaylistMediaSnapshot) -> Result<(), PersistenceError> {
    if media.id.external_id.starts_with('0')
        || media.id.external_id.len() > 20
        || !media
            .id
            .external_id
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid_playlist(
            "Apple Podcasts playlist identity must be a positive episode ID",
        ));
    }
    if media.webpage_url.host_str() != Some("podcasts.apple.com")
        || media.webpage_url.port().is_some()
        || media.replay_locator != media.webpage_url.as_str()
    {
        return Err(invalid_playlist(
            "Apple Podcasts replay must use its canonical public episode page",
        ));
    }
    let query = media
        .webpage_url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    if query.as_slice() != [("i".to_owned(), media.id.external_id.clone())] {
        return Err(invalid_playlist(
            "Apple Podcasts episode page must contain only its matching `i` parameter",
        ));
    }
    Ok(())
}

fn validate_bandcamp_playlist_snapshot(
    media: &PlaylistMediaSnapshot,
) -> Result<(), PersistenceError> {
    let Some(host) = media
        .webpage_url
        .host_str()
        .and_then(|host| host.strip_suffix(".bandcamp.com"))
        .filter(|artist| !artist.is_empty() && !artist.contains('.'))
    else {
        return Err(invalid_playlist(
            "Bandcamp playlist items must use an artist subdomain",
        ));
    };
    let segments = match media.webpage_url.path_segments() {
        Some(segments) => segments.collect::<Vec<_>>(),
        None => Vec::new(),
    };
    let [kind, slug] = segments.as_slice() else {
        return Err(invalid_playlist(
            "Bandcamp playlist items must use one canonical track page",
        ));
    };
    let expected_id = format!("{host}/{kind}/{slug}");
    if *kind != "track"
        || slug.is_empty()
        || media.webpage_url.query().is_some()
        || media.webpage_url.port().is_some()
        || media.id.external_id != expected_id
        || media.replay_locator != media.webpage_url.as_str()
    {
        return Err(invalid_playlist(
            "Bandcamp playlist identity and canonical track page do not match",
        ));
    }
    Ok(())
}

fn encoded_playlist_snapshot(media: &PlaylistMediaSnapshot) -> Result<String, PersistenceError> {
    validate_playlist_snapshot(media)?;
    let encoded = serde_json::to_string(media)?;
    if encoded.len() > MAX_PLAYLIST_SNAPSHOT_BYTES {
        return Err(invalid_playlist(format!(
            "playlist snapshot exceeds {MAX_PLAYLIST_SNAPSHOT_BYTES} encoded bytes"
        )));
    }
    Ok(encoded)
}

fn validate_playlist_segment(
    segment: &crate::domain::Segment,
    media: &PlaylistMediaSnapshot,
) -> Result<(), PersistenceError> {
    if segment.media_id != media.id {
        return Err(invalid_playlist(
            "playlist segment identity does not match its media snapshot",
        ));
    }
    if segment.id < 0
        || segment.created_at < 0
        || segment.end_seconds <= segment.start_seconds
        || media
            .duration_seconds
            .is_some_and(|duration| segment.end_seconds > duration)
    {
        return Err(invalid_playlist(
            "playlist segment has an invalid identity, timestamp, or range",
        ));
    }
    if let Some(label) = &segment.label
        && (label.is_empty()
            || label.len() > MAX_PLAYLIST_TITLE_BYTES
            || label.trim() != label
            || label.chars().any(char::is_control))
    {
        return Err(invalid_playlist(format!(
            "playlist segment label must be trimmed, printable, and at most \
             {MAX_PLAYLIST_TITLE_BYTES} bytes"
        )));
    }
    Ok(())
}

#[cfg(feature = "sqlite-state")]
fn ensure_playlist_capacity(connection: &Connection) -> Result<(), PersistenceError> {
    let count: i64 =
        connection.query_row("SELECT COUNT(*) FROM playlists", [], |row| row.get(0))?;
    let count = usize::try_from(count)
        .map_err(|_| invalid_playlist("playlist count is outside the supported range"))?;
    if count >= MAX_PLAYLISTS {
        return Err(invalid_playlist(format!(
            "playlist count has reached the {MAX_PLAYLISTS}-playlist limit"
        )));
    }
    Ok(())
}

#[cfg(feature = "sqlite-state")]
fn next_playlist_id(connection: &Connection) -> Result<PlaylistId, PersistenceError> {
    for _ in 0..4 {
        let id: String =
            connection.query_row("SELECT 'local:' || lower(hex(randomblob(16)))", [], |row| {
                row.get(0)
            })?;
        validate_playlist_id(&id)?;
        let exists = connection
            .query_row("SELECT 1 FROM playlists WHERE id = ?1", [&id], |_| Ok(()))
            .optional()?
            .is_some();
        if !exists {
            return Ok(id);
        }
    }
    Err(invalid_playlist(
        "could not allocate a collision-free local playlist ID",
    ))
}

#[cfg(feature = "sqlite-state")]
fn insert_playlist(
    connection: &Connection,
    id: &str,
    name: &str,
    name_key: &str,
    description: Option<&str>,
    created_at: i64,
) -> Result<(), PersistenceError> {
    validate_playlist_id(id)?;
    connection
        .prepare_cached(
            r"
			INSERT INTO playlists (
				id, name, name_key, description, created_at, updated_at
			) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
			",
        )?
        .execute(params![id, name, name_key, description, created_at])?;
    Ok(())
}

#[cfg(feature = "sqlite-state")]
fn ensure_todo_playlist(
    connection: &Connection,
    created_at: i64,
) -> Result<bool, PersistenceError> {
    if playlist_summary_by_id(connection, TODO_PLAYLIST_ID)?.is_some() {
        return Ok(false);
    }
    if let Some(owner) = playlist_summary_by_name_key(connection, TODO_PLAYLIST_NAME)? {
        return Err(invalid_playlist(format!(
            "reserved todo name is unexpectedly owned by `{}`",
            owner.id
        )));
    }
    ensure_playlist_capacity(connection)?;
    insert_playlist(
        connection,
        TODO_PLAYLIST_ID,
        TODO_PLAYLIST_NAME,
        TODO_PLAYLIST_NAME,
        None,
        created_at,
    )?;
    Ok(true)
}

#[cfg(feature = "sqlite-state")]
fn ensure_radio_favorites_playlist(
    connection: &Connection,
    created_at: i64,
) -> Result<bool, PersistenceError> {
    if playlist_summary_by_id(connection, RADIO_FAVORITES_PLAYLIST_ID)?.is_some() {
        return Ok(false);
    }
    let name_key = playlist_name_key(RADIO_FAVORITES_PLAYLIST_NAME);
    if let Some(owner) = playlist_summary_by_name_key(connection, &name_key)? {
        return Err(invalid_playlist(format!(
            "reserved Radio favorites name is unexpectedly owned by `{}`",
            owner.id
        )));
    }
    ensure_playlist_capacity(connection)?;
    insert_playlist(
        connection,
        RADIO_FAVORITES_PLAYLIST_ID,
        RADIO_FAVORITES_PLAYLIST_NAME,
        &name_key,
        None,
        created_at,
    )?;
    Ok(true)
}

#[cfg(feature = "sqlite-state")]
fn playlist_summary_from_row(row: &Row<'_>) -> rusqlite::Result<PlaylistSummary> {
    let count: i64 = row.get(3)?;
    let entry_count = usize::try_from(count).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, Type::Integer, Box::new(error))
    })?;
    Ok(PlaylistSummary {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        entry_count,
    })
}

#[cfg(feature = "sqlite-state")]
fn validate_playlist_summary(summary: &PlaylistSummary) -> Result<(), PersistenceError> {
    validate_playlist_id(&summary.id)?;
    validated_playlist_fields(&summary.name, summary.description.as_deref(), 0)?;
    if summary.entry_count > MAX_PLAYLIST_ENTRIES {
        return Err(invalid_playlist(format!(
            "playlist `{}` exceeds the {MAX_PLAYLIST_ENTRIES}-entry limit",
            summary.id
        )));
    }
    Ok(())
}

#[cfg(feature = "sqlite-state")]
fn playlist_summary_by_id(
    connection: &Connection,
    playlist_id: &str,
) -> Result<Option<PlaylistSummary>, PersistenceError> {
    let summary = connection
        .prepare_cached(
            r"
			SELECT p.id, p.name, p.description, COUNT(e.id)
			FROM playlists AS p
			LEFT JOIN playlist_entries AS e ON e.playlist_id = p.id
			WHERE p.id = ?1
			GROUP BY p.id, p.name, p.description
			",
        )?
        .query_row([playlist_id], playlist_summary_from_row)
        .optional()?;
    if let Some(summary) = &summary {
        validate_playlist_summary(summary)?;
    }
    Ok(summary)
}

#[cfg(feature = "sqlite-state")]
fn playlist_summary_by_name_key(
    connection: &Connection,
    name_key: &str,
) -> Result<Option<PlaylistSummary>, PersistenceError> {
    let summary = connection
        .prepare_cached(
            r"
			SELECT p.id, p.name, p.description, COUNT(e.id)
			FROM playlists AS p
			LEFT JOIN playlist_entries AS e ON e.playlist_id = p.id
			WHERE p.name_key = ?1
			GROUP BY p.id, p.name, p.description
			",
        )?
        .query_row([name_key], playlist_summary_from_row)
        .optional()?;
    if let Some(summary) = &summary {
        validate_playlist_summary(summary)?;
    }
    Ok(summary)
}

#[cfg(feature = "sqlite-state")]
fn playlist_summaries(connection: &Connection) -> Result<Vec<PlaylistSummary>, PersistenceError> {
    let mut statement = connection.prepare_cached(
        r"
		SELECT p.id, p.name, p.description, COUNT(e.id)
		FROM playlists AS p
		LEFT JOIN playlist_entries AS e ON e.playlist_id = p.id
		WHERE p.id != ?1
		GROUP BY p.id, p.name, p.description, p.name_key
		ORDER BY p.name_key, p.id
		LIMIT ?2
		",
    )?;
    let limit = i64::try_from(MAX_PLAYLISTS.saturating_add(1)).expect("playlist bound fits SQLite");
    let summaries = statement
        .query_map(
            params![RADIO_FAVORITES_PLAYLIST_ID, limit],
            playlist_summary_from_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    if summaries.len() > MAX_PLAYLISTS {
        return Err(invalid_playlist(format!(
            "playlist count exceeds the {MAX_PLAYLISTS}-playlist limit"
        )));
    }
    for summary in &summaries {
        validate_playlist_summary(summary)?;
    }
    Ok(summaries)
}

#[cfg(feature = "sqlite-state")]
fn playlist_contains_connection(
    connection: &Connection,
    playlist_id: &str,
    media_id: &MediaId,
) -> Result<bool, PersistenceError> {
    Ok(connection
        .prepare_cached(
            r"
			SELECT 1
			FROM playlist_entries
			WHERE playlist_id = ?1 AND source = ?2 AND external_id = ?3
			      AND segment_json IS NULL
			LIMIT 1
			",
        )?
        .query_row(
            params![playlist_id, media_id.source.as_str(), media_id.external_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

#[cfg(feature = "sqlite-state")]
fn add_playlist_entry_in_transaction(
    connection: &Connection,
    playlist_id: &str,
    media: &PlaylistMediaSnapshot,
    snapshot_json: &str,
    added_at: i64,
) -> Result<PlaylistAddOutcome, PersistenceError> {
    if playlist_summary_by_id(connection, playlist_id)?.is_none() {
        return Err(invalid_playlist(format!(
            "playlist `{playlist_id}` does not exist"
        )));
    }
    if playlist_contains_connection(connection, playlist_id, &media.id)? {
        return Ok(PlaylistAddOutcome::AlreadyPresent);
    }
    let (count, maximum_position): (i64, Option<i64>) = connection.query_row(
        "SELECT COUNT(*), MAX(position) FROM playlist_entries WHERE playlist_id = ?1",
        [playlist_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let count = usize::try_from(count)
        .map_err(|_| invalid_playlist("playlist entry count is outside the supported range"))?;
    if count >= MAX_PLAYLIST_ENTRIES {
        return Err(invalid_playlist(format!(
            "playlist `{playlist_id}` has reached the {MAX_PLAYLIST_ENTRIES}-entry limit"
        )));
    }
    let position = maximum_position
        .map_or(Some(0), |position| position.checked_add(1))
        .ok_or_else(|| invalid_playlist("playlist position is too large"))?;
    connection
        .prepare_cached(
            r"
			INSERT INTO playlist_entries (
				playlist_id, position, source, external_id,
				snapshot_json, segment_json, added_at
			) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)
			",
        )?
        .execute(params![
            playlist_id,
            position,
            media.id.source.as_str(),
            media.id.external_id,
            snapshot_json,
            added_at,
        ])?;
    connection
        .prepare_cached("UPDATE playlists SET updated_at = ?1 WHERE id = ?2")?
        .execute(params![added_at, playlist_id])?;
    Ok(PlaylistAddOutcome::Added)
}

#[cfg(feature = "sqlite-state")]
fn remove_playlist_entry_in_transaction(
    connection: &Connection,
    playlist_id: &str,
    media_id: &MediaId,
    updated_at: i64,
) -> Result<bool, PersistenceError> {
    let removed = connection
        .prepare_cached(
            r"
			DELETE FROM playlist_entries
			WHERE playlist_id = ?1 AND source = ?2 AND external_id = ?3
			      AND segment_json IS NULL
			",
        )?
        .execute(params![
            playlist_id,
            media_id.source.as_str(),
            media_id.external_id
        ])?
        > 0;
    if removed {
        connection
            .prepare_cached("UPDATE playlists SET updated_at = ?1 WHERE id = ?2")?
            .execute(params![updated_at, playlist_id])?;
    }
    Ok(removed)
}

#[cfg(feature = "sqlite-state")]
fn playlist_entry_from_row(row: &Row<'_>) -> rusqlite::Result<PlaylistEntry> {
    let source = SourceKind::from(row.get::<_, String>(0)?.as_str());
    let external_id = row.get::<_, String>(1)?;
    let snapshot_json = row.get::<_, String>(2)?;
    if snapshot_json.len() > MAX_PLAYLIST_SNAPSHOT_BYTES {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            2,
            Type::Text,
            Box::new(invalid_playlist(format!(
                "stored playlist snapshot exceeds {MAX_PLAYLIST_SNAPSHOT_BYTES} bytes"
            ))),
        ));
    }
    let media = serde_json::from_str::<PlaylistMediaSnapshot>(&snapshot_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(error))
    })?;
    validate_playlist_snapshot(&media).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(error))
    })?;
    if media.id != MediaId::new(source, external_id) {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            2,
            Type::Text,
            Box::new(invalid_playlist(
                "playlist row identity does not match its encoded snapshot",
            )),
        ));
    }
    let segment_json = row.get::<_, Option<String>>(3)?;
    if segment_json
        .as_ref()
        .is_some_and(|json| json.len() > MAX_PLAYLIST_SEGMENT_BYTES)
    {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            3,
            Type::Text,
            Box::new(invalid_playlist(format!(
                "stored playlist segment exceeds {MAX_PLAYLIST_SEGMENT_BYTES} bytes"
            ))),
        ));
    }
    let segment: Option<crate::domain::Segment> = segment_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(3, Type::Text, Box::new(error))
        })?;
    if let Some(segment) = &segment {
        validate_playlist_segment(segment, &media).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(3, Type::Text, Box::new(error))
        })?;
    }
    let added_at = row.get(4)?;
    validate_playlist_timestamp(added_at).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(4, Type::Integer, Box::new(error))
    })?;
    Ok(PlaylistEntry {
        media,
        segment,
        added_at,
    })
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
#[derive(Debug)]
struct LocalIdentityUpdate {
    old_external_id: String,
    new_external_id: String,
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
#[derive(Debug)]
struct LocalHistoryUpdate {
    id: i64,
    external_id: String,
    replay_locator: Option<String>,
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
#[derive(Debug)]
struct LocalJsonUpdate {
    id: i64,
    json: String,
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
#[derive(Debug)]
struct LocalSessionUpdate {
    slot: String,
    state_json: String,
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
#[derive(Debug)]
struct LocalMetadataUpdate {
    old_external_id: String,
    new_external_id: String,
    media_json: String,
    source_url: Option<String>,
}

/// One validated rewrite of a Local playlist row after a filesystem move.
#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
#[derive(Debug)]
struct LocalPlaylistEntryUpdate {
    /// Surrogate row identity used for the guarded update.
    id: i64,
    /// Provider identity after remapping the path prefix.
    external_id: String,
    /// Bounded playlist snapshot with remapped identity and file URLs.
    snapshot_json: String,
    /// Optional bounded segment with its remapped media identity.
    segment_json: Option<String>,
}

/// Raw bounded fields read for one Local playlist entry during move planning.
#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
#[derive(Debug)]
struct LocalPlaylistEntryRow {
    id: i64,
    playlist_id: String,
    external_id: String,
    snapshot_json: String,
    segment_json: Option<String>,
}

/// Collision identity and optional rewrite produced for one Local playlist row.
#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
#[derive(Debug)]
struct PreparedLocalPlaylistEntry {
    whole_media_owner: Option<((String, String), String)>,
    update: Option<LocalPlaylistEntryUpdate>,
}

/// One validated rewrite of a Local subscription snapshot.
///
/// The row key and its embedded summaries are applied together so readers can
/// never observe a moved folder identity paired with stale descendant paths.
#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
#[derive(Debug)]
struct LocalSubscriptionItemsUpdate {
    /// Folder identity currently stored in the composite primary key.
    old_source_id: String,
    /// Folder identity after applying the completed move mappings.
    new_source_id: String,
    /// Bounded JSON containing remapped item identities and `file:` URLs.
    items_json: String,
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
#[derive(Debug, Default)]
struct LocalMoveStatePlan {
    playback_progress: Vec<LocalIdentityUpdate>,
    playback_history: Vec<LocalHistoryUpdate>,
    private_comments: Vec<LocalJsonUpdate>,
    bookmarks: Vec<LocalJsonUpdate>,
    sessions: Vec<LocalSessionUpdate>,
    metadata_cache: Vec<LocalMetadataUpdate>,
    subscription_items_cache: Vec<LocalSubscriptionItemsUpdate>,
    playlist_entries: Vec<LocalPlaylistEntryUpdate>,
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
impl LocalMoveStatePlan {
    fn report(&self) -> LocalMoveStateRemap {
        LocalMoveStateRemap {
            playback_progress: self.playback_progress.len(),
            playback_history: self.playback_history.len(),
            private_comments: self.private_comments.len(),
            bookmarks: self.bookmarks.len(),
            sessions: self.sessions.len(),
            metadata_cache: self.metadata_cache.len(),
            subscription_items_cache: self.subscription_items_cache.len(),
            playlist_entries: self.playlist_entries.len(),
        }
    }
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
fn invalid_local_move_state(reason: impl Into<String>) -> PersistenceError {
    PersistenceError::InvalidLocalMoveStateRemap {
        reason: reason.into(),
    }
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
fn local_move_path_text<'a>(
    path: &'a Path,
    field: &'static str,
) -> Result<&'a str, PersistenceError> {
    path.to_str().ok_or_else(|| {
        invalid_local_move_state(format!(
            "move journal {field} is not valid UTF-8: `{}`",
            path.display()
        ))
    })
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
fn validate_local_move_mappings(mappings: &[LocalMoveMapping]) -> Result<(), PersistenceError> {
    if mappings.len() > MAX_LOCAL_MOVE_MAPPINGS {
        return Err(invalid_local_move_state(format!(
            "mapping count {} exceeds the {MAX_LOCAL_MOVE_MAPPINGS}-mapping limit",
            mappings.len()
        )));
    }

    for (index, mapping) in mappings.iter().enumerate() {
        validate_local_move_mapping_path(&mapping.source, index, "source")?;
        validate_local_move_mapping_path(&mapping.target, index, "target")?;
        if mapping.source == mapping.target {
            return Err(invalid_local_move_state(format!(
                "mapping {index} has the same source and target"
            )));
        }
    }

    let mut sources = mappings
        .iter()
        .enumerate()
        .map(|(index, mapping)| (mapping.source.as_path(), index))
        .collect::<Vec<_>>();
    sources.sort_unstable_by_key(|(path, _)| *path);
    if let Some([(first, _), (second, _)]) = sources
        .windows(2)
        .find(|pair| local_paths_overlap(pair[0].0, pair[1].0))
    {
        return Err(invalid_local_move_state(format!(
            "mapping sources overlap: `{}` and `{}`",
            first.display(),
            second.display()
        )));
    }

    let mut targets = mappings
        .iter()
        .enumerate()
        .map(|(index, mapping)| (mapping.target.as_path(), index))
        .collect::<Vec<_>>();
    targets.sort_unstable_by_key(|(path, _)| *path);
    if let Some([(first, _), (second, _)]) = targets
        .windows(2)
        .find(|pair| local_paths_overlap(pair[0].0, pair[1].0))
    {
        return Err(invalid_local_move_state(format!(
            "mapping targets overlap: `{}` and `{}`",
            first.display(),
            second.display()
        )));
    }

    let mut all_paths = Vec::with_capacity(mappings.len().saturating_mul(2));
    for (index, mapping) in mappings.iter().enumerate() {
        all_paths.push((mapping.source.as_path(), index, true));
        all_paths.push((mapping.target.as_path(), index, false));
    }
    all_paths.sort_unstable_by_key(|(path, _, _)| *path);
    if let Some(
        [
            (first, first_index, first_is_source),
            (second, second_index, second_is_source),
        ],
    ) = all_paths
        .windows(2)
        .find(|pair| pair[0].2 != pair[1].2 && local_paths_overlap(pair[0].0, pair[1].0))
    {
        let (source, source_index, target, target_index) = if *first_is_source {
            (first, first_index, second, second_index)
        } else {
            (second, second_index, first, first_index)
        };
        debug_assert_ne!(first_is_source, second_is_source);
        return Err(invalid_local_move_state(format!(
            "mapping {source_index} source `{}` overlaps mapping {target_index} target `{}`",
            source.display(),
            target.display()
        )));
    }
    Ok(())
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
fn validate_local_move_mapping_path(
    path: &Path,
    index: usize,
    field: &'static str,
) -> Result<(), PersistenceError> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(invalid_local_move_state(format!(
            "mapping {index} {field} must be an absolute non-root path: `{}`",
            path.display()
        )));
    }
    if path.to_str().is_none() {
        return Err(invalid_local_move_state(format!(
            "mapping {index} {field} is not valid UTF-8: `{}`",
            path.display()
        )));
    }
    if path.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    }) {
        return Err(invalid_local_move_state(format!(
            "mapping {index} {field} is not normalized: `{}`",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
fn local_paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
#[cfg(feature = "sqlite-state")]
fn prepare_local_move_state_remap(
    connection: &Connection,
    mappings: &[LocalMoveMapping],
) -> Result<LocalMoveStatePlan, PersistenceError> {
    Ok(LocalMoveStatePlan {
        playback_progress: prepare_local_keyed_identity_updates(
            connection,
            "playback_progress",
            mappings,
        )?,
        playback_history: prepare_local_history_updates(connection, mappings)?,
        private_comments: prepare_local_comment_updates(connection, mappings)?,
        bookmarks: prepare_local_bookmark_updates(connection, mappings)?,
        sessions: prepare_local_session_updates(connection, mappings)?,
        metadata_cache: prepare_local_metadata_updates(connection, mappings)?,
        subscription_items_cache: prepare_local_subscription_items_updates(connection, mappings)?,
        playlist_entries: prepare_local_playlist_entry_updates(connection, mappings)?,
    })
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
#[cfg(feature = "sqlite-state")]
fn prepare_local_playlist_entry_updates(
    connection: &Connection,
    mappings: &[LocalMoveMapping],
) -> Result<Vec<LocalPlaylistEntryUpdate>, PersistenceError> {
    let mut statement = connection.prepare(
        r"
        SELECT id, playlist_id, external_id, snapshot_json, segment_json
        FROM playlist_entries
        WHERE source = 'local'
        ORDER BY playlist_id, position, id
        ",
    )?;
    let rows = statement
        .query_map([], local_playlist_entry_row_from_sql)?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    let maximum_rows = MAX_PLAYLISTS.saturating_mul(MAX_PLAYLIST_ENTRIES);
    if rows.len() > maximum_rows {
        return Err(invalid_local_move_state(format!(
            "playlist_entries exceeds the {maximum_rows}-row Local remap bound"
        )));
    }

    // Whole-media rows are unique within one playlist. Build the complete
    // final-owner map, including unchanged target rows, before applying any
    // update so a stale destination cannot be overwritten or merged.
    let mut whole_media_owners = HashMap::<(String, String), String>::with_capacity(rows.len());
    let mut updates = Vec::new();
    for row in rows {
        let prepared = prepare_one_local_playlist_entry(row, mappings)?;
        if let Some((identity, external_id)) = prepared.whole_media_owner
            && let Some(owner) = whole_media_owners.insert(identity.clone(), external_id.clone())
            && owner != external_id
        {
            return Err(invalid_local_move_state(format!(
                "playlist `{}` destination identity `{}` conflicts between \
                 `{owner}` and `{external_id}`",
                identity.0, identity.1
            )));
        }
        if let Some(update) = prepared.update {
            updates.push(update);
        }
    }
    Ok(updates)
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
#[cfg(feature = "sqlite-state")]
fn local_playlist_entry_row_from_sql(row: &Row<'_>) -> rusqlite::Result<LocalPlaylistEntryRow> {
    Ok(LocalPlaylistEntryRow {
        id: row.get(0)?,
        playlist_id: row.get(1)?,
        external_id: row.get(2)?,
        snapshot_json: row.get(3)?,
        segment_json: row.get(4)?,
    })
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
fn decode_local_playlist_entry(
    row: &LocalPlaylistEntryRow,
) -> Result<(PlaylistMediaSnapshot, Option<crate::domain::Segment>), PersistenceError> {
    validate_playlist_id(&row.playlist_id).map_err(|error| {
        invalid_local_move_state(format!(
            "playlist entry {} has invalid playlist identity: {error}",
            row.id
        ))
    })?;
    if row.snapshot_json.len() > MAX_PLAYLIST_SNAPSHOT_BYTES {
        return Err(invalid_local_move_state(format!(
            "playlist entry {} snapshot exceeds {MAX_PLAYLIST_SNAPSHOT_BYTES} bytes",
            row.id
        )));
    }
    if row
        .segment_json
        .as_ref()
        .is_some_and(|json| json.len() > MAX_PLAYLIST_SEGMENT_BYTES)
    {
        return Err(invalid_local_move_state(format!(
            "playlist entry {} segment exceeds {MAX_PLAYLIST_SEGMENT_BYTES} bytes",
            row.id
        )));
    }

    let media: PlaylistMediaSnapshot = serde_json::from_str(&row.snapshot_json)?;
    validate_playlist_snapshot(&media).map_err(|error| {
        invalid_local_move_state(format!(
            "playlist entry {} contains an invalid snapshot: {error}",
            row.id
        ))
    })?;
    if media.id.source != SourceKind::Local || media.id.external_id != row.external_id {
        return Err(invalid_local_move_state(format!(
            "playlist entry {} Local row identity does not match its snapshot",
            row.id
        )));
    }
    let segment = row
        .segment_json
        .as_deref()
        .map(serde_json::from_str::<crate::domain::Segment>)
        .transpose()?;
    if let Some(segment) = &segment {
        validate_playlist_segment(segment, &media).map_err(|error| {
            invalid_local_move_state(format!(
                "playlist entry {} contains an invalid segment: {error}",
                row.id
            ))
        })?;
    }
    Ok((media, segment))
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
fn prepare_one_local_playlist_entry(
    row: LocalPlaylistEntryRow,
    mappings: &[LocalMoveMapping],
) -> Result<PreparedLocalPlaylistEntry, PersistenceError> {
    let (mut media, mut segment) = decode_local_playlist_entry(&row)?;
    let identity_changed =
        remap_local_media_id(&mut media.id, mappings).map_err(local_identity_remap_error)?;
    let replay_changed = remap_local_replay_locator(&mut media.replay_locator, mappings)
        .map_err(local_identity_remap_error)?;
    let webpage_changed = remap_local_file_url(&mut media.webpage_url, mappings)?;
    let thumbnail_changed = media
        .thumbnail_url
        .as_mut()
        .map(|url| remap_local_file_url(url, mappings))
        .transpose()?
        .unwrap_or(false);
    let segment_changed = segment
        .as_mut()
        .map(|segment| {
            remap_local_media_id(&mut segment.media_id, mappings)
                .map_err(local_identity_remap_error)
        })
        .transpose()?
        .unwrap_or(false);
    let new_external_id = media.id.external_id.clone();
    let whole_media_owner = segment
        .is_none()
        .then(|| ((row.playlist_id, new_external_id.clone()), row.external_id));
    let changed = identity_changed
        || replay_changed
        || webpage_changed
        || thumbnail_changed
        || segment_changed;
    if !changed {
        return Ok(PreparedLocalPlaylistEntry {
            whole_media_owner,
            update: None,
        });
    }

    if let Some(segment) = &segment {
        validate_playlist_segment(segment, &media).map_err(|error| {
            invalid_local_move_state(format!(
                "playlist entry {} remapped segment is invalid: {error}",
                row.id
            ))
        })?;
    }
    let snapshot_json = encoded_playlist_snapshot(&media).map_err(|error| {
        invalid_local_move_state(format!(
            "playlist entry {} remapped snapshot is invalid: {error}",
            row.id
        ))
    })?;
    let segment_json = segment.as_ref().map(serde_json::to_string).transpose()?;
    if segment_json
        .as_ref()
        .is_some_and(|json| json.len() > MAX_PLAYLIST_SEGMENT_BYTES)
    {
        return Err(invalid_local_move_state(format!(
            "playlist entry {} remapped segment exceeds {MAX_PLAYLIST_SEGMENT_BYTES} bytes",
            row.id
        )));
    }
    Ok(PreparedLocalPlaylistEntry {
        whole_media_owner,
        update: Some(LocalPlaylistEntryUpdate {
            id: row.id,
            external_id: new_external_id,
            snapshot_json,
            segment_json,
        }),
    })
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
#[cfg(feature = "sqlite-state")]
fn prepare_local_keyed_identity_updates(
    connection: &Connection,
    table: &'static str,
    mappings: &[LocalMoveMapping],
) -> Result<Vec<LocalIdentityUpdate>, PersistenceError> {
    let sql = match table {
        "playback_progress" => {
            "SELECT external_id FROM playback_progress WHERE source = 'local' ORDER BY external_id"
        }
        _ => {
            return Err(invalid_local_move_state(format!(
                "unsupported keyed Local identity table `{table}`"
            )));
        }
    };
    let mut statement = connection.prepare(sql)?;
    let external_ids = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    let mut final_owners = HashMap::<String, String>::with_capacity(external_ids.len());
    let mut updates = Vec::new();
    for old_external_id in external_ids {
        let new_external_id = remapped_local_external_id(&old_external_id, mappings)?
            .unwrap_or_else(|| old_external_id.clone());
        if let Some(owner) = final_owners.insert(new_external_id.clone(), old_external_id.clone()) {
            return Err(invalid_local_move_state(format!(
                "{table} destination identity `{new_external_id}` conflicts between `{owner}` and `{old_external_id}`"
            )));
        }
        if new_external_id != old_external_id {
            updates.push(LocalIdentityUpdate {
                old_external_id,
                new_external_id,
            });
        }
    }
    Ok(updates)
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
#[cfg(feature = "sqlite-state")]
fn prepare_local_subscription_items_updates(
    connection: &Connection,
    mappings: &[LocalMoveMapping],
) -> Result<Vec<LocalSubscriptionItemsUpdate>, PersistenceError> {
    let scan_limit =
        i64::try_from(MAX_SAVED_SUBSCRIPTION_SOURCES.saturating_add(1)).map_err(|_| {
            PersistenceError::IntegerOutOfRange {
                field: "subscription source scan limit",
            }
        })?;
    let mut bounds_statement = connection.prepare(
        r"
        SELECT source_id, length(CAST(items_json AS BLOB)), fetched_at
        FROM subscription_items_cache
        WHERE source = 'local'
        ORDER BY source_id
        LIMIT ?1
        ",
    )?;
    let row_bounds = bounds_statement
        .query_map([scan_limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(bounds_statement);

    if row_bounds.len() > MAX_SAVED_SUBSCRIPTION_SOURCES {
        return Err(invalid_local_move_state(format!(
            "subscription_items_cache contains more than the {MAX_SAVED_SUBSCRIPTION_SOURCES}-source scan bound"
        )));
    }
    let maximum_aggregate_bytes =
        MAX_SAVED_SUBSCRIPTION_SOURCES.saturating_mul(MAX_SAVED_SUBSCRIPTION_ITEMS_BYTES);
    let mut aggregate_bytes = 0_usize;
    for (source_id, items_bytes, fetched_at) in &row_bounds {
        validate_subscription_source_identity(&SourceKind::Local, source_id)?;
        let items_bytes = usize::try_from(*items_bytes).unwrap_or(usize::MAX);
        ensure_subscription_snapshot_json_bound(items_bytes)?;
        aggregate_bytes = aggregate_bytes.saturating_add(items_bytes);
        if *fetched_at < 0 {
            return Err(PersistenceError::InvalidSubscriptionSnapshot {
                reason: format!(
                    "Local subscription snapshot `{source_id}` has a negative fetch time"
                ),
            });
        }
    }
    if aggregate_bytes > maximum_aggregate_bytes {
        return Err(invalid_local_move_state(format!(
            "subscription_items_cache exceeds the {maximum_aggregate_bytes}-byte aggregate scan bound"
        )));
    }

    // Read JSON only after every Local row passed the byte bound. The enclosing
    // SQLite transaction keeps this second query on the same database snapshot.
    let mut items_statement = connection.prepare(
        r"
        SELECT source_id, items_json, fetched_at
        FROM subscription_items_cache
        WHERE source = 'local'
        ORDER BY source_id
        LIMIT ?1
        ",
    )?;
    let rows = items_statement
        .query_map([scan_limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(items_statement);

    let mut final_owners = HashMap::<String, String>::with_capacity(rows.len());
    let mut updates = Vec::new();
    for (old_source_id, old_items_json, fetched_at) in rows {
        let cached = CachedSubscriptionItems {
            source: SourceKind::Local,
            source_id: old_source_id.clone(),
            items: serde_json::from_str(&old_items_json)?,
            fetched_at,
        };
        validate_cached_subscription_items(&cached)?;
        let mut items = cached.items;

        let new_source_id = remapped_local_external_id(&old_source_id, mappings)?
            .unwrap_or_else(|| old_source_id.clone());
        validate_subscription_source_identity(&SourceKind::Local, &new_source_id)?;
        if let Some(owner) = final_owners.insert(new_source_id.clone(), old_source_id.clone()) {
            return Err(invalid_local_move_state(format!(
                "subscription_items_cache destination identity `{new_source_id}` conflicts between `{owner}` and `{old_source_id}`"
            )));
        }

        let items_changed = remap_local_subscription_items(&old_source_id, &mut items, mappings)?;
        if new_source_id != old_source_id || items_changed {
            let items_json = if items_changed {
                let items_json = serde_json::to_string(&items)?;
                ensure_subscription_snapshot_json_bound(items_json.len())?;
                items_json
            } else {
                old_items_json
            };
            updates.push(LocalSubscriptionItemsUpdate {
                old_source_id,
                new_source_id,
                items_json,
            });
        }
    }
    Ok(updates)
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
fn remap_local_subscription_items(
    source_id: &str,
    items: &mut [SearchItem],
    mappings: &[LocalMoveMapping],
) -> Result<bool, PersistenceError> {
    let mut changed = false;
    let mut final_media_owners = HashMap::<String, String>::with_capacity(items.len());
    for item in items {
        let SearchItem::Video(video) = item else {
            return Err(invalid_local_move_state(format!(
                "Local subscription snapshot `{source_id}` contains a non-playable item"
            )));
        };

        let old_video_id = video.video_id.clone();
        if let Some(video_id) = remapped_local_external_id(&old_video_id, mappings)? {
            video.video_id = video_id;
            changed = true;
        }
        if let Some(owner) = final_media_owners.insert(video.video_id.clone(), old_video_id.clone())
            && owner != old_video_id
        {
            return Err(invalid_local_move_state(format!(
                "Local subscription snapshot `{source_id}` has colliding remapped media identity `{}` from `{owner}` and `{old_video_id}`",
                video.video_id
            )));
        }

        if let Some(channel_id) = remapped_local_external_id(&video.channel_id, mappings)? {
            video.channel_id = channel_id;
            changed = true;
        }
        for thumbnail in &mut video.thumbnails {
            changed |= remap_local_file_url(&mut thumbnail.url, mappings)?;
        }
        if let Some(webpage_url) = &mut video.webpage_url {
            changed |= remap_local_file_url(webpage_url, mappings)?;
        }
        if let Some(stream_url) = &mut video.stream_url {
            changed |= remap_local_file_url(stream_url, mappings)?;
        }
    }
    Ok(changed)
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
#[cfg(feature = "sqlite-state")]
fn prepare_local_history_updates(
    connection: &Connection,
    mappings: &[LocalMoveMapping],
) -> Result<Vec<LocalHistoryUpdate>, PersistenceError> {
    let mut statement = connection.prepare(
        r"
        SELECT id, external_id, replay_locator
        FROM playback_history
        WHERE source = 'local'
        ORDER BY id
        ",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    let mut updates = Vec::new();
    for (id, old_external_id, old_replay_locator) in rows {
        let external_id = remapped_local_external_id(&old_external_id, mappings)?
            .unwrap_or_else(|| old_external_id.clone());
        let mut replay_locator = old_replay_locator.clone();
        let replay_changed = replay_locator
            .as_mut()
            .map(|locator| remap_local_replay_locator(locator, mappings))
            .transpose()
            .map_err(local_identity_remap_error)?
            .unwrap_or(false);
        if replay_changed {
            bounded_history_replay_locator(&SourceKind::Local, replay_locator.as_deref())?;
        }
        if external_id != old_external_id || replay_changed {
            updates.push(LocalHistoryUpdate {
                id,
                external_id,
                replay_locator,
            });
        }
    }
    Ok(updates)
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
#[cfg(feature = "sqlite-state")]
fn prepare_local_comment_updates(
    connection: &Connection,
    mappings: &[LocalMoveMapping],
) -> Result<Vec<LocalJsonUpdate>, PersistenceError> {
    let mut statement =
        connection.prepare("SELECT id, target_json FROM private_comments ORDER BY id")?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    let mut updates = Vec::new();
    for (id, target_json) in rows {
        let mut target: CommentTarget = serde_json::from_str(&target_json)?;
        let changed = match &mut target {
            CommentTarget::Media { media_id }
            | CommentTarget::Source {
                source_id: media_id,
            }
            | CommentTarget::Position { media_id, .. } => {
                remap_local_media_id(media_id, mappings).map_err(local_identity_remap_error)?
            }
            CommentTarget::Segment { .. } | CommentTarget::Subscription { .. } => false,
        };
        if changed {
            updates.push(LocalJsonUpdate {
                id,
                json: serde_json::to_string(&target)?,
            });
        }
    }
    Ok(updates)
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
#[cfg(feature = "sqlite-state")]
fn prepare_local_bookmark_updates(
    connection: &Connection,
    mappings: &[LocalMoveMapping],
) -> Result<Vec<LocalJsonUpdate>, PersistenceError> {
    let mut statement = connection
        .prepare("SELECT id, external_id FROM bookmarks WHERE source = 'local' ORDER BY id")?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    let mut updates = Vec::new();
    for (id, external_id) in rows {
        if let Some(external_id) = remapped_local_external_id(&external_id, mappings)? {
            updates.push(LocalJsonUpdate {
                id,
                json: external_id,
            });
        }
    }
    Ok(updates)
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
#[cfg(feature = "sqlite-state")]
fn prepare_local_session_updates(
    connection: &Connection,
    mappings: &[LocalMoveMapping],
) -> Result<Vec<LocalSessionUpdate>, PersistenceError> {
    let mut statement =
        connection.prepare("SELECT slot, state_json FROM session_state ORDER BY slot")?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    let mut updates = Vec::new();
    for (slot, state_json) in rows {
        let mut state: SessionState = serde_json::from_str(&state_json)?;
        let before = state.clone();
        if let Some(media_id) = &mut state.selected_media {
            remap_local_media_id(media_id, mappings).map_err(local_identity_remap_error)?;
        }
        if let Some(local_path) = &mut state.local_path {
            remap_local_string_path(local_path, mappings)?;
        }
        remap_local_screen(&mut state.screen, mappings)?;
        for screen in &mut state.back_stack {
            remap_local_screen(screen, mappings)?;
        }
        if state != before {
            updates.push(LocalSessionUpdate {
                slot,
                state_json: serde_json::to_string(&state)?,
            });
        }
    }
    Ok(updates)
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
#[cfg(feature = "sqlite-state")]
fn prepare_local_metadata_updates(
    connection: &Connection,
    mappings: &[LocalMoveMapping],
) -> Result<Vec<LocalMetadataUpdate>, PersistenceError> {
    let mut statement = connection.prepare(
        r"
        SELECT external_id, media_json, source_url
        FROM metadata_cache
        WHERE source = 'local'
        ORDER BY external_id
        ",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    let mut final_owners = HashMap::<String, String>::with_capacity(rows.len());
    let mut updates = Vec::new();
    for (old_external_id, media_json, source_url) in rows {
        let mut media: MediaItem = serde_json::from_str(&media_json)?;
        if media.id.source != SourceKind::Local || media.id.external_id != old_external_id {
            return Err(invalid_local_move_state(format!(
                "metadata_cache Local row `{old_external_id}` has an inconsistent embedded identity"
            )));
        }

        let identity_changed =
            remap_local_media_id(&mut media.id, mappings).map_err(local_identity_remap_error)?;
        let urls_changed = remap_local_media_file_urls(&mut media, mappings)?;
        let mut parsed_source_url = source_url
            .as_deref()
            .map(Url::parse)
            .transpose()
            .map_err(PersistenceError::InvalidCachedUrl)?;
        let source_url_changed = parsed_source_url
            .as_mut()
            .map(|url| remap_local_file_url(url, mappings))
            .transpose()?
            .unwrap_or(false);
        let new_external_id = media.id.external_id.clone();
        if let Some(owner) = final_owners.insert(new_external_id.clone(), old_external_id.clone()) {
            return Err(invalid_local_move_state(format!(
                "metadata_cache destination identity `{new_external_id}` conflicts between `{owner}` and `{old_external_id}`"
            )));
        }

        if identity_changed || urls_changed || source_url_changed {
            updates.push(LocalMetadataUpdate {
                old_external_id,
                new_external_id,
                media_json: serde_json::to_string(&media)?,
                source_url: parsed_source_url.map(|url| url.to_string()),
            });
        }
    }
    Ok(updates)
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
fn remapped_local_external_id(
    external_id: &str,
    mappings: &[LocalMoveMapping],
) -> Result<Option<String>, PersistenceError> {
    let mut media_id = MediaId::new(SourceKind::Local, external_id);
    if remap_local_media_id(&mut media_id, mappings).map_err(local_identity_remap_error)? {
        Ok(Some(media_id.external_id))
    } else {
        Ok(None)
    }
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
fn remap_local_string_path(
    path: &mut String,
    mappings: &[LocalMoveMapping],
) -> Result<bool, PersistenceError> {
    let Some(remapped) = remap_local_path_prefix(Path::new(path), mappings) else {
        return Ok(false);
    };
    let Some(remapped) = remapped.to_str() else {
        return Err(invalid_local_move_state(format!(
            "remapped Local path is not valid UTF-8: `{}`",
            remapped.display()
        )));
    };
    remapped.clone_into(path);
    Ok(true)
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
fn remap_local_screen(
    screen: &mut crate::domain::Screen,
    mappings: &[LocalMoveMapping],
) -> Result<bool, PersistenceError> {
    match screen {
        crate::domain::Screen::Channel(media_id) => {
            remap_local_media_id(media_id, mappings).map_err(local_identity_remap_error)
        }
        _ => Ok(false),
    }
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
fn remap_local_media_file_urls(
    media: &mut MediaItem,
    mappings: &[LocalMoveMapping],
) -> Result<bool, PersistenceError> {
    let mut changed = remap_local_file_url(&mut media.webpage_url, mappings)?;
    if let Some(thumbnail_url) = &mut media.thumbnail_url {
        changed |= remap_local_file_url(thumbnail_url, mappings)?;
    }
    for caption in &mut media.captions {
        changed |= remap_local_file_url(&mut caption.url, mappings)?;
    }
    Ok(changed)
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
fn remap_local_file_url(
    url: &mut Url,
    mappings: &[LocalMoveMapping],
) -> Result<bool, PersistenceError> {
    if url.scheme() != "file" {
        return Ok(false);
    }
    let path = url.to_file_path().map_err(|()| {
        invalid_local_move_state(format!("stored Local file URL is invalid: `{url}`"))
    })?;
    let Some(remapped) = remap_local_path_prefix(&path, mappings) else {
        return Ok(false);
    };
    let query = url.query().map(str::to_owned);
    let fragment = url.fragment().map(str::to_owned);
    let mut remapped_url = Url::from_file_path(&remapped).map_err(|()| {
        invalid_local_move_state(format!(
            "remapped Local file path cannot be represented as a URL: `{}`",
            remapped.display()
        ))
    })?;
    remapped_url.set_query(query.as_deref());
    remapped_url.set_fragment(fragment.as_deref());
    *url = remapped_url;
    Ok(true)
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
fn local_identity_remap_error(error: LocalIdentityRemapError) -> PersistenceError {
    match error {
        LocalIdentityRemapError::NonUtf8Destination(path) => invalid_local_move_state(format!(
            "moved local path is not valid UTF-8: `{}`",
            path.display()
        )),
    }
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
#[cfg(feature = "sqlite-state")]
fn apply_local_move_state_remap(
    connection: &Connection,
    plan: &LocalMoveStatePlan,
) -> Result<(), PersistenceError> {
    for update in &plan.playback_progress {
        ensure_one_local_move_row(
            connection.execute(
                r"
                UPDATE playback_progress
                SET external_id = ?1
                WHERE source = 'local' AND external_id = ?2
                ",
                params![update.new_external_id, update.old_external_id],
            )?,
            "playback_progress",
        )?;
    }
    for update in &plan.playback_history {
        ensure_one_local_move_row(
            connection.execute(
                r"
                UPDATE playback_history
                SET external_id = ?1, replay_locator = ?2
                WHERE id = ?3 AND source = 'local'
                ",
                params![update.external_id, update.replay_locator, update.id],
            )?,
            "playback_history",
        )?;
    }
    for update in &plan.private_comments {
        ensure_one_local_move_row(
            connection.execute(
                "UPDATE private_comments SET target_json = ?1 WHERE id = ?2",
                params![update.json, update.id],
            )?,
            "private_comments",
        )?;
    }
    for update in &plan.bookmarks {
        ensure_one_local_move_row(
            connection.execute(
                r"
                UPDATE bookmarks
                SET external_id = ?1
                WHERE id = ?2 AND source = 'local'
                ",
                params![update.json, update.id],
            )?,
            "bookmarks",
        )?;
    }
    for update in &plan.sessions {
        ensure_one_local_move_row(
            connection.execute(
                "UPDATE session_state SET state_json = ?1 WHERE slot = ?2",
                params![update.state_json, update.slot],
            )?,
            "session_state",
        )?;
    }
    for update in &plan.metadata_cache {
        ensure_one_local_move_row(
            connection.execute(
                r"
                UPDATE metadata_cache
                SET external_id = ?1, media_json = ?2, source_url = ?3
                WHERE source = 'local' AND external_id = ?4
                ",
                params![
                    update.new_external_id,
                    update.media_json,
                    update.source_url,
                    update.old_external_id
                ],
            )?,
            "metadata_cache",
        )?;
    }
    for update in &plan.subscription_items_cache {
        ensure_one_local_move_row(
            connection.execute(
                r"
                UPDATE subscription_items_cache
                SET source_id = ?1, items_json = ?2
                WHERE source = 'local' AND source_id = ?3
                ",
                params![
                    update.new_source_id,
                    update.items_json,
                    update.old_source_id
                ],
            )?,
            "subscription_items_cache",
        )?;
    }
    apply_local_playlist_entry_updates(connection, &plan.playlist_entries)?;
    Ok(())
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
#[cfg(feature = "sqlite-state")]
fn apply_local_playlist_entry_updates(
    connection: &Connection,
    updates: &[LocalPlaylistEntryUpdate],
) -> Result<(), PersistenceError> {
    for update in updates {
        ensure_one_local_move_row(
            connection.execute(
                r"
                UPDATE playlist_entries
                SET external_id = ?1, snapshot_json = ?2, segment_json = ?3
                WHERE id = ?4 AND source = 'local'
                ",
                params![
                    update.external_id,
                    update.snapshot_json,
                    update.segment_json,
                    update.id
                ],
            )?,
            "playlist_entries",
        )?;
    }
    Ok(())
}

#[cfg(all(
    any(feature = "local-rename", feature = "local-move"),
    feature = "sqlite-state"
))]
fn ensure_one_local_move_row(changed: usize, table: &'static str) -> Result<(), PersistenceError> {
    if changed == 1 {
        Ok(())
    } else {
        Err(invalid_local_move_state(format!(
            "{table} changed {changed} rows while exactly one was expected"
        )))
    }
}

fn ensure_saved_search_json_bound(
    field: &'static str,
    actual_bytes: usize,
    maximum_bytes: usize,
) -> Result<(), PersistenceError> {
    if actual_bytes > maximum_bytes {
        return Err(PersistenceError::SavedSearchTooLarge {
            field,
            maximum_bytes,
        });
    }
    Ok(())
}

fn validate_saved_youtube_search(search: &SavedYouTubeSearch) -> Result<(), PersistenceError> {
    search
        .request
        .validate()
        .map_err(|error| PersistenceError::InvalidSavedSearch {
            reason: error.to_string(),
        })?;
    if search.results.len() > MAX_SAVED_YOUTUBE_SEARCH_RESULTS {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: format!(
                "result count {} exceeds the {MAX_SAVED_YOUTUBE_SEARCH_RESULTS}-item limit",
                search.results.len()
            ),
        });
    }
    if search.results.iter().any(|item| {
        !matches!(
            (search.request.target, item),
            (SearchTarget::Videos, SearchItem::Video(_))
                | (SearchTarget::Channels, SearchItem::Channel(_))
        )
    }) {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: "result kinds do not match the request target".to_owned(),
        });
    }
    if let Some(next_page) = search.next_page
        && (next_page <= search.request.page || next_page > 10_000)
    {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: "next page must follow the saved request page and remain at most 10000"
                .to_owned(),
        });
    }
    Ok(())
}

fn validate_saved_youtube_music_search(
    search: &SavedYouTubeMusicSearch,
) -> Result<(), PersistenceError> {
    if search.query.trim().is_empty()
        || search.query.trim() != search.query
        || search.query.len() > MAX_SAVED_MUSIC_QUERY_BYTES
        || search.query.chars().any(char::is_control)
    {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: format!(
                "YouTube Music query must be trimmed, printable, and at most \
                 {MAX_SAVED_MUSIC_QUERY_BYTES} bytes"
            ),
        });
    }
    if search.results.len() > MAX_SAVED_YOUTUBE_SEARCH_RESULTS {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: format!(
                "result count {} exceeds the {MAX_SAVED_YOUTUBE_SEARCH_RESULTS}-item limit",
                search.results.len()
            ),
        });
    }
    if search
        .results
        .iter()
        .any(|item| !matches!(item, SearchItem::Video(_)))
    {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: "YouTube Music snapshots may contain only playable videos".to_owned(),
        });
    }
    Ok(())
}

fn validate_saved_bandcamp_search(search: &SavedBandcampSearch) -> Result<(), PersistenceError> {
    if search.query.trim().is_empty()
        || search.query.trim() != search.query
        || search.query.len() > MAX_SAVED_BANDCAMP_QUERY_BYTES
        || search.query.chars().any(char::is_control)
    {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: format!(
                "Bandcamp query must be trimmed, printable, and at most \
                 {MAX_SAVED_BANDCAMP_QUERY_BYTES} bytes"
            ),
        });
    }
    if !(1..=MAX_SAVED_BANDCAMP_PAGE).contains(&search.page) {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: format!("Bandcamp page must be between 1 and {MAX_SAVED_BANDCAMP_PAGE}"),
        });
    }
    if let Some(next_page) = search.next_page
        && (search.page.checked_add(1) != Some(next_page) || next_page > MAX_SAVED_BANDCAMP_PAGE)
    {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: "Bandcamp next page must be the sequential page within the 100-page limit"
                .to_owned(),
        });
    }
    if search.results.len() > MAX_SAVED_BANDCAMP_SEARCH_RESULTS {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: format!(
                "Bandcamp result count {} exceeds the \
                 {MAX_SAVED_BANDCAMP_SEARCH_RESULTS}-item limit",
                search.results.len()
            ),
        });
    }

    let mut release_ids = HashSet::with_capacity(search.results.len());
    for release in &search.results {
        validate_saved_bandcamp_summary(release)?;
        if !release_ids.insert(release.id.external_id.as_str()) {
            return Err(PersistenceError::InvalidSavedSearch {
                reason: format!(
                    "Bandcamp result ID {} appears more than once",
                    release.id.external_id
                ),
            });
        }
    }
    Ok(())
}

fn validate_saved_bandcamp_summary(
    release: &BandcampSearchSummary,
) -> Result<(), PersistenceError> {
    let (url_kind, stable_id) =
        canonical_bandcamp_identity(&release.webpage_url).ok_or_else(|| {
            PersistenceError::InvalidSavedSearch {
                reason:
                    "Bandcamp pages must use canonical credential-free HTTPS track or album URLs"
                        .to_owned(),
            }
        })?;
    if release.id.source != SourceKind::Bandcamp
        || release.id.external_id != stable_id
        || release.kind != url_kind
    {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: "Bandcamp source, stable ID, kind, and canonical page must agree".to_owned(),
        });
    }
    validate_saved_bandcamp_text(
        "release title",
        &release.title,
        MAX_SAVED_BANDCAMP_TITLE_BYTES,
    )?;
    if let Some(artist) = release.artist.as_deref() {
        validate_saved_bandcamp_text("artist", artist, MAX_SAVED_BANDCAMP_ARTIST_BYTES)?;
    }
    if let Some(artwork) = release.artwork_url.as_ref()
        && !valid_saved_bandcamp_artwork_url(artwork)
    {
        return Err(PersistenceError::InvalidSavedSearch {
            reason:
                "Bandcamp artwork must use a bounded credential-free HTTPS bcbits.com /img/ URL"
                    .to_owned(),
        });
    }
    Ok(())
}

fn canonical_bandcamp_identity(url: &Url) -> Option<(BandcampReleaseKind, String)> {
    if url.as_str().len() > MAX_SAVED_BANDCAMP_CANONICAL_URL_BYTES
        || url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let artist = url
        .host_str()?
        .strip_suffix(".bandcamp.com")
        .filter(|value| !value.contains('.'))
        .filter(|value| *value != "www")
        .filter(|value| valid_bandcamp_slug(value, 63))?;
    let segments = url.path_segments()?.collect::<Vec<_>>();
    let [kind, release_slug] = segments.as_slice() else {
        return None;
    };
    let kind = match *kind {
        "track" => BandcampReleaseKind::Track,
        "album" => BandcampReleaseKind::Album,
        _ => return None,
    };
    if !valid_bandcamp_slug(release_slug, 200) {
        return None;
    }
    let expected_url = format!(
        "https://{artist}.bandcamp.com/{}/{release_slug}",
        match kind {
            BandcampReleaseKind::Track => "track",
            BandcampReleaseKind::Album => "album",
        }
    );
    if url.as_str() != expected_url {
        return None;
    }
    Some((
        kind,
        format!(
            "{artist}/{}/{release_slug}",
            match kind {
                BandcampReleaseKind::Track => "track",
                BandcampReleaseKind::Album => "album",
            }
        ),
    ))
}

fn valid_bandcamp_slug(value: &str, maximum_bytes: usize) -> bool {
    let bytes = value.as_bytes();
    (1..=maximum_bytes).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn validate_saved_bandcamp_text(
    field: &str,
    text: &str,
    maximum_bytes: usize,
) -> Result<(), PersistenceError> {
    if text.trim().is_empty()
        || text.trim() != text
        || text.len() > maximum_bytes
        || text.chars().any(char::is_control)
    {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: format!(
                "Bandcamp {field} must be trimmed, printable, and at most {maximum_bytes} bytes"
            ),
        });
    }
    Ok(())
}

fn valid_saved_bandcamp_artwork_url(url: &Url) -> bool {
    let valid_host = url.host_str().is_some_and(|host| {
        host == "bcbits.com"
            || host.strip_suffix(".bcbits.com").is_some_and(|prefix| {
                !prefix.is_empty()
                    && prefix
                        .split('.')
                        .all(|label| valid_bandcamp_slug(label, 63))
            })
    });
    url.as_str().len() <= MAX_SAVED_BANDCAMP_ARTWORK_URL_BYTES
        && url.scheme() == "https"
        && valid_host
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path().starts_with("/img/")
        && url.path().len() > "/img/".len()
}

fn validate_saved_apple_podcasts_search(
    search: &SavedApplePodcastsSearch,
) -> Result<(), PersistenceError> {
    if search.query.trim().is_empty()
        || search.query.trim() != search.query
        || search.query.len() > MAX_SAVED_APPLE_QUERY_BYTES
        || search.query.chars().any(char::is_control)
    {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: format!(
                "Apple Podcasts query must be trimmed, printable, and at most \
                 {MAX_SAVED_APPLE_QUERY_BYTES} bytes"
            ),
        });
    }
    if search.storefront.len() != 2
        || !search
            .storefront
            .bytes()
            .all(|byte| byte.is_ascii_lowercase())
    {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: "Apple Podcasts storefront must be a lowercase two-letter code".to_owned(),
        });
    }
    if search.results.len() > MAX_SAVED_APPLE_RESULTS {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: format!(
                "Apple Podcasts result count {} exceeds the \
                 {MAX_SAVED_APPLE_RESULTS}-item limit",
                search.results.len()
            ),
        });
    }

    let mut show_ids = HashSet::with_capacity(search.results.len());
    for show in &search.results {
        validate_saved_apple_show(show)?;
        if !show_ids.insert(show.id.external_id.as_str()) {
            return Err(PersistenceError::InvalidSavedSearch {
                reason: format!(
                    "Apple Podcasts result ID {} appears more than once",
                    show.id.external_id
                ),
            });
        }
    }
    Ok(())
}

fn validate_saved_apple_show(show: &PodcastShowSummary) -> Result<(), PersistenceError> {
    if show.id.source != SourceKind::ApplePodcasts
        || show.id.external_id.is_empty()
        || show.id.external_id.len() > 20
        || show.id.external_id.starts_with('0')
        || !show
            .id
            .external_id
            .bytes()
            .all(|byte| byte.is_ascii_digit())
    {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: "Apple Podcasts show IDs must be positive decimal provider IDs".to_owned(),
        });
    }
    validate_saved_apple_text("show title", &show.title)?;
    if let Some(author) = show.author.as_deref() {
        validate_saved_apple_text("show author", author)?;
    }
    if show.genres.len() > MAX_SAVED_APPLE_GENRES {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: format!("Apple Podcasts show has more than {MAX_SAVED_APPLE_GENRES} genres"),
        });
    }
    for genre in &show.genres {
        if genre.trim().is_empty()
            || genre.trim() != genre
            || genre.len() > MAX_SAVED_APPLE_GENRE_BYTES
            || genre.chars().any(char::is_control)
        {
            return Err(PersistenceError::InvalidSavedSearch {
                reason: format!(
                    "Apple Podcasts genres must be trimmed, printable, and at most \
                     {MAX_SAVED_APPLE_GENRE_BYTES} bytes"
                ),
            });
        }
    }
    validate_saved_apple_url("feed URL", show.feed_url.as_ref(), None)?;
    validate_saved_apple_url(
        "webpage URL",
        show.webpage_url.as_ref(),
        Some("podcasts.apple.com"),
    )?;
    validate_saved_apple_url("artwork URL", show.artwork_url.as_ref(), None)?;
    Ok(())
}

fn validate_saved_apple_text(field: &str, text: &str) -> Result<(), PersistenceError> {
    if text.trim().is_empty()
        || text.trim() != text
        || text.len() > MAX_SAVED_APPLE_TEXT_BYTES
        || text.chars().any(char::is_control)
    {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: format!(
                "Apple Podcasts {field} must be trimmed, printable, and at most \
                 {MAX_SAVED_APPLE_TEXT_BYTES} bytes"
            ),
        });
    }
    Ok(())
}

fn validate_saved_apple_url(
    field: &str,
    url: Option<&Url>,
    required_host: Option<&str>,
) -> Result<(), PersistenceError> {
    let Some(url) = url else {
        return Ok(());
    };
    let valid_scheme = required_host.map_or_else(
        || matches!(url.scheme(), "http" | "https"),
        |_| url.scheme() == "https",
    );
    let valid_host = url.host_str().is_some()
        && required_host.is_none_or(|required| url.host_str() == Some(required));
    if url.as_str().len() > MAX_SAVED_APPLE_URL_BYTES
        || !valid_scheme
        || !valid_host
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.fragment().is_some()
        || remote_url_has_non_public_host(url)
    {
        return Err(PersistenceError::InvalidSavedSearch {
            reason: format!(
                "Apple Podcasts {field} must be a bounded credential-free HTTP(S) URL without a non-default port, fragment, or literal local/private host and must use any required exact host"
            ),
        });
    }
    Ok(())
}

fn ensure_subscription_snapshot_json_bound(actual_bytes: usize) -> Result<(), PersistenceError> {
    if actual_bytes > MAX_SAVED_SUBSCRIPTION_ITEMS_BYTES {
        return Err(PersistenceError::SubscriptionSnapshotTooLarge {
            maximum_bytes: MAX_SAVED_SUBSCRIPTION_ITEMS_BYTES,
        });
    }
    Ok(())
}

/// Encodes the longest provider-ordered item prefix within the disk byte cap.
fn encode_bounded_subscription_items(items: &[SearchItem]) -> Result<String, PersistenceError> {
    let mut encoded_page = String::with_capacity(MAX_SAVED_SUBSCRIPTION_ITEMS_BYTES.min(4096));
    encoded_page.push('[');
    for item in items {
        let encoded_item = serde_json::to_string(item)?;
        let separator_bytes = usize::from(encoded_page.len() > 1);
        let candidate_bytes = encoded_page
            .len()
            .saturating_add(separator_bytes)
            .saturating_add(encoded_item.len())
            .saturating_add(1);
        if candidate_bytes > MAX_SAVED_SUBSCRIPTION_ITEMS_BYTES {
            break;
        }
        if separator_bytes != 0 {
            encoded_page.push(',');
        }
        encoded_page.push_str(&encoded_item);
    }
    encoded_page.push(']');
    Ok(encoded_page)
}

fn validate_subscription_source_identity(
    source: &SourceKind,
    source_id: &str,
) -> Result<(), PersistenceError> {
    let source_name = source.as_str();
    if source_name.is_empty()
        || source_name.len() > MAX_SAVED_SUBSCRIPTION_SOURCE_BYTES
        || source_name.trim() != source_name
        || source_name.chars().any(char::is_control)
    {
        return Err(PersistenceError::InvalidSubscriptionSnapshot {
            reason: format!(
                "source must be trimmed, printable, and at most \
                 {MAX_SAVED_SUBSCRIPTION_SOURCE_BYTES} bytes"
            ),
        });
    }
    if source_id.is_empty()
        || source_id.len() > MAX_SAVED_SUBSCRIPTION_SOURCE_ID_BYTES
        || source_id.trim() != source_id
        || source_id.chars().any(char::is_control)
    {
        return Err(PersistenceError::InvalidSubscriptionSnapshot {
            reason: format!(
                "source ID must be trimmed, printable, and at most \
                 {MAX_SAVED_SUBSCRIPTION_SOURCE_ID_BYTES} bytes"
            ),
        });
    }
    Ok(())
}

fn validate_cached_subscription_items(
    cached: &CachedSubscriptionItems,
) -> Result<(), PersistenceError> {
    validate_subscription_source_identity(&cached.source, &cached.source_id)?;
    if cached.fetched_at < 0 {
        return Err(PersistenceError::InvalidSubscriptionSnapshot {
            reason: "fetch time cannot be negative".to_owned(),
        });
    }
    if cached.items.len() > MAX_SAVED_SUBSCRIPTION_ITEMS {
        return Err(PersistenceError::InvalidSubscriptionSnapshot {
            reason: format!(
                "item count {} exceeds the {MAX_SAVED_SUBSCRIPTION_ITEMS}-item limit",
                cached.items.len()
            ),
        });
    }
    if cached.items.iter().any(|item| {
        !matches!(
            (cached.source.clone(), item),
            (_, SearchItem::Video(_)) | (SourceKind::Rss, SearchItem::PodcastEpisode(_))
        )
    }) {
        return Err(PersistenceError::InvalidSubscriptionSnapshot {
            reason: "subscription snapshots may contain only videos or RSS podcast episodes"
                .to_owned(),
        });
    }
    Ok(())
}

#[cfg(feature = "sqlite-state")]
fn run_migrations(connection: &Connection) -> Result<(), PersistenceError> {
    let current: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(PersistenceError::UnsupportedSchema {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }

    for version in (current + 1)..=SCHEMA_VERSION {
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> rusqlite::Result<()> {
            connection.execute_batch(MIGRATIONS[(version - 1) as usize])?;
            connection.pragma_update(None, "user_version", version)?;
            connection.execute(
                "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, unixepoch())",
                [version],
            )?;
            connection.execute_batch("COMMIT")?;
            Ok(())
        })();
        if let Err(error) = result {
            let _ = connection.execute_batch("ROLLBACK");
            return Err(error.into());
        }
    }
    Ok(())
}

#[cfg(feature = "sqlite-state")]
fn to_sql_u64(value: u64, field: &'static str) -> Result<i64, PersistenceError> {
    i64::try_from(value).map_err(|_| PersistenceError::IntegerOutOfRange { field })
}

#[cfg(feature = "sqlite-state")]
fn add_sqlite_listen_seconds(
    connection: &Connection,
    source: &SourceKind,
    seconds: i64,
) -> Result<(), PersistenceError> {
    let maximum_existing = i64::MAX - seconds;
    let changed = connection
        .prepare_cached(
            r"
			INSERT INTO listen_totals (source, total_seconds)
			VALUES (?1, ?2)
			ON CONFLICT(source) DO UPDATE SET
				total_seconds = listen_totals.total_seconds + excluded.total_seconds
			WHERE listen_totals.total_seconds <= ?3
			",
        )?
        .execute(params![source.as_str(), seconds, maximum_existing])?;
    if changed == 0 {
        return Err(PersistenceError::IntegerOutOfRange {
            field: "listen seconds",
        });
    }
    Ok(())
}

/// Enforces the same replay-locator byte bound as the schema migration.
fn bounded_history_replay_locator<'a>(
    source: &SourceKind,
    locator: Option<&'a str>,
) -> Result<Option<&'a str>, PersistenceError> {
    let Some(locator) = locator else {
        return Ok(None);
    };
    if locator.is_empty() {
        return Err(PersistenceError::InvalidHistoryReplayLocator {
            reason: "the value is empty".to_owned(),
        });
    }
    if locator.len() > MAX_HISTORY_REPLAY_LOCATOR_BYTES {
        return Err(PersistenceError::InvalidHistoryReplayLocator {
            reason: format!("the value exceeds the {MAX_HISTORY_REPLAY_LOCATOR_BYTES}-byte limit"),
        });
    }
    validate_history_replay_locator(source, locator)?;
    Ok(Some(locator))
}

/// Rejects credentials and query shapes that can carry transient signatures.
fn validate_history_replay_locator(
    source: &SourceKind,
    locator: &str,
) -> Result<(), PersistenceError> {
    if source == &SourceKind::Local {
        if persisted_local_locator_path(locator).is_some() {
            return Ok(());
        }
        return Err(PersistenceError::InvalidHistoryReplayLocator {
            reason: "a local replay path must be absolute".to_owned(),
        });
    }

    let invalid = |reason: &str| PersistenceError::InvalidHistoryReplayLocator {
        reason: reason.to_owned(),
    };
    let url = Url::parse(locator).map_err(|_| invalid("the remote URL is malformed"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid(
            "remote URLs must be credential-free HTTP(S) without fragments",
        ));
    }

    let query = url.query_pairs().collect::<Vec<_>>();
    match source {
        SourceKind::YouTube => {
            let host = url.host_str().unwrap_or_default();
            let canonical_host = matches!(
                host,
                "youtube.com" | "www.youtube.com" | "music.youtube.com"
            );
            let canonical_path = url.path() == "/watch";
            let video_ids = query
                .iter()
                .filter(|(key, _)| key == "v")
                .map(|(_, value)| value.as_ref())
                .collect::<Vec<_>>();
            let query_is_known = query
                .iter()
                .all(|(key, _)| matches!(key.as_ref(), "v" | "t"));
            let video_id_is_valid = matches!(video_ids.as_slice(), [video_id]
            if video_id.len() == 11
                && video_id.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
                }));
            if !canonical_host || !canonical_path || !query_is_known || !video_id_is_valid {
                return Err(invalid(
                    "YouTube replay URLs may contain only one valid `v` and an optional `t` parameter",
                ));
            }
        }
        SourceKind::ApplePodcasts => {
            let host = url.host_str().unwrap_or_default();
            let canonical_host = host == "podcasts.apple.com";
            let episode_ids = query
                .iter()
                .filter(|(key, _)| key == "i")
                .map(|(_, value)| value.as_ref())
                .collect::<Vec<_>>();
            let query_is_known = query.iter().all(|(key, _)| key == "i");
            let episode_id_is_valid = matches!(episode_ids.as_slice(), [episode_id]
                if !episode_id.is_empty()
                    && episode_id.len() <= 20
                    && !episode_id.starts_with('0')
                    && episode_id.bytes().all(|byte| byte.is_ascii_digit()));
            if !canonical_host || !query_is_known || !episode_id_is_valid {
                return Err(invalid(
                    "Apple Podcasts replay URLs may contain only one positive `i` parameter",
                ));
            }
        }
        SourceKind::LibriVox => {
            if !is_canonical_librivox_audio_url(&url) {
                return Err(invalid(
                    "LibriVox replay URLs must use a stable Archive.org MP3 download",
                ));
            }
        }
        _ if !query.is_empty() => {
            return Err(invalid(
                "query parameters are not trusted for this replay source",
            ));
        }
        _ => {}
    }
    Ok(())
}

/// Decodes a current file-URL or legacy absolute-path persisted Local locator.
fn persisted_local_locator_path(locator: &str) -> Option<PathBuf> {
    if let Ok(url) = Url::parse(locator)
        && url.scheme() == "file"
    {
        return url.to_file_path().ok().filter(|path| path.is_absolute());
    }
    let path = PathBuf::from(locator);
    path.is_absolute().then_some(path)
}

#[cfg(feature = "sqlite-state")]
fn from_sql_u64(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    from_sql_u64_value(row.get(index)?, index)
}

#[cfg(feature = "sqlite-state")]
fn from_sql_optional_u64(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    row.get::<_, Option<i64>>(index)?
        .map(|value| from_sql_u64_value(value, index))
        .transpose()
}

#[cfg(feature = "sqlite-state")]
fn from_sql_u64_value(value: i64, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Integer, Box::new(error))
    })
}

#[cfg(feature = "sqlite-state")]
fn playback_progress_from_row(row: &Row<'_>) -> rusqlite::Result<PlaybackProgress> {
    Ok(PlaybackProgress {
        media_id: MediaId::new(
            SourceKind::from(row.get::<_, String>(0)?.as_str()),
            row.get::<_, String>(1)?,
        ),
        position_seconds: from_sql_u64(row, 2)?,
        duration_seconds: from_sql_optional_u64(row, 3)?,
        played_override: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

#[cfg(feature = "sqlite-state")]
fn history_entry_from_row(row: &Row<'_>) -> rusqlite::Result<HistoryEntry> {
    Ok(HistoryEntry {
        id: row.get(0)?,
        media_id: MediaId::new(
            SourceKind::from(row.get::<_, String>(1)?.as_str()),
            row.get::<_, String>(2)?,
        ),
        title: row.get(3)?,
        replay_locator: row.get(9)?,
        started_at: row.get(4)?,
        last_played_at: row.get(5)?,
        position_seconds: from_sql_u64(row, 6)?,
        duration_seconds: from_sql_optional_u64(row, 7)?,
        finished: row.get(8)?,
    })
}

#[cfg(feature = "sqlite-state")]
fn private_comment_from_row(row: &Row<'_>) -> rusqlite::Result<PrivateComment> {
    let target_json: String = row.get(1)?;
    let target = serde_json::from_str(&target_json).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(1, Type::Text, Box::new(error))
    })?;
    Ok(PrivateComment {
        id: row.get(0)?,
        target,
        body: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

#[cfg(feature = "sqlite-state")]
fn bookmark_from_row(row: &Row<'_>) -> rusqlite::Result<Bookmark> {
    Ok(Bookmark {
        id: row.get(0)?,
        media_id: MediaId::new(
            SourceKind::from(row.get::<_, String>(1)?.as_str()),
            row.get::<_, String>(2)?,
        ),
        position_seconds: from_sql_u64(row, 3)?,
        label: row.get(4)?,
        created_at: row.get(5)?,
    })
}

#[cfg(unix)]
#[cfg(feature = "sqlite-state")]
fn set_private_file_permissions(path: &Path) -> std::io::Result<()> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
#[cfg(feature = "sqlite-state")]
fn set_private_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, feature = "yandex-music"))]
pub(crate) fn assert_yandex_music_reaction_backend_contract(store: &dyn StateBackend) {
    let liked = store
        .queue_yandex_music_reaction("account-1", "track-1", YandexMusicReaction::Liked, 10)
        .expect("queue liked state");
    assert_eq!(liked.generation, 1);
    assert!(
        store
            .acknowledge_yandex_music_reaction("account-1", "track-1", liked.generation)
            .expect("acknowledge liked state")
    );
    assert!(
        store
            .pending_yandex_music_reactions()
            .expect("acknowledged outbox")
            .is_empty()
    );

    let disliked = store
        .queue_yandex_music_reaction("account-1", "track-1", YandexMusicReaction::Disliked, 11)
        .expect("queue disliked state after acknowledgement");
    assert_eq!(
        disliked.generation, 2,
        "an acknowledged ledger entry must retain its monotonic revision"
    );
    assert_eq!(
        store
            .pending_yandex_music_reactions()
            .expect("coalesced outbox"),
        vec![disliked.clone()]
    );
    assert!(
        !store
            .acknowledge_yandex_music_reaction("account-1", "track-1", 1)
            .expect("stale acknowledgement")
    );
    assert_eq!(
        store
            .pending_yandex_music_reactions()
            .expect("newer state survives stale acknowledgement"),
        vec![disliked.clone()]
    );
    assert!(
        store
            .acknowledge_yandex_music_reaction("account-1", "track-1", 2)
            .expect("exact acknowledgement")
    );

    for (account_uid, track_id, reaction, updated_at) in [
        ("account-z", "track-2", YandexMusicReaction::Neutral, 20),
        ("account-a", "track-9", YandexMusicReaction::Liked, 21),
        ("account-a", "track-1", YandexMusicReaction::Disliked, 22),
    ] {
        store
            .queue_yandex_music_reaction(account_uid, track_id, reaction, updated_at)
            .expect("queue canonical-order fixture");
    }
    let ordered = store
        .pending_yandex_music_reactions()
        .expect("canonical reaction order");
    assert_eq!(
        ordered
            .iter()
            .map(|pending| (pending.account_uid.as_str(), pending.track_id.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("account-a", "track-1"),
            ("account-a", "track-9"),
            ("account-z", "track-2"),
        ]
    );

    for (account_uid, track_id, updated_at) in [
        ("", "track-3", 1),
        ("account-2", "track with spaces", 1),
        (
            &"a".repeat(MAX_YANDEX_MUSIC_ACCOUNT_UID_BYTES + 1),
            "track-3",
            1,
        ),
        (
            "account-2",
            &"t".repeat(MAX_YANDEX_MUSIC_TRACK_ID_BYTES + 1),
            1,
        ),
        ("account-2", "track-3", -1),
    ] {
        assert!(matches!(
            store.queue_yandex_music_reaction(
                account_uid,
                track_id,
                YandexMusicReaction::Liked,
                updated_at,
            ),
            Err(PersistenceError::InvalidYandexMusicReaction { .. })
        ));
    }
}

#[cfg(all(test, feature = "sqlite-state"))]
mod tests {
    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::SqliteStateStore as StateStore;
    use super::*;
    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    use crate::domain::CaptionTrack;
    use crate::domain::{
        MediaKind, MediaLicense, MediaStatistics, PanelFocus, Screen, SearchQuery,
    };
    use crate::providers::{
        ChannelSummary, PodcastEpisodeSummary, SearchSort, Thumbnail, VideoSummary,
    };

    fn id(value: &str) -> MediaId {
        MediaId::new(SourceKind::YouTube, value)
    }

    fn media(value: &str) -> MediaItem {
        MediaItem {
            id: id(value),
            kind: MediaKind::Video,
            title: "A title".to_owned(),
            creator: Some("A channel".to_owned()),
            description: Some("A description".to_owned()),
            webpage_url: Url::parse("https://www.youtube.com/watch?v=example")
                .expect("valid media URL"),
            thumbnail_url: None,
            duration_seconds: Some(100),
            published_at: Some(10),
            statistics: MediaStatistics {
                views: Some(50),
                likes: Some(5),
            },
            license: MediaLicense::CreativeCommons("CC BY 3.0".to_owned()),
            chapters: Vec::new(),
            captions: Vec::new(),
        }
    }

    #[cfg(feature = "yandex-music")]
    #[test]
    fn sqlite_yandex_music_reactions_match_the_backend_contract() {
        let store = StateStore::open_in_memory().expect("SQLite state");
        assert_yandex_music_reaction_backend_contract(&store);
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn local_media_id(path: &Path) -> MediaId {
        MediaId::new(
            SourceKind::Local,
            path.to_str().expect("UTF-8 fixture media path"),
        )
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn local_media(path: &Path, artwork: Option<&Path>, caption: Option<&Path>) -> MediaItem {
        MediaItem {
            id: local_media_id(path),
            kind: MediaKind::Audio,
            title: path
                .file_name()
                .expect("fixture basename")
                .to_string_lossy()
                .into_owned(),
            creator: Some("Fixture Artist".to_owned()),
            description: Some("Local fixture metadata".to_owned()),
            webpage_url: Url::from_file_path(path).expect("fixture file URL"),
            thumbnail_url: artwork
                .map(|path| Url::from_file_path(path).expect("fixture artwork URL")),
            duration_seconds: Some(180),
            published_at: None,
            statistics: MediaStatistics::default(),
            license: MediaLicense::Unknown,
            chapters: Vec::new(),
            captions: caption
                .map(|path| {
                    vec![CaptionTrack {
                        language: "en".to_owned(),
                        label: Some("Fixture captions".to_owned()),
                        url: Url::from_file_path(path).expect("fixture caption URL"),
                        auto_generated: false,
                    }]
                })
                .unwrap_or_default(),
        }
    }

    fn search_video(value: &str) -> SearchItem {
        SearchItem::Video(VideoSummary {
            video_id: value.to_owned(),
            title: format!("Fixture {value}"),
            channel_name: "Fixture channel".to_owned(),
            channel_id: "UCfixture".to_owned(),
            description: "Mock search description".to_owned(),
            duration_seconds: Some(90),
            view_count: Some(1_234),
            published_at: Some(100),
            published_text: None,
            live: false,
            orientation: VideoOrientation::Unknown,
            thumbnails: Vec::new(),
            webpage_url: None,
            stream_url: None,
        })
    }

    /// Builds one compact RSS episode from deterministic mock metadata.
    fn podcast_episode(feed_url: &str, episode_id: &str) -> SearchItem {
        SearchItem::PodcastEpisode(Box::new(PodcastEpisodeSummary {
            feed_url: Url::parse(feed_url).expect("valid fixture feed URL"),
            episode_id: episode_id.to_owned(),
            feed_title: "Fixture podcast".to_owned(),
            title: format!("Fixture episode {episode_id}"),
            authors: vec!["Fixture host".to_owned()],
            description: "Mock RSS episode description".to_owned(),
            language: Some("en".to_owned()),
            categories: vec!["Technology".to_owned()],
            duration_seconds: Some(180),
            published_at: Some(1_704_067_200),
            webpage_url: Some(
                Url::parse(&format!(
                    "https://podcasts.example.test/episodes/{episode_id}"
                ))
                .expect("valid fixture episode page"),
            ),
            feed_webpage_url: Some(
                Url::parse("https://podcasts.example.test/").expect("valid fixture feed webpage"),
            ),
            artwork_url: Some(
                Url::parse("https://podcasts.example.test/cover.jpg")
                    .expect("valid fixture artwork URL"),
            ),
            stream_url: None,
            stream_mime_type: Some("audio/mpeg".to_owned()),
            stream_byte_length: Some(1_024),
        }))
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn local_subscription_video(
        media_path: &Path,
        channel_path: &Path,
        thumbnail_path: &Path,
    ) -> SearchItem {
        let mut webpage_url = Url::from_file_path(media_path).expect("fixture Local webpage URL");
        webpage_url.set_query(Some("view=details"));
        webpage_url.set_fragment(Some("metadata"));
        SearchItem::Video(VideoSummary {
            video_id: media_path
                .to_str()
                .expect("UTF-8 fixture media path")
                .to_owned(),
            title: "Fixture Local track".to_owned(),
            channel_name: "Fixture Local folder".to_owned(),
            channel_id: channel_path
                .to_str()
                .expect("UTF-8 fixture channel path")
                .to_owned(),
            description: "Mock Local subscription item".to_owned(),
            duration_seconds: Some(90),
            view_count: None,
            published_at: Some(100),
            published_text: None,
            live: false,
            orientation: VideoOrientation::Unknown,
            thumbnails: vec![Thumbnail {
                url: Url::from_file_path(thumbnail_path).expect("fixture Local thumbnail URL"),
                quality: Some("cover".to_owned()),
                width: None,
                height: None,
            }],
            webpage_url: Some(webpage_url),
            stream_url: Some(Url::from_file_path(media_path).expect("fixture Local stream URL")),
        })
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn assert_local_subscription_video_paths(
        item: &SearchItem,
        media_path: &Path,
        channel_path: &Path,
        thumbnail_path: &Path,
    ) {
        let SearchItem::Video(video) = item else {
            panic!("expected one Local video");
        };
        assert_eq!(video.video_id, media_path.to_string_lossy());
        assert_eq!(video.channel_id, channel_path.to_string_lossy());
        assert_eq!(
            video.thumbnails[0].url,
            Url::from_file_path(thumbnail_path).expect("target subscription cover URL")
        );
        let mut webpage = Url::from_file_path(media_path).expect("target subscription webpage URL");
        webpage.set_query(Some("view=details"));
        webpage.set_fragment(Some("metadata"));
        assert_eq!(video.webpage_url.as_ref(), Some(&webpage));
        assert_eq!(
            video.stream_url,
            Some(Url::from_file_path(media_path).expect("target subscription stream URL"))
        );
    }

    fn bandcamp_release(kind: BandcampReleaseKind, release_slug: &str) -> BandcampSearchSummary {
        let kind_slug = match kind {
            BandcampReleaseKind::Track => "track",
            BandcampReleaseKind::Album => "album",
        };
        BandcampSearchSummary {
            id: MediaId::new(
                SourceKind::Bandcamp,
                format!("fixture-artist/{kind_slug}/{release_slug}"),
            ),
            kind,
            title: release_slug.replace('-', " "),
            artist: Some("Fixture Artist".to_owned()),
            webpage_url: Url::parse(&format!(
                "https://fixture-artist.bandcamp.com/{kind_slug}/{release_slug}"
            ))
            .expect("valid Bandcamp fixture URL"),
            artwork_url: Some(
                Url::parse("https://f4.bcbits.com/img/a1234567890_16.jpg")
                    .expect("valid Bandcamp fixture artwork"),
            ),
        }
    }

    fn apple_show(value: &str) -> PodcastShowSummary {
        PodcastShowSummary {
            id: MediaId::new(SourceKind::ApplePodcasts, value),
            title: format!("Fixture podcast {value}"),
            author: Some("Fixture Network".to_owned()),
            feed_url: Some(
                Url::parse(&format!("https://feeds.example.test/{value}.xml"))
                    .expect("valid fixture feed URL"),
            ),
            webpage_url: Some(
                Url::parse(&format!(
                    "https://podcasts.apple.com/us/podcast/fixture/id{value}"
                ))
                .expect("valid fixture page URL"),
            ),
            artwork_url: Some(
                Url::parse(&format!("https://images.example.test/{value}.jpg"))
                    .expect("valid fixture artwork URL"),
            ),
            episode_count: Some(42),
            genres: vec!["Technology".to_owned(), "Podcasts".to_owned()],
            explicit: Some(false),
        }
    }

    fn youtube_playlist_media(video_id: &str, title: &str) -> PlaylistMediaSnapshot {
        let webpage_url = Url::parse(&format!("https://www.youtube.com/watch?v={video_id}"))
            .expect("valid YouTube playlist fixture URL");
        PlaylistMediaSnapshot {
            id: MediaId::new(SourceKind::YouTube, video_id),
            kind: MediaKind::Video,
            title: title.to_owned(),
            creator: Some("Fixture channel".to_owned()),
            webpage_url: webpage_url.clone(),
            thumbnail_url: Some(
                Url::parse(&format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg"))
                    .expect("valid YouTube artwork fixture URL"),
            ),
            duration_seconds: Some(180),
            replay_locator: webpage_url.to_string(),
        }
    }

    fn apple_playlist_media(episode_id: &str) -> PlaylistMediaSnapshot {
        let webpage_url = Url::parse(&format!(
            "https://podcasts.apple.com/us/podcast/fixture/id123456789?i={episode_id}"
        ))
        .expect("valid Apple episode fixture URL");
        PlaylistMediaSnapshot {
            id: MediaId::new(SourceKind::ApplePodcasts, episode_id),
            kind: MediaKind::PodcastEpisode,
            title: format!("Fixture episode {episode_id}"),
            creator: Some("Fixture podcast".to_owned()),
            webpage_url: webpage_url.clone(),
            thumbnail_url: Some(
                Url::parse("https://is1-ssl.mzstatic.com/image/thumb/fixture/600x600.jpg")
                    .expect("valid Apple artwork fixture URL"),
            ),
            duration_seconds: Some(3_600),
            replay_locator: webpage_url.to_string(),
        }
    }

    fn bandcamp_playlist_media(slug: &str) -> PlaylistMediaSnapshot {
        let webpage_url = Url::parse(&format!("https://fixture-artist.bandcamp.com/track/{slug}"))
            .expect("valid Bandcamp track fixture URL");
        PlaylistMediaSnapshot {
            id: MediaId::new(SourceKind::Bandcamp, format!("fixture-artist/track/{slug}")),
            kind: MediaKind::Audio,
            title: slug.replace('-', " "),
            creator: Some("Fixture Artist".to_owned()),
            webpage_url: webpage_url.clone(),
            thumbnail_url: Some(
                Url::parse("https://f4.bcbits.com/img/a1234567890_16.jpg")
                    .expect("valid Bandcamp artwork fixture URL"),
            ),
            duration_seconds: Some(240),
            replay_locator: webpage_url.to_string(),
        }
    }

    fn librivox_playlist_media() -> PlaylistMediaSnapshot {
        PlaylistMediaSnapshot {
            id: MediaId::new(
                SourceKind::LibriVox,
                librivox_section_external_id(5_936, 77_736),
            ),
            kind: MediaKind::Audio,
            title: "Introduction and Chapter I".to_owned(),
            creator: Some("Alexander Aaronsohn".to_owned()),
            webpage_url: Url::parse(
                "https://librivox.org/with-the-turks-in-palestine-by-alexander-aaronsohn/",
            )
            .expect("valid LibriVox book URL"),
            thumbnail_url: Some(
                Url::parse("https://archive.org/download/fixture/cover.jpg")
                    .expect("valid LibriVox artwork URL"),
            ),
            duration_seconds: Some(667),
            replay_locator: "https://archive.org/download/fixture/chapter_01.mp3".to_owned(),
        }
    }

    fn radio_playlist_media(station_id: &str) -> PlaylistMediaSnapshot {
        let webpage_url =
            Url::parse("http://radio.example/").expect("valid Radio fixture homepage");
        PlaylistMediaSnapshot {
            id: MediaId::new(SourceKind::Radio, station_id),
            kind: MediaKind::LiveStream,
            title: "Fixture Radio".to_owned(),
            creator: None,
            webpage_url: webpage_url.clone(),
            thumbnail_url: None,
            duration_seconds: None,
            replay_locator: webpage_url.to_string(),
        }
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn local_playlist_media(path: &Path, artwork: Option<&Path>) -> PlaylistMediaSnapshot {
        PlaylistMediaSnapshot {
            id: local_media_id(path),
            kind: MediaKind::Audio,
            title: path
                .file_name()
                .expect("fixture filename")
                .to_string_lossy()
                .into_owned(),
            creator: Some("Fixture Artist".to_owned()),
            webpage_url: Url::from_file_path(path).expect("fixture media file URL"),
            thumbnail_url: artwork
                .map(|artwork| Url::from_file_path(artwork).expect("fixture artwork file URL")),
            duration_seconds: Some(180),
            replay_locator: path.to_str().expect("UTF-8 fixture media path").to_owned(),
        }
    }

    fn disk_store() -> (tempfile::TempDir, Config, StateStore) {
        let directory = tempdir().expect("temporary directory");
        let config = Config::for_dir(directory.path().join("youta"));
        let store = StateStore::open(&config).expect("open state store");
        (directory, config, store)
    }

    #[test]
    fn disk_store_uses_wal_current_schema_and_private_file() {
        let (_directory, config, store) = disk_store();
        assert_eq!(
            store.journal_mode().expect("journal mode").to_lowercase(),
            "wal"
        );
        assert_eq!(
            store.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );

        #[cfg(unix)]
        {
            use std::fs;
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(config.database_file())
                .expect("database metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn migration_from_v4_preserves_session_and_adds_channel_summary_cache() {
        let connection = Connection::open_in_memory().expect("open SQLite");
        for version in 1..=4_u32 {
            connection
                .execute_batch(MIGRATIONS[(version - 1) as usize])
                .expect("apply historical migration");
            connection
                .pragma_update(None, "user_version", version)
                .expect("set historical version");
            connection
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                    params![version, i64::from(version)],
                )
                .expect("record historical migration");
        }
        let session = SessionState {
            screen: Screen::History,
            search_text: "preserved".to_owned(),
            ..SessionState::default()
        };
        connection
            .execute(
                r"
				INSERT INTO session_state (slot, state_json, updated_at)
				VALUES ('active', ?1, 3)
				",
                [serde_json::to_string(&session).expect("encode session")],
            )
            .expect("seed version-four session");

        run_migrations(&connection).expect("migrate to current schema");
        let store = StateStore { connection };
        assert_eq!(
            store.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );
        assert_eq!(store.session().expect("preserved session"), Some(session));
        assert_eq!(
            store
                .cached_channel_summary("UCmissing")
                .expect("new channel cache table"),
            None
        );
    }

    #[test]
    fn migration_from_v5_preserves_state_and_adds_youtube_music_search() {
        let connection = Connection::open_in_memory().expect("open SQLite");
        for version in 1..=5_u32 {
            connection
                .execute_batch(MIGRATIONS[(version - 1) as usize])
                .expect("apply historical migration");
            connection
                .pragma_update(None, "user_version", version)
                .expect("set historical version");
            connection
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                    params![version, i64::from(version)],
                )
                .expect("record historical migration");
        }
        let session = SessionState {
            screen: Screen::History,
            search_text: "preserved after v5".to_owned(),
            ..SessionState::default()
        };
        connection
            .execute(
                r"
				INSERT INTO session_state (slot, state_json, updated_at)
				VALUES ('active', ?1, 5)
				",
                [serde_json::to_string(&session).expect("encode session")],
            )
            .expect("seed version-five session");

        run_migrations(&connection).expect("migrate to current schema");
        let store = StateStore { connection };
        assert_eq!(
            store.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );
        assert_eq!(store.session().expect("preserved session"), Some(session));
        assert_eq!(
            store
                .youtube_music_search()
                .expect("new YouTube Music search table"),
            None
        );
    }

    #[test]
    fn migration_from_v6_preserves_state_and_adds_subscription_items_cache() {
        let connection = Connection::open_in_memory().expect("open SQLite");
        for version in 1..=6_u32 {
            connection
                .execute_batch(MIGRATIONS[(version - 1) as usize])
                .expect("apply historical migration");
            connection
                .pragma_update(None, "user_version", version)
                .expect("set historical version");
            connection
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                    params![version, i64::from(version)],
                )
                .expect("record historical migration");
        }
        let session = SessionState {
            screen: Screen::Subscriptions,
            search_text: "preserved after v6".to_owned(),
            ..SessionState::default()
        };
        connection
            .execute(
                r"
				INSERT INTO session_state (slot, state_json, updated_at)
				VALUES ('active', ?1, 6)
				",
                [serde_json::to_string(&session).expect("encode session")],
            )
            .expect("seed version-six session");

        run_migrations(&connection).expect("migrate to current schema");
        let store = StateStore { connection };
        assert_eq!(
            store.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );
        assert_eq!(store.session().expect("preserved session"), Some(session));
        assert_eq!(
            store
                .cached_subscription_items(&SourceKind::YouTube, "UCmissing")
                .expect("new subscription cache table"),
            None
        );
    }

    #[test]
    fn migration_from_v7_preserves_history_without_inventing_replay_locators() {
        let connection = Connection::open_in_memory().expect("open SQLite");
        for version in 1..=7_u32 {
            connection
                .execute_batch(MIGRATIONS[(version - 1) as usize])
                .expect("apply historical migration");
            connection
                .pragma_update(None, "user_version", version)
                .expect("set historical version");
            connection
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                    params![version, i64::from(version)],
                )
                .expect("record historical migration");
        }
        connection
            .execute(
                r"
				INSERT INTO playback_history (
					source, external_id, title, started_at, last_played_at,
					position_seconds, duration_seconds, finished
				) VALUES ('youtube', 'dQw4w9WgXcQ', 'Before v8', 1, 2, 10, 100, 0)
				",
                [],
            )
            .expect("seed version-seven history");

        run_migrations(&connection).expect("migrate to current schema");
        let store = StateStore { connection };
        let entries = store.history(false, 10).expect("migrated history");

        assert_eq!(
            store.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "Before v8");
        assert_eq!(entries[0].replay_locator, None);
    }

    #[test]
    fn migration_from_v8_adds_empty_apple_search_and_defaults_session_fields() {
        let connection = Connection::open_in_memory().expect("open SQLite");
        for version in 1..=8_u32 {
            connection
                .execute_batch(MIGRATIONS[(version - 1) as usize])
                .expect("apply historical migration");
            connection
                .pragma_update(None, "user_version", version)
                .expect("set historical version");
            connection
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                    params![version, i64::from(version)],
                )
                .expect("record historical migration");
        }
        let mut session =
            serde_json::to_value(SessionState::default()).expect("encode session fixture");
        let session_object = session
            .as_object_mut()
            .expect("session fixture should be an object");
        session_object.remove("apple_podcasts_selected_row");
        session_object.remove("apple_podcasts_search_text");
        connection
            .execute(
                r"
				INSERT INTO session_state (slot, state_json, updated_at)
				VALUES ('active', ?1, 8)
				",
                [serde_json::to_string(&session).expect("encode pre-Apple session")],
            )
            .expect("seed version-eight session");

        run_migrations(&connection).expect("migrate to current schema");
        let store = StateStore { connection };
        let restored = store
            .session()
            .expect("load migrated session")
            .expect("preserved session");

        assert_eq!(
            store.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );
        assert_eq!(restored.apple_podcasts_selected_row, None);
        assert!(restored.apple_podcasts_search_text.is_empty());
        assert_eq!(
            store
                .apple_podcasts_search()
                .expect("new Apple search table"),
            None
        );
    }

    #[test]
    fn migration_from_v9_preserves_apple_and_defaults_bandcamp_state() {
        let connection = Connection::open_in_memory().expect("open SQLite");
        for version in 1..=9_u32 {
            connection
                .execute_batch(MIGRATIONS[(version - 1) as usize])
                .expect("apply historical migration");
            connection
                .pragma_update(None, "user_version", version)
                .expect("set historical version");
            connection
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                    params![version, i64::from(version)],
                )
                .expect("record historical migration");
        }
        let mut session =
            serde_json::to_value(SessionState::default()).expect("encode session fixture");
        let session_object = session
            .as_object_mut()
            .expect("session fixture should be an object");
        session_object.remove("bandcamp_selected_row");
        session_object.remove("bandcamp_search_text");
        connection
            .execute(
                r"
				INSERT INTO session_state (slot, state_json, updated_at)
				VALUES ('active', ?1, 9)
				",
                [serde_json::to_string(&session).expect("encode pre-Bandcamp session")],
            )
            .expect("seed version-nine session");
        let apple_results = vec![apple_show("123456789")];
        connection
            .execute(
                r"
				INSERT INTO apple_podcasts_search_state (
					slot, query, storefront, results_json, updated_at
				) VALUES (1, 'science', 'us', ?1, 9)
				",
                [serde_json::to_string(&apple_results).expect("encode Apple fixture")],
            )
            .expect("seed version-nine Apple snapshot");

        run_migrations(&connection).expect("migrate to current schema");
        let store = StateStore { connection };
        let restored = store
            .session()
            .expect("load migrated session")
            .expect("preserved session");

        assert_eq!(
            store.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );
        assert_eq!(restored.bandcamp_selected_row, None);
        assert!(restored.bandcamp_search_text.is_empty());
        assert_eq!(
            store
                .apple_podcasts_search()
                .expect("preserved Apple snapshot"),
            Some(SavedApplePodcastsSearch {
                query: "science".to_owned(),
                storefront: "us".to_owned(),
                results: apple_results,
            })
        );
        assert_eq!(store.bandcamp_search().expect("new Bandcamp table"), None);
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn migration_from_v10_adds_durable_local_move_journal() {
        let connection = Connection::open_in_memory().expect("open SQLite");
        for version in 1..=10_u32 {
            connection
                .execute_batch(MIGRATIONS[(version - 1) as usize])
                .expect("apply historical migration");
            connection
                .pragma_update(None, "user_version", version)
                .expect("set historical version");
            connection
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                    params![version, i64::from(version)],
                )
                .expect("record historical migration");
        }

        run_migrations(&connection).expect("migrate to current schema");
        let store = StateStore { connection };
        let mapping = LocalMoveMapping {
            source: PathBuf::from("/music/source.flac"),
            target: PathBuf::from("/archive/source.flac"),
        };
        store
            .journal_local_move_intent(std::slice::from_ref(&mapping), 11)
            .expect("write new move journal");

        assert_eq!(
            store.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );
        assert_eq!(
            store.local_move_intents().expect("move journal"),
            vec![mapping]
        );
    }

    #[test]
    fn migration_from_v11_preserves_state_and_adds_empty_playlist_tables() {
        let connection = Connection::open_in_memory().expect("open SQLite");
        for version in 1..=11_u32 {
            connection
                .execute_batch(MIGRATIONS[(version - 1) as usize])
                .expect("apply historical migration");
            connection
                .pragma_update(None, "user_version", version)
                .expect("set historical version");
            connection
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                    params![version, i64::from(version)],
                )
                .expect("record historical migration");
        }
        connection
            .execute(
                r"
                INSERT INTO playback_progress (
                    source, external_id, position_seconds, duration_seconds,
                    played_override, updated_at
                ) VALUES ('youtube', 'dQw4w9WgXcQ', 42, 180, NULL, 11)
                ",
                [],
            )
            .expect("seed version-eleven progress");

        run_migrations(&connection).expect("migrate to current schema");
        let store = StateStore { connection };

        assert_eq!(
            store.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );
        assert_eq!(
            store
                .progress(&id("dQw4w9WgXcQ"))
                .expect("preserved progress")
                .expect("version-eleven progress")
                .position_seconds,
            42
        );
        assert!(store.playlists().expect("new playlists table").is_empty());
        assert!(
            store
                .playlist(TODO_PLAYLIST_ID)
                .expect("new playlist entries table")
                .is_none()
        );
    }

    #[cfg(feature = "yandex-music")]
    #[test]
    fn migration_from_v12_preserves_state_and_adds_empty_yandex_reaction_outbox() {
        let connection = Connection::open_in_memory().expect("open SQLite");
        for version in 1..=12_u32 {
            connection
                .execute_batch(MIGRATIONS[(version - 1) as usize])
                .expect("apply historical migration");
            connection
                .pragma_update(None, "user_version", version)
                .expect("set historical version");
            connection
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                    params![version, i64::from(version)],
                )
                .expect("record historical migration");
        }
        connection
            .execute(
                r"
				INSERT INTO playback_progress (
					source, external_id, position_seconds, duration_seconds,
					played_override, updated_at
				) VALUES ('youtube', 'dQw4w9WgXcQ', 84, 180, NULL, 12)
				",
                [],
            )
            .expect("seed version-twelve progress");

        run_migrations(&connection).expect("migrate to current schema");
        let store = StateStore { connection };

        assert_eq!(
            store.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );
        assert_eq!(
            store
                .progress(&id("dQw4w9WgXcQ"))
                .expect("preserved progress")
                .expect("version-twelve progress")
                .position_seconds,
            84
        );
        assert!(
            store
                .pending_yandex_music_reactions()
                .expect("new reaction outbox")
                .is_empty()
        );
    }

    #[cfg(feature = "yandex-music")]
    #[test]
    fn migration_from_v13_preserves_pending_reaction_as_unacknowledged_ledger_entry() {
        let connection = Connection::open_in_memory().expect("open SQLite");
        for version in 1..=13_u32 {
            connection
                .execute_batch(MIGRATIONS[(version - 1) as usize])
                .expect("apply historical migration");
            connection
                .pragma_update(None, "user_version", version)
                .expect("set historical version");
            connection
                .execute(
                    "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                    params![version, i64::from(version)],
                )
                .expect("record historical migration");
        }
        connection
            .execute(
                r"
				INSERT INTO yandex_music_reaction_outbox (
					account_uid, track_id, reaction, generation, updated_at
				) VALUES ('account-1', 'track-1', 'liked', 7, 13)
				",
                [],
            )
            .expect("seed version-thirteen reaction");

        run_migrations(&connection).expect("migrate to current schema");
        let store = StateStore { connection };

        assert_eq!(
            store
                .pending_yandex_music_reactions()
                .expect("migrated reaction ledger"),
            vec![PendingYandexMusicReaction {
                account_uid: "account-1".to_owned(),
                track_id: "track-1".to_owned(),
                reaction: YandexMusicReaction::Liked,
                generation: 7,
                updated_at: 13,
            }]
        );
        assert!(
            store
                .acknowledge_yandex_music_reaction("account-1", "track-1", 7)
                .expect("acknowledge migrated reaction")
        );
        assert_eq!(
            store
                .queue_yandex_music_reaction(
                    "account-1",
                    "track-1",
                    YandexMusicReaction::Disliked,
                    14,
                )
                .expect("queue post-migration reaction")
                .generation,
            8
        );
    }

    #[test]
    fn todo_playlist_is_lazy_idempotent_and_survives_restart() {
        let directory = tempdir().expect("temporary directory");
        let config = Config::for_dir(directory.path().join("youta"));
        let original = youtube_playlist_media("dQw4w9WgXcQ", "First captured title");

        {
            let store = StateStore::open(&config).expect("open state store");
            assert!(
                store
                    .playlist(TODO_PLAYLIST_ID)
                    .expect("read absent todo")
                    .is_none()
            );
            assert_eq!(
                store.add_to_todo(&original, 10).expect("add to todo"),
                PlaylistAddOutcome::Added
            );
            let mut youtube_music_view = original.clone();
            youtube_music_view.kind = MediaKind::Audio;
            youtube_music_view.title = "Later YouTube Music title".to_owned();
            assert_eq!(
                store
                    .add_to_todo(&youtube_music_view, 20)
                    .expect("repeat todo add"),
                PlaylistAddOutcome::AlreadyPresent
            );
        }

        let reopened = StateStore::open(&config).expect("reopen state store");
        let playlist = reopened
            .playlist(TODO_PLAYLIST_ID)
            .expect("read todo")
            .expect("todo playlist");
        assert_eq!(playlist.id, TODO_PLAYLIST_ID);
        assert_eq!(playlist.name, TODO_PLAYLIST_NAME);
        assert_eq!(playlist.description, None);
        assert_eq!(playlist.entries.len(), 1);
        assert_eq!(playlist.entries[0].media, original);
        assert_eq!(playlist.entries[0].added_at, 10);
        assert!(
            reopened
                .todo_contains(&id("dQw4w9WgXcQ"))
                .expect("todo membership")
        );
    }

    #[test]
    fn radio_favorites_are_hidden_and_toggle_without_a_schema_change() {
        let store = StateStore::open_in_memory().expect("open store");
        let station = radio_playlist_media("fixture-radio");

        assert_eq!(
            store
                .toggle_radio_favorite(&station, 1)
                .expect("favorite station"),
            PlaylistToggleOutcome::Added
        );
        assert!(
            store
                .radio_favorite_contains(&station.id)
                .expect("favorite membership")
        );
        assert!(store.playlists().expect("visible playlists").is_empty());
        assert!(
            store
                .playlist_memberships(&station.id)
                .expect("visible membership IDs")
                .is_empty()
        );
        assert!(
            store
                .playlist_memberships_with_names(&station.id)
                .expect("visible membership names")
                .is_empty()
        );
        assert_eq!(
            store
                .playlist(RADIO_FAVORITES_PLAYLIST_ID)
                .expect("hidden playlist")
                .expect("created hidden playlist")
                .entries
                .len(),
            1
        );
        assert_eq!(
            store
                .toggle_radio_favorite(&station, 2)
                .expect("unfavorite station"),
            PlaylistToggleOutcome::Removed
        );
        assert!(
            !store
                .radio_favorite_contains(&station.id)
                .expect("removed favorite")
        );
    }

    #[test]
    fn playlist_crud_preserves_case_insensitive_names_order_and_memberships() {
        let store = StateStore::open_in_memory().expect("open store");
        let created = store
            .create_playlist("Планы", Some("Things to hear"), 1)
            .expect("create playlist");
        let PlaylistCreateOutcome::Created(summary) = created else {
            panic!("expected a new playlist");
        };
        assert_eq!(summary.description.as_deref(), Some("Things to hear"));
        assert_eq!(
            store
                .create_playlist("пЛаНы", Some("Must not replace"), 2)
                .expect("repeat playlist"),
            PlaylistCreateOutcome::Existing(summary.clone())
        );

        let first = youtube_playlist_media("abcdefghijk", "First");
        let second = apple_playlist_media("9876543210");
        let third = bandcamp_playlist_media("third-track");
        for (timestamp, media) in [(10, &first), (11, &second), (12, &third)] {
            assert_eq!(
                store
                    .add_playlist_entry(&summary.id, media, timestamp)
                    .expect("append playlist item"),
                PlaylistAddOutcome::Added
            );
        }
        assert_eq!(
            store
                .add_playlist_entry(&summary.id, &first, 99)
                .expect("repeat playlist item"),
            PlaylistAddOutcome::AlreadyPresent
        );
        assert_eq!(
            store
                .playlist_memberships(&second.id)
                .expect("playlist memberships"),
            vec![summary.id.clone()]
        );
        assert!(
            store
                .remove_playlist_entry(&summary.id, &second.id, 20)
                .expect("remove playlist item")
        );
        assert!(
            !store
                .remove_playlist_entry(&summary.id, &second.id, 21)
                .expect("repeat remove")
        );
        assert_eq!(
            store
                .toggle_playlist_entry(&summary.id, &second, 22)
                .expect("toggle item on"),
            PlaylistToggleOutcome::Added
        );
        assert_eq!(
            store
                .playlist(&summary.id)
                .expect("read playlist")
                .expect("playlist")
                .entries
                .iter()
                .map(|entry| entry.media.title.as_str())
                .collect::<Vec<_>>(),
            vec!["First", "third track", "Fixture episode 9876543210"]
        );

        let updated = store
            .update_playlist(&summary.id, "Listening queue", None, 30)
            .expect("update playlist")
            .expect("updated playlist");
        assert_eq!(updated.name, "Listening queue");
        assert_eq!(updated.description, None);
        assert!(matches!(
            store
                .update_playlist(&summary.id, "todo", None, 31)
                .expect_err("reserved todo name"),
            PersistenceError::InvalidPlaylist { .. }
        ));
        assert_eq!(
            store
                .toggle_playlist_entry(&summary.id, &second, 32)
                .expect("toggle item off"),
            PlaylistToggleOutcome::Removed
        );
        assert!(
            store
                .playlist_memberships(&second.id)
                .expect("removed memberships")
                .is_empty()
        );
    }

    #[test]
    fn playlist_memberships_with_names_returns_current_names_and_fixed_todo_identity() {
        let store = StateStore::open_in_memory().expect("open store");
        let target = youtube_playlist_media("dQw4w9WgXcQ", "Membership target");
        store.add_to_todo(&target, 1).expect("add target to todo");
        store
            .update_playlist(TODO_PLAYLIST_ID, "Listen later", None, 2)
            .expect("rename todo")
            .expect("todo playlist");
        let PlaylistCreateOutcome::Created(zulu) = store
            .create_playlist("Zulu", None, 3)
            .expect("create Zulu playlist")
        else {
            panic!("expected new Zulu playlist");
        };
        let PlaylistCreateOutcome::Created(alpha) = store
            .create_playlist("Alpha", None, 4)
            .expect("create Alpha playlist")
        else {
            panic!("expected new Alpha playlist");
        };
        let PlaylistCreateOutcome::Created(segment_only) = store
            .create_playlist("Segment only", None, 5)
            .expect("create segment-only playlist")
        else {
            panic!("expected new segment-only playlist");
        };
        store
            .add_playlist_entry(&zulu.id, &target, 6)
            .expect("add target to Zulu");
        store
            .add_playlist_entry(&alpha.id, &target, 7)
            .expect("add target to Alpha");
        let segment_json = serde_json::to_string(
            &crate::domain::Segment::new(1, target.id.clone(), 10, 20, None, 8)
                .expect("valid segment fixture"),
        )
        .expect("encode segment fixture");
        store
            .connection
            .execute(
                r"
				INSERT INTO playlist_entries (
					playlist_id, position, source, external_id, snapshot_json,
					segment_json, added_at
					) VALUES (?1, 0, ?2, ?3, ?4, ?5, 8)
				",
                params![
                    segment_only.id,
                    target.id.source.as_str(),
                    target.id.external_id,
                    encoded_playlist_snapshot(&target).expect("encode target snapshot"),
                    segment_json
                ],
            )
            .expect("insert segment-only fixture");

        let memberships = store
            .playlist_memberships_with_names(&target.id)
            .expect("membership names");

        assert_eq!(
            memberships,
            vec![
                PlaylistMembership {
                    playlist_id: alpha.id,
                    playlist_name: "Alpha".to_owned(),
                },
                PlaylistMembership {
                    playlist_id: TODO_PLAYLIST_ID.to_owned(),
                    playlist_name: "Listen later".to_owned(),
                },
                PlaylistMembership {
                    playlist_id: zulu.id,
                    playlist_name: "Zulu".to_owned(),
                },
            ]
        );
        assert!(
            memberships
                .iter()
                .any(|membership| membership.playlist_id == TODO_PLAYLIST_ID),
            "Todo is identified by its stable ID after a rename"
        );
        assert!(
            memberships
                .iter()
                .all(|membership| membership.playlist_id != segment_only.id),
            "a saved segment alone is not complete-item membership"
        );
    }

    #[test]
    fn playlist_membership_name_query_uses_media_index_with_large_unrelated_library() {
        const UNRELATED_PLAYLISTS: usize = 768;

        let store = StateStore::open_in_memory().expect("open store");
        let target = youtube_playlist_media("dQw4w9WgXcQ", "Indexed target");
        let PlaylistCreateOutcome::Created(target_playlist) = store
            .create_playlist_with_entry("Target", None, &target, 1)
            .expect("create target playlist")
        else {
            panic!("expected target playlist");
        };
        let transaction = store
            .connection
            .unchecked_transaction()
            .expect("start fixture transaction");
        {
            let mut playlist_statement = transaction
                .prepare_cached(
                    r"
					INSERT INTO playlists (
						id, name, name_key, description, created_at, updated_at
					) VALUES (?1, ?2, ?3, NULL, 2, 2)
					",
                )
                .expect("prepare playlist fixture insert");
            let mut entry_statement = transaction
                .prepare_cached(
                    r"
					INSERT INTO playlist_entries (
						playlist_id, position, source, external_id, snapshot_json,
						segment_json, added_at
					) VALUES (?1, 0, ?2, ?3, ?4, NULL, 2)
					",
                )
                .expect("prepare entry fixture insert");
            for index in 0..UNRELATED_PLAYLISTS {
                let playlist_id = format!("bulk:{index:04}");
                let playlist_name = format!("Bulk {index:04}");
                let playlist_name_key = format!("bulk {index:04}");
                let unrelated =
                    youtube_playlist_media(&format!("bulk{index:07}"), "Unrelated fixture");
                let snapshot =
                    encoded_playlist_snapshot(&unrelated).expect("encode unrelated snapshot");
                playlist_statement
                    .execute(params![playlist_id, playlist_name, playlist_name_key])
                    .expect("insert unrelated playlist");
                entry_statement
                    .execute(params![
                        playlist_id,
                        unrelated.id.source.as_str(),
                        unrelated.id.external_id,
                        snapshot
                    ])
                    .expect("insert unrelated playlist entry");
            }
        }
        transaction.commit().expect("commit large fixture");

        assert_eq!(
            store
                .playlist_memberships_with_names(&target.id)
                .expect("indexed membership lookup"),
            vec![PlaylistMembership {
                playlist_id: target_playlist.id,
                playlist_name: target_playlist.name,
            }]
        );

        let explain_sql = format!("EXPLAIN QUERY PLAN {PLAYLIST_MEMBERSHIPS_WITH_NAMES_SQL}");
        let limit =
            i64::try_from(MAX_PLAYLISTS.saturating_add(1)).expect("membership limit fits SQLite");
        let mut statement = store
            .connection
            .prepare(&explain_sql)
            .expect("prepare membership query plan");
        let plan = statement
            .query_map(
                params![target.id.source.as_str(), target.id.external_id, limit],
                |row| row.get::<_, String>(3),
            )
            .expect("read membership query plan")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect membership query plan");
        assert!(
            plan.first().is_some_and(|step| {
                step.contains("SEARCH e") && step.contains("playlist_entries_media")
            }),
            "membership lookup must start from an indexed media search: {plan:?}"
        );
        assert!(
            plan.iter().all(|step| !step.contains("SCAN e")),
            "membership lookup must not scan playlist entries: {plan:?}"
        );
        let normalized_sql = PLAYLIST_MEMBERSHIPS_WITH_NAMES_SQL.to_ascii_uppercase();
        assert!(!normalized_sql.contains("COUNT("));
        assert!(!normalized_sql.contains("GROUP BY"));
    }

    #[test]
    fn renamed_todo_keeps_fixed_quick_action_identity() {
        let store = StateStore::open_in_memory().expect("open store");
        let first = youtube_playlist_media("abcdefghijk", "First");
        let second = youtube_playlist_media("ZYXWVUTSRQP", "Second");
        store.add_to_todo(&first, 1).expect("create todo");
        let renamed = store
            .update_playlist(TODO_PLAYLIST_ID, "Listen later", Some("Private queue"), 2)
            .expect("rename todo")
            .expect("todo playlist");
        assert_eq!(renamed.id, TODO_PLAYLIST_ID);
        assert_eq!(renamed.name, "Listen later");
        store
            .add_to_todo(&second, 3)
            .expect("quick add after rename");

        let PlaylistCreateOutcome::Existing(todo_by_reserved_name) = store
            .create_playlist("TODO", None, 4)
            .expect("reserved name lookup")
        else {
            panic!("fixed todo should already exist");
        };
        assert_eq!(todo_by_reserved_name.id, TODO_PLAYLIST_ID);
        assert_eq!(todo_by_reserved_name.name, "Listen later");
        assert_eq!(
            store
                .playlist(TODO_PLAYLIST_ID)
                .expect("read todo")
                .expect("todo")
                .entries
                .len(),
            2
        );
    }

    #[test]
    fn create_playlist_with_entry_is_atomic_and_reports_name_conflicts() {
        let store = StateStore::open_in_memory().expect("open store");
        let media = bandcamp_playlist_media("atomic-track");
        let PlaylistCreateOutcome::Created(created) = store
            .create_playlist_with_entry("Atomic", Some("One transaction"), &media, 1)
            .expect("create playlist with entry")
        else {
            panic!("expected created playlist");
        };
        assert_eq!(created.entry_count, 1);
        assert_eq!(
            store
                .create_playlist_with_entry(
                    "ATOMIC",
                    Some("Must not replace"),
                    &youtube_playlist_media("abcdefghijk", "Other"),
                    2,
                )
                .expect("existing-name result"),
            PlaylistCreateOutcome::Existing(created)
        );

        let mut invalid = youtube_playlist_media("dQw4w9WgXcQ", "Invalid URL");
        invalid.webpage_url =
            Url::parse("https://www.youtube.com/watch?v=abcdefghijk").expect("fixture URL");
        assert!(matches!(
            store.create_playlist_with_entry("Invalid snapshot", None, &invalid, 3),
            Err(PersistenceError::InvalidPlaylist { .. })
        ));
        assert!(
            store
                .playlists()
                .expect("playlists after validation failure")
                .iter()
                .all(|playlist| playlist.name != "Invalid snapshot")
        );

        store
            .connection
            .execute_batch(
                r"
                CREATE TRIGGER reject_atomic_playlist_entry
                BEFORE INSERT ON playlist_entries
                BEGIN
                    SELECT RAISE(ABORT, 'injected playlist-entry failure');
                END;
                ",
            )
            .expect("install failure trigger");
        assert!(matches!(
            store.create_playlist_with_entry("Rollback", None, &media, 4),
            Err(PersistenceError::Sqlite(_))
        ));
        assert!(
            store
                .playlists()
                .expect("playlists after insertion failure")
                .iter()
                .all(|playlist| playlist.name != "Rollback")
        );
    }

    #[test]
    fn playlist_snapshots_reject_noncanonical_or_oversized_state() {
        let store = StateStore::open_in_memory().expect("open store");
        let PlaylistCreateOutcome::Created(playlist) = store
            .create_playlist("Validation", None, 1)
            .expect("create validation playlist")
        else {
            panic!("expected new validation playlist");
        };

        let mut youtube = youtube_playlist_media("dQw4w9WgXcQ", "YouTube");
        youtube.replay_locator = "https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=10".to_owned();
        let mut apple = apple_playlist_media("9876543210");
        apple.webpage_url = Url::parse("https://podcasts.apple.com/us/podcast/fixture/id123?i=111")
            .expect("mismatched Apple fixture");
        let mut bandcamp = bandcamp_playlist_media("fixture-track");
        bandcamp.webpage_url =
            Url::parse("https://fixture-artist.bandcamp.com/album/fixture-track")
                .expect("Bandcamp album fixture");
        bandcamp.replay_locator = bandcamp.webpage_url.to_string();
        let mut remote = bandcamp_playlist_media("query-track");
        remote.id = MediaId::new(SourceKind::Rss, "episode-one");
        remote.webpage_url =
            Url::parse("https://feeds.example.test/episode?token=signed").expect("signed URL");
        remote.replay_locator = remote.webpage_url.to_string();
        let mut oversized = youtube_playlist_media("abcdefghijk", "Oversized");
        oversized.title = "x".repeat(MAX_PLAYLIST_TITLE_BYTES + 1);

        for snapshot in [youtube, apple, bandcamp, remote, oversized] {
            assert!(matches!(
                store.add_playlist_entry(&playlist.id, &snapshot, 2),
                Err(PersistenceError::InvalidPlaylist { .. })
            ));
        }
        assert_eq!(
            store
                .playlist(&playlist.id)
                .expect("validation playlist")
                .expect("playlist")
                .entries,
            Vec::new()
        );
        assert!(matches!(
            store.create_playlist(&"n".repeat(MAX_PLAYLIST_NAME_BYTES + 1), None, 3,),
            Err(PersistenceError::InvalidPlaylist { .. })
        ));
        assert!(matches!(
            store.create_playlist(
                "Oversized description",
                Some(&"d".repeat(MAX_PLAYLIST_DESCRIPTION_BYTES + 1)),
                3,
            ),
            Err(PersistenceError::InvalidPlaylist { .. })
        ));
    }

    #[test]
    fn librivox_playlist_keeps_book_page_and_exact_stable_chapter_audio() {
        let store = StateStore::open_in_memory().expect("open store");
        let media = librivox_playlist_media();
        store.add_to_todo(&media, 1).expect("save LibriVox item");
        assert_eq!(
            store
                .playlist(TODO_PLAYLIST_ID)
                .expect("load todo")
                .expect("todo exists")
                .entries[0]
                .media,
            media
        );
        let mut history_derived = librivox_playlist_media();
        history_derived.webpage_url =
            Url::parse(&history_derived.replay_locator).expect("stable chapter audio URL");
        validate_playlist_snapshot(&history_derived)
            .expect("History-derived LibriVox snapshot remains playlist-safe");

        let mut invalid = librivox_playlist_media();
        for replay_locator in [
            "https://example.org/download/fixture/chapter_01.mp3",
            "https://archive.org/details/fixture",
            "https://archive.org/download/fixture/chapter_01.ogg",
            "https://archive.org/download/fixture/chapter_01.mp3?token=secret",
        ] {
            invalid.replay_locator = replay_locator.to_owned();
            assert!(matches!(
                store.add_to_todo(&invalid, 2),
                Err(PersistenceError::InvalidPlaylist { .. })
            ));
        }
        invalid = librivox_playlist_media();
        invalid.id.external_id = "section:77736".to_owned();
        assert!(matches!(
            store.add_to_todo(&invalid, 2),
            Err(PersistenceError::InvalidPlaylist { .. })
        ));
    }

    #[test]
    fn playlist_read_rejects_snapshot_identity_corruption() {
        let store = StateStore::open_in_memory().expect("open store");
        let media = youtube_playlist_media("dQw4w9WgXcQ", "Original");
        store.add_to_todo(&media, 1).expect("seed todo");
        let mismatched = youtube_playlist_media("abcdefghijk", "Mismatched");
        store
            .connection
            .execute(
                r"
                UPDATE playlist_entries
                SET snapshot_json = ?1
                WHERE playlist_id = ?2
                ",
                params![
                    serde_json::to_string(&mismatched).expect("encode mismatch"),
                    TODO_PLAYLIST_ID
                ],
            )
            .expect("inject inconsistent snapshot");

        assert!(matches!(
            store.playlist(TODO_PLAYLIST_ID),
            Err(PersistenceError::Sqlite(
                rusqlite::Error::FromSqlConversionFailure(..)
            ))
        ));
    }

    #[test]
    fn progress_crud_preserves_completion_inputs() {
        let store = StateStore::open_in_memory().expect("open store");
        let mut progress = PlaybackProgress::new(id("progress"), Some(100), 1);
        progress.record_position(91, 2);
        assert!(progress.is_played());
        store.upsert_progress(&progress).expect("insert progress");
        assert_eq!(
            store.progress(&progress.media_id).expect("read"),
            Some(progress.clone())
        );

        progress.record_position(50, 3);
        progress.set_played(true);
        store.upsert_progress(&progress).expect("update progress");
        assert_eq!(
            store.progress(&progress.media_id).expect("read"),
            Some(progress.clone())
        );
        assert!(store.delete_progress(&progress.media_id).expect("delete"));
        assert_eq!(store.progress(&progress.media_id).expect("read"), None);
    }

    #[test]
    fn bulk_progress_lookup_chunks_mixed_sources_and_omits_missing_ids() {
        let store = StateStore::open_in_memory().expect("open store");
        let requested = (0..405)
            .map(|index| {
                MediaId::new(
                    if index % 2 == 0 {
                        SourceKind::Local
                    } else {
                        SourceKind::YouTube
                    },
                    format!("item-{index:03}"),
                )
            })
            .collect::<Vec<_>>();
        for media_id in requested.iter().step_by(3) {
            let mut progress = PlaybackProgress::new(media_id.clone(), Some(100), 1);
            progress.record_position(50, 2);
            store.upsert_progress(&progress).expect("seed progress");
        }
        let mut with_duplicate_and_missing = requested.clone();
        with_duplicate_and_missing.push(requested[0].clone());
        with_duplicate_and_missing.push(MediaId::new(SourceKind::Local, "missing"));

        let loaded = store
            .progress_for_media_ids(&with_duplicate_and_missing)
            .expect("bulk progress");

        assert_eq!(loaded.len(), 135);
        assert!(loaded.contains_key(&requested[0]));
        assert!(!loaded.contains_key(&requested[1]));
        assert!(!loaded.contains_key(&MediaId::new(SourceKind::Local, "missing")));
    }

    #[test]
    fn history_crud_and_finished_filter_work() {
        let store = StateStore::open_in_memory().expect("open store");
        let mut partial = HistoryEntry {
            id: 0,
            media_id: id("dQw4w9WgXcQ"),
            title: "Partial".to_owned(),
            replay_locator: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_owned()),
            started_at: 1,
            last_played_at: 2,
            position_seconds: 10,
            duration_seconds: Some(100),
            finished: false,
        };
        partial.id = store.insert_history(&partial).expect("insert history");
        let mut finished = HistoryEntry {
            id: 0,
            media_id: id("aqz-KE-bpKQ"),
            title: "Finished".to_owned(),
            replay_locator: Some("https://www.youtube.com/watch?v=aqz-KE-bpKQ".to_owned()),
            started_at: 3,
            last_played_at: 4,
            position_seconds: 95,
            duration_seconds: Some(100),
            finished: true,
        };
        finished.id = store.insert_history(&finished).expect("insert history");
        partial.title = "Updated".to_owned();
        partial.replay_locator = Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_owned());
        assert!(store.update_history(&partial).expect("update history"));

        assert_eq!(
            store.history(true, 10).expect("finished history"),
            vec![finished]
        );
        let all = store.history(false, 10).expect("all history");
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|entry| entry.title == "Updated"));
        assert!(store.delete_history(partial.id).expect("delete history"));
        assert_eq!(store.history(false, 10).expect("all history").len(), 1);
    }

    #[test]
    fn history_replay_locator_rejects_empty_and_oversized_values() {
        let store = StateStore::open_in_memory().expect("open store");
        let mut entry = HistoryEntry {
            id: 0,
            media_id: id("dQw4w9WgXcQ"),
            title: "Bounded".to_owned(),
            replay_locator: Some(String::new()),
            started_at: 1,
            last_played_at: 1,
            position_seconds: 0,
            duration_seconds: None,
            finished: false,
        };

        assert!(matches!(
            store.insert_history(&entry),
            Err(PersistenceError::InvalidHistoryReplayLocator { .. })
        ));
        entry.replay_locator = Some("x".repeat(MAX_HISTORY_REPLAY_LOCATOR_BYTES + 1));
        assert!(matches!(
            store.insert_history(&entry),
            Err(PersistenceError::InvalidHistoryReplayLocator { .. })
        ));
    }

    #[test]
    fn history_replay_locator_rejects_credentials_and_untrusted_queries() {
        let store = StateStore::open_in_memory().expect("open store");
        let mut entry = HistoryEntry {
            id: 0,
            media_id: MediaId::new(SourceKind::RemoteFiles, "https://media.example/track.opus"),
            title: "Remote".to_owned(),
            replay_locator: Some("https://user:password@media.example/track.opus".to_owned()),
            started_at: 1,
            last_played_at: 1,
            position_seconds: 0,
            duration_seconds: None,
            finished: false,
        };

        assert!(matches!(
            store.insert_history(&entry),
            Err(PersistenceError::InvalidHistoryReplayLocator { .. })
        ));
        entry.replay_locator = Some("https://media.example/track.opus?token=secret".to_owned());
        assert!(matches!(
            store.insert_history(&entry),
            Err(PersistenceError::InvalidHistoryReplayLocator { .. })
        ));

        entry.media_id = id("dQw4w9WgXcQ");
        entry.replay_locator = Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=30".to_owned());
        assert!(store.insert_history(&entry).is_ok());

        entry.media_id = MediaId::new(SourceKind::ApplePodcasts, "1000719462606");
        entry.replay_locator = Some(
            "https://us.podcasts.apple.com/us/podcast/show/id1756129194?i=1000719462606".to_owned(),
        );
        assert!(matches!(
            store.insert_history(&entry),
            Err(PersistenceError::InvalidHistoryReplayLocator { .. })
        ));
        entry.replay_locator = Some(
            "https://podcasts.apple.com/us/podcast/show/id1756129194?i=1000719462606".to_owned(),
        );
        assert!(store.insert_history(&entry).is_ok());

        entry.media_id = MediaId::new(
            SourceKind::LibriVox,
            librivox_section_external_id(5_936, 77_736),
        );
        entry.replay_locator =
            Some("https://archive.org/download/fixture/chapter_01.mp3".to_owned());
        assert!(store.insert_history(&entry).is_ok());
        entry.replay_locator = Some("https://archive.org/details/fixture".to_owned());
        assert!(matches!(
            store.insert_history(&entry),
            Err(PersistenceError::InvalidHistoryReplayLocator { .. })
        ));
    }

    #[test]
    fn private_comment_crud_is_targeted() {
        let store = StateStore::open_in_memory().expect("open store");
        let target = CommentTarget::Media {
            media_id: id("commented"),
        };
        let mut comment = PrivateComment {
            id: 0,
            target: target.clone(),
            body: "Remember this".to_owned(),
            created_at: 1,
            updated_at: 1,
        };
        comment.id = store
            .insert_private_comment(&comment)
            .expect("insert comment");
        comment.body = "Updated note".to_owned();
        comment.updated_at = 2;
        assert!(
            store
                .update_private_comment(&comment)
                .expect("update comment")
        );
        assert_eq!(
            store.private_comments(&target).expect("comments"),
            vec![comment.clone()]
        );
        assert!(
            store
                .delete_private_comment(comment.id)
                .expect("delete comment")
        );
        assert!(
            store
                .private_comments(&target)
                .expect("comments")
                .is_empty()
        );
    }

    #[test]
    fn private_note_upsert_is_exact_atomic_and_preserves_creation_time() {
        let store = StateStore::open_in_memory().expect("open store");
        let media_target = CommentTarget::Media {
            media_id: id("private-note"),
        };
        let source_target = CommentTarget::Source {
            source_id: id("private-note"),
        };
        let first_id = store
            .insert_private_comment(&PrivateComment {
                id: 0,
                target: media_target.clone(),
                body: "Legacy first".to_owned(),
                created_at: 10,
                updated_at: 10,
            })
            .expect("seed first legacy row");
        let newest_id = store
            .insert_private_comment(&PrivateComment {
                id: 0,
                target: media_target.clone(),
                body: "Legacy newest".to_owned(),
                created_at: 20,
                updated_at: 30,
            })
            .expect("seed newest legacy row");
        store
            .upsert_private_note(&source_target, "Channel note", 40)
            .expect("save independent source note");

        assert_eq!(
            store
                .private_note(&media_target)
                .expect("read media note")
                .expect("media note")
                .id,
            newest_id
        );
        let updated = store
            .upsert_private_note(&media_target, "Edited once", 50)
            .expect("edit and collapse media note");
        assert_eq!(updated.id, newest_id);
        assert_eq!(updated.created_at, 20);
        assert_eq!(updated.updated_at, 50);
        assert_eq!(
            store
                .private_comments(&media_target)
                .expect("collapsed legacy rows"),
            vec![updated.clone()]
        );
        assert_ne!(updated.id, first_id);
        assert_eq!(
            store
                .private_note(&source_target)
                .expect("read source note")
                .expect("source note")
                .body,
            "Channel note"
        );

        assert!(
            store
                .delete_private_note(&media_target)
                .expect("delete exact note")
        );
        assert!(
            store
                .private_note(&media_target)
                .expect("read deleted note")
                .is_none()
        );
        assert!(
            !store
                .delete_private_note(&media_target)
                .expect("repeat exact deletion")
        );
        assert!(
            store
                .private_note(&source_target)
                .expect("preserve source note")
                .is_some()
        );
    }

    #[test]
    fn bookmark_crud_is_ordered_by_position() {
        let store = StateStore::open_in_memory().expect("open store");
        let media_id = id("bookmarked");
        let mut late = Bookmark {
            id: 0,
            media_id: media_id.clone(),
            position_seconds: 80,
            label: Some("Late".to_owned()),
            created_at: 1,
        };
        late.id = store.insert_bookmark(&late).expect("insert bookmark");
        let mut early = Bookmark {
            id: 0,
            media_id: media_id.clone(),
            position_seconds: 10,
            label: None,
            created_at: 2,
        };
        early.id = store.insert_bookmark(&early).expect("insert bookmark");
        late.label = Some("Updated".to_owned());
        assert!(store.update_bookmark(&late).expect("update bookmark"));
        assert_eq!(
            store.bookmarks(&media_id).expect("bookmarks"),
            vec![early.clone(), late.clone()]
        );
        assert!(store.delete_bookmark(early.id).expect("delete bookmark"));
        assert_eq!(store.bookmarks(&media_id).expect("bookmarks"), vec![late]);
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn local_file_move_atomically_remaps_path_bearing_state_and_preserves_remote_rows() {
        let paths = tempdir().expect("fixture paths");
        let source = paths.path().join("old-track.flac");
        let target = paths.path().join("archive").join("renamed-track.flac");
        let old_id = MediaId::new(
            SourceKind::Local,
            source.to_str().expect("UTF-8 source path"),
        );
        let new_id = MediaId::new(
            SourceKind::Local,
            target.to_str().expect("UTF-8 target path"),
        );
        let mappings = [LocalMoveMapping {
            source: source.clone(),
            target: target.clone(),
        }];
        let store = StateStore::open_in_memory().expect("open store");
        store
            .add_to_todo(&local_playlist_media(&source, None), 1)
            .expect("seed Local playlist entry");

        let mut local_progress = PlaybackProgress::new(old_id.clone(), Some(180), 1);
        local_progress.record_position(42, 2);
        store
            .upsert_progress(&local_progress)
            .expect("seed local progress");
        let remote_id = id("dQw4w9WgXcQ");
        let remote_progress = PlaybackProgress::new(remote_id.clone(), Some(100), 1);
        store
            .upsert_progress(&remote_progress)
            .expect("seed remote progress");

        let local_history = HistoryEntry {
            id: 0,
            media_id: old_id.clone(),
            title: "Moved fixture".to_owned(),
            replay_locator: Some(source.to_string_lossy().into_owned()),
            started_at: 1,
            last_played_at: 2,
            position_seconds: 42,
            duration_seconds: Some(180),
            finished: false,
        };
        store
            .insert_history(&local_history)
            .expect("seed local history");
        let remote_history = HistoryEntry {
            id: 0,
            media_id: remote_id.clone(),
            title: "Remote fixture".to_owned(),
            replay_locator: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ".to_owned()),
            started_at: 1,
            last_played_at: 3,
            position_seconds: 10,
            duration_seconds: Some(100),
            finished: false,
        };
        store
            .insert_history(&remote_history)
            .expect("seed remote history");

        let media_comment = PrivateComment {
            id: 0,
            target: CommentTarget::Media {
                media_id: old_id.clone(),
            },
            body: "Moved note".to_owned(),
            created_at: 1,
            updated_at: 1,
        };
        store
            .insert_private_comment(&media_comment)
            .expect("seed media comment");
        let position_comment = PrivateComment {
            id: 0,
            target: CommentTarget::Position {
                media_id: old_id.clone(),
                position_seconds: 42,
            },
            body: "Moved position note".to_owned(),
            created_at: 2,
            updated_at: 2,
        };
        store
            .insert_private_comment(&position_comment)
            .expect("seed position comment");

        store
            .insert_bookmark(&Bookmark {
                id: 0,
                media_id: old_id.clone(),
                position_seconds: 42,
                label: Some("Moved bookmark".to_owned()),
                created_at: 1,
            })
            .expect("seed bookmark");

        let mut session = SessionState {
            screen: Screen::Channel(old_id.clone()),
            selected_media: Some(old_id.clone()),
            back_stack: vec![Screen::Channel(old_id.clone())],
            ..SessionState::default()
        };
        session.local_path = source.to_str().map(str::to_owned);
        store.save_session(&session, 1).expect("seed session");

        store
            .put_cached_metadata(&CachedMetadata {
                media: local_media(&source, None, Some(&source)),
                provenance: MetadataProvenance {
                    provider: "local-fixture".to_owned(),
                    source_url: Some(Url::from_file_path(&source).expect("fixture source URL")),
                    fetched_at: 1,
                    expires_at: None,
                },
            })
            .expect("seed metadata");
        store
            .put_cached_subscription_items(&CachedSubscriptionItems {
                source: SourceKind::Local,
                source_id: source.to_string_lossy().into_owned(),
                items: vec![search_video("local-subscription-fixture")],
                fetched_at: 1,
            })
            .expect("seed Local subscription cache");

        let report = store
            .remap_local_move_state(&mappings)
            .expect("remap durable state");
        assert_eq!(
            report,
            LocalMoveStateRemap {
                playback_progress: 1,
                playback_history: 1,
                private_comments: 2,
                bookmarks: 1,
                sessions: 1,
                metadata_cache: 1,
                subscription_items_cache: 1,
                playlist_entries: 1,
            }
        );
        assert_eq!(report.total(), 9);

        assert_eq!(store.progress(&old_id).expect("old progress"), None);
        assert_eq!(
            store
                .progress(&new_id)
                .expect("new progress")
                .expect("remapped progress")
                .position_seconds,
            42
        );
        assert_eq!(
            store
                .progress(&remote_id)
                .expect("remote progress")
                .expect("preserved remote progress"),
            remote_progress
        );

        let history = store.history(false, 10).expect("history");
        let moved_history = history
            .iter()
            .find(|entry| entry.media_id.source == SourceKind::Local)
            .expect("remapped local history");
        assert_eq!(moved_history.media_id, new_id);
        assert_eq!(moved_history.replay_locator.as_deref(), target.to_str());
        assert!(history.iter().any(|entry| entry.media_id == remote_id));

        let moved_media_target = CommentTarget::Media {
            media_id: new_id.clone(),
        };
        assert_eq!(
            store
                .private_comments(&moved_media_target)
                .expect("remapped media comments")
                .len(),
            1
        );
        let moved_position_target = CommentTarget::Position {
            media_id: new_id.clone(),
            position_seconds: 42,
        };
        assert_eq!(
            store
                .private_comments(&moved_position_target)
                .expect("remapped position comments")
                .len(),
            1
        );
        assert_eq!(
            store.bookmarks(&new_id).expect("remapped bookmarks").len(),
            1
        );

        let restored_session = store.session().expect("session").expect("saved session");
        assert_eq!(restored_session.selected_media, Some(new_id.clone()));
        assert_eq!(restored_session.local_path.as_deref(), target.to_str());
        assert_eq!(restored_session.screen, Screen::Channel(new_id.clone()));
        assert_eq!(
            restored_session.back_stack,
            vec![Screen::Channel(new_id.clone())]
        );

        let cached = store
            .cached_metadata(&new_id)
            .expect("cached metadata")
            .expect("remapped metadata");
        assert_eq!(cached.media.id, new_id);
        assert_eq!(
            cached.media.webpage_url,
            Url::from_file_path(&target).expect("target file URL")
        );
        assert_eq!(
            cached.media.captions[0].url,
            Url::from_file_path(&target).expect("target caption URL")
        );
        assert_eq!(
            cached.provenance.source_url,
            Some(Url::from_file_path(&target).expect("target source URL"))
        );
        assert!(
            store
                .cached_subscription_items(
                    &SourceKind::Local,
                    target.to_str().expect("UTF-8 target path"),
                )
                .expect("Local subscription cache")
                .is_some()
        );
        assert!(
            store
                .cached_subscription_items(
                    &SourceKind::Local,
                    source.to_str().expect("UTF-8 source path"),
                )
                .expect("old Local subscription cache")
                .is_none()
        );
        assert!(
            !store
                .todo_contains(&old_id)
                .expect("old Local playlist membership")
        );
        assert!(
            store
                .todo_contains(&new_id)
                .expect("remapped Local playlist membership")
        );
        let todo = store
            .playlist(TODO_PLAYLIST_ID)
            .expect("todo playlist")
            .expect("saved todo");
        assert_eq!(todo.entries[0].media.id, new_id);
        assert_eq!(
            todo.entries[0].media.webpage_url,
            Url::from_file_path(&target).expect("target playlist file URL")
        );
        assert_eq!(
            todo.entries[0].media.replay_locator,
            target.to_string_lossy()
        );
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn local_folder_move_remaps_descendants_and_survives_restart() {
        let directory = tempdir().expect("temporary directory");
        let config = Config::for_dir(directory.path().join("youta"));
        let source_folder = directory.path().join("music").join("album");
        let target_folder = directory.path().join("archive").join("album");
        let source_track = source_folder.join("disc").join("track.flac");
        let target_track = target_folder.join("disc").join("track.flac");
        let source_cover = source_folder.join("folder.jpg");
        let target_cover = target_folder.join("folder.jpg");
        let source_caption = source_folder.join("lyrics.vtt");
        let target_caption = target_folder.join("lyrics.vtt");
        let old_id = local_media_id(&source_track);
        let new_id = local_media_id(&target_track);
        let mapping = LocalMoveMapping {
            source: source_folder.clone(),
            target: target_folder.clone(),
        };

        {
            let store = StateStore::open(&config).expect("open state store");
            store
                .upsert_progress(&PlaybackProgress::new(old_id.clone(), Some(180), 1))
                .expect("seed progress");
            store
                .add_to_todo(&local_playlist_media(&source_track, Some(&source_cover)), 1)
                .expect("seed Local playlist snapshot");
            let session = SessionState {
                screen: Screen::Local,
                selected_media: Some(old_id.clone()),
                local_path: source_folder.to_str().map(str::to_owned),
                ..SessionState::default()
            };
            store.save_session(&session, 1).expect("seed session");
            store
                .put_cached_metadata(&CachedMetadata {
                    media: local_media(&source_track, Some(&source_cover), Some(&source_caption)),
                    provenance: MetadataProvenance {
                        provider: "local-fixture".to_owned(),
                        source_url: None,
                        fetched_at: 1,
                        expires_at: None,
                    },
                })
                .expect("seed metadata");
            store
                .put_cached_subscription_items(&CachedSubscriptionItems {
                    source: SourceKind::Local,
                    source_id: source_folder
                        .to_str()
                        .expect("UTF-8 source folder")
                        .to_owned(),
                    items: vec![local_subscription_video(
                        &source_track,
                        &source_folder,
                        &source_cover,
                    )],
                    fetched_at: 1,
                })
                .expect("seed Local subscription");
            store
                .remap_local_move_state(&[mapping])
                .expect("remap folder state");
        }

        let reopened = StateStore::open(&config).expect("reopen state store");
        assert!(reopened.progress(&old_id).expect("old progress").is_none());
        assert!(reopened.progress(&new_id).expect("new progress").is_some());
        let session = reopened.session().expect("session").expect("saved session");
        assert_eq!(session.selected_media, Some(new_id.clone()));
        assert_eq!(session.local_path.as_deref(), target_folder.to_str());
        let metadata = reopened
            .cached_metadata(&new_id)
            .expect("metadata")
            .expect("cached metadata");
        assert_eq!(
            metadata.media.thumbnail_url,
            Some(Url::from_file_path(&target_cover).expect("target cover URL"))
        );
        assert_eq!(
            metadata.media.captions[0].url,
            Url::from_file_path(&target_caption).expect("target caption URL")
        );
        let todo = reopened
            .playlist(TODO_PLAYLIST_ID)
            .expect("todo playlist")
            .expect("saved todo");
        assert_eq!(todo.entries[0].media.id, new_id);
        assert_eq!(
            todo.entries[0].media.thumbnail_url,
            Some(Url::from_file_path(&target_cover).expect("target playlist cover URL"))
        );
        let subscription = reopened
            .cached_subscription_items(
                &SourceKind::Local,
                target_folder.to_str().expect("UTF-8 target folder"),
            )
            .expect("Local subscription")
            .expect("remapped Local subscription");
        let [item] = subscription.items.as_slice() else {
            panic!("expected one Local subscription item");
        };
        assert_local_subscription_video_paths(item, &target_track, &target_folder, &target_cover);
        assert!(
            reopened
                .cached_subscription_items(
                    &SourceKind::Local,
                    source_folder.to_str().expect("UTF-8 source folder"),
                )
                .expect("old Local subscription")
                .is_none()
        );
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn local_subscription_destination_key_collision_is_rejected_without_mutation() {
        let paths = tempdir().expect("fixture paths");
        let source_folder = paths.path().join("source");
        let target_folder = paths.path().join("target");
        let source_track = source_folder.join("track.flac");
        let target_track = target_folder.join("track.flac");
        let store = StateStore::open_in_memory().expect("open store");
        store
            .put_cached_subscription_items(&CachedSubscriptionItems {
                source: SourceKind::Local,
                source_id: source_folder.to_string_lossy().into_owned(),
                items: vec![local_subscription_video(
                    &source_track,
                    &source_folder,
                    &source_folder.join("cover.jpg"),
                )],
                fetched_at: 1,
            })
            .expect("seed source subscription");
        store
            .put_cached_subscription_items(&CachedSubscriptionItems {
                source: SourceKind::Local,
                source_id: target_folder.to_string_lossy().into_owned(),
                items: vec![local_subscription_video(
                    &target_track,
                    &target_folder,
                    &target_folder.join("cover.jpg"),
                )],
                fetched_at: 2,
            })
            .expect("seed target subscription");

        assert!(matches!(
            store.remap_local_move_state(&[LocalMoveMapping {
                source: source_folder.clone(),
                target: target_folder.clone(),
            }]),
            Err(PersistenceError::InvalidLocalMoveStateRemap { .. })
        ));
        let source_cache = store
            .cached_subscription_items(
                &SourceKind::Local,
                source_folder.to_str().expect("UTF-8 source folder"),
            )
            .expect("source subscription")
            .expect("source subscription retained");
        let target_cache = store
            .cached_subscription_items(
                &SourceKind::Local,
                target_folder.to_str().expect("UTF-8 target folder"),
            )
            .expect("target subscription")
            .expect("target subscription retained");
        assert!(matches!(&source_cache.items[0], SearchItem::Video(video)
                if video.video_id == source_track.to_string_lossy()));
        assert!(matches!(&target_cache.items[0], SearchItem::Video(video)
                if video.video_id == target_track.to_string_lossy()));
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn local_move_journal_preflights_subscription_destination_key_bounds() {
        let store = StateStore::open_in_memory().expect("open store");
        let source = PathBuf::from("/music/source");
        store
            .put_cached_subscription_items(&CachedSubscriptionItems {
                source: SourceKind::Local,
                source_id: source.to_string_lossy().into_owned(),
                items: Vec::new(),
                fetched_at: 1,
            })
            .expect("seed Local subscription");
        let oversized_target = PathBuf::from(format!(
            "/{}",
            "d".repeat(MAX_SAVED_SUBSCRIPTION_SOURCE_ID_BYTES)
        ));
        let control_target = PathBuf::from("/archive\ninvalid/source");

        for target in [oversized_target, control_target] {
            assert!(
                store
                    .journal_local_move_intent(
                        &[LocalMoveMapping {
                            source: source.clone(),
                            target,
                        }],
                        1,
                    )
                    .is_err(),
                "invalid remapped cache keys must fail before journaling"
            );
            assert!(store.local_move_intents().expect("move journal").is_empty());
        }
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn local_subscription_remap_scan_rejects_rows_beyond_normal_cache_bound() {
        let store = StateStore::open_in_memory().expect("open store");
        for index in 0..=MAX_SAVED_SUBSCRIPTION_SOURCES {
            store
                .connection
                .execute(
                    r"
                    INSERT INTO subscription_items_cache (
                        source, source_id, items_json, fetched_at
                    ) VALUES ('local', ?1, '[]', 1)
                    ",
                    [format!("/local/{index:03}")],
                )
                .expect("seed excess Local subscription row");
        }
        let mapping = LocalMoveMapping {
            source: PathBuf::from("/move/source"),
            target: PathBuf::from("/move/target"),
        };

        let error = store
            .journal_local_move_intent(&[mapping], 1)
            .expect_err("excess rows must reject bounded remap planning");

        assert!(error.to_string().contains("source scan bound"));
        assert!(store.local_move_intents().expect("move journal").is_empty());
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn local_subscription_embedded_media_collision_is_rejected_without_mutation() {
        let paths = tempdir().expect("fixture paths");
        let source_folder = paths.path().join("source");
        let target_folder = paths.path().join("target");
        let source_track = source_folder.join("track.flac");
        let target_track = target_folder.join("track.flac");
        let subscription_id = paths.path().join("all-media");
        let original_items = vec![
            local_subscription_video(
                &source_track,
                &subscription_id,
                &source_folder.join("cover.jpg"),
            ),
            local_subscription_video(
                &target_track,
                &subscription_id,
                &target_folder.join("cover.jpg"),
            ),
        ];
        let store = StateStore::open_in_memory().expect("open store");
        store
            .put_cached_subscription_items(&CachedSubscriptionItems {
                source: SourceKind::Local,
                source_id: subscription_id.to_string_lossy().into_owned(),
                items: original_items.clone(),
                fetched_at: 1,
            })
            .expect("seed subscription");

        assert!(matches!(
            store.remap_local_move_state(&[LocalMoveMapping {
                source: source_folder,
                target: target_folder,
            }]),
            Err(PersistenceError::InvalidLocalMoveStateRemap { .. })
        ));
        assert_eq!(
            store
                .cached_subscription_items(
                    &SourceKind::Local,
                    subscription_id.to_str().expect("UTF-8 subscription ID"),
                )
                .expect("subscription")
                .expect("subscription retained")
                .items,
            original_items
        );
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn local_subscription_update_failure_rolls_back_earlier_table_updates() {
        let paths = tempdir().expect("fixture paths");
        let source_folder = paths.path().join("source");
        let target_folder = paths.path().join("target");
        let source_track = source_folder.join("track.flac");
        let target_track = target_folder.join("track.flac");
        let source_id = MediaId::new(
            SourceKind::Local,
            source_track.to_str().expect("UTF-8 source track"),
        );
        let target_id = MediaId::new(
            SourceKind::Local,
            target_track.to_str().expect("UTF-8 target track"),
        );
        let store = StateStore::open_in_memory().expect("open store");
        store
            .upsert_progress(&PlaybackProgress::new(source_id.clone(), Some(100), 1))
            .expect("seed progress");
        store
            .put_cached_subscription_items(&CachedSubscriptionItems {
                source: SourceKind::Local,
                source_id: source_folder.to_string_lossy().into_owned(),
                items: vec![local_subscription_video(
                    &source_track,
                    &source_folder,
                    &source_folder.join("cover.jpg"),
                )],
                fetched_at: 1,
            })
            .expect("seed subscription");
        store
            .connection
            .execute_batch(
                r"
                CREATE TRIGGER reject_local_subscription_move
                BEFORE UPDATE ON subscription_items_cache
                BEGIN
                    SELECT RAISE(ABORT, 'injected Local subscription move failure');
                END;
                ",
            )
            .expect("install failure trigger");

        assert!(matches!(
            store.remap_local_move_state(&[LocalMoveMapping {
                source: source_folder.clone(),
                target: target_folder.clone(),
            }]),
            Err(PersistenceError::Sqlite(_))
        ));
        assert!(
            store
                .progress(&source_id)
                .expect("source progress")
                .is_some()
        );
        assert!(
            store
                .progress(&target_id)
                .expect("target progress")
                .is_none()
        );
        assert!(
            store
                .cached_subscription_items(
                    &SourceKind::Local,
                    source_folder.to_str().expect("UTF-8 source folder"),
                )
                .expect("source subscription")
                .is_some()
        );
        assert!(
            store
                .cached_subscription_items(
                    &SourceKind::Local,
                    target_folder.to_str().expect("UTF-8 target folder"),
                )
                .expect("target subscription")
                .is_none()
        );
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn local_subscription_json_bound_is_checked_before_decoding() {
        let paths = tempdir().expect("fixture paths");
        let source = paths.path().join("source");
        let target = paths.path().join("target");
        let source_id = MediaId::new(
            SourceKind::Local,
            source.to_str().expect("UTF-8 source path"),
        );
        let store = StateStore::open_in_memory().expect("open store");
        store
            .upsert_progress(&PlaybackProgress::new(source_id.clone(), Some(100), 1))
            .expect("seed progress");
        store
            .connection
            .execute_batch("PRAGMA ignore_check_constraints = ON")
            .expect("allow oversized corruption fixture");
        store
            .connection
            .execute(
                r"
                INSERT INTO subscription_items_cache (
                    source, source_id, items_json, fetched_at
                ) VALUES ('local', ?1, ?2, 1)
                ",
                params![
                    source.to_str().expect("UTF-8 source path"),
                    "x".repeat(MAX_SAVED_SUBSCRIPTION_ITEMS_BYTES + 1)
                ],
            )
            .expect("seed oversized snapshot");

        assert!(matches!(
            store.remap_local_move_state(&[LocalMoveMapping { source, target }]),
            Err(PersistenceError::SubscriptionSnapshotTooLarge { .. })
        ));
        assert!(
            store
                .progress(&source_id)
                .expect("source progress")
                .is_some()
        );
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn local_subscription_remap_rejects_reencoded_json_over_bound() {
        let paths = tempdir().expect("fixture paths");
        let source_folder = paths.path().join("source");
        let target_folder = paths.path().join("target".repeat(8_000));
        let source_track = source_folder.join("track.flac");
        let subscription_id = paths
            .path()
            .join("subscriptions")
            .join("library")
            .to_string_lossy()
            .into_owned();
        let mut item = local_subscription_video(
            &source_track,
            &source_folder,
            &source_folder.join("cover.jpg"),
        );
        let SearchItem::Video(video) = &mut item else {
            unreachable!("fixture is a video");
        };
        video.description = "x".repeat(400_000);
        let original_items = vec![item];
        let original_json =
            serde_json::to_string(&original_items).expect("encode original fixture");
        assert!(original_json.len() < MAX_SAVED_SUBSCRIPTION_ITEMS_BYTES);
        let store = StateStore::open_in_memory().expect("open store");
        store
            .put_cached_subscription_items(&CachedSubscriptionItems {
                source: SourceKind::Local,
                source_id: subscription_id.clone(),
                items: original_items.clone(),
                fetched_at: 1,
            })
            .expect("seed bounded subscription");

        assert!(matches!(
            store.remap_local_move_state(&[LocalMoveMapping {
                source: source_folder,
                target: target_folder,
            }]),
            Err(PersistenceError::SubscriptionSnapshotTooLarge { .. })
        ));
        assert_eq!(
            store
                .cached_subscription_items(&SourceKind::Local, &subscription_id)
                .expect("subscription")
                .expect("subscription retained")
                .items,
            original_items
        );
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn local_playlist_destination_collision_is_rejected_before_journaling() {
        let paths = tempdir().expect("fixture paths");
        let source = paths.path().join("source.flac");
        let target = paths.path().join("target.flac");
        let source_media = local_playlist_media(&source, None);
        let target_media = local_playlist_media(&target, None);
        let store = StateStore::open_in_memory().expect("open store");
        let PlaylistCreateOutcome::Created(playlist) = store
            .create_playlist_with_entry("Move collision", None, &source_media, 1)
            .expect("create playlist")
        else {
            panic!("expected new playlist");
        };
        store
            .add_playlist_entry(&playlist.id, &target_media, 2)
            .expect("seed stale target entry");
        let mapping = LocalMoveMapping {
            source: source.clone(),
            target: target.clone(),
        };

        assert!(matches!(
            store.journal_local_move_intent(std::slice::from_ref(&mapping), 3),
            Err(PersistenceError::InvalidLocalMoveStateRemap { .. })
        ));
        assert!(store.local_move_intents().expect("move journal").is_empty());
        assert!(matches!(
            store.remap_local_move_state(std::slice::from_ref(&mapping)),
            Err(PersistenceError::InvalidLocalMoveStateRemap { .. })
        ));
        let retained = store
            .playlist(&playlist.id)
            .expect("playlist")
            .expect("retained playlist");
        assert_eq!(
            retained
                .entries
                .iter()
                .map(|entry| entry.media.id.clone())
                .collect::<Vec<_>>(),
            vec![source_media.id, target_media.id]
        );
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn local_playlist_identity_is_scoped_per_playlist_during_move() {
        let paths = tempdir().expect("fixture paths");
        let source = paths.path().join("source.flac");
        let target = paths.path().join("target.flac");
        let source_media = local_playlist_media(&source, None);
        let target_media = local_playlist_media(&target, None);
        let store = StateStore::open_in_memory().expect("open store");
        let PlaylistCreateOutcome::Created(first) = store
            .create_playlist_with_entry("First", None, &source_media, 1)
            .expect("create first playlist")
        else {
            panic!("expected first playlist");
        };
        let PlaylistCreateOutcome::Created(second) = store
            .create_playlist_with_entry("Second", None, &target_media, 2)
            .expect("create second playlist")
        else {
            panic!("expected second playlist");
        };

        let report = store
            .remap_local_move_state(&[LocalMoveMapping {
                source,
                target: target.clone(),
            }])
            .expect("remap across separate playlists");
        assert_eq!(report.playlist_entries, 1);
        let memberships = store
            .playlist_memberships(&local_media_id(&target))
            .expect("target memberships");
        assert_eq!(memberships.len(), 2);
        assert!(memberships.contains(&first.id));
        assert!(memberships.contains(&second.id));
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn local_move_destination_identity_collision_is_rejected_without_mutation() {
        let paths = tempdir().expect("fixture paths");
        let source = paths.path().join("source.flac");
        let target = paths.path().join("target.flac");
        let source_id = MediaId::new(
            SourceKind::Local,
            source.to_str().expect("UTF-8 source path"),
        );
        let target_id = MediaId::new(
            SourceKind::Local,
            target.to_str().expect("UTF-8 target path"),
        );
        let store = StateStore::open_in_memory().expect("open store");
        store
            .upsert_progress(&PlaybackProgress::new(source_id.clone(), Some(100), 1))
            .expect("seed source progress");
        store
            .upsert_progress(&PlaybackProgress::new(target_id.clone(), Some(200), 2))
            .expect("seed target progress");
        store
            .insert_history(&HistoryEntry {
                id: 0,
                media_id: source_id.clone(),
                title: "Source".to_owned(),
                replay_locator: Some(source.to_string_lossy().into_owned()),
                started_at: 1,
                last_played_at: 1,
                position_seconds: 10,
                duration_seconds: Some(100),
                finished: false,
            })
            .expect("seed history");

        assert!(matches!(
            store.remap_local_move_state(&[LocalMoveMapping {
                source: source.clone(),
                target: target.clone(),
            }]),
            Err(PersistenceError::InvalidLocalMoveStateRemap { .. })
        ));
        assert!(
            store
                .progress(&source_id)
                .expect("source progress")
                .is_some()
        );
        assert!(
            store
                .progress(&target_id)
                .expect("target progress")
                .is_some()
        );
        let history = store.history(false, 10).expect("history");
        assert_eq!(history[0].media_id, source_id);
        assert_eq!(history[0].replay_locator.as_deref(), source.to_str());
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn local_move_database_failure_rolls_back_earlier_table_updates() {
        let paths = tempdir().expect("fixture paths");
        let source = paths.path().join("source.flac");
        let target = paths.path().join("target.flac");
        let source_id = MediaId::new(
            SourceKind::Local,
            source.to_str().expect("UTF-8 source path"),
        );
        let target_id = MediaId::new(
            SourceKind::Local,
            target.to_str().expect("UTF-8 target path"),
        );
        let store = StateStore::open_in_memory().expect("open store");
        store
            .upsert_progress(&PlaybackProgress::new(source_id.clone(), Some(100), 1))
            .expect("seed progress");
        store
            .insert_history(&HistoryEntry {
                id: 0,
                media_id: source_id.clone(),
                title: "Source".to_owned(),
                replay_locator: Some(source.to_string_lossy().into_owned()),
                started_at: 1,
                last_played_at: 1,
                position_seconds: 10,
                duration_seconds: Some(100),
                finished: false,
            })
            .expect("seed history");
        store
            .connection
            .execute_batch(
                r"
                CREATE TRIGGER reject_local_history_move
                BEFORE UPDATE ON playback_history
                BEGIN
                    SELECT RAISE(ABORT, 'injected Local move failure');
                END;
                ",
            )
            .expect("install failure trigger");

        assert!(matches!(
            store.remap_local_move_state(&[LocalMoveMapping {
                source: source.clone(),
                target,
            }]),
            Err(PersistenceError::Sqlite(_))
        ));
        assert!(
            store
                .progress(&source_id)
                .expect("source progress")
                .is_some()
        );
        assert!(
            store
                .progress(&target_id)
                .expect("target progress")
                .is_none()
        );
        assert_eq!(
            store.history(false, 10).expect("history")[0].media_id,
            source_id
        );
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn local_move_rejects_overlapping_or_chained_mappings() {
        let paths = tempdir().expect("fixture paths");
        let first_source = paths.path().join("one");
        let nested_source = first_source.join("two");
        let first_target = paths.path().join("moved-one");
        let second_target = paths.path().join("moved-two");
        let store = StateStore::open_in_memory().expect("open store");

        assert!(matches!(
            store.remap_local_move_state(&[
                LocalMoveMapping {
                    source: first_source.clone(),
                    target: first_target.clone(),
                },
                LocalMoveMapping {
                    source: nested_source,
                    target: second_target,
                },
            ]),
            Err(PersistenceError::InvalidLocalMoveStateRemap { .. })
        ));
        assert!(matches!(
            store.remap_local_move_state(&[
                LocalMoveMapping {
                    source: first_source,
                    target: first_target.clone(),
                },
                LocalMoveMapping {
                    source: first_target,
                    target: paths.path().join("third"),
                },
            ]),
            Err(PersistenceError::InvalidLocalMoveStateRemap { .. })
        ));
    }

    #[test]
    fn sqlite_playback_checkpoint_commits_progress_and_statistics_together() {
        let store = StateStore::open_in_memory().expect("open store");
        let media_id = id("transactional-checkpoint");
        let mut progress = PlaybackProgress::new(media_id.clone(), Some(180), 10);
        progress.record_position(42, 11);

        store
            .checkpoint_playback(&progress, &SourceKind::YouTube, 30)
            .expect("transactional checkpoint");
        store
            .flush_playback_checkpoint()
            .expect("SQLite flush is a no-op");

        assert_eq!(
            store.progress(&media_id).expect("saved progress"),
            Some(progress)
        );
        assert_eq!(
            store
                .listened_seconds(&SourceKind::YouTube)
                .expect("saved statistics"),
            30
        );
    }

    #[test]
    fn sqlite_playback_checkpoint_rolls_back_progress_when_statistics_fail() {
        let store = StateStore::open_in_memory().expect("open store");
        store
            .connection
            .execute_batch(
                r"
                CREATE TRIGGER reject_checkpoint_statistics
                BEFORE INSERT ON listen_totals
                BEGIN
                    SELECT RAISE(ABORT, 'injected checkpoint failure');
                END;
                ",
            )
            .expect("failure trigger");
        let media_id = id("failed-checkpoint");
        let progress = PlaybackProgress::new(media_id.clone(), Some(180), 10);

        assert!(matches!(
            store.checkpoint_playback(&progress, &SourceKind::YouTube, 30),
            Err(PersistenceError::Sqlite(_))
        ));
        assert!(
            store
                .progress(&media_id)
                .expect("rolled-back progress")
                .is_none()
        );
        assert_eq!(
            store
                .listened_seconds(&SourceKind::YouTube)
                .expect("rolled-back statistics"),
            0
        );
    }

    #[test]
    fn session_and_listen_totals_upsert_and_reset() {
        let store = StateStore::open_in_memory().expect("open store");
        assert_eq!(store.session().expect("empty session"), None);
        let state = SessionState {
            screen: Screen::History,
            focus: PanelFocus::Right,
            search_text: SearchQuery::new("ambient").text,
            chapter_timestamps_hidden: true,
            ..SessionState::default()
        };
        store.save_session(&state, 10).expect("save session");
        assert_eq!(store.session().expect("session"), Some(state));

        store
            .add_listen_seconds(&SourceKind::YouTube, 3_000)
            .expect("add seconds");
        store
            .add_listen_seconds(&SourceKind::YouTube, 600)
            .expect("add seconds");
        store
            .add_listen_seconds(&SourceKind::Rss, 1_800)
            .expect("add seconds");
        assert_eq!(
            store
                .listened_seconds(&SourceKind::YouTube)
                .expect("listened seconds"),
            3_600
        );
        assert_eq!(
            store.listen_totals().expect("totals"),
            vec![
                ListenTotal {
                    source: SourceKind::YouTube,
                    total_seconds: 3_600,
                },
                ListenTotal {
                    source: SourceKind::Rss,
                    total_seconds: 1_800,
                },
            ]
        );
        assert!(
            store
                .reset_listen_seconds(&SourceKind::YouTube)
                .expect("reset total")
        );
        assert_eq!(
            store
                .listened_seconds(&SourceKind::YouTube)
                .expect("listened seconds"),
            0
        );
    }

    #[test]
    fn youtube_search_snapshot_round_trips_overwrites_and_clears() {
        let store = StateStore::open_in_memory().expect("open store");
        let mut request = SearchRequest::new("mock ambient", SearchTarget::Videos);
        request.page = 2;
        request.sort = SearchSort::UploadDate;
        let first = SavedYouTubeSearch {
            request,
            results: vec![search_video("first"), search_video("second")],
            next_page: Some(3),
        };
        store
            .save_youtube_search(&first, 10)
            .expect("save first search");
        assert_eq!(
            store.youtube_search().expect("load first search"),
            Some(first)
        );

        let replacement = SavedYouTubeSearch {
            request: SearchRequest::new("mock channels", SearchTarget::Channels),
            results: vec![SearchItem::Channel(ChannelSummary {
                channel_id: "UCreplacement".to_owned(),
                name: "Replacement".to_owned(),
                description: "Mock channel".to_owned(),
                subscriber_count: Some(42),
                video_count: Some(7),
                created_at: None,
                auto_generated: false,
                thumbnails: Vec::new(),
                webpage_url: None,
            })],
            next_page: None,
        };
        store
            .save_youtube_search(&replacement, 20)
            .expect("replace search");
        assert_eq!(
            store.youtube_search().expect("load replacement"),
            Some(replacement)
        );
        assert!(store.clear_youtube_search().expect("clear search"));
        assert!(!store.clear_youtube_search().expect("clear absent search"));
        assert_eq!(store.youtube_search().expect("empty search"), None);
    }

    #[test]
    fn youtube_music_search_snapshot_round_trips_and_rejects_channels() {
        let store = StateStore::open_in_memory().expect("open store");
        let saved = SavedYouTubeMusicSearch {
            query: "mock ambient".to_owned(),
            results: vec![search_video("first"), search_video("second")],
        };
        store
            .save_youtube_music_search(&saved, 10)
            .expect("save music search");
        assert_eq!(
            store.youtube_music_search().expect("load music search"),
            Some(saved)
        );

        let invalid = SavedYouTubeMusicSearch {
            query: "channel result".to_owned(),
            results: vec![SearchItem::Channel(ChannelSummary {
                channel_id: "UCfixture".to_owned(),
                name: "Fixture".to_owned(),
                description: String::new(),
                subscriber_count: None,
                video_count: None,
                created_at: None,
                auto_generated: false,
                thumbnails: Vec::new(),
                webpage_url: None,
            })],
        };
        assert!(matches!(
            store.save_youtube_music_search(&invalid, 20),
            Err(PersistenceError::InvalidSavedSearch { .. })
        ));
        assert!(store.clear_youtube_music_search().expect("clear search"));
        assert_eq!(
            store.youtube_music_search().expect("empty music search"),
            None
        );
    }

    #[test]
    fn bandcamp_search_snapshot_survives_restart_replaces_and_clears() {
        let (_directory, config, store) = disk_store();
        let first = SavedBandcampSearch {
            query: "ambient field recordings".to_owned(),
            page: 1,
            results: vec![
                bandcamp_release(BandcampReleaseKind::Track, "first-track"),
                bandcamp_release(BandcampReleaseKind::Album, "first-album"),
            ],
            next_page: Some(2),
        };
        store
            .save_bandcamp_search(&first, 10)
            .expect("save Bandcamp search");
        drop(store);

        let store = StateStore::open(&config).expect("reopen state store");
        assert_eq!(
            store.bandcamp_search().expect("restore Bandcamp search"),
            Some(first)
        );
        let replacement = SavedBandcampSearch {
            query: "Georgian electronic".to_owned(),
            page: 2,
            results: vec![bandcamp_release(
                BandcampReleaseKind::Album,
                "replacement-album",
            )],
            next_page: Some(3),
        };
        store
            .save_bandcamp_search(&replacement, 20)
            .expect("replace Bandcamp search");
        assert_eq!(
            store
                .bandcamp_search()
                .expect("load replacement Bandcamp search"),
            Some(replacement)
        );
        assert!(
            store
                .clear_bandcamp_search()
                .expect("clear Bandcamp search")
        );
        assert!(
            !store
                .clear_bandcamp_search()
                .expect("clear absent Bandcamp search")
        );
        assert_eq!(
            store.bandcamp_search().expect("empty Bandcamp search"),
            None
        );
    }

    #[test]
    fn bandcamp_search_rejects_inconsistent_identity_page_and_duplicates() {
        let store = StateStore::open_in_memory().expect("open store");
        let valid = SavedBandcampSearch {
            query: "ambient".to_owned(),
            page: 1,
            results: vec![bandcamp_release(
                BandcampReleaseKind::Track,
                "fixture-track",
            )],
            next_page: Some(2),
        };

        for invalid in [
            SavedBandcampSearch {
                query: " ambient ".to_owned(),
                ..valid.clone()
            },
            SavedBandcampSearch {
                query: "x".repeat(MAX_SAVED_BANDCAMP_QUERY_BYTES + 1),
                ..valid.clone()
            },
            SavedBandcampSearch {
                page: 0,
                ..valid.clone()
            },
            SavedBandcampSearch {
                next_page: Some(3),
                ..valid.clone()
            },
            SavedBandcampSearch {
                page: MAX_SAVED_BANDCAMP_PAGE,
                next_page: Some(MAX_SAVED_BANDCAMP_PAGE + 1),
                ..valid.clone()
            },
            SavedBandcampSearch {
                results: vec![
                    bandcamp_release(BandcampReleaseKind::Track, "fixture-track"),
                    bandcamp_release(BandcampReleaseKind::Track, "fixture-track"),
                ],
                ..valid.clone()
            },
            SavedBandcampSearch {
                results: vec![BandcampSearchSummary {
                    id: MediaId::new(SourceKind::YouTube, "fixture-artist/track/fixture-track"),
                    ..bandcamp_release(BandcampReleaseKind::Track, "fixture-track")
                }],
                ..valid.clone()
            },
            SavedBandcampSearch {
                results: vec![BandcampSearchSummary {
                    kind: BandcampReleaseKind::Album,
                    ..bandcamp_release(BandcampReleaseKind::Track, "fixture-track")
                }],
                ..valid.clone()
            },
            SavedBandcampSearch {
                results: vec![BandcampSearchSummary {
                    id: MediaId::new(SourceKind::Bandcamp, "fixture-artist/track/different-track"),
                    ..bandcamp_release(BandcampReleaseKind::Track, "fixture-track")
                }],
                ..valid.clone()
            },
        ] {
            assert!(
                matches!(
                    store.save_bandcamp_search(&invalid, 1),
                    Err(PersistenceError::InvalidSavedSearch { .. })
                ),
                "invalid Bandcamp snapshot should be rejected: {invalid:?}"
            );
        }

        let excessive = SavedBandcampSearch {
            results: (0..=MAX_SAVED_BANDCAMP_SEARCH_RESULTS)
                .map(|index| {
                    bandcamp_release(
                        BandcampReleaseKind::Track,
                        &format!("fixture-track-{index}"),
                    )
                })
                .collect(),
            ..valid
        };
        assert!(matches!(
            store.save_bandcamp_search(&excessive, 1),
            Err(PersistenceError::InvalidSavedSearch { .. })
        ));
    }

    #[test]
    fn bandcamp_search_rejects_noncanonical_pages_artwork_and_metadata() {
        let store = StateStore::open_in_memory().expect("open store");
        let valid = SavedBandcampSearch {
            query: "ambient".to_owned(),
            page: 1,
            results: vec![bandcamp_release(
                BandcampReleaseKind::Track,
                "fixture-track",
            )],
            next_page: None,
        };

        for invalid_release in [
            BandcampSearchSummary {
                webpage_url: Url::parse(
                    "https://fixture-artist.bandcamp.com/track/fixture-track?from=search",
                )
                .expect("valid noncanonical URL"),
                ..bandcamp_release(BandcampReleaseKind::Track, "fixture-track")
            },
            BandcampSearchSummary {
                webpage_url: Url::parse(
                    "https://fixture-artist.bandcamp.com.evil.test/track/fixture-track",
                )
                .expect("valid lookalike URL"),
                ..bandcamp_release(BandcampReleaseKind::Track, "fixture-track")
            },
            BandcampSearchSummary {
                artwork_url: Some(
                    Url::parse("https://f4.bcbits.com.evil.test/img/a123_16.jpg")
                        .expect("valid artwork lookalike"),
                ),
                ..bandcamp_release(BandcampReleaseKind::Track, "fixture-track")
            },
            BandcampSearchSummary {
                title: " fixture title ".to_owned(),
                ..bandcamp_release(BandcampReleaseKind::Track, "fixture-track")
            },
            BandcampSearchSummary {
                artist: Some("x".repeat(MAX_SAVED_BANDCAMP_ARTIST_BYTES + 1)),
                ..bandcamp_release(BandcampReleaseKind::Track, "fixture-track")
            },
        ] {
            let invalid = SavedBandcampSearch {
                results: vec![invalid_release],
                ..valid.clone()
            };
            assert!(
                matches!(
                    store.save_bandcamp_search(&invalid, 1),
                    Err(PersistenceError::InvalidSavedSearch { .. })
                ),
                "unsafe Bandcamp summary should be rejected: {invalid:?}"
            );
        }
    }

    #[test]
    fn bandcamp_search_load_checks_size_and_revalidates_modified_rows() {
        let store = StateStore::open_in_memory().expect("open store");
        let saved = SavedBandcampSearch {
            query: "ambient".to_owned(),
            page: 1,
            results: vec![bandcamp_release(
                BandcampReleaseKind::Track,
                "fixture-track",
            )],
            next_page: None,
        };
        store
            .save_bandcamp_search(&saved, 1)
            .expect("save valid Bandcamp snapshot");
        let mut invalid_release = bandcamp_release(BandcampReleaseKind::Track, "fixture-track");
        invalid_release.id.external_id = "fixture-artist/track/other-track".to_owned();
        store
            .connection
            .execute(
                "UPDATE bandcamp_search_state SET results_json = ?1 WHERE slot = 1",
                [serde_json::to_string(&vec![invalid_release])
                    .expect("encode invalid Bandcamp fixture")],
            )
            .expect("corrupt Bandcamp snapshot");
        assert!(matches!(
            store.bandcamp_search(),
            Err(PersistenceError::InvalidSavedSearch { .. })
        ));

        store
            .connection
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("allow oversized corruption fixture");
        store
            .connection
            .execute(
                "UPDATE bandcamp_search_state SET results_json = ?1 WHERE slot = 1",
                ["x".repeat(MAX_SAVED_BANDCAMP_RESULTS_BYTES + 1)],
            )
            .expect("write oversized Bandcamp snapshot");
        assert!(matches!(
            store.bandcamp_search(),
            Err(PersistenceError::SavedSearchTooLarge {
                field: "Bandcamp results",
                ..
            })
        ));
    }

    #[test]
    fn apple_podcasts_search_snapshot_survives_restart_replaces_and_clears() {
        let (_directory, config, store) = disk_store();
        let first = SavedApplePodcastsSearch {
            query: "science history".to_owned(),
            storefront: "us".to_owned(),
            results: vec![apple_show("123456789"), apple_show("987654321")],
        };
        store
            .save_apple_podcasts_search(&first, 10)
            .expect("save Apple search");
        drop(store);

        let store = StateStore::open(&config).expect("reopen state store");
        assert_eq!(
            store.apple_podcasts_search().expect("restore Apple search"),
            Some(first)
        );
        let replacement = SavedApplePodcastsSearch {
            query: "Georgian podcasts".to_owned(),
            storefront: "ge".to_owned(),
            results: vec![apple_show("456789123")],
        };
        store
            .save_apple_podcasts_search(&replacement, 20)
            .expect("replace Apple search");
        assert_eq!(
            store
                .apple_podcasts_search()
                .expect("load replacement Apple search"),
            Some(replacement)
        );
        assert!(
            store
                .clear_apple_podcasts_search()
                .expect("clear Apple search")
        );
        assert!(
            !store
                .clear_apple_podcasts_search()
                .expect("clear absent Apple search")
        );
        assert_eq!(
            store.apple_podcasts_search().expect("empty Apple search"),
            None
        );
    }

    #[test]
    fn apple_podcasts_search_rejects_invalid_identity_storefront_and_duplicates() {
        let store = StateStore::open_in_memory().expect("open store");
        let valid = SavedApplePodcastsSearch {
            query: "science".to_owned(),
            storefront: "us".to_owned(),
            results: vec![apple_show("123456789")],
        };

        for invalid in [
            SavedApplePodcastsSearch {
                query: " science ".to_owned(),
                ..valid.clone()
            },
            SavedApplePodcastsSearch {
                storefront: "US".to_owned(),
                ..valid.clone()
            },
            SavedApplePodcastsSearch {
                results: vec![apple_show("123456789"), apple_show("123456789")],
                ..valid.clone()
            },
            SavedApplePodcastsSearch {
                results: vec![PodcastShowSummary {
                    id: MediaId::new(SourceKind::YouTube, "123456789"),
                    ..apple_show("123456789")
                }],
                ..valid.clone()
            },
            SavedApplePodcastsSearch {
                results: vec![apple_show("0")],
                ..valid.clone()
            },
        ] {
            assert!(
                matches!(
                    store.save_apple_podcasts_search(&invalid, 1),
                    Err(PersistenceError::InvalidSavedSearch { .. })
                ),
                "invalid Apple snapshot should be rejected: {invalid:?}"
            );
        }

        let excessive = SavedApplePodcastsSearch {
            results: (1..=(MAX_SAVED_APPLE_RESULTS + 1))
                .map(|index| apple_show(&index.to_string()))
                .collect(),
            ..valid
        };
        assert!(matches!(
            store.save_apple_podcasts_search(&excessive, 1),
            Err(PersistenceError::InvalidSavedSearch { .. })
        ));
    }

    #[test]
    fn apple_podcasts_search_rejects_unsafe_urls_and_oversized_json() {
        let store = StateStore::open_in_memory().expect("open store");
        let valid = SavedApplePodcastsSearch {
            query: "science".to_owned(),
            storefront: "us".to_owned(),
            results: vec![apple_show("123456789")],
        };
        let unsafe_page = SavedApplePodcastsSearch {
            results: vec![PodcastShowSummary {
                webpage_url: Some(
                    Url::parse("https://podcasts.apple.com.evil.test/us/podcast/id123456789")
                        .expect("valid lookalike URL"),
                ),
                ..apple_show("123456789")
            }],
            ..valid.clone()
        };
        assert!(matches!(
            store.save_apple_podcasts_search(&unsafe_page, 1),
            Err(PersistenceError::InvalidSavedSearch { .. })
        ));

        for raw in [
            "https://us.podcasts.apple.com/us/podcast/id123456789",
            "http://podcasts.apple.com/us/podcast/id123456789",
            "https://podcasts.apple.com:8443/us/podcast/id123456789",
        ] {
            let noncanonical_page = SavedApplePodcastsSearch {
                results: vec![PodcastShowSummary {
                    webpage_url: Some(Url::parse(raw).expect("valid fixture URL")),
                    ..apple_show("123456789")
                }],
                ..valid.clone()
            };
            assert!(
                matches!(
                    store.save_apple_podcasts_search(&noncanonical_page, 1),
                    Err(PersistenceError::InvalidSavedSearch { .. })
                ),
                "noncanonical saved Apple page should be rejected: {raw}"
            );
        }

        let fragmented_feed = SavedApplePodcastsSearch {
            results: vec![PodcastShowSummary {
                feed_url: Some(
                    Url::parse("https://feeds.example.test/show.xml#private")
                        .expect("valid fragment URL"),
                ),
                ..apple_show("123456789")
            }],
            ..valid.clone()
        };
        assert!(matches!(
            store.save_apple_podcasts_search(&fragmented_feed, 1),
            Err(PersistenceError::InvalidSavedSearch { .. })
        ));

        let ported_feed = SavedApplePodcastsSearch {
            results: vec![PodcastShowSummary {
                feed_url: Some(
                    Url::parse("https://feeds.example.test:8443/show.xml")
                        .expect("valid non-default-port URL"),
                ),
                ..apple_show("123456789")
            }],
            ..valid.clone()
        };
        assert!(matches!(
            store.save_apple_podcasts_search(&ported_feed, 1),
            Err(PersistenceError::InvalidSavedSearch { .. })
        ));

        for raw in [
            "http://127.0.0.1/private-feed.xml",
            "https://10.0.0.1/private-feed.xml",
            "https://169.254.169.254/private-feed.xml",
            "https://[::1]/private-feed.xml",
            "https://feeds.localhost/private-feed.xml",
        ] {
            let private_feed = SavedApplePodcastsSearch {
                results: vec![PodcastShowSummary {
                    feed_url: Some(Url::parse(raw).expect("valid private-host URL")),
                    ..apple_show("123456789")
                }],
                ..valid.clone()
            };
            assert!(
                matches!(
                    store.save_apple_podcasts_search(&private_feed, 1),
                    Err(PersistenceError::InvalidSavedSearch { .. })
                ),
                "saved Apple private-host feed should be rejected: {raw}"
            );
        }

        let private_artwork = SavedApplePodcastsSearch {
            results: vec![PodcastShowSummary {
                artwork_url: Some(
                    Url::parse("https://192.168.1.10/private.jpg")
                        .expect("valid private-host artwork URL"),
                ),
                ..apple_show("123456789")
            }],
            ..valid.clone()
        };
        assert!(matches!(
            store.save_apple_podcasts_search(&private_artwork, 1),
            Err(PersistenceError::InvalidSavedSearch { .. })
        ));

        let large_query_value = "x".repeat(15_000);
        let oversized = SavedApplePodcastsSearch {
            results: (1..=150)
                .map(|index| {
                    let mut show = apple_show(&index.to_string());
                    show.feed_url = Some(
                        Url::parse(&format!(
                            "https://feeds.example.test/show.xml?item={index}&data={large_query_value}"
                        ))
                        .expect("valid large fixture URL"),
                    );
                    show
                })
                .collect(),
            ..valid
        };
        assert!(matches!(
            store.save_apple_podcasts_search(&oversized, 1),
            Err(PersistenceError::SavedSearchTooLarge {
                field: "Apple Podcasts results",
                ..
            })
        ));
    }

    #[test]
    fn apple_podcasts_search_revalidates_manually_modified_rows() {
        let store = StateStore::open_in_memory().expect("open store");
        let saved = SavedApplePodcastsSearch {
            query: "science".to_owned(),
            storefront: "us".to_owned(),
            results: vec![apple_show("123456789")],
        };
        store
            .save_apple_podcasts_search(&saved, 1)
            .expect("save valid Apple snapshot");
        let mut invalid_url = apple_show("123456789");
        invalid_url.webpage_url = Some(
            Url::parse("https://podcasts.apple.com:8443/us/podcast/id123456789")
                .expect("valid non-default-port URL"),
        );
        store
            .connection
            .execute(
                "UPDATE apple_podcasts_search_state SET results_json = ?1 WHERE slot = 1",
                [serde_json::to_string(&vec![invalid_url]).expect("encode invalid URL fixture")],
            )
            .expect("corrupt Apple snapshot URL");
        assert!(matches!(
            store.apple_podcasts_search(),
            Err(PersistenceError::InvalidSavedSearch { .. })
        ));

        let mut invalid = apple_show("123456789");
        invalid.id.source = SourceKind::YouTube;
        store
            .connection
            .execute(
                "UPDATE apple_podcasts_search_state SET results_json = ?1 WHERE slot = 1",
                [serde_json::to_string(&vec![invalid]).expect("encode invalid fixture")],
            )
            .expect("corrupt Apple snapshot");

        assert!(matches!(
            store.apple_podcasts_search(),
            Err(PersistenceError::InvalidSavedSearch { .. })
        ));
    }

    #[test]
    fn subscription_items_snapshot_survives_restart_overwrites_and_deletes() {
        let (directory, config, store) = disk_store();
        let first = CachedSubscriptionItems {
            source: SourceKind::YouTube,
            source_id: "UCfixture".to_owned(),
            items: vec![search_video("first"), search_video("second")],
            fetched_at: 100,
        };
        store
            .put_cached_subscription_items(&first)
            .expect("save first-page snapshot");
        drop(store);

        let store = StateStore::open(&config).expect("reopen state store");
        assert_eq!(
            store
                .cached_subscription_items(&SourceKind::YouTube, "UCfixture")
                .expect("restore first-page snapshot"),
            Some(first)
        );

        let replacement = CachedSubscriptionItems {
            source: SourceKind::YouTube,
            source_id: "UCfixture".to_owned(),
            items: vec![search_video("newest")],
            fetched_at: 200,
        };
        store
            .put_cached_subscription_items(&replacement)
            .expect("replace first-page snapshot");
        assert_eq!(
            store
                .cached_subscription_items(&SourceKind::YouTube, "UCfixture")
                .expect("load replacement"),
            Some(replacement)
        );
        assert!(
            store
                .delete_cached_subscription_items(&SourceKind::YouTube, "UCfixture")
                .expect("delete snapshot")
        );
        assert!(
            !store
                .delete_cached_subscription_items(&SourceKind::YouTube, "UCfixture")
                .expect("delete absent snapshot")
        );
        assert_eq!(
            store
                .cached_subscription_items(&SourceKind::YouTube, "UCfixture")
                .expect("snapshot is absent"),
            None
        );
        drop(directory);
    }

    #[test]
    fn subscription_items_snapshot_accepts_only_source_compatible_rss_episodes() {
        let store = StateStore::open_in_memory().expect("open store");
        let feed_url = "https://podcasts.example.test/feed.xml";
        let rss = CachedSubscriptionItems {
            source: SourceKind::Rss,
            source_id: feed_url.to_owned(),
            items: vec![
                podcast_episode(feed_url, "episode-one"),
                podcast_episode(feed_url, "episode-two"),
            ],
            fetched_at: 100,
        };

        store
            .put_cached_subscription_items(&rss)
            .expect("save RSS episode snapshot");
        assert_eq!(
            store
                .cached_subscription_items(&SourceKind::Rss, feed_url)
                .expect("restore RSS episode snapshot"),
            Some(rss.clone())
        );

        let incompatible = CachedSubscriptionItems {
            source: SourceKind::YouTube,
            source_id: "UCfixture".to_owned(),
            ..rss
        };
        assert!(matches!(
            store.put_cached_subscription_items(&incompatible),
            Err(PersistenceError::InvalidSubscriptionSnapshot { reason })
                if reason.contains("RSS podcast episodes")
        ));
    }

    #[test]
    fn subscription_items_snapshot_rejects_invalid_data_and_byte_fits_a_prefix() {
        let store = StateStore::open_in_memory().expect("open store");
        let excessive = CachedSubscriptionItems {
            source: SourceKind::YouTube,
            source_id: "UCexcessive".to_owned(),
            items: vec![search_video("bounded"); MAX_SAVED_SUBSCRIPTION_ITEMS + 1],
            fetched_at: 1,
        };
        assert!(matches!(
            store.put_cached_subscription_items(&excessive),
            Err(PersistenceError::InvalidSubscriptionSnapshot { .. })
        ));

        let channel = CachedSubscriptionItems {
            source: SourceKind::YouTube,
            source_id: "UCchannel".to_owned(),
            items: vec![SearchItem::Channel(ChannelSummary {
                channel_id: "UCfixture".to_owned(),
                name: "Fixture".to_owned(),
                description: String::new(),
                subscriber_count: None,
                video_count: None,
                created_at: None,
                auto_generated: false,
                thumbnails: Vec::new(),
                webpage_url: None,
            })],
            fetched_at: 2,
        };
        assert!(matches!(
            store.put_cached_subscription_items(&channel),
            Err(PersistenceError::InvalidSubscriptionSnapshot { .. })
        ));

        let mut oversized_item = search_video("oversized");
        let SearchItem::Video(video) = &mut oversized_item else {
            unreachable!("fixture is a video");
        };
        video.description = "x".repeat(MAX_SAVED_SUBSCRIPTION_ITEMS_BYTES);
        let oversized = CachedSubscriptionItems {
            source: SourceKind::Rss,
            source_id: "https://podcasts.example/feed.xml".to_owned(),
            items: vec![
                search_video("retained"),
                oversized_item,
                search_video("not-reordered"),
            ],
            fetched_at: 3,
        };
        store
            .put_cached_subscription_items(&oversized)
            .expect("store longest bounded prefix");
        let fitted = store
            .cached_subscription_items(&SourceKind::Rss, "https://podcasts.example/feed.xml")
            .expect("load fitted snapshot")
            .expect("fitted snapshot");
        assert_eq!(fitted.items, vec![search_video("retained")]);

        let invalid_identity = CachedSubscriptionItems {
            source: SourceKind::Rss,
            source_id: " https://podcasts.example/feed.xml".to_owned(),
            items: Vec::new(),
            fetched_at: 4,
        };
        assert!(matches!(
            store.put_cached_subscription_items(&invalid_identity),
            Err(PersistenceError::InvalidSubscriptionSnapshot { .. })
        ));

        let invalid_time = CachedSubscriptionItems {
            source: SourceKind::Rss,
            source_id: "https://podcasts.example/feed.xml".to_owned(),
            items: Vec::new(),
            fetched_at: -1,
        };
        assert!(matches!(
            store.put_cached_subscription_items(&invalid_time),
            Err(PersistenceError::InvalidSubscriptionSnapshot { .. })
        ));
    }

    #[test]
    fn subscription_items_cache_evicts_the_oldest_sources_deterministically() {
        let store = StateStore::open_in_memory().expect("open store");
        for index in 0..=MAX_SAVED_SUBSCRIPTION_SOURCES {
            let source_id = format!("UC{index:03}");
            store
                .put_cached_subscription_items(&CachedSubscriptionItems {
                    source: SourceKind::YouTube,
                    source_id,
                    items: vec![search_video(&format!("video-{index:03}"))],
                    fetched_at: i64::try_from(index).expect("fixture timestamp"),
                })
                .expect("save bounded snapshot");
        }

        let row_count: i64 = store
            .connection
            .query_row("SELECT count(*) FROM subscription_items_cache", [], |row| {
                row.get(0)
            })
            .expect("count retained snapshots");
        assert_eq!(
            row_count,
            i64::try_from(MAX_SAVED_SUBSCRIPTION_SOURCES).expect("source bound fits SQLite")
        );
        assert_eq!(
            store
                .cached_subscription_items(&SourceKind::YouTube, "UC000")
                .expect("oldest lookup"),
            None
        );
        assert!(
            store
                .cached_subscription_items(
                    &SourceKind::YouTube,
                    &format!("UC{MAX_SAVED_SUBSCRIPTION_SOURCES:03}"),
                )
                .expect("newest lookup")
                .is_some()
        );
    }

    #[test]
    fn subscription_items_load_revalidates_manually_modified_rows() {
        let store = StateStore::open_in_memory().expect("open store");
        let invalid_items = vec![SearchItem::Channel(ChannelSummary {
            channel_id: "UCfixture".to_owned(),
            name: "Fixture".to_owned(),
            description: String::new(),
            subscriber_count: None,
            video_count: None,
            created_at: None,
            auto_generated: false,
            thumbnails: Vec::new(),
            webpage_url: None,
        })];
        store
            .connection
            .execute(
                r"
				INSERT INTO subscription_items_cache (
					source, source_id, items_json, fetched_at
				) VALUES ('youtube', 'UCfixture', ?1, 1)
				",
                [serde_json::to_string(&invalid_items).expect("encode invalid fixture")],
            )
            .expect("insert manually modified row");

        assert!(matches!(
            store.cached_subscription_items(&SourceKind::YouTube, "UCfixture"),
            Err(PersistenceError::InvalidSubscriptionSnapshot { .. })
        ));
    }

    #[test]
    fn youtube_orientation_enrichment_updates_only_the_matching_saved_summary() {
        let store = StateStore::open_in_memory().expect("open store");
        let saved = SavedYouTubeSearch {
            request: SearchRequest::new("vertical fixture", SearchTarget::Videos),
            results: vec![search_video("vertical"), search_video("unchanged")],
            next_page: None,
        };
        store.save_youtube_search(&saved, 10).expect("save search");

        assert!(
            store
                .update_saved_youtube_video_orientation("vertical", VideoOrientation::Vertical, 20,)
                .expect("cache orientation")
        );
        assert!(
            !store
                .update_saved_youtube_video_orientation(
                    "missing",
                    VideoOrientation::Horizontal,
                    30,
                )
                .expect("ignore absent video")
        );
        let restored = store
            .youtube_search()
            .expect("load enriched search")
            .expect("saved search");
        let orientations = restored
            .results
            .iter()
            .map(|item| match item {
                SearchItem::Video(video) => video.orientation,
                SearchItem::Channel(_) | SearchItem::PodcastEpisode(_) => {
                    unreachable!("video fixture")
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            orientations,
            [VideoOrientation::Vertical, VideoOrientation::Unknown]
        );
    }

    #[test]
    fn youtube_search_snapshot_rejects_inconsistent_and_excessive_results() {
        let store = StateStore::open_in_memory().expect("open store");
        let mismatched = SavedYouTubeSearch {
            request: SearchRequest::new("channels", SearchTarget::Channels),
            results: vec![search_video("video")],
            next_page: None,
        };
        assert!(matches!(
            store.save_youtube_search(&mismatched, 1),
            Err(PersistenceError::InvalidSavedSearch { .. })
        ));

        let invalid_continuation = SavedYouTubeSearch {
            request: SearchRequest::new("videos", SearchTarget::Videos),
            results: vec![search_video("video")],
            next_page: Some(1),
        };
        assert!(matches!(
            store.save_youtube_search(&invalid_continuation, 1),
            Err(PersistenceError::InvalidSavedSearch { .. })
        ));

        let excessive = SavedYouTubeSearch {
            request: SearchRequest::new("videos", SearchTarget::Videos),
            results: vec![search_video("bounded"); MAX_SAVED_YOUTUBE_SEARCH_RESULTS + 1],
            next_page: None,
        };
        assert!(matches!(
            store.save_youtube_search(&excessive, 1),
            Err(PersistenceError::InvalidSavedSearch { .. })
        ));

        let mut oversized = search_video("oversized");
        let SearchItem::Video(video) = &mut oversized else {
            unreachable!("fixture is a video");
        };
        video.description = "x".repeat(MAX_SAVED_SEARCH_RESULTS_BYTES);
        let oversized = SavedYouTubeSearch {
            request: SearchRequest::new("videos", SearchTarget::Videos),
            results: vec![oversized],
            next_page: None,
        };
        assert!(matches!(
            store.save_youtube_search(&oversized, 1),
            Err(PersistenceError::SavedSearchTooLarge {
                field: "results",
                ..
            })
        ));
    }

    #[test]
    fn youtube_search_load_checks_json_bytes_before_decoding() {
        let store = StateStore::open_in_memory().expect("open store");
        store
            .connection
            .execute(
                r"
				INSERT INTO youtube_search_state (
					slot, request_json, results_json, next_page, updated_at
				) VALUES (1, '{}', ?1, NULL, 1)
				",
                ["x".repeat(MAX_SAVED_SEARCH_RESULTS_BYTES + 1)],
            )
            .expect("insert oversized fixture");

        assert!(matches!(
            store.youtube_search(),
            Err(PersistenceError::SavedSearchTooLarge {
                field: "results",
                ..
            })
        ));
    }

    #[test]
    fn metadata_cache_round_trips_provenance_and_expires() {
        let store = StateStore::open_in_memory().expect("open store");
        let cached = CachedMetadata {
            media: media("cached"),
            provenance: MetadataProvenance {
                provider: "invidious.example".to_owned(),
                source_url: Some(
                    Url::parse("https://invidious.example/api/v1/videos/cached")
                        .expect("valid provenance URL"),
                ),
                fetched_at: 100,
                expires_at: Some(200),
            },
        };
        store.put_cached_metadata(&cached).expect("cache metadata");
        let loaded = store
            .cached_metadata(&cached.media.id)
            .expect("load metadata")
            .expect("cached row");
        assert_eq!(loaded, cached);
        assert!(loaded.is_fresh_at(199));
        assert!(!loaded.is_fresh_at(200));
        assert_eq!(
            store.delete_expired_metadata(200).expect("delete expired"),
            1
        );
        assert_eq!(
            store
                .cached_metadata(&cached.media.id)
                .expect("load metadata"),
            None
        );
    }

    #[test]
    fn channel_summary_cache_survives_restart_and_retains_stale_rows() {
        let directory = tempdir().expect("temporary directory");
        let config = Config::for_dir(directory.path().join("youta"));
        let cached = CachedChannelSummary {
            summary: ChannelSummary {
                channel_id: "UCfixture".to_owned(),
                name: "Fixture channel".to_owned(),
                description: "Mock channel description".to_owned(),
                subscriber_count: Some(1_850_000),
                video_count: Some(321),
                created_at: Some(1_100_000_000),
                auto_generated: false,
                thumbnails: vec![Thumbnail {
                    url: Url::parse("https://example.test/channel-avatar.jpg")
                        .expect("valid thumbnail URL"),
                    quality: Some("high".to_owned()),
                    width: Some(800),
                    height: Some(800),
                }],
                webpage_url: Some(
                    Url::parse("https://www.youtube.com/channel/UCfixture")
                        .expect("valid channel URL"),
                ),
            },
            fetched_at: 100,
            expires_at: 200,
        };

        {
            let store = StateStore::open(&config).expect("open initial store");
            store
                .put_cached_channel_summary(&cached)
                .expect("cache channel summary");
        }

        let store = StateStore::open(&config).expect("reopen store");
        let restored = store
            .cached_channel_summary("UCfixture")
            .expect("load channel summary after restart")
            .expect("cached channel row");
        assert_eq!(restored, cached);
        assert!(restored.is_fresh_at(199));
        assert!(!restored.is_fresh_at(200));
        assert_eq!(
            store
                .cached_channel_summary("UCmissing")
                .expect("load absent channel"),
            None
        );

        let mut replacement = cached;
        replacement.summary.subscriber_count = Some(1_900_000);
        replacement.fetched_at = 200;
        replacement.expires_at = 300;
        store
            .put_cached_channel_summary(&replacement)
            .expect("replace channel summary");
        assert_eq!(
            store
                .cached_channel_summary("UCfixture")
                .expect("load stale channel"),
            Some(replacement)
        );
        assert_eq!(
            store
                .delete_expired_channel_summaries(300)
                .expect("delete expired channel"),
            1
        );
        assert_eq!(
            store
                .cached_channel_summary("UCfixture")
                .expect("load deleted channel"),
            None
        );
    }

    #[test]
    fn wikidata_cache_round_trips_positive_and_empty_results() {
        let store = StateStore::open_in_memory().expect("open store");
        let positive = CachedWikidataLookup {
            property_id: "P6456".to_owned(),
            external_id: "BV1xx411c7mD".to_owned(),
            items: vec![WikidataLink {
                item_id: "Q123".to_owned(),
                label: "Fixture video".to_owned(),
                description: Some("Bilibili video fixture".to_owned()),
                url: Url::parse("https://www.wikidata.org/wiki/Q123").expect("valid Wikidata URL"),
            }],
            fetched_at: 100,
            expires_at: 200,
        };
        let empty = CachedWikidataLookup {
            property_id: "P6455".to_owned(),
            external_id: "546195".to_owned(),
            items: Vec::new(),
            fetched_at: 100,
            expires_at: 150,
        };
        store
            .put_cached_wikidata(&positive)
            .expect("cache positive lookup");
        store
            .put_cached_wikidata(&empty)
            .expect("cache empty lookup");

        assert_eq!(
            store
                .cached_wikidata("P6456", "BV1xx411c7mD")
                .expect("load positive lookup"),
            Some(positive.clone())
        );
        assert!(positive.is_fresh_at(199));
        assert!(!positive.is_fresh_at(200));
        assert_eq!(
            store
                .cached_wikidata("P6455", "546195")
                .expect("load empty lookup"),
            Some(empty)
        );
        assert_eq!(
            store
                .delete_expired_wikidata(150)
                .expect("delete empty lookup"),
            1
        );
        assert_eq!(
            store
                .cached_wikidata("P6455", "546195")
                .expect("load deleted lookup"),
            None
        );
        assert!(
            store
                .cached_wikidata("P6456", "BV1xx411c7mD")
                .expect("load fresh lookup")
                .is_some()
        );
    }
}
