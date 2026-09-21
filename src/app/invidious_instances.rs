//! On-demand instance selection with one bounded, generation-owned HTTP worker.
//!
//! Closing a chooser invalidates its result without blocking the UI. A request
//! already in progress may finish within the provider's deadline; reopening
//! queues only the latest request, never an additional concurrent worker.

use super::*;
#[cfg(feature = "invidious")]
use crate::view::{InvidiousInstancePickerView, InvidiousInstanceView};

#[cfg(feature = "invidious")]
type DirectoryResult = Result<Vec<InvidiousInstanceView>, String>;
#[cfg(feature = "invidious")]
type DirectoryLoader = dyn Fn() -> DirectoryResult + Send + Sync;

/// In-flight work and a single replaceable pending request; no startup fetch.
#[cfg(feature = "invidious")]
pub(super) struct InstanceDirectoryState {
    generation: u64,
    pending: bool,
    worker: Option<DirectoryWorker>,
    loader: Arc<DirectoryLoader>,
}

/// A single bounded response; dropping the controller never waits for HTTP.
#[cfg(feature = "invidious")]
struct DirectoryWorker {
    generation: u64,
    response: Receiver<DirectoryResult>,
    thread: JoinHandle<()>,
}

#[cfg(feature = "invidious")]
impl Default for InstanceDirectoryState {
    fn default() -> Self {
        Self {
            generation: 0,
            pending: false,
            worker: None,
            loader: Arc::new(|| {
                crate::providers::invidious_instances::InvidiousInstancesClient::new()
                    .fetch()
                    .map(|instances| {
                        instances
                            .into_iter()
                            .map(|instance| InvidiousInstanceView {
                                url: instance.url.to_string(),
                                label: instance.label,
                            })
                            .collect()
                    })
                    .map_err(|error| format!("Could not load the instance directory: {error}"))
            }),
        }
    }
}

impl AppController {
    /// Opens a fresh directory snapshot only in response to a user action.
    pub(super) fn open_invidious_instance_picker(&mut self) {
        #[cfg(feature = "invidious")]
        {
            let Some(popup) = self.view.youtube_setup_popup.as_mut() else {
                return;
            };
            // Repeated taps while loading do not enqueue duplicate requests.
            if popup
                .invidious_instances
                .as_ref()
                .is_some_and(|list| list.loading)
            {
                return;
            }
            popup.selected_field = YouTubeSetupField::InvidiousUrl;
            popup.validation_error = None;
            popup.invidious_instances = Some(InvidiousInstancePickerView {
                loading: true,
                ..Default::default()
            });
            self.invidious_instances.generation =
                self.invidious_instances.generation.wrapping_add(1);
            self.invidious_instances.pending = true;
            self.poll_invidious_instances();
        }
        #[cfg(not(feature = "invidious"))]
        self.set_youtube_setup_error("This build omits Invidious support");
    }

    /// Invalidates queued and in-flight results without changing the URL draft.
    pub(super) fn dismiss_invidious_instance_picker(&mut self) {
        if let Some(popup) = self.view.youtube_setup_popup.as_mut() {
            popup.invidious_instances = None;
        }
        #[cfg(feature = "invidious")]
        {
            self.invidious_instances.generation =
                self.invidious_instances.generation.wrapping_add(1);
            self.invidious_instances.pending = false;
        }
    }

    /// Clamps keyboard navigation to real candidates, including empty lists.
    pub(super) fn move_invidious_instance(&mut self, delta: i32) {
        if let Some(list) = self
            .view
            .youtube_setup_popup
            .as_mut()
            .and_then(|popup| popup.invidious_instances.as_mut())
        {
            let distance = usize::try_from(delta.unsigned_abs()).unwrap_or(usize::MAX);
            list.selected = if delta < 0 {
                list.selected.saturating_sub(distance)
            } else {
                list.selected.saturating_add(distance)
            }
            .min(list.instances.len().saturating_sub(1));
        }
    }

