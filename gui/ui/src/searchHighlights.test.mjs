import assert from 'node:assert/strict';
import test from 'node:test';
import { highlightRanges, relativeHighlightRanges } from './searchHighlights.ts';

/** Indexed link groups must not leak styling into adjacent labels or fields. */
test('highlight groups select exact fields and link indices', () => {
	const ranges = [{ start_byte: 7, end_byte: 16 }];
	const groups = [{ field: { LinkLabel: 2 }, ranges }];
	assert.deepEqual(highlightRanges(groups, { LinkLabel: 2 }), ranges);
	assert.deepEqual(highlightRanges(groups, { LinkLabel: 1 }), []);
	assert.deepEqual(highlightRanges(groups, { LinkUrl: 2 }), []);
	assert.deepEqual(highlightRanges(groups, 'Description'), []);
	assert.deepEqual(highlightRanges(undefined, 'Title'), []);
});

/** Nested actionable spans clip byte ranges without decoding or lowercasing text. */
test('highlight slices preserve byte offsets across Unicode and action boundaries', () => {
	const text = '📚 Привет 1:23';
	const bytes = new TextEncoder().encode(text);
	const offset = new TextEncoder().encode('📚 Привет ').length;
	const matches = [{ start_byte: offset, end_byte: bytes.length }];
	assert.deepEqual(relativeHighlightRanges(matches, offset, 4), [{ start_byte: 0, end_byte: 4 }]);
	assert.deepEqual(relativeHighlightRanges(matches, offset + 1, 2), [{ start_byte: 0, end_byte: 2 }]);
	assert.deepEqual(relativeHighlightRanges(matches, 0, offset), []);
	assert.deepEqual(relativeHighlightRanges(matches, bytes.length, 0), []);
});

/** Invalid ranges are ignored rather than widening styling to unrelated text. */
test('malformed match ranges degrade to unhighlighted text', () => {
	assert.deepEqual(relativeHighlightRanges([
		{ start_byte: -1, end_byte: 3 },
		{ start_byte: 2, end_byte: 1 },
		{ start_byte: 0.5, end_byte: 2 },
		{ start_byte: 0, end_byte: Infinity },
	], 0, 10), []);
});
