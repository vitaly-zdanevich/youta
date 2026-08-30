import { useEffect, useRef } from "react";
import type { ReactNode } from "react";

import type { DetailLinkView, DetailView, InformationPanelKind, ViewModel } from "../contract";
import { dispatch } from "../ipc";
import { Artwork } from "./Artwork";
import { Description, WikidataSpoiler } from "./Description";

/**
 * Pixels the reducer's scroll counter steps by.
 *
 * The reducer counts wrapped *terminal* lines, which this window has none of, so
 * the counter is treated as an opaque step and multiplied by one line of body
 * text. Exactness is not needed: `End` arrives as `usize::MAX`, and the browser
 * clamps any oversized `scrollTop` to the bottom of the content by itself.
 */
const DETAILS_LINE_HEIGHT = 19;

/** Provider identity of YouTube, as `SourceKind` serializes it. */
const YOUTUBE = "you-tube";

/** Values a provider fills in with a placeholder rather than leaving empty. */
function fact(value: string | null | undefined): string | null {
  const trimmed = value?.trim() ?? "";
  return trimmed === "" || trimmed.toLowerCase() === "unknown" ? null : trimmed;
}

/** Formats a count the way the terminal panel abbreviates one. */
function count(value: number | null): string | null {
  if (value === null) {
    return null;
  }
  if (value >= 1_000_000) {
    return `${(value / 1_000_000).toFixed(1).replace(/\.0$/, "")}M`;
  }
  if (value >= 1_000) {
    return `${(value / 1_000).toFixed(1).replace(/\.0$/, "")}K`;
  }
  return String(value);
}

/**
 * Which facts each source exposes.
 *
 * The classification comes from Rust with the source list rather than being
 * restated here — see `Screen::details_kind`. What differs between sources is
 * which facts exist at all: a radio station has no like count and a local file
 * has no publication date, so one shared layout would either invent fields or
 * hide real ones.
 */
function factsFor(kind: InformationPanelKind, details: DetailView): Array<[string, string]> {
  const rows: Array<[string, string | null]> = [];
  switch (kind) {
    case "Video":
      rows.push(
        ["Length", fact(details.length)],
        ["Views", fact(details.views)],
        ["Likes", fact(details.likes)],
        ["Comments", fact(details.comments)],
        ["Published", fact(details.published)],
      );
      break;
    case "Podcast":
      rows.push(["Length", fact(details.length)], ["Published", fact(details.published)]);
      break;
    case "Channel":
      rows.push(
        ["Subscribers", count(details.channel_subscriber_count)],
        ["Joined", fact(details.channel_created)],
        ["Videos", count(details.channel_video_count)],
        ["Total views", count(details.channel_total_view_count)],
        ["Country", fact(details.channel_country)],
      );
      break;
    case "Local":
      rows.push(["Length", fact(details.length)]);
      break;
    // Radio, Yandex Music, and the aggregate screens deliberately show no
    // statistics: their providers report none, and a row of blanks reads as
    // missing data rather than as data that does not exist.
    case "Radio":
    case "YandexMusic":
    case "Generic":
      break;
  }
  rows.push(["Source", fact(details.source)], ["License", fact(details.license)]);
  if (details.playlist_names.length > 0) {
    rows.push(["Playlists", details.playlist_names.join(", ")]);
  }
  return rows.filter((row): row is [string, string] => row[1] !== null);
}

/** One Details action, mirroring a terminal hotkey. */
function Action({
  children,
  onClick,
  active = false,
}: {
  children: ReactNode;
  onClick: () => void;
  active?: boolean;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`rounded-[5px] border px-[8px] py-[3px] text-[11px] whitespace-nowrap focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent ${
        active
          ? "border-accent text-ink"
          : "border-line-strong text-ink-dim hover:border-ink-faint hover:text-ink"
      }`}
    >
      {children}
    </button>
  );
}

