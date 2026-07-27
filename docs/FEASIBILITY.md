# Feasibility and roadmap

This document validates the requested surface before implementation. “Feasible”
means an integration has a defensible technical route; it does not mean the
current binary supports it.

## Key decisions

1. **Use both official metadata and opt-in resolver modes, but never blur their
   policies.** The official YouTube Data API is useful for metadata and
   authorized account actions. Its developer policies prohibit downloading,
   offline playback, separating audio, background playback, and interfering
   with ads. Invidious and `yt-dlp` therefore live behind separate,
   user-selected adapters.
2. **Keep external `mpv` first.** JSON IPC provides playback events and
   commands while Youta owns the screen and seek bar. This gives mature codec
   and ALSA support without embedding a second UI.
3. **Use OPML for subscriptions and human-readable TOML for default state.**
   OPML migrates feeds and folder outlines but has no standard representation
   for private notes, history, bookmarks, or playback positions. Deterministic
   TOML split across `state/progress.toml`, `history.toml`, `notes.toml`,
   `bookmarks.toml`, `statistics.toml`, and `playlists.toml` is the default
   source of truth; SQLite is an optional alternative selected with
   `sqlite-state`.
4. **Do not render thumbnails on an unsupported TTY.** Terminal graphics are
   optional runtime capabilities, not an assumption.
5. **Stage providers.** Shipping dozens of brittle adapters together would
   weaken startup time, tests, security, and maintenance. Each service needs a
   capability matrix, mock fixtures, rate limits, and a maintainer.
6. **Do not let “audiophile mode” modify the operating system.** Device
   selection and visible signal-path controls belong in Youta; CPU governor,
   scheduler, and kernel tuning remain documented, external, reversible user
   choices.

The file backend also has an implemented single-writer rule: Youta holds an
exclusive `state/.lock` while it is open. Users should close Youta before
editing human-readable state so in-memory data cannot overwrite a manual edit.

Private notes are implemented as one 16 KiB UTF-8 note per exact
provider-qualified media or source target. The `m` action opens a multiline
Add/Edit/Delete popup for videos, tracks, podcast episodes, direct-source media,
local files, channels, Bandcamp releases, subscriptions, and podcast shows.
Downloaded, History, and playlist selections reuse the underlying media target
instead of creating screen-specific notes. The existing-note action is visibly
highlighted, and both TOML and optional SQLite persistence survive restart.
General multi-note notebooks, rich text, and remote note synchronization remain
roadmap work.

## YouTube capability matrix

| Request | Route | Feasibility and constraint |
| --- | --- | --- |
| Search videos/channels | Official API or Invidious | Implemented; official search has quota cost and API-key configuration. |
| Video title, description, date, duration, thumbnails, likes, license | Official API or Invidious | Implemented where fields are public. “Watched” is local state, not a public video statistic. |
| Channel playlists/uploads | Official API or Invidious | Feasible with pagination. The official channel resource exposes an uploads playlist. |
| Local channel subscription | OPML + channel feed/provider polling | Implemented without changing a YouTube account. |
| YouTube account subscription sync | Official API + OAuth | Roadmap: optional bidirectional sync with consent, conflict handling, and a preview of local and remote additions/removals. An API key alone is insufficient. |
| Read/post comments | Official API + OAuth for writes | Feasible; comments can be disabled or unavailable. |
| Sync watched percentage/position | Official API | Not exposed. The API explicitly does not provide access to watch history playlists. Keep Youta state local. |
| Fetch captions | Invidious, `yt-dlp`, or owner-authorized API route | Feasible when a track exists and access permits it; auto-generated captions are not guaranteed. Cache searchable text separately. |
| Chapters/timecodes | Provider metadata, description parser, media chapters | Feasible. Merge sources with provenance and avoid duplicate timestamps. |
| Audio-only playback/download | `yt-dlp`/Invidious resolver + player, opt-in | Technically feasible, but not an official YouTube API capability. User is responsible for applicable terms and rights. |
| Open video in browser/copy URL | Local action | Feasible. Browser launch remains explicit and headless-safe. |
| Send to NotebookLM | Browser/deep link | A stable public ingestion API is not assumed. Prefer copy/open actions; do not automate a private web UI. |
| Analyze with Codex/Claude/other CLI | Supervised child adapter | Feasible when installed, explicitly enabled, and given a bounded caption/transcript. Never construct a shell string or expose tokens/private notes. |

