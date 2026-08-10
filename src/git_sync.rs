//! Bounded, path-scoped Git synchronization for graceful TUI shutdown.
//!
//! The synchronizer deliberately does not pull, merge, invoke a shell, or
//! stage paths outside Youta's configured root. Repository ignore rules remain
//! authoritative: users may intentionally track generated data or credentials
//! in a private repository.

use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

/// Commit message used for automatic state synchronization.
pub const AUTOMATIC_COMMIT_MESSAGE: &str = "Automatic state update";

const DEFAULT_SYNC_TIMEOUT: Duration = Duration::from_mins(1);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_COMMAND_OUTPUT_BYTES: usize = 64 * 1024;

/// Result of one successful shutdown synchronization attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitSyncOutcome {
    /// The configured directory is not inside a Git worktree.
    NotRepository,
    /// The configured Youta root has no changes to commit or retry pushing.
    NoChanges,
    /// Youta state was pushed, either in a new commit or a retried push.
    Pushed,
}

/// Safely commits and pushes changed Youta state from `config_root`.
///
/// A directory outside a Git worktree is an expected no-op. Inside a
/// worktree, this function runs `git add .` from `config_root`, creates a
/// pathspec-limited commit using [`AUTOMATIC_COMMIT_MESSAGE`], and pushes it
/// without pulling. Existing repository ignore rules decide which paths are
/// eligible.
///
/// If an earlier shutdown committed state but could not push it, a later
/// invocation detects that the current branch is ahead of its upstream and
/// retries the push even when no files changed again. Every Git child is
/// non-interactive and shares one bounded wall-clock deadline. The caller
/// should report an error only after the terminal has been restored; a
/// synchronization failure must not turn an otherwise graceful TUI shutdown
/// into an application failure. If the Git executable is absent and neither
/// `config_root` nor one of its ancestors has a `.git` control path, the
/// directory is treated as outside a worktree and no error is reported.
///
/// # Errors
///
/// Returns an error when Git cannot be started for a discoverable worktree or
/// times out, or a stage, commit, or push command fails.
pub fn sync_config_root(config_root: &Path) -> Result<GitSyncOutcome, GitSyncError> {
    sync_config_root_with(config_root, Path::new("git"), DEFAULT_SYNC_TIMEOUT)
}

fn sync_config_root_with(
    config_root: &Path,
    git_executable: &Path,
    timeout: Duration,
) -> Result<GitSyncOutcome, GitSyncError> {
    sync_config_root_with_prefix(config_root, git_executable, &[], timeout)
}

fn sync_config_root_with_prefix(
    config_root: &Path,
    git_executable: &Path,
    executable_prefix: &[OsString],
    timeout: Duration,
) -> Result<GitSyncOutcome, GitSyncError> {
    let config_root =
        crate::fs_path::canonicalize(config_root).map_err(|source| GitSyncError::ConfigRoot {
            path: config_root.to_path_buf(),
            source,
        })?;
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let mut git = GitRunner {
        executable: git_executable,
        executable_prefix,
        deadline,
    };

    let is_worktree = match is_git_worktree(&mut git, &config_root) {
        Ok(is_worktree) => is_worktree,
        Err(error) if git_is_missing(&error) && !has_git_control_path(&config_root) => false,
        Err(error) => return Err(error),
    };
    if !is_worktree {
        return Ok(GitSyncOutcome::NotRepository);
    }

    git.require_success(
        &[
            OsString::from("-C"),
            config_root.as_os_str().to_owned(),
            OsString::from("add"),
            OsString::from("."),
        ],
        "stage Youta state",
    )?;

    let diff = git.run(
        &[
            OsString::from("-C"),
            config_root.as_os_str().to_owned(),
            OsString::from("diff"),
            OsString::from("--cached"),
            OsString::from("--quiet"),
            OsString::from("--"),
            OsString::from("."),
        ],
        "check staged Youta state",
    )?;
    match diff.status.code() {
        Some(0) => {
            if branch_is_ahead_of_upstream(&mut git, &config_root)? {
                git.require_success(
                    &[
                        OsString::from("-C"),
                        config_root.as_os_str().to_owned(),
                        OsString::from("push"),
                    ],
                    "push Youta state",
                )?;
                return Ok(GitSyncOutcome::Pushed);
            }
            return Ok(GitSyncOutcome::NoChanges);
        }
        Some(1) => {}
        _ => {
            return Err(GitSyncError::CommandFailed {
                operation: "check staged Youta state",
                status: diff.status,
                stderr: printable_output(&diff.stderr),
            });
        }
    }

    git.require_success(
        &[
            OsString::from("-C"),
            config_root.as_os_str().to_owned(),
            OsString::from("commit"),
            OsString::from("--only"),
            OsString::from("-m"),
            OsString::from(AUTOMATIC_COMMIT_MESSAGE),
            OsString::from("--"),
            OsString::from("."),
        ],
        "commit Youta state",
    )?;
    git.require_success(
        &[
            OsString::from("-C"),
            config_root.as_os_str().to_owned(),
            OsString::from("push"),
        ],
        "push Youta state",
    )?;
    Ok(GitSyncOutcome::Pushed)
}

