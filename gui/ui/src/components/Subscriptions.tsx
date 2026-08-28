import { useEffect, useRef, type ReactNode } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";

import type { MediaId, RowView, SubscriptionPane, SubscriptionsView } from "../contract";
import { dispatch } from "../ipc";
import { SUBSCRIPTION_ROW_HEIGHT } from "../subscriptionPageRows";
import { Artwork } from "./Artwork";

/** Abbreviates a count the way the terminal heading does. */
function formatCount(value: number): string {
  if (value >= 1_000_000) {
    return `${(value / 1_000_000).toFixed(1).replace(/\.0$/, "")}M`;
  }
  if (value >= 1_000) {
    return `${(value / 1_000).toFixed(1).replace(/\.0$/, "")}K`;
  }
  return String(value);
}

/** What one source's media are called, by provider family. */
function itemNoun(subscriptions: SubscriptionsView): string {
  return subscriptions.source_kind === "rss" ? "episodes" : "videos";
}

/**
 * The item-list heading, mirroring `subscription_items_heading` in `src/tui.rs`.
 *
 * The wording is restated here rather than shared because it is a label, not a
 * rule: getting it wrong shows the wrong words, not the wrong data.
 */
function itemsHeading(subscriptions: SubscriptionsView): string {
  const provider =
    subscriptions.source_kind === "rss"
      ? "RSS/Atom"
      : subscriptions.source_kind === "you-tube"
        ? "YouTube"
        : "Subscription";
  let heading = subscriptions.source_title
    ? `${subscriptions.source_title} · ${provider}`
    : provider;
  if (subscriptions.source_kind === "you-tube" && subscriptions.source_subscriber_count !== null) {
    heading += ` · ${formatCount(subscriptions.source_subscriber_count)} subscribers`;
  }
  if (subscriptions.source_kind === "you-tube" && subscriptions.source_created !== "") {
    heading += ` · created ${subscriptions.source_created}`;
  }
  return heading;
}

/** Whether a row is the authoritative playing item. */
function isPlaying(row: RowView, playing: MediaId | null): boolean {
  return (
    playing !== null &&
    row.media_id !== null &&
    row.media_id.source === playing.source &&
    row.media_id.external_id === playing.external_id
  );
}

/**
 * One Subscriptions pane.
 *
 * Source and item lists can both contain thousands of rows, so each pane owns
 * an independent virtualizer. The focused pane also exposes its exact scrolling
 * viewport to `App`, which turns Page Up and Page Down into rendered-page moves.
 */
