// The shared vocabulary, as it arrives over IPC.
//
// These declarations mirror `src/view.rs`, `src/keymap.rs`, and `src/playback`
// in the Rust crate. They are the subset this window consumes, not the whole
// contract: the reducer publishes about 65 top-level fields and accepts 164
// actions.
//
// Every field name here is checked against the JSON the reducer actually emits
// by `gui/src/contract_tests.rs`. Adding a field the serializer does not send
// fails that test — which is how the first Details panel's five invented names
// were caught, after they had already shipped an empty panel.
//
// Anything absent here is absent deliberately. Four editors carry a YouTube API
// key, a Yandex OAuth token, a private note body, and a feed URL that may itself
// be a credential; the reducer skips them when serializing, so they never enter
// this process and must never be declared as if they might.

/** A serde `Duration`, which crosses as seconds plus nanoseconds. */
export interface RustDuration {
  secs: number;
  nanos: number;
}

/** One media-time interval already fetched. */
export interface BufferedRange {
  start: RustDuration;
  end: RustDuration;
}

/**
 * Source-neutral identity: `(provider, id)`.
 *
 * Actions that act on displayed media carry this back so the reducer can refuse
 * a click aimed at an item the selection has already moved away from.
 */
export interface MediaId {
  source: string;
  external_id: string;
}

/** Backend playback state, mirroring `PlaybackStatus`. */
export interface PlaybackStatus {
  idle: boolean;
  live: boolean;
  live_seekable_range: BufferedRange | null;
  position: RustDuration;
  duration: RustDuration | null;
  paused: boolean;
  volume: number;
  speed: number;
  chapter: number | null;
  buffering: boolean;
  buffered_ranges: BufferedRange[];
  title: string | null;
  stream_title: string | null;
}

/** One row of the main list. */
export interface RowView {
  media_id: MediaId | null;
  title: string;
  subtitle: string;
  source: string;
  watched_percent: number;
  playback_started: boolean;
  subscribed: boolean;
  thumbnail_url: string | null;
  vertical: boolean;
  hide_watched_marker: boolean;
  compact: boolean;
  radio_favorite: boolean;
  local_marked: boolean;
}

/** One chapter of the playing item. */
export interface Chapter {
  title: string;
  start_seconds: number;
  end_seconds: number | null;
}

/**
 * How a provider wants one Details link rendered.
 *
 * The `*Spaced` variants ask the terminal to reserve a blank row after the
 * link. This window uses ordinary vertical rhythm instead, so it distinguishes
 * only whether the label, the URL, or both are shown.
 */
export type DetailLinkPresentation =
  | "LabelAndUrl"
  | "LabelAndUrlSpaced"
  | "LabelOnlySpaced"
  | "UrlOnly"
  | "UrlOnlySpaced";

/** A Yandex Music destination reachable without leaving Youta. */
export type DetailLinkInternalTarget =
  | { YandexMusicArtist: string }
  | { YandexMusicAlbum: string };

/** One selectable external link beside a media item or channel. */
export interface DetailLinkView {
  prefix: string;
  label: string;
  url: string;
  wikidata_item_id: string | null;
  presentation: DetailLinkPresentation;
  internal_target: DetailLinkInternalTarget | null;
}

/**
 * A timecode found inside an untrusted description.
 *
 * The span is given as UTF-8 *byte* offsets into `DetailView.description`, not
 * as text — see `describe()` in `components/Description.tsx` for why that
 * distinction matters in JavaScript.
 */
export interface DetailTimecodeView {
  start_byte: number;
  end_byte: number;
  seconds: number;
  is_chapter: boolean;
}

/** A YouTube video URL found inside a description, with the same span rule. */
export interface DetailVideoLinkView {
  start_byte: number;
  end_byte: number;
  video_id: string;
  start_seconds: number | null;
}

