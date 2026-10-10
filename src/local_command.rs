//! Explicit Bash commands with an optional selected local path or provider URL.
//!
//! Only the user-authored template is shell source. A selected target travels
//! as an OS-string positional argument, never as interpolated shell syntax.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::process::Command;

pub mod buttons;
mod capture;

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
    ///
    /// # Errors
    /// Returns a validation error for unsupported percent-macro shell grammar.
    pub fn command(&self) -> Result<Command, String> {
        let expanded = self.expanded_source()?;
        let mut command = Command::new("bash");
        command
            .args([
                "--noprofile",
                "--norc",
                "-i",
                "-c",
                "\"$BASH\" --noprofile --norc -c \"$1\" youta \"$2\" \"$3\"; exit \"$?\"",
                "youta",
            ])
            .arg(expanded)
            .arg(&self.argument)
            .arg(
                self.downloaded_path
                    .as_deref()
                    .unwrap_or(std::path::Path::new("")),
            )
            .current_dir(&self.directory)
            .env("HISTFILE", "/dev/null")
            .env_remove("BASH_ENV")
            .env_remove("ENV");
        Ok(command)
    }

    /// Builds a noninteractive shell for a desktop worker, with the same safe argument.
    ///
    /// # Errors
    /// Returns a validation error for unsupported percent-macro shell grammar.
    pub fn noninteractive_command(&self) -> Result<Command, String> {
        let mut command = Command::new("bash");
        command
            .args(["--noprofile", "--norc", "-c"])
            .arg(self.expanded_source()?)
            .arg("youta")
            .arg(&self.argument)
            .arg(
                self.downloaded_path
                    .as_deref()
                    .unwrap_or(std::path::Path::new("")),
            )
            .current_dir(&self.directory)
            .env("HISTFILE", "/dev/null")
            .env_remove("BASH_ENV")
            .env_remove("ENV");
        Ok(command)
    }

    /// Detects whether an unquoted `%` or `%d` macro requires a selected target.
    ///
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

    /// Refuses to execute an unresolved download macro, even through direct API calls.
    fn expanded_source(&self) -> Result<String, String> {
        let expansion = expand_targets(&self.template)?;
        if expansion.requires_download && self.downloaded_path.is_none() {
            return Err("The command requires a completed download for %d".to_owned());
        }
        Ok(expansion.source)
    }
}

/// Expands percent markers that start simple, unquoted shell arguments.
///
/// `printf '%s\n' %` preserves its format and passes the selected path as one
/// argument; `%.mp3` appends a suffix to that same argument. Quoted, escaped,
/// and embedded percent characters remain literal. Complex Bash grammar is
/// deliberately not parsed: combining it with a detected macro is rejected,
/// and callers can instead use the original selected path in quoted `"$1"`.
/// A private readonly variable preserves macros after the user runs `set --`.
fn expand_selected_path(template: &str) -> Result<String, String> {
    expand_targets(template).map(|expansion| expansion.source)
}

/// Validated shell source and the selected-target requirements discovered by its lexer.
struct TargetExpansion {
    /// Shell source with recognized target macros replaced by readonly variables.
    source: String,
    /// At least one original or downloaded target macro was recognized.
    requires_selection: bool,
    /// A downloaded target macro needs a completed local file before execution.
    requires_download: bool,
}

/// Lexes the two supported argument-start macros while preserving literal percent signs.
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
    let mut download = false;
    // Conservatively reject nested grammar whenever a macro was recognized.
    // Looking for these spellings even in quoted text intentionally favors a
    // clear limitation over pretending this small scanner is a Bash parser.
    let mut complex = ["$(", "${", "((", "[[", "`", "<<"]
        .iter()
        .any(|syntax| template.contains(syntax));
    let mut characters = template.chars().peekable();
    while let Some(character) = characters.next() {
        match (quote, character) {
            (Quote::None, '%') if word_start => {
                if characters.peek() == Some(&'d') {
                    characters.next();
                    result.push_str("\"${__youta_downloaded_path}\"");
                    download = true;
                } else {
                    result.push_str("\"${__youta_selected_path}\"");
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
    }
    if !expanded {
        return Ok(TargetExpansion {
            source: template.to_owned(),
            requires_selection: false,
            requires_download: false,
        });
    }
    if complex {
        return Err(
            "The % path shortcut supports simple unquoted arguments only; for Bash substitutions, arithmetic, grouping, or backticks, use the selected path as quoted \"$1\" before changing positional parameters"
                .to_owned(),
        );
    }
    let download_prefix = if download {
        "readonly __youta_downloaded_path=\"$2\"; "
    } else {
        ""
    };
    Ok(TargetExpansion {
        source: format!("readonly __youta_selected_path=\"$1\"; {download_prefix}{result}"),
        requires_selection: expanded,
        requires_download: download,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only unquoted macros at argument starts require a captured media selection.
    #[test]
    fn selection_is_required_only_for_unquoted_argument_start_macros() {
        for (template, selection, download) in [
            ("pwd", false, false),
            ("printf '%s' '%'", false, false),
            ("printf '%s' \"%\"", false, false),
            ("printf '%s' $'%'", false, false),
            ("printf '%s' \\%", false, false),
            ("printf '%s' 100%", false, false),
            ("printf '%d' 7", false, false),
            ("printf '%s' \"%d\"", false, false),
            ("printf '%s' $'%d'", false, false),
            ("printf '%s' \\%d", false, false),
            ("printf '%s' embedded%d", false, false),
            ("printf '%s' plain # % %d are comments", false, false),
            ("printf '%s' $((5%2))", false, false),
            ("printf '%s' ${name%pattern}", false, false),
            ("printf '%s' %", true, false),
            ("printf '%s' %.flac", true, false),
            ("printf '%s' %d", true, true),
            ("printf '%s' %d.flac", true, true),
            ("printf '%s' % %d", true, true),
        ] {
            let plan = ShellCommandPlan {
                template: template.to_owned(),
                argument: OsString::new(),
                downloaded_path: None,
                directory: std::env::temp_dir(),
            };
            assert_eq!(plan.requires_selection().unwrap(), selection, "{template}");
            assert_eq!(plan.requires_download().unwrap(), download, "{template}");
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
        ] {
            let plan = ShellCommandPlan {
                template: template.to_owned(),
                argument: OsString::new(),
                downloaded_path: None,
                directory: std::env::temp_dir(),
            };
            let error = plan.requires_selection().unwrap_err();
            assert_eq!(plan.requires_download().unwrap_err(), error);
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
                directory: std::env::temp_dir(),
            };
            assert_eq!(expand_targets(template).unwrap().source, template);
            assert!(!plan.requires_download().unwrap(), "{template}");
            assert!(plan.command().is_ok(), "{template}");
            assert!(plan.noninteractive_command().is_ok(), "{template}");
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
					directory: std::env::temp_dir(),
				};
				assert!(plan.requires_download().unwrap());
				let terminal = plan.command().unwrap();
				let arguments = terminal.get_args().collect::<Vec<_>>();
				assert_eq!(arguments[arguments.len() - 2], plan.argument.as_os_str());
				assert_eq!(
					arguments[arguments.len() - 1],
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
            let output = Command::new("bash")
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
            let output = Command::new("bash")
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
        let output = Command::new("bash")
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
        let command = plan.command().unwrap();
        assert_eq!(command.get_program(), "bash");
        assert_eq!(command.get_current_dir(), Some(plan.directory.as_path()));
        assert_eq!(command.get_args().last(), Some(plan.path.as_os_str()));
    }
}
