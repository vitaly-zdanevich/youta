//! Explicit native VK identification sharing Local's serialized AcoustID rate limiter.

use super::*;

/// Remote identification stages bounded audio only for an explicit action.
const MAX_AUDIO_BYTES: u64 = 128 * 1024 * 1024;
const MAX_STAGING_TIME: Duration = Duration::from_secs(120);
const MAX_CACHED_TRACKS: usize = 32;

/// Public recording identity and the exact explicit import that owns its session.
#[derive(Clone, Eq, Hash, PartialEq)]
struct Owner {
    source: url::Url,
    session: vk::VkExportOwner,
}

/// Exact public identity and session ownership, without cloning a client during projection.
struct SelectedSource {
    owner: Owner,
}

/// One source-owned result, with no cookies, signed locators or staged bytes.
struct Completion {
    generation: u64,
    owner: Owner,
    result: Result<Vec<MusicBrainzCandidate>, String>,
}

/// Work submitted to the existing serialized Local identification worker.
pub(super) struct Request {
    generation: u64,
    source: SelectedSource,
    /// Immutable provider record and matching authenticated client, never a title-search fallback.
    export_source: crate::archive_upload_media::VkExportSource,
    ffmpeg: PathBuf,
    pub(super) cancellation: AudioIdentificationCancellation,
    responses: ResponseSender<Completion>,
}

/// Cancellation and publication remain owned by the current selected source.
struct Pending {
    generation: u64,
    owner: Owner,
    cancellation: AudioIdentificationCancellation,
}

/// Bounded process-local presentation, independent of authenticated catalogue work.
pub(super) struct State {
    generation: u64,
    pending: Option<Pending>,
    results: HashMap<Owner, Vec<MusicBrainzCandidate>>,
    order: VecDeque<Owner>,
    owned_links: Vec<(String, String)>,
    sender: ResponseSender<Completion>,
    receiver: Receiver<Completion>,
}

impl State {
    /// Connects completion notifications without starting requests or reading cookies.
    pub(super) fn new(notifier: WorkerNotifier) -> Self {
        let (sender, receiver) = unbounded();
        Self {
            generation: 0,
            pending: None,
            results: HashMap::new(),
            order: VecDeque::new(),
            owned_links: Vec::new(),
            sender: ResponseSender::new(sender, notifier),
            receiver,
        }
    }

    /// Remembers confirmed results only for the exact recording and imported session.
    fn remember(&mut self, owner: Owner, candidates: Vec<MusicBrainzCandidate>) {
        self.order.retain(|entry| entry != &owner);
        while self.results.len() >= MAX_CACHED_TRACKS && !self.results.contains_key(&owner) {
            if let Some(oldest) = self.order.pop_front() {
                self.results.remove(&oldest);
            } else {
                break;
            }
        }
        self.order.push_back(owner.clone());
        self.results.insert(owner, candidates);
    }
}

impl AppController {
    /// Accepts matching selected VK metadata with an exact public audio URL.
    fn selected_vk_fingerprint_source(&self) -> Option<SelectedSource> {
        if self.view.screen != Screen::Vk {
            return None;
        }
        let selected = self.view.rows.get(self.view.selected)?.media_id.as_ref()?;
        let details = self.view.details.as_ref()?;
        let source = details.webpage_url.as_ref()?;
        let id = crate::providers::vk::VkAudioId::from_page_url(source).ok()?;
        let session = self.vk_export_owner()?;
        let item = self.selected_vk_queue_item().ok()?;
        if selected.source != SourceKind::Vk
            || selected.external_id != id.to_string()
            || details.media_id.as_ref() != Some(selected)
            || *source != id.webpage_url()
            || item.media.id != *selected
            || item.media.webpage_url != *source
        {
            return None;
        }
        Some(SelectedSource {
            owner: Owner {
                source: source.clone(),
                session,
            },
        })
    }

