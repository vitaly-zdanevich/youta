import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const search = await readFile(new URL('./components/SearchBar.tsx', import.meta.url), 'utf8');
const details = await readFile(new URL('./components/Details.tsx', import.meta.url), 'utf8');

/** Web addresses use the shared editor without posing as provider search terms. */
test('Web offers URL editing, refresh, and a scoped loading indicator', () => {
	assert.match(search, /const web = view\.screen === 'Web'/);
	assert.match(search, /const placeholder = web \? verb :/);
	assert.match(search, /dispatch\('RefreshWeb'\)/);
	assert.match(search, /view\.search_activity === 'Web'/);
	assert.match(search, /Audio only/);
});

/** Empty directories explain the supported input instead of suggesting local files. */
test('Web empty details explain HTTP directories and audio-only playback', () => {
	assert.match(details, /view\.screen === 'Web'/);
	assert.match(details, /Open an HTTP or HTTPS directory URL/);
	assert.match(details, /Audio only/);
});
