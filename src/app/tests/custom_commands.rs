//! User-authored command buttons keep provider/target identity through foreground work.

use super::*;
use crate::download_queue::DownloadQueueState;
use crate::local_command::CommandOutput;
use crate::view::CustomCommandMode;

/// Uses the existing supervised download fixture; no network or user commands run here.
pub(super) fn fixture(commands: &str) -> (AppController, tempfile::TempDir) {
    let directory = crate::test_support::canonical_tempdir("custom command fixture");
    let config = Config::for_dir(directory.path().join("config"));
    config.ensure_directories().unwrap();
    std::fs::write(config.config_dir().join("commands"), commands).unwrap();
    let process = MockRunningDownload {
        progress: Some(Cursor::new(Vec::new())),
        errors: Some(Cursor::new(Vec::new())),
        exits: VecDeque::new(),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let (mut controller, _, _) = controller_with_mock_download(config, process);
    controller.view.details = Some(DetailView {
        media_id: Some(MediaId::new(SourceKind::YouTube, "dQw4w9WgXcQ")),
        title: "Selected YouTube item".to_owned(),
        webpage_url: Some(url::Url::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap()),
        ..Default::default()
    });
    controller.view.search_editing = false;
    controller.set_custom_command_mode(CustomCommandMode::Dialog);
    (controller, directory)
}

/// Omission means all providers; explicit filters select against actual source identities.
#[test]
fn custom_commands_filter_providers_without_exposing_templates_or_descriptions() {
    let (mut controller, directory) = fixture(
        "[[commands]]\nname='Everywhere'\ndescription='SECRET DESCRIPTION'\ncommand='echo SECRET COMMAND %'\n\
         [[commands]]\nname='YouTube only'\nprovider='youtube'\ncommand='echo %'\n\
         [[commands]]\nname='Local only'\nprovider='local'\ncommand='echo %'\n",
    );
    assert_eq!(
        controller
            .view
            .custom_command_buttons
            .iter()
            .map(|button| button.id)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    let json = serde_json::to_string(&controller.view).unwrap();
    assert!(!json.contains("SECRET"));
    let path = directory.path().join("local file.flac");
    std::fs::write(&path, b"fixture").unwrap();
    controller.view.screen = Screen::History;
    controller.view.rows = vec![RowView {
        media_id: Some(local_media_id(&path)),
        ..Default::default()
    }];
    controller.view.details = Some(DetailView {
        media_id: Some(local_media_id(&path)),
        ..Default::default()
    });
    controller.refresh_custom_command_buttons();
    assert_eq!(
        controller
            .view
            .custom_command_buttons
            .iter()
            .map(|button| button.id)
            .collect::<Vec<_>>(),
        [0, 2]
    );
    controller.dispatch(UiAction::RunCustomCommand(1));
    assert!(controller.take_custom_command_plan().is_none());
    controller.dispatch(UiAction::RunCustomCommand(2));
    let plan = controller.take_custom_command_plan().unwrap();
    assert_eq!(plan.argument, path.as_os_str());
    assert_eq!(plan.downloaded_path, Some(path));
}

/// Mouse, IPC and direct bug-report actions cannot bypass the foreground owner's modal.
#[test]
fn custom_commands_are_single_use_and_modal_with_private_output() {
    let (mut controller, _directory) = fixture("[[commands]]\nname='Run'\ncommand='echo %'\n");
    controller.dispatch(UiAction::BeginSearch);
    controller.dispatch(UiAction::RunCustomCommand(0));
    assert!(controller.take_custom_command_plan().is_none());
    controller.dispatch(UiAction::CancelSearch);
    controller.dispatch(UiAction::RunCustomCommand(0));
    let plan = controller.take_custom_command_plan().unwrap();
    assert_eq!(plan.argument, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    controller.dispatch(UiAction::RunCustomCommand(0));
    controller.open_bug_report(Some("PRIVATE OUTPUT".to_owned()));
    assert!(controller.view.bug_report_popup.is_none());
    assert!(controller.take_custom_command_plan().is_none());
    controller.dispatch(UiAction::DismissCustomCommandOutput);
    controller.dispatch(UiAction::Quit);
    assert!(!controller.view.quitting);
    assert!(
        controller
            .view
            .custom_command_output
            .as_ref()
            .unwrap()
            .running
    );
    controller.report_custom_command_result(Ok(CommandOutput {
        output: "PRIVATE OUTPUT".to_owned(),
        success: false,
    }));
    assert!(!format!("{:?}", controller.view).contains("PRIVATE OUTPUT"));
    assert!(!controller.view.bug_report_screenshot_allowed());
    assert!(
        controller
            .view
            .custom_command_output
            .as_ref()
            .unwrap()
            .failed
    );
    controller.dispatch(UiAction::DismissCustomCommandOutput);
    assert!(controller.view.custom_command_output.is_none());
}

/// A source switch during a download cannot retarget an already accepted command.
#[test]
fn custom_commands_download_then_run_only_the_captured_item_and_defer_for_editors() {
    let (mut controller, _directory) =
        fixture("[[commands]]\nname='Convert'\ncommand='echo % %d'\n");
    controller.dispatch(UiAction::RunCustomCommand(0));
    assert!(controller.take_custom_command_plan().is_none());
    let owner = controller.manual_downloads.active.unwrap();
    controller.view.search_editing = true;
    let path = controller.config.downloads_dir().join("finished file.opus");
    std::fs::write(&path, b"finished media").unwrap();
    controller.finish_manual_download_for(owner, Ok(path.clone()));
    controller.refresh_custom_command_buttons();
    assert!(controller.take_custom_command_plan().is_none());
    controller.view.details = Some(DetailView {
        media_id: Some(MediaId::new(SourceKind::YouTube, "abcdefghijk")),
        ..Default::default()
    });
    controller.view.search_editing = false;
    controller.refresh_custom_command_buttons();
    let plan = controller.take_custom_command_plan().unwrap();
    assert_eq!(plan.argument, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    assert_eq!(plan.downloaded_path, Some(path));
}

/// Cancelling either the transfer or its format chooser consumes the continuation.
#[test]
fn custom_commands_cancelled_or_failed_download_never_runs() {
    for cancelled in [false, true] {
        let (mut controller, _directory) =
            fixture("[[commands]]\nname='Convert'\ncommand='echo %d'\n");
        controller.dispatch(UiAction::RunCustomCommand(0));
        let owner = controller.manual_downloads.active.unwrap();
        if cancelled {
            controller.cancel_queued_download(owner.0);
        } else {
            controller.finish_manual_download_for(owner, Err(()));
        }
        controller.refresh_custom_command_buttons();
        assert!(controller.take_custom_command_plan().is_none());
        assert!(controller.view.custom_command_output.is_none());
        assert_ne!(
            controller.manual_downloads.queue.entries[0].state,
            DownloadQueueState::Completed
        );
    }
}

/// Opening a linked YouTube video must not download the still-selected background row.
#[test]
fn custom_commands_linked_video_uses_same_identity_for_url_and_download() {
    let (mut controller, _directory) =
        fixture("[[commands]]\nname='Convert'\ncommand='echo % %d'\n");
    controller.active_description_video = Some(ActiveDescriptionVideo {
        video_id: "abcdefghijk".to_owned(),
        start_seconds: None,
    });
    controller.view.details.as_mut().unwrap().media_id =
        Some(MediaId::new(SourceKind::YouTube, "abcdefghijk"));
    controller.dispatch(UiAction::RunCustomCommand(0));
    assert!(
        !controller.manual_downloads.queue.entries.is_empty(),
        "{}",
        controller.view.status_line
    );
    assert_eq!(
        controller.manual_downloads.queue.entries[0]
            .source
            .media_id
            .external_id,
        "abcdefghijk"
    );
    let owner = controller.manual_downloads.active.unwrap();
    let path = controller.config.downloads_dir().join("linked.opus");
    std::fs::write(&path, b"linked media").unwrap();
    controller.finish_manual_download_for(owner, Ok(path.clone()));
    controller.refresh_custom_command_buttons();
    let plan = controller.take_custom_command_plan().unwrap();
    assert_eq!(plan.argument, "https://www.youtube.com/watch?v=abcdefghijk");
    assert_eq!(plan.downloaded_path, Some(path));
}

/// A completed download remains useful without enqueuing another transfer.
#[test]
fn custom_commands_reuse_validated_downloads_and_reject_stale_detail_targets() {
    let (mut controller, _directory) = fixture("[[commands]]\nname='Convert'\ncommand='echo %d'\n");
    controller.dispatch(UiAction::RunCustomCommand(0));
    let owner = controller.manual_downloads.active.unwrap();
    let path = controller.config.downloads_dir().join("existing.opus");
    std::fs::write(&path, b"media").unwrap();
    controller.finish_manual_download_for(owner, Ok(path));
    controller.refresh_custom_command_buttons();
    assert!(controller.take_custom_command_plan().is_some());
    controller.report_custom_command_result(Ok(CommandOutput {
        output: String::new(),
        success: true,
    }));
    controller.dispatch(UiAction::DismissCustomCommandOutput);
    controller.dispatch(UiAction::RunCustomCommand(0));
    assert!(controller.take_custom_command_plan().is_some());
    assert_eq!(controller.manual_downloads.queue.entries.len(), 1);
    controller.report_custom_command_result(Ok(CommandOutput {
        output: String::new(),
        success: true,
    }));
    controller.dispatch(UiAction::DismissCustomCommandOutput);
    controller.view.details.as_mut().unwrap().media_id =
        Some(MediaId::new(SourceKind::SoundCloud, "stale"));
    controller.dispatch(UiAction::RunCustomCommand(0));
    assert!(controller.take_custom_command_plan().is_none());
}

/// Cancelling the ordinary format chooser must discard its associated command too.
#[test]
fn custom_commands_use_the_normal_format_chooser_and_respect_cancellation() {
    let (mut controller, _directory) = fixture("[[commands]]\nname='Convert'\ncommand='echo %d'\n");
    controller.config.downloads.mode = crate::config::DownloadMode::AskEachTime;
    controller.dispatch(UiAction::RunCustomCommand(0));
    assert!(controller.view.download_choice_popup.is_some());
    assert!(controller.manual_downloads.active.is_none());
    controller.dispatch(UiAction::DismissDownloadChoice);
    controller.refresh_custom_command_buttons();
    assert!(controller.take_custom_command_plan().is_none());
    assert!(controller.view.custom_command_output.is_none());
    assert_eq!(
        controller.manual_downloads.queue.entries[0].state,
        DownloadQueueState::Cancelled
    );
}

/// Remote Log entries use their saved replay identity, never a stale provider array.
#[test]
fn custom_commands_history_uses_the_saved_original_url() {
    let (mut controller, _directory) =
        fixture("[[commands]]\nname='Run'\nprovider='soundcloud'\ncommand='echo %'\n");
    let identity = MediaId::new(SourceKind::SoundCloud, "42");
    let url = "https://soundcloud.com/fixture/track";
    controller.view.screen = Screen::History;
    controller.history_entries = vec![HistoryListEntry {
        entry: HistoryEntry {
            id: 1,
            media_id: identity.clone(),
            title: "Track".to_owned(),
            replay_locator: Some(url.to_owned()),
            started_at: 1,
            last_played_at: 2,
            position_seconds: 0,
            duration_seconds: None,
            finished: false,
        },
        local_removed: false,
    }];
    controller.view.rows = vec![RowView {
        media_id: Some(identity.clone()),
        ..Default::default()
    }];
    controller.view.details = Some(DetailView {
        media_id: Some(identity),
        ..Default::default()
    });
    controller.dispatch(UiAction::RunCustomCommand(0));
    assert_eq!(controller.take_custom_command_plan().unwrap().argument, url);
}

/// Subscriptions own their item selection separately from the preceding provider list.
#[test]
fn custom_commands_subscriptions_ignore_the_previous_generic_list_selection() {
    let (mut controller, _directory) =
        fixture("[[commands]]\nname='Run'\nprovider='youtube'\ncommand='echo %'\n");
    let mut video = fixture_download_video();
    video.video_id = "abcdefghijk".to_owned();
    controller.view.screen = Screen::Subscriptions;
    controller.view.subscriptions.route = SubscriptionRoute::Items;
    controller.view.subscriptions.focus = SubscriptionPane::Items;
    controller.active_subscription_channel_id = Some("UCfixture".to_owned());
    controller.subscription_video_cache.insert(
        "UCfixture".to_owned(),
        CachedSubscriptionVideos {
            items: vec![SearchItem::Video(video)],
            ..Default::default()
        },
    );
    controller.refresh_subscription_video_rows();
    controller.view.details.as_mut().unwrap().media_id =
        Some(MediaId::new(SourceKind::YouTube, "abcdefghijk"));
    controller.dispatch(UiAction::RunCustomCommand(0));
    assert_eq!(
        controller.take_custom_command_plan().unwrap().argument,
        "https://www.youtube.com/watch?v=abcdefghijk"
    );
}

/// A selected row remains usable before its optional Details metadata arrives.
#[test]
fn custom_commands_capture_selected_rows_without_details() {
    let (mut controller, _directory) = fixture("[[commands]]\nname='Run'\ncommand='echo %'\n");
    controller.view.details = None;
    controller.dispatch(UiAction::RunCustomCommand(0));
    assert_eq!(
        controller.take_custom_command_plan().unwrap().argument,
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
    );
}

/// A page without a selected media item must not inherit another page's Details target.
#[test]
fn custom_commands_ignore_stale_details_on_pages_without_selection() {
    let (mut controller, _directory) = fixture("[[commands]]\nname='Run'\ncommand='echo %'\n");
    controller.view.screen = Screen::Statistics;
    controller.view.rows = vec![RowView::default()];
    assert!(controller.custom_command_target().is_none());
    controller.dispatch(UiAction::RunCustomCommand(0));
    assert!(controller.take_custom_command_plan().is_none());
}

/// A selected channel owns its browser URL while finite-media downloads remain unavailable.
#[test]
fn custom_commands_channel_selection_does_not_reuse_the_previous_video() {
    for template in ["echo %", "echo %d"] {
        let (mut controller, _directory) =
            fixture(&format!("[[commands]]\nname='Run'\ncommand='{template}'\n"));
        controller.youtube_results = vec![SearchItem::Channel(ChannelSummary {
            channel_id: "UCselected".to_owned(),
            name: "Selected channel".to_owned(),
            description: String::new(),
            subscriber_count: None,
            video_count: None,
            created_at: None,
            auto_generated: false,
            thumbnails: Vec::new(),
            webpage_url: None,
        })];
        controller.refresh_youtube_rows();
        controller.dispatch(UiAction::RunCustomCommand(0));
        if template == "echo %" {
            assert_eq!(
                controller.take_custom_command_plan().unwrap().argument,
                "https://www.youtube.com/channel/UCselected"
            );
        } else {
            assert!(controller.take_custom_command_plan().is_none());
            assert!(controller.manual_downloads.queue.entries.is_empty());
        }
    }
}

/// Typed invocations keep their captured source while the usual format chooser owns the UI.
#[test]
fn captured_shell_commands_keep_the_source_and_normal_download_chooser() {
    let (mut controller, directory) = fixture("[[commands]]\nname='Run'\ncommand='echo %'\n");
    controller.config.downloads.mode = crate::config::DownloadMode::AskEachTime;
    let download = controller.capture_command_download();
    let plan = crate::local_command::ShellCommandPlan {
        template: "echo % %d".to_owned(),
        argument: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".into(),
        downloaded_path: None,
        directory: directory.path().to_owned(),
    };
    controller.view.screen = Screen::Statistics;
    controller.view.rows.clear();
    controller.view.details = None;
    controller
        .run_captured_shell_command("Command".to_owned(), plan, download, None)
        .unwrap();
    assert!(controller.view.download_choice_popup.is_some());
    assert_eq!(
        controller.manual_downloads.queue.entries[0]
            .source
            .media_id
            .external_id,
        "dQw4w9WgXcQ"
    );
    controller.dispatch(UiAction::DismissDownloadChoice);
    controller.refresh_custom_command_buttons();
    assert!(controller.take_custom_command_plan().is_none());
}

/// A command without macros does not require either media selection or a downloader.
#[test]
fn captured_shell_commands_allow_plain_commands_without_download_sources() {
    let (mut controller, directory) = fixture("[[commands]]\nname='Run'\ncommand='echo %'\n");
    controller.view.screen = Screen::Statistics;
    controller.view.rows.clear();
    controller.view.details = None;
    let download = controller.capture_command_download();
    assert!(download.source.is_err());
    let plan = crate::local_command::ShellCommandPlan {
        template: "pwd".to_owned(),
        argument: Default::default(),
        downloaded_path: None,
        directory: directory.path().to_owned(),
    };
    controller
        .run_captured_shell_command("Command".to_owned(), plan, download, None)
        .unwrap();
    assert_eq!(
        controller.take_custom_command_plan().unwrap().template,
        "pwd"
    );
    assert!(controller.manual_downloads.queue.entries.is_empty());
}

/// An existing validated file remains usable when the captured downloader is unavailable.
#[test]
fn captured_shell_commands_reuse_downloads_before_checking_source_errors() {
    let (mut controller, directory) = fixture("[[commands]]\nname='Convert'\ncommand='echo %d'\n");
    controller.dispatch(UiAction::RunCustomCommand(0));
    let owner = controller.manual_downloads.active.unwrap();
    let path = controller.config.downloads_dir().join("captured.opus");
    std::fs::write(&path, b"finished media").unwrap();
    controller.finish_manual_download_for(owner, Ok(path.clone()));
    controller.refresh_custom_command_buttons();
    assert!(controller.take_custom_command_plan().is_some());
    controller.report_custom_command_result(Ok(CommandOutput {
        output: String::new(),
        success: true,
    }));
    controller.dispatch(UiAction::DismissCustomCommandOutput);
    let mut download = controller.capture_command_download();
    download.source = Err("Downloads are unavailable".to_owned());
    let plan = crate::local_command::ShellCommandPlan {
        template: "echo %d".to_owned(),
        argument: "https://www.youtube.com/watch?v=dQw4w9WgXcQ".into(),
        downloaded_path: None,
        directory: directory.path().to_owned(),
    };
    controller
        .run_captured_shell_command("Command".to_owned(), plan, download, None)
        .unwrap();
    assert_eq!(
        controller
            .take_custom_command_plan()
            .unwrap()
            .downloaded_path,
        Some(path)
    );
    assert_eq!(controller.manual_downloads.queue.entries.len(), 1);
}

/// Shared keyboard routing gives custom bindings priority only outside editors and popups.
#[test]
fn custom_command_hotkeys_respect_modal_precedence_and_keep_stable_ids() {
    use crate::keymap::{Key, KeyPress, custom_command_button_action, key_action};
    let (mut controller, _directory) =
        fixture("[[commands]]\nname='Run'\ncommand='echo %'\nhotkey='Ctrl+W'\n");
    let key = KeyPress {
        key: Key::Char('w'),
        ctrl: true,
        alt: false,
        shift: false,
    };
    assert_eq!(
        key_action(key, &controller.view, None, None),
        Some(UiAction::RunCustomCommand(0))
    );
    controller.view.search_editing = true;
    assert_eq!(
        key_action(key, &controller.view, None, None),
        Some(UiAction::DeleteSearchWord)
    );
    assert_ne!(
        custom_command_button_action(&controller.view, 0),
        Some(UiAction::RunCustomCommand(0))
    );
    controller.view.search_editing = false;
    controller.dispatch(UiAction::RunCustomCommand(0));
    assert_eq!(key_action(key, &controller.view, None, None), None);
    assert_eq!(
        key_action(KeyPress::new(Key::Esc), &controller.view, None, None),
        None
    );
    controller.report_custom_command_result(Ok(CommandOutput {
        output: String::new(),
        success: true,
    }));
    assert_eq!(
        key_action(KeyPress::new(Key::Esc), &controller.view, None, None),
        Some(UiAction::DismissCustomCommandOutput)
    );
}
