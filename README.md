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
> working TUI and core state model, local SQLite persistence, OPML
> import/export, configuration loading, official YouTube Data API v3 and
> Invidious discovery, and supervised `mpv`/`yt-dlp` integration. The larger
> provider, upload, sync, scrobbling, waveform, and audiophile feature set
> described in the roadmap is not implemented yet. Do not treat an existing
> Cargo feature name as a support claim.

## Why this design

- The UI stays responsive while network, metadata, and playback work happen
  outside the render loop.
- Persistent state is local-first and restartable. Youta stores navigation,
  queue, history, notes, bookmarks, and playback positions beneath
  `~/.config/youta/`.
- Local media folders are read-only inputs. Youta does not reorganize or move
  them.
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

`mpv` is a playback engine, not a second UI. It is intentionally kept out of
the terminal and never parses Youta's keystrokes. A future native backend can
implement the same playback interface without changing screens or history.

## Current foundation

The initial `0.1.0` work establishes:

- configuration-file plus `YOUTA_` environment overrides;
- a source-neutral domain model for media, channels, queues, positions, notes,
  and provider capabilities;
- SQLite persistence and versioned migrations;
- local subscriptions with OPML import/export;
- a two-panel terminal UI and restartable screen state;
- official YouTube Data API v3 or Invidious video/channel search and video
  details, with description-link extraction;
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
  a stable public BBC Sounds catalog API.
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
cached in SQLite for seven days; successful empty lookups are cached for 24
hours. Network and response errors are not negative-cache entries.

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