fn is_git_worktree(git: &mut GitRunner<'_>, config_root: &Path) -> Result<bool, GitSyncError> {
    let discovery = git.run(
        &[
            OsString::from("-C"),
            config_root.as_os_str().to_owned(),
            OsString::from("rev-parse"),
            OsString::from("--is-inside-work-tree"),
        ],
        "discover the Git worktree",
    )?;
    if !discovery.status.success() {
        if is_not_repository_error(&discovery.stderr) {
            return Ok(false);
        }
        return Err(GitSyncError::CommandFailed {
            operation: "discover the Git worktree",
            status: discovery.status,
            stderr: printable_output(&discovery.stderr),
        });
    }
    match discovery.stdout.as_slice().trim_ascii() {
        b"true" => Ok(true),
        b"false" => Ok(false),
        output => Err(GitSyncError::UnexpectedCommandOutput {
            operation: "discover the Git worktree",
            output: printable_output(output),
        }),
    }
}

/// Returns whether worktree discovery failed because Git is not installed.
fn git_is_missing(error: &GitSyncError) -> bool {
    matches!(
        error,
        GitSyncError::StartGit {
            operation: "discover the Git worktree",
            source,
        } if source.kind() == io::ErrorKind::NotFound
    )
}

/// Conservatively detects a normal or linked Git worktree control path.
///
/// An unreadable candidate is treated as present so a missing Git executable
/// remains visible instead of silently skipping a potentially configured
/// repository.
fn has_git_control_path(config_root: &Path) -> bool {
    config_root
        .ancestors()
        .any(|directory| is_git_control_path(&directory.join(".git")))
}

/// Recognizes a normal `.git` directory or a linked-worktree control file.
fn is_git_control_path(control_path: &Path) -> bool {
    let metadata = match control_path.metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    if metadata.is_dir() {
        return match control_path.join("HEAD").try_exists() {
            Ok(exists) => exists,
            Err(_) => true,
        };
    }
    if !metadata.is_file() {
        return false;
    }

    let mut prefix = [0_u8; b"gitdir:".len()];
    match std::fs::File::open(control_path).and_then(|mut file| file.read_exact(&mut prefix)) {
        Ok(()) => prefix.eq_ignore_ascii_case(b"gitdir:"),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

fn is_not_repository_error(stderr: &[u8]) -> bool {
    stderr
        .windows(b"not a git repository".len())
        .any(|window| window.eq_ignore_ascii_case(b"not a git repository"))
}

fn branch_is_ahead_of_upstream(
    git: &mut GitRunner<'_>,
    config_root: &Path,
) -> Result<bool, GitSyncError> {
    let status = git.require_success(
        &[
            OsString::from("-C"),
            config_root.as_os_str().to_owned(),
            OsString::from("status"),
            OsString::from("--porcelain=v2"),
            OsString::from("--branch"),
            OsString::from("--untracked-files=no"),
            OsString::from("--"),
            OsString::from("."),
        ],
        "inspect unpushed Git state",
    )?;
    parse_branch_ahead(&status.stdout)
}

fn parse_branch_ahead(output: &[u8]) -> Result<bool, GitSyncError> {
    const PREFIX: &[u8] = b"# branch.ab +";

    let Some(line) = output
        .split(|byte| *byte == b'\n')
        .find_map(|line| line.strip_prefix(PREFIX))
    else {
        // A detached HEAD or branch without an upstream has no branch.ab
        // header. There is no configured destination to retry in that case.
        return Ok(false);
    };
    let ahead = line
        .split(|byte| *byte == b' ')
        .next()
        .filter(|value| !value.is_empty())
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| GitSyncError::UnexpectedCommandOutput {
            operation: "inspect unpushed Git state",
            output: printable_output(output),
        })?;
    Ok(ahead > 0)
}

struct GitRunner<'a> {
    executable: &'a Path,
    executable_prefix: &'a [OsString],
    deadline: Instant,
}

