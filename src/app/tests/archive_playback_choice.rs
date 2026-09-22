//! Exact-file playback choices remain independent of download preferences and navigation.

use super::*;
use crate::config::ArchivePlaybackPreference;
use crate::providers::archive_org::ArchiveOrgItemDetails;

/// A deliberate pending selection takes precedence over automatic EOF continuation.
#[test]
fn archive_playback_explicit_choice_survives_previous_track_eof() {
    let (mut controller, state) = choice_controller();
    controller.config.playback.archive_format = ArchivePlaybackPreference::OriginalFile;
    controller.activate_archive_org_selection();
    controller.config.playback.archive_format = ArchivePlaybackPreference::AskEachTime;
    controller.view.selected = 1;
    controller.activate_archive_org_selection();
    let generation = controller
        .view
        .archive_playback_choice_popup
        .as_ref()
        .unwrap()
        .generation;
    controller.config.playback.autoplay = true;
    controller.playback_phase = PlaybackPhase::Playing;
    controller.handle_playback_end(
        PlaybackEnd {
            reason: PlaybackEndReason::Eof,
            error: None,
            file_error: None,
            diagnostic: None,
        },
        Duration::ZERO,
    );
    assert_eq!(
        controller
            .view
            .archive_playback_choice_popup
            .as_ref()
            .map(|popup| popup.generation),
        Some(generation)
    );
    assert_eq!(state.lock().unwrap().played.len(), 1);
    assert_eq!(controller.playback_queue.items.len(), 1);
    controller.dispatch(UiAction::SelectArchivePlaybackChoice {
        generation,
        index: 2,
    });
    assert_eq!(state.lock().unwrap().played.len(), 2);
    assert_eq!(
        controller
            .playback_queue
            .items
            .last()
            .unwrap()
            .playback_location,
        "https://archive.org/download/fixture/2.ogg"
    );
}

/// EOF consumes the ended slot even with repeat enabled, without starting queued rows.
#[test]
fn archive_playback_pending_choice_consumes_eof_before_queued_items_or_repeat() {
    for repeat in [false, true] {
        for queued in [false, true] {
            let (mut controller, state) = choice_controller();
            controller.config.playback.archive_format = ArchivePlaybackPreference::OriginalFile;
            controller.activate_archive_org_selection();
            if queued {
                let later = controller.playback_queue.items[0].clone();
                controller.playback_queue.push(later);
            }
            controller.playback_queue.repeat_one = repeat;
            controller.config.playback.archive_format = ArchivePlaybackPreference::AskEachTime;
            controller.view.selected = 1;
            controller.activate_archive_org_selection();
            let generation = controller
                .view
                .archive_playback_choice_popup
                .as_ref()
                .unwrap()
                .generation;
            controller.playback_phase = PlaybackPhase::Playing;
            controller.handle_playback_end(
                PlaybackEnd {
                    reason: PlaybackEndReason::Eof,
                    error: None,
                    file_error: None,
                    diagnostic: None,
                },
                Duration::ZERO,
            );
            assert_eq!(controller.playback_queue.current_index, queued.then_some(1));
            assert_eq!(controller.playback_queue.repeat_one, repeat);
            assert_eq!(state.lock().unwrap().played.len(), 1);
            controller.dispatch(UiAction::SelectArchivePlaybackChoice {
                generation,
                index: 2,
            });
            assert_eq!(state.lock().unwrap().played.len(), 2);
            assert_eq!(controller.playback_queue.current_index, Some(1));
            assert_eq!(
                controller.playback_queue.items.len(),
                2 + usize::from(queued)
            );
            assert_eq!(
                controller.playback_queue.items[0].playback_location,
                "https://archive.org/download/fixture/1.mp4"
            );
            assert_eq!(
                controller.playback_queue.items[1].playback_location,
                "https://archive.org/download/fixture/2.ogg"
            );
            if queued {
                assert_eq!(
                    controller.playback_queue.items[2].playback_location,
                    "https://archive.org/download/fixture/1.mp4"
                );
            }
        }
    }
}

