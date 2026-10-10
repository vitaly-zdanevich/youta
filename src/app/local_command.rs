//! Global private shell input, bounded durable history, and frontend-owned execution.

mod completion;

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use super::custom_commands::CapturedCommandDownload;
use super::*;
use crate::local_command::ShellCommandPlan;
use crate::view::{LocalCommandHistoryView, LocalCommandView};

const COMMAND_LIMIT: usize = 8_192;
const HISTORY_LIMIT: usize = 100;
const HISTORY_MATCH_LIMIT: usize = 10;
const HISTORY_FILE_LIMIT: usize = HISTORY_LIMIT * (COMMAND_LIMIT + 1);
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// State kept out of durable session snapshots, diagnostics, and frontend IPC.
#[derive(Default)]
pub(super) struct LocalCommandState {
    history: Vec<String>,
    loaded: bool,
    history_position: Option<usize>,
    history_draft: String,
    selection: Option<CommandSelection>,
    history_save_failed: bool,
    completion: completion::State,
}

/// Freezes the selected target and download context before the user starts typing.
#[derive(Clone)]
struct CommandSelection {
    plan: ShellCommandPlan,
    has_target: bool,
    download: CapturedCommandDownload,
    local_selection: Option<(PathBuf, PathBuf)>,
}

/// Actions admitted while the private command editor owns the interface.
pub(super) fn command_action(action: &UiAction) -> bool {
    matches!(
        action,
        UiAction::BeginLocalCommand
            | UiAction::AppendLocalCommandCharacter(_)
            | UiAction::MoveLocalCommandCursor(_)
            | UiAction::DeleteLocalCommandCharacter
            | UiAction::DeleteLocalCommandForward
            | UiAction::DeleteLocalCommandWord
            | UiAction::CompleteLocalCommand
            | UiAction::BrowseLocalCommandHistory(_)
            | UiAction::OpenLocalCommandHistory
            | UiAction::MoveLocalCommandHistory(_)
            | UiAction::SubmitLocalCommand
            | UiAction::DismissLocalCommandHistory
            | UiAction::DismissLocalCommand
    )
}

impl AppController {
    /// Applies only semantic shell-editor actions; no command executes in the controller.
    pub(super) fn dispatch_local_command(&mut self, action: UiAction) {
        if self.custom_commands.running || self.view.error_popup.is_some() {
            return;
        }
        if !matches!(action, UiAction::CompleteLocalCommand) {
            self.local_command.completion.reset();
        }
        match action {
            UiAction::BeginLocalCommand => self.begin_local_command(),
            UiAction::AppendLocalCommandCharacter(character) => {
                if character.is_control() {
                    return;
                }
                self.edit_local_command(|value, cursor| {
                    if value.len().saturating_add(character.len_utf8()) <= COMMAND_LIMIT {
                        let at = editor_cursor_boundary(value, *cursor);
                        value.insert(at, character);
                        *cursor = at.saturating_add(character.len_utf8());
                    }
                });
            }
            UiAction::MoveLocalCommandCursor(motion) => {
                if let Some(editor) = self.view.local_command.as_mut() {
                    let (value, cursor) = focused_input(editor);
                    *cursor = moved_private_note_cursor(value, *cursor, motion);
                }
            }
            UiAction::DeleteLocalCommandCharacter => self.edit_local_command(|value, cursor| {
                let end = editor_cursor_boundary(value, *cursor);
                let start = moved_private_note_cursor(value, end, PrivateNoteCursorMotion::Left);
                value.drain(start..end);
                *cursor = start;
            }),
            UiAction::DeleteLocalCommandForward => self.edit_local_command(|value, cursor| {
                let start = editor_cursor_boundary(value, *cursor);
                let end = moved_private_note_cursor(value, start, PrivateNoteCursorMotion::Right);
                value.drain(start..end);
                *cursor = start;
            }),
            UiAction::DeleteLocalCommandWord => self.edit_local_command(|value, cursor| {
                delete_previous_editor_word(value, cursor);
            }),
            UiAction::CompleteLocalCommand => self.complete_local_command(),
            UiAction::BrowseLocalCommandHistory(direction) => {
                self.browse_local_command_history(direction)
            }
            UiAction::OpenLocalCommandHistory => {
                if let Some(editor) = self.view.local_command.as_mut() {
                    if editor.history.is_none() {
                        editor.history = Some(LocalCommandHistoryView::default());
                    }
                    self.refresh_local_command_matches();
                }
            }
            UiAction::MoveLocalCommandHistory(direction) => {
                if let Some(history) = self
                    .view
                    .local_command
                    .as_mut()
                    .and_then(|editor| editor.history.as_mut())
                {
                    history.selected = history
                        .selected
                        .saturating_add_signed(isize::from(direction))
                        .min(history.matches.len().saturating_sub(1));
                }
            }
            UiAction::SubmitLocalCommand => self.submit_local_command(),
            UiAction::DismissLocalCommandHistory => {
                if let Some(editor) = self.view.local_command.as_mut() {
                    editor.history = None;
                }
            }
            UiAction::DismissLocalCommand => self.dismiss_local_command(),
            _ => {}
        }
    }

