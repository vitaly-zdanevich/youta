//! Typed terminal commands share captured-target download and execution ownership.

use super::*;
use crate::download_queue::DownloadQueueState;
use crate::local_command::CommandOutput;
use crate::view::{CustomCommandMode, NowPlayingView};

/// Enables the terminal prompt over the existing supervised, offline download fixture.
fn fixture() -> (AppController, tempfile::TempDir) {
    let (mut controller, directory) = super::custom_command_tests::fixture("");
    controller.set_custom_command_mode(CustomCommandMode::Terminal);
    controller.set_local_command_available(true);
    controller.view.playback.idle = false;
    controller.view.playback.paused = false;
    controller.view.now_playing = Some(NowPlayingView {
        media_id: MediaId::new(SourceKind::SoundCloud, "playing-other-source"),
        title: "Unrelated playing item".to_owned(),
        subtitle: String::new(),
    });
    (controller, directory)
}

/// Sends printable editor input through the same semantic actions as the terminal.
fn type_text(controller: &mut AppController, template: &str) {
    for character in template.chars() {
        controller.dispatch(UiAction::AppendLocalCommandCharacter(character));
    }
}

/// Simulates a later selection update without running a provider or changing playback.
fn select_other_item(controller: &mut AppController) {
    let mut video = fixture_download_video();
    video.video_id = "abcdefghijk".to_owned();
    video.webpage_url =
        Some(url::Url::parse("https://www.youtube.com/watch?v=abcdefghijk").unwrap());
    controller.youtube_results = vec![SearchItem::Video(video)];
    controller.refresh_youtube_rows();
    controller.view.details = Some(DetailView {
        media_id: Some(MediaId::new(SourceKind::YouTube, "abcdefghijk")),
        webpage_url: Some(url::Url::parse("https://www.youtube.com/watch?v=abcdefghijk").unwrap()),
        ..Default::default()
    });
}

/// Opening the editor captures the selected provider URL, never playback or later selection.
#[test]
fn typed_prompt_captures_selected_url_before_typing_and_ignores_playback() {
    let (mut controller, _directory) = fixture();
    controller.dispatch(UiAction::BeginLocalCommand);
    assert!(controller.view.local_command.is_some());
    select_other_item(&mut controller);
    type_text(&mut controller, "echo %");
    controller.dispatch(UiAction::SubmitLocalCommand);
    let plan = controller.take_custom_command_plan().unwrap();
    assert_eq!(plan.template, "echo %");
    assert_eq!(plan.argument, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    assert!(plan.downloaded_path.is_none());
    assert!(controller.manual_downloads.queue.entries.is_empty());
    assert!(controller.take_custom_command_plan().is_none());
    controller.report_custom_command_result(Ok(CommandOutput {
        output: String::new(),
        success: true,
    }));
    assert_eq!(controller.view.screen, Screen::Search);
    controller.dispatch(UiAction::BeginLocalCommand);
    assert!(controller.view.local_command.is_some());
}

/// A typed download command retains its captured URL through choice, transfer, and a modal.
#[test]
fn typed_prompt_download_uses_captured_source_and_waits_until_modal_closes() {
    let (mut controller, _directory) = fixture();
    controller.config.downloads.mode = crate::config::DownloadMode::AskEachTime;
    controller.dispatch(UiAction::BeginLocalCommand);
    assert!(controller.view.local_command.is_some());
    select_other_item(&mut controller);
    type_text(&mut controller, "echo % %d");
    controller.dispatch(UiAction::SubmitLocalCommand);
    assert!(controller.view.local_command.is_none());
    let generation = controller
        .view
        .download_choice_popup
        .as_ref()
        .expect("ordinary download format chooser")
        .generation;
    assert!(controller.manual_downloads.active.is_none());
    assert!(controller.take_custom_command_plan().is_none());
    assert_eq!(controller.manual_downloads.queue.entries.len(), 1);
    assert_eq!(
        controller.manual_downloads.queue.entries[0]
            .source
            .media_id
            .external_id,
        "dQw4w9WgXcQ"
    );
    controller.dispatch(UiAction::ConfirmDownloadChoice(generation));
    let owner = controller
        .manual_downloads
        .active
        .expect("mock download started");
    assert!(controller.take_custom_command_plan().is_none());
    controller.view.screen = Screen::Statistics;
    controller.view.rows.clear();
    controller.view.details = None;
    controller.view.help_open = true;
    let path = controller
        .config
        .downloads_dir()
        .join("typed command completed.opus");
    std::fs::write(&path, b"offline media fixture").unwrap();
    controller.finish_manual_download_for(owner, Ok(path.clone()));
    controller.refresh_custom_command_buttons();
    assert!(controller.take_custom_command_plan().is_none());
    assert_eq!(
        controller.manual_downloads.queue.entries[0].state,
        DownloadQueueState::Completed
    );
    controller.dispatch(UiAction::ToggleHelp);
    controller.refresh_custom_command_buttons();
    let plan = controller.take_custom_command_plan().unwrap();
    assert_eq!(plan.template, "echo % %d");
    assert_eq!(plan.argument, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    assert_eq!(plan.downloaded_path, Some(path));
    assert!(controller.take_custom_command_plan().is_none());
    controller.report_custom_command_result(Ok(CommandOutput {
        output: String::new(),
        success: true,
    }));
    assert_eq!(controller.view.screen, Screen::Statistics);
    controller.dispatch(UiAction::BeginLocalCommand);
    assert!(controller.view.local_command.is_some());
}

/// Cancelling the chooser or transfer, and failed transfers, discard typed continuations.
#[test]
fn typed_prompt_cancelled_or_failed_download_does_not_run_and_releases_ownership() {
    for failure in ["chooser", "cancel", "failed"] {
        let (mut controller, _directory) = fixture();
        if failure == "chooser" {
            controller.config.downloads.mode = crate::config::DownloadMode::AskEachTime;
        }
        controller.dispatch(UiAction::BeginLocalCommand);
        assert!(controller.view.local_command.is_some());
        type_text(&mut controller, "echo %d");
        controller.dispatch(UiAction::SubmitLocalCommand);
        assert!(controller.take_custom_command_plan().is_none());
        if failure == "chooser" {
            assert!(controller.view.download_choice_popup.is_some());
            controller.dispatch(UiAction::DismissDownloadChoice);
        } else {
            let owner = controller.manual_downloads.active.unwrap();
            if failure == "cancel" {
                controller.cancel_queued_download(owner.0);
            } else {
                controller.finish_manual_download_for(owner, Err(()));
            }
        }
        controller.refresh_custom_command_buttons();
        assert!(controller.take_custom_command_plan().is_none(), "{failure}");
        assert_ne!(
            controller.manual_downloads.queue.entries[0].state,
            DownloadQueueState::Completed,
            "{failure}"
        );
        controller.dispatch(UiAction::BeginLocalCommand);
        assert!(controller.view.local_command.is_some(), "{failure}");
        type_text(&mut controller, "pwd");
        controller.dispatch(UiAction::SubmitLocalCommand);
        assert_eq!(
            controller.take_custom_command_plan().unwrap().template,
            "pwd"
        );
        controller.report_custom_command_result(Ok(CommandOutput {
            output: String::new(),
            success: true,
        }));
    }
}
