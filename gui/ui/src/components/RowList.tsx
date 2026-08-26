import { useEffect, useRef } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";

import type { MediaId, RowView } from "../contract";
import { dispatch } from "../ipc";
import { Artwork } from "./Artwork";

/** Row height in pixels, kept in one place so paging and layout agree. */
export const ROW_HEIGHT = 46;

/**
 * Whether a row is the authoritative playing item.
 *
 * A row does not carry a "playing" flag: identity is `(provider, id)` and the
 * reducer publishes which identity is playing, so the comparison happens here.
 * Matching on the title instead would light up every copy of the same track.
 */
function isPlaying(row: RowView, playing: MediaId | null): boolean {
  return (
    playing !== null &&
    row.media_id !== null &&
    row.media_id.source === playing.source &&
    row.media_id.external_id === playing.external_id
  );
}

/**
 * The main list.
 *
 * SECURITY: every string here originates from a provider — titles, channel
 * names, file names. React escapes text children, which is what makes that
 * safe. Never reach for `dangerouslySetInnerHTML`; the window's CSP is the
 * second line of defence, not the first.
 */
export function RowList({
  rows,
  selected,
  playing,
}: {
  rows: RowView[];
  selected: number;
  playing: MediaId | null;
}) {
  const parentRef = useRef<HTMLDivElement>(null);
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 8,
  });

  // Selection moves by keyboard, so the list has to follow it. Without this a
  // held arrow key walks the selection straight out of view.
  useEffect(() => {
    if (selected < rows.length) {
      virtualizer.scrollToIndex(selected, { align: "auto" });
    }
  }, [selected, rows.length, virtualizer]);

  return (
    <div ref={parentRef} className="overflow-y-auto py-2 pr-[10px] pl-4" aria-label="Results">
      <div className="relative w-full" style={{ height: virtualizer.getTotalSize() }}>
        {virtualizer.getVirtualItems().map((item) => {
          const row = rows[item.index];
          if (row === undefined) {
            return null;
          }
          const current = item.index === selected;
          return (
            <button
              key={item.key}
              type="button"
              aria-current={current}
              onClick={() => void dispatch({ SelectRow: item.index })}
              onDoubleClick={() => void dispatch("ActivateSelection")}
              className={`absolute top-0 left-0 grid w-full grid-cols-[28px_34px_minmax(0,1fr)_auto] items-center gap-[10px] rounded-[5px] px-2 text-left focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-accent ${
                current ? "bg-raised" : ""
              }`}
              style={{ height: item.size, transform: `translateY(${item.start}px)` }}
            >
              <span className="inline-flex gap-[2px] text-[11px] text-accent">
                {isPlaying(row, playing) ? "▶" : row.subscribed ? "◆" : ""}
                {row.local_marked ? "✓" : ""}
              </span>
              <Artwork url={row.thumbnail_url} className="size-[34px] rounded object-cover" />
              <span className="min-w-0">
                <span
                  className={`block truncate font-medium ${current ? "text-accent" : ""}`}
                >
                  {row.title}
                </span>
                {row.subtitle ? (
                  <span className="block truncate text-[11px] text-ink-faint">
                    {row.subtitle}
                  </span>
                ) : null}
              </span>
              {/* Progress is a bar rather than a number: the reducer reports a
                  rounded percentage, and a started item that has not yet reached
                  one percent must still read as started. */}
              <span
                className="w-[42px] justify-self-end"
                title={
                  row.hide_watched_marker
                    ? undefined
                    : row.watched_percent > 0
                      ? `${row.watched_percent}% played`
                      : row.playback_started
                        ? "Started"
                        : undefined
                }
              >
                {!row.hide_watched_marker && (row.playback_started || row.watched_percent > 0) ? (
                  <span className="block h-[3px] rounded-[3px] bg-line-strong">
                    <i
                      className="block h-full rounded-[3px] bg-accent"
                      style={{ width: `${Math.max(3, row.watched_percent)}%` }}
                    />
                  </span>
                ) : null}
              </span>
            </button>
          );
        })}
      </div>
    </div>
  );
}
