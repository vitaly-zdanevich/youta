//! Provider-filtered, explicit shell buttons and download-bound command continuations.

mod image;

use super::*;
use crate::download_queue::DownloadQueueState;
use crate::local_command::{
    CommandOutput, ShellCommandPlan,
    buttons::{self, CommandButton},
};
use crate::view::{CustomCommandButtonView, CustomCommandMode, CustomCommandOutputView};
use image::CommandImageSource;

/// Private configuration and one captured invocation; never persisted with the session.
#[derive(Default)]
pub(super) struct CustomCommands {
    buttons: Vec<CommandButton>,
    loaded: bool,
    mode: CustomCommandMode,
    pub(super) pending: Option<ShellCommandPlan>,
    pub(super) running: bool,
    /// Whether the accepted invocation owns a typed command's private history result.
    pub(super) from_prompt: bool,
    waiting_download: Option<(u64, String, ShellCommandPlan)>,
    waiting_image: Option<PendingCommandImage>,
    ready: Option<(String, ShellCommandPlan)>,
    local_selection: Option<(PathBuf, PathBuf)>,
}

/// Selection-bound media and image information kept private until a command needs it.
#[derive(Clone)]
pub(super) struct CapturedCommandDownload {
    /// Exact identity used to find a previously completed, validated download.
    pub(super) identity: Option<MediaId>,
    /// Deferred source errors do not prevent plain commands or reuse of completed files.
    pub(super) source: Result<crate::download_queue::DownloadSource, String>,
    /// Frozen artwork source; opening the editor does not fetch or extract it.
    image: Result<CommandImageSource, String>,
}

/// A one-shot worker owns preparation and one continuation, never the current selection.
struct PendingCommandImage {
    replies: Receiver<Result<PathBuf, String>>,
    completed: Option<Result<PathBuf, String>>,
    worker: Option<JoinHandle<()>>,
    name: String,
    plan: ShellCommandPlan,
    download: CapturedCommandDownload,
    local_selection: Option<(PathBuf, PathBuf)>,
}