Youta requires Rust 1.90 or newer.

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
change. It also enables the `thumbnails` Cargo feature; runtime capability
checks decide whether the TUI may fetch and render artwork.

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
exact `config.toml` path before saving. On Unix, this save creates the
configuration directory with mode `0700` and the file with mode `0600`; the
selected API key is stored there as plaintext. Environment values take
precedence over saved values. The popup lists the steps to create a Google
Cloud project, enable YouTube Data API v3, create an API key, and restrict it
to that API so it cannot call unrelated Google APIs. Its `[F1]` link opens
Google's official [credentials
guide](https://developers.google.com/youtube/registering_an_application),
`[F2]` opens [Google Cloud
Credentials](https://console.cloud.google.com/apis/credentials), and `[F3]`
opens the official [Invidious instance
list](https://docs.invidious.io/instances/). All three links also accept mouse
clicks.

The same choice can be configured manually:

```toml
[providers]
youtube_backend = 'auto' # auto, official, or invidious
youtube_api_key = '...'
# invidious_base_url = 'https://inv.example.org/'
```

`auto` prefers `youtube_api_key` when the official adapter is compiled in,
then falls back to `invidious_base_url`. `official` and `invidious` select only
that backend. Both the TUI and `youta search` use this selection.

For a small local-only build:

```sh
cargo build --release --no-default-features --features tui,local,backend-mpv
```

This intentionally omits terminal thumbnails. A custom
`--no-default-features` build must list `thumbnails` explicitly when artwork is
wanted.

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
layer accepts token fields as plain strings. The TUI provider popup says where
it will save the key and applies user-only Unix permissions, but environment
injection avoids storing it in `config.toml`. A system-keyring adapter and
explicit secret references are roadmap work.

## YouTube, the official API, Invidious, and `yt-dlp`

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
- Both adapters provide discovery and metadata only. Online playback remains
  the independent `yt-dlp` resolver plus the invisible `mpv` backend; the
  YouTube API key is not a playback credential.
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
selected-item artwork lazily; off-screen search rows do not trigger thumbnail
requests. Validated original image bytes are cached across restarts in
`~/.config/youta/thumbnail-cache` (or the selected Youta configuration
directory). The private cache expires entries after 30 days and evicts its
oldest files above 256 entries or 64 MiB. Corrupt entries are discarded and
fetched again. The image URL is never printed as detail-panel text or stored as
a filename.

On a Linux virtual console, serial terminal, `TERM=dumb`, or a terminal without
one of those image protocols, Youta neither fetches nor renders artwork. The
detail panel remains text-only. Accepted remote images are limited to bounded
JPEG, PNG, and WebP input before decoding, which prevents unbounded downloads
and image allocations. These gates avoid stray escape sequences and reduce
network traffic, decoding work, memory use, heat, and battery consumption.

Configure the runtime policy in `~/.config/youta/config.toml`:

```toml
[ui]
thumbnails = 'auto' # auto, off, or on
thumbnail_height = 20 # maximum terminal rows; minimum 4
```

`auto` uses conservative protocol detection, `off` disables thumbnail requests
and rendering, and `on` attempts supported terminal artwork but still falls
back without fetching when no supported protocol is available. Thumbnail
height defaults to 20 rows and is reduced automatically when the Details panel
needs space for metadata, links, or description text. To exclude the renderer
and its image dependencies from the binary, use
`--no-default-features` and omit `thumbnails` from `--features`; include
`thumbnails` explicitly in a custom feature list to restore it. The rendering
integration uses
[`ratatui-image`](https://docs.rs/ratatui-image/11.0.6/ratatui_image/).

## Subscriptions and local data

OPML is the interchange format for RSS/podcast feeds and compatible channel
feed URLs. It makes migration possible without a Youta-specific conversion.
Private comments, folders, bookmarks, playback positions, and provider IDs do
not fit OPML reliably, so they remain in SQLite and can be exported separately.

Persistent writes stay under `~/.config/youta/`; transient IPC sockets may use
the operating system's runtime directory. Downloads also default to a
Youta-owned subdirectory rather than a media source folder.

Automatic Git commits and pushes are a roadmap feature and will be opt-in.
The safer design batches an atomic state change, excludes secrets and media,
then commits an allowlisted set of state files. A strict commit-per-change mode
would create noisy history and consume more power, so it should not be the
default.

## Service roadmap

The roadmap is intentionally tiered:

1. **Core:** local audio/video, RSS and OPML, radio/BBC feeds, official
   YouTube metadata, Invidious, PeerTube, Funkwhale, direct
   Vimeo/RuTube/SoundCloud URLs, tracker modules, generic `yt-dlp`, `mpv`, and
   search/history/queue/download state.
2. **Open-data integrations:** SponsorBlock, DeArrow, broader Wikidata
   discovery, Wikimedia Commons, Internet Archive, LibriVox, Podcast Index,
   and gpodder.net.
3. **Authenticated integrations:** YouTube OAuth interactions, including
   bidirectional local/YouTube subscription sync, Last.fm, Discord,
   ListenBrainz, Google Drive, WebDAV, SSH, and optional one-way backups.
4. **Experimental adapters:** Apple Podcasts catalog search, Bandcamp, Odysee,
   Rumble, Bilibili, Telegram, Yandex services, VK, cloud.mail.ru, 4duk,
   knizhnyvoz, archive files, and torrent-backed sources.

Proprietary or scraper-dependent providers are not promised until an adapter
has tests, documented authentication, rate limiting, and a maintenance owner.
RuTracker/torrent support is a separate build feature and must remain stopped
when Youta exits. Youta will not bypass access controls or digital-rights
management.

Useful future open/self-hosted sources include
[Audiobookshelf](https://www.audiobookshelf.org/),
[OpenSubsonic](https://opensubsonic.netlify.app/),
[ListenBrainz](https://listenbrainz.org/),
[MusicBrainz](https://musicbrainz.org/doc/MusicBrainz_API).

Tracker playback depends on
[libopenmpt](https://lib.openmpt.org/libopenmpt/documentation/). The Mod
Archive API key is never bundled; users request and store their own key.
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
required live YouTube, Apple Podcasts, and Wikidata jobs; a newer push does not
cancel the older revision's suite. Clippy blocks compiler hygiene plus its
correctness, suspicious-code, and performance groups; style, complexity, and
pedantic findings remain visible as advisory output while that backlog is
reduced in focused changes. The YouTube job uses Youta's production
mpv/yt-dlp integration to decode a short segment through mpv's null audio
output. Apple Podcasts is checked from public Apple metadata through its RSS
enclosure and silent audio decode. Wikidata is checked through a live exact
P1651 lookup. Each live job retries once for a transient network failure; a
second failure fails CI. Tagged releases build on native amd64 and arm64
runners and publish:

- a binary archive for each architecture; and
- a Cargo vendor archive for offline/external build systems.

The live YouTube job does not use a Google account or cookies. It runs the
account-free [bgutil PO-token
provider](https://github.com/Brainicism/bgutil-ytdlp-pot-provider) in an
isolated, version-and-digest-pinned Deno container bound only to localhost.
The matching Python plugin gives yt-dlp short-lived, video-bound tokens, and
the job removes the container after playback. This follows yt-dlp's
[recommended `mweb` PO-token
setup](https://github.com/yt-dlp/yt-dlp/wiki/PO-Token-Guide), but the provider
cannot guarantee that YouTube will accept every runner address.

The Gentoo ebuild in `packaging/gentoo/` maps provider choices to USE flags and
consumes the release vendor archive. GitHub Actions use Node 24-based action
majors and set the maximum requested job timeout to 360 minutes.

To produce the same artifacts locally:

```sh
scripts/package-release.sh
scripts/package-vendor.sh
```

Run the live playback check locally without sending audio to a device:

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
