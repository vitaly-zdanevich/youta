# Production-code line count

The README's **production code** badge counts nonblank physical lines containing
first-party application code. Comments and documentation-only lines do not
count; a line containing both code and a comment counts once.

Included:

- Rust modules reachable from `src/lib.rs`, `src/main.rs`, `gui/src/main.rs`,
  `build.rs`, and `gui/build.rs`, counting shared files once.
- Desktop frontend source under `gui/ui/src/`, plus its HTML entry point and
  Vite build configuration.
- Optional features and platform-specific implementations, regardless of the
  machine running the counter. This is a source count, not compiled binary size.

Excluded:

- Inline Rust test modules, test-only functions, fields, statements, imports and
  out-of-line modules. `cfg(test)` is disabled; feature/platform conditions
  remain unknown, so `cfg(any(windows, test))` still counts as production.
- Frontend test/spec files and test/fixture directories; integration tests,
  examples, benchmarks, CI and packaging scripts.
- Generated `*_generated` sources, including NPR station snapshots; generated
  schemas, JSON data, lockfiles, dependencies and build output.
- Untracked files. Stage new production files before regenerating the badge.

The counter follows Rust syntax using [Tree-sitter's Rust grammar](https://github.com/tree-sitter/tree-sitter-rust)
and classifies comments with [Pygments](https://pygments.org/docs/api/).
It does not compile or expand macros, fetch application dependencies, or measure
runtime feature combinations. Parse errors and unresolved production modules
fail instead of silently publishing a partial count.

The adjacent SonarCloud badge has a different scope: it analyzes `src/`,
including inline tests and generated data, but not the desktop frontend.
Neither badge includes downloaded dependency source.

## Update and verify

From the repository root, using Python 3.10 or newer:

```sh
python3 -m venv /tmp/youta-production-loc-venv
/tmp/youta-production-loc-venv/bin/python -m pip install -r scripts/production-loc-requirements.txt
/tmp/youta-production-loc-venv/bin/python -m unittest discover -s scripts/tests -p 'test_production_loc.py'
/tmp/youta-production-loc-venv/bin/python scripts/production_loc.py --write
/tmp/youta-production-loc-venv/bin/python scripts/production_loc.py --check
```

The command prints a per-file breakdown and total. Commit the regenerated
`docs/badges/production-code.svg` with production changes. CI runs the same
tests and `--check`, rejecting stale badges without making bot commits or
requiring a badge-hosting service. The parser packages are development-only;
they are not dependencies of Youta or its Gentoo packages.
