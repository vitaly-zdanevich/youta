//! Reversible navigation from a selected track's advertised upload owner.

use super::*;

impl AppController {
    /// Resolves upload ownership only for the selected, visibly matching track.
    fn selected_funkwhale_uploader(&self) -> Option<(FunkwhaleUploader, url::Url)> {
        if self.view.screen != Screen::Funkwhale || self.funkwhale_artist_directory() {
            return None;
        }
        let track = self.funkwhale.items.get(self.view.selected)?;
        let identity = MediaId::new(SourceKind::Funkwhale, &track.video_id);
        if self.view.rows.get(self.view.selected)?.media_id.as_ref() != Some(&identity) {
            return None;
        }
        let details = self.view.details.as_ref()?;
        if details.media_id.as_ref() != Some(&identity) || details.webpage_url != track.webpage_url
        {
            return None;
        }
        let pod = FunkwhaleProvider::instance_url_from_track_id(&track.video_id).ok()?;
        let actor = self
            .funkwhale
            .upload_metadata
            .get(&track.video_id)?
            .uploader_actor
            .clone()?;
        Some((actor, pod))
    }

    /// Opens the selected uploader's playable tracks without visiting a web browser.
    ///
    /// The original selected pod remains the catalogue owner; a remotely hosted
    /// actor never switches configuration or sends requests to another host.
    pub(in crate::app) fn open_funkwhale_uploader(&mut self) {
        let Some(uploader) = self.selected_funkwhale_uploader() else {
            return;
        };
        if self.funkwhale.uploader.as_ref() == Some(&uploader) {
            return;
        }
        let page = FunkwhaleNavigationSnapshot {
            items: self.funkwhale.items.clone(),
            albums: self.funkwhale.albums.clone(),
            standalone: self.funkwhale.standalone,
            standalone_presence: self.funkwhale.standalone_presence,
            next_page: self.funkwhale.next_page,
            page_size: self.funkwhale.page_size,
            artist: self.funkwhale.artist.clone(),
            album: self.funkwhale.album.clone(),
            tag: self.funkwhale.tag.clone(),
            uploader: self.funkwhale.uploader.clone(),
        };
        if self.funkwhale.artist_back.len() == 16 {
            self.funkwhale.artist_back.pop_front();
        }
        self.funkwhale.artist_back.push_back(ArtistReturn {
            page,
            query: self.funkwhale.query.clone(),
            selected: self.view.selected,
        });
        self.funkwhale.artist = None;
        self.funkwhale.standalone = false;
        self.funkwhale.album = None;
        self.funkwhale.tag = None;
        self.funkwhale.uploader = Some(uploader);
        self.start_funkwhale_catalogue(String::new());
    }

