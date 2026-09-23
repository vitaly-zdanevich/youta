//! Explicit, bounded public comments owned by one selected SoundCloud track.

use super::*;
use crate::providers::soundcloak::SoundcloakComment;

/// A closed popup may retain an in-flight worker, but never an unsent request.
#[derive(Default)]
pub(super) struct SoundCloudCommentsState {
    pending: Option<CommentsJob>,
    worker: Option<CommentsWorker>,
    cache: VecDeque<(String, Vec<SoundcloakComment>)>,
}

/// Request identity is independent of whichever row is selected when HTTP finishes.
#[derive(Clone)]
struct CommentsJob {
    generation: u64,
    track_id: String,
    canonical: String,
    title: String,
}

/// One comments request runs at a time; a later explicit request replaces the queued one.
struct CommentsWorker {
    job: CommentsJob,
    response: Receiver<Result<Vec<SoundcloakComment>, String>>,
    thread: JoinHandle<()>,
}

impl AppController {
    /// Fetches comments only after F6/click, never during result selection or artwork loading.
    pub(in crate::app) fn open_soundcloud_comments(&mut self) {
        let Some(track) = self.soundcloud.items.get(self.view.selected).cloned() else {
            self.view.status_line = "Select a SoundCloud track to load its public comments".into();
            return;
        };
        self.invalidate_youtube_video_comments_popup();
        let job = CommentsJob {
            generation: self.youtube_video_comments_generation,
            track_id: track.id,
            canonical: track.webpage_url.to_string(),
            title: track.title,
        };
        self.view.video_comments_popup = Some(VideoCommentsPopupView {
            source: SourceKind::SoundCloud,
            video_id: job.canonical.clone(),
            video_title: job.title.clone(),
            state: VideoCommentsPopupState::Loading,
            comments: Vec::new(),
            scroll_offset: 0,
        });
        if let Some((_, comments)) = self
            .soundcloud
            .comments
            .cache
            .iter()
            .find(|(url, _)| *url == job.canonical)
        {
            self.apply_soundcloud_comments(job, Ok(comments.clone()));
        } else {
            self.soundcloud.comments.pending = Some(job);
            self.poll_soundcloud_comments();
        }
    }

    /// Cancels unsent work; the common popup generation rejects any active late response.
    pub(in crate::app) fn cancel_pending_soundcloud_comments(&mut self) {
        self.soundcloud.comments.pending = None;
    }

    /// Polls finished workers and starts at most one latest explicit comments request.
    pub(in crate::app) fn poll_soundcloud_comments(&mut self) {
        if self
            .soundcloud
            .comments
            .worker
            .as_ref()
            .is_some_and(|worker| worker.thread.is_finished())
        {
            let worker = self
                .soundcloud
                .comments
                .worker
                .take()
                .expect("finished comments worker");
            let result = worker.response.try_recv().unwrap_or_else(|_| {
                Err("Soundcloak comments worker stopped without a result".into())
            });
            let _ = worker.thread.join();
            self.apply_soundcloud_comments(worker.job, result);
        }
        if self.soundcloud.comments.worker.is_some() {
            return;
        }
        let Some(job) = self.soundcloud.comments.pending.take() else {
            return;
        };
        let client = match self.soundcloak_client() {
            Ok(client) => client,
            Err(error) => {
                self.apply_soundcloud_comments(job, Err(error));
                return;
            }
        };
        let track_id = job.track_id.clone();
        let instance = self.soundcloak_instance_label().to_owned();
        let (sender, response) = bounded(1);
        match thread::Builder::new()
            .name("youta-soundcloak-comments".into())
            .spawn(move || {
                let result = client
                    .comments(&track_id)
                    .map_err(|error| format!("Soundcloak ({instance}): {error}"));
                let _ = sender.send(result);
            }) {
            Ok(thread) => {
                self.soundcloud.comments.worker = Some(CommentsWorker {
                    job,
                    response,
                    thread,
                })
            }
            Err(error) => self.apply_soundcloud_comments(job, Err(error.to_string())),
        }
    }

