//! Conditional, captured Funkwhale format choices without downloading audio first.

use super::*;
use crate::providers::funkwhale::FunkwhaleProvider;

/// Cancelled metadata calls remain counted until their bounded network operation exits.
static LIVE_LOOKUPS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Keeps repeated cancellation from creating unbounded retiring HTTP workers.
struct LookupPermit;

impl LookupPermit {
    fn acquire() -> Result<Self, String> {
        LIVE_LOOKUPS
            .fetch_update(AtomicOrdering::AcqRel, AtomicOrdering::Acquire, |count| {
                (count < 4).then_some(count + 1)
            })
            .map(|_| Self)
            .map_err(|_| {
                "Earlier Funkwhale format checks are still stopping; retry shortly".to_owned()
            })
    }
}

impl Drop for LookupPermit {
    fn drop(&mut self) {
        LIVE_LOOKUPS.fetch_sub(1, AtomicOrdering::AcqRel);
    }
}

/// Owns only a metadata result, never an audio locator or downloaded file.
pub(super) struct MetadataLookup {
    response: Receiver<Result<bool, String>>,
    cancellation: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl MetadataLookup {
    /// Inspects the captured original pod, leaving actual byte validation to the native downloader.
    fn start(item: &QueueItem, notifier: WorkerNotifier) -> Result<Self, String> {
        if item.media.kind != MediaKind::Audio
            || item.media.id.source != SourceKind::Funkwhale
            || item.media.webpage_url.as_str() != item.media.id.external_id
        {
            return Err("Select a canonical Funkwhale audio track".to_owned());
        }
        let pod = FunkwhaleProvider::instance_url_from_track_id(&item.media.id.external_id)
            .map_err(|_| "Select a canonical Funkwhale audio track".to_owned())?;
        let provider =
            FunkwhaleProvider::new(pod).map_err(|_| "Invalid original Funkwhale pod".to_owned())?;
        let permit = LookupPermit::acquire()?;
        let identity = item.media.id.external_id.clone();
        let cancellation = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&cancellation);
        let (sender, response) = bounded(1);
        let sender = ResponseSender::new(sender, notifier);
        let thread = thread::Builder::new()
            .name("youta-funkwhale-format".to_owned())
            .spawn(move || {
                let _permit = permit;
                if cancelled.load(AtomicOrdering::Acquire) {
                    return;
                }
                let result = provider
                    .download_source(&identity, true)
                    .map(|source| source.advertised_opus)
                    .map_err(|error| match error {
                        crate::providers::ProviderError::HttpStatus(status) => {
                            format!("Funkwhale audio metadata returned HTTP status {status}")
                        }
                        _ => "Could not inspect this track's public audio format".to_owned(),
                    });
                if !cancelled.load(AtomicOrdering::Acquire) {
                    let _ = sender.send(result);
                }
            })
            .map_err(|_| "Could not start the Funkwhale format check".to_owned())?;
        Ok(Self {
            response,
            cancellation,
            thread: Some(thread),
        })
    }

    /// Consumes a notified result without joining in-flight network work on the UI thread.
    fn poll(&mut self) -> Option<Result<bool, String>> {
        let result = match self.response.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => {
                Err("The Funkwhale format check stopped unexpectedly".to_owned())
            }
        };
        if let Some(thread) = self.thread.take() {
            crate::app::worker_notifications::reap_published_worker(thread);
        }
        Some(result)
    }
}

impl Drop for MetadataLookup {
    fn drop(&mut self) {
        self.cancellation.store(true, AtomicOrdering::Release);
        // Dropping an unfinished handle detaches only bounded, read-only metadata work.
        // Its permit remains held until that operation exits.
    }
}

impl AppController {
    /// Honors explicit formats; the default copy policy asks before any incompatible conversion.
    pub(super) fn choose_funkwhale_download(&mut self, item: QueueItem) {
        let format = match configured_download_format(&self.config.subscriptions.audio_format) {
            Ok(format) => format,
            Err(error) => {
                self.show_actionable_message("Download format is invalid", error);
                self.finish_manual_download_choice(false);
                return;
            }
        };
        if format != DownloadFormat::OpusWithoutTranscoding {
            let source = item.media.webpage_url.clone();
            self.launch_manual_download(item, source, format);
            return;
        }
        let lookup = match MetadataLookup::start(&item, self.worker_notifier.clone()) {
            Ok(lookup) => lookup,
            Err(error) => {
                self.show_actionable_message("Cannot inspect Funkwhale audio format", error);
                self.finish_manual_download_choice(false);
                return;
            }
        };
        self.download_choice_generation = self.download_choice_generation.wrapping_add(1);
        let generation = self.download_choice_generation;
        self.view.download_choice_popup = Some(DownloadChoicePopupView {
            generation,
            title: "Download from Funkwhale".to_owned(),
            explanation: format!("{}\nChecking available audio format...", item.media.title),
            options: Vec::new(),
            selected: 0,
        });
        self.pending_download_choice = Some(PendingDownloadChoice {
            generation,
            item,
            stage: DownloadChoiceStage::LoadingFunkwhale(lookup),
        });
        self.poll_funkwhale_download_choice();
    }

    /// Applies only the live popup's captured metadata; dismissing it revokes all later results.
    pub(super) fn poll_funkwhale_download_choice(&mut self) -> bool {
        let Some(pending) = self.pending_download_choice.as_mut() else {
            return false;
        };
        let DownloadChoiceStage::LoadingFunkwhale(lookup) = &mut pending.stage else {
            return false;
        };
        if self.view.quitting || self.shutdown_persistence_succeeded.is_some() {
            return true;
        }
        let Some(result) = lookup.poll() else {
            return true;
        };
        let pending = self
            .pending_download_choice
            .take()
            .expect("owned Funkwhale format check");
        self.view.download_choice_popup = None;
        match result {
            Ok(true) => {
                let source = pending.item.media.webpage_url.clone();
                self.launch_manual_download(
                    pending.item,
                    source,
                    DownloadFormat::OpusWithoutTranscoding,
                );
            }
            Ok(false) => {
                let source = pending.item.media.webpage_url.clone();
                self.show_download_choices(
                    pending.item,
                    "Download from Funkwhale",
                    "No existing Opus upload is advertised. Keep the original file unchanged, or explicitly convert its audio to Opus. This choice applies only to this download.",
                    vec![
                        DownloadOption::Ready {
                            label: "Original file (unchanged)".to_owned(),
                            source_url: source.clone(),
                            format: DownloadFormat::OriginalBestAudio,
                        },
                        DownloadOption::Ready {
                            label: "Convert to Opus (re-encode)".to_owned(),
                            source_url: source,
                            format: DownloadFormat::TranscodeToOpus,
                        },
                    ],
                );
            }
            Err(error) => {
                self.show_actionable_message("Cannot inspect Funkwhale audio format", error);
                self.finish_manual_download_choice(false);
            }
        }
        true
    }
}
