import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const React = require('react');
const ts = require('typescript');
const source = await readFile(new URL('./components/popups.tsx', import.meta.url), 'utf8');
const contract = await readFile(new URL('./contract.ts', import.meta.url), 'utf8');
const app = await readFile(new URL('./App.tsx', import.meta.url), 'utf8');
const actions = [];

/** Execute the real JSX with a deterministic bridge and a portal-free popup shell. */
const compiled = ts.transpileModule(source, {
	compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
}).outputText;
const module = { exports: {} };
const mockRequire = (name) => {
	if (name === '../ipc') return { dispatch: (...args) => actions.push(args) };
	if (name === './Popup') return {
		Popup: ({ children, footer, onDismiss }) => React.createElement('section', null,
			React.createElement('button', { onClick: onDismiss }, 'Dismiss'), footer, children),
		PopupButton: (props) => React.createElement('button', props),
		PopupError: () => null,
	};
	if (name === './ScrollingText') return { ScrollingText: () => null };
	if (name === '../format') return { humanBytes: (value) => String(value) };
	return require(name);
};
new Function('require', 'module', 'exports', compiled)(mockRequire, module, module.exports);

/** Expand function components into host elements without mounting a browser. */
function nodes(element) {
	if (!React.isValidElement(element)) return [];
	if (typeof element.type === 'function') return nodes(element.type(element.props));
	return [element, ...React.Children.toArray(element.props.children).flatMap(nodes)];
}

function text(element) {
	if (typeof element === 'string' || typeof element === 'number') return String(element);
	if (!React.isValidElement(element)) return '';
	if (typeof element.type === 'function') return text(element.type(element.props));
	return React.Children.toArray(element.props.children).map(text).join('');
}

test('download chooser renders reducer labels and confirms exact option indices', () => {
	assert.equal(typeof module.exports.DownloadChoicePopup, 'function');
	actions.length = 0;
	const popup = {
		generation: 7, title: 'Fixture <audio>', explanation: 'Choose an existing file.',
		options: ['Original <FLAC>', 'Archive MP3'], selected: 1,
	};
	const tree = module.exports.DownloadChoicePopup({ popup });
	const buttons = nodes(tree).filter((node) => node.type === 'button');
	for (const [index, label] of popup.options.entries()) {
		const button = buttons.find((node) => text(node) === label);
		assert.ok(button, label);
		assert.equal(button.props.emphasis, index === popup.selected);
		button.props.onClick();
		assert.deepEqual(actions.pop(), [{ SelectDownloadChoice: { generation: popup.generation, index } }]);
	}
	buttons.find((node) => text(node) === 'Download').props.onClick();
	assert.deepEqual(actions.pop(), [{ ConfirmDownloadChoice: popup.generation }]);
	buttons.find((node) => text(node) === 'Cancel').props.onClick();
	assert.deepEqual(actions.pop(), ['DismissDownloadChoice']);
	assert.ok(nodes(tree).every((node) => !node.props.dangerouslySetInnerHTML));
});

test('Archive playback chooser selects controller-owned formats and confirms only an explicit Play', () => {
	assert.equal(typeof module.exports.ArchivePlaybackChoicePopup, 'function');
	actions.length = 0;
	const popup = {
		generation: 23, title: 'Fixture <video>', explanation: 'Choose a playable file.',
		options: ['Original MPEG4 · 42 MiB', 'Audio only: Ogg Vorbis · 4 MiB'], selected: 1,
	};
	const tree = module.exports.ArchivePlaybackChoicePopup({ popup });
	const buttons = nodes(tree).filter((node) => node.type === 'button');
	for (const [index, label] of popup.options.entries()) {
		const button = buttons.find((node) => text(node) === label);
		assert.ok(button, label);
		assert.equal(button.props.emphasis, index === popup.selected);
		button.props.onClick();
		assert.deepEqual(actions.pop(), [{ SelectArchivePlaybackChoice: { generation: popup.generation, index } }]);
		assert.equal(actions.length, 0, 'Selecting a format must not start playback');
	}
	const play = buttons.find((node) => text(node) === 'Play');
	assert.equal(play.props.disabled, false);
	play.props.onClick();
	assert.deepEqual(actions.pop(), [{ ConfirmArchivePlaybackChoice: popup.generation }]);
	for (const label of ['Cancel', 'Dismiss']) {
		buttons.find((node) => text(node) === label).props.onClick();
		assert.deepEqual(actions.pop(), ['DismissArchivePlaybackChoice']);
	}
	assert.ok(nodes(tree).every((node) => !node.props.dangerouslySetInnerHTML));
	for (const invalid of [{ options: [], selected: 0 }, { selected: -1 }, { selected: 2 }]) {
		const rendered = nodes(module.exports.ArchivePlaybackChoicePopup({ popup: { ...popup, ...invalid } }));
		assert.equal(rendered.find((node) => node.type === 'button' && text(node) === 'Play').props.disabled, true);
	}
});

