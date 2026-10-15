import type { ViewModel } from './contract';

/** Sources whose ordinary empty body can offer a new search, not folder navigation. */
function isSearchRoute(view: ViewModel): boolean {
	if (view.screen === 'YandexMusic') return view.yandex_music_route === 'Search';
	return ['Search', 'YouTubeMusic', 'SoundCloud', 'Funkwhale', 'Bandcamp',
		'ApplePodcasts', 'ArchiveOrg', 'LibriVox', 'TrackerMusic'].includes(view.screen);
}

/** Hide only an empty inactive query; Web, Radio and transient channel controls retain theirs. */
export function searchEditorVisible(view: ViewModel): boolean {
	return view.search_editing || view.search_query.trim() !== ''
		|| (!isSearchRoute(view) && view.screen !== 'YandexMusic');
}

/** Idle guidance cannot cover a result, retained details, request, or modal editor. */
export function emptySearchPromptVisible(view: ViewModel): boolean {
	if (!isSearchRoute(view) || view.search_editing || view.search_activity
		|| view.rows.length !== 0 || view.details || view.help_open) return false;
	const overlays: (keyof ViewModel)[] = [
		'ascii_visualizer', 'site_file_popup', 'audio_quality_popup', 'video_summary_popup',
		'project_history_popup', 'youtube_provider_editor', 'yandex_music_setup_open',
		'funkwhale_setup_popup', 'rss_subscription_open', 'unsubscribe_popup',
		'preferences_popup', 'playlist_popup', 'queue_popup', 'private_note_open',
		'bug_report_popup', 'local_file_popup', 'local_file_progress', 'local_command_open',
		'custom_command_output', 'download_choice_popup', 'archive_playback_choice_popup',
		'download_queue_popup', 'video_comments_popup', 'error_popup', 'channel_download_popup',
		'youtube_captions_popup', 'commons_upload_popup', 'commons_credentials_editor',
		'evernote_popup', 'evernote_credentials_editor', 's3_upload_popup', 's3_credentials_editor',
		'archive_upload_popup', 'archive_credentials_editor', 'video_qr_popup',
		'lan_share_popup', 'podcast_feed_options_popup',
	];
	return !overlays.some((field) => Boolean(view[field]));
}
