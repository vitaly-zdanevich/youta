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

/// Browser URL for Youta's GitHub issue list.
pub const GITHUB_ISSUES_URL: &str = "https://github.com/vitaly-zdanevich/youta/issues";

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
/// Maximum grace period for pipe workers after the direct child terminates.
///
/// A descendant can inherit a pipe and keep it open indefinitely. Completion
/// therefore uses messages received during this bounded grace period and
/// detaches any worker still waiting for EOF when the deadline expires.
const PROCESS_IO_DRAIN_GRACE: Duration = Duration::from_millis(500);
/// Maximum time allowed for `gh` to submit a diagnostic issue.
///
/// Unlike browser and clipboard helpers, a timed-out submission is terminated
/// and reaped instead of being detached. Its remote outcome remains unknown
/// because GitHub may have accepted the request before the response was lost.
const GITHUB_SUBMISSION_TIMEOUT: Duration = Duration::from_secs(30);
/// Maximum stdout and stderr retained from a completion-required helper.
///
/// Reader threads continue draining data after this limit so a noisy child
/// cannot deadlock on a full pipe or grow Youta's memory without bound.
pub const MAX_CAPTURED_HELPER_OUTPUT_BYTES: usize = 16 * 1024;
/// Maximum terminal-safe helper detail included in a user-facing error.
const MAX_DISPLAYED_HELPER_DETAIL_BYTES: usize = 512;

/// Returns the native URL-opening command for the compiled operating system.
///
/// Linux and other freedesktop-oriented Unix systems use `xdg-open`; macOS
/// ships `/usr/bin/open` instead.
#[must_use]
pub(crate) const fn system_url_opener_name() -> &'static str {
    url_opener_name_for_platform(cfg!(target_os = "macos"))
}

const fn url_opener_name_for_platform(macos: bool) -> &'static str {
    if macos { "open" } else { "xdg-open" }
}

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

/// One bounded output stream captured from a completed helper process.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapturedOutput {
    /// UTF-8 text retained from the beginning of the stream.
    pub text: String,
    /// Whether the complete stream was not retained because it exceeded the
    /// byte limit or a descendant prevented the reader from observing EOF.
    pub truncated: bool,
}

impl CapturedOutput {
    fn display_text(&self) -> Option<String> {
        let text = self.text.trim();
        if text.is_empty() {
            return self
                .truncated
                .then(|| "helper output exceeded the capture limit".to_owned());
        }
        let safe = escape_terminal_controls(&redact_diagnostic_text(text));
        Some(bounded_helper_detail(&safe, self.truncated))
    }
}

/// Escapes terminal instructions while retaining readable multiline text.
///
/// Newlines remain structural, CRLF is normalized, tabs become spaces, and
/// every other Unicode control character is rendered through its Rust escape.
/// In particular, an ANSI `ESC` byte becomes the inert text `\\u{1b}`.
fn escape_terminal_controls(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                output.push('\n');
            }
            '\n' => output.push('\n'),
            '\t' => output.push(' '),
            character if character.is_control() => output.extend(character.escape_default()),
            character => output.push(character),
        }
    }
    output
}

/// Bounds escaped helper text while reserving room for truthful suffixes.
fn bounded_helper_detail(input: &str, capture_truncated: bool) -> String {
    const CAPTURE_SUFFIX: &str = "\n… helper output truncated";
    const DISPLAY_SUFFIX: &str = "\n… helper detail truncated";

    let capture_suffix = if capture_truncated {
        CAPTURE_SUFFIX
    } else {
        ""
    };
    let display_truncated =
        input.len().saturating_add(capture_suffix.len()) > MAX_DISPLAYED_HELPER_DETAIL_BYTES;
    let display_suffix = if display_truncated {
        DISPLAY_SUFFIX
    } else {
        ""
    };
    let content_limit = MAX_DISPLAYED_HELPER_DETAIL_BYTES
        .saturating_sub(capture_suffix.len())
        .saturating_sub(display_suffix.len());
    let mut end = input.len().min(content_limit);
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    let mut output = String::with_capacity(
        end.saturating_add(capture_suffix.len())
            .saturating_add(display_suffix.len()),
    );
    output.push_str(&input[..end]);
    output.push_str(display_suffix);
    output.push_str(capture_suffix);
    output
}

/// Terminal state of a helper that must finish before its result is known.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompletedProcessStatus {
    /// The helper exited with a successful status.
    ExitedSuccessfully,
    /// The helper exited unsuccessfully, optionally exposing its status code.
    ExitedUnsuccessfully(Option<i32>),
    /// The helper exceeded its plan's observation time and was terminated.
    ///
    /// Terminating the local process cannot prove that a remote side effect
    /// did not complete before the response was lost.
    TimedOut,
    /// The helper started, but its final state could not be established.
    ///
    /// A caller initiating a remote side effect must treat this as an
    /// indeterminate outcome rather than offering an ordinary retry.
    OutcomeUnknown {
        /// Bounded local explanation of why completion could not be proven.
        reason: String,
    },
}

/// Bounded output and terminal state from a completion-required helper.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedProcess {
    /// How the helper completed.
    pub status: CompletedProcessStatus,
    /// Bounded standard output.
    pub stdout: CapturedOutput,
    /// Bounded standard error.
    pub stderr: CapturedOutput,
}

impl CompletedProcess {
    fn useful_detail(&self) -> Option<String> {
        self.stderr
            .display_text()
            .or_else(|| self.stdout.display_text())
    }
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

