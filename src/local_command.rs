//! Explicit Bash commands with optional media, image, description, and playback data.
//!
//! Only the user-authored template is shell source. Captured targets and playback
//! timestamps travel as positional arguments, never as interpolated shell syntax.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

pub mod buttons;
mod capture;
mod metadata;

pub use metadata::CommandMetadata;

/// Builds the Bash process used by both command wrappers and their execution fixtures.
fn bash_command() -> Command {
    #[cfg(windows)]
    let program = windows_bash_program_from_path(std::env::var_os("PATH").as_deref());
    #[cfg(not(windows))]
    let program = OsString::from("bash");
    Command::new(program)
}

/// Resolves Windows Bash in PATH order before the implicit system-directory lookup.
///
/// Rust searches System32 before the inherited PATH for bare program names, so
/// the legacy WSL launcher can shadow Git Bash. Selecting an absolute executable
/// preserves the caller's PATH choice without rewriting command source or data.
#[cfg(any(windows, test))]
fn windows_bash_program_from_path(search_path: Option<&std::ffi::OsStr>) -> OsString {
    if let Some(search_path) = search_path {
        for directory in std::env::split_paths(search_path).filter(|path| path.is_absolute()) {
            let candidate = directory.join("bash.exe");
            if candidate.is_file() {
                return candidate.into_os_string();
            }
        }
    }
    OsString::from("bash")
}

/// A user-authored command with an optional captured local path or original provider URL.
#[derive(Clone, PartialEq, Eq)]
pub struct ShellCommandPlan {
    /// Explicit shell source from the user's prompt or command configuration.
    pub template: String,
    /// Selected target transported as one OS-string argument, never shell source.
    /// Empty when the invocation has no selected target.
    pub argument: OsString,
    /// Completed media file for `%d`, absent until a requested download succeeds.
    pub downloaded_path: Option<PathBuf>,
    /// Prepared image file for `%i`, absent until the captured artwork is available.
    pub image_path: Option<PathBuf>,
    /// Selected description for `%m`, kept private and prepared only when requested.
    pub metadata: Option<Arc<CommandMetadata>>,
    /// Playback cursor frozen at invocation for `%t`, independent of the selected target.
    pub playback_position: Option<Duration>,
    /// Working directory captured when the user invokes the command.
    pub directory: PathBuf,
}

impl fmt::Debug for ShellCommandPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShellCommandPlan")
            .finish_non_exhaustive()
    }
}

/// Bounded command output intended only for the explicit desktop result dialog.
#[derive(Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Captured standard output and standard error, never added to diagnostics.
    pub output: String,
    /// Whether the shell exited successfully.
    pub success: bool,
}

impl fmt::Debug for CommandOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandOutput")
            .field("success", &self.success)
            .finish_non_exhaustive()
    }
}

/// A command and its captured Local selection, consumed once by the terminal.
#[derive(Clone, PartialEq, Eq)]
pub struct LocalCommandPlan {
    /// Single-line Bash source entered explicitly by the user.
    pub template: String,
    /// Full selected filesystem path, independent of shortened display paths.
    pub path: PathBuf,
    /// Actual Local directory used as the command's working directory.
    pub directory: PathBuf,
}

impl fmt::Debug for LocalCommandPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalCommandPlan")
            .finish_non_exhaustive()
    }
}

impl LocalCommandPlan {
    /// Builds a foreground Bash invocation without exposing filenames as code.
    ///
    /// A fixed interactive outer shell owns terminal job control and restores
    /// it on exit. The user's code runs in a separate noninteractive shell, so
    /// even `exec`, `exit`, or Ctrl+C cannot replace the restoring outer shell.
    /// Both shells skip startup files and neither writes Bash's own history.
    /// The caller must surrender its terminal/input reader before execution.
    ///
    /// # Errors
    ///
    /// Rejects percent macros combined with complex Bash grammar instead of
    /// guessing nested quoting rules. Such commands can explicitly use `"$1"`
    /// before modifying their positional parameters.
    pub fn command(&self) -> Result<Command, String> {
        self.clone().into_shell_plan().command()
    }

    /// Reuses the provider-neutral shell runner without changing Local path semantics.
    #[must_use]
    pub fn into_shell_plan(self) -> ShellCommandPlan {
        ShellCommandPlan {
            template: self.template,
            downloaded_path: Some(self.path.clone()),
            image_path: None,
            metadata: None,
            playback_position: None,
            argument: self.path.into_os_string(),
            directory: self.directory,
        }
    }
}

impl ShellCommandPlan {
    /// Builds the foreground TTY wrapper used by both prompts and configured buttons.
    ///
    /// The explicit final `exit` prevents Bash 5.2 from replacing the outer
    /// shell with its final command, so job control restores the original
    /// foreground process group while preserving the command's exit status.
    /// Keep this plan or a clone alive until the child exits: `%m` reads a private
    /// file whose lifetime belongs to the captured metadata, not to [`Command`].
    ///
    /// # Errors
    /// Returns a validation error for unsupported percent-macro shell grammar.
    pub fn command(&self) -> Result<Command, String> {
        let expanded = self.expanded_source()?;
        let metadata_path = self.metadata_argument()?;
        let playback_position = self.playback_position_argument()?;
        let mut command = bash_command();
        command
            .args([
                "--noprofile",
                "--norc",
                "-i",
                "-c",
                "\"$BASH\" --noprofile --norc -c \"$1\" youta \"$2\" \"$3\" \"$4\" \"$5\" \"$6\"; exit \"$?\"",
                "youta",
            ])
            .arg(expanded)
            .arg(&self.argument)
            .arg(
                self.downloaded_path
                    .as_deref()
                    .unwrap_or(std::path::Path::new("")),
            )
            .arg(
                self.image_path
                    .as_deref()
                    .unwrap_or(std::path::Path::new("")),
            )
			.arg(metadata_path)
			.arg(playback_position)
            .current_dir(&self.directory)
            .env("HISTFILE", "/dev/null")
            .env_remove("BASH_ENV")
            .env_remove("ENV");
        Ok(command)
    }

    /// Builds a noninteractive shell for a desktop worker, with the same safe argument.
    ///
    /// Keep the plan alive until the child exits so its private `%m` file remains available.
    ///
    /// # Errors
    /// Returns a validation error for unsupported percent-macro shell grammar.
    pub fn noninteractive_command(&self) -> Result<Command, String> {
        let expanded = self.expanded_source()?;
        let metadata_path = self.metadata_argument()?;
        let playback_position = self.playback_position_argument()?;
        let mut command = bash_command();
        command
            .args(["--noprofile", "--norc", "-c"])
            .arg(expanded)
            .arg("youta")
            .arg(&self.argument)
            .arg(
                self.downloaded_path
                    .as_deref()
                    .unwrap_or(std::path::Path::new("")),
            )
            .arg(
                self.image_path
                    .as_deref()
                    .unwrap_or(std::path::Path::new("")),
            )
            .arg(metadata_path)
            .arg(playback_position)
            .current_dir(&self.directory)
            .env("HISTFILE", "/dev/null")
            .env_remove("BASH_ENV")
            .env_remove("ENV");
        Ok(command)
    }

    /// Detects whether an unquoted `%`, `%d`, `%i`, or `%m` requires a selected target.
    ///
    /// The playback-position marker `%t` is independent of the selected target.
    /// Quoted, escaped, commented, and embedded percent signs remain literal, so
    /// ordinary commands can run without selecting a media item.
    ///
    /// # Errors
    /// Returns the same grammar validation errors as command construction.
    pub fn requires_selection(&self) -> Result<bool, String> {
        expand_targets(&self.template).map(|expansion| expansion.requires_selection)
    }

    /// Detects the unquoted `%d` macro without mistaking printf formats for downloads.
    ///
    /// # Errors
    /// Returns the same grammar validation errors as command construction.
    pub fn requires_download(&self) -> Result<bool, String> {
        expand_targets(&self.template).map(|expansion| expansion.requires_download)
    }

    /// Detects an unquoted `%i` macro without treating printf formats as image requests.
    ///
    /// # Errors
    /// Returns the same grammar validation errors as command construction.
    pub fn requires_image(&self) -> Result<bool, String> {
        expand_targets(&self.template).map(|expansion| expansion.requires_image)
    }

