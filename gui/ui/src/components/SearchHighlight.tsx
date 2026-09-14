import type { DetailHighlightRange } from '../contract';
import { relativeHighlightRanges } from '../searchHighlights';
import { annotate } from '../spans';

const encoder = new TextEncoder();

/** Applies match styling to text children only, preserving surrounding actions. */
export function SearchHighlight({ text, ranges, offset = 0 }: {
	text: string;
	ranges: readonly DetailHighlightRange[];
	offset?: number;
}) {
	if (ranges.length === 0) return text;
	return annotate(text, relativeHighlightRanges(ranges, offset, encoder.encode(text).length),
		(_range, covered, key) => (
			<mark key={key} className='bg-amber-200 text-black'>{covered}</mark>
		));
}
