//! Bounded literal-word completion without evaluating shell code or loading scripts.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ops::Range;
use std::path::Path;

const MAX_PATH_DIRECTORIES: usize = 64;
const MAX_SCANNED_ENTRIES: usize = 8_192;
const MAX_CANDIDATES: usize = 256;
const MAX_CANDIDATE_BYTES: usize = 256 * 1_024;

/// Bash's documented builtin names; discovering them must not start a shell.
const BUILTINS: &[&str] = &[
    "alias",
    "bg",
    "bind",
    "break",
    "builtin",
    "caller",
    "cd",
    "command",
    "compgen",
    "complete",
    "compopt",
    "continue",
    "declare",
    "dirs",
    "disown",
    "echo",
    "enable",
    "eval",
    "exec",
    "exit",
    "export",
    "false",
    "fc",
    "fg",
    "getopts",
    "hash",
    "help",
    "history",
    "jobs",
    "kill",
    "let",
    "local",
    "logout",
    "mapfile",
    "popd",
    "printf",
    "pushd",
    "pwd",
    "read",
    "readarray",
    "readonly",
    "return",
    "set",
    "shift",
    "shopt",
    "source",
    "suspend",
    "test",
    "times",
    "trap",
    "true",
    "type",
    "typeset",
    "ulimit",
    "umask",
    "unalias",
    "unset",
    "wait",
];

/// Injectable filesystem/environment inputs keep tests independent of process globals.
pub(super) struct Context<'a> {
    pub(super) directory: &'a Path,
    pub(super) search_path: Option<&'a OsStr>,
    pub(super) home: Option<&'a Path>,
}

/// A repeated-Tab cycle containing private command text, never Debug or serialized.
#[derive(Default)]
pub(super) struct State {
    cycle: Option<Cycle>,
}

struct Cycle {
    rendered: String,
    cursor: usize,
    replacement: Range<usize>,
    candidates: Vec<String>,
    next: usize,
}

/// One decoded literal word, including the suffix after an insertion point in its middle.
struct Target {
    replacement: Range<usize>,
    prefix: String,
    suffix: String,
    command_position: bool,
    expand_tilde: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

impl State {
    /// Cancels cycling whenever text, cursor position, or another editor changes.
    pub(super) fn reset(&mut self) {
        self.cycle = None;
    }

