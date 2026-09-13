/**
 * Drives the actual built React app inside an isolated headless browser.
 * Native commands are recorded, never executed. Incoming controller snapshots
 * are explicit fixtures: this checks browser rendering and IPC, not Rust state
 * transitions, media playback, remote provider availability or publication.
 */
(() => {
	const defaults = window.__YOUTA_FIXTURES__;
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
	async function run() {
		await until(() => document.querySelector('[title="Search archive.org"]'), 'Archive search');
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

		assert(failures.length === 0, 'The full browser journey reports no frontend runtime failures');
	}
	void run().then(() => ({ ok: true, checks }), (error) => ({ ok: false, error: String(error), checks,
		body: document.body?.innerText.slice(-12000), calls: calls.slice(-5) }))
		.then((report) => fetch('/__report', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(report) }));
})();