    /// Copies a validated directory candidate into the draft, never to disk.
    pub(super) fn select_invidious_instance(&mut self, index: usize) {
        let Some(popup) = self.view.youtube_setup_popup.as_mut() else {
            return;
        };
        let Some(instance) = popup
            .invidious_instances
            .as_ref()
            .filter(|list| !list.loading)
            .and_then(|list| list.instances.get(index))
        else {
            return;
        };
        popup.invidious_url.clone_from(&instance.url);
        popup.selected_field = YouTubeSetupField::InvidiousUrl;
        popup.validation_error = None;
        self.dismiss_invidious_instance_picker();
    }

    /// Accepts only an existing highlighted candidate, leaving failed/loading lists open.
    pub(super) fn confirm_invidious_instance(&mut self) {
        if let Some(index) = self
            .view
            .youtube_setup_popup
            .as_ref()
            .and_then(|popup| popup.invidious_instances.as_ref())
            .map(|list| list.selected)
        {
            self.select_invidious_instance(index);
        }
    }

    /// Polls without waiting, retires finished workers, and advances the spinner.
    #[cfg(feature = "invidious")]
    pub(super) fn poll_invidious_instances(&mut self) {
        if let Some(list) = self
            .view
            .youtube_setup_popup
            .as_mut()
            .and_then(|popup| popup.invidious_instances.as_mut())
            .filter(|list| list.loading)
        {
            list.loading_frame = list.loading_frame.wrapping_add(1);
        }
        if self
            .invidious_instances
            .worker
            .as_ref()
            .is_some_and(|worker| worker.thread.is_finished())
        {
            let worker = self
                .invidious_instances
                .worker
                .take()
                .expect("finished worker");
            let result = worker.response.try_recv().unwrap_or_else(|_| {
                Err("The instance directory worker stopped; retry the list".to_owned())
            });
            let _ = worker.thread.join();
            if worker.generation == self.invidious_instances.generation
                && let Some(popup) = self.view.youtube_setup_popup.as_mut()
                && let Some(list) = popup.invidious_instances.as_mut()
            {
                list.loading = false;
                match result {
                    Ok(instances) => {
                        list.selected = instances
                            .iter()
                            .position(|instance| {
                                instance.url.trim_end_matches('/')
                                    == popup.invidious_url.trim_end_matches('/')
                            })
                            .unwrap_or(0);
                        list.instances = instances;
                    }
                    Err(error) => list.error = Some(error),
                }
            }
        }
        if !self.invidious_instances.pending || self.invidious_instances.worker.is_some() {
            return;
        }
        self.invidious_instances.pending = false;
        let generation = self.invidious_instances.generation;
        let loader = Arc::clone(&self.invidious_instances.loader);
        let (sender, response) = bounded(1);
        match thread::Builder::new()
            .name("youta-invidious-directory".to_owned())
            .spawn(move || {
                let _ = sender.try_send(loader());
            }) {
            Ok(thread) => {
                self.invidious_instances.worker = Some(DirectoryWorker {
                    generation,
                    response,
                    thread,
                });
            }
            Err(_) => {
                if let Some(list) = self
                    .view
                    .youtube_setup_popup
                    .as_mut()
                    .and_then(|popup| popup.invidious_instances.as_mut())
                {
                    list.loading = false;
                    list.error = Some(
                        "Could not start the instance directory worker; retry the list".to_owned(),
                    );
                }
            }
        }
    }
}

#[cfg(all(test, not(feature = "invidious")))]
#[test]
fn invidious_picker_without_feature_keeps_manual_setup_and_reports_unavailable() {
    let dir = crate::test_support::canonical_tempdir("invidious disabled");
    let config = Config::for_dir(dir.path().join("config"));
    let mut controller =
        AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
    controller.open_youtube_setup();
    controller.dispatch(UiAction::OpenInvidiousInstancePicker);
    let popup = controller.view.youtube_setup_popup.as_ref().unwrap();
    assert!(popup.invidious_instances.is_none());
    assert_eq!(
        popup.validation_error.as_deref(),
        Some("This build omits Invidious support")
    );
    assert!(!controller.config.config_file().exists());
}

