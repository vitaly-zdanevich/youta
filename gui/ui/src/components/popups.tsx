// Every popup the window can draw.
//
// The reducer owns whether a popup is open, what it contains, and where it is
// scrolled. These components render that and dispatch the same actions the
// terminal's keys produce; none of them holds state of its own. That is what
// makes the two front-ends the same application rather than two applications
// with a shared backend.
//
// SECURITY: comment bodies, error reports, and commit messages are untrusted
// text. They are rendered as text children, and `whitespace-pre-wrap` is what
// preserves their shape — never markup.

import type {
  ErrorPopupView,
  LocalFilePopupView,
  PlaylistPopupView,
  PreferencesPopupView,
  ProjectHistoryPopupView,
  VideoCommentsPopupView,
  VideoQrPopupView,
} from "../contract";
import { dispatch } from "../ipc";
import { Popup, PopupButton, PopupError } from "./Popup";
import { ScrollingText } from "./ScrollingText";

/** Stacking order, matching `render_frame` in `src/tui.rs`. */
export const LAYER = {
  help: 0,
  projectHistory: 1,
  credentialEditor: 2,
  preferences: 3,
  localFile: 4,
  playlist: 5,
  videoComments: 6,
  videoQr: 7,
  error: 8,
} as const;

/** A short scrollable region for popups whose offset the reducer does not own. */
function Body({ children }: { children: React.ReactNode }) {
  return <div className="h-full overflow-y-auto px-[18px] py-[11px] text-xs">{children}</div>;
}

/**
 * The keyboard reference.
 *
 * Unlike every other popup this content is front-end specific, and deliberately
 * so: the terminal's help lists its pointer mode, its Linux-console caveats,
 * and its GPM requirement, none of which exist here. What both lists must agree
 * on is the bindings themselves, which come from the one shared map.
 */
export function HelpPopup() {
  const sections: Array<[string, Array<[string, string]>]> = [
    [
      "Navigation",
      [
        ["/", "search"],
        ["Tab · Shift+Tab", "next · previous source"],
        ["j · k · ↑ · ↓", "move the selection"],
        ["Enter", "open or play"],
        ["Backspace", "back"],
        ["F2 · F3 · F4 · F5", "offline · history · playlists · stats"],
        ["S · p · F9", "subscriptions · preferences · recent commits"],
      ],
    ],
    [
      "Playback",
      [
        ["Space", "pause"],
        ["← · →", "seek 5 seconds"],
        ["0–9", "seek by ten percent"],
        ["↑ · ↓", "volume"],
        ["< · >", "speed"],
        ["[ · ]", "chapter"],
        ["T", "chapter timestamps"],
        ["r · A", "repeat · autoplay"],
        ["w", "waveform"],
      ],
    ],
    [
      "Actions",
      [
        ["Ctrl+n · a", "play next · add to queue"],
        ["d · o · y", "download · open page · copy link"],
        ["s · n", "subscribe · private note"],
        ["P · F6 · Q", "playlist · comments · QR code"],
        ["i", "expand artwork"],
        ["? · Esc", "this help · close"],
      ],
    ],
  ];
  return (
    <Popup
      title="Keys"
      subtitle="The same map serves the terminal front-end"
      layer={LAYER.help}
      width="640px"
      onDismiss={() => void dispatch("ToggleHelp")}
    >
      <Body>
        <div className="grid gap-[14px]">
          {sections.map(([heading, bindings]) => (
            <section key={heading}>
              <h3 className="mb-[5px] text-[11px] tracking-wide text-ink-faint uppercase">
                {heading}
              </h3>
              <dl className="grid grid-cols-[minmax(0,auto)_minmax(0,1fr)] gap-x-4 gap-y-[2px]">
                {bindings.map(([keys, meaning]) => (
                  <div key={keys} className="contents">
                    <dt className="font-mono text-[11px] text-accent">{keys}</dt>
                    <dd className="m-0 text-ink-dim">{meaning}</dd>
                  </div>
                ))}
              </dl>
            </section>
          ))}
        </div>
      </Body>
    </Popup>
  );
}

