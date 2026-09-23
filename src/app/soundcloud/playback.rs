//! Metadata admission and progress isolation for public SoundCloud previews.

use super::*;

/// Runtime-only playback admission; no stream URL or preview marker is persisted.
#[derive(Default)]
pub(super) struct SoundCloudPlaybackState {
    generation: u64,
    pending: Option<PlaybackJob>,
    worker: Option<PlaybackWorker>,
    admitting: Option<PlaybackCandidate>,
    active_preview: Option<MediaId>,
    /// One accepted metadata owner supports navigation after its search was replaced.
    active_track: Option<(MediaId, SoundcloakTrack)>,
}

/// A queued intent retains stable replay metadata, never a signed audio locator.
#[derive(Clone)]
struct PlaybackJob {
    generation: u64,
    item: QueueItem,
    canonical: url::Url,
    queue_cursor_already_positioned: bool,
    origin: Option<AutoplayOrigin>,
}

/// One HTTP resolver occupies the lane until completion, including when obsolete.
struct PlaybackWorker {
    job: PlaybackJob,
    response: Receiver<Result<(SoundcloakTrack, PlaybackInput), String>>,
    thread: JoinHandle<()>,
}

/// Synchronous, identity-bound admission for the common playback loader.
struct PlaybackCandidate {
    id: MediaId,
    preview: bool,
    accepted: bool,
}

impl AppController {
    /// Resolves public metadata before any SoundCloud backend load.
    pub(in crate::app) fn request_soundcloud_playback(
        &mut self,
        item: QueueItem,
        queue_cursor_already_positioned: bool,
        origin: Option<AutoplayOrigin>,
    ) {
        self.cancel_pending_soundcloud_playback();
        let canonical = match self
            .soundcloak_client()
            .and_then(|client| canonical_playback_url(&client, &item.media.webpage_url))
        {
            Ok(canonical) => canonical,
            Err(error) => {
                self.soundcloud_playback_error(&error);
                return;
            }
        };
        self.soundcloud.playback.pending = Some(PlaybackJob {
            generation: self.soundcloud.playback.generation,
            item,
            canonical,
            queue_cursor_already_positioned,
            origin,
        });
        // Resolving a new intent must not hide or replace the currently playing
        // item. The common loader starts playback activity only after acceptance.
        self.view.status_line = "Checking public SoundCloud playback availability…".to_owned();
        self.poll_soundcloud_playback();
    }

    /// Polls the bounded playback resolver independently of search navigation.
    pub(in crate::app) fn poll_soundcloud_playback(&mut self) {
        if self
            .soundcloud
            .playback
            .worker
            .as_ref()
            .is_some_and(|worker| worker.thread.is_finished())
        {
            let worker = self
                .soundcloud
                .playback
                .worker
                .take()
                .expect("finished worker exists");
            let result = worker.response.try_recv().unwrap_or_else(|_| {
                Err("The Soundcloak playback worker stopped without a result".to_owned())
            });
            let _ = worker.thread.join();
            if worker.job.generation == self.soundcloud.playback.generation {
                self.apply_soundcloud_playback(worker.job, result);
            }
        }
        if self.soundcloud.playback.worker.is_some() {
            return;
        }
        let Some(job) = self.soundcloud.playback.pending.take() else {
            return;
        };
        let client = match self.soundcloak_client() {
            Ok(client) => client,
            Err(error) => {
                self.apply_soundcloud_playback(job, Err(error));
                return;
            }
        };
        let canonical = job.canonical.clone();
        let (sender, response) = bounded(1);
        match thread::Builder::new()
            .name("soundcloak-playback".to_owned())
            .spawn(move || {
                let result = (|| {
                    let track = client
                        .resolve(&canonical)
                        .map_err(|error| error.to_string())?;
                    let location = client
                        .playback_url(&track)
                        .map_err(|error| error.to_string())?;
                    let mut input = PlaybackInput::new(location.to_string());
                    input.bypass_ytdl = true;
                    Ok((track, input))
                })();
                let _ = sender.send(result);
            }) {
            Ok(thread) => {
                self.soundcloud.playback.worker = Some(PlaybackWorker {
                    job,
                    response,
                    thread,
                })
            }
            Err(error) => self.apply_soundcloud_playback(job, Err(error.to_string())),
        }
    }

