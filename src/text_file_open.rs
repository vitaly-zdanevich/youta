//! Safe command planning for opening local text files.
//!
//! Graphical and pseudo-terminal sessions delegate to the operating system's
//! default file association. A physical Linux virtual console instead uses a
//! confidently recognized terminal editor, falling back to Vim. This module
//! only produces command plans; process execution belongs to the TUI lifecycle
//! so it can suspend raw mode before a terminal editor takes over the console.

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Operating-system behavior relevant to opening a text file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextFilePlatform {
    /// Linux, including desktop sessions and physical virtual consoles.
    Linux,
    /// macOS.
    MacOs,
    /// Windows.
    Windows,
    /// Another Unix-like target that follows the freedesktop opener convention.
    OtherUnix,
}

impl TextFilePlatform {
    /// Returns the platform selected by the current compilation target.
    #[must_use]
    pub const fn current() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::OtherUnix
        }
    }
}

/// Observable facts used to choose a text-file opener.
///
/// Keeping these facts injectable lets tests cover Linux-console and desktop
/// behavior without changing the process environment or starting an editor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextFileOpenContext {
    /// Compiled operating-system family.
    pub platform: TextFilePlatform,
    /// Whether Youta confirmed a direct attachment to `/dev/ttyN` on Linux.
    pub physical_linux_virtual_console: bool,
    /// User's `VISUAL` environment setting, when present.
    pub visual: Option<OsString>,
    /// User's `EDITOR` environment setting, when present.
    pub editor: Option<OsString>,
}

impl TextFileOpenContext {
    /// Captures the current target and editor environment.
    ///
    /// The physical-console result is supplied by Youta's stricter terminal
    /// attachment detector; `TERM=linux` alone is not sufficient evidence.
    #[must_use]
    pub fn current(physical_linux_virtual_console: bool) -> Self {
        Self {
            platform: TextFilePlatform::current(),
            physical_linux_virtual_console,
            visual: env::var_os("VISUAL"),
            editor: env::var_os("EDITOR"),
        }
    }
}

/// Origin of the executable selected by a text-file plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextFileOpenerSource {
    /// The operating system's default file association.
    SystemDefault,
    /// A recognized terminal editor from `VISUAL`.
    VisualEnvironment,
    /// A recognized terminal editor from `EDITOR`.
    EditorEnvironment,
    /// Vim selected because neither environment setting was safely usable.
    VimFallback,
}

/// Process lifecycle required by a text-file opener.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextFileOpenLifecycle {
    /// Start the graphical/system opener without surrendering the TUI.
    Detached,
    /// Suspend the TUI, wait for the editor, then restore the terminal.
    SuspendTuiAndWait,
}

/// A direct, shell-free command for opening one local text file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextFileOpenPlan {
    /// Executable passed directly to `std::process::Command`.
    pub executable: PathBuf,
    /// Arguments passed directly to the executable.
    ///
    /// The selected file path is always exactly one element in this vector.
    pub arguments: Vec<OsString>,
    /// Why this executable was selected.
    pub source: TextFileOpenerSource,
    /// How the TUI must manage the child process.
    pub lifecycle: TextFileOpenLifecycle,
}

/// Plans how to open `path` without invoking a shell.
///
/// A physical-console override is honored only on Linux. In all other cases,
/// the native system opener handles the default application association.
#[must_use]
pub fn plan_text_file_open(path: &Path, context: &TextFileOpenContext) -> TextFileOpenPlan {
    if context.platform == TextFilePlatform::Linux && context.physical_linux_virtual_console {
        return plan_terminal_editor(path, context);
    }

    plan_system_default(path, context.platform)
}

fn plan_terminal_editor(path: &Path, context: &TextFileOpenContext) -> TextFileOpenPlan {
    let configured = context
        .visual
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(|value| (value, TextFileOpenerSource::VisualEnvironment))
        .or_else(|| {
            context
                .editor
                .as_deref()
                .filter(|value| !value.is_empty())
                .map(|value| (value, TextFileOpenerSource::EditorEnvironment))
        });
    let selected = configured.and_then(|(value, source)| {
        recognized_terminal_editor(value).map(|command| (command, source))
    });

    let (mut command, source) = selected.unwrap_or_else(|| {
        (
            EditorCommand {
                executable: PathBuf::from("vim"),
                arguments: Vec::new(),
            },
            TextFileOpenerSource::VimFallback,
        )
    });
    command.arguments.push(path.as_os_str().to_owned());

    TextFileOpenPlan {
        executable: command.executable,
        arguments: command.arguments,
        source,
        lifecycle: TextFileOpenLifecycle::SuspendTuiAndWait,
    }
}

