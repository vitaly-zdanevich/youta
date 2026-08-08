// Youta desktop front-end.
//
// SECURITY: every string below originates from a provider — video titles,
// channel names, descriptions, comments. It is written with textContent only.
// Never introduce innerHTML, insertAdjacentHTML, or a template that interpolates
// provider text; the window's CSP is the second line of defence, not the first.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const VIEW_EVENT = "youta://view";

const elements = {
  tabs: document.getElementById("tabs"),
  list: document.getElementById("list"),
  details: document.getElementById("details"),
  status: document.getElementById("status"),
  progress: document.getElementById("progress"),
  buffered: document.getElementById("buffered"),
  seek: document.getElementById("seek"),
  position: document.getElementById("position"),
  duration: document.getElementById("duration"),
  nowPlaying: document.getElementById("now-playing"),
  playPause: document.getElementById("play-pause"),
  back: document.getElementById("back"),
  forward: document.getElementById("forward"),
  chapters: document.getElementById("chapters"),
  chapterBack: document.getElementById("chapter-back"),
  chapterForward: document.getElementById("chapter-forward"),
  chapterLabel: document.getElementById("chapter-label"),
  slower: document.getElementById("slower"),
  faster: document.getElementById("faster"),
  speed: document.getElementById("speed"),
  repeat: document.getElementById("repeat"),
  autoplay: document.getElementById("autoplay"),
  quieter: document.getElementById("quieter"),
  louder: document.getElementById("louder"),
  volume: document.getElementById("volume"),
  volumeLevel: document.getElementById("volume-level"),
  output: document.getElementById("output"),
};

// Steps match the terminal front-end exactly (src/tui.rs key map): ±5 seconds,
// ±5 percent volume, ±0.1 speed. Two front-ends that step differently are two
// different players.
const SEEK_STEP_SECONDS = 5;
const VOLUME_STEP = 5;
const SPEED_STEP = 0.1;
const SCRUB_RESOLUTION = 1000;

let sources = [];
// The last snapshot, needed because volume is a *relative* action: the reducer
// takes a delta, so a slider has to know the value it is moving away from.
let current = null;
// A snapshot arriving mid-drag must not yank the scrubber out from under the
// pointer.
let scrubbing = false;

/** Sends one semantic action to the shared reducer. */
async function dispatch(action) {
  try {
    await invoke("dispatch", { action });
  } catch (error) {
    elements.status.textContent = String(error);
  }
}

/** Formats a duration the way the player labels timestamps. */
function formatSeconds(totalSeconds) {
  const seconds = Math.max(0, Math.floor(totalSeconds));
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const rest = seconds % 60;
  const padded = `${String(minutes).padStart(hours > 0 ? 2 : 1, "0")}:${String(rest).padStart(2, "0")}`;
  return hours > 0 ? `${hours}:${padded}` : padded;
}

/** Reads a serde `Duration`, which arrives as `{ secs, nanos }`. */
function durationSeconds(value) {
  if (!value) {
    return 0;
  }
  return (value.secs ?? 0) + (value.nanos ?? 0) / 1e9;
}

function renderTabs(view) {
  elements.tabs.replaceChildren();
  for (const source of sources) {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = source.label;
    button.setAttribute("aria-selected", String(source.id === view.screen));
    button.addEventListener("click", () => dispatch({ ShowScreen: source.id }));
    elements.tabs.append(button);
  }
}

function renderList(view) {
  elements.list.replaceChildren();
  view.rows.forEach((row, index) => {
    const button = document.createElement("button");
    button.type = "button";
    button.setAttribute("aria-current", String(index === view.selected));

    const marker = document.createElement("span");
    marker.className = "marker";
    marker.textContent = row.playing ? "▶" : row.subscribed ? "◆" : "";

    const text = document.createElement("span");
    const title = document.createElement("span");
    title.className = "title";
    title.textContent = row.title;
    text.append(title);

    if (row.subtitle) {
      const subtitle = document.createElement("span");
      subtitle.className = "subtitle";
      subtitle.textContent = row.subtitle;
      text.append(document.createElement("br"), subtitle);
    }

    const duration = document.createElement("span");
    duration.className = "duration num";
    duration.textContent = row.duration_label ?? "";

    button.append(marker, text, duration);
    button.addEventListener("click", () => dispatch({ SelectRow: index }));
    button.addEventListener("dblclick", () => dispatch("ActivateSelection"));
    elements.list.append(button);
  });
}

