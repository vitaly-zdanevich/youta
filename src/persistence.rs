//! SQLite-backed restart-safe state.
//!
//! [`StateStore`] keeps frequently updated state in one database under Youta's
//! application directory. Connections use WAL journaling on disk so periodic
//! progress updates do not block UI reads. SQL statements used by normal CRUD
//! operations are prepared through rusqlite's bounded statement cache.

use std::path::Path;
use std::time::Duration;

use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::config::{Config, ConfigError};
use crate::domain::{
    Bookmark, CommentTarget, HistoryEntry, MediaId, MediaItem, PlaybackProgress, PrivateComment,
    SessionState, SourceKind, WikidataLink,
};
use crate::providers::{SearchItem, SearchRequest, SearchTarget};

const MAX_SAVED_SEARCH_REQUEST_BYTES: usize = 16 * 1024;
const MAX_SAVED_SEARCH_RESULTS_BYTES: usize = 4 * 1024 * 1024;

/// Maximum number of `YouTube` summaries retained in one restart snapshot.
///
/// The application also uses this as its accumulated lazy-search limit so the
/// visible list and its durable representation cannot diverge.
pub const MAX_SAVED_YOUTUBE_SEARCH_RESULTS: usize = 500;

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
];

/// Current on-disk schema version.
pub const SCHEMA_VERSION: u32 = 4;

/// One bounded `YouTube` search snapshot retained across application restarts.
///
/// Search summaries intentionally exclude lazily fetched video details,
/// subscriber enrichment, and Wikidata data. Those retain their independent
/// cache and request policies after restoration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SavedYouTubeSearch {
    /// Exact request that produced the most recently accepted page.
    pub request: SearchRequest,
    /// Accumulated video or channel summaries shown in the search list.
    pub results: Vec<SearchItem>,
    /// Next provider page to request when the user reaches the list boundary.
    pub next_page: Option<u32>,
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListenTotal {
    /// Source whose playback time was counted.
    pub source: SourceKind,
    /// Exact accumulated value. The UI can display whole or fractional hours.
    pub total_seconds: u64,
}

/// A migrated `SQLite` connection for Youta state.
pub struct StateStore {
    connection: Connection,
}

impl StateStore {
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
        self.connection
            .prepare_cached(
                r"
				INSERT INTO playback_history (
					source, external_id, title, started_at, last_played_at,
					position_seconds, duration_seconds, finished
				) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
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
					finished = ?9
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
				position_seconds, duration_seconds, finished
			FROM playback_history
			WHERE finished = 1
			ORDER BY last_played_at DESC, id DESC
			LIMIT ?1
			"
        } else {
            r"
			SELECT id, source, external_id, title, started_at, last_played_at,
				position_seconds, duration_seconds, finished
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

    /// Replaces the restart snapshot for the active `YouTube` search.
    ///
    /// The request, accumulated summaries, and continuation page are validated
    /// and byte-bounded before `SQLite` receives them. Enriched details are not
    /// part of this snapshot.
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
        self.connection
            .prepare_cached(
                r"
				INSERT INTO listen_totals (source, total_seconds)
				VALUES (?1, ?2)
				ON CONFLICT(source) DO UPDATE SET
					total_seconds = total_seconds + excluded.total_seconds
				",
            )?
            .execute(params![source.as_str(), seconds])?;
        Ok(())
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
    #[error("SQLite state operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A JSON payload could not be encoded or decoded.
    #[error("stored JSON state is invalid: {0}")]
    Json(#[from] serde_json::Error),
    /// A cached provenance URL is malformed.
    #[error("stored metadata provenance URL is invalid: {0}")]
    InvalidCachedUrl(url::ParseError),
    /// A saved search JSON field exceeds its fixed persistence limit.
    #[error("saved YouTube search {field} exceeds the {maximum_bytes}-byte limit")]
    SavedSearchTooLarge {
        /// Snapshot field that exceeded its bound.
        field: &'static str,
        /// Maximum accepted encoded size.
        maximum_bytes: usize,
    },
    /// A saved search is internally inconsistent or outside provider limits.
    #[error("saved YouTube search is invalid: {reason}")]
    InvalidSavedSearch {
        /// Invariant rejected while saving or restoring.
        reason: String,
    },
    /// An unsigned domain value is too large for `SQLite`'s signed integer.
    #[error("{field} is too large for SQLite")]
    IntegerOutOfRange {
        /// Name of the field that overflowed.
        field: &'static str,
    },
    /// An on-disk database was unable to enter WAL mode.
    #[error("SQLite refused WAL journal mode and selected {0:?}")]
    WalUnavailable(String),
    /// The database was written by a newer Youta schema.
    #[error("database schema {found} is newer than supported schema {supported}")]
    UnsupportedSchema {
        /// Version stored in `SQLite`.
        found: u32,
        /// Newest version this binary understands.
        supported: u32,
    },
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

fn to_sql_u64(value: u64, field: &'static str) -> Result<i64, PersistenceError> {
    i64::try_from(value).map_err(|_| PersistenceError::IntegerOutOfRange { field })
}

fn from_sql_u64(row: &Row<'_>, index: usize) -> rusqlite::Result<u64> {
    from_sql_u64_value(row.get(index)?, index)
}

fn from_sql_optional_u64(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    row.get::<_, Option<i64>>(index)?
        .map(|value| from_sql_u64_value(value, index))
        .transpose()
}

fn from_sql_u64_value(value: i64, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Integer, Box::new(error))
    })
}

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

