# Gentoo source-release template

`youta.ebuild` is an unpublished template for the next source release. It is
not a live ebuild and must not replace an existing version in the overlay.
Its baseline is the published
[0.57.1 source ebuild](https://github.com/vitaly-zdanevich/gentoo-overlay/blob/main/media-sound/youta/youta-0.57.1.ebuild).

After a new tagged release and its vendor archive have been published, copy
the template into the overlay under that release's versioned ebuild filename.
The `${PV}` and `${P}` variables select the source and vendor distfiles.
Regenerate and verify the Manifest using the published bytes, retaining older
versions and entries. This directory intentionally contains no Manifest or
future release number.

The source package defaults to `+archive-org`. `USE="-archive-org"` removes
the provider from the terminal and optional desktop builds. Configuration,
desktop compilation, and desktop tests all disable Cargo defaults and select
the feature explicitly from USE.

`+cmd` enables provider-filtered command buttons in the TUI and GUI, plus the
terminal-only Local-tab `:` Bash prompt. It requires Bash and at least one of
`tui` or `gui`; it no longer requires the `local` USE flag. The feature is
selected explicitly for both frontend builds. `USE="-cmd"` omits the buttons,
runner, terminal editor, and command-history implementation from both.

Buttons use the extensionless `~/.config/youta/commands` TOML file. Youta creates
a disabled `commands.sample`; rename it to `commands` and restart to enable it.
An omitted `provider` matches every eligible provider. `%` passes the selected
original URL (full Local path); `%d` reuses a downloaded file or waits for the
normal download workflow before execution. The GUI uses a blocking output
dialog without standard input; the TUI uses its foreground terminal. See the
[command-button configuration and safety notes](../../README.md#local-files-and-archives)
and [sample](../../commands.sample) for fields, hotkeys, and examples.

The binary package cannot remove a compiled provider without separate
upstream release variants. Use `media-sound/youta`, not `youta-bin`, when
individual provider removal is required; no unsupported binary USE toggle
is supplied here.

`USE="s3-upload"` opts into Amazon S3 uploading in both the terminal and desktop
source builds. It is off by default and does not enable the separate
`archive-upload` feature. Default upstream binaries omit S3; use the source
package to enable it.

Run the mocked, offline phase-selection tests with:

```sh
cargo test --locked --no-default-features --test gentoo_packaging
```

The tests require Bash but do not invoke Portage, download distfiles, or build
Rust/GUI executables. Actual release packaging must also run the overlay's
normal [pkgcheck](https://pkgcore.github.io/pkgcheck/) and Manifest checks.
