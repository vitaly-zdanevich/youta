import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createRequire } from 'node:module';
import test from 'node:test';

const source = await readFile(
	new URL('./components/Subscriptions.tsx', import.meta.url),
	'utf8',
);

test('mouse focus cannot snap a subscription pane back to its old selection', () => {
	assert.match(source, /\[selected, selectedIdentity, virtualizer\]/);
	assert.doesNotMatch(source, /\[focused, selected, selectedIdentity, virtualizer\]/);
	assert.match(source, /<section[\s\S]*onPointerDown=\{focusPane\}/);
	assert.match(source, /<section[\s\S]*onFocusCapture=\{focusPane\}/);
});

test('subscription scrolling coalesces viewport reports but permits an end retry', () => {
	assert.match(source, /lastReportedViewportEnd/);
	assert.match(source, /finalIndex === lastReportedViewportEnd\.current/);
	assert.match(source, /reportViewport\(\);\n\t}, \[rows\.length, virtualizer\]\);/);
	assert.match(source, /onChange: \(instance\) =>/);
	assert.match(source, /event\.deltaY > 0/);
	assert.match(source, /reportViewport\(true\)/);
});

test('same-title same-length source replacement reports its new viewport', () => {
	assert.match(source, /sourceOwner: number;/);
	assert.match(source, /sourceOwner=\{subscriptions\.source_generation\}/);
	assert.doesNotMatch(source, /selectedSource\?\.subtitle/);
	assert.match(
		source,
		/lastReportedViewportEnd\.current = null;[\s\S]*virtualizer\.scrollToIndex\(selected, \{ align: 'start' \}\);/,
	);
	assert.doesNotMatch(source, /\}, \[heading\]\);/);
});

test('inactive item panes neither prefetch nor preserve another source scroll offset', () => {
	assert.match(source, /pane !== 'Items'\s*\|\|\s*!focused/);
	assert.match(source, /if \(focused && !viewportFocusActive\.current\) \{\s*reportViewport\(true\);\s*}/);
	assert.match(source, /viewportFocusActive\.current = focused/);
	assert.match(source, /pane === 'Items'[\s\S]*scrollToIndex\(selected, \{ align: 'start' \}\)/);
});

test('explicit refresh claims Items only after its page-one command', () => {
	assert.match(source, /closest\('\[data-pane-focus-after-command\]'\)/);
	assert.match(
		source,
		/<PaneButton\s+focusAfterCommand\s+onClick=\{\(\) => void dispatch\("RefreshSubscriptionVideos"\)\}/,
	);
	assert.doesNotMatch(source, /data-pane-footer-control/);
});

/** Episode metadata must not expose the channel-only download action. */
test('full-channel download requires a subscribed YouTube channel entity', async () => {
	const details = await readFile(
		new URL('./components/Details.tsx', import.meta.url),
		'utf8',
	);
	const beforeButton = details.split('dispatch("OpenChannelDownload")')[0];
	const guard = beforeButton.slice(beforeButton.lastIndexOf('{view.channel_download_supported &&'));
	assert.match(guard, /details\.media_id === null/);
	assert.match(guard, /view\.screen === 'Subscriptions'/);
	assert.match(guard, /view\.subscriptions\.source_kind === YOUTUBE/);
	assert.match(guard, /details\.channel_subscribed/);
	assert.match(guard, /details\.channel_id !== ''/);
	assert.doesNotMatch(guard, /\|\||view\.screen === 'Search'|\bkind ===/);
});

/** Channel metadata on an episode must not expose the channel podcast button. */
test('YouTube podcast buttons are limited to channel entities, including search channels', async () => {
	const details = await readFile(
		new URL('./components/Details.tsx', import.meta.url),
		'utf8',
	);
	const beforeButton = details.split("dispatch('ShareYouTubeChannelPodcast')")[0];
	const guard = beforeButton.slice(beforeButton.lastIndexOf('{view.lan_share_supported &&'));
	assert.match(guard, /details\.media_id === null/);
	assert.match(guard, /details\.channel_id !== ''/);
	assert.doesNotMatch(guard, /kind === 'Channel'/);
	assert.doesNotMatch(guard, /kind === 'Video'/);
});