    /// Marks only the typed selected upload's appended ownership line as actionable.
    pub(super) fn project_funkwhale_uploader(
        &self,
        track: &VideoSummary,
        details: &mut DetailView,
    ) {
        let identity = MediaId::new(SourceKind::Funkwhale, &track.video_id);
        if details.media_id.as_ref() != Some(&identity)
            || details.webpage_url != track.webpage_url
            || FunkwhaleProvider::instance_url_from_track_id(&track.video_id).is_err()
        {
            return;
        }
        let Some(upload) = self.funkwhale.upload_metadata.get(&track.video_id) else {
            return;
        };
        let Some(actor) = upload.uploader_actor.as_ref() else {
            return;
        };
        let name = upload
            .uploader_name
            .as_deref()
            .unwrap_or(actor.full_username());
        let label = format!("Uploaded by [e]: {name}");
        let Some((start_byte, _)) =
            details
                .description
                .rmatch_indices(label.as_str())
                .find(|(start, _)| {
                    let end = *start + label.len();
                    (*start == 0 || details.description.as_bytes().get(*start - 1) == Some(&b'\n'))
                        && (end == details.description.len()
                            || details.description.as_bytes().get(end) == Some(&b'\n'))
                })
        else {
            return;
        };
        details.links.push(DetailLinkView {
            label: label.clone(),
            url: upload
                .uploader_url
                .as_ref()
                .unwrap_or(actor.actor_url())
                .to_string(),
            internal_target: Some(DetailLinkInternalTarget::FunkwhaleUploader(
                actor.full_username().to_owned(),
            )),
            presentation: DetailLinkPresentation::LabelOnly,
            description_range: Some(DetailHighlightRange {
                start_byte,
                end_byte: start_byte + label.len(),
            }),
            ..DetailLinkView::default()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Supplies selected tracks and public metadata without allowing HTTP work.
    fn controller() -> AppController {
        let mut config = Config::for_dir("/tmp/youta-funkwhale-uploader-controller-fixture");
        config.providers.funkwhale_instance_url =
            Some(url::Url::parse("https://pod.example/").unwrap());
        let mut app = AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
        app.diagnostic_only = true;
        app.view.screen = Screen::Funkwhale;
        app.funkwhale.recent_attempted = true;
        app.funkwhale.query = "previous search".to_owned();
        app.funkwhale.items = vec![track(1), track(2)];
        app.funkwhale.selected = 1;
        let details = details(&track(2));
        app.cache_funkwhale_details(
            details,
            UploadMetadataUpdate::Replace(Some(FunkwhaleUploadMetadata {
                uploader_name: Some("emil@pod.example".to_owned()),
                uploader_url: Some(url::Url::parse("https://pod.example/@emil").unwrap()),
                uploader_actor: Some(actor()),
                created_at: None,
            })),
        );
        app.populate_funkwhale();
        app
    }

    /// Keeps artist and uploader identities explicitly unrelated.
    fn track(id: u32) -> VideoSummary {
        let page = url::Url::parse(&format!("https://pod.example/library/tracks/{id}/")).unwrap();
        VideoSummary {
            video_id: page.to_string(),
            title: format!("Track {id}"),
            channel_id: "https://pod.example/library/artists/9/".to_owned(),
            channel_name: "Recording artist".to_owned(),
            description: String::new(),
            duration_seconds: Some(120),
            view_count: None,
            published_at: None,
            published_text: None,
            live: false,
            was_live: false,
            orientation: VideoOrientation::Unknown,
            thumbnails: Vec::new(),
            webpage_url: Some(page),
            stream_url: None,
        }
    }

    /// Retains the exact library actor rather than deriving it from the display name.
    fn actor() -> FunkwhaleUploader {
        FunkwhaleUploader::new(
            "emil@pod.example".to_owned(),
            url::Url::parse("https://pod.example/federation/actors/emil").unwrap(),
        )
        .unwrap()
    }

    /// Completes only ordinary track facts; optional uploader facts arrive separately.
    fn details(track: &VideoSummary) -> VideoDetails {
        VideoDetails {
            video_id: track.video_id.clone(),
            title: track.title.clone(),
            channel_name: track.channel_name.clone(),
            channel_id: track.channel_id.clone(),
            description: "An ordinary description".to_owned(),
            duration_seconds: track.duration_seconds,
            view_count: None,
            like_count: None,
            comment_count: None,
            published_at: None,
            published_text: None,
            license: None,
            rating: None,
            ratings_allowed: None,
            live: false,
            was_live: false,
            orientation: VideoOrientation::Unknown,
            keywords: Vec::new(),
            thumbnails: Vec::new(),
            webpage_url: track.webpage_url.clone(),
            stream_url: None,
        }
    }

    /// Copies a scoped completion while preserving the production search generation.
    fn complete(app: &mut AppController, tracks: Vec<VideoSummary>, next_page: Option<u32>) {
        let job = Job::Search {
            generation: app.funkwhale.generation,
            pod: app.funkwhale_catalogue_pod().unwrap(),
            query: app.funkwhale.query.clone(),
            page: 1,
            size: app.funkwhale.page_size,
            artist: app
                .funkwhale
                .artist
                .as_ref()
                .map(|(identity, _)| identity.clone()),
            album: app
                .funkwhale
                .album
                .as_ref()
                .map(|(identity, _)| identity.clone()),
            tag: app.funkwhale.tag.as_ref().map(|(tag, _)| tag.clone()),
            standalone: app.funkwhale.standalone,
            uploader: app
                .funkwhale
                .uploader
                .as_ref()
                .map(|(actor, _)| actor.clone()),
        };
        app.apply_funkwhale_reply(
            job,
            Ok(Reply::Search(crate::providers::SearchPage {
                page: 1,
                items: tracks.into_iter().map(SearchItem::Video).collect(),
                next_page,
            })),
        );
    }

    /// One inline button keeps the advertised profile URL on its own following line.
    #[test]
    fn selected_uploader_is_an_inline_internal_link_with_a_separate_profile_line() {
        let app = controller();
        let details = app.view.details.as_ref().unwrap();
        let link = details.links.iter().find(|link| matches!(&link.internal_target, Some(DetailLinkInternalTarget::FunkwhaleUploader(username)) if username == "emil@pod.example")).unwrap();
        assert_eq!(link.presentation, DetailLinkPresentation::LabelOnly);
        let range = link.description_range.as_ref().unwrap();
        assert_eq!(
            &details.description[range.start_byte..range.end_byte],
            "Uploaded by [e]: emil@pod.example"
        );
        assert!(
            details
                .description
                .contains("Uploaded by [e]: emil@pod.example\nhttps://pod.example/@emil")
        );
        assert_eq!(
            link.internal_target.as_ref().unwrap().action(),
            UiAction::OpenFunkwhaleUploader
        );
    }

    /// Opening a user keeps its original pod and restores the prior route and cursor.
    #[test]
    fn uploader_navigation_and_pagination_preserve_owner_order_and_back_snapshot() {
        let mut app = controller();
        app.update_funkwhale_search_page_capacity(37);
        app.funkwhale.album = Some((
            "https://pod.example/library/albums/8/".to_owned(),
            "Album".to_owned(),
        ));
        app.funkwhale.next_page = Some(4);
        app.funkwhale.page_size = 19;
        app.config.providers.funkwhale_instance_url =
            Some(url::Url::parse("https://different.example/").unwrap());
        app.open_funkwhale_uploader();
        assert_eq!(app.funkwhale.uploader.as_ref().unwrap().0, actor());
        assert_eq!(
            app.funkwhale_catalogue_pod().unwrap().as_str(),
            "https://pod.example/"
        );
        assert!(app.funkwhale.artist.is_none());
        assert!(app.funkwhale.album.is_none());
        assert!(app.funkwhale.tag.is_none());
        assert!(!app.funkwhale.standalone);
        assert_eq!(app.funkwhale.page_size, 37);
        complete(&mut app, vec![track(12), track(10)], Some(2));
        complete(&mut app, vec![track(10), track(11)], None);
        assert_eq!(
            app.funkwhale
                .items
                .iter()
                .map(|item| item.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Track 12", "Track 10", "Track 11"]
        );
        assert_eq!(app.funkwhale.uploader.as_ref().unwrap().0, actor());
        assert!(app.back_funkwhale_artist());
        assert!(app.funkwhale.uploader.is_none());
        assert_eq!(app.funkwhale.query, "previous search");
        assert_eq!(app.view.selected, 1);
        assert_eq!(app.funkwhale.items.len(), 2);
        assert_eq!(app.funkwhale.album.as_ref().unwrap().1, "Album");
        assert_eq!(app.funkwhale.next_page, Some(4));
        assert_eq!(app.funkwhale.page_size, 19);
    }

    /// Stale visual metadata and display-only owner text cannot authorize navigation.
    #[test]
    fn uploader_navigation_rejects_stale_selection_missing_actor_and_other_tabs() {
        let mut app = controller();
        app.view.selected = 0;
        let generation = app.funkwhale.generation;
        app.open_funkwhale_uploader();
        assert_eq!(app.funkwhale.generation, generation);
        app.view.selected = 1;
        app.view.details.as_mut().unwrap().media_id =
            Some(MediaId::new(SourceKind::YouTube, "unrelated"));
        app.open_funkwhale_uploader();
        assert_eq!(app.funkwhale.generation, generation);
        app.update_funkwhale_detail();
        app.funkwhale
            .upload_metadata
            .get_mut(&track(2).video_id)
            .unwrap()
            .uploader_actor = None;
        app.open_funkwhale_uploader();
        assert_eq!(app.funkwhale.generation, generation);
        app.view.screen = Screen::Search;
        app.open_funkwhale_uploader();
        assert_eq!(app.funkwhale.generation, generation);
    }

    /// Label-like tag text cannot move the uploader action away from its actual line.
    #[test]
    fn uploader_inline_range_does_not_match_the_same_text_inside_a_tag() {
        let mut app = controller();
        let mut raw = details(&track(2));
        raw.keywords = vec!["Uploaded by [e]: emil@pod.example".to_owned()];
        app.cache_funkwhale_details(raw, UploadMetadataUpdate::Unchanged);
        app.update_funkwhale_detail();
        let detail = app.view.details.as_ref().unwrap();
        let range = detail
            .links
            .iter()
            .find_map(|link| {
                matches!(
                    link.internal_target,
                    Some(DetailLinkInternalTarget::FunkwhaleUploader(_))
                )
                .then(|| link.description_range.as_ref())
                .flatten()
            })
            .unwrap();
        assert_eq!(detail.description.as_bytes()[range.start_byte - 1], b'\n');
        assert!(detail.description[range.end_byte..].starts_with("\nhttps://pod.example/@emil"));
    }

    /// The real queued intents freeze uploader scope on both first and next pages.
    #[test]
    fn uploader_search_jobs_freeze_scope_and_back_rejects_late_completions() {
        let mut app = controller();
        app.update_funkwhale_search_page_capacity(2);
        let (_sender, response) = bounded(1);
        app.funkwhale.worker = Some(Worker {
            job: Job::Pods,
            response,
            thread: thread::spawn(|| {}),
        });
        app.diagnostic_only = false;
        app.open_funkwhale_uploader();
        let first = app
            .funkwhale
            .jobs
            .pop_front()
            .expect("queued uploader catalogue");
        assert!(
            matches!(&first, Job::Search { uploader: Some(owner), pod, page: 1, size: 2, artist: None, album: None, tag: None, standalone: false, .. }
            if owner == &actor() && pod.as_str() == "https://pod.example/")
        );
        app.apply_funkwhale_reply(
            first,
            Ok(Reply::Search(crate::providers::SearchPage {
                page: 1,
                items: vec![SearchItem::Video(track(12)), SearchItem::Video(track(10))],
                next_page: Some(2),
            })),
        );
        app.view.selected = 2;
        app.funkwhale.selected = 2;
        app.activate_funkwhale_selection();
        let next = app
            .funkwhale
            .jobs
            .iter()
            .find(|job| matches!(job, Job::Search { .. }))
            .unwrap()
            .clone();
        assert!(
            matches!(&next, Job::Search { uploader: Some(owner), pod, page: 2, size: 2, artist: None, album: None, tag: None, standalone: false, .. }
            if owner == &actor() && pod.as_str() == "https://pod.example/")
        );
        assert!(app.back_funkwhale_artist());
        app.apply_funkwhale_reply(
            next,
            Ok(Reply::Search(crate::providers::SearchPage {
                page: 2,
                items: vec![SearchItem::Video(track(99))],
                next_page: None,
            })),
        );
        assert_eq!(
            app.funkwhale
                .items
                .iter()
                .map(|item| item.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Track 1", "Track 2"]
        );
        assert!(app.funkwhale.uploader.is_none());
        app.diagnostic_only = true;
        app.funkwhale.jobs.clear();
        app.funkwhale.worker.take().unwrap().thread.join().unwrap();
    }
}
