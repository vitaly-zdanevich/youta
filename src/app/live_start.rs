//! Async, generation-owned playback from an original `YouTube` broadcast start.

use super::*;
use crate::playback::live_from_start::{LiveFromStartConfig, LiveFromStartStream};

/// A retained transport; the abstraction lets controller tests stay entirely offline.
pub(super) trait LiveStartGuard: Send {
    /// Returns the transient capability URL consumed only by the playback backend.
    fn url(&self) -> &url::Url;
    /// Wakes the frontend when a transport failure becomes available.
    fn set_worker_waker(&self, waker: Option<std::task::Waker>);
    /// Consumes a sanitized transport failure, if one has arrived.
    fn take_error(&mut self) -> Option<String>;
    /// Terminates and joins the transport on a cleanup worker or during shutdown.
    fn stop(&mut self);
}

impl LiveStartGuard for LiveFromStartStream {
    fn url(&self) -> &url::Url {
        self.url()
    }
    fn set_worker_waker(&self, waker: Option<std::task::Waker>) {
        self.set_worker_waker(waker);
    }
    fn take_error(&mut self) -> Option<String> {
        self.take_error()
    }
    fn stop(&mut self) {
        self.stop();
    }
}

/// Injectable preparation runs exclusively on a worker, never inside a UI tick.
pub(super) type LiveStartPrepare = Arc<
    dyn Fn(
            &LiveFromStartConfig,
            &url::Url,
            &YouTubePrewarmCancellation,
        ) -> Result<Box<dyn LiveStartGuard>, String>
        + Send
        + Sync,
>;

/// The exact playback owner and intent that may accept one prepared stream.
struct PendingLiveStart {
    generation: u64,
    media_id: MediaId,
    cancellation: YouTubePrewarmCancellation,
}

/// One worker reply owns its guard until accepted or asynchronously retired.
struct LiveStartCompletion {
    generation: u64,
    media_id: MediaId,
    result: Result<Box<dyn LiveStartGuard>, String>,
}

/// A stream retained for precisely one accepted playback generation.
pub(super) struct ActiveLiveStart {
    media_id: MediaId,
    guard: Box<dyn LiveStartGuard>,
}

/// All helper and cleanup handles are retained until completion or explicit shutdown.
pub(super) struct LiveStartState {
    generation: u64,
    pending: Option<PendingLiveStart>,
    pub(super) active: Option<ActiveLiveStart>,
    pub(super) accepting_input: bool,
    pub(super) prepare: LiveStartPrepare,
    sender: ResponseSender<LiveStartCompletion>,
    responses: Receiver<LiveStartCompletion>,
    workers: Vec<JoinHandle<()>>,
}

impl LiveStartState {
    /// Reports preparation without clearing the currently playing media identity.
    pub(super) fn preparing(&self) -> bool {
        self.pending.is_some()
    }

    /// Keeps preparation and future transport failures on the existing frontend wake path.
    pub(super) fn new(notifier: WorkerNotifier) -> Self {
        let (sender, responses) = unbounded();
        Self {
            generation: 0,
            pending: None,
            active: None,
            accepting_input: false,
            prepare: Arc::new(|config, source, cancellation| {
                LiveFromStartStream::prepare(config, source, cancellation)
                    .map(|guard| Box::new(guard) as Box<dyn LiveStartGuard>)
                    .map_err(|error| error.to_string())
            }),
            sender: ResponseSender::new(sender, notifier),
            responses,
            workers: Vec::new(),
        }
    }

    /// Cancels superseded preparation without blocking on a helper or touching active audio.
    fn cancel_pending(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(pending) = self.pending.take() {
            pending.cancellation.cancel();
        }
    }

    /// Retires transport I/O and child cleanup off the frontend thread.
    fn retire(&mut self, mut guard: Box<dyn LiveStartGuard>) {
        self.workers.push(thread::spawn(move || guard.stop()));
    }

    /// Only already-finished workers are joined while the frontend is running.
    fn reap(&mut self) {
        let mut index = 0;
        while index < self.workers.len() {
            if self.workers[index].is_finished() {
                let _ = self.workers.swap_remove(index).join();
            } else {
                index += 1;
            }
        }
    }
}

impl AppController {
    /// Requests original broadcast bytes while the currently playing audio continues.
    pub(super) fn request_live_start(&mut self) {
        let owner = self.current_media.clone().filter(|media| {
            media.source == SourceKind::YouTube
                && !self.view.playback.idle
                && self.view.playback.live
                && self.view.playing_media_id.as_ref() == Some(media)
        });
        let Some(media_id) = owner else {
            self.live_start_unavailable("No active YouTube broadcast owns this request");
            return;
        };
        if self.playback_queue.current().is_none_or(|item| {
            item.media.id != media_id || item.media.kind != MediaKind::LiveStream
        }) {
            self.live_start_unavailable("The active broadcast is no longer in the playback queue");
            return;
        }
        if self
            .live_start
            .pending
            .as_ref()
            .is_some_and(|pending| pending.media_id == media_id)
        {
            return;
        }
        #[cfg(feature = "vk")]
        self.cancel_vk_playback();
        self.live_start.cancel_pending();
        self.live_start.reap();
        let generation = self.live_start.generation;
        let cancellation = YouTubePrewarmCancellation::new();
        let worker_cancellation = cancellation.clone();
        let mut source = url::Url::parse("https://www.youtube.com/watch").expect("fixed watch URL");
        source
            .query_pairs_mut()
            .append_pair("v", &media_id.external_id);
        let config = LiveFromStartConfig {
            executable: self.config.providers.yt_dlp_executable.clone(),
            ..LiveFromStartConfig::default()
        };
        let prepare = Arc::clone(&self.live_start.prepare);
        let responses = self.live_start.sender.clone();
        let worker_owner = media_id.clone();
        let worker = thread::Builder::new()
            .name("youta-live-start".into())
            .spawn(move || {
                let result = prepare(&config, &source, &worker_cancellation);
                // A dropped controller retires the owned guard on this worker.
                let _ = responses.send(LiveStartCompletion {
                    generation,
                    media_id: worker_owner,
                    result,
                });
            });
        match worker {
            Ok(worker) => {
                self.live_start.workers.push(worker);
                self.live_start.pending = Some(PendingLiveStart {
                    generation,
                    media_id,
                    cancellation,
                });
                self.view.status_line = "Preparing the broadcast's original beginning...".into();
            }
            Err(_) => self.live_start_unavailable("The preparation worker could not start"),
        }
    }

