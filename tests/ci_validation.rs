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

/// Diagnostic artifacts would corrupt Release's strict deliverable inventory.
#[test]
fn shared_ci_does_not_upload_diagnostics_into_release_artifacts() {
    let ci = workflow("ci.yml");
    let uploads: Vec<_> = ci
        .split("      - name:")
        .filter(|step| step.contains("uses: actions/upload-artifact@"))
        .collect();
    assert_eq!(uploads.len(), 3);
    for upload in uploads {
        assert!(upload.contains("if: ${{ !inputs.release-validation }}"));
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
