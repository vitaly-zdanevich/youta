//! Retains explicit broadcast history across providers with different metadata coverage.

use super::{AppController, HashMap, HashSet, SearchItem, VideoDetails, VideoSummary, unix_time};
use crate::persistence::PersistenceError;

impl AppController {
    /// Finds positive past-broadcast evidence in the existing bounded RAM caches.
    fn known_youtube_past_broadcasts(&self) -> HashSet<&str> {
        self.youtube_results
            .iter()
            .chain(self.youtube_music_results.iter())
            .chain(
                self.subscription_video_cache
                    .values()
                    .flat_map(|cached| &cached.items),
            )
            .chain(
                self.pending_subscription_refresh
                    .as_ref()
                    .and_then(|pending| pending.staged_cache.as_ref())
                    .into_iter()
                    .flat_map(|cached| &cached.items),
            )
            .filter_map(|item| match item {
                SearchItem::Video(video) if video.was_live && !video.live => {
                    Some(video.video_id.as_str())
                }
                _ => None,
            })
            .chain(
                self.youtube_video_details_cache
                    .values()
                    .filter(|video| video.was_live && !video.live)
                    .map(|video| video.video_id.as_str()),
            )
            .collect()
    }

    /// Preserves confirmed history in provider pages without treating missing metadata as proof.
    pub(super) fn merge_youtube_broadcast_summaries(&mut self, items: &mut [SearchItem]) {
        let known = self.known_youtube_past_broadcasts();
        let updates = items
            .iter_mut()
            .filter_map(|item| {
                let SearchItem::Video(video) = item else {
                    return None;
                };
                video.was_live =
                    !video.live && (video.was_live || known.contains(video.video_id.as_str()));
                Some((video.video_id.as_str(), (video.live, video.was_live)))
            })
            .collect();
        self.remember_youtube_broadcasts(&updates);
    }

    /// Normalizes detail metadata before it replaces summaries or a full-details cache entry.
    pub(super) fn merge_youtube_broadcast_details(&mut self, details: &mut VideoDetails) {
        details.was_live = !details.live
            && (details.was_live
                || self
                    .known_youtube_past_broadcasts()
                    .contains(details.video_id.as_str()));
        self.remember_youtube_broadcasts(&HashMap::from([(
            details.video_id.as_str(),
            (details.live, details.was_live),
        )]));
    }

    /// Synchronizes cached evidence so a current broadcast cannot resurrect stale past flags.
    ///
    /// Only flags are changed here: page ordering, byte budgets, and full detail replacement
    /// remain owned by their existing response handlers. Persistent writes are batched.
    fn remember_youtube_broadcasts(&mut self, updates: &HashMap<&str, (bool, bool)>) {
        if updates.is_empty() {
            return;
        }
        for item in self
            .youtube_results
            .iter_mut()
            .chain(self.youtube_music_results.iter_mut())
        {
            update_summary(item, updates);
        }
        for details in self.youtube_video_details_cache.values_mut() {
            if let Some(&(live, was_live)) = updates.get(details.video_id.as_str()) {
                details.live = live;
                details.was_live = was_live;
            }
        }
        let mut changed_channels = Vec::new();
        for (channel_id, cached) in &mut self.subscription_video_cache {
            let mut changed = false;
            for item in &mut cached.items {
                changed |= update_summary(item, updates);
            }
            if changed {
                changed_channels.push(channel_id.clone());
            }
        }
        if let Some(staged) = self
            .pending_subscription_refresh
            .as_mut()
            .and_then(|pending| pending.staged_cache.as_mut())
        {
            for item in &mut staged.items {
                update_summary(item, updates);
            }
        }
        for channel_id in changed_channels {
            if let Err(error) = self.persist_subscription_video_snapshot(&channel_id) {
                self.show_error("Could not cache the YouTube broadcast state", &error);
            }
        }
        if let Err(error) = self.persist_youtube_broadcasts(updates) {
            self.show_error("Could not cache the YouTube broadcast state", &error);
        }
    }

    /// Updates flags in an existing saved search without storing detail payloads or new searches.
    fn persist_youtube_broadcasts(
        &self,
        updates: &HashMap<&str, (bool, bool)>,
    ) -> Result<(), PersistenceError> {
        if !self.config.persistence.save_playback_history {
            return Ok(());
        }
        let Some(mut saved) = self.store.youtube_search()? else {
            return Ok(());
        };
        let mut changed = false;
        for item in &mut saved.results {
            changed |= update_summary(item, updates);
        }
        if changed {
            self.save_youtube_search(&saved, unix_time())?;
        }
        Ok(())
    }
}

/// Applies one known broadcast state without changing any other summary field.
fn update_summary(item: &mut SearchItem, updates: &HashMap<&str, (bool, bool)>) -> bool {
    let SearchItem::Video(VideoSummary {
        video_id,
        live,
        was_live,
        ..
    }) = item
    else {
        return false;
    };
    let Some(&(updated_live, updated_was_live)) = updates.get(video_id.as_str()) else {
        return false;
    };
    let changed = (*live, *was_live) != (updated_live, updated_was_live);
    *live = updated_live;
    *was_live = updated_was_live;
    changed
}
