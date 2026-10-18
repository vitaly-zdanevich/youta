//! Persist explicit software-volume changes independently of listening history.

use super::{AppController, PlaybackError, PlaybackStatus, PlayerCommand};
use std::time::{Duration, Instant};

impl AppController {
    /// Applies and remembers a volume adjustment, including before player creation.
    ///
    /// An accepted value temporarily masks stale threaded-player snapshots so
    /// consecutive keypresses accumulate until the backend acknowledges the value.
    /// Rejected playback commands leave the preference untouched. A failed save
    /// retains the accepted session value and reports that it will not survive
    /// restart, rather than undoing an audible adjustment.
    pub(super) fn change_volume(&mut self, delta: i8) {
        if self.config.playback.audiophile.enabled {
            self.view.status_line =
                PlaybackError::DirectProfileRestriction("software volume").to_string();
            return;
        }
        let volume = i16::from(self.view.playback.volume)
            .saturating_add(i16::from(delta))
            .clamp(0, 100) as u8;
        if volume == self.view.playback.volume {
            return;
        }
        if let Some(player) = self.player.as_mut()
            && let Err(error) = player.command(PlayerCommand::SetVolume(volume))
        {
            if matches!(error, PlaybackError::DirectProfileRestriction(_)) {
                self.view.status_line = error.to_string();
            } else {
                self.show_error("Playback command failed", &error);
            }
            return;
        }
        self.pending_volume = self
            .player
            .as_ref()
            .map(|_| (volume, Instant::now() + Duration::from_secs(1)));
        self.view.playback.volume = volume;
        if let Err(error) = self.config.save_volume_percent(volume) {
            self.config.playback.volume_percent = volume;
            self.view.status_line =
                format!("Volume {volume}%: could not remember setting: {error}");
        }
    }

    /// Prevents an older snapshot from undoing a successful volume command.
    ///
    /// Once acknowledged, subsequent external player-volume changes are visible
    /// normally and the next UI adjustment starts from that observed value.
    /// The existing one-second maintenance tick also expires the guard if an
    /// external change superseded the target before a snapshot could observe it.
    pub(super) fn reconcile_pending_volume(&mut self, status: &mut PlaybackStatus) {
        if let Some((volume, deadline)) = self.pending_volume {
            if status.volume == volume || Instant::now() >= deadline {
                self.pending_volume = None;
            } else {
                status.volume = volume;
            }
        }
    }
}
