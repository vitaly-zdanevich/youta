import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
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
