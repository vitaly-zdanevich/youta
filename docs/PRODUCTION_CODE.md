# Production and test code line counts

The README's **production code** badge counts nonblank physical lines containing
first-party application code. Comments and documentation-only lines do not
count; a line containing both code and a comment counts once. The **test code**
badge counts test-only code separately. Both are source counts, not counts of
test cases, executable instructions, dependencies, or compiled binary size.

Included:

- Rust modules reachable from `src/lib.rs`, `src/main.rs`, `gui/src/main.rs`,
  `build.rs`, and `gui/build.rs`, counting shared files once.
- Desktop frontend source under `gui/ui/src/`, plus its HTML entry point and
  Vite build configuration.
- Embedded Lua playback hooks under `src/`.
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

## Test code

Included:

- Rust code reachable with `cfg(test)` enabled from the same entry points and
  `tests/*.rs` / `tests/*/main.rs` integration roots. This includes inline tests, test-only
  helpers/fields/imports, and separate test modules, even with neutral filenames.
- Frontend `.test.*` / `.spec.*` files and `tests/` / `__tests__/` directories,
  executable test fixtures, and Python/shell test programs under `tests/` and
  `scripts/tests/`.
- Shared test modules once, including helpers imported by both GUI and CLI tests.

Lines already counted as production are excluded from the test count. A physical
line containing both runtime code and a test item belongs to production only.
Feature/platform branches remain included; this is not the set of tests run on
one particular machine. Comments, Python docstrings, blank lines, generated
sources, data snapshots, dependencies, build output and untracked files do not
count. Orphan Rust files are not tests merely because their names look like tests.

## Method and limitations

The counter follows Rust syntax using [Tree-sitter's Rust grammar](https://github.com/tree-sitter/tree-sitter-rust)
and classifies comments with [Pygments](https://pygments.org/docs/api/).
It does not compile or expand macros, fetch application dependencies, or measure
runtime feature combinations. Parse errors, unresolved modules, and unsupported
conditional module paths fail instead of silently publishing a partial count.

The adjacent SonarCloud badge has a different scope: it analyzes `src/`,
including inline tests and generated data, but not the desktop frontend.
None of the three badges includes downloaded dependency source. The SonarCloud
number is not the sum of the two local badges: its scope and counting rules
differ, and its result comes from the most recent completed analysis.

## Interpreting size and finding code

A large source count does not demonstrate dead code. Tests exercise behavior;
optional providers and platform implementations are not all compiled into one
build. Public library functions can have consumers outside this repository.
Compiler `unused` checks catch many private-code mistakes, but neither those
checks nor this source counter prove that all code is needed.

The largest navigation problem is the controller and terminal renderer combining
runtime implementation with large inline test modules. See the
[source map](ARCHITECTURE.md#source-map) before reading an entire large file.
Extracting those tests into modules would improve browsing, but would not reduce
runtime code, binary size, or the amount of coverage worth preserving. Prefer
small behavior-preserving extractions or consolidations, with regression tests,
over deleting APIs or optional code based only on reference counts.

## Update and verify

From the repository root, using Python 3.10 or newer:

```sh
python3 -m venv /tmp/youta-production-loc-venv
/tmp/youta-production-loc-venv/bin/python -m pip install -r scripts/production-loc-requirements.txt
/tmp/youta-production-loc-venv/bin/python -m unittest discover -s scripts/tests -p 'test_production_loc.py'
/tmp/youta-production-loc-venv/bin/python scripts/production_loc.py --write
/tmp/youta-production-loc-venv/bin/python scripts/production_loc.py --check
```

The command prints production `total` / `files` plus a separate `tests` object
with its own `total` / `files`. Commit the regenerated
`docs/badges/production-code.svg` and `docs/badges/test-code.svg` with source or
test changes. CI runs the same
tests and `--check`, rejecting stale badges without making bot commits or
requiring a badge-hosting service. The parser packages are development-only;
they are not dependencies of Youta or its Gentoo packages.
