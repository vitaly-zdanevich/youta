//! Side effects initiated from Youta's diagnostic error popup.
//!
//! This module deliberately separates command planning from execution. Reports
//! are sent through a child process's standard input, never a shell or command
//! argument. The browser fallback carries only a short instruction because a
//! complete diagnostic report can exceed browser and server URL limits.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::diagnostics::redact_diagnostic_text;

/// GitHub repository that receives Youta diagnostic reports.
pub const GITHUB_REPOSITORY: &str = "vitaly-zdanevich/youta";

/// Maximum number of Unicode scalar values in a pre-filled issue title.
pub const MAX_ISSUE_TITLE_CHARS: usize = 160;

const NEW_ISSUE_URL: &str = "https://github.com/vitaly-zdanevich/youta/issues/new";
const SHORT_ISSUE_BODY: &str =
    "Paste the complete diagnostic report copied by Youta below this line.";
/// Maximum time a synchronous popup action may occupy the terminal event loop.
///
/// Helpers that outlive this short window are handed to a background reaper and
/// are not reported as successful because no zero exit status was observed.
const PROCESS_OBSERVATION_TIME: Duration = Duration::from_millis(100);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// A command and its input, planned without starting a process.
///
/// Keeping standard input separate from arguments makes plans safe to inspect
/// in tests and prevents reports from appearing in process listings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandPlan {
    /// Resolved executable path.
    pub executable: PathBuf,
    /// Individual arguments passed directly to the executable.
    pub arguments: Vec<OsString>,
    /// Complete standard-input payload, when the command consumes one.
    pub standard_input: Option<Vec<u8>>,
    /// How long to observe the process for an immediate launch failure.
    pub observation_time: Duration,
}

/// Result of observing a launched report action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessOutcome {
    /// The process exited successfully during the observation period.
    ExitedSuccessfully,
    /// The process was still running when the observation period ended.
    ///
    /// Production execution has transferred the child to a background reaper,
    /// but callers must not interpret this outcome as confirmed success.
    StillRunning,
    /// The process exited unsuccessfully, with an optional platform exit code.
    ExitedUnsuccessfully(Option<i32>),
}

/// Injectable executor used by diagnostic report actions.
///
/// Production code uses [`SystemRunner`]. Tests can implement this trait to
/// assert exact executable paths, argument boundaries, standard input, action
/// ordering, and terminal output without launching a browser or network client.
pub trait ReportActionRunner {
    /// Executes one direct process plan without involving a shell.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the process cannot be started, its input
    /// cannot be written, or its status cannot be observed.
    fn execute(&self, plan: &CommandPlan) -> io::Result<ProcessOutcome>;

    /// Writes a complete OSC 52 clipboard escape to the controlling terminal.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the escape cannot be written or flushed.
    fn write_terminal_escape(&self, escape: &[u8]) -> io::Result<()>;
}

/// Direct operating-system process and terminal executor.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRunner;

impl ReportActionRunner for SystemRunner {
    fn execute(&self, plan: &CommandPlan) -> io::Result<ProcessOutcome> {
        let mut command = Command::new(&plan.executable);
        command
            .args(&plan.arguments)
            .stdin(if plan.standard_input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = command.spawn()?;
        if let Some(input) = &plan.standard_input {
            let write_result = child
                .stdin
                .take()
                .ok_or_else(|| io::Error::other("child standard input was not piped"))
                .and_then(|mut stdin| stdin.write_all(input));
            if let Err(error) = write_result {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        }

        observe_process(child, plan.observation_time)
    }

    fn write_terminal_escape(&self, escape: &[u8]) -> io::Result<()> {
        let stdout = io::stdout();
        let mut terminal = stdout.lock();
        terminal.write_all(escape)?;
        terminal.flush()
    }
}

fn observe_process(mut child: Child, observation_time: Duration) -> io::Result<ProcessOutcome> {
    let deadline = Instant::now() + observation_time;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(if status.success() {
                    ProcessOutcome::ExitedSuccessfully
                } else {
                    ProcessOutcome::ExitedUnsuccessfully(status.code())
                });
            }
            Ok(None) => {}
            Err(error) => {
                terminate_and_reap(&mut child);
                return Err(error);
            }
        }
        if Instant::now() >= deadline {
            hand_to_background_reaper(child)?;
            return Ok(ProcessOutcome::StillRunning);
        }
        thread::sleep(PROCESS_POLL_INTERVAL.min(observation_time));
    }
}

/// Transfers an unfinished helper to a detached waiter without losing ownership.
///
/// The channel is created before the thread, so every failure path still owns
/// the child and can terminate and reap it synchronously.
fn hand_to_background_reaper(mut child: Child) -> io::Result<()> {
    let (sender, receiver) = mpsc::sync_channel::<Child>(1);
    let reaper = match thread::Builder::new()
        .name("youta-report-helper-reaper".to_owned())
        .spawn(move || {
            if let Ok(mut child) = receiver.recv() {
                let _ = child.wait();
            }
        }) {
        Ok(reaper) => reaper,
        Err(error) => {
            terminate_and_reap(&mut child);
            return Err(error);
        }
    };

    if let Err(mpsc::SendError(mut child)) = sender.send(child) {
        terminate_and_reap(&mut child);
        let _ = reaper.join();
        return Err(io::Error::other(
            "report helper reaper stopped before accepting the child",
        ));
    }
    drop(reaper);
    Ok(())
}