function Pane({
  pane,
  sourceOwner,
  heading,
  rows,
  selected,
  focused,
  playing,
  onSelect,
  empty,
  children,
}: {
  pane: SubscriptionPane;
  /** Selected source whose continuation and viewport this pane represents. */
  sourceOwner: number;
  heading: string;
  rows: RowView[];
  selected: number;
  focused: boolean;
  playing: MediaId | null;
  onSelect: (index: number) => void;
  empty: string;
  children?: ReactNode;
}) {
	const parentRef = useRef<HTMLDivElement>(null);
	const followedSelection = useRef<string | null>(null);
	const reportedSourceOwner = useRef<number | null>(null);
	const lastReportedViewportEnd = useRef<number | null>(null);
	const viewportFocusActive = useRef(false);
	const reportViewportEnd = (finalIndex: number, force = false) => {
		if (
			pane !== 'Items'
			|| !focused
			|| (!force && finalIndex === lastReportedViewportEnd.current)
		) {
			return;
		}
		lastReportedViewportEnd.current = finalIndex;
		void dispatch({ PrefetchSubscriptionVideosThrough: finalIndex });
	};
	const virtualizer = useVirtualizer({
		count: rows.length,
		getScrollElement: () => parentRef.current,
		estimateSize: () => SUBSCRIPTION_ROW_HEIGHT,
		overscan: 8,
		onChange: (instance) => {
			const finalRow = instance.getVirtualItems().at(-1);
			if (finalRow !== undefined) {
				reportViewportEnd(finalRow.index);
			}
		},
	});
	const selectedRow = rows[selected];
	const selectedIdentity = selectedRow?.media_id
		? `${selectedRow.media_id.source}\0${selectedRow.media_id.external_id}`
		: `${selectedRow?.source ?? ''}\0${selectedRow?.title ?? ''}`;
	const selectionIdentity = `${selected}\0${selectedIdentity}`;

	// Keyboard navigation changes reducer state rather than DOM focus. Keep the
	// corresponding virtual row mounted and visible as that state moves. A pane
	// gaining pointer focus alone must preserve the user's manual scroll offset.
	useEffect(() => {
		const selectionChanged = followedSelection.current !== selectionIdentity;
		followedSelection.current = selectionIdentity;
		if (focused && selectionChanged && selected < rows.length) {
			virtualizer.scrollToIndex(selected, { align: 'auto' });
		}
	}, [selected, selectedIdentity, virtualizer]);

	const focusPane = (event?: { target: EventTarget | null }) => {
		const target = event?.target;
		if (
			target instanceof Element
			&& target.closest('[data-pane-focus-after-command]') !== null
		) {
			return;
		}
		if (!focused) {
			void dispatch({ FocusSubscriptionPane: pane });
		}
	};

	const reportViewport = (force = false) => {
		const finalRow = virtualizer.getVirtualItems().at(-1);
		if (finalRow !== undefined) {
			reportViewportEnd(finalRow.index, force);
		}
	};

	// A new source owns a distinct continuation even when its title, row count,
	// and old row count all match the preceding source. Reset the Items pane to
	// its reducer-selected row even while Sources owns focus, so Page Up cannot
	// inherit another channel's scroll offset.
	useEffect(() => {
		if (reportedSourceOwner.current === sourceOwner) {
			return;
		}
		reportedSourceOwner.current = sourceOwner;
		lastReportedViewportEnd.current = null;
		if (pane === 'Items') {
			followedSelection.current = selectionIdentity;
			if (selected < rows.length) {
				virtualizer.scrollToIndex(selected, { align: 'start' });
			} else {
				virtualizer.scrollToOffset(0);
			}
		}
	}, [pane, rows.length, selected, selectionIdentity, sourceOwner, virtualizer]);

	// An inactive split-view Items pane must not spend provider quota. Once it
	// gains focus, report the already-rendered range without requiring a scroll.
	useEffect(() => {
		if (focused && !viewportFocusActive.current) {
			reportViewport(true);
		}
		viewportFocusActive.current = focused;
	}, [focused, sourceOwner, virtualizer]);

	// A newly mounted or extended list may already fill the whole viewport
	// without producing a scroll event. Report it so the following screenful is
	// ready even on tall windows and after an automatic page append.
	useEffect(() => {
		reportViewport();
	}, [rows.length, virtualizer]);

  return (
    <section
		onPointerDown={focusPane}
		onFocusCapture={focusPane}
		className="grid min-h-0 grid-rows-[auto_minmax(0,1fr)_auto] border-r border-line"
	>
      <h2
        className={`truncate px-4 pt-[10px] pb-[6px] text-[11px] tracking-wide uppercase ${
          focused ? "text-accent" : "text-ink-faint"
        }`}
        title={heading}
      >
        {heading}
      </h2>
		<div
			ref={parentRef}
			data-subscription-pane={focused ? 'focused' : 'inactive'}
			className="overflow-y-auto"
			role="region"
			aria-label={heading}
			onWheel={(event) => {
				focusPane();
				const scrollPane = parentRef.current;
				if (
					event.deltaY > 0
					&& scrollPane !== null
					&& scrollPane.scrollTop + scrollPane.clientHeight >= scrollPane.scrollHeight - 1
				) {
					reportViewport(true);
				}
			}}
			onScroll={() => reportViewport()}
		>
        {rows.length === 0 ? (
          <p className="px-4 text-xs text-ink-faint">{empty}</p>
        ) : (
			<ul
				className="relative list-none px-2 pl-2"
				style={{ height: virtualizer.getTotalSize() }}
			>
				{virtualizer.getVirtualItems().map((item) => {
					const row = rows[item.index];
					if (row === undefined) {
						return null;
					}
					const current = item.index === selected;
					return (
						<li
							key={item.key}
							aria-posinset={item.index + 1}
							aria-setsize={rows.length}
							className="absolute top-0 left-2 right-2"
							style={{ height: item.size, transform: `translateY(${item.start}px)` }}
						>
                <button
                  type="button"
							aria-current={current}
							onClick={() => onSelect(item.index)}
                  onDoubleClick={() => void dispatch("ActivateSelection")}
							className={`grid h-full w-full grid-cols-[14px_28px_minmax(0,1fr)] items-center gap-[8px] rounded-[5px] px-2 text-left focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-accent ${
								current ? "bg-raised" : ""
                  }`}
                >
                  <span className="text-[11px] text-accent">
                    {isPlaying(row, playing) ? "▶" : row.subscribed ? "◆" : ""}
                  </span>
                  <Artwork url={row.thumbnail_url} className="size-[28px] rounded object-cover" />
                  <span className="min-w-0">
                    <span
										className={`block truncate ${current ? "text-accent" : ""}`}
                    >
                      {row.title}
                    </span>
                    {row.subtitle ? (
                      <span className="block truncate text-[11px] text-ink-faint">
                        {row.subtitle}
                      </span>
                    ) : null}
                  </span>
                </button>
						</li>
					);
				})}
          </ul>
        )}
      </div>
      <div className="flex gap-[6px] px-4 pb-[8px]">{children}</div>
    </section>
  );
}

