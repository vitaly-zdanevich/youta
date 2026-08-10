# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Youta is a low-resource terminal YouTube/audio player and local subscription
manager in Rust (edition 2024, `rust-version = 1.95`; CI pins `1.95.0`). It
drives an invisible `mpv` child over JSON IPC and uses `yt-dlp` as an optional
resolver/downloader. `README.md` and `docs/ARCHITECTURE.md` are the
authoritative behavioral spec — read the relevant section before changing
behavior, and update them in the same change.

## Commands

```sh
cargo build --locked
cargo run --release --locked              # build and open the TUI
cargo test --locked --all-targets         # unit + integration tests (offline)
cargo test --locked --doc                 # doctests (a separate CI step)
cargo fmt --all -- --check
```

CI's blocking Clippy gate (style/complexity/pedantic stay advisory):

```sh
cargo clippy --locked --all-targets --all-features -- \
  -D unused -D future-incompatible \
  -D clippy::correctness -D clippy::suspicious -D clippy::perf
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
```

Single test (unit tests live inline in the module they cover):

```sh
cargo test --locked --lib config::tests::environment_overrides_toml_in_child_process
cargo test --locked --test e2e -- --exact help_identifies_youtube_and_ytdlp
```

Coverage gate (70% lines, `--lib --bins` only):

```sh
cargo llvm-cov --locked --workspace --all-features --lib --bins \
  --fail-under-lines 70 --lcov --output-path lcov.info
```

### Live/network tests

Every network test is `#[ignore]`d *and* gated on a `YOUTA_RUN_LIVE_*`
environment variable, so an ordinary `cargo test` never touches the network.
They live in `tests/live_services.rs` and `tests/live_youtube.rs` and are run
one-at-a-time by name, e.g.

```sh
YOUTA_RUN_LIVE_YOUTUBE_MUSIC_TEST=1 cargo test --locked --test live_services \
  --no-default-features --features youtube-music -- --ignored \
  --exact youtube_music_keyless_search_returns_playable_tracks_before_timeout --nocapture
```

`scripts/test-live-youtube.sh` is the required local pre-commit check (live
YouTube playback is disabled in hosted CI because GitHub runners get
`LOGIN_REQUIRED`). It decodes silently; `--audible` plays through the default
output. See the README "Packaging and quality" section for the full list.

### Feature-matrix checks

Features are additive and heavily `cfg`-gated; a change that compiles under
default features often breaks a narrow lane. Before pushing, spot-check the
lanes you touched (CI's `feature-contract` job runs ~22 of these):

```sh
cargo check --locked --no-default-features --features tui,local-browser
cargo check --locked --no-default-features --features app-core,images,qr
cargo test  --locked --all-targets --no-default-features   # core only
cargo test  --locked --all-targets --all-features
```

## Architecture

`src/lib.rs` is the feature map: nearly every module is behind a `#[cfg(feature
= ...)]`. `src/main.rs` is a thin clap CLI (`tui`, `search`, `doctor`,
`config`, `extractors`; no subcommand opens the TUI) and requires the `cli`
feature.

Single-threaded reducer, worker threads, **no async runtime**:

```
terminal input ─┐
mouse events ───┼─> AppController reducer ─> UI snapshot ─> ratatui renderer
timers ─────────┤        ├─> provider workers (crossbeam channels)
provider events ┤        ├─> persistence worker
player events ──┘        ├─> mpv JSON IPC worker
                         └─> yt-dlp prewarm/resolve workers
```

- `src/app.rs` (`AppController`) owns all interactive state and is the only
  mutator. Workers return typed events through bounded channels and never touch
  a screen. Several providers get their own *capacity-one, latest-only* lane
  (Apple Podcasts, Bandcamp search/resolve, YouTube Music, Local listings,
  YouTube prewarm) so a slow catalogue cannot block YouTube search or input.
- `src/tui.rs` is input mapping + layout + widgets only. Provider-specific
  response structs must be normalized into `src/domain.rs` types before
  crossing into TUI code.
- `src/domain.rs`: source-neutral identity is `(provider, kind, provider_id)`.
  URLs are attributes, not identity.
- `src/providers/`: adapters advertise *capabilities*; the UI enables an action
  only when the selected source supports it. `Provider` trait in
  `providers/mod.rs`.
- `src/persistence.rs`: `StateBackend` trait. Default backend is deterministic
  TOML files under `~/.config/youta/` (`state/` durable, `runtime/`
  restart-only, `cache/` regenerable); optional `sqlite-state`. Only
  `runtime/*` and `cache/*` may be quarantined and recreated on corruption —
  `state/*` never is, and startup fails instead. An exclusive lock on
  `state/.lock` rejects a second process.
- `src/playback/`: `PlaybackBackend` trait (`playback/mod.rs`), `mpv.rs` (Unix
  socket JSON IPC, requires mpv ≥ 0.38 for per-file `loadfile` options),
  `ytdlp.rs`, `youtube_prewarm.rs`.
- `src/config.rs`: figment layering — file `config.toml`, then
  `secrets/credentials.toml`, then `YOUTA_`-prefixed env with `__` for nesting
  (`YOUTA_PLAYBACK__VOLUME_PERCENT=40`). Env always wins, and an active env
  override blocks the in-app Preferences save for that key.
- `build.rs` generates offline recent-commit metadata from git,
  `.git_archival.txt`, or the checked-in `build/recent-commits.tsv`.

## Working in this repo

- `unsafe_code = "forbid"`. Subprocesses are launched with a fixed executable
  and an argument vector, never a shell; output and metadata sizes are capped;
  children are killed and reaped on cancel/shutdown (Unix resolvers get their
  own process group).
- Secrets, signed media URLs, and auth headers stay in RAM. They must not reach
  durable state, session snapshots, OPML, diagnostics, or child command lines.
  `src/diagnostics.rs` redaction is the boundary — extend it when adding a new
  secret-bearing field.
- Any new provider or costly subsystem gets its own Cargo feature, and must
  still compile with that feature alone and with `--no-default-features`.
- `tests/release_packaging.rs` asserts consistency across `Cargo.toml`
  features, `config.example.toml`, `README.md`, `scripts/package-release.sh`,
  `.github/workflows/ci.yml`, and `.github/workflows/release.yml`. Editing
  features, CI lanes, or release variants will fail these tests until all of
  those files are updated together.
- `src/app.rs` (~58k lines) and `src/tui.rs` (~27k lines) are too large to read
  whole — grep for the symbol, then read around it. Their inline
  `#[cfg(test)] mod tests` start near lines 32169 and 12421 respectively, so
  roughly the back half of each file is tests.
- TUI tests render to an in-memory backend at several terminal sizes;
  `tests/e2e.rs` launches the real binary in a pseudo-terminal with no network.
