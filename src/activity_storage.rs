//! Session-owned storage for automatic activity data.
//!
//! A shared policy lease keeps delayed workers from publishing to persistent
//! storage after saving has been disabled. Private session directories are
//! removed when the last owner, including background workers, is released.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Shared policy and lifetime of automatic session files.
#[derive(Clone, Debug, Default)]
pub struct ActivityStorage {
    state: Arc<RwLock<StorageState>>,
}

#[derive(Debug, Default)]
struct StorageState {
    enabled: Option<bool>,
    generation: u64,
    session: Option<tempfile::TempDir>,
}

// Runtime storage is deliberately excluded from configuration equality.
impl PartialEq for ActivityStorage {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for ActivityStorage {}

impl ActivityStorage {
    /// Captures the loaded preference once; clones retain later live changes.
    pub(crate) fn initialize(&self, enabled: bool) {
        if self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .enabled
            .is_some()
        {
            return;
        }
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .enabled
            .get_or_insert(enabled);
    }

    /// Applies a policy change after all previously leased writes finish.
    ///
    /// Disabling prepares private temporary storage before changing policy. A
    /// filesystem failure leaves the old policy in place and is returned.
    ///
    /// # Errors
    /// Returns an error when private scratch storage or the policy lock is unavailable.
    pub fn set_enabled(&self, enabled: bool) -> io::Result<()> {
        let mut state = self.state.write().map_err(|_| storage_poisoned())?;
        if !enabled {
            prepare_session(&mut state)?;
        }
        if state.enabled != Some(enabled) {
            state.generation = state.generation.wrapping_add(1);
            state.enabled = Some(enabled);
        }
        Ok(())
    }

    /// Returns an operation-scoped path with the supplied persistent location.
    #[must_use]
    pub fn path(&self, persistent: PathBuf) -> ActivityPath {
        let relative = persistent
            .file_name()
            .map_or_else(|| PathBuf::from("activity"), PathBuf::from);
        ActivityPath {
            storage: Some(self.clone()),
            persistent,
            relative,
            generation: None,
            private_only: false,
        }
    }

    /// Returns scratch space that never belongs to the configuration tree.
    ///
    /// Holding a clone of this owner keeps the returned directory alive.
    ///
    /// # Errors
    /// Returns an error when the private directory cannot be created or secured.
    pub fn session_directory(&self) -> io::Result<PathBuf> {
        let mut state = self.state.write().map_err(|_| storage_poisoned())?;
        prepare_session(&mut state)?;
        state
            .session
            .as_ref()
            .map(|directory| directory.path().to_owned())
            .ok_or_else(|| io::Error::other("automatic activity storage was not prepared"))
    }
}

/// A cache location resolved against the current shared persistence policy.
///
/// Workers retain this handle, then call [`Self::with_path`] at the filesystem
/// boundary. Retaining its resolved `PathBuf` across jobs would bypass policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivityPath {
    storage: Option<ActivityStorage>,
    persistent: PathBuf,
    relative: PathBuf,
    generation: Option<u64>,
    private_only: bool,
}

impl From<PathBuf> for ActivityPath {
    fn from(persistent: PathBuf) -> Self {
        Self {
            storage: None,
            persistent,
            relative: PathBuf::new(),
            generation: None,
            private_only: false,
        }
    }
}

impl From<&Path> for ActivityPath {
    fn from(path: &Path) -> Self {
        path.to_owned().into()
    }
}

impl From<&PathBuf> for ActivityPath {
    fn from(path: &PathBuf) -> Self {
        path.clone().into()
    }
}

impl From<&ActivityPath> for ActivityPath {
    fn from(path: &ActivityPath) -> Self {
        path.clone()
    }
}

impl ActivityPath {
    /// Derives a child while retaining the same policy and session owner.
    #[must_use]
    pub fn join(&self, child: impl AsRef<Path>) -> Self {
        Self {
            storage: self.storage.clone(),
            persistent: self.persistent.join(child.as_ref()),
            relative: self.relative.join(child.as_ref()),
            generation: self.generation,
            private_only: self.private_only,
        }
    }

    /// Captures one request's privacy epoch before asynchronous work starts.
    ///
    /// Responses from a private request, or a request crossing a policy change,
    /// remain temporary even if saving is enabled again before they arrive.
    #[must_use]
    pub fn for_operation(&self) -> Self {
        let mut operation = self.clone();
        if operation.generation.is_none()
            && let Some(storage) = &self.storage
        {
            match storage.state.read() {
                Ok(state) => {
                    operation.generation = Some(state.generation);
                    operation.private_only = !state.enabled.unwrap_or(true);
                }
                Err(_) => operation.private_only = true,
            }
        }
        operation
    }

