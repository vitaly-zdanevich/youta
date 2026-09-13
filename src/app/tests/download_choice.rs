//! Manual download confirmation regressions; helpers never launch a real downloader.

use super::*;

fn confirm_current_choice(controller: &mut AppController) {
    let generation = controller
        .view
        .download_choice_popup
        .as_ref()
        .map_or(0, |popup| popup.generation);
    controller.dispatch(UiAction::ConfirmDownloadChoice(generation));
}

fn select_current_choice(controller: &mut AppController, index: usize) {
    let generation = controller
        .view
        .download_choice_popup
        .as_ref()
        .map_or(0, |popup| popup.generation);
    controller.dispatch(UiAction::SelectDownloadChoice { generation, index });
}

fn choice_controller() -> (
    AppController,
    Arc<Mutex<Vec<DownloadRequest>>>,
    tempfile::TempDir,
) {
    let directory = crate::test_support::canonical_tempdir("download choices");
    let config = Config::for_dir(directory.path().join("youta"));
    let process = MockRunningDownload {
        progress: Some(Cursor::new(Vec::new())),
        errors: Some(Cursor::new(Vec::new())),
        exits: VecDeque::new(),
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let (mut controller, requests, _) = controller_with_mock_download(config, process);
    controller.config.downloads.mode = crate::config::DownloadMode::AskEachTime;
    (controller, requests, directory)
}

#[test]
fn default_manual_download_asks_before_starting_a_process() {
    let (mut controller, requests, _directory) = choice_controller();
    controller.dispatch(UiAction::Download);
    assert!(
        requests.lock().unwrap().is_empty(),
        "Ask each time must not start a child"
    );
    let popup = controller
        .view
        .download_choice_popup
        .as_ref()
        .expect("download choice");
    assert_eq!(popup.options.len(), 2);
    assert!(
        popup
            .options
            .iter()
            .any(|label| label.contains("Audio only"))
    );
}

#[test]
fn cancelling_manual_download_does_not_start_a_process() {
    let (mut controller, requests, _directory) = choice_controller();
    controller.dispatch(UiAction::Download);
    controller.dispatch(UiAction::DismissDownloadChoice);
    confirm_current_choice(&mut controller);
    assert!(requests.lock().unwrap().is_empty());
    assert!(controller.view.download_choice_popup.is_none());
}

#[test]
fn saved_manual_modes_skip_the_popup_without_using_the_subscription_transcode_setting() {
    use crate::config::DownloadMode;
    for (mode, format) in [
        (DownloadMode::Video, DownloadFormat::BestVideo),
        (
            DownloadMode::AudioOnly,
            DownloadFormat::AudioOnlyWithoutReencoding,
        ),
    ] {
        let (mut controller, requests, _directory) = choice_controller();
        controller.config.downloads.mode = mode;
        controller.config.subscriptions.audio_format = "transcode-opus".to_owned();
        controller.dispatch(UiAction::Download);
        assert!(controller.view.download_choice_popup.is_none());
        assert_eq!(requests.lock().unwrap()[0].format, format);
    }
}

#[test]
fn confirmation_uses_the_captured_source_once_and_ignores_invalid_indexes() {
    let (mut controller, requests, _directory) = choice_controller();
    controller.dispatch(UiAction::Download);
    controller.dispatch(UiAction::MoveDownloadChoice(i32::MAX));
    assert_eq!(
        controller
            .view
            .download_choice_popup
            .as_ref()
            .unwrap()
            .selected,
        1
    );
    select_current_choice(&mut controller, usize::MAX);
    assert!(requests.lock().unwrap().is_empty());
    controller.youtube_results.clear();
    controller.refresh_youtube_rows();
    confirm_current_choice(&mut controller);
    confirm_current_choice(&mut controller);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].source_url.as_str(),
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
    );
    assert_eq!(
        requests[0].format,
        DownloadFormat::AudioOnlyWithoutReencoding
    );
}

