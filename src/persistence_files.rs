//! Deterministic, human-readable TOML persistence.
//!
//! User-owned records live below `state/`, restart-only UI data below
//! `runtime/`, and regenerable provider snapshots below `cache/`. Documents
//! are rewritten canonically through same-directory atomic replacement, which
//! keeps Git diffs stable and prevents a partial write from replacing the last
//! valid copy. A malformed restart-only or cache document is preserved under a
//! private, collision-safe quarantine name and replaced with an empty canonical
//! document. Authoritative user documents are never quarantined or recreated
//! automatically.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::*;

const FILE_FORMAT_VERSION: u32 = 1;
const MAX_MANIFEST_DOCUMENT_BYTES: usize = 4 * 1024;
const MAX_PROGRESS_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_HISTORY_DOCUMENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_NOTES_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_BOOKMARKS_DOCUMENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_STATISTICS_DOCUMENT_BYTES: usize = 1024 * 1024;
const MAX_LOCAL_MOVES_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_PLAYLISTS_DOCUMENT_BYTES: usize = 128 * 1024 * 1024;
#[cfg(feature = "yandex-music")]
const MAX_YANDEX_MUSIC_REACTIONS_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_RUNTIME_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_PLAYBACK_CHECKPOINT_DOCUMENT_BYTES: usize = 16 * 1024;
const MAX_SEARCH_CACHE_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROVIDER_CACHE_DOCUMENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_REGENERABLE_QUARANTINE_SLOTS: usize = 1_024;
const MAX_PROGRESS_ROWS: usize = 250_000;
const MAX_HISTORY_ROWS: usize = 250_000;
const MAX_PRIVATE_COMMENT_ROWS: usize = 100_000;
const MAX_BOOKMARK_ROWS: usize = 250_000;
const MAX_LISTEN_TOTAL_ROWS: usize = 256;
const MAX_METADATA_CACHE_ROWS: usize = 10_000;
const MAX_CHANNEL_SUMMARY_CACHE_ROWS: usize = 5_000;
const MAX_WIKIDATA_CACHE_ROWS: usize = 5_000;
const MAX_FILE_LOCAL_MOVE_MAPPINGS: usize = 10_000;
const SQLITE_INTEGER_MAX_U64: u64 = i64::MAX as u64;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManifestDocument {
    format_version: u32,
    backend: String,
}

