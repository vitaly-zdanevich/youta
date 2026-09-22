//! Viewport-sized search regressions using only an in-memory Archive transport.

use super::*;
use crate::providers::ProviderError;
use crate::providers::archive_org::ArchiveOrgTransport;

/// Produces consecutive identifiers from the actual HTTP pagination parameters.
struct SearchTransport;

impl ArchiveOrgTransport for SearchTransport {
    fn fetch(&self, url: &url::Url, _: usize) -> Result<Vec<u8>, ProviderError> {
        assert_eq!(url.path(), "/advancedsearch.php");
        let pairs = url.query_pairs().collect::<HashMap<_, _>>();
        let rows = pairs["rows"].parse::<usize>().unwrap();
        let page = pairs["page"].parse::<usize>().unwrap();
        let start = (page - 1) * rows;
        let docs = (start..start + rows)
            .map(|index| {
                serde_json::json!({
                    "identifier": format!("item-{index}"),
                    "title": format!("Item {index}"),
                    "mediatype": "audio"
                })
            })
            .collect::<Vec<_>>();
        Ok(serde_json::to_vec(&serde_json::json!({
            "response": {"numFound": 1000, "start": start, "docs": docs}
        }))
        .unwrap())
    }
}

/// Installs a mock provider and blocks automatic workers for deterministic responses.
fn controller() -> (tempfile::TempDir, AppController) {
    let (temporary, mut app) = super::tests::lookup_controller();
    app.view.screen = Screen::ArchiveOrg;
    app.archive_org.client = ArchiveOrgClient::with_transport(Arc::new(SearchTransport));
    super::tests::occupy_archive_worker(&mut app);
    (temporary, app)
}

/// Reads the exact request owned by the current search, not a stale worker.
fn request(app: &AppController) -> ArchiveOrgSearchRequest {
    let ArchiveRequest::Search(request) = &app.archive_org.pending.as_ref().unwrap().kind else {
        panic!("expected search request");
    };
    request.clone()
}

/// Runs the real provider URL/response path against mock metadata without HTTP.
fn answer(app: &mut AppController) {
    let job = app.archive_org.pending.clone().unwrap();
    let page = app.archive_org.client.search(&request(app)).unwrap();
    app.handle_archive_response(job, Ok(ArchiveResponse::Search(page)));
}

/// Small and tall terminals request only their available slots, with bounded extremes.
#[test]
fn archive_search_page_capacity_controls_first_http_request() {
    for (capacity, expected) in [(10, 10), (100, 100), (0, 1), (usize::MAX, 100)] {
        let (_temporary, mut app) = controller();
        app.set_archive_org_search_page_capacity(capacity);
        assert!(
            app.archive_org.pending.is_none(),
            "geometry alone must not fetch"
        );
        app.submit_archive_org_search("subway".into());
        assert_eq!(request(&app).page, 1);
        assert_eq!(request(&app).limit, expected);
        answer(&mut app);
        assert_eq!(app.archive_org.items.len(), expected);
        assert_eq!(app.view.rows.len(), expected + 1);
        assert_eq!(app.view.rows.last().unwrap().title, "Load more items…");
    }
}

/// Explicit continuation moves by one result viewport, keeping every new row visible.
#[test]
fn archive_load_more_turns_one_screen_and_selects_its_last_row() {
    let (_temporary, mut app) = controller();
    app.set_archive_org_search_page_capacity(10);
    app.submit_archive_org_search("subway".into());
    answer(&mut app);
    for page in 2..=4 {
        app.view.selected = app.archive_org.items.len();
        app.archive_org_selected = app.view.selected;
        app.activate_archive_org_selection();
        assert_eq!((request(&app).page, request(&app).limit), (page, 10));
        answer(&mut app);
        let loaded = usize::try_from(page).unwrap() * 10;
        assert_eq!(
            app.view.selected, loaded,
            "focus the new continuation at the screen bottom"
        );
        assert_eq!(app.archive_org_selected, loaded);
        assert_eq!(app.view.rows[loaded].title, "Load more items…");
        assert_eq!(
            app.archive_org.items[loaded - 10].identifier,
            format!("item-{}", loaded - 10)
        );
        assert_eq!(
            app.archive_org.items[loaded - 1].identifier,
            format!("item-{}", loaded - 1)
        );
    }
}

