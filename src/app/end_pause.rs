//! Keeps a completed YouTube or Archive item seekable without resolving or loading it again.

use super::{AppController, Duration, MediaKind, PlaybackPhase, PlayerCommand, SourceKind};

impl AppController {
    /// Retains only started, finite YouTube or Archive media without requested continuation.
    ///
    /// Explicit queued entries keep their existing priority even with Autoplay
    /// disabled. A held EOF must not hide a decoder failure before audio starts.
    fn should_keep_end_paused(&self) -> bool {
        self.playback_phase == PlaybackPhase::Playing
            && !self.config.playback.autoplay
            && !self.playback_queue.repeat_one
            && self.current_media.as_ref().is_some_and(|media| {
                matches!(media.source, SourceKind::YouTube | SourceKind::ArchiveOrg)
                    && self.playback_queue.current().is_some_and(|item| {
                        item.media.id == *media && item.media.kind != MediaKind::LiveStream
                    })
            })
            && self
                .playback_queue
                .current_index
                .is_some_and(|index| index.saturating_add(1) >= self.playback_queue.items.len())
    }

    /// Does not send a clamped seek that would clear mpv's retained EOF latch.
    #[allow(clippy::float_cmp)] // Only exact 100% is a no-op; invalid values reach backend validation.
    pub(super) fn is_held_end_no_op_seek(&self, command: &PlayerCommand) -> bool {
        self.playback_held_at_end
            && match command {
                PlayerCommand::SeekRelative(seconds) => *seconds >= 0,
                PlayerCommand::SeekPercent(percent) => *percent == 100.0,
                PlayerCommand::SeekAbsolute(position) => self
                    .view
                    .playback
                    .duration
                    .is_some_and(|end| *position >= end),
                _ => false,
            }
    }

    /// Records natural completion while keeping the current media and seek context.
    ///
    /// Returns whether the caller must abandon a pending timeline command. A
    /// retained item is still the same seek target; releasing one may immediately
    /// load a successor, so its old keyboard input must not cross that boundary.
    pub(super) fn handle_held_playback_end(&mut self, elapsed: Duration) -> bool {
        if self.view.playback_end_releasing {
            return true;
        }
        if !self.playback_held_at_end {
            self.account_listen_time(elapsed);
            self.view.playback.paused = true;
            self.view.playback.buffering = false;
            if self.playback_phase == PlaybackPhase::Playing {
                if let Some(duration) = self.view.playback.duration {
                    self.view.playback.position = duration;
                }
                self.persist_position();
            }
        }
        if self.should_keep_end_paused() {
            self.playback_held_at_end = true;
            self.view.playback.idle = false;
            self.view.status_line = format!("Paused at end: {}", self.current_playback_title());
            false
        } else {
            self.release_held_playback_end(elapsed);
            true
        }
    }

    /// Applies Autoplay, Repeat, or newly queued items after an end pause.
    pub(super) fn continue_from_held_end(&mut self, elapsed: Duration) -> bool {
        if self.view.playback_end_releasing {
            return true;
        }
        if self.playback_held_at_end && !self.should_keep_end_paused() {
            self.release_held_playback_end(elapsed);
            return true;
        }
        false
    }

    /// Lets the existing authoritative EOF path advance/repeat exactly once.
    fn release_held_playback_end(&mut self, elapsed: Duration) {
        self.playback_held_at_end = false;
        self.view.playback_end_releasing = true;
        if let Some(player) = self.player.as_mut()
            && let Err(error) = player.command(PlayerCommand::ReleaseEndOfFile)
        {
            self.fail_player("Could not finish playback", &error, elapsed);
        }
    }
}
