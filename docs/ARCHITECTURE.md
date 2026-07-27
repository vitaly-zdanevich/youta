# Architecture

This document describes the intended boundaries of Youta and the foundation
present in `0.5.0`. Items marked **roadmap** are design decisions, not support
claims.

## Goals

1. Start quickly and remain usable on low-memory, battery-powered hardware.
2. Keep the terminal event loop independent from network, database, decoder,
   and subprocess latency.
3. Run without X11 or Wayland and degrade cleanly on a real TTY.
4. Preserve a user's queue, navigation, and playback position after a crash.
5. Let distributions omit providers and integrations at compile time.
6. Treat external content, command output, archives, and provider metadata as
   untrusted input.

## Component boundaries

```text
terminal input ─┐
mouse events ───┼─> application reducer ─> immutable UI snapshot ─> renderer
timers ─────────┤            │
provider events ┤            ├─> command queue ─> provider workers
player events ──┘            ├─> persistence worker ─> SQLite / OPML
                             ├─> playback worker ─> mpv JSON IPC
                             └─> prewarm worker ─> yt-dlp selected-audio URL
```

The reducer is the single owner of interactive state. Workers return typed
events through bounded channels; they never mutate a screen directly. A slow
thumbnail, Wikidata request, database checkpoint, or provider timeout therefore
cannot block keyboard input or redraw.

The library is divided by responsibility:

- `config`: versioned TOML configuration and environment overrides;
- `domain`: source-neutral identifiers and media/application state;
- `persistence`: SQLite migrations, durable state, and short transactions;
- `subscriptions`: local subscriptions and OPML interchange;
- `providers`: discovery and metadata interfaces plus provider adapters;
- `links`: safe extraction and classification of links and timecodes;
- `playback`: backend interface, process supervision, and player events;
- `tui`: input mapping, reducer, layout, widgets, and terminal lifecycle.

Provider-specific response structures do not cross into TUI code. They are
normalized into domain objects with the original source ID retained.

## State and persistence

Youta has three state classes:

| State | Storage | Examples |
| --- | --- | --- |
| configuration | TOML | colors, provider endpoints, player choice |
| durable library | SQLite | subscriptions, history, positions, notes, queue |
| interoperable subscriptions | OPML | podcast/RSS feed outlines |

The default persistent root is `~/.config/youta/`. Browsing source media
directories is read-only; only explicit Local Rename, Move to Trash, and Move
actions mutate selected entries. SQLite uses migrations and foreign-key
checks; each user action is a small transaction. Configuration writes use a
temporary file, an `fsync`, and an atomic rename. Private files are created
with user-only permissions on Unix.

Before a Local Move request reaches the filesystem worker, Youta validates the
complete bounded tree, rejects symbolic links, collisions, and paths that
cannot be represented durably, and records the exact source-to-target mappings
in SQLite. Completion remaps Local history, progress, queue, comments,
bookmarks, subscription snapshots, and session paths in the same durable
transaction that clears the move journal. Startup reconciles moves that
finished or never began. If both endpoints exist or both are missing, Youta
does not guess: it blocks new moves and orderly quit until the user repairs the
ambiguous filesystem state and retries.

Apple Podcasts discovery is isolated from the YouTube routes. The tab uses a
bounded, storefront-specific public catalogue search and persists only compact
show summaries. Selecting a show does not resolve every result in advance;
Enter requests the documented bounded episode lookup for that one show, which
keeps startup and idle network use predictable. Apple search, lookup, and
direct-link resolution share a capacity-one latest-only lane: one call may be
in flight, one newer request replaces queued stale work, and shutdown discards
that pending request before waiting for the active call's bounded timeout.
Apple API redirects are followed manually only within the exact original
scheme, host, and port, with a three-hop ceiling.
Provider-returned feed, artwork, and enclosure URLs reject non-public IP
literals plus `localhost`, `.local`, `.internal`, and single-label names.
Youta does not fetch the returned Apple feed: show episodes come from the
bounded Apple lookup. If terminal artwork is enabled, Youta's thumbnail worker
pins DNS to public addresses and rejects redirects. Episode enclosures are
handed to the external playback backend; later media DNS and redirects belong
to that backend and are not presented as Youta-side SSRF containment.