const require = createRequire(import.meta.url);
const React = require('react');
const ts = require('typescript');
const actions = [];
const detailsSource = await readFile(new URL('./components/Details.tsx', import.meta.url), 'utf8');
const compiledDetails = ts.transpileModule(detailsSource, {
	compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
}).outputText;
const detailsModule = { exports: {} };

/** Render the real Details actions with inert effects and a deterministic IPC bridge. */
function detailsRequire(name) {
	if (name === 'react') return { ...React, useEffect: () => {}, useRef: (current) => ({ current }) };
	if (name === '../ipc') return { dispatch: (action) => actions.push(action) };
	if (name === '../searchHighlights') return { highlightRanges: () => [] };
	if (name === './Artwork') return { Artwork: () => null };
	if (name === './Description') return { Description: () => null, WikidataSpoiler: () => null };
	if (name === './SearchHighlight') return { SearchHighlight: ({ text }) => text };
	return require(name);
}

new Function('require', 'module', 'exports', compiledDetails)(detailsRequire, detailsModule, detailsModule.exports);

/** Expand function components so checks exercise rendered buttons and their handlers. */
function elements(element) {
	if (!React.isValidElement(element)) return [];
	if (typeof element.type === 'function') return elements(element.type(element.props));
	return [element, ...React.Children.toArray(element.props.children).flatMap(elements)];
}

/** Minimal provider fixture keeps unrelated panels and optional capabilities inactive. */
function subscriptionButtons(screen, subscribed, mediaSource = 'you-tube', channelId = 'UCfixture', kind = 'Video') {
	const view = {
		screen, playlist_item: null,
		details: {
			title: 'Fixture video', source: 'YouTube', channel_id: channelId,
			channel_subscribed: subscribed,
			media_id: mediaSource === null ? null : { source: mediaSource, external_id: 'fixture-video' },
			playlist_names: [], links: [], dearrow_title: null,
			channel_subscriber_count: null, channel_video_count: null, channel_total_view_count: null,
		},
	};
	return elements(detailsModule.exports.Details({ view, kind }))
		.filter((node) => node.type === 'button' && ['Subscribe', 'Unsubscribe'].includes(node.props.children));
}

test('unsubscribed YouTube search videos expose Subscribe and dispatch its action', () => {
	const buttons = subscriptionButtons('Search', false);
	assert.deepEqual(buttons.map((node) => node.props.children), ['Subscribe']);
	actions.length = 0;
	buttons[0].props.onClick();
	assert.deepEqual(actions, ['ToggleSubscription']);
});

test('video subscription controls reject subscribed channels, other screens and other providers', () => {
	for (const [screen, subscribed, source, channelId] of [
		['Search', true, 'you-tube', 'UCfixture'],
		['Search', false, 'you-tube', ''],
		['Search', false, 'archive-org', 'UCfixture'],
		['Subscriptions', false, 'you-tube', 'UCfixture'],
		['Subscriptions', true, 'you-tube', 'UCfixture'],
		['Playlists', false, 'you-tube', 'UCfixture'],
		['Downloaded', false, 'you-tube', 'UCfixture'],
	]) {
		assert.deepEqual(subscriptionButtons(screen, subscribed, source, channelId), [],
			`${screen}, subscribed=${subscribed}, source=${source}, channel=${channelId}`);
	}
});

test('channel entities retain both subscription actions in search and channel layouts', () => {
	for (const [screen, kind] of [['Search', 'Video'], ['Subscriptions', 'Channel']]) {
		for (const subscribed of [false, true]) {
			const buttons = subscriptionButtons(screen, subscribed, null, 'UCfixture', kind);
			assert.deepEqual(buttons.map((node) => node.props.children), [subscribed ? 'Unsubscribe' : 'Subscribe']);
			actions.length = 0;
			buttons[0].props.onClick();
			assert.deepEqual(actions, ['ToggleSubscription']);
			assert.deepEqual(subscriptionButtons(screen, subscribed, null, '', kind), []);
		}
	}
});
