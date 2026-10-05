//! Regression contracts for shared CI gates and publication ordering.

/// Reads workflow definitions from the checked-out revision under test.
fn workflow(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".github/workflows")
        .join(name);
    std::fs::read_to_string(path).expect("repository workflow")
}

/// Isolates a job so another job cannot accidentally satisfy a gate assertion.
fn job<'a>(workflow: &'a str, name: &str) -> &'a str {
    let marker = format!("\n  {name}:\n");
    let body = workflow.split_once(&marker).expect("required job").1;
    let mut end = 0;
    for line in body.split_inclusive('\n') {
        if line.starts_with("  ") && !line.starts_with("    ") && !line.trim().is_empty() {
            break;
        }
        end += line.len();
    }
    &body[..end]
}

/// Release publication must wait for the same complete suite used by branches.
#[test]
fn release_publication_requires_shared_ci_and_tag_validation() {
    let release = workflow("release.yml");
    let validate = job(&release, "validate");
    assert!(validate.contains("needs: validate-tag"));
    assert!(validate.contains("uses: ./.github/workflows/ci.yml"));
    assert!(validate.contains("release-validation: true"));
    assert!(!validate.contains("if:"));
    assert!(!validate.contains("continue-on-error:"));
    assert!(!validate.contains("secrets: inherit"));
    assert!(job(&release, "validate-tag").contains("Require a matching version tag"));
    for name in ["binaries", "desktop", "desktop-i686", "vendor"] {
        assert!(job(&release, name).contains("needs: validate"), "{name}");
    }
    assert!(job(&release, "publish").contains("      - validate\n"));
    assert!(job(&release, "publish-crates").contains("needs: publish"));
    for duplicated in ["cargo fmt", "cargo clippy", "cargo doc", "cargo test"] {
        assert!(!validate.contains(duplicated), "{duplicated}");
    }
}

/// Tag deduplication must retain all branch pushes and normal pull requests.
#[test]
fn shared_ci_retains_branch_triggers_and_declares_boolean_release_mode() {
    let ci = workflow("ci.yml");
    let events = ci.split_once("\npermissions:").unwrap().0;
    assert!(events.contains("  workflow_call:\n"));
    assert!(events.contains("      release-validation:\n"));
    assert!(events.contains("        type: boolean\n        default: false"));
    assert!(events.contains("  push:\n    branches:\n      - '**'\n"));
    assert!(events.contains("    tags-ignore:\n      - 'v*'"));
    assert!(events.contains("  pull_request:\n"));
    assert!(events.contains("  workflow_dispatch:\n"));
    for name in [
        "lint",
        "test",
        "feature-contract",
        "linux-i686-compile",
        "desktop",
        "desktop-linux-i686",
        "windows-compile",
        "freebsd-compile",
        "e2e",
        "coverage",
    ] {
        let gate = job(&ci, name);
        assert!(gate.contains("timeout-minutes: 360"), "{name}");
        assert!(!gate.contains("continue-on-error:"), "{name}");
        assert!(
            !gate.lines().any(|line| line.starts_with("    if:")),
            "{name}"
        );
    }
    assert!(job(&ci, "desktop").contains("npm --prefix gui/ui run test:browser"));
    assert!(job(&ci, "coverage").contains("--fail-under-lines 70"));
}

/// The production-code badge must be tested and rejected when its count is stale.
#[test]
fn production_code_badge_has_a_read_only_reproducible_gate() {
    let ci = workflow("ci.yml");
    let badge = job(&ci, "production-code");
    assert!(badge.contains("timeout-minutes: 360"));
    assert!(badge.contains("scripts/production-loc-requirements.txt"));
    assert!(badge.contains("test_production_loc.py"));
    assert!(badge.contains("scripts/production_loc.py --check"));
    assert!(!badge.contains("contents: write"));
    assert!(!badge.contains("--write"));
    assert!(!badge.contains("continue-on-error:"));
}