function renderDetails(view) {
  elements.details.replaceChildren();
  const details = view.details;
  if (!details) {
    const empty = document.createElement("p");
    empty.className = "empty";
    empty.textContent = "Nothing selected.";
    elements.details.append(empty);
    return;
  }

  const heading = document.createElement("h2");
  heading.textContent = details.title ?? "";
  elements.details.append(heading);

  const list = document.createElement("dl");
  const rows = [
    ["Source", details.source_label],
    ["Channel", details.channel_title],
    ["Published", details.published_label],
    ["Length", details.length_label],
    ["Views", details.view_count_label],
  ];
  for (const [label, value] of rows) {
    if (!value) {
      continue;
    }
    const term = document.createElement("dt");
    term.textContent = label;
    const definition = document.createElement("dd");
    definition.textContent = value;
    list.append(term, definition);
  }
  if (list.childElementCount > 0) {
    elements.details.append(list);
  }

  if (details.description) {
    const description = document.createElement("p");
    description.className = "description";
    description.textContent = details.description;
    elements.details.append(description);
  }
}

/** Whether the backend offers a usable timeline; mirrors `PlaybackStatus::seeking_available`. */
function seekingAvailable(playback) {
  return !playback.live || Boolean(playback.live_seekable_range);
}

/** Paints the already-fetched spans behind the played fill. */
function renderBuffered(playback, duration) {
  elements.buffered.replaceChildren();
  if (duration <= 0) {
    return;
  }
  for (const range of playback.buffered_ranges ?? []) {
    const start = durationSeconds(range.start);
    const end = durationSeconds(range.end);
    const span = document.createElement("span");
    span.style.left = `${(start / duration) * 100}%`;
    span.style.width = `${Math.max(0, ((end - start) / duration) * 100)}%`;
    elements.buffered.append(span);
  }
}

function renderChapters(view) {
  const chapters = view.playback_chapters ?? [];
  elements.chapters.hidden = chapters.length === 0;
  if (chapters.length === 0) {
    return;
  }
  const index = view.playback?.chapter;
  const chapter = typeof index === "number" ? chapters[index] : undefined;
  elements.chapterLabel.textContent = chapter?.title ?? `${chapters.length} chapters`;
}

function renderPlayer(view) {
  const playback = view.playback ?? {};
  const position = durationSeconds(playback.position);
  const duration = durationSeconds(playback.duration);
  const idle = playback.idle !== false;
  const seekable = !idle && duration > 0 && seekingAvailable(playback);

  elements.position.textContent = formatSeconds(position);
  elements.duration.textContent = duration > 0 ? formatSeconds(duration) : "--:--";
  const progress = duration > 0 ? Math.min(100, (position / duration) * 100) : 0;
  elements.progress.style.width = `${progress}%`;
  renderBuffered(playback, duration);

  elements.seek.disabled = !seekable;
  if (!scrubbing) {
    elements.seek.value = String(Math.round((progress / 100) * SCRUB_RESOLUTION));
  }

  elements.nowPlaying.textContent = playback.stream_title ?? playback.title ?? "";

  const paused = playback.paused !== false;
  elements.playPause.textContent = paused ? "▶" : "⏸";
  elements.playPause.setAttribute("aria-label", paused ? "Play" : "Pause");
  elements.playPause.disabled = idle;
  elements.back.disabled = !seekable;
  elements.forward.disabled = !seekable;

  const speed = playback.speed ?? 1;
  elements.speed.textContent = `${speed.toFixed(2)}×`;
  elements.slower.disabled = idle || speed <= 0.5;
  elements.faster.disabled = idle || speed >= 3;

  elements.repeat.setAttribute("aria-pressed", String(Boolean(view.repeating)));
  elements.autoplay.setAttribute("aria-pressed", String(Boolean(view.autoplay)));

  const volume = playback.volume ?? 100;
  elements.volume.value = String(volume);
  elements.volumeLevel.textContent = String(volume);

  renderChapters(view);
}

