// The only place this window talks to Rust.
//
// Keeping every call here means the security-relevant rules are stated once:
// the window never fetches over the network, never reads the filesystem, and
// never decides what a key means.

import type {
  AudioOutputView,
  KeyPress,
  PlaybackTick,
  PopupGeometry,
  ScreenEntry,
  UiAction,
  ViewModel,
} from "./contract";

// Resolved per call, never destructured at module scope. Reading the global
// while this module is first evaluated would throw before anything is wired up,
// taking the failure reporter down with it and leaving a blank window with an
// empty process log — which is precisely the state this file must never create.
function bridge(): NonNullable<Window["__TAURI__"]> {
  const resolved = window.__TAURI__;
  if (resolved === undefined) {
    throw new Error("the Tauri bridge is unavailable");
  }
  return resolved;
}

/**
 * Copies a window-side failure into the process log.
 *
 * Exported because startup reports it too, from outside any invoke.
 */
export function reportFailure(context: string, message: string): void {
  try {
    // Reporting a failed report would recurse, so this one call is raw.
    void bridge()
      .core.invoke("report_window_failure", { context, message })
      .catch(() => undefined);
  } catch {
    // Without a bridge there is nowhere to report to. The console is all that
    // is left, and devtools is where a developer would already be looking.
    console.error(`${context}: ${message}`);
  }
}

/**
 * Calls one command and reports a rejection instead of swallowing it.
 *
 * Every failure here is silent by nature: a rejected promise in a click handler
 * neither changes the window nor reaches stderr. A single misspelled `UiAction`
 * would present as a button that quietly does nothing, which is the hardest
 * possible bug to notice in a front-end whose state is owned elsewhere.
 */
function invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  let call: Promise<T>;
  try {
    call = bridge().core.invoke<T>(command, args);
  } catch (error) {
    reportFailure(command, String(error));
    return Promise.reject(error instanceof Error ? error : new Error(String(error)));
  }
  return call.catch((error: unknown) => {
    reportFailure(command, String(error));
    throw error;
  });
}

function listen<T>(
  event: string,
  handler: (event: { payload: T }) => void,
): Promise<() => void> {
  try {
    return bridge().event.listen<T>(event, handler);
  } catch (error) {
    return Promise.reject(error instanceof Error ? error : new Error(String(error)));
  }
}

/** Snapshot channel: the whole view, published only when it changes. */
const VIEW_EVENT = "youta://view";
/** High-frequency channel: only the fields a playing item rewrites. */
const PLAYBACK_EVENT = "youta://playback";

/** Lists the sources compiled into this build. */
export function screens(): Promise<ScreenEntry[]> {
  return invoke<ScreenEntry[]>("screens");
}

/** Reads the snapshot the window should render right now. */
export function snapshot(): Promise<ViewModel> {
  return invoke<ViewModel>("snapshot");
}

/** Reads the configured audio engine, driver, and device. */
export function audioOutput(): Promise<AudioOutputView> {
  return invoke<AudioOutputView>("audio_output");
}

/**
 * Applies one semantic action.
 *
 * Fire-and-forget: `invoke` has already reported any rejection, and every
 * caller is a click handler with nothing to do about it. Rethrowing here would
 * only surface the same failure a second time through the window's global
 * unhandled-rejection reporter.
 */
export function dispatch(action: UiAction): Promise<void> {
  return invoke<void>("dispatch", { action }).catch(() => undefined);
}

/**
 * Sends one key press to the shared keyboard map.
 *
 * The window reports the press and how much it rendered, and nothing else.
 * Which action a key produces depends on an ordered chain of modal
 * priorities that lives in `src/keymap.rs` and serves the terminal too;
 * restating any part of it here is how the two front-ends drift apart.
 */
export function sendKey(
  press: KeyPress,
  pageRows: number | null,
  popups: PopupGeometry | null,
): Promise<void> {
  return invoke<void>("key", { press, pageRows, popups }).catch(() => undefined);
}

/**
 * Reads the waveform envelope for one exact generation, as normalized peaks.
 *
 * The reply is bytes rather than JSON — four per column, a signed 16-bit
 * minimum then maximum, little-endian — because a window-wide envelope is
 * several thousand numbers that change once per media rather than per frame.
 * `DataView` reads them with the endianness stated explicitly; a typed array
 * would silently inherit the platform's.
 *
 * Rust refuses a generation it no longer owns and answers with nothing, so an
 * empty result means "stale or nothing to draw", never "draw this instead".
 */
export async function waveformPeaks(
  generation: number,
  columns: number,
): Promise<Float32Array> {
  const buffer = await invoke<ArrayBuffer>("waveform_peaks", { generation, columns });
  const bytes = new DataView(buffer);
  // Two values per column, each scaled to -1..1 so the canvas never repeats the
  // sample-format arithmetic.
  const peaks = new Float32Array(Math.floor(bytes.byteLength / 2));
  for (let index = 0; index < peaks.length; index += 1) {
    peaks[index] = bytes.getInt16(index * 2, true) / 32_768;
  }
  return peaks;
}

/** Subscribes to whole-snapshot updates. */
export function onView(handler: (view: ViewModel) => void): Promise<() => void> {
  return listen<ViewModel>(VIEW_EVENT, (event) => handler(event.payload));
}

/** Subscribes to playback-only updates. */
export function onPlayback(handler: (tick: PlaybackTick) => void): Promise<() => void> {
  return listen<PlaybackTick>(PLAYBACK_EVENT, (event) => handler(event.payload));
}

/**
 * Builds the source for one artwork URL.
 *
 * The window never fetches artwork itself. Rust serves these bytes from its
 * private cache behind the same public-origin, no-redirect, size-capped guards
 * the terminal uses, so a provider sees Youta's agent rather than this web
 * view, and `file:` sources are refused on that path outright.
 */
export function artworkSource(url: string | null | undefined): string | null {
  return url ? `youta://artwork/${encodeURIComponent(url)}` : null;
}