/** Recent commits and how this binary was installed. */
export function ProjectHistoryPopup({ popup }: { popup: ProjectHistoryPopupView }) {
  const remote =
    typeof popup.remote_state === "string"
      ? popup.remote_state === "UpToDate"
        ? "up to date"
        : popup.remote_state.toLowerCase()
      : `offline check failed: ${popup.remote_state.Unavailable}`;
  return (
    <Popup
      title="Recent commits"
      subtitle={`${popup.installation} · ${remote}`}
      layer={LAYER.projectHistory}
      onDismiss={() => void dispatch("DismissProjectHistory")}
      footer={
        <>
          <span className="font-mono">{popup.executable_path}</span>
          {popup.build_source ? <span className="font-mono">· {popup.build_source}</span> : null}
        </>
      }
    >
      <ScrollingText
        popup="project_history"
        offset={popup.scroll_offset}
        onScroll={(offset) => void dispatch({ SetProjectHistoryScroll: offset })}
      >
        {popup.commits.map((commit) => (
          <div key={commit.hash} className="mb-[8px]">
            <span
              className={
                commit.hash === popup.current_hash ? "text-accent" : "text-ink-faint"
              }
            >
              {commit.hash.slice(0, 9)} {commit.committed_at}
            </span>
            {"\n"}
            <span className="text-ink-dim">{commit.message.trimEnd()}</span>
          </div>
        ))}
      </ScrollingText>
    </Popup>
  );
}

/** The complete diagnostic report for a recoverable failure. */
export function ErrorPopup({
  popup,
  externalOpener,
}: {
  popup: ErrorPopupView;
  externalOpener: boolean;
}) {
  return (
    <Popup
      title={popup.title}
      subtitle={popup.action_status ?? undefined}
      layer={LAYER.error}
      onDismiss={() => void dispatch("DismissErrorPopup")}
      footer={
        <>
          <PopupButton onClick={() => void dispatch("CopyErrorReport")}>Copy report</PopupButton>
          {popup.gh_available ? (
            <PopupButton onClick={() => void dispatch("FillGitHubIssue")}>
              Prepare an issue
            </PopupButton>
          ) : null}
          {popup.gh_available && externalOpener ? (
            <PopupButton onClick={() => void dispatch("CopyAndOpenGitHubIssue")}>
              Copy and open GitHub
            </PopupButton>
          ) : null}
        </>
      }
    >
      <div className="h-full overflow-y-auto px-[18px] py-[10px] font-mono text-[11px] leading-[17px] whitespace-pre-wrap text-ink-dim">
        {popup.report}
      </div>
    </Popup>
  );
}

/** Twenty bounded public comments for one selected video. */
export function VideoCommentsPopup({ popup }: { popup: VideoCommentsPopupView }) {
  const state = popup.state;
  return (
    <Popup
      title="Comments"
      subtitle={popup.video_title}
      layer={LAYER.videoComments}
      onDismiss={() => void dispatch("DismissVideoComments")}
    >
      {state === "Loading" ? (
        <Body>
          <p className="text-ink-faint">Loading…</p>
        </Body>
      ) : state === "Empty" ? (
        <Body>
          <p className="text-ink-faint">This video has no public top-level comments.</p>
        </Body>
      ) : typeof state === "object" ? (
        <Body>
          <p role="alert" className="text-accent">
            {state.Error}
          </p>
        </Body>
      ) : (
        <ScrollingText
          popup="video_comments"
          offset={popup.scroll_offset}
          onScroll={(offset) => void dispatch({ SetVideoCommentsScroll: offset })}
        >
          {popup.comments.map((comment, index) => (
            <div key={`${comment.author_name}-${index}`} className="mb-[8px]">
              <span className="text-accent">{comment.author_name}</span>
              <span className="text-ink-faint">
                {` · ${comment.like_count} likes${comment.published ? ` · ${comment.published}` : ""}`}
              </span>
              {"\n"}
              <span className="text-ink-dim">{comment.text.trimEnd()}</span>
            </div>
          ))}
        </ScrollingText>
      )}
    </Popup>
  );
}