    /// Extends a common prefix, then cycles literal candidates on repeated Tab presses.
    pub(super) fn complete(
        &mut self,
        command: &mut String,
        cursor: &mut usize,
        context: &Context<'_>,
    ) {
        if let Some(cycle) = self.cycle.as_mut()
            && cycle.rendered == *command
            && cycle.cursor == *cursor
        {
            let value = &cycle.candidates[cycle.next];
            if let Some(replacement) = replace_word(
                command,
                cursor,
                cycle.replacement.clone(),
                value,
                value.len(),
            ) {
                cycle.replacement = replacement;
                cycle.rendered.clone_from(command);
                cycle.cursor = *cursor;
                cycle.next = (cycle.next + 1) % cycle.candidates.len();
            }
            return;
        }
        self.reset();
        let Some(target) = literal_target(command, *cursor) else {
            return;
        };
        let candidates = candidates(&target, context);
        if candidates.is_empty() {
            return;
        }
        let common = common_prefix(&candidates);
        let (value, cursor_in_value, next) = if candidates.len() == 1 {
            (candidates[0].clone(), candidates[0].len(), 0)
        } else if common.len() > target.prefix.len() && target.suffix.is_empty() {
            (common.to_owned(), common.len(), 0)
        } else if !target.suffix.is_empty()
            && common.len() > target.prefix.len()
            && !common.ends_with(&target.suffix)
        {
            (format!("{common}{}", target.suffix), common.len(), 0)
        } else {
            // With no longer common prefix, the first Tab starts the deterministic cycle.
            (
                candidates[0].clone(),
                candidates[0].len(),
                1 % candidates.len(),
            )
        };
        let Some(replacement) =
            replace_word(command, cursor, target.replacement, &value, cursor_in_value)
        else {
            return;
        };
        if candidates.len() == 1 {
            return;
        }
        self.cycle = Some(Cycle {
            rendered: command.clone(),
            cursor: *cursor,
            replacement,
            candidates,
            next,
        });
    }
}

/// Replaces exactly one shell word and tracks the corresponding encoded cursor offset.
fn replace_word(
    command: &mut String,
    cursor: &mut usize,
    range: Range<usize>,
    value: &str,
    cursor_in_value: usize,
) -> Option<Range<usize>> {
    let (encoded, encoded_cursor) = escape_literal(value, cursor_in_value);
    let new_length = command
        .len()
        .saturating_sub(range.len())
        .saturating_add(encoded.len());
    if new_length > super::COMMAND_LIMIT {
        return None;
    }
    command.replace_range(range.clone(), &encoded);
    *cursor = range.start.saturating_add(encoded_cursor);
    Some(range.start..range.start.saturating_add(encoded.len()))
}

/// Quotes metacharacters individually so filenames cannot introduce shell syntax or `%` macros.
fn escape_literal(value: &str, cursor_in_value: usize) -> (String, usize) {
    let mut encoded = String::new();
    let mut cursor = 0;
    for (index, character) in value.char_indices() {
        if index == cursor_in_value {
            cursor = encoded.len();
        }
        if character.is_whitespace()
            || matches!(
                character,
                '\\' | '\''
                    | '"'
                    | '$'
                    | '`'
                    | '!'
                    | '%'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '['
                    | ']'
                    | '*'
                    | '?'
                    | ';'
                    | '<'
                    | '>'
                    | '|'
                    | '&'
                    | '~'
                    | '#'
                    | '='
            )
        {
            encoded.push('\\');
        }
        encoded.push(character);
    }
    if cursor_in_value == value.len() {
        cursor = encoded.len();
    }
    (encoded, cursor)
}

/// Parses only literal words; expansions and structural shell syntax are never evaluated.
fn literal_target(command: &str, cursor: usize) -> Option<Target> {
    if cursor > command.len() || !command.is_char_boundary(cursor) {
        return None;
    }
    let mut quote = Quote::None;
    let mut escaped = false;
    let mut start = None;
    let mut value = String::new();
    let mut decoded_cursor = None;
    let mut command_position = true;
    let mut target_command_position = true;
    let mut unsupported = false;
    let mut expand_tilde = false;
    for (index, character) in command.char_indices() {
        if index == cursor {
            decoded_cursor = Some(value.len());
        }
        if !escaped
            && quote == Quote::None
            && (character.is_ascii_whitespace() || matches!(character, ';' | '|' | '&' | '<' | '>'))
        {
            if let Some(start) = start {
                if cursor >= start && cursor <= index {
                    return (!unsupported).then(|| Target {
                        replacement: start..index,
                        prefix: value[..decoded_cursor.unwrap_or(value.len())].to_owned(),
                        suffix: value[decoded_cursor.unwrap_or(value.len())..].to_owned(),
                        command_position: target_command_position,
                        expand_tilde,
                    });
                }
                command_position = false;
            }
            if matches!(character, ';' | '|' | '&') {
                command_position = true;
            }
            if matches!(character, '<' | '>') {
                command_position = false;
            }
            start = None;
            value.clear();
            decoded_cursor = None;
            unsupported = false;
            expand_tilde = false;
            continue;
        }
        if start.is_none() {
            start = Some(index);
            target_command_position = command_position;
            if index == cursor {
                decoded_cursor = Some(0);
            }
        }
        if escaped {
            // Inside double quotes Bash only consumes escapes before these special characters.
            if quote == Quote::Double && !matches!(character, '$' | '`' | '"' | '\\') {
                value.push('\\');
            }
            value.push(character);
            escaped = false;
            continue;
        }
        match (quote, character) {
            (Quote::None | Quote::Double, '\\') => escaped = true,
            (Quote::None, '\'') => quote = Quote::Single,
            (Quote::None, '"') => quote = Quote::Double,
            (Quote::Single, '\'') | (Quote::Double, '"') => quote = Quote::None,
            (Quote::None | Quote::Double, '$' | '`') => return None,
            (Quote::None, '(' | ')' | '{' | '}') => return None,
            (Quote::None, '#') if value.is_empty() => return None,
            (Quote::None, '~') if start == Some(index) => {
                expand_tilde = true;
                value.push(character);
            }
            (Quote::None, '*' | '?' | '[' | ']' | '%') => {
                unsupported = true;
                value.push(character);
            }
            _ => value.push(character),
        }
    }
    let start = start.unwrap_or(cursor);
    if cursor < start || escaped || unsupported {
        return None;
    }
    let decoded_cursor = decoded_cursor.unwrap_or(value.len());
    Some(Target {
        replacement: start..command.len(),
        prefix: value[..decoded_cursor].to_owned(),
        suffix: value[decoded_cursor..].to_owned(),
        command_position: if value.is_empty() {
            command_position
        } else {
            target_command_position
        },
        expand_tilde,
    })
}

/// Collects an alphabetically stable bounded set without spawning a process.
fn candidates(target: &Target, context: &Context<'_>) -> Vec<String> {
    let mut collector = Collector::default();
    if target.command_position && !target.prefix.contains('/') {
        for builtin in BUILTINS {
            collector.insert((*builtin).to_owned(), target);
        }
        if let Some(path) = context.search_path {
            for directory in std::env::split_paths(path).take(MAX_PATH_DIRECTORIES) {
                let directory = if directory.is_absolute() {
                    directory
                } else {
                    context.directory.join(directory)
                };
                collect_directory(&directory, "", target, true, true, &mut collector);
                if collector.full() {
                    break;
                }
            }
        }
    } else {
        let (prefix, name) = target
            .prefix
            .rsplit_once('/')
            .map_or(("", target.prefix.as_str()), |(directory, name)| {
                (&target.prefix[..directory.len() + 1], name)
            });
        let (directory, display_prefix) =
            if let Some(rest) = prefix.strip_prefix("~/").filter(|_| target.expand_tilde) {
                let Some(home) = context.home else {
                    return Vec::new();
                };
                let directory = home.join(rest);
                let Some(display) = directory.to_str() else {
                    return Vec::new();
                };
                (
                    directory.clone(),
                    format!("{}/", display.trim_end_matches('/')),
                )
            } else {
                let path = Path::new(prefix);
                (
                    if path.is_absolute() {
                        path.to_owned()
                    } else {
                        context.directory.join(path)
                    },
                    prefix.to_owned(),
                )
            };
        let file_target = Target {
            prefix: format!("{display_prefix}{name}"),
            suffix: target.suffix.clone(),
            replacement: target.replacement.clone(),
            command_position: target.command_position,
            expand_tilde: false,
        };
        collect_directory(
            &directory,
            &display_prefix,
            &file_target,
            target.command_position,
            false,
            &mut collector,
        );
    }
    collector.values.into_keys().collect()
}

/// Shared scan budget prevents long PATHs or giant directories from consuming unbounded work.
#[derive(Default)]
struct Collector {
    values: BTreeMap<String, ()>,
    scanned: usize,
    bytes: usize,
}

impl Collector {
    fn full(&self) -> bool {
        self.scanned >= MAX_SCANNED_ENTRIES
            || self.values.len() >= MAX_CANDIDATES
            || self.bytes >= MAX_CANDIDATE_BYTES
    }