#[cfg(all(test, feature = "invidious"))]
mod tests {
    use super::*;

    /// Uses only channel-backed mock requests; no network or stored credentials.
    fn fixture() -> (tempfile::TempDir, AppController) {
        let dir = crate::test_support::canonical_tempdir("invidious chooser");
        let config = Config::for_dir(dir.path().join("config"));
        let controller =
            AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
        (dir, controller)
    }

    /// Constructs a deterministic candidate without performing a DNS lookup.
    fn candidate(domain: &str) -> InvidiousInstanceView {
        InvidiousInstanceView {
            label: domain.to_owned(),
            url: format!("https://{domain}/"),
        }
    }

    /// A deadline diagnoses stuck worker tests without relying on scheduler timing.
    fn poll_until(controller: &mut AppController, condition: impl Fn(&AppController) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition(controller) {
            assert!(
                Instant::now() < deadline,
                "mock directory worker did not finish"
            );
            controller.poll_invidious_instances();
            thread::yield_now();
        }
    }

    #[test]
    fn invidious_picker_is_lazy_animated_and_selection_only_edits_draft() {
        let (_dir, mut controller) = fixture();
        let (calls_tx, calls) = bounded(4);
        let (reply, replies) = bounded(1);
        controller.invidious_instances.loader = Arc::new(move || {
            calls_tx.send(()).unwrap();
            replies.recv_timeout(Duration::from_secs(5)).unwrap()
        });
        controller.open_youtube_setup();
        controller.poll_invidious_instances();
        assert!(calls.is_empty());
        controller.dispatch(UiAction::OpenInvidiousInstancePicker);
        calls.recv_timeout(Duration::from_secs(5)).unwrap();
        let first_frame = controller
            .view
            .youtube_setup_popup
            .as_ref()
            .unwrap()
            .invidious_instances
            .as_ref()
            .unwrap()
            .loading_frame;
        controller.tick();
        assert_ne!(
            first_frame,
            controller
                .view
                .youtube_setup_popup
                .as_ref()
                .unwrap()
                .invidious_instances
                .as_ref()
                .unwrap()
                .loading_frame
        );
        controller.dispatch(UiAction::OpenInvidiousInstancePicker);
        controller.dispatch(UiAction::SubmitYouTubeSetup);
        assert!(
            controller
                .view
                .youtube_setup_popup
                .as_ref()
                .unwrap()
                .invidious_instances
                .is_some()
        );
        reply
            .send(Ok(vec![candidate("a.example"), candidate("b.example")]))
            .unwrap();
        poll_until(&mut controller, |controller| {
            !controller
                .view
                .youtube_setup_popup
                .as_ref()
                .unwrap()
                .invidious_instances
                .as_ref()
                .unwrap()
                .loading
        });
        assert!(calls.is_empty());
        controller.dispatch(UiAction::MoveInvidiousInstance(i32::MAX));
        controller.dispatch(UiAction::ConfirmInvidiousInstance);
        let popup = controller.view.youtube_setup_popup.as_ref().unwrap();
        assert_eq!(popup.invidious_url, "https://b.example/");
        assert_eq!(popup.selected_field, YouTubeSetupField::InvidiousUrl);
        assert!(popup.invidious_instances.is_none());
        assert!(!controller.config.config_file().exists());
        assert!(!controller.youtube_provider_available);
    }

