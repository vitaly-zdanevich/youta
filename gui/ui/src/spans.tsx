// Rendering provider text that Rust has annotated with spans.
//
// SECURITY: this is the one place rich content is built out of untrusted text,
// and it is built out of *positions*, never out of markup. Rust finds the
// timecodes, video URLs, and Wikidata values and reports where they are; this
// file slices the string at those offsets and hands each piece to React as a
// text child. No provider string is ever parsed here, and none is ever
// interpreted as HTML.

import type { ReactNode } from "react";

/**
 * One annotated region, in the UTF-8 byte offsets Rust reports.
 *
 * The unit matters. Rust indexes strings by UTF-8 byte and JavaScript indexes
 * them by UTF-16 code unit, so the two agree only across ASCII — and a video
 * description is exactly the kind of text that is not ASCII. Slicing the
 * JavaScript string with Rust's numbers would silently mis-cut every
 * description containing an emoji or a Cyrillic character.
 */
export interface ByteSpan {
  start_byte: number;
  end_byte: number;
}

/** Renders one annotated region, given the exact text it covers. */
export type SpanRenderer<T extends ByteSpan> = (span: T, text: string, key: string) => ReactNode;

const encoder = new TextEncoder();
// Non-fatal by default: a malformed slice degrades to U+FFFD rather than
// throwing and blanking the panel. The reducer reports offsets on character
// boundaries, so this is a backstop, not an expectation.
const decoder = new TextDecoder();

/**
 * Splits `text` at the given spans and renders each piece.
 *
 * Spans are sorted and de-overlapped: the reducer produces disjoint ranges, but
 * one bad range must degrade to plain text rather than duplicate or drop it.
 */
export function annotate<T extends ByteSpan>(
  text: string,
  spans: readonly T[],
  render: SpanRenderer<T>,
): ReactNode {
  if (text === "") {
    return null;
  }
  const bytes = encoder.encode(text);
  const ordered = [...spans]
    .filter((span) => span.start_byte < span.end_byte && span.end_byte <= bytes.length)
    .sort((left, right) => left.start_byte - right.start_byte);

  const pieces: ReactNode[] = [];
  let cursor = 0;
  for (const [index, span] of ordered.entries()) {
    if (span.start_byte < cursor) {
      continue;
    }
    if (span.start_byte > cursor) {
      pieces.push(decoder.decode(bytes.subarray(cursor, span.start_byte)));
    }
    pieces.push(
      render(span, decoder.decode(bytes.subarray(span.start_byte, span.end_byte)), `s${index}`),
    );
    cursor = span.end_byte;
  }
  if (cursor < bytes.length) {
    pieces.push(decoder.decode(bytes.subarray(cursor)));
  }
  return pieces;
}
