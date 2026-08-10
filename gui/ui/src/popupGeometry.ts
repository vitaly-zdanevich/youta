// How much of each scrollable popup this window rendered.
//
// The shared keyboard map resolves PageUp, PageDown, and End against the
// viewport that is actually on screen; in the terminal those numbers come from
// the renderer's hit map. Here they come from the DOM, and they have to be
// readable synchronously inside a `keydown` handler — which is why this is a
// module-level record rather than React state. Routing it through state would
// re-render the whole window on every resize to deliver three integers to one
// event handler.

import type { PopupGeometry, ScrollGeometry } from "./contract";

/**
 * Line height of the scrollable popup bodies, in pixels.
 *
 * The reducer counts wrapped *lines*, so the window has to agree with itself
 * about how tall a line is. Pinning it here and setting the same value in the
 * popups' styles keeps the conversion exact instead of measured-then-rounded.
 */
export const POPUP_LINE_HEIGHT = 17;

/** Which popups report geometry. */
type Scrollable = keyof PopupGeometry;

const EMPTY: ScrollGeometry = { offset: 0, maximum: 0, page_lines: 1 };

const measured: Record<Scrollable, ScrollGeometry> = {
  project_history: { ...EMPTY },
  video_comments: { ...EMPTY },
};

/**
 * Records what a popup rendered.
 *
 * `offset` is not measured: it is the offset the reducer published, because the
 * reducer owns it. Reading the DOM's `scrollTop` back would let rounding drift
 * accumulate into a scroll position neither side agrees on.
 */
export function reportGeometry(popup: Scrollable, element: HTMLElement | null, offset: number) {
  if (element === null) {
    measured[popup] = { ...EMPTY };
    return;
  }
  const pageLines = Math.max(1, Math.floor(element.clientHeight / POPUP_LINE_HEIGHT));
  const totalLines = Math.ceil(element.scrollHeight / POPUP_LINE_HEIGHT);
  measured[popup] = {
    offset,
    maximum: Math.max(0, totalLines - pageLines),
    page_lines: pageLines,
  };
}

/** Reads the current geometry for a key press. */
export function popupGeometry(): PopupGeometry {
  return { project_history: measured.project_history, video_comments: measured.video_comments };
}