impl Default for ManifestDocument {
    fn default() -> Self {
        Self {
            format_version: FILE_FORMAT_VERSION,
            backend: "files".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct ProgressDocument {
    format_version: u32,
    progress: Vec<PlaybackProgress>,
}

impl ProgressDocument {
    fn empty() -> Self {
        Self {
            format_version: FILE_FORMAT_VERSION,
            progress: Vec::new(),
        }
    }

    fn canonicalize(&mut self) {
        self.progress
            .sort_by(|left, right| media_key(&left.media_id).cmp(&media_key(&right.media_id)));
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct HistoryDocument {
    format_version: u32,
    next_id: i64,
    history: Vec<HistoryEntry>,
}

impl HistoryDocument {
    fn empty() -> Self {
        Self {
            format_version: FILE_FORMAT_VERSION,
            next_id: 1,
            history: Vec::new(),
        }
    }

    fn canonicalize(&mut self) {
        self.history.sort_by_key(|entry| entry.id);
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct NotesDocument {
    format_version: u32,
    next_id: i64,
    comments: Vec<PrivateComment>,
}

impl NotesDocument {
    fn empty() -> Self {
        Self {
            format_version: FILE_FORMAT_VERSION,
            next_id: 1,
            comments: Vec::new(),
        }
    }

    fn canonicalize(&mut self) {
        self.comments.sort_by_key(|comment| comment.id);
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct BookmarksDocument {
    format_version: u32,
    next_id: i64,
    bookmarks: Vec<Bookmark>,
}

impl BookmarksDocument {
    fn empty() -> Self {
        Self {
            format_version: FILE_FORMAT_VERSION,
            next_id: 1,
            bookmarks: Vec::new(),
        }
    }

    fn canonicalize(&mut self) {
        self.bookmarks.sort_by_key(|bookmark| bookmark.id);
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct StatisticsDocument {
    format_version: u32,
    listen_totals: Vec<ListenTotal>,
}

impl StatisticsDocument {
    fn empty() -> Self {
        Self {
            format_version: FILE_FORMAT_VERSION,
            listen_totals: Vec::new(),
        }
    }

    fn canonicalize(&mut self) {
        self.listen_totals.sort_by(|left, right| {
            left.source
                .as_str()
                .cmp(right.source.as_str())
                .then_with(|| left.total_seconds.cmp(&right.total_seconds))
        });
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PlaylistRecord {
    id: PlaylistId,
    name: String,
    description: Option<String>,
    created_at: i64,
    updated_at: i64,
    entries: Vec<PlaylistEntry>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct PlaylistsDocument {
    format_version: u32,
    next_local_id: u64,
    playlists: Vec<PlaylistRecord>,
}

impl PlaylistsDocument {
    fn empty() -> Self {
        Self {
            format_version: FILE_FORMAT_VERSION,
            next_local_id: 1,
            playlists: Vec::new(),
        }
    }

    fn canonicalize(&mut self) {
        self.playlists.sort_by(|left, right| left.id.cmp(&right.id));
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct RuntimeDocument {
    format_version: u32,
    session: Option<SessionState>,
    session_updated_at: Option<i64>,
}

impl RuntimeDocument {
    fn empty() -> Self {
        Self {
            format_version: FILE_FORMAT_VERSION,
            ..Self::default()
        }
    }
}

/// Small crash-recovery record replaced by periodic playback checkpoints.
///
/// Listening time is stored as an absolute target so replay remains
/// idempotent if a process stops between the canonical progress and statistics
/// document replacements.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct PlaybackCheckpointDocument {
    format_version: u32,
    progress: Option<PlaybackProgress>,
    listen_total: Option<ListenTotal>,
}

impl PlaybackCheckpointDocument {
    fn empty() -> Self {
        Self {
            format_version: FILE_FORMAT_VERSION,
            ..Self::default()
        }
    }

    fn is_empty(&self) -> bool {
        self.progress.is_none() && self.listen_total.is_none()
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct LocalMovesDocument {
    format_version: u32,
    intents: Vec<DiskLocalMoveMapping>,
}

impl LocalMovesDocument {
    fn empty() -> Self {
        Self {
            format_version: FILE_FORMAT_VERSION,
            intents: Vec::new(),
        }
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn canonicalize(&mut self) {
        self.intents
            .sort_by(|left, right| left.source.cmp(&right.source));
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DiskLocalMoveMapping {
    source: String,
    target: String,
    created_at: i64,
}

/// Authoritative, human-editable Yandex Music desired-state ledger.
///
/// Only stable identities and desired-state metadata belong here. Credentials
/// and expiring media URLs are deliberately absent from the schema.
#[cfg(feature = "yandex-music")]
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct YandexMusicReactionsDocument {
    format_version: u32,
    reactions: Vec<YandexMusicReactionLedgerEntry>,
}

#[cfg(feature = "yandex-music")]
impl YandexMusicReactionsDocument {
    fn empty() -> Self {
        Self {
            format_version: FILE_FORMAT_VERSION,
            reactions: Vec::new(),
        }
    }

    fn canonicalize(&mut self) {
        self.reactions.sort_by(|left, right| {
            left.account_uid
                .cmp(&right.account_uid)
                .then_with(|| left.track_id.cmp(&right.track_id))
        });
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct SearchCacheDocument {
    format_version: u32,
    youtube: Option<Timed<SavedYouTubeSearch>>,
    youtube_music: Option<Timed<SavedYouTubeMusicSearch>>,
    bandcamp: Option<Timed<SavedBandcampSearch>>,
    apple_podcasts: Option<Timed<SavedApplePodcastsSearch>>,
}

impl SearchCacheDocument {
    fn empty() -> Self {
        Self {
            format_version: FILE_FORMAT_VERSION,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Timed<T> {
    updated_at: i64,
    value: T,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct ProviderCacheDocument {
    format_version: u32,
    subscription_items: Vec<CachedSubscriptionItems>,
    metadata: Vec<CachedMetadata>,
    channel_summaries: Vec<CachedChannelSummary>,
    wikidata: Vec<CachedWikidataLookup>,
}

impl ProviderCacheDocument {
    fn empty() -> Self {
        Self {
            format_version: FILE_FORMAT_VERSION,
            ..Self::default()
        }
    }

    fn canonicalize(&mut self) {
        self.subscription_items.sort_by(|left, right| {
            left.source
                .as_str()
                .cmp(right.source.as_str())
                .then_with(|| left.source_id.cmp(&right.source_id))
        });
        self.metadata
            .sort_by(|left, right| media_key(&left.media.id).cmp(&media_key(&right.media.id)));
        self.channel_summaries
            .sort_by(|left, right| left.summary.channel_id.cmp(&right.summary.channel_id));
        self.wikidata.sort_by(|left, right| {
            left.property_id
                .cmp(&right.property_id)
                .then_with(|| left.external_id.cmp(&right.external_id))
        });
    }
}

#[derive(Clone, Debug)]
struct FileDocuments {
    progress: ProgressDocument,
    history: HistoryDocument,
    notes: NotesDocument,
    bookmarks: BookmarksDocument,
    statistics: StatisticsDocument,
    local_moves: LocalMovesDocument,
    playlists: PlaylistsDocument,
    #[cfg(feature = "yandex-music")]
    yandex_music_reactions: YandexMusicReactionsDocument,
    runtime: RuntimeDocument,
    playback_checkpoint: PlaybackCheckpointDocument,
    searches: SearchCacheDocument,
    provider_cache: ProviderCacheDocument,
}

#[derive(Clone, Debug)]
struct FilePaths {
    manifest: PathBuf,
    progress: PathBuf,
    history: PathBuf,
    notes: PathBuf,
    bookmarks: PathBuf,
    statistics: PathBuf,
    local_moves: PathBuf,
    playlists: PathBuf,
    #[cfg(feature = "yandex-music")]
    yandex_music_reactions: PathBuf,
    runtime: PathBuf,
    playback_checkpoint: PathBuf,
    searches: PathBuf,
    provider_cache: PathBuf,
}

impl FilePaths {
    fn from_config(config: &Config) -> Self {
        Self {
            manifest: config.state_dir().join("manifest.toml"),
            progress: config.state_dir().join("progress.toml"),
            history: config.state_dir().join("history.toml"),
            notes: config.state_dir().join("notes.toml"),
            bookmarks: config.state_dir().join("bookmarks.toml"),
            statistics: config.state_dir().join("statistics.toml"),
            local_moves: config.state_dir().join("local-moves.toml"),
            playlists: config.state_dir().join("playlists.toml"),
            #[cfg(feature = "yandex-music")]
            yandex_music_reactions: config.state_dir().join("yandex-music.toml"),
            runtime: config.runtime_dir().join("session.toml"),
            playback_checkpoint: config.runtime_dir().join("playback-checkpoint.toml"),
            searches: config.cache_dir().join("searches.toml"),
            provider_cache: config.cache_dir().join("providers.toml"),
        }
    }
}

fn open_state_lock(config: &Config) -> Result<File, PersistenceError> {
    let path = config.state_dir().join(".lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    set_private_file_permissions(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(PersistenceError::FileStateAlreadyOpen),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

fn file_generation(path: &Path) -> Result<Option<FileGeneration>, PersistenceError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Ok(Some(FileGeneration {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        modified_seconds: metadata.mtime(),
        #[cfg(unix)]
        modified_nanoseconds: metadata.mtime_nsec(),
    }))
}

fn capture_document_generations(
    paths: &FilePaths,
) -> Result<HashMap<&'static str, Option<FileGeneration>>, PersistenceError> {
    let documents = [
        ("progress", &paths.progress),
        ("history", &paths.history),
        ("notes", &paths.notes),
        ("bookmarks", &paths.bookmarks),
        ("statistics", &paths.statistics),
        ("Local move journal", &paths.local_moves),
        ("playlists", &paths.playlists),
        ("runtime session", &paths.runtime),
        ("playback checkpoint", &paths.playback_checkpoint),
        ("search cache", &paths.searches),
        ("provider cache", &paths.provider_cache),
    ]
    .into_iter();
    #[cfg(feature = "yandex-music")]
    let documents = documents.chain(std::iter::once((
        YANDEX_MUSIC_REACTIONS_DOCUMENT,
        &paths.yandex_music_reactions,
    )));
    documents
        .map(|(document, path)| Ok((document, file_generation(path)?)))
        .collect()
}

fn validate_playback_checkpoint(
    checkpoint: &PlaybackCheckpointDocument,
) -> Result<(), PersistenceError> {
    ensure_format(checkpoint.format_version)?;
    if let Some(progress) = &checkpoint.progress {
        validate_sqlite_integer_range(
            "playback checkpoint",
            "position_seconds",
            progress.position_seconds,
        )?;
        if let Some(duration) = progress.duration_seconds {
            validate_sqlite_integer_range("playback checkpoint", "duration_seconds", duration)?;
        }
        if progress.updated_at < 0 {
            return Err(invalid_file_document(
                "playback checkpoint",
                "progress timestamp cannot be negative",
            ));
        }
        if progress.media_id.external_id.is_empty()
            || progress.media_id.external_id.chars().any(char::is_control)
        {
            return Err(invalid_file_document(
                "playback checkpoint",
                "progress media identity must be non-empty and printable",
            ));
        }
        if let Some(duration) = progress.duration_seconds
            && duration > 0
            && progress.position_seconds > duration
        {
            return Err(invalid_file_document(
                "playback checkpoint",
                "progress position cannot exceed its known duration",
            ));
        }
    }
    if let Some(total) = &checkpoint.listen_total {
        validate_sqlite_integer_range("playback checkpoint", "total_seconds", total.total_seconds)?;
        if total.total_seconds == 0 {
            return Err(invalid_file_document(
                "playback checkpoint",
                "a zero listening target must be omitted",
            ));
        }
    }
    Ok(())
}

fn merge_playback_checkpoint(
    documents: &FileDocuments,
) -> Result<(ProgressDocument, StatisticsDocument, bool, bool), PersistenceError> {
    validate_playback_checkpoint(&documents.playback_checkpoint)?;
    let mut progress = documents.progress.clone();
    let mut statistics = documents.statistics.clone();
    let mut progress_changed = false;
    let mut statistics_changed = false;

    if let Some(checkpoint) = &documents.playback_checkpoint.progress {
        if let Some(existing) = progress
            .progress
            .iter_mut()
            .find(|existing| existing.media_id == checkpoint.media_id)
        {
            if checkpoint.updated_at >= existing.updated_at && existing != checkpoint {
                *existing = checkpoint.clone();
                progress_changed = true;
            }
        } else {
            ensure_can_append("progress", progress.progress.len(), MAX_PROGRESS_ROWS)?;
            progress.progress.push(checkpoint.clone());
            progress_changed = true;
        }
    }

    if let Some(checkpoint) = &documents.playback_checkpoint.listen_total {
        if let Some(existing) = statistics
            .listen_totals
            .iter_mut()
            .find(|existing| existing.source == checkpoint.source)
        {
            if checkpoint.total_seconds > existing.total_seconds {
                existing.total_seconds = checkpoint.total_seconds;
                statistics_changed = true;
            }
        } else {
            ensure_can_append(
                "statistics",
                statistics.listen_totals.len(),
                MAX_LISTEN_TOTAL_ROWS,
            )?;
            statistics.listen_totals.push(checkpoint.clone());
            statistics_changed = true;
        }
    }

    if progress_changed {
        progress.canonicalize();
    }
    if statistics_changed {
        statistics.canonicalize();
    }
    Ok((progress, statistics, progress_changed, statistics_changed))
}

fn checked_listen_checkpoint_target(
    documents: &FileDocuments,
    source: &SourceKind,
    listened_seconds: u64,
) -> Result<Option<ListenTotal>, PersistenceError> {
    ensure_sqlite_integer_range(listened_seconds, "listen seconds")?;
    if listened_seconds == 0 {
        return Ok(None);
    }
    let canonical_total = documents
        .statistics
        .listen_totals
        .iter()
        .find(|total| total.source == *source)
        .map_or(0, |total| total.total_seconds);
    let effective_total = documents
        .playback_checkpoint
        .listen_total
        .as_ref()
        .filter(|total| total.source == *source)
        .map_or(canonical_total, |total| {
            total.total_seconds.max(canonical_total)
        });
    let total_seconds = effective_total.checked_add(listened_seconds).ok_or(
        PersistenceError::IntegerOutOfRange {
            field: "listen seconds",
        },
    )?;
    ensure_sqlite_integer_range(total_seconds, "listen seconds")?;
    Ok(Some(ListenTotal {
        source: source.clone(),
        total_seconds,
    }))
}

/// Replays an interrupted checkpoint before the store begins serving reads.
///
/// Canonical documents are replaced before the checkpoint is cleared. Because
/// progress uses a latest-timestamp merge and statistics stores an absolute
/// target, repeating this sequence after a crash cannot double-count time.
fn recover_playback_checkpoint_files(
    paths: &FilePaths,
    documents: &mut FileDocuments,
) -> Result<(), PersistenceError> {
    if documents.playback_checkpoint.is_empty() {
        return Ok(());
    }
    let (progress, statistics, progress_changed, statistics_changed) =
        merge_playback_checkpoint(documents)?;
    if progress_changed {
        write_document(
            &paths.progress,
            "progress",
            MAX_PROGRESS_DOCUMENT_BYTES,
            &progress,
        )?;
    }
    if statistics_changed {
        write_document(
            &paths.statistics,
            "statistics",
            MAX_STATISTICS_DOCUMENT_BYTES,
            &statistics,
        )?;
    }
    let empty = PlaybackCheckpointDocument::empty();
    write_document(
        &paths.playback_checkpoint,
        "playback checkpoint",
        MAX_PLAYBACK_CHECKPOINT_DOCUMENT_BYTES,
        &empty,
    )?;
    documents.progress = progress;
    documents.statistics = statistics;
    documents.playback_checkpoint = empty;
    Ok(())
}

/// TOML-backed implementation of Youta's complete persistence boundary.
pub(super) struct FileStateStore {
    paths: Option<FilePaths>,
    documents: Mutex<FileDocuments>,
    generations: Mutex<HashMap<&'static str, Option<FileGeneration>>>,
    publication_blocked: AtomicBool,
    state_lock: Option<File>,
}

impl Drop for FileStateStore {
    fn drop(&mut self) {
        // Explicitly unlock before closing so a helper that inherited a
        // duplicated descriptor cannot keep a completed Youta session locked.
        if let Some(lock) = self.state_lock.take() {
            let _ = lock.unlock();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileGeneration {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    modified_seconds: i64,
    #[cfg(unix)]
    modified_nanoseconds: i64,
}

impl FileStateStore {
    /// Opens or initializes deterministic files rooted at `config`.
    ///
    /// Invalid restart-only and cache documents are quarantined beside their
    /// canonical paths and reset. Invalid authoritative documents fail open
    /// without being changed.
    pub(super) fn open(config: &Config) -> Result<Self, PersistenceError> {
        for directory in [config.state_dir(), config.runtime_dir(), config.cache_dir()] {
            create_private_directory(&directory)?;
        }
        let paths = FilePaths::from_config(config);
        let lock = open_state_lock(config)?;
        let initialized = paths.manifest.try_exists()?;
        let manifest = if initialized {
            load_required(&paths.manifest, "manifest", MAX_MANIFEST_DOCUMENT_BYTES)?
        } else {
            ManifestDocument::default()
        };
        ensure_format(manifest.format_version)?;
        if manifest.backend != "files" {
            return Err(PersistenceError::InvalidSavedSearch {
                reason: format!(
                    "state manifest backend must be `files`, found {:?}",
                    manifest.backend
                ),
            });
        }
        let mut documents = FileDocuments {
            progress: load_authoritative(
                &paths.progress,
                "progress",
                MAX_PROGRESS_DOCUMENT_BYTES,
                initialized,
                ProgressDocument::empty,
            )?,
            history: load_authoritative(
                &paths.history,
                "history",
                MAX_HISTORY_DOCUMENT_BYTES,
                initialized,
                HistoryDocument::empty,
            )?,
            notes: load_authoritative(
                &paths.notes,
                "notes",
                MAX_NOTES_DOCUMENT_BYTES,
                initialized,
                NotesDocument::empty,
            )?,
            bookmarks: load_authoritative(
                &paths.bookmarks,
                "bookmarks",
                MAX_BOOKMARKS_DOCUMENT_BYTES,
                initialized,
                BookmarksDocument::empty,
            )?,
            statistics: load_authoritative(
                &paths.statistics,
                "statistics",
                MAX_STATISTICS_DOCUMENT_BYTES,
                initialized,
                StatisticsDocument::empty,
            )?,
            local_moves: load_authoritative(
                &paths.local_moves,
                "Local move journal",
                MAX_LOCAL_MOVES_DOCUMENT_BYTES,
                initialized,
                LocalMovesDocument::empty,
            )?,
            playlists: load_authoritative(
                &paths.playlists,
                "playlists",
                MAX_PLAYLISTS_DOCUMENT_BYTES,
                initialized,
                PlaylistsDocument::empty,
            )?,
            #[cfg(feature = "yandex-music")]
            yandex_music_reactions: load_or_default(
                &paths.yandex_music_reactions,
                YANDEX_MUSIC_REACTIONS_DOCUMENT,
                MAX_YANDEX_MUSIC_REACTIONS_DOCUMENT_BYTES,
                YandexMusicReactionsDocument::empty,
            )?,
            runtime: load_regenerable(
                &paths.runtime,
                "runtime session",
                MAX_RUNTIME_DOCUMENT_BYTES,
                RuntimeDocument::empty,
                |document| ensure_format(document.format_version),
            )?,
            playback_checkpoint: load_regenerable(
                &paths.playback_checkpoint,
                "playback checkpoint",
                MAX_PLAYBACK_CHECKPOINT_DOCUMENT_BYTES,
                PlaybackCheckpointDocument::empty,
                validate_playback_checkpoint,
            )?,
            searches: load_regenerable(
                &paths.searches,
                "search cache",
                MAX_SEARCH_CACHE_DOCUMENT_BYTES,
                SearchCacheDocument::empty,
                |document| {
                    ensure_format(document.format_version)?;
                    validate_search_cache_document(document)
                },
            )?,
            provider_cache: load_regenerable(
                &paths.provider_cache,
                "provider cache",
                MAX_PROVIDER_CACHE_DOCUMENT_BYTES,
                ProviderCacheDocument::empty,
                |document| {
                    ensure_format(document.format_version)?;
                    validate_provider_cache_document(document)
                },
            )?,
        };
        validate_file_documents(&documents)?;
        if !initialized {
            write_document(
                &paths.progress,
                "progress",
                MAX_PROGRESS_DOCUMENT_BYTES,
                &documents.progress,
            )?;
            write_document(
                &paths.history,
                "history",
                MAX_HISTORY_DOCUMENT_BYTES,
                &documents.history,
            )?;
            write_document(
                &paths.notes,
                "notes",
                MAX_NOTES_DOCUMENT_BYTES,
                &documents.notes,
            )?;
            write_document(
                &paths.bookmarks,
                "bookmarks",
                MAX_BOOKMARKS_DOCUMENT_BYTES,
                &documents.bookmarks,
            )?;
            write_document(
                &paths.statistics,
                "statistics",
                MAX_STATISTICS_DOCUMENT_BYTES,
                &documents.statistics,
            )?;
            write_document(
                &paths.local_moves,
                "Local move journal",
                MAX_LOCAL_MOVES_DOCUMENT_BYTES,
                &documents.local_moves,
            )?;
            write_document(
                &paths.playlists,
                "playlists",
                MAX_PLAYLISTS_DOCUMENT_BYTES,
                &documents.playlists,
            )?;
            #[cfg(feature = "yandex-music")]
            write_document(
                &paths.yandex_music_reactions,
                YANDEX_MUSIC_REACTIONS_DOCUMENT,
                MAX_YANDEX_MUSIC_REACTIONS_DOCUMENT_BYTES,
                &documents.yandex_music_reactions,
            )?;
            write_document(
                &paths.runtime,
                "runtime session",
                MAX_RUNTIME_DOCUMENT_BYTES,
                &documents.runtime,
            )?;
            write_document(
                &paths.playback_checkpoint,
                "playback checkpoint",
                MAX_PLAYBACK_CHECKPOINT_DOCUMENT_BYTES,
                &documents.playback_checkpoint,
            )?;
            write_document(
                &paths.searches,
                "search cache",
                MAX_SEARCH_CACHE_DOCUMENT_BYTES,
                &documents.searches,
            )?;
            write_document(
                &paths.provider_cache,
                "provider cache",
                MAX_PROVIDER_CACHE_DOCUMENT_BYTES,
                &documents.provider_cache,
            )?;
            // Publish the manifest last so an interrupted first open is
            // retried as initialization rather than accepted as complete.
            write_document(
                &paths.manifest,
                "manifest",
                MAX_MANIFEST_DOCUMENT_BYTES,
                &manifest,
            )?;
        }
        recover_playback_checkpoint_files(&paths, &mut documents)?;
        let generations = capture_document_generations(&paths)?;
        Ok(Self {
            paths: Some(paths),
            documents: Mutex::new(documents),
            generations: Mutex::new(generations),
            publication_blocked: AtomicBool::new(false),
            state_lock: Some(lock),
        })
    }

    /// Creates an in-memory file-backend store.
    pub(super) fn open_in_memory() -> Result<Self, PersistenceError> {
        Ok(Self {
            paths: None,
            documents: Mutex::new(FileDocuments {
                progress: ProgressDocument::empty(),
                history: HistoryDocument::empty(),
                notes: NotesDocument::empty(),
                bookmarks: BookmarksDocument::empty(),
                statistics: StatisticsDocument::empty(),
                local_moves: LocalMovesDocument::empty(),
                playlists: PlaylistsDocument::empty(),
                #[cfg(feature = "yandex-music")]
                yandex_music_reactions: YandexMusicReactionsDocument::empty(),
                runtime: RuntimeDocument::empty(),
                playback_checkpoint: PlaybackCheckpointDocument::empty(),
                searches: SearchCacheDocument::empty(),
                provider_cache: ProviderCacheDocument::empty(),
            }),
            generations: Mutex::new(HashMap::new()),
            publication_blocked: AtomicBool::new(false),
            state_lock: None,
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, FileDocuments>, PersistenceError> {
        self.documents
            .lock()
            .map_err(|_| PersistenceError::StateLockPoisoned)
    }

    fn ensure_writes_allowed(&self) -> Result<(), PersistenceError> {
        if self.publication_blocked.load(Ordering::Acquire) {
            return Err(PersistenceError::PartialFilePublicationRequiresRestart);
        }
        Ok(())
    }

    fn persist_document<T: Serialize>(
        &self,
        path: &Path,
        document: &'static str,
        maximum_bytes: usize,
        value: &T,
    ) -> Result<(), PersistenceError> {
        self.ensure_writes_allowed()?;
        let current = file_generation(path)?;
        {
            let generations = self
                .generations
                .lock()
                .map_err(|_| PersistenceError::StateLockPoisoned)?;
            if generations.get(document).copied().flatten() != current {
                return Err(PersistenceError::StateDocumentChangedExternally { document });
            }
        }
        write_document(path, document, maximum_bytes, value)?;
        let written = file_generation(path)?;
        self.generations
            .lock()
            .map_err(|_| PersistenceError::StateLockPoisoned)?
            .insert(document, written);
        Ok(())
    }

    fn persist_progress(&self, document: &ProgressDocument) -> Result<(), PersistenceError> {
        if let Some(paths) = &self.paths {
            self.persist_document(
                &paths.progress,
                "progress",
                MAX_PROGRESS_DOCUMENT_BYTES,
                document,
            )?;
        }
        Ok(())
    }

    fn persist_history(&self, document: &HistoryDocument) -> Result<(), PersistenceError> {
        if let Some(paths) = &self.paths {
            self.persist_document(
                &paths.history,
                "history",
                MAX_HISTORY_DOCUMENT_BYTES,
                document,
            )?;
        }
        Ok(())
    }

    fn persist_notes(&self, document: &NotesDocument) -> Result<(), PersistenceError> {
        if let Some(paths) = &self.paths {
            self.persist_document(&paths.notes, "notes", MAX_NOTES_DOCUMENT_BYTES, document)?;
        }
        Ok(())
    }

    fn persist_bookmarks(&self, document: &BookmarksDocument) -> Result<(), PersistenceError> {
        if let Some(paths) = &self.paths {
            self.persist_document(
                &paths.bookmarks,
                "bookmarks",
                MAX_BOOKMARKS_DOCUMENT_BYTES,
                document,
            )?;
        }
        Ok(())
    }

    fn persist_statistics(&self, document: &StatisticsDocument) -> Result<(), PersistenceError> {
        if let Some(paths) = &self.paths {
            self.persist_document(
                &paths.statistics,
                "statistics",
                MAX_STATISTICS_DOCUMENT_BYTES,
                document,
            )?;
        }
        Ok(())
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn persist_local_moves(&self, document: &LocalMovesDocument) -> Result<(), PersistenceError> {
        if let Some(paths) = &self.paths {
            self.persist_document(
                &paths.local_moves,
                "Local move journal",
                MAX_LOCAL_MOVES_DOCUMENT_BYTES,
                document,
            )?;
        }
        Ok(())
    }

    fn persist_playlists(&self, document: &PlaylistsDocument) -> Result<(), PersistenceError> {
        if let Some(paths) = &self.paths {
            self.persist_document(
                &paths.playlists,
                "playlists",
                MAX_PLAYLISTS_DOCUMENT_BYTES,
                document,
            )?;
        }
        Ok(())
    }

    #[cfg(feature = "yandex-music")]
    fn persist_yandex_music_reactions(
        &self,
        document: &YandexMusicReactionsDocument,
    ) -> Result<(), PersistenceError> {
        if let Some(paths) = &self.paths {
            self.persist_document(
                &paths.yandex_music_reactions,
                YANDEX_MUSIC_REACTIONS_DOCUMENT,
                MAX_YANDEX_MUSIC_REACTIONS_DOCUMENT_BYTES,
                document,
            )?;
        }
        Ok(())
    }

    fn persist_runtime(&self, document: &RuntimeDocument) -> Result<(), PersistenceError> {
        if let Some(paths) = &self.paths {
            self.persist_document(
                &paths.runtime,
                "runtime session",
                MAX_RUNTIME_DOCUMENT_BYTES,
                document,
            )?;
        }
        Ok(())
    }

    fn persist_playback_checkpoint(
        &self,
        document: &PlaybackCheckpointDocument,
    ) -> Result<(), PersistenceError> {
        if let Some(paths) = &self.paths {
            self.persist_document(
                &paths.playback_checkpoint,
                "playback checkpoint",
                MAX_PLAYBACK_CHECKPOINT_DOCUMENT_BYTES,
                document,
            )?;
        }
        Ok(())
    }

    fn persist_searches(&self, document: &SearchCacheDocument) -> Result<(), PersistenceError> {
        if let Some(paths) = &self.paths {
            self.persist_document(
                &paths.searches,
                "search cache",
                MAX_SEARCH_CACHE_DOCUMENT_BYTES,
                document,
            )?;
        }
        Ok(())
    }

    fn persist_provider_cache(
        &self,
        document: &ProviderCacheDocument,
    ) -> Result<(), PersistenceError> {
        if let Some(paths) = &self.paths {
            self.persist_document(
                &paths.provider_cache,
                "provider cache",
                MAX_PROVIDER_CACHE_DOCUMENT_BYTES,
                document,
            )?;
        }
        Ok(())
    }

    /// Publishes one pending checkpoint into the human-readable state files.
    ///
    /// The checkpoint is cleared last. A failure after either canonical
    /// document replacement therefore leaves enough absolute state for an
    /// idempotent retry during this process or the next startup.
    fn flush_playback_checkpoint_locked(
        &self,
        documents: &mut FileDocuments,
    ) -> Result<(), PersistenceError> {
        if documents.playback_checkpoint.is_empty() {
            return Ok(());
        }
        let (progress, statistics, progress_changed, statistics_changed) =
            merge_playback_checkpoint(documents)?;
        if progress_changed {
            self.persist_progress(&progress)?;
            documents.progress = progress;
        }
        if statistics_changed {
            self.persist_statistics(&statistics)?;
            documents.statistics = statistics;
        }
        let empty = PlaybackCheckpointDocument::empty();
        self.persist_playback_checkpoint(&empty)?;
        documents.playback_checkpoint = empty;
        Ok(())
    }
}

impl StateBackend for FileStateStore {
    fn backend_name(&self) -> &'static str {
        "files"
    }

    fn format_version(&self) -> Result<u32, PersistenceError> {
        let documents = self.lock()?;
        Ok(documents.progress.format_version)
    }

    fn create_playlist(
        &self,
        name: &str,
        description: Option<&str>,
        created_at: i64,
    ) -> Result<PlaylistCreateOutcome, PersistenceError> {
        self.create_playlist_inner(name, description, None, created_at)
    }

    fn create_playlist_with_entry(
        &self,
        name: &str,
        description: Option<&str>,
        media: &PlaylistMediaSnapshot,
        created_at: i64,
    ) -> Result<PlaylistCreateOutcome, PersistenceError> {
        let _ = encoded_playlist_snapshot(media)?;
        self.create_playlist_inner(name, description, Some(media), created_at)
    }

    fn update_playlist(
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
        let mut documents = self.lock()?;
        let mut next = documents.playlists.clone();
        if next.playlists.iter().any(|playlist| {
            playlist.id != playlist_id && playlist_name_key(&playlist.name) == name_key
        }) {
            return Err(invalid_playlist(
                "playlist name conflicts with an existing playlist",
            ));
        }
        let summary = {
            let Some(playlist) = next
                .playlists
                .iter_mut()
                .find(|playlist| playlist.id == playlist_id)
            else {
                return Ok(None);
            };
            playlist.name = name.to_owned();
            playlist.description = description.map(str::to_owned);
            playlist.updated_at = updated_at;
            summary_from_record(playlist)
        };
        next.canonicalize();
        self.persist_playlists(&next)?;
        documents.playlists = next;
        Ok(Some(summary))
    }

    fn playlists(&self) -> Result<Vec<PlaylistSummary>, PersistenceError> {
        let documents = self.lock()?;
        if documents.playlists.playlists.len() > MAX_PLAYLISTS {
            return Err(invalid_playlist("playlist count exceeds its fixed limit"));
        }
        let mut summaries = documents
            .playlists
            .playlists
            .iter()
            .filter(|playlist| !is_hidden_builtin_playlist_id(&playlist.id))
            .map(summary_from_record)
            .collect::<Vec<_>>();
        summaries.sort_by(|left, right| {
            playlist_name_key(&left.name)
                .cmp(&playlist_name_key(&right.name))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(summaries)
    }

    fn playlist(&self, playlist_id: &str) -> Result<Option<Playlist>, PersistenceError> {
        validate_playlist_id(playlist_id)?;
        let documents = self.lock()?;
        documents
            .playlists
            .playlists
            .iter()
            .find(|playlist| playlist.id == playlist_id)
            .map(record_to_playlist)
            .transpose()
    }

    fn playlist_contains(
        &self,
        playlist_id: &str,
        media_id: &MediaId,
    ) -> Result<bool, PersistenceError> {
        validate_playlist_id(playlist_id)?;
        validate_playlist_media_id(media_id)?;
        let documents = self.lock()?;
        Ok(documents
            .playlists
            .playlists
            .iter()
            .find(|playlist| playlist.id == playlist_id)
            .is_some_and(|playlist| contains_whole_media(playlist, media_id)))
    }

    fn playlist_memberships(
        &self,
        media_id: &MediaId,
    ) -> Result<Vec<PlaylistId>, PersistenceError> {
        validate_playlist_media_id(media_id)?;
        let documents = self.lock()?;
        let mut ids = documents
            .playlists
            .playlists
            .iter()
            .filter(|playlist| contains_whole_media(playlist, media_id))
            .filter(|playlist| !is_hidden_builtin_playlist_id(&playlist.id))
            .map(|playlist| playlist.id.clone())
            .collect::<Vec<_>>();
        ids.sort();
        Ok(ids)
    }

    fn playlist_memberships_with_names(
        &self,
        media_id: &MediaId,
    ) -> Result<Vec<PlaylistMembership>, PersistenceError> {
        validate_playlist_media_id(media_id)?;
        let documents = self.lock()?;
        let mut memberships = documents
            .playlists
            .playlists
            .iter()
            .filter(|playlist| contains_whole_media(playlist, media_id))
            .filter(|playlist| !is_hidden_builtin_playlist_id(&playlist.id))
            .map(|playlist| PlaylistMembership {
                playlist_id: playlist.id.clone(),
                playlist_name: playlist.name.clone(),
            })
            .collect::<Vec<_>>();
        memberships.sort_by(|left, right| {
            playlist_name_key(&left.playlist_name)
                .cmp(&playlist_name_key(&right.playlist_name))
                .then_with(|| left.playlist_id.cmp(&right.playlist_id))
        });
        Ok(memberships)
    }

    fn add_playlist_entry(
        &self,
        playlist_id: &str,
        media: &PlaylistMediaSnapshot,
        added_at: i64,
    ) -> Result<PlaylistAddOutcome, PersistenceError> {
        self.mutate_playlist_entry(playlist_id, media, added_at, EntryMutation::Add)
            .map(|outcome| match outcome {
                PlaylistToggleOutcome::Added => PlaylistAddOutcome::Added,
                PlaylistToggleOutcome::Removed => PlaylistAddOutcome::AlreadyPresent,
            })
    }

    fn remove_playlist_entry(
        &self,
        playlist_id: &str,
        media_id: &MediaId,
        updated_at: i64,
    ) -> Result<bool, PersistenceError> {
        validate_playlist_id(playlist_id)?;
        validate_playlist_media_id(media_id)?;
        validate_playlist_timestamp(updated_at)?;
        let mut documents = self.lock()?;
        let mut next = documents.playlists.clone();
        let Some(playlist) = next
            .playlists
            .iter_mut()
            .find(|playlist| playlist.id == playlist_id)
        else {
            return Ok(false);
        };
        let before = playlist.entries.len();
        playlist
            .entries
            .retain(|entry| entry.segment.is_some() || entry.media.id != *media_id);
        let removed = before != playlist.entries.len();
        if removed {
            playlist.updated_at = updated_at;
            self.persist_playlists(&next)?;
            documents.playlists = next;
        }
        Ok(removed)
    }

    fn toggle_playlist_entry(
        &self,
        playlist_id: &str,
        media: &PlaylistMediaSnapshot,
        updated_at: i64,
    ) -> Result<PlaylistToggleOutcome, PersistenceError> {
        self.mutate_playlist_entry(playlist_id, media, updated_at, EntryMutation::Toggle)
    }

    fn add_to_todo(
        &self,
        media: &PlaylistMediaSnapshot,
        added_at: i64,
    ) -> Result<PlaylistAddOutcome, PersistenceError> {
        self.ensure_todo(added_at)?;
        self.add_playlist_entry(TODO_PLAYLIST_ID, media, added_at)
    }

    fn toggle_todo(
        &self,
        media: &PlaylistMediaSnapshot,
        updated_at: i64,
    ) -> Result<PlaylistToggleOutcome, PersistenceError> {
        if self.todo_contains(&media.id)? {
            self.remove_playlist_entry(TODO_PLAYLIST_ID, &media.id, updated_at)?;
            return Ok(PlaylistToggleOutcome::Removed);
        }
        self.ensure_todo(updated_at)?;
        self.mutate_playlist_entry(TODO_PLAYLIST_ID, media, updated_at, EntryMutation::Add)
    }

    fn todo_contains(&self, media_id: &MediaId) -> Result<bool, PersistenceError> {
        self.playlist_contains(TODO_PLAYLIST_ID, media_id)
    }

    fn toggle_radio_favorite(
        &self,
        media: &PlaylistMediaSnapshot,
        updated_at: i64,
    ) -> Result<PlaylistToggleOutcome, PersistenceError> {
        validate_radio_favorite_snapshot(media)?;
        if self.radio_favorite_contains(&media.id)? {
            self.remove_playlist_entry(RADIO_FAVORITES_PLAYLIST_ID, &media.id, updated_at)?;
            return Ok(PlaylistToggleOutcome::Removed);
        }
        self.ensure_radio_favorites(updated_at)?;
        self.mutate_playlist_entry(
            RADIO_FAVORITES_PLAYLIST_ID,
            media,
            updated_at,
            EntryMutation::Add,
        )
    }

    fn radio_favorite_contains(&self, media_id: &MediaId) -> Result<bool, PersistenceError> {
        if media_id.source != SourceKind::Radio {
            return Ok(false);
        }
        self.playlist_contains(RADIO_FAVORITES_PLAYLIST_ID, media_id)
    }

    #[cfg(feature = "yandex-music")]
    fn pending_yandex_music_reactions(
        &self,
    ) -> Result<Vec<PendingYandexMusicReaction>, PersistenceError> {
        let documents = self.lock()?;
        validate_yandex_music_reactions_document(&documents.yandex_music_reactions)?;
        let mut reactions = documents
            .yandex_music_reactions
            .reactions
            .iter()
            .filter_map(YandexMusicReactionLedgerEntry::pending_intent)
            .collect::<Vec<_>>();
        reactions.sort_by(|left, right| {
            left.account_uid
                .cmp(&right.account_uid)
                .then_with(|| left.track_id.cmp(&right.track_id))
        });
        Ok(reactions)
    }

    #[cfg(feature = "yandex-music")]
    fn queue_yandex_music_reaction(
        &self,
        account_uid: &str,
        track_id: &str,
        reaction: YandexMusicReaction,
        updated_at: i64,
    ) -> Result<PendingYandexMusicReaction, PersistenceError> {
        validate_yandex_music_reaction_identity(account_uid, track_id)?;
        validate_yandex_music_reaction_timestamp(updated_at)?;
        let mut documents = self.lock()?;
        let mut next = documents.yandex_music_reactions.clone();
        if let Some(existing) = next
            .reactions
            .iter_mut()
            .find(|existing| existing.account_uid == account_uid && existing.track_id == track_id)
        {
            existing.generation = next_yandex_music_reaction_generation(existing.generation)?;
            existing.reaction = reaction;
            existing.updated_at = updated_at;
        } else {
            ensure_can_append(
                YANDEX_MUSIC_REACTIONS_DOCUMENT,
                next.reactions.len(),
                MAX_YANDEX_MUSIC_REACTION_ROWS,
            )?;
            next.reactions.push(YandexMusicReactionLedgerEntry {
                account_uid: account_uid.to_owned(),
                track_id: track_id.to_owned(),
                reaction,
                generation: 1,
                acknowledged_generation: 0,
                updated_at,
            });
        }
        next.canonicalize();
        let pending = next
            .reactions
            .iter()
            .find(|entry| entry.account_uid == account_uid && entry.track_id == track_id)
            .and_then(YandexMusicReactionLedgerEntry::pending_intent)
            .expect("a newly queued reaction ledger entry must be pending");
        self.persist_yandex_music_reactions(&next)?;
        documents.yandex_music_reactions = next;
        Ok(pending)
    }

    #[cfg(feature = "yandex-music")]
    fn acknowledge_yandex_music_reaction(
        &self,
        account_uid: &str,
        track_id: &str,
        generation: u64,
    ) -> Result<bool, PersistenceError> {
        validate_yandex_music_reaction_identity(account_uid, track_id)?;
        validate_yandex_music_reaction_generation(generation)?;
        let mut documents = self.lock()?;
        let mut next = documents.yandex_music_reactions.clone();
        let Some(entry) = next.reactions.iter_mut().find(|entry| {
            entry.account_uid == account_uid
                && entry.track_id == track_id
                && entry.generation == generation
                && entry.acknowledged_generation < generation
        }) else {
            return Ok(false);
        };
        entry.acknowledged_generation = generation;
        next.canonicalize();
        self.persist_yandex_music_reactions(&next)?;
        documents.yandex_music_reactions = next;
        Ok(true)
    }

    fn checkpoint_playback(
        &self,
        progress: &PlaybackProgress,
        listen_source: &SourceKind,
        listened_seconds: u64,
    ) -> Result<(), PersistenceError> {
        validate_progress_numeric_range(progress)?;
        let mut documents = self.lock()?;
        let listen_target =
            checked_listen_checkpoint_target(&documents, listen_source, listened_seconds)?;
        let changes_media = documents
            .playback_checkpoint
            .progress
            .as_ref()
            .is_some_and(|pending| pending.media_id != progress.media_id);
        let changes_source = listened_seconds > 0
            && documents
                .playback_checkpoint
                .listen_total
                .as_ref()
                .is_some_and(|pending| pending.source != *listen_source);
        if changes_media || changes_source {
            self.flush_playback_checkpoint_locked(&mut documents)?;
        }

        let mut next = documents.playback_checkpoint.clone();
        if next
            .progress
            .as_ref()
            .is_none_or(|pending| progress.updated_at >= pending.updated_at)
        {
            next.progress = Some(progress.clone());
        }
        if let Some(listen_target) = listen_target {
            next.listen_total = Some(listen_target);
        }
        validate_playback_checkpoint(&next)?;
        self.persist_playback_checkpoint(&next)?;
        documents.playback_checkpoint = next;
        Ok(())
    }

    fn checkpoint_listening(
        &self,
        source: &SourceKind,
        listened_seconds: u64,
    ) -> Result<(), PersistenceError> {
        ensure_sqlite_integer_range(listened_seconds, "listen seconds")?;
        if listened_seconds == 0 {
            return Ok(());
        }
        let mut documents = self.lock()?;
        let Some(listen_target) =
            checked_listen_checkpoint_target(&documents, source, listened_seconds)?
        else {
            return Ok(());
        };
        let changes_source = documents
            .playback_checkpoint
            .listen_total
            .as_ref()
            .is_some_and(|pending| pending.source != *source);
        if changes_source {
            self.flush_playback_checkpoint_locked(&mut documents)?;
        }

        let mut next = documents.playback_checkpoint.clone();
        next.listen_total = Some(listen_target);
        validate_playback_checkpoint(&next)?;
        self.persist_playback_checkpoint(&next)?;
        documents.playback_checkpoint = next;
        Ok(())
    }

    fn flush_playback_checkpoint(&self) -> Result<(), PersistenceError> {
        let mut documents = self.lock()?;
        self.flush_playback_checkpoint_locked(&mut documents)
    }

    fn upsert_progress(&self, progress: &PlaybackProgress) -> Result<(), PersistenceError> {
        validate_progress_numeric_range(progress)?;
        let mut documents = self.lock()?;
        self.flush_playback_checkpoint_locked(&mut documents)?;
        let mut next = documents.progress.clone();
        if let Some(existing) = next
            .progress
            .iter_mut()
            .find(|existing| existing.media_id == progress.media_id)
        {
            *existing = progress.clone();
        } else {
            ensure_can_append("progress", next.progress.len(), MAX_PROGRESS_ROWS)?;
            next.progress.push(progress.clone());
        }
        next.canonicalize();
        self.persist_progress(&next)?;
        documents.progress = next;
        Ok(())
    }

    fn progress(&self, media_id: &MediaId) -> Result<Option<PlaybackProgress>, PersistenceError> {
        let documents = self.lock()?;
        let canonical = documents
            .progress
            .progress
            .iter()
            .find(|progress| progress.media_id == *media_id)
            .cloned();
        let checkpoint = documents
            .playback_checkpoint
            .progress
            .as_ref()
            .filter(|progress| progress.media_id == *media_id)
            .cloned();
        Ok(match (canonical, checkpoint) {
            (Some(canonical), Some(checkpoint))
                if checkpoint.updated_at >= canonical.updated_at =>
            {
                Some(checkpoint)
            }
            (Some(canonical), _) => Some(canonical),
            (None, checkpoint) => checkpoint,
        })
    }

    fn progress_for_media_ids(
        &self,
        media_ids: &[MediaId],
    ) -> Result<HashMap<MediaId, PlaybackProgress>, PersistenceError> {
        let requested = media_ids.iter().collect::<HashSet<_>>();
        let documents = self.lock()?;
        let mut progress = documents
            .progress
            .progress
            .iter()
            .filter(|progress| requested.contains(&progress.media_id))
            .map(|progress| (progress.media_id.clone(), progress.clone()))
            .collect::<HashMap<_, _>>();
        if let Some(checkpoint) = documents
            .playback_checkpoint
            .progress
            .as_ref()
            .filter(|checkpoint| requested.contains(&checkpoint.media_id))
        {
            let replace = progress
                .get(&checkpoint.media_id)
                .is_none_or(|canonical| checkpoint.updated_at >= canonical.updated_at);
            if replace {
                progress.insert(checkpoint.media_id.clone(), checkpoint.clone());
            }
        }
        Ok(progress)
    }

    fn delete_progress(&self, media_id: &MediaId) -> Result<bool, PersistenceError> {
        let mut documents = self.lock()?;
        let checkpoint_removed = documents
            .playback_checkpoint
            .progress
            .as_ref()
            .is_some_and(|progress| progress.media_id == *media_id);
        if checkpoint_removed {
            let mut next_checkpoint = documents.playback_checkpoint.clone();
            next_checkpoint.progress = None;
            self.persist_playback_checkpoint(&next_checkpoint)?;
            documents.playback_checkpoint = next_checkpoint;
        }
        let mut next = documents.progress.clone();
        let before = next.progress.len();
        next.progress
            .retain(|progress| progress.media_id != *media_id);
        let removed = next.progress.len() != before;
        if removed {
            self.persist_progress(&next)?;
            documents.progress = next;
        }
        Ok(removed || checkpoint_removed)
    }

    fn insert_history(&self, entry: &HistoryEntry) -> Result<i64, PersistenceError> {
        validate_history_entry(entry)?;
        let mut documents = self.lock()?;
        let mut next = documents.history.clone();
        ensure_can_append("history", next.history.len(), MAX_HISTORY_ROWS)?;
        let id = allocate_id(&mut next.next_id)?;
        let mut stored = entry.clone();
        stored.id = id;
        next.history.push(stored);
        next.canonicalize();
        self.persist_history(&next)?;
        documents.history = next;
        Ok(id)
    }

    fn update_history(&self, entry: &HistoryEntry) -> Result<bool, PersistenceError> {
        validate_history_entry(entry)?;
        let mut documents = self.lock()?;
        let mut next = documents.history.clone();
        let Some(existing) = next.history.iter_mut().find(|row| row.id == entry.id) else {
            return Ok(false);
        };
        *existing = entry.clone();
        self.persist_history(&next)?;
        documents.history = next;
        Ok(true)
    }

    fn history(
        &self,
        finished_only: bool,
        limit: usize,
    ) -> Result<Vec<HistoryEntry>, PersistenceError> {
        let documents = self.lock()?;
        let mut entries = documents
            .history
            .history
            .iter()
            .filter(|entry| !finished_only || entry.finished)
            .cloned()
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            right
                .last_played_at
                .cmp(&left.last_played_at)
                .then_with(|| right.id.cmp(&left.id))
        });
        entries.truncate(limit);
        Ok(entries)
    }

    fn delete_history(&self, id: i64) -> Result<bool, PersistenceError> {
        let mut documents = self.lock()?;
        let mut next = documents.history.clone();
        let before = next.history.len();
        next.history.retain(|entry| entry.id != id);
        let removed = before != next.history.len();
        if removed {
            self.persist_history(&next)?;
            documents.history = next;
        }
        Ok(removed)
    }

    fn insert_private_comment(&self, comment: &PrivateComment) -> Result<i64, PersistenceError> {
        let mut documents = self.lock()?;
        let mut next = documents.notes.clone();
        ensure_can_append("notes", next.comments.len(), MAX_PRIVATE_COMMENT_ROWS)?;
        let id = allocate_id(&mut next.next_id)?;
        let mut stored = comment.clone();
        stored.id = id;
        next.comments.push(stored);
        next.canonicalize();
        self.persist_notes(&next)?;
        documents.notes = next;
        Ok(id)
    }

    fn update_private_comment(&self, comment: &PrivateComment) -> Result<bool, PersistenceError> {
        let mut documents = self.lock()?;
        let mut next = documents.notes.clone();
        let Some(existing) = next.comments.iter_mut().find(|row| row.id == comment.id) else {
            return Ok(false);
        };
        *existing = comment.clone();
        self.persist_notes(&next)?;
        documents.notes = next;
        Ok(true)
    }

    fn private_comments(
        &self,
        target: &CommentTarget,
    ) -> Result<Vec<PrivateComment>, PersistenceError> {
        let mut comments = self
            .lock()?
            .notes
            .comments
            .iter()
            .filter(|comment| comment.target == *target)
            .cloned()
            .collect::<Vec<_>>();
        comments.sort_by_key(|comment| (comment.created_at, comment.id));
        Ok(comments)
    }

    fn private_note(
        &self,
        target: &CommentTarget,
    ) -> Result<Option<PrivateComment>, PersistenceError> {
        Ok(self
            .lock()?
            .notes
            .comments
            .iter()
            .filter(|comment| comment.target == *target)
            .max_by_key(|comment| (comment.updated_at, comment.id))
            .cloned())
    }

    fn upsert_private_note(
        &self,
        target: &CommentTarget,
        body: &str,
        updated_at: i64,
    ) -> Result<PrivateComment, PersistenceError> {
        let mut documents = self.lock()?;
        let mut next = documents.notes.clone();
        let existing = next
            .comments
            .iter()
            .filter(|comment| comment.target == *target)
            .max_by_key(|comment| (comment.updated_at, comment.id))
            .cloned();
        let stored = if let Some(existing) = existing {
            PrivateComment {
                body: body.to_owned(),
                updated_at,
                ..existing
            }
        } else {
            ensure_can_append("notes", next.comments.len(), MAX_PRIVATE_COMMENT_ROWS)?;
            PrivateComment {
                id: allocate_id(&mut next.next_id)?,
                target: target.clone(),
                body: body.to_owned(),
                created_at: updated_at,
                updated_at,
            }
        };
        next.comments
            .retain(|comment| comment.target != *target || comment.id == stored.id);
        if let Some(existing) = next
            .comments
            .iter_mut()
            .find(|comment| comment.id == stored.id)
        {
            *existing = stored.clone();
        } else {
            next.comments.push(stored.clone());
        }
        next.canonicalize();
        self.persist_notes(&next)?;
        documents.notes = next;
        Ok(stored)
    }

    fn delete_private_note(&self, target: &CommentTarget) -> Result<bool, PersistenceError> {
        let mut documents = self.lock()?;
        let mut next = documents.notes.clone();
        let before = next.comments.len();
        next.comments.retain(|comment| comment.target != *target);
        let removed = before != next.comments.len();
        if removed {
            self.persist_notes(&next)?;
            documents.notes = next;
        }
        Ok(removed)
    }

    fn delete_private_comment(&self, id: i64) -> Result<bool, PersistenceError> {
        let mut documents = self.lock()?;
        let mut next = documents.notes.clone();
        let before = next.comments.len();
        next.comments.retain(|comment| comment.id != id);
        let removed = before != next.comments.len();
        if removed {
            self.persist_notes(&next)?;
            documents.notes = next;
        }
        Ok(removed)
    }

    fn insert_bookmark(&self, bookmark: &Bookmark) -> Result<i64, PersistenceError> {
        validate_bookmark_numeric_range(bookmark)?;
        let mut documents = self.lock()?;
        let mut next = documents.bookmarks.clone();
        ensure_can_append("bookmarks", next.bookmarks.len(), MAX_BOOKMARK_ROWS)?;
        let id = allocate_id(&mut next.next_id)?;
        let mut stored = bookmark.clone();
        stored.id = id;
        next.bookmarks.push(stored);
        next.canonicalize();
        self.persist_bookmarks(&next)?;
        documents.bookmarks = next;
        Ok(id)
    }

    fn update_bookmark(&self, bookmark: &Bookmark) -> Result<bool, PersistenceError> {
        validate_bookmark_numeric_range(bookmark)?;
        let mut documents = self.lock()?;
        let mut next = documents.bookmarks.clone();
        let Some(existing) = next.bookmarks.iter_mut().find(|row| row.id == bookmark.id) else {
            return Ok(false);
        };
        *existing = bookmark.clone();
        self.persist_bookmarks(&next)?;
        documents.bookmarks = next;
        Ok(true)
    }

    fn bookmarks(&self, media_id: &MediaId) -> Result<Vec<Bookmark>, PersistenceError> {
        let mut bookmarks = self
            .lock()?
            .bookmarks
            .bookmarks
            .iter()
            .filter(|bookmark| bookmark.media_id == *media_id)
            .cloned()
            .collect::<Vec<_>>();
        bookmarks.sort_by_key(|bookmark| (bookmark.position_seconds, bookmark.id));
        Ok(bookmarks)
    }

    fn delete_bookmark(&self, id: i64) -> Result<bool, PersistenceError> {
        let mut documents = self.lock()?;
        let mut next = documents.bookmarks.clone();
        let before = next.bookmarks.len();
        next.bookmarks.retain(|bookmark| bookmark.id != id);
        let removed = before != next.bookmarks.len();
        if removed {
            self.persist_bookmarks(&next)?;
            documents.bookmarks = next;
        }
        Ok(removed)
    }

    fn save_session(&self, state: &SessionState, updated_at: i64) -> Result<(), PersistenceError> {
        let mut documents = self.lock()?;
        let mut next = documents.runtime.clone();
        next.session = Some(state.clone());
        next.session_updated_at = Some(updated_at);
        self.persist_runtime(&next)?;
        documents.runtime = next;
        Ok(())
    }

    fn session(&self) -> Result<Option<SessionState>, PersistenceError> {
        Ok(self.lock()?.runtime.session.clone())
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn remap_local_move_state(
        &self,
        mappings: &[LocalMoveMapping],
    ) -> Result<LocalMoveStateRemap, PersistenceError> {
        validate_local_move_mappings(mappings)?;
        if mappings.is_empty() {
            return Ok(LocalMoveStateRemap::default());
        }
        let mut documents = self.lock()?;
        self.flush_playback_checkpoint_locked(&mut documents)?;
        let plan = prepare_file_local_move_state(&documents, mappings)?;

        // Each document replacement is atomic. The Local move journal remains
        // until the final replacement, allowing startup reconciliation if the
        // process stops between document publishes.
        let on_disk = self.paths.is_some();
        let mut published = false;
        macro_rules! publish {
            ($write:expr) => {
                match $write {
                    Ok(()) => published |= on_disk,
                    Err(error) => {
                        if published {
                            self.publication_blocked.store(true, Ordering::Release);
                        }
                        return Err(error);
                    }
                }
            };
        }
        publish!(self.persist_progress(&plan.progress));
        publish!(self.persist_history(&plan.history));
        publish!(self.persist_notes(&plan.notes));
        publish!(self.persist_bookmarks(&plan.bookmarks));
        publish!(self.persist_playlists(&plan.playlists));
        publish!(self.persist_provider_cache(&plan.provider_cache));
        publish!(self.persist_runtime(&plan.runtime));
        if let Err(error) = self.persist_local_moves(&plan.local_moves) {
            if published {
                self.publication_blocked.store(true, Ordering::Release);
            }
            return Err(error);
        }
        documents.progress = plan.progress;
        documents.history = plan.history;
        documents.notes = plan.notes;
        documents.bookmarks = plan.bookmarks;
        documents.playlists = plan.playlists;
        documents.provider_cache = plan.provider_cache;
        documents.runtime = plan.runtime;
        documents.local_moves = plan.local_moves;
        Ok(plan.report)
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn journal_local_move_intent(
        &self,
        mappings: &[LocalMoveMapping],
        created_at: i64,
    ) -> Result<(), PersistenceError> {
        validate_local_move_mappings(mappings)?;
        if mappings.is_empty() {
            return Ok(());
        }
        let mut documents = self.lock()?;
        // Prove that the complete durable state can accept this exact remap
        // before allowing the filesystem worker to mutate anything.
        let _ = prepare_file_local_move_state(&documents, mappings)?;
        let mut next = documents.local_moves.clone();
        if next.intents.len().saturating_add(mappings.len()) > MAX_FILE_LOCAL_MOVE_MAPPINGS {
            return Err(invalid_local_move_state(
                "pending Local move journal exceeds its fixed limit",
            ));
        }
        for mapping in mappings {
            let source = mapping.source.to_string_lossy().into_owned();
            let target = mapping.target.to_string_lossy().into_owned();
            if next.intents.iter().any(|pending| {
                pending.source == source
                    || pending.target == target
                    || pending.source == target
                    || pending.target == source
            }) {
                return Err(invalid_local_move_state(
                    "Local move journal contains a conflicting path",
                ));
            }
            next.intents.push(DiskLocalMoveMapping {
                source,
                target,
                created_at,
            });
        }
        next.canonicalize();
        self.persist_local_moves(&next)?;
        documents.local_moves = next;
        Ok(())
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn local_move_intents(&self) -> Result<Vec<LocalMoveMapping>, PersistenceError> {
        self.lock()?
            .local_moves
            .intents
            .iter()
            .map(|mapping| {
                Ok(LocalMoveMapping {
                    source: PathBuf::from(&mapping.source),
                    target: PathBuf::from(&mapping.target),
                })
            })
            .collect()
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    fn discard_local_move_intents(
        &self,
        mappings: &[LocalMoveMapping],
    ) -> Result<(), PersistenceError> {
        validate_local_move_mappings(mappings)?;
        let mut documents = self.lock()?;
        let mut next = documents.local_moves.clone();
        for mapping in mappings {
            let source = mapping.source.to_string_lossy();
            let target = mapping.target.to_string_lossy();
            let Some(index) = next
                .intents
                .iter()
                .position(|pending| pending.source == source && pending.target == target)
            else {
                return Err(invalid_local_move_state(
                    "Local move journal no longer contains the requested mapping",
                ));
            };
            next.intents.remove(index);
        }
        self.persist_local_moves(&next)?;
        documents.local_moves = next;
        Ok(())
    }

    fn save_youtube_search(
        &self,
        search: &SavedYouTubeSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        validate_saved_youtube_search(search)?;
        ensure_saved_search_json_bound(
            "request",
            serde_json::to_vec(&search.request)?.len(),
            MAX_SAVED_SEARCH_REQUEST_BYTES,
        )?;
        ensure_saved_search_json_bound(
            "results",
            serde_json::to_vec(&search.results)?.len(),
            MAX_SAVED_SEARCH_RESULTS_BYTES,
        )?;
        self.update_searches(|document| {
            document.youtube = Some(Timed {
                updated_at,
                value: search.clone(),
            });
        })
    }

    fn youtube_search(&self) -> Result<Option<SavedYouTubeSearch>, PersistenceError> {
        let search = self
            .lock()?
            .searches
            .youtube
            .as_ref()
            .map(|timed| timed.value.clone());
        if let Some(search) = &search {
            validate_saved_youtube_search(search)?;
        }
        Ok(search)
    }

    fn update_saved_youtube_video_orientation(
        &self,
        video_id: &str,
        orientation: VideoOrientation,
        updated_at: i64,
    ) -> Result<bool, PersistenceError> {
        validate_youtube_video_id(video_id).map_err(|error| {
            PersistenceError::InvalidSavedSearch {
                reason: error.to_string(),
            }
        })?;
        let mut documents = self.lock()?;
        let mut next = documents.searches.clone();
        let Some(search) = &mut next.youtube else {
            return Ok(false);
        };
        let mut changed = false;
        for item in &mut search.value.results {
            if let SearchItem::Video(video) = item
                && video.video_id == video_id
                && video.orientation != orientation
            {
                video.orientation = orientation;
                changed = true;
            }
        }
        if changed {
            search.updated_at = updated_at;
            validate_saved_youtube_search(&search.value)?;
            self.persist_searches(&next)?;
            documents.searches = next;
        }
        Ok(changed)
    }

    fn clear_youtube_search(&self) -> Result<bool, PersistenceError> {
        self.clear_search(|document| document.youtube.take().is_some())
    }

    fn save_youtube_music_search(
        &self,
        search: &SavedYouTubeMusicSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        validate_saved_youtube_music_search(search)?;
        ensure_saved_search_json_bound(
            "results",
            serde_json::to_vec(&search.results)?.len(),
            MAX_SAVED_SEARCH_RESULTS_BYTES,
        )?;
        self.update_searches(|document| {
            document.youtube_music = Some(Timed {
                updated_at,
                value: search.clone(),
            });
        })
    }

    fn youtube_music_search(&self) -> Result<Option<SavedYouTubeMusicSearch>, PersistenceError> {
        let search = self
            .lock()?
            .searches
            .youtube_music
            .as_ref()
            .map(|timed| timed.value.clone());
        if let Some(search) = &search {
            validate_saved_youtube_music_search(search)?;
        }
        Ok(search)
    }

    fn clear_youtube_music_search(&self) -> Result<bool, PersistenceError> {
        self.clear_search(|document| document.youtube_music.take().is_some())
    }

    fn save_bandcamp_search(
        &self,
        search: &SavedBandcampSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        validate_saved_bandcamp_search(search)?;
        ensure_saved_search_json_bound(
            "Bandcamp results",
            serde_json::to_vec(&search.results)?.len(),
            MAX_SAVED_BANDCAMP_RESULTS_BYTES,
        )?;
        self.update_searches(|document| {
            document.bandcamp = Some(Timed {
                updated_at,
                value: search.clone(),
            });
        })
    }

    fn bandcamp_search(&self) -> Result<Option<SavedBandcampSearch>, PersistenceError> {
        let search = self
            .lock()?
            .searches
            .bandcamp
            .as_ref()
            .map(|timed| timed.value.clone());
        if let Some(search) = &search {
            validate_saved_bandcamp_search(search)?;
        }
        Ok(search)
    }

    fn clear_bandcamp_search(&self) -> Result<bool, PersistenceError> {
        self.clear_search(|document| document.bandcamp.take().is_some())
    }

    fn save_apple_podcasts_search(
        &self,
        search: &SavedApplePodcastsSearch,
        updated_at: i64,
    ) -> Result<(), PersistenceError> {
        validate_saved_apple_podcasts_search(search)?;
        ensure_saved_search_json_bound(
            "Apple Podcasts results",
            serde_json::to_vec(&search.results)?.len(),
            MAX_SAVED_APPLE_RESULTS_BYTES,
        )?;
        self.update_searches(|document| {
            document.apple_podcasts = Some(Timed {
                updated_at,
                value: search.clone(),
            });
        })
    }

    fn apple_podcasts_search(&self) -> Result<Option<SavedApplePodcastsSearch>, PersistenceError> {
        let search = self
            .lock()?
            .searches
            .apple_podcasts
            .as_ref()
            .map(|timed| timed.value.clone());
        if let Some(search) = &search {
            validate_saved_apple_podcasts_search(search)?;
        }
        Ok(search)
    }

    fn clear_apple_podcasts_search(&self) -> Result<bool, PersistenceError> {
        self.clear_search(|document| document.apple_podcasts.take().is_some())
    }

    fn put_cached_subscription_items(
        &self,
        cached: &CachedSubscriptionItems,
    ) -> Result<(), PersistenceError> {
        validate_cached_subscription_items(cached)?;
        let mut bounded = cached.clone();
        let _ = encode_bounded_subscription_items(&bounded.items)?;
        while serde_json::to_vec(&bounded.items)?.len() > MAX_SAVED_SUBSCRIPTION_ITEMS_BYTES {
            if bounded.items.pop().is_none() {
                break;
            }
        }
        ensure_subscription_snapshot_json_bound(serde_json::to_vec(&bounded.items)?.len())?;
        let mut documents = self.lock()?;
        let mut next = documents.provider_cache.clone();
        next.subscription_items
            .retain(|entry| entry.source != bounded.source || entry.source_id != bounded.source_id);
        let written_source = bounded.source.clone();
        let written_source_id = bounded.source_id.clone();
        next.subscription_items.push(bounded);
        next.subscription_items.sort_by(|left, right| {
            right
                .fetched_at
                .cmp(&left.fetched_at)
                .then_with(|| {
                    let left_is_written =
                        left.source == written_source && left.source_id == written_source_id;
                    let right_is_written =
                        right.source == written_source && right.source_id == written_source_id;
                    right_is_written.cmp(&left_is_written)
                })
                .then_with(|| left.source.as_str().cmp(right.source.as_str()))
                .then_with(|| left.source_id.cmp(&right.source_id))
        });
        next.subscription_items
            .truncate(MAX_SAVED_SUBSCRIPTION_SOURCES);
        next.canonicalize();
        self.persist_provider_cache(&next)?;
        documents.provider_cache = next;
        Ok(())
    }

    fn cached_subscription_items(
        &self,
        source: &SourceKind,
        source_id: &str,
    ) -> Result<Option<CachedSubscriptionItems>, PersistenceError> {
        validate_subscription_source_identity(source, source_id)?;
        let cached = self
            .lock()?
            .provider_cache
            .subscription_items
            .iter()
            .find(|cached| cached.source == *source && cached.source_id == source_id)
            .cloned();
        if let Some(cached) = &cached {
            validate_cached_subscription_items(cached)?;
        }
        Ok(cached)
    }

    fn delete_cached_subscription_items(
        &self,
        source: &SourceKind,
        source_id: &str,
    ) -> Result<bool, PersistenceError> {
        validate_subscription_source_identity(source, source_id)?;
        self.update_provider_cache(|cache| {
            let before = cache.subscription_items.len();
            cache
                .subscription_items
                .retain(|entry| entry.source != *source || entry.source_id != source_id);
            before != cache.subscription_items.len()
        })
    }

    fn add_listen_seconds(
        &self,
        source: &SourceKind,
        seconds: u64,
    ) -> Result<(), PersistenceError> {
        ensure_sqlite_integer_range(seconds, "listen seconds")?;
        let mut documents = self.lock()?;
        self.flush_playback_checkpoint_locked(&mut documents)?;
        let mut next = documents.statistics.clone();
        if let Some(total) = next
            .listen_totals
            .iter_mut()
            .find(|total| total.source == *source)
        {
            let total_seconds = total.total_seconds.checked_add(seconds).ok_or(
                PersistenceError::IntegerOutOfRange {
                    field: "listen seconds",
                },
            )?;
            ensure_sqlite_integer_range(total_seconds, "listen seconds")?;
            total.total_seconds = total_seconds;
        } else {
            ensure_can_append(
                "statistics",
                next.listen_totals.len(),
                MAX_LISTEN_TOTAL_ROWS,
            )?;
            next.listen_totals.push(ListenTotal {
                source: source.clone(),
                total_seconds: seconds,
            });
        }
        next.canonicalize();
        self.persist_statistics(&next)?;
        documents.statistics = next;
        Ok(())
    }

    fn listened_seconds(&self, source: &SourceKind) -> Result<u64, PersistenceError> {
        let documents = self.lock()?;
        let canonical = documents
            .statistics
            .listen_totals
            .iter()
            .find(|total| total.source == *source)
            .map_or(0, |total| total.total_seconds);
        Ok(documents
            .playback_checkpoint
            .listen_total
            .as_ref()
            .filter(|total| total.source == *source)
            .map_or(canonical, |total| total.total_seconds.max(canonical)))
    }

    fn listen_totals(&self) -> Result<Vec<ListenTotal>, PersistenceError> {
        let documents = self.lock()?;
        let mut totals = documents.statistics.listen_totals.clone();
        if let Some(checkpoint) = &documents.playback_checkpoint.listen_total {
            if let Some(existing) = totals
                .iter_mut()
                .find(|total| total.source == checkpoint.source)
            {
                existing.total_seconds = existing.total_seconds.max(checkpoint.total_seconds);
            } else {
                totals.push(checkpoint.clone());
            }
        }
        totals.sort_by(|left, right| {
            right
                .total_seconds
                .cmp(&left.total_seconds)
                .then_with(|| left.source.as_str().cmp(right.source.as_str()))
        });
        Ok(totals)
    }

    fn reset_listen_seconds(&self, source: &SourceKind) -> Result<bool, PersistenceError> {
        let mut documents = self.lock()?;
        let checkpoint_removed = documents
            .playback_checkpoint
            .listen_total
            .as_ref()
            .is_some_and(|total| total.source == *source);
        if checkpoint_removed {
            let mut next_checkpoint = documents.playback_checkpoint.clone();
            next_checkpoint.listen_total = None;
            self.persist_playback_checkpoint(&next_checkpoint)?;
            documents.playback_checkpoint = next_checkpoint;
        }
        let mut next = documents.statistics.clone();
        let before = next.listen_totals.len();
        next.listen_totals.retain(|total| total.source != *source);
        let removed = before != next.listen_totals.len();
        if removed {
            self.persist_statistics(&next)?;
            documents.statistics = next;
        }
        Ok(removed || checkpoint_removed)
    }

    fn put_cached_metadata(&self, cached: &CachedMetadata) -> Result<(), PersistenceError> {
        let written_id = cached.media.id.clone();
        self.update_provider_cache(|cache| {
            cache
                .metadata
                .retain(|entry| entry.media.id != cached.media.id);
            cache.metadata.push(cached.clone());
            cache.metadata.sort_by(|left, right| {
                right
                    .provenance
                    .fetched_at
                    .cmp(&left.provenance.fetched_at)
                    .then_with(|| {
                        let left_is_written = left.media.id == written_id;
                        let right_is_written = right.media.id == written_id;
                        right_is_written.cmp(&left_is_written)
                    })
            });
            cache.metadata.truncate(MAX_METADATA_CACHE_ROWS);
        })
    }

    fn cached_metadata(
        &self,
        media_id: &MediaId,
    ) -> Result<Option<CachedMetadata>, PersistenceError> {
        Ok(self
            .lock()?
            .provider_cache
            .metadata
            .iter()
            .find(|cached| cached.media.id == *media_id)
            .cloned())
    }

    fn delete_expired_metadata(&self, now: i64) -> Result<usize, PersistenceError> {
        self.update_provider_cache(|cache| {
            let before = cache.metadata.len();
            cache.metadata.retain(|cached| {
                cached
                    .provenance
                    .expires_at
                    .is_none_or(|expires_at| expires_at > now)
            });
            before - cache.metadata.len()
        })
    }

    fn put_cached_channel_summary(
        &self,
        cached: &CachedChannelSummary,
    ) -> Result<(), PersistenceError> {
        let written_channel_id = cached.summary.channel_id.clone();
        self.update_provider_cache(|cache| {
            cache
                .channel_summaries
                .retain(|entry| entry.summary.channel_id != cached.summary.channel_id);
            cache.channel_summaries.push(cached.clone());
            cache.channel_summaries.sort_by(|left, right| {
                right.fetched_at.cmp(&left.fetched_at).then_with(|| {
                    let left_is_written = left.summary.channel_id == written_channel_id;
                    let right_is_written = right.summary.channel_id == written_channel_id;
                    right_is_written.cmp(&left_is_written)
                })
            });
            cache
                .channel_summaries
                .truncate(MAX_CHANNEL_SUMMARY_CACHE_ROWS);
        })
    }

    fn cached_channel_summary(
        &self,
        channel_id: &str,
    ) -> Result<Option<CachedChannelSummary>, PersistenceError> {
        Ok(self
            .lock()?
            .provider_cache
            .channel_summaries
            .iter()
            .find(|cached| cached.summary.channel_id == channel_id)
            .cloned())
    }

    fn delete_expired_channel_summaries(&self, now: i64) -> Result<usize, PersistenceError> {
        self.update_provider_cache(|cache| {
            let before = cache.channel_summaries.len();
            cache
                .channel_summaries
                .retain(|cached| cached.expires_at > now);
            before - cache.channel_summaries.len()
        })
    }

    fn put_cached_wikidata(&self, cached: &CachedWikidataLookup) -> Result<(), PersistenceError> {
        let written_property_id = cached.property_id.clone();
        let written_external_id = cached.external_id.clone();
        self.update_provider_cache(|cache| {
            cache.wikidata.retain(|entry| {
                entry.property_id != cached.property_id || entry.external_id != cached.external_id
            });
            cache.wikidata.push(cached.clone());
            cache.wikidata.sort_by(|left, right| {
                right.fetched_at.cmp(&left.fetched_at).then_with(|| {
                    let left_is_written = left.property_id == written_property_id
                        && left.external_id == written_external_id;
                    let right_is_written = right.property_id == written_property_id
                        && right.external_id == written_external_id;
                    right_is_written.cmp(&left_is_written)
                })
            });
            cache.wikidata.truncate(MAX_WIKIDATA_CACHE_ROWS);
        })
    }

    fn cached_wikidata(
        &self,
        property_id: &str,
        external_id: &str,
    ) -> Result<Option<CachedWikidataLookup>, PersistenceError> {
        Ok(self
            .lock()?
            .provider_cache
            .wikidata
            .iter()
            .find(|cached| cached.property_id == property_id && cached.external_id == external_id)
            .cloned())
    }

    fn delete_expired_wikidata(&self, now: i64) -> Result<usize, PersistenceError> {
        self.update_provider_cache(|cache| {
            let before = cache.wikidata.len();
            cache.wikidata.retain(|cached| cached.expires_at > now);
            before - cache.wikidata.len()
        })
    }
}

#[derive(Clone, Copy)]
enum EntryMutation {
    Add,
    Toggle,
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
struct FileLocalMovePlan {
    progress: ProgressDocument,
    history: HistoryDocument,
    notes: NotesDocument,
    bookmarks: BookmarksDocument,
    playlists: PlaylistsDocument,
    runtime: RuntimeDocument,
    local_moves: LocalMovesDocument,
    provider_cache: ProviderCacheDocument,
    report: LocalMoveStateRemap,
}

/// Builds and validates the complete post-move document set without mutating
/// either memory or disk.
///
/// The same preflight is used before journaling and before publication, so a
/// destination identity collision can never authorize filesystem mutation.
#[cfg(any(feature = "local-rename", feature = "local-move"))]
fn prepare_file_local_move_state(
    documents: &FileDocuments,
    mappings: &[LocalMoveMapping],
) -> Result<FileLocalMovePlan, PersistenceError> {
    let mut progress = documents.progress.clone();
    let mut history = documents.history.clone();
    let mut notes = documents.notes.clone();
    let mut bookmarks = documents.bookmarks.clone();
    let mut playlists = documents.playlists.clone();
    let mut runtime = documents.runtime.clone();
    let mut local_moves = documents.local_moves.clone();
    let mut provider_cache = documents.provider_cache.clone();
    let mut report = LocalMoveStateRemap::default();

    for row in &mut progress.progress {
        report.playback_progress += usize::from(remap_media_id(&mut row.media_id, mappings)?);
    }
    for row in &mut history.history {
        let mut changed = remap_media_id(&mut row.media_id, mappings)?;
        if let Some(locator) = &mut row.replay_locator {
            changed |= remap_replay_locator(locator, mappings)?;
        }
        report.playback_history += usize::from(changed);
    }
    for comment in &mut notes.comments {
        let changed = match &mut comment.target {
            CommentTarget::Media { media_id }
            | CommentTarget::Source {
                source_id: media_id,
            }
            | CommentTarget::Position { media_id, .. } => remap_media_id(media_id, mappings)?,
            CommentTarget::Segment { .. } | CommentTarget::Subscription { .. } => false,
        };
        report.private_comments += usize::from(changed);
    }
    for bookmark in &mut bookmarks.bookmarks {
        report.bookmarks += usize::from(remap_media_id(&mut bookmark.media_id, mappings)?);
    }
    if let Some(session) = &mut runtime.session {
        let mut changed = false;
        if let Some(media_id) = &mut session.selected_media {
            changed |= remap_media_id(media_id, mappings)?;
        }
        if let Some(path) = &mut session.local_path {
            changed |= remap_string_path(path, mappings)?;
        }
        changed |= remap_screen(&mut session.screen, mappings)?;
        for screen in &mut session.back_stack {
            changed |= remap_screen(screen, mappings)?;
        }
        report.sessions += usize::from(changed);
    }
    for playlist in &mut playlists.playlists {
        for entry in &mut playlist.entries {
            let mut changed = remap_media_id(&mut entry.media.id, mappings)?;
            if entry.media.id.source == SourceKind::Local {
                changed |= remap_replay_locator(&mut entry.media.replay_locator, mappings)?;
                changed |= remap_file_url(&mut entry.media.webpage_url, mappings)?;
                if let Some(thumbnail) = &mut entry.media.thumbnail_url {
                    changed |= remap_file_url(thumbnail, mappings)?;
                }
            }
            report.playlist_entries += usize::from(changed);
        }
    }
    for cached in &mut provider_cache.metadata {
        let mut changed = remap_media_id(&mut cached.media.id, mappings)?;
        changed |= remap_media_file_urls(&mut cached.media, mappings)?;
        if let Some(source_url) = &mut cached.provenance.source_url {
            changed |= remap_file_url(source_url, mappings)?;
        }
        report.metadata_cache += usize::from(changed);
    }
    for cached in &mut provider_cache.subscription_items {
        let mut changed = false;
        if cached.source == SourceKind::Local {
            changed |= remap_string_path(&mut cached.source_id, mappings)?;
        }
        for item in &mut cached.items {
            changed |= remap_search_item(item, mappings)?;
        }
        report.subscription_items_cache += usize::from(changed);
    }

    progress.canonicalize();
    history.canonicalize();
    notes.canonicalize();
    bookmarks.canonicalize();
    playlists.canonicalize();
    local_moves.intents.retain(|pending| {
        !mappings.iter().any(|mapping| {
            pending.source == mapping.source.to_string_lossy()
                && pending.target == mapping.target.to_string_lossy()
        })
    });
    local_moves.canonicalize();
    provider_cache.canonicalize();
    validate_file_local_move_plan(&progress, &playlists, &provider_cache)?;

    Ok(FileLocalMovePlan {
        progress,
        history,
        notes,
        bookmarks,
        playlists,
        runtime,
        local_moves,
        provider_cache,
        report,
    })
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
fn validate_file_local_move_plan(
    progress_document: &ProgressDocument,
    playlists: &PlaylistsDocument,
    provider_cache: &ProviderCacheDocument,
) -> Result<(), PersistenceError> {
    let mut progress = HashSet::with_capacity(progress_document.progress.len());
    for row in &progress_document.progress {
        if !progress.insert(row.media_id.clone()) {
            return Err(invalid_local_move_state(format!(
                "watched progress already exists at destination `{}`",
                row.media_id.external_id
            )));
        }
    }

    let mut metadata = HashSet::with_capacity(provider_cache.metadata.len());
    for row in &provider_cache.metadata {
        if !metadata.insert(row.media.id.clone()) {
            return Err(invalid_local_move_state(format!(
                "metadata already exists at destination `{}`",
                row.media.id.external_id
            )));
        }
    }

    let mut subscription_keys = HashSet::with_capacity(provider_cache.subscription_items.len());
    for cached in &provider_cache.subscription_items {
        let key = (cached.source.clone(), cached.source_id.clone());
        if !subscription_keys.insert(key) {
            return Err(invalid_local_move_state(format!(
                "subscription cache already exists at destination `{}`",
                cached.source_id
            )));
        }
        validate_cached_subscription_items(cached)?;
        ensure_subscription_snapshot_json_bound(serde_json::to_vec(&cached.items)?.len())?;
    }

    for playlist in &playlists.playlists {
        let mut whole_media = HashSet::new();
        for entry in &playlist.entries {
            if entry.segment.is_none() && !whole_media.insert(entry.media.id.clone()) {
                return Err(invalid_local_move_state(format!(
                    "playlist `{}` already contains the destination identity `{}`",
                    playlist.name, entry.media.id.external_id
                )));
            }
        }
        let _ = record_to_playlist(playlist)?;
    }
    Ok(())
}

impl FileStateStore {
    fn create_playlist_inner(
        &self,
        name: &str,
        description: Option<&str>,
        first_media: Option<&PlaylistMediaSnapshot>,
        created_at: i64,
    ) -> Result<PlaylistCreateOutcome, PersistenceError> {
        let (name, name_key, description) =
            validated_playlist_fields(name, description, created_at)?;
        if is_radio_favorites_name_key(&name_key) {
            return Err(invalid_playlist(
                "the case-insensitive `Favorite radio stations` name is reserved for Radio favorites",
            ));
        }
        if let Some(media) = first_media {
            let _ = encoded_playlist_snapshot(media)?;
        }
        let mut documents = self.lock()?;
        let mut next = documents.playlists.clone();
        if let Some(existing) = next
            .playlists
            .iter()
            .find(|playlist| playlist_name_key(&playlist.name) == name_key)
        {
            return Ok(PlaylistCreateOutcome::Existing(summary_from_record(
                existing,
            )));
        }
        if next.playlists.len() >= MAX_PLAYLISTS {
            return Err(invalid_playlist(format!(
                "playlist count has reached the {MAX_PLAYLISTS}-playlist limit"
            )));
        }
        let id = if name_key == TODO_PLAYLIST_NAME {
            TODO_PLAYLIST_ID.to_owned()
        } else {
            let mut id;
            loop {
                id = format!("local:{:032x}", next.next_local_id);
                next.next_local_id = next.next_local_id.checked_add(1).ok_or(
                    PersistenceError::IntegerOutOfRange {
                        field: "local playlist ID",
                    },
                )?;
                if next.playlists.iter().all(|playlist| playlist.id != id) {
                    break;
                }
            }
            id
        };
        let entries = first_media.map_or_else(Vec::new, |media| {
            vec![PlaylistEntry {
                media: media.clone(),
                segment: None,
                added_at: created_at,
            }]
        });
        let record = PlaylistRecord {
            id: id.clone(),
            name: name.to_owned(),
            description: description.map(str::to_owned),
            created_at,
            updated_at: created_at,
            entries,
        };
        let summary = summary_from_record(&record);
        next.playlists.push(record);
        next.canonicalize();
        self.persist_playlists(&next)?;
        documents.playlists = next;
        Ok(PlaylistCreateOutcome::Created(summary))
    }

    fn ensure_todo(&self, created_at: i64) -> Result<(), PersistenceError> {
        let mut documents = self.lock()?;
        if documents
            .playlists
            .playlists
            .iter()
            .any(|playlist| playlist.id == TODO_PLAYLIST_ID)
        {
            return Ok(());
        }
        if documents.playlists.playlists.len() >= MAX_PLAYLISTS {
            return Err(invalid_playlist(
                "playlist count has reached its fixed limit",
            ));
        }
        if documents
            .playlists
            .playlists
            .iter()
            .any(|playlist| playlist_name_key(&playlist.name) == TODO_PLAYLIST_NAME)
        {
            return Err(invalid_playlist(
                "the reserved todo name belongs to a non-todo playlist",
            ));
        }
        let mut next = documents.playlists.clone();
        next.playlists.push(PlaylistRecord {
            id: TODO_PLAYLIST_ID.to_owned(),
            name: TODO_PLAYLIST_NAME.to_owned(),
            description: None,
            created_at,
            updated_at: created_at,
            entries: Vec::new(),
        });
        next.canonicalize();
        self.persist_playlists(&next)?;
        documents.playlists = next;
        Ok(())
    }

    fn ensure_radio_favorites(&self, created_at: i64) -> Result<(), PersistenceError> {
        let mut documents = self.lock()?;
        if documents
            .playlists
            .playlists
            .iter()
            .any(|playlist| playlist.id == RADIO_FAVORITES_PLAYLIST_ID)
        {
            return Ok(());
        }
        if documents.playlists.playlists.len() >= MAX_PLAYLISTS {
            return Err(invalid_playlist(
                "playlist count has reached its fixed limit",
            ));
        }
        let name_key = playlist_name_key(RADIO_FAVORITES_PLAYLIST_NAME);
        if documents
            .playlists
            .playlists
            .iter()
            .any(|playlist| playlist_name_key(&playlist.name) == name_key)
        {
            return Err(invalid_playlist(
                "the reserved Radio favorites name belongs to another playlist",
            ));
        }
        let mut next = documents.playlists.clone();
        next.playlists.push(PlaylistRecord {
            id: RADIO_FAVORITES_PLAYLIST_ID.to_owned(),
            name: RADIO_FAVORITES_PLAYLIST_NAME.to_owned(),
            description: None,
            created_at,
            updated_at: created_at,
            entries: Vec::new(),
        });
        next.canonicalize();
        self.persist_playlists(&next)?;
        documents.playlists = next;
        Ok(())
    }

    fn mutate_playlist_entry(
        &self,
        playlist_id: &str,
        media: &PlaylistMediaSnapshot,
        updated_at: i64,
        mutation: EntryMutation,
    ) -> Result<PlaylistToggleOutcome, PersistenceError> {
        validate_playlist_id(playlist_id)?;
        let _ = encoded_playlist_snapshot(media)?;
        validate_playlist_timestamp(updated_at)?;
        let mut documents = self.lock()?;
        let mut next = documents.playlists.clone();
        let Some(playlist) = next
            .playlists
            .iter_mut()
            .find(|playlist| playlist.id == playlist_id)
        else {
            return Err(invalid_playlist(format!(
                "playlist `{playlist_id}` does not exist"
            )));
        };
        if let Some(index) = playlist
            .entries
            .iter()
            .position(|entry| entry.segment.is_none() && entry.media.id == media.id)
        {
            if matches!(mutation, EntryMutation::Toggle) {
                playlist.entries.remove(index);
                playlist.updated_at = updated_at;
                self.persist_playlists(&next)?;
                documents.playlists = next;
                return Ok(PlaylistToggleOutcome::Removed);
            }
            return Ok(PlaylistToggleOutcome::Removed);
        }
        if playlist.entries.len() >= MAX_PLAYLIST_ENTRIES {
            return Err(invalid_playlist(format!(
                "playlist `{playlist_id}` has reached the {MAX_PLAYLIST_ENTRIES}-entry limit"
            )));
        }
        playlist.entries.push(PlaylistEntry {
            media: media.clone(),
            segment: None,
            added_at: updated_at,
        });
        playlist.updated_at = updated_at;
        self.persist_playlists(&next)?;
        documents.playlists = next;
        Ok(PlaylistToggleOutcome::Added)
    }

    fn update_searches(
        &self,
        update: impl FnOnce(&mut SearchCacheDocument),
    ) -> Result<(), PersistenceError> {
        let mut documents = self.lock()?;
        let mut next = documents.searches.clone();
        update(&mut next);
        self.persist_searches(&next)?;
        documents.searches = next;
        Ok(())
    }

    fn clear_search(
        &self,
        clear: impl FnOnce(&mut SearchCacheDocument) -> bool,
    ) -> Result<bool, PersistenceError> {
        let mut documents = self.lock()?;
        let mut next = documents.searches.clone();
        let removed = clear(&mut next);
        if removed {
            self.persist_searches(&next)?;
            documents.searches = next;
        }
        Ok(removed)
    }

    fn update_provider_cache<R>(
        &self,
        update: impl FnOnce(&mut ProviderCacheDocument) -> R,
    ) -> Result<R, PersistenceError> {
        let mut documents = self.lock()?;
        let mut next = documents.provider_cache.clone();
        let result = update(&mut next);
        next.canonicalize();
        self.persist_provider_cache(&next)?;
        documents.provider_cache = next;
        Ok(result)
    }
}

fn media_key(media_id: &MediaId) -> (&str, &str) {
    (media_id.source.as_str(), &media_id.external_id)
}

fn playlist_name_key(name: &str) -> String {
    name.chars().flat_map(char::to_lowercase).collect()
}

fn summary_from_record(record: &PlaylistRecord) -> PlaylistSummary {
    PlaylistSummary {
        id: record.id.clone(),
        name: record.name.clone(),
        description: record.description.clone(),
        entry_count: record.entries.len(),
    }
}

fn record_to_playlist(record: &PlaylistRecord) -> Result<Playlist, PersistenceError> {
    validate_playlist_id(&record.id)?;
    validated_playlist_fields(
        &record.name,
        record.description.as_deref(),
        record.updated_at,
    )?;
    if record.entries.len() > MAX_PLAYLIST_ENTRIES {
        return Err(invalid_playlist(format!(
            "playlist `{}` exceeds the {MAX_PLAYLIST_ENTRIES}-entry limit",
            record.id
        )));
    }
    for entry in &record.entries {
        let _ = encoded_playlist_snapshot(&entry.media)?;
        validate_playlist_timestamp(entry.added_at)?;
        if let Some(segment) = &entry.segment {
            validate_playlist_segment(segment, &entry.media)?;
            ensure_saved_search_json_bound(
                "playlist segment",
                serde_json::to_vec(segment)?.len(),
                MAX_PLAYLIST_SEGMENT_BYTES,
            )?;
        }
    }
    Ok(Playlist {
        id: record.id.clone(),
        name: record.name.clone(),
        description: record.description.clone(),
        entries: record.entries.clone(),
    })
}

fn contains_whole_media(record: &PlaylistRecord, media_id: &MediaId) -> bool {
    record
        .entries
        .iter()
        .any(|entry| entry.segment.is_none() && entry.media.id == *media_id)
}

fn allocate_id(next: &mut i64) -> Result<i64, PersistenceError> {
    if *next <= 0 {
        return Err(PersistenceError::IntegerOutOfRange {
            field: "persistent row ID",
        });
    }
    let id = *next;
    *next = next
        .checked_add(1)
        .ok_or(PersistenceError::IntegerOutOfRange {
            field: "persistent row ID",
        })?;
    Ok(id)
}

fn ensure_sqlite_integer_range(value: u64, field: &'static str) -> Result<(), PersistenceError> {
    if value > SQLITE_INTEGER_MAX_U64 {
        return Err(PersistenceError::IntegerOutOfRange { field });
    }
    Ok(())
}

fn validate_sqlite_integer_range(
    document: &'static str,
    field: &'static str,
    value: u64,
) -> Result<(), PersistenceError> {
    ensure_sqlite_integer_range(value, field).map_err(|_| {
        invalid_file_document(
            document,
            format!("`{field}` exceeds the maximum `SQLite INTEGER` value"),
        )
    })
}

fn validate_progress_numeric_range(progress: &PlaybackProgress) -> Result<(), PersistenceError> {
    ensure_sqlite_integer_range(progress.position_seconds, "position_seconds")?;
    if let Some(duration) = progress.duration_seconds {
        ensure_sqlite_integer_range(duration, "duration_seconds")?;
    }
    Ok(())
}

fn validate_history_entry(entry: &HistoryEntry) -> Result<(), PersistenceError> {
    ensure_sqlite_integer_range(entry.position_seconds, "position_seconds")?;
    if let Some(duration) = entry.duration_seconds {
        ensure_sqlite_integer_range(duration, "duration_seconds")?;
    }
    let _ =
        bounded_history_replay_locator(&entry.media_id.source, entry.replay_locator.as_deref())?;
    Ok(())
}

fn validate_bookmark_numeric_range(bookmark: &Bookmark) -> Result<(), PersistenceError> {
    ensure_sqlite_integer_range(bookmark.position_seconds, "position_seconds")
}

fn invalid_file_document(document: &'static str, reason: impl Into<String>) -> PersistenceError {
    PersistenceError::InvalidFileDocument {
        document,
        reason: reason.into(),
    }
}

fn ensure_row_limit(
    document: &'static str,
    rows: usize,
    maximum_rows: usize,
) -> Result<(), PersistenceError> {
    if rows > maximum_rows {
        return Err(invalid_file_document(
            document,
            format!("contains {rows} rows; the limit is {maximum_rows}"),
        ));
    }
    Ok(())
}

fn ensure_can_append(
    document: &'static str,
    rows: usize,
    maximum_rows: usize,
) -> Result<(), PersistenceError> {
    if rows >= maximum_rows {
        return Err(PersistenceError::StateRowLimitReached {
            document,
            maximum_rows,
        });
    }
    Ok(())
}

fn validate_next_id(
    document: &'static str,
    next_id: i64,
    maximum_id: i64,
) -> Result<(), PersistenceError> {
    if next_id <= 0 || next_id <= maximum_id {
        return Err(invalid_file_document(
            document,
            format!("next_id {next_id} must be greater than every positive row ID"),
        ));
    }
    Ok(())
}

fn validate_file_documents(documents: &FileDocuments) -> Result<(), PersistenceError> {
    ensure_documents_supported(documents)?;

    ensure_row_limit(
        "progress",
        documents.progress.progress.len(),
        MAX_PROGRESS_ROWS,
    )?;
    let mut progress_ids = HashSet::with_capacity(documents.progress.progress.len());
    for progress in &documents.progress.progress {
        validate_progress_numeric_range(progress)
            .map_err(|error| invalid_file_document("progress", error.to_string()))?;
        if !progress_ids.insert(progress.media_id.clone()) {
            return Err(invalid_file_document(
                "progress",
                "contains duplicate provider-qualified media identities",
            ));
        }
    }

    ensure_row_limit("history", documents.history.history.len(), MAX_HISTORY_ROWS)?;
    let mut history_ids = HashSet::with_capacity(documents.history.history.len());
    let mut maximum_history_id = 0;
    for entry in &documents.history.history {
        if entry.id <= 0 || !history_ids.insert(entry.id) {
            return Err(invalid_file_document(
                "history",
                "row IDs must be positive and unique",
            ));
        }
        validate_history_entry(entry)
            .map_err(|error| invalid_file_document("history", error.to_string()))?;
        maximum_history_id = maximum_history_id.max(entry.id);
    }
    validate_next_id("history", documents.history.next_id, maximum_history_id)?;

    ensure_row_limit(
        "notes",
        documents.notes.comments.len(),
        MAX_PRIVATE_COMMENT_ROWS,
    )?;
    let mut note_ids = HashSet::with_capacity(documents.notes.comments.len());
    let mut maximum_note_id = 0;
    for note in &documents.notes.comments {
        if note.id <= 0 || !note_ids.insert(note.id) {
            return Err(invalid_file_document(
                "notes",
                "row IDs must be positive and unique",
            ));
        }
        maximum_note_id = maximum_note_id.max(note.id);
    }
    validate_next_id("notes", documents.notes.next_id, maximum_note_id)?;

    ensure_row_limit(
        "bookmarks",
        documents.bookmarks.bookmarks.len(),
        MAX_BOOKMARK_ROWS,
    )?;
    let mut bookmark_ids = HashSet::with_capacity(documents.bookmarks.bookmarks.len());
    let mut maximum_bookmark_id = 0;
    for bookmark in &documents.bookmarks.bookmarks {
        validate_bookmark_numeric_range(bookmark)
            .map_err(|error| invalid_file_document("bookmarks", error.to_string()))?;
        if bookmark.id <= 0 || !bookmark_ids.insert(bookmark.id) {
            return Err(invalid_file_document(
                "bookmarks",
                "row IDs must be positive and unique",
            ));
        }
        maximum_bookmark_id = maximum_bookmark_id.max(bookmark.id);
    }
    validate_next_id(
        "bookmarks",
        documents.bookmarks.next_id,
        maximum_bookmark_id,
    )?;

    ensure_row_limit(
        "statistics",
        documents.statistics.listen_totals.len(),
        MAX_LISTEN_TOTAL_ROWS,
    )?;
    let mut statistic_sources = HashSet::with_capacity(documents.statistics.listen_totals.len());
    for total in &documents.statistics.listen_totals {
        validate_sqlite_integer_range("statistics", "total_seconds", total.total_seconds)?;
        if !statistic_sources.insert(total.source.clone()) {
            return Err(invalid_file_document(
                "statistics",
                "contains duplicate source totals",
            ));
        }
    }

    validate_playback_checkpoint(&documents.playback_checkpoint)?;
    validate_playlists_document(&documents.playlists)?;
    validate_local_moves_document(&documents.local_moves)?;
    #[cfg(feature = "yandex-music")]
    validate_yandex_music_reactions_document(&documents.yandex_music_reactions)?;
    validate_search_cache_document(&documents.searches)?;
    validate_provider_cache_document(&documents.provider_cache)?;
    Ok(())
}

#[cfg(feature = "yandex-music")]
fn validate_yandex_music_reactions_document(
    document: &YandexMusicReactionsDocument,
) -> Result<(), PersistenceError> {
    ensure_row_limit(
        YANDEX_MUSIC_REACTIONS_DOCUMENT,
        document.reactions.len(),
        MAX_YANDEX_MUSIC_REACTION_ROWS,
    )?;
    let mut identities = HashSet::with_capacity(document.reactions.len());
    for entry in &document.reactions {
        validate_yandex_music_reaction_ledger_entry(entry).map_err(|error| {
            invalid_file_document(YANDEX_MUSIC_REACTIONS_DOCUMENT, error.to_string())
        })?;
        if !identities.insert((entry.account_uid.clone(), entry.track_id.clone())) {
            return Err(invalid_file_document(
                YANDEX_MUSIC_REACTIONS_DOCUMENT,
                "contains duplicate account-and-track identities",
            ));
        }
    }
    Ok(())
}

fn validate_playlists_document(document: &PlaylistsDocument) -> Result<(), PersistenceError> {
    ensure_row_limit("playlists", document.playlists.len(), MAX_PLAYLISTS)?;
    if document.next_local_id == 0 {
        return Err(invalid_file_document(
            "playlists",
            "next_local_id must be positive",
        ));
    }
    let mut ids = HashSet::with_capacity(document.playlists.len());
    let mut names = HashSet::with_capacity(document.playlists.len());
    let mut maximum_local_id = 0_u64;
    for record in &document.playlists {
        if !ids.insert(record.id.clone()) {
            return Err(invalid_file_document(
                "playlists",
                "playlist IDs must be unique",
            ));
        }
        let name_key = playlist_name_key(&record.name);
        if !names.insert(name_key.clone()) {
            return Err(invalid_file_document(
                "playlists",
                "playlist names must be case-insensitively unique",
            ));
        }
        if record.id == TODO_PLAYLIST_ID {
            if name_key != TODO_PLAYLIST_NAME {
                return Err(invalid_file_document(
                    "playlists",
                    "the built-in todo ID must retain the todo name",
                ));
            }
        } else if name_key == TODO_PLAYLIST_NAME {
            return Err(invalid_file_document(
                "playlists",
                "the todo name is reserved for the built-in playlist",
            ));
        }
        if record.id == RADIO_FAVORITES_PLAYLIST_ID {
            if !is_radio_favorites_name_key(&name_key) {
                return Err(invalid_file_document(
                    "playlists",
                    "the built-in Radio favorites ID must retain the Radio favorites name",
                ));
            }
            for entry in &record.entries {
                validate_radio_favorite_snapshot(&entry.media)
                    .map_err(|error| invalid_file_document("playlists", error.to_string()))?;
            }
        } else if is_radio_favorites_name_key(&name_key) {
            return Err(invalid_file_document(
                "playlists",
                "the Radio favorites name is reserved for the built-in Radio favorites playlist",
            ));
        }
        validate_playlist_timestamp(record.created_at)
            .map_err(|error| invalid_file_document("playlists", error.to_string()))?;
        record_to_playlist(record)
            .map_err(|error| invalid_file_document("playlists", error.to_string()))?;

        let mut whole_media = HashSet::new();
        for entry in &record.entries {
            if entry.segment.is_none() && !whole_media.insert(entry.media.id.clone()) {
                return Err(invalid_file_document(
                    "playlists",
                    format!(
                        "playlist {:?} contains the same complete media item more than once",
                        record.id
                    ),
                ));
            }
        }
        if let Some(suffix) = record.id.strip_prefix("local:") {
            let local_id = u64::from_str_radix(suffix, 16).map_err(|_| {
                invalid_file_document("playlists", "a local playlist ID is malformed")
            })?;
            maximum_local_id = maximum_local_id.max(local_id);
        }
    }
    if document.next_local_id <= maximum_local_id {
        return Err(invalid_file_document(
            "playlists",
            "next_local_id must be greater than every allocated local playlist ID",
        ));
    }
    Ok(())
}

fn validate_local_moves_document(document: &LocalMovesDocument) -> Result<(), PersistenceError> {
    ensure_row_limit(
        "Local move journal",
        document.intents.len(),
        MAX_FILE_LOCAL_MOVE_MAPPINGS,
    )?;
    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    {
        let mappings = document
            .intents
            .iter()
            .map(|mapping| LocalMoveMapping {
                source: PathBuf::from(&mapping.source),
                target: PathBuf::from(&mapping.target),
            })
            .collect::<Vec<_>>();
        validate_local_move_mappings(&mappings)
            .map_err(|error| invalid_file_document("Local move journal", error.to_string()))?;
    }
    #[cfg(not(any(feature = "local-rename", feature = "local-move")))]
    if !document.intents.is_empty() {
        return Err(invalid_file_document(
            "Local move journal",
            "cannot be restored by a build without local rename or move support",
        ));
    }
    Ok(())
}

fn validate_search_cache_document(document: &SearchCacheDocument) -> Result<(), PersistenceError> {
    if let Some(search) = &document.youtube {
        validate_saved_youtube_search(&search.value)
            .map_err(|error| invalid_file_document("search cache", error.to_string()))?;
    }
    if let Some(search) = &document.youtube_music {
        validate_saved_youtube_music_search(&search.value)
            .map_err(|error| invalid_file_document("search cache", error.to_string()))?;
    }
    if let Some(search) = &document.bandcamp {
        validate_saved_bandcamp_search(&search.value)
            .map_err(|error| invalid_file_document("search cache", error.to_string()))?;
    }
    if let Some(search) = &document.apple_podcasts {
        validate_saved_apple_podcasts_search(&search.value)
            .map_err(|error| invalid_file_document("search cache", error.to_string()))?;
    }
    Ok(())
}

fn validate_provider_cache_document(
    document: &ProviderCacheDocument,
) -> Result<(), PersistenceError> {
    ensure_row_limit(
        "provider cache subscriptions",
        document.subscription_items.len(),
        MAX_SAVED_SUBSCRIPTION_SOURCES,
    )?;
    ensure_row_limit(
        "provider metadata cache",
        document.metadata.len(),
        MAX_METADATA_CACHE_ROWS,
    )?;
    ensure_row_limit(
        "provider channel cache",
        document.channel_summaries.len(),
        MAX_CHANNEL_SUMMARY_CACHE_ROWS,
    )?;
    ensure_row_limit(
        "Wikidata cache",
        document.wikidata.len(),
        MAX_WIKIDATA_CACHE_ROWS,
    )?;

    let mut subscriptions = HashSet::with_capacity(document.subscription_items.len());
    for cached in &document.subscription_items {
        if !subscriptions.insert((cached.source.clone(), cached.source_id.clone())) {
            return Err(invalid_file_document(
                "provider cache",
                "contains duplicate subscription snapshots",
            ));
        }
        validate_cached_subscription_items(cached)
            .map_err(|error| invalid_file_document("provider cache", error.to_string()))?;
    }
    let mut metadata = HashSet::with_capacity(document.metadata.len());
    for cached in &document.metadata {
        if !metadata.insert(cached.media.id.clone()) {
            return Err(invalid_file_document(
                "provider cache",
                "contains duplicate media metadata identities",
            ));
        }
    }
    let mut channels = HashSet::with_capacity(document.channel_summaries.len());
    for cached in &document.channel_summaries {
        if !channels.insert(cached.summary.channel_id.clone()) {
            return Err(invalid_file_document(
                "provider cache",
                "contains duplicate channel summaries",
            ));
        }
    }
    let mut wikidata = HashSet::with_capacity(document.wikidata.len());
    for cached in &document.wikidata {
        if !wikidata.insert((cached.property_id.clone(), cached.external_id.clone())) {
            return Err(invalid_file_document(
                "provider cache",
                "contains duplicate Wikidata lookup identities",
            ));
        }
    }
    Ok(())
}

fn ensure_documents_supported(documents: &FileDocuments) -> Result<(), PersistenceError> {
    for version in [
        documents.progress.format_version,
        documents.history.format_version,
        documents.notes.format_version,
        documents.bookmarks.format_version,
        documents.statistics.format_version,
        documents.local_moves.format_version,
        documents.playlists.format_version,
        documents.runtime.format_version,
        documents.playback_checkpoint.format_version,
        documents.searches.format_version,
        documents.provider_cache.format_version,
    ] {
        ensure_format(version)?;
    }
    #[cfg(feature = "yandex-music")]
    ensure_format(documents.yandex_music_reactions.format_version)?;
    Ok(())
}

fn ensure_format(version: u32) -> Result<(), PersistenceError> {
    if version > FILE_FORMAT_VERSION {
        return Err(PersistenceError::UnsupportedFileFormat {
            found: version,
            supported: FILE_FORMAT_VERSION,
        });
    }
    if version == 0 {
        return Err(PersistenceError::UnsupportedFileFormat {
            found: version,
            supported: FILE_FORMAT_VERSION,
        });
    }
    Ok(())
}

fn load_or_default<T>(
    path: &Path,
    document: &'static str,
    maximum_bytes: usize,
    default: impl FnOnce() -> T,
) -> Result<T, PersistenceError>
where
    T: for<'de> Deserialize<'de>,
{
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(default()),
        Err(error) => return Err(error.into()),
    };
    load_document(file, document, maximum_bytes)
}

/// Loads a disposable document, preserving and resetting only corrupt content.
///
/// Operational failures such as permission errors still fail startup. The
/// caller must use this helper only for state that can be reconstructed without
/// losing authoritative user data.
fn load_regenerable<T>(
    path: &Path,
    document: &'static str,
    maximum_bytes: usize,
    empty: impl Fn() -> T,
    validate: impl Fn(&T) -> Result<(), PersistenceError>,
) -> Result<T, PersistenceError>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    let expected_generation = file_generation(path)?;
    let loaded = load_or_default(path, document, maximum_bytes, &empty)
        .and_then(|loaded| validate(&loaded).map(|()| loaded));
    match loaded {
        Ok(loaded) => Ok(loaded),
        Err(error) if is_regenerable_corruption(&error) => {
            let replacement = empty();
            validate(&replacement)?;
            quarantine_and_reset_regenerable(
                path,
                document,
                maximum_bytes,
                expected_generation,
                &replacement,
            )?;
            Ok(replacement)
        }
        Err(error) => Err(error),
    }
}

fn is_regenerable_corruption(error: &PersistenceError) -> bool {
    match error {
        PersistenceError::TomlDecode(_)
        | PersistenceError::StateDocumentTooLarge { .. }
        | PersistenceError::InvalidFileDocument { .. }
        | PersistenceError::UnsupportedFileFormat { .. } => true,
        PersistenceError::Io(error) => error.kind() == std::io::ErrorKind::InvalidData,
        _ => false,
    }
}

/// Preserves one corrupt disposable document before canonical replacement.
///
/// A hard link makes the common path constant-time. Filesystems without hard
/// links fall back to a private, exclusively-created byte-for-byte copy. The
/// canonical replacement happens only after the quarantine file is durable.
fn quarantine_and_reset_regenerable(
    path: &Path,
    document: &'static str,
    maximum_bytes: usize,
    expected_generation: Option<FileGeneration>,
    replacement: &impl Serialize,
) -> Result<(), PersistenceError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(invalid_file_document(
            document,
            "corrupt regenerable path is not a regular file",
        ));
    }
    if file_generation(path)? != expected_generation {
        return Err(PersistenceError::StateDocumentChangedExternally { document });
    }

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "regenerable document has no parent directory",
        )
    })?;
    let quarantine = create_regenerable_quarantine(path, document)?;
    set_private_file_permissions(&quarantine)?;
    File::open(&quarantine)?.sync_all()?;
    File::open(parent)?.sync_all()?;
    write_document(path, document, maximum_bytes, replacement)
}

fn create_regenerable_quarantine(
    path: &Path,
    document: &'static str,
) -> Result<PathBuf, PersistenceError> {
    for index in 0..MAX_REGENERABLE_QUARANTINE_SLOTS {
        let candidate = regenerable_quarantine_path(path, index)?;
        match fs::hard_link(path, &candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => match copy_to_new_private_file(path, &candidate) {
                Ok(()) => return Ok(candidate),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            },
        }
    }
    Err(invalid_file_document(
        document,
        format!(
            "all {MAX_REGENERABLE_QUARANTINE_SLOTS} private corruption quarantine names are occupied"
        ),
    ))
}

fn regenerable_quarantine_path(path: &Path, index: usize) -> Result<PathBuf, PersistenceError> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "regenerable document has no parent directory",
        )
    })?;
    let original_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "regenerable document has no file name",
        )
    })?;
    let mut quarantine_name = std::ffi::OsString::from(".");
    quarantine_name.push(original_name);
    if index == 0 {
        quarantine_name.push(".corrupt");
    } else {
        quarantine_name.push(format!(".corrupt.{index}"));
    }
    Ok(parent.join(quarantine_name))
}

fn copy_to_new_private_file(source: &Path, target: &Path) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut target_file = options.open(target)?;
    let result = (|| {
        let mut source = File::open(source)?;
        std::io::copy(&mut source, &mut target_file)?;
        target_file.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(target);
    }
    result
}

fn load_authoritative<T>(
    path: &Path,
    document: &'static str,
    maximum_bytes: usize,
    initialized: bool,
    default: impl FnOnce() -> T,
) -> Result<T, PersistenceError>
where
    T: for<'de> Deserialize<'de>,
{
    if initialized {
        load_required(path, document, maximum_bytes)
    } else {
        load_or_default(path, document, maximum_bytes, default)
    }
}

fn load_required<T>(
    path: &Path,
    document: &'static str,
    maximum_bytes: usize,
) -> Result<T, PersistenceError>
where
    T: for<'de> Deserialize<'de>,
{
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(invalid_file_document(
                document,
                "required file is missing after the state manifest was published",
            ));
        }
        Err(error) => return Err(error.into()),
    };
    load_document(file, document, maximum_bytes)
}

fn load_document<T>(
    mut file: File,
    document: &'static str,
    maximum_bytes: usize,
) -> Result<T, PersistenceError>
where
    T: for<'de> Deserialize<'de>,
{
    let maximum_bytes_u64 =
        u64::try_from(maximum_bytes).map_err(|_| PersistenceError::IntegerOutOfRange {
            field: "state document byte limit",
        })?;
    if file.metadata()?.len() > maximum_bytes_u64 {
        return Err(PersistenceError::StateDocumentTooLarge {
            document,
            maximum_bytes,
        });
    }
    let mut text = String::new();
    Read::by_ref(&mut file)
        .take(maximum_bytes_u64.saturating_add(1))
        .read_to_string(&mut text)?;
    if text.len() > maximum_bytes {
        return Err(PersistenceError::StateDocumentTooLarge {
            document,
            maximum_bytes,
        });
    }
    Ok(toml::from_str(&text)?)
}

fn write_document(
    path: &Path,
    document_name: &'static str,
    maximum_bytes: usize,
    document: &impl Serialize,
) -> Result<(), PersistenceError> {
    let mut encoded = toml::to_string_pretty(document)?;
    if !encoded.ends_with('\n') {
        encoded.push('\n');
    }
    if encoded.len() > maximum_bytes {
        return Err(PersistenceError::StateDocumentTooLarge {
            document: document_name,
            maximum_bytes,
        });
    }
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "state document has no parent directory",
        )
    })?;
    create_private_directory(parent)?;
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state.toml");
    let temporary = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let result = (|| -> Result<(), PersistenceError> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(encoded.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        set_private_file_permissions(path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
fn remap_media_id(
    media_id: &mut MediaId,
    mappings: &[LocalMoveMapping],
) -> Result<bool, PersistenceError> {
    remap_local_media_id(media_id, mappings)
        .map_err(|error| invalid_local_move_state(format!("cannot remap Local identity: {error}")))
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
fn remap_string_path(
    path: &mut String,
    mappings: &[LocalMoveMapping],
) -> Result<bool, PersistenceError> {
    let Some(remapped) = remap_local_path_prefix(Path::new(path), mappings) else {
        return Ok(false);
    };
    let remapped = remapped.to_str().ok_or_else(|| {
        invalid_local_move_state("remapped Local path cannot be represented as UTF-8")
    })?;
    remapped.clone_into(path);
    Ok(true)
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
fn remap_replay_locator(
    locator: &mut String,
    mappings: &[LocalMoveMapping],
) -> Result<bool, PersistenceError> {
    remap_local_replay_locator(locator, mappings).map_err(|error| {
        invalid_local_move_state(format!("cannot remap Local replay locator: {error}"))
    })
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
fn remap_screen(
    screen: &mut crate::domain::Screen,
    mappings: &[LocalMoveMapping],
) -> Result<bool, PersistenceError> {
    match screen {
        crate::domain::Screen::Channel(media_id) => remap_media_id(media_id, mappings),
        _ => Ok(false),
    }
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
fn remap_file_url(
    url: &mut url::Url,
    mappings: &[LocalMoveMapping],
) -> Result<bool, PersistenceError> {
    if url.scheme() != "file" {
        return Ok(false);
    }
    let path = url
        .to_file_path()
        .map_err(|()| invalid_local_move_state("stored Local file URL is invalid"))?;
    let Some(remapped) = remap_local_path_prefix(&path, mappings) else {
        return Ok(false);
    };
    *url = url::Url::from_file_path(&remapped)
        .map_err(|()| invalid_local_move_state("remapped Local file URL is invalid"))?;
    Ok(true)
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
fn remap_media_file_urls(
    media: &mut MediaItem,
    mappings: &[LocalMoveMapping],
) -> Result<bool, PersistenceError> {
    let mut changed = remap_file_url(&mut media.webpage_url, mappings)?;
    if let Some(thumbnail) = &mut media.thumbnail_url {
        changed |= remap_file_url(thumbnail, mappings)?;
    }
    for caption in &mut media.captions {
        changed |= remap_file_url(&mut caption.url, mappings)?;
    }
    Ok(changed)
}

#[cfg(any(feature = "local-rename", feature = "local-move"))]
fn remap_search_item(
    item: &mut SearchItem,
    mappings: &[LocalMoveMapping],
) -> Result<bool, PersistenceError> {
    match item {
        SearchItem::Video(video) => {
            let mut changed = false;
            if let Some(url) = &mut video.webpage_url {
                changed |= remap_file_url(url, mappings)?;
            }
            if let Some(url) = &mut video.stream_url {
                changed |= remap_file_url(url, mappings)?;
            }
            Ok(changed)
        }
        SearchItem::Channel(channel) => {
            if let Some(url) = &mut channel.webpage_url {
                remap_file_url(url, mappings)
            } else {
                Ok(false)
            }
        }
        SearchItem::PodcastEpisode(_) => Ok(false),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use tempfile::tempdir;

    use super::*;

    fn radio_favorite_fixture() -> PlaylistMediaSnapshot {
        let webpage_url =
            url::Url::parse("https://radio.example/").expect("valid fixture homepage");
        PlaylistMediaSnapshot {
            id: MediaId::new(SourceKind::Radio, "fixture-radio"),
            kind: MediaKind::LiveStream,
            title: "Fixture Radio".to_owned(),
            creator: None,
            webpage_url: webpage_url.clone(),
            thumbnail_url: None,
            duration_seconds: None,
            replay_locator: webpage_url.to_string(),
        }
    }

    fn inode(path: &Path) -> u64 {
        fs::metadata(path).expect("document metadata").ino()
    }

    fn canonical_bytes(document: &impl Serialize) -> Vec<u8> {
        let mut encoded = toml::to_string_pretty(document).expect("encode canonical fixture");
        if !encoded.ends_with('\n') {
            encoded.push('\n');
        }
        encoded.into_bytes()
    }

    fn assert_human_edited_playlists_are_rejected(
        document: &PlaylistsDocument,
        expected_reason: &str,
    ) {
        let directory = tempdir().expect("temporary directory");
        let config = Config::for_dir(directory.path().join("youta"));
        {
            let _store = FileStateStore::open(&config).expect("initialize file state");
        }
        let playlists = FilePaths::from_config(&config).playlists;
        fs::write(&playlists, canonical_bytes(document)).expect("write human-edited playlist TOML");

        let error = FileStateStore::open(&config)
            .err()
            .expect("invalid human-edited playlist state must fail open");
        assert!(matches!(
            error,
            PersistenceError::InvalidFileDocument {
                document: "playlists",
                ref reason,
            } if reason.contains(expected_reason)
        ));
    }

    #[test]
    fn radio_favorite_survives_file_restart_but_stays_out_of_playlist_ui_queries() {
        let directory = tempdir().expect("temporary directory");
        let config = Config::for_dir(directory.path().join("youta"));
        let fixture = radio_favorite_fixture();
        {
            let store = FileStateStore::open(&config).expect("file state");
            assert_eq!(
                store
                    .toggle_radio_favorite(&fixture, 1)
                    .expect("favorite station"),
                PlaylistToggleOutcome::Added
            );
            assert!(store.playlists().expect("visible playlists").is_empty());
        }

        let store = FileStateStore::open(&config).expect("reopen file state");
        assert!(
            store
                .radio_favorite_contains(&fixture.id)
                .expect("restored favorite")
        );
        let encoded =
            fs::read_to_string(config.state_dir().join("playlists.toml")).expect("playlist TOML");
        assert!(encoded.contains(RADIO_FAVORITES_PLAYLIST_NAME));
        assert!(encoded.contains("fixture-radio"));
    }

    #[cfg(feature = "yandex-music")]
    #[test]
    fn file_yandex_music_reactions_match_the_backend_contract() {
        let store = FileStateStore::open_in_memory().expect("in-memory file state");
        crate::persistence::assert_yandex_music_reaction_backend_contract(&store);
    }

    #[cfg(feature = "yandex-music")]
    #[test]
    fn yandex_music_reaction_round_trip_preserves_acknowledged_revision() {
        let directory = tempdir().expect("temporary directory");
        let config = Config::for_dir(directory.path().join("youta"));
        let liked;
        {
            let store = FileStateStore::open(&config).expect("file state");
            liked = store
                .queue_yandex_music_reaction("account-1", "track-1", YandexMusicReaction::Liked, 10)
                .expect("queue like");
            assert_eq!(liked.generation, 1);
            assert!(
                store
                    .acknowledge_yandex_music_reaction("account-1", "track-1", liked.generation,)
                    .expect("acknowledge like")
            );
        }

        let disliked;
        {
            let store = FileStateStore::open(&config).expect("reopen file state");
            assert!(
                store
                    .pending_yandex_music_reactions()
                    .expect("acknowledged reaction ledger")
                    .is_empty()
            );
            disliked = store
                .queue_yandex_music_reaction(
                    "account-1",
                    "track-1",
                    YandexMusicReaction::Disliked,
                    11,
                )
                .expect("queue dislike");
            assert_eq!(
                disliked.generation, 2,
                "restart must not reset an acknowledged reaction revision"
            );
            assert_eq!(
                store
                    .pending_yandex_music_reactions()
                    .expect("restored reactions"),
                vec![disliked.clone()]
            );
            assert!(
                store
                    .acknowledge_yandex_music_reaction("account-1", "track-1", disliked.generation,)
                    .expect("acknowledge exact generation")
            );
        }
        let store = FileStateStore::open(&config).expect("reopen cleared file state");
        assert!(
            store
                .pending_yandex_music_reactions()
                .expect("cleared reactions")
                .is_empty()
        );
    }

    #[cfg(feature = "yandex-music")]
    #[test]
    fn yandex_music_reaction_document_is_canonical_and_contains_no_secrets_or_urls() {
        let directory = tempdir().expect("temporary directory");
        let config = Config::for_dir(directory.path().join("youta"));
        let store = FileStateStore::open(&config).expect("file state");
        for (account_uid, track_id, reaction) in [
            ("account-z", "track-2", YandexMusicReaction::Neutral),
            ("account-a", "track-9", YandexMusicReaction::Liked),
            ("account-a", "track-1", YandexMusicReaction::Disliked),
        ] {
            store
                .queue_yandex_music_reaction(account_uid, track_id, reaction, 1)
                .expect("queue reaction");
        }
        drop(store);

        let path = config.state_dir().join("yandex-music.toml");
        let encoded = fs::read_to_string(path).expect("Yandex Music reaction TOML");
        let document: YandexMusicReactionsDocument =
            toml::from_str(&encoded).expect("canonical reaction document");
        assert_eq!(
            document
                .reactions
                .iter()
                .map(|pending| (pending.account_uid.as_str(), pending.track_id.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("account-a", "track-1"),
                ("account-a", "track-9"),
                ("account-z", "track-2"),
            ]
        );
        for forbidden in [
            "token",
            "oauth",
            "signed_url",
            "stream_url",
            "download_url",
            "https://",
        ] {
            assert!(
                !encoded.contains(forbidden),
                "reaction state must not contain {forbidden:?}"
            );
        }
    }

    #[cfg(feature = "yandex-music")]
    #[test]
    fn yandex_music_reaction_document_rejects_unknown_secret_fields() {
        let directory = tempdir().expect("temporary directory");
        let config = Config::for_dir(directory.path().join("youta"));
        {
            let _store = FileStateStore::open(&config).expect("initialize file state");
        }
        fs::write(
            config.state_dir().join("yandex-music.toml"),
            "format_version = 1\ntoken = \"must-not-be-stored\"\nreactions = []\n",
        )
        .expect("human-edited reaction TOML");

        assert!(matches!(
            FileStateStore::open(&config),
            Err(PersistenceError::TomlDecode(_))
        ));
    }

    #[test]
    fn human_edited_radio_favorites_require_the_reserved_id_and_name_pair() {
        let record = |id: &str, name: &str| PlaylistRecord {
            id: id.to_owned(),
            name: name.to_owned(),
            description: None,
            created_at: 1,
            updated_at: 1,
            entries: Vec::new(),
        };

        assert_human_edited_playlists_are_rejected(
            &PlaylistsDocument {
                format_version: FILE_FORMAT_VERSION,
                next_local_id: 1,
                playlists: vec![record(
                    RADIO_FAVORITES_PLAYLIST_ID,
                    "A different hidden name",
                )],
            },
            "Radio favorites ID must retain the Radio favorites name",
        );
        assert_human_edited_playlists_are_rejected(
            &PlaylistsDocument {
                format_version: FILE_FORMAT_VERSION,
                next_local_id: 2,
                playlists: vec![record("local:1", RADIO_FAVORITES_PLAYLIST_NAME)],
            },
            "Radio favorites name is reserved",
        );
    }

    #[test]
    fn human_edited_radio_favorites_reject_non_radio_playlist_entries() {
        let mut non_radio = radio_favorite_fixture();
        non_radio.id = MediaId::new(SourceKind::YouTube, "abcdefghijk");
        non_radio.kind = MediaKind::Video;
        non_radio.webpage_url =
            url::Url::parse("https://www.youtube.com/watch?v=abcdefghijk").expect("YouTube URL");
        non_radio.replay_locator = non_radio.webpage_url.to_string();
        let document = PlaylistsDocument {
            format_version: FILE_FORMAT_VERSION,
            next_local_id: 1,
            playlists: vec![PlaylistRecord {
                id: RADIO_FAVORITES_PLAYLIST_ID.to_owned(),
                name: RADIO_FAVORITES_PLAYLIST_NAME.to_owned(),
                description: None,
                created_at: 1,
                updated_at: 1,
                entries: vec![PlaylistEntry {
                    media: non_radio,
                    segment: None,
                    added_at: 1,
                }],
            }],
        };

        assert_human_edited_playlists_are_rejected(
            &document,
            "Radio favorites accept only live Radio station snapshots",
        );
    }

    fn assert_regenerable_documents_are_empty(store: &FileStateStore, paths: &FilePaths) {
        {
            let documents = store.lock().expect("recovered documents");
            assert!(documents.runtime.session.is_none());
            assert!(documents.runtime.session_updated_at.is_none());
            assert!(documents.playback_checkpoint.is_empty());
            assert!(documents.searches.youtube.is_none());
            assert!(documents.searches.youtube_music.is_none());
            assert!(documents.searches.bandcamp.is_none());
            assert!(documents.searches.apple_podcasts.is_none());
            assert!(documents.provider_cache.subscription_items.is_empty());
            assert!(documents.provider_cache.metadata.is_empty());
            assert!(documents.provider_cache.channel_summaries.is_empty());
            assert!(documents.provider_cache.wikidata.is_empty());
        }
        assert_eq!(
            fs::read(&paths.runtime).expect("reset runtime"),
            canonical_bytes(&RuntimeDocument::empty())
        );
        assert_eq!(
            fs::read(&paths.playback_checkpoint).expect("reset checkpoint"),
            canonical_bytes(&PlaybackCheckpointDocument::empty())
        );
        assert_eq!(
            fs::read(&paths.searches).expect("reset searches"),
            canonical_bytes(&SearchCacheDocument::empty())
        );
        assert_eq!(
            fs::read(&paths.provider_cache).expect("reset provider cache"),
            canonical_bytes(&ProviderCacheDocument::empty())
        );
    }

    fn assert_integer_out_of_range<T>(
        result: Result<T, PersistenceError>,
        expected_field: &'static str,
    ) {
        assert!(matches!(
            result,
            Err(PersistenceError::IntegerOutOfRange { field })
                if field == expected_field
        ));
    }

    fn history_fixture(external_id: &str) -> HistoryEntry {
        HistoryEntry {
            id: 0,
            media_id: MediaId::new(SourceKind::YouTube, external_id),
            title: "Numeric range fixture".to_owned(),
            replay_locator: None,
            started_at: 1,
            last_played_at: 1,
            position_seconds: 0,
            duration_seconds: None,
            finished: false,
        }
    }

    fn assert_sqlite_integer_mutation_bounds(store: &dyn StateBackend) {
        let overflow = SQLITE_INTEGER_MAX_U64 + 1;

        let mut position_progress = PlaybackProgress::new(
            MediaId::new(SourceKind::YouTube, "overflow-progress-position"),
            None,
            1,
        );
        position_progress.position_seconds = overflow;
        assert_integer_out_of_range(
            store.upsert_progress(&position_progress),
            "position_seconds",
        );

        let duration_progress = PlaybackProgress::new(
            MediaId::new(SourceKind::YouTube, "overflow-progress-duration"),
            Some(overflow),
            1,
        );
        assert_integer_out_of_range(
            store.upsert_progress(&duration_progress),
            "duration_seconds",
        );
        assert_integer_out_of_range(
            store.checkpoint_playback(&position_progress, &SourceKind::YouTube, 0),
            "position_seconds",
        );

        let mut position_history = history_fixture("overflow-history-position");
        position_history.position_seconds = overflow;
        assert_integer_out_of_range(store.insert_history(&position_history), "position_seconds");

        let mut duration_history = history_fixture("overflow-history-duration");
        duration_history.duration_seconds = Some(overflow);
        assert_integer_out_of_range(store.insert_history(&duration_history), "duration_seconds");

        let bookmark = Bookmark {
            id: 0,
            media_id: MediaId::new(SourceKind::YouTube, "overflow-bookmark"),
            position_seconds: overflow,
            label: None,
            created_at: 1,
        };
        assert_integer_out_of_range(store.insert_bookmark(&bookmark), "position_seconds");

        assert_integer_out_of_range(
            store.add_listen_seconds(&SourceKind::YouTube, overflow),
            "listen seconds",
        );

        let checkpoint_progress = PlaybackProgress::new(
            MediaId::new(SourceKind::YouTube, "overflow-checkpoint-listen"),
            Some(60),
            1,
        );
        assert_integer_out_of_range(
            store.checkpoint_playback(&checkpoint_progress, &SourceKind::YouTube, overflow),
            "listen seconds",
        );
        assert_integer_out_of_range(
            store.checkpoint_listening(&SourceKind::Radio, overflow),
            "listen seconds",
        );
    }

    fn assert_listening_only_checkpoint_semantics(store: &dyn StateBackend) {
        let source = SourceKind::Radio;
        let nonexistent_progress = MediaId::new(source.clone(), "live-radio-has-no-position");

        store
            .checkpoint_listening(&source, 0)
            .expect("zero listening checkpoint");
        assert_eq!(
            store.listened_seconds(&source).expect("empty total"),
            0,
            "zero delta must not create a listening total"
        );
        assert!(
            store
                .listen_totals()
                .expect("empty totals")
                .iter()
                .all(|total| total.source != source),
            "zero delta must not create a statistics row"
        );

        store
            .checkpoint_listening(&source, 25)
            .expect("first live listening checkpoint");
        store
            .checkpoint_listening(&source, 35)
            .expect("second live listening checkpoint");
        assert_eq!(store.listened_seconds(&source).expect("live total"), 60);
        assert!(
            store
                .progress(&nonexistent_progress)
                .expect("live progress lookup")
                .is_none(),
            "listening-only checkpoints must never invent playback progress"
        );
    }

    fn assert_listening_only_checkpoint_overflow_rolls_back(store: &dyn StateBackend) {
        let source = SourceKind::Radio;
        let nonexistent_progress = MediaId::new(source.clone(), "overflow-has-no-position");
        store
            .checkpoint_listening(&source, SQLITE_INTEGER_MAX_U64)
            .expect("maximum listening checkpoint");

        assert_integer_out_of_range(store.checkpoint_listening(&source, 1), "listen seconds");
        assert_eq!(
            store
                .listened_seconds(&source)
                .expect("unchanged maximum total"),
            SQLITE_INTEGER_MAX_U64
        );
        assert!(
            store
                .progress(&nonexistent_progress)
                .expect("overflow progress lookup")
                .is_none(),
            "a rejected listening-only checkpoint must not create progress"
        );
    }

    fn assert_cumulative_listen_total_bound(store: &dyn StateBackend) {
        store
            .add_listen_seconds(&SourceKind::YouTube, SQLITE_INTEGER_MAX_U64)
            .expect("maximum SQLite integer");
        let checkpoint = PlaybackProgress::new(
            MediaId::new(SourceKind::YouTube, "maximum-listen-total"),
            Some(60),
            1,
        );

        assert_integer_out_of_range(
            store.checkpoint_playback(&checkpoint, &SourceKind::YouTube, 1),
            "listen seconds",
        );
        assert!(
            store
                .progress(&checkpoint.media_id)
                .expect("rolled-back checkpoint progress")
                .is_none()
        );
        assert_integer_out_of_range(
            store.add_listen_seconds(&SourceKind::YouTube, 1),
            "listen seconds",
        );
        assert_eq!(
            store
                .listened_seconds(&SourceKind::YouTube)
                .expect("unchanged total"),
            SQLITE_INTEGER_MAX_U64
        );
    }

    #[test]
    fn file_mutations_match_sqlite_integer_bounds() {
        let store = FileStateStore::open_in_memory().expect("open file state");
        assert_sqlite_integer_mutation_bounds(&store);
    }

    #[cfg(feature = "sqlite-state")]
    #[test]
    fn file_and_sqlite_mutations_reject_the_same_u64_overflows() {
        let files = FileStateStore::open_in_memory().expect("open file state");
        let sqlite = SqliteStateStore::open_in_memory().expect("open SQLite state");

        assert_sqlite_integer_mutation_bounds(&files);
        assert_sqlite_integer_mutation_bounds(&sqlite);
        assert_cumulative_listen_total_bound(&files);
        assert_cumulative_listen_total_bound(&sqlite);
    }

    #[test]
    fn file_listen_total_cannot_cross_the_sqlite_integer_maximum() {
        let store = FileStateStore::open_in_memory().expect("open file state");
        assert_cumulative_listen_total_bound(&store);
    }

    #[test]
    fn file_listening_only_checkpoint_has_live_stream_semantics() {
        let semantics = FileStateStore::open_in_memory().expect("open file state");
        assert_listening_only_checkpoint_semantics(&semantics);
        let overflow = FileStateStore::open_in_memory().expect("open overflow file state");
        assert_listening_only_checkpoint_overflow_rolls_back(&overflow);
    }

    #[cfg(feature = "sqlite-state")]
    #[test]
    fn file_and_sqlite_listening_only_checkpoints_have_parity() {
        let files = FileStateStore::open_in_memory().expect("open file state");
        let sqlite = SqliteStateStore::open_in_memory().expect("open SQLite state");
        assert_listening_only_checkpoint_semantics(&files);
        assert_listening_only_checkpoint_semantics(&sqlite);

        let files = FileStateStore::open_in_memory().expect("open overflow file state");
        let sqlite = SqliteStateStore::open_in_memory().expect("open overflow SQLite state");
        assert_listening_only_checkpoint_overflow_rolls_back(&files);
        assert_listening_only_checkpoint_overflow_rolls_back(&sqlite);
    }

    #[test]
    fn file_listening_only_checkpoint_is_bounded_and_recovers_idempotently() {
        let temporary = tempdir().expect("temporary state root");
        let config = Config::for_dir(temporary.path().join("youta"));
        let paths = FilePaths::from_config(&config);
        let source = SourceKind::Radio;
        let nonexistent_progress = MediaId::new(source.clone(), "live-radio-has-no-position");
        {
            let store = FileStateStore::open(&config).expect("file state");
            let before_zero =
                file_generation(&paths.playback_checkpoint).expect("checkpoint generation");
            store
                .checkpoint_listening(&source, 0)
                .expect("zero checkpoint");
            assert_eq!(
                file_generation(&paths.playback_checkpoint)
                    .expect("unchanged checkpoint generation"),
                before_zero,
                "a zero delta must not replace the checkpoint file"
            );

            store
                .checkpoint_listening(&source, 30)
                .expect("live checkpoint");
            let checkpoint_bytes =
                fs::read(&paths.playback_checkpoint).expect("live checkpoint bytes");
            assert!(checkpoint_bytes.len() <= MAX_PLAYBACK_CHECKPOINT_DOCUMENT_BYTES);
            let checkpoint: PlaybackCheckpointDocument = toml::from_str(
                std::str::from_utf8(&checkpoint_bytes).expect("UTF-8 live checkpoint"),
            )
            .expect("live checkpoint TOML");
            assert!(checkpoint.progress.is_none());
            assert_eq!(
                checkpoint
                    .listen_total
                    .as_ref()
                    .expect("absolute listening target")
                    .total_seconds,
                30
            );

            let before_overflow = checkpoint_bytes;
            store
                .checkpoint_listening(&source, SQLITE_INTEGER_MAX_U64 - 30)
                .expect("maximum live checkpoint");
            let maximum_checkpoint =
                fs::read(&paths.playback_checkpoint).expect("maximum checkpoint bytes");
            assert_ne!(maximum_checkpoint, before_overflow);
            assert_integer_out_of_range(store.checkpoint_listening(&source, 1), "listen seconds");
            assert_eq!(
                fs::read(&paths.playback_checkpoint).expect("rolled-back checkpoint bytes"),
                maximum_checkpoint,
                "overflow must not rewrite the pending checkpoint"
            );
        }

        {
            let recovered = FileStateStore::open(&config).expect("recover live checkpoint");
            assert_eq!(
                recovered
                    .listened_seconds(&source)
                    .expect("recovered listening total"),
                SQLITE_INTEGER_MAX_U64
            );
            assert!(
                recovered
                    .progress(&nonexistent_progress)
                    .expect("recovered progress lookup")
                    .is_none()
            );
        }
        let replayed = FileStateStore::open(&config).expect("reopen recovered live checkpoint");
        assert_eq!(
            replayed
                .listened_seconds(&source)
                .expect("idempotently recovered listening total"),
            SQLITE_INTEGER_MAX_U64
        );
        assert!(
            replayed
                .progress(&nonexistent_progress)
                .expect("replayed progress lookup")
                .is_none()
        );
    }

    #[test]
    fn file_document_validation_rejects_sqlite_integer_overflows() {
        let store = FileStateStore::open_in_memory().expect("open file state");
        let base = store.lock().expect("file documents").clone();
        let overflow = SQLITE_INTEGER_MAX_U64 + 1;

        let mut progress = base.clone();
        progress.progress.progress.push(PlaybackProgress {
            media_id: MediaId::new(SourceKind::YouTube, "overflow-progress"),
            position_seconds: overflow,
            duration_seconds: Some(overflow),
            played_override: None,
            updated_at: 1,
        });
        assert!(matches!(
            validate_file_documents(&progress),
            Err(PersistenceError::InvalidFileDocument {
                document: "progress",
                ..
            })
        ));

        let mut history = base.clone();
        let mut history_row = history_fixture("overflow-history");
        history_row.id = 1;
        history_row.position_seconds = overflow;
        history_row.duration_seconds = Some(overflow);
        history.history.next_id = 2;
        history.history.history.push(history_row);
        assert!(matches!(
            validate_file_documents(&history),
            Err(PersistenceError::InvalidFileDocument {
                document: "history",
                ..
            })
        ));

        let mut bookmarks = base.clone();
        bookmarks.bookmarks.next_id = 2;
        bookmarks.bookmarks.bookmarks.push(Bookmark {
            id: 1,
            media_id: MediaId::new(SourceKind::YouTube, "overflow-bookmark"),
            position_seconds: overflow,
            label: None,
            created_at: 1,
        });
        assert!(matches!(
            validate_file_documents(&bookmarks),
            Err(PersistenceError::InvalidFileDocument {
                document: "bookmarks",
                ..
            })
        ));

        let mut statistics = base.clone();
        statistics.statistics.listen_totals.push(ListenTotal {
            source: SourceKind::YouTube,
            total_seconds: overflow,
        });
        assert!(matches!(
            validate_file_documents(&statistics),
            Err(PersistenceError::InvalidFileDocument {
                document: "statistics",
                ..
            })
        ));

        let mut checkpoint = base;
        checkpoint.playback_checkpoint.listen_total = Some(ListenTotal {
            source: SourceKind::YouTube,
            total_seconds: overflow,
        });
        assert!(matches!(
            validate_file_documents(&checkpoint),
            Err(PersistenceError::InvalidFileDocument {
                document: "playback checkpoint",
                ..
            })
        ));
    }

    #[test]
    fn private_note_upsert_is_exact_atomic_and_preserves_creation_time() {
        let store = FileStateStore::open_in_memory().expect("open file state");
        let media_id = MediaId::new(SourceKind::YouTube, "private-note");
        let media_target = CommentTarget::Media {
            media_id: media_id.clone(),
        };
        let source_target = CommentTarget::Source {
            source_id: media_id,
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
    fn progress_checkpoint_replaces_only_progress_document() {
        let temporary = tempdir().expect("temporary state root");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = FileStateStore::open(&config).expect("file state");
        let paths = FilePaths::from_config(&config);
        let unrelated = [
            paths.history.clone(),
            paths.notes.clone(),
            paths.bookmarks.clone(),
            paths.statistics.clone(),
            paths.local_moves.clone(),
            paths.playlists.clone(),
        ];
        let unrelated_before = unrelated
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    inode(path),
                    fs::read(path).expect("document bytes"),
                )
            })
            .collect::<Vec<_>>();
        let progress_inode = inode(&paths.progress);

        store
            .upsert_progress(&PlaybackProgress::new(
                MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ"),
                Some(213),
                1,
            ))
            .expect("save progress");

        assert_ne!(
            inode(&paths.progress),
            progress_inode,
            "atomic progress publication must replace progress.toml"
        );
        for (path, original_inode, original_bytes) in unrelated_before {
            assert_eq!(
                inode(&path),
                original_inode,
                "{} must not be replaced by a progress checkpoint",
                path.display()
            );
            assert_eq!(
                fs::read(&path).expect("unchanged document bytes"),
                original_bytes,
                "{} must not be rewritten by a progress checkpoint",
                path.display()
            );
        }
    }

    #[test]
    fn periodic_playback_checkpoint_is_bounded_and_independent_of_progress_history() {
        let temporary = tempdir().expect("temporary state root");
        let config = Config::for_dir(temporary.path().join("many"));
        let store = FileStateStore::open(&config).expect("file state");
        let paths = FilePaths::from_config(&config);
        let mut seeded = ProgressDocument::empty();
        seeded.progress = (0..10_000)
            .map(|index| {
                PlaybackProgress::new(
                    MediaId::new(SourceKind::YouTube, format!("historical-{index:05}")),
                    Some(300),
                    1,
                )
            })
            .collect();
        seeded.canonicalize();
        store.persist_progress(&seeded).expect("seed progress once");
        store.lock().expect("documents").progress = seeded;
        let progress_before = fs::read(&paths.progress).expect("progress bytes");
        let statistics_before = fs::read(&paths.statistics).expect("statistics bytes");
        let progress_inode = inode(&paths.progress);
        let statistics_inode = inode(&paths.statistics);
        let mut current = PlaybackProgress::new(
            MediaId::new(SourceKind::YouTube, "current-checkpoint"),
            Some(600),
            10,
        );
        current.record_position(123, 11);

        store
            .checkpoint_playback(&current, &SourceKind::YouTube, 30)
            .expect("periodic checkpoint");
        let checkpoint_with_history =
            fs::read(&paths.playback_checkpoint).expect("checkpoint bytes");

        assert!(checkpoint_with_history.len() <= MAX_PLAYBACK_CHECKPOINT_DOCUMENT_BYTES);
        assert_eq!(inode(&paths.progress), progress_inode);
        assert_eq!(inode(&paths.statistics), statistics_inode);
        assert_eq!(
            fs::read(&paths.progress).expect("unchanged progress"),
            progress_before
        );
        assert_eq!(
            fs::read(&paths.statistics).expect("unchanged statistics"),
            statistics_before
        );
        assert_eq!(
            store.progress(&current.media_id).expect("overlay progress"),
            Some(current.clone())
        );
        assert_eq!(
            store
                .listened_seconds(&SourceKind::YouTube)
                .expect("overlay statistics"),
            30
        );

        let baseline_config = Config::for_dir(temporary.path().join("empty"));
        let baseline = FileStateStore::open(&baseline_config).expect("baseline state");
        baseline
            .checkpoint_playback(&current, &SourceKind::YouTube, 30)
            .expect("baseline checkpoint");
        let checkpoint_without_history =
            fs::read(FilePaths::from_config(&baseline_config).playback_checkpoint)
                .expect("baseline checkpoint bytes");
        assert_eq!(
            checkpoint_with_history, checkpoint_without_history,
            "periodic serialization must not scale with canonical progress rows"
        );
    }

    #[test]
    fn checkpoint_recovery_after_progress_only_publish_is_idempotent() {
        let temporary = tempdir().expect("temporary state root");
        let config = Config::for_dir(temporary.path().join("youta"));
        let paths = FilePaths::from_config(&config);
        let media_id = MediaId::new(SourceKind::YouTube, "crash-after-progress");
        let mut checkpoint = PlaybackProgress::new(media_id.clone(), Some(200), 10);
        checkpoint.record_position(75, 11);
        {
            let store = FileStateStore::open(&config).expect("file state");
            store
                .checkpoint_playback(&checkpoint, &SourceKind::YouTube, 30)
                .expect("periodic checkpoint");
        }
        let checkpoint_bytes =
            fs::read(&paths.playback_checkpoint).expect("pending checkpoint bytes");
        let mut progress = ProgressDocument::empty();
        progress.progress.push(checkpoint.clone());
        write_document(
            &paths.progress,
            "progress",
            MAX_PROGRESS_DOCUMENT_BYTES,
            &progress,
        )
        .expect("simulate first canonical publication");

        {
            let recovered = FileStateStore::open(&config).expect("recover checkpoint");
            assert_eq!(
                recovered.progress(&media_id).expect("recovered progress"),
                Some(checkpoint.clone())
            );
            assert_eq!(
                recovered
                    .listened_seconds(&SourceKind::YouTube)
                    .expect("recovered statistics"),
                30
            );
        }

        fs::write(&paths.playback_checkpoint, &checkpoint_bytes)
            .expect("simulate crash before checkpoint clear");
        let replayed = FileStateStore::open(&config).expect("replay checkpoint");
        assert_eq!(
            replayed
                .listened_seconds(&SourceKind::YouTube)
                .expect("idempotent statistics"),
            30,
            "an absolute target must not double-count replayed listening time"
        );
        assert_eq!(
            replayed.progress(&media_id).expect("idempotent progress"),
            Some(checkpoint)
        );
    }

    #[test]
    fn checkpoint_recovery_after_statistics_only_publish_restores_backward_seek() {
        let temporary = tempdir().expect("temporary state root");
        let config = Config::for_dir(temporary.path().join("youta"));
        let paths = FilePaths::from_config(&config);
        let media_id = MediaId::new(SourceKind::YouTube, "backward-seek");
        let mut canonical = PlaybackProgress::new(media_id.clone(), Some(200), 10);
        canonical.record_position(120, 10);
        let mut checkpoint = canonical.clone();
        checkpoint.record_position(20, 10);
        {
            let store = FileStateStore::open(&config).expect("file state");
            store
                .upsert_progress(&canonical)
                .expect("canonical progress");
            store
                .checkpoint_playback(&checkpoint, &SourceKind::YouTube, 45)
                .expect("periodic checkpoint");
        }
        let statistics = StatisticsDocument {
            format_version: FILE_FORMAT_VERSION,
            listen_totals: vec![ListenTotal {
                source: SourceKind::YouTube,
                total_seconds: 45,
            }],
        };
        write_document(
            &paths.statistics,
            "statistics",
            MAX_STATISTICS_DOCUMENT_BYTES,
            &statistics,
        )
        .expect("simulate statistics publication");

        let recovered = FileStateStore::open(&config).expect("recover checkpoint");
        assert_eq!(
            recovered
                .progress(&media_id)
                .expect("recovered backward seek")
                .expect("progress")
                .position_seconds,
            20,
            "the checkpoint wins equal timestamps so backward seeks survive"
        );
        assert_eq!(
            recovered
                .listened_seconds(&SourceKind::YouTube)
                .expect("non-duplicated statistics"),
            45
        );
    }

    #[test]
    fn clean_flush_and_explicit_deletion_cannot_resurrect_checkpoint_state() {
        let temporary = tempdir().expect("temporary state root");
        let config = Config::for_dir(temporary.path().join("youta"));
        let paths = FilePaths::from_config(&config);
        let media_id = MediaId::new(SourceKind::YouTube, "clean-boundary");
        let mut progress = PlaybackProgress::new(media_id.clone(), Some(90), 1);
        progress.record_position(40, 2);
        {
            let store = FileStateStore::open(&config).expect("file state");
            store
                .checkpoint_playback(&progress, &SourceKind::YouTube, 30)
                .expect("periodic checkpoint");
            store
                .flush_playback_checkpoint()
                .expect("clean lifecycle flush");
            let empty: PlaybackCheckpointDocument = toml::from_str(
                &fs::read_to_string(&paths.playback_checkpoint).expect("checkpoint text"),
            )
            .expect("empty checkpoint document");
            assert!(empty.is_empty());

            store
                .checkpoint_playback(&progress, &SourceKind::YouTube, 15)
                .expect("new pending checkpoint");
            assert!(store.delete_progress(&media_id).expect("delete progress"));
            assert!(
                store
                    .reset_listen_seconds(&SourceKind::YouTube)
                    .expect("reset statistics")
            );
        }

        let reopened = FileStateStore::open(&config).expect("reopen after explicit removals");
        assert!(
            reopened
                .progress(&media_id)
                .expect("deleted progress")
                .is_none()
        );
        assert_eq!(
            reopened
                .listened_seconds(&SourceKind::YouTube)
                .expect("reset total"),
            0
        );
    }

    #[cfg(any(feature = "local-rename", feature = "local-move"))]
    #[test]
    fn local_remap_flushes_and_moves_pending_checkpoint_progress() {
        let temporary = tempdir().expect("temporary state root");
        let config = Config::for_dir(temporary.path().join("youta"));
        let source = temporary.path().join("old").join("track.flac");
        let target = temporary.path().join("new").join("track.flac");
        let source_id = MediaId::new(SourceKind::Local, source.to_string_lossy());
        let target_id = MediaId::new(SourceKind::Local, target.to_string_lossy());
        let mut progress = PlaybackProgress::new(source_id.clone(), Some(100), 1);
        progress.record_position(55, 2);
        {
            let store = FileStateStore::open(&config).expect("file state");
            store
                .checkpoint_playback(&progress, &SourceKind::Local, 30)
                .expect("pending local checkpoint");
            let report = store
                .remap_local_move_state(&[LocalMoveMapping {
                    source: source.clone(),
                    target: target.clone(),
                }])
                .expect("remap pending progress");
            assert_eq!(report.playback_progress, 1);
            assert!(store.progress(&source_id).expect("old progress").is_none());
            assert_eq!(
                store
                    .progress(&target_id)
                    .expect("new progress")
                    .expect("remapped progress")
                    .position_seconds,
                55
            );
        }

        let reopened = FileStateStore::open(&config).expect("reopen remapped state");
        assert!(
            reopened
                .progress(&source_id)
                .expect("old progress")
                .is_none()
        );
        assert_eq!(
            reopened
                .progress(&target_id)
                .expect("new progress")
                .expect("durable remapped progress")
                .position_seconds,
            55
        );
        assert_eq!(
            reopened
                .listened_seconds(&SourceKind::Local)
                .expect("durable listening time"),
            30
        );
    }

    #[test]
    fn file_backend_leaves_existing_sqlite_database_untouched() {
        let temporary = tempdir().expect("temporary state root");
        let config = Config::for_dir(temporary.path().join("youta"));
        fs::create_dir_all(config.config_dir()).expect("configuration root");
        let sqlite = config.database_file();
        let sentinel = b"legacy sqlite fixture";
        fs::write(&sqlite, sentinel).expect("legacy database fixture");

        let store = FileStateStore::open(&config).expect("file state beside legacy database");

        assert_eq!(store.backend_name(), "files");
        assert_eq!(fs::read(sqlite).expect("legacy database"), sentinel);
        assert!(config.state_dir().join("manifest.toml").is_file());
    }

    #[test]
    fn published_manifest_requires_every_authoritative_document() {
        let temporary = tempdir().expect("temporary state root");
        let authoritative_names = [
            "progress",
            "history",
            "notes",
            "bookmarks",
            "statistics",
            "Local move journal",
            "playlists",
        ];

        for (missing_index, expected_document) in authoritative_names.iter().enumerate() {
            let config = Config::for_dir(temporary.path().join(format!("missing-{missing_index}")));
            {
                let _store = FileStateStore::open(&config).expect("initialize complete state");
            }
            let paths = FilePaths::from_config(&config);
            let authoritative_paths = [
                paths.progress.clone(),
                paths.history.clone(),
                paths.notes.clone(),
                paths.bookmarks.clone(),
                paths.statistics.clone(),
                paths.local_moves.clone(),
                paths.playlists.clone(),
            ];
            let missing_path = authoritative_paths[missing_index].clone();
            let preserved = std::iter::once(paths.manifest.clone())
                .chain(
                    authoritative_paths
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| *index != missing_index)
                        .map(|(_, path)| path.clone()),
                )
                .map(|path| {
                    let bytes = fs::read(&path).expect("authoritative fixture bytes");
                    (path, bytes)
                })
                .collect::<Vec<_>>();
            fs::remove_file(&missing_path).expect("delete authoritative fixture");

            let error = FileStateStore::open(&config)
                .err()
                .expect("missing authoritative state must fail open");
            assert!(matches!(
                error,
                PersistenceError::InvalidFileDocument {
                    document,
                    ref reason,
                } if document == *expected_document
                    && reason.contains("required file is missing")
            ));
            assert!(
                !missing_path.exists(),
                "failed open must not recreate {}",
                missing_path.display()
            );
            for (path, original) in &preserved {
                assert_eq!(
                    fs::read(path).expect("preserved authoritative bytes"),
                    *original,
                    "failed open must not rewrite {}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn corrupt_regenerable_documents_are_quarantined_and_reset_only() {
        let temporary = tempdir().expect("temporary state root");
        let config = Config::for_dir(temporary.path().join("youta"));
        {
            let _store = FileStateStore::open(&config).expect("initialize complete state");
        }
        let paths = FilePaths::from_config(&config);
        let authoritative = [
            paths.progress.clone(),
            paths.history.clone(),
            paths.notes.clone(),
            paths.bookmarks.clone(),
            paths.statistics.clone(),
            paths.local_moves.clone(),
            paths.playlists.clone(),
        ]
        .into_iter()
        .map(|path| {
            let generation = file_generation(&path).expect("authoritative generation");
            let bytes = fs::read(&path).expect("authoritative bytes");
            (path, generation, bytes)
        })
        .collect::<Vec<_>>();
        let corrupt = [
            (paths.runtime.clone(), b"format_version = [\n".as_slice()),
            (
                paths.playback_checkpoint.clone(),
                b"format_version = 1\ninvalid = \"\xff\"\n".as_slice(),
            ),
            (paths.searches.clone(), b"format_version = 2\n".as_slice()),
            (
                paths.provider_cache.clone(),
                b"format_version = 0\n".as_slice(),
            ),
        ];
        for (path, bytes) in &corrupt {
            fs::write(path, bytes).expect("write corrupt disposable fixture");
        }

        let reopened =
            FileStateStore::open(&config).expect("corrupt disposable state must recover");
        assert_regenerable_documents_are_empty(&reopened, &paths);
        for (path, bytes) in corrupt {
            let quarantine =
                regenerable_quarantine_path(&path, 0).expect("quarantine fixture path");
            assert_eq!(
                fs::read(&quarantine).expect("quarantined corrupt bytes"),
                bytes,
                "{} must preserve the exact corrupt bytes",
                quarantine.display()
            );
            assert_eq!(
                fs::metadata(&quarantine)
                    .expect("quarantine metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "{} must remain private",
                quarantine.display()
            );
        }
        for (path, generation, bytes) in authoritative {
            assert_eq!(
                file_generation(&path).expect("unchanged authoritative generation"),
                generation,
                "{} must not be replaced while disposable state recovers",
                path.display()
            );
            assert_eq!(
                fs::read(&path).expect("unchanged authoritative bytes"),
                bytes,
                "{} must not be rewritten while disposable state recovers",
                path.display()
            );
        }
    }

    #[test]
    fn corrupt_authoritative_documents_fail_without_quarantine_or_reset() {
        let temporary = tempdir().expect("temporary state root");
        let authoritative_names = [
            "progress",
            "history",
            "notes",
            "bookmarks",
            "statistics",
            "Local move journal",
            "playlists",
        ];
        let corrupt = b"format_version = [\n";

        for (corrupt_index, expected_document) in authoritative_names.iter().enumerate() {
            let config = Config::for_dir(temporary.path().join(format!("corrupt-{corrupt_index}")));
            {
                let _store = FileStateStore::open(&config).expect("initialize complete state");
            }
            let paths = FilePaths::from_config(&config);
            let authoritative_paths = [
                paths.progress,
                paths.history,
                paths.notes,
                paths.bookmarks,
                paths.statistics,
                paths.local_moves,
                paths.playlists,
            ];
            let corrupt_path = &authoritative_paths[corrupt_index];
            fs::write(corrupt_path, corrupt).expect("write corrupt authoritative fixture");

            let error = FileStateStore::open(&config)
                .err()
                .expect("corrupt authoritative state must fail open");
            assert!(
                matches!(error, PersistenceError::TomlDecode(_)),
                "{expected_document} must report its malformed TOML"
            );
            assert_eq!(
                fs::read(corrupt_path).expect("preserved authoritative corruption"),
                corrupt
            );
            assert!(
                !regenerable_quarantine_path(corrupt_path, 0)
                    .expect("candidate quarantine path")
                    .exists(),
                "{expected_document} must never be quarantined automatically"
            );
        }
    }

    #[test]
    fn regenerable_quarantine_never_overwrites_an_earlier_copy() {
        let temporary = tempdir().expect("temporary state root");
        let config = Config::for_dir(temporary.path().join("youta"));
        {
            let _store = FileStateStore::open(&config).expect("initialize complete state");
        }
        let runtime = FilePaths::from_config(&config).runtime;
        let first_quarantine =
            regenerable_quarantine_path(&runtime, 0).expect("first quarantine path");
        let second_quarantine =
            regenerable_quarantine_path(&runtime, 1).expect("second quarantine path");
        let earlier = b"earlier quarantined runtime";
        let corrupt = b"format_version = [\n";
        fs::write(&first_quarantine, earlier).expect("earlier quarantine fixture");
        fs::write(&runtime, corrupt).expect("new corrupt runtime fixture");

        let _store = FileStateStore::open(&config).expect("recover with occupied quarantine name");

        assert_eq!(
            fs::read(first_quarantine).expect("preserved earlier quarantine"),
            earlier
        );
        assert_eq!(
            fs::read(second_quarantine).expect("new collision-safe quarantine"),
            corrupt
        );
        assert_eq!(
            fs::read(runtime).expect("reset runtime"),
            canonical_bytes(&RuntimeDocument::empty())
        );
    }

    #[test]
    fn file_backend_rejects_a_second_live_writer() {
        let temporary = tempdir().expect("temporary state root");
        let config = Config::for_dir(temporary.path().join("youta"));
        let first = FileStateStore::open(&config).expect("first file state");

        let error = FileStateStore::open(&config)
            .err()
            .expect("second writer must be rejected");
        assert!(matches!(error, PersistenceError::FileStateAlreadyOpen));

        drop(first);
        FileStateStore::open(&config).expect("lock released with first store");
    }

    #[cfg(unix)]
    #[test]
    fn dropping_file_backend_unlocks_an_inherited_descriptor() {
        let temporary = tempdir().expect("temporary state root");
        let config = Config::for_dir(temporary.path().join("youta"));
        let first = FileStateStore::open(&config).expect("first file state");
        let inherited = first
            .state_lock
            .as_ref()
            .expect("disk-backed lock")
            .try_clone()
            .expect("inherited descriptor fixture");

        drop(first);
        let reopened =
            FileStateStore::open(&config).expect("explicit unlock releases inherited descriptor");

        drop(inherited);
        drop(reopened);
    }

    #[test]
    fn file_backend_does_not_overwrite_an_external_document_edit() {
        let temporary = tempdir().expect("temporary state root");
        let config = Config::for_dir(temporary.path().join("youta"));
        let store = FileStateStore::open(&config).expect("file state");
        let progress_path = FilePaths::from_config(&config).progress;
        let externally_edited = b"format_version = 1\n# external edit\nprogress = []\n";
        fs::write(&progress_path, externally_edited).expect("external edit fixture");

        let error = store
            .upsert_progress(&PlaybackProgress::new(
                MediaId::new(SourceKind::YouTube, "external-edit"),
                Some(60),
                1,
            ))
            .expect_err("external edit must win");

        assert!(matches!(
            error,
            PersistenceError::StateDocumentChangedExternally {
                document: "progress"
            }
        ));
        assert_eq!(
            fs::read(progress_path).expect("preserved external edit"),
            externally_edited
        );
    }

    #[test]
    fn file_backend_rejects_duplicate_authoritative_identities_on_open() {
        let temporary = tempdir().expect("temporary state root");
        let config = Config::for_dir(temporary.path().join("youta"));
        let paths = FilePaths::from_config(&config);
        {
            let _store = FileStateStore::open(&config).expect("initialize file state");
        }
        let media_id = MediaId::new(SourceKind::YouTube, "duplicate");
        let duplicate = ProgressDocument {
            format_version: FILE_FORMAT_VERSION,
            progress: vec![
                PlaybackProgress::new(media_id.clone(), Some(60), 1),
                PlaybackProgress::new(media_id, Some(60), 2),
            ],
        };
        write_document(
            &paths.progress,
            "progress",
            MAX_PROGRESS_DOCUMENT_BYTES,
            &duplicate,
        )
        .expect("duplicate fixture");

        let error = FileStateStore::open(&config)
            .err()
            .expect("duplicate identities must be rejected");
        assert!(matches!(
            error,
            PersistenceError::InvalidFileDocument {
                document: "progress",
                ..
            }
        ));
    }
}