The implemented official metadata adapter calls
[`search.list`](https://developers.google.com/youtube/v3/docs/search/list),
[`videos.list`](https://developers.google.com/youtube/v3/docs/videos/list), and
[`channels.list`](https://developers.google.com/youtube/v3/docs/channels/list).
It uses `providers.youtube_api_key`; the alternative Invidious adapter uses
`providers.invidious_base_url`. `providers.youtube_backend` accepts `auto`,
`official`, or `invidious`. Auto mode prefers a configured API key when its
adapter is compiled, then falls back to Invidious. The TUI and CLI search paths
share this selection.

Attempting a TUI search with neither provider configured opens a setup popup
for either value. It displays both exact destinations before saving:
`secrets/credentials.toml` for a YouTube API key and `config.toml` for an
Invidious instance URL. On Unix, the save uses mode `0700` for private
directories and `0600` for files; a stored API key is plaintext.
`YOUTA_PROVIDERS__YOUTUBE_API_KEY` overrides the saved value. This API path is
metadata-only: `yt-dlp` still resolves media and the invisible `mpv` process
still performs playback.

The official video resource exposes `status.license`, where uploaders may choose
the Standard YouTube License or Creative Commons. A Creative Commons value is a
necessary transfer check, not sufficient copyright proof.

## SponsorBlock and DeArrow

SponsorBlock's read API returns crowdsourced timestamp ranges with categories,
actions, votes, and duration context. Youta can cache those ranges and perform
a normal playback seek when an enabled category begins. It skips in-video
segments such as sponsor messages. It does **not** block YouTube's
platform-inserted/native advertisements, and using it alongside an official
YouTube API player would conflict with YouTube policy.

Privacy-aware hash-prefix queries should be preferred where practical.
Submission/voting is a later authenticated feature with upstream rate and
automation rules; read-only support comes first.

DeArrow can supply a crowdsourced title and thumbnail timestamp. Title
replacement is toggleable globally and per item, with the original always
inspectable. A thumbnail timestamp is not itself an image; generation may fail,
so the detail panel keeps a normal fallback. No thumbnail is fetched on an
unsupported TTY.

References:

- [SponsorBlock API](https://wiki.sponsor.ajay.app/w/API_Docs)
- [DeArrow API](https://wiki.sponsor.ajay.app/w/API_Docs/DeArrow)
- [DeArrow project](https://dearrow.ajay.app/)

## Wikimedia, Wikidata, and Internet Archive

### Wikidata discovery

The implemented enrichment is deliberately limited to exact external-ID
statements in the public [Wikidata Query Service
(WDQS)](https://wikitech.wikimedia.org/wiki/Wikidata_Query_Service/Technical_interactions):

| Selected object | Exact property and accepted identifier |
| --- | --- |
| YouTube video | [P1651](https://www.wikidata.org/wiki/Property:P1651), a validated 11-character video ID |
| YouTube channel | [P2397](https://www.wikidata.org/wiki/Property:P2397), a 24-character `UC…` channel ID |
| SoundCloud account/track | [P3040](https://www.wikidata.org/wiki/Property:P3040), the exact account or `account/track` path |
| Bilibili video | [P6456](https://www.wikidata.org/wiki/Property:P6456), an exact lowercase `av` number or 12-character `BV…` ID |
| Bilibili channel/user | [P6455](https://www.wikidata.org/wiki/Property:P6455), a positive numeric UID |

The request is lazy: selecting a YouTube video/channel result, or opening a
recognized SoundCloud/Bilibili direct link, triggers it. Startup does not.
Queries run outside the render loop and responses are bounded to 512 KiB and
20 bindings. The selected persistence backend caches a successful positive
result for seven days and a successful empty result for 24 hours; transport
and malformed-response failures are not cached as “not found.”

URL extraction is intentionally strict. SoundCloud recognizes canonical
`soundcloud.com`, `www.soundcloud.com`, or `m.soundcloud.com` links with one
account segment or two account/track segments. A track lookup checks both its
exact path and its exact account value. It does not follow SoundCloud short
redirects. Bilibili recognizes `[www.]bilibili.com/video/{BV…|av…}` and
`space.bilibili.com/{numeric-UID}`; `b23.tv` and other redirect hosts are not
resolved for enrichment. YouTube channel handles and custom names are not
substitutes for P2397 IDs.

There is no title/name matching, fuzzy ranking, generic canonical-URL matching,
or claim that Wikidata covers every selected item. Podcast and audiobook
catalog discovery remains separate from this exact-ID enrichment.

### Wikimedia Commons transfer

Transfer is feasible with MediaWiki OAuth and the upload/Wikibase APIs. Before
showing the final confirmation Youta must:

- accept only a Commons-compatible license and display attribution;
- warn about third-party material despite the source license marker;
- search by source URL, external ID, normalized filename/title, and later
  content hash;
- inspect duplicate/deleted-file warnings from the upload API;
- prepare Ogg Opus audio or WebM with VP9/AV1 plus Opus where a direct
  compatible stream is unavailable;
- retain the YouTube URL in file-page source metadata;
- add structured source/creator/depicts statements where supported;
- keep model-suggested Wikidata items and categories editable;
- append `Uploaded by Youta` after a blank line;
- display the canonical uploaded-file URL.

Commons also accepts other formats, including FLAC, WAVE, Ogg Vorbis and, for
eligible users, MP3. Youta chooses a narrow preferred open profile; it should
not claim those are Commons' only formats.

References:

- [Commons: YouTube files](https://commons.wikimedia.org/wiki/Commons:YouTube_files)
- [Commons file types](https://commons.wikimedia.org/wiki/Commons:File_types)
- [Commons structured data](https://commons.wikimedia.org/wiki/Commons:Structured_data)
- [MediaWiki upload API](https://www.mediawiki.org/wiki/API:Upload)

### Internet Archive

An authenticated S3-compatible upload API exists, so transfer is feasible.
Duplicate detection should use a stored source-URL metadata field, normalized
title/creator search, identifier lookup, and checksum after staging. Archive
metadata and collection rules still require review. Creative Commons status on
YouTube does not guarantee Internet Archive scope or ownership.

## Podcasts, audiobooks, and radio

| Source | Plan | Notes |
| --- | --- | --- |
| RSS/Atom | Core | Feed URL is canonical; enclosure media and Podcasting 2.0 chapters can be optional enhancements. |
| Apple Podcasts | Catalog search then RSS | Search is feasible; Apple playback-position/subscription sync has no suitable public cross-platform API. |
| gpodder.net | Tier 1 | Device/subscription APIs plus JSON episode actions (`started`, `position`, `total`, and `timestamp`) fit optional import/export/sync; TOML state remains authoritative. |
| Podcast Index | Tier 1 | Broad search with API credentials and attribution requirements. |
| Wikidata | Enrichment | Useful linked-data search, not complete enough as the only catalog. |
| LibriVox | Tier 1 | Open catalog and public-domain recordings; retain author/reader/license metadata. |
| Wikimedia Commons | Tier 1 | Category/API search plus structured-data enrichment. |
| Internet Archive | Tier 1 | Advanced search and metadata APIs; stream formats vary. |
| Online radio | Implemented core | The account-free `radio` feature uses a static, zero-startup-network catalogue of reviewed direct streams and M3U entry points. A dynamic Radio Browser adapter remains optional future discovery work. |
| BBC Radio | Core adapter | Official podcast/RSS feeds use the RSS path; BBC Sounds landing URLs can resolve through `yt-dlp`. Do not assume a stable public Sounds catalog API. |
| Funkwhale | Core adapter | Configurable pods expose a stable REST API and an administrator-controlled subset of Subsonic. Keep pod identity and rate limits explicit. |
| [Jamendo](https://developer.jamendo.com/v3.0/tracks) | Core music adapter | Official v3 track search and lookup require a user-provided application client ID. Keep requests and pagination bounded, use only HTTPS media links, preserve the exact `license_ccurl`, and expose `audiodownload` only when `audiodownload_allowed` is true. CC-NC and CC-ND metadata is not a Wikimedia Commons eligibility decision. |
| [LitRes podcasts](https://docs.litres.ru/public/39063068.html) | Opt-in catalog adapter | Documented CataLit search/details/episode calls require a LitRes-issued application ID and secret and use only the documented anonymous session. Pace calls to one per second. Parse exact public pages only for bounded schema.org metadata and explicit unsigned media; never synthesize podcast download URLs from file IDs or access login-, payment-, DRM-, or signature-gated files. |
| Local folders | Core | Read metadata and artwork without moving files; watch/rescan is configurable. |
| ZIP/RAR | Experimental | Index safely, cap expansion, reject traversal/symlinks, and stream/extract only beneath Youta staging. RAR may need an external tool. |

OPML remains the portable subscription-list format, not a progress format.
gPodder's episode-actions protocol is the closest interoperable model for
podcast progress: a `play` JSON action identifies the feed and enclosure and
can carry `started`, `position`, `total`, and `timestamp`. Youta can map local
current position, known duration, and update time to the latter three fields
and capture the per-play start offset for `started`, without forcing
non-podcast media into a podcast protocol. See the
[gPodder episode-actions API](https://gpoddernet.readthedocs.io/en/latest/api/reference/events.html)
and [gPodder synchronization manual](https://gpodder.github.io/docs/user-manual.html).

Additional good targets are Audiobookshelf, OpenSubsonic, ListenBrainz,
MusicBrainz, and public-library OPDS catalogs. Every catalog result still needs
a legal stream/download capability check.

Local formats requested—Opus, M4A, AAC, FLAC, WAV, MP3, Ogg/OGA, plus audio
tracks in local video—are within `mpv`/FFmpeg's normal scope. Exact availability
depends on how the installed player was built. Codec, container, bitrate,
duration, size, tags, chapters, and artwork can be indexed through a bounded
metadata worker.

## Proprietary and scraper-dependent services

| Service | Feasibility | Decision |
| --- | --- | --- |
| Bandcamp | Unofficial extraction/public pages | Experimental `yt-dlp` resolver first; no account bypass. |
| PeerTube | Public per-instance REST API | First-class configurable instance adapter for video/channel/playlist search. Results may be instance-local, federated-known, or administrator-enabled global-index results. |
| Vimeo | Official token API plus extractor | Direct URL through `yt-dlp` first. Rich search uses a registered Vimeo app/token and API scopes; downloadable-file access is not assumed. |
| RuTube | Extractor; no assumed stable public catalog API | Direct validated URLs first. Defer rich search until a supported official boundary is documented. |
| SoundCloud | Official OAuth 2.1 API plus extractor | Direct URL through `yt-dlp`; rich search/subscriptions only with user-provided app credentials and official access rules. |
| SoundStream | Undocumented read-only v3 web metadata endpoints; search and audio signing require an anonymous token | Exact playlist/clip links only. Keep requests bounded and redirect-free, expose public RSS/enclosure URLs when returned, and do not automate anonymous registration or claim catalog search/playback. |
| Odysee | Public API/extractor options | Separate adapter is plausible after terms and fixtures are reviewed. |
| Rumble | Public pages/extractor | Experimental; expect breakage. |
| Bilibili | API/extractor with regional/auth variation | Separate build flag and fixture tests. |
| Yandex Music/podcasts | Token-based unofficial ecosystem | Experimental; user-supplied token, no bundled credentials. |
| VK audio/video | Restricted and account-sensitive APIs | Defer until a documented lawful API route and scopes are verified. |
| knizhnyvoz.com | Site-specific scraping | Defer; author navigation is feasible but brittle and needs permission/robots review. |
| 4duk.ru | Public MP3 live stream plus bounded current-track JSON | Implemented in the static Radio catalogue. Keep its published HTTP stream/metadata warning visible, send no credentials, retry passive metadata with capped backoff, and do not infer an open-content licence. |
| cloud.mail.ru | Proprietary storage API | Experimental only after OAuth/auth flow review. |
| Telegram channel audio | Telegram client protocol | Use a local user-authorized client such as TDLib, not an AWS Lambda proxy holding user sessions. Bots cannot read arbitrary channel history. |
| RuTracker/torrents | Sequential torrent streaming | Technically feasible but legally and operationally high-risk. Separate disabled-by-default feature; enforce shutdown and configurable seeding. |

An AWS Lambda proxy is a poor default for Telegram: it adds token custody,
cost, privacy exposure, execution limits, and another failure point while not
granting access that the Telegram account or bot lacks.

### Generic `yt-dlp` URLs

The installed `yt-dlp` exposes many built-in extractors. Youta can safely offer
a generic direct-URL action by validating the URL, invoking the fixed executable
without a shell, and normalizing its JSON metadata. This does not imply:

- that every upstream-listed site still works;
- that Youta can search or subscribe on that site;
- that authentication, geo-restriction, DRM, or access control will be
  bypassed; or
- that a technically downloadable item may be lawfully downloaded.

Named adapters add richer capability only after an official API or maintained,
tested source contract exists.

## Tracker music

Tracker modules are compact music programs containing patterns, instruments,
and samples. No single archive has both complete coverage and a uniform modern
API, so each catalog needs a narrow adapter:

| Source | Confirmed access | Youta decision |
| --- | --- | --- |
| [The Mod Archive](https://modarchive.org/) | Official XML API for module/artist/genre search; a user-requested API key is required. Module IDs have HTTPS download URLs. | First structured adapter. Never bundle a project or release key; honor API limits and per-module license metadata. |
| [Modland](https://modland.com/) | HTTPS directory archive, direct files, mirrors, and a compact `allmods.zip` file list containing size/path records; no catalog API. | Enabled by default. Cache the published list and search locally instead of crawling more than 500,000 files. |
| [Scene.org](https://files.scene.org/) | Official unauthenticated HTTPS JSON [search and resolve API](https://files.scene.org/api/), plus direct downloads and mirrors. | Strong next adapter for party and scene music. Results are archives/releases, so inspect them safely and identify playable files. |
| [Amiga Music Preservation](https://amp.dascene.net/) (AMP) | HTTPS HTML form search, composer/module pages, stable `downmod.php` IDs, and a downloadable plain-text offline database; no documented public API was found. | Rate-limited HTML adapter after fixture approval. Amiga-only metadata is valuable; do not couple its page parser to the Modland adapter. |
| [UnExoticA](https://www.exotica.org.uk/wiki/UnExoticA) | MediaWiki game-soundtrack pages and direct LhA archives. The site also provides a Modland search front end, but automated requests can meet browser-verification gates. | Useful game-music adapter, not a dependable generic API. Cache conservatively and report formats the active decoder cannot play. |
| [Aminet](https://aminet.net/tree?path=mods) | HTTPS browse/search for its `mods` tree, direct mirrored LhA packages, per-package readmes, and [RSS updates](https://aminet.net/feed); no JSON catalog API was found. | Strong archive/subscription candidate. Index metadata and extract only bounded media entries beneath Youta staging. |
| [modules.pl](https://www.modules.pl/) | HTTPS HTML module/author search and filters, per-module downloads, newest-module RSS, and ModFM radio. No documented public API was found; the radio playlist is HTTP-only. | Experimental rate-limited HTML/RSS adapter. Keep the HTTP radio stream behind the insecure-transport gate and honor author-blocked items. |
| [Mirsoft Game Music Base](http://www.mirsoft.info/gamemods.php) | Searchable game metadata and individually downloadable ZIP soundtracks for MOD/MED/XM/S3M/IT. The site asks clients not to bulk-download. On 25 July 2026 HTTP returned 200 while HTTPS port 443 refused connections. | Enabled by default at the user's request with a one-time warning. `providers.allow_insecure_http = false` disables it. Never send credentials; rate-limit and never crawl the archive. |
| [Demozoo](https://demozoo.org/) | Public JSON API and database dumps provide production, party, platform, credit, and external-file metadata; search/filter coverage is incomplete. | Later metadata enrichment for Scene.org results, not a primary module-download catalog. |

Mirsoft's plain HTTP exposes searches, metadata, and downloads to observation
and in-transit modification. `allow_insecure_http` is a transport exception,
not a claim of safety. It must not relax TLS requirements for authenticated
providers or permit credentials, cookies, or account identifiers on an HTTP
request.

Playback is delegated to `mpv`/FFmpeg only when the installed FFmpeg includes
libopenmpt. Common formats include MOD, XM, IT, S3M, MPTM, 669, MTM, STM, UMX,
MED, and dozens of legacy variants supported by the installed libopenmpt.
Diagnostics must report the decoder capability before enabling Play.
UnExoticA and Modland contain custom/exotic Amiga formats that libopenmpt does
not cover; a future optional UADE backend is a better fit than silently
transcoding or claiming support.

A module being downloadable from an archive does not make its composition or
samples freely licensed. License metadata and creator terms must be displayed;
Youta never offers automatic Commons or Internet Archive re-upload based only
on catalog presence. Scene.org holds distribution rights only, and
[Aminet warns](https://wiki.aminet.net/Copyright_status_and_disclaimer) that
freely downloadable files are not necessarily freely redistributable.

References:

- [libopenmpt formats and FAQ](https://lib.openmpt.org/libopenmpt/faq/)
- [libopenmpt documentation](https://lib.openmpt.org/libopenmpt/documentation/)
- [Scene.org redistribution policy](https://files.scene.org/faq/)
- [UnExoticA FAQ](https://www.exotica.org.uk/wiki/UnExoticA/FAQ)

## Storage and sync

| Target | Feasibility | First safe scope |
| --- | --- | --- |
| Git repository | Implemented locally | On successful graceful shutdown, path-scoped `git add .`, commit `Automatic state update`, and push; never pull. Existing `.gitignore` rules decide what is included. |
| Google Drive | Feasible with OAuth | Separate one-way state backup and optional media backup. Remote-folder playback uses a cache. |
| WebDAV | Feasible | One-way upload with ETag/precondition checks. |
| SSH/SFTP | Feasible | Host-key verification required; no automatic trust-on-first-use in unattended mode. |
| Yandex Disk | Feasible with documented API | Optional adapter and OAuth token reference. |
| Evernote | Feasible with API limits | Saving a rich item is separate from configuration sync. A single-note append model needs size/conflict handling. |

“Sync only to Evernote and never fetch” is backup, not sync, and should be named
accordingly. Youta avoids commit storms by attempting Git synchronization only
after a successful graceful shutdown, rather than after every position write.
It creates a default `.gitignore` for credentials and generated data only when
the root has none. Users remain in control: edited or removed ignore rules are
honored, and Youta does not refuse a commit merely because it would include an
API key.

Remote media locations are read-only sources unless the user explicitly selects
backup/upload. Encrypted keyring references are portable only as names; a new
machine must provide its own secret values.

## Playback UX feasibility

The requested controls fit the state/back-end design:

- Space toggles pause.
- Left/right seek 5 seconds; Alt-left/right seek 20 seconds.
- Digits seek to 0–90% as on YouTube.
- Up/down adjust volume when the focused widget does not consume them.
- Mouse click maps a seek-bar column to a clamped duration position.
- Chapter and description timecodes push the old position onto a back stack.
- Speed uses 0.1 steps from 0.5× through 3.0×.
- Repeat, play-next, queue append, and completion toggles are reducer state.
- Equalizer is backend DSP and therefore disables bit-perfect status.
- A waveform is a cached peak envelope, not decoded audio held in memory.

“Cut without re-encoding” is conditionally feasible. Container timestamps and
codec frames limit exact boundaries. A cut playlist can always be represented
non-destructively as timestamps. Export by stream copy may begin/end at nearby
packet boundaries and must say so.

## Service interactions

- Last.fm scrobbling is feasible with its API; listen thresholds and offline
  retries need correct handling. ListenBrainz is a useful open alternative.
- Discord Rich Presence is feasible through local IPC and should expose a
  privacy toggle, since listening titles may be sensitive.
- YouTube Music does not have a general public playback API. Treat it as
  YouTube discovery/resolution in opt-in mode, not a promised official adapter.
- Leaving a YouTube comment is feasible with OAuth and explicit confirmation.
- YouTube and Apple Podcasts played-position synchronization is not available
  through suitable public APIs; keep it local.
- Caption text can use a backend-specific text index after language/source
  attribution; SQLite FTS is one optional implementation. An external
  transcription model is an optional local/remote effect when no caption is
  available.
- Hashtags and media links are internal typed actions; external links require a
  browser/open confirmation appropriate to the terminal environment.

## Security gates

The following are required before enabling a provider in a release:

- URL and redirect validation, including private-network policy;
- bounded response, archive, image, transcript, and command-output sizes;
- no shell interpolation;
- explicit child-process lifetime and shutdown;
- secret redaction tests;
- provider timeouts, pagination limits, and rate handling;
- mock-data success and failure tests;
- documented terms/licensing boundary;
- destructive remote writes behind confirmation.

Torrent metadata, archives, remote playlists, captions, descriptions, and LLM
output are untrusted. They never become a filename, terminal escape sequence,
SQL fragment, shell string, or URL without normalization for that sink.

## Delivery phases

### Phase 0 — foundation

Configuration, domain model, default TOML state, optional SQLite, OPML
subscriptions, TUI state, official YouTube Data API and Invidious discovery,
description links, external `mpv` IPC, `yt-dlp` supervision, and diagnostics.

### Phase 1 — useful local player

Queue/history/download screens, position persistence, local media scan, RSS,
chapters, notes/bookmarks/segments, search filters, keyboard/mouse coverage,
and robust offline behavior.

### Phase 2 — open ecosystem

SponsorBlock, DeArrow, Wikidata, Commons/Internet Archive/LibriVox discovery,
gpodder.net, Podcast Index, radio catalogs, and Last.fm/ListenBrainz.

### Phase 3 — reviewed remote writes

Official YouTube OAuth interactions, Commons and Archive transfers, cloud/git
backup, Discord, and external transcript/LLM tools.

### Phase 4 — isolated experimental adapters

Proprietary services, archive streaming, Telegram client support, and optional
torrents. Each ships independently and may remain out of default binaries.

This order creates a dependable player before accumulating fragile services.
