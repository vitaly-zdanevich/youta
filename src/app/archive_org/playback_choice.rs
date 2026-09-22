//! Snapshot-owned Archive playback selection, independent of download settings.

use super::*;
use crate::config::ArchivePlaybackPreference;
use crate::providers::archive_org::{ArchiveOrgFileProvenance, preferred_audio_variant};
use crate::view::ArchivePlaybackChoicePopupView;

/// Defers queue mutation until the user confirms an exact source.
#[derive(Clone, Copy)]
pub(in crate::app) enum ArchivePlaybackOwner {
    /// Direct activation and EOF continuation use the usual playback path.
    Append,
    /// Captured queue-edge insertion point remains stable if the prior track reaches EOF.
    ManualStep { insert_at: usize },
}

/// Owns bounded provider metadata and authoritative URLs while the view shows labels only.
pub(super) struct PendingArchivePlaybackChoice {
    generation: u64,
    details: Arc<ArchiveOrgItemDetails>,
    index: usize,
    options: Vec<ArchiveOrgDownloadVariant>,
    owner: ArchivePlaybackOwner,
}

/// Reports a genuine original video only when its provenance is explicit.
fn original_video(track: &ArchiveOrgTrack) -> Option<&ArchiveOrgDownloadVariant> {
    track.download_variants.iter().find(|variant| {
        variant.is_video && variant.provenance == ArchiveOrgFileProvenance::Original
    })
}

/// Resolves a source mode without side effects; EOF and manual stepping share this policy.
pub(in crate::app) fn playback_step(
    details: &Arc<ArchiveOrgItemDetails>,
    index: usize,
    preference: ArchivePlaybackPreference,
) -> AutoplayStep {
    let Some(track) = details.tracks.get(index) else {
        return AutoplayStep::Exhausted;
    };
    let audio = preferred_audio_variant(track);
    if preference == ArchivePlaybackPreference::AskEachTime
        && original_video(track).is_some()
        && audio.is_some()
    {
        return AutoplayStep::ChooseArchivePlayback {
            details: Arc::clone(details),
            index,
        };
    }
    let mut item = queue_item(&details.item, track);
    if preference == ArchivePlaybackPreference::AudioOnly {
        let Some(variant) = audio else {
            return AutoplayStep::ArchiveAudioUnavailable;
        };
        apply_variant(&mut item, variant);
    } else if preference == ArchivePlaybackPreference::OriginalFile
        && let Some(variant) = original_video(track).or_else(|| {
            audio.filter(|variant| variant.provenance == ArchiveOrgFileProvenance::Original)
        })
    {
        // Older metadata snapshots may still name an audio derivative as their
        // default, even when their authoritative inventory contains the original.
        apply_variant(&mut item, variant);
    }
    AutoplayStep::Play {
        item: Box::new(item),
        origin: AutoplayOrigin::ArchiveOrg {
            details: Arc::clone(details),
            index,
            preference,
        },
    }
}

/// Preserves title, duration and waveform while making every replay identity exact.
fn apply_variant(item: &mut QueueItem, variant: &ArchiveOrgDownloadVariant) {
    item.media.id = MediaId::new(SourceKind::ArchiveOrg, variant.download_url.as_str());
    item.media.webpage_url = variant.download_url.clone();
    item.playback_location = variant.download_url.to_string();
}

impl AppController {
    /// Starts or prompts for a bounded Archive metadata snapshot, never the current UI row.
    pub(in crate::app) fn begin_archive_playback(
        &mut self,
        details: Arc<ArchiveOrgItemDetails>,
        index: usize,
        preference: ArchivePlaybackPreference,
        owner: ArchivePlaybackOwner,
    ) {
        match playback_step(&details, index, preference) {
            AutoplayStep::Play { item, origin } => {
                self.start_archive_playback_item(*item, origin, owner);
            }
            AutoplayStep::ChooseArchivePlayback { details, index } => {
                self.open_archive_playback_choices(details, index, owner);
            }
            AutoplayStep::ArchiveAudioUnavailable => self.archive_audio_unavailable(),
            _ => {}
        }
    }

