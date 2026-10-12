/** Exercise the actual list and Details JSX with provider data and inert browser hooks. */
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const React = require('react');
const ts = require('typescript');
const modules = new Map();

/** Keep rendering real components while replacing only effects, viewport measurement and IPC. */
function component(name) {
	if (modules.has(name)) return modules.get(name);
	const source = readFileSync(new URL(`./components/${name}.tsx`, import.meta.url), 'utf8');
	const compiled = ts.transpileModule(source, {
		compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
	}).outputText;
	const module = { exports: {} };
	const mockRequire = (path) => {
		if (path === 'react') return { ...React, useRef: (current) => ({ current }),
			useState: (value) => [value, () => {}], useEffect: () => {} };
		if (path === '@tanstack/react-virtual') return {
			useVirtualizer: ({ count, estimateSize }) => ({
				getTotalSize: () => count * estimateSize(),
				getVirtualItems: () => Array.from({ length: count }, (_, index) => ({
					index, key: index, start: index * estimateSize(), size: estimateSize(),
				})),
			}),
		};
		if (path === '../ipc') return { dispatch: () => {} };
		if (path === '../format') return require('./format.ts');
		if (path === '../subscriptionPageRows') return { SUBSCRIPTION_ROW_HEIGHT: 46 };
		if (path === '../searchHighlights') return { highlightRanges: () => [] };
		if (path === './Artwork') return { Artwork: () => null };
		if (path === './Description') return { Description: () => null, WikidataSpoiler: () => null };
		if (path === './SearchHighlight') return { SearchHighlight: ({ text }) => text };
		if (path === './CustomCommands') return { CustomCommandButtons: () => null };
		if (path.startsWith('./')) return component(path.slice(2));
		return require(path);
	};
	new Function('require', 'module', 'exports', compiled)(mockRequire, module, module.exports);
	modules.set(name, module.exports);
	return module.exports;
}

/** Expand function children so assertions inspect host elements and their displayed text. */
function nodes(element) {
	if (!React.isValidElement(element)) return [];
	if (typeof element.type === 'function') return nodes(element.type(element.props));
	return [element, ...React.Children.toArray(element.props.children).flatMap(nodes)];
}

/** Provider strings remain text children throughout the rendered tree. */
function text(element) {
	if (typeof element === 'string' || typeof element === 'number') return String(element);
	if (!React.isValidElement(element)) return '';
	if (typeof element.type === 'function') return text(element.type(element.props));
	return React.Children.toArray(element.props.children).map(text).join('');
}

const redNodes = (tree) => nodes(tree).filter((node) => node.props.className?.split(/\s+/).includes('text-red-400'));

/** Render both list types with the same controller-owned playback and broadcast flags. */
function listTree(pane, subtitle, live, { row: overrides = {}, selected = 0, playing = false } = {}) {
	const row = { title: 'LIVE', subtitle, live, source: 'YouTube',
		media_id: { source: 'you-tube', external_id: 'fixture-live' }, watched_percent: 0,
		playback_started: false, ...overrides };
	const rows = [row, { title: 'Other item', subtitle: '', media_id: null }];
	const playingId = playing ? row.media_id : null;
	if (pane === 'main') return component('RowList').RowList({ rows, selected, playing: playingId });
	return component('Subscriptions').Subscriptions({ playing: playingId, details: null, subscriptions: {
		layout: 'drill-down', route: 'Items', focus: 'Items', sources: [], items: rows, selected_item: selected,
		source_kind: 'you-tube', source_title: '', source_created: '', source_subscriber_count: null,
	} });
}

