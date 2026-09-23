//! Review-only Evernote offers for successfully published, finite Radio recordings.

use super::*;

const MAX_COMPLETED_RECORDING_OFFERS: usize = 8;

/// An exact completed local file, never a live stream or private staging pathname.
pub(super) struct CompletedRecordingOffer {
    selection: EvernoteSelection,
    identity: LocalFileIdentity,
}

impl AppController {
    /// Queues only a closed, nonempty regular recording; older overflow files remain in Downloaded.
    pub(super) fn offer_completed_radio_recording_to_evernote(&mut self, path: PathBuf) {
        if self.view.quitting
            || self.diagnostic_only
            || self.shutdown_persistence_succeeded.is_some()
        {
            return;
        }
        let Some(identity) = completed_recording_identity(&path) else {
            return;
        };
        let Ok(item) =
            queue_item_from_local(&local_media_item_stub(path.clone(), Some(identity.length)))
        else {
            return;
        };
        let selection = EvernoteSelection {
            media: item.media,
            source: OpusAudioSource::LocalFile(path),
        };
        if selection.draft().validate().is_err() {
            return;
        }
        if self.pending_radio_evernote_offers.len() == MAX_COMPLETED_RECORDING_OFFERS {
            self.pending_radio_evernote_offers.pop_front();
        }
        self.pending_radio_evernote_offers
            .push_back(CompletedRecordingOffer {
                selection,
                identity,
            });
        self.maybe_offer_completed_radio_recording_to_evernote();
    }

    /// Opens existing review UI only after all current modal/draft ownership has been released.
    ///
    /// No worker, conversion, credential lookup or upload starts here. Missing credentials
    /// are requested only if the user explicitly submits the offered note.
    pub(super) fn maybe_offer_completed_radio_recording_to_evernote(&mut self) {
        if self.pending_radio_evernote_offers.is_empty()
            || self.view.quitting
            || self.diagnostic_only
            || self.shutdown_persistence_succeeded.is_some()
            || self.evernote_selection.is_some()
            || self.evernote_thread.is_some()
            || self.evernote_credentials_draft.is_some()
            || recording_offer_modal_is_busy(&self.view)
        {
            return;
        }
        while let Some(offer) = self.pending_radio_evernote_offers.pop_front() {
            let OpusAudioSource::LocalFile(path) = &offer.selection.source else {
                continue;
            };
            if completed_recording_identity(path).as_ref() != Some(&offer.identity) {
                self.view.status_line = format!(
                    "Skipped unavailable or changed recording offer: {}",
                    path.display()
                );
                continue;
            }
            let draft = offer.selection.draft();
            self.evernote_selection = Some(offer.selection);
            self.evernote_body_undo.clear();
            self.open_evernote_review(draft);
            self.view.status_line = "Recording saved locally. Review saving this completed file to Evernote, or Esc to skip".into();
            break;
        }
    }
}

/// Rejects replacement symlinks, empty captures, missing files and relative paths without decoding.
fn completed_recording_identity(path: &Path) -> Option<LocalFileIdentity> {
    if !path.is_absolute() {
        return None;
    }
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() == 0 {
        return None;
    }
    local_file_identity(path)
}

/// Mirrors the shared modal surfaces so a completion cannot cover an unrelated editor or result.
fn recording_offer_modal_is_busy(view: &ViewModel) -> bool {
    if view.help_open
        || view.search_editing
        || view.expanded_thumbnail_available()
        || view.project_history_popup.is_some()
        || view.error_popup.is_some()
        || view.audio_quality_popup.is_some()
        || view.video_summary_popup.is_some()
        || view.video_comments_popup.is_some()
        || view.youtube_setup_popup.is_some()
        || view.yandex_music_setup_popup.is_some()
        || view.rss_subscription_popup.is_some()
        || view.preferences_popup.is_some()
        || view.playlist_popup.is_some()
        || view.queue_popup.is_some()
        || view.private_note_popup.is_some()
        || view.local_file_popup.is_some()
        || view.download_choice_popup.is_some()
        || view.archive_playback_choice_popup.is_some()
        || view.download_queue_popup.is_some()
        || view.evernote_popup.is_some()
        || view.evernote_credentials_popup.is_some()
    {
        return true;
    }
    #[cfg(feature = "ascii-visualizer")]
    if view.ascii_visualizer.is_some() {
        return true;
    }
    #[cfg(feature = "youtube-captions")]
    if view.youtube_captions_popup.is_some() {
        return true;
    }
    #[cfg(feature = "qr")]
    if view.video_qr_popup.is_some() {
        return true;
    }
    #[cfg(feature = "lan-sharing")]
    if view.lan_share_popup.is_some() || view.podcast_feed_options_popup.is_some() {
        return true;
    }
    #[cfg(feature = "commons-upload")]
    if view.commons_upload_popup.is_some() || view.commons_credentials_popup.is_some() {
        return true;
    }
    #[cfg(feature = "s3-upload")]
    if view.s3_upload_popup.is_some() || view.s3_credentials_popup.is_some() {
        return true;
    }
    #[cfg(feature = "archive-upload")]
    if view.archive_upload_popup.is_some() || view.archive_credentials_popup.is_some() {
        return true;
    }
    #[cfg(feature = "yt-dlp")]
    if view.channel_download_popup.is_some() {
        return true;
    }
    false
}
