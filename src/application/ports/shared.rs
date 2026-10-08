//! The ports every context uses (docs/design/architecture.md, section
//! "portのmodule"): the clock and the IDs, the start of a process, the
//! asks, and [`Queue`], the whole queue as one connection, which names the
//! store ports of every context.

use super::execution::{RunCoordination, RunLog, RunRecovery, RunTransitions, SessionRegistry};
use super::host::{HeadlessJobStore, SupervisorRegistry};
use super::observation::QueueRecords;
use super::planning::{
    DraftPlannerStore, GoalReviewStore, PlanRequestStore, PlanReviewStore, PlanningRecords,
    TaskStore,
};
use crate::application::{timestamp, unix_seconds};
use crate::domain::{
    Ask, AskId, AskKind, AskOutcome, LeaseToken, NewAsk, RunId, SessionRole, TaskId,
};
use anyhow::Result;
use std::{
    ffi::{OsStr, OsString},
    fmt, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

/// A process to start: its program, arguments, environment changes and
/// working directory, built like a command and started by a [`Spawner`],
/// which also decides where its standard streams go.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandSpec {
    program: OsString,
    args: Vec<OsString>,
    /// In the order given; `None` removes the variable.
    envs: Vec<(OsString, Option<OsString>)>,
    current_dir: Option<PathBuf>,
    /// Start it in a session and process group of its own, so neither a
    /// signal to the starter's group nor the close of its terminal reaches
    /// it (the automatic update's job, ADR-0045 decision 13).
    new_session: bool,
    /// What the process reads on its standard input, whatever the
    /// [`Streams`] say: a headless job's prompt, which on the command line
    /// could pass the system's limit on the arguments (`E2BIG`, task 1560).
    stdin: Option<String>,
}

impl CommandSpec {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            ..Self::default()
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            self.arg(arg);
        }
        self
    }

    /// Add options `args` before the `--` that ends the options given so
    /// far, or after them all when there is none.
    pub fn option_args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let at = self
            .args
            .iter()
            .position(|arg| arg == "--")
            .unwrap_or(self.args.len());
        let rest = self.args.split_off(at);
        self.args(args);
        self.args.extend(rest);
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.envs
            .push((key.as_ref().to_owned(), Some(value.as_ref().to_owned())));
        self
    }

    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (key, value) in vars {
            self.env(key, value);
        }
        self
    }

    /// Take out the first two consecutive arguments `first` `second`;
    /// whether they were there.
    pub fn remove_arg_pair(&mut self, first: impl AsRef<OsStr>, second: impl AsRef<OsStr>) -> bool {
        let (first, second) = (first.as_ref(), second.as_ref());
        let at = self
            .args
            .windows(2)
            .position(|pair| pair[0] == first && pair[1] == second);
        if let Some(at) = at {
            self.args.drain(at..at + 2);
        }
        at.is_some()
    }

    /// The process does not inherit `key`.
    pub fn env_remove(&mut self, key: impl AsRef<OsStr>) -> &mut Self {
        self.envs.push((key.as_ref().to_owned(), None));
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.current_dir = Some(dir.as_ref().to_owned());
        self
    }

    /// See [`Self::get_new_session`].
    pub fn new_session(&mut self) -> &mut Self {
        self.new_session = true;
        self
    }

    /// Whether the process starts in a session of its own.
    pub fn get_new_session(&self) -> bool {
        self.new_session
    }

    /// See [`Self::get_stdin`].
    pub fn stdin(&mut self, text: impl Into<String>) -> &mut Self {
        self.stdin = Some(text.into());
        self
    }

    /// What the process reads on its standard input instead of the input
    /// its [`Streams`] give; `None` leaves it to them.
    pub fn get_stdin(&self) -> Option<&str> {
        self.stdin.as_deref()
    }

    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    /// Start `program` instead: the agent's executable found again by its
    /// provider's name (ADR-t2079-1).
    pub fn set_program(&mut self, program: impl AsRef<OsStr>) -> &mut Self {
        self.program = program.as_ref().to_owned();
        self
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.args.iter().map(OsString::as_os_str)
    }

    /// Every change to the environment in the order given; `None` removes.
    pub fn get_envs(&self) -> impl Iterator<Item = (&OsStr, Option<&OsStr>)> {
        self.envs
            .iter()
            .map(|(key, value)| (key.as_os_str(), value.as_deref()))
    }

    pub fn get_current_dir(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }
}