/// Makes a best effort to stop and synchronously reap an owned helper.
fn terminate_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Supported native clipboard helpers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardHelper {
    /// Wayland's `wl-copy`.
    WlCopy(PathBuf),
    /// X11's `xclip`.
    Xclip(PathBuf),
    /// X11's `xsel`.
    Xsel(PathBuf),
    /// macOS's `pbcopy`.
    Pbcopy(PathBuf),
}

impl ClipboardHelper {
    fn label(&self) -> &'static str {
        match self {
            Self::WlCopy(_) => "wl-copy",
            Self::Xclip(_) => "xclip",
            Self::Xsel(_) => "xsel",
            Self::Pbcopy(_) => "pbcopy",
        }
    }

    fn executable(&self) -> &Path {
        match self {
            Self::WlCopy(path) | Self::Xclip(path) | Self::Xsel(path) | Self::Pbcopy(path) => path,
        }
    }

    fn arguments(&self) -> Vec<OsString> {
        match self {
            Self::WlCopy(_) | Self::Pbcopy(_) => Vec::new(),
            Self::Xclip(_) => ["-selection", "clipboard", "-in"]
                .into_iter()
                .map(OsString::from)
                .collect(),
            Self::Xsel(_) => ["--clipboard", "--input"]
                .into_iter()
                .map(OsString::from)
                .collect(),
        }
    }

    fn command_plan(&self, report: &str) -> CommandPlan {
        CommandPlan {
            executable: self.executable().to_owned(),
            arguments: self.arguments(),
            standard_input: Some(report.as_bytes().to_vec()),
            observation_time: PROCESS_OBSERVATION_TIME,
        }
    }
}

/// Executables discovered for diagnostic report actions.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReportActionTools {
    /// GitHub CLI executable, when present.
    pub github_cli: Option<PathBuf>,
    /// Clipboard helper appropriate for the active graphical session.
    pub clipboard: Option<ClipboardHelper>,
    /// `xdg-open` executable used by the browser fallback.
    pub xdg_open: Option<PathBuf>,
}

impl ReportActionTools {
    /// Discovers tools from the current process environment without running
    /// any candidate executable.
    ///
    /// Only absolute `PATH` entries are considered. This prevents an untrusted
    /// executable in the working directory from being selected through an
    /// empty or relative `PATH` component.
    #[must_use]
    pub fn discover() -> Self {
        let path = env::var_os("PATH").unwrap_or_default();
        let wayland = nonempty_environment_variable("WAYLAND_DISPLAY");
        let x11 = nonempty_environment_variable("DISPLAY");
        discover_tools(&path, wayland, x11, cfg!(target_os = "macos"))
    }
}