/// A manual backward choice retains its insertion point when EOF consumes the cursor.
#[test]
fn archive_playback_manual_backward_choice_keeps_insertion_point_across_eof() {
    let (mut controller, state) = choice_controller();
    let mut details = (*choices_fixture()).clone();
    details.tracks[1].download_variants.remove(0);
    details.tracks[1].download_url = details.tracks[1].download_variants[0].download_url.clone();
    details.tracks[1].filename = "2.mp3".into();
    let earlier = archive_org::queue_item(&details.item, &details.tracks[0]);
    archive_org::set_playback_test_details(&mut controller, Some(Arc::new(details)));
    controller.view.selected = 1;
    controller.activate_archive_org_selection();
    controller.playback_queue.items.insert(0, earlier);
    controller.playback_queue.current_index = Some(1);
    controller.step_into_source_list(true);
    let generation = controller
        .view
        .archive_playback_choice_popup
        .as_ref()
        .unwrap()
        .generation;
    controller.playback_phase = PlaybackPhase::Playing;
    controller.handle_playback_end(
        PlaybackEnd {
            reason: PlaybackEndReason::Eof,
            error: None,
            file_error: None,
            diagnostic: None,
        },
        Duration::ZERO,
    );
    assert_eq!(controller.playback_queue.current_index, None);
    assert_eq!(state.lock().unwrap().played.len(), 1);
    controller.dispatch(UiAction::SelectArchivePlaybackChoice {
        generation,
        index: 2,
    });
    assert_eq!(controller.playback_queue.current_index, Some(1));
    assert_eq!(controller.playback_queue.items.len(), 3);
    assert_eq!(
        controller.playback_queue.items[1].playback_location,
        "https://archive.org/download/fixture/1.ogg"
    );
    assert_eq!(
        controller.playback_queue.items[2].playback_location,
        "https://archive.org/download/fixture/2.mp3"
    );
    assert_eq!(state.lock().unwrap().played.len(), 2);
}

/// Two admitted audio families with original video and differently sized audio derivatives.
fn choices_fixture() -> Arc<ArchiveOrgItemDetails> {
    let tracks = (1..=2).map(|number| {
        let prefix = format!("https://archive.org/download/fixture/{number}");
        serde_json::json!({
            "filename": format!("{number}.mp4"), "title": format!("Track {number}"),
            "download_url": format!("{prefix}.mp4"), "duration_seconds": 120,
            "waveform_url": format!("https://archive.org/services/img/fixture/{number}.png"),
            "download_variants": [
                {"filename": format!("{number}.mp4"), "download_url": format!("{prefix}.mp4"),
                    "format": "MPEG4", "size_bytes": 104857600, "provenance": "original", "is_video": true},
                {"filename": format!("{number}.mp3"), "download_url": format!("{prefix}.mp3"),
                    "format": "VBR MP3", "size_bytes": 10485760, "provenance": "derivative", "is_video": false},
                {"filename": format!("{number}.ogg"), "download_url": format!("{prefix}.ogg"),
                    "format": "Ogg Vorbis", "size_bytes": 1048576, "provenance": "derivative", "is_video": false}
            ]
        })
    }).collect::<Vec<_>>();
    Arc::new(serde_json::from_value(serde_json::json!({
        "item": {"identifier": "fixture", "title": "Fixture", "description": "Description",
            "webpage_url": "https://archive.org/details/fixture", "collections": [], "topics": [], "languages": []},
        "tracks": tracks, "comments": []
    })).unwrap())
}

/// Opens a selected metadata track without network requests or a real player.
fn choice_controller() -> (AppController, Arc<Mutex<MockPlaybackState>>) {
    let (mut controller, state) = controller_with_mock_statuses([]);
    archive_org::set_playback_test_details(&mut controller, Some(choices_fixture()));
    controller.view.screen = Screen::ArchiveOrg;
    controller.view.selected = 0;
    (controller, state)
}

#[test]
fn archive_playback_asks_with_actual_formats_sizes_and_cancel_starts_nothing() {
    let (mut controller, state) = choice_controller();
    controller.activate_archive_org_selection();
    let popup = controller
        .view
        .archive_playback_choice_popup
        .as_ref()
        .expect("playback chooser");
    assert_eq!(popup.options.len(), 3);
    assert!(popup.options[0].contains("MPEG4") && popup.options[0].contains("100.00 MiB"));
    assert!(popup.options[1].contains("VBR MP3") && popup.options[1].contains("10.00 MiB"));
    assert!(popup.options[2].contains("Ogg Vorbis") && popup.options[2].contains("1.00 MiB"));
    assert!(state.lock().unwrap().played.is_empty());
    assert!(controller.playback_queue.items.is_empty());
    controller.dispatch(UiAction::DismissArchivePlaybackChoice);
    assert!(controller.view.archive_playback_choice_popup.is_none());
    assert!(state.lock().unwrap().played.is_empty());
    assert!(controller.playback_queue.items.is_empty());
}