/// The standard input of a process ([`CommandSpec::get_stdin`]) could not
/// be prepared (its file not made, written or removed): the starter's
/// environment (its `TMPDIR` missing, unwritable or full), not the program,
/// so [`job_start_failure`] takes it for no provider's (task 1560). `what`
/// says which step and file, `source` why.
///
/// [`job_start_failure`]: super::host::job_start_failure
#[derive(Debug)]
pub struct StdinUnprepared {
    pub what: String,
    pub source: io::Error,
}

impl fmt::Display for StdinUnprepared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.what)
    }
}

impl std::error::Error for StdinUnprepared {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Where the standard streams of a started process go.
#[derive(Debug, Clone, Copy)]
pub enum Streams<'a> {
    /// The starting process's own: the session wrapper's terminal.
    Inherit,
    /// Nowhere.
    Null,
    /// No input (unless the command has its own,
    /// [`CommandSpec::get_stdin`]); stdout and stderr to these files,
    /// created or truncated.
    Files { stdout: &'a Path, stderr: &'a Path },
    /// No input; stdout and stderr both to this one file, created or
    /// truncated.
    Log(&'a Path),
}

/// How a process ended: `description` as the operating system words it
/// (`exit status: 1`), `code` absent when a signal ended it, and `signal`
/// the signal then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    pub success: bool,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub description: String,
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.description)
    }
}

/// A process a [`Spawner`] started.
pub trait Spawned: Send {
    fn id(&self) -> u32;
    /// `None` while it runs.
    fn try_wait(&mut self) -> Result<Option<Exit>>;
    fn kill(&mut self) -> Result<()>;
    fn wait(&mut self) -> Result<Exit>;
    /// Kill it with every process in its group, for one started in a
    /// session of its own ([`CommandSpec::new_session`]): a headless turn
    /// is stopped with what it runs (ADR-t813-1 decision 3). One started
    /// otherwise is killed alone.
    fn kill_group(&mut self) -> Result<()> {
        self.kill()
    }
}

/// Starts processes: the agent under the session wrapper, the headless
/// review and triage jobs, and the observer.
pub trait Spawner: Send + Sync {
    fn spawn(&self, command: &CommandSpec, streams: Streams<'_>) -> Result<Box<dyn Spawned>>;
}

/// How long a connection to the queue waits for another connection's
/// write lock before its statement fails as busy.
pub const QUEUE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The context of an error a write met because another connection held the
/// queue's lock past [`QUEUE_BUSY_TIMEOUT`] (SQLite's busy or locked): a
/// passing condition, which the same write may try again (task 1119).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueBusy;

impl fmt::Display for QueueBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the queue database is locked by another writer")
    }
}

impl QueueBusy {
    /// Whether `error` is one the queue met as busy.
    pub fn is(error: &anyhow::Error) -> bool {
        error.downcast_ref::<QueueBusy>().is_some()
    }
}

/// The current time, injected so a use case reads it through this port and
/// a test can fix it (ADR-0013 policy 7). One operation reads it once and
/// passes the value on, so its steps share one reference time.
pub trait Clock: Send + Sync {
    fn system_time(&self) -> SystemTime;

    /// The monotonic clock the supervisor's waits are measured on (the
    /// registration, resume, exit and reopen timeouts): it does not move
    /// with the wall clock. Read once where a wait starts or is judged,
    /// and handed to the decision as a value (task 1557).
    fn monotonic(&self) -> Instant;

    /// Unix seconds, the form of `heartbeat_at`, `closed_at` and the other
    /// INTEGER times.
    fn now(&self) -> i64 {
        unix_seconds(self.system_time())
    }

    /// `%Y-%m-%dT%H:%M:%fZ` in UTC (RFC 3339 with milliseconds), the form
    /// SQLite's `strftime` gives `created_at` / `updated_at`.
    fn timestamp(&self) -> String {
        timestamp(self.system_time())
    }
}

