//! Process-level checks for Youta's command-line interface.

use std::fs;

use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use tempfile::tempdir;

/// Maximum time a hosted runner may take to expose one expected TUI effect.
#[cfg(all(
    target_os = "linux",
    feature = "tui",
    any(
        feature = "youtube-official",
        feature = "invidious",
        feature = "backend-mpv"
    )
))]
const TUI_READINESS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Poll interval for transcript and helper-output readiness checks.
#[cfg(all(
    target_os = "linux",
    feature = "tui",
    any(
        feature = "youtube-official",
        feature = "invidious",
        feature = "backend-mpv"
    )
))]
const TUI_READINESS_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Waits for a flushed pseudo-terminal transcript to contain `expected`.
///
/// The bounded poll replaces startup and rendering timing guesses. It tolerates
/// the transcript not existing yet and reports a bounded tail on timeout.
#[cfg(all(
    target_os = "linux",
    feature = "tui",
    any(
        feature = "youtube-official",
        feature = "invidious",
        feature = "backend-mpv"
    )
))]
fn wait_for_transcript_text(path: &std::path::Path, expected: &str) {
    wait_for_text_file(path, expected, "terminal transcript");
}

/// Waits for an asynchronous test helper's output file to contain `expected`.
///
/// This readiness boundary proves that an external action ran before the test
/// sends another hotkey. The wait is capped by [`TUI_READINESS_TIMEOUT`].
#[cfg(all(
    target_os = "linux",
    feature = "tui",
    any(
        feature = "youtube-official",
        feature = "invidious",
        feature = "backend-mpv"
    )
))]
fn wait_for_helper_output(path: &std::path::Path, expected: &str) {
    wait_for_text_file(path, expected, "helper output");
}

/// Polls one UTF-8-ish text file until it contains a readiness marker.
#[cfg(all(
    target_os = "linux",
    feature = "tui",
    any(
        feature = "youtube-official",
        feature = "invidious",
        feature = "backend-mpv"
    )
))]
fn wait_for_text_file(path: &std::path::Path, expected: &str, label: &str) {
    use std::io::ErrorKind;
    use std::time::Instant;

    let deadline = Instant::now()
        .checked_add(TUI_READINESS_TIMEOUT)
        .expect("readiness deadline");
    let mut observed = String::new();
    loop {
        match fs::read(path) {
            Ok(bytes) => observed = String::from_utf8_lossy(&bytes).into_owned(),
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => panic!("cannot read {label} {}: {error}", path.display()),
        }
        if observed.contains(expected) {
            return;
        }
        if Instant::now() >= deadline {
            let tail = observed
                .chars()
                .rev()
                .take(4_096)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            panic!(
                "timed out waiting for `{expected}` in {label} {};\nlast output tail:\n{tail}",
                path.display()
            );
        }
        std::thread::sleep(TUI_READINESS_POLL);
    }
}

/// Runs one deterministic key sequence against Youta in a pseudo-terminal.
#[cfg(all(target_os = "linux", feature = "tui"))]
fn run_tui_session(
    launcher: &std::path::Path,
    binary: &std::path::Path,
    config_directory: &std::path::Path,
    helpers: &std::path::Path,
    transcript: &std::path::Path,
    opened_links: &std::path::Path,
    subscriptions_layout_override: Option<&str>,
    inputs: &[(&[u8], u64)],
) -> std::process::Output {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    let mut command = Command::new("/usr/bin/timeout");
    command
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
        .arg(transcript)
        .arg("--command")
        .arg(launcher)
        .env_clear()
        .env("TERM", "xterm-256color")
        .env("PATH", helpers)
        .env("YOUTA_TEST_BINARY", binary)
        .env("YOUTA_TEST_CONFIG_DIR", config_directory)
        .env("YOUTA_TEST_OPEN_LOG", opened_links)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(layout) = subscriptions_layout_override {
        command.env("YOUTA_UI__SUBSCRIPTIONS_LAYOUT", layout);
    }
    let mut child = command.spawn().expect("launch Youta in a pseudo-terminal");
    {
        let input = child.stdin.as_mut().expect("pseudo-terminal input");
        thread::sleep(Duration::from_millis(500));
        for (bytes, delay_millis) in inputs {
            input.write_all(bytes).expect("write pseudo-terminal input");
            input.flush().expect("flush pseudo-terminal input");
            thread::sleep(Duration::from_millis(*delay_millis));
        }
    }
    child.stdin.take();
    child.wait_with_output().expect("wait for pseudo-terminal")
}