/** The external-link list, with its lazily expanded Wikidata spoilers. */
function Links({
  links,
  details,
  selectedLink,
  selectedMedia,
}: {
  links: DetailLinkView[];
  details: DetailView;
  selectedLink: number | null;
  selectedMedia: number | null;
}) {
  if (links.length === 0) {
    return null;
  }
  return (
    <ul className="mt-3 grid list-none gap-[5px] border-t border-line pt-[10px] pl-0 text-xs">
      {links.map((link, index) => {
        const expanded =
          link.wikidata_item_id !== null &&
          link.wikidata_item_id === details.expanded_wikidata_item;
        const loading =
          link.wikidata_item_id !== null && link.wikidata_item_id === details.loading_wikidata_item;
        const entity = expanded
          ? details.wikidata_entities.find((value) => value.item_id === link.wikidata_item_id)
          : undefined;
        const label = link.presentation.startsWith("Url") ? link.url : link.label || link.url;
        const showUrl = link.presentation.startsWith("LabelAndUrl") && link.label !== "";
        // Resolved outside the handler so the union stays narrowed: TypeScript
        // widens `link.internal_target` again inside a closure.
        const target = link.internal_target;
        const internal =
          target === null
            ? null
            : "YandexMusicArtist" in target
              ? { OpenYandexMusicArtistById: target.YandexMusicArtist }
              : { OpenYandexMusicAlbumById: target.YandexMusicAlbum };
        return (
          <li key={`${link.url}-${index}`}>
            <span className="flex flex-wrap items-baseline gap-x-[6px]">
              {link.prefix ? <span className="text-ink-faint">{link.prefix}</span> : null}
              <button
                type="button"
                title={link.url}
                onClick={() => void dispatch({ ActivateDetailLink: index })}
                onFocus={() => void dispatch({ SelectDetailLink: index })}
                className={`rounded-[3px] text-left underline decoration-dotted underline-offset-2 hover:text-accent focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent ${
                  index === selectedLink ? "text-accent" : "text-ink"
                }`}
              >
                {label}
              </button>
              {showUrl ? (
                <span className="min-w-0 truncate text-[11px] text-ink-faint">{link.url}</span>
              ) : null}
              {internal !== null ? (
                <button
                  type="button"
                  title="Open inside Youta"
                  onClick={() => void dispatch(internal)}
                  className="rounded-[3px] px-[2px] text-accent hover:brightness-125 focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent"
                >
                  ↪
                </button>
              ) : null}
              {link.wikidata_item_id !== null ? (
                <button
                  type="button"
                  aria-expanded={expanded}
                  onClick={() => void dispatch({ ToggleWikidataStatements: index })}
                  className="rounded-[3px] text-[11px] text-ink-faint hover:text-ink focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent"
                >
                  {loading ? "loading…" : expanded ? "▾ statements" : "▸ statements"}
                </button>
              ) : null}
            </span>
            {entity ? <WikidataSpoiler entity={entity} selectedMedia={selectedMedia} /> : null}
          </li>
        );
      })}
    </ul>
  );
}

/**
 * The Details panel.
 *
 * SECURITY: every string here originates from a provider. React escapes text
 * children, which is what makes that safe; rich content is built from Rust's
 * structured spans rather than from markup. Never reach for
 * `dangerouslySetInnerHTML` — the CSP is the second line of defence, not the
 * first.
 */
