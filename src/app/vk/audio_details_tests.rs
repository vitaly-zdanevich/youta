//! Offline regression coverage for playback-owned VK technical metadata.

use super::*;

/// Supplies observations without a subprocess, credentials or network requests.
struct AudioPlayer(PlaybackStatus);

impl PlaybackBackend for AudioPlayer {
    fn play(&mut self, _: &PlaybackInput) -> crate::playback::Result<()> {
        Ok(())
    }
    fn command(&mut self, _: PlayerCommand) -> crate::playback::Result<()> {
        Ok(())
    }
    fn status(&mut self) -> crate::playback::Result<PlaybackStatus> {
        Ok(self.0.clone())
    }
    fn shutdown(&mut self) -> crate::playback::Result<()> {
        Ok(())
    }
}

/// Keeps the config directory alive and supplies two distinct recordings with the same title.
fn controller() -> (tempfile::TempDir, AppController) {
    let directory = tempfile::tempdir().unwrap();
    let mut app = AppController::new(
        Config::for_dir(directory.path()),
        StateStore::open_in_memory().unwrap(),
        None,
        None,
    );
    app.diagnostic_only = true;
    app.view.screen = Screen::Vk;
    app.vk.items = [1, 2]
        .into_iter()
        .map(|id| {
            VkTrack::fixture_with_album(VkAudioId::new(7, id).unwrap(), "Same title", "Artist")
        })
        .collect();
    app.current_media = Some(media_id(app.vk.items[0].id));
    app.view.playing_media_id = app.current_media.clone();
    app.playback_phase = PlaybackPhase::Playing;
    app.view.playback = status();
    app.update_vk_detail();
    (directory, app)
}

/// Uses decimal megabytes, not MiB, and a packet-measured bitrate in bits per second.
fn status() -> PlaybackStatus {
    PlaybackStatus {
        idle: false,
        audio_codec: Some("mp3".to_owned()),
        audio_bitrate_bps: Some(192_000),
        file_size_bytes: Some(4_300_000),
        ..PlaybackStatus::default()
    }
}

/// The requested compact line contains no redundant heading.
#[test]
fn vk_audio_details_compact_line_for_matching_playing_track() {
    let (_directory, app) = controller();
    let details = app.view.details.as_ref().unwrap();
    assert_eq!(details.description, "MP3 192 kb/s, 4,3 MB");
    assert_eq!(
        details.links[0].url,
        app.vk.items[0].id.webpage_url().as_str()
    );
    assert!(details.links.iter().any(|link| matches!(
        link.internal_target,
        Some(DetailLinkInternalTarget::VkAlbum(_))
    )));
}

/// Pausing retains the loaded audio's information; absent values have no placeholders.
#[test]
fn vk_audio_details_omit_unknown_fields_and_remain_when_paused() {
    let (_directory, mut app) = controller();
    app.view.playback.paused = true;
    app.view.playback.file_size_bytes = None;
    app.update_vk_detail();
    assert_eq!(
        app.view.details.as_ref().unwrap().description,
        "MP3 192 kb/s"
    );
    app.view.playback.audio_codec = None;
    app.update_vk_detail();
    assert_eq!(app.view.details.as_ref().unwrap().description, "192 kb/s");
    app.view.playback.audio_bitrate_bps = None;
    app.update_vk_detail();
    assert!(app.view.details.as_ref().unwrap().description.is_empty());
}

/// Neither title matches nor equal external IDs from another provider grant ownership.
#[test]
fn vk_audio_details_never_borrow_another_tracks_metadata() {
    let (_directory, mut app) = controller();
    app.view.selected = 1;
    app.update_vk_detail();
    assert!(app.view.details.as_ref().unwrap().description.is_empty());
    app.view.selected = 0;
    app.update_vk_detail();
    assert_eq!(
        app.view.details.as_ref().unwrap().description,
        "MP3 192 kb/s, 4,3 MB"
    );
    app.current_media.as_mut().unwrap().source = SourceKind::YouTube;
    app.update_vk_detail();
    assert!(app.view.details.as_ref().unwrap().description.is_empty());
}