impl GitRunner<'_> {
    fn require_success(
        &mut self,
        arguments: &[OsString],
        operation: &'static str,
    ) -> Result<CommandOutput, GitSyncError> {
        let output = self.run(arguments, operation)?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(GitSyncError::CommandFailed {
                operation,
                status: output.status,
                stderr: printable_output(&output.stderr),
            })
        }
    }

    fn run(
        &mut self,
        arguments: &[OsString],
        operation: &'static str,
    ) -> Result<CommandOutput, GitSyncError> {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or(GitSyncError::TimedOut { operation })?;
        if remaining.is_zero() {
            return Err(GitSyncError::TimedOut { operation });
        }

        let mut command = Command::new(self.executable);
        crate::child_process::quiet(&mut command);
        command
            .args(self.executable_prefix)
            .args(arguments)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "Never")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|source| GitSyncError::StartGit { operation, source })?;
        let stdout = child
            .stdout
            .take()
            .ok_or(GitSyncError::MissingCommandPipe { operation })?;
        let stderr = child
            .stderr
            .take()
            .ok_or(GitSyncError::MissingCommandPipe { operation })?;
        let (output_sender, output_receiver) = mpsc::channel();
        let stdout_sender = output_sender.clone();
        thread::spawn(move || {
            let _ = stdout_sender.send((CommandStream::Stdout, read_bounded(stdout)));
        });
        thread::spawn(move || {
            let _ = output_sender.send((CommandStream::Stderr, read_bounded(stderr)));
        });

        let status = loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|source| GitSyncError::WaitGit { operation, source })?
            {
                break status;
            }
            if Instant::now() >= self.deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GitSyncError::TimedOut { operation });
            }
            thread::sleep(
                PROCESS_POLL_INTERVAL.min(self.deadline.saturating_duration_since(Instant::now())),
            );
        };

        let mut stdout = None;
        let mut stderr = None;
        while stdout.is_none() || stderr.is_none() {
            let remaining = self
                .deadline
                .checked_duration_since(Instant::now())
                .ok_or(GitSyncError::TimedOut { operation })?;
            if remaining.is_zero() {
                return Err(GitSyncError::TimedOut { operation });
            }
            let (stream, output) = match output_receiver.recv_timeout(remaining) {
                Ok(output) => output,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(GitSyncError::TimedOut { operation });
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(GitSyncError::OutputReaderPanicked { operation });
                }
            };
            let output = output?;
            match stream {
                CommandStream::Stdout => stdout = Some(output),
                CommandStream::Stderr => stderr = Some(output),
            }
        }
        Ok(CommandOutput {
            status,
            stdout: stdout.expect("stdout output was collected"),
            stderr: stderr.expect("stderr output was collected"),
        })
    }
}

#[derive(Clone, Copy)]
enum CommandStream {
    Stdout,
    Stderr,
}

fn read_bounded(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut retained = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let remaining = MAX_COMMAND_OUTPUT_BYTES.saturating_sub(retained.len());
        retained.extend_from_slice(&buffer[..read.min(remaining)]);
    }
    Ok(retained)
}

