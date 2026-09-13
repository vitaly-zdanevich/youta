//! Mocked feature-selection tests for the unpublished Gentoo release template.
//!
//! No Portage installation, downloads, GUI libraries, or Cargo builds are
//! needed: the shell fixture records the ebuild's selected features and args.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;

/// Evaluates only the configuration, compilation, and testing phase dispatch.
fn evaluate_template(archive_org: bool, ascii_visualizer: bool) -> String {
    let template = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("packaging/gentoo/youta.ebuild");
    assert!(
        template.is_file(),
        "the future-release source template is missing"
    );
    let script = r#"
inherit() { :; }
use() { [[ " ${USE_FIXTURE} " == *" $1 "* ]]; }
usev() { if use "$1"; then printf '%s\n' "${2:-$1}"; fi; }
usex() { if use "$1"; then printf '%s' "$2"; else printf '%s' "$3"; fi; }
die() { printf '%s\n' "$*" >&2; exit 1; }
cargo_src_configure() {
	printf 'TUI'; printf '|%s' "${myfeatures[@]}"; printf '\n'
	printf 'CONFIGURE'; printf '|%s' "$@"; printf '\n'
}
cargo_src_compile() { :; }
cargo_src_test() { :; }
cargo_env() { printf 'GUI'; printf '|%s' "$@"; printf '\n'; }
source "$1"
printf 'IUSE'; for flag in ${IUSE}; do printf '|%s' "${flag}"; done; printf '\n'
src_configure
src_compile
src_test
"#;
    let output = Command::new("bash")
        .args(["-ec", script, "gentoo-template-test"])
        .arg(template)
        .env(
            "USE_FIXTURE",
            format!(
                "gui {} {}",
                if archive_org { "archive-org" } else { "" },
                if ascii_visualizer {
                    "ascii-visualizer"
                } else {
                    ""
                },
            ),
        )
        .env("PN", "youta")
        .env("PV", "99.0.0")
        .env("P", "youta-99.0.0")
        .env("S", "/unused/youta-99.0.0")
        .env("CARGO", "cargo")
        .output()
        .expect("run mocked ebuild phases");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 phase trace")
}

/// Extracts one phase's argument vector from the deterministic shell trace.
fn trace_fields<'a>(trace: &'a str, phase: &str) -> Vec<&'a str> {
    trace
        .lines()
        .find(|line| line.starts_with(phase))
        .unwrap_or_else(|| panic!("missing phase {phase}: {trace}"))
        .split('|')
        .collect()
}

#[test]
fn archive_org_is_default_on_in_the_future_source_ebuild() {
    let trace = evaluate_template(true, false);
    let flags = trace_fields(&trace, "IUSE|");
    assert!(flags.contains(&"+archive-org"));
    assert!(!flags.contains(&"archive-org"));
}

#[test]
fn archive_org_use_controls_terminal_and_both_desktop_phases() {
    for (enabled, ascii_visualizer) in [(true, false), (false, false), (true, true), (false, true)]
    {
        let trace = evaluate_template(enabled, ascii_visualizer);
        let features = trace_fields(&trace, "TUI|");
        assert_eq!(features.contains(&"archive-org"), enabled);
        assert_eq!(features.contains(&"ascii-visualizer"), ascii_visualizer);
        let configure = trace_fields(&trace, "CONFIGURE|");
        assert!(configure.contains(&"--no-default-features"));
        assert!(configure.contains(&"--locked"));
        for operation in ["build", "test"] {
            let fields = trace_fields(&trace, &format!("GUI|cargo|{operation}|"));
            assert!(fields.contains(&"--no-default-features"));
            assert!(fields.contains(&"--offline"));
            assert!(fields.contains(&"--locked"));
            let selected = fields
                .windows(2)
                .find_map(|pair| (pair[0] == "--features").then_some(pair[1]))
                .unwrap_or("");
            assert_eq!(
                selected.split(',').any(|feature| feature == "archive-org"),
                enabled,
                "GUI {operation} must follow the archive-org USE flag"
            );
            assert_eq!(
                selected
                    .split(',')
                    .any(|feature| feature == "ascii-visualizer"),
                ascii_visualizer,
                "Archive selection must preserve other GUI feature separators"
            );
        }
    }
}