/// Consolidation must retain behavior and documentation checks formerly in Release.
#[test]
fn shared_feature_matrix_preserves_release_only_boundaries() {
    let ci = workflow("ci.yml");
    let matrix = job(&ci, "test");
    for features in [
        "app-core,images",
        "audio-quality",
        "tui,audio-quality",
        "ascii-visualizer",
        "summary",
        "youtube-captions",
        "tui,summary",
        "sponsorblock",
        "evernote",
        "tui,evernote",
        "lan-sharing",
        "soundcloud",
        "tui,soundcloud",
        "tui,soundcloud,wikidata",
    ] {
        assert!(
            matrix.contains(&format!(
                "feature_arguments: --no-default-features --features {features}\n"
            )),
            "{features}"
        );
    }
    assert!(matrix.contains("cargo test --locked --all-targets ${{ matrix.feature_arguments }}"));
    assert!(matrix.contains("cargo test --locked --doc ${{ matrix.feature_arguments }}"));
    for features in [
        "controller,web-browser",
        "controller,web-browser,local-metadata",
        "controller,web-browser,local-artwork",
    ] {
        assert!(
            ci.contains(&format!(
                "cargo test --locked --lib --no-default-features --features {features} web_\n"
            )),
            "{features}"
        );
    }
}

/// External-service probes run on ordinary CI but cannot become release gates.
#[test]
fn live_service_gates_respect_release_mode() {
    let ci = workflow("ci.yml");
    for name in [
        "live-youtube",
        "live-youtube-music",
        "live-apple-podcasts",
        "live-librivox",
        "live-wikidata",
        "live-radio",
    ] {
        let probe = job(&ci, name);
        assert!(
            probe
                .lines()
                .any(|line| line.starts_with("    if:")
                    && line.contains("!inputs.release-validation")),
            "{name}"
        );
    }
    assert!(job(&ci, "live-youtube").contains("vars.RUN_LIVE_YOUTUBE == 'true'"));
}

/// Slow live Wikidata requests retain bounded retries and all real-service checks.
#[test]
fn wikidata_live_probe_retains_bounded_retries_and_real_provider_checks() {
    let ci = workflow("ci.yml");
    let probe = job(&ci, "live-wikidata");
    assert!(probe.contains("timeout-minutes: 360"));
    assert!(!probe.contains("continue-on-error:"));
    assert!(!probe.contains("|| true"));
    assert_eq!(probe.matches("for attempt in 1 2; do").count(), 4);
    assert_eq!(probe.matches("--exact ").count(), 4);
    for fixture in [
        "wikidata_finds_the_youtube_video_fixture_item",
        "wikidata_finds_the_youtube_channel_fixture_item",
        "wikidata_loads_the_media_fixture_statements",
        "wikidata_loads_the_follower_history_fixture",
    ] {
        let exact_fixture = format!("--exact {fixture} ");
        assert_eq!(probe.matches(&exact_fixture).count(), 1, "{fixture}");
        let step = probe
            .split("      - name:")
            .find(|step| step.contains(&exact_fixture))
            .expect("each live fixture retains its own CI step");
        for required in [
            "YOUTA_RUN_LIVE_WIKIDATA_TEST: '1'",
            "for attempt in 1 2; do",
            "if cargo test",
            "--locked",
            "--test live_services",
            "--no-default-features",
            "--features wikidata",
            "--ignored",
            "exit 0",
            "if [ \"${attempt}\" -eq 2 ]; then",
            "exit 1",
            "sleep 10",
        ] {
            assert!(step.contains(required), "{fixture} omits `{required}`");
        }
    }

    let live_tests = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/live_services.rs"),
    )
    .expect("live-service test source");
    let helper = live_tests
        .split_once("fn live_wikidata_provider()")
        .expect("the Wikidata fixtures share one live client")
        .1
        .split_once("\n}\n")
        .expect("the live-provider helper has a complete body")
        .0;
    let compact_helper = helper
        .split_whitespace()
        .collect::<String>()
        .replace(",)", ")");
    assert!(
        compact_helper
            .contains("WikidataProvider::with_request_timeout(std::time::Duration::from_secs(65))"),
        "live probes allow the documented query deadline without changing the interactive default"
    );
    assert!(!compact_helper.contains("WikidataProvider::new("));
}