fn nonempty_environment_variable(name: &str) -> bool {
    env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn discover_tools(path: &OsStr, wayland: bool, x11: bool, macos: bool) -> ReportActionTools {
    let github_cli = find_executable(path, "gh");
    let xdg_open = find_executable(path, "xdg-open");
    let clipboard = if wayland {
        find_executable(path, "wl-copy").map(ClipboardHelper::WlCopy)
    } else {
        None
    }
    .or_else(|| {
        x11.then(|| find_executable(path, "xclip"))
            .flatten()
            .map(ClipboardHelper::Xclip)
    })
    .or_else(|| {
        x11.then(|| find_executable(path, "xsel"))
            .flatten()
            .map(ClipboardHelper::Xsel)
    })
    .or_else(|| {
        macos
            .then(|| find_executable(path, "pbcopy"))
            .flatten()
            .map(ClipboardHelper::Pbcopy)
    });

    ReportActionTools {
        github_cli,
        clipboard,
        xdg_open,
    }
}

fn find_executable(path: &OsStr, name: &str) -> Option<PathBuf> {
    env::split_paths(path)
        .filter(|directory| directory.is_absolute())
        .flat_map(|directory| executable_candidates(&directory, name))
        .find(|candidate| is_executable(candidate))
}

fn executable_candidates(directory: &Path, name: &str) -> Vec<PathBuf> {
    let direct = directory.join(name);
    #[cfg(windows)]
    {
        vec![direct, directory.join(format!("{name}.exe"))]
    }
    #[cfg(not(windows))]
    {
        vec![direct]
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Errors reported by diagnostic clipboard and issue actions.
#[derive(Debug)]
pub enum ReportActionError {
    /// The GitHub CLI was not discovered on a safe `PATH` entry.
    GitHubCliUnavailable,
    /// `xdg-open` was not discovered on a safe `PATH` entry.
    UrlOpenerUnavailable,
    /// A direct helper process could not be started or supplied with input.
    ProcessIo {
        /// Human-readable helper name.
        helper: &'static str,
        /// Underlying I/O error.
        source: io::Error,
    },
    /// A helper exited unsuccessfully during the observation period.
    ProcessFailed {
        /// Human-readable helper name.
        helper: &'static str,
        /// Platform exit code, when one was available.
        exit_code: Option<i32>,
    },
    /// A helper remained alive after the short foreground observation window.
    ProcessStillRunning {
        /// Human-readable helper name.
        helper: &'static str,
    },
    /// Neither the selected clipboard helper nor OSC 52 could copy the report.
    ClipboardFailed {
        /// Failure from the native clipboard helper, when one was attempted.
        helper_failure: Option<String>,
        /// Failure returned while writing the OSC 52 terminal sequence.
        osc52_failure: io::Error,
    },
    /// The report was copied, but the new-issue page could not be opened.
    OpenAfterCopy {
        /// Clipboard transport that succeeded.
        transport: String,
        /// Browser-helper failure.
        reason: String,
    },
}

impl fmt::Display for ReportActionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitHubCliUnavailable => {
                formatter.write_str("GitHub CLI was not found on an absolute PATH entry")
            }
            Self::UrlOpenerUnavailable => {
                formatter.write_str("xdg-open was not found on an absolute PATH entry")
            }
            Self::ProcessIo { helper, source } => {
                write!(formatter, "cannot run {helper}: {source}")
            }
            Self::ProcessFailed { helper, exit_code } => match exit_code {
                Some(code) => write!(formatter, "{helper} exited with status {code}"),
                None => write!(formatter, "{helper} terminated unsuccessfully"),
            },
            Self::ProcessStillRunning { helper } => write!(
                formatter,
                "{helper} did not report a successful exit promptly; \
                 it remains supervised in the background"
            ),
            Self::ClipboardFailed {
                helper_failure,
                osc52_failure,
            } => {
                if let Some(helper_failure) = helper_failure {
                    write!(
                        formatter,
                        "clipboard helper failed ({helper_failure}); \
                         OSC 52 fallback failed: {osc52_failure}"
                    )
                } else {
                    write!(
                        formatter,
                        "OSC 52 clipboard fallback failed: {osc52_failure}"
                    )
                }
            }
            Self::OpenAfterCopy { transport, reason } => write!(
                formatter,
                "report was copied with {transport}, but the issue page could not open: {reason}"
            ),
        }
    }
}

impl std::error::Error for ReportActionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ProcessIo { source, .. } => Some(source),
            Self::ClipboardFailed { osc52_failure, .. } => Some(osc52_failure),
            Self::GitHubCliUnavailable
            | Self::UrlOpenerUnavailable
            | Self::ProcessFailed { .. }
            | Self::ProcessStillRunning { .. }
            | Self::OpenAfterCopy { .. } => None,
        }
    }
}

/// Diagnostic report actions backed by an injectable executor.
#[derive(Clone, Debug)]
pub struct ReportActions<R> {
    runner: R,
    tools: ReportActionTools,
}

/// Diagnostic report actions that launch direct operating-system processes.
pub type SystemReportActions = ReportActions<SystemRunner>;

impl ReportActions<SystemRunner> {
    /// Discovers available helpers and creates the production action handler.
    ///
    /// Discovery checks filesystem metadata only; in particular, `gh` is not
    /// executed until the user explicitly activates the corresponding button.
    #[must_use]
    pub fn new() -> Self {
        Self {
            runner: SystemRunner,
            tools: ReportActionTools::discover(),
        }
    }
}

impl Default for ReportActions<SystemRunner> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: ReportActionRunner> ReportActions<R> {
    /// Creates an action handler with explicit tools and an injected executor.
    ///
    /// This constructor is intended for deterministic tests and embedding.
    #[must_use]
    pub fn with_runner(runner: R, tools: ReportActionTools) -> Self {
        Self { runner, tools }
    }

    /// Returns whether a GitHub CLI executable was found without running it.
    #[must_use]
    pub fn gh_available(&self) -> bool {
        self.tools.github_cli.is_some()
    }

    /// Copies the complete report using a native helper or OSC 52 fallback.
    ///
    /// The returned string names the transport that accepted the report. OSC
    /// 52 works in terminals that permit clipboard escape sequences; terminal
    /// multiplexers may require passthrough configuration.
    ///
    /// # Errors
    ///
    /// Returns an error only when the selected native helper fails and the
    /// terminal OSC 52 fallback cannot be written.
    pub fn copy_report(&self, report: &str) -> Result<String, ReportActionError> {
        let helper_failure = if let Some(helper) = &self.tools.clipboard {
            let plan = helper.command_plan(report);
            match self.execute_successfully(&plan, helper.label()) {
                Ok(()) => return Ok(helper.label().to_owned()),
                Err(error) => Some(error.to_string()),
            }
        } else {
            None
        };

        self.runner
            .write_terminal_escape(&osc52_sequence(report))
            .map_err(|osc52_failure| ReportActionError::ClipboardFailed {
                helper_failure,
                osc52_failure,
            })?;
        Ok("OSC 52".to_owned())
    }

