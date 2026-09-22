/**
 * Drives the actual built React app inside an isolated headless browser.
 * Native commands are recorded, never executed. Incoming controller snapshots
 * are explicit fixtures: this checks browser rendering and IPC, not Rust state
 * transitions, media playback, remote provider availability or publication.
 */
(() => {
	const defaults = window.__YOUTA_FIXTURES__;
	// Mock only the native artwork protocol boundary for one deterministic image.
	// The production component must still request the same cached native URL.
	const waveformUrl = 'https://archive.org/download/fixture/waveform.png';
	const nativeWaveformUrl = `youta://artwork/${encodeURIComponent(waveformUrl)}`;
	const waveformImage = 'data:image/svg+xml,' + encodeURIComponent(
		'<svg xmlns="http://www.w3.org/2000/svg" width="800" height="200"><path d="M0 100H100L150 10L200 190L250 50L300 150L350 100H800" stroke="white" fill="none"/></svg>',
	);
	const setAttribute = Element.prototype.setAttribute;
	const imageSource = Object.getOwnPropertyDescriptor(HTMLImageElement.prototype, 'src');
	Object.defineProperty(HTMLImageElement.prototype, 'src', {
		...imageSource,
		set(value) {
			if (value === nativeWaveformUrl) {
				setAttribute.call(this, 'data-native-artwork', value);
				imageSource.set.call(this, waveformImage);
			} else {
				imageSource.set.call(this, value);
			}
		},
	});
	Element.prototype.setAttribute = function(name, value) {
		if (this instanceof HTMLImageElement && name === 'src' && value === nativeWaveformUrl) {
			setAttribute.call(this, 'data-native-artwork', value);
			return setAttribute.call(this, name, waveformImage);
		}
		return setAttribute.call(this, name, value);
	};
	const clone = (value) => structuredClone(value);
	const calls = [];
	const failures = [];
	const checks = [];
	const listeners = new Map();
	let view = {
		...clone(defaults.ViewModel), screen: 'ArchiveOrg', playback_history_enabled: true,
		status_line: 'Isolated browser fixture',
		playback: { ...clone(defaults.ViewModel.playback), idle: true, speed: 1, volume: 50 },
	};
	const sources = [
		{ id: 'Search', label: 'YouTube', details_kind: 'Video', search_verb: 'Search' },
		{ id: 'YouTubeMusic', label: 'YT Music', details_kind: 'Video', search_verb: 'Search' },
		{ id: 'SoundCloud', label: 'SoundCloud', details_kind: 'Generic', search_verb: 'Search' },
		{ id: 'ArchiveOrg', label: 'archive.org', details_kind: 'Generic', search_verb: 'Search' },
		{ id: 'LibriVox', label: 'LibriVox', details_kind: 'Podcast', search_verb: 'Search' },
	];
	window.addEventListener('error', (event) => failures.push(event.message));
	window.addEventListener('unhandledrejection', (event) => failures.push(String(event.reason)));
	window.__TAURI__ = {
		core: {
			async invoke(command, args) {
				calls.push({ command, args });
				if (command === 'screens') return clone(sources);
				if (command === 'snapshot') return clone(view);
				if (command === 'audio_output') return { engine: 'fixture', driver: 'none', device: null };
				if (command === 'dispatch' || command === 'key') return null;
				if (command === 'report_window_failure') { failures.push(JSON.stringify(args)); return null; }
				throw new Error(`Unexpected native command: ${command}`);
			},
		},
		event: {
			async listen(event, callback) {
				if (!listeners.has(event)) listeners.set(event, new Set());
				listeners.get(event).add(callback);
				return () => listeners.get(event).delete(callback);
			},
		},
	};
	const emit = (event, payload) => {
		for (const callback of listeners.get(event) ?? []) callback({ payload: clone(payload) });
	};
	const snapshot = (patch) => { view = { ...view, ...clone(patch) }; emit('youta://view', view); };
	const tick = (patch) => {
		view.playback = { ...view.playback, ...clone(patch) };
		emit('youta://playback', {
			playback: view.playback, radio_now_playing: null, playback_starting: false,
			playback_start_animation_frame: 0, search_animation_frame: 0,
			local_fingerprint_animation_frame: 0, youtube_caption_line: null,
		});
	};
	const assert = (condition, label) => {
		if (!condition) throw new Error(label);
		checks.push(label);
	};
	const until = async (predicate, label) => {
		for (let attempt = 0; attempt < 150; attempt++) {
			if (failures.length) throw new Error(failures.join('\n'));
			const value = predicate();
			if (value) return value;
			await new Promise((resolve) => setTimeout(resolve, 20));
		}
		throw new Error(`Timed out: ${label}`);
	};
	const button = (label, root = document) => [...root.querySelectorAll('button')]
		.find((node) => node.textContent.trim() === label || node.getAttribute('aria-label') === label);
	const dialog = () => [...document.querySelectorAll('[role=dialog]')].at(-1);
	const action = async (expected, perform, label) => {
		const start = calls.length;
		perform();
		await until(() => calls.slice(start).some((call) => call.command === 'dispatch'
			&& JSON.stringify(call.args.action) === JSON.stringify(expected)), label);
		checks.push(label);
	};
	const key = async (keyName, expected, options = {}) => {
		const start = calls.length;
		document.dispatchEvent(new KeyboardEvent('keydown', { key: keyName, bubbles: true, cancelable: true, ...options }));
		await until(() => calls.slice(start).some((call) => call.command === 'key'
			&& JSON.stringify(call.args.press.key) === JSON.stringify(expected)
			&& call.args.press.ctrl === Boolean(options.ctrlKey)), `forward ${keyName}`);
		checks.push(`Keyboard ${keyName} uses the shared Rust keymap IPC`);
	};
	const mediaId = { source: 'archive-org', external_id: 'https://archive.org/download/fixture/first.mp3' };
	const row = (title, id = mediaId) => ({ ...clone(defaults.RowView), title, media_id: id, source: 'archive.org' });
	const details = (title, id = mediaId) => ({
		...clone(defaults.DetailView), title, source: 'archive.org', media_id: id,
		description: 'Complete fixture description.\nSecond paragraph remains visible.',
		webpage_url: 'https://archive.org/details/fixture',
	});
	/** Exercise provider settings without providing a credential to the web-view fixture. */
	async function checkProviderSettings() {
		const aboutUrl = 'https://en.wikipedia.org/wiki/Invidious';
		/** Link Enter belongs to its native opener, not the editor's save/confirm keymap. */
		const openAboutWithEnter = async (label) => {
			const about = button(aboutUrl, dialog());
			about.focus();
			assert(document.activeElement === about, 'The Wikipedia link can receive keyboard focus');
			const start = calls.length;
			const enter = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true });
			await action('OpenInvidiousAbout', () => about.dispatchEvent(enter), label);
			assert(enter.defaultPrevented, 'Link Enter suppresses a duplicate native click');
			assert(calls.slice(start).filter((call) => call.command === 'dispatch').length === 1,
				'Link Enter dispatches exactly one article action');
			assert(!calls.slice(start).some((call) => call.command === 'key'),
				'Link Enter never reaches the provider save or instance confirmation keymap');
		};
		const preferences = { ...clone(defaults.PreferencesPopupView), youtube_provider_settings_supported: true };
		snapshot({ preferences_popup: preferences });
		await until(() => button('YouTube API / Invidious…', dialog()), 'provider settings in Preferences');
		await action('OpenYouTubeProviderSettings', () => button('YouTube API / Invidious…', dialog()).click(), 'Preferences opens the shared provider editor');
		const editor = { selected_field: 'ApiKey', api_key_length: 23, invidious_url_length: 0,
			invidious_url: null, invidious_instances: null, validation_failed: false, from_preferences: true,
			official_supported: true, invidious_supported: true };
		snapshot({ preferences_popup: null, youtube_provider_editor: editor });
		await until(() => dialog()?.textContent.includes('23 characters entered'), 'masked YouTube editor');
		assert(document.querySelectorAll('[role=dialog]').length === 1, 'Preferences is parked while the provider child editor is open');
		assert(!dialog().querySelector('input, textarea'), 'Provider drafts are not copied into browser text controls');
		assert(button('Save', dialog()) && !button('Save and retry', dialog()), 'Preferences provider changes save without retrying a search');
		const about = button(aboutUrl, dialog());
		assert(about?.getAttribute('role') === 'link', 'The closed chooser still shows the complete Wikipedia URL as a link');
		assert(getComputedStyle(about).textDecorationLine.includes('underline'), 'The Wikipedia URL is visibly underlined');
		assert(about.disabled, 'An unavailable native opener keeps the article visible but disabled');
		const beforeAbout = calls.length;
		about.click();
		assert(calls.length === beforeAbout, 'A disabled Wikipedia link cannot dispatch an external open');
		snapshot({ external_opener_available: true });
		await until(() => !button(aboutUrl, dialog()).disabled, 'available article opener');
		await action('OpenInvidiousAbout', () => button(aboutUrl, dialog()).click(), 'The Wikipedia link opens through the native action without opening the chooser');
		await openAboutWithEnter('Focused Enter opens Wikipedia while the chooser is closed');
		await action({ SelectYouTubeSetupField: 'ApiKey' }, () => button('YouTube API key', dialog()).click(), 'API key field selection uses the shared reducer');
		await key('k', { Char: 'k' });
		await key('Backspace', 'Backspace');
		await action({ SelectYouTubeSetupField: 'InvidiousUrl' }, () => button('Invidious instance URL', dialog()).click(), 'Manual Invidious editing remains available');
		await action('OpenInvidiousInstancePicker', () => button('Choose instance', dialog()).click(), 'The instance directory loads only on request');
		const picker = { loading: true, loading_frame: 0, instances: [], selected: 0, error: null };
		snapshot({ youtube_provider_editor: { ...editor, selected_field: 'InvidiousUrl', invidious_instances: picker } });
		await until(() => dialog()?.querySelector('[role=status]')?.textContent.includes('|'), 'first loading animation frame');
		assert(Boolean(button(aboutUrl, dialog())), 'The Wikipedia URL remains visible while instances load');
		snapshot({ youtube_provider_editor: { ...editor, invidious_instances: { ...picker, loading_frame: 1 } } });
		await until(() => dialog()?.querySelector('[role=status]')?.textContent.includes('/'), 'second loading animation frame');
		assert(button('Save', dialog()).disabled, 'An open directory cannot accidentally save the provider draft');
		await key('Escape', 'Esc');
		await action('DismissInvidiousInstancePicker', () => button('Close instance list', dialog()).click(), 'Closing the chooser leaves the provider editor open');
		snapshot({ youtube_provider_editor: { ...editor, invidious_instances: { ...picker, loading: false, error: 'Directory temporarily unavailable' } } });
		await until(() => dialog()?.textContent.includes('Directory temporarily unavailable'), 'instance directory error');
		assert(Boolean(button(aboutUrl, dialog())), 'The Wikipedia URL remains visible after a directory error');
		await action('OpenInvidiousInstancePicker', () => button('Retry', dialog()).click(), 'Directory failure can be retried explicitly');
		snapshot({ youtube_provider_editor: { ...editor, invidious_instances: { ...picker, loading: false } } });
		await until(() => dialog()?.textContent.includes('No public instances are available'), 'empty instance directory');
		assert(Boolean(button(aboutUrl, dialog())), 'The Wikipedia URL remains visible with an empty directory');
		const instances = Array.from({ length: 30 }, (_, index) => ({ url: `https://instance-${index}.example/`, label: `instance-${index}.example (Fixture)` }));
		snapshot({ youtube_provider_editor: { ...editor, invidious_instances: { ...picker, loading: false, instances, selected: 29 } } });
		const selectedInstance = await until(() => dialog()?.querySelector('[role=option][aria-selected=true]'), 'selected instance row');
		await until(() => dialog()?.querySelector('[role=listbox]')?.scrollTop > 0, 'selected instance scrolls into the bounded list');
		assert(selectedInstance.textContent.includes('instance-29.example'), 'The controller owns directory selection');
		await action('OpenInvidiousAbout', () => button(aboutUrl, dialog()).click(), 'The Wikipedia link also opens while the chooser is populated');
		await openAboutWithEnter('Focused Enter opens Wikipedia while the chooser is populated');
		dialog().style.width = '260px';
		dialog().style.maxHeight = '360px';
		await until(() => dialog()?.clientWidth <= 260, 'narrow provider editor');
		assert(dialog().scrollWidth <= dialog().clientWidth, 'The provider editor remains within a narrow window');
		await key('ArrowUp', 'Up');
		await key('Enter', 'Enter');
		const selectionStart = calls.length;
		await action({ SelectInvidiousInstance: 29 }, () => selectedInstance.click(), 'Clicking an instance fills the reducer draft');
		assert(!calls.slice(selectionStart).some((call) => call.command === 'dispatch' && call.args.action === 'SubmitYouTubeSetup'), 'Instance selection does not automatically save');
		snapshot({ youtube_provider_editor: { ...editor, selected_field: 'InvidiousUrl', invidious_url: instances[29].url, invidious_url_length: instances[29].url.length } });
		await until(() => dialog()?.textContent.includes(instances[29].url), 'safe validated instance URL');
		assert(Boolean(button(aboutUrl, dialog())), 'The Wikipedia URL remains visible after choosing an instance');
		await action('SubmitYouTubeSetup', () => button('Save', dialog()).click(), 'Provider Save is a separate explicit action');
		snapshot({ youtube_provider_editor: { ...editor, validation_failed: true, invidious_url_length: 41 } });
		await until(() => dialog()?.querySelector('[role=alert]'), 'generic provider validation feedback');
		assert(dialog().textContent.includes('41 characters entered'), 'Invalid manual URL contents stay masked in the web view');
		await action('DismissYouTubeSetup', () => button('Cancel', dialog()).click(), 'Cancel returns through the shared reducer');
		snapshot({ youtube_provider_editor: { ...editor, official_supported: false, from_preferences: false } });
		await until(() => button('Save and retry', dialog()), 'search-origin setup retry action');
		assert(!button('YouTube API key', dialog()), 'Builds without the official provider hide the API-key field');
		snapshot({ youtube_provider_editor: { ...editor, invidious_supported: false } });
		await until(() => !button('Choose instance', dialog()), 'feature-disabled Invidious chooser');
		assert(!button('Invidious instance URL', dialog()), 'Builds without Invidious hide its manual field and directory');
		assert(!button(aboutUrl, dialog()), 'Builds without Invidious omit the article link');
		snapshot({ youtube_provider_editor: null, preferences_popup: { ...preferences, youtube_provider_settings_supported: false } });
		await until(() => dialog()?.textContent.includes('Preferences'), 'restored preferences');
		assert(!button('YouTube API / Invidious…', dialog()), 'Unsupported builds hide provider settings in Preferences');
		snapshot({ preferences_popup: null, external_opener_available: defaults.ViewModel.external_opener_available });
		await until(() => !dialog(), 'closed provider fixtures');
	}
	/** Playback choices are display-only snapshots; only explicit actions reach the native reducer. */
	async function checkArchivePlaybackChoices() {
		const popup = {
			generation: 27, title: 'Fixture video <original>', explanation: 'Choose a playable version.',
			options: ['Original MPEG4 · 42 MiB', 'Audio only: Ogg Vorbis · 4 MiB'], selected: 1,
		};
		snapshot({ archive_playback_choice_popup: popup, preferences_popup: clone(defaults.PreferencesPopupView) });
		await until(() => button(popup.options[1], dialog()), 'Archive playback format chooser');
		const stacked = [...document.querySelectorAll('[role=dialog]')];
		const preferenceLayer = Number.parseInt(getComputedStyle(stacked[0].parentElement).zIndex, 10);
		const playbackLayer = Number.parseInt(getComputedStyle(dialog().parentElement).zIndex, 10);
		assert(Number.isInteger(playbackLayer) && playbackLayer > preferenceLayer,
			'The playback chooser has a valid integer CSS layer above Preferences');
		snapshot({ preferences_popup: null });
		await until(() => document.querySelectorAll('[role=dialog]').length === 1, 'lower Preferences closed');
		assert(dialog().textContent.includes(popup.explanation), 'Playback explanation comes from the controller');
		assert(!dialog().querySelector('video, audio, a'), 'The chooser receives labels, not media sources to fetch');
		await action({ SelectArchivePlaybackChoice: { generation: 27, index: 0 } },
			() => button(popup.options[0], dialog()).click(), 'Choosing the original sends its generation and index');
		await action({ ConfirmArchivePlaybackChoice: 27 }, () => button('Play', dialog()).click(), 'Play explicitly confirms the current playback stage');
		/** Focused native controls own Enter/Space, while list navigation still uses Rust. */
		const focusedKey = async (label, keyName, expected, control = button(label, dialog())) => {
			control.focus();
			const start = calls.length;
			const event = new KeyboardEvent('keydown', { key: keyName, bubbles: true, cancelable: true });
			await action(expected, () => control.dispatchEvent(event), `Focused ${label} activates with ${JSON.stringify(keyName)}`);
			assert(event.defaultPrevented, 'Focused activation suppresses a duplicate native click');
			assert(calls.slice(start).filter((call) => call.command === 'dispatch').length === 1,
				'Focused playback control sends exactly one semantic action');
			assert(!calls.slice(start).some((call) => call.command === 'key'),
				'Focused activation cannot bubble into the shared confirm/play shortcut');
		};
		await focusedKey('Cancel', 'Enter', 'DismissArchivePlaybackChoice');
		await focusedKey('Cancel', ' ', 'DismissArchivePlaybackChoice');
		await focusedKey('footer Cancel', 'Enter', 'DismissArchivePlaybackChoice', button('Cancel', dialog().querySelector('footer')));
		await focusedKey(popup.options[1], 'Enter', { SelectArchivePlaybackChoice: { generation: 27, index: 1 } });
		await focusedKey('Play', ' ', { ConfirmArchivePlaybackChoice: 27 });
		await key('ArrowDown', 'Down');
		await key('Escape', 'Esc');
		snapshot({ archive_playback_choice_popup: { ...popup, generation: 28, selected: 0 } });
		await until(() => button(popup.options[0], dialog())?.className.includes('border-accent'), 'updated playback selection');
		await action({ ConfirmArchivePlaybackChoice: 28 }, () => button('Play', dialog()).click(), 'A replacement chooser confirms only its new generation');
		snapshot({ archive_playback_choice_popup: { ...popup, options: [], selected: 0 } });
		await until(() => button('Play', dialog())?.disabled, 'empty playback stage');
		const start = calls.length;
		button('Play', dialog()).click();
		assert(calls.length === start, 'An empty playback stage cannot dispatch Play');
		const coveredCancel = button('Cancel', dialog());
		snapshot({ error_popup: {
			title: 'Fixture error above playback chooser', report: 'Mock failure', scroll_offset: 0,
			gh_available: false, reportable: false, action_status: null,
			yt_dlp_forbidden: null, github_issue_submission: 'Idle',
		} });
		await until(() => dialog()?.textContent.includes('Fixture error above playback chooser'), 'covering error popup');
		assert(Number.parseInt(getComputedStyle(dialog().parentElement).zIndex, 10) > playbackLayer,
			'The error dialog has a higher computed CSS layer than the playback chooser');
		const beforeCovered = calls.length;
		coveredCancel.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }));
		await until(() => calls.slice(beforeCovered).some((call) => call.command === 'key' && call.args.press.key === 'Enter'), 'topmost modal keyboard routing');
		assert(!calls.slice(beforeCovered).some((call) => call.command === 'dispatch'),
			'A covered playback button cannot intercept the topmost modal keyboard action');
		snapshot({ archive_playback_choice_popup: null, error_popup: null, preferences_popup: {
			...clone(defaults.PreferencesPopupView), archive_playback_supported: true,
			archive_playback_preference: 'audio-only', auto_download_supported: false,
		} });
		await until(() => dialog()?.textContent.includes('archive.org playback'), 'Archive playback Preferences row');
		await action('CycleArchivePlaybackPreference', () => button('Audio only', dialog()).click(),
			'Archive playback preference remains editable without the downloader feature');
		snapshot({ preferences_popup: { ...view.preferences_popup, archive_playback_supported: false } });
		await until(() => !dialog()?.textContent.includes('archive.org playback'), 'feature-disabled playback preference');
		assert(!button('Audio only', dialog()), 'Builds without Archive playback hide its preference');
		snapshot({ preferences_popup: null });
		await until(() => !dialog(), 'closed playback fixtures');
	}
	/** SoundCloud stays on the shared tab, search-editor, and playback-action paths. */
	async function checkSoundCloudTab() {
		const previous = clone(view);
		const tabs = [...document.querySelectorAll('[aria-label=Sources] button')].map((node) => node.textContent);
		assert(tabs.indexOf('SoundCloud') === tabs.indexOf('YT Music') + 1, 'SoundCloud follows YT Music in the source catalogue');
		await action({ ShowScreen: 'SoundCloud' }, () => button('SoundCloud').click(), 'SoundCloud tab selects its shared reducer screen');
		snapshot({ screen: 'SoundCloud', search_query: '', rows: [], details: null });
		await until(() => document.querySelector('[title="Search SoundCloud"]'), 'SoundCloud search');
		await action('BeginSearch', () => document.querySelector('[title="Search SoundCloud"]')
			.dispatchEvent(new MouseEvent('mousedown', { bubbles: true })), 'SoundCloud search enters the shared query editor');
		snapshot({ search_editing: true, search_query: 'ambient', search_cursor_byte: 7 });
		await until(() => document.querySelector('[role=search]').textContent.includes('ambient'), 'SoundCloud query snapshot');
		await key('Enter', 'Enter');
		const track = { ...row('SoundCloud fixture track', { source: 'sound-cloud', external_id: 'https://soundcloud.com/artist/track' }), source: 'SoundCloud' };
		snapshot({ search_editing: false, rows: [track] });
		const item = await until(() => button(track.title, document.querySelector('[aria-label=Results]')), 'SoundCloud result');
		await action({ SelectRow: 0 }, () => item.click(), 'SoundCloud result selects through the shared reducer');
		await action('ActivateSelection', () => item.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })), 'SoundCloud result requests playback through the shared reducer');
		snapshot(previous);
		await until(() => document.querySelector('[title="Search archive.org"]'), 'restored Archive fixture');
	}
	/** Focus snapshots and native checkbox/button keys must agree with the shared keymap. */
	async function checkPreferencesFocus() {
		const preferences = { ...clone(defaults.PreferencesPopupView), selected_field: 'SubscriptionsLayout',
			subscriptions_layout: 'drill-down', auto_download_supported: true, youtube_provider_settings_supported: true };
		snapshot({ preferences_popup: preferences });
		await until(() => dialog()?.querySelector('[data-preferences-focused=true]')?.dataset.preferencesField === 'SubscriptionsLayout', 'initial Preferences focus');
		await key('ArrowDown', 'Down');
		snapshot({ preferences_popup: { ...preferences, selected_field: 'PlaybackHistory' } });
		await until(() => dialog()?.querySelector('[data-preferences-focused=true]')?.dataset.preferencesField === 'PlaybackHistory', 'moved Preferences focus');
		const checkbox = dialog().querySelector('input[type=checkbox]');
		await action({ SelectPreferencesField: 'HourlyDownloads' }, () => checkbox.focus(), 'Focusing a Preferences checkbox selects its shared control without toggling');
		snapshot({ preferences_popup: { ...preferences, selected_field: 'HourlyDownloads' } });
		await until(() => dialog()?.querySelector('[data-preferences-focused=true]')?.dataset.preferencesField === 'HourlyDownloads', 'checkbox Preferences focus');
		for (const [name, wire] of [['ArrowDown', 'Down'], ['ArrowUp', 'Up'], [' ', { Char: ' ' }], ['Enter', 'Enter']]) {
			const before = calls.length;
			const event = new KeyboardEvent('keydown', { key: name, bubbles: true, cancelable: true });
			checkbox.dispatchEvent(event);
			await until(() => calls.slice(before).some((call) => call.command === 'key' && JSON.stringify(call.args.press.key) === JSON.stringify(wire)), `Preferences checkbox forwards ${name}`);
			assert(event.defaultPrevented, `Preferences ${name} suppresses native duplicate activation`);
			assert(calls.slice(before).filter((call) => call.command === 'key').length === 1, `Preferences ${name} reaches the reducer once`);
			assert(!calls.slice(before).some((call) => call.command === 'dispatch'), `Preferences ${name} does not also toggle the checkbox`);
		}
		snapshot({ preferences_popup: { ...preferences, selected_field: 'YouTubeProvider' } });
		const provider = await until(() => dialog()?.querySelector('[data-preferences-focused=true]')?.dataset.preferencesField === 'YouTubeProvider'
			&& dialog().querySelector('[data-preferences-focused=true]'), 'last Preferences focus');
		const bounds = provider.getBoundingClientRect();
		assert(bounds.top >= 0 && bounds.bottom <= window.innerHeight, 'Last Preferences control scrolls into the window');
		snapshot({ preferences_popup: null });
		await until(() => !dialog(), 'closed Preferences focus fixture');
	}
	async function run() {
		await until(() => document.querySelector('[title="Search archive.org"]'), 'Archive search');
		await checkSoundCloudTab();
		await checkPreferencesFocus();
		await checkProviderSettings();
		await checkArchivePlaybackChoices();
		assert(!button('[Esc] Back'), 'Archive root hides Back when no return route exists');
		const tabs = [...document.querySelectorAll('[aria-label=Sources] button')].map((node) => node.textContent);
		assert(tabs.indexOf('archive.org') < tabs.indexOf('LibriVox'), 'Archive tab preserves source catalogue order');
		await action('BeginSearch', () => document.querySelector('[title="Search archive.org"]')
			.dispatchEvent(new MouseEvent('mousedown', { bubbles: true })), 'Search click enters the shared editor');
		snapshot({ search_editing: true });
		await key('f', { Char: 'f' });
		snapshot({ search_query: 'fixture', search_cursor_byte: 7 });
		await until(() => document.querySelector('[role=search]').textContent.includes('fixture'), 'query snapshot');
		await key('Enter', 'Enter');
		snapshot({ search_editing: false, search_activity: 'ArchiveOrg' });
		await until(() => document.querySelector('[aria-label="Loading archive.org"]'), 'loading status');
		checks.push('Archive loading status renders while the controller request is pending');
		snapshot({ search_activity: null, rows: [row('Fixture archive item')], details: details('Fixture archive item') });
		const item = await until(() => button('Fixture archive item', document.querySelector('[aria-label=Results]')), 'catalogue item');
		await action({ SelectRow: 0 }, () => item.click(), 'Catalogue click selects its row');
		await action('ActivateSelection', () => item.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })), 'Catalogue double click requests item tracks');
		await key('Insert', 'Insert');
		await key('d', { Char: 'd' }, { ctrlKey: true });
		await action({ ToggleDownloadMarkAt: 0 }, () => item.dispatchEvent(new MouseEvent('click', { bubbles: true, ctrlKey: true })), 'Ctrl-click marks the exact catalogue row');
		snapshot({ rows: [{ ...row('Fixture archive item'), download_marked: true, downloaded: true }] });
		await until(() => document.querySelector('[aria-label="Marked for download"]'), 'marked download row');
		assert(Boolean(document.querySelector('[aria-label="Downloaded"]')), 'Downloaded marker remains separate from the download selection mark');
		snapshot({ download_queue_popup: { entries: [
			{ id: 7, title: 'Failed fixture download', state: 'Failed' },
			{ id: 42, title: 'Waiting fixture download', state: 'Queued' },
		], selected: 0 } });
		await until(() => dialog()?.textContent.includes('Download queue'), 'persistent download queue');
		await action({ SelectDownloadQueueEntry: 42 }, () => button('Waiting fixture download · Queued', dialog()).click(), 'Download queue selection carries its stable job identity');
		await action({ RetryQueuedDownload: 7 }, () => button('Retry', dialog()).click(), 'Download retry targets the selected stable job');
		await action({ CancelQueuedDownload: 7 }, () => button('Cancel download', dialog()).click(), 'Download cancellation targets the selected stable job');
		await action('DismissDownloadQueue', () => button('Close', dialog()).click(), 'Closing the download queue is distinct from cancellation');
		snapshot({ download_queue_popup: null });
		await until(() => !dialog(), 'closed download queue');
		snapshot({ rows: [row('First fixture track'), row('Second fixture track', { ...mediaId, external_id: mediaId.external_id.replace('first', 'second') })], details: details('First fixture track') });
		const track = await until(() => button('First fixture track', document.querySelector('[aria-label=Results]')), 'track rows');
		assert(document.querySelector('[aria-label=Results]').textContent.includes('Second fixture track'), 'Item snapshot displays all fixture tracks');
		await action('ActivateSelection', () => track.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })), 'Track double click requests playback');
		snapshot({ playing_media_id: mediaId, now_playing: { media_id: mediaId, title: 'First fixture track', subtitle: 'archive.org' } });
		tick({ idle: false, paused: false, position: { secs: 1, nanos: 0 }, duration: { secs: 120, nanos: 0 } });
		await until(() => button('Pause') && !document.querySelector('[aria-label="Playback position"]').disabled, 'playing controls');
		checks.push('Playing snapshot enables transport and seek controls');
		tick({ paused: true, position: { secs: 120, nanos: 0 } });
		await until(() => button('Play') && !button('Play').disabled, 'retained EOF pause');
		assert(!document.querySelector('[aria-label="Playback position"]').disabled, 'Retained EOF snapshot keeps the seekbar enabled');
		await action({ SeekRelative: -5 }, () => button('Back 5 seconds').click(), 'EOF back-seek forwards the same relative-seek action');
		tick({ paused: false, position: { secs: 115, nanos: 0 } });
		await until(() => button('Pause'), 'seek resume snapshot');
		assert(document.querySelector('[aria-label=Results]').textContent.includes('First fixture track'), 'Seek-resume snapshot retains the same track catalogue');
		await action({ SeekPercent: 50 }, () => {
			const slider = document.querySelector('[aria-label="Playback position"]');
			Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(slider, '500');
			slider.dispatchEvent(new Event('input', { bubbles: true }));
			slider.dispatchEvent(new Event('change', { bubbles: true }));
		}, 'Seekbar forwards the exact requested percentage');
		tick({ position: { secs: 60, nanos: 0 } });
		assert(button('Repeat').getAttribute('aria-pressed') === 'false' && button('Autoplay').getAttribute('aria-pressed') === 'false', 'Archive playback shows global repeat and autoplay off');
		await action('ToggleRepeat', () => button('Repeat').click(), 'Repeat click uses the shared global action');
		await action('ToggleAutoplay', () => button('Autoplay').click(), 'Autoplay click uses the shared global action');
		snapshot({ archive_org_back_available: true, search_editing: true });
		await until(() => button('[Esc] Back')?.disabled, 'Back while editing search');
		checks.push('Archive Back does not interrupt an active search edit');
		snapshot({ search_editing: false, search_activity: 'ArchiveOrg' });
		await until(() => button('[Esc] Back') && !button('[Esc] Back').disabled, 'Back during topic loading');
		await action('GoBack', () => button('[Esc] Back').click(), 'Archive Back uses shared navigation even while a topic is loading');
		snapshot({ archive_org_back_available: false, search_activity: null });
		await until(() => !button('[Esc] Back'), 'Back hidden after returning to root');
		checks.push('Archive Back disappears when its last return route is consumed');

		// Expansion is a shared controller state, not an independent browser modal.
		const waveformDetails = { ...details('Waveform fixture'), thumbnail_url: waveformUrl,
			expanded_thumbnail_url: waveformUrl, thumbnail_expanded: false };
		snapshot({ details: waveformDetails });
		const compactWaveform = await until(() => {
			const image = document.querySelector('[aria-label=Details] img[data-native-artwork]');
			return image?.naturalWidth === 800 && image;
		}, 'native waveform fixture');
		assert(compactWaveform.naturalHeight === 200, 'Waveform fixture preserves its native aspect ratio');
		const compactWidth = compactWaveform.getBoundingClientRect().width;
		await action('ToggleThumbnailExpansion', () => compactWaveform.click(), 'Waveform click requests shared artwork expansion');
		snapshot({ details: { ...waveformDetails, thumbnail_expanded: true } });
		const expandedWaveform = await until(() => dialog()?.querySelector('img[data-native-artwork]'), 'expanded waveform dialog');
		assert(expandedWaveform.dataset.nativeArtwork === nativeWaveformUrl, 'Enlarged waveform reuses the cached native image URL');
		assert(expandedWaveform.getBoundingClientRect().width > compactWidth, 'Expanded waveform uses space beyond the Details column');
		assert(getComputedStyle(expandedWaveform).objectFit === 'contain', 'Expanded artwork fits completely without cropping');
		await action('ToggleThumbnailExpansion', () => expandedWaveform.click(), 'Enlarged waveform click requests collapse');
		await action('ToggleThumbnailExpansion', () => button('Close artwork', dialog()).click(), 'Artwork close button uses the same controller action');
		await key('Escape', 'Esc');
		snapshot({ details: waveformDetails });
		await until(() => !dialog(), 'collapsed waveform snapshot');
		checks.push('Shared collapsed snapshot removes the enlarged artwork');

		// Highlight ranges come from the reducer; browser rendering must preserve
		// the original Unicode text and existing actions even when styles overlap.
		const description = 'Creator: Vitaly Zdanevich\nTopics: БРЭДБЕРИ\n📚 1:23 Zdanevich & <b>plain text</b>.';
		const range = (text, match, start = 0) => {
			const index = text.indexOf(match, start);
			const prefix = new TextEncoder().encode(text.slice(0, index)).length;
			return { start_byte: prefix, end_byte: prefix + new TextEncoder().encode(match).length };
		};
		const highlighted = { ...details('The Zdanevich collection'), description, license: '\uFEFFééé',
			links: [{ prefix: 'Uploader: ', label: 'Vitaly Zdanevich', url: 'https://archive.org/details/@fixture',
				wikidata_item_id: null, presentation: 'LabelAndUrl', internal_target: null }],
			timecodes: [{ ...range(description, '1:23'), seconds: 83, is_chapter: true }],
			search_highlights: [
				{ field: 'Title', ranges: [range('The Zdanevich collection', 'Zdanevich')] },
				{ field: 'Description', ranges: [range(description, 'Zdanevich'), range(description, 'Zdanevich', 30)] },
				{ field: { LinkLabel: 0 }, ranges: [range('Vitaly Zdanevich', 'Zdanevich')] },
				{ field: 'License', ranges: [{ start_byte: 3, end_byte: 5 }, { start_byte: 5, end_byte: 7 }, { start_byte: 7, end_byte: 9 }] },
			],
		};
		snapshot({ details: highlighted });
		await until(() => document.querySelectorAll('[aria-label=Details] mark').length >= 4, 'search highlights');
		const licenseValue = [...document.querySelectorAll('[aria-label=Details] dt')].find((node) => node.textContent === 'License').nextElementSibling;
		assert(licenseValue.textContent === 'ééé', 'Trimming a metadata prefix preserves Unicode match positions and text');
		assert(licenseValue.querySelectorAll('mark').length === 3, 'All matches in trimmed metadata remain highlighted');
		assert([...document.querySelectorAll('[aria-label=Details] mark')].filter((node) => node.textContent === 'Zdanevich').length === 4, 'All submitted search matches retain the original case');
		assert(document.querySelector('[aria-label=Details]').textContent.includes(description), 'Search highlighting preserves the complete description text');
		assert(!document.querySelector('[aria-label=Details] b'), 'Description markup remains literal text while highlighted');
		await action({ ActivateDetailLink: 0 }, () => button('Vitaly Zdanevich').click(), 'Highlighted link still dispatches its original action');
		snapshot({ details: { ...highlighted, search_highlights: [
			{ field: 'Description', ranges: [range(description, '1:23'), range(description, 'БРЭДБЕРИ')] },
		] } });
		await until(() => button('1:23')?.querySelector('mark'), 'highlighted timecode');
		assert([...document.querySelectorAll('[aria-label=Details] mark')].some((node) => node.textContent === 'БРЭДБЕРИ'), 'UTF-8 highlight positions preserve Cyrillic labels');
		await action({ ActivateTimecode: { media_id: mediaId, seconds: 83 } }, () => button('1:23').click(), 'Highlighted timecode still seeks to its exact timestamp');
		snapshot({ details: { ...highlighted, search_highlights: [] } });
		await until(() => document.querySelectorAll('[aria-label=Details] mark').length === 0, 'cleared highlights');
		checks.push('Clearing search highlights does not leave stale marks');

		// Creator/topic values are inline internal links, not additional rail rows.
		const metadataText = 'Creator: Vitaly Zdanevich\nTopics: space, здоровье\n\nFull item description.';
		const inlineLink = (label, target, text = metadataText) => ({
			prefix: '', label, url: '', wikidata_item_id: null, presentation: 'LabelOnly',
			internal_target: target, description_range: range(text, label),
		});
		const linkedDetails = { ...details('Metadata navigation fixture'), description: metadataText,
			links: [
				{ prefix: 'Uploader: ', label: 'Uploader fixture', url: 'https://archive.org/details/@fixture',
					wikidata_item_id: null, presentation: 'LabelOnly', internal_target: null, description_range: null },
				inlineLink('Vitaly Zdanevich', { ArchiveCreator: 'Vitaly Zdanevich' }),
				inlineLink('space', { ArchiveTopic: 'space' }),
				inlineLink('здоровье', { ArchiveTopic: 'здоровье' }),
			],
			search_highlights: [{ field: 'Description', ranges: [range(metadataText, 'Zdanevich')] }],
		};
		snapshot({ details: linkedDetails, external_opener_available: false });
		const creator = await until(() => document.querySelector('[data-detail-link="1"]'), 'inline creator link');
		assert(document.querySelector('[aria-label=Details]').textContent.includes(metadataText), 'Inline metadata keeps the original compact text');
		assert(document.querySelectorAll('[aria-label=Details] ul li').length === 1, 'Inline Creator and Topics do not add fixed action rows');
		assert(creator.querySelector('mark')?.textContent === 'Zdanevich', 'Inline links retain active search highlighting');
		await action({ SelectDetailLink: 1 }, () => creator.focus(), 'Keyboard focus selects the same global Creator link index');
		await action({ ActivateDetailLink: 1 }, () => creator.click(), 'Creator navigation works without an external browser opener');
		await action({ ActivateDetailLink: 2 }, () => document.querySelector('[data-detail-link="2"]').click(), 'First topic uses its own global link index');
		await action({ ActivateDetailLink: 3 }, () => document.querySelector('[data-detail-link="3"]').click(), 'Unicode topic uses its own global link index');
		const numericTopic = 'Topics: 1:23';
		snapshot({ details: { ...linkedDetails, description: numericTopic, search_highlights: [],
			links: [inlineLink('1:23', { ArchiveTopic: '1:23' }, numericTopic)],
			timecodes: [{ ...range(numericTopic, '1:23'), seconds: 83, is_chapter: false }],
		} });
		await until(() => document.querySelector('[data-detail-link="0"]'), 'timestamp-shaped topic');
		await action({ ActivateDetailLink: 0 }, () => document.querySelector('[data-detail-link="0"]').click(), 'Timestamp-shaped topic navigates instead of seeking');
		const longText = metadataText + '\n' + 'Description line\n'.repeat(120) + 'distant topic';
		snapshot({ details: { ...linkedDetails, description: longText, links: [...linkedDetails.links,
			inlineLink('distant topic', { ArchiveTopic: 'distant topic' }, longText),
		] }, selected_detail_link: 4, detail_link_reveal: 4 });
		await until(() => {
			const bounds = document.querySelector('[data-detail-link="4"]')?.getBoundingClientRect();
			const panel = document.querySelector('[aria-label=Details]').getBoundingClientRect();
			return bounds && bounds.top >= panel.top && bounds.bottom <= panel.bottom;
		}, 'keyboard reveals distant inline link');
		checks.push('Keyboard selection reveals an offscreen inline value');
		snapshot({ detail_link_reveal: null });
		await until(() => !failures.length, 'reveal cleared');
		const detailPanel = document.querySelector('[aria-label=Details]');
		detailPanel.scrollTop = 0;
		await new Promise((resolve) => setTimeout(resolve, 60));
		assert(detailPanel.scrollTop === 0, 'Manual scrolling is not trapped by selected inline metadata');

		// Existing provider markers keep their direct internal actions: indexed
		// external-link capability checks must not make them require a browser.
		for (const [target, expected] of [
			[{ YandexMusicArtist: 'artist-fixture' }, { OpenYandexMusicArtistById: 'artist-fixture' }],
			[{ YandexMusicAlbum: 'album-fixture' }, { OpenYandexMusicAlbumById: 'album-fixture' }],
		]) {
			const label = `Provider fixture ${Object.keys(target)[0]}`;
			snapshot({ details: { ...details('Existing provider navigation'), links: [{
				prefix: '', label, url: 'https://music.yandex.ru/fixture',
				wikidata_item_id: null, presentation: 'LabelOnly', internal_target: target, description_range: null,
			}] }, external_opener_available: false });
			await until(() => document.querySelector('[title="Open inside Youta"]')?.closest('li').textContent.includes(label), 'existing provider marker');
			await action(expected, () => document.querySelector('[title="Open inside Youta"]').click(), 'Existing provider marker retains its browser-independent action');
		}

		// The name browses uploads internally; its separate URL still opens the
		// public profile and obeys the external-opener capability.
		const uploaderUrl = 'https://archive.org/details/@different_account';
		const uploaderDetails = { ...details('Uploader navigation fixture'), channel_webpage_url: uploaderUrl,
			links: [{ prefix: 'Uploader: ', label: 'Public uploader name', url: uploaderUrl,
				wikidata_item_id: null, presentation: 'LabelAndUrl', description_range: null,
				internal_target: { ArchiveUploader: '@different_account' } }] };
		snapshot({ details: uploaderDetails, external_opener_available: false });
		const uploaderName = await until(() => button('Public uploader name'), 'uploader name');
		await action({ ActivateDetailLink: 0 }, () => uploaderName.click(), 'Uploader name browses internally without a browser');
		assert(!document.querySelector('[title="Open inside Youta"]'), 'Clickable uploader name does not add a redundant arrow');
		const profile = button(uploaderUrl);
		assert(profile?.disabled, 'Uploader profile URL remains visible but disabled without an external opener');
		const beforeProfile = calls.length;
		profile.click();
		assert(calls.length === beforeProfile, 'Disabled uploader profile cannot dispatch an external open');
		snapshot({ external_opener_available: true });
		await until(() => button(uploaderUrl) && !button(uploaderUrl).disabled, 'enabled uploader profile URL');
		await action('OpenChannelInBrowser', () => button(uploaderUrl).click(), 'Uploader URL opens its original public profile separately');

		// Readable URL labels retain original UTF-8 positions and copy payloads.
		const encodedUrl = 'https://commons.wikimedia.org/wiki/File:%D0%9F%D1%80.opus?literal=%2F';
		const encodedDescription = `📚 Умываю руки здесь!\n${encodedUrl}\n1:23 More audio\nTopics: space`;
		const readableDescription = encodedDescription.replace('%D0%9F%D1%80', 'Пр');
		const escapedDetails = { ...details('Encoded URL fixture'), description: encodedDescription,
			description_url_escapes: [
				{ ...range(encodedDescription, '%D0%9F'), text: 'П' },
				{ ...range(encodedDescription, '%D1%80'), text: 'р' },
			],
			search_highlights: [{ field: 'Description', ranges: [range(encodedDescription, '%D0%9F%D1%80')] }],
			timecodes: [{ ...range(encodedDescription, '1:23'), seconds: 83, is_chapter: false }],
			links: [inlineLink('space', { ArchiveTopic: 'space' }, encodedDescription)],
		};
		snapshot({ details: escapedDetails, detail_link_reveal: null });
		const readable = await until(() => document.querySelector('[data-description]')?.textContent === readableDescription
			&& document.querySelector('[data-description]'), 'readable URL description');
		assert(readable.querySelectorAll('[data-url-escape-original]').length === 2, 'Display uses mapped Unicode URL characters without altering other escapes');
		assert([...readable.querySelectorAll('mark')].map((node) => node.textContent).join('') === 'Пр', 'Original byte-range highlights follow the decoded URL characters');
		await action({ ActivateTimecode: { media_id: mediaId, seconds: 83 } }, () => button('1:23', readable).click(), 'Timecode after a decoded URL retains its original seek target');
		await action({ ActivateDetailLink: 0 }, () => button('space', readable).click(), 'Topic after a decoded URL retains its global link index');
		const selection = document.getSelection();
		const selectedRange = document.createRange();
		selectedRange.selectNodeContents(readable);
		selection.removeAllRanges();
		selection.addRange(selectedRange);
		const clipboard = {};
		const copy = new Event('copy', { bubbles: true, cancelable: true });
		Object.defineProperty(copy, 'clipboardData', { value: { setData: (type, value) => { clipboard[type] = value; } } });
		document.dispatchEvent(copy);
		assert(copy.defaultPrevented && clipboard['text/plain'] === encodedDescription, 'Copying the readable description preserves the exact original encoded URLs');
		const descriptionTitle = [...document.querySelectorAll('h2')].find((node) => node.textContent === escapedDetails.title);
		selectedRange.setStartBefore(descriptionTitle);
		selectedRange.setEnd(readable, readable.childNodes.length);
		selection.removeAllRanges();
		selection.addRange(selectedRange);
		const crossCopy = new Event('copy', { bubbles: true, cancelable: true });
		Object.defineProperty(crossCopy, 'clipboardData', { value: { setData: (type, value) => { clipboard[type] = value; } } });
		document.dispatchEvent(crossCopy);
		assert(crossCopy.defaultPrevented && clipboard['text/plain'].startsWith(escapedDetails.title)
			&& clipboard['text/plain'].endsWith(encodedDescription), 'Selection spanning the title and description also preserves original URL bytes');
		selectedRange.selectNodeContents(descriptionTitle);
		selection.removeAllRanges();
		selection.addRange(selectedRange);
		const outsideCopy = new Event('copy', { bubbles: true, cancelable: true });
		Object.defineProperty(outsideCopy, 'clipboardData', { value: { setData: () => { throw new Error('Outside selection copy overridden'); } } });
		document.dispatchEvent(outsideCopy);
		assert(!outsideCopy.defaultPrevented, 'Selection outside the description retains native clipboard handling');
		const decodedCharacter = readable.querySelector('[data-url-escape-original]');
		selectedRange.selectNodeContents(decodedCharacter);
		selection.removeAllRanges();
		selection.addRange(selectedRange);
		const partialCopy = new Event('copy', { bubbles: true, cancelable: true });
		Object.defineProperty(partialCopy, 'clipboardData', { value: { setData: (type, value) => { clipboard[type] = value; } } });
		document.dispatchEvent(partialCopy);
		assert(clipboard['text/plain'] === '%D0%9F', 'Copying a single decoded character restores its complete encoded byte sequence');
		const editor = document.createElement('input');
		document.body.append(editor);
		const editorCopy = new Event('copy', { bubbles: true, cancelable: true });
		Object.defineProperty(editorCopy, 'clipboardData', { value: { setData: () => { throw new Error('Description intercepted editor copy'); } } });
		editor.dispatchEvent(editorCopy);
		assert(!editorCopy.defaultPrevented, 'Editing fields keep their native copy behavior despite an old description selection');
		editor.remove();
		selectedRange.selectNodeContents(button('space', readable));
		selection.removeAllRanges();
		selection.addRange(selectedRange);
		const plainCopy = new Event('copy', { bubbles: true, cancelable: true });
		Object.defineProperty(plainCopy, 'clipboardData', { value: { setData: () => { throw new Error('Unchanged text copy overridden'); } } });
		document.dispatchEvent(plainCopy);
		assert(!plainCopy.defaultPrevented, 'Unchanged description selections retain native clipboard handling');
		selection.removeAllRanges();

		// Upload responses are manually emitted: no reducer or service is faked by
		// inferring transitions from labels or turning a click into a real upload.
		const youtubeId = { source: 'you-tube', external_id: 'dQw4w9WgXcQ' };
		snapshot({ screen: 'Search', rows: [], details: { ...details('Fixture YouTube video', youtubeId), source: 'YouTube' },
			archive_upload_supported: true, archive_upload_available: true, s3_upload_supported: true, s3_upload_available: true });
		await until(() => button('Upload to archive.org'), 'Archive upload action');
		await action('OpenArchiveUpload', () => button('Upload to archive.org').click(), 'Archive Details action opens publication review');
		const archive = { ...clone(defaults.ArchiveUploadPopupView), generation: 7, selected_field: 'Description', phase: 'Review',
			draft: { identifier: 'fixture-review', title: 'Fixture title', description: 'Full description\nAnother line', creator: 'Fixture creator',
				source_url: 'https://www.youtube.com/watch?v=dQw4w9WgXcQ', upload_video: false } };
		snapshot({ archive_upload_popup: archive });
		await until(() => dialog()?.textContent.includes('Fixture title'), 'Archive review');
		assert(dialog().querySelectorAll('[role=checkbox]').length === 1, 'Archive review has only the remembered video checkbox');
		assert(!/Source:|youtube\.com\/watch|I own this content|▶\s*Description/.test(dialog().textContent), 'Archive review omits the removed source, permission and disclosure-looking rows');
		await key('s', { Char: 's' }, { ctrlKey: true });
		await action({ SubmitArchiveUpload: 7 }, () => button('Upload', dialog()).click(), 'Archive submit carries its rendered generation');
		snapshot({ archive_upload_popup: { ...archive, generation: 8 }, archive_credentials_editor: {
			access_key_length: 12, secret_key_length: 18, secret_selected: true, validation_failed: false } });
		await until(() => dialog()?.textContent.includes('Session-only credentials'), 'Archive credential editor');
		assert(dialog().textContent.includes('12 characters entered') && dialog().textContent.includes('18 characters entered'), 'Archive credential fields render counts, not key values');
		assert(dialog().textContent.includes('secrets/archive-org.toml') && dialog().textContent.includes('Get archive.org upload keys'), 'Archive credentials show optional file instructions and the approved guide label');
		await action('SubmitArchiveCredentials', () => button('Use for session', dialog()).click(), 'Credential acceptance requests review, not publication');
		snapshot({ archive_credentials_editor: null, archive_upload_popup: { ...archive, generation: 42 } });
		await until(() => document.querySelectorAll('[role=dialog]').length === 1, 'review after credentials');
		await action({ SubmitArchiveUpload: 42 }, () => button('Upload', dialog()).click(), 'Archive review refreshes the confirmation generation after credentials');
		snapshot({ archive_upload_popup: { ...archive, phase: 'Uploading', uploaded_bytes: 50, total_bytes: 100 } });
		await until(() => dialog()?.textContent.includes('Uploading 50%'), 'Archive progress');
		assert(!button('Upload', dialog()) && dialog().querySelector('[role=checkbox]').disabled, 'Busy Archive publication has no submit button or editable video choice');
		await action('DismissArchiveUpload', () => button('Cancel', dialog()).click(), 'Busy cancel delegates cancellation to the controller');
		snapshot({ archive_upload_popup: { ...archive, phase: 'Failed', validation_error: 'Inspect the destination before opening a fresh review.' } });
		await until(() => dialog()?.textContent.includes('Inspect the destination before opening a fresh review.'), 'Archive failure');
		assert(!button('Upload', dialog()), 'Failed Archive publication cannot retry the old review');
		snapshot({ archive_upload_popup: { ...archive, phase: 'Complete', result_url: 'https://archive.org/details/fixture-review' } });
		await until(() => button('Open item', dialog()), 'Archive accepted result');
		assert(dialog().textContent.includes('Upload accepted; archive.org may still be processing.'), 'Archive success distinguishes acceptance from completed ingestion');
		await action('OpenArchiveUploadResult', () => button('Open item', dialog()).click(), 'Archive result uses the explicit native opener action');
		snapshot({ archive_upload_popup: null });
		await until(() => !dialog(), 'closed Archive review');

		await action('OpenS3Upload', () => button('Upload to S3').click(), 'S3 Details action opens destination review');
		const s3 = { ...clone(defaults.S3UploadPopupView), generation: 55, phase: 'Review', video_available: false,
			draft: { region: 'us-east-1', bucket: 'fixture-bucket', object_key: 'audio/fixture.opus', profile: '', upload_video: false } };
		snapshot({ s3_upload_popup: s3 });
		await until(() => dialog()?.textContent.includes('Destination: s3://fixture-bucket/audio/fixture.opus'), 'S3 destination');
		assert(dialog().textContent.includes('Bucket permissions apply.') && dialog().textContent.includes('Existing objects are not overwritten.'), 'S3 review states permissions and no-overwrite semantics');
		const beforeDisabledClick = calls.length;
		dialog().querySelector('[role=checkbox]').click();
		assert(dialog().querySelector('[role=checkbox]').disabled && calls.length === beforeDisabledClick, 'Audio-only source cannot request video from its disabled checkbox');
		await action('OpenS3Credentials', () => button('Session keys…', dialog()).click(), 'S3 review offers explicit session-key replacement');
		snapshot({ s3_credentials_editor: { access_key_length: 16, secret_key_length: 32, session_token_length: 48,
			selected_field: 'SessionToken', validation_failed: false }, s3_upload_popup: { ...s3, generation: 56 } });
		await until(() => dialog()?.textContent.includes('Session token (optional)'), 'S3 credential editor');
		assert(dialog().textContent.includes('48 characters entered') && dialog().textContent.includes('they are not saved'), 'S3 optional session token is count-only and session-scoped');
		await action('SubmitS3Credentials', () => button('Use for session', dialog()).click(), 'S3 credential acceptance is separate from upload');
		snapshot({ s3_credentials_editor: null, s3_upload_popup: { ...s3, generation: 57 } });
		await until(() => document.querySelectorAll('[role=dialog]').length === 1, 'S3 fresh review');
		await action({ SubmitS3Upload: 57 }, () => button('Upload', dialog()).click(), 'S3 submit uses the latest generation after editing credentials');
		snapshot({ s3_upload_popup: { ...s3, phase: 'Complete', result_location: 's3://fixture-bucket/audio/fixture.opus' } });
		await until(() => dialog()?.textContent.includes('Upload complete. Bucket permissions still apply.'), 'S3 completion');
		assert(!button('Upload', dialog()) && !button('Open item', dialog()), 'S3 completion is read-only and does not imply a public web link');
		snapshot({ s3_upload_popup: null, s3_upload_supported: false, s3_upload_available: false,
			archive_upload_supported: false, archive_upload_available: false });
		await until(() => !dialog() && !button('Upload to S3') && !button('Upload to archive.org'), 'feature-trimmed Details');
		checks.push('Feature-trimmed snapshots expose neither upload action');
		assert(failures.length === 0, 'The full browser journey reports no frontend runtime failures');
	}
	void run().then(() => ({ ok: true, checks }), (error) => ({ ok: false, error: String(error), checks,
		body: document.body?.innerText.slice(-12000), calls: calls.slice(-5) }))
		.then((report) => fetch('/__report', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(report) }));
})();