Bandcamp discovery has its own restart snapshot containing the trimmed query,
current page, advertised next page, compact canonical track/album summaries,
and selected row. Search never stores or resolves direct stream URLs. Those
short-lived URLs and required headers exist only in RAM after an explicit
playback action. Public-page search uses a capacity-one latest-only worker
separate from the general YouTube provider lane.

The YouTube provider setup popup displays the exact `config.toml` destination
before it saves; that path's parent directory is mode `0700` and the file is
mode `0600` on Unix.

The player reports progress in memory more often than it is written. A dirty
position is persisted every 30 seconds, on pause, on a media switch, and during
orderly shutdown. Completion is `position / duration >= 0.90` when duration is
known. Returning to a partially played item resumes at the stored position
minus 30 seconds, clamped to zero. Manual played/unplayed status remains an
explicit override.

Screen identity, selection, scroll offsets, detail navigation history, queue,
and active item are snapshotted so restart returns to the same useful context.
Transient errors and open secret prompts are never restored.

### OPML

OPML is an import/export boundary, not the database schema. RSS feeds use
standard `xmlUrl`, `htmlUrl`, `text`, and `title` attributes. YouTube channel
feed URLs can also be represented. Nested outlines preserve portable folder
structure where possible. Youta-specific private notes, playback positions,
colors, and bookmarks remain in SQLite rather than being smuggled into
non-standard OPML attributes.

From the subscription-source root, `[a] Add RSS feed` accepts an absolute
HTTP(S) RSS or Atom URL, rejects embedded username/password credentials, strips
its fragment, detects duplicates across the nested tree, and atomically saves
the source in the private portable OPML file. Query parameters remain part of
the stored URL because some private feeds require them; the popup's custom
debug representation redacts both its draft and validation error. This path
currently stores the subscription outline only. RSS episode browsing in the
Subscriptions screen is not implemented.

## Source model

Every source adapter advertises capabilities instead of requiring a large,
mostly empty interface:

```text
search media        search channels       list channel items
list playlists      resolve stream        fetch captions
fetch comments      write comment         subscribe remotely
sync played state   download              upload/transfer
```

The UI enables an action only when the selected source and current
authentication have that capability. Unsupported actions are absent or carry a
short explanation; they do not fail after collecting input.

Stable domain identifiers use `(provider, kind, provider_id)`. URLs are
attributes, because provider URLs and host instances can change. A local file
also stores a stable filesystem identity when available. Youta never moves it
automatically; an explicit Local Move remaps every retained path after the
filesystem operation succeeds.

### Provider tiers

- Tier 0 adapters are expected to be dependable without an account: local
  media, RSS/OPML, radio, Invidious, configurable PeerTube/Funkwhale
  instances, Apple Podcasts catalogue search, direct URLs, and `yt-dlp`.
- Tier 1 uses public/open data: SponsorBlock, DeArrow, Wikidata, Commons,
  Internet Archive, LibriVox, Podcast Index, gpodder.net, Jamendo with a
  user-provided application client ID, and official YouTube public metadata
  with a user-provided API key.
- Tier 2 needs OAuth, a token, or account policy review: YouTube account
  actions, SoundCloud rich search, Vimeo rich search, Last.fm, Discord, cloud
  storage, and remote subscription sync.
- Experimental adapters rely on unofficial or frequently changing APIs and
  live behind individual build flags.

The implemented Bandcamp tab is a credential-free public-page adapter rather
than an API integration. Its bounded HTML search is best-effort and may need
maintenance when Bandcamp changes markup, but it remains isolated behind the
`bandcamp` feature and cannot turn a search result into authenticated access.