/** One expandable Wikidata property spoiler. */
export interface DetailWikidataEntityView {
  item_id: string;
  text: string;
  value_links: DetailWikidataValueLinkView[];
  media_controls: DetailWikidataMediaView[];
  image_url: string | null;
}

/** A statement value inside expanded Wikidata text that opens a canonical page. */
export interface DetailWikidataValueLinkView {
  start_byte: number;
  end_byte: number;
  url: string;
}

/** A playable Commons value embedded in expanded Wikidata text. */
export interface DetailWikidataMediaView {
  marker_start_byte: number;
  marker_end_byte: number;
  media_id: MediaId;
  kind: string;
  title: string;
}

/**
 * The Details panel.
 *
 * Rich description content arrives as *structured* spans — timecodes, video
 * links, Wikidata entities — precisely so that no HTML is ever assembled from a
 * provider string.
 */
export interface DetailView {
  media_id: MediaId | null;
  title: string;
  source: string;
  channel_name: string;
  channel_id: string;
  channel_webpage_url: string | null;
  channel_subscribed: boolean;
  channel_subscriber_count: number | null;
  channel_video_count: number | null;
  channel_total_view_count: number | null;
  channel_created: string;
  channel_country: string;
  channel_links_truncated: boolean;
  length: string;
  description: string;
  timecodes: DetailTimecodeView[];
  video_links: DetailVideoLinkView[];
  likes: string;
  views: string;
  comments: string;
  published: string;
  license: string;
  radio_favorite: boolean;
  playlist_names: string[];
  has_private_note: boolean;
  wikidata: string;
  links: DetailLinkView[];
  expanded_wikidata_item: string | null;
  loading_wikidata_item: string | null;
  wikidata_entities: DetailWikidataEntityView[];
  thumbnail_url: string | null;
  expanded_thumbnail_url: string | null;
  thumbnail_expanded: boolean;
  local_renamable: boolean;
  local_movable: boolean;
  local_trashable: boolean;
  local_fingerprint_available: boolean;
  local_fingerprint_pending: boolean;
  local_audio_quality_available: boolean;
  local_audio_quality_pending: boolean;
  local_audio_quality_description: string;
}

/**
 * Which facts a source's Details panel presents.
 *
 * Chosen in Rust and delivered with the source list, so the window never
 * restates the mapping. See `Screen::details_kind` in `src/view.rs`.
 */
export type InformationPanelKind =
  | "Video"
  | "Podcast"
  | "Channel"
  | "Radio"
  | "YandexMusic"
  | "Local"
  | "Generic";

/** Progressive result of one bounded yt-dlp version lookup. */
export type YtDlpVersionLookupView =
  | "Loading"
  | { Available: { version: string; released_on: string | null } }
  | { Unavailable: { reason: string } };

/** Lifecycle of one explicitly confirmed diagnostic-report submission. */
export type GitHubIssueSubmissionView =
  | "Idle"
  | "Confirming"
  | "Submitting"
  | { Submitted: { url: string } }
  | { OutcomeUnknown: { issues_url: string } }
  | { Failed: { message: string } };

/** Gentoo's architecture-specific stable yt-dlp package metadata. */
export interface YtDlpGentooVersionView {
  arch: string;
  package_url: string;
  latest_stable: YtDlpVersionLookupView;
}

/** Progressive version metadata for an HTTP 403 attributed to yt-dlp. */
export interface YtDlpForbiddenView {
  project_url: string;
  installed: YtDlpVersionLookupView;
  github_latest: YtDlpVersionLookupView;
  gentoo: YtDlpGentooVersionView | null;
}

/** A diagnostic report or actionable setup message shown in the error layer. */
export interface ErrorPopupView {
  title: string;
  report: string;
  scroll_offset: number;
  gh_available: boolean;
  /** Whether this popup may offer either GitHub issue-creation path. */
  reportable: boolean;
  action_status: string | null;
  yt_dlp_forbidden: YtDlpForbiddenView | null;
  github_issue_submission: GitHubIssueSubmissionView;
}