    /// Caches public results without reopening a dismissed popup or replacing another item.
    fn apply_soundcloud_comments(
        &mut self,
        job: CommentsJob,
        result: Result<Vec<SoundcloakComment>, String>,
    ) {
        if let Ok(comments) = &result {
            self.soundcloud
                .comments
                .cache
                .retain(|(url, _)| *url != job.canonical);
            self.soundcloud
                .comments
                .cache
                .push_back((job.canonical.clone(), comments.clone()));
            while self.soundcloud.comments.cache.len() > 8 {
                self.soundcloud.comments.cache.pop_front();
            }
        }
        if job.generation != self.youtube_video_comments_generation
            || self.view.screen != Screen::SoundCloud
            || !self
                .view
                .video_comments_popup
                .as_ref()
                .is_some_and(|popup| {
                    popup.source == SourceKind::SoundCloud && popup.video_id == job.canonical
                })
            || !self.view.details.as_ref().is_some_and(|details| {
                details
                    .media_id
                    .as_ref()
                    .is_some_and(|id| id.source == SourceKind::SoundCloud)
                    && details
                        .webpage_url
                        .as_ref()
                        .is_some_and(|url| url.as_str() == job.canonical)
            })
        {
            return;
        }
        let popup = self
            .view
            .video_comments_popup
            .as_mut()
            .expect("matching comments popup");
        match result {
            Ok(comments) => {
                popup.comments = comments
                    .into_iter()
                    .take(MAX_VIDEO_COMMENTS)
                    .map(|comment| {
                        let date = soundcloud_date(comment.created_at.as_deref());
                        VideoCommentView {
                            author_name: comment.author,
                            like_count: 0,
                            published: (!date.is_empty()).then_some(date),
                            text: comment.timestamp_seconds.map_or_else(
                                || comment.body.clone(),
                                |seconds| format!("[{}] {}", format_seconds(seconds), comment.body),
                            ),
                        }
                    })
                    .collect();
                popup.state = if popup.comments.is_empty() {
                    VideoCommentsPopupState::Empty
                } else {
                    VideoCommentsPopupState::Ready
                };
            }
            Err(error) => popup.state = VideoCommentsPopupState::Error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serves comments only; an unexpected passive search/artwork request fails immediately.
    struct CommentsTransport(Mutex<Vec<url::Url>>);

    impl crate::providers::soundcloak::SoundcloakTransport for CommentsTransport {
        fn fetch(
            &self,
            url: &url::Url,
            _: usize,
        ) -> Result<Vec<u8>, crate::providers::ProviderError> {
            assert_eq!(url.path(), "/_/api/v2/tracks/123/comments");
            self.0.lock().unwrap().push(url.clone());
            Ok(serde_json::to_vec(&serde_json::json!({"collection": [{
                "kind": "comment", "user": {"username": "Comment author"}, "body": "Public comment", "created_at": "2024-03-02T10:20:30Z", "timestamp": 12000
            }]})).unwrap())
        }
    }

    /// Uses controller fixtures but installs a no-network comments-only transport.
    fn controller() -> (AppController, Arc<CommentsTransport>) {
        let mut app = super::super::tests::controller();
        let transport = Arc::new(CommentsTransport(Mutex::new(Vec::new())));
        app.soundcloud.client = Some(
            SoundcloakClient::with_transport(
                url::Url::parse("https://soundcloak.example/").unwrap(),
                transport.clone(),
            )
            .unwrap(),
        );
        let mut track = super::super::tests::track("one");
        track.id = "123".into();
        super::super::tests::complete(&mut app, 0, vec![track], None);
        (app, transport)
    }

    /// Waits for mock HTTP only; all UI effects are applied by the normal poll method.
    fn finish(app: &mut AppController) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app
            .soundcloud
            .comments
            .worker
            .as_ref()
            .unwrap()
            .thread
            .is_finished()
        {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        app.poll_soundcloud_comments();
    }

    #[test]
    fn soundcloud_comments_are_explicit_selected_cached_and_source_correct() {
        let (mut app, requests) = controller();
        app.update_soundcloud_detail();
        app.poll_soundcloud_comments();
        assert!(requests.0.lock().unwrap().is_empty());
        app.dispatch(UiAction::OpenVideoComments);
        assert_eq!(
            app.view.video_comments_popup.as_ref().unwrap().state,
            VideoCommentsPopupState::Loading
        );
        finish(&mut app);
        let popup = app.view.video_comments_popup.as_ref().unwrap();
        assert_eq!(popup.source, SourceKind::SoundCloud);
        assert_eq!(popup.state, VideoCommentsPopupState::Ready);
        assert_eq!(popup.comments[0].author_name, "Comment author");
        assert_eq!(popup.comments[0].text, "[0:12] Public comment");
        app.dispatch(UiAction::DismissVideoComments);
        app.dispatch(UiAction::OpenVideoComments);
        assert_eq!(
            app.view.video_comments_popup.as_ref().unwrap().state,
            VideoCommentsPopupState::Ready
        );
        assert_eq!(requests.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn soundcloud_comments_late_response_never_reopens_dismissed_popup() {
        let (mut app, _) = controller();
        app.dispatch(UiAction::OpenVideoComments);
        app.dispatch(UiAction::DismissVideoComments);
        finish(&mut app);
        assert!(app.view.video_comments_popup.is_none());
        assert_eq!(app.soundcloud.comments.cache.len(), 1);
    }

    #[test]
    fn soundcloud_comments_late_response_does_not_replace_a_different_owner() {
        for change in ["row", "tab", "popup source"] {
            let (mut app, _) = controller();
            app.dispatch(UiAction::OpenVideoComments);
            match change {
                "row" => {
                    app.soundcloud.items.push(super::super::tests::track("two"));
                    app.refresh_soundcloud_rows();
                    app.select_row(1);
                }
                "tab" => app.show_screen(Screen::Search),
                _ => app.view.video_comments_popup.as_mut().unwrap().source = SourceKind::YouTube,
            }
            let popup = app.view.video_comments_popup.clone();
            finish(&mut app);
            assert_eq!(app.view.video_comments_popup, popup, "{change}");
        }
    }
}
