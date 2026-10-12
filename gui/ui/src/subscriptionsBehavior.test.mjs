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

const require = createRequire(import.meta.url);
const React = require('react');
const ts = require('typescript');
const actions = [];

/** Render App's route composition with inert effects and named component boundaries. */
async function channelRouteTree(screen, browsing) {
	const appSource = await readFile(new URL('./App.tsx', import.meta.url), 'utf8');
	const compiled = ts.transpileModule(appSource, {
		compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
	}).outputText;
	const view = {
		screen, youtube_channel_browsing: browsing, playback_history_enabled: true,
		playback: { idle: true }, subscriptions: { layout: 'drill-down', route: 'Sources' },
	};
	const appModule = { exports: {} };
	const appRequire = (name) => {
		if (name === 'react') return { ...React, useEffect: () => {}, useRef: (current) => ({ current }), useCallback: (fn) => fn };
		if (name === './useYouta') return { useYouta: () => ({ view, sources: [
			{ id: 'Search', label: 'YouTube', search_verb: 'Search', details_kind: 'Video' },
			{ id: 'Subscriptions', label: 'Subs', search_verb: null, details_kind: 'Video' },
		], output: null, failure: null }) };
		if (name.startsWith('./')) return new Proxy({}, { get: (_, key) => key });
		return require(name);
	};
	new Function('require', 'module', 'exports', compiled)(appRequire, appModule, appModule.exports);
	return appModule.exports.App();
}

test('transient channel uses the YouTube tab and search bar while retaining shared channel panes', async () => {
	const tree = elements(await channelRouteTree('Subscriptions', true));
	assert.equal(tree.find((node) => node.type === 'Tabs').props.active, 'Search');
	assert.equal(tree.find((node) => node.type === 'SearchBar').props.label, 'YouTube');
	assert.equal(tree.find((node) => node.type === 'SearchBar').props.view.screen, 'Subscriptions', 'the query editor retains its adapter-scoped control guards');
	assert.equal(tree.find((node) => node.type === 'Subscriptions').props.transientChannel, true);
	assert.equal(tree.find((node) => node.type === 'Subscriptions').props.details.props.kind, 'Channel');
	assert.ok(tree.some((node) => node.props['data-subscriptions-screen'] === true));
});

test('real Subs and hashtag search keep their own tab and list projection', async () => {
	for (const [screen, browsing] of [['Subscriptions', false], ['Search', true]]) {
		const tree = elements(await channelRouteTree(screen, browsing));
		assert.equal(tree.find((node) => node.type === 'Tabs').props.active, screen);
		assert.equal(tree.some((node) => node.type === 'Subscriptions'), screen === 'Subscriptions');
		assert.equal(tree.some((node) => node.type === 'SearchBar'), screen === 'Search');
	}
});

test('transient channel panes hide RSS management without changing source selection or Enter', () => {
	const compiled = ts.transpileModule(source, {
		compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
	}).outputText;
	const subscriptionsModule = { exports: {} };
	const subscriptionsRequire = (name) => {
		if (name === 'react') return { ...React, useEffect: () => {}, useRef: (current) => ({ current }) };
		if (name === '@tanstack/react-virtual') return { useVirtualizer: () => ({
			getTotalSize: () => 60, getVirtualItems: () => [{ index: 0, key: 0, start: 0, size: 60 }],
		}) };
		if (name === '../ipc') return { dispatch: (action) => actions.push(action) };
		if (name === '../subscriptionPageRows') return { SUBSCRIPTION_ROW_HEIGHT: 60 };
		if (name === './Artwork') return { Artwork: () => null };
		if (name === './RowSubtitle') return { RowSubtitle: () => null, rowSubtitleColorClass: () => '' };
		if (name === '../format') return { formatCount: String };
		return require(name);
	};
	new Function('require', 'module', 'exports', compiled)(subscriptionsRequire, subscriptionsModule, subscriptionsModule.exports);
	for (const transientChannel of [false, true]) {
		actions.length = 0;
		const tree = elements(subscriptionsModule.exports.Subscriptions({
			transientChannel, playing: null, details: null,
			subscriptions: { layout: 'drill-down', route: 'Sources', focus: 'Sources', selected_source: 0,
				source_kind: 'you-tube', sources: [{ title: 'Fixture channel', subtitle: '', media_id: null }], items: [] },
		}));
		assert.equal(tree.some((node) => node.type === 'button' && buttonText(node) === 'Add RSS feed...'), !transientChannel);
		const row = tree.find((node) => node.type === 'button' && node.props.onDoubleClick);
		row.props.onClick({ ctrlKey: false });
		row.props.onDoubleClick();
		assert.deepEqual(actions, [{ SelectSubscriptionSource: 0 }, 'ActivateSelection']);
	}
});
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
	if (name === './CustomCommands') return { CustomCommandButtons: () => null };
	return require(name);
}

new Function('require', 'module', 'exports', compiledDetails)(detailsRequire, detailsModule, detailsModule.exports);

/** Expand function components so checks exercise rendered buttons and their handlers. */
function elements(element) {
	if (!React.isValidElement(element)) return [];
	if (typeof element.type === 'function') return elements(element.type(element.props));
	return [element, ...React.Children.toArray(element.props.children).flatMap(elements)];
}

/** Flatten shortcut markup and labels as users read the rendered action. */
function buttonText(element) {
	if (typeof element === 'string' || typeof element === 'number') return String(element);
	if (!React.isValidElement(element)) return '';
	if (typeof element.type === 'function') return buttonText(element.type(element.props));
	return React.Children.toArray(element.props.children).map(buttonText).join('');
}