/** Copyable progress and results for one local audio-quality batch. */
export interface AudioQualityPopupView {
  title: string;
  summary: string;
  completed: number;
  total: number;
  report: string;
  action_status: string | null;
  pending: boolean;
  scroll_offset: number;
}

/** One public top-level comment. */
export interface VideoCommentView {
  author_name: string;
  like_count: number;
  published: string | null;
  text: string;
}

/** Explicit request state of the bounded comments popup. */
export type VideoCommentsPopupState = "Loading" | "Ready" | "Empty" | { Error: string };

/** Scrollable public comments for one selected video. */
export interface VideoCommentsPopupView {
  video_id: string;
  video_title: string;
  state: VideoCommentsPopupState;
  comments: VideoCommentView[];
  scroll_offset: number;
}

/** An offline QR matrix, without its mandatory quiet zone. */
export interface QrMatrix {
  width: number;
  modules: boolean[];
}

/** The offline QR code for one selected video. */
export interface VideoQrPopupView {
  video_id: string;
  video_title: string;
  url: string;
  matrix: QrMatrix;
}

/** One source-control commit in the offline-first history popup. */
export interface ProjectCommitView {
  hash: string;
  committed_at: string;
  message: string;
}

/** State of the one-per-process GitHub history check. */
export type ProjectHistoryRemoteState =
  | "Embedded"
  | "Checking"
  | "UpToDate"
  | "Updated"
  | { Unavailable: string };

/** Recent commits plus how this binary was installed. */
export interface ProjectHistoryPopupView {
  commits: ProjectCommitView[];
  current_hash: string | null;
  installation: string;
  executable_path: string;
  started_in: string;
  build_source: string | null;
  remote_state: ProjectHistoryRemoteState;
  scroll_offset: number;
}

/** The runtime preferences editor. Values are drafts until it is submitted. */
export interface PreferencesPopupView {
  subscriptions_layout: string;
  skip_advertisement_chapters: boolean;
  youtube_prewarm: boolean;
  youtube_thumbnail_size: string;
  show_local_folder_sizes: boolean;
  show_images_in_tty: boolean;
  bandcamp_audio_format: string;
  config_path: string;
  environment_override: string | null;
  validation_error: string | null;
}

/**
 * One entry of the playback queue.
 *
 * The playable location is deliberately absent. For several providers it is a
 * signed, short-lived media URL, and queue actions address entries by position
 * so it never has to leave the reducer. The provider is read from `media_id`
 * rather than repeated, so this window keeps no copy of the source-label map.
 */
export interface QueueRowView {
  media_id: MediaId;
  title: string;
  subtitle: string;
  length: string;
}

/** The playback queue and its cursor. */
export interface QueuePopupView {
  items: QueueRowView[];
  current: number | null;
  selected: number;
  repeat_one: boolean;
}

/**
 * What is playing, in Youta's own words rather than the engine's.
 *
 * `PlaybackStatus.title` is whatever the backend parsed out of the stream; this
 * is the queue entry it came from. The window title, the tray tooltip, and the
 * track-change notification are set from this in Rust — the page reads it only
 * to label the player bar with the same words those surfaces use.
 */
export interface NowPlayingView {
  media_id: MediaId;
  title: string;
  subtitle: string;
}

/** One playlist row in the chooser, with the selected item's membership. */
export interface PlaylistChoiceView {
  playlist_id: string;
  name: string;
  contains_item: boolean;
}

/** Chooser, create, or edit route inside the playlist popup. */
export type PlaylistPopupMode = "Choose" | "Create" | "Edit";

/** Which editor field receives printable characters. */
export type PlaylistEditorField = "Name" | "Description";

/** The local-playlist chooser and its create/edit form. */
export interface PlaylistPopupView {
  item_title: string;
  playlists: PlaylistChoiceView[];
  selected: number;
  mode: PlaylistPopupMode;
  editing_playlist_id: string | null;
  editor_field: PlaylistEditorField;
  editor_name: string;
  editor_description: string;
  name_limit: number;
  description_limit: number;
  validation_error: string | null;
}

