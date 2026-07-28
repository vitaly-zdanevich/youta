//! Terminal-attachment detection shared by non-graphical TUI capabilities.

use std::path::{Path, PathBuf};

/// Observable facts needed to confirm a directly attached Linux virtual console.
///
/// A `TERM=linux` value alone is not authoritative: it can be copied through
/// SSH or supplied inside a pseudo-terminal. Callers must also verify both
/// standard streams and the resolved output device before disabling features
/// that require a desktop session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalAttachment {
    /// Whether this binary is running on Linux.
    pub(crate) linux: bool,
    /// Whether standard input is attached to a terminal.
    pub(crate) stdin_is_terminal: bool,
    /// Whether standard output is attached to a terminal.
    pub(crate) stdout_is_terminal: bool,
    /// Current `TERM` value.
    pub(crate) term: Option<String>,
    /// Whether an SSH transport variable is present.
    pub(crate) ssh: bool,
    /// Whether the process is nested inside tmux.
    pub(crate) tmux: bool,
    /// Resolved standard-output device, when the operating system exposes it.
    pub(crate) output_device: Option<PathBuf>,
}

impl TerminalAttachment {
    /// Returns whether this is a confirmed local Linux `/dev/ttyN` attachment.
    pub(crate) fn is_physical_linux_virtual_console(&self) -> bool {
        self.linux
            && self.stdin_is_terminal
            && self.stdout_is_terminal
            && !self.ssh
            && !self.tmux
            && self
                .term
                .as_deref()
                .is_some_and(|term| term.eq_ignore_ascii_case("linux"))
            && self
                .output_device
                .as_deref()
                .is_some_and(is_linux_virtual_console)
    }

    /// Returns whether controls that launch a graphical external opener apply.
    pub(crate) fn external_opener_available(&self) -> bool {
        !self.is_physical_linux_virtual_console()
    }
}

/// Recognizes kernel virtual-console device names without accepting serial TTYs.
pub(crate) fn is_linux_virtual_console(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    name.strip_prefix("tty").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graphical_attachment() -> TerminalAttachment {
        TerminalAttachment {
            linux: true,
            stdin_is_terminal: true,
            stdout_is_terminal: true,
            term: Some("xterm-256color".to_owned()),
            ssh: false,
            tmux: false,
            output_device: Some(PathBuf::from("/dev/pts/4")),
        }
    }

    #[test]
    fn physical_linux_virtual_console_disables_external_opener() {
        let attachment = TerminalAttachment {
            term: Some("linux".to_owned()),
            output_device: Some(PathBuf::from("/dev/tty2")),
            ..graphical_attachment()
        };

        assert!(attachment.is_physical_linux_virtual_console());
        assert!(!attachment.external_opener_available());
    }

    #[test]
    fn pty_graphical_and_ssh_attachments_keep_external_opener() {
        let graphical = graphical_attachment();
        assert!(!graphical.is_physical_linux_virtual_console());
        assert!(graphical.external_opener_available());

        let linux_pty = TerminalAttachment {
            term: Some("linux".to_owned()),
            ..graphical.clone()
        };
        assert!(!linux_pty.is_physical_linux_virtual_console());
        assert!(linux_pty.external_opener_available());

        let ssh = TerminalAttachment {
            term: Some("linux".to_owned()),
            ssh: true,
            output_device: Some(PathBuf::from("/dev/tty2")),
            ..graphical
        };
        assert!(!ssh.is_physical_linux_virtual_console());
        assert!(ssh.external_opener_available());
    }
}