fn printable_output(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let text = text.trim();
    if text.is_empty() {
        "no diagnostic output".to_owned()
    } else {
        text.to_owned()
    }
}

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Errors raised by safe shutdown Git synchronization.
#[derive(Debug, Error)]
pub enum GitSyncError {
    /// The configured application directory cannot be resolved.
    #[error("cannot resolve Youta config root {path}: {source}")]
    ConfigRoot {
        /// Configured root.
        path: PathBuf,
        /// Filesystem failure.
        source: io::Error,
    },
    /// Git could not be started.
    #[error("cannot {operation}: {source}")]
    StartGit {
        /// Logical command purpose.
        operation: &'static str,
        /// Process-start failure.
        source: io::Error,
    },
    /// Waiting for Git failed.
    #[error("cannot wait while attempting to {operation}: {source}")]
    WaitGit {
        /// Logical command purpose.
        operation: &'static str,
        /// Wait failure.
        source: io::Error,
    },
    /// A piped process stream was unexpectedly unavailable.
    #[error("cannot capture Git output while attempting to {operation}")]
    MissingCommandPipe {
        /// Logical command purpose.
        operation: &'static str,
    },
    /// A bounded output-draining thread failed.
    #[error("Git output collection failed while attempting to {operation}")]
    OutputReaderPanicked {
        /// Logical command purpose.
        operation: &'static str,
    },
    /// Reading a child stream failed.
    #[error("cannot read Git command output: {0}")]
    ReadCommandOutput(#[from] io::Error),
    /// The shared synchronization deadline elapsed.
    #[error("timed out while attempting to {operation}")]
    TimedOut {
        /// Logical command purpose.
        operation: &'static str,
    },
    /// Git reported an unsuccessful status.
    #[error("failed to {operation} ({status}): {stderr}")]
    CommandFailed {
        /// Logical command purpose.
        operation: &'static str,
        /// Git exit status.
        status: ExitStatus,
        /// Bounded diagnostic text.
        stderr: String,
    },
    /// Git returned successful but malformed machine-readable output.
    #[error("unexpected Git output while attempting to {operation}: {output}")]
    UnexpectedCommandOutput {
        /// Logical command purpose.
        operation: &'static str,
        /// Bounded output suitable for diagnostics.
        output: String,
    },
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::process::Command;

    use tempfile::TempDir;

    use super::*;

    #[derive(Clone, Copy)]
    enum Scenario {
        NotRepository,
        DiscoveryFailure,
        NoChanges,
        Changes,
        PushFailure,
        PushFailureThenRetry,
        PushTimeout,
        ExitedChildWithOpenDescendantPipe,
    }

    struct Fixture {
        temporary: TempDir,
        repository: PathBuf,
        config: PathBuf,
        mock_git: PathBuf,
        log: PathBuf,
    }

    impl Fixture {
        fn new(scenario: Scenario) -> Self {
            let temporary = tempfile::tempdir().expect("temporary directory");
            let repository = temporary.path().join("repository");
            let config = repository.join("config").join("youta");
            fs::create_dir_all(&config).expect("config directory");
            fs::write(
                config.join("config.toml"),
                "[persistence]\ngit_commit_on_change = true\n",
            )
            .expect("safe config");
            let log = temporary.path().join("git.log");
            let state = temporary.path().join("mock-state");
            let mock_git = temporary.path().join("mock-git");
            let script = mock_script(scenario, &log, &state);
            fs::write(&mock_git, script).expect("mock git");
            Self {
                temporary,
                repository,
                config,
                mock_git,
                log,
            }
        }

        fn sync(&self, timeout: Duration) -> Result<GitSyncOutcome, GitSyncError> {
            // Let the system shell read the fixture as data instead of
            // executing a just-created script directly. This keeps parallel
            // tests independent of filesystems that transiently report
            // `ETXTBSY` for new executable text files.
            sync_config_root_with_prefix(
                &self.config,
                Path::new("/bin/sh"),
                &[self.mock_git.as_os_str().to_owned()],
                timeout,
            )
        }

        fn log(&self) -> String {
            fs::read_to_string(&self.log).unwrap_or_default()
        }
    }

    #[test]
    fn non_repository_is_a_no_op_before_any_mutation() {
        let fixture = Fixture::new(Scenario::NotRepository);

        assert_eq!(
            fixture.sync(Duration::from_secs(2)).expect("no-op"),
            GitSyncOutcome::NotRepository
        );
        let log = fixture.log();
        assert!(log.contains("rev-parse\t--is-inside-work-tree"));
        assert!(!log.contains("\tadd\t"));
        assert!(!log.contains("\tcommit\t"));
        assert!(!log.contains("\tpush"));
    }

    #[test]
    fn missing_git_is_a_no_op_outside_a_discoverable_worktree() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let config = temporary.path().join("config").join("youta");
        fs::create_dir_all(&config).expect("config directory");
        let missing_git = temporary.path().join("missing-git");

        assert_eq!(
            sync_config_root_with(&config, &missing_git, Duration::from_secs(2))
                .expect("non-repository no-op"),
            GitSyncOutcome::NotRepository
        );
    }

