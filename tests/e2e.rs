//! Process-level checks for Youta's command-line interface.

use std::fs;

use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use tempfile::tempdir;

#[test]
fn help_identifies_youtube_and_ytdlp() {
    cargo_bin_cmd!("youta")
        .env_clear()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("YouTube"))
        .stdout(predicate::str::contains("yt-dlp"));
}

#[test]
fn version_comes_from_the_package_manifest() {
    cargo_bin_cmd!("youta")
        .env_clear()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::eq(format!(
            "youta {}\n",
            env!("CARGO_PKG_VERSION")
        )));
}

#[test]
fn config_command_reports_paths_and_redacts_credentials() {
    let temporary = tempdir().expect("temporary directory");
    fs::write(
        temporary.path().join("config.toml"),
        "[providers]\nyoutube_api_key = \"must-not-leak\"\n",
    )
    .expect("configuration fixture");

    cargo_bin_cmd!("youta")
        .env_clear()
        .args(["--config-dir"])
        .arg(temporary.path())
        .arg("config")
        .assert()
        .success()
        .stdout(predicate::str::contains(format!(
            "config_dir = {}",
            temporary.path().display()
        )))
        .stdout(predicate::str::contains(
            "youtube_api_key = configured (redacted)",
        ))
        .stdout(predicate::str::contains("must-not-leak").not());
}

#[test]
fn fatal_configuration_error_prints_a_redacted_diagnostic_report() {
    let temporary = tempdir().expect("temporary directory");
    fs::write(
        temporary.path().join("config.toml"),
        "[providers]\nyoutube_api_key = 'fatal-secret-canary'\nallow_insecure_http = 'invalid'\n",
    )
    .expect("invalid configuration fixture");

    cargo_bin_cmd!("youta")
        .env_clear()
        .args(["--config-dir"])
        .arg(temporary.path())
        .arg("config")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Youta diagnostic report"))
        .stderr(predicate::str::contains("Youta version:"))
        .stderr(predicate::str::contains("Operating system:"))
        .stderr(predicate::str::contains("Cargo.lock packages"))
        .stderr(predicate::str::contains("Forced backtrace:"))
        .stderr(predicate::str::contains("fatal-secret-canary").not());
}

#[cfg(all(target_os = "linux", feature = "tui"))]
#[test]
fn tui_missing_provider_opens_setup_with_storage_location() {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    let temporary = tempdir().expect("temporary directory");
    let config_directory = temporary.path().join("configuration");
    let transcript = temporary.path().join("typescript.txt");
    let launcher = temporary.path().join("launch-youta");
    let helpers = temporary.path().join("helpers");
    let opened_links = temporary.path().join("opened-links.txt");
    fs::create_dir(&helpers).expect("helper directory");
    let xdg_open = helpers.join("xdg-open");
    fs::write(
        &xdg_open,
        "#!/bin/sh\n[ \"$1\" = '--' ] || exit 64\nprintf '%s\\n' \"$2\" >> \"$YOUTA_TEST_OPEN_LOG\"\n",
    )
    .expect("browser helper");
    fs::set_permissions(&xdg_open, fs::Permissions::from_mode(0o700))
        .expect("browser helper permissions");
    fs::write(
        &launcher,
        "#!/bin/sh\n/bin/stty cols 140 rows 42\n\
         exec \"$YOUTA_TEST_BINARY\" --config-dir \"$YOUTA_TEST_CONFIG_DIR\" tui\n",
    )
    .expect("launcher fixture");
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700))
        .expect("launcher permissions");

    let mut child = Command::new("/usr/bin/timeout")
        .args([
            "--signal=TERM",
            "--kill-after=2",
            "20",
            "/usr/bin/script",
            "--quiet",
            "--return",
            "--flush",
            "--echo",
            "never",
            "--output-limit",
            "2MiB",
            "--log-out",
        ])
        .arg(&transcript)
        .arg("--")
        .arg(&launcher)
        .env_clear()
        .env("TERM", "xterm-256color")
        .env("PATH", &helpers)
        .env("YOUTA_TEST_BINARY", assert_cmd::cargo_bin!("youta"))
        .env("YOUTA_TEST_CONFIG_DIR", &config_directory)
        .env("YOUTA_TEST_OPEN_LOG", &opened_links)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch Youta in a pseudo-terminal");
    let input = child.stdin.as_mut().expect("pseudo-terminal input");
    thread::sleep(Duration::from_millis(500));
    input.write_all(b"/ambient focus\r").expect("submit search");
    input.flush().expect("flush search");
    thread::sleep(Duration::from_millis(500));
    for function_key in [b"\x1bOP".as_slice(), b"\x1bOQ", b"\x1bOR"] {
        input
            .write_all(function_key)
            .expect("open provider setup link");
        input.flush().expect("flush provider setup link");
        thread::sleep(Duration::from_millis(150));
    }
    input.write_all(b"\x1b").expect("cancel setup");
    input.flush().expect("flush setup cancellation");
    thread::sleep(Duration::from_millis(300));
    input.write_all(b"q").expect("quit");
    input.flush().expect("flush quit");
    child.stdin.take();

    let output = child.wait_with_output().expect("wait for pseudo-terminal");
    assert!(
        output.status.success(),
        "TUI process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let terminal_output = fs::read_to_string(&transcript).expect("terminal transcript");
    let config_path = config_directory.join("config.toml").display().to_string();
    for expected in [
        "Configure YouTube search",
        "YouTube API key (masked)",
        "Invidious instance URL",
        "create/select",
        "enable YouTube Data API v3",
        "Credentials > Create",
        "credentials",
        "key;",
        "restrictions",
        "Restrict key > YouTube Data API v3 > Save",
        "Restriction",
        "blocks",
        "other",
        "APIs.",
        "Google guide",
        "developers.google.com/youtube/registering_an_application",
        "Google Cloud",
        "console.cloud.google.com/apis/credentials",
        "public",
        "official",
        "self-hosted base URL above.",
        "Instance list",
        "docs.invidious.io/instances/",
        "API keys are plaintext",
        "Unix permissions: directory 0700",
        "0600.",
        config_path.as_str(),
    ] {
        assert!(
            terminal_output.contains(expected),
            "terminal transcript omitted `{expected}`:\n{terminal_output}"
        );
    }
    assert!(
        !config_directory.join("config.toml").exists(),
        "cancelling setup must not create a configuration file"
    );
    let opened_links = fs::read_to_string(&opened_links).expect("opened provider links");
    assert_eq!(
        opened_links.lines().collect::<Vec<_>>(),
        [
            youta::tui::YOUTUBE_API_KEY_GUIDE_URL,
            youta::tui::GOOGLE_CLOUD_CREDENTIALS_URL,
            youta::tui::INVIDIOUS_INSTANCES_URL,
        ]
    );
}

