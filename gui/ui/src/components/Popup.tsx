import * as Dialog from "@radix-ui/react-dialog";
import type { ReactNode } from "react";

/**
 * The shell every popup is drawn in.
 *
 * The reducer is the single source of truth for whether a popup is open, so
 * every path by which Radix would close one itself is disabled: Escape,
 * outside clicks, and focus loss all reach the reducer as ordinary keys or are
 * simply ignored. Letting the library close a popup would leave the window
 * hiding a modal the reducer still considers open — and while it is open, the
 * shared keyboard map routes every key into it.
 *
 * `modal={false}` for the same reason. Youta stacks popups (an error report can
 * appear over Preferences), and Radix's modal mode would fight itself over
 * `aria-hidden` and the focus trap when two are mounted. Focus is not how this
 * window receives keys: a single `keydown` listener on the document is, so a
 * trap would add nothing and could take input away.
 */
export function Popup({
  title,
  subtitle,
  width = "760px",
  layer,
  onDismiss,
  dismissDisabled = false,
  footer,
  children,
}: {
  title: string;
  subtitle?: string | undefined;
  width?: string;
  /**
   * Stacking position, matching the order `render_frame` draws these in.
   *
   * Portals mount in React's order, but relying on that would make the stack a
   * property of where a component happens to sit in the tree. The terminal has
   * one explicit order; so does this.
   */
  layer: number;
  onDismiss: () => void;
  /** Prevents closing while an irreversible background action is unresolved. */
  dismissDisabled?: boolean;
  footer?: ReactNode;
  children: ReactNode;
}) {
  return (
    <Dialog.Root open modal={false}>
      <Dialog.Portal>
        <div
          className="fixed inset-0 grid place-items-center bg-black/55 p-6"
          style={{ zIndex: 50 + layer }}
        >
          <Dialog.Content
            onOpenAutoFocus={(event) => event.preventDefault()}
            onEscapeKeyDown={(event) => event.preventDefault()}
            onPointerDownOutside={(event) => event.preventDefault()}
            onInteractOutside={(event) => event.preventDefault()}
            aria-describedby={undefined}
            style={{ width, maxWidth: "100%" }}
            className="grid max-h-full grid-rows-[auto_minmax(0,1fr)_auto] overflow-hidden rounded-[10px] border border-line-strong bg-surface shadow-[0_18px_48px_rgba(0,0,0,0.55)]"
          >
            <header className="flex items-baseline gap-3 border-b border-line px-[18px] py-[11px]">
              <Dialog.Title className="text-[13px] font-semibold tracking-tight">
                {title}
              </Dialog.Title>
              {subtitle ? (
                <p className="min-w-0 grow truncate text-[11px] text-ink-faint">{subtitle}</p>
              ) : null}
              <button
                type="button"
                aria-label="Close"
                disabled={dismissDisabled}
                onClick={onDismiss}
                className="ml-auto shrink-0 rounded-[5px] border border-line-strong px-[7px] py-[2px] text-[11px] text-ink-dim disabled:cursor-not-allowed disabled:opacity-40 not-disabled:hover:border-ink-faint not-disabled:hover:text-ink focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
              >
                Esc
              </button>
            </header>

            <div className="min-h-0 overflow-hidden">{children}</div>

            {footer ? (
              <footer className="flex flex-wrap items-center gap-[8px] border-t border-line px-[18px] py-[9px] text-[11px] text-ink-faint">
                {footer}
              </footer>
            ) : null}
          </Dialog.Content>
        </div>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

/** A popup control. Popups use text buttons rather than the player's icons. */
export function PopupButton({
  children,
  onClick,
  emphasis = false,
  disabled = false,
}: {
  children: ReactNode;
  onClick: () => void;
  emphasis?: boolean;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      disabled={disabled}
      onClick={onClick}
      className={`rounded-[5px] border px-[9px] py-[3px] text-[11px] disabled:opacity-40 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent ${
        emphasis
          ? "border-accent text-ink"
          : "border-line-strong text-ink-dim not-disabled:hover:border-ink-faint not-disabled:hover:text-ink"
      }`}
    >
      {children}
    </button>
  );
}

/** The error line a popup keeps instead of closing. */
export function PopupError({ message }: { message: string | null }) {
  if (message === null || message === "") {
    return null;
  }
  return (
    <p role="alert" className="px-[18px] py-[8px] text-[11px] text-accent">
      {message}
    </p>
  );
}