test('main and subscription metadata is brighter only for unwatched YouTube videos', () => {
	const subtitle = 'Fixture channel · 81,100 subscribers · 2026 October 12 · 8:43';
	for (const pane of ['main', 'subscriptions']) {
		for (const [label, row, brighter] of [
			['unwatched', {}, true],
			['unwatched Short', { vertical: true }, true],
			['unwatched former broadcast', { was_live: true }, true],
			['accepted zero-percent start', { playback_started: true }, false],
			['partly watched', { playback_started: true, watched_percent: 42 }, false],
			['watched', { playback_started: true, watched_percent: 100 }, false],
			['progress without started flag', { watched_percent: 42 }, false],
			['YouTube Music', { source: 'YouTube Music' }, false],
			['YouTube channel', { source: 'YouTube channel' }, false],
			['restored generic source', { source: 'youtube' }, false],
			['missing identity', { media_id: null }, false],
			['different provider', { media_id: { source: 'rss', external_id: 'fixture' } }, false],
		]) {
			const tree = listTree(pane, subtitle, false, { row, selected: 1 });
			const metadata = nodes(tree).find((node) => node.type === 'span' && text(node) === subtitle);
			assert.ok(metadata, `${pane}: ${label}: complete metadata stays visible`);
			assert.equal(metadata.props.className.split(/\s+/).includes('text-ink-dim'), brighter, `${pane}: ${label}`);
			assert.equal(metadata.props.className.split(/\s+/).includes('text-ink-faint'), !brighter, `${pane}: ${label}`);
		}
	}
});

test('metadata brightness preserves selected and playing row indicators', () => {
	const subtitle = 'Fixture channel · 8:43';
	for (const pane of ['main', 'subscriptions']) {
		for (const selected of [0, 1]) {
			for (const playing of [false, true]) {
				for (const playback_started of [false, true]) {
					const tree = listTree(pane, subtitle, false, { selected, playing, row: { playback_started } });
					const row = nodes(tree).find((node) => node.type === 'button' && node.props['aria-current'] !== undefined);
					const title = nodes(row).find((node) => node.type === 'span' && text(node) === 'LIVE');
					const metadata = nodes(row).find((node) => node.type === 'span' && text(node) === subtitle);
					const context = `${pane}: selected=${selected === 0}, playing=${playing}, started=${playback_started}`;
					assert.equal(row.props['aria-current'], selected === 0, context);
					assert.equal(row.props.className.split(/\s+/).includes('bg-raised'), selected === 0, context);
					assert.equal(title.props.className.split(/\s+/).includes('text-accent'), selected === 0, context);
					assert.equal(text(row).includes('▶'), playing, context);
					assert.equal(metadata.props.className.split(/\s+/).includes('text-ink-dim'), !playback_started, context);
				}
			}
		}
	}
});

test('main and subscription rows color only the canonical live duration field', () => {
	for (const pane of ['main', 'subscriptions']) {
		for (const subtitle of ['LIVE', 'Channel · LIVE', 'LIVE · LIVE']) {
			const tree = listTree(pane, subtitle, true);
			assert.deepEqual(redNodes(tree).map(text), ['LIVE'], `${pane}: ${subtitle}`);
			assert.ok(text(tree).includes(`LIVE${subtitle}`), 'title and complete subtitle stay visible');
		}
	}
});

test('LIVE in a title or channel never becomes an inferred broadcast indicator', () => {
	for (const pane of ['main', 'subscriptions']) {
		for (const [subtitle, live] of [
			['LIVE', false], ['Channel · LIVE', false], ['LIVE · 0:00', false],
			['ChannelLIVE', true], ['LIVE show', true], ['Channel · 0:00', false], ['Channel · 3:21', false],
		]) {
			const tree = listTree(pane, subtitle, live);
			assert.deepEqual(redNodes(tree), [], `${pane}: ${subtitle}, live=${live}`);
			assert.ok(text(tree).includes(subtitle));
		}
	}
});

test('Details omits the redundant live length while preserving finite length and counts', () => {
	for (const [live, length, expected] of [[true, 'LIVE', []], [false, 'LIVE', []],
		[false, '0:00', []], [false, '3:21', []], [true, '3:21', []]]) {
		const tree = component('Details').Details({ kind: 'Video', view: {
			screen: 'Search', playlist_item: null, details: {
				live, length, title: 'LIVE', channel_name: 'LIVE', source: 'LIVE', likes: '7',
				views: '12', comments: '4',
				channel_id: '', media_id: { source: 'you-tube', external_id: 'fixture-live' },
				playlist_names: [], links: [], dearrow_title: null,
			},
		} });
		assert.deepEqual(redNodes(tree).map(text), expected, `length=${length}, live=${live}`);
		const lengthLabel = nodes(tree).find((node) => node.type === 'dt' && text(node) === 'Length');
		assert.equal(Boolean(lengthLabel), !live, `length=${length}, live=${live}`);
		for (const value of ['12', '7', '4']) {
			assert.ok(nodes(tree).some((node) => node.type === 'dd' && text(node) === value));
		}
	}
});