export function Details({ view, kind }: { view: ViewModel; kind: InformationPanelKind }) {
  const panel = useRef<HTMLElement>(null);
  // Last offset this window put on screen. Without it the panel would fight
  // itself: a native scroll reports an offset, the reducer publishes it back,
  // and the effect would scroll to it again mid-gesture.
  const applied = useRef<number | null>(null);

  // The reducer owns the offset because a key can move it — Home, End, PageDown
  // and Alt-u/d all scroll the focused panel — and only the reducer knows
  // whether the panel is focused at all.
  useEffect(() => {
    const element = panel.current;
    if (element === null || applied.current === view.details_scroll) {
      return;
    }
    applied.current = view.details_scroll;
    element.scrollTop = view.details_scroll * DETAILS_LINE_HEIGHT;
  }, [view.details_scroll, view.details]);

  const details = view.details;
  if (details === null) {
    return (
      <aside className="overflow-y-auto border-l border-line px-[18px] py-[14px]">
        <p className="text-xs text-ink-faint">Select an item to load details lazily.</p>
      </aside>
    );
  }

  const facts = factsFor(kind, details);
  const isYouTube = details.media_id?.source === YOUTUBE;
  const openable =
    view.external_opener_available &&
    (kind === "Video" || kind === "Podcast" || kind === "Radio" || kind === "YandexMusic");
  const yandex = view.yandex_music_actions;
  const audioQuality = fact(details.local_audio_quality_description);

  return (
    <aside
      ref={panel}
      className="overflow-y-auto border-l border-line px-[18px] py-[14px] select-text"
      aria-label="Details"
      // Clicking the panel gives it keyboard focus, as clicking it does in the
      // terminal. Doing this on hover instead would silently change what the
      // arrow keys mean as the pointer crossed the window.
      onClick={() => void dispatch({ SetDetailsFocus: true })}
      // Scrolling is native here — a web view has real momentum scrolling, and
      // selecting text has to be able to drag past the edge. The reducer is told
      // where the panel ended up so its counter and these pixels agree, and so
      // the terminal's Home/End keep working against the same number.
      onScroll={(event) => {
        const offset = Math.round(event.currentTarget.scrollTop / DETAILS_LINE_HEIGHT);
        if (offset !== applied.current) {
          applied.current = offset;
          void dispatch({ SetDetailsScroll: offset });
        }
      }}
    >
      <Artwork
        url={details.expanded_thumbnail_url ?? details.thumbnail_url}
        className="mb-3 block max-h-[220px] w-full rounded-md object-cover"
        onClick={() => void dispatch("ToggleThumbnailExpansion")}
      />

      <h2 className="mb-[2px] text-base leading-tight font-bold tracking-tight text-balance">
        {details.title}
      </h2>
      {fact(details.channel_name) ? (
        <p className="mb-[10px] text-xs text-ink-faint">{details.channel_name}</p>
      ) : null}

      {facts.length > 0 ? (
        <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-[3px] text-xs">
          {facts.map(([label, value]) => (
            <div key={label} className="contents">
              <dt className="text-ink-faint">{label}</dt>
              <dd className="m-0">{value}</dd>
            </div>
          ))}
        </dl>
      ) : null}
      {kind === "Channel" && details.channel_links_truncated ? (
        <p className="mt-[6px] text-[11px] text-ink-faint">
          Some channel links were omitted by safety limits.
        </p>
      ) : null}

      <div className="mt-3 flex flex-wrap gap-[6px]">
        {details.channel_id !== "" ? (
          <Action
            active={details.channel_subscribed}
            onClick={() => void dispatch("ToggleSubscription")}
          >
            {details.channel_subscribed ? "Unsubscribe" : "Subscribe"}
          </Action>
        ) : null}
        {view.playlist_item !== null ? (
          <>
            <Action
              active={view.playlist_item.in_todo}
              onClick={() => void dispatch("ToggleTodoPlaylist")}
            >
              To-do
            </Action>
            <Action onClick={() => void dispatch("OpenPlaylistPopup")}>Playlist…</Action>
          </>
        ) : null}
        {view.screen === "Playlists" && view.playlist_edit_available ? (
          <Action onClick={() => void dispatch("EditSelectedPlaylist")}>Edit playlist</Action>
        ) : null}
        {view.private_note_available ? (
          <Action active={details.has_private_note} onClick={() => void dispatch("EditPrivateNote")}>
            {details.has_private_note ? "Edit note" : "Add note"}
          </Action>
        ) : null}
        {kind === "Radio" ? (
          <Action active={details.radio_favorite} onClick={() => void dispatch("ToggleRadioFavorite")}>
            Favorite
          </Action>
        ) : null}
        {kind === "Video" && view.video_comments_available && isYouTube ? (
          <Action onClick={() => void dispatch("OpenVideoComments")}>Comments</Action>
        ) : null}
        {kind === "Video" && view.video_summary_available && isYouTube ? (
          <Action onClick={() => void dispatch("GenerateVideoSummary")}>Summarize</Action>
        ) : null}
        {kind === "Video" && isYouTube ? (
          <Action onClick={() => void dispatch("OpenVideoQr")}>QR</Action>
        ) : null}
        {openable ? (
          <Action onClick={() => void dispatch("OpenInBrowser")}>
            {kind === "Radio" ? "Open homepage" : "Open page"}
          </Action>
        ) : null}
        {view.external_opener_available &&
        (kind === "Video" || kind === "Channel") &&
        details.channel_webpage_url !== null ? (
          <Action onClick={() => void dispatch("OpenChannelInBrowser")}>Open channel</Action>
        ) : null}
        {details.media_id !== null ? (
          <Action onClick={() => void dispatch("CopyLink")}>Copy link</Action>
        ) : null}
        {kind !== "Local" && details.media_id !== null ? (
          <Action onClick={() => void dispatch("Download")}>Download</Action>
        ) : null}
        {view.commons_upload_available ? (
          <Action onClick={() => void dispatch("OpenCommonsUpload")}>Upload to Commons</Action>
        ) : null}
        {view.screen === "Downloaded" ? (
          <Action onClick={() => void dispatch("RequestDownloadedTrash")}>Move to Trash</Action>
        ) : null}
        {details.local_renamable ? (
          <Action onClick={() => void dispatch("BeginLocalRename")}>Rename</Action>
        ) : null}
        {details.local_movable ? (
          <Action onClick={() => void dispatch("BeginLocalMove")}>Move…</Action>
        ) : null}
        {details.local_trashable ? (
          <Action onClick={() => void dispatch("RequestLocalTrash")}>Trash</Action>
        ) : null}
        {details.local_fingerprint_available ? (
          <Action onClick={() => void dispatch("FingerprintLocalAudio")}>
            {details.local_fingerprint_pending ? "Identifying…" : "Identify"}
          </Action>
        ) : null}
        {view.audio_quality_supported &&
        kind === "Local" &&
        details.local_audio_quality_available ? (
          <Action
            active={details.local_audio_quality_pending}
            onClick={() => void dispatch("AnalyzeLocalAudioQuality")}
          >
            {details.local_audio_quality_pending ? "Cancel analysis" : "Analyze quality"}
          </Action>
        ) : null}
        {kind === "YandexMusic" && yandex.track_selected ? (
          <>
            <Action
              active={yandex.reaction === "Liked"}
              onClick={() => void dispatch("ToggleYandexMusicLike")}
            >
              Like
            </Action>
            <Action
              active={yandex.reaction === "Disliked"}
              onClick={() => void dispatch("ToggleYandexMusicDislike")}
            >
              Dislike
            </Action>
          </>
        ) : null}
        {kind === "YandexMusic" && yandex.artist_available ? (
          <Action onClick={() => void dispatch("OpenYandexMusicArtist")}>Artist</Action>
        ) : null}
        {kind === "YandexMusic" && yandex.album_available ? (
          <Action onClick={() => void dispatch("OpenYandexMusicAlbum")}>Album</Action>
        ) : null}
        {kind === "YandexMusic" && yandex.album_open ? (
          <Action onClick={() => void dispatch("DownloadYandexMusicAlbum")}>Download album</Action>
        ) : null}
        {kind === "YandexMusic" && yandex.twenty_recommendations_available ? (
          <Action onClick={() => void dispatch("DownloadTwentyYandexMusicRecommendations")}>
            Download 20
          </Action>
        ) : null}
      </div>

      <Description
        text={details.description}
        timecodes={details.timecodes}
        videoLinks={details.video_links}
        mediaId={details.media_id}
      />

      {kind === "Local" && audioQuality !== null ? (
        <section
          aria-label="Audio quality analysis"
          className="mt-3 border-t border-line pt-[10px] text-xs"
        >
          <h3 className="mb-[4px] text-[11px] tracking-wide text-ink-faint uppercase">
            Audio quality analysis
          </h3>
          <p className="m-0 whitespace-pre-wrap text-ink-dim">{audioQuality}</p>
        </section>
      ) : null}

      {fact(details.wikidata) && details.links.length === 0 ? (
        <p className="mt-3 border-t border-line pt-[10px] text-xs text-ink-faint">
          {details.wikidata}
        </p>
      ) : null}

      <Links
        links={details.links}
        details={details}
        selectedLink={view.selected_detail_link}
        selectedMedia={view.selected_wikidata_media}
      />
    </aside>
  );
}
