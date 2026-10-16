//! Offline regression coverage for lazy, selection-owned VK album metadata.

use super::*;

/// Supplies deterministic album candidates without opening files or starting requests.
fn controller() -> AppController {
    let mut app = AppController::new(
        Config::for_dir("/tmp/youta-vk-album-controller-fixture"),
        StateStore::open_in_memory().unwrap(),
        None,
        None,
    );
    app.diagnostic_only = true;
    app.show_screen(Screen::Vk);
    app.config.providers.vk_cookies_file = Some(PathBuf::from("/tmp/vk-album-fixture-cookies"));
    app.vk.cookie_file = app.config.providers.vk_cookies_file.clone();
    app.vk.items = (1..=2)
        .map(|id| {
            VkTrack::fixture_with_album(
                VkAudioId::new(7, id).unwrap(),
                "Fixture track",
                "Fixture artist",
            )
        })
        .collect();
    app.refresh_vk_rows();
    app.update_vk_detail();
    app
}

/// Selection, source, editing, and attempted state all gate background work.
#[test]
fn album_lookup_only_targets_an_unresolved_selected_vk_track() {
    let mut app = controller();
    assert!(
        app.selected_vk_album_job().is_none(),
        "diagnostics never fetch albums"
    );
    app.diagnostic_only = false;
    let job = app.selected_vk_album_job().unwrap();
    assert_eq!(job.track.id, app.vk.items[0].id);
    app.vk.album_attempted.insert(job.track.id);
    assert!(app.selected_vk_album_job().is_none());
    app.view.selected = 1;
    assert!(app.selected_vk_album_job().is_some());
    app.view.search_editing = true;
    assert!(app.selected_vk_album_job().is_none());
    app.view.search_editing = false;
    app.view.screen = Screen::Local;
    assert!(app.selected_vk_album_job().is_none());
    app.view.screen = Screen::Vk;
    app.vk.items[1].album_title = Some("Known album".to_owned());
    assert!(app.selected_vk_album_job().is_none());
    app.vk.items[1] = VkTrack::fixture(VkAudioId::new(7, 2).unwrap(), "No album", "Artist");
    assert!(app.selected_vk_album_job().is_none());
    app.view.selected = app.vk.items.len();
    assert!(
        app.selected_vk_album_job().is_none(),
        "load-more has no album"
    );
}

/// A late reply may enrich its own row but must never repaint another selection.
#[test]
fn album_reply_updates_only_matching_current_generation_metadata() {
    let mut app = controller();
    let track = app.vk.items[0].clone();
    app.retain_vk_track(track.clone());
    let job = AlbumJob {
        generation: app.vk.generation,
        track: Box::new(track),
    };
    app.view.selected = 1;
    app.update_vk_detail();
    let details = app.view.details.clone();
    app.apply_vk_album(
        job.clone(),
        Ok(Reply::Album(Some("Fixture album".to_owned()))),
    );
    assert_eq!(app.view.details, details);
    assert_eq!(
        app.vk.items[0].album_title.as_deref(),
        Some("Fixture album")
    );
    assert_eq!(
        app.vk.retained[&job.track.id].album_title.as_deref(),
        Some("Fixture album")
    );
    app.view.selected = 0;
    app.update_vk_detail();
    assert_eq!(
        app.view.details.as_ref().unwrap().links[1].label,
        "Fixture album"
    );
    app.vk.generation += 1;
    let details = app.view.details.clone();
    app.apply_vk_album(job, Ok(Reply::Album(Some("Obsolete album".to_owned()))));
    assert_eq!(app.view.details, details);
    assert_eq!(
        app.vk.items[0].album_title.as_deref(),
        Some("Fixture album")
    );
}

