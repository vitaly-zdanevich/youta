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

![Youta logo](gui/icons/icon.png)

Youta is a low-resource terminal YouTube audio player and subscription manager
written in Rust. It saves and shows listening progress. Subscriptions are
currently stored and managed locally; YouTube-account synchronization is not
implemented yet. Youta uses an invisible `mpv` process for playback,
communicates with it over JSON IPC, and can use `yt-dlp` as an explicitly
enabled media resolver and downloader. The terminal UI remains the only visible
interface: its seek bar, queue, volume, pause state, and hotkeys control `mpv`.

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
  after known sizes. `Enter` opens a folder, `Esc` returns to its parent while
  reselecting the folder just left, and `PageUp`/`PageDown` move by the visible
  Local page. Selecting media shows filename metadata immediately while
  tags and bounded `ffprobe` codec/container details load off the TUI thread;
  completed records remain in a fixed-size RAM cache for fast revisits. When
  terminal images are enabled, selecting a finite local video lazily extracts
  its midpoint frame through a bounded `ffmpeg` worker and reuses the persistent
  thumbnail cache on later visits.
  `[w]` lazily generates a waveform for a local audio or video file with the
  existing `ffmpeg` helper and replaces the normal seek bar without hiding
  Details. Peak extraction is cancellable, runs outside the UI thread, aligns
  delayed or shorter audio with the whole media timeline, skips mathematically
  inevitable intermediate compactions for long files, and retains only a
  bounded min/max envelope in RAM; clicking any waveform row starts or seeks
  the exact selected file at that position.
  For an audio file, `[f] Fingerprint` explicitly runs Chromaprint's official
  `fpcalc` helper off the UI thread and submits only its encoded fingerprint
  and duration to [AcoustID](https://acoustid.org/). Ranked
  [MusicBrainz](https://musicbrainz.org/) recording links are cached in bounded
  RAM by file identity; the best match is also offered to Wikidata enrichment
  through [MusicBrainz recording ID (P4404)](https://www.wikidata.org/wiki/Property:P4404)
  when that feature is enabled. Once the recording's Wikidata link is visible,
  the optional `lastfm` adapter follows its performer to
  [Last.fm ID (P3192)](https://www.wikidata.org/wiki/Property:P3192) and requests
  the artist's full public `/+wiki` biography as a separate best-effort step.
  Biography text and its attribution link remain in the same identity-bound
  RAM cache; Last.fm errors do not delay or remove the Wikidata result.
  Changing the selection cancels obsolete fingerprint work, and Youta never
  scans or uploads local media automatically.
  A conservative display-only fallback repairs strong Windows-1251 text that
  legacy MP3 tags incorrectly declare as Latin-1; Unicode tags and media files
  are never rewritten.
- Optional providers are isolated behind Cargo features, so a local/RSS-only
  build does not need YouTube or cloud integrations.
- A plain Linux TTY is a primary target. A confirmed local `/dev/ttyN` can use
  Unicode half-block thumbnails; unsupported, remote, and serial terminals
  remain text-only.

See [Architecture](docs/ARCHITECTURE.md), [feasibility and service
tiers](docs/FEASIBILITY.md), and [audiophile guidance](docs/AUDIOPHILE.md).

## The `mpv` backend and the TUI

Yes: external `mpv` still plays through the same Youta TUI and seek bar.
Youta starts `mpv` without a window or terminal input and controls it through a
private IPC socket. Playback position, duration, pause, volume, end-of-file,
and errors flow back into Youta's state. Seeking from keys, mouse clicks,
chapters, or a local waveform sends IPC commands to the same player process.
The backend requires `mpv` 0.38 or newer so resume positions and extractor
options can be applied atomically through `loadfile` per-file options.

Youta prepares the selected YouTube video's audio by default so `Enter` can
start playback without first waiting for a complete foreground resolution.
Selection must remain unchanged for 200 ms before one bounded worker invokes
`yt-dlp`; moving through the result list therefore cancels stale work instead
of resolving every row. During playback, the same worker prepares exactly one
known next YouTube queue or autoplay item during the final 30 seconds. This
late look-ahead avoids aging signed URLs during long videos and performs no
work when autoplay is disabled and the queue has no next item. Short-lived
signed media URLs and their HTTP headers remain in RAM only, are never written
to session state, history, or configuration, and are redacted from debug and
diagnostic output. If the prepared URL is absent, expired, or fails before
audible playback begins, Youta falls back to the video's canonical YouTube URL
and the normal `yt-dlp`/`mpv` path.
Disable this with
`playback.youtube_prewarm = false`, `[y] Prepare selected YouTube audio` in
Preferences, or `YOUTA_PLAYBACK__YOUTUBE_PREWARM=false`.

`[A] Autoplay` is off by default and persists its state in
`playback.autoplay`. When enabled, EOF advances through the same YouTube,
YouTube Music, subscription-channel, Local, Downloaded, playlist, or
MOD/tracker list. Items added with **Play next** or **Add to queue** always run
first; Youta then resumes the original source list. Replacing a live search
stops that list's continuation instead of accidentally playing an unrelated
new result. Playlist entries whose replay needs a provider round-trip
(Bandcamp, Apple Podcasts, BBC, SoundStream, LitRes, Jamendo) are skipped by
continuation, the way scheduled YouTube rows are: continuation only starts
what it can start directly. The same-source position is tracked even while
autoplay is off, so a manual skip can use it; the toggle decides only whether
end-of-file continues on its own.

`u` opens that queue. It lists the entries in play order, marks the one
playback is on, and starts the selected entry from where it sits, drops a
single entry, or clears everything except what is playing. The entry that is
playing cannot be removed — stop it first — because the queue would otherwise
stop describing what you are listening to. The list is rebuilt on every tick,
so it keeps up with entries the user did not add: reaching the end of a track
moves the cursor, and starting playback records an entry beside it.

`{` and `}` step to the previous and next entry without opening it, the way a
media key or a tray menu does. They are the shifted neighbours of `[` and `]`
for the next size up: those move within one item, these move between them.
Repeat-one is not consulted, because somebody who asked for the next track has
already said what they want. At either end of the queue the step continues
into the same-source list playback started from, backward as well as forward,
whether or not autoplay is enabled — that toggle governs only what end-of-file
does on its own. The crossed-into item is recorded as a queue entry exactly as
end-of-file continuation records one. A missing, replaced, or exhausted list
is a stated refusal rather than a wrap-around.

`y` copies the selected item's link to the system clipboard. The controller
only decides *what* to copy; each front-end reaches its own clipboard, because
the two are genuinely different. The terminal uses a native helper
(`wl-copy`, `xclip`, `xsel`, `pbcopy`) and falls back to an OSC 52 escape
written to its own tty; the window uses the platform clipboard directly, since
it has no tty and an escape there would be written into nothing and then
reported as a successful copy.

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
- an experimental authenticated YandexMusic tab for account recommendations,
  music and podcast search, best-effort audiobook discovery, reactions, album
  browsing, and bounded batch downloads;
- an independent Bandcamp tab that searches public track and album pages and
  resolves only the selected release for explicit playback through `yt-dlp`;
- an independent Apple Podcasts tab that searches the public, unauthenticated
  Apple catalogue by storefront and lazily loads playable episode metadata;
- an independent LibriVox tab for public-domain audiobook discovery, book and
  author navigation, chapter playback, genres, and public-page keywords;
- an account-free Radio tab backed by a static, zero-startup-network catalogue
  of direct public streams;
- lazy Wikidata enrichment for exact YouTube, SoundCloud, Bilibili, LibriVox
  author, and fingerprint-derived MusicBrainz external identifiers;
- supervised, argument-safe `mpv` JSON IPC and `yt-dlp` metadata/download
  commands;
- `doctor` and configuration inspection commands;
- parsing foundations for optional SponsorBlock and DeArrow data.

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
- **BBC Radio** adds the stable services exposed by the public
  [BBC Sounds station directory](https://www.bbc.co.uk/sounds/stations) to the
  Radio tab. On each explicit Play action, Youta reads the public station page,
  asks BBC Media Selector for the highest HTTPS HLS or DASH audio profile
  offered to the current region, derives quality from that current manifest,
  and passes the action-scoped manifest directly to `mpv`. The last resolved
  quality label for each station is cached in RAM so it remains visible during
  the process. Signed playback tokens and manifests are not reused for a later
  Play action or persisted. BBC podcast feeds remain importable through
  RSS/OPML. The `bbc-radio` feature enables the shared `radio` feature.
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
- **LibriVox** is a credential-free first-class source backed by the public
  [LibriVox API](https://librivox.org/api/info). The default tab shows one
  bounded catalogue page, while `/` searches books independently from
  the other providers. Enter opens one book and its playable chapter list;
  author links open that exact LibriVox author's books inside Youta. Large
  bibliographies use explicit 20-book continuation pages, so opening a prolific
  author never starts one unbounded catalogue download. Book descriptions,
  genres, readers, duration, chapter metadata, and cover art come from the
  bounded API response. The API does not expose a book's public
  keywords, so Youta enriches only the selected book from its bounded canonical
  LibriVox page and renders those readable keyword links without making a
  request for every result row.

  LibriVox describes its recordings as public domain in the United States.
  Copyright terms differ by country, and a catalog entry is not a blanket
  claim that its source text or recording may be redistributed everywhere;
  Youta preserves the canonical book and recording links rather than extending
  that status to another jurisdiction. The `librivox` Cargo feature is included
  in the normal `app`/`app-core` profiles and can be omitted from a custom
  `--no-default-features` build. When `wikidata` is also enabled, author
  enrichment uses only the exact
  [LibriVox author ID (P1899)](https://www.wikidata.org/wiki/Property:P1899),
  never a name or book-title guess.
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
backend. Live streams do not restore or save a playback position. When `mpv`
reports a cached seekable range, Youta shows that rolling buffer as a bounded
seek bar: mouse clicks, left/right, and number keys seek only inside bytes
already retained by `mpv`. A station without a reported cache range stays
non-seekable, and synthetic multi-day stream timestamps are never displayed.
Repeat remains disabled. Live streams remain marker-free as
`Radio · live` entries in
History, `todo`, and other playlists. Listening time still contributes to the
Radio total on the Stats screen.

`[/] Search` is a zero-network live filter on this tab: every typed character
immediately narrows the catalogue. Whitespace-separated terms match station
names, summaries, formats, bitrates, sample rates, and channel layouts, so
queries such as `flac`, `aac 320`, and `44100 stereo` work. `Enter` accepts the
current filter, `Esc` restores the filter and station selected before editing,
and Backspace broadens the list immediately.

The station catalogue is maintained in the
[curated preset source](src/providers/radio.rs) and the checked-in
[generated NPR snapshot](src/providers/npr_stations_generated.rs), rather than
duplicated in this README. It covers public-service, regional, talk, music,
ambient, classical, soundtrack, game-music, meditation, and lossless FLAC
streams. Curated presets hardcode only codec, stream-reported or directly
probed bitrate, sample rate, and channel fields verified from a reviewed source
or bounded maintenance probe. Missing fields remain unknown. FLAC has no fixed
encoded bitrate, so a verified FLAC stream without a trustworthy numeric rate
is shown as `variable bitrate`; Opus and Vorbis remain unknown unless their rate
mode is explicitly verified.

The NPR snapshot is generated from NPR's official
[station finder](https://www.npr.org/stations), includes primary and additional
services, and deduplicates inherited transmitters by stream GUID. NPR publishes
no bitrate, sample-rate, or channel fields in that directory, so Youta does not
invent them. Verified NPR quality is populated only by the generator's explicit,
bounded [`ffprobe`](https://ffmpeg.org/ffprobe.html) maintenance mode and stored
in the checked-in
[quality sidecar](src/providers/npr_station_quality_generated.json). Normal
startup and playback never launch `ffprobe` to discover station quality.

The unpaginated NPR API has no complete-enumeration contract; the checked-in
count describes a dated state-and-territory snapshot rather than a permanent
total. Short-lived signed stream URLs are omitted, while static PLS/M3U entry
points are resolved during generation. The maintenance probe tries each stable
advertised alternative before treating a service as unresolved. A failed
quality probe alone never removes a station.

Exact stream URLs that repeatedly fail both metadata and playback checks can be
recorded with a review date and reason in the quality sidecar. Normal generation
omits only that exact service URL. If NPR replaces the URL, the station
automatically returns for verification; a later successful explicit probe also
clears the exclusion. Regenerate the snapshot, probe quality, and review both
generated files with:

```sh
cargo run --locked --example update_npr_stations --features radio -- \
	--probe-quality --probe-date "$(date -u +%F)"
```

A regeneration without `--probe-quality` reuses matching verified sidecar
records and performs no stream-quality probes. See NPR's
[Terms of Use](https://www.npr.org/about-npr/179876898/terms-of-use).

When a station explicitly publishes a suitable 128 kbps AAC alternative, the
bundled preset can prefer it over a larger MP3 stream. That generally improves
compression efficiency, but does not guarantee higher fidelity than a 192,
256, or 320 kbps MP3 stream. Encoder quality and the station's source chain
still matter; Youta does not choose a 64 or 48 kbps AAC alternative merely
because it uses AAC.

Details shows only quality attributes known for that preset, the readable
playback endpoint, and a summary. `[O] xdg-open · <URL>` on Linux—or
`[O] open · <URL>` on macOS—is the sole station website row and opens the
homepage rather than the audio endpoint. The same stable station identity is
used for playlists, History replay, private station
notes, and the now-playing click target; transient redirects are never
persisted. `[B]` cycles name, high-to-low bitrate, and low-to-high bitrate order
while the selected station remains stable across ordering changes and
restarts. Routine two-channel streams omit the repetitive `stereo` label;
known mono or unusual multichannel streams still disclose that distinction.

Station ICY metadata observed by `mpv` can appear beside the stable station
title. Selected presets and generated NPR services also have bounded passive
metadata adapters. NPR's endpoint supplies the current programme when
available, not dependable song/artist metadata. Fresh provider data wins, ICY
is the playing fallback, and a failed refresh retains the last successful
value only as clearly stale selected-station details. Failures stay silent and
retry with a station-scoped capped 1/2/5/10-minute backoff, so an unavailable
service does not create an idle polling loop.

Some providers publish only plain-HTTP streams or HTTPS playlists that resolve
to plain-HTTP audio. Those presets remain enabled by default as requested, but
the transport is unauthenticated and can be observed or modified on the
network. Youta sends no credentials to them. Inclusion describes technical
public reachability, not an assertion that broadcast content is openly
licensed or reusable.

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
| LibriVox author | [LibriVox author ID (P1899)](https://www.wikidata.org/wiki/Property:P1899) |
| Fingerprinted local recording | [MusicBrainz recording ID (P4404)](https://www.wikidata.org/wiki/Property:P4404) |

Each response is limited to 512 KiB and 20 matches. Successful lookups are
cached in the selected persistence backend for seven days; successful empty
lookups are cached for 24 hours. Network and response errors are not
negative-cache entries.

Each matched entity appears once under External links as a collapsed
`[W] ▸` row. Activating that row lazily requests the entity's bounded,
human-readable statements plus canonical Wikipedia article sitelinks and
expands them in the scrollable Details pane. Statement values and Wikipedia
rows retain validated clickable targets. Activating `[W] ▾` collapses the
spoiler again. Entity data is not fetched for items the user never expands.

Radio stations use the same items for a second purpose: artwork. A station that
the checked-in [verified mapping](src/providers/radio_wikidata.rs) already links
to a Wikidata item has its logotype resolved when it is selected, from
[logo image (P154)](https://www.wikidata.org/wiki/Property:P154), falling back
to [image (P18)](https://www.wikidata.org/wiki/Property:P18) — a broadcaster's
logotype identifies the station, while its representative image is as likely to
be a transmitter mast. That takes two bounded requests rather than one, because
Commons' stable file address is a redirect and Youta's artwork agent refuses
redirects on purpose; the second asks Commons for the raster URL itself at a
bounded width, which also rasterizes an SVG logotype to PNG. One lookup runs at
a time and every answer is remembered for the session, including "this station
has no image", so moving through the catalogue costs at most one lookup per
station rather than one per selection.

Wikidata knows only the broadcasters notable enough to have an item, which is
about a tenth of Youta's catalogue: a hobby FLAC stream has no item to link to
and never will. The rest ask the station's own homepage, which already
advertises its logo to browsers and messaging apps. Youta reads one bounded page
and takes the first of `apple-touch-icon`, `og:image`, and a `rel="icon"` that
is a PNG, JPEG, or WebP — an ICO or SVG favicon is skipped because the artwork
pipeline cannot render one. The address requested is the compile-time homepage
from Youta's own curated catalogue, never anything a provider or a user
supplied; the address the page returns is untrusted and is validated exactly
like any other remote artwork URL — public host, no credentials, size-capped,
identified by its bytes — before it is fetched. Selecting a station therefore
contacts that station's website once per session. Builds without `wikidata`
simply start from the homepage.

This is exact-ID enrichment, not title, name, or arbitrary-URL matching.
YouTube video IDs come from validated links, bare IDs, or search results;
channel lookup requires the 24-character `UC…` ID and does not resolve handles
or custom names. SoundCloud accepts only one- or two-segment canonical
`soundcloud.com` account/track paths; for a track it checks both the exact
`account/track` value and the exact account value. SoundCloud short redirects
are not resolved. Bilibili accepts canonical `[www.]bilibili.com/video/{BV…}`
or `/video/{av…}` links and `space.bilibili.com/{numeric-UID}` links. It does
not resolve `b23.tv` or other redirect hosts before Wikidata lookup.
LibriVox enrichment accepts only the positive numeric author ID carried by the
catalogue; book titles and author display names are never used as entity keys.

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
It also expects Chromaprint's `fpcalc` only when an AcoustID key enables local
audio identification. On Gentoo emerge [media-libs/chromaprint](https://packages.gentoo.org/packages/media-libs/chromaprint) with USE flag `tools`. These remain separate executables so they can be updated without rebuilding Youta. Human-readable persistence is part of the core
build. The default feature set enables `images` and offline `qr` rendering.
Runtime capability checks decide whether the TUI may fetch and render artwork.

Install `fpcalc` from your operating system's
[Chromaprint](https://github.com/acoustid/chromaprint) tools package:

- Gentoo: `USE=tools emerge media-libs/chromaprint`
- Debian/Ubuntu: `apt install libchromaprint-tools`
- Fedora: `dnf install chromaprint-tools`
- macOS with Homebrew: `brew install chromaprint`

Build the complete application without image decoding, terminal-image
dependencies, or the optional Linux virtual-console mouse client with:

```sh
cargo build --release --locked --no-default-features \
	--features app,qr
```

The `app` profile includes the experimental YandexMusic adapter but does not
force GPM into distribution builds. Add `gpm` explicitly to either feature
list when Linux virtual-console mouse input is wanted. Build the same
application without Yandex Music code, and retain the default image support,
with:

```sh
cargo build --release --locked --no-default-features \
	--features app-core,images,qr
```

Omit `images` from that command for the Yandex-free text-only variant. Omit
`qr` to remove QR encoding and its shortcut from any custom build. Cargo
features are additive: `app-core` is the complete profile without
`yandex-music`, and `gpm` is the positive opt-in for virtual-console mouse
input. The ordinary default feature set still enables both Yandex Music and
GPM.

Both configurations use human-readable TOML persistence. SQLite is included
only when `sqlite-state` or `bundled-sqlite` is requested explicitly.

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
# OAuth access token issued for your Yandex Music account. This is not an API
# key; Youta never asks for or stores the account password.
yandex_music_token = '...'
# Create an application key at https://acoustid.org/api-key.
acoustid_client_key = '...'
```

`auto` prefers that key when the official adapter is compiled in, then falls
back to `invidious_base_url`. `official` and `invidious` select only that
backend. Both the TUI and `youta search` use this selection. The AcoustID key
enables the Local Details `[f] Fingerprint` action; `fpcalc_executable` in
`config.toml` can select a non-default Chromaprint helper path. `FFmpeg` and
`FFprobe` are named the same way, by `ffmpeg_executable` and
`ffprobe_executable`: the first draws local waveforms, extracts the midpoint
frame a local video is previewed by, and decodes tracker modules; the second
reads codec, bitrate, and exact duration for local media. Both default to the
bare name, which a Unix installation puts on `PATH`; a Windows build of FFmpeg
is usually unpacked rather than installed, so a full path is the normal setting
there. The Yandex
Music credential can instead be supplied for one process with
`YOUTA_PROVIDERS__YANDEX_MUSIC_TOKEN`; environment values take precedence over
the private credentials file.

For a small local-only build:

```sh
cargo build --release --no-default-features \
	--features tui,local,waveform,backend-mpv
```

This intentionally omits terminal thumbnails while retaining local waveform
generation. A custom
`--no-default-features` build must list `images` explicitly when artwork is
wanted.

### The desktop window

Youta also has a desktop window, in the `youta-gui` workspace crate. It is a
second front-end to the same reducer the terminal drives: the same state, the
same keyboard map, the same providers and playback engine. Neither front-end
replaces the other.

Its page is built with Vite, so it needs Node once before the Rust build:

```sh
npm --prefix gui/ui ci
npm --prefix gui/ui run build
cargo run --locked -p youta-gui
```

On Linux the window is WebKitGTK, so building it additionally needs
`libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libayatana-appindicator3-dev` for the
tray, and `libdbus-1-dev` for the media keys. WebKitGTK 2.40 is the floor —
that is the release that introduced the 4.1 API this depends on. Some
Nvidia and older Mesa configurations render the window as a blank or torn
surface until WebKitGTK's DMA-BUF path is turned off:

```sh
WEBKIT_DISABLE_DMABUF_RENDERER=1 youta-desktop
```

That is a WebKitGTK workaround rather than a Youta setting, and it is worth
trying first whenever the window appears but shows nothing.

Native desktop artifacts are built by `scripts/package-desktop.sh`. It
publishes a standalone GUI executable and whatever the host platform's bundler
can make — `.deb`, `.rpm` and AppImage on Linux, `.dmg` on macOS, an NSIS
installer on Windows — with a `.sha256` beside every file. Installers are not
cross-compiled; the release workflow runs that script once per host. Linux
i686 is the deliberate unbundled exception:
`scripts/package-desktop-executable.sh` cross-builds its GUI against Ubuntu's
i386 WebKitGTK/GTK/D-Bus packages with Tauri's production protocol enabled,
but does not claim that a cross-built installer is native. The resulting raw
executable remains dynamically linked; distribution packages must supply its
32-bit GUI libraries, as the Gentoo x86 ebuild does.

The installers are **not signed**, on any platform. macOS will refuse a
downloaded `.dmg` until it is opened through the right-click "Open" menu, and
Windows SmartScreen will warn about an unrecognised publisher. Signing is wired
into the release workflow and turns itself on the moment the maintainer adds
the certificate secrets; until then, unsigned is the honest state and this is
where it is written down. For the same reason the window carries **no automatic
updater**: an updater needs a signing key pair whose private half only the
maintainer can hold, and an endpoint to publish manifests to. Neither exists,
and shipping an update channel that nobody can sign for would be worse than
shipping none. Deep links — opening a `youta://` address from a browser — are
not registered either; they need the bundle that now exists plus a decision
about handing a URL to an already-running copy, which is its own change.

The page is embedded into the binary when the Rust crate compiles, so editing
the front-end means running both commands again: `npm --prefix gui/ui run build`
followed by `cargo build -p youta-gui`. Rebuilding only the page leaves the
running binary serving the assets it was compiled with.

The rest of the repository needs no JavaScript toolchain. `youta-gui` is not a
default workspace member, so `cargo build`, `cargo test`, and the lint gates
never touch it, and a Rust-only build of the window falls back to a placeholder
page that says what to run.

The window links no terminal code at all. It selects `controller` and `sources`
rather than `tui`, which is checked by `cargo tree -p youta-gui -i ratatui`
matching no package.

Subscriptions keeps both navigation models the terminal offers, because which
one is active is a saved preference the two front-ends share. Where the window
differs is width: it shows the source list, the item list, and the information
panel at once, which four terminal rows cannot. The panel follows the reducer —
a channel or a feed while a source is being chosen, the selected item once one
has been entered.

Details text selects and copies natively, so Ctrl-C is left to the web view
whenever something is selected rather than being claimed by Youta. Scrolling is
native too, but the reducer still owns the offset, because Home, End, PageDown
and Alt-u/d move it and only the reducer knows whether the panel has focus.

Everything the window renders arrives as JSON except the waveform. A
window-wide envelope is a few thousand sixteen-bit peaks, which JSON inflates by
roughly an order of magnitude, and it changes once per file rather than once per
frame — so the window asks for it as bytes, four per column, once per file and
once per resize. Rust reduces it to exactly the number of device pixels the
canvas will draw, using the same code the terminal draws its four rows with. The
request names a generation, and a generation the reducer no longer holds is
answered with nothing rather than with the current file's peaks: otherwise a
reply that outlived the selection would paint one file's envelope where another
belongs, and a click on those pixels would seek the wrong media.

Artwork is the other exception, and it never enters a snapshot either: the
window asks for it with an ordinary `<img src>` pointing at `youta://artwork/`,
and Rust answers with the bytes. That keeps the network in the player process,
so a provider sees Youta's guarded agent — public addresses only, no redirects,
size-capped — rather than a request from a web view.

Local covers reach the same endpoint but are trusted differently, because they
are real files: the cover extracted out of a download, or the image `yt-dlp`
left beside it. No path pattern separates the user's own `cover.jpg` from the
rest of their filesystem, so the endpoint does not try to invent one. It serves
a file only when the reducer itself published that URL in a snapshot the window
was given, remembering the last several selections so an image request that
outlives its selection still resolves. A `file:` URL arriving from a provider is
refused, because no snapshot ever named it.

Two things the reducer decides but cannot do itself are done here rather than
in it: copying to the clipboard, and opening a local text file. The window
reaches the platform clipboard directly and starts the system opener detached;
the terminal reaches a native helper or writes an OSC 52 escape to its own tty,
and can suspend itself so a terminal editor may take the console. The
controller supplies the text and the command, never the transport.

The window carries a menu bar, a tray icon, and a drop target — the three ways
of reaching Youta from outside its page. Every entry in the menu and the tray is
a semantic action: a menu item's identity *is* its serialized `UiAction`, so an
entry cannot name a command that does not exist and there is no second table to
drift. None of them carries a keyboard accelerator, because an accelerator is
resolved by the operating system before the page sees the key, while the shared
keyboard map resolves the same key against live modal state — `Space` as an
accelerator would pause playback while a space was typed into the search field.
The Edit submenu is the exception and is not decoration: its predefined items
are what give the web view working cut, copy, paste, and select-all, which is
how the natively selected Details text is actually copied.

The tray does not keep Youta alive. Closing the window still ends the process,
stops the player, and releases the durable-state lock; a tray that outlived its
window would be a second, invisible way to hold that lock. Its menu opens on an
ordinary click on every platform, because it exists to carry controls — and
every entry on it has to mean something with no list in sight, which is why it
carries the queue's neighbours rather than "queue the selected item next".

Dropping files or folders on the window shows them in Local: the folder itself
when a folder was dropped, otherwise the folder the first file lives in with
that file selected. Several files out of one folder therefore land on all of
them at once. Nothing is read per path — only the first is inspected and the
rest are counted — and the folder it opens is one the arrow keys could already
reach, so a drop opens no door Local does not already open.

The window title, the tray tooltip, and a track-change notification all read one
field, `now_playing`, which is the queue entry playback is on rather than
whatever the engine parsed out of the stream. Deriving it from the visible rows
would follow the cursor instead of the sound, since the playing item leaves the
list as soon as the user browses elsewhere. A notification is raised only when
the track changes while the window is *not* focused, and never for the first
snapshot after startup: a queue restored from durable state is not a track that
started. Titles reaching an operating system are bounded like every other piece
of provider text Youta shows.

The keyboard's media keys work too, through whatever the desktop uses for them:
MPRIS on Linux, the System Media Transport Controls on Windows, Now Playing on
macOS. Play and Pause name a destination rather than a change, so both are
answered against the reducer's live state instead of the last snapshot — a Play
arriving at something already playing does nothing rather than pausing it. Next
and Previous step through the *queue*, continuing into the playing source list
at its edges, which is also what those entries mean in the tray, where there is
no cursor for "queue the selected item" to refer to.
Youta has no stop, so the Stop button holds the item where it is; a dragged
position is refused rather than approximated while the running time is unknown;
and a URI arriving from the session bus is ignored, because Youta plays what its
providers resolved.

No cover art is published there. Every one of the three hands the URL to the
platform's own image loader, which would fetch a provider's thumbnail without
the guarded agent that the `youta://artwork/` endpoint exists to keep in front
of it. On Linux the media surface is MPRIS, which is to say any process on the
user's session bus can then ask Youta to pause or quit — the bargain every MPRIS
player makes, and no reach that running as the same user did not already grant.

The position is not pushed on every tick. macOS and Windows extrapolate elapsed
time from the last value and the rate, so they are told again only when playback
*jumps*; MPRIS answers `Position` with exactly what it was last told, so there
it is refreshed every second as well. Getting this wrong is measurable rather
than theoretical: souvlaki's macOS backend rebuilds and re-copies the whole
now-playing dictionary per call from a thread with no autorelease pool, and a
call costs 0.9 KiB that is never returned.

On Linux this needs `libdbus-1-dev` at build time, next to the WebKitGTK
development packages the window already requires.

A search field appears on every screen that collects a query and nowhere else;
both front-ends ask `Screen::search_verb` which those are, so a screen whose
Enter would answer "search is not available" is never given a field. It is not
a text input: clicking it asks the reducer to open its editor, and the typing,
Enter, and Escape that follow travel through the shared keyboard map, exactly
as `/` does in the terminal. The query, the insertion point, and the modal
precedence stay in `src/app.rs`, so the window displays an editor it does not
own.

Four editors are terminal-only: the YouTube API key, the Yandex Music OAuth
token, the RSS feed URL, and private notes. Their contents never leave the
player process, so the window cannot draw them; it receives one bit saying an
editor is open and shows a notice with a way out. Without that bit the window
would look like an ordinary screen that had stopped responding, because those
editors are modal and the keyboard map routes every key into them — and the
YouTube one opens by itself the first time a search runs without credentials.
Set those values in the terminal front-end or in the configuration files.

`images` is terminal artwork: it adds decoding and the graphics protocols on
top of `remote-artwork`, which is the fetching and private on-disk cache alone.
A build that wants artwork bytes without a terminal renderer — a different
front-end, or a tool — selects `remote-artwork` by itself and links no Ratatui.
`qr` is likewise renderer-free: it encodes a module matrix and draws nothing.

`local-artwork` is renderer-free for the same reason, and it is also offline:
finding a cover means reading tags and directory entries, so it links neither
Ratatui nor an HTTP client and a text-only local build stays exactly as
network-free as it was. It looks in two places. A picture embedded in the media
file is extracted under bounded limits and copied into the private artwork cache
under an opaque hashed name, so a renderer is never handed a byte range inside
the user's media. An image beside the file — `Track.webp` next to `Track.opus`,
which is what `yt-dlp --write-thumbnail` leaves behind, or `cover.jpg` in an
album folder — is published where it lies, because copying a large scan into a
4 MiB cache would only lose it. The embedded picture wins when a file has both:
it belongs to that file, while a sidecar may describe a whole download batch.
Both are identified by their leading bytes rather than by a file extension or a
tag's claimed MIME type, and neither is decoded here — pixel and allocation
limits belong to whichever renderer actually decodes, which is the only side
that knows what those limits are.

Downloaded rows carry their covers in the list itself, not only in the
information panel, because that is where a sidecar thumbnail is cheap: the whole
list comes from one directory, so one extra pass over it covers every row
instead of one lookup per row. Embedded pictures stay lazy and per selection,
since reading them means parsing each media file's tags.

For a small TUI build containing only the curated Radio catalogue and `mpv`
playback:

```sh
cargo run --release --locked --no-default-features \
	--features tui,radio,backend-mpv
```

For metadata through the official YouTube Data API instead of Invidious:

```sh
cargo build --release --no-default-features \
	--features tui,images,local,rss,youtube-official,backend-mpv
```

Copy [config.example.toml](config.example.toml) to
`~/.config/youta/config.toml`. Environment variables override file values;
nested keys use two underscores, for example:

```sh
YOUTA_UI__THEME=dark youta
YOUTA_PROVIDERS__YOUTUBE_API_KEY='...' youta search 'query'
```

Do not place tokens in shell history. The configuration
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

Press `n`, or activate the **Add private note** / **Edit private note** row in
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
  Selected videos expose the public comment count and a bounded, RAM-cached
  popup containing up to twenty relevance-ordered top-level comments through
  [`commentThreads.list`](https://developers.google.com/youtube/v3/docs/commentThreads/list).
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
  `providers.invidious_base_url`. It provides the same selected-video comment
  count and top-comments popup through the documented
  [`videos/:id` and `comments/:id` endpoints](https://docs.invidious.io/api/)
  without requiring an API key.
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
- The experimental **YandexMusic** tab uses Yandex Music's private client API,
  which is neither a documented public developer API nor a stability
  commitment from Yandex. It is isolated behind the `yandex-music` Cargo
  feature so distributors can omit the client and its signing dependencies.
  Normal builds include it through `app`; `app-core` is the complete
  Yandex-free application profile. An upstream API or authentication change
  may break this adapter independently of the rest of Youta.

  YandexMusic requires an OAuth access token already issued for the user's
  account. This credential is **not an API key** and is password-equivalent:
  Youta never asks for an account password and does not implement a token
  acquisition flow. Yandex documents the credential model in its
  [OAuth overview](https://yandex.com/dev/id/doc/en/concepts/ya-oauth-intro);
  that page does not document a public Yandex Music API or issue a Music token
  for Youta. Store an already-issued token in
  `~/.config/youta/secrets/credentials.toml`:

  ```toml
  [providers]
  yandex_music_token = '...'
  ```

  Alternatively, set `YOUTA_PROVIDERS__YANDEX_MUSIC_TOKEN` for the Youta
  process. Do not put a token in `config.toml`, command arguments, issue
  reports, or diagnostic output.

  The tab opens account recommendations by default and provides bounded search
  scopes for music, podcasts, and exact audiobook metadata. Albums can be
  opened and downloaded. My Wave makes at most four recommendation requests
  and displays up to twenty unique playable tracks, stopping early when the
  service adds no new track. When confirmed playback reaches the last retained
  track, Youta makes one guarded continuation request and appends only new
  tracks. Its twenty-track batch download is enabled only when the bounded
  responses contain all twenty tracks.
  Playback and downloads request the highest quality that the account,
  subscription tier, catalogue item, and region permit. A
  requested quality is not a promise of a particular codec or lossless tier.
  Likes and dislikes update the local desired state immediately. Failed or
  offline reactions stay in a durable outbox and are retried on startup and
  graceful shutdown without silently reversing the user's latest choice.

  Audiobook search is best-effort and may return no results. The inspected
  private clients expose no stable first-class audiobook search or playback
  contract, so Youta queries the generic catalogue and retains only rows whose
  exact API `type` or `metaType` identifies an audiobook or chapter. It never
  classifies one from its title, artist, genre, or description. A discovered
  row is not a promise that the Music API will expose playable media. Youta
  does not silently route the request through the separate Bookmate service or
  claim that podcast matches are audiobooks.

  For a selected track, artist, or album, Wikidata enrichment uses exact
  external identifiers where available and keeps the existing collapsed `[W]`
  details behavior. It does not guess an entity from a title-only,
  artist-name-only, or album-title-only match.
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
treated as secrets. In addition to yt-dlp's default Deno JavaScript runtime,
Youta enables QuickJS-ng as a lightweight fallback for platforms where Deno is
unavailable. Keep `yt-dlp` updated because extractor fixes and security fixes
ship frequently. See the upstream [FAQ](https://github.com/yt-dlp/yt-dlp/wiki/FAQ)
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

The default build includes the positive `images` feature. Youta renders the
selected item's artwork only when it detects the [Kitty graphics
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
a filename. Within one run, Youta also keeps up to 16 recently prepared
terminal images within a 16 MiB decoded-pixel budget. Returning from one local
file to an unchanged JPEG therefore reuses its encoded terminal image without
another decode or protocol-encoding pass. Local entries include a filesystem
fingerprint in that RAM key, so replacing an image at the same path invalidates
the prepared result.

A directly attached Linux virtual console (`TERM=linux`, with output resolved
to `/dev/ttyN`) uses Unicode half-block cells as a conservative artwork
fallback by default. The focused Preferences editor can disable this fallback
without changing image support in graphical terminals. This does not access
`/dev/fb0` or draw outside the terminal; image quality is limited by the
console font and palette. Serial terminals, SSH, `TERM=dumb`, a Linux-looking
PTY, and terminals without a supported graphics protocol remain text-only and
perform no thumbnail network work. Accepted remote images are limited to
bounded JPEG, PNG, and WebP input before decoding, which prevents unbounded
downloads and image allocations. Remote image fetches reject non-public literal
and DNS-resolved addresses, `.local`, `.internal`, and single-label hosts;
redirects are not followed. These gates avoid stray escape sequences and reduce
network traffic, decoding work, memory use, heat, and battery consumption.

On that confirmed physical console, Youta also hides external-opener controls
and ignores their hotkeys because no graphical session is attached. URLs remain
visible and selectable as text. Pseudo-terminals and SSH sessions retain the
controls because their opener may be configured on the host. Linux uses
`xdg-open`; macOS uses its native `open` command.

The default Linux virtual-console keymap reserves `Alt+Up` for a kernel action
and does not preserve the Alt modifier on `Alt+Down`. Terminal emulators keep
the usual `Alt+Up`/`Alt+Down` Details scrolling; on `/dev/ttyN`, use `Alt+u`
and `Alt+d` for the same line-by-line movement. During normal navigation, these
aliases work whenever Details are visible and do not require moving keyboard
focus into that pane.
Youta does not alter the system-wide console keymap or add an escape-sequence
timeout.

Configure the runtime policy in `~/.config/youta/config.toml`:

```toml
[ui]
thumbnails = 'auto' # auto, off, or on
youtube_thumbnail_size = 'automatic'
show_images_in_tty = true # physical Linux TTY half-block artwork
thumbnail_height = 20 # maximum terminal rows; minimum 4
prefetch_search_thumbnails = true
```

`auto` uses conservative protocol detection, `off` disables thumbnail requests
and rendering, and `on` attempts supported terminal artwork but still falls
back without fetching when no supported protocol is available.
`youtube_thumbnail_size` independently chooses the exact YouTube
video-thumbnail entry used by the normal Details preview:

- `automatic`: use `high` (480×360) through 1366 terminal-window pixels,
  `standard` (640×480) from 1367 through 1920 pixels, and `maxres` (1280×720)
  above 1920 pixels. If the terminal does not report a pixel width, use
  `standard`.
- `disabled`: do not fetch or render YouTube video thumbnails.
- `default`: 120×90.
- `medium`: 320×180.
- `high`: 480×360.
- `standard`: 640×480.
- `maxres`: 1280×720.

Explicit sizes are strict: if a video does not expose the selected entry,
Youta shows no video thumbnail and does not fetch another size as a fallback.
When that preview exists and terminal images are enabled, Youta also warms the
largest image explicitly advertised for the selected video. Clicking the
preview opens that cached or in-flight image across the terminal; it does not
replace the configured preview or prefetch maximum-resolution images for every
list row. Selecting `disabled` suppresses both the preview and expansion
request.
YouTube's 4:3 `default`, `high`, and `standard` JPEG canvases can contain
symmetric black bands around 16:9 artwork. Youta removes those bands only when
both expected edge regions are near-black; non-dark 4:3 images and non-YouTube
artwork retain their original composition.
The environment override is
`YOUTA_UI__YOUTUBE_THUMBNAIL_SIZE=standard`. Channel artwork and artwork from
other sources are unaffected by this YouTube-only setting.
`show_images_in_tty = false` disables only the physical Linux-console
half-block fallback; Kitty, iTerm2, and Sixel images remain governed by
`thumbnails`. Its environment override is
`YOUTA_UI__SHOW_IMAGES_IN_TTY=false`. Thumbnail height defaults to 20 rows and
is reduced automatically when the Details panel needs space for metadata,
links, or description text. YouTube video thumbnails instead expand to the
full Details-pane width at the selected entry's source aspect ratio when the description
occupies fewer than 15 wrapped rows or the terminal window itself is at least
1080 pixels tall. Youta reads the attached terminal window's pixel dimensions,
so a small window on a 1080p monitor does not trigger the height-based layout.
`prefetch_search_thumbnails = false` disables background warming for global
YouTube and YouTube Music search results; the equivalent environment override
is `YOUTA_UI__PREFETCH_SEARCH_THUMBNAILS=false`. Previously learned channel
artwork for local subscriptions is warmed independently, so moving between
known channels can reuse the persistent cache without a foreground network
request. Unsupported terminals perform no thumbnail network work regardless of
this preference. To exclude the renderer and its image
dependencies while retaining the other defaults, build with
`--no-default-features --features app,qr`. For a smaller custom build, omit
`images`; include it explicitly to restore rendering. The rendering integration
uses
[`ratatui-image`](https://docs.rs/ratatui-image/11.0.6/ratatui_image/).

## Mouse input on a Linux virtual console

The default build includes the small `gpm` feature. When Youta is attached
directly to `/dev/ttyN`, it opportunistically connects to an already-running
[GPM](https://www.nico.schottelius.org/software/gpm/) daemon through
`/dev/gpmctl`. Move, press, release, drag, and wheel packets use the same
hitboxes and actions as Crossterm mouse events. The client is safe Rust, waits
for descriptor readiness instead of polling in a loop, and does not link
`libgpm`; therefore enabling it adds no link-time system-library dependency.
Physical mouse input still requires the GPM daemon to be installed and
running. On Gentoo/OpenRC, start it with `rc-service gpm start` and enable it
across restarts with `rc-update add gpm default`. A missing or inaccessible
socket retains keyboard input. Each F8 press retries the socket immediately;
Youta performs no background reconnect probes. If an activation attempt fails,
Youta briefly replaces the one-line hotkey footer with a notice. When a
non-empty OpenRC runtime softlevel identifies the active init system, that
notice begins with `rc-service gpm start`. Builds without the `gpm` feature
instead say that GPM support is absent and never suggest starting a daemon.

Youta does not open GPM from `/dev/pts/*`, so terminal emulators retain their
normal mouse-capture behavior. `F8` provides a keyboard pointer on every
terminal: arrow keys move its reversed cell cursor, `Enter` clicks the current
cell, and `Esc` or `F8` exits. On a virtual console with GPM running, the
physical mouse moves this same square while it is visible. Keyboard movement
remains available when GPM is not installed or not running. Custom builds omit
the Linux-console client with `--no-default-features` by leaving `gpm` out of
their feature list; neither `app` nor `app-core` adds it transitively. See the
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
episodes, LibriVox chapters, and supported local media:

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
selected playlist; another `Enter` replays its selected item, and `Esc` or
`Backspace` returns to the playlist index. Local entries replay their original
file when it still exists. Remote entries resolve a fresh stream from their
saved canonical public page.

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
use them for access; the popup redacts the draft URL from debug output. Opening
the saved source parses its RSS or Atom feed on an isolated worker, shows
playable audio/video episodes, and starts the preferred media enclosure on
`Enter`. A bounded snapshot is reused across restarts and refreshed in the
background, so cached episodes remain visible while the network request runs.
Feed artwork, publisher metadata, episode descriptions, dates, and durations
are shown when the feed supplies them. Enclosure URLs are treated as transient
playback data and are not written to the restart snapshot.

YouTube subscriptions are currently local-only channel subscriptions. Choosing
`Subscribe (locally)` while a video is selected adds its channel to Youta's
OPML-backed source list; it does not subscribe the signed-in YouTube account.
OAuth-based synchronization remains roadmap work. In Details, uppercase
`[O]` opens the selected YouTube channel's webpage, while lowercase `[o]`
opens the selected video's webpage. Their labels identify the platform helper:
`xdg-open` on Linux and `open` on macOS. Youta waits for the system opener's
exit status before reporting success; a missing browser association or
headless-session failure is shown as a diagnostic instead.

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
  left with channel or podcast information on the right. Press `Enter` to
  activate the selected source, render any restart snapshot, and refresh its
  videos or episodes in the usual list-and-Details view; `Backspace` or `Esc`
  returns to the source list. `[R] Refresh videos` requests a YouTube channel's
  first page again, while `[R] Refresh episodes` reloads an RSS or Atom feed.
- `split` keeps sources on the left and the selected source's videos or
  episodes on the right. Moving across sources uses only cached rows and makes
  no provider request; press `Enter` to activate the source, loading it
  initially or refreshing its cache, and move into its items. The `[i] Details`
  button replaces the item list with the selected item's Details; `[i]` or
  `Esc` returns to the item list. The source-aware `[R]` refresh action is
  available after the source has been opened.

Refresh deliberately bypasses the process-local item cache so newly published
videos or episodes can appear. The current rows remain visible while the
request runs, and Youta restores the selected item by its stable source identity
when it is still in the refreshed result; a refresh failure also leaves the
existing rows intact.

Open the current in-app preferences with `[p] Preferences` or `F7`, choose
Drill-down or Split, choose whether exact `Реклама` chapters are hidden and
skipped, choose whether selected YouTube audio is prepared, choose whether
Local folder sizes are measured, choose the exact YouTube video-thumbnail
size, and press `Enter` to save. These preferences can be configured directly:

```toml
[playback]
autoplay = false
youtube_prewarm = true
skip_advertisement_chapters = true

[ui]
subscriptions_layout = 'drill-down' # drill-down or split
show_local_folder_sizes = true
youtube_thumbnail_size = 'automatic'
```

`YOUTA_UI__SUBSCRIPTIONS_LAYOUT=split` and
`YOUTA_PLAYBACK__AUTOPLAY=true` and
`YOUTA_PLAYBACK__YOUTUBE_PREWARM=false` and
`YOUTA_PLAYBACK__SKIP_ADVERTISEMENT_CHAPTERS=false` override the corresponding
TOML values. `YOUTA_UI__SHOW_LOCAL_FOLDER_SIZES=false` disables recursive size
work, hides cached folder sizes, and removes the Local size-sort control.
`YOUTA_UI__YOUTUBE_THUMBNAIL_SIZE=high` selects the strict 480×360 YouTube
video-thumbnail entry.
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
   public track/album discovery and playback, LibriVox public-domain audiobook
   discovery and chapter playback, tracker modules, generic
   `yt-dlp`, `mpv`, and search/history/queue/download state.
2. **Open-data integrations:** SponsorBlock, DeArrow, broader Wikidata
   discovery, Wikimedia Commons, Internet Archive, Podcast Index, and
   gpodder.net.
3. **Authenticated integrations:** YouTube OAuth interactions, including
   bidirectional local/YouTube subscription sync, Last.fm scrobbling, Discord,
   ListenBrainz, Google Drive, WebDAV, SSH, and optional one-way backups.
4. **Experimental adapters:** Odysee, Rumble, Bilibili, Telegram, Yandex Disk,
   VK, cloud.mail.ru, 4duk, knizhnyvoz, archive files, and
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
failure fails CI. Tagged releases build for Linux on amd64, i686, and arm64,
and natively for macOS on amd64 and arm64. Linux i686 requires a Pentium 4/SSE2
or newer processor. Each operating-system/architecture pair publishes directly
downloadable executables for all four combinations of the default-on `images`
and `qr` capabilities. These established executables retain GPM support. Linux
additionally publishes the same four combinations with a trailing `-no-gpm`
suffix for distributions where GPM is opt-in. The `-text` suffix omits images,
while `-no-qr` omits QR support. These are raw GitHub artifacts rather than ZIP
or tar wrappers. GitHub does not preserve their Unix executable bit, so a
downloaded Linux or macOS file needs `chmod +x ./youta-*` before it is run. The
release also publishes one Cargo vendor archive for offline/external build systems. It
contains the locked Rust dependency graph and the already-built GUI page, so a
source package can compile both front ends without npm network access. It is
source input and is the deliberate archive exception. No published executable
enables SQLite; human-readable TOML remains the standard persistence backend.

The desktop window is published as a standalone GUI executable for Linux
amd64, i686 and arm64, macOS amd64 and arm64, and Windows amd64. Native bundle
forms remain `.deb`, `.rpm` and AppImage for Linux amd64 and arm64, `.dmg` for
macOS amd64 and arm64, and NSIS for Windows amd64. The macOS executable can be
launched from a terminal; Finder users should use the `.dmg`.
`scripts/package-desktop.sh` builds whichever native forms its host can make,
while `scripts/package-desktop-executable.sh` produces the standalone Linux
i686 program without pretending to cross-compile an installer. Every file has
a `.sha256`; the complete asset list is asserted before publication, so a
missing architecture or bundle fails the release instead of shrinking it.

The window has its own CI lane on Linux, macOS, and Windows, which compiles it,
runs its tests, lints it, type-checks its page, and proves by `cargo tree` that
it links no terminal renderer. It is deliberately left out of the coverage gate:
measuring it would mean installing the WebKitGTK toolchain on the coverage
runner to instrument a thin shell over the reducer that gate already covers.

Windows amd64 and arm64 are compile-checked in CI. The platform work is done:
`mpv` is driven over a named pipe rather than a Unix socket, directory
durability and private-file access ask the platform instead of assuming POSIX,
helper trees are ended with `taskkill /T`, helper processes get no console
window of their own, and a file's identity is read from the volume serial
number and file index rather than given up. The desktop window ships a Windows
installer. What is still missing before a Windows *terminal* binary is
advertised is evidence: no part of the test suite has ever been executed on
Windows. The `windows-test` job runs it and reports without gating, precisely so
that evidence exists to work through.

FreeBSD x86_64 receives a cross-target compile check of the portable
TUI/local-browser boundary. It is not advertised as a release target until a
native or validated cross-build can also run playback tests.

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

The credential-free LibriVox smoke checks a stable public-domain book against
the production API adapter, book-page keywords and full-quality chapter link,
author biography, and a bounded Archive.org audio request. CI runs this check
on every push; run it locally with:

```sh
YOUTA_RUN_LIVE_LIBRIVOX_TEST=1 cargo test --locked --test live_services --no-default-features --features librivox -- --ignored --exact librivox_catalogue_book_author_and_audio_are_usable --nocapture
```

The authenticated Yandex Music smoke is intentionally local and opt-in because
it requires a user's OAuth token. Export
`YOUTA_PROVIDERS__YANDEX_MUSIC_TOKEN` through a secret-aware shell or CI store,
or save the token through Youta's private OAuth-token editor. The smoke then
validates account authentication, one bounded recommendation response,
catalogue search, and highest-available media metadata without downloading or
mutating durable account state:

```sh
YOUTA_RUN_LIVE_YANDEX_MUSIC_TEST=1 cargo test --locked --test live_services --no-default-features --features yandex-music -- --ignored --exact yandex_music_account_recommendations_search_and_media_metadata_are_usable --nocapture
```

Run the Radio smokes locally to resolve and decode real HTTP(S) M3U, PLS, MP3,
and FLAC streams through Youta's `mpv` backend, independently confirm declared
FLAC codecs with `ffprobe`, observe real ICY metadata, and parse bounded public
now-playing responses from curated and NPR providers. The
separate BBC smoke follows Youta's production Sounds-page and Media Selector
path, then decodes the returned regional manifest:

```sh
YOUTA_RUN_LIVE_RADIO_TEST=1 cargo test --locked --test live_services --no-default-features --features radio,backend-mpv -- --ignored --exact radio_stream_and_passive_metadata_are_usable --nocapture
YOUTA_RUN_LIVE_RADIO_TEST=1 cargo test --locked --test live_services --no-default-features --features radio,backend-mpv -- --ignored --exact generated_npr_station_stream_and_program_are_usable --nocapture
YOUTA_RUN_LIVE_BBC_RADIO_TEST=1 cargo test --locked --test live_services --no-default-features --features bbc-radio,backend-mpv -- --ignored --exact bbc_sounds_resolution_and_audio_are_usable --nocapture
```

The Gentoo ebuild is maintained as
[`media-sound/youta`](https://github.com/vitaly-zdanevich/gentoo-overlay/tree/main/media-sound/youta)
in the
[`vitaly-zdanevich-overlay`](https://github.com/vitaly-zdanevich/gentoo-overlay).
It maps provider choices to USE flags and consumes the release vendor archive.
Both the source and binary packages expose an opt-in `gui` USE flag; enabling
it installs `youta` and `youta-gui` together. The GUI is available on amd64 and
arm64, while x86 retains the TUI. The positive `images` and `qr` USE flags are
enabled by default. Gentoo users can independently disable them with
conventional `USE="-images"` and `USE="-qr"` overrides.
GPM mouse-daemon integration is opt-in with `USE="gpm"` in both packages. The
binary ebuild selects an unsuffixed GPM-enabled executable only when that flag
is enabled; otherwise it uses the corresponding `-no-gpm` release executable.
GitHub Actions use Node 24-based action majors and set the maximum requested job
timeout to 360 minutes.

To produce the same artifacts locally:

```sh
scripts/package-release.sh x86_64-unknown-linux-gnu dist images
scripts/package-release.sh x86_64-unknown-linux-gnu dist text
scripts/package-release.sh x86_64-unknown-linux-gnu dist images-no-qr
scripts/package-release.sh x86_64-unknown-linux-gnu dist text-no-qr
scripts/package-release.sh x86_64-unknown-linux-gnu dist images-no-gpm
scripts/package-release.sh x86_64-unknown-linux-gnu dist text-no-gpm
scripts/package-release.sh x86_64-unknown-linux-gnu dist images-no-qr-no-gpm
scripts/package-release.sh x86_64-unknown-linux-gnu dist text-no-qr-no-gpm
scripts/package-release.sh i686-unknown-linux-gnu dist images
scripts/package-release.sh i686-unknown-linux-gnu dist text
scripts/package-release.sh i686-unknown-linux-gnu dist images-no-qr
scripts/package-release.sh i686-unknown-linux-gnu dist text-no-qr
scripts/package-release.sh i686-unknown-linux-gnu dist images-no-gpm
scripts/package-release.sh i686-unknown-linux-gnu dist text-no-gpm
scripts/package-release.sh i686-unknown-linux-gnu dist images-no-qr-no-gpm
scripts/package-release.sh i686-unknown-linux-gnu dist text-no-qr-no-gpm
npm --prefix gui/ui ci
npm --prefix gui/ui run build
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

## My other Wikimedia-related projects

### GitHub

- **[wikimedia_commons_pwa_viewer](https://github.com/vitaly-zdanevich/wikimedia_commons_pwa_viewer)** —
  minimal PWA for browsing Wikimedia Commons images by feed, category, search, or
  location ([open the app](https://vitaly-zdanevich.github.io/wikimedia_commons_pwa_viewer/))
- [bot_telegram_wikimedia_commons_uploader](https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons_uploader) —
  Telegram bot that uploads images and media to Wikimedia Commons under each
  user's own account
- [bot_telegram_wikimedia_commons](https://github.com/vitaly-zdanevich/bot_telegram_wikimedia_commons) —
  Telegram and CLI bot for searching Wikimedia Commons media
- [bot_telegram_wikipedia](https://github.com/vitaly-zdanevich/bot_telegram_wikipedia) —
  Telegram bot for Wikipedia search
- [gthumb-copy-wikimedia-commons-link](https://github.com/vitaly-zdanevich/gthumb-copy-wikimedia-commons-link) —
  gThumb extension that copies the Wikimedia Commons link for a local file
- [wikipedia_diffs_to_evernote](https://github.com/vitaly-zdanevich/wikipedia_diffs_to_evernote) —
  daily synchronization of a Wikipedia user's edits to Evernote
- [wikipedia-userstyle-dark-minimum](https://github.com/vitaly-zdanevich/wikipedia-userstyle-dark-minimum) —
  dark, minimal Wikipedia userstyle that does not require a browser extension
- [PWAWikimediaCommonsUploader](https://github.com/vitaly-zdanevich/PWAWikimediaCommonsUploader) —
  PWA that uploads photos and videos (with automatic conversion) to Wikimedia Commons

### GitLab

- [wiki2man_on_rust](https://gitlab.com/vitaly_zdanevich_wikimedia/wiki2man_on_rust) —
  converts official Wikipedia XML dumps into roff man pages for offline reading
  in a terminal
- [gthumb-wikimedia-commons-extension](https://gitlab.com/vitaly_zdanevich_wikimedia/gthumb-wikimedia-commons-extension) —
  gThumb extension for viewing Wikimedia Commons images
- [commons-fuse](https://gitlab.com/vitaly_zdanevich_wikimedia/commons-fuse) —
  read-only FUSE filesystem for Wikimedia Commons
- [upload_to_commons_with_categories_from_iptc](https://gitlab.com/vitaly_zdanevich_wikimedia/upload_to_commons_with_categories_from_iptc) —
  Python script for uploading images from gThumb with IPTC categories
- [pwb_wrapper_for_simpler_uploading_to_commons](https://gitlab.com/vitaly_zdanevich_wikimedia/pwb_wrapper_for_simpler_uploading_to_commons) —
  stateless CLI wrapper around Pywikibot for single-file and batch uploads
- [web-extension-uploading-to-wikimedia-commons](https://gitlab.com/vitaly-zdanevich-extensions/uploading-to-wikimedia-commons) —
  browser extension for uploading images to Wikimedia Commons
- [commons-wikimedia-find-by-hash](https://gitlab.com/vitaly-zdanevich/commons-wikimedia-find-by-hash) —
  CLI tool that finds a Wikimedia Commons file with the same SHA-1 as a local file
- [webextension_find_by_hash](https://gitlab.com/vitaly_zdanevich_wikimedia/webextension_find_by_hash) —
  browser extension for finding Wikimedia Commons files by hash
- [video-to-webm-av1-opus](https://gitlab.com/vitaly-zdanevich/video-to-webm-av1-opus) —
  file-manager script that converts video to Commons-compatible AV1/Opus WebM

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