/** Minimal provider fixture keeps unrelated panels and optional capabilities inactive. */
function channelDetailButtons(screen, subscribed, mediaSource = 'you-tube', channelId = 'UCfixture', kind = 'Video') {
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
		.filter((node) => node.type === 'button');
}

/** Isolate subscribe controls while keeping the shared real-render fixture. */
function subscriptionButtons(...args) {
	return channelDetailButtons(...args)
		.filter((node) => ['Subscribe', 'Unsubscribe'].includes(node.props.children));
}

/** Feed controls keep the selected video boundary in the existing core action. */
function podcastButtons({ screen = 'Search', kind = 'Video', mediaSource = 'you-tube',
	channelId = 'UCfixture', sourceKind = 'you-tube', supported = true } = {}) {
	const view = {
		screen, lan_share_supported: supported, playlist_item: null,
		subscriptions: { source_kind: sourceKind },
		details: { title: 'Fixture episode', source: 'YouTube', channel_id: channelId,
			media_id: mediaSource === null ? null : { source: mediaSource, external_id: 'fixture-video' },
			playlist_names: [], links: [], dearrow_title: null,
			channel_subscriber_count: null, channel_video_count: null, channel_total_view_count: null },
	};
	return elements(detailsModule.exports.Details({ view, kind }))
		.filter((node) => node.type === 'button' && buttonText(node).endsWith('Podcast'));
}

test('YouTube episodes and channels show the compact Podcast label with Shift+F2', () => {
	for (const screen of ['Search', 'Subscriptions']) {
		for (const mediaSource of ['you-tube', null]) {
			actions.length = 0;
			const buttons = podcastButtons({ screen, mediaSource });
			assert.equal(buttons.length, 1, `${screen}, source=${mediaSource}`);
			assert.match(buttonText(buttons[0]), /^\[Shift\+F2\]\s*Podcast$/);
			assert.equal(elements(buttons[0]).filter((node) => node.type === 'kbd').length, 1);
			assert.deepEqual(actions, [], 'rendering cannot start feed preparation');
			buttons[0].props.onClick();
			assert.deepEqual(actions, ['ShareYouTubeChannelPodcast']);
		}
	}
});

test('YouTube podcast controls retain feature, route and channel identity gates', () => {
	for (const patch of [
		{ supported: false }, { channelId: '' }, { mediaSource: 'sound-cloud' },
		{ screen: 'YouTubeMusic' }, { screen: 'History' }, { screen: 'Downloaded' },
		{ screen: 'Subscriptions', sourceKind: 'rss' },
	]) assert.deepEqual(podcastButtons(patch), [], JSON.stringify(patch));
});

test('Local shows the compact Podcast label with Shift+F2 and keeps its action', () => {
	actions.length = 0;
	const patch = { screen: 'Local', kind: 'Local', mediaSource: 'local', channelId: '' };
	const buttons = podcastButtons(patch);
	assert.equal(buttons.length, 1);
	assert.match(buttonText(buttons[0]), /^\[Shift\+F2\]\s*Podcast$/);
	assert.equal(elements(buttons[0]).filter((node) => node.type === 'kbd').length, 1);
	buttons[0].props.onClick();
	assert.deepEqual(actions, ['ShareLocalPodcast']);
	assert.deepEqual(podcastButtons({ ...patch, supported: false }), []);
});

test('unsubscribed YouTube search videos expose Subscribe and dispatch its action', () => {
	const buttons = subscriptionButtons('Search', false);
	assert.deepEqual(buttons.map((node) => node.props.children), ['Subscribe']);
	actions.length = 0;
	buttons[0].props.onClick();
	assert.deepEqual(actions, ['ToggleSubscription']);
});

test('To channel immediately follows Subscribe and dispatches in-app channel browsing', () => {
	actions.length = 0;
	const buttons = channelDetailButtons('Search', false);
	const labels = buttons.map((node) => node.props.children);
	const subscribeIndex = labels.indexOf('Subscribe');
	assert.ok(subscribeIndex >= 0);
	assert.equal(labels[subscribeIndex + 1], 'To channel');
	assert.deepEqual(actions, [], 'rendering must not navigate or subscribe');
	buttons[subscribeIndex + 1].props.onClick();
	assert.deepEqual(actions, ['BrowseYouTubeChannel']);
});

test('already subscribed YouTube videos retain To channel without a Subscribe button', () => {
	actions.length = 0;
	const buttons = channelDetailButtons('Search', true);
	assert.ok(!buttons.some((node) => node.props.children === 'Subscribe'));
	const browse = buttons.filter((node) => node.props.children === 'To channel');
	assert.equal(browse.length, 1);
	browse[0].props.onClick();
	assert.deepEqual(actions, ['BrowseYouTubeChannel']);
});

test('To channel requires a YouTube search video with a channel identity', () => {
	for (const [screen, source, channelId, kind] of [
		['Search', 'you-tube', '', 'Video'],
		['Search', 'sound-cloud', 'UCfixture', 'Video'],
		['Search', 'archive-org', 'UCfixture', 'Video'],
		['Search', null, 'UCfixture', 'Video'],
		['Subscriptions', null, 'UCfixture', 'Channel'],
		['Subscriptions', 'you-tube', 'UCfixture', 'Video'],
		['YouTubeMusic', 'you-tube', 'UCfixture', 'Video'],
		['History', 'you-tube', 'UCfixture', 'Video'],
		['Downloaded', 'you-tube', 'UCfixture', 'Video'],
	]) {
		const buttons = channelDetailButtons(screen, false, source, channelId, kind);
		assert.ok(!buttons.some((node) => node.props.children === 'To channel'),
			`${screen}, source=${source}, channel=${channelId}`);
	}
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