/// A response must not drag the viewport away after the user deliberately moves back.
#[test]
fn archive_load_more_does_not_steal_changed_selection() {
    let (_temporary, mut app) = controller();
    app.set_archive_org_search_page_capacity(10);
    app.submit_archive_org_search("subway".into());
    answer(&mut app);
    app.view.selected = 10;
    app.archive_org_selected = 10;
    app.activate_archive_org_selection();
    app.select_row(2);
    answer(&mut app);
    assert_eq!(app.view.selected, 2);
    assert_eq!(app.archive_org.items.len(), 20);
}

/// Exhausted search pages select their final media row, never a phantom continuation.
#[test]
fn archive_load_more_final_page_selects_last_item_without_load_more() {
    let (_temporary, mut app) = controller();
    app.set_archive_org_search_page_capacity(10);
    app.submit_archive_org_search("subway".into());
    answer(&mut app);
    app.view.selected = 10;
    app.activate_archive_org_selection();
    let job = app.archive_org.pending.clone().unwrap();
    let mut page = app.archive_org.client.search(&request(&app)).unwrap();
    page.total = 20;
    page.next_page = None;
    app.handle_archive_response(job, Ok(ArchiveResponse::Search(page)));
    assert_eq!(app.view.selected, 19);
    assert_eq!(app.archive_org_selected, 19);
    assert_eq!(app.view.rows.len(), 20);
    assert_eq!(app.view.rows.last().unwrap().title, "Item 19");
    assert!(
        app.view
            .rows
            .iter()
            .all(|row| row.title != "Load more items…")
    );
}

/// Resizing affects new searches, never offsets within an existing result set or Back cache.
#[test]
fn archive_search_page_capacity_keeps_continuations_and_back_aligned() {
    let (_temporary, mut app) = controller();
    app.set_archive_org_search_page_capacity(10);
    app.submit_archive_org_search("small".into());
    let generation = app.archive_org.generation;
    app.set_archive_org_search_page_capacity(100);
    assert_eq!(app.archive_org.generation, generation);
    assert_eq!(request(&app).limit, 10);
    answer(&mut app);
    app.view.selected = app.archive_org.items.len();
    app.activate_archive_org_selection();
    assert_eq!((request(&app).page, request(&app).limit), (2, 10));
    answer(&mut app);
    assert_eq!(app.archive_org.items.len(), 20);
    assert_eq!(app.archive_org.items.last().unwrap().identifier, "item-19");

    app.search_archive_metadata("large".into(), ArchiveOrgSearchScope::Creator);
    assert_eq!((request(&app).page, request(&app).limit), (1, 100));
    answer(&mut app);
    app.set_archive_org_search_page_capacity(25);
    assert!(app.go_back_archive_org());
    assert_eq!(app.archive_org.items.len(), 20);
    app.view.selected = app.archive_org.items.len();
    app.activate_archive_org_selection();
    assert_eq!((request(&app).page, request(&app).limit), (3, 10));
    answer(&mut app);
    assert_eq!(app.archive_org.items.len(), 30);
    for (index, item) in app.archive_org.items.iter().enumerate() {
        assert_eq!(item.identifier, format!("item-{index}"));
    }
}

/// A restored Archive tab must not fetch a fixed-size page before the frontend reports size.
#[test]
fn archive_search_page_capacity_is_known_before_restored_tab_browse() {
    let (temporary, mut saved) = controller();
    saved.archive_org_search_query = "restored".into();
    saved.view.search_query = "restored".into();
    assert!(saved.save_session());
    let session = saved.store.session().unwrap().unwrap();
    let store = StateStore::open_in_memory().unwrap();
    store.save_session(&session, 0).unwrap();
    let config = Config::for_dir(temporary.path().join("restored-config"));
    let mut app = AppController::new(config, store, None, None);
    assert_eq!(app.view.screen, Screen::ArchiveOrg);
    assert!(
        app.archive_org.worker.is_none(),
        "construction must not start HTTP"
    );
    assert!(app.archive_org.pending.is_none());
    app.archive_org.client = ArchiveOrgClient::with_transport(Arc::new(SearchTransport));
    app.set_archive_org_search_page_capacity(80);
    app.poll_archive_org_worker();
    assert!(app.archive_org.initialized);
    if matches!(
        app.archive_org.pending.as_ref().map(|job| &job.kind),
        Some(ArchiveRequest::Search(_))
    ) {
        assert_eq!(request(&app).limit, 80);
        assert_eq!(request(&app).query, "restored");
    } else {
        // The in-memory transport may finish during this same tick.
        assert_eq!(app.archive_org.items.len(), 80);
        assert_eq!(app.archive_org.submitted_query, "restored");
    }
    if let Some(worker) = app.archive_org.worker.take() {
        worker.thread.join().unwrap();
    }
}