    /// Opens a GitHub issue editor through `gh`, pre-filled but not submitted.
    ///
    /// The complete report is piped through standard input using
    /// `--body-file -`. `--web` delegates final review and submission to the
    /// user's browser.
    ///
    /// # Errors
    ///
    /// Returns an error when `gh` is unavailable, cannot launch, cannot accept
    /// its input, exits unsuccessfully, or does not confirm a successful exit
    /// within the short foreground observation window.
    pub fn fill_github_issue(&self, title: &str, report: &str) -> Result<(), ReportActionError> {
        let Some(github_cli) = &self.tools.github_cli else {
            return Err(ReportActionError::GitHubCliUnavailable);
        };
        let plan = github_cli_plan(github_cli, title, report);
        self.execute_successfully(&plan, "gh")
    }

    /// Copies the report and opens a short pre-filled GitHub issue page.
    ///
    /// Both buttons remain useful when `gh` is installed: this browser-only
    /// action always copies the complete report first, then opens a URL that
    /// contains only a bounded title and a short paste instruction.
    ///
    /// # Errors
    ///
    /// Returns an error when copying fails, `xdg-open` is unavailable, or the
    /// browser helper does not promptly confirm a successful exit. An opener
    /// error states which clipboard transport already succeeded.
    pub fn copy_and_open_github_issue(
        &self,
        title: &str,
        report: &str,
    ) -> Result<String, ReportActionError> {
        let transport = self.copy_report(report)?;
        let opener_result = self.open_issue_page(title);
        if let Err(error) = opener_result {
            return Err(ReportActionError::OpenAfterCopy {
                transport,
                reason: error.to_string(),
            });
        }
        Ok(transport)
    }

    fn open_issue_page(&self, title: &str) -> Result<(), ReportActionError> {
        let Some(xdg_open) = &self.tools.xdg_open else {
            return Err(ReportActionError::UrlOpenerUnavailable);
        };
        let plan = issue_page_plan(xdg_open, title);
        self.execute_successfully(&plan, "xdg-open")
    }

    fn execute_successfully(
        &self,
        plan: &CommandPlan,
        helper: &'static str,
    ) -> Result<(), ReportActionError> {
        match self
            .runner
            .execute(plan)
            .map_err(|source| ReportActionError::ProcessIo { helper, source })?
        {
            ProcessOutcome::ExitedSuccessfully => Ok(()),
            ProcessOutcome::ExitedUnsuccessfully(exit_code) => {
                Err(ReportActionError::ProcessFailed { helper, exit_code })
            }
            ProcessOutcome::StillRunning => Err(ReportActionError::ProcessStillRunning { helper }),
        }
    }
}

fn github_cli_plan(executable: &Path, title: &str, report: &str) -> CommandPlan {
    CommandPlan {
        executable: executable.to_owned(),
        arguments: vec![
            "issue".into(),
            "create".into(),
            "--web".into(),
            "--repo".into(),
            GITHUB_REPOSITORY.into(),
            "--title".into(),
            bounded_issue_title(title).into(),
            "--body-file".into(),
            "-".into(),
        ],
        standard_input: Some(report.as_bytes().to_vec()),
        observation_time: PROCESS_OBSERVATION_TIME,
    }
}

fn issue_page_plan(executable: &Path, title: &str) -> CommandPlan {
    CommandPlan {
        executable: executable.to_owned(),
        // Unlike many command-line tools, `xdg-open` does not accept `--` as
        // an end-of-options marker. The bounded URL always starts with HTTPS,
        // so it is safe and valid as the command's sole target argument.
        arguments: vec![short_issue_url(title).into()],
        standard_input: None,
        observation_time: PROCESS_OBSERVATION_TIME,
    }
}

fn bounded_issue_title(title: &str) -> String {
    let redacted = redact_diagnostic_text(title);
    let flattened = redacted.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = if flattened.is_empty() {
        "Youta diagnostic report"
    } else {
        flattened.as_str()
    };
    let mut chars = title.chars();
    let bounded = chars
        .by_ref()
        .take(MAX_ISSUE_TITLE_CHARS)
        .collect::<String>();
    if chars.next().is_none() {
        return bounded;
    }

    let mut shortened = bounded
        .chars()
        .take(MAX_ISSUE_TITLE_CHARS.saturating_sub(1))
        .collect::<String>();
    shortened.push('…');
    shortened
}

fn short_issue_url(title: &str) -> String {
    format!(
        "{NEW_ISSUE_URL}?title={}&body={}",
        percent_encode_query_value(&bounded_issue_title(title)),
        percent_encode_query_value(SHORT_ISSUE_BODY)
    )
}

fn percent_encode_query_value(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            output.push(char::from(byte));
        } else {
            output.push('%');
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    output
}