#[cfg(all(target_os = "linux", feature = "tui", feature = "backend-mpv"))]
#[test]
fn tui_error_popup_runs_copy_and_both_issue_review_actions() {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    let temporary = tempdir().expect("temporary directory");
    let helpers = temporary.path().join("helpers");
    fs::create_dir(&helpers).expect("helper directory");
    let copy_log = temporary.path().join("clipboard.txt");
    let open_log = temporary.path().join("browser.txt");
    let gh_args_log = temporary.path().join("gh-args.txt");
    let gh_body_log = temporary.path().join("gh-body.txt");
    let transcript = temporary.path().join("typescript.txt");

    write_executable(
        &helpers.join("wl-copy"),
        "#!/bin/sh\n/bin/cat > \"$YOUTA_TEST_COPY_LOG\"\n",
    );
    write_executable(
        &helpers.join("xdg-open"),
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$YOUTA_TEST_OPEN_LOG\"\n",
    );
    write_executable(
        &helpers.join("gh"),
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$YOUTA_TEST_GH_ARGS_LOG\"\n\
         /bin/cat > \"$YOUTA_TEST_GH_BODY_LOG\"\n",
    );
    let launcher = temporary.path().join("launch-youta");
    write_executable(
        &launcher,
        "#!/bin/sh\n/bin/stty cols 120 rows 40\n\
         exec \"$YOUTA_TEST_BINARY\" --config-dir \"$YOUTA_TEST_CONFIG_DIR\" tui\n",
    );
    fs::write(
        temporary.path().join("config.toml"),
        format!(
            "[providers]\nmpv_executable = {:?}\nyt_dlp_executable = {:?}\n",
            temporary.path().join("missing-mpv").display().to_string(),
            temporary
                .path()
                .join("missing-yt-dlp")
                .display()
                .to_string(),
        ),
    )
    .expect("TUI configuration fixture");

    let mut child = Command::new("/usr/bin/timeout")
        .args([
            "--signal=TERM",
            "--kill-after=2",
            "30",
            "/usr/bin/script",
            "--quiet",
            "--return",
            "--flush",
            "--echo",
            "never",
            "--output-limit",
            "4MiB",
            "--log-out",
        ])
        .arg(&transcript)
        .arg("--")
        .arg(&launcher)
        .env_clear()
        .env("TERM", "xterm-256color")
        .env("PATH", &helpers)
        .env("WAYLAND_DISPLAY", "youta-test")
        .env("YOUTA_TEST_BINARY", assert_cmd::cargo_bin!("youta"))
        .env("YOUTA_TEST_CONFIG_DIR", temporary.path())
        .env("YOUTA_TEST_COPY_LOG", &copy_log)
        .env("YOUTA_TEST_OPEN_LOG", &open_log)
        .env("YOUTA_TEST_GH_ARGS_LOG", &gh_args_log)
        .env("YOUTA_TEST_GH_BODY_LOG", &gh_body_log)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch Youta in a pseudo-terminal");
    let input = child.stdin.as_mut().expect("pseudo-terminal input");
    thread::sleep(Duration::from_millis(500));
    input
        .write_all(b"/https://media.example.test/fixture.opus\r")
        .expect("submit direct media fixture");
    input.flush().expect("flush search input");
    thread::sleep(Duration::from_millis(300));
    input.write_all(b"\r").expect("activate selected video");
    input.flush().expect("flush activation");
    thread::sleep(Duration::from_secs(1));
    for action in *b"cig" {
        input.write_all(&[action]).expect("send popup action");
        input.flush().expect("flush popup action");
        thread::sleep(Duration::from_millis(250));
    }
    input.write_all(b"\x1b").expect("close diagnostic popup");
    input.flush().expect("flush popup close");
    thread::sleep(Duration::from_millis(300));
    input.write_all(b"q").expect("quit Youta");
    input.flush().expect("flush quit");
    child.stdin.take();

    let output = child.wait_with_output().expect("wait for pseudo-terminal");
    assert!(
        output.status.success(),
        "TUI process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let terminal_output = fs::read_to_string(&transcript).expect("terminal transcript");
    assert!(
        terminal_output.contains("start playback"),
        "{terminal_output}"
    );
    // Ratatui writes only changed cells, so cursor-position escapes can separate a
    // button's words in a transcript. Renderer tests cover the complete labels;
    // this terminal test covers each hotkey and the actions' external effects.
    for expected in ["[c]", "[i]", "[g]"] {
        assert!(
            terminal_output.contains(expected),
            "terminal transcript omitted `{expected}`:\n{terminal_output}"
        );
    }

    let copied = fs::read_to_string(&copy_log).expect("copied report");
    assert!(copied.contains("Youta diagnostic report"));
    assert!(copied.contains("Forced backtrace:"));
    let opened = fs::read_to_string(&open_log).expect("opened issue URL");
    assert!(opened.contains("github.com/vitaly-zdanevich/youta/issues/new"));
    assert!(
        opened.len() < 1_000,
        "browser issue URL must remain bounded"
    );
    assert!(!opened.contains("Forced%20backtrace"));
    let gh_arguments = fs::read_to_string(&gh_args_log).expect("GitHub CLI arguments");
    for expected in [
        "issue",
        "create",
        "--web",
        "--repo",
        "vitaly-zdanevich/youta",
        "--body-file",
        "-",
    ] {
        assert!(gh_arguments.lines().any(|argument| argument == expected));
    }
    let gh_body = fs::read_to_string(&gh_body_log).expect("GitHub CLI report body");
    assert_eq!(gh_body, copied);

    fn write_executable(path: &std::path::Path, contents: &str) {
        fs::write(path, contents).expect("helper fixture");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("helper permissions");
    }
}

#[cfg(unix)]
#[test]
fn doctor_uses_configured_helpers_only_when_features_need_them() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = tempdir().expect("temporary directory");
    let helper = temporary.path().join("mock-helper");
    fs::write(&helper, "#!/bin/sh\nprintf 'mock-helper 1.0\\n'\n").expect("helper fixture");
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).expect("helper permissions");
    fs::write(
        temporary.path().join("config.toml"),
        format!(
            "[providers]\nmpv_executable = {:?}\nyt_dlp_executable = {:?}\n",
            helper.display().to_string(),
            helper.display().to_string()
        ),
    )
    .expect("configuration fixture");

    let output = cargo_bin_cmd!("youta")
        .env_clear()
        .args(["--config-dir"])
        .arg(temporary.path())
        .arg("doctor")
        .output()
        .expect("doctor output");
    assert!(
        output.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    if cfg!(feature = "backend-mpv") {
        assert!(stdout.contains("mpv: mock-helper 1.0"));
    } else {
        assert!(stdout.contains("mpv: skipped (backend omitted at build time)"));
    }
    if cfg!(feature = "yt-dlp") {
        assert!(stdout.contains("yt-dlp: mock-helper 1.0"));
    } else {
        assert!(stdout.contains("yt-dlp: skipped (feature omitted at build time)"));
    }
}
