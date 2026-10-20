//! Command history follows the activity-persistence preference without deleting old data.

use super::tests::{fixture, type_text};
use super::*;

/// Uses literal commands without requesting local-directory refreshes after completion.
fn command_fixture() -> (AppController, tempfile::TempDir) {
    let (mut controller, directory, _, _) = fixture();
    controller.view.screen = Screen::Search;
    (controller, directory)
}

/// Submits through the real editor, checks the queued command, and completes it in memory.
fn submit(controller: &mut AppController, command: &str) {
    controller.dispatch(UiAction::BeginLocalCommand);
    type_text(controller, command);
    finish_submission(controller, command);
}

/// Confirms that privacy never disables the normal command execution path.
fn finish_submission(controller: &mut AppController, expected: &str) {
    controller.dispatch(UiAction::SubmitLocalCommand);
    assert_eq!(
        controller.take_custom_command_plan().unwrap().template,
        expected
    );
    controller.report_custom_command_result(Ok(crate::local_command::CommandOutput {
        output: String::new(),
        success: true,
    }));
}

/// Disabled logging preserves old bytes while retaining private history navigation in RAM.
#[test]
fn disabled_history_preserves_command_log_and_keeps_ram_navigation() {
    let (mut controller, _directory) = command_fixture();
    let path = history_path(&controller.config);
    write_history(&path, &["echo saved".to_owned()]).unwrap();
    fs::write(&path, b"echo saved\n\n").unwrap();
    let before = fs::read(&path).unwrap();
    controller.config.persistence.save_playback_history = false;
    submit(&mut controller, "echo private");
    assert_eq!(fs::read(&path).unwrap(), before);
    controller.dispatch(UiAction::BeginLocalCommand);
    type_text(&mut controller, "draft");
    controller.dispatch(UiAction::BrowseLocalCommandHistory(-1));
    assert_eq!(
        controller.view.local_command.as_ref().unwrap().command,
        "echo private"
    );
    controller.dispatch(UiAction::BrowseLocalCommandHistory(1));
    assert_eq!(
        controller.view.local_command.as_ref().unwrap().command,
        "draft"
    );
    assert_eq!(fs::read(&path).unwrap(), before);
}

/// A private command must not create a log or a command-log temporary file.
#[test]
fn disabled_history_does_not_create_command_log() {
    let (mut controller, _directory) = command_fixture();
    controller.config.persistence.save_playback_history = false;
    submit(&mut controller, "echo private");
    let path = history_path(&controller.config);
    assert!(!path.exists());
    if let Ok(mut entries) = fs::read_dir(path.parent().unwrap()) {
        assert!(entries.all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("cmd-log")
        }));
    }
}

/// Enabled logging continues appending commands to the existing durable history.
#[test]
fn enabled_history_persists_new_commands() {
    let (mut controller, _directory) = command_fixture();
    let path = history_path(&controller.config);
    write_history(&path, &["echo saved".to_owned()]).unwrap();
    submit(&mut controller, "echo public");
    assert_eq!(read_history(&path).unwrap(), ["echo saved", "echo public"]);
}

/// Re-enabling history never flushes private commands or a draft already being edited.
#[test]
fn reenabled_history_excludes_private_commands_and_preserves_pending_draft() {
    let (mut controller, _directory) = command_fixture();
    let path = history_path(&controller.config);
    write_history(&path, &["echo saved".to_owned()]).unwrap();
    controller.config.persistence.save_playback_history = false;
    submit(&mut controller, "echo private");
    controller.dispatch(UiAction::BeginLocalCommand);
    type_text(&mut controller, "echo private draft");
    controller.config.persistence.save_playback_history = true;
    finish_submission(&mut controller, "echo private draft");
    assert_eq!(read_history(&path).unwrap(), ["echo saved"]);
    submit(&mut controller, "echo public");
    assert_eq!(read_history(&path).unwrap(), ["echo saved", "echo public"]);
}