    fn insert(&mut self, value: String, target: &Target) {
        if self.values.len() >= MAX_CANDIDATES
            || value.len() > super::COMMAND_LIMIT
            || self.bytes.saturating_add(value.len()) > MAX_CANDIDATE_BYTES
            || value.chars().any(char::is_control)
            || !value.starts_with(&target.prefix)
            || (!target.suffix.is_empty() && !value.ends_with(&target.suffix))
            || self.values.contains_key(&value)
        {
            return;
        }
        self.bytes += value.len();
        self.values.insert(value, ());
    }
}

/// Lists only one directory, with metadata checks limited to prefix-matching entries.
fn collect_directory(
    directory: &Path,
    display_prefix: &str,
    target: &Target,
    executables_only: bool,
    path_search: bool,
    collector: &mut Collector,
) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let basename_prefix = target.prefix.rsplit('/').next().unwrap_or_default();
    for entry in entries {
        if collector.full() {
            break;
        }
        collector.scanned += 1;
        let Ok(entry) = entry else {
            continue;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(basename_prefix)
            || (name.starts_with('.') && !basename_prefix.starts_with('.'))
        {
            continue;
        }
        let Ok(metadata) = entry.path().metadata() else {
            continue;
        };
        if metadata.is_dir() {
            if !path_search {
                collector.insert(format!("{display_prefix}{name}/"), target);
            }
        } else if metadata.is_file() && (!executables_only || executable(&metadata)) {
            collector.insert(format!("{display_prefix}{name}"), target);
        }
    }
}

/// Unix executable bits are authoritative; other platforms retain ordinary files in PATH.
fn executable(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

/// Computes a UTF-8-safe common prefix across sorted completion values.
fn common_prefix(values: &[String]) -> &str {
    let Some(first) = values.first() else {
        return "";
    };
    let Some(last) = values.last() else {
        return first;
    };
    let length = first
        .chars()
        .zip(last.chars())
        .take_while(|(left, right)| left == right)
        .map(|(character, _)| character.len_utf8())
        .sum::<usize>();
    &first[..length]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn context(directory: &Path) -> Context<'_> {
        Context {
            directory,
            search_path: None,
            home: None,
        }
    }

    fn complete(command: &str, directory: &Path) -> String {
        let mut command = command.to_owned();
        let mut cursor = command.len();
        State::default().complete(&mut command, &mut cursor, &context(directory));
        command
    }

    #[test]
    fn file_completion_escapes_spaces_quotes_percent_and_shell_syntax() {
        let fixture = crate::test_support::canonical_tempdir("command completion files");
        std::fs::write(fixture.path().join("track 'name';$(no)%!.flac"), b"").unwrap();
        let result = complete("cat tra", fixture.path());
        assert_eq!(result, "cat track\\ \\'name\\'\\;\\$\\(no\\)\\%\\!.flac");
        let target = literal_target(&result, result.len()).unwrap();
        assert_eq!(target.prefix, "track 'name';$(no)%!.flac");
    }

    #[test]
    fn quotes_unicode_mid_word_and_other_arguments_survive_completion() {
        let fixture = crate::test_support::canonical_tempdir("command completion Unicode");
        std::fs::write(fixture.path().join("café song.flac"), b"").unwrap();
        for quote in ["'", "\""] {
            let mut command = format!("cat {quote}café s.flac{quote} --keep");
            let mut cursor = command.find(".flac").unwrap();
            State::default().complete(&mut command, &mut cursor, &context(fixture.path()));
            assert_eq!(command, "cat café\\ song.flac --keep");
            assert!(command.is_char_boundary(cursor));
        }
    }

    #[test]
    fn common_prefix_then_repeated_tab_cycles_sorted_choices() {
        let fixture = crate::test_support::canonical_tempdir("command completion cycling");
        for name in ["song-b.flac", "song-a.flac"] {
            std::fs::write(fixture.path().join(name), b"").unwrap();
        }
        let mut command = "cat so".to_owned();
        let mut cursor = command.len();
        let mut state = State::default();
        for expected in [
            "cat song-",
            "cat song-a.flac",
            "cat song-b.flac",
            "cat song-a.flac",
        ] {
            state.complete(&mut command, &mut cursor, &context(fixture.path()));
            assert_eq!(command, expected);
        }
    }

    #[test]
    fn filenames_after_a_command_and_directory_paths_are_completed_without_expansion() {
        let fixture = crate::test_support::canonical_tempdir("command completion directories");
        std::fs::create_dir(fixture.path().join("album one")).unwrap();
        std::fs::write(fixture.path().join("album one/song.flac"), b"").unwrap();
        assert_eq!(complete("cat alb", fixture.path()), "cat album\\ one/");
        assert_eq!(
            complete("cat 'album one/so", fixture.path()),
            "cat album\\ one/song.flac"
        );
        let mut command = "cat alb".to_owned();
        let mut cursor = command.len();
        let mut state = State::default();
        state.complete(&mut command, &mut cursor, &context(fixture.path()));
        assert_eq!(command, "cat album\\ one/");
        state.complete(&mut command, &mut cursor, &context(fixture.path()));
        assert_eq!(command, "cat album\\ one/song.flac");
        for command in [
            "cat $(touch marker)/x",
            "cat `touch marker`/x",
            "cat $HOME/x",
            "cat *.fl",
        ] {
            assert_eq!(complete(command, fixture.path()), command);
        }
        assert!(!fixture.path().join("marker").exists());
    }

    #[test]
    fn command_completion_uses_injected_path_executable_bits_and_builtins() {
        let fixture = crate::test_support::canonical_tempdir("command completion executables");
        let executable_path = fixture.path().join("youta-fixture-tool");
        std::fs::write(&executable_path, b"must never execute").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable_path, std::fs::Permissions::from_mode(0o700))
                .unwrap();
            std::fs::write(fixture.path().join("youta-fixture-text"), b"not executable").unwrap();
        }
        let path = std::env::join_paths([fixture.path()]).unwrap();
        let context = Context {
            directory: fixture.path(),
            search_path: Some(&path),
            home: None,
        };
        let mut command = "youta-fixture-".to_owned();
        let mut cursor = command.len();
        State::default().complete(&mut command, &mut cursor, &context);
        assert_eq!(command, "youta-fixture-tool");
        assert_eq!(complete("prin", fixture.path()), "printf");
        assert_eq!(
            complete("echo ok | prin", fixture.path()),
            "echo ok | printf"
        );
    }

    #[test]
    fn hidden_files_require_dot_and_tilde_uses_only_injected_home() {
        let fixture = crate::test_support::canonical_tempdir("command completion home");
        std::fs::write(fixture.path().join(".secret"), b"").unwrap();
        assert_eq!(complete("cat se", fixture.path()), "cat se");
        assert_eq!(complete("cat .se", fixture.path()), "cat .secret");
        let mut command = "cat ~/.se".to_owned();
        let mut cursor = command.len();
        State::default().complete(
            &mut command,
            &mut cursor,
            &Context {
                directory: fixture.path(),
                search_path: None,
                home: Some(fixture.path()),
            },
        );
        let target = literal_target(&command, cursor).unwrap();
        assert_eq!(PathBuf::from(target.prefix), fixture.path().join(".secret"));
        std::fs::create_dir(fixture.path().join("~")).unwrap();
        std::fs::write(fixture.path().join("~/literal.flac"), b"").unwrap();
        for template in ["cat '~/li'", "cat \\~/li", "cat \"~/li\""] {
            let mut command = template.to_owned();
            let mut cursor = command.len();
            State::default().complete(
                &mut command,
                &mut cursor,
                &Context {
                    directory: fixture.path(),
                    search_path: None,
                    home: Some(fixture.path()),
                },
            );
            assert_eq!(command, "cat \\~/literal.flac");
        }
    }

    #[test]
    fn completion_bounds_candidates_bytes_and_command_length() {
        let fixture = crate::test_support::canonical_tempdir("command completion bounds");
        for index in 0..MAX_CANDIDATES + 10 {
            std::fs::write(fixture.path().join(format!("file-{index}")), b"").unwrap();
        }
        let target = literal_target("cat file-", 9).unwrap();
        assert_eq!(
            candidates(&target, &context(fixture.path())).len(),
            MAX_CANDIDATES
        );
        let mut command = format!("{} cat file-", "x".repeat(super::super::COMMAND_LIMIT - 11));
        let original = command.clone();
        let mut cursor = command.len();
        State::default().complete(&mut command, &mut cursor, &context(fixture.path()));
        assert!(command.len() <= super::super::COMMAND_LIMIT);
        assert!(command == original || command.len() >= original.len());
    }
}