    /// Detects an unquoted `%m` without preparing the captured description file.
    ///
    /// # Errors
    /// Returns the same grammar validation errors as command construction.
    pub fn requires_metadata(&self) -> Result<bool, String> {
        expand_targets(&self.template).map(|expansion| expansion.requires_metadata)
    }

    /// Detects an unquoted `%t` without requiring a selected media item.
    ///
    /// # Errors
    /// Returns the same grammar validation errors as command construction.
    pub fn requires_playback_position(&self) -> Result<bool, String> {
        expand_targets(&self.template).map(|expansion| expansion.requires_playback_position)
    }

    /// Formats only requested cursor data as one argument, keeping it out of shell source.
    fn playback_position_argument(&self) -> Result<String, String> {
        if !self.requires_playback_position()? {
            return Ok(String::new());
        }
        self.playback_position
            .map(format_playback_position)
            .ok_or_else(|| "The command requires a captured playback position for %t".to_owned())
    }

    /// Prepares the private description file only when an executable plan requests `%m`.
    fn metadata_argument(&self) -> Result<PathBuf, String> {
        if !self.requires_metadata()? {
            return Ok(PathBuf::new());
        }
        self.metadata
            .as_ref()
            .ok_or_else(|| "The command requires selected description text for %m".to_owned())?
            .path()
    }

    /// Refuses unresolved media, image, description, and playback macros, including direct calls.
    fn expanded_source(&self) -> Result<String, String> {
        let expansion = expand_targets(&self.template)?;
        if expansion.requires_download && self.downloaded_path.is_none() {
            return Err("The command requires a completed download for %d".to_owned());
        }
        if expansion.requires_image && self.image_path.is_none() {
            return Err("The command requires a prepared image for %i".to_owned());
        }
        if expansion.requires_metadata && self.metadata.is_none() {
            return Err("The command requires selected description text for %m".to_owned());
        }
        if expansion.requires_playback_position && self.playback_position.is_none() {
            return Err("The command requires a captured playback position for %t".to_owned());
        }
        Ok(expansion.source)
    }
}

/// Formats a captured cursor as `HH:MM:SS.mmm`, retaining hours beyond one day.
///
/// Integer Duration components avoid floating-point rounding and discard submillisecond data.
fn format_playback_position(position: Duration) -> String {
    let seconds = position.as_secs();
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    let milliseconds = position.subsec_millis();
    format!("{hours:02}:{minutes:02}:{seconds:02}.{milliseconds:03}")
}

/// Expands percent markers that start simple, unquoted shell arguments.
///
/// `printf '%s\n' %` preserves its format and passes the selected path as one
/// argument; `%.mp3` appends a suffix to that same argument. Quoted, escaped,
/// and embedded percent characters remain literal. Complex Bash grammar is
/// deliberately not parsed: combining it with a detected macro is rejected,
/// and callers can instead use the original selected path in quoted `"$1"`.
/// A private readonly variable preserves macros after the user runs `set --`.
/// `%m` prints the captured description at a simple command start, or supplies
/// one quoted argument after a command name. Its private file preserves long
/// descriptions and trailing newlines without inserting provider text as code.
/// `%t` supplies the captured playback position as one quoted argument, independently of selection.
fn expand_selected_path(template: &str) -> Result<String, String> {
    expand_targets(template).map(|expansion| expansion.source)
}

/// Validated shell source and the captured-data requirements discovered by its lexer.
struct TargetExpansion {
    /// Shell source with recognized target macros replaced by readonly variables.
    source: String,
    /// At least one original, downloaded, image, or description macro was recognized.
    requires_selection: bool,
    /// A downloaded target macro needs a completed local file before execution.
    requires_download: bool,
    /// An image target macro needs a prepared local artwork file before execution.
    requires_image: bool,
    /// Description text must have been captured from the selected Details pane.
    requires_metadata: bool,
    /// Playback position must have been captured when the command was invoked.
    requires_playback_position: bool,
}

