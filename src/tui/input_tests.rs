//! Real-PTY coverage for event notifications and exclusive foreground input.

use super::*;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::process::Command;

const CHILD_SOCKET: &str = "YOUTA_INPUT_TEST_SOCKET";

/// Runs the real portable input adapter in an isolated controlling terminal.
///
/// The Python driver supplies a PTY without adding unsafe code or a production
/// dependency. The child reaches the same test through an explicit environment
/// marker, leaving the parent test process's global Crossterm reader untouched.
#[test]
fn terminal_completion_events_and_editor_handoff() {
    if let Some(socket) = std::env::var_os(CHILD_SOCKET) {
        run_child(PathBuf::from(socket));
        return;
    }
    let fixture = crate::test_support::canonical_tempdir("terminal event fixture");
    let output = Command::new("python3")
        .args([
            "-c",
            include_str!("../../tests/fixtures/terminal_events.py"),
        ])
        .arg(std::env::current_exe().unwrap())
        .arg(fixture.path().join("worker.sock"))
        .output()
        .expect("Python 3 PTY test driver");
    assert!(
        output.status.success(),
        "PTY driver failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Signals the test driver without waiting for a render or animation deadline.
fn marker(text: &str) {
    print!("\r\n{text}\r\n");
    io::stdout().flush().unwrap();
}

/// Waits for one semantic event while allowing a queued terminal resize.
fn expect_input(input: &mut TerminalInput, expected: &Event) {
    let started = std::time::Instant::now();
    loop {
        let remaining = Duration::from_secs(5).saturating_sub(started.elapsed());
        assert!(!remaining.is_zero(), "missing input: {expected:?}");
        assert_eq!(input.poll(remaining).unwrap(), WaitOutcome::TerminalEvent);
        let actual = input.read().unwrap();
        if &actual == expected {
            return;
        }
        assert!(
            matches!(actual, Event::Resize(..)),
            "expected {expected:?}, got {actual:?}"
        );
    }
}

/// Exercises the real OS wait, input decoding, and the production editor handoff.
fn run_child(socket: PathBuf) {
    let mut session = TerminalSession::enter().unwrap();
    let mut input = TerminalInput::new().unwrap();
    let listener = UnixListener::bind(socket).unwrap();
    let wake = input.worker_waker();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut signal = [0];
        stream.read_exact(&mut signal).unwrap();
        assert_eq!(signal, [b'w']);
        wake.wake();
    });
    marker("WORKER_WAIT_READY");
    loop {
        match input.poll(Duration::from_secs(10)).unwrap() {
            WaitOutcome::WorkerReady => break,
            WaitOutcome::TerminalEvent => {
                assert!(matches!(input.read().unwrap(), Event::Resize(..)));
            }
            other => panic!("worker failed to interrupt the idle wait: {other:?}"),
        }
    }
    worker.join().unwrap();
    marker("INPUT_READY");
    expect_input(
        &mut input,
        &Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
    );
    expect_input(
        &mut input,
        &Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 5,
            modifiers: KeyModifiers::NONE,
        }),
    );
    marker("RESIZE_READY");
    expect_input(&mut input, &Event::Resize(100, 30));
    for cycle in 0..2 {
        // Arm a library read immediately before surrendering this same terminal.
        while input.poll(Duration::ZERO).unwrap() == WaitOutcome::TerminalEvent {
            let _ = input.read().unwrap();
        }
        let editor = format!(
            "import sys; print('EDITOR_READY_{cycle}', flush=True); text=sys.stdin.readline(); assert text == 'editor-owns-keys-{cycle}\\n', repr(text)"
        );
        let plan = TextFileOpenPlan {
            executable: PathBuf::from("python3"),
            arguments: vec!["-c".into(), editor.into()],
            source: crate::text_file_open::TextFileOpenerSource::VisualEnvironment,
            lifecycle: TextFileOpenLifecycle::SuspendTuiAndWait,
        };
        assert_eq!(
            execute_text_file_open_plan(&mut session, &mut input, plan).unwrap(),
            TextFileOpenLifecycle::SuspendTuiAndWait
        );
        marker(&format!("EDITOR_RETURNED_{cycle}"));
        expect_input(
            &mut input,
            &Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        );
    }
    // Pending input must not delay shutdown or outlive raw-mode restoration.
    let _ = input.poll(Duration::ZERO).unwrap();
    drop(input);
    drop(session);
    marker("INPUT_TEST_DONE");
}