/// A Youta command whose environment the test sets rather than the shell.
///
/// Clearing it is what keeps a stray `YOUTA_`-prefixed variable out of the
/// child, since an environment override always wins over the configuration
/// file and would quietly change what these assertions describe. Windows needs
/// three names put back: the account Youta restricts its private directories to
/// is read from `USERNAME` and `USERDOMAIN`, and the ACL editor is resolved
/// under `SystemRoot`, so a child without them cannot create its application
/// directory at all and fails long before reaching the behaviour under test.
/// Unix restricts by mode bits and needs nothing returned.
fn youta_command() -> assert_cmd::Command {
    let mut command = cargo_bin_cmd!("youta");
    command.env_clear();
    #[cfg(windows)]
    for name in ["USERNAME", "USERDOMAIN", "SystemRoot"] {
        if let Ok(value) = std::env::var(name) {
            command.env(name, value);
        }
    }
    command
}

#[test]
fn help_identifies_youtube_and_ytdlp() {
    youta_command()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("YouTube"))
        .stdout(predicate::str::contains("yt-dlp"));
}

#[test]
fn version_comes_from_the_package_manifest() {
    youta_command()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::eq(format!(
            "youta {}\n",
            env!("CARGO_PKG_VERSION")
        )));
}

#[test]
fn license_notice_is_embedded_in_the_executable() {
    let temporary = tempdir().expect("temporary directory");
    fs::write(
        temporary.path().join("config.toml"),
        "[providers]\nallow_insecure_http = 'not-a-boolean'\n",
    )
    .expect("invalid configuration fixture");

    youta_command()
        .args(["--config-dir"])
        .arg(temporary.path())
        .arg("--license")
        .assert()
        .success()
        .stdout(predicate::eq(include_str!("../LICENSE")))
        .stderr(predicate::str::is_empty());
}

#[test]
fn config_command_reports_paths_and_redacts_credentials() {
    let temporary = tempdir().expect("temporary directory");
    fs::write(
        temporary.path().join("config.toml"),
        "[providers]\nyoutube_api_key = \"must-not-leak-credential\"\n",
    )
    .expect("configuration fixture");

    youta_command()
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
        .stdout(predicate::str::contains("cava_executable = cava"))
        .stdout(predicate::str::contains("must-not-leak-credential").not());
}

