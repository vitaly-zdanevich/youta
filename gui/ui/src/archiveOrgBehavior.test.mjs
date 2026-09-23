import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const details = await readFile(new URL('./components/Details.tsx', import.meta.url), 'utf8');
const popups = await readFile(new URL('./components/popups.tsx', import.meta.url), 'utf8');
const search = await readFile(new URL('./components/SearchBar.tsx', import.meta.url), 'utf8');
const contract = await readFile(new URL('./contract.ts', import.meta.url), 'utf8');

/** Archive facts retain their actual meanings rather than borrowing YouTube labels. */
test('Archive.org generic details expose downloads, favourites, and upload date', () => {
	assert.match(details, /details\.media_id\?\.source === 'archive-org'/);
	for (const [label, field] of [
		['Favourites', 'likes'], ['Downloads', 'views'], ['Comments', 'comments'], ['Uploaded', 'published'],
	]) {
		assert.ok(details.includes(`['${label}', fact(details.${field})]`));
	}
});

/** Item-page actions work independently of the Video layout. */
test('Archive.org item pages are available in the generic panel', () => {
	assert.match(details, /const isArchiveOrg = details\.media_id\?\.source === 'archive-org'/);
	assert.match(details, /isArchiveOrg && details\.webpage_url !== null/);
});

/** Exercise the actual JSX guard with mock provider capabilities and tab state. */
function commentsButtonVisible(view, kind, source) {
	const guard = details.match(/\{([^\n]+) \? \(\s*<Action onClick=\{\(\) => void dispatch\("OpenVideoComments"\)\}>Comments<\/Action>/)?.[1];
	assert.ok(guard, 'Comments button must have a provider-aware visibility guard');
	return new Function('view', 'kind', 'isYouTube', 'isArchiveOrg', 'isSoundCloud', `return (${guard});`)(
		view, kind, source === 'youtube', source === 'archive-org', source === 'sound-cloud',
	);
}

test('Archive.org comments do not require a YouTube backend', () => {
	assert.equal(commentsButtonVisible({
		screen: 'ArchiveOrg', video_comments_available: false,
	}, 'Generic', 'archive-org'), true);
});

test('Archive.org comments stay hidden on history and other tabs', () => {
	for (const screen of ['History', 'Downloaded', 'Playlists', 'Search']) {
		for (const video_comments_available of [false, true]) {
			assert.equal(commentsButtonVisible({ screen, video_comments_available }, 'Generic', 'archive-org'), false);
		}
	}
});

test('YouTube comments still require its backend and Video layout', () => {
	for (const video_comments_available of [false, true]) {
		for (const kind of ['Generic', 'Video']) {
			assert.equal(commentsButtonVisible({
				screen: 'Search', video_comments_available,
			}, kind, 'youtube'), video_comments_available && kind === 'Video');
		}
	}
});

/** Public SoundCloud comments belong only to the selected SoundCloud source tab. */
test('SoundCloud comments do not borrow YouTube capability or appear on History', () => {
	for (const screen of ['SoundCloud', 'History', 'Downloaded', 'Playlists', 'Search']) {
		assert.equal(commentsButtonVisible({ screen, video_comments_available: false }, 'Generic', 'sound-cloud'), screen === 'SoundCloud');
	}
});

/** Review stars must not become likes; empty reviews are not called video comments. */
test('Archive.org comments popup retains source identity and omits fictional likes', () => {
	assert.match(popups, /const archiveOrg = popup\.source === 'archive-org'/);
	assert.match(popups, /title=\{archiveOrg \? 'archive.org comments' : soundcloud \? 'SoundCloud comments' : 'Comments'\}/);
	assert.match(popups, /This item has no public reviews\./);
	assert.match(popups, /archiveOrg \|\| soundcloud\s*\? comment\.published/);
	assert.match(contract, /export interface VideoCommentsPopupView \{\s*source: string;/);
});

/** Search, item loading, and parent navigation reuse the shared reducer actions. */
test('Archive.org browsing has loading feedback and a back button', () => {
	assert.match(search, /view\.screen === 'ArchiveOrg'/);
	assert.match(search, /view\.search_activity === 'ArchiveOrg'/);
	assert.match(search, /aria-label='Loading archive\.org'/);
	assert.match(search, /dispatch\('GoBack'\)/);
});