    /// Executes a plan to a definite result while capturing bounded output.
    ///
    /// A process that outlives [`CommandPlan::observation_time`] is terminated
    /// and reaped. Implementations must keep draining stdout and stderr after
    /// [`MAX_CAPTURED_HELPER_OUTPUT_BYTES`] so a noisy child cannot block.
    ///
    /// # Errors
    ///
    /// Returns an I/O error only when the process could not be started. Once a
    /// helper starts, implementations must return
    /// [`CompletedProcessStatus::OutcomeUnknown`] for local observation or I/O
    /// failures so callers cannot mistake a possibly completed remote action
    /// for a safe-to-retry pre-launch failure.
    fn execute_to_completion(&self, plan: &CommandPlan) -> io::Result<CompletedProcess>;

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
        crate::child_process::quiet(&mut command);

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

    fn execute_to_completion(&self, plan: &CommandPlan) -> io::Result<CompletedProcess> {
        execute_process_to_completion(plan)
    }

    fn write_terminal_escape(&self, escape: &[u8]) -> io::Result<()> {
        let stdout = io::stdout();
        let mut terminal = stdout.lock();
        terminal.write_all(escape)?;
        terminal.flush()
    }
}

fn execute_process_to_completion(plan: &CommandPlan) -> io::Result<CompletedProcess> {
    let mut command = Command::new(&plan.executable);
    command
        .args(&plan.arguments)
        .stdin(if plan.standard_input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::child_process::quiet(&mut command);

    let mut child = command.spawn()?;
    let Some(stdout) = child.stdout.take() else {
        return Ok(completion_setup_failed(
            &mut child,
            "child standard output was not piped",
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        return Ok(completion_setup_failed(
            &mut child,
            "child standard error was not piped",
        ));
    };
    let stdin = if plan.standard_input.is_some() {
        let Some(stdin) = child.stdin.take() else {
            return Ok(completion_setup_failed(
                &mut child,
                "child standard input was not piped",
            ));
        };
        Some(stdin)
    } else {
        None
    };
    let (events, completion_events) = mpsc::channel();
    if let Err(error) = spawn_output_worker(
        stdout,
        CompletionWorker::Stdout,
        "standard-output reader",
        events.clone(),
    ) {
        return Ok(completion_setup_failed(
            &mut child,
            &format!("cannot start helper standard-output reader: {error}"),
        ));
    }
    if let Err(error) = spawn_output_worker(
        stderr,
        CompletionWorker::Stderr,
        "standard-error reader",
        events.clone(),
    ) {
        return Ok(completion_setup_failed(
            &mut child,
            &format!("cannot start helper standard-error reader: {error}"),
        ));
    }
    let stdin_expected = stdin.is_some();
    if let Some(stdin) = stdin
        && let Err(error) = spawn_input_worker(
            stdin,
            plan.standard_input.clone().unwrap_or_default(),
            events.clone(),
        )
    {
        return Ok(completion_setup_failed(
            &mut child,
            &format!("cannot start helper standard-input writer: {error}"),
        ));
    }
    drop(events);

    let deadline = Instant::now() + plan.observation_time;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break if status.success() {
                    CompletedProcessStatus::ExitedSuccessfully
                } else {
                    CompletedProcessStatus::ExitedUnsuccessfully(status.code())
                };
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(PROCESS_POLL_INTERVAL.min(plan.observation_time));
            }
            Ok(None) => {
                terminate_and_reap(&mut child);
                break CompletedProcessStatus::TimedOut;
            }
            Err(error) => {
                terminate_and_reap(&mut child);
                break CompletedProcessStatus::OutcomeUnknown {
                    reason: safe_local_failure(&format!(
                        "cannot observe gh after it started: {error}"
                    )),
                };
            }
        }
    };

    let io_deadline = Instant::now() + PROCESS_IO_DRAIN_GRACE;
    let captured = collect_completion_events(completion_events, stdin_expected, io_deadline);
    let status = if matches!(&status, CompletedProcessStatus::ExitedSuccessfully) {
        captured.stdin_failure_reason().map_or(status, |reason| {
            CompletedProcessStatus::OutcomeUnknown { reason }
        })
    } else {
        status
    };
    Ok(CompletedProcess {
        status,
        stdout: captured.stdout,
        stderr: captured.stderr,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompletionWorker {
    Stdin,
    Stdout,
    Stderr,
}

impl CompletionWorker {
    const fn index(self) -> usize {
        match self {
            Self::Stdin => 0,
            Self::Stdout => 1,
            Self::Stderr => 2,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Stdin => "standard-input writer",
            Self::Stdout => "standard-output reader",
            Self::Stderr => "standard-error reader",
        }
    }
}

enum CompletionEvent {
    Output {
        worker: CompletionWorker,
        bytes: Vec<u8>,
    },
    OutputTruncated(CompletionWorker),
    Finished {
        worker: CompletionWorker,
        result: io::Result<()>,
    },
}

#[derive(Debug, Default)]
struct CapturedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

impl CapturedBytes {
    fn push(&mut self, bytes: &[u8]) {
        let remaining = MAX_CAPTURED_HELPER_OUTPUT_BYTES.saturating_sub(self.bytes.len());
        let retained = remaining.min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..retained]);
        self.truncated |= retained < bytes.len();
    }

    fn finish(self) -> CapturedOutput {
        CapturedOutput {
            text: String::from_utf8_lossy(&self.bytes).into_owned(),
            truncated: self.truncated,
        }
    }
}

struct CompletionCapture {
    stdout: CapturedOutput,
    stderr: CapturedOutput,
    finished: [bool; 3],
    failures: Vec<(CompletionWorker, String)>,
    stdin_expected: bool,
}

impl CompletionCapture {
    fn stdin_failure_reason(&self) -> Option<String> {
        if !self.stdin_expected {
            return None;
        }
        let mut failures = self
            .failures
            .iter()
            .filter(|(worker, _)| *worker == CompletionWorker::Stdin)
            .map(|(_, failure)| failure.clone())
            .collect::<Vec<_>>();
        if !self.finished[CompletionWorker::Stdin.index()] {
            failures.push(format!(
                "{} did not finish within {} ms after gh exited",
                CompletionWorker::Stdin.label(),
                PROCESS_IO_DRAIN_GRACE.as_millis()
            ));
        }
        (!failures.is_empty()).then(|| safe_local_failure(&failures.join("; ")))
    }
}

fn spawn_output_worker(
    mut reader: impl io::Read + Send + 'static,
    worker: CompletionWorker,
    thread_name: &'static str,
    events: mpsc::Sender<CompletionEvent>,
) -> io::Result<()> {
    let handle = thread::Builder::new()
        .name(format!("youta-report-{thread_name}"))
        .spawn(move || {
            let result = stream_bounded_output(&mut reader, worker, &events);
            let _ = events.send(CompletionEvent::Finished { worker, result });
        })?;
    drop(handle);
    Ok(())
}

fn stream_bounded_output(
    reader: &mut impl io::Read,
    worker: CompletionWorker,
    events: &mpsc::Sender<CompletionEvent>,
) -> io::Result<()> {
    let mut retained = 0_usize;
    let mut truncation_sent = false;
    let mut buffer = [0_u8; 4096];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        let retained_count = MAX_CAPTURED_HELPER_OUTPUT_BYTES
            .saturating_sub(retained)
            .min(count);
        if retained_count > 0 {
            if events
                .send(CompletionEvent::Output {
                    worker,
                    bytes: buffer[..retained_count].to_vec(),
                })
                .is_err()
            {
                return Ok(());
            }
            retained = retained.saturating_add(retained_count);
        }
        if retained_count < count && !truncation_sent {
            if events
                .send(CompletionEvent::OutputTruncated(worker))
                .is_err()
            {
                return Ok(());
            }
            truncation_sent = true;
        }
    }
}