    /// Projects explicit lookup results while preserving album, artist and original links.
    pub(super) fn apply_vk_fingerprint_details(&mut self) {
        let selected = self.selected_vk_fingerprint_source();
        if self.vk_fingerprint.pending.as_ref().is_some_and(|pending| {
            selected
                .as_ref()
                .is_none_or(|source| source.owner != pending.owner)
        }) {
            self.cancel_vk_fingerprint();
        }
        if self.view.screen != Screen::Vk {
            return;
        }
        let state = &mut self.vk_fingerprint;
        let Some(details) = self.view.details.as_mut() else {
            state.owned_links.clear();
            return;
        };
        details.links.retain(|link| {
            !state.owned_links.iter().any(|(label, url)| {
                link.label == *label && link.url == *url && link.wikidata_item_id.is_none()
            })
        });
        state.owned_links.clear();
        details.local_fingerprint_available = selected.is_some();
        details.local_fingerprint_pending = state.pending.is_some();
        let Some(selected) = selected else {
            return;
        };
        let Some(candidates) = state.results.get(&selected.owner) else {
            return;
        };
        for (index, candidate) in candidates.iter().enumerate() {
            let label = format!(
                "MusicBrainz recording {} · {:.1}% match",
                index.saturating_add(1),
                candidate.score() * 100.0
            );
            let url = candidate.url().to_string();
            if details.links.iter().any(|link| link.url == url) {
                continue;
            }
            state.owned_links.push((label.clone(), url.clone()));
            details.links.push(DetailLinkView {
                label,
                url,
                ..DetailLinkView::default()
            });
        }
    }

    /// Revokes only this source; the common worker owns queue draining and shutdown.
    pub(super) fn cancel_vk_fingerprint(&mut self) {
        if let Some(pending) = self.vk_fingerprint.pending.take() {
            pending.cancellation.cancel();
        }
    }

    /// Replacing credentials cannot revive cached or in-flight results from an older import.
    pub(super) fn clear_vk_fingerprint_session(&mut self) {
        self.cancel_vk_fingerprint();
        self.vk_fingerprint.results.clear();
        self.vk_fingerprint.order.clear();
    }

    /// Shares the existing activity animation only while explicit VK work is pending.
    pub(super) fn vk_fingerprint_pending(&self) -> bool {
        self.vk_fingerprint.pending.is_some()
    }

