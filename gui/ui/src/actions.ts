/**
 * Exact actions emitted by this frontend, using Serde's existing external tags.
 * Numeric payloads stay JSON numbers; Rust retains integer/range validation.
 * Views remain the separate, intentionally redacted subset in contract.ts.
 */
import type {
	ArchiveUploadField,
	CommonsUploadField,
	EvernoteNoteField,
	MediaId,
	PlaylistEditorField,
	S3CredentialField,
	S3UploadField,
	SubscriptionPane,
	SubscriptionsLayout,
} from './contract';

/** Rust view::Screen spellings returned by the screen-metadata command. */
export type ActionScreen =
	| 'Search'
	| 'YouTubeMusic'
	| 'YandexMusic'
	| 'Bandcamp'
	| 'ApplePodcasts'
	| 'ArchiveOrg'
	| 'LibriVox'
	| 'Radio'
	| 'TrackerMusic'
	| 'Subscriptions'
	| 'Local'
	| 'Web'
	| 'Downloaded'
	| 'History'
	| 'Playlists'
	| 'Statistics';

/** Serde spellings of domain::SourceKind, including a future provider's tag. */
export type SourceKind =
	| 'you-tube'
	| 'rss'
	| 'apple-podcasts'
	| 'local'
	| 'wikimedia-commons'
	| 'archive-org'
	| 'libri-vox'
	| 'yandex-music'
	| 'bandcamp'
	| 'odysee'
	| 'rumble'
	| 'bilibili'
	| 'peer-tube'
	| 'funkwhale'
	| 'vimeo'
	| 'ru-tube'
	| 'sound-cloud'
	| 'jamendo'
	| 'sound-stream'
	| 'lit-res'
	| 'bbc-radio'
	| 'mod-archive'
	| 'generic-yt-dlp'
	| 'vk'
	| 'telegram'
	| 'radio'
	| 'remote-files'
	| { other: string };

/** Unit variants reachable through controls or their typed action tables. */
export type UnitUiAction =
	| 'ActivateLocalMoveDestination'
	| 'ActivateSelection'
	| 'AnalyzeLocalAudioQuality'
	| 'BeginLocalMove'
	| 'BeginLocalRename'
	| 'BeginNewPlaylist'
	| 'BeginSearch'
	| 'CancelAudioQualityAnalysis'
	| 'CancelDownload'
	| 'CancelGitHubIssueSubmission'
	| 'CancelVideoSummary'
	| 'CheckAndDownloadNewEpisodes'
	| 'ClearQueue'
	| 'ConfirmChannelDownload'
	| 'ConfirmDownloadedTrash'
	| 'ConfirmGitHubIssueSubmission'
	| 'ConfirmLocalMoveHere'
	| 'ConfirmLocalTrash'
	| 'ConfirmPodcastFeed'
	| 'CopyAndOpenGitHubIssue'
	| 'CopyAudioQualityReport'
	| 'CopyErrorReport'
	| 'CopyLink'
	| 'CopyVideoSummary'
	| 'CreatePlaylistAndAdd'
	| 'CycleArchiveDownloadPreference'
	| 'CycleBandcampAudioFormat'
	| 'CycleCommonsAuthMethod'
	| 'CycleCommonsUploadLicense'
	| 'CycleDownloadModePreference'
	| 'CycleVideoSummaryBackend'
	| 'CycleYouTubeThumbnailSize'
	| 'DismissArchiveCredentials'
	| 'DismissArchiveUpload'
	| 'DismissAsciiVisualizer'
	| 'DismissAudioQualityPopup'
	| 'DismissChannelDownload'
	| 'DismissCommonsCredentials'
	| 'DismissCommonsUpload'
	| 'DismissDownloadChoice'
	| 'DismissDownloadQueue'
	| 'DismissErrorPopup'
	| 'DismissEvernoteCredentials'
	| 'DismissEvernoteNote'
	| 'DismissLanShare'
	| 'DismissLocalFilePopup'
	| 'DismissPlaylistPopup'
	| 'DismissPodcastFeed'
	| 'DismissPreferences'
	| 'DismissPrivateNotePopup'
	| 'DismissProjectHistory'
	| 'DismissQueuePopup'
	| 'DismissRssSubscriptionPopup'
	| 'DismissS3Credentials'
	| 'DismissS3Upload'
	| 'DismissVideoComments'
	| 'DismissVideoQr'
	| 'DismissVideoSummary'
	| 'DismissYandexMusicSetup'
	| 'DismissYouTubeCaptions'
	| 'DismissYouTubeSetup'
	| 'Download'
	| 'DownloadTwentyYandexMusicRecommendations'
	| 'DownloadYandexMusicAlbum'
	| 'EditPrivateNote'
	| 'EditSelectedPlaylist'
	| 'FingerprintLocalAudio'
	| 'GenerateVideoSummary'
	| 'GoBack'
	| 'InsertEvernoteCaptions'
	| 'OpenArchiveCredentialsGuide'
	| 'OpenArchiveUpload'
	| 'OpenArchiveUploadResult'
	| 'OpenChannelDownload'
	| 'OpenChannelInBrowser'
	| 'OpenCommonsAccountRegistration'
	| 'OpenCommonsBotPasswordGuide'
	| 'OpenCommonsUpload'
	| 'OpenCommonsUploadResult'
	| 'OpenEvernoteDeveloperTokenGuide'
	| 'OpenEvernoteNote'
	| 'OpenEvernoteNoteResult'
	| 'OpenGentooYtDlpPackage'
	| 'OpenGitHubIssueSubmissionTarget'
	| 'OpenInBrowser'
	| 'OpenPlaylistPopup'
	| 'OpenRssSubscriptionPopup'
	| 'OpenS3Credentials'
	| 'OpenS3Upload'
	| 'OpenVideoComments'
	| 'OpenVideoQr'
	| 'OpenYandexMusicAlbum'
	| 'OpenYandexMusicArtist'
	| 'OpenYtDlpProject'
	| 'RefreshSubscriptionVideos'
	| 'RefreshWeb'
	| 'RequestDownloadedTrash'
	| 'RequestGitHubIssueSubmission'
	| 'RequestLocalTrash'
	| 'ShareLocalFiles'
	| 'ShareLocalPodcast'
	| 'ShareYouTubeChannelPodcast'
	| 'ShowNowPlaying'
	| 'StopLanShare'
	| 'SubmitArchiveCredentials'
	| 'SubmitCommonsCredentials'
	| 'SubmitCommonsUpload'
	| 'SubmitEvernoteCredentials'
	| 'SubmitEvernoteNote'
	| 'SubmitLocalRename'
	| 'SubmitPreferences'
	| 'SubmitS3Credentials'
	| 'ToggleArchiveUploadVideo'
	| 'ToggleAutoplay'
	| 'ToggleChannelAutoDownload'
	| 'ToggleChannelDownloadIgnoreBefore'
	| 'ToggleChannelDownloadSkipShorts'
	| 'ToggleHelp'
	| 'ToggleHourlyAutoDownload'
	| 'ToggleLocalFolderSizes'
	| 'ToggleNyanCatSeekbar'
	| 'TogglePause'
	| 'TogglePlaybackHistorySaving'
	| 'TogglePodcastFeedIgnoreBefore'
	| 'TogglePodcastFeedSkipShorts'
	| 'ToggleRadioFavorite'
	| 'ToggleRepeat'
	| 'ToggleS3UploadVideo'
	| 'ToggleSelectedPlaylistMembership'
	| 'ToggleSkipAdvertisementChapters'
	| 'ToggleSponsorBlock'
	| 'ToggleSubscription'
	| 'ToggleSubscriptionDescription'
	| 'ToggleSubscriptionShorts'
	| 'ToggleThumbnailExpansion'
	| 'ToggleTodoPlaylist'
	| 'ToggleTtyImages'
	| 'ToggleYandexMusicDislike'
	| 'ToggleYandexMusicLike'
	| 'ToggleYouTubePrewarm'
	| 'UpdatePlaylist';