    /// Captures only the selected item, never the playing item or a shortened path.
    fn begin_local_command(&mut self) {
        if !self.view.local_command_available
            || self.view.local_command.is_some()
            || self.view.local_file_popup.is_some()
            || (self.view.screen == Screen::Local && self.view.local_browse_pending)
            || self.local_move_is_executing()
        {
            return;
        }
        if self.view.screen == Screen::Local && self.local_archive_read_only() {
            self.view.status_line =
                "Commands are unavailable inside read-only archive folders".to_owned();
            return;
        }
        let target = self.custom_command_target();
        let local_path = target.as_ref().and_then(|(_, _, path)| path.clone());
        let local_directory = (self.view.screen == Screen::Local)
            .then(|| {
                self.local_listing
                    .as_ref()
                    .map(|listing| listing.path.clone())
            })
            .flatten();
        let directory = local_directory
            .clone()
            .or_else(|| {
                local_path
                    .as_deref()
                    .and_then(Path::parent)
                    .map(Path::to_owned)
            })
            .map_or_else(std::env::current_dir, Ok);
        let directory = match directory {
            Ok(directory) => directory,
            Err(_) => {
                self.view.status_line = "Cannot determine the command working directory".to_owned();
                return;
            }
        };
        if !self.local_command.loaded {
            self.local_command.loaded = true;
            match read_history(&history_path(&self.config)) {
                Ok(history) => self.local_command.history = history,
                Err(_) => {
                    self.view.status_line =
                        "Command history could not be read; starting with empty history".to_owned()
                }
            }
        }
        self.local_command.selection = Some(CommandSelection {
            has_target: target.is_some(),
            plan: ShellCommandPlan {
                template: String::new(),
                argument: target.map(|(_, argument, _)| argument).unwrap_or_default(),
                downloaded_path: local_path.clone(),
                directory,
            },
            download: self.capture_command_download(),
            local_selection: local_directory.zip(local_path),
        });
        self.local_command.history_position = None;
        self.local_command.history_draft.clear();
        self.view.search_editing = false;
        self.view.local_command = Some(LocalCommandView::default());
    }

    /// Mutates one grapheme-aware input and recomputes history matches only when needed.
    fn edit_local_command(&mut self, edit: impl FnOnce(&mut String, &mut usize)) {
        let Some(editor) = self.view.local_command.as_mut() else {
            return;
        };
        let searching = editor.history.is_some();
        let (value, cursor) = focused_input(editor);
        edit(value, cursor);
        if searching {
            self.refresh_local_command_matches();
        } else {
            self.local_command.history_position = None;
        }
    }

    /// Completes only literal words using the captured directory and bounded system PATH.
    fn complete_local_command(&mut self) {
        let Some(editor) = self
            .view
            .local_command
            .as_mut()
            .filter(|editor| editor.history.is_none())
        else {
            return;
        };
        let Some(selection) = self.local_command.selection.as_ref() else {
            return;
        };
        let search_path = std::env::var_os("PATH");
        let home = std::env::var_os("HOME").map(PathBuf::from);
        self.local_command.completion.complete(
            &mut editor.command,
            &mut editor.cursor_byte,
            &completion::Context {
                directory: &selection.plan.directory,
                search_path: search_path.as_deref(),
                home: home.as_deref(),
            },
        );
        self.local_command.history_position = None;
    }