test('Archive playback preference is independent of downloader availability and uses exact saved modes', () => {
	for (const supported of [false, true]) {
		for (const [mode, label] of [['ask-each-time', 'Ask each time'], ['original-file', 'Original file'], ['audio-only', 'Audio only']]) {
			const popup = {
				auto_download_supported: false, archive_playback_supported: supported,
				archive_playback_preference: mode, subscriptions_layout: 'split',
				video_summary_supported: false, sponsorblock_supported: false, nyan_cat_supported: false,
			};
			const rendered = nodes(module.exports.PreferencesPopup({ popup, archiveSupported: true }));
			const button = rendered.find((node) => node.type === 'button' && text(node) === label);
			assert.equal(Boolean(button), supported, `${mode}: Archive capability, not yt-dlp, controls playback preferences`);
			if (button) {
				button.props.onClick();
				assert.deepEqual(actions.pop(), ['CycleArchivePlaybackPreference']);
				assert.ok(rendered.some((node) => text(node).includes('archive.org playback')));
			}
		}
	}
});

test('window mounts the generation-bound Archive playback chooser and declares its exact contract', () => {
	assert.match(contract, /export interface ArchivePlaybackChoicePopupView/);
	assert.match(contract, /archive_playback_choice_popup: ArchivePlaybackChoicePopupView \| null/);
	assert.match(contract, /archive_playback_preference: ArchivePlaybackPreference/);
	assert.match(contract, /archive_playback_supported: boolean/);
	assert.match(app, /<ArchivePlaybackChoicePopup popup=\{view\.archive_playback_choice_popup\}/);
});

test('persistent download queue selects, retries and cancels exact stable entries', () => {
	assert.equal(typeof module.exports.DownloadQueuePopup, 'function');
	actions.length = 0;
	const popup = { entries: [
		{ id: 7, title: 'Failed <track>', state: 'Failed' },
		{ id: 42, title: 'Waiting track', state: 'Queued' },
	], selected: 0 };
	const tree = module.exports.DownloadQueuePopup({ popup });
	const buttons = nodes(tree).filter((node) => node.type === 'button');
	buttons.find((node) => text(node).includes('Waiting track')).props.onClick();
	assert.deepEqual(actions.pop(), [{ SelectDownloadQueueEntry: 42 }]);
	buttons.find((node) => text(node) === 'Retry').props.onClick();
	assert.deepEqual(actions.pop(), [{ RetryQueuedDownload: 7 }]);
	buttons.find((node) => text(node) === 'Cancel download').props.onClick();
	assert.deepEqual(actions.pop(), [{ CancelQueuedDownload: 7 }]);
	buttons.find((node) => text(node) === 'Close').props.onClick();
	assert.deepEqual(actions.pop(), ['DismissDownloadQueue']);
	assert.ok(nodes(tree).every((node) => !node.props.dangerouslySetInnerHTML));
	const empty = nodes(module.exports.DownloadQueuePopup({ popup: { entries: [], selected: 0 } }));
	assert.equal(empty.some((node) => node.type === 'button' && text(node) === 'Retry'), false);
});

/** Exercise rendered row semantics with deterministic virtual rows and inert effects. */
test('normal and subscription rows distinguish marks from downloaded files and dispatch Ctrl-click', async () => {
	const load = async (file) => {
		const code = ts.transpileModule(await readFile(new URL(file, import.meta.url), 'utf8'), {
			compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
		}).outputText;
		const loaded = { exports: {} };
		const rowRequire = (name) => {
			if (name === 'react') return { ...React, useEffect: () => {}, useRef: (current) => ({ current }) };
			if (name === '@tanstack/react-virtual') return { useVirtualizer: ({ count }) => ({
				getTotalSize: () => count * 46,
				getVirtualItems: () => Array.from({ length: count }, (_, index) => ({ index, key: index, size: 46, start: index * 46 })),
			}) };
			if (name === './Artwork') return { Artwork: () => null };
			if (name === '../subscriptionPageRows') return { SUBSCRIPTION_ROW_HEIGHT: 46 };
			return mockRequire(name);
		};
		new Function('require', 'module', 'exports', code)(rowRequire, loaded, loaded.exports);
		return loaded.exports;
	};
	const { RowList } = await load('./components/RowList.tsx');
	const { Subscriptions } = await load('./components/Subscriptions.tsx');
	for (const marked of [false, true]) {
		for (const downloaded of [false, true]) {
			const row = { title: 'Fixture track', media_id: null, download_marked: marked, downloaded };
			const trees = [
				RowList({ rows: [row], selected: 0, playing: null }),
				Subscriptions({ subscriptions: {
					layout: 'drill-down', route: 'Items', focus: 'Items', items: [row], selected_item: 0,
					source_kind: 'rss', source_title: 'Fixture feed', source_generation: 1,
				}, playing: null, details: null }),
			];
			for (const tree of trees) {
				const rendered = nodes(tree);
				assert.equal(rendered.some((node) => node.props['aria-label'] === 'Marked for download'), marked);
				assert.equal(rendered.some((node) => node.props['aria-label'] === 'Downloaded'), downloaded);
				const button = rendered.find((node) => node.type === 'button' && text(node).includes('Fixture track'));
				actions.length = 0;
				button.props.onClick({ ctrlKey: true });
				assert.deepEqual(actions.at(-1), [{ ToggleDownloadMarkAt: 0 }]);
				assert.equal(nodes(button).filter((node) => node.type === 'button').length, 1);
			}
		}
	}
});

