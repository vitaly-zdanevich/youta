//! Keeping Youta's own files to Youta's own user, on each platform's terms.
//!
//! Youta stores API keys, OAuth tokens, session snapshots and private notes
//! under its configuration home. On Unix that is one `chmod`: `0o600` for a
//! file, `0o700` for a directory, and the call costs nothing, so every file is
//! hardened as it is written.
//!
//! Windows has no mode bits. `fs::set_permissions` there can only toggle the
//! read-only flag, which protects nothing — so every `0o600` in this codebase
//! was, on Windows, a line that did nothing while looking like it did. The real
//! mechanism is the discretionary access control list, and the only way to
//! reach it without `unsafe` — which this crate forbids — is `icacls.exe`, the
//! ACL editor shipped with Windows itself.
//!
//! # Why directories and not files
//!
//! Spawning a process per file write would be absurd: state is saved whenever
//! anything changes. It is also unnecessary, because an NTFS access control
//! entry marked `(OI)(CI)` is *inherited* by everything created inside the
//! directory afterwards. So Windows hardens the directory once, when it is
//! created, and every file written into it is born private. That is why
//! [`set_private_file_permissions`] does nothing on Windows and says so, rather
//! than pretending.
//!
//! The window this leaves is the moment between `create_dir_all` and the
//! `icacls` call. It is Youta's own freshly created and still empty directory,
//! so there is nothing inside it to expose.
//!
//! # What it does not do
//!
//! It does not encrypt. A determined administrator, and any process running as
//! the same user, can still read these files — the same as on Unix. The goal is
//! that *other* users on a shared machine cannot, which is exactly what the
//! Unix mode bits buy.

use std::fs::OpenOptions;
use std::io;
use std::path::Path;

/// Creates a directory that only the current user may enter.
///
/// # Errors
///
/// Returns the underlying error when the directory cannot be created or its
/// access control cannot be tightened.
pub fn create_private_directory(path: &Path) -> io::Result<()> {
    std::fs::create_dir_all(path)?;
    set_private_directory_permissions(path)
}

/// Restricts an existing directory, and anything created in it later, to the
/// current user.
///
/// # Errors
///
/// Returns the underlying error when the access control cannot be tightened.
pub fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
    }
    #[cfg(windows)]
    {
        windows_acl::restrict_directory(path)
    }
}

/// Restricts an existing file to the current user.
///
/// On Windows this is deliberately nothing: the file inherits the access
/// control of the directory [`create_private_directory`] already tightened, and
/// running the ACL editor once per save would cost a process per keystroke.
///
/// # Errors
///
/// Returns the underlying error when the mode cannot be set.
pub fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Asks for a private file at the moment it is created, where the platform
/// allows it, so the file is never briefly readable by anyone else.
pub fn open_privately(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600)
    }
    #[cfg(not(unix))]
    {
        options
    }
}

/// How the ACL editor is asked to make a directory private.
///
/// Compiled on Windows, where it is used, and under `cfg(test)` everywhere,
/// where it is asserted: the argument vector is the whole security decision, so
/// it should not be a thing only the Windows lane ever reads.
#[cfg(any(windows, test))]
mod acl_plan {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    /// Names the account whose grant an access control entry should carry.
    ///
    /// `icacls` accepts `DOMAIN\user`, and plain `user` for a machine-local
    /// account. Youta asks the environment rather than the security API,
    /// because the security API is not reachable without `unsafe`.
    pub(super) fn account_name(domain: Option<&str>, user: Option<&str>) -> Option<String> {
        let user = user.map(str::trim).filter(|user| !user.is_empty())?;
        // A domain equal to the machine name is what a workgroup PC reports; it
        // is still the correct qualifier, so it is not filtered out.
        let domain = domain.map(str::trim).filter(|domain| !domain.is_empty());
        Some(match domain {
            Some(domain) => format!("{domain}\\{user}"),
            None => user.to_owned(),
        })
    }

    /// Builds the invocation that makes `path` private to `account`.
    ///
    /// `(OI)` and `(CI)` mark the entry inheritable by files and by
    /// subdirectories, which is what lets every later write inherit privacy
    /// instead of asking for it. `/inheritance:r` then drops the entries the
    /// directory inherited from its parent, so a permissive profile directory
    /// does not leak back in. `icacls` applies the grant before removing
    /// inheritance, so the account is never locked out of its own directory.
    pub(super) fn restrict_directory_arguments(path: &Path, account: &str) -> Vec<OsString> {
        vec![
            path.as_os_str().to_owned(),
            OsString::from("/inheritance:r"),
            OsString::from("/grant:r"),
            OsString::from(format!("{account}:(OI)(CI)F")),
            // Report only failures, and keep going rather than stopping on the
            // first object, so the exit status is the whole answer.
            OsString::from("/Q"),
            OsString::from("/C"),
        ]
    }

