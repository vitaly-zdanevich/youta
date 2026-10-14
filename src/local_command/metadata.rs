//! Lazy private description files whose lifetime follows the captured command plan.

use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Captured description text with one lazily materialized private file.
///
/// Keep this value inside the command plan's `Arc` for the entire process
/// invocation. Its directory is removed when the last owner drops, so cloned
/// plans retain the same exact bytes through downloads, image preparation, and
/// frontend handoffs without persisting them in Youta's session or history.
/// Equality considers only the captured text; debugging reveals neither text
/// nor the temporary path.
pub struct CommandMetadata {
    /// Immutable description captured from the selected item.
    text: String,
    /// One initialized temporary directory or a redacted preparation failure.
    temporary: OnceLock<Result<tempfile::TempDir, String>>,
}

impl CommandMetadata {
    /// Captures description text without performing filesystem I/O.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            temporary: OnceLock::new(),
        }
    }

    /// Creates the private UTF-8 description file once and returns its full path.
    ///
    /// Empty descriptions still produce a valid empty file. Every byte, including
    /// trailing newlines, is retained. The caller must keep the command plan and
    /// its metadata owner alive until the command finishes reading the file.
    ///
    /// # Errors
    /// Rejects embedded NUL, which Bash cannot represent, and reports preparation
    /// failures without exposing the private description or filesystem path.
    pub(crate) fn path(&self) -> Result<PathBuf, String> {
        self.temporary
            .get_or_init(|| self.materialize())
            .as_ref()
            .map(|directory| directory.path().join("description"))
            .map_err(Clone::clone)
    }

    /// Writes exact bytes only after the temporary directory is made private.
    fn materialize(&self) -> Result<tempfile::TempDir, String> {
        if self.text.contains('\0') {
            return Err(
                "The selected description contains NUL, which Bash cannot preserve".to_owned(),
            );
        }
        let directory = tempfile::Builder::new()
            .prefix("youta-command-metadata-")
            .tempdir()
            .map_err(|_| {
                "Cannot create a temporary directory for the selected description".to_owned()
            })?;
        crate::private_files::set_private_directory_permissions(directory.path())
            .map_err(|_| "Cannot make the selected description directory private".to_owned())?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = crate::private_files::open_privately(&mut options)
            .open(directory.path().join("description"))
            .map_err(|_| "Cannot create a private file for the selected description".to_owned())?;
        file.write_all(self.text.as_bytes())
            .map_err(|_| "Cannot write the selected description to its private file".to_owned())?;
        Ok(directory)
    }
}

impl PartialEq for CommandMetadata {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
    }
}

impl Eq for CommandMetadata {}

impl fmt::Debug for CommandMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandMetadata")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Arc;

    /// Capturing description text alone must never create a temporary directory.
    #[test]
    fn capture_is_lazy_and_materialization_reuses_one_file() {
        let metadata = CommandMetadata::new("captured description");
        assert!(metadata.temporary.get().is_none());
        let first = metadata.path().unwrap();
        assert_eq!(first.file_name().unwrap(), "description");
        assert_eq!(metadata.path().unwrap(), first);
        assert!(metadata.temporary.get().is_some());
    }

    /// Metadata is transported byte for byte, including Unicode and final newlines.
    #[test]
    fn materialized_descriptions_preserve_exact_bytes_and_empty_values() {
        for description in [
            "",
            "line one\n",
            "Unicode: Мінск 🦀\r\n\n\n",
            " \t leading and trailing \t ",
        ] {
            let metadata = CommandMetadata::new(description);
            assert_eq!(
                fs::read(metadata.path().unwrap()).unwrap(),
                description.as_bytes()
            );
        }
    }

    /// A large description does not need to fit within a process argument vector.
    #[test]
    fn a_256_kib_description_is_written_without_truncation() {
        let description = "x".repeat(256 * 1024);
        let metadata = CommandMetadata::new(description.clone());
        assert_eq!(
            fs::read(metadata.path().unwrap()).unwrap(),
            description.as_bytes()
        );
    }

    /// Bash cannot represent NUL, so failure must precede temporary file creation.
    #[test]
    fn nul_is_rejected_without_echoing_text_or_creating_a_directory() {
        let metadata = CommandMetadata::new("PRIVATE SECRET\0suffix");
        let error = metadata.path().unwrap_err();
        assert!(error.contains("NUL"));
        assert!(!error.contains("PRIVATE SECRET"));
        assert!(metadata.temporary.get().unwrap().is_err());
        assert_eq!(metadata.path().unwrap_err(), error);
    }

    /// Independently captured plans must not share a writable metadata file.
    #[test]
    fn independent_captures_have_distinct_files() {
        let first = CommandMetadata::new("same description");
        let second = CommandMetadata::new("same description");
        assert_ne!(first.path().unwrap(), second.path().unwrap());
    }

    /// Preparation state never changes equality between captured descriptions.
    #[test]
    fn equality_compares_only_captured_text_not_temporary_state() {
        let first = CommandMetadata::new("same description");
        let second = CommandMetadata::new("same description");
        assert_eq!(first, second);
        let _ = first.path().unwrap();
        assert_eq!(first, second);
        let _ = second.path().unwrap();
        assert_eq!(first, second);
        assert_ne!(first, CommandMetadata::new("different description"));
    }

    /// Debug snapshots disclose neither the description nor its temporary location.
    #[test]
    fn debug_is_redacted_before_and_after_materialization() {
        let metadata = CommandMetadata::new("PRIVATE DESCRIPTION");
        let before = format!("{metadata:?}");
        assert!(!before.contains("PRIVATE DESCRIPTION"));
        let path = metadata.path().unwrap();
        let after = format!("{metadata:?}");
        assert_eq!(before, after);
        assert!(!after.contains(path.to_str().unwrap()));
        assert!(!after.contains("youta-command-metadata-"));
    }

    /// A clone keeps metadata alive across downloads and foreground handoffs.
    #[test]
    fn the_last_arc_drop_removes_the_file_and_private_directory() {
        let metadata = Arc::new(CommandMetadata::new("captured description"));
        let clone = Arc::clone(&metadata);
        let path = metadata.path().unwrap();
        let directory = path.parent().unwrap().to_path_buf();
        drop(metadata);
        assert_eq!(fs::read(&path).unwrap(), b"captured description");
        drop(clone);
        assert!(!path.exists());
        assert!(!directory.exists());
    }

    /// Unix metadata paths grant access only to the launching account.
    #[cfg(unix)]
    #[test]
    fn metadata_file_and_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let metadata = CommandMetadata::new("private description");
        let path = metadata.path().unwrap();
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
}