fn spawn_input_worker(
    mut stdin: impl io::Write + Send + 'static,
    input: Vec<u8>,
    events: mpsc::Sender<CompletionEvent>,
) -> io::Result<()> {
    let handle = thread::Builder::new()
        .name("youta-report-standard-input-writer".to_owned())
        .spawn(move || {
            let result = stdin.write_all(&input);
            let _ = events.send(CompletionEvent::Finished {
                worker: CompletionWorker::Stdin,
                result,
            });
        })?;
    drop(handle);
    Ok(())
}

fn collect_completion_events(
    events: mpsc::Receiver<CompletionEvent>,
    stdin_expected: bool,
    deadline: Instant,
) -> CompletionCapture {
    let mut stdout = CapturedBytes::default();
    let mut stderr = CapturedBytes::default();
    let mut finished = [!stdin_expected, false, false];
    let mut failures = Vec::new();
    while !finished.iter().all(|finished| *finished) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match events.recv_timeout(deadline.saturating_duration_since(now)) {
            Ok(event) => apply_completion_event(
                event,
                &mut stdout,
                &mut stderr,
                &mut finished,
                &mut failures,
            ),
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    for event in events.try_iter() {
        apply_completion_event(
            event,
            &mut stdout,
            &mut stderr,
            &mut finished,
            &mut failures,
        );
    }
    if !finished[CompletionWorker::Stdout.index()] {
        stdout.truncated = true;
    }
    if !finished[CompletionWorker::Stderr.index()] {
        stderr.truncated = true;
    }
    CompletionCapture {
        stdout: stdout.finish(),
        stderr: stderr.finish(),
        finished,
        failures,
        stdin_expected,
    }
}

fn apply_completion_event(
    event: CompletionEvent,
    stdout: &mut CapturedBytes,
    stderr: &mut CapturedBytes,
    finished: &mut [bool; 3],
    failures: &mut Vec<(CompletionWorker, String)>,
) {
    match event {
        CompletionEvent::Output { worker, bytes } => match worker {
            CompletionWorker::Stdout => stdout.push(&bytes),
            CompletionWorker::Stderr => stderr.push(&bytes),
            CompletionWorker::Stdin => {}
        },
        CompletionEvent::OutputTruncated(worker) => match worker {
            CompletionWorker::Stdout => stdout.truncated = true,
            CompletionWorker::Stderr => stderr.truncated = true,
            CompletionWorker::Stdin => {}
        },
        CompletionEvent::Finished { worker, result } => {
            finished[worker.index()] = true;
            if let Err(error) = result {
                match worker {
                    CompletionWorker::Stdout => stdout.truncated = true,
                    CompletionWorker::Stderr => stderr.truncated = true,
                    CompletionWorker::Stdin => {}
                }
                failures.push((worker, format!("{} failed: {error}", worker.label())));
            }
        }
    }
}

fn completion_setup_failed(child: &mut Child, reason: &str) -> CompletedProcess {
    terminate_and_reap(child);
    CompletedProcess {
        status: CompletedProcessStatus::OutcomeUnknown {
            reason: safe_local_failure(reason),
        },
        stdout: CapturedOutput::default(),
        stderr: CapturedOutput::default(),
    }
}

fn safe_local_failure(reason: &str) -> String {
    bounded_helper_detail(
        &escape_terminal_controls(&redact_diagnostic_text(reason)),
        false,
    )
}

#[cfg(test)]
fn read_bounded_output(mut reader: impl io::Read) -> io::Result<CapturedOutput> {
    let mut retained = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 4096];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let remaining = MAX_CAPTURED_HELPER_OUTPUT_BYTES.saturating_sub(retained.len());
        let retained_count = remaining.min(count);
        retained.extend_from_slice(&buffer[..retained_count]);
        truncated |= retained_count < count;
    }
    Ok(CapturedOutput {
        text: String::from_utf8_lossy(&retained).into_owned(),
        truncated,
    })
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
    /// Native operating-system URL opener used by the browser fallback.
    pub url_opener: Option<PathBuf>,
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
    let url_opener = find_executable(path, url_opener_name_for_platform(macos));
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
        url_opener,
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
    /// The native URL opener was not discovered on a safe `PATH` entry.
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
        /// Bounded stderr, or stdout when stderr was empty.
        detail: Option<String>,
    },
    /// A started GitHub submission has an indeterminate remote outcome.
    GitHubSubmissionOutcomeUnknown {
        /// Bounded explanation of the local process result.
        reason: String,
        /// Bounded stderr, or stdout when stderr was empty.
        detail: Option<String>,
    },
    /// `gh` succeeded but did not print the created issue URL.
    GitHubIssueUrlMissing {
        /// Bounded command output retained for diagnosis.
        detail: Option<String>,
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
            Self::UrlOpenerUnavailable => formatter.write_str(
                "the operating-system URL opener was not found on an absolute PATH entry",
            ),
            Self::ProcessIo { helper, source } => {
                write!(formatter, "cannot run {helper}: {source}")
            }
            Self::ProcessFailed {
                helper,
                exit_code,
                detail,
            } => {
                match exit_code {
                    Some(code) => write!(formatter, "{helper} exited with status {code}"),
                    None => write!(formatter, "{helper} terminated unsuccessfully"),
                }?;
                if let Some(detail) = detail {
                    write!(formatter, ": {detail}")?;
                }
                Ok(())
            }
            Self::GitHubSubmissionOutcomeUnknown { reason, detail } => {
                write!(
                    formatter,
                    "{reason}; the GitHub issue submission outcome is unknown, so check existing issues before retrying"
                )?;
                if let Some(detail) = detail {
                    write!(formatter, ": {detail}")?;
                }
                Ok(())
            }
            Self::GitHubIssueUrlMissing { detail } => {
                formatter.write_str("gh created an issue but did not return its URL")?;
                if let Some(detail) = detail {
                    write!(formatter, ": {detail}")?;
                }
                Ok(())
            }
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
            | Self::GitHubSubmissionOutcomeUnknown { .. }
            | Self::GitHubIssueUrlMissing { .. }
            | Self::ProcessStillRunning { .. }
            | Self::OpenAfterCopy { .. } => None,
        }
    }
}

