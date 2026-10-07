//! Monotonic scheduling deadlines shared by event-driven frontends.

use super::*;

impl AppController {
    /// Preserves existing delayed work while bounding unsupported-source maintenance.
    ///
    /// Only actionable deadlines shorten the wait: a due lookup behind another
    /// in-flight lookup relies on that worker's completion event, not a spin loop.
    pub(super) fn scheduled_tick_delay(&self, now: Instant) -> Duration {
        let mut delay = Duration::from_secs(1);
        let mut include = |deadline: Option<Instant>| {
            if let Some(deadline) = deadline {
                delay = delay.min(deadline.saturating_duration_since(now));
            }
        };
        include(self.transient_footer_notice_deadline);
        if self.diagnostic_only {
            return delay.max(Duration::from_millis(1));
        }
        include(
            self.scheduled_channel_details
                .as_ref()
                .map(|work| work.due_at),
        );
        include(
            self.scheduled_subscription_video_metadata
                .as_ref()
                .map(|work| work.due_at),
        );
        #[cfg(feature = "wikidata")]
        include(
            self.scheduled_channel_wikidata
                .as_ref()
                .map(|work| work.due_at),
        );
        #[cfg(all(feature = "wikidata", feature = "yandex-music"))]
        if self.pending_yandex_music_wikidata.is_none() {
            include(
                self.scheduled_yandex_music_wikidata
                    .as_ref()
                    .map(|work| work.due_at),
            );
        }
        #[cfg(all(feature = "wikidata", feature = "soundcloud"))]
        include(self.soundcloud_wikidata_deadline());
        #[cfg(feature = "yt-dlp")]
        {
            include(
                self.scheduled_youtube_prewarm
                    .as_ref()
                    .map(|work| work.due_at),
            );
            if self.automatic_download_queue.is_empty()
                && !self
                    .active_download
                    .as_ref()
                    .is_some_and(ActiveDownload::is_automatic)
            {
                include(self.next_auto_download_check_at);
            }
            include(self.download_completion_notice_deadline);
            include(self.download_cancellation_notice_deadline);
        }
        #[cfg(feature = "commons-upload")]
        if !self.commons_category_lookup_pending {
            include(self.commons_category_lookup_deadline);
        }
        #[cfg(feature = "audio-quality")]
        if self.displayed_local_audio_quality.is_some() {
            include(self.local_audio_quality_revalidate_at);
        }
        #[cfg(feature = "acoustid")]
        if self.displayed_local_fingerprint.is_some() {
            include(self.local_fingerprint_revalidate_at);
        }
        #[cfg(feature = "waveform")]
        if self.view.waveform_visible {
            if matches!(self.view.waveform, WaveformView::Ready { .. }) {
                include(self.local_waveform_revalidate_at);
            } else if matches!(self.view.waveform, WaveformView::Failed { .. }) {
                include(
                    self.local_waveform_retry
                        .as_ref()
                        .map(|retry| retry.retry_at),
                );
            }
        }
        // Autosave and external maintenance retain the one-second bound. An
        // overdue failed save must not turn into a one-millisecond retry loop.
        // Yield even for an overdue deadline so shutdown and input remain fair.
        delay.max(Duration::from_millis(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fatal diagnostics stop normal scheduling but still expire footer notices.
    #[test]
    fn diagnostic_mode_ignores_suspended_work_deadlines() {
        let temporary = crate::test_support::canonical_tempdir("diagnostic deadlines");
        let config = Config::for_dir(temporary.path());
        let store = StateStore::open(&config).unwrap();
        let mut controller = AppController::new(config, store, None, None);
        let now = Instant::now();
        controller.diagnostic_only = true;
        controller.scheduled_channel_details = Some(ScheduledChannelDetails {
            generation: 1,
            channel_id: "fixture".to_owned(),
            due_at: now,
        });
        assert_eq!(controller.scheduled_tick_delay(now), Duration::from_secs(1));
        controller.transient_footer_notice_deadline = Some(now + Duration::from_millis(50));
        assert_eq!(
            controller.scheduled_tick_delay(now),
            Duration::from_millis(50)
        );
    }

    /// A selected channel keeps its debounce instead of waiting a full idle tick.
    #[test]
    fn idle_deadline_preserves_debounce_and_never_busy_loops() {
        let temporary = crate::test_support::canonical_tempdir("reducer deadlines");
        let config = Config::for_dir(temporary.path());
        let store = StateStore::open(&config).unwrap();
        let mut controller = AppController::new(config, store, None, None);
        #[cfg(feature = "yt-dlp")]
        {
            // Startup schedules an immediate subscription check even with an
            // empty library; this fixture represents settled idle maintenance.
            controller.next_auto_download_check_at = None;
        }
        let now = Instant::now();
        assert_eq!(controller.scheduled_tick_delay(now), Duration::from_secs(1));
        controller.scheduled_channel_details = Some(ScheduledChannelDetails {
            generation: 1,
            channel_id: "fixture".to_owned(),
            due_at: now + Duration::from_millis(200),
        });
        assert_eq!(
            controller.scheduled_tick_delay(now),
            Duration::from_millis(200)
        );
        assert_eq!(
            controller.scheduled_tick_delay(now + Duration::from_millis(200)),
            Duration::from_millis(1)
        );
        controller.scheduled_channel_details = None;
        controller.transient_footer_notice_deadline = Some(now + Duration::from_millis(50));
        assert_eq!(
            controller.scheduled_tick_delay(now),
            Duration::from_millis(50)
        );
    }

    /// An overdue hourly check waits for the active queue instead of spinning.
    #[cfg(feature = "yt-dlp")]
    #[test]
    fn an_existing_automatic_download_queue_does_not_busy_loop() {
        let temporary = crate::test_support::canonical_tempdir("download deadlines");
        let config = Config::for_dir(temporary.path());
        let store = StateStore::open(&config).unwrap();
        let mut controller = AppController::new(config, store, None, None);
        let now = Instant::now();
        controller.next_auto_download_check_at = Some(now);
        controller
            .automatic_download_queue
            .push_back(AutomaticDownloadJob {
                channel_id: "fixture".to_owned(),
                channel_name: "Fixture".to_owned(),
                source_url: url::Url::parse("https://www.youtube.com/@fixture/videos").unwrap(),
            });
        assert_eq!(controller.scheduled_tick_delay(now), Duration::from_secs(1));
        controller.automatic_download_queue.clear();
        assert_eq!(
            controller.scheduled_tick_delay(now),
            Duration::from_millis(1)
        );
    }
}