test('live rows never present a percent-complete value or progress bar', () => {
	for (const live of [true, false]) {
		for (const pane of ['main', 'subscriptions']) {
			const tree = listTree(pane, live ? 'Creator · LIVE' : 'Creator · 4:05', live, {
				row: { watched_percent: 38, playback_started: true },
			});
			const percentage = nodes(tree).find((node) => node.props.title === '38% played');
			const bar = nodes(tree).find((node) => node.type === 'i' && node.props.style?.width === '38%');
			assert.equal(Boolean(percentage), !live && pane === 'main');
			assert.equal(Boolean(bar), !live && pane === 'main');
		}
	}
});

test('LIVE footer prefers stable elapsed time over synthetic backend timestamps', () => {
	const tree = component('Player').Player({
		playback: { idle: false, live: true, position: 8641222,
			duration: 8641235, live_seekable_range: null, paused: false,
			buffering: false, buffered_ranges: [], speed: 1, volume: 50, title: null, chapter: null },
		liveElapsedSeconds: 23,
		chapters: [], track: { media_id: { source: 'you-tube', external_id: 'fixture-live' }, title: 'Live', subtitle: '' },
	});
	assert.ok(text(tree).includes('LIVE 0:23'));
	assert.ok(!text(tree).includes('2400:20'));
});

test('the player shows red LIVE with current playback time while preserving radio and seeking', () => {
	for (const [source, live, buffered, position, labels] of [
		['you-tube', true, false, 0, ['LIVE 0:00', '']],
		['you-tube', true, false, 23, ['LIVE 0:23', '']],
		['you-tube', true, true, 120, ['LIVE 2:00', '']],
		['you-tube', true, true, 240, ['LIVE 4:00', '']],
		['you-tube', false, false, 0, ['0:00', '--:--']],
		['you-tube', false, true, 0, ['0:00', '3:00']],
		['you-tube', false, true, 120, ['2:00', '3:00']],
		['radio', true, false, 0, ['radio', '']],
		['radio', true, true, 120, ['radio', '']],
	]) {
		const duration = buffered ? { secs: 180, nanos: 0 } : null;
		const tree = component('Player').Player({
			playback: { idle: false, paused: false, live, position: { secs: position, nanos: 0 }, duration,
				live_seekable_range: buffered ? { start: { secs: 0, nanos: 0 }, end: duration } : null,
				buffered_ranges: [], speed: 1, volume: 50, title: 'LIVE', chapter: null },
			chapters: [], track: { media_id: { source, external_id: 'fixture-live' }, title: 'LIVE', subtitle: '' },
			captionLine: null, radioNowPlaying: null, output: null,
		});
		const status = nodes(tree).find((node) => node.props.className?.includes('justify-between'));
		assert.deepEqual(React.Children.toArray(status.props.children).filter((node) => node.type === 'span').map(text), labels,
			`${source}: live=${live}, buffered=${buffered}`);
		assert.deepEqual(redNodes(tree).map(text), live && source !== 'radio' ? ['LIVE'] : []);
		const title = nodes(tree).find((node) => node.type === 'button' && node.props.title === 'Show the playing item');
		assert.equal(text(title), 'LIVE', 'the playing title stays separate from the indicator');
		assert.equal(nodes(tree).find((node) => node.props['aria-label'] === 'Playback position').props.disabled,
			!buffered, 'label changes preserve the existing seekability rule');
		for (const label of ['Back 5 seconds', 'Forward 5 seconds']) {
			assert.equal(nodes(tree).find((node) => node.props['aria-label'] === label).props.disabled,
				!buffered, `${label} still uses the available seekable cache`);
		}
	}
});