#[test]
fn archive_playback_confirmation_owns_snapshot_and_rejects_stale_generation() {
    let (mut controller, state) = choice_controller();
    controller.activate_archive_org_selection();
    let stale = controller
        .view
        .archive_playback_choice_popup
        .as_ref()
        .unwrap()
        .generation;
    controller.dispatch(UiAction::DismissArchivePlaybackChoice);
    controller.activate_archive_org_selection();
    let current = controller
        .view
        .archive_playback_choice_popup
        .as_ref()
        .unwrap()
        .generation;
    assert_ne!(current, stale);
    controller.dispatch(UiAction::ConfirmArchivePlaybackChoice(stale));
    controller.dispatch(UiAction::SelectArchivePlaybackChoice {
        generation: current,
        index: 99,
    });
    assert!(state.lock().unwrap().played.is_empty());
    controller.view.selected = 1;
    archive_org::set_playback_test_details(&mut controller, None);
    controller.dispatch(UiAction::SelectArchivePlaybackChoice {
        generation: current,
        index: 2,
    });
    assert_eq!(state.lock().unwrap().played.len(), 1);
    let item = &controller.playback_queue.items[0];
    assert_eq!(
        item.playback_location,
        "https://archive.org/download/fixture/1.ogg"
    );
    assert_eq!(item.media.id.external_id, item.playback_location);
    assert_eq!(item.media.webpage_url.as_str(), item.playback_location);
    assert_eq!(item.media.kind, crate::domain::MediaKind::Audio);
    assert_eq!(
        item.media.thumbnail_url,
        choices_fixture().tracks[0].waveform_url
    );
    let snapshot = playlist_snapshot_from_queue_item(item).unwrap();
    assert_eq!(snapshot.replay_locator, item.playback_location);
    controller.dispatch(UiAction::ConfirmArchivePlaybackChoice(current));
    assert_eq!(state.lock().unwrap().played.len(), 1);
}

#[test]
fn archive_playback_preferences_choose_original_or_ranked_audio_not_smallest() {
    for (preference, extension) in [
        (ArchivePlaybackPreference::OriginalFile, "mp4"),
        (ArchivePlaybackPreference::AudioOnly, "mp3"),
    ] {
        let (mut controller, state) = choice_controller();
        controller.config.playback.archive_format = preference;
        controller.config.downloads.archive_format =
            crate::config::ArchiveDownloadPreference::OriginalFile;
        controller.activate_archive_org_selection();
        assert!(controller.view.archive_playback_choice_popup.is_none());
        assert_eq!(state.lock().unwrap().played.len(), 1);
        assert_eq!(
            controller.playback_queue.items[0].playback_location,
            format!("https://archive.org/download/fixture/1.{extension}")
        );
    }
}

#[test]
fn archive_playback_audio_choice_is_retained_for_autoplay_despite_preference_changes() {
    let (mut controller, _) = choice_controller();
    controller.activate_archive_org_selection();
    let generation = controller
        .view
        .archive_playback_choice_popup
        .as_ref()
        .unwrap()
        .generation;
    controller.dispatch(UiAction::SelectArchivePlaybackChoice {
        generation,
        index: 2,
    });
    controller.config.playback.archive_format = ArchivePlaybackPreference::OriginalFile;
    let origin = controller.current_autoplay_origin.clone().unwrap();
    controller.continue_autoplay(origin);
    assert!(controller.view.archive_playback_choice_popup.is_none());
    assert_eq!(
        controller
            .playback_queue
            .items
            .last()
            .unwrap()
            .playback_location,
        "https://archive.org/download/fixture/2.mp3"
    );
}

#[test]
fn archive_playback_unambiguous_audio_then_mixed_track_asks_at_eof() {
    let (mut controller, state) = choice_controller();
    let mut details = (*choices_fixture()).clone();
    let first = &mut details.tracks[0];
    first.download_variants.remove(0);
    first.filename = "1.mp3".into();
    first.download_url = first.download_variants[0].download_url.clone();
    archive_org::set_playback_test_details(&mut controller, Some(Arc::new(details)));
    controller.activate_archive_org_selection();
    assert!(controller.view.archive_playback_choice_popup.is_none());
    assert_eq!(state.lock().unwrap().played.len(), 1);
    controller.config.playback.autoplay = true;
    controller.playback_phase = PlaybackPhase::Playing;
    controller.handle_playback_end(
        PlaybackEnd {
            reason: PlaybackEndReason::Eof,
            error: None,
            file_error: None,
            diagnostic: None,
        },
        Duration::ZERO,
    );
    assert!(controller.view.archive_playback_choice_popup.is_some());
    assert_eq!(state.lock().unwrap().played.len(), 1);
    controller.dispatch(UiAction::DismissArchivePlaybackChoice);
    assert!(controller.current_autoplay_origin.is_none());
    assert_eq!(state.lock().unwrap().played.len(), 1);
}

#[test]
fn archive_playback_audio_only_does_not_fall_back_to_video() {
    let (mut controller, state) = choice_controller();
    let mut details = (*choices_fixture()).clone();
    details.tracks[0].download_variants.truncate(1);
    archive_org::set_playback_test_details(&mut controller, Some(Arc::new(details)));
    controller.config.playback.archive_format = ArchivePlaybackPreference::AudioOnly;
    controller.activate_archive_org_selection();
    assert!(state.lock().unwrap().played.is_empty());
    assert!(controller.playback_queue.items.is_empty());
    assert!(controller.view.status_line.contains("audio-only"));
}

