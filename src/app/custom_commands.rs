//! Provider-filtered, explicit shell buttons and download-bound command continuations.

use super::*;
use crate::download_queue::DownloadQueueState;
use crate::local_command::{
    CommandOutput, ShellCommandPlan,
    buttons::{self, CommandButton},
};
use crate::view::{CustomCommandButtonView, CustomCommandMode, CustomCommandOutputView};

/// Private configuration and one captured invocation; never persisted with the session.
#[derive(Default)]
pub(super) struct CustomCommands {
    buttons: Vec<CommandButton>,
    loaded: bool,
    mode: CustomCommandMode,
    pub(super) pending: Option<ShellCommandPlan>,
    pub(super) running: bool,
    waiting_download: Option<(u64, String, ShellCommandPlan)>,
    ready: Option<(String, ShellCommandPlan)>,
    local_selection: Option<(PathBuf, PathBuf)>,
}

impl AppController {
    /// Reads the separate commands file once per session, never its disabled sample.
    pub(super) fn configure_custom_commands(&mut self, mode: CustomCommandMode) {
        self.custom_commands.mode = mode;
        if mode == CustomCommandMode::Unavailable {
            self.custom_commands.waiting_download = None;
            self.custom_commands.ready = None;
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
    fn custom_command_target(&self) -> Option<(SourceKind, std::ffi::OsString, Option<PathBuf>)> {
        if self.view.screen == Screen::Local {
            if self.view.local_browse_pending || self.local_archive_read_only() {
                return None;
            }
            let path = self.selected_local_path()?;
            return Some((SourceKind::Local, path.clone().into_os_string(), Some(path)));
        }
        let details = self.view.details.as_ref()?;
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
            && let (Some(selected), Some(shown)) = (&selected_identity, &details.media_id)
            && selected != shown
        {
            return None;
        }
        let identity = details.media_id.as_ref().or(selected_identity.as_ref());
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
                .or_else(|| self.current_url())
                .or_else(|| details.webpage_url.as_ref().map(ToString::to_string))
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
        let mut plan = ShellCommandPlan {
            template: button.command.clone(),
            argument,
            directory,
            downloaded_path: local.clone(),
        };
        self.custom_commands.local_selection = if self.view.screen == Screen::Local {
            local.and_then(|path| {
                self.local_listing
                    .as_ref()
                    .map(|listing| (listing.path.clone(), path))
            })
        } else {
            None
        };
        let needs_download = match plan.requires_download() {
            Ok(value) => value,
            Err(error) => {
                self.view.status_line = error;
                return;
            }
        };
        if needs_download && plan.downloaded_path.is_none() {
            // Existing validated files also work in builds without a downloader.
            if let Some(identity) = self
                .view
                .details
                .as_ref()
                .and_then(|details| details.media_id.as_ref())
            {
                plan.downloaded_path = self.custom_command_download_path(identity);
            }
            if plan.downloaded_path.is_some() {
                self.start_custom_command(name, plan);
                return;
            }
            let download_source = if self.active_description_video.is_some() {
                self.view
                    .details
                    .as_ref()
                    .and_then(|details| {
                        let identity = details.media_id.as_ref()?;
                        (identity.source == SourceKind::YouTube).then(|| {
                            crate::download_queue::DownloadSource {
                                media_id: identity.clone(),
                                kind: MediaKind::Video,
                                title: details.title.clone(),
                                creator: (!details.channel_name.trim().is_empty())
                                    .then(|| details.channel_name.trim().to_owned()),
                                webpage_url: url::Url::parse(&youtube_video_url(
                                    &identity.external_id,
                                ))
                                .expect("canonical YouTube URL"),
                                download_url: url::Url::parse(&youtube_video_url(
                                    &identity.external_id,
                                ))
                                .expect("canonical YouTube URL"),
                                duration_seconds: None,
                            }
                        })
                    })
                    .ok_or_else(|| "No downloadable linked item is selected".to_owned())
            } else {
                self.selected_manual_download_source()
            };
            let source = match download_source {
                Ok(source) if source.media_id.source != SourceKind::Radio => source,
                Ok(_) => {
                    self.view.status_line =
                        "%d requires finite media; record live radio first".to_owned();
                    return;
                }
                Err(error) => {
                    self.view.status_line = error;
                    return;
                }
            };
            let shown_identity = self
                .view
                .details
                .as_ref()
                .and_then(|details| details.media_id.as_ref())
                .or_else(|| {
                    self.view
                        .rows
                        .get(self.view.selected)
                        .and_then(|row| row.media_id.as_ref())
                });
            if shown_identity != Some(&source.media_id) {
                self.view.status_line =
                    "Wait for the selected item details before downloading for a command"
                        .to_owned();
                return;
            }
            plan.downloaded_path = self.custom_command_download_path(&source.media_id);
            if plan.downloaded_path.is_none() {
                match self.enqueue_custom_command_download(source) {
                    Ok(id) => {
                        self.custom_commands.waiting_download = Some((id, name, plan));
                        self.view.status_line = "Downloading for custom command...".to_owned();
                        self.poll_manual_download_queue();
                    }
                    Err(error) => self.view.status_line = error,
                }
                return;
            }
        }
        self.start_custom_command(name, plan);
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
    }
}
