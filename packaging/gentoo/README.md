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

The binary package cannot remove a compiled provider without separate
upstream release variants. Use `media-sound/youta`, not `youta-bin`, when
individual provider removal is required; no unsupported binary USE toggle
is supplied here.

Run the mocked, offline phase-selection tests with:

```sh
cargo test --locked --no-default-features --test gentoo_packaging
```

The tests require Bash but do not invoke Portage, download distfiles, or build
Rust/GUI executables. Actual release packaging must also run the overlay's
normal [pkgcheck](https://pkgcore.github.io/pkgcheck/) and Manifest checks.
