//! Optional command buttons loaded from the user's separate `commands` file.
//!
//! Parsing never executes shell source. Only button labels, provider filters,
//! shortcut labels, and normalized colors belong in a frontend's public view.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use serde::Deserialize;

use crate::domain::SourceKind;

const FILE_LIMIT: usize = 1_048_576;
const BUTTON_LIMIT: usize = 64;
const NAME_LIMIT: usize = 256;
const COMMAND_LIMIT: usize = 8_192;
const SAMPLE: &str = include_str!("../../commands.sample");

/// One validated command, retaining shell source only in the Rust controller.
#[derive(Clone, Eq, PartialEq)]
pub struct CommandButton {
    /// Visible button text, independent of the private description and command.
    pub name: String,
    /// User-authored single-line Bash source using the existing selected-item macro.
    pub command: String,
    /// Canonical provider name, or no restriction when omitted.
    pub provider: Option<String>,
    /// Optional shortcut shared by terminal and graphical frontends.
    pub hotkey: Option<Hotkey>,
    /// Optional lowercase six-digit CSS/RGB foreground color.
    pub font_color: Option<String>,
    /// Optional lowercase six-digit CSS/RGB background color.
    pub background_color: Option<String>,
}

impl fmt::Debug for CommandButton {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandButton")
            .finish_non_exhaustive()
    }
}

impl CommandButton {
    /// Returns whether this button accepts the selected item's source provider.
    #[must_use]
    pub fn matches_provider(&self, source: &SourceKind) -> bool {
        self.provider
            .as_deref()
            .is_none_or(|provider| provider == source.as_str())
    }
}

/// A normalized shortcut that remains independent of the optional controller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hotkey {
    /// Canonical human-readable spelling, for example `Ctrl+Alt+F`.
    pub label: String,
    key: HotkeyKey,
    ctrl: bool,
    alt: bool,
    shift: bool,
}

/// Local shortcut vocabulary; converting frontend keys requires `controller`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HotkeyKey {
    Character(char),
    Enter,
    Esc,
    Backspace,
    Delete,
    Insert,
    Tab,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Function(u8),
}

