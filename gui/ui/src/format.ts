// Display formatting shared by the player and the list.

import type { RustDuration } from "./contract";

/** Reads a serde `Duration`. */
export function durationSeconds(value: RustDuration | null | undefined): number {
  if (!value) {
    return 0;
  }
  return value.secs + value.nanos / 1e9;
}

/** Formats a byte count exactly as `human_bytes` in `src/tui.rs` does. */
export function humanBytes(bytes: number): string {
  const KIB = 1024;
  const units: Array<[number, string]> = [
    [KIB ** 3, "GiB"],
    [KIB ** 2, "MiB"],
    [KIB, "KiB"],
  ];
  for (const [unit, suffix] of units) {
    if (bytes >= unit) {
      const tenths = Math.round((bytes * 10) / unit);
      return `${Math.floor(tenths / 10)}.${tenths % 10} ${suffix}`;
    }
  }
  return `${bytes} B`;
}

/** Formats a duration the way the player labels timestamps. */
export function formatSeconds(totalSeconds: number): string {
  const seconds = Math.max(0, Math.floor(totalSeconds));
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const rest = seconds % 60;
  const padded = `${String(minutes).padStart(hours > 0 ? 2 : 1, "0")}:${String(rest).padStart(2, "0")}`;
  return hours > 0 ? `${hours}:${padded}` : padded;
}