/// Recognizes assignment prefixes that make a later `%m` command position ambiguous.
fn shell_assignment_prefix(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let name = name.strip_suffix('+').unwrap_or(name);
    let name = name.split_once('[').map_or(name, |(name, _)| name);
    let mut characters = name.chars();
    matches!(characters.next(), Some('a'..='z' | 'A'..='Z' | '_'))
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

/// Lexes argument macros and standalone `%m` commands without parsing nested Bash grammar.
fn expand_targets(template: &str) -> Result<TargetExpansion, String> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        None,
        Single,
        AnsiSingle,
        Double,
    }
    let mut result = String::with_capacity(template.len());
    let mut quote = Quote::None;
    let mut word_start = true;
    let mut expanded = false;
    let mut selection = false;
    let mut download = false;
    let mut image = false;
    let mut metadata = false;
    let mut playback_position = false;
    let mut command_start = true;
    let mut first_word = String::new();
    let mut first_word_finished = false;
    let mut ambiguous_metadata_position = false;
    let mut previous = None;
    // Track only the first word and ordinary separators of each simple command.
    // Leading assignments, redirections, or control words make `%m` ambiguous,
    // so reject those combinations instead of guessing Bash's command position.
    // Conservatively reject nested grammar whenever a macro was recognized.
    // Looking for these spellings even in quoted text intentionally favors a
    // clear limitation over pretending this small scanner is a Bash parser.
    let mut complex = ["$(", "${", "((", "[[", "`", "<<"]
        .iter()
        .any(|syntax| template.contains(syntax));
    let mut characters = template.chars().peekable();
    while let Some(character) = characters.next() {
        let at_command_start = command_start;
        if quote == Quote::None
            && matches!(character, ' ' | '\t' | '\n' | ';' | '|' | '&' | '<' | '>')
        {
            if !first_word_finished && !first_word.is_empty() {
                ambiguous_metadata_position |= shell_assignment_prefix(&first_word)
                    || matches!(
                        first_word.as_str(),
                        "!" | "time"
                            | "if"
                            | "then"
                            | "else"
                            | "elif"
                            | "while"
                            | "until"
                            | "do"
                            | "for"
                            | "select"
                            | "case"
                            | "in"
                            | "function"
                    );
                first_word_finished = true;
            }
            let redirect_ampersand = character == '&'
                && (characters.peek() == Some(&'>') || matches!(previous, Some('<' | '>')));
            if matches!(character, '<' | '>') || redirect_ampersand {
                ambiguous_metadata_position |= command_start
                    || (!first_word.is_empty()
                        && first_word
                            .chars()
                            .all(|character| character.is_ascii_digit()));
            } else if matches!(character, '\n' | ';' | '|' | '&') {
                command_start = true;
                first_word.clear();
                first_word_finished = false;
                ambiguous_metadata_position = false;
            }
        } else {
            if !first_word_finished {
                first_word.push(character);
            }
            command_start = false;
        }
        match (quote, character) {
            (Quote::None, '%') if word_start => {
                if characters.peek() == Some(&'d') {
                    characters.next();
                    result.push_str("\"${__youta_downloaded_path}\"");
                    download = true;
                    selection = true;
                } else if characters.peek() == Some(&'i') {
                    characters.next();
                    result.push_str("\"${__youta_image_path}\"");
                    image = true;
                    selection = true;
                } else if characters.peek() == Some(&'m') {
                    characters.next();
                    if ambiguous_metadata_position {
                        return Err(
							"The %m shortcut supports simple command or argument positions only; leading assignments, leading redirects, and Bash control statements are unsupported"
								.to_owned(),
						);
                    }
                    if at_command_start {
                        if characters.peek().is_some_and(|character| {
                            !matches!(character, ' ' | '\t' | '\n' | ';' | '|' | '&' | '<' | '>')
                        }) {
                            return Err("The command-position %m shortcut must be a standalone word; use printf '%s' %m for description suffixes".to_owned());
                        }
                        result.push_str("builtin printf '%s' \"${__youta_metadata}\"");
                    } else {
                        result.push_str("\"${__youta_metadata}\"");
                    }
                    metadata = true;
                    selection = true;
                } else if characters.peek() == Some(&'t') {
                    characters.next();
                    result.push_str("\"${__youta_playback_position}\"");
                    playback_position = true;
                } else {
                    result.push_str("\"${__youta_selected_path}\"");
                    selection = true;
                }
                expanded = true;
                word_start = false;
            }
            (Quote::None, '#') if word_start => {
                result.push(character);
                result.extend(characters);
                break;
            }
            (Quote::None, '(' | ')' | '{' | '}') => {
                complex = true;
                result.push(character);
                word_start = true;
            }
            (Quote::None, '$') if characters.peek() == Some(&'\'') => {
                result.push_str("$'");
                characters.next();
                quote = Quote::AnsiSingle;
                word_start = false;
            }
            (Quote::None, '\'') => {
                result.push(character);
                quote = Quote::Single;
                word_start = false;
            }
            (Quote::Single | Quote::AnsiSingle, '\'') => {
                result.push(character);
                quote = Quote::None;
            }
            (Quote::None, '"') => {
                result.push(character);
                quote = Quote::Double;
                word_start = false;
            }
            (Quote::Double, '"') => {
                result.push(character);
                quote = Quote::None;
            }
            (Quote::None | Quote::Double | Quote::AnsiSingle, '\\') => {
                result.push(character);
                if let Some(escaped) = characters.next() {
                    result.push(escaped);
                }
                word_start = false;
            }
            (Quote::None, character) => {
                result.push(character);
                word_start = matches!(character, ' ' | '\t' | '\n' | ';' | '|' | '&' | '<' | '>');
            }
            _ => result.push(character),
        }
        previous = Some(character);
    }
    if !expanded {
        return Ok(TargetExpansion {
            source: template.to_owned(),
            requires_selection: false,
            requires_download: false,
            requires_image: false,
            requires_metadata: false,
            requires_playback_position: false,
        });
    }
    if complex {
        return Err(
			"The % path shortcuts support simple unquoted arguments only; for Bash substitutions, arithmetic, grouping, or backticks, use the selected path as quoted \"$1\", downloaded path as quoted \"$2\", or image path as quoted \"$3\" before changing positional parameters. The %m description and %t playback-position shortcuts also require simple shell grammar"
                .to_owned(),
        );
    }
    let download_prefix = if download {
        "readonly __youta_downloaded_path=\"$2\"; "
    } else {
        ""
    };
    let image_prefix = if image {
        "readonly __youta_image_path=\"$3\"; "
    } else {
        ""
    };
    let metadata_prefix = if metadata {
        concat!(
            "if ! builtin test -r \"$4\"; then builtin printf '%s\\n' 'Cannot read captured description for %m' >&2; exit 1; fi; ",
            "__youta_metadata=''; IFS= builtin read -r -d '' __youta_metadata < \"$4\" || ",
            "if builtin test \"$?\" -ne 1; then builtin printf '%s\\n' 'Cannot read captured description for %m' >&2; exit 1; fi; ",
            "readonly __youta_metadata; "
        )
    } else {
        ""
    };
    let selection_prefix = if selection {
        "readonly __youta_selected_path=\"$1\"; "
    } else {
        ""
    };
    let playback_position_prefix = if playback_position {
        "readonly __youta_playback_position=\"$5\"; "
    } else {
        ""
    };
    Ok(TargetExpansion {
        source: format!(
            "{selection_prefix}{download_prefix}{image_prefix}{metadata_prefix}{playback_position_prefix}{result}"
        ),
        requires_selection: selection,
        requires_download: download,
        requires_image: image,
        requires_metadata: metadata,
        requires_playback_position: playback_position,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Native Bash on PATH must win over Windows' implicit System32 WSL launcher lookup.
    #[test]
    fn windows_bash_resolution_uses_absolute_path_order() {
        let directory = tempfile::tempdir().unwrap();
        let native = directory.path().join("Git bin");
        let system = directory.path().join("System32");
        std::fs::create_dir(&native).unwrap();
        std::fs::create_dir(&system).unwrap();
        let native_bash = native.join("bash.exe");
        let system_bash = system.join("bash.exe");
        std::fs::write(&native_bash, b"native Bash fixture").unwrap();
        std::fs::write(&system_bash, b"WSL launcher fixture").unwrap();
        for (directories, expected) in [
            ([&native, &system], &native_bash),
            ([&system, &native], &system_bash),
        ] {
            let search_path = std::env::join_paths(directories).unwrap();
            assert_eq!(
                windows_bash_program_from_path(Some(&search_path)),
                expected.as_os_str()
            );
        }
    }

    /// Explicit lookup ignores missing files, directories, and relative PATH entries.
    #[test]
    fn windows_bash_resolution_ignores_unusable_candidates_and_preserves_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let directory_candidate = directory.path().join("not-an-executable");
        std::fs::create_dir(&directory_candidate).unwrap();
        std::fs::create_dir(directory_candidate.join("bash.exe")).unwrap();
        let native = directory.path().join("Git bin");
        std::fs::create_dir(&native).unwrap();
        std::fs::write(native.join("bash.exe"), b"native Bash fixture").unwrap();
        let relative_directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        std::fs::write(
            relative_directory.path().join("bash.exe"),
            b"relative shell fixture",
        )
        .unwrap();
        let working = std::env::current_dir().unwrap();
        let relative = relative_directory.path().strip_prefix(&working).unwrap();
        let missing = directory.path().join("missing");
        let search_path = std::env::join_paths([
            relative,
            missing.as_path(),
            directory_candidate.as_path(),
            native.as_path(),
        ])
        .unwrap();
        assert_eq!(
            windows_bash_program_from_path(Some(&search_path)),
            native.join("bash.exe").as_os_str()
        );
        for search_path in [None, Some(OsString::new()), Some(missing.into_os_string())] {
            assert_eq!(
                windows_bash_program_from_path(search_path.as_deref()),
                "bash"
            );
        }
    }

    /// The playback marker uses its own argument instead of appending `t` to the selection.
    #[test]
    fn playback_marker_uses_fifth_argument_instead_of_selected_path() {
        let output = bash_command()
            .args(["--noprofile", "--norc", "-c"])
            .arg(expand_targets("printf '%s' %t").unwrap().source)
            .args(["youta", "/tmp/selected file", "", "", "", "01:02:03.456"])
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .output()
            .expect("Bash playback timestamp fixture");
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        assert_eq!(output.stdout, b"01:02:03.456");
    }

    /// Builds private timestamp fixtures without requiring selection or media preparation.
    fn playback_plan(template: &str, position: Option<Duration>) -> ShellCommandPlan {
        ShellCommandPlan {
            template: template.to_owned(),
            argument: OsString::new(),
            downloaded_path: None,
            image_path: None,
            metadata: None,
            playback_position: position,
            directory: std::env::temp_dir(),
        }
    }

    /// Playback timestamps preserve integer precision, fixed widths, and hours beyond 24.
    #[test]
    fn playback_position_format_preserves_hours_and_floors_submilliseconds() {
        for (position, expected) in [
            (Duration::ZERO, "00:00:00.000"),
            (Duration::from_millis(1), "00:00:00.001"),
            (Duration::from_secs(3_600), "01:00:00.000"),
            (Duration::new(3_723, 456_999_999), "01:02:03.456"),
            (Duration::from_secs(90_061), "25:01:01.000"),
            (Duration::from_nanos(999_999), "00:00:00.000"),
            (Duration::from_nanos(1_999_999), "00:00:00.001"),
            (Duration::MAX, "5124095576030431:00:15.999"),
        ] {
            assert_eq!(format_playback_position(position), expected);
        }
    }

    /// A playback marker has an independent requirement and retains existing macro requirements.
    #[test]
    fn playback_requirements_are_independent_of_selected_target_requirements() {
        for (template, selection, download, image, metadata) in [
            ("%t", false, false, false, false),
            ("printf '%s' %t %t.suffix", false, false, false, false),
            ("printf '%s' %t %", true, false, false, false),
            ("printf '%s' %t %d", true, true, false, false),
            ("printf '%s' %t %i", true, false, true, false),
            ("printf '%s' %t %m", true, false, false, true),
        ] {
            let plan = playback_plan(template, None);
            assert!(plan.requires_playback_position().unwrap(), "{template}");
            assert_eq!(plan.requires_selection().unwrap(), selection, "{template}");
            assert_eq!(plan.requires_download().unwrap(), download, "{template}");
            assert_eq!(plan.requires_image().unwrap(), image, "{template}");
            assert_eq!(plan.requires_metadata().unwrap(), metadata, "{template}");
        }
        for template in ["printf '%s' %", "printf '%s' %d", "printf '%s' %i", "%m"] {
            let plan = playback_plan(template, None);
            assert!(!plan.requires_playback_position().unwrap(), "{template}");
            assert!(plan.requires_selection().unwrap(), "{template}");
        }
    }

    /// Quotes, escaped and embedded markers, and comments never request playback data.
    #[test]
    fn literal_playback_markers_do_not_request_a_position() {
        for template in [
            "printf '%t'",
            "printf \"%t\"",
            "printf '%s' $'%t'",
            "printf '%s' \\%t",
            "printf '%s' embedded%t",
            "printf '%s' plain # %t is a comment",
            "printf '%s' \"$(printf '%s' '%t')\"",
        ] {
            let plan = playback_plan(template, None);
            assert!(!plan.requires_playback_position().unwrap(), "{template}");
            assert!(!plan.requires_selection().unwrap(), "{template}");
            assert_eq!(expand_targets(template).unwrap().source, template);
            assert!(plan.command().is_ok(), "{template}");
            assert!(plan.noninteractive_command().is_ok(), "{template}");
        }
    }

    /// Unsupported nested grammar is rejected even when `%t` does not require selection.
    #[test]
    fn playback_markers_preserve_complex_shell_grammar_validation() {
        for template in [
            "printf '%s' $(echo %t)",
            "printf '%s' $((5%2)) %t",
            "(printf '%s' %t)",
            "cat <<EOF %t",
            "printf '%s' `echo plain` %t",
        ] {
            let plan = playback_plan(template, Some(Duration::ZERO));
            let error = plan.requires_playback_position().unwrap_err();
            assert!(
                error.contains("support simple unquoted arguments only"),
                "{template}"
            );
            assert_eq!(plan.requires_selection().unwrap_err(), error);
            assert_eq!(plan.command().unwrap_err(), error);
            assert_eq!(plan.noninteractive_command().unwrap_err(), error);
        }
    }

    /// Both shell builders refuse missing cursor data before a command can execute.
    #[test]
    fn unresolved_playback_marker_rejects_both_command_builders() {
        for template in [
            "printf '%s' %t",
            "set -- replacement; printf '%s' %t.suffix",
        ] {
            let plan = playback_plan(template, None);
            for error in [
                plan.command().unwrap_err(),
                plan.noninteractive_command().unwrap_err(),
            ] {
                assert_eq!(
                    error,
                    "The command requires a captured playback position for %t"
                );
            }
        }
    }

    /// Playback data stays one argument with suffixes, even after positional parameters change.
    #[test]
    fn playback_markers_preserve_suffixes_and_survive_set_without_a_selection() {
        for prefix in ["", "set -- replacement; "] {
            let plan = playback_plan(
                &format!("{prefix}printf '%s\\0' %t %t.suffix"),
                Some(Duration::new(3_723, 456_999_999)),
            );
            for mut command in metadata_test_commands(&plan) {
                let output = command.output().expect("Bash playback suffix fixture");
                assert!(output.status.success(), "{output:?}");
                assert!(output.stderr.is_empty(), "{output:?}");
                assert_eq!(
                    output.stdout,
                    concat!("01:02:03.456\0", "01:02:03.456.suffix\0").as_bytes()
                );
            }
        }
    }

    /// The fifth argument preserves all older mappings and command-position metadata behavior.
    #[test]
    fn playback_argument_does_not_displace_existing_macro_arguments() {
        let mut plan = metadata_plan(
            "set -- replacement; printf '%s\\0' % %d %i %m %t; %m",
            "selected description\n\n",
        );
        plan.downloaded_path = Some(PathBuf::from("/tmp/downloaded media.flac"));
        plan.image_path = Some(PathBuf::from("/tmp/selected artwork.png"));
        plan.playback_position = Some(Duration::new(3_723, 456_000_000));
        let metadata_path = plan.metadata.as_ref().unwrap().path().unwrap();
        let terminal = plan.command().unwrap();
        assert!(
            terminal
                .get_args()
                .nth(4)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("\"$6\"")
        );
        for command in [terminal, plan.noninteractive_command().unwrap()] {
            let arguments = command.get_args().collect::<Vec<_>>();
            let captured = &arguments[arguments.len() - 5..];
            assert_eq!(captured[0], plan.argument.as_os_str());
            assert_eq!(
                captured[1],
                plan.downloaded_path.as_ref().unwrap().as_os_str()
            );
            assert_eq!(captured[2], plan.image_path.as_ref().unwrap().as_os_str());
            assert_eq!(captured[3], metadata_path.as_os_str());
            assert_eq!(captured[4], std::ffi::OsStr::new("01:02:03.456"));
        }
        for mut command in metadata_test_commands(&plan) {
            let output = command.output().expect("Bash combined playback fixture");
            assert!(output.status.success(), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            assert_eq!(
                output.stdout,
                concat!(
                    "https://example.test/original\0/tmp/downloaded media.flac\0",
                    "/tmp/selected artwork.png\0selected description\n\n\0",
                    "01:02:03.456\0selected description\n\n"
                )
                .as_bytes()
            );
        }
        assert_eq!(format!("{plan:?}"), "ShellCommandPlan { .. }");
    }

    /// Unused captured time never appears in either shell's arguments.
    #[test]
    fn unused_playback_position_stays_out_of_shell_arguments() {
        let plan = playback_plan("printf '%s' plain", Some(Duration::new(3_723, 456_000_000)));
        for command in [
            plan.command().unwrap(),
            plan.noninteractive_command().unwrap(),
        ] {
            assert_eq!(command.get_args().last(), Some(std::ffi::OsStr::new("")));
            assert!(
                command
                    .get_args()
                    .all(|argument| !argument.to_string_lossy().contains("01:02:03.456"))
            );
        }
    }

    /// The requested command-position macro saves the complete selected description.
    #[test]
    fn metadata_command_redirects_description_without_added_newlines() {
        let directory = tempfile::Builder::new()
            .prefix("youta metadata Мінск ")
            .tempdir()
            .unwrap();
        let description = "Selected description\nSecond line\n\n";
        let metadata = directory.path().join("metadata ' & description");
        std::fs::write(&metadata, description).unwrap();
        let output = bash_command()
            .args(["--noprofile", "--norc", "-c"])
            .arg(expand_targets("%m > mynote").unwrap().source)
            .args(["youta", "original", "", ""])
            .arg(metadata)
            .current_dir(directory.path())
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .output()
            .expect("Bash metadata redirection fixture");
        assert!(output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert_eq!(
            std::fs::read(directory.path().join("mynote")).unwrap(),
            description.as_bytes()
        );
    }

    /// Captures test descriptions privately without requiring provider or filesystem fixtures.
    fn metadata_plan(template: &str, description: &str) -> ShellCommandPlan {
        ShellCommandPlan {
            template: template.to_owned(),
            argument: "https://example.test/original".into(),
            downloaded_path: None,
            image_path: None,
            metadata: Some(Arc::new(CommandMetadata::new(description))),
            playback_position: None,
            directory: std::env::temp_dir(),
        }
    }

    /// Keeps the foreground argv mapping while omitting job control from pipe-based tests.
    fn metadata_test_commands(plan: &ShellCommandPlan) -> [Command; 2] {
        let terminal = plan.command().unwrap();
        let mut outer = Command::new(terminal.get_program());
        outer
            .args(terminal.get_args().filter(|argument| *argument != "-i"))
            .current_dir(&plan.directory)
            .env_remove("BASH_ENV")
            .env_remove("ENV");
        [outer, plan.noninteractive_command().unwrap()]
    }

    /// Windows-style paths stay opaque arguments through both wrappers, alongside private metadata.
    #[test]
    fn command_wrappers_preserve_windows_paths_and_hostile_arguments() {
        let directory = tempfile::tempdir().unwrap();
        let original = r"C:\Media folder\Мінск ' $(touch COMMAND_INJECTION).flac";
        let downloaded = r"D:\Saved media\%track [01].flac";
        let image = r"E:\Artwork\cover ' & image.png";
        let description = "Selected description ' \" $(touch COMMAND_INJECTION)\n\n";
        let mut plan = metadata_plan("set -- replacement; printf '%s\\0' % %d %i %m", description);
        plan.argument = original.into();
        plan.downloaded_path = Some(PathBuf::from(downloaded));
        plan.image_path = Some(PathBuf::from(image));
        plan.directory = directory.path().to_path_buf();
        let expected = [original, downloaded, image, description]
            .map(|value| format!("{value}\0"))
            .concat();
        for mut command in metadata_test_commands(&plan) {
            let output = command
                .output()
                .expect("Windows-path shell argument fixture");
            assert!(output.status.success(), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            assert_eq!(output.stdout, expected.as_bytes());
        }
        assert!(!directory.path().join("COMMAND_INJECTION").exists());
    }

    /// Metadata has a distinct requirement and remains literal when quoted or embedded.
    #[test]
    fn metadata_detection_preserves_quotes_escapes_comments_and_suffixes() {
        for template in [
            "printf '%m'",
            "printf \"%m\"",
            "printf '%s' $'%m'",
            "printf '%s' \\%m",
            "printf '%s' embedded%m",
            "printf '%s' plain # %m is a comment",
            "printf '%s' \"$(printf '%s' '%m')\"",
        ] {
            let mut plan = metadata_plan(template, "unused description\0");
            plan.metadata = None;
            assert!(!plan.requires_metadata().unwrap(), "{template}");
            assert!(!plan.requires_selection().unwrap(), "{template}");
            assert_eq!(expand_targets(template).unwrap().source, template);
            assert!(plan.command().is_ok(), "{template}");
            assert!(plan.noninteractive_command().is_ok(), "{template}");
        }
        for template in ["%m", "%m > mynote", "printf '%s' %m", "printf '%s' %m.txt"] {
            let plan = metadata_plan(template, "selected description");
            assert!(plan.requires_metadata().unwrap(), "{template}");
            assert!(plan.requires_selection().unwrap(), "{template}");
            assert!(!plan.requires_download().unwrap(), "{template}");
            assert!(!plan.requires_image().unwrap(), "{template}");
        }
    }

    /// Plain commands do not prepare description files or validate unused text.
    #[test]
    fn unused_metadata_never_reaches_the_shell_argument_vector() {
        let plan = metadata_plan("printf '%s' plain", "private description\0");
        for command in [
            plan.command().unwrap(),
            plan.noninteractive_command().unwrap(),
        ] {
            assert_eq!(command.get_args().last(), Some(std::ffi::OsStr::new("")));
            assert!(command.get_args().all(|argument| {
                !argument.to_string_lossy().contains("private description")
                    && !argument
                        .to_string_lossy()
                        .contains("youta-command-metadata-")
            }));
        }
    }

    /// Missing descriptions fail before execution, while a selected empty description is valid.
    #[test]
    fn metadata_requires_a_capture_but_accepts_an_empty_description() {
        for template in ["%m", "printf '%s' %m"] {
            let mut plan = metadata_plan(template, "");
            plan.metadata = None;
            for error in [
                plan.command().unwrap_err(),
                plan.noninteractive_command().unwrap_err(),
            ] {
                assert!(error.contains("selected description text for %m"));
                assert!(!error.contains("example.test"));
            }
            plan.metadata = Some(Arc::new(CommandMetadata::new("")));
            for mut command in metadata_test_commands(&plan) {
                let output = command.output().expect("empty metadata fixture");
                assert!(output.status.success(), "{output:?}");
                assert!(output.stdout.is_empty());
                assert!(output.stderr.is_empty());
            }
        }
    }

    /// Command-position metadata remains an emitter across ordinary shell separators.
    #[test]
    fn metadata_command_positions_follow_simple_shell_boundaries() {
        for template in [
            "%m",
            " \t %m",
            "true; %m",
            "true && %m",
            "false || %m",
            ": | %m",
            "true & %m",
            "true\n%m",
            "VALUE=original; %m",
        ] {
            let plan = metadata_plan(template, "selected description\n\n");
            for mut command in metadata_test_commands(&plan) {
                let output = command.output().expect("metadata command-position fixture");
                assert!(output.status.success(), "{template}: {output:?}");
                assert_eq!(output.stdout, b"selected description\n\n", "{template}");
                assert!(output.stderr.is_empty(), "{template}: {output:?}");
            }
        }
    }

    /// Ambiguous leading assignments and redirections are rejected before macro expansion.
    #[test]
    fn metadata_rejects_ambiguous_command_prefixes_and_command_suffixes() {
        for template in [
            "VALUE=original %m",
            "VALUE='with spaces' %m",
            "VALUE+=original %m",
            "VALUE[0]=original %m",
            "> mynote %m",
            "2> mynote %m",
            "2>&1 %m",
            ">&2 %m",
            "&> mynote %m",
            "< input %m",
            "true; VALUE=original %m",
            "true && 2> mynote %m",
            "! %m",
            "time %m",
            "if true; then %m; fi",
        ] {
            let plan = metadata_plan(template, "private description");
            let error = plan.requires_metadata().unwrap_err();
            assert!(
                error.contains("simple command or argument positions"),
                "{template}: {error}"
            );
            assert_eq!(plan.requires_selection().unwrap_err(), error);
            assert_eq!(plan.command().unwrap_err(), error);
            assert_eq!(plan.noninteractive_command().unwrap_err(), error);
        }
        for template in ["%m.txt", "%m'suffix'", "%m\\ suffix"] {
            let error = expand_targets(template)
                .err()
                .expect("unsupported command suffix");
            assert!(error.contains("standalone word"), "{template}: {error}");
        }
    }

    /// Metadata follows the existing macro prohibition on nested Bash grammar.
    #[test]
    fn metadata_rejects_complex_macro_grammar_without_changing_literals() {
        for template in [
            "printf '%s' $(echo %m)",
            "printf '%s' $((5%2)) %m",
            "(printf '%s' %m)",
            "cat <<EOF %m",
            "printf '%s' `echo plain` %m",
        ] {
            let plan = metadata_plan(template, "private description");
            let error = plan.requires_metadata().unwrap_err();
            assert!(
                error.contains("support simple unquoted arguments only"),
                "{template}: {error}"
            );
            assert_eq!(plan.command().unwrap_err(), error);
            assert_eq!(plan.noninteractive_command().unwrap_err(), error);
        }
    }

    /// Multiline descriptions stay one argument and cannot inject code, including after `set --`.
    #[test]
    fn metadata_arguments_preserve_hostile_text_and_literal_suffixes() {
        let directory = tempfile::tempdir().unwrap();
        let description = "-n ' \"; $(touch METADATA_INJECTION) `touch METADATA_INJECTION` \\\n%m %d %i % * [glob]\nUnicode: Мінск 🦀\n\n\n";
        for prefix in ["", "set -- replacement; "] {
            let mut plan =
                metadata_plan(&format!("{prefix}printf '%s\\0' %m %m.suffix"), description);
            plan.directory = directory.path().to_path_buf();
            for mut command in metadata_test_commands(&plan) {
                let output = command.output().expect("hostile metadata argument fixture");
                assert!(output.status.success(), "{output:?}");
                assert!(output.stderr.is_empty(), "{output:?}");
                assert_eq!(
                    output.stdout,
                    [
                        description.as_bytes(),
                        b"\0",
                        description.as_bytes(),
                        b".suffix\0",
                    ]
                    .concat()
                );
            }
        }
        assert!(!directory.path().join("METADATA_INJECTION").exists());
    }

    /// Description transport does not displace original, downloaded, or artwork arguments.
    #[test]
    fn metadata_and_path_macros_remain_independent_after_set() {
        let mut plan = metadata_plan(
            "set -- replacement; printf '%s\\0' % %d %i %m; %m",
            "selected description\n\n",
        );
        plan.downloaded_path = Some(PathBuf::from("/tmp/downloaded media.flac"));
        plan.image_path = Some(PathBuf::from("/tmp/selected artwork.png"));
        assert!(plan.requires_metadata().unwrap());
        assert!(plan.requires_download().unwrap());
        assert!(plan.requires_image().unwrap());
        for mut command in metadata_test_commands(&plan) {
            let output = command
                .output()
                .expect("combined metadata and path fixture");
            assert!(output.status.success(), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            assert_eq!(
                output.stdout,
                concat!(
                    "https://example.test/original\0/tmp/downloaded media.flac\0",
                    "/tmp/selected artwork.png\0selected description\n\n\0",
                    "selected description\n\n"
                )
                .as_bytes()
            );
        }
    }

    /// Descriptions exceeding the OS argument bound and all trailing newlines survive both shells.
    #[test]
    fn metadata_emitters_preserve_long_descriptions_and_exact_trailing_newlines() {
        for description in [
            String::new(),
            "no final newline".to_owned(),
            "\n\n\n".to_owned(),
            format!("{}\n\n\n", "x".repeat(256 * 1024)),
        ] {
            for template in ["%m", "set -- replacement; %m"] {
                let plan = metadata_plan(template, &description);
                for mut command in metadata_test_commands(&plan) {
                    let output = command.output().expect("exact metadata emitter fixture");
                    assert!(output.status.success(), "{output:?}");
                    assert!(output.stderr.is_empty(), "{output:?}");
                    assert_eq!(output.stdout, description.as_bytes());
                }
            }
        }
    }

    /// File loading does not replace the shell's IFS or treat normal EOF as an errexit failure.
    #[test]
    fn metadata_loading_preserves_ifs_and_errexit_options() {
        let plan = metadata_plan("builtin printf '%s' \"$IFS\"; %m", "selected\n\n");
        for mut command in metadata_test_commands(&plan) {
            let output = command
                .env("SHELLOPTS", "errexit:nounset")
                .output()
                .expect("metadata shell option fixture");
            assert!(output.status.success(), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            assert_eq!(output.stdout, b" \t\nselected\n\n");
        }
    }

    /// The fixed file-read error does not disclose a missing private path or description.
    #[test]
    fn metadata_loading_reports_missing_files_without_private_paths() {
        let output = bash_command()
            .args(["--noprofile", "--norc", "-c"])
            .arg(expand_targets("%m").unwrap().source)
            .args(["youta", "", "", "", "/missing/private-description-path"])
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .output()
            .expect("missing metadata file fixture");
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert_eq!(output.stderr, b"Cannot read captured description for %m\n");
    }

    /// Shell arguments contain a private file path rather than provider text or added shell source.
    #[test]
    fn metadata_uses_only_the_fourth_inner_argument_and_stays_out_of_debug() {
        let plan = metadata_plan("printf '%s' %m", "PRIVATE PROVIDER DESCRIPTION");
        let terminal = plan.command().unwrap();
        let terminal_arguments = terminal.get_args().collect::<Vec<_>>();
        let path = plan.metadata.as_ref().unwrap().path().unwrap();
        assert_eq!(
            terminal_arguments[terminal_arguments.len() - 5],
            plan.argument.as_os_str()
        );
        assert_eq!(
            terminal_arguments[terminal_arguments.len() - 2],
            path.as_os_str()
        );
        assert!(terminal_arguments[4].to_str().unwrap().contains("\"$5\""));
        let desktop = plan.noninteractive_command().unwrap();
        let desktop_arguments = desktop.get_args().collect::<Vec<_>>();
        assert_eq!(
            desktop_arguments[desktop_arguments.len() - 5],
            plan.argument.as_os_str()
        );
        assert_eq!(
            desktop_arguments[desktop_arguments.len() - 2],
            path.as_os_str()
        );
        for command in [terminal, desktop] {
            assert!(command.get_args().all(|argument| {
                !argument
                    .to_string_lossy()
                    .contains("PRIVATE PROVIDER DESCRIPTION")
            }));
        }
        assert_eq!(format!("{plan:?}"), "ShellCommandPlan { .. }");
        assert!(!format!("{:?}", plan.metadata).contains("PRIVATE PROVIDER DESCRIPTION"));
    }

    /// Image macros use a separate positional argument and retain literal suffixes.
    #[test]
    fn image_markers_expand_independently_from_media_target_markers() {
        let expansion = expand_targets("printf '%s' % %d %i %i.png").unwrap();
        assert!(expansion.requires_selection);
        assert!(expansion.requires_download);
        assert!(expansion.requires_image);
        assert_eq!(
            expansion.source,
            concat!(
                "readonly __youta_selected_path=\"$1\"; ",
                "readonly __youta_downloaded_path=\"$2\"; ",
                "readonly __youta_image_path=\"$3\"; ",
                "printf '%s' \"${__youta_selected_path}\" \"${__youta_downloaded_path}\" ",
                "\"${__youta_image_path}\" \"${__youta_image_path}\".png"
            )
        );
    }

    /// Only unquoted macros at argument starts require a captured media selection.
    #[test]
    fn selection_is_required_only_for_unquoted_argument_start_macros() {
        for (template, selection, download, image) in [
            ("pwd", false, false, false),
            ("printf '%s' '%'", false, false, false),
            ("printf '%s' \"%\"", false, false, false),
            ("printf '%s' $'%'", false, false, false),
            ("printf '%s' \\%", false, false, false),
            ("printf '%s' 100%", false, false, false),
            ("printf '%d' 7", false, false, false),
            ("printf '%s' \"%d\"", false, false, false),
            ("printf '%s' $'%d'", false, false, false),
            ("printf '%s' \\%d", false, false, false),
            ("printf '%s' embedded%d", false, false, false),
            ("printf '%i' 7", false, false, false),
            ("printf '%s' \"%i\"", false, false, false),
            ("printf '%s' $'%i'", false, false, false),
            ("printf '%s' \\%i", false, false, false),
            ("printf '%s' embedded%i", false, false, false),
            (
                "printf '%s' plain # % %d %i are comments",
                false,
                false,
                false,
            ),
            ("printf '%s' $((5%2))", false, false, false),
            ("printf '%s' ${name%pattern}", false, false, false),
            ("printf '%s' %", true, false, false),
            ("printf '%s' %.flac", true, false, false),
            ("printf '%s' %d", true, true, false),
            ("printf '%s' %d.flac", true, true, false),
            ("printf '%s' % %d", true, true, false),
            ("printf '%s' %i", true, false, true),
            ("printf '%s' %i.png", true, false, true),
            ("printf '%s' % %d %i", true, true, true),
        ] {
            let plan = ShellCommandPlan {
                template: template.to_owned(),
                argument: OsString::new(),
                downloaded_path: None,
                image_path: None,
                metadata: None,
                playback_position: None,
                directory: std::env::temp_dir(),
            };
            assert_eq!(plan.requires_selection().unwrap(), selection, "{template}");
            assert_eq!(plan.requires_download().unwrap(), download, "{template}");
            assert_eq!(plan.requires_image().unwrap(), image, "{template}");
        }
    }

    /// Selection detection and command construction reject the same unsafe macro grammar.
    #[test]
    fn selection_requirements_preserve_macro_grammar_validation() {
        for template in [
            "printf '%s' $(echo %) %",
            "printf '%s' $((5%2)) %d",
            "(printf '%s' %)",
            "cat <<EOF %d",
            "printf '%s' $(echo %i) %i",
            "printf '%s' $((5%2)) %i",
            "(printf '%s' %i)",
            "cat <<EOF %i",
        ] {
            let plan = ShellCommandPlan {
                template: template.to_owned(),
                argument: OsString::new(),
                downloaded_path: None,
                image_path: None,
                metadata: None,
                playback_position: None,
                directory: std::env::temp_dir(),
            };
            let error = plan.requires_selection().unwrap_err();
            assert_eq!(plan.requires_download().unwrap_err(), error);
            assert_eq!(plan.requires_image().unwrap_err(), error);
            assert_eq!(plan.command().unwrap_err(), error);
            assert_eq!(plan.noninteractive_command().unwrap_err(), error);
        }
    }

    #[test]
    fn markers_preserve_shell_formats_quotes_escapes_and_multiple_arguments() {
        for (template, expected) in [
            (
                "ffmpeg -i % %.mp3",
                "ffmpeg -i \"${__youta_selected_path}\" \"${__youta_selected_path}\".mp3",
            ),
            (
                "printf '%s\\n' %",
                "printf '%s\\n' \"${__youta_selected_path}\"",
            ),
            (
                "printf \"%s\\n\" %",
                "printf \"%s\\n\" \"${__youta_selected_path}\"",
            ),
            (
                "printf $'it\\'s %s' %",
                "printf $'it\\'s %s' \"${__youta_selected_path}\"",
            ),
        ] {
            assert_eq!(
                expand_selected_path(template).unwrap(),
                format!("readonly __youta_selected_path=\"$1\"; {expected}"),
                "{template}"
            );
        }
    }

    /// Ordinary Bash percent syntax is not a filename macro inside another word.
    #[test]
    fn literal_formats_escapes_and_complex_commands_remain_unchanged() {
        for template in [
            "printf '\\%' \\%",
            "printf '%s' \"$1\"",
            "printf '%s' 100%",
            "printf '%s' \"$(printf '%s' \"$1\")\"",
            "printf '%s' \"$(echo \"%\")\"",
            "printf '%s' ${name%pattern}",
            "printf '%s' $((5%2))",
            "tmp=1; echo $((1000%))",
            "printf '%s' \"escaped \\\" %s\"",
            "printf '%s' plain # % is a comment",
        ] {
            assert_eq!(expand_selected_path(template).unwrap(), template);
        }
    }

    /// Download detection ignores printf formats, quotes, escapes, comments, and word suffixes.
    #[test]
    fn literal_download_markers_do_not_request_a_download() {
        for template in [
            "printf '%d' 7",
            "printf \"%d\" 7",
            "printf '%s' $'%d'",
            "printf '%s' \\%d",
            "printf '%s' embedded%d",
            "printf '%s' plain # %d is a comment",
        ] {
            let plan = ShellCommandPlan {
                template: template.to_owned(),
                argument: "https://example.test/original".into(),
                downloaded_path: None,
                image_path: None,
                metadata: None,
                playback_position: None,
                directory: std::env::temp_dir(),
            };
            assert_eq!(expand_targets(template).unwrap().source, template);
            assert!(!plan.requires_download().unwrap(), "{template}");
            assert!(plan.command().is_ok(), "{template}");
            assert!(plan.noninteractive_command().is_ok(), "{template}");
        }
    }

    /// Image detection preserves formats, quoting, escaping, comments, and embedded markers.
    #[test]
    fn literal_image_markers_do_not_request_an_image() {
        for template in [
            "printf '%i' 7",
            "printf \"%i\" 7",
            "printf '%s' $'%i'",
            "printf '%s' \\%i",
            "printf '%s' embedded%i",
            "printf '%s' plain # %i is a comment",
        ] {
            let plan = ShellCommandPlan {
                template: template.to_owned(),
                argument: OsString::new(),
                downloaded_path: None,
                image_path: None,
                metadata: None,
                playback_position: None,
                directory: std::env::temp_dir(),
            };
            assert_eq!(expand_targets(template).unwrap().source, template);
            assert!(!plan.requires_image().unwrap(), "{template}");
            assert!(plan.command().is_ok(), "{template}");
            assert!(plan.noninteractive_command().is_ok(), "{template}");
        }
    }

    /// Direct callers cannot run an image macro until its captured artwork is prepared.
    #[test]
    fn unresolved_image_marker_rejects_both_command_builders() {
        for template in ["printf '%s' %i", "printf '%s' % %i.png"] {
            let mut plan = ShellCommandPlan {
                template: template.to_owned(),
                argument: "https://example.test/private-original".into(),
                downloaded_path: None,
                image_path: None,
                metadata: None,
                playback_position: None,
                directory: std::env::temp_dir(),
            };
            assert!(plan.requires_selection().unwrap());
            assert!(plan.requires_image().unwrap());
            assert!(!plan.requires_download().unwrap());
            for error in [
                plan.command().unwrap_err(),
                plan.noninteractive_command().unwrap_err(),
            ] {
                assert!(error.contains("prepared image for %i"));
                assert!(!error.contains("private-original"));
            }
            plan.image_path = Some(std::env::temp_dir().join("captured artwork.png"));
            assert!(plan.command().is_ok());
            assert!(plan.noninteractive_command().is_ok());
        }
    }

    /// Image bytes remain opaque through both wrappers, including after `set --`.
    #[cfg(unix)]
    #[test]
    fn image_markers_preserve_hostile_bytes_suffixes_and_argument_boundaries() {
        use std::os::unix::ffi::OsStringExt;
        for image in [
            b"/tmp/with spaces/artwork.png".to_vec(),
            b"/tmp/'\";$(printf INJECTED >&2)`printf INJECTED >&2`\\\n%*.png".to_vec(),
            b"/tmp/non-utf8-\xff\xfe artwork.png".to_vec(),
        ] {
            for prefix in ["", "set -- replacement; "] {
                let plan = ShellCommandPlan {
                    template: format!("{prefix}printf '%s\\0' % %d %i %i.png"),
                    argument: "https://example.test/original path".into(),
                    downloaded_path: Some(PathBuf::from("/tmp/downloaded media.flac")),
                    image_path: Some(PathBuf::from(OsString::from_vec(image.clone()))),
                    metadata: None,
                    playback_position: None,
                    directory: std::env::temp_dir(),
                };
                let terminal = plan.command().unwrap();
                let arguments = terminal.get_args().collect::<Vec<_>>();
                assert_eq!(
                    arguments.get(arguments.len() - 3),
                    Some(&plan.image_path.as_ref().unwrap().as_os_str())
                );
                assert!(arguments[4].to_str().unwrap().contains("\"$4\""));
                // Omit interactive job control for the fixture while retaining the outer argv mapping.
                let mut outer = Command::new(terminal.get_program());
                outer
                    .args(arguments.into_iter().filter(|argument| *argument != "-i"))
                    .env_remove("BASH_ENV")
                    .env_remove("ENV");
                for mut command in [outer, plan.noninteractive_command().unwrap()] {
                    let output = command.output().expect("Bash fixture");
                    assert!(output.status.success());
                    assert!(output.stderr.is_empty());
                    assert_eq!(
                        output.stdout,
                        [
                            b"https://example.test/original path\0/tmp/downloaded media.flac\0"
                                .to_vec(),
                            image.clone(),
                            vec![0],
                            image.clone(),
                            b".png\0".to_vec(),
                        ]
                        .concat()
                    );
                }
            }
        }
    }

    /// Original and downloaded targets receive distinct readonly variables and safe suffixes.
    #[test]
    fn download_markers_expand_independently_from_original_target_markers() {
        let expansion = expand_targets("printf '%s' % %d %.flac %d.flac").unwrap();
        assert!(expansion.requires_download);
        assert_eq!(
            expansion.source,
            concat!(
                "readonly __youta_selected_path=\"$1\"; ",
                "readonly __youta_downloaded_path=\"$2\"; ",
                "printf '%s' \"${__youta_selected_path}\" \"${__youta_downloaded_path}\" ",
                "\"${__youta_selected_path}\".flac \"${__youta_downloaded_path}\".flac"
            )
        );
    }

    /// Neither frontend can construct an executable plan before a required download resolves.
    #[test]
    fn unresolved_download_marker_rejects_both_command_builders() {
        for template in ["printf '%s' %d", "printf '%s' % %d.flac"] {
            let mut plan = ShellCommandPlan {
                template: template.to_owned(),
                argument: "https://example.test/private-original".into(),
                downloaded_path: None,
                image_path: None,
                metadata: None,
                playback_position: None,
                directory: std::env::temp_dir(),
            };
            assert!(plan.requires_download().unwrap(), "{template}");
            for error in [
                plan.command().unwrap_err(),
                plan.noninteractive_command().unwrap_err(),
            ] {
                assert!(error.contains("completed download for %d"));
                assert!(!error.contains("private-original"));
            }
            plan.downloaded_path = Some(std::env::temp_dir().join("downloaded fixture.flac"));
            assert!(plan.command().is_ok());
            assert!(plan.noninteractive_command().is_ok());
        }
    }

    /// Remote arguments and downloaded filenames remain opaque bytes, including hostile syntax.
    #[cfg(unix)]
    #[test]
    fn both_target_macros_preserve_hostile_bytes_and_suffixes_without_execution() {
        use std::os::unix::ffi::OsStringExt;
        for (original, downloaded) in [
			(
				b"https://example.test/a path?x='\";$(printf INJECTED >&2)`printf INJECTED >&2`&y=*".to_vec(),
				b"/tmp/'\";$(printf INJECTED >&2)`printf INJECTED >&2`\\\n%*.mp3".to_vec(),
			),
			(
				b"https://example.test/non-utf8-\xff\xfe?x=original".to_vec(),
				b"/tmp/non-utf8-\xfe\xff downloaded.mp3".to_vec(),
			),
		] {
			for prefix in ["", "set -- replacement; "] {
				let plan = ShellCommandPlan {
					template: format!("{prefix}printf '%s\\0' % %d %.flac %d.flac"),
					argument: OsString::from_vec(original.clone()),
					downloaded_path: Some(PathBuf::from(OsString::from_vec(downloaded.clone()))),
					image_path: None,
					metadata: None,
					playback_position: None,
					directory: std::env::temp_dir(),
				};
				assert!(plan.requires_download().unwrap());
				let terminal = plan.command().unwrap();
				let arguments = terminal.get_args().collect::<Vec<_>>();
				assert_eq!(arguments[arguments.len() - 5], plan.argument.as_os_str());
				assert_eq!(
					arguments[arguments.len() - 4],
					plan.downloaded_path.as_ref().unwrap().as_os_str()
				);
				let output = plan.noninteractive_command().unwrap().output().expect("Bash fixture");
				assert!(output.status.success());
				assert!(output.stderr.is_empty());
				assert_eq!(
					output.stdout,
					[
						original.clone(), vec![0], downloaded.clone(), vec![0],
						original.clone(), b".flac\0".to_vec(),
						downloaded.clone(), b".flac\0".to_vec(),
					].concat()
				);
			}
		}
    }

    /// The existing Local prompt supplies the same path to original and downloaded macros.
    #[cfg(unix)]
    #[test]
    fn local_plan_supplies_the_same_byte_exact_path_to_both_macros() {
        use std::os::unix::ffi::OsStringExt;
        let bytes = b"/tmp/local \xff'\";$(printf INJECTED >&2).flac".to_vec();
        let path = PathBuf::from(OsString::from_vec(bytes.clone()));
        let directory = std::env::temp_dir();
        let plan = LocalCommandPlan {
            template: "printf '%s\\0' % %d".to_owned(),
            path: path.clone(),
            directory: directory.clone(),
        }
        .into_shell_plan();
        assert_eq!(plan.argument.as_os_str(), path.as_os_str());
        assert_eq!(plan.downloaded_path.as_ref(), Some(&path));
        assert!(plan.image_path.is_none());
        assert_eq!(plan.directory, directory);
        assert!(plan.requires_download().unwrap());
        let output = plan
            .noninteractive_command()
            .unwrap()
            .output()
            .expect("Bash fixture");
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        assert_eq!(
            output.stdout,
            [bytes.clone(), vec![0], bytes, vec![0]].concat()
        );
    }

    /// Unsupported grammar never receives a partially quoted path expansion.
    #[test]
    fn percent_macros_reject_nested_shell_grammar() {
        for template in [
            "echo $(( % ))",
            "tmp=1; echo $((1000%)); echo %",
            "echo ${name%pattern}; echo %",
            "printf '%s' \"$(echo \"%\")\" %",
            "printf '%s' $(echo %)",
            "printf '%s' `echo %`",
            "(printf '%s' %)",
            "show() { printf '%s' %; }; show",
            "printf '%s' \"escaped \\\" $(echo %)\" %",
            "cat <<EOF %",
        ] {
            let error = expand_selected_path(template).unwrap_err();
            assert!(error.contains("quoted \"$1\""), "{template}: {error}");
        }
    }

    /// Shell argument expansion must not turn an attacker-named file into code.
    #[cfg(unix)]
    #[test]
    fn selected_path_preserves_arbitrary_bytes_without_evaluating_them() {
        use std::os::unix::ffi::OsStringExt;
        for path in [
            b"/tmp/with spaces/a song.mp3".to_vec(),
            b"/tmp/'\";$(printf INJECTED)`printf INJECTED`\\\n%*.mp3".to_vec(),
            b"/tmp/non-utf8-\xff\xfe.mp3".to_vec(),
        ] {
            let output = bash_command()
                .args(["--noprofile", "--norc", "-c"])
                .arg(expand_selected_path("printf '%s\\0' %").unwrap())
                .arg("youta")
                .arg(std::ffi::OsString::from_vec(path.clone()))
                .env_remove("BASH_ENV")
                .output()
                .expect("Bash fixture");
            assert!(output.status.success());
            assert_eq!(output.stdout, [path, vec![0]].concat());
            assert!(output.stderr.is_empty());
        }
    }

    /// Macro identity survives positional-parameter changes, without filename code.
    #[cfg(unix)]
    #[test]
    fn selected_path_survives_set_and_advanced_bash_keeps_argument_boundaries() {
        let path = "/tmp/x[$(printf INJECTED >&2)] with spaces";
        for template in [
            "set -- replacement; printf '%s\\0' %",
            "printf '%s\\0' \"$(printf '%s' \"$1\")\"",
            "value=\"$1\"; printf '%s\\0' \"${value%nomatch}\"",
            "printf '%s\\0' $((5%2)) \"$1\"",
            "printf '%s\\0' \"escaped \\\" %s\" %",
            "printf '%s\\0' \"$(printf '%s' \"%\")\"",
        ] {
            let output = bash_command()
                .args(["--noprofile", "--norc", "-c"])
                .arg(expand_selected_path(template).unwrap())
                .args(["youta", path])
                .env_remove("BASH_ENV")
                .output()
                .expect("Bash fixture");
            assert!(output.status.success(), "{template}: {output:?}");
            let expected = if template.contains("$((5%2))") {
                format!("1\0{path}\0")
            } else if template.contains("escaped") {
                format!("escaped \" %s\0{path}\0")
            } else if template.contains("\"%\"") {
                "%\0".to_owned()
            } else {
                format!("{path}\0")
            };
            assert_eq!(output.stdout, expected.as_bytes(), "{template}");
            assert!(output.stderr.is_empty(), "{template}: {output:?}");
        }
    }

    /// Arithmetic percent is not a route for recursively evaluating filename code.
    #[cfg(unix)]
    #[test]
    fn arithmetic_never_receives_the_selected_path_implicitly() {
        let template = "tmp=1; printf '%s' $((1000%))";
        let script = expand_selected_path(template).unwrap();
        assert_eq!(script, template);
        let output = bash_command()
            .args(["--noprofile", "--norc", "-c", &script, "youta"])
            .arg("/tmp/x[$(printf INJECTED >&2)]")
            .env_remove("BASH_ENV")
            .output()
            .expect("Bash arithmetic fixture");
        assert!(
            !output.status.success(),
            "an incomplete modulo remains invalid"
        );
        assert!(!String::from_utf8_lossy(&output.stderr).contains("INJECTED"));
        assert!(output.stdout.is_empty());
    }

    #[test]
    fn plans_keep_private_templates_and_paths_out_of_debug_output() {
        let plan = LocalCommandPlan {
            template: "echo secret-token".to_owned(),
            path: PathBuf::from("/private/file"),
            directory: PathBuf::from("/private"),
        };
        assert_eq!(format!("{plan:?}"), "LocalCommandPlan { .. }");
        let mut shell_plan = plan.clone().into_shell_plan();
        shell_plan.image_path = Some(PathBuf::from("/private/artwork.png"));
        assert_eq!(format!("{shell_plan:?}"), "ShellCommandPlan { .. }");
        let command = plan.command().unwrap();
        assert_eq!(command.get_program(), bash_command().get_program());
        assert_eq!(command.get_current_dir(), Some(plan.directory.as_path()));
        let arguments = command.get_args().collect::<Vec<_>>();
        assert_eq!(arguments[arguments.len() - 5], plan.path.as_os_str());
        assert_eq!(arguments[arguments.len() - 4], plan.path.as_os_str());
        assert_eq!(arguments.last(), Some(&std::ffi::OsStr::new("")));
    }
}