fn history_entry_from_row(row: &Row<'_>) -> rusqlite::Result<HistoryEntry> {
    Ok(HistoryEntry {
        id: row.get(0)?,
        media_id: MediaId::new(
            SourceKind::from(row.get::<_, String>(1)?.as_str()),
            row.get::<_, String>(2)?,
        ),
        title: row.get(3)?,
        started_at: row.get(4)?,
        last_played_at: row.get(5)?,
        position_seconds: from_sql_u64(row, 6)?,
        duration_seconds: from_sql_optional_u64(row, 7)?,
        finished: row.get(8)?,
    })
}

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
fn set_private_file_permissions(path: &Path) -> std::io::Result<()> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::domain::{
        MediaKind, MediaLicense, MediaStatistics, PanelFocus, Screen, SearchQuery,
    };
    use crate::providers::{ChannelSummary, SearchSort, VideoSummary};

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
            thumbnails: Vec::new(),
            webpage_url: None,
            stream_url: None,
        })
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
    fn migration_from_v3_preserves_session_and_adds_youtube_search_state() {
        let connection = Connection::open_in_memory().expect("open SQLite");
        for version in 1..=3_u32 {
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
            .expect("seed version-three session");

        run_migrations(&connection).expect("migrate to current schema");
        let store = StateStore { connection };
        assert_eq!(store.schema_version().expect("schema version"), 4);
        assert_eq!(store.session().expect("preserved session"), Some(session));
        assert_eq!(store.youtube_search().expect("new search table"), None);
    }

    #[test]
    fn progress_crud_preserves_completion_inputs() {
        let store = StateStore::open_in_memory().expect("open store");
        let mut progress = PlaybackProgress::new(id("progress"), Some(100), 1);
        progress.record_position(90, 2);
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
    fn history_crud_and_finished_filter_work() {
        let store = StateStore::open_in_memory().expect("open store");
        let mut partial = HistoryEntry {
            id: 0,
            media_id: id("partial"),
            title: "Partial".to_owned(),
            started_at: 1,
            last_played_at: 2,
            position_seconds: 10,
            duration_seconds: Some(100),
            finished: false,
        };
        partial.id = store.insert_history(&partial).expect("insert history");
        let mut finished = HistoryEntry {
            id: 0,
            media_id: id("finished"),
            title: "Finished".to_owned(),
            started_at: 3,
            last_played_at: 4,
            position_seconds: 95,
            duration_seconds: Some(100),
            finished: true,
        };
        finished.id = store.insert_history(&finished).expect("insert history");
        partial.title = "Updated".to_owned();
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

    #[test]
    fn session_and_listen_totals_upsert_and_reset() {
        let store = StateStore::open_in_memory().expect("open store");
        assert_eq!(store.session().expect("empty session"), None);
        let state = SessionState {
            screen: Screen::History,
            focus: PanelFocus::Right,
            search_text: SearchQuery::new("ambient").text,
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
