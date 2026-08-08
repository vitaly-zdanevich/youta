import { useCallback, useEffect, useRef, useState } from "react";
import type { MouseEvent } from "react";

import type { MediaId, ViewModel, WaveformReady } from "../contract";
import { dispatch, waveformPeaks } from "../ipc";
import { durationSeconds } from "../format";

/** Drawn height of the envelope, in CSS pixels. */
const WAVEFORM_HEIGHT = 64;

/** Peaks currently held, together with the generation that owns them. */
interface LoadedPeaks {
  generation: number;
  columns: number;
  /** Two entries per column: minimum then maximum, each scaled to -1..1. */
  values: Float32Array;
}

/** Reads the ready branch of the waveform union, or null. */
function ready(view: ViewModel): WaveformReady | null {
  const waveform = view.waveform;
  return typeof waveform === "object" && "Ready" in waveform ? waveform.Ready : null;
}

/** The message the terminal shows for each non-drawable waveform state. */
function message(view: ViewModel): string | null {
  const waveform = view.waveform;
  if (waveform === "Unavailable") {
    return "Waveform is available for playable local files.";
  }
  if ("Loading" in waveform) {
    return "Generating local waveform…";
  }
  if ("Failed" in waveform) {
    return `Waveform unavailable: ${waveform.Failed.message}`;
  }
  return null;
}

/**
 * Maps a horizontal position to an absolute second.
 *
 * This mirrors the terminal's column arithmetic in `key_action`'s waveform hit
 * test: the last column is the divisor, so the rightmost pixel lands on the end
 * of the media, and the result is clamped below the duration so a click on that
 * pixel seeks to the final second rather than past it.
 */
function secondsAt(offset: number, width: number, duration: number): number {
  const lastColumn = Math.max(width - 1, 1);
  const position = (Math.floor(offset) * duration) / lastColumn;
  return Math.max(0, Math.floor(Math.min(position, Math.max(duration - 1e-9, 0))));
}

/** Paints the envelope, split at the playback position. */
function paint(
  canvas: HTMLCanvasElement,
  values: Float32Array,
  playedFraction: number,
  colors: { played: string; remaining: string },
): void {
  const context = canvas.getContext("2d");
  if (context === null) {
    return;
  }
  const columns = Math.floor(values.length / 2);
  const height = canvas.height;
  const middle = height / 2;
  context.clearRect(0, 0, canvas.width, height);
  if (columns === 0) {
    return;
  }
  const playedColumns = Math.round(playedFraction * columns);
  for (let column = 0; column < columns; column += 1) {
    const minimum = values[column * 2] ?? 0;
    const maximum = values[column * 2 + 1] ?? 0;
    const top = middle - maximum * middle;
    const bottom = middle - minimum * middle;
    context.fillStyle = column < playedColumns ? colors.played : colors.remaining;
    // Silence still gets a hairline, so the seek target stays visible and
    // clickable across a quiet passage — the same reason the terminal keeps a
    // one-eighth baseline block.
    context.fillRect(column, top, 1, Math.max(bottom - top, 1));
  }
}

/**
 * The local waveform.
 *
 * Peaks are fetched as bytes for the exact generation the current snapshot
 * names, and a reply is dropped unless that generation is still the one on
 * screen. Without that check a reply in flight while the selection moves would
 * paint one file's envelope over another's — and then a click on those pixels
 * would seek the wrong media, because the seek carries the identity the panel
 * believes it is showing.
 *
 * Unlike the terminal this does not replace the seek bar. The waveform belongs
 * to the *selected* local file, which need not be the playing one — that is
 * exactly what `waveform_playback_matches` reports — so hiding the seek bar
 * would take away the only control for what is actually playing.
 */
export function Waveform({ view }: { view: ViewModel }) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [peaks, setPeaks] = useState<LoadedPeaks | null>(null);
  const [columns, setColumns] = useState(0);

  const owner = ready(view);
  const generation = owner?.generation ?? null;
  const duration = durationSeconds(owner?.duration);
  // The canvas exists only in the ready state, so the observer has to be
  // re-attached when the waveform becomes drawable rather than set up once.
  const drawable = owner !== null;

  // The canvas is measured rather than assumed: the number of columns asked for
  // is the number of device pixels that will be drawn, so Rust reduces to
  // exactly the resolution shown and the window never resamples.
  useEffect(() => {
    const canvas = canvasRef.current;
    if (canvas === null) {
      return;
    }
    const observer = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (entry === undefined) {
        return;
      }
      const ratio = window.devicePixelRatio || 1;
      setColumns(Math.max(1, Math.round(entry.contentRect.width * ratio)));
    });
    observer.observe(canvas);
    return () => observer.disconnect();
  }, [drawable]);

  useEffect(() => {
    if (generation === null || columns === 0) {
      setPeaks(null);
      return;
    }
    let cancelled = false;
    void waveformPeaks(generation, columns)
      .then((values) => {
        // Two guards, and both are needed: `cancelled` covers a re-render, and
        // the generation covers a reply that outlived the media it described.
        if (!cancelled && values.length > 0) {
          setPeaks({ generation, columns, values });
        }
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [generation, columns]);

  const playedFraction =
    view.waveform_playback_matches && !view.playback.idle && duration > 0
      ? Math.min(1, durationSeconds(view.playback.position) / duration)
      : 0;

  useEffect(() => {
    const canvas = canvasRef.current;
    if (canvas === null || peaks === null || peaks.generation !== generation) {
      return;
    }
    canvas.width = peaks.columns;
    canvas.height = Math.round(WAVEFORM_HEIGHT * (window.devicePixelRatio || 1));
    // The two colours come from the design tokens rather than being repeated
    // here, so the played/remaining split stays the same accent the rest of the
    // window spends on the current item.
    const styles = getComputedStyle(canvas);
    paint(canvas, peaks.values, playedFraction, {
      played: styles.getPropertyValue("--color-accent").trim() || "#e4744f",
      remaining: styles.getPropertyValue("--color-line-strong").trim() || "#383544",
    });
  }, [peaks, generation, playedFraction]);

  const seek = useCallback(
    (event: MouseEvent<HTMLCanvasElement>) => {
      const media: MediaId | undefined = owner?.media_id;
      if (owner === null || media === undefined || duration <= 0) {
        return;
      }
      const bounds = event.currentTarget.getBoundingClientRect();
      void dispatch({
        ActivateWaveformTimecode: {
          media_id: media,
          // The generation travels back with the click. The reducer refuses a
          // seek whose generation no longer matches the file it holds, so a
          // click on a frame that has just gone stale is dropped rather than
          // applied to whatever replaced it.
          generation: owner.generation,
          seconds: secondsAt(event.clientX - bounds.left, bounds.width, duration),
        },
      });
    },
    [owner, duration],
  );

  const notice = message(view);
  if (notice !== null) {
    return (
      <p className="border-t border-line bg-surface px-[18px] py-[6px] text-[11px] text-ink-faint">
        {notice}
      </p>
    );
  }

  return (
    <div className="border-t border-line bg-surface px-[18px] py-[6px]">
      <canvas
        ref={canvasRef}
        onClick={seek}
        role="slider"
        aria-label="Waveform"
        aria-valuemin={0}
        aria-valuemax={Math.round(duration)}
        aria-valuenow={Math.round(playedFraction * duration)}
        tabIndex={-1}
        className="block w-full cursor-pointer"
        style={{ height: WAVEFORM_HEIGHT }}
      />
    </div>
  );
}
