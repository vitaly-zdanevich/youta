//! Real-PTY coverage for Local commands, job control, and terminal restoration.

use super::*;
use crate::local_command::LocalCommandPlan;
use std::io::Write;
use std::process::Command;

const CHILD_DIRECTORY: &str = "YOUTA_LOCAL_COMMAND_TEST_DIRECTORY";

/// Hands an isolated controlling terminal to the actual foreground command runner.
#[test]
fn local_commands_preserve_output_and_restore_terminal_input() {
    if let Some(directory) = std::env::var_os(CHILD_DIRECTORY) {
        run_child(PathBuf::from(directory));
        return;
    }

    let fixture = crate::test_support::canonical_tempdir("Local command PTY fixture");
    let output = Command::new("python3")
        .args(["-c", include_str!("../../tests/fixtures/local_commands.py")])
        .arg(std::env::current_exe().unwrap())
        .arg(fixture.path())
        .output()
        .expect("Python 3 Local command PTY driver");
    assert!(
        output.status.success(),
        "Local command PTY driver failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !fixture.path().join("COMMAND_INJECTION").exists(),
        "a filename must never execute shell syntax"
    );
}

/// Flushes a test marker without waiting for the next TUI render.
fn marker(text: &str) {
    print!("\r\n{text}\r\n");
    io::stdout().flush().unwrap();
}

/// Verifies that the dismissal key is consumed and subsequent keys reach Youta.
fn expect_ui_key(input: &mut TerminalInput) {
    let started = std::time::Instant::now();
    loop {
        let remaining = Duration::from_secs(5).saturating_sub(started.elapsed());
        assert!(!remaining.is_zero(), "missing key after Local command");
        assert_eq!(input.poll(remaining).unwrap(), WaitOutcome::TerminalEvent);
        let actual = input.read().unwrap();
        match actual {
            Event::Resize(..) => {}
            Event::Key(key)
                if key.code == KeyCode::Char('q')
                    && key.modifiers == KeyModifiers::NONE
                    && key.kind == KeyEventKind::Press =>
            {
                return;
            }
            other => panic!("dismissal leaked into Youta input: {other:?}"),
        }
    }
}

/// Exercises output, validation/spawn errors, interrupted exec, and interactive stdin.
fn run_child(directory: PathBuf) {
    let path = directory.join("track' $(touch COMMAND_INJECTION) ;\n song.flac");
    std::fs::write(&path, b"command fixture").unwrap();

    let mut session = TerminalSession::enter().unwrap();
    let mut input = TerminalInput::new().unwrap();
    let cases = [
        (
            "normal",
            "expected=\"$1\"; set -- %; \
             [ \"$#\" -eq 1 ] && [ \"$1\" = \"$expected\" ] && [ -f \"$1\" ] || exit 31; \
             [ \"$PWD\" = \"$YOUTA_LOCAL_COMMAND_TEST_DIRECTORY\" ] || exit 32; \
             printf 'PATH_ONE_ARG_OK\\n'; \
             printf '%s\\n' COMMAND_STDOUT; printf '%s\\n' COMMAND_STDERR >&2",
            true,
        ),
        ("nonzero", "printf 'COMMAND_NONZERO\\n'; exit 17", false),
        ("validation", "echo $(echo %)", false),
        ("missing_directory", "printf 'UNEXPECTED_SPAWN\\n'", false),
        (
            "interrupt",
            "printf 'COMMAND_INTERRUPT_READY\\n'; exec sleep 100",
            false,
        ),
        (
            "read",
            "printf 'COMMAND_INPUT_READY\\n'; IFS= read -r reply; \
             [ \"$reply\" = 'command owns input' ] || exit 33; \
             printf 'COMMAND_INPUT_OK\\n'",
            true,
        ),
    ];

    for (name, template, succeeds) in cases {
        // Arm a pending read before handoff, so a lingering reader would steal
        // the command's input or the key that dismisses its retained output.
        while input.poll(Duration::ZERO).unwrap() == WaitOutcome::TerminalEvent {
            let _ = input.read().unwrap();
        }
        marker(&format!("COMMAND_BEGIN_{name}"));
        let result = execute_local_command_plan(
            &mut session,
            &mut input,
            LocalCommandPlan {
                template: template.to_owned(),
                path: path.clone(),
                directory: if name == "missing_directory" {
                    directory.join("missing-command-working-directory")
                } else {
                    directory.clone()
                },
            },
        );
        assert_eq!(result.is_ok(), succeeds, "{name}: {result:?}");
        marker(&format!("COMMAND_RETURNED_{name}"));
        expect_ui_key(&mut input);
        marker(&format!("COMMAND_DONE_{name}"));
    }

    assert!(!directory.join("COMMAND_INJECTION").exists());
    drop(input);
    drop(session);
    marker("LOCAL_COMMAND_TEST_DONE");
}