    /// Applies normalized metadata while preserving the persisted full-track identity.
    fn apply_soundcloud_playback(
        &mut self,
        job: PlaybackJob,
        result: Result<(SoundcloakTrack, PlaybackInput), String>,
    ) {
        let (track, input) = match result {
            Ok(resolved) => resolved,
            Err(error) => {
                self.soundcloud_playback_error(&error);
                return;
            }
        };
        let preview = track.playback == SoundcloakPlayback::Preview;
        let mut item = queue_item_from_soundcloud(&track);
        // Older History/playlist identities may predate permalink normalization.
        // Keep their progress key, but refresh replay metadata from the provider.
        item.media.id = job.item.media.id;
        item.added_at = job.item.added_at;
        // The common loader forces previews to zero without persisting that
        // override. Replaying this queue item as Full can still resume its work.
        item.start_at_seconds = job.item.start_at_seconds;
        if preview {
            item.media.title.push_str(" [preview]");
        }
        self.soundcloud.playback.admitting = Some(PlaybackCandidate {
            id: item.media.id.clone(),
            preview,
            accepted: false,
        });
        self.play_queue_item_with_origin_and_input(
            item.clone(),
            job.queue_cursor_already_positioned,
            job.origin,
            Some(input),
        );
        let accepted = self
            .soundcloud
            .playback
            .admitting
            .take()
            .is_some_and(|candidate| candidate.accepted);
        if accepted {
            self.soundcloud.playback.active_track = Some((item.media.id.clone(), track));
        }
        // Positioned queue transitions normally retain their saved entry. Replace
        // only its accepted metadata so a preview is also labeled in the queue.
        if accepted
            && job.queue_cursor_already_positioned
            && let Some(index) = self.playback_queue.current_index
            && let Some(queued) = self.playback_queue.items.get_mut(index)
            && queued.media.id == item.media.id
        {
            *queued = item;
        }
    }

    /// Attributes failures without exposing an invalid configured credential URL.
    fn soundcloud_playback_error(&mut self, error: &str) {
        let configured = self.soundcloak_instance_label();
        let instance = if url::Url::parse(configured).is_ok_and(|url| {
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        }) {
            configured
        } else {
            "invalid configured instance"
        };
        self.view.status_line =
            format!("Soundcloak: {error}; instance: {instance} (providers.soundcloak_base_url)");
    }

    /// Invalidates a queued or running intent without interrupting current audio.
    pub(in crate::app) fn cancel_pending_soundcloud_playback(&mut self) {
        self.soundcloud.playback.generation = self.soundcloud.playback.generation.wrapping_add(1);
        self.soundcloud.playback.pending = None;
        // Retain the running worker until it exits: replacing an intent must not
        // permit unbounded parallel HTTP work. Only its result becomes obsolete.
    }

    /// Limits the resolved-input capability to the currently admitted identity.
    pub(in crate::app) fn soundcloud_resolved_candidate(&self, id: &MediaId) -> bool {
        id.source == SourceKind::SoundCloud
            && self
                .soundcloud
                .playback
                .admitting
                .as_ref()
                .is_some_and(|candidate| &candidate.id == id)
    }

    /// Identifies a preview before resume state is read by the common loader.
    pub(in crate::app) fn soundcloud_preview_candidate(&self, id: &MediaId) -> bool {
        self.soundcloud_resolved_candidate(id)
            && self
                .soundcloud
                .playback
                .admitting
                .as_ref()
                .is_some_and(|candidate| candidate.preview)
    }

    /// Changes the active preview identity only after the backend accepts a load.
    pub(in crate::app) fn accept_soundcloud_playback(&mut self, id: &MediaId) {
        let preview = self.soundcloud_preview_candidate(id);
        self.soundcloud.playback.active_preview = preview.then(|| id.clone());
        self.soundcloud.playback.active_track = None;
        if let Some(candidate) = self.soundcloud.playback.admitting.as_mut()
            && &candidate.id == id
        {
            candidate.accepted = true;
        }
    }

    /// Returns whether the current accepted item is an admitted public preview.
    pub(in crate::app) fn current_soundcloud_preview(&self) -> bool {
        self.current_media.as_ref().is_some_and(|id| {
            id.source == SourceKind::SoundCloud
                && self.soundcloud.playback.active_preview.as_ref() == Some(id)
        })
    }

    /// Returns metadata owned by the exact currently accepted SoundCloud track.
    pub(in crate::app) fn current_soundcloud_track(&self, id: &MediaId) -> Option<SoundcloakTrack> {
        self.soundcloud
            .playback
            .active_track
            .as_ref()
            .filter(|(owner, _)| owner == id && self.current_media.as_ref() == Some(id))
            .map(|(_, track)| track.clone())
    }