/// Album failures are optional metadata failures, not playback errors or retry loops.
#[test]
fn missing_or_failed_album_lookup_leaves_details_and_playback_status_unchanged() {
    let mut app = controller();
    let job = AlbumJob {
        generation: app.vk.generation,
        track: Box::new(app.vk.items[0].clone()),
    };
    app.vk.album_attempted.insert(job.track.id);
    app.view.status_line = "Playing".to_owned();
    let details = app.view.details.clone();
    app.apply_vk_album(job.clone(), Err("fixture failure".to_owned()));
    app.apply_vk_album(job, Ok(Reply::Album(None)));
    assert_eq!(app.view.details, details);
    assert_eq!(app.view.status_line, "Playing");
    app.diagnostic_only = false;
    assert!(app.selected_vk_album_job().is_none());
}

/// Hidden tabs keep their own Details when a previously selected album finishes.
#[test]
fn album_completion_does_not_replace_a_different_tabs_details() {
    let mut app = controller();
    let job = AlbumJob {
        generation: app.vk.generation,
        track: Box::new(app.vk.items[0].clone()),
    };
    app.show_screen(Screen::Local);
    let details = app.view.details.clone();
    app.apply_vk_album(job, Ok(Reply::Album(Some("Fixture album".to_owned()))));
    assert_eq!(app.view.details, details);
}

/// Automatic metadata must not reuse an old authenticated client after a path change.
#[test]
fn idle_album_poll_revokes_old_cookie_owner_and_visible_metadata() {
    let mut app = controller();
    app.vk.album_attempted.insert(app.vk.items[0].id);
    let generation = app.vk.generation;
    let cookie_generation = app.vk.cookie_generation;
    let path = PathBuf::from("/tmp/vk-album-replacement-fixture-cookies");
    app.config.providers.vk_cookies_file = Some(path.clone());
    app.poll_vk_worker();
    assert_eq!(app.vk.cookie_file, Some(path));
    assert!(app.vk.generation > generation);
    assert!(app.vk.cookie_generation > cookie_generation);
    assert!(app.vk.items.is_empty());
    assert!(app.vk.album_attempted.is_empty());
    assert!(app.view.rows.is_empty());
    assert!(app.view.details.is_none());
    assert!(app.vk.worker.is_none());
}

/// A published old-session album cannot survive replacing the cookie-file owner.
#[test]
fn album_poll_discards_completion_after_cookie_path_changes() {
    let mut app = controller();
    let job = AlbumJob {
        generation: app.vk.generation,
        track: Box::new(app.vk.items[0].clone()),
    };
    let (sender, response) = bounded(1);
    sender
        .send(Completion {
            client: None,
            result: Ok(Reply::Album(Some("Old-session album".to_owned()))),
        })
        .unwrap();
    app.vk.worker = Some(Worker {
        job: Job::Album(job),
        cookie_file: app.vk.cookie_file.clone().unwrap(),
        cookie_generation: app.vk.cookie_generation,
        response,
        thread: thread::spawn(|| {}),
    });
    app.config.providers.vk_cookies_file = Some(PathBuf::from("/tmp/vk-new-album-session"));
    app.poll_vk_worker();
    assert!(app.vk.worker.is_none());
    assert!(app.vk.client.is_none());
    assert!(app.vk.items.is_empty());
    assert!(app.view.details.is_none());
}

/// Published metadata flows through the notified lane without blocking or triggering playback.
#[test]
fn album_poll_projects_success_for_the_still_selected_track() {
    let mut app = controller();
    let job = AlbumJob {
        generation: app.vk.generation,
        track: Box::new(app.vk.items[0].clone()),
    };
    app.vk.album_attempted.insert(job.track.id);
    let (sender, response) = bounded(1);
    sender
        .send(Completion {
            client: None,
            result: Ok(Reply::Album(Some("Fixture album".to_owned()))),
        })
        .unwrap();
    app.vk.worker = Some(Worker {
        job: Job::Album(job),
        cookie_file: app.vk.cookie_file.clone().unwrap(),
        cookie_generation: app.vk.cookie_generation,
        response,
        thread: thread::spawn(|| {}),
    });
    app.poll_vk_worker();
    assert_eq!(
        app.view.details.as_ref().unwrap().links[1].label,
        "Fixture album"
    );
    assert!(!app.vk_playback_pending());
    assert!(app.vk.worker.is_none());
}
