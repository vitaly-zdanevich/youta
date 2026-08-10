import type {
  DetailTimecodeView,
  DetailVideoLinkView,
  DetailWikidataEntityView,
  MediaId,
} from "../contract";
import { formatSeconds } from "../format";
import { dispatch } from "../ipc";
import { annotate } from "../spans";

/**
 * A description span, tagged so one pass can render both kinds.
 *
 * Timecodes and video links are separate lists in Rust but interleave in the
 * text, so they have to be merged before slicing — two passes would each cut
 * the string with the other's ranges still inside.
 */
type DescriptionSpan =
  | ({ kind: "timecode" } & DetailTimecodeView)
  | ({ kind: "video" } & DetailVideoLinkView);

/**
 * The description, with its timecodes and internal video links live.
 *
 * SECURITY: the text is untrusted provider content rendered as text nodes. The
 * clickable pieces are located by Rust and cut out by byte offset — see
 * `src/spans.tsx`. Nothing here parses or interprets the string.
 */
export function Description({
  text,
  timecodes,
  videoLinks,
  mediaId,
}: {
  text: string;
  timecodes: DetailTimecodeView[];
  videoLinks: DetailVideoLinkView[];
  mediaId: MediaId | null;
}) {
  if (text === "") {
    return null;
  }

  const spans: DescriptionSpan[] = [
    // A timecode is only actionable against the media it was parsed from; if
    // the selection has no identity there is nothing to seek within, so those
    // spans are dropped rather than rendered as buttons that cannot work.
    ...(mediaId === null
      ? []
      : timecodes.map((timecode): DescriptionSpan => ({ kind: "timecode", ...timecode }))),
    ...videoLinks.map((link): DescriptionSpan => ({ kind: "video", ...link })),
  ];

  return (
    <div className="mt-3 border-t border-line pt-[10px] text-xs leading-relaxed whitespace-pre-wrap text-ink-dim">
      {annotate(text, spans, (span, covered, key) => {
        if (span.kind === "timecode") {
          return (
            <button
              key={key}
              type="button"
              title={`Seek to ${formatSeconds(span.seconds)}`}
              onClick={() =>
                void dispatch({
                  // The identity captured when the span was rendered travels
                  // with the click, so the reducer can refuse a seek aimed at
                  // media the selection has since moved away from.
                  ActivateTimecode: { media_id: mediaId, seconds: span.seconds },
                })
              }
              className={`rounded-[3px] font-mono tabular-nums underline decoration-dotted underline-offset-2 hover:text-ink focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent ${
                span.is_chapter ? "text-accent" : "text-ink"
              }`}
            >
              {covered}
            </button>
          );
        }
        return (
          <span key={key}>
            <span className="text-ink-faint">{covered}</span>
            <button
              type="button"
              title="Open this video in Youta"
              onClick={() =>
                void dispatch({
                  ActivateDescriptionVideo: {
                    video_id: span.video_id,
                    start_seconds: span.start_seconds,
                  },
                })
              }
              className="ml-[2px] rounded-[3px] px-[2px] text-accent hover:brightness-125 focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent"
            >
              ↪
            </button>
          </span>
        );
      })}
    </div>
  );
}

/**
 * The expanded Wikidata spoiler for one link.
 *
 * The property text carries its own two span lists: statement values that open
 * a canonical page, and Commons media that Youta can play. Both are byte
 * offsets into this entity's text, never into the description.
 */
export function WikidataSpoiler({
  entity,
  selectedMedia,
}: {
  entity: DetailWikidataEntityView;
  selectedMedia: number | null;
}) {
  type WikidataSpan =
    | { kind: "value"; start_byte: number; end_byte: number; url: string }
    | { kind: "media"; start_byte: number; end_byte: number; index: number; title: string };

  const spans: WikidataSpan[] = [
    ...entity.value_links.map(
      (link): WikidataSpan => ({
        kind: "value",
        start_byte: link.start_byte,
        end_byte: link.end_byte,
        url: link.url,
      }),
    ),
    ...entity.media_controls.map(
      (media, index): WikidataSpan => ({
        kind: "media",
        start_byte: media.marker_start_byte,
        end_byte: media.marker_end_byte,
        index,
        title: media.title,
      }),
    ),
  ];

  return (
    <div className="mt-[6px] rounded-[6px] border border-line bg-raised px-[10px] py-[7px] text-[11px] leading-relaxed whitespace-pre-wrap text-ink-dim">
      {annotate(entity.text, spans, (span, covered, key) =>
        span.kind === "value" ? (
          <button
            key={key}
            type="button"
            title={span.url}
            onClick={() => void dispatch({ OpenWikidataValue: span.url })}
            className="rounded-[3px] text-ink underline decoration-dotted underline-offset-2 hover:text-accent focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent"
          >
            {covered}
          </button>
        ) : (
          <button
            key={key}
            type="button"
            title={span.title}
            onClick={() => void dispatch({ ActivateWikidataMedia: span.index })}
            className={`rounded-[3px] px-[1px] focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent ${
              span.index === selectedMedia ? "text-accent" : "text-ink-faint hover:text-ink"
            }`}
          >
            {covered}
          </button>
        ),
      )}
    </div>
  );
}