/// New observations update only the technical text, preserving selected-item enrichment.
#[test]
fn vk_audio_details_refresh_from_player_status_without_rebuilding_details() {
    let (_directory, mut app) = controller();
    app.view
        .details
        .as_mut()
        .unwrap()
        .links
        .push(DetailLinkView {
            label: "Enriched link".to_owned(),
            url: "https://example.org/recording".to_owned(),
            ..DetailLinkView::default()
        });
    let links = app.view.details.as_ref().unwrap().links.clone();
    app.player = Some(Box::new(AudioPlayer(PlaybackStatus {
        audio_codec: Some("opus".to_owned()),
        audio_bitrate_bps: Some(128_000),
        file_size_bytes: None,
        ..status()
    })));
    app.update_player();
    assert_eq!(
        app.view.details.as_ref().unwrap().description,
        "Opus 128 kb/s"
    );
    assert_eq!(app.view.details.as_ref().unwrap().links, links);
}

/// Old observations are unavailable while replacing a source, after stop, or while idle.
#[test]
fn vk_audio_details_clear_at_playback_boundaries() {
    let (_directory, mut app) = controller();
    app.playback_phase = PlaybackPhase::Loading;
    app.update_vk_detail();
    assert!(app.view.details.as_ref().unwrap().description.is_empty());
    app.playback_phase = PlaybackPhase::Playing;
    app.view.playback.idle = true;
    app.update_vk_detail();
    assert!(app.view.details.as_ref().unwrap().description.is_empty());
    app.view.playback = status();
    app.update_vk_detail();
    assert!(!app.view.details.as_ref().unwrap().description.is_empty());
    app.reset_playback_state();
    assert!(app.view.details.as_ref().unwrap().description.is_empty());
}

/// Album summaries and another tab's descriptions are not playback metadata destinations.
#[test]
fn vk_audio_details_leave_album_and_other_provider_descriptions_intact() {
    let (_directory, mut app) = controller();
    let mut album = VkAlbum::fixture(7, 3, "Album", "Artist");
    album.year = Some(2024);
    app.vk.albums = vec![album];
    app.vk.results_kind = VkSearchKind::Albums;
    app.update_vk_detail();
    let original = app.view.details.clone();
    app.player = Some(Box::new(AudioPlayer(status())));
    app.update_player();
    assert_eq!(app.view.details, original);
    app.view.screen = Screen::Search;
    app.view.details = Some(DetailView {
        description: "An unrelated description".to_owned(),
        ..DetailView::default()
    });
    let original = app.view.details.clone();
    app.update_player();
    assert_eq!(app.view.details, original);
}

/// Returning through Details history cannot resurrect observations from stopped audio.
#[test]
fn vk_audio_details_refresh_restored_navigation_snapshot() {
    let (_directory, mut app) = controller();
    let snapshot = app.take_detail_navigation_snapshot();
    app.reset_playback_state();
    app.restore_detail_navigation_snapshot(snapshot);
    assert!(app.view.details.as_ref().unwrap().description.is_empty());
}

/// A linked document remains untouched even while its parent track is selected in VK.
#[test]
fn vk_audio_details_do_not_overwrite_a_linked_document() {
    let (_directory, mut app) = controller();
    app.view.details.as_mut().unwrap().media_id = Some(media_id(app.vk.items[1].id));
    app.view.details.as_mut().unwrap().description = "Linked document".to_owned();
    app.player = Some(Box::new(AudioPlayer(status())));
    app.update_player();
    assert_eq!(
        app.view.details.as_ref().unwrap().description,
        "Linked document"
    );
}

/// Inactive backend snapshots clear technical values without ending the queue implicitly.
#[test]
fn vk_audio_details_clear_on_an_idle_backend_snapshot() {
    let (_directory, mut app) = controller();
    app.player = Some(Box::new(AudioPlayer(PlaybackStatus::default())));
    app.update_player();
    assert!(app.view.details.as_ref().unwrap().description.is_empty());
    assert_eq!(app.playback_phase, PlaybackPhase::Playing);
}

/// Sizes use decimal SI units and bitrate rounds packet measurements for a compact display.
#[test]
fn vk_audio_details_format_partial_sizes_without_extra_separators() {
    for (bytes, expected) in [
        (0, ""),
        (999, "999 B"),
        (1_000, "1,0 kB"),
        (4_300_000, "4,3 MB"),
        (1_500_000_000, "1,5 GB"),
        (u64::MAX, "18,4 EB"),
    ] {
        assert_eq!(
            compact_audio_description(&PlaybackStatus {
                file_size_bytes: Some(bytes),
                ..PlaybackStatus::default()
            }),
            expected
        );
    }
    assert_eq!(
        compact_audio_description(&PlaybackStatus {
            audio_bitrate_bps: Some(191_997),
            ..PlaybackStatus::default()
        }),
        "192 kb/s"
    );
}