#[test]
fn preference_drafts_persist_both_choices_without_changing_automation() {
    let (mut controller, _requests, _directory) = choice_controller();
    let legacy = controller.config.subscriptions.audio_format.clone();
    controller.open_preferences();
    controller.dispatch(UiAction::CycleDownloadModePreference);
    #[cfg(feature = "archive-org")]
    controller.dispatch(UiAction::CycleArchiveDownloadPreference);
    controller.submit_preferences();
    assert!(controller.view.error_popup.is_none());
    assert_eq!(
        controller.config.downloads.mode,
        crate::config::DownloadMode::Video
    );
    #[cfg(feature = "archive-org")]
    assert_eq!(
        controller.config.downloads.archive_format,
        crate::config::ArchiveDownloadPreference::OriginalFile
    );
    assert_eq!(controller.config.subscriptions.audio_format, legacy);
    let saved = std::fs::read_to_string(controller.config.config_file()).unwrap();
    assert!(saved.contains("[downloads]"));
    assert!(saved.contains("mode = \"video\""));
}

#[cfg(feature = "archive-org")]
fn archive_variant(
    extension: &str,
    provenance: crate::providers::archive_org::ArchiveOrgFileProvenance,
    is_video: bool,
) -> crate::providers::archive_org::ArchiveOrgDownloadVariant {
    crate::providers::archive_org::ArchiveOrgDownloadVariant {
        filename: format!("track.{extension}"),
        download_url: url::Url::parse(&format!(
            "https://archive.org/download/fixture/track.{extension}"
        ))
        .unwrap(),
        format: extension.to_ascii_uppercase(),
        size_bytes: Some(1024),
        provenance,
        is_video,
    }
}

#[cfg(feature = "archive-org")]
#[test]
fn archive_original_and_generated_audio_are_exact_separate_choices() {
    use crate::providers::archive_org::ArchiveOrgFileProvenance::{Derivative, Original};
    let (mut controller, requests, _directory) = choice_controller();
    let item = controller.selected_queue_item().unwrap();
    let variants = vec![
        archive_variant("flac", Original, false),
        archive_variant("mp3", Derivative, false),
    ];
    controller.choose_archive_download(item, variants);
    let popup = controller.view.download_choice_popup.as_ref().unwrap();
    assert!(popup.options[0].contains("Original: FLAC"));
    assert!(popup.options[1].contains("Archive-generated: MP3"));
    assert!(popup.options[0].contains("KiB"));
    assert!(requests.lock().unwrap().is_empty());
    select_current_choice(&mut controller, 0);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].format, DownloadFormat::ExactFile);
    assert!(requests[0].source_url.as_str().ends_with("track.flac"));
}

#[cfg(feature = "archive-org")]
#[test]
fn archive_preference_selects_only_one_matching_available_variant() {
    use crate::config::ArchiveDownloadPreference;
    use crate::providers::archive_org::ArchiveOrgFileProvenance::{Derivative, Original};
    for (preference, extension) in [
        (ArchiveDownloadPreference::OriginalFile, "flac"),
        (ArchiveDownloadPreference::ArchiveMp3, "mp3"),
    ] {
        let (mut controller, requests, _directory) = choice_controller();
        controller.config.downloads.archive_format = preference;
        let item = controller.selected_queue_item().unwrap();
        controller.choose_archive_download(
            item,
            vec![
                archive_variant("flac", Original, false),
                archive_variant("mp3", Derivative, false),
            ],
        );
        assert!(controller.view.download_choice_popup.is_none());
        assert_eq!(
            requests.lock().unwrap()[0].format,
            DownloadFormat::ExactFile
        );
        assert!(
            requests.lock().unwrap()[0]
                .source_url
                .as_str()
                .ends_with(extension)
        );
    }
}

