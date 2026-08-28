import { useCallback, useEffect, useRef } from "react";

import type { InformationPanelKind } from "./contract";
import { Details } from "./components/Details";
import { DownloadBar } from "./components/DownloadBar";
import { Player } from "./components/Player";
import { ROW_HEIGHT, RowList } from "./components/RowList";
import { SearchBar } from "./components/SearchBar";
import { Subscriptions } from "./components/Subscriptions";
import { Tabs } from "./components/Tabs";
import { Waveform } from "./components/Waveform";
import {
  AudioQualityPopup,
  CredentialEditorNotice,
  ErrorPopup,
  HelpPopup,
  LocalFilePopup,
  PlaylistPopup,
  PreferencesPopup,
  ProjectHistoryPopup,
  QueuePopup,
  VideoCommentsPopup,
  VideoQrPopup,
} from "./components/popups";
import { namedKey } from "./keys";
import { sendKey } from "./ipc";
import { popupGeometry } from "./popupGeometry";
import { subscriptionPageRows } from "./subscriptionPageRows";
import { useYouta } from "./useYouta";

/** Whether the user currently has text selected anywhere in the window. */
function hasSelection(): boolean {
  return (document.getSelection()?.toString().length ?? 0) > 0;
}

export function App() {
  const { view, sources, output, failure } = useYouta();
  const body = useRef<HTMLDivElement>(null);

  /** Reports how many list rows are on screen, for page-sized movement. */
  const pageRows = useCallback((): number | null => {
	const root = body.current;
	if (root?.hasAttribute('data-subscriptions-screen')) {
		const focusedPane = root.querySelector<HTMLElement>("[data-subscription-pane='focused']");
		const height = focusedPane?.clientHeight ?? 0;
		return subscriptionPageRows(height);
	}
	const height = root?.clientHeight ?? 0;
    return height > 0 ? Math.max(1, Math.floor(height / ROW_HEIGHT)) : null;
  }, []);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      // A text field or slider owns its own keys.
      const target = event.target;
      if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) {
        return;
      }
      // The platform keeps its window and application chords.
      if (event.metaKey) {
        return;
      }
      // Copying a selection is the web view's job, and on Windows and Linux the
      // chord for it is Ctrl-C — which Youta otherwise binds. Selecting Details
      // text and then losing it to a reducer action would make the panel's one
      // desktop-native affordance useless.
      if (event.ctrlKey && (event.key === "c" || event.key === "C") && hasSelection()) {
        return;
      }
      const key = namedKey(event);
      if (key === null) {
        return;
      }
      // Youta binds bare letters, so the browser's own defaults must not fire.
      event.preventDefault();
      void sendKey(
        { key, ctrl: event.ctrlKey, alt: event.altKey, shift: event.shiftKey },
        pageRows(),
        // Paging inside the scrollable popups is resolved against what this
        // window actually rendered. Sending nothing would make the reducer fall
        // back to a guessed page height and an unbounded End.
        popupGeometry(),
      );
    };

    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [pageRows]);

  if (failure !== null) {
    return (
      <div className="grid h-full place-items-center p-8 text-center text-sm text-ink-dim">
        Youta could not start its window: {failure}
      </div>
    );
  }

  if (view === null) {
    return <div className="grid h-full place-items-center text-xs text-ink-faint">Starting…</div>;
  }

  const visibleSources = view.playback_history_enabled
    ? sources
    : sources.filter((source) => source.id !== "History");
  const onSubscriptions = view.screen === "Subscriptions";
  // While a subscription *source* is being chosen, the panel is describing a
  // channel or a feed rather than a media item, and those expose an entirely
  // different set of facts — subscribers and join date instead of views and
  // length. Once a source has been entered, the panel is back to its screen's
  // ordinary layout.
  const showingChannel =
    onSubscriptions &&
    (view.subscriptions.layout === "drill-down"
      ? view.subscriptions.route === "Sources"
      : !view.subscriptions.description_expanded);
  const active = visibleSources.find((source) => source.id === view.screen);
  const kind: InformationPanelKind = showingChannel
    ? "Channel"
    : (active?.details_kind ?? "Generic");
  const details = <Details view={view} kind={kind} />;

  return (
    <>
      <div className="grid h-full grid-rows-[auto_auto_minmax(0,1fr)_auto_auto_auto_auto]">
        <Tabs sources={visibleSources} active={view.screen} />

        {/* Only where the reducer accepts a query; the catalogue says which
            screens those are, so this window never offers a field whose Enter
            would answer "search is not available on this screen". */}
        {active?.search_verb == null ? (
          <span />
        ) : (
          <SearchBar view={view} verb={active.search_verb} label={active.label} />
        )}

        {onSubscriptions ? (
		<div ref={body} data-subscriptions-screen className="grid min-h-0">
            <Subscriptions
              subscriptions={view.subscriptions}
              playing={view.playing_media_id}
              details={details}
            />
          </div>
        ) : (
          <main ref={body} className="grid min-h-0 grid-cols-[minmax(0,1.3fr)_minmax(0,1fr)]">
            <RowList rows={view.rows} selected={view.selected} playing={view.playing_media_id} />
            {details}
          </main>
        )}

        {view.waveform_visible ? <Waveform view={view} /> : <span />}

        {view.download === null ? <span /> : <DownloadBar download={view.download} />}

        <Player
          playback={view.playback}
          chapters={view.playback_chapters}
          showChapterTimestamps={view.show_chapter_timestamps}
          artwork={view.details?.thumbnail_url ?? null}
          autoplay={view.autoplay}
          repeating={view.repeating}
          radioNowPlaying={view.radio_now_playing}
          track={view.now_playing}
          output={output}
        />

        <p className="min-h-[24px] border-t border-line bg-surface px-[18px] pt-[5px] pb-[7px] text-[11px] text-ink-faint">
          {view.transient_footer_notice ?? view.status_line}
        </p>
      </div>

      {/* Drawn in the order `render_frame` draws them, bottom to top. Each
          popup carries its own stacking layer, so the order is stated once
          rather than implied by where a component sits in this tree. */}
      {view.help_open ? (
        <HelpPopup
          audioQualitySupported={view.audio_quality_supported}
          playbackHistoryEnabled={view.playback_history_enabled}
        />
      ) : null}
      {view.project_history_popup ? (
        <ProjectHistoryPopup popup={view.project_history_popup} />
      ) : null}
      {view.youtube_setup_open ? <CredentialEditorNotice editor="youtube_setup" /> : null}
      {view.yandex_music_setup_open ? (
        <CredentialEditorNotice editor="yandex_music_setup" />
      ) : null}
      {view.rss_subscription_open ? <CredentialEditorNotice editor="rss_subscription" /> : null}
      {view.preferences_popup ? <PreferencesPopup popup={view.preferences_popup} /> : null}
      {view.local_file_popup ? <LocalFilePopup popup={view.local_file_popup} /> : null}
      {view.playlist_popup ? <PlaylistPopup popup={view.playlist_popup} /> : null}
      {view.queue_popup ? <QueuePopup popup={view.queue_popup} /> : null}
      {view.private_note_open ? <CredentialEditorNotice editor="private_note" /> : null}
      {view.video_comments_popup ? (
        <VideoCommentsPopup popup={view.video_comments_popup} />
      ) : null}
      {view.video_qr_popup ? <VideoQrPopup popup={view.video_qr_popup} /> : null}
      {view.audio_quality_popup ? (
        <AudioQualityPopup popup={view.audio_quality_popup} />
      ) : null}
      {view.error_popup ? (
        <ErrorPopup
          popup={view.error_popup}
          externalOpener={view.external_opener_available}
        />
      ) : null}
    </>
  );
}
