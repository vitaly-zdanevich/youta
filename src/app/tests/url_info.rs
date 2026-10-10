//! On-demand lookup regressions, using no public network or browser.

use super::*;

/// Holds the sole worker open so scheduling/cancellation tests never touch the network.
fn hold_site_file_worker(
    controller: &mut AppController,
    url: &str,
) -> (
    crossbeam_channel::Sender<Result<crate::url_info::site_files::SiteFile, String>>,
    Arc<AtomicBool>,
) {
    let (sender, receiver) = bounded(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    controller.url_info.site_file_worker = Some(super::super::url_info::SiteFileWorker {
        url: url::Url::parse(url).unwrap(),
        cancelled: Arc::clone(&cancelled),
        receiver,
    });
    (sender, cancelled)
}

#[test]
fn site_files_require_explicit_open_and_close_ignores_late_response() {
    let mut controller = url_info_controller();
    let origin = "https://example.com/page";
    open_cached_url_info(&mut controller, origin, "Title: cached");
    assert!(controller.view.site_file_popup.is_none());
    assert!(controller.url_info.site_file_worker.is_none());
    let (sender, cancelled) = hold_site_file_worker(&mut controller, "https://example.com/old.txt");
    controller.dispatch(UiAction::OpenUrlRobots(0));
    let popup = controller
        .view
        .site_file_popup
        .as_ref()
        .expect("explicit root file");
    assert_eq!(popup.url, "https://example.com/robots.txt");
    assert!(popup.loading);
    assert!(cancelled.load(AtomicOrdering::Relaxed));
    controller.dispatch(UiAction::DismissSiteFile);
    sender.send(Err("late old completion".into())).unwrap();
    controller.refresh_url_info();
    assert!(controller.view.site_file_popup.is_none());
    assert!(controller.url_info.site_file_worker.is_none());
}

#[test]
fn site_file_child_back_restores_selection_and_scroll_without_fetching() {
    let mut controller = url_info_controller();
    let parent = crate::view::SiteFilePopupView {
        title: "Sitemap".into(),
        url: "https://example.com/sitemap.xml".into(),
        sitemap: true,
        sitemap_index: true,
        selected: 1,
        scroll_offset: 1,
        entries: (0..3)
            .map(|index| crate::view::SiteFileEntryView {
                url: format!("https://example.com/child{index}.xml"),
                metadata: vec![("Last modified".into(), "2026-10-10".into())],
            })
            .collect(),
        ..crate::view::SiteFilePopupView::default()
    };
    controller.view.site_file_popup = Some(parent.clone());
    let (_sender, cancelled) =
        hold_site_file_worker(&mut controller, "https://example.com/held.xml");
    controller.dispatch(UiAction::ActivateSiteFileEntry(1));
    assert_eq!(
        controller.view.site_file_popup.as_ref().unwrap().url,
        "https://example.com/child1.xml"
    );
    assert!(
        controller
            .view
            .site_file_popup
            .as_ref()
            .unwrap()
            .can_go_back
    );
    assert!(cancelled.load(AtomicOrdering::Relaxed));
    controller.dispatch(UiAction::BackSiteFile);
    assert_eq!(controller.view.site_file_popup, Some(parent));
    controller.dispatch(UiAction::DismissSiteFile);
}

#[test]
fn site_file_completion_preserves_full_robots_and_structured_sitemap() {
    use crate::url_info::site_files::{SiteFile, SiteFileContent, SitemapEntry};
    let mut controller = url_info_controller();
    for sitemap in [false, true] {
        let url = if sitemap {
            "https://example.com/sitemap.xml"
        } else {
            "https://example.com/robots.txt"
        };
        controller.view.site_file_popup = Some(crate::view::SiteFilePopupView {
            url: url.into(),
            sitemap,
            loading: true,
            ..crate::view::SiteFilePopupView::default()
        });
        let (sender, _) = hold_site_file_worker(&mut controller, url);
        sender
            .send(Ok(SiteFile {
                url: url::Url::parse(url).unwrap(),
                content: if sitemap {
                    SiteFileContent::Sitemap {
                        index: false,
                        entries: vec![SitemapEntry {
                            url: url::Url::parse("https://example.com/page").unwrap(),
                            metadata: vec![("Last modified".into(), "2026-10-10".into())],
                        }],
                    }
                } else {
                    SiteFileContent::Robots("User-agent: *\nDisallow: /private\n".into())
                },
            }))
            .unwrap();
        controller.refresh_url_info();
        let popup = controller.view.site_file_popup.as_ref().unwrap();
        assert!(!popup.loading);
        if sitemap {
            assert_eq!(popup.entries[0].metadata[0].1, "2026-10-10");
        } else {
            assert_eq!(popup.text, "User-agent: *\nDisallow: /private\n");
        }
        assert!(controller.url_info.site_file_worker.is_none());
    }
    #[cfg(not(feature = "web-browser"))]
    {
        controller.dispatch(UiAction::ActivateSiteFileEntry(0));
        assert_eq!(
            controller
                .view
                .site_file_popup
                .as_ref()
                .unwrap()
                .error
                .as_deref(),
            Some("This build does not include the Web tab.")
        );
    }
}

/// Uses a drive-qualified path on Windows without creating or probing a fixture file.
fn url_info_fixture_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("youta-url-info-{name}.mp3"))
}