/// New identifiers: run IDs, supervisor tokens and integrate tokens, each
/// a UUID string.
pub trait IdGenerator: Send + Sync {
    fn uuid(&self) -> String;

    /// A new supervisor or integrate token.
    fn lease_token(&self) -> LeaseToken {
        LeaseToken::new(self.uuid())
    }
}

/// The clock and the ID generator a use case is given.
#[derive(Clone)]
pub struct Generators {
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGenerator>,
}

impl fmt::Debug for Generators {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Generators").finish_non_exhaustive()
    }
}

/// The questions the runtime and its sessions put to a person (ADR-0022).
pub trait AskStore {
    /// Open the ask of the automatic update of `kind` (`update_failed`,
    /// `approve_update` or `approve_release`), closing an older open one of
    /// it (ADR-0073 decision 17); `subject` is the release an
    /// `approve_release` is about, and the keys of `details` (an object, or
    /// null) are written into its `ask_opened` (the release update's
    /// `plugin_only`).
    fn open_update_ask(
        &mut self,
        kind: crate::domain::AskKind,
        question: &str,
        options: &[&str],
        asked_by: &str,
        subject: Option<&str>,
        details: serde_json::Value,
    ) -> Result<crate::domain::Ask>;
    /// The payload of the `ask_opened` of the ask `id`; null when there is
    /// none.
    fn ask_opened_payload(&self, id: crate::domain::AskId) -> Result<serde_json::Value>;
    /// Answer the open `update_failed` ask `id` `answer` as the runtime
    /// and close it, its `ask_answered` naming `installed`, the commit put
    /// in place that contains the failed one; `None` when it is no longer
    /// open.
    fn close_installed_update_ask(
        &mut self,
        id: crate::domain::AskId,
        answer: &str,
        installed: &str,
    ) -> Result<Option<crate::domain::Ask>>;
    /// Whether the host's `[update]` of this queue looks for releases
    /// (`release` other than `off`, ADR-t618-1): whether a supervisor of a
    /// release build applies the release update's answers.
    fn release_updates_on(&self) -> bool;
    /// The answered, unclosed asks of the automatic update of `kind`,
    /// oldest first.
    fn update_answers(&self, kind: &crate::domain::AskKind) -> Result<Vec<crate::domain::Ask>>;
    /// Asks matching `query`, oldest first.
    fn asks(&self, query: AskQuery) -> Result<Vec<Ask>>;
    /// Whether the run has an ask of `kind` nobody closed, answered or not.
    fn has_unclosed_ask(&self, run_id: &RunId, kind: AskKind) -> Result<bool>;
    /// Register an ask, or return the open one it repeats.
    fn ask(&mut self, ask: NewAsk) -> Result<AskOutcome>;
    /// Open the authentication or cost ask of the hold with its run, or add
    /// the run to the open one (ADR-0047 decision 42).
    fn hold(&mut self, hold: crate::domain::NewHold) -> Result<crate::domain::HoldOutcome>;
    /// The open `queue_hold` ask that holds the run, if any.
    fn hold_of(&self, run_id: &RunId) -> Result<Option<Ask>>;
    /// Whether a `queue_hold` ask that holds the run is still unclosed:
    /// open, or answered and not yet applied by a supervisor.
    fn hold_unclosed(&self, run_id: &RunId) -> Result<bool>;
    /// Close the `queue_hold` asks of `reason` and `subject` nobody
    /// closed, answering the open ones `answer` (task 377).
    fn close_hold_asks(
        &mut self,
        reason: crate::domain::AskReason,
        subject: Option<&str>,
        answer: &str,
    ) -> Result<Vec<Ask>>;
    fn read_ask(&self, id: AskId) -> Result<Ask>;
    /// Write the answer of an open ask, given by `answerer` (its
    /// `answered_by` and the authority it carries).
    fn answer_as(
        &mut self,
        id: AskId,
        text: &str,
        answerer: crate::domain::Answerer,
    ) -> Result<Ask>;
    fn close_ask(&mut self, id: AskId) -> Result<Ask>;
    /// Answered `approve_landing` asks nobody closed.
    fn landing_answers(&self) -> Result<Vec<Ask>>;
    /// Answered `decide` asks of the triage nobody closed.
    fn triage_answers(&self) -> Result<Vec<Ask>>;
    /// Answered `worker_question` asks of the run not yet delivered.
    fn undelivered_answers(&self, run_id: &RunId) -> Result<Vec<Ask>>;
    fn ask_delivered(&mut self, id: AskId, workspace_id: &str) -> Result<Ask>;
    fn has_unclosed_worker_question(&self, run_id: &RunId) -> Result<bool>;
    /// Whether the run has a `worker_question` nobody closed that was
    /// created at or after `created_from` (unix seconds).
    fn has_unclosed_worker_question_since(&self, run_id: &RunId, created_from: i64)
    -> Result<bool>;
    /// When the run's `worker_question` closed last (unix seconds).
    fn last_worker_question_closed(&self, run_id: &RunId) -> Result<Option<i64>>;
    /// Close the run's `stuck_exit` asks nobody closed, with `answer`.
    fn close_stuck_exit_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>>;
    /// Close the run's `answer_prompt` asks nobody closed, with `answer`.
    fn close_answer_prompt_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>>;
    /// The run's `stalled` ask nobody closed, answered or not.
    fn unclosed_stalled_ask(&self, run_id: &RunId) -> Result<Option<Ask>>;
    /// Close the run's `stalled` asks nobody closed, with `answer`.
    fn close_stalled_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>>;
    /// Close the run's `stalled` asks nobody closed, with `answer`, and
    /// record `stall_resolved` (outcome `run_ended`) for each of its
    /// stalled detections with no end recorded yet, once.
    fn end_stalled_detections(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>>;
    /// Make `note` the landing recheck's one paragraph of the question of
    /// every ask of the run nobody closed, replacing its earlier ones and
    /// recording `ask_updated` with `why`; an ask left as it was is not
    /// written. The asks noted.
    fn note_on_asks(&mut self, run_id: &RunId, note: &str, why: &str) -> Result<Vec<Ask>>;
    /// Close the run's `approve_landing` asks nobody closed, with `answer`.
    fn close_approve_landing_asks(&mut self, run_id: &RunId, answer: &str) -> Result<Vec<Ask>>;
    /// Close the `approve_landing` asks nobody closed of the runs, of
    /// `task` or of all, that ended without landing
    /// ([`crate::domain::ended_landing_ask_answer`]).
    fn close_ended_landing_asks(&mut self, task: Option<TaskId>) -> Result<Vec<Ask>>;
    /// Close the `blocked` asks of the run, or of its task with no run
    /// named, nobody closed, with `answer`.
    fn close_blocked_asks(
        &mut self,
        run_id: &RunId,
        task_id: TaskId,
        answer: &str,
    ) -> Result<Vec<Ask>>;
}

/// Which asks [`AskStore::asks`] lists. By default the ones nobody closed;
/// `all` adds the closed ones, `open` keeps only the unanswered ones, and
/// `role` keeps those that wait for that role ([`Ask::waits_for`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct AskQuery {
    pub all: bool,
    pub open: bool,
    pub role: Option<SessionRole>,
}

/// The queue a use case works on: its tasks and goals, its runs and its
/// asks. A use case that needs only some of it takes those ports instead.
pub trait Queue:
    TaskStore
    + RunTransitions
    + RunRecovery
    + RunCoordination
    + SupervisorRegistry
    + SessionRegistry
    + RunLog
    + QueueRecords
    + PlanningRecords
    + AskStore
    + DraftPlannerStore
    + PlanRequestStore
    + PlanReviewStore
    + GoalReviewStore
    + HeadlessJobStore
{
}

impl<
    T: TaskStore
        + RunTransitions
        + RunRecovery
        + RunCoordination
        + SupervisorRegistry
        + SessionRegistry
        + RunLog
        + QueueRecords
        + PlanningRecords
        + AskStore
        + DraftPlannerStore
        + PlanRequestStore
        + PlanReviewStore
        + GoalReviewStore
        + HeadlessJobStore
        + ?Sized,
> Queue for T
{
}
