import type { DetailHighlightField, DetailHighlightRange, DetailHighlightView } from './contract';

/** Selects controller-owned byte ranges without redoing the search in JavaScript. */
export function highlightRanges(
	groups: readonly DetailHighlightView[] | undefined,
	field: DetailHighlightField,
): readonly DetailHighlightRange[] {
	return groups?.find((group) => JSON.stringify(group.field) === JSON.stringify(field))?.ranges ?? [];
}

/** Clips source-relative match ranges into one existing text or action span. */
export function relativeHighlightRanges(
	ranges: readonly DetailHighlightRange[],
	offset: number,
	byteLength: number,
): DetailHighlightRange[] {
	return ranges.flatMap((range) => {
		if (!Number.isSafeInteger(range.start_byte) || !Number.isSafeInteger(range.end_byte)
			|| range.start_byte < 0 || range.end_byte <= range.start_byte) return [];
		const start = Math.max(offset, range.start_byte);
		const end = Math.min(offset + byteLength, range.end_byte);
		return start < end ? [{ start_byte: start - offset, end_byte: end - offset }] : [];
	});
}