#[test]
fn archive_playback_original_preference_uses_inventory_in_older_audio_default_snapshot() {
    let (mut controller, state) = choice_controller();
    let mut details = (*choices_fixture()).clone();
    details.tracks[0].download_url = details.tracks[0].download_variants[1].download_url.clone();
    details.tracks[0].filename = "1.mp3".into();
    archive_org::set_playback_test_details(&mut controller, Some(Arc::new(details)));
    controller.config.playback.archive_format = ArchivePlaybackPreference::OriginalFile;
    controller.activate_archive_org_selection();
    assert_eq!(state.lock().unwrap().played.len(), 1);
    assert_eq!(
        controller.playback_queue.items[0].playback_location,
        "https://archive.org/download/fixture/1.mp4"
    );
}

#[test]
fn archive_playback_queue_edge_inserts_only_after_confirmation() {
    let (mut controller, state) = choice_controller();
    let mut details = (*choices_fixture()).clone();
    details.tracks[0].download_variants.remove(0);
    details.tracks[0].download_url = details.tracks[0].download_variants[0].download_url.clone();
    details.tracks[0].filename = "1.mp3".into();
    archive_org::set_playback_test_details(&mut controller, Some(Arc::new(details)));
    controller.activate_archive_org_selection();
    controller.step_into_source_list(false);
    let generation = controller
        .view
        .archive_playback_choice_popup
        .as_ref()
        .unwrap()
        .generation;
    assert_eq!(controller.playback_queue.items.len(), 1);
    assert_eq!(state.lock().unwrap().played.len(), 1);
    controller.dispatch(UiAction::MoveArchivePlaybackChoice(1));
    controller.dispatch(UiAction::ConfirmArchivePlaybackChoice(generation));
    assert_eq!(state.lock().unwrap().played.len(), 2);
    assert_eq!(controller.playback_queue.items.len(), 2);
    assert_eq!(controller.playback_queue.current_index, Some(1));
    assert_eq!(
        controller.playback_queue.items[1].playback_location,
        "https://archive.org/download/fixture/2.mp3"
    );
}

#[test]
fn archive_playback_new_source_revokes_pending_confirmation() {
    let (mut controller, state) = choice_controller();
    controller.activate_archive_org_selection();
    let generation = controller
        .view
        .archive_playback_choice_popup
        .as_ref()
        .unwrap()
        .generation;
    let details = choices_fixture();
    controller.play_queue_item(
        archive_org::queue_item(&details.item, &details.tracks[1]),
        false,
    );
    assert!(controller.view.archive_playback_choice_popup.is_none());
    controller.dispatch(UiAction::ConfirmArchivePlaybackChoice(generation));
    assert_eq!(state.lock().unwrap().played.len(), 1);
}

#[test]
fn archive_playback_preferences_are_drafted_and_saved_independently_of_downloads() {
    let directory = crate::test_support::canonical_tempdir("archive playback preferences");
    let (mut controller, _) = choice_controller();
    controller.config = Config::for_dir(directory.path().join("config"));
    controller.config.downloads.archive_format =
        crate::config::ArchiveDownloadPreference::ArchiveMp3;
    controller.open_preferences();
    assert_eq!(
        controller
            .view
            .preferences_popup
            .as_ref()
            .unwrap()
            .archive_playback_preference,
        ArchivePlaybackPreference::AskEachTime
    );
    controller.dispatch(UiAction::CycleArchivePlaybackPreference);
    controller.dispatch(UiAction::DismissPreferences);
    assert_eq!(
        controller.config.playback.archive_format,
        ArchivePlaybackPreference::AskEachTime
    );
    controller.open_preferences();
    controller.dispatch(UiAction::CycleArchivePlaybackPreference);
    assert_eq!(
        controller.config.playback.archive_format,
        ArchivePlaybackPreference::AskEachTime
    );
    assert_eq!(
        controller
            .view
            .preferences_popup
            .as_ref()
            .unwrap()
            .archive_playback_preference,
        ArchivePlaybackPreference::OriginalFile
    );
    controller.submit_preferences();
    assert!(controller.view.preferences_popup.is_none());
    assert_eq!(
        controller.config.playback.archive_format,
        ArchivePlaybackPreference::OriginalFile
    );
    assert_eq!(
        controller.config.downloads.archive_format,
        crate::config::ArchiveDownloadPreference::ArchiveMp3
    );
    assert!(
        std::fs::read_to_string(controller.config.config_file())
            .unwrap()
            .contains("archive_format = \"original-file\"")
    );
    controller.open_preferences();
    assert_eq!(
        controller
            .view
            .preferences_popup
            .as_ref()
            .unwrap()
            .archive_playback_preference,
        ArchivePlaybackPreference::OriginalFile
    );
}