/** The offline QR code for one selected video. */
export function VideoQrPopup({ popup }: { popup: VideoQrPopupView }) {
  const { width, modules } = popup.matrix;
  // The quiet zone is mandatory and excluded from the matrix, so it is added
  // here as padding rather than as four rings of light modules.
  return (
    <Popup
      title="QR code"
      subtitle={popup.video_title}
      layer={LAYER.videoQr}
      width="auto"
      onDismiss={() => void dispatch("DismissVideoQr")}
      footer={<span className="font-mono break-all">{popup.url}</span>}
    >
      <div className="grid place-items-center p-6">
        <div
          role="img"
          aria-label={`QR code for ${popup.url}`}
          className="grid bg-white p-4"
          style={{
            gridTemplateColumns: `repeat(${width}, 5px)`,
            gridAutoRows: "5px",
          }}
        >
          {modules.map((dark, index) => (
            <span key={index} style={{ background: dark ? "#000" : "#fff" }} />
          ))}
        </div>
      </div>
    </Popup>
  );
}

/** The runtime preferences editor. Every value is a draft until Save. */
export function PreferencesPopup({ popup }: { popup: PreferencesPopupView }) {
  const toggles: Array<[string, boolean, string]> = [
    ["Skip advertisement chapters", popup.skip_advertisement_chapters, "ToggleSkipAdvertisementChapters"],
    ["Prewarm the selected YouTube video", popup.youtube_prewarm, "ToggleYouTubePrewarm"],
    ["Show Local folder sizes", popup.show_local_folder_sizes, "ToggleLocalFolderSizes"],
    ["Show artwork on a Linux console", popup.show_images_in_tty, "ToggleTtyImages"],
  ];
  const cycles: Array<[string, string, string]> = [
    ["Subscriptions layout", popup.subscriptions_layout, "SetSubscriptionsLayout"],
    ["YouTube thumbnail size", popup.youtube_thumbnail_size, "CycleYouTubeThumbnailSize"],
    ["Bandcamp audio format", popup.bandcamp_audio_format, "CycleBandcampAudioFormat"],
  ];
  return (
    <Popup
      title="Preferences"
      subtitle={popup.config_path}
      layer={LAYER.preferences}
      width="620px"
      onDismiss={() => void dispatch("DismissPreferences")}
      footer={
        <>
          <PopupButton emphasis onClick={() => void dispatch("SubmitPreferences")}>
            Save
          </PopupButton>
          <PopupButton onClick={() => void dispatch("DismissPreferences")}>Cancel</PopupButton>
          {popup.environment_override ? (
            // An environment variable wins over the file, so saving this key
            // would write a value the running process would keep ignoring.
            <span className="text-accent">
              {popup.environment_override} overrides the file; that setting cannot be saved here.
            </span>
          ) : null}
        </>
      }
    >
      <Body>
        <div className="grid gap-[7px]">
          {toggles.map(([label, value, action]) => (
            <label key={label} className="flex items-center justify-between gap-4">
              <span className="text-ink-dim">{label}</span>
              <PopupButton emphasis={value} onClick={() => void dispatch(action)}>
                {value ? "on" : "off"}
              </PopupButton>
            </label>
          ))}
          {cycles.map(([label, value, action]) => (
            <label key={label} className="flex items-center justify-between gap-4">
              <span className="text-ink-dim">{label}</span>
              <PopupButton
                onClick={() =>
                  void dispatch(
                    // One of these takes an explicit value and the rest cycle;
                    // the reducer decides the next value in every case, so the
                    // window never computes one.
                    action === "SetSubscriptionsLayout"
                      ? { SetSubscriptionsLayout: value === "drill-down" ? "split" : "drill-down" }
                      : action,
                  )
                }
              >
                {value}
              </PopupButton>
            </label>
          ))}
        </div>
      </Body>
      <PopupError message={popup.validation_error} />
    </Popup>
  );
}