    /// Resolves the ACL editor by absolute path rather than through `PATH`.
    ///
    /// A fixed executable is the rule everywhere else in Youta, and it matters
    /// more here than usual: this command is what stands between a stored OAuth
    /// token and the other accounts on the machine.
    pub(super) fn acl_editor(system_root: Option<&str>) -> PathBuf {
        let root = system_root
            .map(str::trim)
            .filter(|root| !root.is_empty())
            .unwrap_or(r"C:\Windows");
        Path::new(root).join("System32").join("icacls.exe")
    }
}

#[cfg(windows)]
mod windows_acl {
    use std::env;
    use std::io;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::acl_plan::{account_name, acl_editor, restrict_directory_arguments};

    pub(super) fn restrict_directory(path: &Path) -> io::Result<()> {
        let Some(account) = account_name(
            env::var("USERDOMAIN").ok().as_deref(),
            env::var("USERNAME").ok().as_deref(),
        ) else {
            return Err(io::Error::other(
                "cannot restrict this directory because Windows did not name the current account",
            ));
        };
        let editor = acl_editor(env::var("SystemRoot").ok().as_deref());
        let status = Command::new(&editor)
            .args(restrict_directory_arguments(path, &account))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "cannot run {} to restrict access: {error}",
                        editor.display()
                    ),
                )
            })?;
        if status.success() {
            return Ok(());
        }
        Err(io::Error::other(format!(
            "{} refused to restrict {} ({status}); a filesystem without access control, such as \
             FAT32 or exFAT, cannot hold Youta's private files",
            editor.display(),
            path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::acl_plan::{account_name, acl_editor, restrict_directory_arguments};
    use super::*;

    #[test]
    fn an_account_is_qualified_by_its_domain_when_there_is_one() {
        assert_eq!(
            account_name(Some("STUDIO"), Some("kt")).as_deref(),
            Some(r"STUDIO\kt")
        );
        assert_eq!(account_name(None, Some("kt")).as_deref(), Some("kt"));
        assert_eq!(account_name(Some("  "), Some("kt")).as_deref(), Some("kt"));
    }

    #[test]
    fn an_unnamed_account_produces_no_grant_rather_than_an_empty_one() {
        // An empty principal would make `icacls` grant nothing to nobody and
        // still exit zero, which is the one failure that would look like
        // success. Refusing to build the command is the point.
        assert_eq!(account_name(Some("STUDIO"), None), None);
        assert_eq!(account_name(Some("STUDIO"), Some("   ")), None);
    }

    #[test]
    fn the_grant_is_inheritable_and_replaces_what_the_parent_offered() {
        let arguments = restrict_directory_arguments(Path::new(r"C:\state"), r"STUDIO\kt");
        let rendered: Vec<String> = arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();

        assert_eq!(rendered[0], r"C:\state");
        assert!(rendered.contains(&"/inheritance:r".to_owned()));
        assert!(rendered.contains(&r"STUDIO\kt:(OI)(CI)F".to_owned()));
        // The account is one argument, so a display name containing a space
        // cannot become two.
        assert_eq!(arguments.len(), 6);
    }

    #[test]
    fn the_acl_editor_is_named_absolutely_and_never_searched_for() {
        // Asserted by component rather than by rendered string: this test also
        // runs on Unix, where `Path::join` writes a different separator, and
        // the claim is about *which* file is named, not how it is spelled.
        let editor = acl_editor(Some(r"D:\Windows"));
        assert_eq!(editor.file_name(), Some(OsStr::new("icacls.exe")));
        assert_eq!(
            editor.parent().and_then(Path::file_name),
            Some(OsStr::new("System32"))
        );
        assert!(editor.starts_with(r"D:\Windows"));

        assert!(acl_editor(None).starts_with(r"C:\Windows"));
        assert_eq!(acl_editor(Some("  ")), acl_editor(None));
    }

    #[test]
    fn a_private_directory_is_created_and_kept_to_its_owner() {
        let root = tempfile::tempdir().expect("temporary directory");
        let nested = root.path().join("state").join("secrets");

        create_private_directory(&nested).expect("private directory");
        assert!(nested.is_dir());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = std::fs::metadata(&nested)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[test]
    fn a_private_file_is_written_unreadable_to_everyone_else() {
        let root = tempfile::tempdir().expect("temporary directory");
        let path = root.path().join("credentials.toml");

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let file = open_privately(&mut options).open(&path).expect("create");
        drop(file);
        set_private_file_permissions(&path).expect("restrict");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