    /// Builds only available original-video and audio-only rows with their reported sizes.
    pub(in crate::app) fn open_archive_playback_choices(
        &mut self,
        details: Arc<ArchiveOrgItemDetails>,
        index: usize,
        owner: ArchivePlaybackOwner,
    ) {
        let Some(track) = details.tracks.get(index) else {
            return;
        };
        let Some(original) = original_video(track) else {
            return;
        };
        let options = std::iter::once(original.clone())
            .chain(
                track
                    .download_variants
                    .iter()
                    .filter(|variant| !variant.is_video)
                    .cloned(),
            )
            .collect::<Vec<_>>();
        self.archive_org.playback_choice_generation =
            self.archive_org.playback_choice_generation.wrapping_add(1);
        let generation = self.archive_org.playback_choice_generation;
        self.view.archive_playback_choice_popup = Some(ArchivePlaybackChoicePopupView {
            generation,
            title: "Play from archive.org".to_owned(),
            explanation: format!(
                "{}\nChoose the original video (audio playback) or an existing audio-only file. Video containers may transfer video data too.",
                track.title
            ),
            options: options
                .iter()
                .map(|variant| {
                    let kind = if variant.is_video {
                        "Original video"
                    } else {
                        "Audio-only"
                    };
                    let size = variant
                        .size_bytes
                        .map_or_else(|| "size unknown".to_owned(), human_bytes);
                    format!("{kind}: {} · {size} · {}", variant.format, variant.filename)
                })
                .collect(),
            selected: 0,
        });
        self.archive_org.playback_choice = Some(PendingArchivePlaybackChoice {
            generation,
            details,
            index,
            options,
            owner,
        });
        self.view.status_line = "Choose an archive.org playback file".to_owned();
    }

    /// Moves within authoritative option bounds without changing the playback source.
    pub(in crate::app) fn move_archive_playback_choice(&mut self, direction: i32) {
        if let Some(popup) = self.view.archive_playback_choice_popup.as_mut() {
            popup.selected = popup
                .selected
                .saturating_add_signed(direction as isize)
                .min(popup.options.len().saturating_sub(1));
        }
    }

    /// Consumes one matching generation exactly once and retains the selected mode for autoplay.
    pub(in crate::app) fn confirm_archive_playback_choice(
        &mut self,
        generation: u64,
        clicked: Option<usize>,
    ) {
        let Some(popup) = &self.view.archive_playback_choice_popup else {
            return;
        };
        let Some(pending) = &self.archive_org.playback_choice else {
            return;
        };
        let selected = clicked.unwrap_or(popup.selected);
        if generation != pending.generation
            || popup.generation != generation
            || selected >= pending.options.len()
        {
            return;
        }
        let pending = self
            .archive_org
            .playback_choice
            .take()
            .expect("validated pending choice");
        self.view.archive_playback_choice_popup = None;
        let variant = &pending.options[selected];
        let mut item = queue_item(
            &pending.details.item,
            &pending.details.tracks[pending.index],
        );
        apply_variant(&mut item, variant);
        let preference = if variant.is_video {
            ArchivePlaybackPreference::OriginalFile
        } else {
            ArchivePlaybackPreference::AudioOnly
        };
        self.start_archive_playback_item(
            item,
            AutoplayOrigin::ArchiveOrg {
                details: pending.details,
                index: pending.index,
                preference,
            },
            pending.owner,
        );
    }

    /// Revokes the pending snapshot; dismissal cannot start playback or alter the queue.
    pub(in crate::app) fn dismiss_archive_playback_choice(&mut self) {
        self.cancel_archive_playback_choice();
        self.view.status_line = "Archive playback choice cancelled".to_owned();
    }

    /// Invalidates obsolete confirmations when another source starts playing.
    pub(in crate::app) fn cancel_archive_playback_choice(&mut self) {
        self.archive_org.playback_choice = None;
        self.view.archive_playback_choice_popup = None;
    }

    /// Stops selection rather than silently transferring video in audio-only mode.
    pub(in crate::app) fn archive_audio_unavailable(&mut self) {
        self.cancel_archive_playback_choice();
        self.view.status_line =
            "No available audio-only file for this Archive track; refresh metadata or choose Original file in Preferences".to_owned();
    }

    /// Applies a confirmed source using the same queue placement as its initiating action.
    fn start_archive_playback_item(
        &mut self,
        item: QueueItem,
        origin: AutoplayOrigin,
        owner: ArchivePlaybackOwner,
    ) {
        let positioned = match owner {
            ArchivePlaybackOwner::Append => false,
            ArchivePlaybackOwner::ManualStep { insert_at } => {
                self.queued_autoplay_resume_origin = None;
                let index = insert_at.min(self.playback_queue.items.len());
                self.playback_queue.items.insert(index, item.clone());
                self.playback_queue.current_index = Some(index);
                true
            }
        };
        self.play_queue_item_with_origin(item, positioned, Some(origin));
    }
}