    /// Rechecks the playback owner after queued events may have ended or replaced it.
    fn live_start_completion_matches(&self, completion: &LiveStartCompletion) -> bool {
        self.live_start.pending.as_ref().is_some_and(|pending| {
            pending.generation == completion.generation && pending.media_id == completion.media_id
        }) && self.live_start.generation == completion.generation
            && self.current_media.as_ref() == Some(&completion.media_id)
            && self.view.playing_media_id.as_ref() == Some(&completion.media_id)
            && !self.view.playback.idle
            && self.view.playback.live
    }

    /// Accepts only the newest ready stream and keeps every stale cleanup off the UI.
    pub(super) fn poll_live_start(&mut self) {
        self.live_start.reap();
        while let Ok(completion) = self.live_start.responses.try_recv() {
            if self.live_start_completion_matches(&completion) && completion.result.is_ok() {
                // Settle the old input's lifecycle before a new stream claims it.
                self.update_player();
            }
            if !self.live_start_completion_matches(&completion) {
                if let Ok(guard) = completion.result {
                    self.live_start.retire(guard);
                }
                continue;
            }
            self.live_start.pending = None;
            match completion.result {
                Ok(guard) => {
                    let Some(item) = self
                        .playback_queue
                        .current()
                        .filter(|item| item.media.id == completion.media_id)
                        .cloned()
                    else {
                        self.live_start.retire(guard);
                        continue;
                    };
                    let mut input = PlaybackInput::new(guard.url().as_str());
                    input.bypass_ytdl = true;
                    self.live_start.accepting_input = true;
                    self.play_queue_item_with_origin_and_input(
                        item,
                        true,
                        self.current_autoplay_origin.clone(),
                        Some(input),
                    );
                    self.live_start.accepting_input = false;
                    if self.current_media.as_ref() == Some(&completion.media_id)
                        && self.playback_phase == PlaybackPhase::Loading
                    {
                        guard.set_worker_waker(Some(self.worker_notifier.as_waker()));
                        self.live_start.active = Some(ActiveLiveStart {
                            media_id: completion.media_id,
                            guard,
                        });
                    } else {
                        self.live_start.retire(guard);
                    }
                }
                Err(error) => self.live_start_unavailable(&error),
            }
        }
        let failure = self.live_start.active.as_mut().and_then(|active| {
            if self.current_media.as_ref() != Some(&active.media_id) {
                Some("The from-start playback owner changed".to_owned())
            } else {
                active.guard.take_error()
            }
        });
        if let Some(failure) = failure {
            self.fail_live_start_playback(&failure);
        }
    }

    /// Consumes a published helper failure before lifecycle events can discard its guard.
    pub(super) fn take_live_start_error(&mut self) -> Option<String> {
        self.live_start
            .active
            .as_mut()
            .and_then(|active| active.guard.take_error())
    }

    /// Cancels old intents and asynchronously retires the transport on replacement or EOF.
    pub(super) fn cancel_live_start(&mut self) {
        self.live_start.cancel_pending();
        self.retire_active_live_start();
    }

    /// Deferred play intents invalidate old preparation while retaining accepted audio.
    pub(super) fn cancel_live_start_preparation(&mut self) {
        self.live_start.cancel_pending();
    }

    /// A transport is stopped only when its backend input is actually replaced or cleared.
    pub(super) fn retire_active_live_start(&mut self) {
        if let Some(active) = self.live_start.active.take() {
            self.live_start.retire(active.guard);
        }
    }

    /// Fails closed instead of resolving a different source or exposing helper credentials.
    pub(super) fn fail_live_start_playback(&mut self, error: &str) {
        self.prepare_to_clear_playback(Duration::ZERO);
        self.reset_playback_state();
        if let Some(mut player) = self.player.take() {
            let _ = player.shutdown();
        }
        self.live_start_unavailable(error);
    }

    /// Expected unavailable beginnings use concise information and never trigger helper probes.
    pub(super) fn live_start_unavailable(&mut self, error: &str) {
        let reason = if error.contains("timed out") || error.contains("timeout") {
            "Preparing initial audio timed out."
        } else if error.contains("executable") {
            "The configured yt-dlp executable is unavailable."
        } else {
            "Original broadcast audio could not be retrieved."
        };
        let message = format!("The beginning of this broadcast is unavailable. {reason}");
        let report =
            DiagnosticReport::capture_message(&message, self.unprobed_diagnostic_helpers())
                .render();
        self.show_report_popup(
            "Broadcast beginning is unavailable".into(),
            report,
            false,
            Some(message.clone()),
        );
        self.view.status_line = message;
    }

    /// Cancellation precedes joining, and stale published guards are stopped during shutdown.
    pub(super) fn shutdown_live_start(&mut self) {
        self.cancel_live_start();
        for worker in self.live_start.workers.drain(..) {
            let _ = worker.join();
        }
        while let Ok(completion) = self.live_start.responses.try_recv() {
            if let Ok(mut guard) = completion.result {
                guard.stop();
            }
        }
    }
}
