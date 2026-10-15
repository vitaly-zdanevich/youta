//! Bounded, nonblocking retries of one public Funkwhale stream before audio starts.

use super::*;

const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(500), Duration::from_secs(2)];

/// Retains the selected canonical track rather than an expiring failed audio URL.
pub(super) struct FunkwhalePlaybackRetry {
    generation: u64,
    item: QueueItem,
    origin: Option<AutoplayOrigin>,
    attempts: usize,
    due_at: Option<Instant>,
}

impl AppController {
    /// Clears delayed work when the shared player resets without starting another source.
    pub(in crate::app) fn clear_funkwhale_playback_retry(&mut self) {
        if self
            .funkwhale
            .playback_retry
            .as_ref()
            .is_some_and(|retry| retry.attempts > 0)
        {
            self.cancel_funkwhale_playback();
        } else if self.pending_funkwhale_playback_job().is_none() {
            self.funkwhale.playback_retry = None;
        }
    }

    /// Starts a new explicit intent's budget only after older pending retries are revoked.
    pub(super) fn remember_funkwhale_playback_retry(
        &mut self,
        item: &QueueItem,
        origin: Option<&AutoplayOrigin>,
    ) {
        self.funkwhale.playback_retry = Some(FunkwhalePlaybackRetry {
            generation: self.funkwhale.playback_generation,
            item: item.clone(),
            origin: origin.cloned(),
            attempts: 0,
            due_at: None,
        });
    }

    /// Schedules only temporary HTTP failures for the exact current pre-start queue entry.
    pub(in crate::app) fn retry_funkwhale_playback_end(
        &mut self,
        end: &PlaybackEnd,
        now: Instant,
    ) -> bool {
        if !matches!(
            end.reason,
            PlaybackEndReason::Error | PlaybackEndReason::Eof
        ) || !matches!(
            self.playback_phase,
            PlaybackPhase::Loading | PlaybackPhase::Loaded
        ) {
            return false;
        }
        let resolving = self.pending_funkwhale_playback_job().is_some();
        let Some(retry) = self.funkwhale.playback_retry.as_mut() else {
            return false;
        };
        if retry.generation != self.funkwhale.playback_generation
            || self.current_media.as_ref() != Some(&retry.item.media.id)
            || self.playback_queue.current().map(|item| &item.media.id)
                != Some(&retry.item.media.id)
        {
            return false;
        }
        if resolving {
            // An old load must not consume a newer explicit same-track intent's
            // budget. A retry refresh already owns that failed load and merely
            // suppresses duplicate terminal events until its fresh result arrives.
            return retry.attempts > 0;
        }
        if retry.due_at.is_some() {
            return true;
        }
        if !playback_end_http_status(end)
            .is_some_and(|status| matches!(status, 500 | 502 | 503 | 504))
        {
            return false;
        }
        let Some(delay) = RETRY_DELAYS.get(retry.attempts) else {
            return false;
        };
        retry.attempts += 1;
        retry.due_at = Some(now.checked_add(*delay).unwrap_or(now));
        retry.item.start_at_seconds = Some(
            self.pending_history
                .as_ref()
                .map_or(self.view.playback.position.as_secs(), |history| {
                    history.position_seconds
                }),
        );
        self.view.status_line = format!("Retrying Funkwhale audio ({}/2)...", retry.attempts);
        self.view.playback.buffering = true;
        self.begin_playback_start_activity();
        true
    }

    /// Exposes an owned retry deadline to the shared event-driven frontend scheduler.
    pub(in crate::app) fn funkwhale_playback_retry_deadline(&self) -> Option<Instant> {
        self.funkwhale
            .playback_retry
            .as_ref()
            .and_then(|retry| retry.due_at)
    }

    /// A failed refresh shares the original stream's retry budget and cannot leave a start spinner stuck.
    pub(super) fn retry_funkwhale_resolution_error(
        &mut self,
        item: &QueueItem,
        error: &str,
    ) -> bool {
        let retrying = matches!(
            self.playback_phase,
            PlaybackPhase::Loading | PlaybackPhase::Loaded
        ) && self.funkwhale.playback_retry.as_ref().is_some_and(|retry| {
            retry.attempts > 0
                && retry.item.media.id == item.media.id
                && self.current_media.as_ref() == Some(&item.media.id)
        });
        if !retrying {
            return false;
        }
        let end = PlaybackEnd {
            reason: PlaybackEndReason::Error,
            error: Some(error.to_owned()),
            file_error: None,
            diagnostic: None,
        };
        if self.retry_funkwhale_playback_end(&end, Instant::now()) {
            return true;
        }
        self.reset_playback_state();
        false
    }

    /// Resolves a fresh original-pod stream without blocking or advancing the queue cursor.
    pub(super) fn poll_due_funkwhale_playback_retry(&mut self, now: Instant) {
        if !self
            .funkwhale_playback_retry_deadline()
            .is_some_and(|due| now >= due)
        {
            return;
        }
        let Some(mut retry) = self.funkwhale.playback_retry.take() else {
            return;
        };
        if retry.generation != self.funkwhale.playback_generation
            || !matches!(
                self.playback_phase,
                PlaybackPhase::Loading | PlaybackPhase::Loaded
            )
            || self.current_media.as_ref() != Some(&retry.item.media.id)
            || self.playback_queue.current().map(|item| &item.media.id)
                != Some(&retry.item.media.id)
        {
            return;
        }
        retry.due_at = None;
        self.request_funkwhale_playback(retry.item.clone(), true, retry.origin.clone());
        // The normal resolver entry revokes older intents and creates a fresh budget;
        // this continuation instead carries forward the already-consumed attempts.
        retry.generation = self.funkwhale.playback_generation;
        self.funkwhale.playback_retry = Some(retry);
    }
}