impl ReportActionError {
    /// Returns whether a GitHub submission may have completed remotely.
    ///
    /// Callers should discourage an immediate retry and direct the user to
    /// inspect [`GITHUB_ISSUES_URL`] for a possibly created duplicate.
    #[must_use]
    pub const fn submission_outcome_unknown(&self) -> bool {
        matches!(
            self,
            Self::GitHubSubmissionOutcomeUnknown { .. } | Self::GitHubIssueUrlMissing { .. }
        )
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

/// A GitHub issue created from a complete diagnostic report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmittedGitHubIssue {
    /// Canonical URL printed by `gh issue create`.
    pub url: String,
}

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

    /// Submits a GitHub issue through `gh` and returns its canonical URL.
    ///
    /// The complete report is piped through standard input using
    /// `--body-file -`. The command intentionally does not use `--web`, which
    /// would encode the full report into a length-limited browser URL.
    ///
    /// # Errors
    ///
    /// Returns an error when `gh` is unavailable, cannot launch, cannot accept
    /// its input, exits unsuccessfully, times out, or does not print the
    /// created issue URL. Subprocess output included in errors is bounded and
    /// redacted.
    pub fn submit_github_issue(
        &self,
        title: &str,
        report: &str,
    ) -> Result<SubmittedGitHubIssue, ReportActionError> {
        let Some(github_cli) = &self.tools.github_cli else {
            return Err(ReportActionError::GitHubCliUnavailable);
        };
        let plan = github_cli_plan(github_cli, title, report);
        let completed = self.runner.execute_to_completion(&plan).map_err(|source| {
            ReportActionError::ProcessIo {
                helper: "gh",
                source,
            }
        })?;
        let detail = completed.useful_detail();
        match completed.status {
            CompletedProcessStatus::ExitedSuccessfully => {
                let url = github_issue_url(&completed.stdout.text)
                    .ok_or(ReportActionError::GitHubIssueUrlMissing { detail })?;
                Ok(SubmittedGitHubIssue { url })
            }
            CompletedProcessStatus::ExitedUnsuccessfully(exit_code) => {
                let status = exit_code.map_or_else(
                    || "gh terminated unsuccessfully".to_owned(),
                    |code| format!("gh exited with status {code}"),
                );
                Err(ReportActionError::GitHubSubmissionOutcomeUnknown {
                    reason: status,
                    detail,
                })
            }
            CompletedProcessStatus::TimedOut => {
                Err(ReportActionError::GitHubSubmissionOutcomeUnknown {
                    reason: format!(
                        "gh did not finish within {} seconds and was stopped",
                        plan.observation_time.as_secs()
                    ),
                    detail,
                })
            }
            CompletedProcessStatus::OutcomeUnknown { reason } => {
                Err(ReportActionError::GitHubSubmissionOutcomeUnknown { reason, detail })
            }
        }
    }