    /// Clears accepted preview/navigation state, preserving an explicit pending next request.
    pub(in crate::app) fn reset_soundcloud_preview(&mut self) {
        self.soundcloud.playback.active_preview = None;
        self.soundcloud.playback.active_track = None;
    }

    /// Checkpoints preview listening without recording full-track progress.
    pub(in crate::app) fn checkpoint_soundcloud_preview(&mut self) -> Option<bool> {
        if !self.current_soundcloud_preview() {
            return None;
        }
        let seconds = self.unflushed_listen_time.as_secs();
        Some(
            match self
                .store
                .checkpoint_listening(&SourceKind::SoundCloud, seconds)
            {
                Ok(()) => {
                    self.unflushed_listen_time -= Duration::from_secs(seconds);
                    true
                }
                Err(error) => {
                    self.show_error("Could not checkpoint SoundCloud preview listening", &error);
                    false
                }
            },
        )
    }

    /// Reports only a current intent, not an obsolete worker waiting to finish.
    pub(in crate::app) fn soundcloud_playback_pending(&self) -> bool {
        self.soundcloud.playback.pending.is_some()
            || self
                .soundcloud
                .playback
                .worker
                .as_ref()
                .is_some_and(|worker| worker.job.generation == self.soundcloud.playback.generation)
    }
    /// Reports an explicit queue selection, including a duplicate track identity.
    pub(in crate::app) fn soundcloud_playback_owns_queue_cursor(&self) -> bool {
        self.soundcloud
            .playback
            .pending
            .as_ref()
            .or_else(|| {
                self.soundcloud
                    .playback
                    .worker
                    .as_ref()
                    .filter(|worker| worker.job.generation == self.soundcloud.playback.generation)
                    .map(|worker| &worker.job)
            })
            .is_some_and(|job| job.queue_cursor_already_positioned)
    }
}