    #[test]
    fn invidious_picker_reopen_discards_stale_results_and_bounds_concurrency() {
        let (_dir, mut controller) = fixture();
        let (calls_tx, calls) = bounded(4);
        let (reply, replies) = bounded(4);
        controller.invidious_instances.loader = Arc::new(move || {
            calls_tx.send(()).unwrap();
            replies.recv_timeout(Duration::from_secs(5)).unwrap()
        });
        controller.open_youtube_setup();
        controller
            .view
            .youtube_setup_popup
            .as_mut()
            .unwrap()
            .invidious_url = "https://private.example/".to_owned();
        controller.dispatch(UiAction::OpenInvidiousInstancePicker);
        calls.recv_timeout(Duration::from_secs(5)).unwrap();
        controller.dispatch(UiAction::DismissYouTubeSetup);
        controller.open_youtube_setup();
        controller.dispatch(UiAction::OpenInvidiousInstancePicker);
        assert!(
            calls.is_empty(),
            "reopening must not spawn a parallel request"
        );
        reply.send(Ok(vec![candidate("stale.example")])).unwrap();
        poll_until(&mut controller, |_| !calls.is_empty());
        calls.try_recv().unwrap();
        assert!(
            controller
                .view
                .youtube_setup_popup
                .as_ref()
                .unwrap()
                .invidious_instances
                .as_ref()
                .unwrap()
                .instances
                .is_empty()
        );
        reply.send(Ok(vec![candidate("fresh.example")])).unwrap();
        poll_until(&mut controller, |controller| {
            !controller
                .view
                .youtube_setup_popup
                .as_ref()
                .unwrap()
                .invidious_instances
                .as_ref()
                .unwrap()
                .loading
        });
        controller.dispatch(UiAction::SelectInvidiousInstance(99));
        assert!(
            controller
                .view
                .youtube_setup_popup
                .as_ref()
                .unwrap()
                .invidious_url
                .is_empty()
        );
        controller.dispatch(UiAction::SelectInvidiousInstance(0));
        assert_eq!(
            controller
                .view
                .youtube_setup_popup
                .as_ref()
                .unwrap()
                .invidious_url,
            "https://fresh.example/"
        );
    }

    #[test]
    fn invidious_picker_failure_empty_and_cancel_preserve_manual_url_and_allow_retry() {
        let (_dir, mut controller) = fixture();
        let (reply, replies) = bounded(3);
        controller.invidious_instances.loader =
            Arc::new(move || replies.recv_timeout(Duration::from_secs(5)).unwrap());
        controller.open_youtube_setup();
        controller
            .view
            .youtube_setup_popup
            .as_mut()
            .unwrap()
            .invidious_url = "http://localhost:3000/".to_owned();
        controller.dispatch(UiAction::OpenInvidiousInstancePicker);
        reply.send(Err("fixture failure".to_owned())).unwrap();
        poll_until(&mut controller, |controller| {
            !controller
                .view
                .youtube_setup_popup
                .as_ref()
                .unwrap()
                .invidious_instances
                .as_ref()
                .unwrap()
                .loading
        });
        assert_eq!(
            controller
                .view
                .youtube_setup_popup
                .as_ref()
                .unwrap()
                .invidious_instances
                .as_ref()
                .unwrap()
                .error
                .as_deref(),
            Some("fixture failure")
        );
        controller.dispatch(UiAction::OpenInvidiousInstancePicker);
        reply.send(Ok(Vec::new())).unwrap();
        poll_until(&mut controller, |controller| {
            !controller
                .view
                .youtube_setup_popup
                .as_ref()
                .unwrap()
                .invidious_instances
                .as_ref()
                .unwrap()
                .loading
        });
        controller.dispatch(UiAction::MoveInvidiousInstance(i32::MIN));
        controller.dispatch(UiAction::ConfirmInvidiousInstance);
        let list = controller
            .view
            .youtube_setup_popup
            .as_ref()
            .unwrap()
            .invidious_instances
            .as_ref()
            .unwrap();
        assert!(list.error.is_none());
        assert_eq!(list.selected, 0);
        controller.dispatch(UiAction::DismissInvidiousInstancePicker);
        let popup = controller.view.youtube_setup_popup.as_ref().unwrap();
        assert_eq!(popup.invidious_url, "http://localhost:3000/");
        assert!(popup.invidious_instances.is_none());
        assert!(!controller.config.config_file().exists());
    }
}