impl Drop for PendingCommandImage {
    fn drop(&mut self) {
        // Shutdown retains ownership of bounded I/O instead of abandoning a download.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(all(test, feature = "remote-artwork"))]
impl CustomCommands {
    /// Makes modal tests wait for publication, not for an arbitrary scheduling delay.
    pub(super) fn wait_for_image_fixture(&mut self) {
        let pending = self.waiting_image.as_mut().expect("pending image fixture");
        pending.completed = Some(
            pending
                .replies
                .recv_timeout(Duration::from_secs(5))
                .expect("image fixture completion"),
        );
    }
}

impl AppController {
    /// Reads the separate commands file once per session, never its disabled sample.
    pub(super) fn configure_custom_commands(&mut self, mode: CustomCommandMode) {
        self.custom_commands.mode = mode;
        if mode == CustomCommandMode::Unavailable {
            self.custom_commands.waiting_image = None;
            self.custom_commands.waiting_download = None;
            self.custom_commands.ready = None;
            self.custom_commands.from_prompt = false;
        }
        if mode != CustomCommandMode::Unavailable && !self.custom_commands.loaded {
            self.custom_commands.loaded = true;
            match buttons::load(self.config.config_dir()) {
                Ok(buttons) => self.custom_commands.buttons = buttons,
                Err(error) => self.show_actionable_message("Could not load commands", error),
            }
        }
        self.refresh_custom_command_buttons();
    }

    /// Resolves only selected identities; display abbreviations and signed streams are not inputs.
    pub(super) fn custom_command_target(
        &self,
    ) -> Option<(SourceKind, std::ffi::OsString, Option<PathBuf>)> {
        if self.view.screen == Screen::Statistics {
            return None;
        }
        if self.view.screen == Screen::Playlists
            && !matches!(self.playlists_route, PlaylistsRoute::Entries { .. })
        {
            return None;
        }
        if self.view.screen == Screen::Local {
            if self.view.local_browse_pending || self.local_archive_read_only() {
                return None;
            }
            let path = self.selected_local_path()?;
            return Some((SourceKind::Local, path.clone().into_os_string(), Some(path)));
        }
        let details = self.view.details.as_ref();
        let selected_identity = if self.view.screen == Screen::Subscriptions {
            self.selected_playlist_identity().map(|(id, _)| id)
        } else {
            self.view
                .rows
                .get(self.view.selected)
                .and_then(|row| row.media_id.clone())
        };
        // A slow metadata response must not expose the preceding row's URL or path.
        if self.active_description_video.is_none()
            && let (Some(selected), Some(shown)) = (
                &selected_identity,
                details.and_then(|details| details.media_id.as_ref()),
            )
            && selected != shown
        {
            return None;
        }
        // Optional Details metadata may lag, but another tab's retained Details
        // never creates a selection on an empty or informational page.
        if self.active_description_video.is_none()
            && selected_identity.is_none()
            && self.view.rows.get(self.view.selected).is_none()
            && !(self.view.screen == Screen::Search
                && (self.resolved_direct.is_some() || self.direct_item.is_some()))
        {
            return None;
        }
        if self.active_description_video.is_none()
            && let Some(identity) = selected_identity.as_ref()
            && identity.source == SourceKind::Local
        {
            let path = local_path_from_media_id(identity)?;
            return Some((SourceKind::Local, path.clone().into_os_string(), Some(path)));
        }
        let selected = self
            .active_description_video
            .is_none()
            .then(|| {
                self.selected_queue_item()
                    .map(|item| (item.media.id, item.media.webpage_url))
                    .or_else(|_| {
                        self.selected_playlist_snapshot()
                            .map(|snapshot| (snapshot.id, snapshot.webpage_url))
                    })
                    .ok()
            })
            .flatten();
        let linked_identity = self
            .active_description_video
            .as_ref()
            .map(|linked| MediaId::new(SourceKind::YouTube, &linked.video_id));
        let identity = linked_identity
            .as_ref()
            .or_else(|| selected.as_ref().map(|(identity, _)| identity))
            .or(selected_identity.as_ref());
        if let Some(identity) = identity
            && identity.source == SourceKind::Local
        {
            let path = local_path_from_media_id(identity)?;
            return Some((SourceKind::Local, path.clone().into_os_string(), Some(path)));
        }
        let source = identity
            .map(|id| id.source.clone())
            .or_else(|| match self.view.screen {
                Screen::Search | Screen::YouTubeMusic => Some(SourceKind::YouTube),
                Screen::SoundCloud => Some(SourceKind::SoundCloud),
                Screen::Bandcamp => Some(SourceKind::Bandcamp),
                Screen::ApplePodcasts => Some(SourceKind::ApplePodcasts),
                Screen::ArchiveOrg => Some(SourceKind::ArchiveOrg),
                Screen::LibriVox => Some(SourceKind::LibriVox),
                Screen::YandexMusic => Some(SourceKind::YandexMusic),
                Screen::Radio => Some(SourceKind::Radio),
                Screen::TrackerMusic => Some(SourceKind::ModArchive),
                Screen::Web => Some(SourceKind::RemoteFiles),
                Screen::Subscriptions => Some(match self.view.subscriptions.source_kind {
                    SubscriptionKind::YouTube => SourceKind::YouTube,
                    _ => SourceKind::Rss,
                }),
                _ => None,
            })?;
        let url = if source == SourceKind::YouTube {
            identity
                .map(|id| youtube_video_url(&id.external_id))
                .or_else(|| self.current_url())
        } else {
            (self.view.screen == Screen::History)
                .then(|| self.history_entries.get(self.view.selected))
                .flatten()
                .and_then(
                    |history| match history_replay_target(&history.entry).ok()? {
                        HistoryReplayTarget::Remote(url) => Some(url.to_string()),
                        HistoryReplayTarget::Local(_) => None,
                    },
                )
                .or_else(|| selected.as_ref().map(|(_, url)| url.to_string()))
                .or_else(|| self.current_url())
                .or_else(|| {
                    details
                        .filter(|details| {
                            selected_identity.is_some() && details.media_id == selected_identity
                        })
                        .and_then(|details| details.webpage_url.as_ref())
                        .map(ToString::to_string)
                })
        }?;
        let parsed = url::Url::parse(&url).ok()?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return None;
        }
        Some((source, url.into(), None))
    }

    /// Projects only matching labels and styles; descriptions and commands remain private.
    pub(super) fn refresh_custom_command_buttons(&mut self) {
        if self.custom_commands.ready.is_some()
            && !self.view.quitting
            && crate::keymap::custom_command_button_action(&self.view, usize::MAX)
                == Some(UiAction::RunCustomCommand(usize::MAX))
        {
            let (name, plan) = self.custom_commands.ready.take().expect("ready command");
            self.start_custom_command(name, plan);
        }
        let source = (self.custom_commands.mode != CustomCommandMode::Unavailable)
            .then(|| self.custom_command_target().map(|(source, _, _)| source))
            .flatten();
        let matches = self
            .custom_commands
            .buttons
            .iter()
            .enumerate()
            .filter(|(_, button)| {
                source
                    .as_ref()
                    .is_some_and(|source| button.matches_provider(source))
            });
        if self
            .view
            .custom_command_buttons
            .iter()
            .map(|button| button.id)
            .eq(matches.clone().map(|(id, _)| id))
        {
            return;
        }
        self.view.custom_command_buttons = matches
            .map(|(id, button)| CustomCommandButtonView {
                id,
                name: button.name.clone(),
                hotkey: button.hotkey.as_ref().map(|key| key.label.clone()),
                binding: button.hotkey.clone(),
                font_color: button.font_color.clone(),
                background_color: button.background_color.clone(),
            })
            .collect();
    }

    /// Captures the target before downloading or yielding the frontend its command plan.
    pub(super) fn run_custom_command(&mut self, index: usize) {
        if self.custom_commands.running
            || self.custom_commands.waiting_image.is_some()
            || self.custom_commands.waiting_download.is_some()
            || self.custom_commands.ready.is_some()
            || self.custom_commands.mode == CustomCommandMode::Unavailable
        {
            return;
        }
        self.refresh_custom_command_buttons();
        if crate::keymap::custom_command_button_action(&self.view, index)
            != Some(UiAction::RunCustomCommand(index))
        {
            return;
        }
        let Some((source, argument, local)) = self.custom_command_target() else {
            return;
        };
        let Some(button) = self
            .custom_commands
            .buttons
            .get(index)
            .filter(|button| button.matches_provider(&source))
        else {
            return;
        };
        let name = button.name.clone();
        let directory = local
            .as_ref()
            .and_then(|path| path.parent().map(Path::to_owned))
            .or_else(|| std::env::current_dir().ok());
        let Some(directory) = directory else {
            self.view.status_line = "Cannot resolve the command working directory".to_owned();
            return;
        };
        let plan = ShellCommandPlan {
            template: button.command.clone(),
            argument,
            directory,
            downloaded_path: local.clone(),
            image_path: None,
            metadata: self.capture_command_metadata(),
            playback_position: self.capture_command_playhead(),
        };
        let local_selection = if self.view.screen == Screen::Local {
            local.and_then(|path| {
                self.local_listing
                    .as_ref()
                    .map(|listing| (listing.path.clone(), path))
            })
        } else {
            None
        };
        let download = self.capture_command_download();
        if let Err(error) = self.run_captured_shell_command(name, plan, download, local_selection) {
            self.view.status_line = error;
        }
    }

    /// Captures download identity now; later edits, playback, and source changes cannot retarget it.
    pub(super) fn capture_command_download(&self) -> CapturedCommandDownload {
        let identity = if let Some(linked) = self.active_description_video.as_ref() {
            Some(MediaId::new(SourceKind::YouTube, &linked.video_id))
        } else if self.view.screen == Screen::Subscriptions {
            self.selected_playlist_identity()
                .map(|(identity, _)| identity)
        } else {
            self.view
                .rows
                .get(self.view.selected)
                .and_then(|row| row.media_id.clone())
        };
        let source = if let Some(linked) = self.active_description_video.as_ref() {
            self.view
                .details
                .as_ref()
                .and_then(|details| {
                    let identity = details.media_id.as_ref()?;
                    (identity.source == SourceKind::YouTube
                        && identity.external_id == linked.video_id)
                        .then(|| crate::download_queue::DownloadSource {
                            media_id: identity.clone(),
                            kind: MediaKind::Video,
                            title: details.title.clone(),
                            creator: (!details.channel_name.trim().is_empty())
                                .then(|| details.channel_name.trim().to_owned()),
                            webpage_url: url::Url::parse(&youtube_video_url(&identity.external_id))
                                .expect("canonical YouTube URL"),
                            download_url: url::Url::parse(&youtube_video_url(
                                &identity.external_id,
                            ))
                            .expect("canonical YouTube URL"),
                            duration_seconds: None,
                        })
                })
                .ok_or_else(|| "No downloadable linked item is selected".to_owned())
        } else {
            self.selected_manual_download_source()
        };
        let identity =
            identity.or_else(|| source.as_ref().ok().map(|source| source.media_id.clone()));
        let source = source.and_then(|source| {
            if identity.as_ref() != Some(&source.media_id) {
                return Err(
                    "Wait for the selected item details before downloading for a command"
                        .to_owned(),
                );
            }
            Ok(source)
        });
        CapturedCommandDownload {
            identity,
            source,
            image: self.capture_command_image(),
        }
    }

    /// Captures an authoritative playing or paused cursor without querying the backend.
    pub(super) fn capture_command_playhead(&self) -> Option<Duration> {
        if self.playback_phase != PlaybackPhase::Playing || self.view.playback.idle {
            return None;
        }
        let owner = self.current_media.as_ref()?;
        if self.view.playing_media_id.as_ref() != Some(owner) {
            return None;
        }
        // A LIVE cursor is buffer-relative. No range means normalization supplied
        // a zero placeholder, not a timestamp a command can reliably consume.
        if self.view.playback.live && self.view.playback.live_seekable_range.is_none() {
            return None;
        }
        Some(self.view.playback.position)
    }

    /// Freezes the selected Details description, never stale Local or playing metadata.
    pub(super) fn capture_command_metadata(
        &self,
    ) -> Option<Arc<crate::local_command::CommandMetadata>> {
        let (_, argument, local) = self.custom_command_target()?;
        let details = self.view.details.as_ref()?;
        let expected = if let Some(path) = local {
            Some(local_media_id(&path))
        } else if let Some(linked) = self.active_description_video.as_ref() {
            Some(MediaId::new(SourceKind::YouTube, &linked.video_id))
        } else if self.view.screen == Screen::Subscriptions {
            self.selected_playlist_identity()
                .map(|(identity, _)| identity)
        } else {
            self.view
                .rows
                .get(self.view.selected)
                .and_then(|row| row.media_id.clone())
        };
        if let Some(expected) = expected {
            if details.media_id.as_ref() != Some(&expected) {
                return None;
            }
        } else {
            // Channels and direct routes can lack a playable row identity. Their
            // original URL must still match the description currently displayed.
            let target = argument
                .to_str()
                .and_then(|value| url::Url::parse(value).ok())?;
            if details.webpage_url.as_ref() != Some(&target) {
                return None;
            }
        }
        Some(Arc::new(crate::local_command::CommandMetadata::new(
            details.description.clone(),
        )))
    }

    /// Captures only selected, provider-advertised artwork; terminal size cannot limit `%i`.
    fn capture_command_image(&self) -> Result<CommandImageSource, String> {
        let (source, argument, local) = self
            .custom_command_target()
            .ok_or_else(|| "Select an item before using %i in a command".to_owned())?;
        if let Some(path) = local {
            let is_image = crate::local_browser::classify_local_file(&path)
                == Some(crate::local_browser::LocalEntryKind::Image);
            return Ok(CommandImageSource::Local { path, is_image });
        }
        let details = self.view.details.as_ref();
        let largest = if source == SourceKind::YouTube {
            let id = argument
                .to_str()
                .and_then(|argument| url::Url::parse(argument).ok())
                .and_then(|url| {
                    url.query_pairs()
                        .find(|(key, _)| key == "v")
                        .map(|(_, value)| value.into_owned())
                });
            let thumbnails = id.as_ref().and_then(|id| {
                let cached = self
                    .youtube_video_details_cache
                    .get(id)
                    .map(|details| details.thumbnails.as_slice())
                    .unwrap_or_default();
                let selected = match self.selected_youtube_item() {
                    Some(SearchItem::Video(video)) if video.video_id == *id => {
                        video.thumbnails.as_slice()
                    }
                    _ => &[],
                };
                // Partial detail replies must not hide an upgrade already advertised in search.
                let candidates = cached.iter().chain(selected).cloned().collect::<Vec<_>>();
                (!candidates.is_empty()).then_some(candidates)
            });
            thumbnails.and_then(|thumbnails| {
                thumbnails
                    .first()
                    .and_then(|preview| largest_youtube_thumbnail_url(&thumbnails, preview, None))
                    .or_else(|| thumbnails.last().map(|thumbnail| thumbnail.url.clone()))
            })
        } else {
            None
        };
        let image = largest
            .or_else(|| {
                details.and_then(|details| {
                    details
                        .expanded_thumbnail_url
                        .clone()
                        .or_else(|| details.thumbnail_url.clone())
                })
            })
            .filter(|url| matches!(url.scheme(), "http" | "https"));
        image
            .map(CommandImageSource::Remote)
            .ok_or_else(|| "No image is available for the selected item (%i)".to_owned())
    }

    /// Shares validated cache reuse, the ordinary format chooser, and one captured continuation.
    pub(super) fn run_captured_shell_command(
        &mut self,
        name: String,
        mut plan: ShellCommandPlan,
        download: CapturedCommandDownload,
        local_selection: Option<(PathBuf, PathBuf)>,
    ) -> Result<(), String> {
        if self.custom_commands.running
            || self.custom_commands.waiting_image.is_some()
            || self.custom_commands.waiting_download.is_some()
            || self.custom_commands.ready.is_some()
        {
            return Err("Wait for the current command to finish".to_owned());
        }
        if self.custom_commands.mode == CustomCommandMode::Unavailable {
            return Err("Shell commands are unavailable in this frontend".to_owned());
        }
        if plan.requires_playback_position()? && plan.playback_position.is_none() {
            return Err(
                "No playback cursor was available when this command was captured (%t)".to_owned(),
            );
        }
        if plan.requires_metadata()? && plan.metadata.is_none() {
            return Err("Wait for the selected item's description before using %m".to_owned());
        }
        if plan.requires_image()? && plan.image_path.is_none() {
            let source = download.image.clone()?;
            let cache_directory = self.config.thumbnail_cache_dir();
            let (sender, replies) = bounded(1);
            let responses = ResponseSender::new(sender, self.worker_notifier.clone());
            let worker = thread::Builder::new()
                .name("youta-command-image".to_owned())
                .spawn(move || {
                    let _ = responses.send(image::prepare(source, &cache_directory));
                })
                .map_err(|_| "Cannot start the command image worker".to_owned())?;
            self.custom_commands.waiting_image = Some(PendingCommandImage {
                replies,
                completed: None,
                worker: Some(worker),
                name,
                plan,
                download,
                local_selection,
            });
            self.view.status_line = "Preparing image for custom command...".to_owned();
            return Ok(());
        }
        if plan.requires_download()? && plan.downloaded_path.is_none() {
            // Reuse completed output even when no downloader is enabled in this build.
            plan.downloaded_path = download
                .identity
                .as_ref()
                .and_then(|identity| self.custom_command_download_path(identity));
            if plan.downloaded_path.is_none() {
                let source = download.source?;
                if source.media_id.source == SourceKind::Radio {
                    return Err("%d requires finite media; record live radio first".to_owned());
                }
                plan.downloaded_path = self.custom_command_download_path(&source.media_id);
                if plan.downloaded_path.is_none() {
                    let id = self.enqueue_custom_command_download(source)?;
                    self.custom_commands.local_selection = local_selection;
                    self.custom_commands.waiting_download = Some((id, name, plan));
                    self.view.status_line = "Downloading for custom command...".to_owned();
                    self.poll_manual_download_queue();
                    return Ok(());
                }
            }
        }
        self.custom_commands.local_selection = local_selection;
        self.start_custom_command(name, plan);
        Ok(())
    }

    /// Accepts one published image without waiting on network I/O in the UI loop.
    pub(super) fn poll_custom_command_image(&mut self) {
        let Some(pending) = self.custom_commands.waiting_image.as_mut() else {
            return;
        };
        if pending.completed.is_none() {
            pending.completed = match pending.replies.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => {
                    Some(Err("The command image worker stopped".to_owned()))
                }
                Err(TryRecvError::Empty) => return,
            };
        }
        // A successful preparation waits behind an editor or popup just like `%d`.
        if pending.completed.as_ref().is_some_and(Result::is_ok)
            && (self.view.quitting
                || crate::keymap::custom_command_button_action(&self.view, usize::MAX)
                    != Some(UiAction::RunCustomCommand(usize::MAX)))
        {
            return;
        }
        let mut pending = self
            .custom_commands
            .waiting_image
            .take()
            .expect("pending image");
        let result = pending.completed.take().expect("completed image");
        if let Some(worker) = pending.worker.take() {
            // Sending was its final operation; unfinished handles only retain thread cleanup.
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
        let result = result.and_then(|path| {
            pending.plan.image_path = Some(path);
            self.run_captured_shell_command(
                pending.name.clone(),
                pending.plan.clone(),
                pending.download.clone(),
                pending.local_selection.clone(),
            )
        });
        if let Err(error) = result {
            self.custom_commands.from_prompt = false;
            self.view.status_line = format!("Custom command not run: {error}");
        }
    }

    /// Transfers a validated captured plan once; repeated clicks cannot start another process.
    fn start_custom_command(&mut self, name: String, plan: ShellCommandPlan) {
        self.custom_commands.running = true;
        self.custom_commands.pending = Some(plan);
        if self.custom_commands.mode == CustomCommandMode::Dialog {
            self.view.custom_command_output = Some(CustomCommandOutputView {
                name,
                running: true,
                ..Default::default()
            });
        }
        self.view.status_line = "Running custom command...".to_owned();
        self.worker_notifier.wake();
    }

    /// Reacts to durable queue transitions; failed/cancelled downloads never execute commands.
    pub(super) fn update_custom_command_download(&mut self) {
        let Some((id, _, _)) = self.custom_commands.waiting_download.as_ref() else {
            return;
        };
        let entry = self
            .manual_downloads
            .queue
            .entries
            .iter()
            .find(|entry| entry.id == *id);
        if entry.is_some_and(|entry| {
            matches!(
                entry.state,
                DownloadQueueState::Queued | DownloadQueueState::Running
            )
        }) {
            return;
        }
        let path = entry
            .filter(|entry| entry.state == DownloadQueueState::Completed)
            .and_then(|entry| self.custom_command_entry_path(entry));
        let (_, name, mut plan) = self
            .custom_commands
            .waiting_download
            .take()
            .expect("captured download");
        if let Some(path) = path {
            plan.downloaded_path = Some(path);
            self.custom_commands.ready = Some((name, plan));
        } else {
            self.custom_commands.local_selection = None;
            self.custom_commands.from_prompt = false;
            self.view.status_line =
                "Custom command not run: download failed or was cancelled".to_owned();
        }
    }

    /// Keeps raw output out of diagnostics and refreshes only the captured Local listing.
    pub(super) fn finish_custom_command(&mut self, result: Result<CommandOutput, String>) {
        if !self.custom_commands.running {
            return;
        }
        self.custom_commands.running = false;
        self.custom_commands.pending = None;
        let success = result.as_ref().is_ok_and(|output| output.success);
        if let Some(popup) = self.view.custom_command_output.as_mut() {
            popup.running = false;
            popup.failed = !success;
            popup.output = match result {
                Ok(output) => output.output,
                Err(error) => error,
            };
        }
        if let Some((directory, path)) = self.custom_commands.local_selection.take() {
            self.browse_local_directory_with_reselection(directory, Some(path));
        }
        self.view.status_line = if success {
            "Custom command finished"
        } else {
            "Custom command failed; see its output"
        }
        .to_owned();
        if std::mem::take(&mut self.custom_commands.from_prompt) {
            self.finish_prompt_command();
        }
    }
}
