// Naming DOM key events in the shared vocabulary.
//
// This file names keys. It does not decide what they do — that is
// `src/keymap.rs`, which serves the terminal too. A binding added here instead
// of there is a binding the terminal will not have.

import type { Key } from "./contract";

const NAMED: ReadonlyMap<string, Key> = new Map<string, Key>([
  ["Enter", "Enter"],
  ["Escape", "Esc"],
  ["Backspace", "Backspace"],
  ["Delete", "Delete"],
  ["Tab", "Tab"],
  ["ArrowLeft", "Left"],
  ["ArrowRight", "Right"],
  ["ArrowUp", "Up"],
  ["ArrowDown", "Down"],
  ["Home", "Home"],
  ["End", "End"],
  ["PageUp", "PageUp"],
  ["PageDown", "PageDown"],
]);

const FUNCTION_KEY = /^F([1-9]|1[0-9]|2[0-4])$/;

/** Translates a DOM key event into the shared `Key`, or null if unnamed. */
export function namedKey(event: KeyboardEvent): Key | null {
  const named = NAMED.get(event.key);
  if (named !== undefined) {
    // A terminal reports Shift+Tab as its own key; a browser does not.
    return named === "Tab" && event.shiftKey ? "BackTab" : named;
  }
  const functionKey = FUNCTION_KEY.exec(event.key);
  if (functionKey?.[1] !== undefined) {
    return { F: Number(functionKey[1]) };
  }
  // `key` is already resolved for layout and Shift, so a printable press is one
  // character. Anything longer names a key this vocabulary has no word for.
  return [...event.key].length === 1 ? { Char: event.key } : null;
}
