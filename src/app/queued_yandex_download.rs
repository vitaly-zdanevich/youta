//! Native original-quality downloads launched from durable, credential-free intent.

use super::*;
use crate::download_queue::DownloadSource;

impl AppController {
    /// Resolves one persisted provider identity freshly in the existing native worker.
    ///
    /// # Errors
    /// Returns a startup failure before the scheduler waits for a terminal response.
    pub(super) fn start_queued_yandex_download(
        &mut self,
        source: &DownloadSource,
    ) -> Result<(), String> {
        if source.media_id.source != SourceKind::YandexMusic {
            return Err("The native Yandex downloader requires a Yandex Music track".to_owned());
        }
        source.validate()?;
        let artists = source
            .creator
            .as_deref()
            .unwrap_or("Yandex Music")
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| crate::providers::yandex_music::YandexMusicArtist {
                id: None,
                name: name.to_owned(),
            })
            .collect();
        let track = YandexMusicTrack {
            id: source.media_id.external_id.clone(),
            title: source.title.clone(),
            artists,
            album: None,
            duration_ms: source
                .duration_seconds
                .and_then(|seconds| seconds.checked_mul(1_000)),
            artwork_url: None,
            content_kind: YandexMusicContentKind::Music,
            reaction: YandexMusicReaction::Neutral,
            webpage_url: source.webpage_url.clone(),
        };
        let file_stem = format!(
            "{} — {}",
            yandex_music_artist_names(&track.artists),
            track.title
        );
        let generation = self.try_start_yandex_music_download_batch(
            format!("Yandex Music track — {}", track.title),
            vec![YandexMusicDownloadItem { track, file_stem }],
        )?;
        self.queued_yandex_download_started(generation);
        Ok(())
    }

    /// Requests native cancellation without blocking input on a provider HTTP call.
    ///
    /// The stopped worker retains its slot until it exits. Its old generation may
    /// no longer publish progress or complete another durable download attempt.
    pub(super) fn cancel_yandex_music_download(&mut self) -> bool {
        if self.yandex_music_download_thread.is_none() {
            return false;
        }
        let generation = self.yandex_music_download_generation;
        let queued_owner = self.queued_yandex_download_is_current(generation);
        self.yandex_music_download_generation = generation.wrapping_add(1);
        if let Some(cancellation) = self.yandex_music_download_cancel.take() {
            cancellation.store(true, AtomicOrdering::Release);
        }
        if let Some(download) = self.view.download.as_mut() {
            download.active = false;
            download.eta_seconds = None;
        }
        #[cfg(feature = "yt-dlp")]
        {
            self.download_completion_notice_deadline = None;
            self.download_cancellation_notice_deadline =
                Some(Instant::now() + DOWNLOAD_CANCELLATION_NOTICE_DURATION);
        }
        if queued_owner {
            self.cancel_manual_download_owner();
        }
        self.view.status_line = "Cancelled the Yandex Music download".to_owned();
        self.reap_cancelled_yandex_music_download();
        true
    }

    /// Releases a cancelled worker's slot only after joining it cannot block input.
    pub(super) fn reap_cancelled_yandex_music_download(&mut self) {
        if self.yandex_music_download_cancel.is_none()
            && self
                .yandex_music_download_thread
                .as_ref()
                .is_some_and(JoinHandle::is_finished)
            && let Some(handle) = self.yandex_music_download_thread.take()
        {
            let _ = handle.join();
        }
    }
}