    /// Copies the report and opens a short pre-filled GitHub issue page.
    ///
    /// Both buttons remain useful when `gh` is installed: this browser-only
    /// action always copies the complete report first, then opens a URL that
    /// contains only a bounded title and a short paste instruction.
    ///
    /// # Errors
    ///
    /// Returns an error when copying fails, the native URL opener is
    /// unavailable, or the browser helper does not promptly confirm a
    /// successful exit. An opener error states which clipboard transport
    /// already succeeded.
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
        let Some(url_opener) = &self.tools.url_opener else {
            return Err(ReportActionError::UrlOpenerUnavailable);
        };
        let plan = issue_page_plan(url_opener, title);
        self.execute_successfully(&plan, system_url_opener_name())
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
                Err(ReportActionError::ProcessFailed {
                    helper,
                    exit_code,
                    detail: None,
                })
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
            "--repo".into(),
            GITHUB_REPOSITORY.into(),
            "--title".into(),
            bounded_issue_title(title).into(),
            "--body-file".into(),
            "-".into(),
        ],
        standard_input: Some(report.as_bytes().to_vec()),
        observation_time: GITHUB_SUBMISSION_TIMEOUT,
    }
}

fn github_issue_url(stdout: &str) -> Option<String> {
    let prefix = format!("https://github.com/{GITHUB_REPOSITORY}/issues/");
    stdout.split_whitespace().find_map(|token| {
        let candidate = token.trim_matches(|character: char| {
            matches!(character, '<' | '>' | '(' | ')' | '[' | ']' | ',' | ';')
        });
        let issue_number = candidate.strip_prefix(&prefix)?;
        (!issue_number.is_empty() && issue_number.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| candidate.to_owned())
    })
}