/**
 * Payloads keyed by their Rust variant name. Fields containing identities,
 * generations, and explicit null values are part of the wire contract.
 */
export interface UiActionPayloads {
	ActivateDescriptionVideo: { video_id: string; start_seconds: number | null; };
	ActivateDetailLink: number;
	ActivateQueuePopupRow: number;
	ActivateTimecode: { media_id: MediaId; seconds: number };
	ActivateWaveformTimecode: { media_id: MediaId; generation: number; seconds: number; };
	ActivateWikidataMedia: number;
	ActivateYouTubeCaption: number;
	AddCommonsCategorySuggestionAt: number;
	CancelQueuedDownload: number;
	ChangeChapter: number;
	ChangeSpeed: number;
	ChangeVolume: number;
	ConfirmDownloadChoice: number;
	FocusSubscriptionPane: SubscriptionPane;
	OpenCommonsCategorySuggestionAt: number;
	OpenWikidataValue: string;
	OpenYandexMusicAlbumById: string;
	OpenYandexMusicArtistById: string;
	PlayQueueNeighbour: number;
	PrefetchSubscriptionVideosThrough: number;
	RemoveCommonsUploadCategory: number;
	RemoveQueuePopupRow: number;
	RetryQueuedDownload: number;
	SeekPercent: number;
	SeekRelative: number;
	SelectArchiveCredentialField: boolean;
	SelectArchiveUploadField: ArchiveUploadField;
	SelectCommonsCredentialField: boolean;
	SelectCommonsUploadField: CommonsUploadField;
	SelectDetailLink: number;
	SelectDownloadChoice: { generation: number; index: number; };
	SelectDownloadQueueEntry: number;
	SelectEvernoteNoteField: EvernoteNoteField;
	SelectLocalMoveDestination: number;
	SelectPlaylistEditorField: PlaylistEditorField;
	SelectPlaylistPopupRow: number;
	SelectQueuePopupRow: number;
	SelectRow: number;
	SelectS3CredentialField: S3CredentialField;
	SelectS3UploadField: S3UploadField;
	SelectSubscriptionItem: number;
	SelectSubscriptionSource: number;
	SetAudioQualityPopupScroll: number;
	SetDetailsFocus: boolean;
	SetDetailsScroll: number;
	SetProjectHistoryScroll: number;
	SetSubscriptionsLayout: SubscriptionsLayout;
	SetVideoCommentsScroll: number;
	SetVideoSummaryScroll: number;
	ShowScreen: ActionScreen;
	SubmitArchiveUpload: number;
	SubmitS3Upload: number;
	ToggleDownloadMarkAt: number;
	ToggleWikidataStatements: number;
}

/** Excludes a second action tag, including when the object is held in a variable. */
type PayloadUiAction = {
	[Name in keyof UiActionPayloads]:
		{ [Key in Name]: UiActionPayloads[Key] }
		& { [Other in Exclude<keyof UiActionPayloads | UnitUiAction, Name>]?: never }
}[keyof UiActionPayloads];

/** One unit string or exactly one frontend-supported payload variant. */
export type UiAction = UnitUiAction | PayloadUiAction;
