import type { ScreenEntry } from "../contract";
import { dispatch } from "../ipc";

/**
 * The source tabs.
 *
 * The catalogue comes from Rust so a feature-trimmed build never offers a tab
 * it cannot serve, and so this window does not restate the source list.
 */
export function Tabs({ sources, active }: { sources: ScreenEntry[]; active: string }) {
  return (
    <nav
      className="flex gap-[3px] overflow-x-auto border-b border-line bg-surface px-[13px] py-[9px]"
      aria-label="Sources"
    >
      {sources.map((source) => {
        const selected = source.id === active;
        return (
          <button
            key={source.id}
            type="button"
            aria-selected={selected}
            onClick={() => void dispatch({ ShowScreen: source.id })}
            className={`shrink-0 rounded-[5px] px-[11px] py-[5px] text-xs whitespace-nowrap transition-colors focus-visible:outline-2 focus-visible:outline-accent ${
              selected
                ? "bg-raised font-semibold text-ink shadow-[inset_0_-2px_0_var(--color-accent)]"
                : "text-ink-faint hover:text-ink"
            }`}
          >
            {source.label}
          </button>
        );
      })}
    </nav>
  );
}
