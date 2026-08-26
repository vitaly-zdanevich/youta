import type { ViewModel } from "../contract";
import { dispatch } from "../ipc";

/**
 * How many bytes Rust spends on one code point.
 *
 * The reducer reports the insertion point as a UTF-8 byte offset, because that
 * is what its own editing arithmetic uses. JavaScript indexes UTF-16 code
 * units, so the two disagree on every non-ASCII query.
 */
function utf8Length(codePoint: number): number {
  if (codePoint < 0x80) {
    return 1;
  }
  if (codePoint < 0x800) {
    return 2;
  }
  return codePoint < 0x10000 ? 3 : 4;
}

/** Splits a query where the reducer's UTF-8 byte cursor sits. */
export function splitAtByte(text: string, byte: number): [string, string] {
  let bytes = 0;
  let index = 0;
  for (const character of text) {
    if (bytes >= byte) {
      break;
    }
    bytes += utf8Length(character.codePointAt(0) ?? 0);
    index += character.length;
  }
  return [text.slice(0, index), text.slice(index)];
}

/**
 * The query editor for a screen that collects one.
 *
 * This is deliberately not an `<input>`. The reducer owns the query — it holds
 * the text, the insertion point, and the ordered modal precedence that
 * decides what a key means — and `App` forwards keys to it only while no text
 * field has focus. A real input would capture those keys, leaving this window
 * to reimplement editing that already exists in `src/app.rs`, and the two
 * copies would drift on the first Radio filter or Yandex scope. So the field
 * displays reducer state and asks for the editor by dispatching `BeginSearch`;
 * every keystroke after that travels the same path as in the terminal.
 */
export function SearchBar({
  view,
  verb,
  label,
}: {
  view: ViewModel;
  verb: string;
  label: string;
}) {
  const [before, after] = splitAtByte(view.search_query, view.search_cursor_byte);
  const placeholder = `${verb} ${label}`;

  return (
    <div
      role="search"
      aria-label={placeholder}
      className="flex items-center gap-[9px] border-b border-line bg-surface px-[13px] py-[7px]"
    >
      <span aria-hidden className="text-[13px] text-ink-faint">
        ⌕
      </span>
      <div
        // Focus must stay off this element: the window's key handler ignores
        // presses aimed at a focusable editor, so taking focus here would stop
        // the very typing this field is for.
        onMouseDown={(event) => {
          event.preventDefault();
          if (!view.search_editing) {
            void dispatch("BeginSearch");
          }
        }}
        title={view.search_editing ? undefined : placeholder}
        className={`min-w-0 flex-1 cursor-text truncate rounded-[5px] px-[9px] py-[3px] text-[13px] ${
          view.search_editing ? "bg-raised text-ink" : "text-ink-dim hover:bg-raised"
        }`}
      >
        {view.search_editing ? (
          <>
            {before}
            <span
              aria-hidden
              className="mx-px inline-block h-[15px] w-[2px] translate-y-[3px] animate-pulse bg-accent"
            />
            {after}
          </>
        ) : view.search_query === "" ? (
          <span className="text-ink-faint">{placeholder}</span>
        ) : (
          view.search_query
        )}
      </div>
      <span className="shrink-0 text-[11px] whitespace-nowrap text-ink-faint">
        {view.search_editing
          ? `Enter to ${verb.toLowerCase()} · Esc to cancel`
          : "Press /"}
      </span>
    </div>
  );
}