    /// Rebuilds the ten newest substring matches without changing the pending command.
    fn refresh_local_command_matches(&mut self) {
        let Some(history) = self
            .view
            .local_command
            .as_mut()
            .and_then(|editor| editor.history.as_mut())
        else {
            return;
        };
        history.matches = self
            .local_command
            .history
            .iter()
            .rev()
            .filter(|command| command.contains(&history.query))
            .take(HISTORY_MATCH_LIMIT)
            .cloned()
            .collect();
        history.selected = 0;
    }

    /// Recalls older/newer raw templates and restores the draft beyond the newest entry.
    fn browse_local_command_history(&mut self, direction: i8) {
        let Some(editor) = self.view.local_command.as_mut() else {
            return;
        };
        if editor.history.is_some() || self.local_command.history.is_empty() || direction == 0 {
            return;
        }
        let length = self.local_command.history.len();
        let position = match (self.local_command.history_position, direction < 0) {
            (None, true) => {
                self.local_command.history_draft.clone_from(&editor.command);
                Some(length - 1)
            }
            (None, false) => return,
            (Some(position), true) => Some(position.saturating_sub(1)),
            (Some(position), false) if position + 1 < length => Some(position + 1),
            (Some(_), false) => None,
        };
        self.local_command.history_position = position;
        editor.command.clone_from(
            position.map_or(&self.local_command.history_draft, |position| {
                &self.local_command.history[position]
            }),
        );
        editor.cursor_byte = editor.command.len();
    }

    /// Validates selected-item macros and shares the normal download/foreground execution flow.
    fn submit_local_command(&mut self) {
        let Some(editor) = self.view.local_command.as_ref() else {
            return;
        };
        let template = if let Some(history) = editor.history.as_ref() {
            let Some(command) = history.matches.get(history.selected) else {
                return;
            };
            command.clone()
        } else {
            editor.command.clone()
        };
        if !valid_command(&template) {
            return;
        }
        let Some(mut selection) = self.local_command.selection.clone() else {
            return;
        };
        selection.plan.template = template.clone();
        match selection.plan.requires_selection() {
            Ok(true) if !selection.has_target => {
                self.view.status_line =
                    "Select an item before using % or %d in a command".to_owned();
                return;
            }
            Err(error) => {
                self.view.status_line = error;
                return;
            }
            _ => {}
        }
        // Queue setup may fail synchronously and clear ownership before returning.
        let previous_prompt = std::mem::replace(&mut self.custom_commands.from_prompt, true);
        if let Err(error) = self.run_captured_shell_command(
            "Command".to_owned(),
            selection.plan,
            selection.download,
            selection.local_selection,
        ) {
            self.custom_commands.from_prompt = previous_prompt;
            self.view.status_line = error;
            return;
        }
        self.local_command.history.push(template);
        if self.local_command.history.len() > HISTORY_LIMIT {
            self.local_command.history.remove(0);
        }
        self.local_command.history_save_failed =
            write_history(&history_path(&self.config), &self.local_command.history).is_err();
        self.view.local_command = None;
        self.local_command.selection = None;
    }

    /// Cancels unsent input without touching persistent command history.
    pub(super) fn dismiss_local_command(&mut self) {
        if self.custom_commands.running {
            return;
        }
        self.view.local_command = None;
        self.local_command.selection = None;
        self.local_command.history_position = None;
        self.local_command.history_draft.clear();
    }

    /// Retains history-write failures after the shared runner reports command completion.
    pub(super) fn finish_prompt_command(&mut self) {
        if self.local_command.history_save_failed {
            self.view
                .status_line
                .push_str("; its history could not be saved");
        }
        self.local_command.history_save_failed = false;
    }
}

/// Borrows whichever one-line editor currently owns text input.
fn focused_input(editor: &mut LocalCommandView) -> (&mut String, &mut usize) {
    match editor.history.as_mut() {
        Some(history) => (&mut history.query, &mut history.cursor_byte),
        None => (&mut editor.command, &mut editor.cursor_byte),
    }
}

/// Commands are single-line private text, never terminal escape sequences.
fn valid_command(command: &str) -> bool {
    !command.trim().is_empty()
        && command.len() <= COMMAND_LIMIT
        && !command.chars().any(char::is_control)
}

/// Keeps command history separately removable and outside ordinary session/log records.
fn history_path(config: &Config) -> PathBuf {
    config.config_dir().join("cmd-log")
}

