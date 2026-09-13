//! Process-level startup URL parsing without external requests or terminal UI.
#![cfg(feature = "cli")]

use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use std::time::Duration;

/// A URL can accompany global options without bypassing the license preflight.
#[test]
fn startup_url_license_keeps_configuration_and_network_unopened() {
    let directory = tempfile::tempdir().expect("configuration fixture");
    std::fs::write(directory.path().join("config.toml"), "not valid TOML")
        .expect("invalid configuration");
    for url in ["https://example.com/", "http://127.0.0.1:8000/music/"] {
        cargo_bin_cmd!("youta")
            .timeout(Duration::from_secs(10))
            .arg("--config-dir")
            .arg(directory.path())
            .args([url, "--license"])
            .assert()
            .success()
            .stdout(predicate::eq(youta::LICENSE_TEXT))
            .stderr(predicate::str::is_empty());
    }
}

/// Invalid schemes, credentials, and extra positional data fail before startup.
#[test]
fn startup_url_invalid_and_ambiguous_arguments_are_rejected() {
    for arguments in [
        vec!["https://example.com/", "tui", "--license"],
        vec!["tui", "https://example.com/", "--license"],
        vec![
            "https://example.com/",
            "https://second.example/",
            "--license",
        ],
        vec!["file:///etc/passwd", "--license"],
        vec!["https://user:secret@example.com/", "--license"],
    ] {
        cargo_bin_cmd!("youta")
            .timeout(Duration::from_secs(10))
            .args(arguments)
            .assert()
            .failure()
            .stderr(predicate::str::contains("error:"));
    }
}

/// Global options still work before an existing subcommand.
#[test]
fn startup_url_preserves_config_subcommand_with_leading_global_option() {
    let directory = tempfile::tempdir().expect("configuration fixture");
    cargo_bin_cmd!("youta")
        .timeout(Duration::from_secs(10))
        .arg("--config-dir")
        .arg(directory.path())
        .arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains("config_dir ="));
}

/// Unsupported builds fail explicitly, before malformed configuration or any UI.
#[cfg(not(all(feature = "tui", feature = "web-browser")))]
#[test]
fn startup_url_disabled_capability_reports_the_missing_feature() {
    let directory = tempfile::tempdir().expect("configuration fixture");
    std::fs::write(directory.path().join("config.toml"), "not valid TOML")
        .expect("invalid configuration");
    cargo_bin_cmd!("youta")
        .timeout(Duration::from_secs(10))
        .arg("--config-dir")
        .arg(directory.path())
        .arg("http://127.0.0.1:8000/")
        .assert()
        .failure()
        .stderr(predicate::str::contains(if cfg!(feature = "web-browser") {
            "requires the `tui` feature"
        } else {
            "requires the `web-browser` feature"
        }))
        .stderr(predicate::str::contains("cannot load Youta configuration").not());
}