function render(view) {
  current = view;
  renderTabs(view);
  renderList(view);
  renderDetails(view);
  renderPlayer(view);
  elements.status.textContent = view.status_line ?? "";
}

/** Wires the transport controls to their semantic actions. */
function bindTransport() {
  elements.playPause.addEventListener("click", () => dispatch("TogglePause"));
  elements.back.addEventListener("click", () => dispatch({ SeekRelative: -SEEK_STEP_SECONDS }));
  elements.forward.addEventListener("click", () => dispatch({ SeekRelative: SEEK_STEP_SECONDS }));
  elements.chapterBack.addEventListener("click", () => dispatch({ ChangeChapter: -1 }));
  elements.chapterForward.addEventListener("click", () => dispatch({ ChangeChapter: 1 }));
  elements.slower.addEventListener("click", () => dispatch({ ChangeSpeed: -SPEED_STEP }));
  elements.faster.addEventListener("click", () => dispatch({ ChangeSpeed: SPEED_STEP }));
  elements.repeat.addEventListener("click", () => dispatch("ToggleRepeat"));
  elements.autoplay.addEventListener("click", () => dispatch("ToggleAutoplay"));
  elements.quieter.addEventListener("click", () => dispatch({ ChangeVolume: -VOLUME_STEP }));
  elements.louder.addEventListener("click", () => dispatch({ ChangeVolume: VOLUME_STEP }));

  elements.seek.addEventListener("pointerdown", () => {
    scrubbing = true;
  });
  const commitSeek = () => {
    scrubbing = false;
    dispatch({ SeekPercent: (Number(elements.seek.value) / SCRUB_RESOLUTION) * 100 });
  };
  elements.seek.addEventListener("pointerup", commitSeek);
  // A keyboard scrub never enters the pointer path, so `change` covers it and
  // also catches a pointer released outside the control.
  elements.seek.addEventListener("change", commitSeek);

  elements.volume.addEventListener("input", () => {
    // `ChangeVolume` is a delta, so the slider sends the difference from the
    // volume the reducer last reported rather than an absolute level.
    const target = Number(elements.volume.value);
    const delta = target - (current?.playback?.volume ?? target);
    elements.volumeLevel.textContent = String(target);
    if (delta !== 0) {
      dispatch({ ChangeVolume: delta });
    }
  });
}

// Keyboard mapping is deliberately minimal here. The terminal front-end owns a
// 17-level modal precedence chain in Rust; the window must eventually call that
// same map rather than restate it in JavaScript.
const KEYS = new Map([
  ["j", { MoveSelection: 1 }],
  ["ArrowDown", { MoveSelection: 1 }],
  ["k", { MoveSelection: -1 }],
  ["ArrowUp", { MoveSelection: -1 }],
  ["Enter", "ActivateSelection"],
  [" ", "TogglePause"],
]);

document.addEventListener("keydown", (event) => {
  if (event.ctrlKey || event.metaKey || event.altKey) {
    return;
  }
  // Arrow keys inside a slider belong to the slider, not to list navigation.
  if (event.target instanceof HTMLInputElement) {
    return;
  }
  const action = KEYS.get(event.key);
  if (action) {
    event.preventDefault();
    dispatch(action);
  }
});

/** Shows which engine and audio output this build is configured to use. */
function renderOutput(output) {
  const parts = [output.engine, output.driver];
  if (output.device) {
    parts.push(output.device);
  }
  elements.output.textContent = parts.join(" · ");
}

async function start() {
  bindTransport();
  sources = await invoke("screens");
  renderOutput(await invoke("audio_output"));
  render(await invoke("snapshot"));
  await listen(VIEW_EVENT, (event) => render(event.payload));
}

start().catch((error) => {
  const message = String(error);
  elements.status.textContent = `Youta could not start its window: ${message}`;
  // Also report it out of the web view: a failure visible only in here looks
  // like a clean start from every angle outside the window.
  invoke("report_startup_failure", { message }).catch(() => {});
});