#[cfg(feature = "archive-org")]
#[test]
fn archive_missing_or_ambiguous_preference_asks_instead_of_substituting() {
    use crate::config::ArchiveDownloadPreference;
    use crate::providers::archive_org::ArchiveOrgFileProvenance::Original;
    for ambiguous in [false, true] {
        let (mut controller, requests, _directory) = choice_controller();
        controller.config.downloads.archive_format = if ambiguous {
            ArchiveDownloadPreference::OriginalFile
        } else {
            ArchiveDownloadPreference::ArchiveMp3
        };
        let item = controller.selected_queue_item().unwrap();
        let mut variants = vec![archive_variant("flac", Original, false)];
        if ambiguous {
            variants.push(archive_variant("wav", Original, false));
        }
        controller.choose_archive_download(item, variants);
        assert!(requests.lock().unwrap().is_empty());
        assert!(
            controller
                .view
                .download_choice_popup
                .as_ref()
                .unwrap()
                .explanation
                .contains("unavailable or matches multiple")
        );
    }
}

#[cfg(feature = "archive-org")]
#[test]
fn archive_video_asks_for_original_or_strict_audio_after_file_selection() {
    use crate::providers::archive_org::ArchiveOrgFileProvenance::Original;
    for (index, format) in [
        (0, DownloadFormat::ExactFile),
        (1, DownloadFormat::AudioOnlyWithoutReencoding),
    ] {
        let (mut controller, requests, _directory) = choice_controller();
        let item = controller.selected_queue_item().unwrap();
        controller.choose_archive_download(item, vec![archive_variant("mkv", Original, true)]);
        assert_eq!(
            controller
                .view
                .download_choice_popup
                .as_ref()
                .unwrap()
                .options
                .len(),
            2
        );
        assert!(requests.lock().unwrap().is_empty());
        select_current_choice(&mut controller, index);
        assert_eq!(requests.lock().unwrap()[0].format, format);
        assert!(
            requests.lock().unwrap()[0]
                .source_url
                .as_str()
                .ends_with(".mkv")
        );
    }
}

#[test]
fn source_validation_is_repeated_before_launching_a_chosen_download() {
    let (mut controller, requests, _directory) = choice_controller();
    let mut item = controller.selected_queue_item().unwrap();
    item.media.webpage_url = url::Url::parse("https://user:password@example.com/media").unwrap();
    controller.choose_manual_download(item);
    confirm_current_choice(&mut controller);
    assert!(requests.lock().unwrap().is_empty());
    assert!(controller.view.status_line.contains("credential-free"));
}

#[cfg(feature = "archive-org")]
#[test]
fn stale_archive_file_click_cannot_confirm_the_following_video_mode_popup() {
    use crate::providers::archive_org::ArchiveOrgFileProvenance::{Derivative, Original};
    let (mut controller, requests, _directory) = choice_controller();
    let item = controller.selected_queue_item().unwrap();
    controller.choose_archive_download(
        item,
        vec![
            archive_variant("flac", Original, false),
            archive_variant("mp4", Derivative, true),
        ],
    );
    let first = controller
        .view
        .download_choice_popup
        .as_ref()
        .unwrap()
        .generation;
    controller.dispatch(UiAction::SelectDownloadChoice {
        generation: first,
        index: 1,
    });
    let second = controller
        .view
        .download_choice_popup
        .as_ref()
        .unwrap()
        .generation;
    assert_ne!(first, second);
    assert!(
        !controller
            .view
            .download_choice_popup
            .as_ref()
            .unwrap()
            .options[0]
            .contains("Original")
    );
    controller.dispatch(UiAction::SelectDownloadChoice {
        generation: first,
        index: 1,
    });
    controller.dispatch(UiAction::ConfirmDownloadChoice(first));
    assert!(
        requests.lock().unwrap().is_empty(),
        "a stale click must not confirm a new question"
    );
    assert_eq!(
        controller
            .view
            .download_choice_popup
            .as_ref()
            .unwrap()
            .generation,
        second
    );
    controller.dispatch(UiAction::SelectDownloadChoice {
        generation: second,
        index: 1,
    });
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].format,
        DownloadFormat::AudioOnlyWithoutReencoding
    );
    assert!(requests[0].source_url.as_str().ends_with("track.mp4"));
}
