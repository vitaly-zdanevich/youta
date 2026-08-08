// The window's single connection to the reducer.

import { useCallback, useEffect, useRef, useState } from "react";

import type { AudioOutputView, PlaybackTick, ScreenEntry, ViewModel } from "./contract";
import { audioOutput, onPlayback, onView, reportFailure, screens, snapshot } from "./ipc";

/** Everything the window renders, plus how it started. */
export interface YoutaState {
  view: ViewModel | null;
  sources: ScreenEntry[];
  output: AudioOutputView | null;
  failure: string | null;
}

/**
 * Subscribes to the reducer and keeps the latest view.
 *
 * Two channels arrive. `youta://view` carries the whole snapshot and is sent
 * only when it changes; `youta://playback` carries just the fields a playing
 * item rewrites four times a second. Merging the second into the last snapshot
 * is what keeps a moving position from retransmitting the list — measured at
 * 391 bytes against 19 KiB.
 */
export function useYouta(): YoutaState {
  const [state, setState] = useState<YoutaState>({
    view: null,
    sources: [],
    output: null,
    failure: null,
  });
  // The merge target must be the newest view, and a stale closure would silently
  // merge into an old one, so the ref is the source of truth for merging.
  const latest = useRef<ViewModel | null>(null);

  const applyView = useCallback((view: ViewModel) => {
    latest.current = view;
    setState((previous) => ({ ...previous, view }));
  }, []);

  const applyTick = useCallback((tick: PlaybackTick) => {
    const base = latest.current;
    if (base === null) {
      return;
    }
    const merged: ViewModel = { ...base, ...tick };
    latest.current = merged;
    setState((previous) => ({ ...previous, view: merged }));
  }, []);

  useEffect(() => {
    let cancelled = false;
    const unsubscribers: Array<() => void> = [];

    void (async () => {
      try {
        // Subscribing comes first, and the order matters. The reducer publishes
        // only what changed, so anything that moves between reading the snapshot
        // and attaching the listener is not merely late — it is lost, and the
        // window stays wrong until something unrelated happens to change. A
        // background listing or an OPML load finishing during startup is exactly
        // that window.
        unsubscribers.push(await onView(applyView), await onPlayback(applyTick));
        const [sources, output, initial] = await Promise.all([
          screens(),
          audioOutput(),
          snapshot(),
        ]);
        if (cancelled) {
          return;
        }
        // A snapshot that raced a published event is the older of the two: the
        // reducer writes the shared snapshot before emitting, so any event
        // already applied is at least as new as what this read returned.
        const view = latest.current ?? initial;
        latest.current = view;
        setState({ view, sources, output, failure: null });
      } catch (error) {
        if (!cancelled) {
          const message = String(error);
          setState((previous) => ({ ...previous, failure: message }));
          // Also report it out of the web view. A failure visible only in here
          // looks like a clean start from every angle outside the window.
          reportFailure("startup", message);
        }
      }
    })();

    return () => {
      cancelled = true;
      for (const unsubscribe of unsubscribers) {
        unsubscribe();
      }
    };
  }, [applyView, applyTick]);

  return state;
}