fn plan_system_default(path: &Path, platform: TextFilePlatform) -> TextFileOpenPlan {
    let (executable, mut arguments) = match platform {
        TextFilePlatform::MacOs => (PathBuf::from("/usr/bin/open"), Vec::new()),
        TextFilePlatform::Windows => (
            PathBuf::from("rundll32.exe"),
            vec![OsString::from("url.dll,FileProtocolHandler")],
        ),
        TextFilePlatform::Linux | TextFilePlatform::OtherUnix => {
            (PathBuf::from("xdg-open"), Vec::new())
        }
    };
    arguments.push(path.as_os_str().to_owned());

    TextFileOpenPlan {
        executable,
        arguments,
        source: TextFileOpenerSource::SystemDefault,
        lifecycle: TextFileOpenLifecycle::Detached,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EditorCommand {
    executable: PathBuf,
    arguments: Vec<OsString>,
}

/// Parses only unambiguous terminal-editor settings.
///
/// Environment editor variables conventionally contain shell syntax. Youta
/// deliberately accepts either one executable token or the two documented
/// non-graphical Emacs forms. Everything else falls back to Vim instead of
/// guessing, evaluating quoting, or accidentally launching a GUI editor.
fn recognized_terminal_editor(value: &OsStr) -> Option<EditorCommand> {
    let value = value.to_str()?.trim();
    if value.is_empty() || value.chars().any(is_shell_quoting_character) {
        return None;
    }

    let tokens = value.split_ascii_whitespace().collect::<Vec<_>>();
    let executable = PathBuf::from(*tokens.first()?);
    let name = executable.file_name()?.to_str()?.to_ascii_lowercase();
    let arguments = match (name.as_str(), tokens.as_slice()) {
        (
            "vi" | "vim" | "nvim" | "nano" | "micro" | "hx" | "helix" | "kak" | "kakoune" | "joe"
            | "jed" | "mg",
            [_],
        ) => Vec::new(),
        ("emacs", [_, "-nw"]) => vec![OsString::from("-nw")],
        ("emacs", [_, "--no-window-system"]) => {
            vec![OsString::from("--no-window-system")]
        }
        ("emacsclient", [_, "-t"]) => vec![OsString::from("-t")],
        ("emacsclient", [_, "--tty"]) => vec![OsString::from("--tty")],
        _ => return None,
    };

    Some(EditorCommand {
        executable,
        arguments,
    })
}

fn is_shell_quoting_character(character: char) -> bool {
    matches!(
        character,
        '\'' | '"' | '\\' | '`' | '$' | ';' | '|' | '&' | '<' | '>'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(platform: TextFilePlatform) -> TextFileOpenContext {
        TextFileOpenContext {
            platform,
            physical_linux_virtual_console: false,
            visual: None,
            editor: None,
        }
    }

    #[test]
    fn graphical_sessions_use_native_default_association() {
        let path = Path::new("/tmp/notes with spaces.txt");

        let linux = plan_text_file_open(path, &context(TextFilePlatform::Linux));
        assert_eq!(linux.executable, Path::new("xdg-open"));
        assert_eq!(linux.arguments, [path.as_os_str()]);
        assert_eq!(linux.source, TextFileOpenerSource::SystemDefault);
        assert_eq!(linux.lifecycle, TextFileOpenLifecycle::Detached);

        let macos = plan_text_file_open(path, &context(TextFilePlatform::MacOs));
        assert_eq!(macos.executable, Path::new("/usr/bin/open"));
        assert_eq!(macos.arguments, [path.as_os_str()]);

        let windows = plan_text_file_open(path, &context(TextFilePlatform::Windows));
        assert_eq!(windows.executable, Path::new("rundll32.exe"));
        assert_eq!(
            windows.arguments,
            [OsStr::new("url.dll,FileProtocolHandler"), path.as_os_str()]
        );
    }

    #[test]
    fn physical_console_prefers_recognized_visual_and_waits() {
        let path = Path::new("/tmp/a file;still-one-argument.txt");
        let mut facts = context(TextFilePlatform::Linux);
        facts.physical_linux_virtual_console = true;
        facts.visual = Some(OsString::from("/usr/bin/nvim"));
        facts.editor = Some(OsString::from("nano"));

        let plan = plan_text_file_open(path, &facts);

        assert_eq!(plan.executable, Path::new("/usr/bin/nvim"));
        assert_eq!(plan.arguments, [path.as_os_str()]);
        assert_eq!(plan.source, TextFileOpenerSource::VisualEnvironment);
        assert_eq!(plan.lifecycle, TextFileOpenLifecycle::SuspendTuiAndWait);
    }

    #[test]
    fn physical_console_replaces_a_gui_visual_even_when_editor_is_terminal() {
        let path = Path::new("notes.txt");
        let mut facts = context(TextFilePlatform::Linux);
        facts.physical_linux_virtual_console = true;
        facts.visual = Some(OsString::from("code"));
        facts.editor = Some(OsString::from("nano"));

        let plan = plan_text_file_open(path, &facts);

        assert_eq!(plan.executable, Path::new("vim"));
        assert_eq!(plan.arguments, [path.as_os_str()]);
        assert_eq!(plan.source, TextFileOpenerSource::VimFallback);
    }

    #[test]
    fn physical_console_uses_editor_when_visual_is_empty() {
        let path = Path::new("notes.txt");
        let mut facts = context(TextFilePlatform::Linux);
        facts.physical_linux_virtual_console = true;
        facts.visual = Some(OsString::new());
        facts.editor = Some(OsString::from("nano"));

        let plan = plan_text_file_open(path, &facts);

        assert_eq!(plan.executable, Path::new("nano"));
        assert_eq!(plan.arguments, [path.as_os_str()]);
        assert_eq!(plan.source, TextFileOpenerSource::EditorEnvironment);
    }

    #[test]
    fn physical_console_falls_back_to_vim_for_ambiguous_or_gui_commands() {
        let path = Path::new("notes.txt");
        let mut facts = context(TextFilePlatform::Linux);
        facts.physical_linux_virtual_console = true;
        facts.visual = Some(OsString::from("code --wait"));
        facts.editor = Some(OsString::from("vim -c 'set number'"));

        let plan = plan_text_file_open(path, &facts);

        assert_eq!(plan.executable, Path::new("vim"));
        assert_eq!(plan.arguments, [path.as_os_str()]);
        assert_eq!(plan.source, TextFileOpenerSource::VimFallback);
    }

    #[test]
    fn physical_console_falls_back_to_vim_for_a_gui_editor() {
        let path = Path::new("notes.txt");
        let mut facts = context(TextFilePlatform::Linux);
        facts.physical_linux_virtual_console = true;
        facts.editor = Some(OsString::from("code"));

        let plan = plan_text_file_open(path, &facts);

        assert_eq!(plan.executable, Path::new("vim"));
        assert_eq!(plan.arguments, [path.as_os_str()]);
        assert_eq!(plan.source, TextFileOpenerSource::VimFallback);
        assert_eq!(plan.lifecycle, TextFileOpenLifecycle::SuspendTuiAndWait);
    }

    #[test]
    fn explicitly_terminal_emacs_forms_are_direct_commands() {
        let path = Path::new("notes.txt");
        let mut facts = context(TextFilePlatform::Linux);
        facts.physical_linux_virtual_console = true;
        facts.visual = Some(OsString::from("/usr/bin/emacs -nw"));

        let plan = plan_text_file_open(path, &facts);

        assert_eq!(plan.executable, Path::new("/usr/bin/emacs"));
        assert_eq!(plan.arguments, [OsStr::new("-nw"), path.as_os_str()]);
        assert_eq!(plan.lifecycle, TextFileOpenLifecycle::SuspendTuiAndWait);
    }

    #[test]
    fn physical_console_override_is_ignored_on_non_linux_targets() {
        let path = Path::new("notes.txt");
        let mut facts = context(TextFilePlatform::MacOs);
        facts.physical_linux_virtual_console = true;
        facts.visual = Some(OsString::from("vim"));

        let plan = plan_text_file_open(path, &facts);

        assert_eq!(plan.executable, Path::new("/usr/bin/open"));
        assert_eq!(plan.source, TextFileOpenerSource::SystemDefault);
        assert_eq!(plan.lifecycle, TextFileOpenLifecycle::Detached);
    }
}