/// Reads only bounded regular plaintext files and retains the newest valid hundred lines.
fn read_history(path: &Path) -> io::Result<Vec<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() || metadata.len() > HISTORY_FILE_LIMIT as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid command history file",
        ));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(HISTORY_FILE_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > HISTORY_FILE_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "command history is too large",
        ));
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "command history is not UTF-8"))?;
    let mut history: Vec<String> = text
        .split('\n')
        .rev()
        .filter(|line| valid_command(line))
        .take(HISTORY_LIMIT)
        .map(str::to_owned)
        .collect();
    history.reverse();
    Ok(history)
}

/// Atomically replaces a private plaintext history without following a pre-existing target link.
fn write_history(path: &Path, history: &[String]) -> io::Result<()> {
    let directory = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing command history directory",
        )
    })?;
    crate::private_files::create_private_directory(directory)?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && !metadata.file_type().is_file()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "command history is not a regular file",
        ));
    }
    let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!(".cmd-log.{}.{sequence}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = crate::private_files::open_privately(&mut options).open(&temporary)?;
    let result = (|| {
        let start = history.len().saturating_sub(HISTORY_LIMIT);
        for command in history[start..]
            .iter()
            .filter(|command| valid_command(command))
        {
            file.write_all(command.as_bytes())?;
            file.write_all(b"\n")?;
        }
        file.sync_all()?;
        drop(file);
        crate::private_files::set_private_file_permissions(&temporary)?;
        fs::rename(&temporary, path)?;
        crate::private_files::set_private_file_permissions(path)?;
        crate::durability::sync_parent_directory(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keeps all command controller tests independent of user directories and network helpers.
    fn fixture() -> (
        AppController,
        tempfile::TempDir,
        PathBuf,
        Receiver<LocalBrowseRequest>,
    ) {
        let directory = crate::test_support::canonical_tempdir("Local command fixture");
        let media = directory.path().join("music");
        fs::create_dir(&media).unwrap();
        let selected = media.join("track with space.flac");
        fs::write(&selected, b"fixture audio").unwrap();
        let mut config = Config::for_dir(directory.path().join("config"));
        config.ui.show_local_folder_sizes = false;
        let store = StateStore::open_in_memory().unwrap();
        let mut controller = AppController::new(config, store, None, None);
        controller.shutdown_local_browse_worker();
        controller.diagnostic_helpers_cache = Some(Vec::new());
        controller.view.screen = Screen::Local;
        controller.local_listing = Some(
            crate::local_browser::list_local_directory(
                &media,
                crate::local_browser::LocalBrowseLimits::default(),
            )
            .unwrap(),
        );
        controller.refresh_local_browser_rows();
        controller.select_local_path(Some(&selected));
        controller.set_local_command_available(true);
        controller.set_custom_command_mode(crate::view::CustomCommandMode::Terminal);
        let (sender, requests) = unbounded();
        controller.local_browse_requests = Some(sender);
        (controller, directory, selected, requests)
    }

    /// Types through real semantic actions rather than setting hidden controller state.
    fn type_text(controller: &mut AppController, text: &str) {
        for character in text.chars() {
            controller.dispatch(UiAction::AppendLocalCommandCharacter(character));
        }
    }

    #[test]
    fn command_submission_captures_full_path_blocks_navigation_and_refreshes_selection() {
        let (mut controller, _directory, selected, requests) = fixture();
        controller.dispatch(UiAction::BeginLocalCommand);
        type_text(&mut controller, "printf '%s\\n' %");
        controller.dispatch(UiAction::ShowScreen(Screen::Search));
        assert_eq!(controller.view.screen, Screen::Local);
        controller.dispatch(UiAction::SubmitLocalCommand);
        let plan = controller.take_custom_command_plan().unwrap();
        assert_eq!(plan.argument, selected.as_os_str());
        assert_eq!(plan.downloaded_path, Some(selected.clone()));
        assert_eq!(plan.directory, selected.parent().unwrap());
        assert_eq!(plan.template, "printf '%s\\n' %");
        assert!(controller.take_custom_command_plan().is_none());
        controller.dispatch(UiAction::Quit);
        assert!(!controller.view.quitting);
        controller.report_custom_command_result(Ok(crate::local_command::CommandOutput {
            output: String::new(),
            success: true,
        }));
        let LocalBrowseRequest::Browse {
            directory,
            preferred_child,
            ..
        } = requests.try_recv().unwrap()
        else {
            panic!("expected refresh");
        };
        assert_eq!(directory, selected.parent().unwrap());
        assert_eq!(preferred_child, Some(selected));
        assert_eq!(
            read_history(&history_path(&controller.config)).unwrap(),
            ["printf '%s\\n' %"]
        );
    }

    #[test]
    fn history_arrows_restore_unsent_draft_and_search_is_bounded_newest_first() {
        let (mut controller, _directory, _selected, _requests) = fixture();
        controller.local_command.loaded = true;
        controller.local_command.history =
            (0..20).map(|index| format!("echo match{index}")).collect();
        controller.dispatch(UiAction::BeginLocalCommand);
        type_text(&mut controller, "draft");
        controller.dispatch(UiAction::BrowseLocalCommandHistory(-1));
        assert_eq!(
            controller.view.local_command.as_ref().unwrap().command,
            "echo match19"
        );
        controller.dispatch(UiAction::BrowseLocalCommandHistory(-1));
        controller.dispatch(UiAction::BrowseLocalCommandHistory(1));
        controller.dispatch(UiAction::BrowseLocalCommandHistory(1));
        assert_eq!(
            controller.view.local_command.as_ref().unwrap().command,
            "draft"
        );
        controller.dispatch(UiAction::OpenLocalCommandHistory);
        let history = controller
            .view
            .local_command
            .as_ref()
            .unwrap()
            .history
            .as_ref()
            .unwrap();
        assert_eq!(history.matches.len(), 10);
        assert_eq!(history.matches[0], "echo match19");
        type_text(&mut controller, "match1");
        let history = controller
            .view
            .local_command
            .as_ref()
            .unwrap()
            .history
            .as_ref()
            .unwrap();
        assert_eq!(history.matches.len(), 10);
        assert!(
            history
                .matches
                .iter()
                .all(|command| command.contains("match1"))
        );
        controller.dispatch(UiAction::DismissLocalCommandHistory);
        assert_eq!(
            controller.view.local_command.as_ref().unwrap().command,
            "draft"
        );
        controller.dispatch(UiAction::OpenLocalCommandHistory);
        controller.dispatch(UiAction::MoveLocalCommandHistory(1));
        controller.dispatch(UiAction::SubmitLocalCommand);
        assert_eq!(
            controller.take_custom_command_plan().unwrap().template,
            "echo match18"
        );
    }

    #[test]
    fn command_editor_preserves_graphemes_and_refuses_control_characters() {
        let (mut controller, _directory, _selected, _requests) = fixture();
        controller.dispatch(UiAction::BeginLocalCommand);
        type_text(&mut controller, "A👩‍💻Б");
        controller.dispatch(UiAction::MoveLocalCommandCursor(
            PrivateNoteCursorMotion::Left,
        ));
        controller.dispatch(UiAction::DeleteLocalCommandCharacter);
        type_text(&mut controller, "\n\r\0\x1b");
        assert_eq!(
            controller.view.local_command.as_ref().unwrap().command,
            "AБ"
        );
        controller.dispatch(UiAction::DeleteLocalCommandForward);
        assert_eq!(controller.view.local_command.as_ref().unwrap().command, "A");
        controller.dispatch(UiAction::DismissLocalCommand);
        assert!(controller.take_custom_command_plan().is_none());
        assert!(!history_path(&controller.config).exists());
    }

    /// Completion uses the captured Local folder, while reverse-history input stays untouched.
    #[test]
    fn tab_completes_selected_folder_filenames_without_running_or_saving() {
        let (mut controller, _directory, selected, _requests) = fixture();
        controller.dispatch(UiAction::BeginLocalCommand);
        type_text(&mut controller, "cat tra");
        controller.dispatch(UiAction::CompleteLocalCommand);
        assert_eq!(
            controller.view.local_command.as_ref().unwrap().command,
            "cat track\\ with\\ space.flac"
        );
        assert!(controller.take_custom_command_plan().is_none());
        assert!(!history_path(&controller.config).exists());
        controller.dispatch(UiAction::OpenLocalCommandHistory);
        type_text(&mut controller, "cat tra");
        controller.dispatch(UiAction::CompleteLocalCommand);
        assert_eq!(
            controller
                .view
                .local_command
                .as_ref()
                .unwrap()
                .history
                .as_ref()
                .unwrap()
                .query,
            "cat tra"
        );
        controller.dispatch(UiAction::DismissLocalCommand);
        for name in ["song-a.flac", "song-b.flac"] {
            fs::write(selected.parent().unwrap().join(name), b"").unwrap();
        }
        controller.dispatch(UiAction::BeginLocalCommand);
        type_text(&mut controller, "cat so");
        controller.dispatch(UiAction::CompleteLocalCommand);
        assert_eq!(
            controller.view.local_command.as_ref().unwrap().command,
            "cat song-"
        );
        controller.dispatch(UiAction::CompleteLocalCommand);
        assert_eq!(
            controller.view.local_command.as_ref().unwrap().command,
            "cat song-a.flac"
        );
        controller.dispatch(UiAction::DeleteLocalCommandCharacter);
        controller.dispatch(UiAction::CompleteLocalCommand);
        assert_eq!(
            controller.view.local_command.as_ref().unwrap().command,
            "cat song-a.flac",
            "editing cancels the cycle, rather than choosing song-b"
        );
    }

    #[test]
    fn command_prompt_requires_a_terminal_but_opens_on_every_tab_without_selection() {
        let (mut controller, _directory, _selected, _requests) = fixture();
        controller.set_local_command_available(false);
        controller.dispatch(UiAction::BeginLocalCommand);
        assert!(controller.view.local_command.is_none());
        controller.set_local_command_available(true);
        controller.local_listing = None;
        controller.view.rows.clear();
        controller.view.details = None;
        for screen in Screen::ALL {
            for (idle, paused) in [(true, false), (false, false), (false, true)] {
                controller.view.screen = screen;
                controller.view.playback.idle = idle;
                controller.view.playback.paused = paused;
                controller.dispatch(UiAction::BeginLocalCommand);
                assert!(
                    controller.view.local_command.is_some(),
                    "{screen:?}, idle={idle}, paused={paused}"
                );
                controller.dispatch(UiAction::DismissLocalCommand);
            }
        }
        assert!(controller.take_custom_command_plan().is_none());
    }

    /// The synthetic parent row is a real command target, not an empty selection.
    #[test]
    fn command_prompt_targets_parent_folder_without_changing_working_directory() {
        let (mut controller, _directory, selected, _requests) = fixture();
        controller.view.selected = 0;
        controller.dispatch(UiAction::BeginLocalCommand);
        type_text(&mut controller, "printf '%s\\n' % %d");
        controller.dispatch(UiAction::SubmitLocalCommand);
        let plan = controller.take_custom_command_plan().unwrap();
        let current = selected.parent().unwrap();
        let parent = current.parent().unwrap();
        assert_eq!(plan.argument, parent.as_os_str());
        assert_eq!(plan.downloaded_path.as_deref(), Some(parent));
        assert_eq!(plan.directory, current);
    }

    /// Playing metadata never supplies a missing selection, while plain shell commands work.
    #[test]
    fn command_prompt_without_selection_rejects_macros_but_accepts_literal_percent() {
        let (mut controller, _directory, _selected, requests) = fixture();
        controller.view.screen = Screen::Search;
        controller.view.rows.clear();
        controller.view.details = None;
        controller.view.playback.idle = false;
        controller.view.now_playing = Some(NowPlayingView {
            media_id: MediaId::new(SourceKind::YouTube, "playing-id"),
            title: "Playing item is not selected".to_owned(),
            subtitle: String::new(),
        });
        for template in ["echo %", "echo %d"] {
            controller.dispatch(UiAction::BeginLocalCommand);
            type_text(&mut controller, template);
            controller.dispatch(UiAction::SubmitLocalCommand);
            assert!(controller.view.local_command.is_some());
            assert!(controller.view.status_line.contains("Select an item"));
            assert!(controller.take_custom_command_plan().is_none());
            assert!(!history_path(&controller.config).exists());
            controller.dispatch(UiAction::DismissLocalCommand);
        }
        controller.dispatch(UiAction::BeginLocalCommand);
        type_text(&mut controller, "printf '%s' '100%'");
        controller.dispatch(UiAction::SubmitLocalCommand);
        let plan = controller.take_custom_command_plan().unwrap();
        assert!(plan.argument.is_empty());
        assert_eq!(plan.directory, std::env::current_dir().unwrap());
        controller.report_custom_command_result(Ok(crate::local_command::CommandOutput {
            output: String::new(),
            success: true,
        }));
        assert_eq!(controller.view.screen, Screen::Search);
        assert!(
            requests.try_recv().is_err(),
            "remote commands must not refresh Local"
        );
    }

    #[cfg(feature = "local-archives")]
    #[test]
    fn extracted_archives_do_not_expose_mutable_cache_paths_to_commands() {
        let (mut controller, directory, selected, _requests) = fixture();
        controller
            .local_archive_stack
            .push(crate::local_archive::MaterializedLocalArchive {
                source_path: directory.path().join("album.zip"),
                root_path: selected.parent().unwrap().to_owned(),
                reused_cache: false,
            });
        controller.dispatch(UiAction::BeginLocalCommand);
        assert!(controller.view.local_command.is_none());
        assert!(controller.view.status_line.contains("read-only archive"));
    }

    #[test]
    fn empty_submission_and_empty_history_matches_never_create_a_plan() {
        let (mut controller, _directory, _selected, _requests) = fixture();
        controller.dispatch(UiAction::BeginLocalCommand);
        type_text(&mut controller, "  ");
        controller.dispatch(UiAction::SubmitLocalCommand);
        assert!(controller.take_custom_command_plan().is_none());
        controller.dispatch(UiAction::OpenLocalCommandHistory);
        type_text(&mut controller, "no match");
        controller.dispatch(UiAction::SubmitLocalCommand);
        assert!(controller.take_custom_command_plan().is_none());
        assert!(controller.view.local_command.is_some());
        assert!(!history_path(&controller.config).exists());
    }

    #[test]
    fn command_history_is_private_plaintext_durable_bounded_and_ignores_invalid_lines() {
        let directory = crate::test_support::canonical_tempdir("command history persistence");
        let path = directory.path().join("config/cmd-log");
        let entries: Vec<_> = (0..120).map(|index| format!("echo {index}")).collect();
        write_history(&path, &entries).unwrap();
        assert_eq!(read_history(&path).unwrap(), entries[20..]);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .starts_with("echo 20\necho 21\n")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        fs::write(
            &path,
            format!(
                "echo good\n\x1bbad\n\n{}\necho last\n",
                "x".repeat(COMMAND_LIMIT + 1)
            ),
        )
        .unwrap();
        assert_eq!(read_history(&path).unwrap(), ["echo good", "echo last"]);
        fs::write(&path, vec![b'x'; HISTORY_FILE_LIMIT + 1]).unwrap();
        assert!(read_history(&path).is_err());
        fs::write(&path, [0xff]).unwrap();
        assert!(read_history(&path).is_err());
    }

    #[test]
    fn command_history_reloads_across_controller_instances_without_executing() {
        let (mut controller, _directory, selected, _requests) = fixture();
        let config = controller.config.clone();
        controller.dispatch(UiAction::BeginLocalCommand);
        type_text(&mut controller, "echo prior %");
        controller.dispatch(UiAction::SubmitLocalCommand);
        assert!(controller.take_custom_command_plan().is_some());
        drop(controller);
        let mut controller =
            AppController::new(config, StateStore::open_in_memory().unwrap(), None, None);
        controller.shutdown_local_browse_worker();
        controller.diagnostic_helpers_cache = Some(Vec::new());
        controller.view.screen = Screen::Local;
        controller.local_listing = Some(
            crate::local_browser::list_local_directory(
                selected.parent().unwrap(),
                crate::local_browser::LocalBrowseLimits::default(),
            )
            .unwrap(),
        );
        controller.refresh_local_browser_rows();
        controller.select_local_path(Some(&selected));
        controller.set_local_command_available(true);
        controller.dispatch(UiAction::BeginLocalCommand);
        controller.dispatch(UiAction::BrowseLocalCommandHistory(-1));
        assert_eq!(
            controller.view.local_command.as_ref().unwrap().command,
            "echo prior %"
        );
        assert!(controller.take_custom_command_plan().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn history_refuses_links_without_changing_their_targets() {
        let directory = crate::test_support::canonical_tempdir("command history symlink");
        let target = directory.path().join("target");
        let path = directory.path().join("cmd-log");
        fs::write(&target, b"untouched\n").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(read_history(&path).is_err());
        assert!(write_history(&path, &["echo nope".to_owned()]).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"untouched\n");
    }
}
