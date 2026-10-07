//! Explicit, foreground Bash commands for a selected Local entry.
//!
//! Only the user-authored template is shell source. The selected path travels
//! as an OS-string positional argument, never as interpolated shell syntax.

use std::fmt;
use std::path::PathBuf;
use std::process::Command;

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
        let expanded = expand_selected_path(&self.template)?;
        let mut command = Command::new("bash");
        command
            .args([
                "--noprofile",
                "--norc",
                "-i",
                "-c",
                "\"$BASH\" --noprofile --norc -c \"$1\" youta \"$2\"",
                "youta",
            ])
            .arg(expanded)
            .arg(&self.path)
            .current_dir(&self.directory)
            .env("HISTFILE", "/dev/null")
            .env_remove("BASH_ENV")
            .env_remove("ENV");
        Ok(command)
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
                result.push_str("\"${__youta_selected_path}\"");
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
        return Ok(template.to_owned());
    }
    if complex {
        return Err(
            "The % path shortcut supports simple unquoted arguments only; for Bash substitutions, arithmetic, grouping, or backticks, use the selected path as quoted \"$1\" before changing positional parameters"
                .to_owned(),
        );
    }
    Ok(format!("readonly __youta_selected_path=\"$1\"; {result}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
