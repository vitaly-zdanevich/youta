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
	const soundcloudArtwork = ['https://soundcloak.example/artwork-500', 'https://soundcloak.example/artwork-1080'];
	const nativeSoundcloudArtwork = soundcloudArtwork.map((url) => `youta://artwork/${encodeURIComponent(url)}`);
	const requestedSoundcloudArtwork = new Set();
	const controlledSoundcloudArtwork = new Map();
	const decodedSoundcloudArtwork = [];
	const waveformImage = 'data:image/svg+xml,' + encodeURIComponent(
		'<svg xmlns="http://www.w3.org/2000/svg" width="800" height="200"><path d="M0 100H100L150 10L200 190L250 50L300 150L350 100H800" stroke="white" fill="none"/></svg>',
	);
	const setAttribute = Element.prototype.setAttribute;
	const imageSource = Object.getOwnPropertyDescriptor(HTMLImageElement.prototype, 'src');
	const decodeImage = HTMLImageElement.prototype.decode;
	HTMLImageElement.prototype.decode = function() {
		const native = this.dataset.nativeArtwork;
		if (nativeSoundcloudArtwork.includes(native) || controlledSoundcloudArtwork.has(native)) {
			decodedSoundcloudArtwork.push({ native, image: this });
			if (controlledSoundcloudArtwork.get(native)?.rejectDecode) return Promise.reject(new Error('Fixture decode failure'));
		}
		return decodeImage.call(this);
	};
	/** Keep all image bytes local while allowing delayed and failed native responses. */
	function mockedArtwork(image, value, assign) {
		if (value !== nativeWaveformUrl && !nativeSoundcloudArtwork.includes(value) && !controlledSoundcloudArtwork.has(value)) return false;
		if (value !== nativeWaveformUrl) requestedSoundcloudArtwork.add(value);
		setAttribute.call(image, 'data-native-artwork', value);
		const fixture = controlledSoundcloudArtwork.get(value);
		if (fixture) {
			fixture.images.push(image);
			if (fixture.held) return true;
		}
		assign(fixture?.fail ? 'data:image/png;base64,broken' : waveformImage);
		return true;
	}
	Object.defineProperty(HTMLImageElement.prototype, 'src', {
		...imageSource,
		set(value) {
			if (!mockedArtwork(this, value, (source) => imageSource.set.call(this, source))) imageSource.set.call(this, value);
		},
	});
	Element.prototype.setAttribute = function(name, value) {
		if (this instanceof HTMLImageElement && name === 'src'
			&& mockedArtwork(this, value, (source) => setAttribute.call(this, name, source))) return;
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
		{ id: 'Local', label: 'Local', details_kind: 'Local', search_verb: null },
		{ id: 'History', label: 'Log', details_kind: 'Generic', search_verb: null },
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
				if (command === 'dispatch' || command === 'key' || command === 'frontend') return null;
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
	/** Core-projected email spans stay literal, selectable and explicitly activated through native actions. */
	async function checkEmailLinks() {
		const previous = clone(view);
		const address = 'sales+music@example.org';
		const description = `📮 Напишите (${address}), #Music; raw@test.example <b>literal</b>`;
		const range = (body, value, from = 0) => {
			const offset = body.indexOf(value, from);
			const start_byte = new TextEncoder().encode(body.slice(0, offset)).length;
			return { start_byte, end_byte: start_byte + new TextEncoder().encode(value).length };
		};
		const link = (label, url, description_range = null, internal_target = null) => ({
			prefix: '', label, url, description_range, internal_target, presentation: 'LabelOnly', wikidata_item_id: null,
		});
		const fixture = { ...details('Email links', { source: 'you-tube', external_id: 'fixture-mail' }), source: 'YouTube', description,
			links: [link('Contact', `mailto:${address}`), link(address, `mailto:${address}`, range(description, address)),
				link('#Music', 'https://www.youtube.com/hashtag/music', range(description, '#Music'), { YouTubeHashtag: 'Music' })],
			search_highlights: [{ field: 'Description', ranges: [range(description, 'sales+music')] }] };
		const beforeRender = calls.length;
		snapshot({ screen: 'Search', details: fixture, details_focused: false, external_opener_available: false });
		const panel = await until(() => document.querySelector('[data-description]')?.textContent === description
			&& document.querySelector('[aria-label=Details]'), 'email description snapshot');
		const inline = button(address, panel);
		const rail = button('Contact', panel);
		assert(inline.disabled && rail.disabled, 'inline and rail emails honor unavailable external opener');
		assert(inline.title === 'No external opener available' && rail.title === inline.title, 'email tooltip explains unavailable opener');
		assert(!button('#Music', panel).disabled, 'internal metadata remains usable without external opener');
		assert(inline.querySelector('mark')?.textContent === 'sales+music', 'email span retains search highlighting after Unicode prefix');
		assert(!panel.querySelector('b') && !button('raw@test.example', panel), 'markup and unannotated email-looking text remain inert');
		inline.click();
		rail.click();
		assert(!calls.slice(beforeRender).some((call) => call.command === 'dispatch'), 'rendering and disabled email clicks never open an app');
		await action({ ActivateDetailLink: 2 }, () => button('#Music', panel).click(), 'email gating preserves internal metadata action');
		snapshot({ external_opener_available: true });
		await until(() => !inline.disabled && !rail.disabled, 'available email opener');
		assert(inline.title === 'Compose email in your default mail app' && rail.title === inline.title, 'email tooltip names default mail application');
		const selection = window.getSelection();
		const selected = document.createRange();
		selected.selectNodeContents(inline);
		selection.removeAllRanges();
		selection.addRange(selected);
		assert(selection.toString() === address, 'email address remains selectable independently of punctuation');
		selection.removeAllRanges();
		for (const [email, index] of [[inline, 1], [rail, 0]]) {
			await action({ SelectDetailLink: index }, () => email.focus(), 'tab focus selects email without changing reducer Details focus');
			const start = calls.length;
			const enter = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true });
			await action({ ActivateDetailLink: index }, () => email.dispatchEvent(enter), 'focused email Enter composes using its exact detail index');
			assert(enter.defaultPrevented && calls.slice(start).filter((call) => call.command === 'dispatch').length === 1
				&& !calls.slice(start).some((call) => call.command === 'key'), 'email Enter cannot activate the selected track or dispatch twice');
		}
		await action({ SelectDetailLink: 1 }, () => inline.focus(), 'focusing email selects its existing global detail index');
		await action({ ActivateDetailLink: 1 }, () => inline.click(), 'inline email dispatches its core link index');
		await action({ ActivateDetailLink: 0 }, () => rail.click(), 'rail email dispatches its core link index');
		const commentText = `📮 (${address}), again ${address}. <script>literal</script>\n`;
		const email_links = [range(commentText, address), range(commentText, address, commentText.indexOf(address) + address.length)]
			.map((span) => ({ ...span, url: `mailto:${address}` }));
		for (const source of ['you-tube', 'sound-cloud', 'archive-org']) {
			const popup = { source, video_id: `mail-${source}`, video_title: 'Email comments', state: 'Ready', scroll_offset: 0,
				comments: [{ author_name: 'Reader', author_url: null, like_count: 2, published: null, text: commentText, email_links }] };
			snapshot({ video_comments_popup: popup, external_opener_available: false });
			await until(() => dialog()?.querySelectorAll('button[title="No external opener available"]').length === 2, 'disabled comment emails');
			assert(dialog().textContent.includes(commentText.trimEnd()) && !dialog().querySelector('script'), `${source} comments keep literal body text`);
			assert([...dialog().querySelectorAll('button[title="No external opener available"]')].every((node) => node.disabled), `${source} comment emails honor opener availability`);
			snapshot({ external_opener_available: true });
			const emails = await until(() => {
				const buttons = dialog()?.querySelectorAll('button[title="Compose email in your default mail app"]');
				return buttons?.length === 2 && buttons;
			}, 'clickable comment emails');
			const payload = (email_index) => ({ ActivateCommentEmail: { source, video_id: popup.video_id, comment_index: 0, email_index } });
			await action(payload(1), () => emails[1].click(), `${source} email click captures popup owner and occurrence`);
			const beforeEnter = calls.length;
			emails[0].focus();
			assert(calls.length === beforeEnter, 'focusing comment email does not open an app');
			const enter = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true });
			await action(payload(0), () => emails[0].dispatchEvent(enter), `${source} comment email Enter activates once`);
			assert(enter.defaultPrevented && calls.slice(beforeEnter).filter((call) => call.command === 'dispatch').length === 1
				&& !calls.slice(beforeEnter).some((call) => call.command === 'key'), 'comment email Enter consumes the document key path without duplicate dispatch');
			await action('DismissVideoComments', () => dialog().querySelector('button[aria-label=Close]').click(), 'email comments remain closeable');
			snapshot({ video_comments_popup: null });
			await until(() => !dialog(), 'closed email comments');
		}
		snapshot(previous);
	}
	/** YouTube exposes the saved Shorts filter alongside the existing global playback controls. */
	async function checkYouTubeSearchControls() {
		const previous = clone(view);
		for (const enabled of [false, true]) {
			snapshot({ screen: 'Search', details: null, rows: [], search_query: '', search_editing: false,
				autoplay: enabled, repeating: enabled,
				subscriptions: { ...view.subscriptions, show_youtube_shorts: enabled } });
			const shorts = await until(() => {
				const search = document.querySelector('[role=search]');
				return search && button(`[h] Shorts: ${enabled ? 'on' : 'off'}`, search);
			}, 'YouTube search Shorts state');
			assert(shorts.getAttribute('aria-pressed') === String(enabled) && !shorts.disabled,
				'YouTube Shorts reflects the shared saved preference');
			const player = document.querySelector('footer');
			assert(button('Autoplay', player)?.getAttribute('aria-pressed') === String(enabled)
				&& button('Repeat', player)?.getAttribute('aria-pressed') === String(enabled),
				'YouTube retains the global Autoplay and Repeat controls with their shared states');
			await action('ToggleSubscriptionShorts', () => shorts.click(), 'YouTube Shorts dispatches its existing shared action');
		}
		await action('ToggleAutoplay', () => button('Autoplay', document.querySelector('footer')).click(), 'YouTube Autoplay dispatches its global action');
		await action('ToggleRepeat', () => button('Repeat', document.querySelector('footer')).click(), 'YouTube Repeat dispatches its global action');
		snapshot({ search_editing: true, search_query: 'h', search_cursor_byte: 1 });
		const disabled = await until(() => button('[h] Shorts: on')?.disabled && button('[h] Shorts: on'), 'Shorts disabled while editing search');
		const beforeTyping = calls.length;
		disabled.click();
		await key('h', { Char: 'h' });
		assert(!calls.slice(beforeTyping).some((call) => call.command === 'dispatch'),
			'While editing, h reaches the shared keymap and the disabled button cannot toggle Shorts');
		snapshot({ search_query: 'hh', search_cursor_byte: 2 });
		await until(() => document.querySelector('[role=search]')?.textContent.includes('hh'), 'typed h remains in the search query');
		for (const screen of ['YouTubeMusic', 'ArchiveOrg', 'SoundCloud', 'Local']) {
			snapshot({ screen, search_editing: false });
			await until(() => !button('[h] Shorts: on'), `No YouTube Shorts search control on ${screen}`);
			assert(!button('[h] Shorts: off'), `Shorts filtering stays scoped to YouTube: ${screen}`);
		}
		snapshot(previous);
	}

	/** The live duration is red in results, subscriptions, Details and the transport. */
	async function checkLiveDuration() {
		const previous = clone(view);
		const id = { source: 'you-tube', external_id: 'fixture-live' };
		const liveRow = { ...row('LIVE', id), source: 'YouTube', subtitle: 'LIVE · LIVE', live: true };
		const ordinary = { ...row('Ordinary video', id), subtitle: 'LIVE · 0:00', live: false };
		snapshot({ screen: 'Search', rows: [liveRow, ordinary],
			details: { ...details('LIVE', id), source: 'YouTube', channel_name: 'LIVE', length: 'LIVE', live: true },
			now_playing: { media_id: id, title: 'LIVE', subtitle: '' },
			playback: { ...view.playback, idle: false, live: true, duration: null,
				position: { secs: 0, nanos: 0 }, live_seekable_range: null } });
		const resultMarker = await until(() => document.querySelector('[aria-label=Results] .text-red-400'), 'red LIVE in search results');
		const lengthMarker = await until(() => document.querySelector('[aria-label=Details] dd.text-red-400'), 'red LIVE Length in Details');
		const playerMarker = await until(() => document.querySelector('footer .text-red-400'), 'red LIVE in the player');
		const probe = document.createElement('span');
		probe.style.color = 'var(--color-red-400)';
		document.body.append(probe);
		for (const marker of [resultMarker, lengthMarker, playerMarker]) {
			assert(marker.textContent === 'LIVE' && getComputedStyle(marker).color === getComputedStyle(probe).color,
				'The canonical LIVE indicator uses the built red color');
			assert(getComputedStyle(marker).color !== getComputedStyle(marker.parentElement).color,
				'LIVE styling remains distinct from its surrounding title and channel text');
		}
		probe.remove();
		assert(document.querySelectorAll('[aria-label=Results] .text-red-400').length === 1,
			'A channel named LIVE and unknown ordinary duration stay uncolored');
		assert(resultMarker.parentElement.textContent === 'LIVE · LIVE', 'The complete channel and live duration remain visible');
		assert(!document.querySelector('footer').textContent.includes('--:--'), 'Live playback omits an unknown finite duration');
		assert(document.querySelector('[aria-label="Playback position"]').disabled, 'An unseekable live stream keeps seeking disabled');
		snapshot({ playback: { ...view.playback, position: { secs: 240, nanos: 0 }, duration: { secs: 300, nanos: 0 },
			live_seekable_range: { start: { secs: 0, nanos: 0 }, end: { secs: 300, nanos: 0 } } } });
		await until(() => document.querySelector('footer').textContent.includes('LIVE −1:00'), 'live edge offset in DVR playback');
		assert(document.querySelector('footer').textContent.includes('5:00 buffer'), 'Seekable live playback retains the available buffer duration');
		assert(!document.querySelector('[aria-label="Playback position"]').disabled, 'The live DVR buffer remains seekable');
		const dvrMarker = document.querySelector('footer .text-red-400');
		assert(dvrMarker.textContent === 'LIVE'
			&& getComputedStyle(dvrMarker).color !== getComputedStyle(dvrMarker.parentElement).color,
			'Only LIVE is red; its DVR offset retains the muted status color');
		snapshot({ queue_popup: { current: 0, selected: 0, repeat_one: false, items: [
			{ media_id: id, title: 'LIVE', subtitle: 'LIVE', length: 'LIVE' },
			{ media_id: { ...id, external_id: 'ordinary' }, title: 'LIVE', subtitle: 'LIVE', length: '0:00' },
			{ media_id: { source: 'local', external_id: '/music/live.flac' }, title: 'LIVE', subtitle: 'LIVE', length: '3:21' },
		] } });
		const queueMarker = await until(() => dialog()?.querySelector('.text-red-400'), 'red LIVE in the playback queue');
		assert(queueMarker.textContent === 'LIVE' && dialog().querySelectorAll('.text-red-400').length === 1
			&& queueMarker.closest('button') === null, 'Queue styling applies only to the canonical YouTube LIVE length');
		snapshot({ queue_popup: null });
		await until(() => !dialog(), 'close the live queue fixture');
		snapshot({ screen: 'Subscriptions', details: null, subscriptions: {
			...clone(defaults.ViewModel.subscriptions), layout: 'drill-down', route: 'Items', focus: 'Items',
			source_kind: 'you-tube', source_title: 'LIVE', items: [liveRow, ordinary],
		} });
		const subscriptionMarker = await until(() => document.querySelector('[data-subscription-pane] .text-red-400'), 'red LIVE in subscription items');
		assert(subscriptionMarker.textContent === 'LIVE'
			&& document.querySelectorAll('[data-subscription-pane] .text-red-400').length === 1,
			'Subscriptions shares the confirmed live duration styling without coloring the channel');
		snapshot(previous);
		await until(() => !document.querySelector('[data-subscriptions-screen]'), 'restore the prior screen after live fixtures');
	}

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
		await until(() => button('YouTube API / Invidious...', dialog()), 'provider settings in Preferences');
		await action('OpenYouTubeProviderSettings', () => button('YouTube API / Invidious...', dialog()).click(), 'Preferences opens the shared provider editor');
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
		assert(!button('YouTube API / Invidious...', dialog()), 'Unsupported builds hide provider settings in Preferences');
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

	/** A live station has no finite duration; recording exports are not live-stream actions. */
	async function checkRadioPresentation() {
		const previous = clone(view);
		const radioId = { source: 'radio', external_id: 'fixture-station' };
		snapshot({ screen: 'Radio', now_playing: { media_id: radioId, title: 'Fixture station', subtitle: '' }, playing_media_id: radioId,
			evernote_available: true, details: { ...details('Fixture station', radioId), source: 'Radio' },
			playback: { ...view.playback, idle: false, live: true, title: 'Fixture station', position: { secs: 86387, nanos: 0 },
				duration: { secs: 86400, nanos: 0 }, live_seekable_range: { start: { secs: 0, nanos: 0 }, end: { secs: 86400, nanos: 0 } } } });
		await until(() => document.querySelector('footer')?.textContent.includes('Fixture station'), 'live station footer');
		const footer = document.querySelector('footer');
		assert(footer.textContent.includes('radio') && !footer.textContent.includes('24:00:00'), 'radio footer omits live-buffer duration');
		assert(!document.querySelector('[aria-label="Playback position"]').disabled, 'radio label keeps buffered seeking available');
		assert(!button('[E] To Evernote'), 'live radio has no direct Evernote upload action');
		await action('ShowNowPlaying', () => button('Fixture station', footer).click(), 'radio title retains source navigation');
		const beforeOffer = calls.length;
		snapshot({ evernote_popup: {
			draft: { title: 'Fixture station recording', body: '', tags: '', source_url: '' },
			selected_field: 'Title', phase: 'Review', animation_frame: 0, total_bytes: null,
			validation_error: null, result_url: null, captions_available: false, undo_available: false,
		} });
		await until(() => dialog()?.textContent.includes('Fixture station recording'), 'completed recording review');
		assert(!calls.slice(beforeOffer).some((call) => call.command === 'dispatch'), 'Showing a completed recording offer does not submit it');
		assert(Boolean(button('Save note', dialog())) && Boolean(button('Cancel', dialog())), 'Completed recording offer requires an explicit save or cancel');
		await action('SubmitEvernoteNote', () => button('Save note', dialog()).click(), 'Recording review saves only through the existing explicit submit action');
		await action('DismissEvernoteNote', () => button('Cancel', dialog()).click(), 'Recording offer can be skipped without uploading');
		snapshot({ evernote_popup: null });
		await until(() => !dialog(), 'closed recording review');
		snapshot(previous);
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
		const trackDetails = { ...details(track.title, track.media_id), source: 'SoundCloud', length: '0:30 preview (full track 3:00)',
			likes: '1,234', comments: '12', license: 'cc-by', thumbnail_url: soundcloudArtwork[0], expanded_thumbnail_url: soundcloudArtwork[1],
			soundcloud: { plays: 5678, reposts: 90, created: '2024 March 2', modified: '2025 January 4',
				tags: ['ambient', 'field recording'], preview_duration_seconds: 30 },
			links: [{ prefix: 'Genre: ', label: 'Ambient & Field', url: 'https://soundcloak.example/tags/Ambient%20%26%20Field',
				presentation: 'LabelOnly', description_range: null, internal_target: null, wikidata_item_id: null, youtube_channel_id: null, media: null }] };
		snapshot({ details: trackDetails });
		const panel = await until(() => document.querySelector('[aria-label=Details]')?.textContent.includes('Reposts') && document.querySelector('[aria-label=Details]'), 'SoundCloud source-specific metadata');
		for (const label of ['Plays', '5.7K', 'Likes', '1,234', 'Reposts', '90', 'Created', '2024 March 2', 'Modified', '2025 January 4', 'cc-by', 'field recording', '0:30 preview']) {
			assert(panel.textContent.includes(label), `SoundCloud Details renders ${label}`);
		}
		assert(!panel.textContent.includes('Views') && !panel.textContent.includes('Published'), 'SoundCloud facts do not mislabel plays or creation');
		await action({ ActivateDetailLink: 0 }, () => button('Ambient & Field', panel).click(), 'SoundCloud genre remains an explicit clickable source link');
		await action('OpenVideoComments', () => button('Comments', panel).click(), 'SoundCloud comments load only through the explicit shared action');
		await until(() => panel.querySelector('img[data-native-artwork]'), 'SoundCloud Details preview artwork');
		await until(() => requestedSoundcloudArtwork.has(nativeSoundcloudArtwork[1]), 'selected SoundCloud 1080px artwork prefetch before expansion');
		await until(() => decodedSoundcloudArtwork.some(({ native }) => native === nativeSoundcloudArtwork[1]), 'selected SoundCloud background artwork decode');
		assert(decodedSoundcloudArtwork.find(({ native }) => native === nativeSoundcloudArtwork[1]).image.isConnected === false, 'SoundCloud prefetch uses one detached image without replacing Details');
		assert(panel.querySelector('img').dataset.nativeArtwork === nativeSoundcloudArtwork[0], 'SoundCloud Details stays at 500px while 1080px loads in the background');
		await action('ToggleThumbnailExpansion', () => panel.querySelector('img').click(), 'SoundCloud image expansion is explicit');
		snapshot({ details: { ...trackDetails, thumbnail_expanded: true } });
		await until(() => dialog()?.querySelector('img')?.dataset.nativeArtwork === nativeSoundcloudArtwork[1], 'expanded SoundCloud artwork uses the prefetched cached URL');
		snapshot({ details: trackDetails });
		await until(() => !dialog(), 'collapsed SoundCloud artwork');
		await checkSoundCloudArtworkOwnership(trackDetails);
		await checkSoundCloudNavigation(trackDetails);
		snapshot(previous);
		await until(() => document.querySelector('[title="Search archive.org"]'), 'restored Archive fixture');
	}

	/** Author, artist and tag navigation remain internal shared actions, without a browser opener. */
	async function checkSoundCloudNavigation(base) {
		const profile = 'https://soundcloud.com/canonical-artist';
		snapshot({ video_comments_popup: { source: 'sound-cloud', video_id: 'track', video_title: 'Commented track', state: 'Ready', scroll_offset: 0,
			comments: [{ author_name: 'Display artist', author_url: profile, like_count: 0, published: '2026 September 23', text: 'Comment text', email_links: [] },
				{ author_name: 'Unknown author', author_url: null, like_count: 0, published: null, text: 'Another comment', email_links: [] }] } });
		const author = await until(() => button('Display artist', dialog()), 'clickable SoundCloud comment author');
		assert(author.title === profile && author.className.includes('underline'), 'comment author is visibly linked to its canonical profile');
		assert(!button('Unknown author', dialog()), 'comment without canonical author URL stays plain');
		await action({ OpenVideoCommentAuthor: 0 }, () => author.click(), 'comment author opens its artist in Youta');
		snapshot({ video_comments_popup: { ...view.video_comments_popup, source: 'youtube' } });
		await until(() => !button('Display artist', dialog()), 'YouTube author does not inherit SoundCloud navigation');
		snapshot({ video_comments_popup: null, soundcloud_back_available: true, external_opener_available: false,
			details: { ...base, thumbnail_url: null, expanded_thumbnail_url: null, links: [
				{ prefix: 'Artist: ', label: 'Artist tracks', url: profile, presentation: 'LabelOnlySpaced', description_range: null,
					internal_target: { SoundCloudArtist: profile }, wikidata_item_id: null },
				{ prefix: '', label: 'Artist albums', url: profile, presentation: 'LabelOnlySpaced', description_range: null,
					internal_target: { SoundCloudArtistAlbums: profile }, wikidata_item_id: null },
				{ prefix: 'Tag: ', label: 'field recording', url: 'https://soundcloud.com/tags/field%20recording', presentation: 'LabelOnlySpaced', description_range: null,
					internal_target: { SoundCloudTag: 'field recording' }, wikidata_item_id: null },
			] } });
		await until(() => !dialog() && button('Artist tracks'), 'SoundCloud artist and tag links');
		const panel = document.querySelector('[aria-label=Details]');
		for (const [index, label] of ['Artist tracks', 'Artist albums', 'field recording'].entries()) {
			await action({ ActivateDetailLink: index }, () => button(label, panel).click(), `${label} navigates internally without external opener`);
		}
		assert(!panel.textContent.includes('Tags'), 'clickable tags replace duplicate plain-text tags');
		await action('GoBack', () => button('[Esc] Back').click(), 'SoundCloud Back restores the preceding route');
		snapshot({ soundcloud_back_available: false });
		await until(() => !button('[Esc] Back'), 'SoundCloud root hides unavailable Back');
	}

	/** A selected rendition owns its callbacks; hidden rows and stale failures never do. */
	async function checkSoundCloudArtworkOwnership(base) {
		const fixture = (slug, preview = {}, expanded = {}) => {
			const urls = [500, 1080].map((size) => `https://soundcloak.example/artwork-${slug}-${size}`);
			const native = urls.map((url) => `youta://artwork/${encodeURIComponent(url)}`);
			for (const [index, options] of [preview, expanded].entries()) controlledSoundcloudArtwork.set(native[index], { images: [], ...options });
			return { native, urls, details: { ...base, title: `SoundCloud ${slug}`,
				media_id: { source: 'sound-cloud', external_id: `https://soundcloud.com/artist/${slug}` },
				thumbnail_url: urls[0], expanded_thumbnail_url: urls[1], thumbnail_expanded: false } };
		};
		const selected = fixture('held', { held: true }, { held: true });
		const unselected = fixture('unselected');
		snapshot({ details: selected.details, rows: [row('Selected SoundCloud', selected.details.media_id),
			{ ...row('Unselected SoundCloud', unselected.details.media_id), thumbnail_url: unselected.urls[0] }] });
		await until(() => controlledSoundcloudArtwork.get(selected.native[0]).images.length, 'held selected preview request');
		await new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)));
		assert(!requestedSoundcloudArtwork.has(selected.native[1]), 'SoundCloud background loading waits for the visible 500px image');
		for (const image of controlledSoundcloudArtwork.get(selected.native[0]).images) imageSource.set.call(image, waveformImage);
		const old = await until(() => controlledSoundcloudArtwork.get(selected.native[1]).images[0], 'selected held 1080px prefetch');
		const staleLoad = old.onload;
		assert(typeof staleLoad === 'function', 'The selected prefetch has an explicit guarded completion');
		assert(!requestedSoundcloudArtwork.has(unselected.native[1]), 'Unselected result artwork never triggers a 1080px prefetch');
		const replacement = fixture('replacement', {}, { rejectDecode: true });
		snapshot({ details: replacement.details });
		await until(() => decodedSoundcloudArtwork.some(({ native }) => native === replacement.native[1]), 'replacement prefetch attempts decode');
		assert(old.onload === null && old.onerror === null && old.getAttribute('src') === null, 'Changing selection retires the previous image owner and callbacks');
		staleLoad.call(old, new Event('load'));
		assert(!decodedSoundcloudArtwork.some(({ native }) => native === selected.native[1]), 'A captured stale completion cannot decode the previous track');
		assert(document.querySelector('[aria-label=Details] img')?.dataset.nativeArtwork === replacement.native[0], 'A quiet background decode failure leaves the replacement preview intact');
		const failedPreview = fixture('failed-preview', { fail: true });
		snapshot({ details: failedPreview.details });
		await until(() => document.querySelector('[aria-label=Details]')?.textContent.includes(failedPreview.details.title)
			&& !document.querySelector('[aria-label=Details] img'), 'failed preview placeholder');
		assert(!requestedSoundcloudArtwork.has(failedPreview.native[1]), 'A failed 500px preview does not prefetch its larger rendition');
		const failedLarge = fixture('failed-large', {}, { fail: true });
		snapshot({ details: failedLarge.details });
		await until(() => requestedSoundcloudArtwork.has(failedLarge.native[1]), 'failed background rendition request');
		assert(document.querySelector('[aria-label=Details] img')?.dataset.nativeArtwork === failedLarge.native[0], 'A new selection recovers after preview failure and background HTTP/image failure stays quiet');
		snapshot({ details: null });
		await until(() => !document.querySelector('[aria-label=Details]'), 'cleared selected artwork owner');
	}
	/** Local timestamps follow the selectable standalone path without becoming seek controls. */
	async function checkLocalFullPath() {
		const previous = clone(view);
		for (const [path, kind, created, modified] of [
			['/fixture/library/audio.flac', 'file', '2026 August 25 14:20', '2026 September 3 09:05'],
			['~/library/audio.flac', 'file', '2026 August 25 14:20', '2026 September 3 09:05'],
			['/fixture/library/no-birth-time.flac', 'file', 'unavailable', '2026 September 3 09:05'],
			['/fixture/library/no-modified-time.flac', 'file', '2026 August 25 14:20', 'unavailable'],
			['/fixture/library/unavailable.flac', 'file', 'unavailable', 'unavailable'],
			['/fixture/library/archive.zip!/audio.flac', 'archive member', null, null],
			['/fixture/library', 'folder', null, null],
		]) {
			const lines = ['Full path:', path];
			if (created !== null) lines.push(`Created: ${created}`, `Modified: ${modified}`);
			const description = lines.join('\n');
			snapshot({ screen: 'Local', details: { ...clone(defaults.DetailView),
				title: 'Local path fixture', source: 'Local', description,
				media_id: kind === 'folder' ? null : { source: 'local', external_id: path },
			} });
			const rendered = await until(() => {
				const node = document.querySelector('[data-description]');
				return node?.textContent === description && node;
			}, `Local path description: ${path}`);
			const text = document.createTreeWalker(rendered, NodeFilter.SHOW_TEXT).nextNode();
			const range = document.createRange();
			range.setStart(text, 0);
			range.setEnd(text, 'Full path:'.length);
			const heading = range.getBoundingClientRect();
			range.setStart(text, 'Full path:\n'.length);
			range.setEnd(text, 'Full path:\n'.length + 1);
			const value = range.getBoundingClientRect();
			const lineHeight = Number.parseFloat(getComputedStyle(rendered).lineHeight);
			assert(Math.abs(value.top - heading.top - lineHeight) < 1, `Local path follows its heading on the next line: ${path}`);
			assert(Math.abs(value.left - heading.left) < 1, `Local path starts at the heading's left edge: ${path}`);
			const panel = document.querySelector('[aria-label=Details]');
			const styles = getComputedStyle(panel);
			const availableWidth = panel.clientWidth - Number.parseFloat(styles.paddingLeft) - Number.parseFloat(styles.paddingRight);
			assert(Math.abs(rendered.getBoundingClientRect().width - availableWidth) < 1, `Local path keeps the full description width: ${path}`);
			range.setEnd(text, 'Full path:\n'.length + path.length);
			const selection = window.getSelection();
			selection.removeAllRanges();
			selection.addRange(range);
			assert(styles.userSelect === 'text' && selection.toString() === path,
				`The complete Local path remains selectable without timestamp text: ${path}`);
			selection.removeAllRanges();
			assert(rendered.querySelector('button, a') === null,
				`Local path and HH:MM metadata remain plain text without seek controls: ${path}`);
			if (created !== null) {
				for (const [offset, label] of ['Created:', 'Modified:'].entries()) {
					const start = description.indexOf(label);
					range.setStart(text, start);
					range.setEnd(text, start + label.length);
					const timestamp = range.getBoundingClientRect();
					assert(Math.abs(timestamp.top - value.top - lineHeight * (offset + 1)) < 1,
						`${label} follows the path in order on its own line: ${path}`);
				}
				assert(rendered.textContent === `Full path:\n${path}\nCreated: ${created}\nModified: ${modified}`,
					`Local dates retain the exact English-month minute format or unavailable value: ${path}`);
			} else {
				assert(!rendered.textContent.includes('Created:') && !rendered.textContent.includes('Modified:'),
					`The ${kind} description does not invent filesystem timestamps`);
			}
		}
		snapshot(previous);
	}
	/** Local track metadata stays in the shared description and missing tags remain absent. */
	async function checkLocalTrackMetadata() {
		const previous = clone(view);
		for (const [label, description] of [
			['number', 'Album: Fixture album\nTrack: 3\n\nFull path:\n/fixture/music/audio.flac'],
			['number and total', 'Album: Fixture album\nTrack: 3/12\n\nFull path:\n/fixture/music/audio.flac'],
			['missing', 'Album: Fixture album\n\nFull path:\n/fixture/music/audio.flac'],
		]) {
			const title = `Local track metadata: ${label}`;
			snapshot({ screen: 'Local', details: { ...clone(defaults.DetailView),
				title, source: 'Local', description,
				media_id: { source: 'local', external_id: '/fixture/music/audio.flac' },
			} });
			await until(() => document.querySelector('[aria-label=Details] h2')?.textContent === title, title);
			const panel = document.querySelector('[aria-label=Details]');
			assert(panel.querySelector('[data-description]')?.textContent === description,
				`Local Details preserves the shared track metadata description: ${label}`);
			if (label === 'missing') assert(!panel.textContent.includes('Track:'), 'Missing Local track metadata does not fabricate a field');
		}
		snapshot(previous);
	}
	/** Local mutations keep Move then Rename while each capability remains independent. */
	async function checkLocalActionOrder() {
		const previous = clone(view);
		for (const [movable, renamable] of [[true, true], [true, false], [false, true], [false, false]]) {
			const title = `Local actions: move=${movable}, rename=${renamable}`;
			snapshot({ screen: 'Local', details: { ...clone(defaults.DetailView),
				title, source: 'Local', local_movable: movable, local_renamable: renamable,
				local_trashable: true,
			} });
			await until(() => document.querySelector('[aria-label=Details] h2')?.textContent === title, title);
			const panel = document.querySelector('[aria-label=Details]');
			const move = button('Move...', panel);
			const rename = button('Rename', panel);
			assert(Boolean(move) === movable, `Move follows its capability: ${title}`);
			assert(Boolean(rename) === renamable, `Rename follows its capability: ${title}`);
			if (move && rename) assert(move.nextElementSibling === rename, 'Local Rename immediately follows Move');
			if (move) await action('BeginLocalMove', () => move.click(), `Local Move dispatches its shared action: ${title}`);
			if (rename) await action('BeginLocalRename', () => rename.click(), `Local Rename dispatches its shared action: ${title}`);
			await action('RequestLocalTrash', () => button('Trash', panel).click(), `Local Trash remains independent: ${title}`);
		}
		snapshot(previous);
	}
	/** Copy uses its own capability and confirmation, while the destination browser is shared. */
	async function checkLocalCopy() {
		const previous = clone(view);
		for (const copyable of [true, false]) {
			const title = `Local copy capability: ${copyable}`;
			snapshot({ screen: 'Local', details: { ...clone(defaults.DetailView), title, source: 'Local',
				local_copyable: copyable, local_movable: true, local_renamable: true } });
			await until(() => document.querySelector('[aria-label=Details] h2')?.textContent === title, title);
			const panel = document.querySelector('[aria-label=Details]');
			const copy = button('Copy', panel);
			assert(Boolean(copy) === copyable, `Copy follows its own capability: ${copyable}`);
			if (copy) {
				assert(copy.nextElementSibling === button('Move...', panel), 'Copy precedes the adjacent Move and Rename actions');
				await action('BeginLocalCopy', () => copy.click(), 'Copy opens the shared destination workflow');
			}
		}
		const destination = { source_names: ['track.flac', 'Album'], destination: '/fixture/destination',
			directories: [{ name: '..', path: '/fixture' }, { name: 'Target', path: '/fixture/destination/Target' }],
			selected: 0, pending: false, error: 'Destination already contains track.flac' };
		for (const mode of ['Copy', 'Move']) {
			snapshot({ local_file_popup: { [mode]: destination }, local_file_progress: null });
			await until(() => dialog()?.textContent.includes(`${mode} 2 items`), `${mode} destination picker`);
			assert(dialog().textContent.includes(destination.error), `${mode} retains its recoverable error`);
			const directory = [...dialog().querySelectorAll('button')].find((node) => node.textContent.includes('Target'));
			await action({ SelectLocalMoveDestination: 1 }, () => directory.click(), `${mode} selects a shared destination row`);
			await action('ActivateLocalMoveDestination', () => directory.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })), `${mode} opens a shared destination row`);
			await action(`ConfirmLocal${mode}Here`, () => button(`${mode} here`, dialog()).click(), `${mode} confirms only its own operation`);
			for (const [totalBytes, totalEntries, completedBytes, completedEntries, label] of [
				[null, 0, 0, 0, 'Preparing...'], [100, 2, 50, 1, '50 / 100 bytes'],
				[null, 2, 0, 1, '1 / 2 entries'], [0, 2, 0, 1, '0 / 0 bytes'],
				[null, 2, 50, 0, '50 bytes - 0 / 2 entries'],
			]) {
				const progress = { completed_bytes: completedBytes, total_bytes: totalBytes,
					completed_entries: completedEntries, total_entries: totalEntries };
				snapshot({ local_file_progress: progress });
				await until(() => dialog()?.textContent.includes(label), `${mode} progress: ${label}`);
				assert([...dialog().querySelectorAll('button')].every((node) => node.disabled), `${mode} blocks dismissal, confirmation and destination navigation during ${label}`);
				if (totalEntries) assert(dialog().querySelector('progress')?.value === (totalBytes > 0 ? completedBytes / totalBytes : completedEntries / totalEntries), `${mode} shows bounded byte or entry progress for ${label}`);
				const start = calls.length;
				button(`${mode} here`, dialog()).click();
				button('Cancel', dialog()).click();
				for (const keyName of ['Escape', 'Enter', 'c', 'm', 'q']) {
					const event = new KeyboardEvent('keydown', { key: keyName, bubbles: true, cancelable: true });
					document.dispatchEvent(event);
					assert(event.defaultPrevented, `${mode} blocks ${keyName} during ${label}`);
				}
				assert(calls.length === start, `${mode} progress cannot start or dismiss an operation`);
			}
			snapshot({ local_file_popup: { [mode]: { ...destination, pending: true } } });
			await until(() => dialog()?.textContent.includes(mode === 'Copy' ? 'Copying...' : 'Moving...'), `${mode} foreground status`);
			assert(!dialog().textContent.includes('Listing'), `${mode} progress does not claim it is listing destinations`);
		}
		snapshot({ ...previous, local_file_popup: null, local_file_progress: null });
		await until(() => !dialog(), 'closed transfer fixture');
	}
	/** Local publication uses controller capabilities and never exposes a private file path in review. */
	async function checkArchiveLocalUpload() {
		const previous = clone(view);
		for (const [supported, available, folder] of [[true, true, false], [true, false, false], [true, false, true], [false, false, false]]) {
			const title = `Local upload: supported=${supported}, available=${available}, folder=${folder}`;
			snapshot({ screen: 'Local', archive_upload_supported: supported, archive_upload_available: available,
				details: { ...clone(defaults.DetailView), title, source: 'Local',
					media_id: folder ? null : { source: 'local', external_id: 'file:///private/library/track.flac' } } });
			await until(() => document.querySelector('[aria-label=Details] h2')?.textContent === title, title);
			const upload = button('[I] To archive.org');
			assert(Boolean(upload) === (supported && available), `Archive Local button follows shared capability: ${title}`);
			if (upload) await action('OpenArchiveUpload', () => upload.click(), 'Local file opens explicit Archive upload review');
		}
		for (const video_available of [false, true]) {
			const popup = { ...clone(defaults.ArchiveUploadPopupView), generation: 72, video_available,
				draft: { ...clone(defaults.ArchiveUploadPopupView.draft), source: 'Local', source_url: '',
					identifier: 'local-fixture', title: 'Local fixture', description: 'Public description', upload_video: true } };
			snapshot({ archive_upload_popup: popup });
			await until(() => dialog()?.textContent.includes('Local fixture'), 'Local Archive review');
			const checkbox = dialog().querySelector('[role=checkbox]');
			await until(() => checkbox.disabled === !video_available, 'Local Archive video capability');
			assert(checkbox.getAttribute('aria-checked') === String(video_available), 'Local audio never displays a stale video choice');
			assert(!dialog().textContent.includes('/private/library') && !dialog().textContent.includes('file://'), 'Local Archive review omits private path/source URL');
			if (video_available) await action('ToggleArchiveUploadVideo', () => checkbox.click(), 'Local video keeps the shared export toggle');
			await action({ SubmitArchiveUpload: 72 }, () => button('Upload', dialog()).click(), 'Local publication requires explicit generation-bound submission');
		}
		snapshot({ ...previous, archive_upload_popup: null });
		await until(() => !dialog(), 'closed Local Archive review');
	}
	/** Compact Evernote entrypoints preserve selected-item capability and defer key mapping to Rust. */
	async function checkEvernoteButton() {
		const previous = clone(view);
		for (const [screen, source] of [['Local', 'local'], ['Search', 'you-tube'], ['Radio', 'radio']]) {
			for (const available of [false, true]) {
				const title = `Evernote ${source}, available=${available}`;
				snapshot({ screen, evernote_available: available,
					details: { ...clone(defaults.DetailView), title, source,
						media_id: { source, external_id: 'fixture-item' } } });
				await until(() => document.querySelector('[aria-label=Details] h2')?.textContent === title, title);
				const note = button('[E] To Evernote');
				assert(Boolean(note) === (available && source !== 'radio'), `Evernote entrypoint preserves its source/capability gate: ${title}`);
				assert(!button('Save audio to Evernote'), 'Evernote no longer uses its old Details label');
				if (note) {
					await action('OpenEvernoteNote', () => note.click(), `Compact Evernote button opens the existing ${source} review`);
					await key('E', { Char: 'E' }, { shiftKey: true });
				}
			}
		}
		snapshot(previous);
	}
	/** A shorter display label never changes the persisted screen identity or history policy. */
	async function checkLogTab() {
		const previous = clone(view);
		const tabs = document.querySelector('[aria-label=Sources]');
		const log = await until(() => button('Log', tabs), 'Log tab from the native catalogue fixture');
		assert(!button('History', tabs), 'The playback tab shows Log rather than History');
		await action({ ShowScreen: 'History' }, () => log.click(), 'Log tab still dispatches the stable History screen identity');
		snapshot({ screen: 'History' });
		await until(() => button('Log', tabs)?.getAttribute('aria-selected') === 'true', 'Log is selected for a History snapshot');
		await key('F3', { F: 3 });
		snapshot({ help_open: true });
		await until(() => dialog()?.textContent.includes('The same map serves the terminal front-end'), 'Log navigation help');
		const shortcut = [...dialog().querySelectorAll('dt')].find((node) => node.textContent === 'F2 · F3 · F4 · F5');
		assert(shortcut?.nextElementSibling.textContent === 'offline · log · lists · stats', 'F3 help names the Log tab');
		snapshot({ screen: previous.screen, playback_history_enabled: false });
		await until(() => !button('Log', tabs) && !dialog()?.textContent.includes('F3'), 'disabled playback history hides Log and its F3 help');
		snapshot({ help_open: false, playback_history_enabled: true });
		await until(() => !dialog() && button('Log', tabs), 'reenabling playback history restores Log');
		snapshot(previous);
	}
	/** The browser forwards arrow modifiers; Rust alone decides navigation and seek distances. */
	async function checkArrowShortcuts() {
		for (const [name, shared] of [['ArrowLeft', 'Left'], ['ArrowRight', 'Right']]) {
			for (const [label, ctrl, alt] of [['plain', false, false], ['Ctrl', true, false], ['Alt', false, true]]) {
				const start = calls.length;
				const event = new KeyboardEvent('keydown', { key: name, ctrlKey: ctrl, altKey: alt,
					bubbles: true, cancelable: true });
				document.dispatchEvent(event);
				await until(() => calls.slice(start).some((call) => call.command === 'key'), `${label}+${name} IPC`);
				const forwarded = calls.slice(start).filter((call) => call.command === 'key');
				assert(forwarded.length === 1, `${label}+${name} forwards exactly one key`);
				const press = forwarded[0].args.press;
				assert(press.key === shared && press.ctrl === ctrl && press.alt === alt && press.shift === false,
					`${label}+${name} preserves its shared key and modifiers`);
				assert(event.defaultPrevented && !calls.slice(start).some((call) => call.command === 'dispatch'),
					`${label}+${name} suppresses browser defaults without duplicating Rust actions`);
			}
		}
		snapshot({ help_open: true });
		await until(() => dialog()?.textContent.includes('The same map serves the terminal front-end'), 'keyboard help');
		for (const [keys, meaning] of [
			['Left / Right', 'seek backward / forward 5 seconds'],
			['Ctrl+Left / Ctrl+Right', 'seek backward / forward 20 seconds'],
			['Alt+Left / Alt+Right', 'back / forward'],
		]) {
			const row = [...dialog().querySelectorAll('dt')].find((node) => node.textContent === keys);
			assert(row?.nextElementSibling.textContent === meaning, `Keyboard help explains ${keys}: ${meaning}`);
		}
		snapshot({ help_open: false });
		await until(() => !dialog(), 'closed keyboard help');
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
		const fullPaths = dialog().querySelector('[data-preferences-field=FullLocalPaths]');
		assert(fullPaths?.textContent.includes('Show full Local paths'), 'Preferences exposes the Local path display toggle');
		assert(fullPaths.previousElementSibling?.dataset.preferencesField === 'LocalFolderSizes', 'Full Local paths follows the Local folder sizes toggle');
		assert(button('off', fullPaths), 'Full Local paths is disabled by default');
		await action('ToggleFullLocalPaths', () => button('off', fullPaths).click(), 'Local path display uses the shared toggle action');
		snapshot({ preferences_popup: { ...preferences, selected_field: 'FullLocalPaths', show_full_local_paths: true } });
		await until(() => dialog()?.querySelector('[data-preferences-focused=true]')?.dataset.preferencesField === 'FullLocalPaths', 'Local path Preferences focus');
		assert(button('on', dialog().querySelector('[data-preferences-field=FullLocalPaths]')), 'The Local path toggle reflects the updated draft');
		await key(' ', { Char: ' ' });
		const naturalSort = dialog().querySelector('[data-preferences-field=NaturalLocalSort]');
		assert(naturalSort?.textContent.includes('Natural Local filename sorting (1, 2, 10)'), 'Preferences explains numeric filename ordering');
		assert(naturalSort.previousElementSibling?.dataset.preferencesField === 'FullLocalPaths', 'Natural Local sorting follows full Local paths');
		const naturalCheckbox = naturalSort.querySelector('input[type=checkbox]');
		assert(naturalCheckbox && !naturalCheckbox.checked, 'Natural Local sorting is disabled by default');
		await action('ToggleNaturalLocalSort', () => naturalCheckbox.click(), 'Natural Local sorting uses the shared toggle action');
		snapshot({ preferences_popup: { ...preferences, selected_field: 'NaturalLocalSort', natural_local_sort: true } });
		await until(() => dialog()?.querySelector('[data-preferences-focused=true]')?.dataset.preferencesField === 'NaturalLocalSort', 'Natural Local sorting Preferences focus');
		assert(dialog().querySelector('[data-preferences-field=NaturalLocalSort] input').checked, 'Natural Local sorting reflects the updated draft');
		for (const [name, wire] of [['ArrowDown', 'Down'], ['ArrowUp', 'Up'], [' ', { Char: ' ' }]]) {
			const before = calls.length;
			const event = new KeyboardEvent('keydown', { key: name, bubbles: true, cancelable: true });
			dialog().querySelector('[data-preferences-field=NaturalLocalSort] input').dispatchEvent(event);
			await until(() => calls.slice(before).some((call) => call.command === 'key' && JSON.stringify(call.args.press.key) === JSON.stringify(wire)), `Natural Local sorting forwards ${name}`);
			assert(event.defaultPrevented, `Natural Local sorting ${name} prevents duplicate native activation`);
			assert(!calls.slice(before).some((call) => call.command === 'dispatch'), `Natural Local sorting ${name} reaches only the shared keymap`);
		}
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
	/** Confirmation retains its captured identity if background Details are replaced. */
	async function checkUnsubscribeConfirmation() {
		const previous = { screen: view.screen, details: view.details };
		const popup = { channel_id: 'UCcaptured', channel_name: 'Saved <channel>' };
		const beforeOpen = calls.length;
		snapshot({ unsubscribe_popup: popup });
		await until(() => dialog()?.textContent.includes('Remove this channel from your local subscriptions?'), 'unsubscribe confirmation');
		assert(dialog().textContent.includes(popup.channel_name) && dialog().textContent.includes(popup.channel_id), 'Unsubscribe confirmation displays the captured channel name and identity');
		assert(!calls.slice(beforeOpen).some((call) => call.command === 'dispatch'), 'Opening unsubscribe confirmation cannot remove a channel');
		snapshot({ details: { ...details('Different channel', null), channel_id: 'UCdifferent', channel_name: 'Different channel' } });
		await until(() => document.querySelector('[aria-label=Details]')?.textContent.includes('Different channel'), 'background channel replacement');
		assert(dialog().textContent.includes(popup.channel_name), 'The pending unsubscribe target does not follow background selection');
		await action({ ConfirmUnsubscribe: { channel_id: popup.channel_id } }, () => button('Unsubscribe', dialog()).click(), 'Unsubscribe confirms the exact captured channel');
		await action('DismissUnsubscribe', () => button('Cancel', dialog()).click(), 'Cancel dismisses without removing a subscription');
		await action('DismissUnsubscribe', () => dialog().querySelector('button[aria-label=Cancel]').click(), 'The close control also cancels unsubscribe');
		await key('Escape', 'Esc');
		await key('Enter', 'Enter');
		snapshot({ unsubscribe_popup: null, ...previous });
		await until(() => !dialog(), 'closed unsubscribe confirmation');
	}

	/** Report editing and publication use mocked native commands only. */
	async function checkBugReportComposer() {
		const previous = clone(view);
		const nativeInput = document.createElement('input');
		nativeInput.value = 'Underlying private fixture value';
		document.body.append(nativeInput);
		nativeInput.focus();
		let underlyingKeys = 0;
		nativeInput.addEventListener('keydown', () => { underlyingKeys++; });
		const opened = calls.length;
		nativeInput.dispatchEvent(new KeyboardEvent('keydown', { key: 'b', ctrlKey: true, altKey: true, bubbles: true, cancelable: true }));
		const request = await until(() => calls.slice(opened).find((call) => call.command === 'frontend'), 'bug report capture command');
		assert(typeof request.args.action.OpenBugReport.screenshot === 'string'
			&& request.args.action.OpenBugReport.screenshot.includes('GUI text snapshot'), 'Ctrl+Alt+B captures a labeled text-only GUI snapshot');
		assert(!calls.slice(opened).some((call) => call.command === 'key'), 'The capture shortcut opens only once');
		assert(!request.args.action.OpenBugReport.screenshot.includes(nativeInput.value), 'Native input values are excluded from the text capture');
		const popup = { title: 'Wrong title <b>literal</b>', body: 'First line\nSecond 📮 line', selected_field: 'Title',
			title_cursor_byte: 5, body_cursor_byte: 0, body_scroll_offset: 0, follow_cursor: true,
			with_screenshot: true, screenshot_available: true, screenshot_notice: null,
			footer: 'Youta 0.fixture\nOS: Fixture OS', gh_available: true, validation_error: null,
			submission: 'Idle', animation_frame: 0 };
		snapshot({ bug_report_popup: popup });
		await until(() => dialog()?.textContent.includes('Report a bug'), 'bug report composer');
		nativeInput.focus();
		const typingStart = calls.length;
		nativeInput.dispatchEvent(new KeyboardEvent('keydown', { key: 'x', bubbles: true, cancelable: true }));
		await until(() => calls.slice(typingStart).some((call) => call.command === 'key' && call.args.press.key.Char === 'x'), 'composer typing while a native input has focus');
		assert(underlyingKeys === 0, 'Composer capture-phase keys never edit the underlying input');
		assert(dialog().textContent.includes(popup.title) && dialog().textContent.includes(popup.body), 'Title/body remain literal multiline text');
		assert(!dialog().querySelector('input, textarea, b'), 'Composer uses controller-owned fields without interpreting markup');
		assert(dialog().querySelector('[role=checkbox]').getAttribute('aria-checked') === 'true', 'ASCII screenshot defaults checked');
		assert(dialog().textContent.includes('published publicly') && dialog().textContent.includes(popup.footer), 'The composer shows privacy notice and final metadata before Submit');
		await action({ SelectBugReportField: 'Body' }, () => button('Body', dialog()).click(), 'Body selects the shared editor field');
		nativeInput.focus();
		const bodyTypingStart = calls.length;
		nativeInput.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true }));
		await until(() => calls.slice(bodyTypingStart).some((call) => call.command === 'key' && call.args.press.key === 'Enter'), 'body newline while native input has focus');
		assert(underlyingKeys === 0, 'Body editing also bypasses the underlying input');
		nativeInput.blur();
		await key('Enter', 'Enter');
		await key('ArrowLeft', 'Left');
		await action('ToggleBugReportScreenshot', () => dialog().querySelector('[role=checkbox]').click(), 'Screenshot checkbox toggles through the reducer');
		const enabledBorder = getComputedStyle(button('Submit', dialog())).borderColor;
		await action('SubmitBugReport', () => button('Submit', dialog()).click(), 'Submit sends directly without a review or confirmation step');
		snapshot({ bug_report_popup: { ...popup, submission: 'Submitting' } });
		const submit = await until(() => button('Submit', dialog())?.disabled && button('Submit', dialog()), 'disabled pending Submit');
		assert(getComputedStyle(submit).borderColor !== enabledBorder && Number(getComputedStyle(submit).opacity) < 1,
			'Pending Submit is visually muted rather than highlighted');
		assert(dialog().querySelector('[role=status]').textContent.includes('|'), 'Pending submission shows a simple ASCII spinner');
		const pendingCalls = calls.length;
		submit.click();
		assert(calls.length === pendingCalls, 'Disabled Submit cannot publish a duplicate');
		await key('Enter', 'Enter', { ctrlKey: true });
		assert(!calls.slice(pendingCalls).some((call) => call.command === 'dispatch'), 'Pending keyboard submission is delegated only to the guarded shared keymap');
		const repeatedStart = calls.length;
		document.dispatchEvent(new KeyboardEvent('keydown', { key: 'b', ctrlKey: true, altKey: true, repeat: true, bubbles: true, cancelable: true }));
		assert(calls.length === repeatedStart, 'Repeated composer shortcut cannot recapture or reopen a pending draft');
		assert(dialog().querySelector('button[aria-label=Close]').disabled, 'Pending publication cannot be dismissed');
		snapshot({ bug_report_popup: { ...popup, submission: 'Submitting', animation_frame: 1 } });
		await until(() => dialog()?.querySelector('[role=status]')?.textContent.includes('/'), 'animated ASCII spinner');
		snapshot({ bug_report_popup: { ...popup, submission: { Failed: { message: 'Fixture submission failed' } } } });
		await until(() => button('Submit', dialog())?.disabled === false, 'retryable failure');
		assert(dialog().textContent.includes('Fixture submission failed'), 'Submission failure remains inline with the authored draft');
		await action('CopyBugReport', () => button('Copy report', dialog()).click(), 'Copy retains an explicit fallback');
		const issueUrl = 'https://github.com/vitaly-zdanevich/youta/issues/123';
		snapshot({ bug_report_popup: { ...popup, submission: { Submitted: { url: issueUrl } } }, external_opener_available: true });
		await until(() => dialog()?.textContent.includes(issueUrl), 'visible submitted issue URL');
		assert(button('Submit', dialog()).disabled, 'A completed issue cannot be submitted again');
		await action('OpenBugReportResult', () => button('Open issue', dialog()).click(), 'A completed issue opens the controller-validated result');
		const issuesUrl = 'https://github.com/vitaly-zdanevich/youta/issues';
		snapshot({ bug_report_popup: { ...popup, submission: { OutcomeUnknown: { issues_url: issuesUrl } } } });
		await until(() => dialog()?.textContent.includes('outcome is unknown'), 'uncertain submission outcome');
		assert(dialog().textContent.includes(issuesUrl) && button('Submit', dialog()).disabled, 'Unknown publication shows its check URL and prevents retry duplicates');
		snapshot({ bug_report_popup: { ...popup, gh_available: false, screenshot_available: false, screenshot_notice: 'Capture omitted for privacy.' } });
		await until(() => dialog()?.textContent.includes('gh auth login'), 'GitHub CLI fallback guidance');
		assert(button('Submit', dialog()).disabled && dialog().querySelector('[role=checkbox]').disabled,
			'Missing helpers or capture disable unavailable controls while Copy remains available');
		await action('DismissBugReport', () => button('Close', dialog()).click(), 'Close dismisses the composer');
		snapshot({ bug_report_popup: null, private_note_open: true });
		await until(() => dialog()?.textContent.includes('Private note'), 'private editor before report');
		const privateStart = calls.length;
		document.dispatchEvent(new KeyboardEvent('keydown', { key: 'B', ctrlKey: true, altKey: true, bubbles: true, cancelable: true }));
		const privateRequest = await until(() => calls.slice(privateStart).find((call) => call.command === 'frontend'), 'private-context bug report');
		assert(privateRequest.args.action.OpenBugReport.screenshot === null, 'Private editor DOM is not captured');
		snapshot(previous);
		nativeInput.remove();
		await until(() => !dialog(), 'composer fixture cleanup');
	}

	async function run() {
		await until(() => document.querySelector('[title="Search archive.org"]'), 'Archive search');
		await checkBugReportComposer();
		await checkSoundCloudTab();
		const beforeTabMarkers = clone(view);
		for (const [idle, paused, label] of [[false, false, '▶ SoundCloud'], [false, true, '|| SoundCloud'], [true, false, 'SoundCloud']]) {
			snapshot({ playing_screen: 'SoundCloud', playback: { ...view.playback, idle, paused } });
			const tab = await until(() => button(label, document.querySelector('[aria-label=Sources]')), 'source playback tab marker');
			assert(tab.getAttribute('aria-selected') === 'false', 'playback marker is independent of the selected tab');
			await action({ ShowScreen: 'SoundCloud' }, () => tab.click(), `${label} preserves tab navigation`);
		}
		snapshot(beforeTabMarkers);
		await checkRadioPresentation();
		await checkYouTubeSearchControls();
		await checkEmailLinks();
		await checkLiveDuration();
		await checkLocalFullPath();
		await checkLocalTrackMetadata();
		await checkLocalActionOrder();
		await checkLocalCopy();
		await checkArchiveLocalUpload();
		await checkEvernoteButton();
		await checkLogTab();
		await checkArrowShortcuts();
		await checkPreferencesFocus();
		await checkUnsubscribeConfirmation();
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
		// A plain Playlist label keeps the existing chooser action available without an ellipsis glyph.
		snapshot({ playlist_item: { media_id: mediaId, title: 'First fixture track', in_todo: false } });
		await until(() => button('To-do'), 'playlist actions for the selected track');
		assert(Boolean(button('Playlist')), 'Playlist action uses its plain ASCII label');
		await action('OpenPlaylistPopup', () => button('Playlist').click(), 'Playlist opens the shared chooser action');
		snapshot({ playlist_item: null });
		await action('ActivateSelection', () => track.dispatchEvent(new MouseEvent('dblclick', { bubbles: true })), 'Track double click requests playback');
		snapshot({ playing_media_id: mediaId, now_playing: { media_id: mediaId, title: 'First fixture track', subtitle: 'archive.org' } });
		tick({ idle: false, paused: false, position: { secs: 1, nanos: 0 }, duration: { secs: 120, nanos: 0 } });
		await until(() => button('Pause') && !document.querySelector('[aria-label="Playback position"]').disabled, 'playing controls');
		assert(button('Pause').textContent.trim() === '||', 'Pause uses two ASCII bars without a special-font dependency');
		// Keep the speed marker portable while preserving two decimal places on playback updates.
		assert(button('Slower').nextElementSibling.textContent === '1.00x', 'Default playback speed uses an ASCII x');
		tick({ speed: 1.25 });
		await until(() => button('Slower').nextElementSibling.textContent === '1.25x', 'updated ASCII playback speed');
		assert(button('Slower').nextElementSibling.textContent === '1.25x', 'Updated playback speed retains two decimal places and an ASCII x');
		await action('TogglePause', () => button('Pause').click(), 'ASCII pause control retains the shared playback action');
		checks.push('Playing snapshot enables transport and seek controls');
		tick({ paused: true, position: { secs: 120, nanos: 0 } });
		await until(() => button('Play') && !button('Play').disabled, 'retained EOF pause');
		assert(button('Play').textContent.trim() === '▶', 'Replacing the pause marker does not change the play control');
		await action('TogglePause', () => button('Play').click(), 'Play control retains the shared playback action');
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

		// A linked licence belongs in the link list, without a duplicate facts row.
		snapshot({ details: { ...details('Licensed archive item'), archive_file_counts: { total: 42, playable: 12 }, license: 'CC BY-NC-ND 3.0', links: [{
			prefix: 'License: ', label: 'CC BY-NC-ND 3.0', url: 'https://creativecommons.org/licenses/by-nc-nd/3.0/',
			presentation: 'LabelAndUrl', description_range: null, internal_target: null, wikidata_item_id: null,
		}] } });
		await until(() => button('CC BY-NC-ND 3.0'), 'linked archive licence');
		assert(document.querySelector('[aria-label=Details]').textContent.includes('42 total · 12 playable'), 'Archive displays file counts from loaded item metadata');
		assert([...document.querySelectorAll('[aria-label=Details] dt')].every((node) => node.textContent !== 'License'), 'Archive keeps its linked licence without a duplicate fact');
		await action({ ActivateDetailLink: 0 }, () => button('CC BY-NC-ND 3.0').click(), 'Archive licence remains clickable');
		snapshot({ details: { ...view.details, archive_file_counts: { total: 0, playable: 0 } } });
		await until(() => document.querySelector('[aria-label=Details]').textContent.includes('0 total · 0 playable'), 'known empty Archive item displays zero counts');
		snapshot({ details: { ...view.details, archive_file_counts: null } });
		await until(() => ![...document.querySelectorAll('[aria-label=Details] dt')].some((node) => node.textContent === 'Files'), 'unknown Archive counts do not invent zero');

		// A loading waveform shows plain text until its owner has seekable peaks.
		snapshot({ waveform_visible: true, waveform: { Loading: {
			media_id: { source: 'local', external_id: '/fixture/audio.flac' },
		} } });
		const loadingWaveform = await until(() => [...document.querySelectorAll('p')]
			.find((node) => node.textContent.startsWith('Generating local waveform')), 'local waveform loading notice');
		assert(loadingWaveform.textContent === 'Generating local waveform', 'Waveform loading notice uses plain ASCII text');
		assert(!document.querySelector('[aria-label="Waveform"]'), 'Loading waveform does not expose a seekable canvas');
		snapshot({ waveform_visible: false, waveform: 'Unavailable' });

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
		// Hashtags share indexed inline navigation without parsing provider text
		// again in JavaScript or taking over nearby timestamps and video links.
		const hashtagText = '📍 #Minsk, (#Беларусь).\n1:23 Visit https://youtu.be/fixture1234\nNot links: C# https://example.test/#fragment <b>plain</b>';
		const hashtagMedia = { source: 'you-tube', external_id: 'fixture1234' };
		const hashtagDetails = { ...details('YouTube hashtag fixture', hashtagMedia), source: 'YouTube', description: hashtagText,
			links: [
				{ ...inlineLink('#Minsk', { YouTubeHashtag: 'Minsk' }, hashtagText), url: 'https://www.youtube.com/hashtag/Minsk' },
				{ ...inlineLink('#Беларусь', { YouTubeHashtag: 'Беларусь' }, hashtagText), url: 'https://www.youtube.com/hashtag/%D0%91%D0%B5%D0%BB%D0%B0%D1%80%D1%83%D1%81%D1%8C' },
			],
			timecodes: [{ ...range(hashtagText, '1:23'), seconds: 83, is_chapter: true }],
			video_links: [{ ...range(hashtagText, 'https://youtu.be/fixture1234'), video_id: 'fixture1234', start_seconds: null }],
			search_highlights: [{ field: 'Description', ranges: [range(hashtagText, 'Беларусь')] }],
		};
		snapshot({ screen: 'Search', details: hashtagDetails, external_opener_available: false });
		const hashtag = await until(() => button('#Беларусь'), 'Unicode YouTube hashtag');
		const hashtagDescription = document.querySelector('[data-description]');
		assert(hashtagDescription.textContent === hashtagText.replace('https://youtu.be/fixture1234', 'https://youtu.be/fixture1234↪'),
			'Clickable hashtags preserve Unicode, punctuation, literal markup and description text');
		assert(hashtag.querySelector('mark')?.textContent === 'Беларусь', 'Unicode hashtag retains search highlighting');
		assert(hashtagDescription.querySelectorAll('[data-detail-link]').length === 2,
			'Only controller-provided hashtag spans become inline links');
		assert(!document.querySelector('[aria-label=Details] ul li'), 'YouTube hashtags do not duplicate in the fixed link rail');
		assert(!hashtagDescription.querySelector('b'), 'Hashtags never turn description markup into HTML');
		await action({ SelectDetailLink: 1 }, () => hashtag.focus(), 'Focusing a Unicode hashtag selects its global link index');
		await action({ ActivateDetailLink: 1 }, () => hashtag.click(), 'YouTube hashtag search does not require an external opener');
		const beforeHashtagEnter = calls.length;
		const hashtagEnter = new KeyboardEvent('keydown', { key: 'Enter', bubbles: true, cancelable: true });
		hashtag.dispatchEvent(hashtagEnter);
		await until(() => calls.slice(beforeHashtagEnter).some((call) => call.command === 'key' && call.args.press.key === 'Enter'),
			'focused hashtag shared Enter');
		assert(hashtagEnter.defaultPrevented && !calls.slice(beforeHashtagEnter).some((call) => call.command === 'dispatch'),
			'Focused hashtag Enter reaches the shared selection keymap without a duplicate browser click');
		await action({ ActivateTimecode: { media_id: hashtagMedia, seconds: 83 } }, () => button('1:23', hashtagDescription).click(),
			'Timestamp beside YouTube hashtags keeps its exact seek target');
		await action({ ActivateDescriptionVideo: { video_id: 'fixture1234', start_seconds: null } },
			() => hashtagDescription.querySelector('[title="Open this video in Youta"]').click(),
			'Video link beside YouTube hashtags retains internal video navigation');
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
		snapshot({ screen: 'ArchiveOrg', details: linkedDetails, external_opener_available: false });
		const creator = await until(() => button('Vitaly Zdanevich', document.querySelector('[data-description]')), 'inline creator link');
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
			commons_upload_available: true, archive_upload_supported: true, archive_upload_available: true,
			s3_upload_supported: true, s3_upload_available: true });
		const commons = await until(() => button('[U] To Commons'), 'compact Commons action with uppercase shortcut');
		assert(!button('Upload to Commons'), 'Commons no longer uses its old button label');
		await action('OpenCommonsUpload', () => commons.click(), 'To Commons retains its existing review action');
		// Forward uppercase U unchanged; the Rust keymap, not this fixture, selects the action.
		await key('U', { Char: 'U' }, { shiftKey: true });
		snapshot({ commons_upload_available: false });
		await until(() => !button('[U] To Commons'), 'unavailable Commons action remains hidden');
		await until(() => button('[I] To archive.org'), 'Archive upload action with uppercase shortcut');
		assert(!button('Upload to archive.org'), 'Archive no longer uses its old Details button label');
		await action('OpenArchiveUpload', () => button('[I] To archive.org').click(), 'Archive Details action opens publication review');
		// Forward uppercase I unchanged; review and publication remain shared-reducer actions.
		await key('I', { Char: 'I' }, { shiftKey: true });
		const archive = { ...clone(defaults.ArchiveUploadPopupView), generation: 7, selected_field: 'Description', phase: 'Review', video_available: true,
			draft: { source: 'YouTube', identifier: 'fixture-review', title: 'Fixture title', description: 'Full description\nAnother line', creator: 'Fixture creator',
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
		await action('OpenS3Credentials', () => button('Session keys...', dialog()).click(), 'S3 review offers explicit session-key replacement');
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
		await until(() => !dialog() && !button('Upload to S3') && !button('[I] To archive.org'), 'feature-trimmed Details');
		checks.push('Feature-trimmed snapshots expose neither upload action');
		assert(failures.length === 0, 'The full browser journey reports no frontend runtime failures');
	}
	void run().then(() => ({ ok: true, checks }), (error) => ({ ok: false, error: String(error), checks,
		body: document.body?.innerText.slice(-12000), calls: calls.slice(-5) }))
		.then((report) => fetch('/__report', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(report) }));
})();
