import { useEffect, useRef, useState } from "react";

import type { AudioOutputView, Chapter, NowPlayingView, PlaybackStatus } from "../contract";
import { durationSeconds, formatSeconds } from "../format";
import { dispatch } from "../ipc";
import { Artwork } from "./Artwork";

// Steps match the terminal front-end exactly (src/keymap.rs): ±5 seconds,
// ±5 percent volume, ±0.1 speed. Two front-ends that step differently are two
// different players.
const SEEK_STEP_SECONDS = 5;
const VOLUME_STEP = 5;
const SPEED_STEP = 0.1;
const SCRUB_RESOLUTION = 1000;

/** Mirrors `PlaybackStatus::seeking_available` in src/playback/mod.rs. */
function seekingAvailable(playback: PlaybackStatus): boolean {
  return !playback.idle && (!playback.live || playback.live_seekable_range !== null);
}

/** Buffered spans painted behind the played fill. */
function Buffered({ playback, duration }: { playback: PlaybackStatus; duration: number }) {
  if (duration <= 0) {
    return null;
  }
  return (
    <div className="absolute inset-0 overflow-hidden rounded-[3px]">
      {playback.buffered_ranges.map((range, index) => {
        const start = durationSeconds(range.start);
        const end = durationSeconds(range.end);
        return (
          <span
            key={`${start}-${end}-${index}`}
            className="absolute top-0 bottom-0 bg-line-strong brightness-[1.7]"
            style={{
              left: `${(start / duration) * 100}%`,
              width: `${Math.max(0, ((end - start) / duration) * 100)}%`,
            }}
          />
        );
      })}
    </div>
  );
}

/** The chapter timeline, proportional to the playing item's duration. */
function ChapterTimeline({
  chapters,
  duration,
  active,
  showTimestamps,
}: {
  chapters: Chapter[];
  duration: number;
  active: number | null;
  showTimestamps: boolean;
}) {
  if (chapters.length === 0 || duration <= 0) {
    return null;
  }
  return (
    <div className="mt-[6px] flex h-[14px] gap-[2px]" aria-label="Chapters">
      {chapters.map((chapter, index) => {
        const next = chapters[index + 1];
        const end = chapter.end_seconds ?? next?.start_seconds ?? duration;
        const width = Math.max(0, ((end - chapter.start_seconds) / duration) * 100);
        return (
          <button
            key={`${chapter.start_seconds}-${chapter.title}`}
            type="button"
            title={chapter.title}
            onClick={() => void dispatch({ SeekPercent: (chapter.start_seconds / duration) * 100 })}
            style={{ width: `${width}%` }}
            className={`min-w-[3px] overflow-hidden rounded-[2px] px-1 text-left text-[9px] leading-[14px] whitespace-nowrap ${
              index === active ? "bg-accent text-ground" : "bg-raised text-ink-faint hover:text-ink"
            }`}
          >
            {showTimestamps
              ? `${formatSeconds(chapter.start_seconds)} ${chapter.title}`
              : chapter.title}
          </button>
        );
      })}
    </div>
  );
}

