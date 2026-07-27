# Youta

[![CI](https://github.com/vitaly-zdanevich/youta/actions/workflows/ci.yml/badge.svg)](https://github.com/vitaly-zdanevich/youta/actions/workflows/ci.yml)
[![Quality Gate Status](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_youta&metric=alert_status)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_youta)
[![Coverage](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_youta&metric=coverage)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_youta)
[![Bugs](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_youta&metric=bugs)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_youta)
[![Vulnerabilities](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_youta&metric=vulnerabilities)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_youta)
[![Code Smells](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_youta&metric=code_smells)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_youta)
[![Duplicated Lines](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_youta&metric=duplicated_lines_density)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_youta)
[![Maintainability](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_youta&metric=sqale_rating)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_youta)
[![Reliability](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_youta&metric=reliability_rating)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_youta)
[![Security](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_youta&metric=security_rating)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_youta)
[![Lines of Code](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_youta&metric=ncloc)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_youta)
[![Technical Debt](https://sonarcloud.io/api/project_badges/measure?project=vitaly-zdanevich_youta&metric=sqale_index)](https://sonarcloud.io/summary/new_code?id=vitaly-zdanevich_youta)

Youta is a low-resource terminal YouTube audio player and subscription manager
written in Rust. It saves and shows listening progress. Subscriptions are
currently stored and managed locally; YouTube-account synchronization is not
implemented yet. Youta uses an invisible `mpv` process for playback,
communicates with it over JSON IPC, and can use `yt-dlp` as an explicitly
enabled media resolver and downloader. The terminal UI remains the only visible
interface: its seek bar, queue, volume, pause state, and hotkeys control `mpv`.

> **Project status: pre-alpha foundation.** The repository contains the first
> working TUI and core state model, human-readable TOML persistence, optional
> SQLite persistence, OPML import/export, configuration loading, official
> YouTube Data API v3 and Invidious discovery, and supervised `mpv`/`yt-dlp`
> integration. The larger provider, upload, remote sync, scrobbling, waveform,
> and audiophile feature set described in the roadmap is not implemented yet.
> Do not treat an existing Cargo feature name as a support claim.

## Why this design

- The UI stays responsive while network, metadata, and playback work happen
  outside the render loop.
- Persistent state is local-first and restartable. Youta stores navigation,
  queue, playlists, history, notes, bookmarks, and playback positions beneath
  `~/.config/youta/`.
- The Local tab browses supported media and images in place. Youta never
  reorganizes folders automatically; only explicit Rename, Move to Trash, and
  Move actions change selected entries. A durable move journal lets startup
  finish or reconcile interrupted moves without guessing which copy is
  authoritative. Recursive folder sizes are enabled by default, calculated
  asynchronously one folder at a time, and never follow symbolic links. `[Z]`
  cycles size sorting off, ascending, and descending; unknown folders remain
  after known sizes.
- Optional providers are isolated behind Cargo features, so a local/RSS-only
  build does not need YouTube or cloud integrations.
- A plain Linux TTY is a primary target. Youta does not attempt thumbnail
  graphics when no supported terminal image protocol is available.

See [Architecture](docs/ARCHITECTURE.md), [feasibility and service
tiers](docs/FEASIBILITY.md), and [audiophile guidance](docs/AUDIOPHILE.md).

## The `mpv` backend and the TUI

Yes: external `mpv` still plays through the same Youta TUI and seek bar.
Youta starts `mpv` without a window or terminal input and controls it through a
private IPC socket. Playback position, duration, pause, volume, end-of-file,
and errors flow back into Youta's state. Seeking from keys, mouse clicks,
chapters, or a future waveform sends IPC commands to the same player process.
The backend requires `mpv` 0.38 or newer so resume positions and extractor
options can be applied atomically through `loadfile` per-file options.

Youta prepares the selected YouTube video's audio by default so `Enter` can
start playback without first waiting for a complete foreground resolution.
Selection must remain unchanged for 200 ms before one bounded worker invokes
`yt-dlp`; moving through the result list therefore cancels stale work instead
of resolving every row. Short-lived signed media URLs and their HTTP headers
remain in RAM only, are never written to session state, history, or
configuration, and are redacted from debug and diagnostic output. If the
prepared URL is absent, expired, or fails before audible playback begins,
Youta falls back to the video's canonical YouTube URL and the normal
`yt-dlp`/`mpv` path.
Disable this with
`playback.youtube_prewarm = false`, `[y] Prepare selected YouTube audio` in
Preferences, or `YOUTA_PLAYBACK__YOUTUBE_PREWARM=false`.

`[A] Autoplay` is off by default and persists its state in
`playback.autoplay`. When enabled, EOF advances through the same YouTube,
YouTube Music, subscription-channel, Local, or MOD/tracker list. Items added
with **Play next** or **Add to queue** always run first; Youta then resumes the
original source list. Replacing a live search stops that list's continuation
instead of accidentally playing an unrelated new result.

Description timecodes become exact chapter split and mouse-seek targets. Dense
chapter labels grow to as many as four rows when the terminal has spare height;
`T` toggles between timestamps plus names and names only without moving those
targets. This label preference is restored with the previous session. By
default, Youta hides and skips only chapters whose normalized title is
exactly `Реклама`; set `playback.skip_advertisement_chapters` to `false` to
retain them.

Vertical YouTube videos use a distinct title color once the configured
provider reports a portrait aspect ratio. The official adapter uses player
dimensions already returned by its batched video request, while Invidious
enriches the selected row from its existing video-format response. Youta does
not infer orientation from YouTube's often letterboxed thumbnail canvas or
issue one metadata request per search result.

`mpv` is a playback engine, not a second UI. It is intentionally kept out of
the terminal and never parses Youta's keystrokes. A future native backend can
implement the same playback interface without changing screens or history.

## Current foundation

The current pre-alpha foundation includes:

- configuration-file plus `YOUTA_` environment overrides;
- a source-neutral domain model for media, channels, queues, positions, notes,
  and provider capabilities;
- deterministic, human-readable TOML state by default, with SQLite available
  behind an optional Cargo feature;
- local subscriptions with OPML import/export;
- persistent local playlists with editable descriptions, cross-source replay,
  and a built-in `todo` list;
- a two-panel terminal UI and restartable screen state;
- official YouTube Data API v3 or Invidious video/channel search and video
  details, with description-link extraction;
- an independent YouTube Music tab that searches playable tracks through
  `yt-dlp` without requiring a YouTube Data API key;
- an independent Bandcamp tab that searches public track and album pages and
  resolves only the selected release for explicit playback through `yt-dlp`;
- an independent Apple Podcasts tab that searches the public, unauthenticated
  Apple catalogue by storefront and lazily loads playable episode metadata;
- an account-free Radio tab backed by a static, zero-startup-network catalogue
  of direct public streams;
- lazy Wikidata enrichment for exact YouTube, SoundCloud, and Bilibili
  external identifiers;
- supervised, argument-safe `mpv` JSON IPC and `yt-dlp` metadata/download
  commands;
- `doctor` and configuration inspection commands;
- parsing foundations for optional SponsorBlock and DeArrow data.

Exact commands and enabled adapters may change while the pre-alpha CLI settles.
Run `youta --help` for the binary's authoritative command list.

### Additional provider boundaries

The first provider set deliberately distinguishes a rich adapter from a URL
resolver:

- **PeerTube** is a first-class, configurable-instance provider. Its REST API
  can search videos, channels, and playlists known to that instance; federated
  or global-search coverage depends on the instance administrator. See the
  [PeerTube REST API](https://docs.joinpeertube.org/api-rest-reference).
- **Funkwhale** is a first-class, configurable-instance audio provider. Youta
  targets Funkwhale's stable REST API first and may share a narrow compatibility
  layer with its supported subset of Subsonic. See the [Funkwhale API
  documentation](https://docs.funkwhale.audio/developer/api/).
- **Jamendo** is a first-class music provider using only the official v3 tracks
  API. It offers bounded paginated search, duration/release filters,
  total-listen ordering, direct track lookup, artwork, stream links, and
  downloads only when `audiodownload_allowed` is true. Users must register
  their own `providers.jamendo_client_id`; Youta does not bundle the
  documentation/testing ID. The exact Creative Commons licence URL is shown,
  but NC or ND tracks are not automatically treated as Wikimedia
  Commons-compatible. See the [Jamendo v3 tracks API](https://developer.jamendo.com/v3.0/tracks).
- **Vimeo** and **RuTube** begin as validated direct-URL adapters using
  `yt-dlp`. Rich Vimeo search requires a registered application and a Vimeo API
  token; it is a later adapter. Youta does not assume a stable public RuTube
  catalog API.
- **BBC Radio** accepts BBC Sounds landing URLs through `yt-dlp` and imports
  official BBC podcast/RSS feeds through the RSS provider. Youta does not claim
  a stable public BBC Sounds catalog API. Its `bbc-radio` build feature is
  independent from the curated `radio` feature.
- **SoundCloud** accepts direct URLs through `yt-dlp`. Rich search and
  subscriptions use the official API only when users provide their own
  application credentials; the API uses OAuth 2.1. See the [SoundCloud API
  guide](https://developers.soundcloud.com/docs/api/).
- **SoundStream** accepts exact `soundstream.media` playlist and clip links
  through its current read-only v3 metadata endpoints. Those endpoints are not
  documented for third-party clients and may change. Youta does not automate
  anonymous-account registration, catalog search, or auth-gated audio signing;
  it exposes a feed or direct enclosure only when the public response includes
  one. The generic direct-URL fallback remains available, but the installed
  extractor may report the site as unsupported.
- **LitRes podcasts** are an opt-in `litres` feature. Catalog search, item
  details, and episode pagination use the documented
  [CataLit 2.0 API](https://docs.litres.ru/public/6424300.html), a user-provided
  LitRes application ID/secret, and only the documented anonymous session.
  Requests are bounded and limited to one per second. Exact public podcast
  pages may contribute schema.org metadata and an explicit unsigned media URL,
  but Youta never derives downloads from file IDs or framework state and never
  bypasses login, payment, DRM, or signed-link controls. This follows the
  [LitRes public offer](https://www.litres.ru/pages/litres_oferta/).
- **Generic `yt-dlp`** accepts a direct URL handled by any built-in extractor
  present in the installed `yt-dlp`. It provides resolution, metadata, and
  permitted downloads, not universal search or subscriptions. Extractor
  presence is not a guarantee that a site works today.
- **Tracker music** starts with The Mod Archive's official XML API and a
  user-provided key. Modland is also a default catalog: its HTTPS
  `allmods.zip` index and direct files avoid page scraping. Mirsoft Game Music
  Base is enabled by default at the user's request, but currently works only
  over HTTP; Youta shows a one-time transport warning and
  `providers.allow_insecure_http = false` disables it. Scene.org offers an
  official search API; AMP, UnExoticA, Aminet, and modules.pl need separate,
  rate-limited adapters. Playback requires a compatible decoder such as
  libopenmpt, and some exotic Amiga formats need a future UADE backend.
  Archive availability does not grant a free license or re-upload rights.

### Public Radio tab

The default build includes a separate **Radio** tab. Its catalogue is compiled
into Youta, needs no account, and performs no directory request at startup.
`Enter` sends the selected live stream directly to the normal invisible `mpv`
backend. Live streams do not restore or save a playback position; seeking and
repeat are disabled. They remain marker-free as
`Radio · live` entries in
History, `todo`, and other playlists. Listening time still contributes to the
Radio total on the Stats screen.

The current station set is:

- [Sector Radio — Progressive](https://sectorradio.com/), [4duk Radio](https://4duk.ru/),
  [SomaFM Groove Salad](https://somafm.com/groovesalad/), [KEXP](https://www.kexp.org/),
  [NTS 1 and NTS 2](https://www.nts.live/), [WFMU](https://wfmu.org/), and
  [Radio Paradise](https://radioparadise.com/);
- [R/a/dio](https://r-a-d.io/), [AnimeRadio.de](https://www.animeradio.de/),
  [Anison.FM](https://en.anison.fm/), and [LISTEN.moe](https://listen.moe/);
- [FIP](https://www.radiofrance.fr/fip),
  [Radio Swiss Classic](https://www.radioswissclassic.ch/en),
  [France Musique](https://www.radiofrance.fr/francemusique),
  [All Classical Radio](https://www.allclassical.org/),
  [NPO Klassiek](https://www.npoklassiek.nl/), and
  [Deutschlandfunk](https://www.deutschlandfunk.de/).

Details shows only quality attributes known for that preset, the readable
playback endpoint, a summary, and the station homepage. `[O] xdg-open` opens
the homepage rather than the audio endpoint. The same stable station identity
is used for playlists, History replay, private station notes, and the
now-playing click target; transient redirects are never persisted.

Station ICY metadata observed by `mpv` can appear beside the stable station
title. France Musique and 4duk also have bounded passive metadata adapters:
fresh provider data wins, ICY is the playing fallback, and a failed refresh
retains the last successful value only as clearly stale selected-station
details. Failures stay silent and retry with a station-scoped capped
1/2/5/10-minute backoff, so an unavailable service does not create an idle
polling loop.

Sector Radio, 4duk, WFMU, and Radio Paradise currently publish the selected
playback endpoint over plain HTTP. These streams are enabled by default as
requested, but transport is unauthenticated and can be observed or modified
on the network. Youta sends no credentials to them. Inclusion describes
technical public reachability, not an assertion that the broadcast content is
openly licensed or reusable.

### Lazy Wikidata enrichment

Selecting a supported search result or opening a supported direct link can
start an exact external-ID lookup against the public [Wikidata Query Service
(WDQS)](https://wikitech.wikimedia.org/wiki/Wikidata_Query_Service/Technical_interactions).
No Wikidata request is made at startup. The current mappings are:

| Source object | Exact Wikidata property |
| --- | --- |
| YouTube video | [YouTube video ID (P1651)](https://www.wikidata.org/wiki/Property:P1651) |
| YouTube channel | [YouTube channel ID (P2397)](https://www.wikidata.org/wiki/Property:P2397) |
| SoundCloud account or track path | [SoundCloud ID (P3040)](https://www.wikidata.org/wiki/Property:P3040) |
| Bilibili video | [Bilibili video ID (P6456)](https://www.wikidata.org/wiki/Property:P6456) |
| Bilibili channel/user | [Bilibili user ID (P6455)](https://www.wikidata.org/wiki/Property:P6455) |

Each response is limited to 512 KiB and 20 matches. Successful lookups are
cached in the selected persistence backend for seven days; successful empty
lookups are cached for 24 hours. Network and response errors are not
negative-cache entries.

Each matched entity appears once under External links as a collapsed
`[W] 🧾▸` row. Activating that row lazily requests the entity's bounded,
human-readable statements plus canonical Wikipedia article sitelinks and
expands them in the scrollable Details pane. Statement values and Wikipedia
rows retain validated clickable targets. Activating `[W] 🧾▾` collapses the
spoiler again. Entity data is not fetched for items the user never expands.

This is exact-ID enrichment, not title, name, or arbitrary-URL matching.
YouTube video IDs come from validated links, bare IDs, or search results;
channel lookup requires the 24-character `UC…` ID and does not resolve handles
or custom names. SoundCloud accepts only one- or two-segment canonical
`soundcloud.com` account/track paths; for a track it checks both the exact
`account/track` value and the exact account value. SoundCloud short redirects
are not resolved. Bilibili accepts canonical `[www.]bilibili.com/video/{BV…}`
or `/video/{av…}` links and `space.bilibili.com/{numeric-UID}` links. It does
not resolve `b23.tv` or other redirect hosts before Wikidata lookup.

## Build and run

Youta requires Rust 1.95 or newer.

Build an optimized binary and start Youta with one command:

```sh
cargo run --release --locked
```

If another operating-system account previously built the same checkout and
Cargo reports a permission error below `target/`, remove only the generated
build artifacts once, then repeat the command above:

```sh
cargo clean
```

```sh
cargo build --locked
cargo test --locked --all-targets
cargo run --locked -- --help
```

The default build expects `mpv` and `yt-dlp` at runtime for online playback.
They remain separate executables so they can be updated quickly when sites
change. Human-readable persistence is part of the core build. The default
feature set also enables `thumbnails`; runtime capability checks decide whether
the TUI may fetch and render artwork.

After installation, the current commands are:

```text
youta                         # open the TUI
youta tui                     # open the TUI explicitly
youta search QUERY            # search videos with the configured YouTube provider
youta search --channels QUERY # search channels with the configured YouTube provider
youta doctor                  # inspect helpers, paths, and decoder support
youta config                  # print non-secret effective paths and settings
youta extractors              # list extractors reported by installed yt-dlp
```

The TUI starts without a network request. On the first YouTube search without a
configured metadata provider, it opens a setup popup where the user can enter
either a YouTube Data API key or an Invidious instance URL. The popup shows the
exact destination before saving: API keys go to
`~/.config/youta/secrets/credentials.toml`, while an Invidious instance URL
goes to `~/.config/youta/config.toml`. On Unix, Youta creates private
directories with mode `0700` and files with mode `0600`; stored keys remain
plaintext. Environment values take precedence over both files. The popup lists
the steps to create a Google Cloud project, enable YouTube Data API v3, create
an API key, and restrict it to that API so it cannot call unrelated Google
APIs. Its `[F1]` link opens Google's official [credentials
guide](https://developers.google.com/youtube/registering_an_application),
`[F2]` opens [Google Cloud
Credentials](https://console.cloud.google.com/apis/credentials), and `[F3]`
opens the official [Invidious instance
list](https://docs.invidious.io/instances/). All three links also accept mouse
clicks.

The provider selection and Invidious URL can be configured manually in
`~/.config/youta/config.toml`:

```toml
[providers]
youtube_backend = 'auto' # auto, official, or invidious
# invidious_base_url = 'https://inv.example.org/'
```

Store the plaintext API key separately in
`~/.config/youta/secrets/credentials.toml`:

```toml
[providers]
youtube_api_key = '...'
```

`auto` prefers that key when the official adapter is compiled in, then falls
back to `invidious_base_url`. `official` and `invidious` select only that
backend. Both the TUI and `youta search` use this selection.

For a small local-only build:

```sh
cargo build --release --no-default-features \
	--features tui,local,backend-mpv
```

This intentionally omits terminal thumbnails. A custom
`--no-default-features` build must list `thumbnails` explicitly when artwork is
wanted.

For a small TUI build containing only the curated Radio catalogue and `mpv`
playback:

```sh
cargo run --release --locked --no-default-features \
	--features tui,radio,backend-mpv
```

For metadata through the official YouTube Data API instead of Invidious:

```sh
cargo build --release --no-default-features \
	--features tui,thumbnails,local,rss,youtube-official,backend-mpv
```

Copy [config.example.toml](config.example.toml) to
`~/.config/youta/config.toml`. Environment variables override file values;
nested keys use two underscores, for example:

```sh
YOUTA_UI__THEME=dark youta
YOUTA_PROVIDERS__YOUTUBE_API_KEY='...' youta search 'query'
```

Do not place tokens in shell history. The current pre-alpha configuration
layer accepts token fields as plain strings in `secrets/credentials.toml`. The
TUI provider popup says where it will save the key and applies user-only Unix
permissions. Environment injection avoids storing it on disk. A system-keyring
adapter and explicit secret references are roadmap work.

## Human-readable state, OPML, and optional SQLite

The default files backend is part of Youta's core and writes deterministic TOML
beneath `~/.config/youta/`:

```text
state/manifest.toml      format and backend marker
state/progress.toml      positions, durations, and played overrides
state/history.toml       playback history
state/notes.toml         private notes
state/bookmarks.toml     media and segment bookmarks
state/statistics.toml    listening totals
state/local-moves.toml   crash-recoverable Local move journal
state/playlists.toml     playlist metadata and ordered entries
runtime/session.toml     restart-only UI and session state
runtime/playback-checkpoint.toml
                         bounded periodic playback crash recovery
cache/searches.toml      regenerable search snapshots
cache/providers.toml     regenerable provider metadata
subscriptions.opml       portable RSS, podcast, and compatible channel feeds
```

The `state/` files are the canonical user-owned state for this backend.
`runtime/` and `cache/` can be regenerated or replaced by later application
activity. Writes use canonical ordering and same-directory atomic replacement
so diffs remain readable and an interrupted write does not replace the last
complete document. Each kind of state has its own document, so saving playback
progress does not rewrite history, notes, bookmarks, statistics, or playlists.
At startup, a corrupt `runtime/` or `cache/` document is preserved beside its
canonical path under a private hidden `.corrupt` name and replaced with an
empty valid document. Existing quarantine files are never overwritten.
Canonical `state/*.toml` documents are not reset or quarantined automatically;
Youta stops and leaves them untouched for manual recovery.

Only one Youta process can open the files backend at a time. It holds an
exclusive `state/.lock` for the lifetime of the store and reports an error
instead of risking concurrent writers. Close Youta before editing `state/*.toml`
by hand, then reopen it so the validated files are loaded from disk.

TOML is ordinary text: Firefox can display it, although Firefox is not itself a
general editor for local `file://` documents. Once the directory is committed,
GitHub and GitLab can display diffs and edit TOML in their browser editors; a
normal text editor remains the direct local editing route.

SQLite is optional. Build with `sqlite-state` to make
`persistence.backend = 'sqlite'` available, or use `bundled-sqlite` to compile
that backend with vendored SQLite:

```sh
cargo build --release --features sqlite-state
cargo build --release --features bundled-sqlite
```

SQLite uses `~/.config/youta/state.sqlite3`; it is not the default or a second
simultaneous source of truth. The TOML files and an untouched SQLite database
may coexist. `persistence.backend` alone selects which state is active, so
switching back to `sqlite` reopens the database rather than migrating or
deleting it.

## Private notes

Press `m`, or activate the **Add private note** / **Edit private note** row in
Details, to open the focused multiline editor. The row is highlighted when the
exact selection already has a note and remains a selectable mouse action.
Youta keeps one private note per exact target:

- media targets include a YouTube video, YouTube Music or Bandcamp track,
  Apple Podcasts episode, MOD/tracker item, resolved direct-source item, or
  local file; the same media target is reused when selected through
  **Downloaded**, **History**, or a playlist;
- source targets include a YouTube channel, Bandcamp album/release, an
  RSS/podcast subscription, or an Apple Podcasts show.

Provider-qualified IDs keep equal-looking titles from sharing a note, and a
channel/show note remains independent from notes on its videos or episodes.
The note is limited to 16 KiB of UTF-8 text.

| Editor key | Action |
| --- | --- |
| `Enter` | Insert a new line. |
| `Backspace` | Delete the previous complete character/grapheme. |
| Arrow keys, `Home`, `End` | Move the insertion cursor. |
| `Ctrl+S` | Add or save the sole note for the exact target. |
| `Delete`, then `Delete` or `Enter` | Confirm deletion of an existing note. |
| `Esc` | Close without saving the current draft. |

Notes survive restarts in `state/notes.toml` with the default files backend, or
in `state.sqlite3` when the optional SQLite backend is selected. The editor
shows the active destination. Empty notes are rejected; use the explicit
delete action to remove one.

OPML deliberately remains the subscription interchange format. It carries
feed URLs and outline folders, but it has no standard listening-progress
fields. Youta stores source-neutral current position, total duration, update
time, and played override so the model also covers YouTube, Bandcamp,
MOD/tracker, and local media. For podcasts, a future `gpodder` adapter maps
those values to `position`, `total`, and `timestamp`, and captures the
per-play start offset required for `started`. It can import, export, or
synchronize episode-action JSON without making that service protocol Youta's
canonical file format. See the
[gPodder episode-actions API](https://gpoddernet.readthedocs.io/en/latest/api/reference/events.html)
and [gPodder synchronization manual](https://gpodder.github.io/docs/user-manual.html).

## Online discovery and `yt-dlp`

These are distinct integration modes:

- The implemented official [YouTube Data API
  v3](https://developers.google.com/youtube/v3) metadata adapter uses the
  user's API key for video/channel
  [search](https://developers.google.com/youtube/v3/docs/search/list) and
  public video/channel details from
  [`videos.list`](https://developers.google.com/youtube/v3/docs/videos/list)
  and
  [`channels.list`](https://developers.google.com/youtube/v3/docs/channels/list).
  Account actions such as subscribing or posting comments require OAuth; an
  API key alone cannot authorize them. The roadmap includes opt-in,
  bidirectional subscription sync between Youta's local OPML file and the
  user's YouTube account through authenticated
  [`subscriptions.list`](https://developers.google.com/youtube/v3/docs/subscriptions/list),
  [`subscriptions.insert`](https://developers.google.com/youtube/v3/docs/subscriptions/insert),
  and
  [`subscriptions.delete`](https://developers.google.com/youtube/v3/docs/subscriptions/delete),
  with a preview before remote additions or removals.
- Invidious is the keyless alternative when the user configures an instance.
  `providers.youtube_backend = 'auto'` prefers the official adapter when
  `providers.youtube_api_key` is set, then uses
  `providers.invidious_base_url`.
- The separate **YouTube Music** tab searches the public
  `music.youtube.com` catalog through
  [yt-dlp](https://github.com/yt-dlp/yt-dlp), so discovery and playback do not
  require a YouTube Data API key. Youta recursively resolves music browse
  containers but retains only playable track-level video IDs, with strict
  process, output, timeout, and result limits. Its query, results, and selected
  row are saved independently from the normal YouTube and MOD/tracker tabs.
  Search runs on a capacity-one latest-only worker, so a slow `yt-dlp` search
  cannot delay general YouTube provider requests.
  When an official or Invidious metadata provider is configured, it may enrich
  the selected track with full public video details; basic music search and
  playback remain keyless.
- The separate **Apple Podcasts** tab uses Apple's documented,
  unauthenticated [iTunes Search
  API](https://developer.apple.com/library/archive/documentation/AudioVideo/Conceptual/iTuneSearchAPI/Searching.html)
  to discover podcast shows. Apple documents podcast-show search, but not
  episode search or result pagination, so Youta keeps one bounded ranked
  result set and, only after Enter, loads the bounded associated episodes Apple
  returns from its documented lookup. Youta preserves Apple's returned order
  without claiming that this is a complete or newest-first episode list. The
  same tab accepts official Apple Podcasts show and episode URLs; direct
  episodes play from their public RSS enclosure, while direct shows open the
  same bounded episode view and preserve Back navigation. The storefront,
  query, show results, and selected row are cached independently across
  restarts. No Apple account, API key, or played-status synchronization is
  implied. Apple API redirects stay on the exact original origin. Returned
  feed, artwork, and enclosure URLs reject non-public literals and obvious
  local-only names; Youta does not fetch the returned feed. Thumbnail fetches
  also require public DNS results and reject redirects. Enclosures are handed
  to the external playback backend, which owns later media DNS and redirects.
- The separate **Bandcamp** tab performs bounded, best-effort searches of
  Bandcamp's public HTTPS search page and accepts only canonical track and
  album pages on artist or label subdomains. Search persists the query, current
  page, advertised next page, compact public metadata, and selected row, but
  never a resolved stream. Pressing Enter resolves only the selected release
  through a bounded `yt-dlp` worker. It passes no cookies and does not provide
  access to authenticated purchases; resolved media URLs and headers remain in
  RAM. A canonical `https://artist.bandcamp.com/track/...` or `/album/...`
  input opens directly in this first-class tab without issuing a text search.
  Public-page search has its own capacity-one latest-only worker and cannot
  hold the general YouTube provider lane.
- `[N] Sort: relevance/newest` changes the order and reloads the current
  YouTube search. The official adapter sends `order=date` for newest-first
  searches. Invidious currently
  [documents](https://docs.invidious.io/search-filters/) relevance and
  view-count sorting, but no upload-date ordering, so Youta keeps its supported
  relevance request and stably sorts each returned video page by its
  publication timestamps.
  That fallback is page-local and does not claim a global order across pages.
- `[C] CC only: off/on` reloads the current YouTube video search and retains
  the choice across pagination and sort changes. The official adapter uses
  `videoLicense=creativeCommon`, as documented by
  [`search.list`](https://developers.google.com/youtube/v3/docs/search/list).
  Invidious uses its documented
  [`features=creative_commons`](https://docs.invidious.io/search-filters/)
  filter. Invidious search results do not independently prove the exact
  licence terms, and instance behavior can depend on its deployed version, so
  Youta still loads and displays the selected video's licence metadata before
  offering a Commons workflow. The toggle applies to videos, not channels.
- The official YouTube and Invidious adapters provide discovery and metadata
  only. Online playback remains the independent `yt-dlp` resolver plus the
  invisible `mpv` backend; the YouTube API key is not a playback credential.
- The official [YouTube API developer
  policies](https://developers.google.com/youtube/terms/developer-policies)
  prohibit API clients from downloading or offering offline playback of
  YouTube audiovisual content, separating audio from video, background
  playback, and interfering with advertisements. Therefore Youta must not
  present its audio extraction, downloading, SponsorBlock, or ad-related
  behavior as an official-API feature.
- [Invidious](https://docs.invidious.io/api/) and
  [yt-dlp](https://github.com/yt-dlp/yt-dlp) are opt-in, independently
  configurable tools. Their availability and site compatibility can change.
  Users are responsible for the terms, copyright, and laws that apply to media
  they access.

Bandcamp audio defaults to **Best available** (`best-available`). The `[b]`
control in the `[p]` Preferences popup cycles the same closed set accepted by
`providers.bandcamp_audio_format` and
`YOUTA_PROVIDERS__BANDCAMP_AUDIO_FORMAT`: `best-available`, `flac`, `alac`,
`wav`, `aiff`, `mp3-320`, `mp3-v0`, `aac`, `ogg-vorbis`, and
`public-stream-mp3-128`. These are Youta-owned selectors, not arbitrary
`yt-dlp` format expressions. A requested encoding remains a preference:
availability depends on the public release and the installed extractor.

Youta passes validated URLs and an allowlisted argument set directly to
`yt-dlp`; it does not construct a shell command. It does not import browser
cookies automatically. Cookie files can expose logged-in sessions and must be
treated as secrets. Keep `yt-dlp` updated because extractor fixes and security
fixes ship frequently. See the upstream [FAQ](https://github.com/yt-dlp/yt-dlp/wiki/FAQ)
and [supported-sites warning](https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md).

If YouTube rejects the initial media URL with HTTP 403 before audio starts,
Youta retries once with yt-dlp's
[`--check-formats`](https://github.com/yt-dlp/yt-dlp#video-format-options)
validation. Normal playback does not pay that extra request cost. A repeated
403 remains a visible diagnostic instead of advancing the queue or retrying in
a loop. Current YouTube deployments may require a Proof of Origin token for
some clients or formats; yt-dlp recommends an automatic token-provider plugin
rather than manually maintained tokens. Follow its current
[PO Token guide](https://github.com/yt-dlp/yt-dlp/wiki/PO-Token-Guide) when the
checked-format retry also fails.

SponsorBlock is for crowdsourced in-video segments such as sponsor messages;
it is not a blocker for YouTube's platform-inserted advertisements. Its
integration cannot be combined with a policy-compliant official YouTube player.
DeArrow supplies optional crowdsourced titles and thumbnail timestamps; the
original title remains available and the feature is toggleable.

## Thumbnails and real TTYs

The default build includes the `thumbnails` feature. Youta renders the selected
item's artwork only when it detects the [Kitty graphics
protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/), [iTerm2 inline
images](https://iterm2.com/documentation-images.html), or
[Sixel](https://vt100.net/docs/vt3xx-gp/chapter14.html). It downloads
selected-item artwork with queue priority. By default, one low-priority worker
also warms the persistent cache for artwork from all currently loaded global
Search rows; it does not load unseen pagination or subscription feeds.
Validated original image bytes are cached across restarts in
`~/.config/youta/thumbnail-cache` (or the selected Youta configuration
directory). The private cache expires entries after 30 days and evicts its
oldest files above 512 entries or 64 MiB. Corrupt entries are discarded and
fetched again. The image URL is never printed as detail-panel text or stored as
a filename.

On a Linux virtual console, serial terminal, `TERM=dumb`, or a terminal without
one of those image protocols, Youta neither fetches nor renders artwork. The
detail panel remains text-only. Accepted remote images are limited to bounded
JPEG, PNG, and WebP input before decoding, which prevents unbounded downloads
and image allocations. Remote image fetches reject non-public literal and
DNS-resolved addresses, `.local`, `.internal`, and single-label hosts;
redirects are not followed. These gates avoid stray escape sequences and
reduce network traffic, decoding work, memory use, heat, and battery
consumption.

Configure the runtime policy in `~/.config/youta/config.toml`:

```toml
[ui]
thumbnails = 'auto' # auto, off, or on
thumbnail_height = 20 # maximum terminal rows; minimum 4
prefetch_search_thumbnails = true
```

`auto` uses conservative protocol detection, `off` disables thumbnail requests
and rendering, and `on` attempts supported terminal artwork but still falls
back without fetching when no supported protocol is available. Thumbnail
height defaults to 20 rows and is reduced automatically when the Details panel
needs space for metadata, links, or description text.
`prefetch_search_thumbnails = false` disables background warming for global
YouTube and YouTube Music search results; the equivalent environment override
is `YOUTA_UI__PREFETCH_SEARCH_THUMBNAILS=false`. Previously learned channel
artwork for local subscriptions is warmed independently, so moving between
known channels can reuse the persistent cache without a foreground network
request. Unsupported terminals and plain TTYs perform no thumbnail network
work regardless of this preference. To exclude the renderer and its image
dependencies from the binary, use
`--no-default-features` and omit `thumbnails` from `--features`; include
`thumbnails` explicitly in a custom feature list to restore it. The rendering
integration uses
[`ratatui-image`](https://docs.rs/ratatui-image/11.0.6/ratatui_image/).

## Mouse input on a Linux virtual console

The default build includes the small `gpm` feature. When Youta is attached
directly to `/dev/ttyN`, it opportunistically connects to an already-running
[GPM](https://www.nico.schottelius.org/software/gpm/) daemon through
`/dev/gpmctl`. Move, press, release, drag, and wheel packets use the same
hitboxes and actions as Crossterm mouse events. The client is safe Rust, waits
for descriptor readiness instead of polling in a loop, and does not link
`libgpm`; therefore enabling it adds no mandatory system library or daemon
dependency. A missing or inaccessible socket falls back silently to keyboard
input.

Youta does not open GPM from `/dev/pts/*`, so terminal emulators retain their
normal mouse-capture behavior. `F8` provides a keyboard pointer on every
terminal: arrow keys move its reversed cell cursor, `Enter` clicks the current
cell, and `Esc` or `F8` exits. This remains available when GPM is not installed
or not running. Minimal builds can omit the Linux-console client with
`--no-default-features` or by leaving `gpm` out of their feature list. See the
[GPM protocol definitions](https://sources.debian.org/src/gpm/1.20.7-12/src/headers/gpm.h/)
for the control-socket contract.

## Local playlists and `todo`

Youta stores playlists in `~/.config/youta/state/playlists.toml` with the
default human-readable backend, or in `~/.config/youta/state.sqlite3` when the
optional SQLite backend is selected. A playlist has a required name, an
optional editable description, and ordered media entries. It stores stable
replay information rather than copying a local file or persisting an expiring
remote stream URL.

Playlist actions appear only when the current selection can be replayed. This
includes YouTube videos, YouTube Music and Bandcamp tracks, Apple Podcasts
episodes, and supported local media:

| Key | Action |
| --- | --- |
| `l` | Toggle the selected item in the persistent built-in `todo` playlist. |
| `P` | Open the playlist chooser for the selected item. |
| `j` / `k` or `↓` / `↑` | Move through the open chooser. |
| `Enter` | Add to or remove from the selected playlist without closing the chooser. |
| `n` | Open the new-playlist form from the chooser. |
| `Esc` | Return from the form to the chooser, or close the chooser. |

The new-playlist form requires a name and accepts an optional description.
`Tab`, `Shift+Tab`, `↑`, and `↓` switch fields; `Enter` creates the playlist
and adds the original item. Validation failures remain in the form so the
draft can be corrected.

Details shows `Playlists: name1, name2` only when the selected item belongs to
one or more playlists. The line wraps with the Details panel and remains
selectable in Details text-selection mode.

Open the **Playlists** tab with `F4` or normal tab navigation. `Enter` opens the
selected playlist; another `Enter` replays its selected item, and `Backspace`
returns to the playlist index. Local entries replay their original file when
it still exists. Remote entries resolve a fresh stream from their saved
canonical public page.

On the playlist index, `e` opens the same name-and-description editor. The
built-in `todo` playlist can also be renamed or described, but its internal
identity is fixed: `l` continues to target it after a rename.

## Subscriptions and local data

OPML is the interchange format for RSS/podcast feeds and compatible channel
feed URLs. It makes migration possible without a Youta-specific conversion.
Private notes, folders, bookmarks, playback positions, and provider IDs do
not fit OPML reliably, so they remain in the selected state backend and can be
exported separately.

At the Subscriptions source root, `[a] Add RSS feed` accepts an absolute
HTTP(S) RSS or Atom URL without an embedded username or password. Youta removes
the URL fragment and saves the subscription to the private portable OPML file
shown in the popup. Query parameters are preserved because some private feeds
use them for access; the popup redacts the draft URL from debug output. This
currently saves and displays the source subscription only—RSS episode browsing
inside the Subscriptions screen is not implemented yet.

YouTube subscriptions are currently local-only channel subscriptions. Choosing
`Subscribe (locally)` while a video is selected adds its channel to Youta's
OPML-backed source list; it does not subscribe the signed-in YouTube account.
OAuth-based synchronization remains roadmap work. In Details,
`[O] xdg-open channel` opens the selected YouTube channel's webpage, while
lowercase `[o] xdg-open video` opens the selected video's webpage. Youta waits
for the system opener's exit status before reporting success; a missing browser
association or headless-session failure is shown as a diagnostic instead.

Selecting a YouTube channel lazily loads its description, subscriber count,
joined date, public video count, aggregate public views, and country when those
fields are available. The configured official API or Invidious adapter remains
the primary metadata source. A separate best-effort request to the channel's
public About page can fill missing fields and add the websites and social
profiles advertised by the channel owner, including Telegram, Facebook,
X/Twitter, TikTok, Instagram, YouTube, and other website links. This request
uses no account, cookie, or API key; if YouTube omits a field, changes the page,
or rejects the public request, Youta keeps the primary provider result and
omits the unavailable field instead of showing an error placeholder.

Full selected-channel profiles and their external links use a bounded
process-local RAM cache, so revisiting a channel during the same run does not
repeat the About-page request. The compact channel summary continues to use
Youta's existing persistent metadata cache; the richer country, aggregate-view,
and link data is fetched again after a restart.

`Tab` cycles forward through every enabled top-level screen, while `Shift+Tab`
cycles backward; both wrap at the ends. `Ctrl+Tab` and `Ctrl+Shift+Tab` are
aliases when the terminal reports those combinations distinctly. Uppercase
`S` is the global Subscriptions shortcut and always returns to the
subscription-source root. Youta provides two layouts:

- `drill-down` is the default for narrow terminals. Sources appear on the
  left with channel information on the right. Press `Enter` to activate the
  selected YouTube channel, render any restart snapshot, and refresh its videos
  in the usual list-and-Details view; `Backspace` or `Esc` returns to the source
  list. While inside the channel, `[R] Refresh videos` requests its first page
  again.
- `split` keeps sources on the left and the selected source's videos on the
  right. Moving across sources uses only cached rows and makes no provider
  request; press `Enter` to activate the source, loading it initially or
  refreshing its cached first page, and move into its videos. The
  `[i] Description` button expands the selected video's Details; `[i]` or
  `Esc` returns to the video list. `[R] Refresh videos` is available after the
  source has been opened.

Refresh deliberately bypasses the process-local channel-list cache so newly
published videos can appear. The current rows remain visible while the request
runs, and Youta restores the selected video by provider ID when it is still in
the refreshed result; a refresh failure also leaves the existing rows intact.

Open the current in-app preferences with `[p] Preferences` or `F7`, choose
Drill-down or Split, choose whether exact `Реклама` chapters are hidden and
skipped, choose whether selected YouTube audio is prepared, choose whether
Local folder sizes are measured, and press `Enter` to save. These preferences
can be configured directly:

```toml
[playback]
autoplay = false
youtube_prewarm = true
skip_advertisement_chapters = true

[ui]
subscriptions_layout = 'drill-down' # drill-down or split
show_local_folder_sizes = true
```

`YOUTA_UI__SUBSCRIPTIONS_LAYOUT=split` and
`YOUTA_PLAYBACK__AUTOPLAY=true` and
`YOUTA_PLAYBACK__YOUTUBE_PREWARM=false` and
`YOUTA_PLAYBACK__SKIP_ADVERTISEMENT_CHAPTERS=false` override the corresponding
TOML values. `YOUTA_UI__SHOW_LOCAL_FOLDER_SIZES=false` disables recursive size
work, hides cached folder sizes, and removes the Local size-sort control.
While any of these environment variables is present, the Preferences popup
shows the override and does not partially replace its draft in `config.toml`.

One Local visit schedules at most 256 folder measurements, with one request in
flight. A folder traversal inspects at most 25,000 entries to depth 64; a
bounded or failed traversal displays no partial value and is not retried for
60 seconds. Later visits rotate through folders that did not fit in the first
batch. Complete results use a 512-entry, 60-second RAM-only cache keyed by
path and filesystem identity; it is never written to disk.

Video pages are requested only after `Enter` activates the selected channel,
so moving through a long source list cannot spend API quota. The official
adapter resolves the channel's uploads playlist, calls
[`playlistItems.list`](https://developers.google.com/youtube/v3/docs/playlistItems/list),
then enriches the ordered rows through
[`videos.list`](https://developers.google.com/youtube/v3/docs/videos/list).
The alternative adapter uses the documented [Invidious channel-videos
endpoint](https://docs.invidious.io/api/channels_endpoint/). Youta loads
another page when selection approaches the current page's end. It keeps a
bounded, process-local cache of recently opened channels, so switching back
does not immediately repeat the request. It retains at most 24 channels and
250 videos per channel under a shared approximate 8 MiB heap budget; list
descriptions and thumbnails are compacted before caching.

A compact first-page snapshot also survives restarts in the selected cache
backend. Activating a channel renders that snapshot immediately, then refreshes
page one in the background so new videos appear and provider-deleted videos
disappear. Moving between sources with the Split layout's arrow navigation
remains request-free; `Enter` activates the source and starts the initial load
or refresh. Short-lived or signed direct stream URLs are never persisted in
this snapshot, so playback resolves a fresh stream from the canonical video
page. The detailed disk bounds are documented in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md#subscription-navigation-and-channel-videos).

Application-owned persistent state stays under `~/.config/youta/`; transient
IPC sockets may use the operating system's runtime directory. The Local tab's
explicit Rename, Move to Trash, and Move actions are the only operations that
mutate selected source entries. Downloads default to a Youta-owned subdirectory
rather than a media source folder.

On a successful graceful shutdown, `persistence.git_commit_on_change = true`
(the default) checks whether the configured Youta root is inside a Git
worktree. If it is, Youta runs `git add .` from that root, creates a
path-scoped commit named `Automatic state update` when Youta files changed,
and runs `git push`. It never pulls or merges. Set the option to `false` to
disable this behavior. Before invoking Git, Youta publishes its pending
playback checkpoint and session, completes controller shutdown, and releases
the state lock. A persistence failure skips Git synchronization.

When Youta first creates its root, its default `.gitignore` excludes
`secrets/`, caches, runtime snapshots, downloads, thumbnail data, SQLite files,
locks, and temporary state files. Existing Git ignore rules remain
authoritative during shutdown sync. Youta does not enforce a secret policy or
refuse the commit: users may edit or remove those rules and intentionally
version credentials, for example in a private repository. Git failures are
reported after the terminal is restored and do not roll back local state.

## Service roadmap

The roadmap is intentionally tiered:

1. **Core:** local audio/video, RSS and OPML, radio/BBC feeds, official
   YouTube metadata, Invidious, PeerTube, Funkwhale, direct
   Vimeo/RuTube/SoundCloud URLs, Apple Podcasts catalogue search, Bandcamp
   public track/album discovery and playback, tracker modules, generic
   `yt-dlp`, `mpv`, and search/history/queue/download state.
2. **Open-data integrations:** SponsorBlock, DeArrow, broader Wikidata
   discovery, Wikimedia Commons, Internet Archive, LibriVox, Podcast Index,
   and gpodder.net.
3. **Authenticated integrations:** YouTube OAuth interactions, including
   bidirectional local/YouTube subscription sync, Last.fm, Discord,
   ListenBrainz, Google Drive, WebDAV, SSH, and optional one-way backups.
4. **Experimental adapters:** Odysee, Rumble, Bilibili, Telegram, Yandex
   services, VK, cloud.mail.ru, 4duk, knizhnyvoz, archive files, and
   torrent-backed sources.

Additional proprietary or scraper-dependent providers are not promised until
an adapter has tests, documented authentication, rate limiting, and a
maintenance owner. The implemented Bandcamp public-page adapter remains
best-effort and makes no stability or authenticated-access claim.
RuTracker/torrent support is a separate build feature and must remain stopped
when Youta exits. Youta will not bypass access controls or digital-rights
management.

Useful future open/self-hosted sources include
[Audiobookshelf](https://www.audiobookshelf.org/),
[OpenSubsonic](https://opensubsonic.netlify.app/),
[ListenBrainz](https://listenbrainz.org/),
[MusicBrainz](https://musicbrainz.org/doc/MusicBrainz_API).

Tracker results are downloaded and inspected into Youta's bounded private
cache before playback; compressed payloads are never passed to `yt-dlp`.
Playback depends on
[libopenmpt](https://lib.openmpt.org/libopenmpt/documentation/), normally
through FFmpeg/mpv. The Mod Archive API key is never bundled; users request and
store their own key.
See the [tracker archive matrix](docs/FEASIBILITY.md#tracker-music) before
enabling another catalog: several archives have no supported API, and Mirsoft
has no HTTPS endpoint.

## Wikimedia Commons transfer

A future transfer action is shown only when source metadata reports a
Commons-compatible license, and still requires user review. A YouTube Creative
Commons marker is not proof that the uploader owned every element. Youta will:

1. show the source license and attribution;
2. check Commons by normalized source URL, proposed name, and content hash;
3. prefer Ogg Opus for audio and WebM with VP9/AV1 plus Opus for video;
4. collect title, optional caption, optional description, source URL,
   attribution, structured-data statements, and categories;
5. keep suggested categories editable and append `Uploaded by Youta` after a
   blank line;
6. upload only after an explicit confirmation, then display the resulting
   Commons file URL.

Commons accepts more formats than only Opus, VP9, and AV1; those are Youta's
preferred open output profiles. Consult [Commons file
types](https://commons.wikimedia.org/wiki/Commons:File_types),
[YouTube files on Commons](https://commons.wikimedia.org/wiki/Commons:YouTube_files),
and the [MediaWiki upload API](https://www.mediawiki.org/wiki/API:Upload).

Internet Archive transfer follows the same explicit-confirmation and duplicate
check model and is also restricted to material the user may lawfully upload.

## Diagnostics and issue review

Recoverable operational errors open a scrollable report containing the Youta
version, operating-system identity, enabled build features, exact Rust
dependency versions, configured helper paths, the error chain, and a forced
backtrace. Tokens, URL credentials and query strings, authorization headers,
environment contents, and home-directory paths are redacted or omitted.
Helper-version processes are never launched at startup. Recoverable TUI
reports lazily probe the configured `mpv` and `yt-dlp` concurrently; fatal CLI
and TUI reports also probe `ffmpeg` and `ffprobe`. Every probe uses fixed
version arguments and an independent 1.5-second deadline.

The popup always offers separate `Copy` and `Copy + open issue` actions. When
`gh` is installed, it additionally offers `Fill GitHub issue`. Both issue
actions open an editor for review and never submit automatically. The complete
report travels through a helper's standard input or the terminal's OSC 52
clipboard protocol; the fallback browser URL contains only a short bounded
title and paste instruction.

The normal `release` profile keeps panic unwinding and line-table symbols so a
panic can restore terminal state and produce useful frames. The optional
`release-small` profile strips symbols and aborts on panic to minimize the
binary; that explicit size tradeoff weakens panic diagnostics and cannot
guarantee terminal cleanup after a panic.

## Audiophiles

Youta aims for predictable, bit-transparent-capable playback, not magic sound
claims. The `mpv` backend can select an explicit ALSA device and preserve the
source sample format where the device accepts it. Equalization, speed changes,
volume DSP, channel conversion, and sample-rate conversion are never
bit-perfect and must be visible in status.

Youta does not change CPU governors, real-time priorities, kernel parameters,
or power settings. Pinning a CPU frequency is hardware-dependent, can increase
heat and fan noise, and does not by itself improve decoded PCM. Measure
dropouts and scheduling latency before changing a system. Detailed, reversible
guidance is in [docs/AUDIOPHILE.md](docs/AUDIOPHILE.md).

## Packaging and quality

Every pushed revision and pull request runs formatting, Clippy, Rustdoc,
deterministic tests with default, no-default, and all features, an explicit
terminal end-to-end target, and a 70% minimum line-coverage gate. It also runs
required live Apple Podcasts, keyless YouTube Music, Wikidata, and public Radio
jobs; a newer push does not cancel the older revision's suite. Clippy blocks
compiler hygiene plus its correctness, suspicious-code, and performance groups; style,
complexity, and pedantic findings remain visible as advisory output while that
backlog is reduced in focused changes. Apple Podcasts is checked from public
Apple metadata through its RSS enclosure and silent audio decode. YouTube Music
is checked through yt-dlp's public songs search with a 15-second process bound
and no Google API key. Wikidata is checked through a live exact P1651 lookup.
Each enabled live job retries once for a transient network failure; a second
failure fails CI. Tagged releases build on native amd64 and arm64 runners and
publish:

- a binary archive for each architecture; and
- a Cargo vendor archive for offline/external build systems.

Live YouTube playback is temporarily excluded from automatic hosted CI because
YouTube returns `LOGIN_REQUIRED` for GitHub-hosted runner addresses even with
the account-free [bgutil PO-token
provider](https://github.com/Brainicism/bgutil-ytdlp-pot-provider). The
disabled job remains in the workflow so it can be re-enabled when that path is
reliable by setting the repository Actions variable `RUN_LIVE_YOUTUBE` to
`true`; it does not use a Google account or cookies. Until then,
`scripts/test-live-youtube.sh` is the required local pre-commit check. It
exercises Youta's production mpv/yt-dlp integration and decodes a short segment
through mpv's null audio output. This setup follows yt-dlp's [PO-token
guidance](https://github.com/yt-dlp/yt-dlp/wiki/PO-Token-Guide), whose provider
documentation notes that not every runner address can be accepted.

The changing public channel About-page parser also has an account-free,
opt-in live check:

```sh
YOUTA_RUN_LIVE_YOUTUBE_CHANNEL_TEST=1 cargo test --locked --test live_services --all-features -- --ignored --exact youtube_channel_about_profile_is_usable --nocapture
```

Run the same keyless YouTube Music search check locally with:

```sh
YOUTA_RUN_LIVE_YOUTUBE_MUSIC_TEST=1 cargo test --locked --test live_services --no-default-features --features youtube-music -- --ignored --exact youtube_music_keyless_search_returns_playable_tracks_before_timeout --nocapture
```

Run the Radio smoke locally to decode a real HTTPS stream through Youta's
`mpv` backend, observe real ICY metadata, and parse 4duk's bounded public
now-playing response:

```sh
YOUTA_RUN_LIVE_RADIO_TEST=1 cargo test --locked --test live_services --no-default-features --features radio,backend-mpv -- --ignored --exact radio_stream_and_passive_metadata_are_usable --nocapture
```

The Gentoo ebuild in `packaging/gentoo/` maps provider choices to USE flags and
consumes the release vendor archive. GitHub Actions use Node 24-based action
majors and set the maximum requested job timeout to 360 minutes.

To produce the same artifacts locally:

```sh
scripts/package-release.sh
scripts/package-vendor.sh
```

Before each commit, run the live YouTube playback check locally without sending
audio to a device:

```sh
scripts/test-live-youtube.sh
```

Pass `--audible` to hear the test through the default output. The default
fixture is the Blender Foundation's Creative Commons-licensed *Big Buck Bunny*
upload. `YOUTA_LIVE_YOUTUBE_URL` can select another public YouTube URL.

## License

Youta is licensed under the [MIT License](LICENSE).

## Similar terminal YouTube projects

- [youtube-tui](https://github.com/Siriusmart/youtube-tui) is a Rust TUI for
  browsing YouTube videos, channels, and playlists, with filters, history,
  subscriptions, and external or embedded `mpv` playback.
- [GopherTube](https://github.com/KrishnaSSH/gophertube) is a Go TUI for
  searching, watching, and downloading YouTube videos through `mpv`, `yt-dlp`,
  and `chafa`.
- [invidtui](https://github.com/darkhz/invidtui) is a Go TUI backed by
  Invidious instances, with audio and video playback, browsing, downloads, and
  Invidious account feeds, playlists, and subscriptions.
- [YTerMusic](https://github.com/ccgauche/ytermusic) is a Rust YouTube Music
  TUI focused on playlists and Supermix, caching, offline playback, and
  background downloads.
- [Feather](https://github.com/13unk0wn/Feather) is an early-development Rust
  and Ratatui YouTube Music player that uses `yt-dlp` and `mpv`.
- [yewtube](https://github.com/mps-youtube/yewtube) is a Python terminal
  YouTube player and downloader with search, local and YouTube playlists,
  comments, and support for external players.
- [terminal-yt](https://github.com/jooooscha/terminal-yt) is a
  Newsboat-inspired Rust TUI that reads YouTube RSS/Atom subscriptions, marks
  videos as played, and opens them in a configurable external player.
- [ytfzf](https://github.com/pystardust/ytfzf) is a POSIX and `fzf`-based
  search, watch, and download frontend with thumbnails, subscriptions, and
  history; its upstream repository says it is no longer actively maintained.