fn issue_page_plan(executable: &Path, title: &str) -> CommandPlan {
    CommandPlan {
        executable: executable.to_owned(),
        // `xdg-open` does not accept the conventional `--` separator, while
        // macOS `open` does not require it for an HTTPS URL. The bounded URL
        // always starts with HTTPS, so it is safe as the sole target argument.
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
        completed: RefCell<VecDeque<io::Result<CompletedProcess>>>,
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

        fn with_completed(completed: impl IntoIterator<Item = CompletedProcess>) -> Self {
            Self {
                completed: RefCell::new(completed.into_iter().map(Ok).collect()),
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

        fn execute_to_completion(&self, plan: &CommandPlan) -> io::Result<CompletedProcess> {
            self.plans.borrow_mut().push(plan.clone());
            self.completed.borrow_mut().pop_front().unwrap_or_else(|| {
                Ok(CompletedProcess {
                    status: CompletedProcessStatus::ExitedSuccessfully,
                    stdout: CapturedOutput {
                        text: format!("https://github.com/{GITHUB_REPOSITORY}/issues/123\n"),
                        truncated: false,
                    },
                    stderr: CapturedOutput::default(),
                })
            })
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
            url_opener: Some(PathBuf::from("/usr/bin").join(system_url_opener_name())),
        }
    }

    #[test]
    fn github_cli_plan_has_exact_arguments_and_full_report_on_stdin() {
        let runner = MockRunner::default();
        let actions = ReportActions::with_runner(runner, tools());
        let report = "line one\nline two\nfull backtrace";

        let submission = actions
            .submit_github_issue("Playback failed", report)
            .expect("plan should be accepted");

        assert_eq!(
            submission.url,
            format!("https://github.com/{GITHUB_REPOSITORY}/issues/123")
        );

        let plans = actions.runner.plans.borrow();
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].executable, Path::new("/usr/bin/gh"));
        assert_eq!(
            plans[0].arguments,
            [
                "issue",
                "create",
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
        assert_eq!(plans[0].observation_time, GITHUB_SUBMISSION_TIMEOUT);
    }

    #[test]
    fn github_submission_extracts_the_created_issue_url_from_bounded_stdout() {
        let runner = MockRunner::with_completed([CompletedProcess {
            status: CompletedProcessStatus::ExitedSuccessfully,
            stdout: CapturedOutput {
                text: format!(
                    "warning: using default template\nhttps://github.com/{GITHUB_REPOSITORY}/issues/456\n"
                ),
                truncated: false,
            },
            stderr: CapturedOutput::default(),
        }]);
        let actions = ReportActions::with_runner(runner, tools());

        let submission = actions
            .submit_github_issue("Playback failed", "report")
            .expect("gh success should return the issue URL");

        assert_eq!(
            submission,
            SubmittedGitHubIssue {
                url: format!("https://github.com/{GITHUB_REPOSITORY}/issues/456")
            }
        );
    }

    #[test]
    fn github_submission_accepts_exit_zero_url_when_only_output_eof_is_incomplete() {
        let runner = MockRunner::with_completed([CompletedProcess {
            status: CompletedProcessStatus::ExitedSuccessfully,
            stdout: CapturedOutput {
                text: format!("https://github.com/{GITHUB_REPOSITORY}/issues/457\n"),
                truncated: true,
            },
            stderr: CapturedOutput {
                text: String::new(),
                truncated: true,
            },
        }]);
        let actions = ReportActions::with_runner(runner, tools());

        let submission = actions
            .submit_github_issue("Playback failed", "report")
            .expect("exit zero and a canonical URL prove successful submission");

        assert_eq!(
            submission.url,
            format!("https://github.com/{GITHUB_REPOSITORY}/issues/457")
        );
    }

    #[test]
    fn github_submission_never_accepts_a_url_from_an_unconfirmed_process_result() {
        let statuses = [
            CompletedProcessStatus::ExitedUnsuccessfully(Some(7)),
            CompletedProcessStatus::TimedOut,
            CompletedProcessStatus::OutcomeUnknown {
                reason: "lost child status after gh started".to_owned(),
            },
        ];

        for status in statuses {
            let runner = MockRunner::with_completed([CompletedProcess {
                status,
                stdout: CapturedOutput {
                    text: format!("https://github.com/{GITHUB_REPOSITORY}/issues/999\n"),
                    truncated: false,
                },
                stderr: CapturedOutput::default(),
            }]);
            let actions = ReportActions::with_runner(runner, tools());

            let error = actions
                .submit_github_issue("Playback failed", "report")
                .expect_err("canonical-looking stdout must not override an unconfirmed status");

            assert!(error.submission_outcome_unknown());
        }
    }

    #[test]
    fn github_submission_nonzero_exit_is_indeterminate_and_includes_safe_stderr() {
        let runner = MockRunner::with_completed([CompletedProcess {
            status: CompletedProcessStatus::ExitedUnsuccessfully(Some(1)),
            stdout: CapturedOutput::default(),
            stderr: CapturedOutput {
                text: "HTTP 403: token=secret was rejected".to_owned(),
                truncated: true,
            },
        }]);
        let actions = ReportActions::with_runner(runner, tools());

        let error = actions
            .submit_github_issue("Playback failed", "report")
            .expect_err("gh failure should be reported");
        let message = error.to_string();

        assert!(message.contains("gh exited with status 1"));
        assert!(message.contains("HTTP 403"));
        assert!(message.contains("token= <redacted>"));
        assert!(message.contains("helper output truncated"));
        assert!(!message.contains("secret"));
        assert!(error.submission_outcome_unknown());
        assert!(message.contains("check existing issues before retrying"));
    }

    #[test]
    fn github_submission_pre_start_io_failure_is_safe_to_retry() {
        let runner = MockRunner::default();
        runner.completed.borrow_mut().push_back(Err(io::Error::new(
            io::ErrorKind::NotFound,
            "gh disappeared before it could start",
        )));
        let actions = ReportActions::with_runner(runner, tools());

        let error = actions
            .submit_github_issue("Playback failed", "report")
            .expect_err("a process that never started should fail");

        assert!(matches!(error, ReportActionError::ProcessIo { .. }));
        assert!(!error.submission_outcome_unknown());
    }

    #[test]
    fn github_submission_post_start_observation_failure_is_indeterminate() {
        let runner = MockRunner::with_completed([CompletedProcess {
            status: CompletedProcessStatus::OutcomeUnknown {
                reason: "cannot observe gh after it started: lost child status".to_owned(),
            },
            stdout: CapturedOutput::default(),
            stderr: CapturedOutput::default(),
        }]);
        let actions = ReportActions::with_runner(runner, tools());

        let error = actions
            .submit_github_issue("Playback failed", "report")
            .expect_err("a post-start observation failure has an unknown remote outcome");

        assert!(error.submission_outcome_unknown());
        let message = error.to_string();
        assert!(message.contains("lost child status"));
        assert!(message.contains("check existing issues before retrying"));
    }

    #[test]
    fn helper_failure_detail_escapes_terminal_controls_and_has_a_display_limit() {
        let raw = format!(
            "\x1b]52;c;attacker-controlled\x07\x1b[31mred\x1b[0m\rrewritten\x08{}",
            "x".repeat(MAX_DISPLAYED_HELPER_DETAIL_BYTES * 2)
        );
        let detail = CapturedOutput {
            text: raw,
            truncated: false,
        }
        .display_text()
        .expect("nonempty helper detail");

        assert!(detail.len() <= MAX_DISPLAYED_HELPER_DETAIL_BYTES);
        assert!(
            detail
                .chars()
                .all(|character| character == '\n' || !character.is_control()),
            "terminal controls remained in {detail:?}"
        );
        assert!(detail.contains("\\u{1b}]52;c;attacker-controlled\\u{7}"));
        assert!(detail.contains("helper detail truncated"));
    }

    #[test]
    fn github_submission_timeout_reports_indeterminate_outcome_and_diagnostics() {
        let runner = MockRunner::with_completed([CompletedProcess {
            status: CompletedProcessStatus::TimedOut,
            stdout: CapturedOutput::default(),
            stderr: CapturedOutput {
                text: "network stalled".to_owned(),
                truncated: false,
            },
        }]);
        let actions = ReportActions::with_runner(runner, tools());

        let error = actions
            .submit_github_issue("Playback failed", "report")
            .expect_err("timed-out submission should be reported");
        assert!(matches!(
            &error,
            ReportActionError::GitHubSubmissionOutcomeUnknown { .. }
        ));
        assert!(error.submission_outcome_unknown());
        let message = error.to_string();

        assert!(message.contains("gh did not finish within 30 seconds and was stopped"));
        assert!(message.contains("outcome is unknown"));
        assert!(message.contains("check existing issues before retrying"));
        assert!(message.contains("network stalled"));
    }

    #[test]
    fn github_submission_rejects_success_without_an_issue_url() {
        let runner = MockRunner::with_completed([CompletedProcess {
            status: CompletedProcessStatus::ExitedSuccessfully,
            stdout: CapturedOutput {
                text: "success without a URL".to_owned(),
                truncated: false,
            },
            stderr: CapturedOutput::default(),
        }]);
        let actions = ReportActions::with_runner(runner, tools());

        let error = actions
            .submit_github_issue("Playback failed", "report")
            .expect_err("a structured success requires the created issue URL");

        assert!(matches!(
            &error,
            ReportActionError::GitHubIssueUrlMissing { .. }
        ));
        assert!(error.to_string().contains("success without a URL"));
        assert!(error.submission_outcome_unknown());
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
            .submit_github_issue("Failure; rm -rf something", report)
            .expect("mock command should succeed");

        let plans = actions.runner.plans.borrow();
        assert_eq!(plans[0].executable, Path::new("/usr/bin/gh"));
        assert_eq!(plans[0].arguments[5], "Failure; rm -rf something");
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
                url_opener: None,
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
        assert_eq!(
            plans[1].executable,
            Path::new("/usr/bin").join(system_url_opener_name())
        );
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
        assert!(error.to_string().contains(&format!(
            "{} exited with status 3",
            system_url_opener_name()
        )));
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
        assert!(message.contains(&format!(
            "{} did not report a successful exit promptly",
            system_url_opener_name()
        )));
        assert!(!message.contains("private report"));
        assert!(!message.contains(NEW_ISSUE_URL));
    }

    #[test]
    fn submit_issue_requires_discovered_github_cli() {
        let runner = MockRunner::default();
        let actions = ReportActions::with_runner(
            runner,
            ReportActionTools {
                github_cli: None,
                ..tools()
            },
        );

        let error = actions
            .submit_github_issue("Failure", "report")
            .expect_err("gh should be required");
        assert!(matches!(error, ReportActionError::GitHubCliUnavailable));
        assert!(actions.runner.plans.borrow().is_empty());
    }

    #[test]
    fn timed_out_submission_is_not_reported_as_successful() {
        let runner = MockRunner::with_completed([CompletedProcess {
            status: CompletedProcessStatus::TimedOut,
            stdout: CapturedOutput::default(),
            stderr: CapturedOutput::default(),
        }]);
        let actions = ReportActions::with_runner(runner, tools());

        let error = actions
            .submit_github_issue("Failure", "report")
            .expect_err("a timed-out submission has an unknown outcome");

        assert!(error.submission_outcome_unknown());
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

    #[cfg(unix)]
    #[test]
    fn system_runner_submits_a_430_line_report_and_captures_the_issue_url() {
        let temporary = tempfile::tempdir().expect("temporary command directory");
        let helper = temporary.path().join("fake-gh-success");
        let shell = find_test_executable(&["/bin/sh", "/usr/bin/sh"]);
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\ncount=0\nwhile IFS= read -r line; do count=$((count + 1)); done\nif [ \"$count\" -ne 430 ]; then printf 'read %s lines\\n' \"$count\" >&2; exit 9; fi\nprintf '%s\\n' 'https://github.com/{GITHUB_REPOSITORY}/issues/430'\n"
            ),
        )
        .expect("fake gh success fixture");
        let report = (1..=430)
            .map(|line| format!("diagnostic line {line}\n"))
            .collect::<String>();
        let plan = CommandPlan {
            executable: shell,
            arguments: vec![helper.into_os_string()],
            standard_input: Some(report.into_bytes()),
            observation_time: Duration::from_secs(2),
        };

        let completed = SystemRunner
            .execute_to_completion(&plan)
            .expect("complete fake gh submission");

        assert_eq!(completed.status, CompletedProcessStatus::ExitedSuccessfully);
        assert_eq!(
            github_issue_url(&completed.stdout.text),
            Some(format!("https://github.com/{GITHUB_REPOSITORY}/issues/430"))
        );
        assert!(completed.stderr.text.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn system_runner_preserves_stderr_when_child_rejects_large_stdin_early() {
        let temporary = tempfile::tempdir().expect("temporary command directory");
        let helper = temporary.path().join("fake-gh-failure");
        let shell = find_test_executable(&["/bin/sh", "/usr/bin/sh"]);
        fs::write(
            &helper,
            "#!/bin/sh\nprintf '%s\\n' 'API rejected the issue body' >&2\nexit 7\n",
        )
        .expect("fake gh failure fixture");
        let plan = CommandPlan {
            executable: shell,
            arguments: vec![helper.into_os_string()],
            standard_input: Some(vec![b'x'; 4 * 1024 * 1024]),
            observation_time: Duration::from_secs(2),
        };

        let completed = SystemRunner
            .execute_to_completion(&plan)
            .expect("nonzero exit must not be masked by a broken stdin pipe");

        assert_eq!(
            completed.status,
            CompletedProcessStatus::ExitedUnsuccessfully(Some(7))
        );
        assert_eq!(completed.stderr.text.trim(), "API rejected the issue body");
    }

    #[cfg(unix)]
    #[test]
    fn system_runner_does_not_wait_for_a_descendant_holding_stdout_open() {
        let temporary = tempfile::tempdir().expect("temporary command directory");
        let helper = temporary.path().join("fake-gh-descendant");
        let shell = find_test_executable(&["/bin/sh", "/usr/bin/sh"]);
        let sleep = find_test_executable(&["/usr/bin/sleep", "/bin/sleep"]);
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\nwhile IFS= read -r line; do :; done\n\"$1\" 2 &\nprintf '%s\\n' 'https://github.com/{GITHUB_REPOSITORY}/issues/731'\n"
            ),
        )
        .expect("fake gh inherited-pipe fixture");
        let plan = CommandPlan {
            executable: shell,
            arguments: vec![helper.into_os_string(), sleep.into_os_string()],
            standard_input: Some(b"complete diagnostic report\n".to_vec()),
            observation_time: Duration::from_secs(1),
        };

        let started_at = Instant::now();
        let completed = SystemRunner
            .execute_to_completion(&plan)
            .expect("direct child should complete without waiting for its descendant");
        let elapsed = started_at.elapsed();

        assert!(
            elapsed < Duration::from_millis(1_500),
            "inherited stdout kept completion blocked for {elapsed:?}"
        );
        assert_eq!(completed.status, CompletedProcessStatus::ExitedSuccessfully);
        assert_eq!(
            github_issue_url(&completed.stdout.text),
            Some(format!("https://github.com/{GITHUB_REPOSITORY}/issues/731"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_runner_preserves_nonzero_exit_while_descendant_holds_stdin_open() {
        let temporary = tempfile::tempdir().expect("temporary command directory");
        let helper = temporary.path().join("fake-gh-stdin-descendant");
        let shell = find_test_executable(&["/bin/sh", "/usr/bin/sh"]);
        let sleep = find_test_executable(&["/usr/bin/sleep", "/bin/sleep"]);
        fs::write(
            &helper,
            "#!/bin/sh\n\"$1\" 2 3<&0 </dev/null >/dev/null 2>&1 &\nprintf '%s\\n' 'API rejected the issue body' >&2\nexit 7\n",
        )
        .expect("fake gh inherited-stdin fixture");
        let plan = CommandPlan {
            executable: shell,
            arguments: vec![helper.into_os_string(), sleep.into_os_string()],
            standard_input: Some(vec![b'x'; 4 * 1024 * 1024]),
            observation_time: Duration::from_secs(1),
        };

        let started_at = Instant::now();
        let completed = SystemRunner
            .execute_to_completion(&plan)
            .expect("nonzero child result must not wait for an inherited stdin reader");
        let elapsed = started_at.elapsed();

        assert!(
            elapsed < Duration::from_millis(1_500),
            "inherited stdin kept completion blocked for {elapsed:?}"
        );
        assert_eq!(
            completed.status,
            CompletedProcessStatus::ExitedUnsuccessfully(Some(7))
        );
        assert_eq!(completed.stderr.text.trim(), "API rejected the issue body");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn system_runner_times_out_reaps_and_bounds_noisy_helper_output() {
        let temporary = tempfile::tempdir().expect("temporary command directory");
        let helper = temporary.path().join("fake-gh-timeout");
        let pid_file = temporary.path().join("pid");
        let shell = find_test_executable(&["/bin/sh", "/usr/bin/sh"]);
        let sleep = find_test_executable(&["/usr/bin/sleep", "/bin/sleep"]);
        let noise = "x".repeat(MAX_CAPTURED_HELPER_OUTPUT_BYTES + 4096);
        fs::write(
            &helper,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > \"$1\"\nprintf '%s' '{noise}'\nexec \"$2\" 5\n"
            ),
        )
        .expect("fake gh timeout fixture");
        let plan = CommandPlan {
            executable: shell,
            arguments: vec![
                helper.into_os_string(),
                pid_file.as_os_str().to_owned(),
                sleep.into_os_string(),
            ],
            standard_input: None,
            observation_time: Duration::from_millis(250),
        };

        let completed = SystemRunner
            .execute_to_completion(&plan)
            .expect("time out fake gh submission");

        assert_eq!(completed.status, CompletedProcessStatus::TimedOut);
        assert_eq!(
            completed.stdout.text.len(),
            MAX_CAPTURED_HELPER_OUTPUT_BYTES
        );
        assert!(completed.stdout.truncated);
        let pid = wait_for_test_pid(&pid_file, Duration::from_secs(1));
        assert!(
            !PathBuf::from(format!("/proc/{pid}")).exists(),
            "timed-out helper PID {pid} was not reaped"
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
        for name in [
            "gh", "open", "xdg-open", "wl-copy", "xclip", "xsel", "pbcopy",
        ] {
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
        assert_eq!(
            wayland.url_opener.as_deref(),
            Some(directory.path().join("xdg-open").as_path())
        );
        assert_eq!(
            macos.url_opener.as_deref(),
            Some(directory.path().join("open").as_path())
        );
    }

    #[test]
    fn url_opener_name_matches_linux_and_macos_platform_conventions() {
        assert_eq!(url_opener_name_for_platform(false), "xdg-open");
        assert_eq!(url_opener_name_for_platform(true), "open");
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

    #[test]
    fn helper_output_capture_is_bounded_while_draining_the_complete_stream() {
        let input = vec![b'x'; MAX_CAPTURED_HELPER_OUTPUT_BYTES + 4096];

        let captured = read_bounded_output(io::Cursor::new(input))
            .expect("in-memory output capture should succeed");

        assert_eq!(captured.text.len(), MAX_CAPTURED_HELPER_OUTPUT_BYTES);
        assert!(captured.text.bytes().all(|byte| byte == b'x'));
        assert!(captured.truncated);
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