#[test]
fn fatal_configuration_error_prints_a_redacted_diagnostic_report() {
    let temporary = tempdir().expect("temporary directory");
    fs::write(
        temporary.path().join("config.toml"),
        "[providers]\nyoutube_api_key = 'fatal-secret-canary'\nallow_insecure_http = 'invalid'\n",
    )
    .expect("invalid configuration fixture");

    youta_command()
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

#[cfg(feature = "tui")]
#[test]
fn second_tui_instance_exits_before_terminal_initialization_with_one_plain_line() {
    use youta::config::Config;
    use youta::persistence::StateStore;

    let temporary = tempdir().expect("temporary directory");
    let config = Config::for_dir(temporary.path());
    let _running_instance = StateStore::open(&config).expect("first Youta instance state lock");

    let output = youta_command()
        .args(["--config-dir"])
        .arg(temporary.path())
        .arg("tui")
        .output()
        .expect("second Youta instance output");

    assert_eq!(output.status.code(), Some(1));
    assert!(
        output.stdout.is_empty(),
        "terminal UI wrote to stdout before rejecting the second instance: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        output.stderr,
        b"Another instance of Youta is already running\n"
    );
}

#[cfg(all(
    target_os = "linux",
    feature = "tui",
    any(feature = "youtube-official", feature = "invidious")
))]
#[test]
fn tui_missing_provider_opens_setup_with_storage_location() {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::{Command, Stdio};
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
        "#!/bin/sh\n[ \"$#\" -eq 1 ] || exit 64\nprintf '%s\\n' \"$1\" >> \"$YOUTA_TEST_OPEN_LOG\"\n",
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
        // Ubuntu 24.04's util-linux 2.39 supports `--command`, but not the
        // newer `-- <command> [arguments...]` invocation form.
        .arg("--command")
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
    wait_for_transcript_text(&transcript, "Video search");
    let input = child.stdin.as_mut().expect("pseudo-terminal input");
    input.write_all(b"/ambient focus\r").expect("submit search");
    input.flush().expect("flush search");
    wait_for_transcript_text(&transcript, "Configure YouTube metadata");
    for (function_key, expected_url) in [
        (b"\x1bOP".as_slice(), youta::tui::YOUTUBE_API_KEY_GUIDE_URL),
        (
            b"\x1bOQ".as_slice(),
            youta::tui::GOOGLE_CLOUD_CREDENTIALS_URL,
        ),
        (b"\x1bOR".as_slice(), youta::tui::INVIDIOUS_INSTANCES_URL),
    ] {
        input
            .write_all(function_key)
            .expect("open provider setup link");
        input.flush().expect("flush provider setup link");
        wait_for_helper_output(&opened_links, expected_url);
    }
    input.write_all(b"\x1b").expect("cancel setup");
    input.flush().expect("flush setup cancellation");
    // Keep a short standalone-Escape gap so the terminal decoder cannot
    // interpret the following quit key as an Alt chord.
    std::thread::sleep(Duration::from_millis(100));
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
        "Configure YouTube metadata",
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
        "Unix permissions: directories 0700",
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

#[cfg(all(target_os = "linux", feature = "tui"))]
#[test]
fn tui_subscriptions_openers_and_preferences_persist_end_to_end() {
    use std::os::unix::fs::PermissionsExt as _;

    let temporary = tempdir().expect("temporary directory");
    let config_directory = temporary.path().join("configuration");
    let helpers = temporary.path().join("helpers");
    let launcher = temporary.path().join("launch-youta");
    let opened_links = temporary.path().join("opened-links.txt");
    let first_transcript = temporary.path().join("first-typescript.txt");
    let locked_transcript = temporary.path().join("locked-typescript.txt");
    fs::create_dir_all(&config_directory).expect("configuration directory");
    fs::create_dir(&helpers).expect("helper directory");
    fs::write(
        config_directory.join("subscriptions.opml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<opml version="2.0">
  <head><title>Youta subscriptions</title></head>
  <body>
    <outline text="Fixture channel" title="Fixture channel" type="rss"
      xmlUrl="https://www.youtube.com/feeds/videos.xml?channel_id=UCfixture"
      htmlUrl="https://www.youtube.com/channel/UCfixture"
      description="Fixture subscription"/>
  </body>
</opml>
"#,
    )
    .expect("subscription fixture");
    let xdg_open = helpers.join("xdg-open");
    fs::write(
        &xdg_open,
        "#!/bin/sh\n[ \"$#\" -eq 1 ] || exit 64\n\
         printf 'start %s\\n' \"$1\" >> \"$YOUTA_TEST_OPEN_LOG\"\n\
         /bin/sleep 1\n\
         printf 'done %s\\n' \"$1\" >> \"$YOUTA_TEST_OPEN_LOG\"\n",
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
    let binary = assert_cmd::cargo_bin!("youta");

    let output = run_tui_session(
        &launcher,
        binary,
        &config_directory,
        &helpers,
        &first_transcript,
        &opened_links,
        None,
        &[
            (b"S", 300),
            // Both opener hotkeys must be accepted while the first helper is
            // still running; otherwise the ordered log below exposes a blocked
            // terminal event loop.
            (b"o", 100),
            (b"O", 1_200),
            (b"p", 300),
            (b"s", 200),
            (b"y", 200),
            (b"\r", 400),
            (b"q", 200),
        ],
    );
    assert!(
        output.status.success(),
        "first TUI process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first_output = fs::read_to_string(&first_transcript).expect("first transcript");
    // The pseudo-terminal transcript records ratatui's incremental updates,
    // so unchanged cells inside a title can be omitted from its byte stream.
    // Assert a preference unique to this popup instead of requiring its title
    // to appear as one contiguous substring.
    for expected in [
        "Sources",
        "Fixture channel",
        "Save playback history: on",
        "Split",
        "Prepare selected YouTube audio: off",
    ] {
        assert!(
            first_output.contains(expected),
            "first transcript omitted `{expected}`:\n{first_output}"
        );
    }
    assert_eq!(
        fs::read_to_string(&opened_links)
            .expect("opened channel link")
            .lines()
            .collect::<Vec<_>>(),
        [
            "start https://www.youtube.com/channel/UCfixture",
            "start https://www.youtube.com/channel/UCfixture",
            "done https://www.youtube.com/channel/UCfixture",
            "done https://www.youtube.com/channel/UCfixture",
        ]
    );
    let config_path = config_directory.join("config.toml");
    let saved_config = fs::read_to_string(&config_path).expect("saved preferences");
    assert!(saved_config.contains("[ui]"));
    assert!(saved_config.contains("subscriptions_layout = \"split\""));
    assert!(saved_config.contains("[playback]"));
    assert!(saved_config.contains("youtube_prewarm = false"));

    let output = run_tui_session(
        &launcher,
        binary,
        &config_directory,
        &helpers,
        &locked_transcript,
        &opened_links,
        Some("drill-down"),
        &[
            (b"p", 300),
            (b"s", 200),
            (b"\r", 300),
            (b"\x1b", 200),
            (b"q", 200),
        ],
    );
    assert!(
        output.status.success(),
        "locked TUI process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let locked_output = fs::read_to_string(&locked_transcript).expect("locked transcript");
    assert!(locked_output.contains("YOUTA_UI__SUBSCRIPTIONS_LAYOUT"));
    assert!(
        locked_output.contains("change or remove")
            || locked_output.contains("controls this preference")
    );
    assert_eq!(
        fs::read_to_string(config_path).expect("unchanged preferences"),
        saved_config,
        "an environment-locked preference must not rewrite TOML"
    );
}

#[cfg(all(target_os = "linux", feature = "tui", feature = "backend-mpv"))]
#[test]
fn tui_error_popup_runs_copy_browser_review_and_confirmed_submission_actions() {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::{Command, Stdio};
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
        "#!/bin/sh\n[ \"$#\" -eq 1 ] || exit 64\nprintf '%s\\n' \"$1\" > \"$YOUTA_TEST_OPEN_LOG\"\n",
    );
    write_executable(
        &helpers.join("gh"),
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$YOUTA_TEST_GH_ARGS_LOG\"\n\
         /bin/cat > \"$YOUTA_TEST_GH_BODY_LOG\"\n\
         printf '%s\\n' 'https://github.com/vitaly-zdanevich/youta/issues/123'\n",
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
        // Ubuntu 24.04's util-linux 2.39 supports `--command`, but not the
        // newer `-- <command> [arguments...]` invocation form.
        .arg("--command")
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
    wait_for_transcript_text(&transcript, "Video search");
    let input = child.stdin.as_mut().expect("pseudo-terminal input");
    input
        .write_all(b"/https://media.example.test/fixture.opus\r")
        .expect("submit direct media fixture");
    input.flush().expect("flush search input");
    wait_for_transcript_text(
        &transcript,
        "Direct link recognized for media.example.test.",
    );
    input.write_all(b"\r").expect("activate selected video");
    input.flush().expect("flush activation");
    wait_for_transcript_text(&transcript, "Youta diagnostic report");
    wait_for_transcript_text(&transcript, "[g]");
    for (action, output_path, expected) in [
        (b'c', &copy_log, "- No bounded input reached its limit."),
        (
            b'i',
            &open_log,
            "github.com/vitaly-zdanevich/youta/issues/new",
        ),
    ] {
        input.write_all(&[action]).expect("send popup action");
        input.flush().expect("flush popup action");
        wait_for_helper_output(output_path, expected);
    }
    input
        .write_all(b"g")
        .expect("request GitHub issue submission");
    input.flush().expect("flush submission request");
    wait_for_transcript_text(&transcript, "This creates a public issue");
    assert!(
        !gh_body_log.exists(),
        "requesting submission must not invoke gh before confirmation"
    );
    input
        .write_all(b"\r")
        .expect("confirm GitHub issue submission");
    input.flush().expect("flush submission confirmation");
    wait_for_helper_output(&gh_body_log, "- No bounded input reached its limit.");
    wait_for_transcript_text(
        &transcript,
        "https://github.com/vitaly-zdanevich/youta/issues/123",
    );
    input.write_all(b"\x1b").expect("close diagnostic popup");
    input.flush().expect("flush popup close");
    // Keep a short standalone-Escape gap so the terminal decoder cannot
    // interpret the following quit key as an Alt chord.
    std::thread::sleep(Duration::from_millis(100));
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
    assert_eq!(opened.lines().count(), 1, "xdg-open must receive one URL");
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
        "--repo",
        "vitaly-zdanevich/youta",
        "--body-file",
        "-",
    ] {
        assert!(gh_arguments.lines().any(|argument| argument == expected));
    }
    assert!(!gh_arguments.lines().any(|argument| argument == "--web"));
    let gh_body = fs::read_to_string(&gh_body_log).expect("GitHub CLI report body");
    assert_eq!(gh_body, copied);

    fn write_executable(path: &std::path::Path, contents: &str) {
        fs::write(path, contents).expect("helper fixture");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("helper permissions");
    }
}

#[cfg(all(target_os = "linux", feature = "tui"))]
#[test]
fn tui_history_enter_reports_a_removed_local_file_without_deleting_history() {
    use std::os::unix::fs::PermissionsExt as _;

    use youta::config::Config;
    use youta::domain::{HistoryEntry, MediaId, Screen, SessionState, SourceKind};
    use youta::persistence::StateStore;

    let temporary = tempdir().expect("temporary directory");
    let config_directory = temporary.path().join("configuration");
    let helpers = temporary.path().join("helpers");
    let transcript = temporary.path().join("typescript.txt");
    let opened_links = temporary.path().join("opened-links.txt");
    let launcher = temporary.path().join("launch-youta");
    let removed = temporary.path().join("removed-history.opus");
    fs::create_dir(&helpers).expect("helper directory");
    for helper in ["mpv", "yt-dlp"] {
        let path = helpers.join(helper);
        fs::write(&path, "#!/bin/sh\nprintf 'mock-helper 1.0\\n'\n").expect("diagnostic helper");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("diagnostic helper permissions");
    }
    fs::write(
        &launcher,
        "#!/bin/sh\n/bin/stty cols 120 rows 40\n\
         exec \"$YOUTA_TEST_BINARY\" --config-dir \"$YOUTA_TEST_CONFIG_DIR\" tui\n",
    )
    .expect("launcher fixture");
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700))
        .expect("launcher permissions");

    let config = Config::for_dir(&config_directory);
    let store = StateStore::open(&config).expect("state store");
    store
        .insert_history(&HistoryEntry {
            id: 0,
            media_id: MediaId::new(SourceKind::Local, removed.display().to_string()),
            title: "Removed History fixture".to_owned(),
            replay_locator: Some(removed.display().to_string()),
            started_at: 1,
            last_played_at: 2,
            position_seconds: 0,
            duration_seconds: None,
            finished: false,
        })
        .expect("history fixture");
    store
        .save_session(
            &SessionState {
                screen: Screen::History,
                ..SessionState::default()
            },
            2,
        )
        .expect("History session");
    drop(store);

    let output = run_tui_session(
        &launcher,
        assert_cmd::cargo::cargo_bin!("youta"),
        &config_directory,
        &helpers,
        &transcript,
        &opened_links,
        None,
        &[(b"\r", 700), (b"\x1b", 200), (b"q", 200)],
    );

    assert!(
        output.status.success(),
        "TUI process failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let terminal_output = fs::read_to_string(&transcript).expect("terminal transcript");
    assert!(terminal_output.contains("Removed History fixture"));
    assert!(terminal_output.contains("Removed"));
    assert!(terminal_output.contains("History item is unavailable"));

    let store = StateStore::open(&config).expect("reopen state store");
    assert_eq!(
        store.history(false, 10).expect("retained History").len(),
        1,
        "failed replay must retain the History record"
    );
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
            "[providers]\nmpv_executable = {:?}\nyt_dlp_executable = {:?}\ncava_executable = {:?}\n",
            helper.display().to_string(),
            helper.display().to_string(),
            helper.display().to_string()
        ),
    )
    .expect("configuration fixture");

    let output = youta_command()
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
    if cfg!(feature = "ascii-visualizer") {
        assert!(stdout.contains("cava: mock-helper 1.0"));
    } else {
        assert!(stdout.contains("cava: skipped (ASCII visualizer feature omitted at build time)"));
    }
}