/// Installs a tagged local item without probing the filesystem.
fn url_info_controller() -> AppController {
    let (mut controller, _) = controller_with_mock_statuses([]);
    let path = url_info_fixture_path("fixture");
    select_url_info_local(
        &mut controller,
        &path,
        "See https://example.com/page. Original comment.",
    );
    controller.refresh_url_info();
    controller
}

/// Changes selection using in-memory rows and tags, without any metadata worker.
fn select_url_info_local(controller: &mut AppController, path: &Path, comment: &str) {
    assert!(
        path.is_absolute(),
        "Local fixtures need native absolute paths"
    );
    assert_eq!(
        local_path_from_media_id(&local_media_id(path)).as_deref(),
        Some(path)
    );
    let mut item = local_media_item_stub(path.to_owned(), Some(10));
    item.comment = Some(comment.into());
    controller.local_results = vec![item];
    controller.view.screen = Screen::Local;
    controller.view.selected = 0;
    controller.local_listing = Some(crate::local_browser::LocalDirectoryListing {
        path: path.parent().unwrap().to_owned(),
        parent: None,
        entries: vec![crate::local_browser::LocalEntry {
            name: path.file_name().unwrap().to_owned(),
            path: path.to_owned(),
            kind: crate::local_browser::LocalEntryKind::Audio,
            size_bytes: Some(10),
            image_dimensions: None,
            directory_identity: None,
        }],
        truncated: false,
        inspected_entries: 1,
    });
    controller.view.details = Some(DetailView {
        media_id: Some(local_media_id(path)),
        description: format!("Comment: {comment}"),
        ..DetailView::default()
    });
}