impl Hotkey {
    /// Parses a character, named key, or F1–F24 with optional Ctrl/Alt/Shift.
    ///
    /// Letters after Ctrl or Alt are case-insensitive; explicit Shift remains a
    /// separate modifier for those chords. Unchorded capital letters match the
    /// resolved character whether a terminal reports its Shift bit or omits it.
    ///
    /// # Errors
    ///
    /// Rejects unknown or repeated modifiers, unsupported keys, and reserved
    /// interrupt/bug-report shortcuts without echoing the supplied string.
    pub fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        if value.is_empty() || value.len() > 64 || value.chars().any(char::is_control) {
            return Err(
                "hotkey must name one printable key with optional Ctrl, Alt, or Shift".to_owned(),
            );
        }
        let parts = if value == "+" {
            vec![value]
        } else {
            value.split('+').collect::<Vec<_>>()
        };
        let (last, modifiers) = parts.split_last().expect("a nonempty value has one part");
        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        for modifier in modifiers {
            let flag = match modifier.trim().to_ascii_lowercase().as_str() {
                "ctrl" | "control" => &mut ctrl,
                "alt" => &mut alt,
                "shift" => &mut shift,
                _ => return Err("hotkey modifiers must be Ctrl, Alt, or Shift".to_owned()),
            };
            if *flag {
                return Err("hotkey modifiers must not be repeated".to_owned());
            }
            *flag = true;
        }
        let key_name = last.trim();
        let lower = key_name.to_ascii_lowercase();
        let key = match lower.as_str() {
            "enter" | "return" => HotkeyKey::Enter,
            "esc" | "escape" => HotkeyKey::Esc,
            "backspace" => HotkeyKey::Backspace,
            "delete" | "del" => HotkeyKey::Delete,
            "insert" | "ins" => HotkeyKey::Insert,
            "tab" => HotkeyKey::Tab,
            "backtab" => {
                shift = true;
                HotkeyKey::Tab
            }
            "left" => HotkeyKey::Left,
            "right" => HotkeyKey::Right,
            "up" => HotkeyKey::Up,
            "down" => HotkeyKey::Down,
            "home" => HotkeyKey::Home,
            "end" => HotkeyKey::End,
            "pageup" => HotkeyKey::PageUp,
            "pagedown" => HotkeyKey::PageDown,
            "space" => HotkeyKey::Character(' '),
            "plus" => HotkeyKey::Character('+'),
            _ if lower.starts_with('f') && lower.len() > 1 => {
                let number = lower[1..]
                    .parse::<u8>()
                    .ok()
                    .filter(|number| (1..=24).contains(number))
                    .ok_or_else(|| "hotkey function keys must be F1 through F24".to_owned())?;
                HotkeyKey::Function(number)
            }
            _ => {
                let mut characters = key_name.chars();
                let character = characters
                    .next()
                    .filter(|_| characters.next().is_none())
                    .ok_or_else(|| {
                        "hotkey must name one character, a named key, or F1 through F24".to_owned()
                    })?;
                HotkeyKey::Character(character)
            }
        };
        let key = if let HotkeyKey::Character(mut character) = key {
            if ctrl || alt {
                character = character.to_ascii_lowercase();
            } else {
                if shift {
                    character = character.to_ascii_uppercase();
                }
                // Printable characters already encode their resolved keyboard layout.
                shift = false;
            }
            if ctrl && ((!alt && character == 'c') || (alt && character == 'b')) {
                return Err(
                    "Ctrl+C and Ctrl+Alt+B are reserved and cannot run configured commands"
                        .to_owned(),
                );
            }
            HotkeyKey::Character(character)
        } else {
            key
        };
        let mut label = String::new();
        if ctrl {
            label.push_str("Ctrl+");
        }
        if alt {
            label.push_str("Alt+");
        }
        if shift {
            label.push_str("Shift+");
        }
        label.push_str(&key.label(ctrl || alt));
        Ok(Self {
            label,
            key,
            ctrl,
            alt,
            shift,
        })
    }

    /// Matches the same layout-resolved key vocabulary used by both frontends.
    #[cfg(feature = "controller")]
    #[must_use]
    pub fn matches(&self, press: crate::keymap::KeyPress) -> bool {
        use crate::keymap::Key;
        if self.ctrl != press.ctrl || self.alt != press.alt {
            return false;
        }
        let (key, shift) = match press.key {
            Key::Char(character) => (HotkeyKey::Character(character), press.shift),
            Key::Enter => (HotkeyKey::Enter, press.shift),
            Key::Esc => (HotkeyKey::Esc, press.shift),
            Key::Backspace => (HotkeyKey::Backspace, press.shift),
            Key::Delete => (HotkeyKey::Delete, press.shift),
            Key::Insert => (HotkeyKey::Insert, press.shift),
            Key::Tab => (HotkeyKey::Tab, press.shift),
            Key::BackTab => (HotkeyKey::Tab, true),
            Key::Left => (HotkeyKey::Left, press.shift),
            Key::Right => (HotkeyKey::Right, press.shift),
            Key::Up => (HotkeyKey::Up, press.shift),
            Key::Down => (HotkeyKey::Down, press.shift),
            Key::Home => (HotkeyKey::Home, press.shift),
            Key::End => (HotkeyKey::End, press.shift),
            Key::PageUp => (HotkeyKey::PageUp, press.shift),
            Key::PageDown => (HotkeyKey::PageDown, press.shift),
            Key::F(number) => (HotkeyKey::Function(number), press.shift),
        };
        match (self.key, key) {
            (HotkeyKey::Character(expected), HotkeyKey::Character(actual))
                if self.ctrl || self.alt =>
            {
                expected.eq_ignore_ascii_case(&actual) && self.shift == shift
            }
            (HotkeyKey::Character(expected), HotkeyKey::Character(actual)) => expected == actual,
            _ => self.key == key && self.shift == shift,
        }
    }
}