/// Diagnostic artifacts would corrupt Release's strict deliverable inventory.
#[test]
fn shared_ci_does_not_upload_diagnostics_into_release_artifacts() {
    let ci = workflow("ci.yml");
    let uploads: Vec<_> = ci
        .split("      - name:")
        .filter(|step| step.contains("uses: actions/upload-artifact@"))
        .collect();
    assert_eq!(uploads.len(), 4);
    for upload in uploads {
        assert!(upload.contains("if: ${{ !inputs.release-validation }}"));
    }
}

/// Offline baselines report measurements without imposing timing thresholds.
#[test]
fn performance_baseline_builds_before_measuring_and_checks_report_shape() {
    let ci = workflow("ci.yml");
    let performance = job(&ci, "performance");
    assert!(performance.contains("timeout-minutes: 360"));
    assert!(!performance.contains("continue-on-error:"));
    assert!(!performance.lines().any(|line| line.starts_with("    if:")));
    assert!(
        performance
            .contains("python3 -m unittest discover -s scripts/tests -p 'test_performance.py'")
    );
    assert!(performance.contains("python3 scripts/performance.py --output-dir performance-report"));
    let runner = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/performance.py"),
    )
    .unwrap();
    let main = runner.split_once("def main():").unwrap().1;
    assert!(main.find("build_fixtures(").unwrap() < main.find("collect_report(").unwrap());
    assert!(
        runner.contains("'--locked', '--release', '--no-default-features', '--features', 'tui'")
    );
    assert!(
        !performance.contains("cargo build"),
        "runner owns the fixed unmeasured build phase"
    );
    assert!(performance.contains("name: performance-baseline"));
    assert!(performance.contains("path: performance-report/"));
    assert!(performance.contains("if-no-files-found: error"));
    for forbidden in [
        "--fail-under",
        "--threshold",
        "SONAR_TOKEN",
        "node-version: 20",
    ] {
        assert!(!performance.contains(forbidden), "{forbidden}");
    }
}

/// Sonar reuses CI's successful report without rerunning instrumented tests.
#[test]
fn sonar_consumes_same_run_coverage_with_explicit_optional_credentials() {
    let ci = workflow("ci.yml");
    let sonar = job(&ci, "sonarcloud");
    assert!(sonar.contains("needs: coverage"));
    assert!(sonar.contains("uses: ./.github/workflows/build.yml"));
    assert!(sonar.contains("SONAR_TOKEN: ${{ secrets.SONAR_TOKEN }}"));
    assert!(!sonar.contains("secrets: inherit"));
    for condition in [
        "!inputs.release-validation",
        "github.ref == 'refs/heads/main'",
        "github.event_name == 'push'",
        "github.event_name == 'pull_request'",
        "github.event_name == 'workflow_dispatch'",
    ] {
        assert!(sonar.contains(condition), "{condition}");
    }
    let scan = workflow("build.yml");
    assert!(scan.contains("  workflow_call:"));
    assert!(scan.contains("      SONAR_TOKEN:\n        required: false"));
    assert!(scan.contains("uses: actions/download-artifact@v8"));
    assert!(scan.contains("name: coverage-lcov\n          path: coverage"));
    assert!(scan.contains("test -s coverage/lcov.info"));
    assert!(scan.contains("if: ${{ env.SONAR_TOKEN_AVAILABLE != 'true' }}"));
    for forbidden in [
        "cargo llvm-cov",
        "cargo install",
        "workflow_run:",
        "run-id:",
        "github-token:",
        "  push:",
        "  pull_request:",
    ] {
        assert!(!scan.contains(forbidden), "{forbidden}");
    }
    assert_eq!(ci.matches("cargo llvm-cov ").count(), 1);
}
