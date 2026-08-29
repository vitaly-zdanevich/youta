import { useEffect, useRef } from "react";
import type { ReactNode } from "react";

import { POPUP_LINE_HEIGHT, popupGeometry, reportGeometry } from "../popupGeometry";

/**
 * A popup body whose scroll position the reducer owns.
 *
 * Scrolling is not a local gesture here. The reducer keeps the offset so it
 * survives resizes and so both front-ends behave the same, which means this
 * component drives `scrollTop` from the published offset rather than the other
 * way round, and reports back only how much fits on screen.
 */
export function ScrollingText({
  popup,
  offset,
  onScroll,
  children,
}: {
  popup: "audio_quality" | "video_summary" | "project_history" | "video_comments";
  offset: number;
  onScroll: (offset: number) => void;
  children: ReactNode;
}) {
  const body = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const element = body.current;
    if (element === null) {
      return;
    }
    element.scrollTop = offset * POPUP_LINE_HEIGHT;
    reportGeometry(popup, element, offset);

    const observer = new ResizeObserver(() => reportGeometry(popup, element, offset));
    observer.observe(element);
    return () => {
      observer.disconnect();
      reportGeometry(popup, null, 0);
    };
  }, [popup, offset, children]);

  return (
    <div
      ref={body}
      // A wheel gesture has to become the same action a key would produce, or
      // the reducer's offset and the visible position part ways.
      onWheel={(event) => {
        const lines = Math.round(event.deltaY / POPUP_LINE_HEIGHT) || Math.sign(event.deltaY);
        if (lines !== 0) {
          const { offset: renderedOffset, maximum } = popupGeometry()[popup];
          onScroll(Math.min(maximum, Math.max(0, renderedOffset + lines)));
        }
      }}
      className="h-full overflow-hidden px-[18px] py-[10px] font-mono text-[11px] whitespace-pre-wrap"
      style={{ lineHeight: `${POPUP_LINE_HEIGHT}px` }}
    >
      {children}
    </div>
  );
}