/// Supplies a completion channel rather than starting a real lookup thread.
fn inject_url_info_worker(
    controller: &mut AppController,
    url: &str,
) -> (
    crossbeam_channel::Sender<(crate::url_info::UrlInfoClient, Vec<String>)>,
    Arc<AtomicBool>,
) {
    assert!(controller.url_info.worker.is_none());
    let (sender, receiver) = bounded(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    controller.url_info.worker = Some(super::super::url_info::LookupWorker {
        url: url::Url::parse(url).unwrap(),
        cancelled: Arc::clone(&cancelled),
        receiver,
    });
    (sender, cancelled)
}

/// Keeps website results in memory so opening the disclosure cannot do I/O.
fn open_cached_url_info(controller: &mut AppController, url: &str, line: &str) {
    controller
        .url_info
        .remember(url::Url::parse(url).unwrap(), vec![line.into()]);
    controller.toggle_url_info(0);
    assert!(controller.url_info.worker.is_none());
}

#[test]
fn url_info_selection_does_not_start_network_and_keeps_comment() {
    let mut controller = url_info_controller();
    let original = controller
        .view
        .details
        .as_ref()
        .unwrap()
        .description
        .clone();
    for _ in 0..5 {
        controller.refresh_url_info();
    }
    assert!(controller.url_info.worker.is_none());
    let details = controller.view.details.as_ref().unwrap();
    assert_eq!(details.description, original);
    assert_eq!(details.url_info.len(), 1);
    assert_eq!(details.url_info[0].url, "https://example.com/page");
    assert!(!details.url_info[0].expanded);
}

#[test]
fn url_info_cached_disclosure_is_explicit_and_survives_detail_rebuild() {
    let mut controller = url_info_controller();
    let url = url::Url::parse("https://example.com/page").unwrap();
    controller
        .url_info
        .remember(url, vec!["Title: Example".into()]);
    controller.toggle_url_info(0);
    assert!(controller.url_info.worker.is_none());
    controller.view.details.as_mut().unwrap().url_info.clear();
    controller.refresh_url_info();
    let info = &controller.view.details.as_ref().unwrap().url_info[0];
    assert!(info.expanded);
    assert_eq!(info.lines, ["Title: Example"]);
    controller.toggle_url_info(0);
    assert!(!controller.view.details.as_ref().unwrap().url_info[0].expanded);
    assert!(
        controller.view.details.as_ref().unwrap().url_info[0]
            .lines
            .is_empty()
    );
    controller.view.screen = Screen::Search;
    controller.refresh_url_info();
    assert!(
        controller
            .view
            .details
            .as_ref()
            .unwrap()
            .url_info
            .is_empty()
    );
}

#[test]
fn url_info_changed_comment_cannot_keep_an_old_disclosure() {
    let mut controller = url_info_controller();
    controller
        .url_info
        .remember(url::Url::parse("https://example.com/page").unwrap(), vec![]);
    controller.toggle_url_info(0);
    controller.local_results[0].comment = Some("https://example.org/new".into());
    controller.refresh_url_info();
    let info = &controller.view.details.as_ref().unwrap().url_info[0];
    assert_eq!(info.url, "https://example.org/new");
    assert!(!info.expanded);
}

#[test]
fn url_info_yields_the_details_body_to_another_disclosure() {
    let mut controller = url_info_controller();
    open_cached_url_info(
        &mut controller,
        "https://example.com/page",
        "Title: Example",
    );
    controller.dispatch(UiAction::ToggleWikidataStatements(0));
    let info = &controller.view.details.as_ref().unwrap().url_info[0];
    assert!(!info.expanded);
    assert!(info.lines.is_empty());
    assert!(controller.url_info.worker.is_none());
}

#[test]
fn url_info_late_local_completion_cannot_cross_selection_or_poison_cache() {
    let mut controller = url_info_controller();
    let url = "https://example.com/page";
    open_cached_url_info(&mut controller, url, "Title: Previously cached");
    let (sender, cancelled) = inject_url_info_worker(&mut controller, url);
    // Queue the response before the next tick: owner validation must happen first.
    sender
        .send((Default::default(), vec!["Title: Stale completion".into()]))
        .unwrap();
    select_url_info_local(
        &mut controller,
        &url_info_fixture_path("other"),
        "See https://example.com/page. Original comment.",
    );
    controller.refresh_url_info();
    assert!(cancelled.load(AtomicOrdering::Relaxed));
    assert!(controller.url_info.worker.is_none());
    let info = &controller.view.details.as_ref().unwrap().url_info[0];
    assert!(!info.expanded);
    assert!(!info.loading);
    assert!(info.lines.is_empty());
    controller.toggle_url_info(0);
    assert_eq!(
        controller.view.details.as_ref().unwrap().url_info[0].lines,
        ["Title: Previously cached"],
    );
    assert!(controller.url_info.worker.is_none());
}

#[test]
fn url_info_new_requests_wait_for_the_single_cancelled_worker() {
    let mut controller = url_info_controller();
    controller.local_results[0].comment =
        Some("https://example.com/page http://127.0.0.1/second http://127.0.0.1/third".into());
    controller.refresh_url_info();
    open_cached_url_info(&mut controller, "https://example.com/page", "Title: Cached");
    let (sender, cancelled) = inject_url_info_worker(&mut controller, "https://example.com/page");
    for index in [1, 2] {
        controller.toggle_url_info(index);
        let worker = controller.url_info.worker.as_ref().unwrap();
        assert!(Arc::ptr_eq(&worker.cancelled, &cancelled));
        assert!(cancelled.load(AtomicOrdering::Relaxed));
        let entries = &controller.view.details.as_ref().unwrap().url_info;
        assert_eq!(entries.iter().filter(|entry| entry.expanded).count(), 1);
        assert!(entries[index].loading);
    }
    // Closing the pending disclosure must also prevent its deferred request.
    controller.toggle_url_info(2);
    sender
        .send((Default::default(), vec!["Title: Cancelled result".into()]))
        .unwrap();
    controller.refresh_url_info();
    assert!(controller.url_info.worker.is_none());
    assert!(
        controller
            .view
            .details
            .as_ref()
            .unwrap()
            .url_info
            .iter()
            .all(|entry| !entry.expanded
                && !entry.loading
                && !entry.lines.iter().any(|line| line.contains("Cancelled")))
    );
    controller.toggle_url_info(0);
    assert_eq!(
        controller.view.details.as_ref().unwrap().url_info[0].lines,
        ["Title: Cached"],
    );
    assert!(controller.url_info.worker.is_none());
}

#[test]
fn url_info_completed_results_are_cached_by_url_but_never_auto_expanded() {
    let mut controller = url_info_controller();
    let url = "https://example.com/page";
    open_cached_url_info(&mut controller, url, "Title: Old cache entry");
    let (sender, cancelled) = inject_url_info_worker(&mut controller, url);
    sender
        .send((Default::default(), vec!["Title: Fresh result".into()]))
        .unwrap();
    controller.refresh_url_info();
    assert!(!cancelled.load(AtomicOrdering::Relaxed));
    assert_eq!(
        controller.view.details.as_ref().unwrap().url_info[0].lines,
        ["Title: Fresh result"],
    );
    select_url_info_local(
        &mut controller,
        &url_info_fixture_path("cache-reuse"),
        "https://example.com/page#another-fragment",
    );
    controller.refresh_url_info();
    assert!(!controller.view.details.as_ref().unwrap().url_info[0].expanded);
    assert!(controller.url_info.worker.is_none());
    controller.toggle_url_info(0);
    assert_eq!(
        controller.view.details.as_ref().unwrap().url_info[0].lines,
        ["Title: Fresh result"],
    );
    assert!(controller.url_info.worker.is_none());
}

#[test]
fn url_info_youtube_description_is_explicit_and_rejects_late_selection_results() {
    let (mut controller, _) = controller_with_mock_statuses([]);
    let first_id = MediaId::new(SourceKind::YouTube, "first-video");
    let second_id = MediaId::new(SourceKind::YouTube, "second-video");
    let description = "Description: https://example.com/page Original text.";
    controller.view.screen = Screen::Search;
    controller.view.selected = 0;
    controller.view.rows = vec![RowView {
        media_id: Some(first_id.clone()),
        ..RowView::default()
    }];
    controller.view.details = Some(DetailView {
        media_id: Some(first_id),
        description: description.into(),
        ..DetailView::default()
    });
    controller.refresh_url_info();
    let info = &controller.view.details.as_ref().unwrap().url_info[0];
    assert_eq!(info.url, "https://example.com/page");
    assert!(!info.expanded);
    assert!(controller.url_info.worker.is_none());
    open_cached_url_info(&mut controller, "https://example.com/page", "Title: Cached");
    let (sender, cancelled) = inject_url_info_worker(&mut controller, "https://example.com/page");
    controller.view.rows[0].media_id = Some(second_id.clone());
    controller.view.details.as_mut().unwrap().media_id = Some(second_id);
    sender
        .send((
            Default::default(),
            vec!["Title: Stale YouTube result".into()],
        ))
        .unwrap();
    controller.refresh_url_info();
    assert!(cancelled.load(AtomicOrdering::Relaxed));
    assert!(controller.url_info.worker.is_none());
    let details = controller.view.details.as_ref().unwrap();
    assert_eq!(details.description, description);
    assert!(!details.url_info[0].expanded);
    assert!(details.url_info[0].lines.is_empty());
}