/** One destination row in the Local Move browser. */
export interface LocalMoveDestinationView {
  name: string;
  path: string;
}

/** An explicit filesystem mutation awaiting input or confirmation. */
export type LocalFilePopupView =
  | { Rename: { value: string; cursor_byte: number; error: string | null } }
  | { Trash: { name: string; path: string; error: string | null } }
  | { DownloadedTrash: { name: string; path: string; error: string | null } }
  | {
      Move: {
        source_names: string[];
        destination: string;
        directories: LocalMoveDestinationView[];
        selected: number;
        pending: boolean;
        error: string | null;
      };
    };

/** The selected playable item, when playlist actions apply to it. */
export interface PlaylistItemView {
  media_id: MediaId;
  title: string;
  in_todo: boolean;
}

/** Selection-sensitive actions offered by the Yandex Music source. */
export interface YandexMusicActionsView {
  track_selected: boolean;
  artist_available: boolean;
  album_available: boolean;
  album_open: boolean;
  twenty_recommendations_available: boolean;
  reaction: string;
}

/**
 * A generated waveform that is ready to draw.
 *
 * The peaks themselves are deliberately absent: they are the one part of the
 * view too large for JSON, and arrive as bytes from `waveformPeaks` instead.
 * `generation` is what makes that safe — it names the exact file identity these
 * peaks belong to, so a reply that arrives after the selection moved can be
 * recognised as stale rather than drawn over the new selection.
 */
export interface WaveformReady {
  media_id: MediaId;
  generation: number;
  duration: RustDuration;
}

/** Owner-aware state of the lazily generated local waveform. */
export type WaveformView =
  | "Unavailable"
  | { Loading: { media_id: MediaId } }
  | { Ready: WaveformReady }
  | { Failed: { media_id: MediaId; message: string } };

/** How the Subscriptions screen navigates between sources and their media. */
export type SubscriptionsLayout = "drill-down" | "split";

/** Whether the Subscriptions screen is showing sources or one source's media. */
export type SubscriptionRoute = "Sources" | "Items";

/** Which Subscriptions pane the shared keyboard map is steering. */
export type SubscriptionPane = "Sources" | "Items";

/** Provider family of a subscribed source, as `SubscriptionKind` serializes. */
export type SubscriptionKind = "you-tube" | "rss" | "other";

/** Navigation and list state owned by the Subscriptions screen. */
export interface SubscriptionsView {
  layout: SubscriptionsLayout;
  route: SubscriptionRoute;
  sources: RowView[];
  selected_source: number;
  items: RowView[];
  selected_item: number;
  focus: SubscriptionPane;
  description_expanded: boolean;
  loading: boolean;
  show_youtube_shorts: boolean;
  source_title: string;
  source_kind: SubscriptionKind;
  source_subscriber_count: number | null;
  source_created: string;
}

/** The active or most recently finished supervised download. */
export interface DownloadView {
  title: string;
  downloaded_bytes: number;
  total_bytes: number | null;
  bytes_per_second: number | null;
  eta_seconds: number | null;
  active: boolean;
  completed_path: string | null;
}