impl HotkeyKey {
    /// Produces a stable spelling after shortcut identity has been normalized.
    fn label(self, chorded: bool) -> String {
        match self {
            Self::Character(' ') => "Space".to_owned(),
            Self::Character('+') => "Plus".to_owned(),
            Self::Character(character) => if chorded {
                character.to_ascii_uppercase()
            } else {
                character
            }
            .to_string(),
            Self::Function(number) => format!("F{number}"),
            Self::Enter => "Enter".to_owned(),
            Self::Esc => "Esc".to_owned(),
            Self::Backspace => "Backspace".to_owned(),
            Self::Delete => "Delete".to_owned(),
            Self::Insert => "Insert".to_owned(),
            Self::Tab => "Tab".to_owned(),
            Self::Left => "Left".to_owned(),
            Self::Right => "Right".to_owned(),
            Self::Up => "Up".to_owned(),
            Self::Down => "Down".to_owned(),
            Self::Home => "Home".to_owned(),
            Self::End => "End".to_owned(),
            Self::PageUp => "PageUp".to_owned(),
            Self::PageDown => "PageDown".to_owned(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandsFile {
    #[serde(default)]
    commands: Vec<ConfiguredButton>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredButton {
    name: String,
    command: String,
    #[serde(rename = "description")]
    _description: Option<String>,
    provider: Option<String>,
    hotkey: Option<String>,
    font_color: Option<String>,
    background_color: Option<String>,
}

/// Loads only the active `commands` file, never the disabled sample.
///
/// # Errors
///
/// Rejects nonregular or oversized files, malformed TOML, unsupported fields,
/// invalid metadata, and duplicate normalized shortcuts. Errors omit input
/// excerpts because command templates and descriptions may contain secrets.
pub fn load(config_dir: &Path) -> Result<Vec<CommandButton>, String> {
    let path = config_dir.join("commands");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("cannot inspect commands: {error}")),
    };
    if !metadata.file_type().is_file() || metadata.len() > FILE_LIMIT as u64 {
        return Err("commands must be a regular file no larger than 1 MiB".to_owned());
    }
    let mut contents = String::new();
    fs::File::open(path)
        .map_err(|error| format!("cannot open commands: {error}"))?
        .take(FILE_LIMIT as u64 + 1)
        .read_to_string(&mut contents)
        .map_err(|_| "commands must contain readable UTF-8 text".to_owned())?;
    if contents.len() > FILE_LIMIT {
        return Err("commands must be no larger than 1 MiB".to_owned());
    }
    let configured: CommandsFile = toml::from_str(&contents).map_err(|_| {
        "commands must contain valid TOML [[commands]] entries with supported fields".to_owned()
    })?;
    if configured.commands.len() > BUTTON_LIMIT {
        return Err("commands may contain at most 64 buttons".to_owned());
    }
    let mut buttons: Vec<CommandButton> = Vec::with_capacity(configured.commands.len());
    for (index, entry) in configured.commands.into_iter().enumerate() {
        let field_error =
            |field: &str, reason: &str| format!("commands entry {} {field}: {reason}", index + 1);
        for (field, value, limit) in [
            ("name", &entry.name, NAME_LIMIT),
            ("command", &entry.command, COMMAND_LIMIT),
        ] {
            if value.trim().is_empty() || value.len() > limit || value.chars().any(char::is_control)
            {
                return Err(field_error(
                    field,
                    &format!(
                        "must be one nonempty line of at most {limit} bytes without control characters"
                    ),
                ));
            }
        }
        super::expand_selected_path(&entry.command)
            .map_err(|reason| field_error("command", &reason))?;
        let provider = entry
            .provider
            .map(|provider| {
                let provider = provider.trim().to_ascii_lowercase();
                if matches!(SourceKind::from(provider.as_str()), SourceKind::Other(_)) {
                    return Err(field_error(
                        "provider",
                        "must name a supported provider, such as local or youtube",
                    ));
                }
                Ok(provider)
            })
            .transpose()?;
        let hotkey = entry
            .hotkey
            .as_deref()
            .map(Hotkey::parse)
            .transpose()
            .map_err(|reason| field_error("hotkey", &reason))?;
        if let Some(hotkey) = &hotkey
            && buttons.iter().any(|button| {
                button.hotkey.as_ref() == Some(hotkey)
                    && (button.provider.is_none()
                        || provider.is_none()
                        || button.provider == provider)
            })
        {
            return Err(field_error(
                "hotkey",
                "duplicates an earlier command's shortcut for the same provider",
            ));
        }
        let font_color = entry
            .font_color
            .as_deref()
            .map(parse_color)
            .transpose()
            .map_err(|reason| field_error("font_color", reason))?;
        let background_color = entry
            .background_color
            .as_deref()
            .map(parse_color)
            .transpose()
            .map_err(|reason| field_error("background_color", reason))?;
        buttons.push(CommandButton {
            name: entry.name,
            command: entry.command,
            provider,
            hotkey,
            font_color,
            background_color,
        });
    }
    Ok(buttons)
}

/// Normalizes only the hexadecimal colors supported identically by both UIs.
fn parse_color(value: &str) -> Result<String, &'static str> {
    let digits = value
        .strip_prefix('#')
        .ok_or("must use #rgb or #rrggbb hexadecimal notation")?;
    if !matches!(digits.len(), 3 | 6) || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("must use #rgb or #rrggbb hexadecimal notation");
    }
    let mut normalized = String::from("#");
    for digit in digits.chars() {
        normalized.push(digit.to_ascii_lowercase());
        if digits.len() == 3 {
            normalized.push(digit.to_ascii_lowercase());
        }
    }
    Ok(normalized)
}

/// Creates the disabled sample once, preserving existing files and symlinks.
///
/// The caller creates and secures the configuration directory first. This
/// function never creates the active `commands` file or enables a button.
///
/// # Errors
///
/// Returns filesystem errors other than the sample already existing.
pub fn ensure_sample(config_dir: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut sample = match crate::private_files::open_privately(&mut options)
        .open(config_dir.join("commands.sample"))
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error),
    };
    sample.write_all(SAMPLE.as_bytes())?;
    sample.sync_all()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    /// Loads an isolated configuration fixture without touching the user's directory.
    fn fixture(contents: &str) -> Result<Vec<CommandButton>, String> {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("commands"), contents).unwrap();
        load(directory.path())
    }

    /// A disabled example remains inert until the user explicitly enables it.
    #[test]
    fn missing_commands_and_sample_alone_load_no_buttons() {
        let directory = tempdir().unwrap();
        assert!(load(directory.path()).unwrap().is_empty());
        ensure_sample(directory.path()).unwrap();
        assert!(directory.path().join("commands.sample").is_file());
        assert!(!directory.path().join("commands").exists());
        assert!(load(directory.path()).unwrap().is_empty());
    }

    /// Enabling the shipped example loads its documented metadata and safe command.
    #[test]
    fn sample_loads_after_explicit_rename_and_existing_files_survive() {
        let directory = tempdir().unwrap();
        ensure_sample(directory.path()).unwrap();
        fs::rename(
            directory.path().join("commands.sample"),
            directory.path().join("commands"),
        )
        .unwrap();
        let buttons = load(directory.path()).unwrap();
        assert_eq!(buttons.len(), 1);
        assert_eq!(buttons[0].command, "ffmpeg -n -i % %.flac");
        assert_eq!(buttons[0].hotkey.as_ref().unwrap().label, "Ctrl+Alt+F");
        fs::write(directory.path().join("commands.sample"), "keep this edit").unwrap();
        ensure_sample(directory.path()).unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join("commands.sample")).unwrap(),
            "keep this edit"
        );
        assert_eq!(load(directory.path()).unwrap(), buttons);
    }

    /// Shell templates and config-only descriptions never appear in debug output.
    #[test]
    fn optional_metadata_is_normalized_without_exposing_private_fields() {
        let buttons = fixture(
			"[[commands]]\nname = 'Convert'\ncommand = 'printf secret-token %'\ndescription = 'private-description'\nfont_color = '#AbC'\nbackground_color = '#0123EF'\nhotkey = 'alt+ctrl+f'\n",
		)
		.unwrap();
        assert_eq!(buttons[0].name, "Convert");
        assert_eq!(buttons[0].font_color.as_deref(), Some("#aabbcc"));
        assert_eq!(buttons[0].background_color.as_deref(), Some("#0123ef"));
        assert_eq!(buttons[0].hotkey.as_ref().unwrap().label, "Ctrl+Alt+F");
        let debug = format!("{buttons:?}");
        assert!(!debug.contains("secret-token"));
        assert!(!debug.contains("private-description"));
        let buttons = fixture("[[commands]]\nname = 'No hotkey'\ncommand = 'true'\n").unwrap();
        assert!(buttons[0].hotkey.is_none());
        assert!(buttons[0].font_color.is_none());
        assert!(buttons[0].background_color.is_none());
    }

    /// Parsing and validating shell source never starts a process.
    #[test]
    fn loading_commands_never_executes_their_source() {
        let directory = tempdir().unwrap();
        let marker = directory.path().join("execution-marker");
        fs::write(
            directory.path().join("commands"),
            format!(
                "[[commands]]\nname = 'Do not run'\ncommand = 'touch \"{}\"'\n",
                marker.display()
            ),
        )
        .unwrap();
        assert_eq!(load(directory.path()).unwrap().len(), 1);
        assert!(!marker.exists());
    }

    /// Unfiltered buttons accept every source; filtered shortcuts may be reused elsewhere.
    #[test]
    fn provider_filters_use_canonical_names_and_reject_only_overlapping_hotkeys() {
        let unfiltered = fixture("[[commands]]\nname = 'All'\ncommand = 'true'\n").unwrap();
        assert!(unfiltered[0].provider.is_none());
        assert!(unfiltered[0].matches_provider(&SourceKind::YouTube));
        assert!(unfiltered[0].matches_provider(&SourceKind::Local));
        let youtube = "[[commands]]\nname = 'YouTube'\ncommand = 'true'\nprovider = 'YouTube'\nhotkey = 'Ctrl+Alt+F'\n";
        let local = "[[commands]]\nname = 'Local'\ncommand = 'true'\nprovider = 'local'\nhotkey = 'alt+ctrl+f'\n";
        let buttons = fixture(&format!("{youtube}{local}")).unwrap();
        assert_eq!(buttons[0].provider.as_deref(), Some("youtube"));
        assert!(buttons[0].matches_provider(&SourceKind::YouTube));
        assert!(!buttons[0].matches_provider(&SourceKind::Local));
        assert!(buttons[1].matches_provider(&SourceKind::Local));
        assert!(fixture(&format!("{youtube}{youtube}")).is_err());
        let global = local.replace("provider = 'local'\n", "");
        assert!(fixture(&format!("{global}{youtube}")).is_err());
        assert!(fixture(&format!("{youtube}{global}")).is_err());
        for invalid in ["", "unknown-provider", "you-tube"] {
            assert!(
                fixture(&format!(
                    "[[commands]]\nname = 'Invalid'\ncommand = 'true'\nprovider = '{invalid}'\n"
                ))
                .is_err()
            );
        }
    }

    /// Invalid user text produces useful field locations without echoing secrets.
    #[test]
    fn invalid_fields_and_unsupported_macros_are_rejected_privately() {
        for contents in [
            "secret-token =",
            "unknown = 'secret-token'",
            "[[commands]]\nname = 'Convert'\ncommand = 'true'\nunknown = 'secret-token'",
            "[[commands]]\nname = ''\ncommand = 'true'",
            "[[commands]]\nname = '   '\ncommand = 'true'",
            "[[commands]]\nname = \"secret-token\\nname\"\ncommand = 'true'",
            "[[commands]]\nname = 'Convert'\ncommand = '   '",
            "[[commands]]\nname = 'Convert'\ncommand = \"secret-token\\ntrue\"",
            "[[commands]]\nname = 'Convert'\ncommand = \"secret-token\\ttrue\"",
            "[[commands]]\nname = 'Convert'\ncommand = 'echo $(secret-token) %'",
            "[[commands]]\nname = 'Convert'\ncommand = 'true'\nfont_color = 'red'",
            "[[commands]]\nname = 'Convert'\ncommand = 'true'\nfont_color = '#1234'",
            "[[commands]]\nname = 'Convert'\ncommand = 'true'\nbackground_color = '#gggggg'",
            "[[commands]]\nname = 'Convert'\ncommand = 'true'\nhotkey = 'Ctrl+C'",
            "[[commands]]\nname = 'Convert'\ncommand = 'true'\nhotkey = 'Ctrl+Alt+B'",
        ] {
            let error = fixture(contents).unwrap_err();
            assert!(error.contains("commands"), "{error}");
            assert!(!error.contains("secret-token"), "{error}");
        }
    }

    /// File, command, name, and entry bounds are independent of parser allocation.
    #[test]
    fn oversized_and_nonregular_commands_are_rejected() {
        let directory = tempdir().unwrap();
        fs::create_dir(directory.path().join("commands")).unwrap();
        assert!(load(directory.path()).is_err());
        let directory = tempdir().unwrap();
        fs::write(
            directory.path().join("commands"),
            vec![b' '; FILE_LIMIT + 1],
        )
        .unwrap();
        assert!(load(directory.path()).is_err());
        fs::write(directory.path().join("commands"), [0xff]).unwrap();
        assert!(load(directory.path()).is_err());
        for (name, command) in [
            ("n".repeat(NAME_LIMIT + 1), "true".to_owned()),
            ("Name".to_owned(), "x".repeat(COMMAND_LIMIT + 1)),
        ] {
            assert!(
                fixture(&format!(
                    "[[commands]]\nname = '{name}'\ncommand = '{command}'\n"
                ))
                .is_err()
            );
        }
        let entry = "[[commands]]\nname = 'Entry'\ncommand = 'true'\n";
        assert_eq!(
            fixture(&entry.repeat(BUTTON_LIMIT)).unwrap().len(),
            BUTTON_LIMIT
        );
        assert!(fixture(&entry.repeat(BUTTON_LIMIT + 1)).is_err());
    }

    /// Sample writes use owner-only permissions and never follow an existing symlink.
    #[cfg(unix)]
    #[test]
    fn sample_is_private_and_symlink_targets_are_preserved() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempdir().unwrap();
        ensure_sample(directory.path()).unwrap();
        assert_eq!(
            fs::metadata(directory.path().join("commands.sample"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let linked = tempdir().unwrap();
        let target = linked.path().join("target");
        fs::write(&target, "preserve").unwrap();
        symlink(&target, linked.path().join("commands.sample")).unwrap();
        ensure_sample(linked.path()).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "preserve");
        symlink(&target, linked.path().join("commands")).unwrap();
        assert!(load(linked.path()).is_err());
    }

    /// Shortcut aliases, modifier order, and shifted letters share one identity.
    #[test]
    fn hotkey_parsing_normalizes_and_rejects_ambiguous_bindings() {
        for (left, right) in [
            ("Ctrl+Alt+F", "alt+ctrl+f"),
            ("Shift+f", "F"),
            ("Shift+Tab", "BackTab"),
            ("Esc", "Escape"),
            ("F24", "f24"),
        ] {
            assert_eq!(Hotkey::parse(left).unwrap(), Hotkey::parse(right).unwrap());
            assert!(fixture(&format!("[[commands]]\nname = 'One'\ncommand = 'true'\nhotkey = '{left}'\n[[commands]]\nname = 'Two'\ncommand = 'true'\nhotkey = '{right}'\n")).is_err());
        }
        for invalid in [
            "",
            "Ctrl",
            "Ctrl+",
            "Ctrl+Ctrl+F",
            "Meta+F",
            "F0",
            "F25",
            "Ctrl+C",
            "Ctrl+Shift+C",
            "Ctrl+Alt+B",
            "Ctrl+Alt+Shift+B",
        ] {
            assert!(Hotkey::parse(invalid).is_err(), "{invalid}");
        }
        for valid in [
            "q",
            "F",
            "Ctrl+Q",
            "Alt+Delete",
            "Ctrl+F24",
            "Space",
            "Plus",
            "+",
            "é",
        ] {
            assert!(Hotkey::parse(valid).is_ok(), "{valid}");
        }
    }

    /// DOM Shift information and terminal-resolved uppercase letters agree.
    #[cfg(feature = "controller")]
    #[test]
    fn hotkeys_match_shared_events_in_both_frontends() {
        use crate::keymap::{Key, KeyPress};
        let upper = Hotkey::parse("F").unwrap();
        for shift in [false, true] {
            assert!(upper.matches(KeyPress {
                key: Key::Char('F'),
                ctrl: false,
                alt: false,
                shift
            }));
        }
        assert!(!upper.matches(KeyPress::new(Key::Char('f'))));
        let chord = Hotkey::parse("Ctrl+Alt+F").unwrap();
        for character in ['f', 'F'] {
            assert!(chord.matches(KeyPress {
                key: Key::Char(character),
                ctrl: true,
                alt: true,
                shift: false
            }));
        }
        assert!(!chord.matches(KeyPress {
            key: Key::Char('f'),
            ctrl: true,
            alt: false,
            shift: false
        }));
        assert!(!chord.matches(KeyPress {
            key: Key::Char('F'),
            ctrl: true,
            alt: true,
            shift: true
        }));
        let shifted = Hotkey::parse("Shift+Ctrl+F").unwrap();
        assert!(shifted.matches(KeyPress {
            key: Key::Char('F'),
            ctrl: true,
            alt: false,
            shift: true
        }));
        let tab = Hotkey::parse("Shift+Tab").unwrap();
        assert!(tab.matches(KeyPress {
            key: Key::Tab,
            ctrl: false,
            alt: false,
            shift: true
        }));
        assert!(tab.matches(KeyPress::new(Key::BackTab)));
        assert!(Hotkey::parse("Alt+F24").unwrap().matches(KeyPress {
            key: Key::F(24),
            ctrl: false,
            alt: true,
            shift: false
        }));
    }
}