    /// Runs filesystem work while preventing a concurrent policy transition.
    ///
    /// Returns an error if private storage cannot be prepared. Existing files
    /// are never read, migrated, pruned, or removed when saving is disabled.
    ///
    /// # Errors
    /// Returns an error when private scratch storage or the policy lock is unavailable.
    pub fn with_path<T>(&self, operation: impl FnOnce(&Path) -> T) -> io::Result<T> {
        let Some(storage) = &self.storage else {
            return Ok(operation(&self.persistent));
        };
        loop {
            let state = storage.state.read().map_err(|_| storage_poisoned())?;
            if state.enabled.unwrap_or(true)
                && !self.private_only
                && self
                    .generation
                    .is_none_or(|generation| generation == state.generation)
            {
                return Ok(operation(&self.persistent));
            }
            if let Some(session) = &state.session {
                return Ok(operation(&session.path().join(&self.relative)));
            }
            drop(state);
            storage.session_directory()?;
        }
    }
}

/// Creates an owner-only directory without weakening an existing policy.
fn prepare_session(state: &mut StorageState) -> io::Result<()> {
    if state.session.is_none() {
        let session = tempfile::Builder::new()
            .prefix("youta-session-")
            .tempdir()?;
        crate::private_files::set_private_directory_permissions(session.path())?;
        state.session = Some(session);
    }
    Ok(())
}

fn storage_poisoned() -> io::Error {
    io::Error::other("automatic activity storage policy is unavailable")
}

#[cfg(test)]
mod tests {
    use super::ActivityStorage;
    use std::fs;

    #[test]
    fn disabling_redirects_existing_handles_without_touching_saved_data() {
        let root = tempfile::tempdir().unwrap();
        let storage = ActivityStorage::default();
        storage.initialize(true);
        let cache = storage.path(root.path().join("cache"));
        cache
            .with_path(|path| {
                fs::create_dir_all(path).unwrap();
                fs::write(path.join("existing"), b"keep").unwrap();
            })
            .unwrap();
        storage.set_enabled(false).unwrap();
        let temporary = cache
            .with_path(|path| {
                fs::create_dir_all(path).unwrap();
                fs::write(path.join("new"), b"session").unwrap();
                path.to_path_buf()
            })
            .unwrap();
        assert!(!temporary.starts_with(root.path()));
        assert_eq!(
            fs::read(root.path().join("cache/existing")).unwrap(),
            b"keep"
        );
        assert!(!root.path().join("cache/new").exists());
        drop(storage);
        assert!(
            temporary.exists(),
            "worker handle keeps session files alive"
        );
        drop(cache);
        assert!(!temporary.exists(), "last owner removes session files");
    }

    #[test]
    fn enabling_reuses_original_cache_without_copying_private_activity() {
        let root = tempfile::tempdir().unwrap();
        let storage = ActivityStorage::default();
        storage.initialize(false);
        let cache = storage.path(root.path().join("cache"));
        cache
            .with_path(|path| {
                fs::create_dir_all(path).unwrap();
                fs::write(path.join("private"), b"session").unwrap();
            })
            .unwrap();
        storage.set_enabled(true).unwrap();
        cache
            .with_path(|path| {
                assert_eq!(path, root.path().join("cache"));
                assert!(!path.join("private").exists());
            })
            .unwrap();
    }

    #[test]
    fn a_private_configuration_never_creates_its_persistent_cache() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("uncreated-config");
        let mut config = crate::config::Config::for_dir(&root);
        config.persistence.save_playback_history = false;
        config
            .activity_cache_dir()
            .with_path(|path| {
                fs::create_dir_all(path).unwrap();
                fs::write(path.join("mock-title"), b"session data").unwrap();
            })
            .unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn reenabling_does_not_persist_results_from_private_or_earlier_requests() {
        let fixture = tempfile::tempdir().unwrap();
        let storage = ActivityStorage::default();
        storage.initialize(true);
        let cache = storage.path(fixture.path().join("cache"));
        let older = cache.for_operation();
        storage.set_enabled(false).unwrap();
        let private = cache.for_operation();
        storage.set_enabled(true).unwrap();
        for operation in [older, private] {
            operation
                .with_path(|path| assert!(!path.starts_with(fixture.path())))
                .unwrap();
        }
        cache
            .for_operation()
            .with_path(|path| {
                assert_eq!(path, fixture.path().join("cache"));
            })
            .unwrap();
    }

    #[test]
    fn disabling_waits_for_a_leased_write_then_redirects_later_jobs() {
        use std::sync::mpsc;
        use std::time::Duration;
        let fixture = tempfile::tempdir().unwrap();
        let storage = ActivityStorage::default();
        storage.initialize(true);
        let path = storage.path(fixture.path().join("cache"));
        let worker_path = path.clone();
        let (entered, active) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            worker_path
                .with_path(|path| {
                    fs::create_dir_all(path).unwrap();
                    entered.send(()).unwrap();
                    released.recv().unwrap();
                    fs::write(path.join("accepted-before-off"), b"prior activity").unwrap();
                })
                .unwrap();
        });
        active.recv().unwrap();
        let policy = storage.clone();
        let (changed, change) = mpsc::channel();
        let toggler = std::thread::spawn(move || {
            policy.set_enabled(false).unwrap();
            changed.send(()).unwrap();
        });
        assert!(matches!(
            change.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        release.send(()).unwrap();
        writer.join().unwrap();
        change.recv_timeout(Duration::from_secs(2)).unwrap();
        toggler.join().unwrap();
        path.with_path(|path| {
            assert!(!path.starts_with(fixture.path()));
        })
        .unwrap();
    }
}