/** The published snapshot. */
export interface ViewModel {
  screen: string;
  search_editing: boolean;
  search_query: string;
  search_cursor_byte: number;
  rows: RowView[];
  selected: number;
  playing_media_id: MediaId | null;
  now_playing: NowPlayingView | null;
  details: DetailView | null;
  details_focused: boolean;
  details_scroll: number;
  selected_detail_link: number | null;
  selected_wikidata_media: number | null;
  subscriptions: SubscriptionsView;
  waveform: WaveformView;
  waveform_visible: boolean;
  waveform_playback_matches: boolean;
  playback: PlaybackStatus;
  playback_starting: boolean;
  playback_chapters: Chapter[];
  show_chapter_timestamps: boolean;
  radio_now_playing: string | null;
  autoplay: boolean;
  repeating: boolean;
  status_line: string;
  transient_footer_notice: string | null;
  external_opener_available: boolean;
  video_comments_available: boolean;
  private_note_available: boolean;
  playlist_item: PlaylistItemView | null;
  playlist_edit_available: boolean;
  yandex_music_actions: YandexMusicActionsView;
  download: DownloadView | null;
  help_open: boolean;
  project_history_popup: ProjectHistoryPopupView | null;
  error_popup: ErrorPopupView | null;
  audio_quality_supported: boolean;
  audio_quality_popup: AudioQualityPopupView | null;
  video_comments_popup: VideoCommentsPopupView | null;
  video_qr_popup: VideoQrPopupView | null;
  preferences_popup: PreferencesPopupView | null;
  playlist_popup: PlaylistPopupView | null;
  queue_popup: QueuePopupView | null;
  local_file_popup: LocalFilePopupView | null;
  // The four credential-bearing editors cross as a single bit each: enough to
  // know the reducer is modal and every key is going into an editor this window
  // cannot draw, and nothing more. See the module header in `src/view.rs`.
  youtube_setup_open: boolean;
  yandex_music_setup_open: boolean;
  rss_subscription_open: boolean;
  private_note_open: boolean;
  quitting: boolean;
  search_animation_frame: number;
  local_fingerprint_animation_frame: number;
  playback_start_animation_frame: number;
}

/** The high-frequency group carried on its own channel. */
export interface PlaybackTick {
  playback: PlaybackStatus;
  radio_now_playing: string | null;
  playback_starting: boolean;
  playback_start_animation_frame: number;
  search_animation_frame: number;
  local_fingerprint_animation_frame: number;
}

/**
 * One selectable source, as the reducer labels it.
 *
 * `search_verb` is null on a screen that collects no query, which is how this
 * window knows not to draw a search field there.
 */
export interface ScreenEntry {
  id: string;
  label: string;
  details_kind: InformationPanelKind;
  search_verb: string | null;
}

/** The configured audio engine, driver, and device. */
export interface AudioOutputView {
  engine: string;
  driver: string;
  device: string | null;
}

/**
 * A key, named independently of any input source.
 *
 * Unit variants cross as bare strings; the two payload variants cross as
 * single-key objects. This is Serde's external tagging, and `src/keymap.rs`
 * pins the exact shape with a test because this file builds it by hand.
 */
export type Key =
  | "Enter"
  | "Esc"
  | "Backspace"
  | "Delete"
  | "Tab"
  | "BackTab"
  | "Left"
  | "Right"
  | "Up"
  | "Down"
  | "Home"
  | "End"
  | "PageUp"
  | "PageDown"
  | { Char: string }
  | { F: number };

/** One key press with its modifier state. */
export interface KeyPress {
  key: Key;
  ctrl: boolean;
  alt: boolean;
  shift: boolean;
}

/** How much of a scrollable popup this window rendered, in wrapped lines. */
export interface ScrollGeometry {
  offset: number;
  maximum: number;
  page_lines: number;
}

/** The scroll geometry of every popup whose paging keys the reducer resolves. */
export interface PopupGeometry {
  audio_quality: ScrollGeometry;
  project_history: ScrollGeometry;
  video_comments: ScrollGeometry;
}

/**
 * A semantic action.
 *
 * Unit variants are bare strings and payload variants are single-key objects,
 * matching Serde's external tagging. The window emits only the handful its
 * controls produce; every other action reaches the reducer through the shared
 * keyboard map instead, which is why this type is deliberately loose.
 *
 * A misspelled variant is rejected by the reducer rather than ignored, and
 * `ipc.ts` reports that rejection into the process log — see `dispatch`.
 */
export type UiAction = string | Record<string, unknown>;