/** The local-playlist chooser and its create/edit form. */
export function PlaylistPopup({ popup }: { popup: PlaylistPopupView }) {
  const editing = popup.mode !== "Choose";
  return (
    <Popup
      title={
        popup.mode === "Create" ? "New playlist" : popup.mode === "Edit" ? "Edit playlist" : "Playlists"
      }
      subtitle={popup.item_title}
      layer={LAYER.playlist}
      width="560px"
      onDismiss={() => void dispatch("DismissPlaylistPopup")}
      footer={
        editing ? (
          <>
            <PopupButton
              emphasis
              onClick={() =>
                void dispatch(popup.mode === "Create" ? "CreatePlaylistAndAdd" : "UpdatePlaylist")
              }
            >
              {popup.mode === "Create" ? "Create and add" : "Save"}
            </PopupButton>
            <PopupButton onClick={() => void dispatch("DismissPlaylistPopup")}>Back</PopupButton>
            <span>Typing edits the selected field; Tab switches it.</span>
          </>
        ) : (
          <>
            <PopupButton onClick={() => void dispatch("BeginNewPlaylist")}>New playlist</PopupButton>
            <span>Enter toggles membership.</span>
          </>
        )
      }
    >
      {editing ? (
        <Body>
          <div className="grid gap-[10px]">
            {(
              [
                ["Name", popup.editor_name, "Name", popup.name_limit],
                ["Description", popup.editor_description, "Description", popup.description_limit],
              ] as const
            ).map(([label, value, field, limit]) => (
              <button
                key={label}
                type="button"
                onClick={() => void dispatch({ SelectPlaylistEditorField: field })}
                className={`rounded-[6px] border px-[10px] py-[7px] text-left ${
                  popup.editor_field === field ? "border-accent" : "border-line-strong"
                }`}
              >
                <span className="block text-[11px] text-ink-faint">
                  {label} · {value.length}/{limit}
                </span>
                <span className="block break-words whitespace-pre-wrap">
                  {value}
                  {popup.editor_field === field ? (
                    <span className="bg-accent text-ground">&nbsp;</span>
                  ) : null}
                </span>
              </button>
            ))}
          </div>
        </Body>
      ) : (
        <div className="h-full overflow-y-auto py-[6px]">
          {popup.playlists.length === 0 ? (
            <p className="px-[18px] text-xs text-ink-faint">No playlists yet.</p>
          ) : (
            <ul className="m-0 list-none p-0">
              {popup.playlists.map((playlist, index) => (
                <li key={playlist.playlist_id}>
                  <button
                    type="button"
                    aria-current={index === popup.selected}
                    onClick={() => void dispatch({ SelectPlaylistPopupRow: index })}
                    onDoubleClick={() => void dispatch("ToggleSelectedPlaylistMembership")}
                    className={`flex w-full items-center gap-[10px] px-[18px] py-[5px] text-left text-xs ${
                      index === popup.selected ? "bg-raised text-ink" : "text-ink-dim"
                    }`}
                  >
                    <span className="w-[12px] text-accent">
                      {playlist.contains_item ? "✓" : ""}
                    </span>
                    <span className="min-w-0 truncate">{playlist.name}</span>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
      <PopupError message={popup.validation_error} />
    </Popup>
  );
}

/** Rename, Trash, and Move — the explicit filesystem mutations. */
export function LocalFilePopup({ popup }: { popup: LocalFilePopupView }) {
  const dismiss = () => void dispatch("DismissLocalFilePopup");

  if ("Rename" in popup) {
    const { value, error } = popup.Rename;
    return (
      <Popup
        title="Rename"
        layer={LAYER.localFile}
        width="520px"
        onDismiss={dismiss}
        footer={
          <>
            <PopupButton emphasis onClick={() => void dispatch("SubmitLocalRename")}>
              Rename
            </PopupButton>
            <PopupButton onClick={dismiss}>Cancel</PopupButton>
            <span>Typing edits the name.</span>
          </>
        }
      >
        <Body>
          <p className="rounded-[6px] border border-accent px-[10px] py-[7px] font-mono break-all">
            {value}
            <span className="bg-accent text-ground">&nbsp;</span>
          </p>
        </Body>
        <PopupError message={error} />
      </Popup>
    );
  }

  if ("Trash" in popup || "DownloadedTrash" in popup) {
    const downloaded = "DownloadedTrash" in popup;
    const { name, path, error } = downloaded ? popup.DownloadedTrash : popup.Trash;
    return (
      <Popup
        title="Move to Trash"
        subtitle={downloaded ? "Downloaded media" : undefined}
        layer={LAYER.localFile}
        width="520px"
        onDismiss={dismiss}
        footer={
          <>
            <PopupButton
              emphasis
              onClick={() =>
                void dispatch(downloaded ? "ConfirmDownloadedTrash" : "ConfirmLocalTrash")
              }
            >
              Move to Trash
            </PopupButton>
            <PopupButton onClick={dismiss}>Cancel</PopupButton>
            <span>Recoverable from the system Trash.</span>
          </>
        }
      >
        <Body>
          <p className="mb-[6px]">{name}</p>
          <p className="font-mono text-[11px] break-all text-ink-faint">{path}</p>
        </Body>
        <PopupError message={error} />
      </Popup>
    );
  }

  const { source_names, destination, directories, selected, pending, error } = popup.Move;
  return (
    <Popup
      title={`Move ${source_names.length} item${source_names.length === 1 ? "" : "s"}`}
      subtitle={destination}
      layer={LAYER.localFile}
      width="620px"
      onDismiss={dismiss}
      footer={
        <>
          <PopupButton emphasis onClick={() => void dispatch("ConfirmLocalMoveHere")}>
            Move here
          </PopupButton>
          <PopupButton onClick={dismiss}>Cancel</PopupButton>
          {pending ? <span>Listing…</span> : null}
        </>
      }
    >
      <div className="grid h-full grid-rows-[auto_minmax(0,1fr)]">
        <p className="truncate px-[18px] pt-[9px] text-[11px] text-ink-faint">
          {source_names.join(", ")}
        </p>
        <ul className="m-0 list-none overflow-y-auto p-0 py-[6px]">
          {directories.map((directory, index) => (
            <li key={directory.path}>
              <button
                type="button"
                aria-current={index === selected}
                onClick={() => void dispatch({ SelectLocalMoveDestination: index })}
                onDoubleClick={() => void dispatch("ActivateLocalMoveDestination")}
                className={`flex w-full gap-[8px] px-[18px] py-[4px] text-left text-xs ${
                  index === selected ? "bg-raised text-ink" : "text-ink-dim"
                }`}
              >
                <span className="text-ink-faint">{directory.name === ".." ? "↑" : "▸"}</span>
                <span className="min-w-0 truncate">{directory.name}</span>
              </button>
            </li>
          ))}
        </ul>
      </div>
      <PopupError message={error} />
    </Popup>
  );
}

/** Which credential-bearing editor the reducer has opened. */
export type CredentialEditor =
  | "youtube_setup"
  | "yandex_music_setup"
  | "rss_subscription"
  | "private_note";

const CREDENTIAL_EDITORS: Record<CredentialEditor, { title: string; body: string; dismiss: string }> =
  {
    youtube_setup: {
      title: "YouTube credentials needed",
      body:
        "YouTube asked for an API key or an Invidious instance. That editor holds a credential, so it is only offered by the terminal front-end — run youta in a terminal to fill it in, or set the key in the configuration file.",
      dismiss: "DismissYouTubeSetup",
    },
    yandex_music_setup: {
      title: "Yandex Music token needed",
      body:
        "Yandex Music asked for an OAuth token. That editor holds a credential, so it is only offered by the terminal front-end.",
      dismiss: "DismissYandexMusicSetup",
    },
    rss_subscription: {
      title: "Add a podcast feed",
      body:
        "A feed URL can itself carry a private token, so this editor is only offered by the terminal front-end.",
      dismiss: "DismissRssSubscriptionPopup",
    },
    private_note: {
      title: "Private note",
      body:
        "Private notes are user-authored text that never leaves the player process, so this editor is only offered by the terminal front-end.",
      dismiss: "DismissPrivateNotePopup",
    },
  };

/**
 * What the window shows while an editor it may not receive is open.
 *
 * This is a real gap rather than a placeholder for one. These four editors are
 * modal, so while one is open the shared keyboard map routes every key into it
 * — and the YouTube one opens by itself the first time a search runs without
 * credentials. Without this the window would look like an ordinary screen that
 * had simply stopped responding. The reducer sends one bit for exactly this
 * reason; see the module header in `src/view.rs`.
 */
export function CredentialEditorNotice({ editor }: { editor: CredentialEditor }) {
  const { title, body, dismiss } = CREDENTIAL_EDITORS[editor];
  return (
    <Popup
      title={title}
      layer={LAYER.credentialEditor}
      width="520px"
      onDismiss={() => void dispatch(dismiss)}
      footer={<PopupButton emphasis onClick={() => void dispatch(dismiss)}>Close</PopupButton>}
    >
      <Body>
        <p className="text-ink-dim">{body}</p>
      </Body>
    </Popup>
  );
}