/**
 * One pane-footer control.
 *
 * A command that takes reducer focus after it starts can opt out of the pane's
 * earlier DOM focus event. This lets Refresh queue page one before Items owns
 * PageDown, without changing focus semantics for the other footer controls.
 */
function PaneButton({
	onClick,
	pressed,
	focusAfterCommand = false,
	children,
}: {
	onClick: () => void;
	pressed?: boolean;
	focusAfterCommand?: boolean;
	children: ReactNode;
}) {
	return (
		<button
			type="button"
			data-pane-focus-after-command={focusAfterCommand || undefined}
			onClick={onClick}
			aria-pressed={pressed}
			className="rounded-[5px] border border-line-strong px-[8px] py-[3px] text-[11px] whitespace-nowrap text-ink-dim hover:border-ink-faint hover:text-ink focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
		>
			{children}
		</button>
	);
}

/**
 * The Subscriptions screen.
 *
 * The reducer offers two navigation models and the window honours both, because
 * which one is active is a saved preference the terminal shares. In drill-down
 * the sources pane hands over to an item pane; in split both stay visible and
 * `focus` says which one the shared keyboard map is steering.
 *
 * The right-hand panel is the window's own arrangement rather than a copy of the
 * terminal's: a desktop window has room for source list, item list, and the
 * information panel at once, where four terminal rows do not. What it shows
 * still follows the reducer's state — the channel while a source is being
 * chosen, the selected item once one has been.
 */
export function Subscriptions({
  subscriptions,
  playing,
  details,
}: {
  subscriptions: SubscriptionsView;
  playing: MediaId | null;
  details: ReactNode;
}) {
  const drillingIntoItems =
    subscriptions.layout === "drill-down" && subscriptions.route === "Items";
  const showSources = !drillingIntoItems;
  const showItems = subscriptions.layout === "split" || drillingIntoItems;
  const noun = itemNoun(subscriptions);
	const refreshing = subscriptions.loading && !subscriptions.loading_more;

  return (
    <main
      className="grid min-h-0"
      style={{
        gridTemplateColumns: `${showSources ? "minmax(0,0.8fr) " : ""}${
          showItems ? "minmax(0,1fr) " : ""
        }minmax(0,1.1fr)`,
      }}
    >
      {showSources ? (
        <Pane
          pane="Sources"
          sourceOwner={0}
          heading="Subscriptions"
          rows={subscriptions.sources}
          selected={subscriptions.selected_source}
          focused={subscriptions.focus === "Sources"}
          playing={playing}
          onSelect={(index) => void dispatch({ SelectSubscriptionSource: index })}
          empty="No subscriptions yet. Subscribe from a channel's Details panel."
        >
          {/* Adding a feed opens a terminal-only editor: the URL may itself be a
              credential, so the window can ask for it to be opened but never
              draws it. */}
          <PaneButton onClick={() => void dispatch("OpenRssSubscriptionPopup")}>
            Add RSS feed…
          </PaneButton>
        </Pane>
      ) : null}

      {showItems ? (
        <Pane
          pane="Items"
          sourceOwner={subscriptions.source_generation}
          heading={itemsHeading(subscriptions)}
          rows={subscriptions.items}
          selected={subscriptions.selected_item}
          focused={subscriptions.focus === "Items"}
          playing={playing}
          onSelect={(index) => void dispatch({ SelectSubscriptionItem: index })}
          empty={
			refreshing ? `Loading ${noun}…` : `No ${noun} loaded for this source.`
          }
        >
		<PaneButton
			focusAfterCommand
			onClick={() => void dispatch("RefreshSubscriptionVideos")}
		>
			{refreshing ? `Refreshing ${noun}…` : `Refresh ${noun}`}
		</PaneButton>
          {subscriptions.source_kind === "you-tube" ? (
            <PaneButton
              pressed={subscriptions.show_youtube_shorts}
              onClick={() => void dispatch("ToggleSubscriptionShorts")}
            >
              Shorts: {subscriptions.show_youtube_shorts ? "on" : "off"}
            </PaneButton>
          ) : null}
          {subscriptions.items.length > 0 ? (
            <PaneButton onClick={() => void dispatch("ToggleSubscriptionDescription")}>
              {subscriptions.description_expanded ? `Back to ${noun}` : "Details"}
            </PaneButton>
          ) : null}
			{subscriptions.loading_more ? (
				<span role='status' className="self-center text-[11px] whitespace-nowrap text-ink-faint">
					Loading more…
				</span>
			) : null}
        </Pane>
      ) : null}

      {details}
    </main>
  );
}
