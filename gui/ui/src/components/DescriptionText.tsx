import type { DetailHighlightRange, DetailUrlEscapeView } from '../contract';
import { annotate } from '../spans';
import { SearchHighlight } from './SearchHighlight';

const encoder = new TextEncoder();

/** Looks up an intersection in the controller's sorted, disjoint match ranges. */
function highlighted(ranges: readonly DetailHighlightRange[], start: number, end: number): boolean {
	let low = 0;
	let high = ranges.length;
	while (low < high) {
		const middle = (low + high) >>> 1;
		if (ranges[middle]!.end_byte <= start) low = middle + 1;
		else high = middle;
	}
	return low < ranges.length && ranges[low]!.start_byte < end;
}

/**
 * Displays controller-decoded URL graphemes without rewriting the description.
 * Original source bytes remain attached for selection copying. Highlight and
 * action offsets still refer to the unmodified text, including after a URL.
 */
export function DescriptionText({ text, offset = 0, escapes, highlights }: {
	text: string;
	offset?: number;
	escapes: readonly DetailUrlEscapeView[];
	highlights: readonly DetailHighlightRange[];
}) {
	const end = offset + encoder.encode(text).length;
	const replacements = escapes.filter((span) => span.start_byte >= offset && span.end_byte <= end)
		.map((span) => ({ ...span, start_byte: span.start_byte - offset, end_byte: span.end_byte - offset }));
	return annotate(text, replacements, (span, original, key) => (
		<span key={key} data-url-escape-original={original}>
			{highlighted(highlights, offset + span.start_byte, offset + span.end_byte)
				? <mark className='bg-amber-200 text-black'>{span.text}</mark> : span.text}
		</span>
	), (plain, start) => <SearchHighlight key={`plain-${start}`} text={plain} ranges={highlights} offset={offset + start} />);
}