/// Normalizes benign old sharing links, then delegates strict public-track
/// validation to the provider without requesting an audio or metadata resource.
fn canonical_playback_url(client: &SoundcloakClient, saved: &url::Url) -> Result<url::Url, String> {
    let mut canonical = saved.clone();
    if matches!(canonical.scheme(), "http" | "https")
        && matches!(
            canonical.host_str(),
            Some("soundcloud.com" | "www.soundcloud.com")
        )
        && canonical.port().is_none()
        && !canonical
            .query_pairs()
            .any(|(key, _)| key == "secret_token")
    {
        canonical
            .set_scheme("https")
            .map_err(|()| "Invalid SoundCloud scheme".to_owned())?;
        canonical
            .set_host(Some("soundcloud.com"))
            .map_err(|error| error.to_string())?;
        canonical.set_query(None);
        canonical.set_fragment(None);
        let path = canonical.path().trim_end_matches('/').to_owned();
        canonical.set_path(&path);
    }
    client
        .page_url(&canonical)
        .map_err(|error| error.to_string())?;
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{ProviderError, soundcloak::SoundcloakTransport};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Mock metadata is the only input to the actual provider parser.
    fn metadata(slug: &str, preview: bool) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": 123, "kind": "track", "title": format!("Resolved {slug}"),
            "permalink_url": format!("https://soundcloud.com/artist/{slug}"),
            "user": {"username": "Fixture artist"}, "streamable": true,
            "policy": if preview { "SNIP" } else { "ALLOW" },
            "duration": if preview { 30_000 } else { 3_600_000 },
            "full_duration": 3_600_000,
            "media": {"transcodings": [{
                "snipped": preview, "duration": if preview { 30_000 } else { 3_600_000 },
                "format": {"protocol": if preview { "progressive" } else { "hls" },
                    "mime_type": "audio/mpeg"}
            }]}
        }))
        .unwrap()
    }

    /// Records bounded resolver calls and optionally gates their completion.
    struct Transport {
        responses: Mutex<VecDeque<Vec<u8>>>,
        requests: Mutex<Vec<(url::Url, usize)>>,
        gate: Option<(Sender<()>, Receiver<()>)>,
    }

    impl SoundcloakTransport for Transport {
        fn fetch(&self, url: &url::Url, limit: usize) -> Result<Vec<u8>, ProviderError> {
            self.requests.lock().unwrap().push((url.clone(), limit));
            if let Some((started, release)) = &self.gate {
                started
                    .send_timeout((), Duration::from_secs(5))
                    .map_err(|error| ProviderError::Transport(error.to_string()))?;
                release
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|error| ProviderError::Transport(error.to_string()))?;
            }
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| ProviderError::Transport("fixture metadata unavailable".to_owned()))
        }
    }

    /// Captures backend loads and lifecycle events without running an audio device.
    #[derive(Default)]
    struct PlayerState {
        inputs: Vec<PlaybackInput>,
        events: VecDeque<PlaybackEvent>,
        reject: bool,
    }

    struct Player(Arc<Mutex<PlayerState>>);

    impl PlaybackBackend for Player {
        fn play(&mut self, input: &PlaybackInput) -> PlaybackResult<()> {
            let mut state = self.0.lock().unwrap();
            if state.reject {
                return Err(PlaybackError::Protocol("fixture rejected load".to_owned()));
            }
            state.inputs.push(input.clone());
            Ok(())
        }

        fn command(&mut self, _: PlayerCommand) -> PlaybackResult<()> {
            Ok(())
        }
        fn status(&mut self) -> PlaybackResult<PlaybackStatus> {
            Ok(PlaybackStatus::default())
        }
        fn shutdown(&mut self) -> PlaybackResult<()> {
            Ok(())
        }
        fn poll_event(&mut self) -> PlaybackResult<Option<PlaybackEvent>> {
            Ok(self.0.lock().unwrap().events.pop_front())
        }
    }

    /// Creates stable public replay metadata with an intentionally obsolete full-track title.
    fn item(slug: &str) -> QueueItem {
        let url = url::Url::parse(&format!("https://soundcloud.com/artist/{slug}")).unwrap();
        QueueItem {
            media: MediaItem {
                id: MediaId::new(SourceKind::SoundCloud, url.as_str()),
                kind: MediaKind::Audio,
                title: "Old full-track title".to_owned(),
                creator: None,
                description: None,
                webpage_url: url.clone(),
                thumbnail_url: None,
                duration_seconds: Some(3_600),
                published_at: None,
                statistics: MediaStatistics::default(),
                license: MediaLicense::Unknown,
                chapters: Vec::new(),
                captions: Vec::new(),
            },
            playback_location: url.to_string(),
            start_at_seconds: None,
            added_at: 0,
        }
    }

    /// Uses only in-memory storage, mock metadata, and a mock backend.
    fn fixture(
        responses: Vec<Vec<u8>>,
        gate: Option<(Sender<()>, Receiver<()>)>,
    ) -> (AppController, Arc<Transport>, Arc<Mutex<PlayerState>>) {
        let transport = Arc::new(Transport {
            responses: Mutex::new(responses.into()),
            requests: Mutex::default(),
            gate,
        });
        let mut config = Config::for_dir("/tmp/youta-soundcloud-preview-test");
        config.providers.soundcloak_base_url =
            Some(url::Url::parse("https://soundcloak.example/").unwrap());
        config.playback.autoplay = false;
        config.persistence.save_playback_history = true;
        let mut app = AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
        app.soundcloud.client = Some(
            SoundcloakClient::with_transport(
                url::Url::parse("https://soundcloak.example/").unwrap(),
                transport.clone(),
            )
            .unwrap(),
        );
        let player = Arc::new(Mutex::new(PlayerState::default()));
        app.player = Some(Box::new(Player(player.clone())));
        (app, transport, player)
    }

    /// Polls completion with a deadline; no sleeps or real HTTP requests are needed.
    fn finish(app: &mut AppController) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.soundcloud_playback_pending() && Instant::now() < deadline {
            app.poll_soundcloud_playback();
            thread::yield_now();
        }
        assert!(
            !app.soundcloud_playback_pending(),
            "resolver did not finish"
        );
    }

    fn started(app: &mut AppController, player: &Arc<Mutex<PlayerState>>) {
        player
            .lock()
            .unwrap()
            .events
            .push_back(PlaybackEvent::PlaybackStarted);
        app.drain_player_events(Duration::ZERO);
    }

    #[test]
    fn soundcloud_preview_preserves_full_progress_at_start_checkpoint_and_eof() {
        let (mut app, transport, player) = fixture(vec![metadata("one", true)], None);
        let mut queued = item("one");
        let mut full = PlaybackProgress::new(queued.media.id.clone(), Some(3_600), 123);
        full.record_position(900, 123);
        app.store.upsert_progress(&full).unwrap();
        queued.start_at_seconds = Some(999);
        app.play_queue_item_with_origin(queued, false, None);
        assert!(
            player.lock().unwrap().inputs.is_empty(),
            "metadata must resolve first"
        );
        finish(&mut app);
        let input = player
            .lock()
            .unwrap()
            .inputs
            .first()
            .cloned()
            .expect("preview loaded");
        assert_eq!(input.start_at, Duration::ZERO);
        assert!(input.bypass_ytdl);
        assert!(
            input
                .location
                .contains("/_/api/progressive/artist/one?redirect=false")
        );
        assert_eq!(input.title.as_deref(), Some("Resolved one [preview]"));
        assert!(app.current_soundcloud_preview());
        assert!(app.current_playback_progress.is_none());
        assert_eq!(app.view.playback.duration, Some(Duration::from_secs(30)));
        started(&mut app, &player);
        app.unflushed_listen_time = Duration::from_millis(10_500);
        app.view.playback.position = Duration::from_secs(29);
        assert!(app.persist_position());
        assert_eq!(app.unflushed_listen_time, Duration::from_millis(500));
        assert_eq!(
            app.store.listened_seconds(&SourceKind::SoundCloud).unwrap(),
            10
        );
        assert_eq!(
            app.store.progress(&full.media_id).unwrap(),
            Some(full.clone())
        );
        app.handle_playback_end(
            PlaybackEnd {
                reason: PlaybackEndReason::Eof,
                error: None,
                file_error: None,
                diagnostic: None,
            },
            Duration::ZERO,
        );
        assert!(!app.current_soundcloud_preview());
        assert_eq!(app.store.progress(&full.media_id).unwrap(), Some(full));
        let history = app.store.history(false, 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].title, "Resolved one [preview]");
        assert_eq!(history[0].duration_seconds, Some(30));
        assert!(!history[0].finished);
        assert_eq!(
            history[0].replay_locator.as_deref(),
            Some("https://soundcloud.com/artist/one")
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn soundcloud_full_after_preview_restores_the_saved_full_track_position() {
        let (mut app, _, player) =
            fixture(vec![metadata("one", true), metadata("one", false)], None);
        let queued = item("one");
        let mut full = PlaybackProgress::new(queued.media.id.clone(), Some(3_600), 123);
        full.record_position(900, 123);
        app.store.upsert_progress(&full).unwrap();
        app.play_queue_item_with_origin(queued.clone(), false, None);
        finish(&mut app);
        assert!(app.current_soundcloud_preview());
        started(&mut app, &player);
        let saved_queue_item = app.playback_queue.current().unwrap().clone();
        app.play_queue_item_with_origin(saved_queue_item, false, None);
        finish(&mut app);
        let inputs = &player.lock().unwrap().inputs;
        assert_eq!(inputs.len(), 2);
        assert_eq!(
            inputs[1].start_at.as_secs(),
            full.resume_position_with_rewind(app.config.playback.resume_rewind_seconds)
        );
        assert!(inputs[1].location.contains("/_/api/hls/artist/one?"));
        assert_eq!(inputs[1].title.as_deref(), Some("Resolved one"));
        assert!(!app.current_soundcloud_preview());
        assert_eq!(
            app.current_playback_progress
                .as_ref()
                .unwrap()
                .duration_seconds,
            Some(3_600)
        );
    }

    #[test]
    fn soundcloud_failed_metadata_or_backend_load_preserves_the_active_preview() {
        for reject_backend in [false, true] {
            let responses = if reject_backend {
                vec![metadata("one", true), metadata("two", false)]
            } else {
                vec![metadata("one", true)]
            };
            let (mut app, _, player) = fixture(responses, None);
            app.play_queue_item_with_origin(item("one"), false, None);
            finish(&mut app);
            started(&mut app, &player);
            assert!(app.current_soundcloud_preview());
            let prior = app.current_media.clone();
            let queue = app.playback_queue.clone();
            player.lock().unwrap().reject = reject_backend;
            app.play_queue_item_with_origin(item("two"), false, None);
            assert_eq!(app.view.playing_media_id, prior);
            finish(&mut app);
            assert_eq!(app.current_media, prior);
            assert_eq!(app.view.playing_media_id, prior);
            assert_eq!(app.playback_queue, queue);
            assert!(app.current_soundcloud_preview());
        }
    }

    /// Navigation retains rich accepted metadata, not a failed replacement or another identity.
    #[test]
    fn soundcloud_now_playing_metadata_is_rich_identity_bound_and_accepted_only() {
        let mut rich: serde_json::Value = serde_json::from_slice(&metadata("one", true)).unwrap();
        rich["likes_count"] = serde_json::json!(123);
        rich["genre"] = serde_json::json!("Ambient");
        let (mut app, _, player) = fixture(
            vec![serde_json::to_vec(&rich).unwrap(), metadata("two", false)],
            None,
        );
        let first = item("one").media.id;
        assert!(app.current_soundcloud_track(&first).is_none());
        app.play_queue_item_with_origin(item("one"), false, None);
        finish(&mut app);
        started(&mut app, &player);
        let track = app
            .current_soundcloud_track(&first)
            .expect("accepted full metadata");
        assert_eq!(track.title, "Resolved one");
        assert_eq!(track.likes_count, Some(123));
        assert_eq!(track.genre.as_deref(), Some("Ambient"));
        assert_eq!(track.playback, SoundcloakPlayback::Preview);
        assert!(
            app.current_soundcloud_track(&item("two").media.id)
                .is_none()
        );

        player.lock().unwrap().reject = true;
        app.play_queue_item_with_origin(item("two"), false, None);
        finish(&mut app);
        assert_eq!(
            app.current_soundcloud_track(&first).unwrap().title,
            "Resolved one"
        );
        assert!(
            app.current_soundcloud_track(&item("two").media.id)
                .is_none()
        );
        app.current_media = Some(item("two").media.id);
        assert!(app.current_soundcloud_track(&first).is_none());
        app.current_media = Some(first.clone());
        app.handle_playback_end(
            PlaybackEnd {
                reason: PlaybackEndReason::Stop,
                error: None,
                file_error: None,
                diagnostic: None,
            },
            Duration::ZERO,
        );
        assert!(app.current_soundcloud_track(&first).is_none());
    }

    /// Accepting another source cannot expose metadata retained for an older SoundCloud track.
    #[test]
    fn soundcloud_now_playing_metadata_is_retired_after_another_source_is_accepted() {
        let (mut app, _, player) = fixture(vec![metadata("one", false)], None);
        let first = item("one").media.id;
        app.play_queue_item_with_origin(item("one"), false, None);
        finish(&mut app);
        started(&mut app, &player);
        assert!(app.current_soundcloud_track(&first).is_some());
        let mut replacement = item("other");
        replacement.media.id = MediaId::new(SourceKind::GenericYtDlp, "other");
        replacement.media.webpage_url = url::Url::parse("https://example.org/other.mp3").unwrap();
        replacement.playback_location = replacement.media.webpage_url.to_string();
        app.play_queue_item_with_origin(replacement, false, None);
        assert!(app.current_soundcloud_track(&first).is_none());
        app.current_media = Some(first.clone());
        assert!(
            app.current_soundcloud_track(&first).is_none(),
            "previous owner was discarded"
        );
    }

    #[test]
    fn soundcloud_playback_resolver_is_latest_only_and_cancellation_rejects_stale_results() {
        let (started_tx, started_rx) = bounded(4);
        let (release_tx, release_rx) = bounded(4);
        let (mut app, transport, player) = fixture(
            vec![metadata("a", false), metadata("c", true)],
            Some((started_tx, release_rx)),
        );
        app.play_queue_item_with_origin(item("a"), false, None);
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("A started");
        app.play_queue_item_with_origin(item("b"), false, None);
        app.play_queue_item_with_origin(item("c"), false, None);
        assert_eq!(transport.requests.lock().unwrap().len(), 1);
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while started_rx.try_recv().is_err() {
            assert!(Instant::now() < deadline);
            app.poll_soundcloud_playback();
            thread::yield_now();
        }
        assert!(
            player.lock().unwrap().inputs.is_empty(),
            "obsolete A must not load"
        );
        assert_eq!(transport.requests.lock().unwrap().len(), 2);
        app.cancel_pending_soundcloud_playback();
        release_tx.send(()).unwrap();
        // A cancelled worker still occupies the lane until reaped; a new request
        // cannot create another simultaneous resolver.
        app.play_queue_item_with_origin(item("d"), false, None);
        release_tx.send(()).unwrap();
        finish(&mut app);
        assert!(
            player.lock().unwrap().inputs.is_empty(),
            "cancelled C must not load"
        );
        let requests = transport.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        for (request, limit) in requests.iter() {
            assert_eq!(request.path(), "/_/api/v2/resolve");
            assert!((1..=4 * 1024 * 1024).contains(limit));
        }
        assert!(
            requests[1]
                .0
                .query_pairs()
                .any(|(k, v)| k == "url" && v.ends_with("/c"))
        );
    }

    #[test]
    fn soundcloud_playback_normalizes_public_share_links_and_rejects_private_or_foreign_urls() {
        for raw in [
            "http://www.soundcloud.com/artist/one/?utm_source=share#t=4",
            "https://soundcloud.com/artist/one",
        ] {
            let (mut app, transport, player) = fixture(vec![metadata("one", false)], None);
            let mut queued = item("one");
            queued.media.webpage_url = url::Url::parse(raw).unwrap();
            queued.playback_location = raw.to_owned();
            app.play_queue_item_with_origin(queued, false, None);
            finish(&mut app);
            assert_eq!(player.lock().unwrap().inputs.len(), 1, "{raw}");
            assert!(
                transport.requests.lock().unwrap()[0]
                    .0
                    .query_pairs()
                    .any(|(k, v)| k == "url" && v == "https://soundcloud.com/artist/one")
            );
        }
        for raw in [
            "https://secret@soundcloud.com/artist/one",
            "https://soundcloud.com/artist/one?secret_token=secret",
            "https://other.example/artist/one",
            "https://soundcloud.com/artist",
            "https://soundcloud.com/artist/sets/one",
            "https://soundcloud.com/artist/one%2Fsecret",
        ] {
            let (mut app, transport, player) = fixture(Vec::new(), None);
            let mut queued = item("one");
            queued.media.webpage_url = url::Url::parse(raw).unwrap();
            app.play_queue_item_with_origin(queued, false, None);
            finish(&mut app);
            assert!(player.lock().unwrap().inputs.is_empty(), "{raw}");
            assert!(transport.requests.lock().unwrap().is_empty(), "{raw}");
        }
    }

    #[test]
    fn soundcloud_preview_from_history_playlist_or_direct_override_always_resolves_metadata() {
        for route in 0..3 {
            let (mut app, transport, player) = fixture(vec![metadata("one", true)], None);
            let original = item("one");
            let queued = match route {
                0 => {
                    let history = HistoryEntry {
                        id: 1,
                        media_id: original.media.id.clone(),
                        title: original.media.title.clone(),
                        replay_locator: Some(original.playback_location.clone()),
                        started_at: 1,
                        last_played_at: 1,
                        position_seconds: 1_000,
                        duration_seconds: Some(3_600),
                        finished: false,
                    };
                    queue_item_from_history(&history, &history_replay_target(&history).unwrap())
                        .unwrap()
                }
                1 => queue_item_from_playlist_entry(&PlaylistEntry {
                    media: playlist_snapshot_from_queue_item(&original).unwrap(),
                    segment: None,
                    added_at: 1,
                })
                .unwrap(),
                _ => original,
            };
            // Merely providing a transient input does not grant resolved admission.
            let forged =
                (route == 2).then(|| PlaybackInput::new("https://forged.example/audio.mp3"));
            app.play_queue_item_with_origin_and_input(queued, false, None, forged);
            finish(&mut app);
            let inputs = &player.lock().unwrap().inputs;
            assert_eq!(inputs.len(), 1);
            assert!(
                inputs[0]
                    .location
                    .contains("/_/api/progressive/artist/one?")
            );
            assert_eq!(inputs[0].start_at, Duration::ZERO);
            assert!(app.current_soundcloud_preview());
            assert_eq!(transport.requests.lock().unwrap().len(), 1);
        }
    }

    #[test]
    fn soundcloud_unavailable_metadata_never_reaches_the_backend() {
        let mut denied: serde_json::Value =
            serde_json::from_slice(&metadata("one", false)).unwrap();
        denied["policy"] = serde_json::json!("BLOCK");
        let (mut app, _, player) = fixture(vec![serde_json::to_vec(&denied).unwrap()], None);
        app.play_queue_item_with_origin(item("one"), false, None);
        finish(&mut app);
        assert!(player.lock().unwrap().inputs.is_empty());
        assert!(app.current_media.is_none());
        assert!(app.soundcloud.playback.admitting.is_none());
        assert!(app.view.status_line.contains("Soundcloak:"));
        assert!(app.view.status_line.contains("https://soundcloak.example/"));
    }

    #[test]
    fn soundcloud_pending_positioned_queue_item_survives_old_eof_and_updates_accepted_metadata() {
        for requested_slug in ["requested", "old"] {
            let (started_tx, started_rx) = bounded(2);
            let (release_tx, release_rx) = bounded(2);
            let (mut app, _, player) = fixture(
                vec![metadata(requested_slug, true)],
                Some((started_tx, release_rx)),
            );
            let old = item("old");
            app.playback_queue.items = vec![old.clone(), item(requested_slug), item("next")];
            app.playback_queue.current_index = Some(0);
            app.current_media = Some(old.media.id.clone());
            app.view.playing_media_id = app.current_media.clone();
            app.playback_phase = PlaybackPhase::Playing;
            app.view.playback.duration = Some(Duration::from_secs(3_600));
            app.activate_queue_row(1);
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert_eq!(app.playback_queue.current_index, Some(1));
            app.handle_playback_end(
                PlaybackEnd {
                    reason: PlaybackEndReason::Eof,
                    error: None,
                    file_error: None,
                    diagnostic: None,
                },
                Duration::ZERO,
            );
            assert_eq!(
                app.playback_queue.current_index,
                Some(1),
                "old EOF must not consume the requested slot"
            );
            assert!(app.soundcloud_playback_pending());
            release_tx.send(()).unwrap();
            finish(&mut app);
            assert_eq!(player.lock().unwrap().inputs.len(), 1);
            assert_eq!(app.playback_queue.current_index, Some(1));
            assert_eq!(
                app.playback_queue.items[1].media.title,
                format!("Resolved {requested_slug} [preview]")
            );
            assert_eq!(app.playback_queue.items[1].media.duration_seconds, Some(30));
            assert_eq!(app.playback_queue.items.len(), 3);
            assert_eq!(app.playback_queue.items[2].media.id, item("next").media.id);
        }
    }

    #[test]
    fn soundcloud_latest_metadata_request_wins_even_after_leaving_the_tab() {
        let (started_tx, started_rx) = bounded(4);
        let (release_tx, release_rx) = bounded(4);
        let (mut app, transport, player) = fixture(
            vec![metadata("a", false), metadata("c", true)],
            Some((started_tx, release_rx)),
        );
        app.play_queue_item_with_origin(item("a"), false, None);
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        app.play_queue_item_with_origin(item("b"), false, None);
        app.play_queue_item_with_origin(item("c"), false, None);
        app.view.screen = Screen::History;
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while started_rx.try_recv().is_err() {
            assert!(Instant::now() < deadline);
            app.poll_soundcloud_playback();
            thread::yield_now();
        }
        assert!(player.lock().unwrap().inputs.is_empty());
        release_tx.send(()).unwrap();
        finish(&mut app);
        assert_eq!(
            transport.requests.lock().unwrap().len(),
            2,
            "intermediate B is never fetched"
        );
        assert_eq!(app.current_media, Some(item("c").media.id));
        assert_eq!(player.lock().unwrap().inputs.len(), 1);
        assert!(app.current_soundcloud_preview());
        assert_eq!(app.view.screen, Screen::History);
    }

    #[test]
    fn soundcloud_preview_marker_is_identity_bound_and_cleared_by_stop() {
        let (mut app, _, player) = fixture(vec![metadata("one", true)], None);
        app.play_queue_item_with_origin(item("one"), false, None);
        finish(&mut app);
        started(&mut app, &player);
        assert!(app.current_soundcloud_preview());
        app.current_media = Some(item("two").media.id);
        assert!(!app.current_soundcloud_preview());
        assert!(app.checkpoint_soundcloud_preview().is_none());
        app.current_media = Some(item("one").media.id);
        app.handle_playback_end(
            PlaybackEnd {
                reason: PlaybackEndReason::Stop,
                error: None,
                file_error: None,
                diagnostic: None,
            },
            Duration::ZERO,
        );
        assert!(!app.current_soundcloud_preview());
        assert!(app.soundcloud.playback.active_preview.is_none());
    }

    #[test]
    fn soundcloud_stop_cancels_a_pending_initial_playback_request() {
        let (started_tx, started_rx) = bounded(2);
        let (release_tx, release_rx) = bounded(2);
        let (mut app, _, player) =
            fixture(vec![metadata("one", true)], Some((started_tx, release_rx)));
        app.play_queue_item_with_origin(item("one"), false, None);
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        app.handle_playback_end(
            PlaybackEnd {
                reason: PlaybackEndReason::Stop,
                error: None,
                file_error: None,
                diagnostic: None,
            },
            Duration::ZERO,
        );
        assert!(!app.soundcloud_playback_pending());
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.soundcloud.playback.worker.is_some() && Instant::now() < deadline {
            app.poll_soundcloud_playback();
            thread::yield_now();
        }
        assert!(app.soundcloud.playback.worker.is_none());
        assert!(player.lock().unwrap().inputs.is_empty());
        assert!(app.current_media.is_none());
    }
}