fn osc52_sequence(report: &str) -> Vec<u8> {
    let encoded = base64_encode(report.as_bytes());
    let mut sequence = Vec::with_capacity(encoded.len() + 8);
    sequence.extend_from_slice(b"\x1b]52;c;");
    sequence.extend_from_slice(encoded.as_bytes());
    sequence.push(0x07);
    sequence
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    let mut chunks = input.chunks_exact(3);
    for chunk in &mut chunks {
        let first = chunk[0];
        let second = chunk[1];
        let third = chunk[2];
        output.push(char::from(TABLE[usize::from(first >> 2)]));
        output.push(char::from(
            TABLE[usize::from(((first & 0x03) << 4) | (second >> 4))],
        ));
        output.push(char::from(
            TABLE[usize::from(((second & 0x0f) << 2) | (third >> 6))],
        ));
        output.push(char::from(TABLE[usize::from(third & 0x3f)]));
    }

    match chunks.remainder() {
        [] => {}
        [first] => {
            output.push(char::from(TABLE[usize::from(first >> 2)]));
            output.push(char::from(TABLE[usize::from((first & 0x03) << 4)]));
            output.push_str("==");
        }
        [first, second] => {
            output.push(char::from(TABLE[usize::from(first >> 2)]));
            output.push(char::from(
                TABLE[usize::from(((first & 0x03) << 4) | (second >> 4))],
            ));
            output.push(char::from(TABLE[usize::from((second & 0x0f) << 2)]));
            output.push('=');
        }
        _ => unreachable!("chunks_exact(3) leaves fewer than three bytes"),
    }
    output
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::fs;

    use super::*;

    #[derive(Debug, Default)]
    struct MockRunner {
        plans: RefCell<Vec<CommandPlan>>,
        outcomes: RefCell<VecDeque<io::Result<ProcessOutcome>>>,
        terminal_escapes: RefCell<Vec<Vec<u8>>>,
        terminal_error: RefCell<Option<io::Error>>,
    }

    impl MockRunner {
        fn with_outcomes(outcomes: impl IntoIterator<Item = ProcessOutcome>) -> Self {
            Self {
                outcomes: RefCell::new(outcomes.into_iter().map(Ok).collect()),
                ..Self::default()
            }
        }
    }

    impl ReportActionRunner for MockRunner {
        fn execute(&self, plan: &CommandPlan) -> io::Result<ProcessOutcome> {
            self.plans.borrow_mut().push(plan.clone());
            self.outcomes
                .borrow_mut()
                .pop_front()
                .unwrap_or(Ok(ProcessOutcome::ExitedSuccessfully))
        }

        fn write_terminal_escape(&self, escape: &[u8]) -> io::Result<()> {
            if let Some(error) = self.terminal_error.borrow_mut().take() {
                return Err(error);
            }
            self.terminal_escapes.borrow_mut().push(escape.to_vec());
            Ok(())
        }
    }

    fn tools() -> ReportActionTools {
        ReportActionTools {
            github_cli: Some(PathBuf::from("/usr/bin/gh")),
            clipboard: Some(ClipboardHelper::WlCopy(PathBuf::from("/usr/bin/wl-copy"))),
            xdg_open: Some(PathBuf::from("/usr/bin/xdg-open")),
        }
    }

    #[test]
    fn github_cli_plan_has_exact_arguments_and_full_report_on_stdin() {
        let runner = MockRunner::default();
        let actions = ReportActions::with_runner(runner, tools());
        let report = "line one\nline two\nfull backtrace";

        actions
            .fill_github_issue("Playback failed", report)
            .expect("plan should be accepted");

        let plans = actions.runner.plans.borrow();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].executable, Path::new("/usr/bin/gh"));
        assert_eq!(
            plans[0].arguments,
            [
                "issue",
                "create",
                "--web",
                "--repo",
                GITHUB_REPOSITORY,
                "--title",
                "Playback failed",
                "--body-file",
                "-"
            ]
            .map(OsString::from)
        );
        assert_eq!(plans[0].standard_input.as_deref(), Some(report.as_bytes()));
    }

    #[test]
    fn every_clipboard_helper_has_explicit_non_shell_arguments() {
        let report = "report";
        let cases = [
            (
                ClipboardHelper::WlCopy("/bin/wl-copy".into()),
                Vec::<OsString>::new(),
            ),
            (
                ClipboardHelper::Xclip("/bin/xclip".into()),
                ["-selection", "clipboard", "-in"]
                    .map(OsString::from)
                    .to_vec(),
            ),
            (
                ClipboardHelper::Xsel("/bin/xsel".into()),
                ["--clipboard", "--input"].map(OsString::from).to_vec(),
            ),
            (
                ClipboardHelper::Pbcopy("/bin/pbcopy".into()),
                Vec::<OsString>::new(),
            ),
        ];

        for (helper, expected_arguments) in cases {
            let plan = helper.command_plan(report);
            assert_eq!(plan.arguments, expected_arguments);
            assert_eq!(plan.standard_input.as_deref(), Some(report.as_bytes()));
        }
    }

    #[test]
    fn github_cli_plan_keeps_shell_metacharacters_inside_one_argument_and_stdin() {
        let runner = MockRunner::default();
        let actions = ReportActions::with_runner(runner, tools());
        let report = "$(touch /tmp/not-run); `id`";

        actions
            .fill_github_issue("Failure; rm -rf something", report)
            .expect("mock command should succeed");

        let plans = actions.runner.plans.borrow();
        assert_eq!(plans[0].executable, Path::new("/usr/bin/gh"));
        assert_eq!(plans[0].arguments[6], "Failure; rm -rf something");
        assert_eq!(plans[0].standard_input.as_deref(), Some(report.as_bytes()));
        assert!(!plans[0].arguments.iter().any(|argument| argument == "-c"));
    }

    #[test]
    fn title_is_redacted_flattened_and_unicode_bounded() {
        let long_title = format!(
            "Playback\nfailed token=secret {}",
            "🎵".repeat(MAX_ISSUE_TITLE_CHARS)
        );
        let title = bounded_issue_title(&long_title);

        assert!(!title.contains('\n'));
        assert!(!title.contains("secret"));
        assert!(title.contains("<redacted>"));
        assert_eq!(title.chars().count(), MAX_ISSUE_TITLE_CHARS);
        assert!(title.ends_with('…'));
    }

    #[test]
    fn empty_title_gets_safe_default() {
        assert_eq!(bounded_issue_title(" \n\t"), "Youta diagnostic report");
    }

    #[test]
    fn copy_uses_wayland_helper_with_complete_stdin() {
        let runner = MockRunner::default();
        let actions = ReportActions::with_runner(runner, tools());
        let report = "complete report\0including unusual bytes";

        let transport = actions.copy_report(report).expect("copy should succeed");

        assert_eq!(transport, "wl-copy");
        let plans = actions.runner.plans.borrow();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].executable, Path::new("/usr/bin/wl-copy"));
        assert!(plans[0].arguments.is_empty());
        assert_eq!(plans[0].standard_input.as_deref(), Some(report.as_bytes()));
        assert!(actions.runner.terminal_escapes.borrow().is_empty());
    }

    #[test]
    fn native_clipboard_failure_falls_back_to_complete_osc52() {
        let runner = MockRunner::with_outcomes([ProcessOutcome::ExitedUnsuccessfully(Some(1))]);
        let actions = ReportActions::with_runner(runner, tools());
        let report = "complete diagnostic\nwith backtrace";

        let transport = actions.copy_report(report).expect("OSC 52 should work");

        assert_eq!(transport, "OSC 52");
        assert_eq!(
            actions.runner.terminal_escapes.borrow().as_slice(),
            [osc52_sequence(report)]
        );
    }

    #[test]
    fn unconfirmed_native_clipboard_falls_back_to_complete_osc52() {
        let runner = MockRunner::with_outcomes([ProcessOutcome::StillRunning]);
        let actions = ReportActions::with_runner(runner, tools());
        let report = "complete diagnostic\nwith backtrace";

        let transport = actions.copy_report(report).expect("OSC 52 should work");

        assert_eq!(transport, "OSC 52");
        assert_eq!(
            actions.runner.terminal_escapes.borrow().as_slice(),
            [osc52_sequence(report)]
        );
    }

    #[test]
    fn osc52_is_used_when_no_native_clipboard_matches() {
        let runner = MockRunner::default();
        let actions = ReportActions::with_runner(
            runner,
            ReportActionTools {
                github_cli: None,
                clipboard: None,
                xdg_open: None,
            },
        );

        assert_eq!(
            actions.copy_report("Youta").expect("OSC 52 should work"),
            "OSC 52"
        );
        assert_eq!(
            actions.runner.terminal_escapes.borrow().as_slice(),
            [b"\x1b]52;c;WW91dGE=\x07".to_vec()]
        );
    }

    #[test]
    fn both_clipboard_failures_are_preserved() {
        let runner = MockRunner::with_outcomes([ProcessOutcome::ExitedUnsuccessfully(Some(9))]);
        *runner.terminal_error.borrow_mut() =
            Some(io::Error::new(io::ErrorKind::BrokenPipe, "terminal closed"));
        let actions = ReportActions::with_runner(runner, tools());

        let error = actions
            .copy_report("report")
            .expect_err("both transports should fail");
        let message = error.to_string();
        assert!(message.contains("wl-copy exited with status 9"));
        assert!(message.contains("terminal closed"));
    }

    #[test]
    fn copy_and_open_runs_both_buttons_action_in_order() {
        let runner = MockRunner::default();
        let actions = ReportActions::with_runner(runner, tools());
        let report = "a long report that must never enter the URL";

        let transport = actions
            .copy_and_open_github_issue("Decoder failed", report)
            .expect("copy and open should succeed");

        assert_eq!(transport, "wl-copy");
        let plans = actions.runner.plans.borrow();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].executable, Path::new("/usr/bin/wl-copy"));
        assert_eq!(plans[1].executable, Path::new("/usr/bin/xdg-open"));
        assert_eq!(plans[1].arguments.len(), 1);
        let url = plans[1].arguments[0].to_string_lossy();
        assert!(url.starts_with(NEW_ISSUE_URL));
        assert!(url.contains("title=Decoder%20failed"));
        assert!(url.contains("body=Paste%20the%20complete"));
        assert!(!url.contains("long%20report"));
        assert!(url.len() < 512);
        assert!(plans[1].standard_input.is_none());
    }

    #[test]
    fn copy_and_open_reports_that_copy_already_succeeded() {
        let runner = MockRunner::with_outcomes([
            ProcessOutcome::ExitedSuccessfully,
            ProcessOutcome::ExitedUnsuccessfully(Some(3)),
        ]);
        let actions = ReportActions::with_runner(runner, tools());

        let error = actions
            .copy_and_open_github_issue("Failure", "report")
            .expect_err("opener should fail");

        let plans = actions.runner.plans.borrow();
        assert_eq!(plans[1].arguments.len(), 1);
        assert!(
            plans[1].arguments[0]
                .to_string_lossy()
                .starts_with(NEW_ISSUE_URL)
        );
        assert!(error.to_string().contains("report was copied with wl-copy"));
        assert!(error.to_string().contains("xdg-open exited with status 3"));
    }

    #[test]
    fn copy_and_open_does_not_claim_success_for_a_running_opener() {
        let runner = MockRunner::with_outcomes([
            ProcessOutcome::ExitedSuccessfully,
            ProcessOutcome::StillRunning,
        ]);
        let actions = ReportActions::with_runner(runner, tools());

        let error = actions
            .copy_and_open_github_issue("Failure", "private report")
            .expect_err("a running opener has not confirmed success");
        let message = error.to_string();

        assert!(message.contains("report was copied with wl-copy"));
        assert!(message.contains("xdg-open did not report a successful exit promptly"));
        assert!(!message.contains("private report"));
        assert!(!message.contains(NEW_ISSUE_URL));
    }

    #[test]
    fn fill_issue_requires_discovered_github_cli() {
        let runner = MockRunner::default();
        let actions = ReportActions::with_runner(
            runner,
            ReportActionTools {
                github_cli: None,
                ..tools()
            },
        );

        let error = actions
            .fill_github_issue("Failure", "report")
            .expect_err("gh should be required");
        assert!(matches!(error, ReportActionError::GitHubCliUnavailable));
        assert!(actions.runner.plans.borrow().is_empty());
    }

    #[test]
    fn running_helper_is_not_reported_as_a_successful_launch() {
        let runner = MockRunner::with_outcomes([ProcessOutcome::StillRunning]);
        let actions = ReportActions::with_runner(runner, tools());

        let error = actions
            .fill_github_issue("Failure", "report")
            .expect_err("a running process has not confirmed success");

        assert!(matches!(
            error,
            ReportActionError::ProcessStillRunning { helper: "gh" }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn system_runner_executes_direct_processes_and_pipes_stdin() {
        let success = find_test_executable(&["/usr/bin/true", "/bin/true"]);
        let failure = find_test_executable(&["/usr/bin/false", "/bin/false"]);
        let input_sink = find_test_executable(&["/usr/bin/cat", "/bin/cat"]);
        let runner = SystemRunner;

        let success_plan = CommandPlan {
            executable: success,
            arguments: Vec::new(),
            standard_input: None,
            observation_time: Duration::from_secs(1),
        };
        assert_eq!(
            runner.execute(&success_plan).expect("run true"),
            ProcessOutcome::ExitedSuccessfully
        );

        let failure_plan = CommandPlan {
            executable: failure,
            arguments: Vec::new(),
            standard_input: None,
            observation_time: Duration::from_secs(1),
        };
        assert!(matches!(
            runner.execute(&failure_plan).expect("run false"),
            ProcessOutcome::ExitedUnsuccessfully(Some(1))
        ));

        let input_plan = CommandPlan {
            executable: input_sink,
            arguments: Vec::new(),
            standard_input: Some(b"complete report through stdin".to_vec()),
            observation_time: Duration::from_secs(1),
        };
        assert_eq!(
            runner.execute(&input_plan).expect("pipe report to cat"),
            ProcessOutcome::ExitedSuccessfully
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn system_runner_returns_promptly_and_reaps_an_unfinished_helper() {
        let temporary = tempfile::tempdir().expect("temporary command directory");
        let helper = temporary.path().join("slow-helper");
        let pid_file = temporary.path().join("pid");
        let shell = find_test_executable(&["/bin/sh", "/usr/bin/sh"]);
        let sleep = find_test_executable(&["/usr/bin/sleep", "/bin/sleep"]);
        fs::write(
            &helper,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$1\"\n\"$2\" 0.2\n",
        )
        .expect("slow helper fixture");
        // Execute the fixture through a stable interpreter. Overwriting an
        // executable fixture and launching it immediately can race with CI
        // filesystem or security tooling and transiently return ETXTBSY.
        let plan = CommandPlan {
            executable: shell,
            arguments: vec![
                helper.into_os_string(),
                pid_file.as_os_str().to_owned(),
                sleep.into_os_string(),
            ],
            standard_input: None,
            observation_time: Duration::from_millis(20),
        };

        let started_at = Instant::now();
        let outcome = SystemRunner.execute(&plan).expect("run slow helper");
        let foreground_elapsed = started_at.elapsed();

        assert_eq!(outcome, ProcessOutcome::StillRunning);
        assert!(
            foreground_elapsed < Duration::from_millis(500),
            "foreground observation took {foreground_elapsed:?}"
        );
        let pid = wait_for_test_pid(&pid_file, Duration::from_secs(1));
        let process_path = PathBuf::from(format!("/proc/{pid}"));
        let reaping_deadline = Instant::now() + Duration::from_secs(2);
        while process_path.exists() && Instant::now() < reaping_deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !process_path.exists(),
            "background waiter did not reap helper PID {pid}"
        );
    }

    #[test]
    fn system_runner_reports_a_missing_executable_without_a_shell() {
        let directory = tempfile::tempdir().expect("temporary command directory");
        let plan = CommandPlan {
            executable: directory.path().join("does-not-exist"),
            arguments: vec!["argument".into()],
            standard_input: Some(b"report".to_vec()),
            observation_time: Duration::from_millis(1),
        };

        let error = SystemRunner
            .execute(&plan)
            .expect_err("missing executable should fail");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn helper_selection_respects_session_and_priority() {
        let directory = tempfile::tempdir().expect("temporary PATH");
        for name in ["gh", "xdg-open", "wl-copy", "xclip", "xsel", "pbcopy"] {
            make_executable(&directory.path().join(name));
        }
        let path = env::join_paths([directory.path()]).expect("valid PATH");

        let wayland = discover_tools(&path, true, true, false);
        assert!(matches!(
            wayland.clipboard,
            Some(ClipboardHelper::WlCopy(_))
        ));
        let x11 = discover_tools(&path, false, true, false);
        assert!(matches!(x11.clipboard, Some(ClipboardHelper::Xclip(_))));
        let macos = discover_tools(&path, false, false, true);
        assert!(matches!(macos.clipboard, Some(ClipboardHelper::Pbcopy(_))));
        assert!(wayland.github_cli.is_some());
        assert!(wayland.xdg_open.is_some());
    }

    #[test]
    fn xsel_is_used_when_xclip_is_absent() {
        let directory = tempfile::tempdir().expect("temporary PATH");
        make_executable(&directory.path().join("xsel"));
        let path = env::join_paths([directory.path()]).expect("valid PATH");

        let tools = discover_tools(&path, false, true, false);

        assert!(matches!(tools.clipboard, Some(ClipboardHelper::Xsel(_))));
    }

    #[test]
    fn discovery_ignores_relative_path_entries_and_does_not_execute_gh() {
        let directory = tempfile::tempdir().expect("temporary PATH");
        let gh = directory.path().join("gh");
        let sentinel = directory.path().join("executed");
        make_executable(&gh);
        fs::write(&gh, format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()))
            .expect("write fake gh");
        let path = env::join_paths([Path::new("."), directory.path()]).expect("valid PATH");

        let tools = discover_tools(&path, false, false, false);

        assert_eq!(tools.github_cli.as_deref(), Some(gh.as_path()));
        assert!(!sentinel.exists(), "discovery must not execute gh");
    }

    #[test]
    fn base64_encoder_covers_all_remainder_lengths_and_utf8() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode("🎵".as_bytes()), "8J+OtQ==");
    }

    #[cfg(unix)]
    fn find_test_executable(candidates: &[&str]) -> PathBuf {
        candidates
            .iter()
            .map(PathBuf::from)
            .find(|candidate| is_executable(candidate))
            .expect("standard test executable should exist")
    }

    /// Waits for the reaper fixture to publish its process ID.
    ///
    /// A short observation window can return control before the operating
    /// system has scheduled the helper far enough to create its PID file.
    #[cfg(target_os = "linux")]
    fn wait_for_test_pid(path: &Path, timeout: Duration) -> u32 {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(contents) = fs::read_to_string(path) {
                if let Ok(pid) = contents.trim().parse::<u32>() {
                    return pid;
                }
            }
            assert!(
                Instant::now() < deadline,
                "helper did not publish a numeric PID within {timeout:?}: {:?}",
                fs::read_to_string(path)
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;

        fs::write(path, b"not executed").expect("write executable candidate");
        let mut permissions = fs::metadata(path)
            .expect("candidate metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).expect("mark candidate executable");
    }

    #[cfg(not(unix))]
    fn make_executable(path: &Path) {
        fs::write(path, b"not executed").expect("write executable candidate");
    }
}