    #[test]
    fn missing_git_is_reported_inside_a_discoverable_worktree() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let repository = temporary.path().join("repository");
        let config = repository.join("config").join("youta");
        fs::create_dir_all(repository.join(".git")).expect("Git control directory");
        fs::write(
            repository.join(".git").join("HEAD"),
            "ref: refs/heads/main\n",
        )
        .expect("Git HEAD");
        fs::create_dir_all(&config).expect("config directory");
        let missing_git = temporary.path().join("missing-git");

        let error = sync_config_root_with(&config, &missing_git, Duration::from_secs(2))
            .expect_err("missing Git in a worktree must be reported");

        assert!(matches!(
            error,
            GitSyncError::StartGit {
                operation: "discover the Git worktree",
                ref source,
            } if source.kind() == io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn missing_git_is_reported_inside_a_linked_worktree() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let repository = temporary.path().join("linked-worktree");
        let config = repository.join("config").join("youta");
        let control_directory = temporary
            .path()
            .join("git")
            .join("worktrees")
            .join("linked");
        fs::create_dir_all(&control_directory).expect("linked-worktree control directory");
        fs::write(control_directory.join("HEAD"), "ref: refs/heads/linked\n")
            .expect("linked-worktree HEAD");
        fs::create_dir_all(&config).expect("config directory");
        fs::write(
            repository.join(".git"),
            format!("gitdir: {}\n", control_directory.display()),
        )
        .expect("linked-worktree control file");
        let missing_git = temporary.path().join("missing-git");

        let error = sync_config_root_with(&config, &missing_git, Duration::from_secs(2))
            .expect_err("missing Git in a linked worktree must be reported");

        assert!(matches!(
            error,
            GitSyncError::StartGit {
                operation: "discover the Git worktree",
                ref source,
            } if source.kind() == io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn repository_discovery_failures_are_reported() {
        let fixture = Fixture::new(Scenario::DiscoveryFailure);

        let error = fixture
            .sync(Duration::from_secs(2))
            .expect_err("discovery failure");

        assert!(matches!(
            error,
            GitSyncError::CommandFailed {
                operation: "discover the Git worktree",
                ..
            }
        ));
        assert!(error.to_string().contains("fixture discovery failure"));
        assert!(!fixture.log().contains("\tadd\t"));
    }

    #[test]
    fn upstream_ahead_header_is_parsed_strictly() {
        assert!(!parse_branch_ahead(b"# branch.head main\n").expect("no upstream"));
        assert!(!parse_branch_ahead(b"# branch.ab +0 -3\n").expect("not ahead"));
        assert!(parse_branch_ahead(b"# branch.ab +2 -0\n").expect("ahead"));
        assert!(matches!(
            parse_branch_ahead(b"# branch.ab +unknown -0\n"),
            Err(GitSyncError::UnexpectedCommandOutput {
                operation: "inspect unpushed Git state",
                ..
            })
        ));
    }

    #[test]
    fn unchanged_state_is_staged_and_checked_without_commit_or_push() {
        let fixture = Fixture::new(Scenario::NoChanges);

        assert_eq!(
            fixture.sync(Duration::from_secs(2)).expect("no changes"),
            GitSyncOutcome::NoChanges
        );
        let log = fixture.log();
        assert!(log.contains("\tadd\t."));
        assert!(log.contains("\tdiff\t--cached\t--quiet\t--\t."));
        assert!(log.contains("\tstatus\t--porcelain=v2\t--branch"));
        assert!(!log.contains("\tcommit\t"));
        assert!(!log.contains("\tpush"));
    }

    #[test]
    fn changed_state_uses_only_pathspec_commit_and_pushes_without_pull() {
        let fixture = Fixture::new(Scenario::Changes);

        assert_eq!(
            fixture.sync(Duration::from_secs(2)).expect("pushed"),
            GitSyncOutcome::Pushed
        );
        let log = fixture.log();
        assert!(log.contains("\tadd\t."));
        assert!(log.contains(&format!(
            "\tcommit\t--only\t-m\t{AUTOMATIC_COMMIT_MESSAGE}\t--\t."
        )));
        assert!(log.contains("\tpush\n"));
        assert!(!log.contains("\tpull"));
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("\tcommit\t"))
                .count(),
            1
        );
        assert_eq!(
            log.lines().filter(|line| line.contains("\tpush")).count(),
            1
        );
    }

    #[test]
    fn real_git_commit_stays_scoped_and_pushes_config_state() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let temporary = tempfile::tempdir().expect("temporary directory");
        let repository = temporary.path().join("repository");
        let remote = temporary.path().join("remote.git");
        let config = repository.join("config").join("youta");
        fs::create_dir_all(&config).expect("config directory");

        run_git(
            None,
            ["init", "--bare", remote.to_str().expect("UTF-8 remote")],
        );
        run_git(
            None,
            [
                "init",
                "--initial-branch=main",
                repository.to_str().expect("UTF-8 repository"),
            ],
        );
        run_git(
            Some(&repository),
            ["config", "user.name", "Youta test user"],
        );
        run_git(
            Some(&repository),
            ["config", "user.email", "youta@example.invalid"],
        );
        run_git(
            Some(&repository),
            [
                "remote",
                "add",
                "origin",
                remote.to_str().expect("UTF-8 remote"),
            ],
        );
        fs::write(repository.join("README"), "fixture\n").expect("initial file");
        run_git(Some(&repository), ["add", "README"]);
        run_git(Some(&repository), ["commit", "-m", "Initial fixture"]);
        run_git(Some(&repository), ["push", "-u", "origin", "main"]);

        fs::write(repository.join("outside.txt"), "remain staged\n").expect("outside state");
        run_git(Some(&repository), ["add", "outside.txt"]);
        fs::create_dir_all(config.join("state")).expect("state directory");
        fs::write(config.join("state/progress.toml"), "format_version = 1\n").expect("Youta state");

        assert_eq!(
            sync_config_root_with(&config, Path::new("git"), Duration::from_secs(10))
                .expect("real Git synchronization"),
            GitSyncOutcome::Pushed
        );
        assert_eq!(
            sync_config_root_with(&config, Path::new("git"), Duration::from_secs(10))
                .expect("real Git no-change check"),
            GitSyncOutcome::NoChanges
        );
        let subject = run_git(Some(&repository), ["log", "-1", "--pretty=%s"]);
        assert_eq!(
            String::from_utf8(subject.stdout)
                .expect("UTF-8 subject")
                .trim(),
            AUTOMATIC_COMMIT_MESSAGE
        );
        let staged = run_git(Some(&repository), ["diff", "--cached", "--name-only", "--"]);
        assert_eq!(
            String::from_utf8(staged.stdout)
                .expect("UTF-8 staged paths")
                .trim(),
            "outside.txt"
        );
        let remote_state = run_git(
            None,
            [
                "--git-dir",
                remote.to_str().expect("UTF-8 remote"),
                "show",
                "main:config/youta/state/progress.toml",
            ],
        );
        assert_eq!(
            String::from_utf8(remote_state.stdout)
                .expect("UTF-8 remote state")
                .trim(),
            "format_version = 1"
        );
    }

    #[test]
    fn push_failure_is_reported_after_the_commit_command() {
        let fixture = Fixture::new(Scenario::PushFailure);

        let error = fixture
            .sync(Duration::from_secs(2))
            .expect_err("push must fail");
        assert!(matches!(
            error,
            GitSyncError::CommandFailed {
                operation: "push Youta state",
                ..
            }
        ));
        let log = fixture.log();
        assert!(log.contains("\tcommit\t--only"));
        assert!(log.contains("\tpush\n"));
    }

    #[test]
    fn failed_push_is_retried_without_creating_another_commit() {
        let fixture = Fixture::new(Scenario::PushFailureThenRetry);

        let first_error = fixture
            .sync(Duration::from_secs(2))
            .expect_err("first push must fail");
        assert!(matches!(
            first_error,
            GitSyncError::CommandFailed {
                operation: "push Youta state",
                ..
            }
        ));
        assert_eq!(
            fixture.sync(Duration::from_secs(2)).expect("retry push"),
            GitSyncOutcome::Pushed
        );

        let log = fixture.log();
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("\tcommit\t"))
                .count(),
            1
        );
        assert_eq!(
            log.lines().filter(|line| line.contains("\tpush")).count(),
            2
        );
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("\tstatus\t--porcelain=v2"))
                .count(),
            1
        );
    }

    #[test]
    fn a_hung_push_is_killed_at_the_shared_deadline() {
        let fixture = Fixture::new(Scenario::PushTimeout);
        let started = Instant::now();

        let error = fixture
            .sync(Duration::from_millis(500))
            .expect_err("push timeout");

        assert!(matches!(
            error,
            GitSyncError::TimedOut {
                operation: "push Youta state"
            }
        ));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(fixture.log().contains("\tpush\n"));
    }

    #[test]
    fn an_exited_child_cannot_extend_the_deadline_via_inherited_pipes() {
        let fixture = Fixture::new(Scenario::ExitedChildWithOpenDescendantPipe);
        let started = Instant::now();

        let error = fixture
            .sync(Duration::from_millis(250))
            .expect_err("inherited pipe timeout");

        assert!(matches!(
            error,
            GitSyncError::TimedOut {
                operation: "discover the Git worktree"
            }
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    fn mock_script(scenario: Scenario, log: &Path, state: &Path) -> String {
        let log = shell_quote(log);
        let committed = shell_quote(&state.with_extension("committed"));
        let failed_once = shell_quote(&state.with_extension("failed-once"));
        let scenario = match scenario {
            Scenario::NotRepository => "not-repository",
            Scenario::DiscoveryFailure => "discovery-failure",
            Scenario::NoChanges => "no-changes",
            Scenario::Changes => "changes",
            Scenario::PushFailure => "push-failure",
            Scenario::PushFailureThenRetry => "push-failure-then-retry",
            Scenario::PushTimeout => "push-timeout",
            Scenario::ExitedChildWithOpenDescendantPipe => "open-descendant-pipe",
        };
        format!(
            r#"#!/bin/sh
set -eu
printf '%s' "$1" >> {log}
shift
for argument in "$@"; do
	printf '\t%s' "$argument" >> {log}
done
printf '\n' >> {log}

operation=''
previous=''
for argument in "$@"; do
	if [ "$previous" = '-C' ]; then
		previous=''
		continue
	fi
	case "$argument" in
		-C) previous='-C' ;;
		rev-parse|add|diff|status|commit|push) operation="$argument"; break ;;
	esac
done

case "$operation" in
	rev-parse)
		if [ '{scenario}' = 'not-repository' ]; then
			printf '%s\n' 'fatal: not a git repository (or any parent directory): .git' >&2
			exit 128
		fi
		if [ '{scenario}' = 'discovery-failure' ]; then
			printf '%s\n' 'fatal: fixture discovery failure' >&2
			exit 128
		fi
		if [ '{scenario}' = 'open-descendant-pipe' ]; then
			sleep 2 &
		fi
		printf '%s\n' 'true'
		;;
	diff)
		if [ '{scenario}' = 'no-changes' ]; then
			exit 0
		fi
		if [ '{scenario}' = 'push-failure-then-retry' ] && [ -e {committed} ]; then
			exit 0
		fi
		exit 1
		;;
	status)
		if [ '{scenario}' = 'push-failure-then-retry' ]; then
			printf '%s\n' '# branch.ab +1 -0'
		fi
		;;
	commit)
		if [ '{scenario}' = 'push-failure-then-retry' ]; then
			touch {committed}
		fi
		;;
	push)
		if [ '{scenario}' = 'push-failure' ]; then
			printf '%s\n' 'remote rejected fixture' >&2
			exit 1
		fi
		if [ '{scenario}' = 'push-failure-then-retry' ] && [ ! -e {failed_once} ]; then
			touch {failed_once}
			printf '%s\n' 'temporary remote failure' >&2
			exit 1
		fi
		if [ '{scenario}' = 'push-timeout' ]; then
			while :; do :; done
		fi
		;;
esac
"#
        )
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
    }

    fn run_git<const N: usize>(
        directory: Option<&Path>,
        arguments: [&str; N],
    ) -> std::process::Output {
        let mut command = Command::new("git");
        command.args(arguments);
        if let Some(directory) = directory {
            command.current_dir(directory);
        }
        let output = command.output().expect("run Git fixture command");
        assert!(
            output.status.success(),
            "Git fixture command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    #[test]
    fn fixture_paths_remain_alive_for_the_complete_test() {
        let fixture = Fixture::new(Scenario::NoChanges);
        assert!(fixture.temporary.path().exists());
        assert!(fixture.repository.exists());
    }
}