    /// Starts native staging only after the user invokes the fingerprint action.
    pub(super) fn fingerprint_selected_vk_audio(&mut self) {
        let Some(source) = self.selected_vk_fingerprint_source() else {
            return;
        };
        let Some(export_source) = self
            .selected_vk_queue_item()
            .ok()
            .and_then(|item| self.capture_vk_export_source(&item))
        else {
            return;
        };
        if self
            .vk_fingerprint
            .pending
            .as_ref()
            .is_some_and(|pending| pending.owner == source.owner)
        {
            return;
        }
        if self.config.providers.acoustid_client_key.is_none() {
            self.show_actionable_message("AcoustID API key required", format!(
				"Create an AcoustID application key:\nhttps://acoustid.org/api-key\n\nAdd it to:\n{}\n\n[providers]\nacoustid_client_key = '...'\n\nThen restart Youta.",
				self.config.credentials_file().display(),
			));
            return;
        }
        self.cancel_local_fingerprint();
        let Some(sender) = self.local_fingerprint_requests.clone() else {
            self.show_actionable_message(
                "Fingerprinting unavailable",
                "The fingerprint worker is unavailable. Restart Youta and try again.",
            );
            return;
        };
        let state = &mut self.vk_fingerprint;
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        let owner = source.owner.clone();
        let cancellation = AudioIdentificationCancellation::new();
        let mut command = LocalFingerprintWorkerRequest::IdentifyVk(Request {
            generation,
            source,
            export_source,
            ffmpeg: self.config.providers.ffmpeg_executable.clone(),
            cancellation: cancellation.clone(),
            responses: state.sender.clone(),
        });
        loop {
            match sender.try_send(command) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) => {
                    if let Ok(stale) = self.local_fingerprint_request_drain.try_recv() {
                        stale.cancel();
                    }
                    command = returned;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.show_actionable_message(
                        "Fingerprinting unavailable",
                        "The fingerprint worker stopped. Restart Youta and try again.",
                    );
                    return;
                }
            }
        }
        state.results.remove(&owner);
        state.pending = Some(Pending {
            generation,
            owner,
            cancellation,
        });
        self.view.local_fingerprint_animation_frame = 0;
        self.apply_vk_fingerprint_details();
        self.view.status_line = "Identifying VK audio...".to_owned();
    }

    /// Publishes only the still-selected recording and import, leaving hidden panels untouched.
    pub(super) fn drain_vk_fingerprint_responses(&mut self) {
        let selected = self.selected_vk_fingerprint_source();
        if self.vk_fingerprint.pending.as_ref().is_some_and(|pending| {
            selected
                .as_ref()
                .is_none_or(|source| source.owner != pending.owner)
        }) {
            self.cancel_vk_fingerprint();
        }
        while let Ok(response) = self.vk_fingerprint.receiver.try_recv() {
            let Some(pending) = self.vk_fingerprint.pending.as_ref() else {
                continue;
            };
            if pending.generation != response.generation || pending.owner != response.owner {
                continue;
            }
            let pending = self
                .vk_fingerprint
                .pending
                .take()
                .expect("matching VK fingerprint");
            self.view.local_fingerprint_animation_frame = 0;
            if pending.cancellation.is_cancelled()
                || selected
                    .as_ref()
                    .is_none_or(|source| source.owner != pending.owner)
            {
                continue;
            }
            match response.result {
                Ok(candidates) => {
                    self.view.status_line = if candidates.is_empty() {
                        "AcoustID found no linked MusicBrainz recording".to_owned()
                    } else {
                        format!(
                            "Found {} MusicBrainz recording candidates",
                            candidates.len()
                        )
                    };
                    self.vk_fingerprint.remember(response.owner, candidates);
                }
                Err(error) => self.show_actionable_message("VK identification failed", error),
            }
        }
        self.apply_vk_fingerprint_details();
    }
}

/// Native VK staging and fpcalc share the common worker's serialized lookup limiter.
pub(super) fn identify(request: Request, identifier: &mut dyn AudioIdentifier) {
    let result = identify_with_staging(&request.cancellation, identifier, |directory| {
        crate::archive_upload_media::vk::prepare_bounded_session_opus(
            &request.export_source,
            directory,
            &request.ffmpeg,
            request.cancellation.as_atomic(),
            MAX_AUDIO_BYTES,
            MAX_STAGING_TIME,
            |_, _| {},
        )
    });
    let _ = request.responses.send(Completion {
        generation: request.generation,
        owner: request.source.owner,
        result,
    });
}

/// Keeps staging private until fpcalc returns, with cleanup on success, error or cancellation.
fn identify_with_staging(
    cancellation: &AudioIdentificationCancellation,
    identifier: &mut dyn AudioIdentifier,
    stage: impl FnOnce(&Path) -> Result<PathBuf, String>,
) -> Result<Vec<MusicBrainzCandidate>, String> {
    check_cancelled(cancellation)?;
    let mut builder = tempfile::Builder::new();
    builder.prefix("youta-vk-identify-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    let directory = builder
        .tempdir()
        .map_err(|_| "Cannot create private VK identification staging")?;
    let path = stage(directory.path())?;
    check_cancelled(cancellation)?;
    let candidates = identifier
        .identify(&path, cancellation)
        .map_err(|error| error.to_string())?;
    check_cancelled(cancellation)?;
    Ok(candidates)
}

/// Prevents queued or obsolete work from reaching another external boundary.
fn check_cancelled(cancellation: &AudioIdentificationCancellation) -> Result<(), String> {
    if cancellation.is_cancelled() {
        Err("VK identification cancelled".to_owned())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