/** The transport bar. */
export function Player({
  playback,
  chapters,
  showChapterTimestamps,
  nyanCatSeekbar,
  captionLine,
  artwork,
  autoplay,
  repeating,
  radioNowPlaying,
  track,
  output,
}: {
  playback: PlaybackStatus;
  chapters: Chapter[];
  showChapterTimestamps: boolean;
  nyanCatSeekbar: boolean;
  captionLine: string | null;
  artwork: string | null;
  autoplay: boolean;
  repeating: boolean;
  radioNowPlaying: string | null;
  track: NowPlayingView | null;
  output: AudioOutputView | null;
}) {
  const position = durationSeconds(playback.position);
  const duration = durationSeconds(playback.duration);
  const seekable = duration > 0 && seekingAvailable(playback);
  const progress = duration > 0 ? Math.min(100, (position / duration) * 100) : 0;

  // A snapshot arriving mid-drag must not yank the scrubber out from under the
  // pointer, so the control is uncontrolled while the pointer is down.
  const [scrubbing, setScrubbing] = useState(false);
  const scrub = useRef<HTMLInputElement>(null);
  useEffect(() => {
    if (!scrubbing && scrub.current) {
      scrub.current.value = String(Math.round((progress / 100) * SCRUB_RESOLUTION));
    }
  }, [progress, scrubbing]);

  return (
    <footer className="border-t border-line bg-surface px-[18px] pt-[10px] pb-[11px]">
      <div className="flex gap-3">
        {playback.idle ? null : (
          <Artwork url={artwork} className="size-[46px] shrink-0 rounded-[5px] object-cover" />
        )}

        <div className="min-w-0 grow">
          {captionLine ? (
            <p className="mb-[8px] truncate text-center text-xs font-medium text-ink" title={captionLine}>
              CC&nbsp;&nbsp;{captionLine}
            </p>
          ) : null}
          <div className="relative h-[3px] rounded-[3px] bg-line-strong">
            <Buffered playback={playback} duration={duration} />
            <i
              className="absolute inset-y-0 left-0 rounded-[3px] bg-accent"
              style={{
                width: `${progress}%`,
                backgroundImage: nyanCatSeekbar
                  ? 'linear-gradient(90deg, #ef4444, #eab308, #22c55e, #06b6d4, #3b82f6, #d946ef)'
                  : undefined,
              }}
            />
            {nyanCatSeekbar && !playback.idle && duration > 0 ? (
              <span
                aria-hidden="true"
                className="pointer-events-none absolute -top-[9px] font-mono text-[9px] leading-none font-bold text-ink"
                style={{
                  left: `clamp(14px, ${progress}%, calc(100% - 14px))`,
                  transform: 'translateX(-50%)',
                }}
              >
                =^.^=
              </span>
            ) : null}
            <input
              ref={scrub}
              type="range"
              min={0}
              max={SCRUB_RESOLUTION}
              step={1}
              defaultValue={0}
              disabled={!seekable}
              aria-label="Playback position"
              onPointerDown={() => setScrubbing(true)}
              onPointerUp={() => setScrubbing(false)}
              onChange={(event) => {
                setScrubbing(false);
                void dispatch({
                  SeekPercent: (Number(event.currentTarget.value) / SCRUB_RESOLUTION) * 100,
                });
              }}
              className="absolute -top-[7px] left-0 h-[17px] w-full cursor-pointer appearance-none bg-transparent opacity-0 disabled:cursor-default"
            />
          </div>

          <div className="mt-[5px] flex justify-between gap-3 text-[11px] text-ink-faint">
            <span className="font-mono tabular-nums">{formatSeconds(position)}</span>
            {/* Clicking the title jumps the list to whatever is playing, which
                is the only way back to it after browsing elsewhere.

                The order is most-specific first. Radio metadata names the song
                a station is on right now, which the station name never does.
                `track` is Youta's own answer, taken from the queue entry, and
                it is the same text the window title, the tray, and a
                track-change notification carry. `playback.title` is last
                because it is whatever the engine parsed out of the stream. */}
            <button
              type="button"
              disabled={playback.idle}
              title={playback.idle ? undefined : "Show the playing item"}
              onClick={() => void dispatch("ShowNowPlaying")}
              className="min-w-0 truncate rounded-[3px] not-disabled:hover:text-ink focus-visible:outline-2 focus-visible:outline-offset-1 focus-visible:outline-accent"
            >
              {radioNowPlaying ??
                playback.stream_title ??
                (track === null
                  ? (playback.title ?? "")
                  : track.subtitle === ""
                    ? track.title
                    : `${track.title} — ${track.subtitle}`)}
            </button>
            <span className="font-mono tabular-nums">
              {duration > 0 ? formatSeconds(duration) : "--:--"}
            </span>
          </div>

          <ChapterTimeline
            chapters={chapters}
            duration={duration}
            active={playback.chapter}
            showTimestamps={showChapterTimestamps}
          />
        </div>
      </div>

      <div className="mt-[9px] flex flex-wrap items-center gap-[18px]">
        <div className="flex items-center gap-[6px]">
          {/* Moving between queue entries, which is what a media key and a
              tray menu mean by next and previous. The chapter pair below moves
              within one item and is a different thing entirely. */}
          <Control label="Previous track" disabled={playback.idle} onClick={() => void dispatch({ PlayQueueNeighbour: -1 })}>
            ⏮
          </Control>
          <Control label="Back 5 seconds" disabled={!seekable} onClick={() => void dispatch({ SeekRelative: -SEEK_STEP_SECONDS })}>
            ↶
          </Control>
          <button
            type="button"
            aria-label={playback.paused ? "Play" : "Pause"}
            disabled={playback.idle}
            onClick={() => void dispatch("TogglePause")}
            className="h-[26px] min-w-[32px] rounded-[5px] border border-accent bg-accent px-[7px] text-[13px] leading-none text-ground disabled:opacity-40 hover:brightness-110 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
          >
            {playback.paused ? "▶" : "⏸"}
          </button>
          <Control label="Forward 5 seconds" disabled={!seekable} onClick={() => void dispatch({ SeekRelative: SEEK_STEP_SECONDS })}>
            ↷
          </Control>
          <Control label="Next track" disabled={playback.idle} onClick={() => void dispatch({ PlayQueueNeighbour: 1 })}>
            ⏭
          </Control>
        </div>

        {chapters.length > 0 ? (
          <div className="flex items-center gap-[6px]">
            <Control label="Previous chapter (hotkey: [)" onClick={() => void dispatch({ ChangeChapter: -1 })}>
              |◀
            </Control>
            <Control label="Next chapter (hotkey: ])" onClick={() => void dispatch({ ChangeChapter: 1 })}>
              ▶|
            </Control>
          </div>
        ) : null}

        <div className="flex items-center gap-[6px]">
          <Control label="Slower" disabled={playback.idle || playback.speed <= 0.5} onClick={() => void dispatch({ ChangeSpeed: -SPEED_STEP })}>
            −
          </Control>
          <span className="min-w-[34px] text-center font-mono text-[11px] tabular-nums text-ink-faint">
            {playback.speed.toFixed(2)}×
          </span>
          <Control label="Faster" disabled={playback.idle || playback.speed >= 3} onClick={() => void dispatch({ ChangeSpeed: SPEED_STEP })}>
            +
          </Control>
        </div>

        <div className="flex items-center gap-[6px]">
          <Toggle pressed={repeating} onClick={() => void dispatch("ToggleRepeat")}>
            Repeat
          </Toggle>
          <Toggle pressed={autoplay} onClick={() => void dispatch("ToggleAutoplay")}>
            Autoplay
          </Toggle>
        </div>

        <div className="ml-auto flex items-center gap-[6px]">
          <Control label="Quieter" onClick={() => void dispatch({ ChangeVolume: -VOLUME_STEP })}>
            🔈
          </Control>
          <input
            type="range"
            min={0}
            max={100}
            step={1}
            value={playback.volume}
            aria-label="Volume"
            // `ChangeVolume` is a delta, so the slider sends the difference from
            // the volume the reducer last reported rather than a level.
            onChange={(event) => {
              const delta = Number(event.currentTarget.value) - playback.volume;
              if (delta !== 0) {
                void dispatch({ ChangeVolume: delta });
              }
            }}
            className="h-[3px] w-[88px] cursor-pointer appearance-none rounded-[3px] bg-line-strong [&::-webkit-slider-thumb]:size-[11px] [&::-webkit-slider-thumb]:appearance-none [&::-webkit-slider-thumb]:rounded-full [&::-webkit-slider-thumb]:bg-ink-dim"
          />
          <span className="min-w-[26px] text-center font-mono text-[11px] tabular-nums text-ink-faint">
            {playback.volume}
          </span>
        </div>

        {output ? (
          <div className="border-l border-line pl-[14px] text-[11px] whitespace-nowrap text-ink-faint">
            {[output.engine, output.driver, output.device].filter(Boolean).join(" · ")}
          </div>
        ) : null}
      </div>
    </footer>
  );
}

function Control({
  label,
  children,
  onClick,
  disabled = false,
}: {
  label: string;
  children: React.ReactNode;
  onClick: () => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      disabled={disabled}
      onClick={onClick}
      className="h-[26px] min-w-[26px] rounded-[5px] border border-line-strong px-[7px] text-xs leading-none text-ink-dim disabled:opacity-40 not-disabled:hover:border-ink-faint not-disabled:hover:text-ink focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
    >
      {children}
    </button>
  );
}

function Toggle({
  pressed,
  children,
  onClick,
}: {
  pressed: boolean;
  children: React.ReactNode;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      aria-pressed={pressed}
      onClick={onClick}
      className={`h-[26px] rounded-[5px] border px-[7px] text-xs leading-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent ${
        pressed
          ? "border-accent text-ink shadow-[inset_0_0_0_1px_var(--color-accent)]"
          : "border-line-strong text-ink-dim hover:border-ink-faint hover:text-ink"
      }`}
    >
      {children}
    </button>
  );
}
