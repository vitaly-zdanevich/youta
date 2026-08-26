import type { ReactNode } from "react";

import type { MediaId, RowView, SubscriptionsView } from "../contract";
import { dispatch } from "../ipc";
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
 * These lists are short — an OPML file of channels, and one page of a source's
 * recent media — so they are not virtualized. The main list is, because a
 * provider search is not bounded that way.
 */
function Pane({
  heading,
  rows,
  selected,
  focused,
  playing,
  onSelect,
  empty,
  children,
}: {
  heading: string;
  rows: RowView[];
  selected: number;
  focused: boolean;
  playing: MediaId | null;
  onSelect: (index: number) => void;
  empty: string;
  children?: ReactNode;
}) {
  return (
    <section className="grid min-h-0 grid-rows-[auto_minmax(0,1fr)_auto] border-r border-line">
      <h2
        className={`truncate px-4 pt-[10px] pb-[6px] text-[11px] tracking-wide uppercase ${
          focused ? "text-accent" : "text-ink-faint"
        }`}
        title={heading}
      >
        {heading}
      </h2>
      <div className="overflow-y-auto pb-2">
        {rows.length === 0 ? (
          <p className="px-4 text-xs text-ink-faint">{empty}</p>
        ) : (
          <ul className="grid list-none gap-[1px] px-2 pl-2">
            {rows.map((row, index) => (
              <li key={`${row.media_id?.external_id ?? row.title}-${index}`}>
                <button
                  type="button"
                  aria-current={index === selected}
                  onClick={() => onSelect(index)}
                  onDoubleClick={() => void dispatch("ActivateSelection")}
                  className={`grid w-full grid-cols-[14px_28px_minmax(0,1fr)] items-center gap-[8px] rounded-[5px] px-2 py-[5px] text-left focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-accent ${
                    index === selected ? "bg-raised" : ""
                  }`}
                >
                  <span className="text-[11px] text-accent">
                    {isPlaying(row, playing) ? "▶" : row.subscribed ? "◆" : ""}
                  </span>
                  <Artwork url={row.thumbnail_url} className="size-[28px] rounded object-cover" />
                  <span className="min-w-0">
                    <span
                      className={`block truncate ${index === selected ? "text-accent" : ""}`}
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
            ))}
          </ul>
        )}
      </div>
      <div className="flex gap-[6px] px-4 pb-[8px]">{children}</div>
    </section>
  );
}

/** One pane-footer control. */
function PaneButton({
  onClick,
  pressed,
  children,
}: {
  onClick: () => void;
  pressed?: boolean;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
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
          heading={itemsHeading(subscriptions)}
          rows={subscriptions.items}
          selected={subscriptions.selected_item}
          focused={subscriptions.focus === "Items"}
          playing={playing}
          onSelect={(index) => void dispatch({ SelectSubscriptionItem: index })}
          empty={
            subscriptions.loading ? `Loading ${noun}…` : `No ${noun} loaded for this source.`
          }
        >
          <PaneButton onClick={() => void dispatch("RefreshSubscriptionVideos")}>
            {subscriptions.loading ? `Refreshing ${noun}…` : `Refresh ${noun}`}
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
        </Pane>
      ) : null}

      {details}
    </main>
  );
}