test('download preferences show typed values and gate unsupported source capabilities', () => {
	for (const [enabled, archiveSupported] of [[false, false], [false, true], [true, false], [true, true]]) {
		actions.length = 0;
		const popup = {
			auto_download_supported: enabled, download_mode: 'ask-each-time',
			archive_download_preference: 'original-file', subscriptions_layout: 'split',
			video_summary_supported: false, sponsorblock_supported: false,
			nyan_cat_supported: false,
		};
		const tree = module.exports.PreferencesPopup({ popup, archiveSupported });
		const buttons = nodes(tree).filter((node) => node.type === 'button');
		const mode = buttons.find((node) => text(node) === 'Ask each time');
		const archive = buttons.find((node) => text(node) === 'Original file');
		assert.equal(Boolean(mode), enabled);
		assert.equal(Boolean(archive), enabled && archiveSupported);
		if (enabled) {
			mode.props.onClick();
			assert.deepEqual(actions.pop(), ['CycleDownloadModePreference']);
		}
		if (enabled && archiveSupported) {
			archive.props.onClick();
			assert.deepEqual(actions.pop(), ['CycleArchiveDownloadPreference']);
		}
	}
});

test('preferences preserve shared navigation order and distinguish focus from activation', () => {
	const popup = {
		selected_field: 'PlaybackHistory', subscriptions_layout: 'drill-down',
		auto_download_supported: true, download_mode: 'ask-each-time',
		archive_download_preference: 'original-file', archive_playback_supported: true,
		archive_playback_preference: 'audio-only', video_summary_supported: true,
		sponsorblock_supported: true, nyan_cat_supported: true,
		youtube_provider_settings_supported: true,
	};
	const rendered = nodes(module.exports.PreferencesPopup({ popup, archiveSupported: true }));
	const controls = rendered.filter((node) => node.props['data-preferences-field']);
	assert.deepEqual(controls.map((node) => node.props['data-preferences-field']), [
		'SubscriptionsLayout', 'PlaybackHistory', 'AdvertisementChapters', 'SponsorBlock',
		'YouTubePrewarm', 'NyanCat', 'HourlyDownloads', 'CheckDownloads', 'DownloadMode',
		'ArchiveDownload', 'ArchivePlayback', 'YouTubeThumbnails', 'LocalFolderSizes',
		'TtyImages', 'BandcampAudio', 'VideoSummaries', 'YouTubeProvider',
	]);
	assert.equal(controls.filter((node) => node.props['data-preferences-focused'] === 'true').length, 1);
	actions.length = 0;
	controls.find((node) => node.props['data-preferences-field'] === 'PlaybackHistory').props.onFocusCapture();
	assert.equal(actions.length, 0, 'existing focus is not dispatched again');
	controls.find((node) => node.props['data-preferences-field'] === 'LocalFolderSizes').props.onFocusCapture();
	assert.deepEqual(actions, [[{ SelectPreferencesField: 'LocalFolderSizes' }]], 'focus alone cannot mutate a draft');
	const checkbox = rendered.find((node) => node.type === 'input');
	assert.equal(checkbox.props['data-youta-preferences'], 'true', 'checkbox keys reach the shared preferences keymap');
});

test('window mounts the shared download chooser and declares the exact snapshot contract', () => {
	assert.match(contract, /export interface DownloadChoicePopupView/);
	assert.match(contract, /download_choice_popup: DownloadChoicePopupView \| null/);
	assert.match(contract, /download_mode: DownloadMode/);
	assert.match(contract, /archive_download_preference: ArchiveDownloadPreference/);
	assert.match(app, /view\.download_choice_popup/);
	assert.match(app, /<DownloadChoicePopup popup=\{view\.download_choice_popup\}/);
	assert.match(app, /archiveSupported=\{sources\.some\(\(source\) => source\.id === 'ArchiveOrg'\)\}/);
});