Retries are bounded and use jittered backoff. Each adapter has explicit
timeouts, a user agent, pagination limits, and a concurrency budget. Search
results are cached with provider-specific expiry; errors do not overwrite good
cached metadata.

Federated providers carry their instance origin in configuration and domain
identity. PeerTube search coverage can be local, federated-known, or an
administrator-enabled external index; the UI labels the scope. Funkwhale uses
its stable REST API and may use only the subset of Subsonic that the selected
pod advertises. Credentials never migrate silently between instances.

YouTube discovery has two interchangeable metadata adapters. The official
YouTube Data API v3 adapter uses `providers.youtube_api_key` with
[`search.list`](https://developers.google.com/youtube/v3/docs/search/list),
[`videos.list`](https://developers.google.com/youtube/v3/docs/videos/list), and
[`channels.list`](https://developers.google.com/youtube/v3/docs/channels/list);
the Invidious adapter uses `providers.invidious_base_url`. With
`providers.youtube_backend = "auto"`, Youta prefers the official adapter when
its key and Cargo feature are present, then falls back to Invidious. Explicit
`"official"` and `"invidious"` values do not fall through. This selection is
shared by the TUI and CLI search paths. It does not select playback:
`yt-dlp` resolution and `mpv` playback remain separate workers.

YouTube Music discovery is a separate provider route and persistent result
snapshot. It invokes the configured `yt-dlp` executable on the public
`music.youtube.com` search page from a capacity-one latest-only worker,
recursively resolves browse containers, and accepts only playable YouTube video
leaves. Its process cannot hold the general YouTube provider lane. The child
process, retained output, metadata fields, timeout, and result count are
bounded. This route needs no Data API key and never overwrites the normal
YouTube or MOD/tracker query, rows, or selection.

Jamendo uses its fixed HTTPS v3 tracks endpoint rather than a configurable
mirror or page extractor. The adapter sends only the user's application client
ID, bounds every blocking response, and validates page, artwork, stream, and
download locators as credential-free HTTPS. It retains `license_ccurl`
verbatim as licence metadata; a download flag or Creative Commons label is not
an automatic Wikimedia Commons upload decision.

Direct-URL adapters are deliberately smaller. Vimeo, RuTube, BBC Sounds, and
SoundCloud can initially resolve through `yt-dlp`, while official search needs
a service-specific documented API and credentials. The generic `yt-dlp`
provider accepts installed built-in extractors for URL resolution only; it does
not synthesize search, channel subscription, or account support.

Tracker modules have their own source boundary. The Mod Archive search adapter
uses a user-provided official API key and caches within its request allowance.
Modland is a default keyless catalog: Youta periodically fetches its compact
HTTPS `allmods.zip` file list, searches a local index, and constructs direct
archive URLs without crawling its directory tree. Scene.org can use its
documented HTTPS JSON search API.

HTML-only catalogs are isolated behind site-specific, rate-limited adapters;
Youta does not pretend they share an API. AMP exposes form search, stable
module download IDs, and an offline metadata database. UnExoticA combines a
MediaWiki catalog with direct LhA soundtrack archives. Aminet offers HTML
search, RSS updates, mirrors, and package readmes. modules.pl exposes rich
HTML filters, module pages, RSS, and HTTPS downloads, but no documented public
API was found.

Mirsoft Game Music Base is the transport exception. As checked on 25 July
2026, its HTTP catalog returned successfully while port 443 refused
connections. It is enabled by default at the user's request, guarded by
`providers.allow_insecure_http`, and produces a one-time warning before the
first request. Setting the option to `false` removes Mirsoft results and rejects
all other plain-HTTP provider URLs. The adapter sends no cookie, token, API
key, or user data; downloaded bytes and metadata remain vulnerable to network
observation or modification. An HTTP redirect never overrides this gate.

Playback is delegated to `mpv`/FFmpeg only when libopenmpt support is detected.
UnExoticA and Modland also contain exotic Amiga formats outside libopenmpt's
scope; those remain unplayable until a separately sandboxed UADE backend is
available. File names, archives, and metadata are untrusted, and catalog
presence says nothing about license or permission to redistribute.

## Search and detail loading

Search returns a first page of lightweight rows. Selecting a row schedules
detail fields independently:

1. cached core metadata;
2. provider details such as duration, description, statistics, and license;
3. optional thumbnail, only when the terminal can render it;
4. lazy Wikidata lookup;
5. optional DeArrow title and SponsorBlock segments.

Each request carries the selected media ID and generation number. A late result
for an old selection is cached but not painted over the new selection.

Apple Podcasts and Bandcamp keep independent query, result, and selection
state. Apple show search returns one bounded, storefront-specific ranked set
because the documented Search API has no episode-search entity or pagination;
Enter performs one documented lookup for the selected collection and renders
Apple's returned order of at most 200 associated episodes. Bandcamp search
accepts bounded pages of canonical track and album URLs from the fixed public
HTTPS endpoint and follows only the sequential next page advertised in its
HTML. Neither tab performs per-row stream resolution while the user navigates.
Official Apple show/episode URLs and strict canonical Bandcamp release URLs
enter these same first-class state machines instead of falling through to a
generic extractor; direct pages retain Back navigation and canonical restart
identity.

Description links, hashtags, timecodes, and YouTube IDs are parsed into typed
spans. Following an internal media link pushes the current detail state onto a
bounded navigation stack. Back restores the exact selection and scroll offset.
Timecode navigation also records a previous playback position. Chapter labels
may reserve up to four rows when the terminal has height to spare, and the
timestamp prefix can be hidden without changing exact seek targets. The
inverse visibility bit is stored in the restart-safe session so older session
documents keep timestamps visible by default.

Video orientation is provider metadata, never a thumbnail-ratio heuristic.
Official YouTube results use `player` embed dimensions returned by the existing
batched video-resource request. Invidious selected-video details use encoded
format dimensions. Unknown rows retain the normal title color, avoiding
per-result network fan-out and false portrait classifications from letterboxed
artwork.

Wikidata matching is advisory. Exact external-ID claims rank above URL and
normalized-title matches. Ambiguous title matches are displayed as suggestions,
not asserted links.

A matching entity is represented by one collapsed External links row rather
than duplicated in the channel/video metadata body. Activating its `[W] 🧾`
control starts a bounded `wbgetentities` claims-plus-sitelinks request and
bounded label requests, then replaces the Details description body with a
scrollable property spoiler. Only human-facing statements and canonical,
validated Wikipedia article sitelinks are formatted; closing the spoiler
restores the ordinary Details body. Entity details therefore remain lazy even
after the lighter exact-ID match has completed.

### Subscription navigation and channel videos

Subscriptions are local OPML entries. Selecting `Subscribe (locally)` for a
YouTube video stores its channel; an API key is public-metadata authentication,
not authorization to mutate a YouTube account. Remote subscription sync
therefore remains an OAuth-gated feature.

The source root also exposes `[a] Add RSS feed`, whose validation and private
OPML behavior are described above. RSS sources remain portable entries in this
release; the channel-video list controller below is YouTube-specific and does
not yet browse their episodes.

The TUI reducer owns two subscription layouts over the same state:

- drill-down starts with sources and enters one source's list-and-Details view;
- split retains the source list while showing the selected source's videos.

Uppercase `S` resets either layout to the source root from any main screen;
`Tab` and `Shift+Tab` cycle the enabled top-level screens. The
Preferences popup changes `ui.subscriptions_layout` together with
`playback.skip_advertisement_chapters` and `playback.youtube_prewarm`; their
`YOUTA_` environment overrides have higher precedence and prevent a partial
in-app save until removed. The targeted configuration writer updates both
tables atomically and preserves unrelated TOML content instead of serializing
secret-bearing configuration state again.

Selected-channel profile work is debounced and lazy. The configured official
API or Invidious adapter first returns `ChannelDetails`, so its description,
subscriber count, joined timestamp, public video count, aggregate public views,
country, artwork, and canonical URL can be rendered without waiting for
page enrichment. When the `network` feature is enabled and that primary request
succeeds, the provider worker performs a second best-effort request for the
exact public `/channel/{UC…}/about?hl=en` page. It reads the embedded
`ytInitialData` only
to fill missing joined/video/view/country fields and the channel owner's public
website and social links. Recognized destinations include Telegram, Facebook,
X/Twitter, TikTok, Instagram, and YouTube; other credential-free HTTP(S)
website links retain their channel-supplied labels.

The About-page supplement sends no account credential, cookie, or API key.
Its HTML is capped at 8 MiB, JSON traversal at 100,000 values, external links
at 32, and each link label at 128 characters. The exact channel ID is validated
before the request and again against the parsed model. Oversized, malformed,
consent-gated, or changed pages are ignored after the primary provider result;
individual unavailable fields are omitted from the UI.

Full `ChannelDetails`, including country, aggregate views, and external links,
uses the existing 64-entry process-local channel-details LRU and is discarded
at shutdown. Compact `ChannelSummary` fields such as description, subscriber
count, joined timestamp, video count, artwork, and canonical URL continue
through the seven-day SQLite summary cache. This separation avoids repeating
About-page work while moving between channels without persisting the richer
external-link profile.

Channel-video work is selected-source lazy. For the official API, the worker
uses `channels.list` to find the uploads playlist,
[`playlistItems.list`](https://developers.google.com/youtube/v3/docs/playlistItems/list)
to preserve upload order, and
[`videos.list`](https://developers.google.com/youtube/v3/docs/videos/list) to
batch row metadata. The Invidious worker uses its documented [channel videos
endpoint](https://docs.invidious.io/api/channels_endpoint/). Both adapters
translate provider continuation tokens into sequential page numbers at the
provider boundary.

Moving across split-view sources performs no remote work. Arrow navigation may
render an existing RAM or restart snapshot, but only `Enter` activates a source
and starts its initial provider load or page-one refresh. The controller keeps
a bounded least-recently-used RAM cache: at most 24 channels and 250 videos per
channel under a shared approximate 8 MiB heap budget. Description excerpts and
thumbnail sets are compacted before insertion. Approaching the end of visible
rows requests the next page; bounded automatic continuation skips
private/unavailable-only pages. Every response carries a subscription
generation; a response for an older selection is ignored and cannot replace
the newly selected channel's rows.

Successful YouTube page-one refreshes also replace a compact
`subscription_items_cache` snapshot in SQLite. Each source retains at most 50
provider-ordered items and 512 KiB of encoded item data; when the byte limit is
reached, the longest whole-item prefix that fits is stored. SQLite keeps the 32
most recently refreshed sources. Later pages remain process-local.

On activation after a restart, Youta restores the first-page snapshot into RAM
and renders it before issuing the background page-one refresh. The successful
refresh replaces the visible and durable first page, reconciling both newly
published and provider-deleted videos. Failed refreshes leave the restored rows
and selection available. Direct `stream_url` values are removed before writing
and again after reading because they may be short-lived, signed, or carry query
secrets; the restart snapshot retains canonical video pages and compact public
metadata instead.

Inside an activated channel, uppercase `R` explicitly refreshes page one. This
request bypasses the RAM list cache but does not clear the rendered rows while
it is in flight. A successful response restores selection by stable provider
video ID, with the previous index as a fallback; a failed response leaves the
old rows and selection available.

## Playback

`PlaybackBackend` owns loading, play/pause, seek, volume, speed, current
position, duration, track metadata, and shutdown. It emits state changes and
errors through a bounded channel.

### External `mpv`

The first backend launches `mpv` as an invisible child, broadly equivalent to:

```text
mpv --no-video --terminal=no --input-terminal=no --idle=yes
    --no-config --input-ipc-server=<private socket>
```

The exact options are constructed as argument values without a shell. Youta
uses JSON IPC commands and observes properties including `time-pos`,
`duration`, `pause`, `volume`, `speed`, and `eof-reached`. The TUI remains the
only visible interface and renders its own seek bar. It can therefore seek with
left/right keys, digits, chapters, mouse clicks, description timecodes, or a
future waveform while `mpv` performs decoding and device output.

The socket lives in a user-owned runtime directory with restrictive
permissions. IPC is local and unauthenticated by design, so it must not be
placed in a shared writable directory. The child is terminated and reaped on
shutdown. Youta detects exit, broken IPC, malformed messages, and stalled
startup separately.

User `mpv.conf` is bypassed by default for reproducibility. A configuration
switch may allow it, but Youta still appends safety-critical IPC and terminal
options after user options. Audio output, device, and decoder choices are
visible in the diagnostics screen.

### Native playback (**roadmap**)

A native backend is useful for installations that do not want an external
player, but it increases codec, output, gapless, DSP, and device-maintenance
scope. It should be introduced only after conformance tests can run the same
queue, seek, resume, speed, and error cases against both backends. Backend
selection belongs in configuration and is compile-time removable.

### Queue and cuts

The active queue is an ordered list independent from user playlists. `Play
next` inserts after the active item; `Add to queue` appends. A cut is a
non-destructive media reference plus `[start, end)` timestamps, title, comment,
and playlist placement. Lossless export is possible only when container and
codec boundaries permit a stream copy; otherwise Youta must label the operation
as approximate or offer explicit re-encoding. The source file is never edited.

## `yt-dlp` supervision

`yt-dlp` is an optional resolver/downloader, never a linked trust boundary.
Youta:

- validates accepted URL schemes and provider hosts;
- passes a fixed executable and argument vector directly, never through a
  shell;
- uses machine-readable JSON/progress output;
- caps output and metadata size;
- rejects unexpected output paths and path traversal;
- downloads into a Youta-owned staging directory and atomically promotes a
  completed file;
- cancels the child on user request and application shutdown;
- does not read browser cookies unless the user explicitly configures a cookie
  file;
- records tool version and stderr in redacted diagnostics.

Arbitrary `yt-dlp` configuration files, plugins, `--exec`, `--netrc-cmd`,
postprocessor commands, and user-supplied output templates are outside the safe
default. The command is for media a user is allowed to access; it is not a
rights or policy bypass.

### Bandcamp on-demand resolution

Pressing Enter on a Bandcamp result sends its canonical page and the configured
audio preference to one bounded resolver worker. The worker ignores external
`yt-dlp` configuration and plugins, supplies no cookies, validates that the
Bandcamp extractor and public CDN produced the response, and discards stale
completions when the selected release changes. Album entries are bounded; the
queue and persisted state retain canonical release identity rather than
resolved URLs or headers.

`providers.bandcamp_audio_format` defaults to `best-available`. The complete
closed set is `best-available`, `flac`, `alac`, `wav`, `aiff`, `mp3-320`,
`mp3-v0`, `aac`, `ogg-vorbis`, and `public-stream-mp3-128`;
`YOUTA_PROVIDERS__BANDCAMP_AUDIO_FORMAT` has higher precedence. The `[b]`
control in Preferences cycles this exact order. Every value maps to a static
Youta-owned selector, so configuration cannot inject a raw `yt-dlp` format
expression. `best-available` prioritizes lossless encodings before lossy
downloads and the public stream. Each named encoding falls back to the public
MP3-128 stream and then the extractor's best audio, so the setting is a
preference rather than a guarantee. Resolving a free-download format may
consume an artist-configured download allocation.

### Selected YouTube audio prewarm

With `playback.youtube_prewarm = true`, the reducer waits for a YouTube video
selection to remain stable for 200 ms, then submits only that row to a
latest-only bounded resolver worker. Selection changes cancel stale work, so
keyboard navigation does not create a request per result. The equivalent
environment override is `YOUTA_PLAYBACK__YOUTUBE_PREWARM`; the Preferences
popup exposes the setting as `[y] Prepare selected YouTube audio`.
On Unix, each resolver runs in a dedicated process group so cancellation,
timeout, and shutdown terminate extractor helper descendants as well as the
top-level `yt-dlp` process.

The resolver returns a short-lived direct audio URL, required HTTP headers,
and expiry metadata. These values remain in process memory only; the queue,
history, and persisted session retain the canonical video identity, while
diagnostics redact URLs and headers. A prepared result is discarded when its
selection or lifetime no longer matches. If no usable result exists, or direct
loading fails before `mpv` reports audible playback, the controller retries
the canonical YouTube URL. This includes a speculative payload that reaches
`file-loaded` but then fails demuxing before `playback-restart`. The existing
bounded checked-format retry remains the final response to a canonical
pre-start HTTP 403; playback does not loop across fallback paths.

## Terminal rendering

The baseline renderer uses text, Unicode where supported, and the terminal
palette. It does not assume true color. Theme selection has `auto`, `dark`,
`light`, and explicit palette modes. Auto mode should first use a standardized
terminal hint when present and otherwise choose a conservative palette; there
is no universal reliable query for terminal background color.

Thumbnail support is a separate feature and runtime capability:

- supported graphics protocol detected: fetch a size-bounded thumbnail lazily;
- remote artwork rejects non-public literal, DNS-resolved, `.local`,
  `.internal`, and single-label destinations, and HTTP redirects are not
  followed;
- Linux virtual console, serial/real TTY, `TERM=dumb`, or unknown protocol:
  show text details only;
- selected artwork has priority; one bounded worker may then prefetch the
  currently loaded global Search rows into the persistent cache;
- unseen pagination, subscription feeds, and non-Search screens are excluded
  from background prefetch;
- cache has byte and entry limits and can be disabled.

Mouse regions are derived from the same layout rectangles used to render
widgets, avoiding invisible or stale click targets. Every mouse action has a
keyboard equivalent. Buttons can include their hotkey, and `?` opens the
context-sensitive help layer.

On a real Linux `/dev/ttyN`, the optional `gpm` feature registers both standard
input and `/dev/gpmctl` with one readiness poll. Native-endian GPM packets are
decoded in safe Rust and converted into the same mouse-event path as terminal
emulator reporting. PTYs never open GPM. Socket absence or disconnect is a
silent capability loss, and the `F8` keyboard pointer continues to drive the
rendered hit map without a daemon.

Waveform rendering is a separate library-shaped module (**roadmap**). It
produces a resolution-independent peak envelope cached by media fingerprint,
then downsamples to terminal columns. Generation is cancellable, low priority,
and never required for playback. The widget consumes only an envelope and
timeline interface so it can later be extracted into its own crate.

## Uploads and external writes

Uploads, comments, remote subscriptions, scrobbles, and sync are explicit
effects. The reducer first creates a reviewable request; the adapter performs it
only after confirmation where the action is difficult to undo.

### Wikimedia Commons transfer (**roadmap**)

The transfer pipeline is:

1. retrieve and display source license and author;
2. refuse Standard YouTube License and unknown-license media;
3. warn that a Creative Commons marker does not prove ownership of embedded
   works;
4. search Commons by source URL/external identifier and normalized title;
5. resolve the stream into a staging file and calculate a hash;
6. use an existing open codec without generation loss where possible, otherwise
   transcode to Ogg Opus or WebM VP9/AV1 plus Opus;
7. show filename, title, multilingual caption, description, attribution,
   source URL, structured-data statements, and categories for review;
8. append `Uploaded by Youta` after a blank line in the category editor;
9. honor all duplicate, filename, deleted-file, and policy warnings from the
   MediaWiki upload API instead of forcing `ignorewarnings`;
10. add structured source statements and display the canonical file URL.

LLM category and Wikidata-item suggestions are optional and never submitted
without review. Suggestions state which local or remote model received which
text. No token or private note is included.

Internet Archive follows a similar plan with service-specific duplicate keys
and metadata. A transfer to one service does not imply permission to transfer
to another.

## Authentication and secrets

The current pre-alpha configuration layer accepts API keys as plain strings.
The YouTube setup popup masks input, states the exact destination, and
atomically stores the selected value in the user-only configuration file. The
key remains plaintext at rest. `YOUTA_PROVIDERS__YOUTUBE_API_KEY` and other
environment overrides take precedence over TOML and avoid this persistent
copy. System-keyring and explicit secret-reference backends remain roadmap
work.

Diagnostics redact tokens, cookies, authorization headers, signed URLs, and
provider query secrets. API credentials never enter SQLite, OPML, git sync,
crash-state snapshots, logs, LLM prompts, or child command lines when a safer
file descriptor or environment channel exists. One explicit exception is a
user-supplied private RSS query parameter: it is part of the subscribed feed
URL and is stored in the private OPML file, while its popup debug
representation remains redacted. RSS URLs with embedded usernames or passwords
are rejected.

Future OAuth adapters should use a local callback with state and PKCE where the
provider supports it. Refresh-token storage will be keyring-backed. Provider
scopes must be minimized and documented next to the action that needs them.

## Git and cloud sync (**roadmap**)

Automatic git sync is off by default. When enabled, Youta commits only an
allowlist of portable state exports after an atomic application transaction.
Tokens, the SQLite write-ahead log, IPC sockets, caches, downloads, and media
are excluded. Push failures remain queued and never roll back the local action.

Debounced commits are the default because commit-per-keystroke history wastes
storage and power. A strict per-change option can exist for users who accept
that cost. All generated commit messages are deterministic and shell-safe.

Cloud sync is one-way backup in the first implementation. Remote restore is a
separate, explicit workflow with conflict preview. Listening from a remote
folder uses a read/cache interface and cannot silently turn backup into
bidirectional sync.

## Resource and power budgets

- Draw on input, state change, or a low-frequency playback tick; do not animate
  hidden screens.
- Use one shared HTTP client and bounded worker pool.
- Coalesce progress writes, scrobbles, and subscription refreshes.
- Poll subscriptions only while Youta is open and honor cache validators.
- Suspend auto-download on metered connection or low battery when detectable
  and configured.
- Bound every cache by bytes as well as entry count.
- Encode terminal thumbnails and render waveforms only for the selected item;
  background thumbnail work stops after bounded validation and cache storage.
- Prefer event notifications from `mpv` over high-frequency property polling.

The funny DOS-RPG theme and Nyan Cat seek bar are presentation modes. They must
not increase the default render frequency or network activity.

## Testing

The core is designed for deterministic tests:

- reducers receive events and a clock, then return state plus effects;
- provider tests use recorded, minimized mock JSON and a local mock server;
- persistence tests use temporary databases and exercise every migration;
- OPML tests cover nested folders, duplicate feeds, invalid XML, and round
  trips;
- playback tests use a fake backend plus a fake JSON IPC server;
- `yt-dlp` tests use a mock executable that records argv and emits controlled
  progress;
- TUI tests render to an in-memory backend at small and large terminal sizes;
- end-to-end tests launch the binary in a pseudo-terminal without network.

Network-dependent live-provider checks are scheduled separately and do not make
ordinary pull requests flaky. Coverage is generated in CI, but meaningful
branch and failure-path assertions matter more than a single percentage.

## Build features and packaging

Each provider or costly subsystem has a Cargo feature. Features select code;
runtime configuration selects whether compiled code is active. Default builds
include both official YouTube metadata and Invidious adapters; runtime
`providers.youtube_backend` chooses one. Distribution/minimal builds can use
`--no-default-features`.

Tagged releases are produced natively on amd64 and arm64. The release also
contains a `cargo vendor` archive and matching Cargo source configuration so
Gentoo and other external/offline builders use the exact locked dependency
graph. The Gentoo ebuild still prefers system executables such as `mpv`,
`yt-dlp`, and FFmpeg rather than bundling them.
