//! The recheck of the runs that wait to land after each landing
//! (ADR-0068): what it found for one run, how that is written into the
//! run's events, its asks and its `last_error`, and how the events are
//! read back, for a run found no longer landing and, since ADR-t1311-1,
//! for one found still landing. The supervisor runs the checks; this
//! module is the record.

use crate::domain::EventKind;
use serde::Deserialize;
use serde_json::{Value, json};

use super::run::payload::{restore, typed};
use super::verify_failure::VerifyFailure;
use super::{CommitSha, ReasonCode, RecordedKind, RunEvent, RunId, TaskId, scope::glob_matches};

/// A run the recheck found no longer landing on main (ADR-0068 decision
/// 3). Its `action` says what followed: [`RESUMED`] (it was parked for a
/// resume in the same transaction), [`HELD`] (a session or the landing
/// holds it; it is parked when it would land) or [`SKIPPED`] (the host
/// kept the command from judging the code, ADR-t639-1, or the command
/// failed on main alone too, ADR-t2141-1, which `main_failed: true` and
/// `main_log_path` tell: the run stays awaiting integration, neither
/// parked nor held).
pub const LANDING_RECHECK_FAILED: &str = super::event_kind::LANDING_RECHECK_FAILED;

/// A run the recheck found still landing on main (ADR-t1311-1): the main
/// and head it checked, the command that passed on them (null when only
/// `git merge-tree` was looked at) and the landing that moved main.
pub const LANDING_RECHECK_CLEAN: &str =
    crate::domain::event_kind::EventKind::LandingRecheckClean.as_str();

/// One recheck ended, recorded on the run whose landing moved main (or,
/// when no dagq landing did, on the first run it checked, ADR-t1310-1):
/// the main it checked against and what it found.
pub const LANDING_RECHECK_FINISHED: &str =
    crate::domain::event_kind::EventKind::LandingRecheckFinished.as_str();

/// `action` of a failure that parked the run for a resume.
pub const RESUMED: &str = "resumed";

/// `action` of a failure recorded on a run this supervisor holds in a slot
/// (waiting for the landing slot, or for its session's `/exit`).
pub const HELD: &str = "held";

/// `action` of a failure the recheck's command met on the host
/// ([`RecheckFailure::is_environmental`]), or that it met on main alone
/// too ([`kept_by_main`]): nothing followed, and the run keeps waiting to
/// land as it was.
pub const SKIPPED: &str = "skipped";

/// `command_skipped` of a clean finding whose run's diff touches none of
/// `[recheck] paths` (ADR-t2032-1): the recheck had a command and did not
/// run it. A clean finding without a command at all has no
/// `command_skipped`.
pub const SKIPPED_BY_PATHS: &str = "paths";

/// `[recheck]` of `dagq.toml` (ADR-0068 decision 2, ADR-t2032-1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecheckConfig {
    /// The command run on main's tree with a waiting run merged in; none
    /// checks the merge only. A command of only whitespace is refused when
    /// `dagq.toml` is read.
    pub command: Option<String>,
    /// The globs a run's diff against main must touch for the command to
    /// run on it; empty (no `paths`) runs it on every run. The globs follow
    /// the rules of `[e2e] paths`, and so do the errors for a glob it does
    /// not take, a value that is not an array and a duplicate key; an empty
    /// array is an error too (leave `paths` out instead).
    pub paths: Vec<String>,
}

/// Whether the recheck runs its command on a run whose merged tree differs
/// from main in `changed` (ADR-t2032-1): always without `paths`, otherwise
/// only when a changed path matches one of them.
pub fn runs_command(paths: &[String], changed: &[String]) -> bool {
    paths.is_empty()
        || changed
            .iter()
            .any(|path| paths.iter().any(|glob| glob_matches(glob, path)))
}

/// The landing whose commit the recheck checked against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Landed {
    pub run_id: RunId,
    pub task_id: TaskId,
}

/// Why a waiting run no longer lands on main.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecheckFailure {
    /// `git merge-tree` finds these paths conflicting.
    Conflict { paths: Vec<String> },
    /// The tree merged cleanly, but the recheck's command failed on it,
    /// for the class `failure` reads from its exit and its log.
    CheckFailed {
        command: String,
        exit_code: i32,
        log_path: String,
        output_tail: String,
        failure: VerifyFailure,
    },
    /// The command of a run in a container could not be run in the
    /// verification's container, for `why` (ADR-t2424-1): it was not run
    /// on the host instead, so nothing was judged.
    ContainerRefused { command: String, why: String },
}

impl RecheckFailure {
    /// Whether the command failed on the host rather than on the code
    /// ([`super::verify_failure::FailureClass::is_environmental`],
    /// ADR-t639-1): such a finding says nothing of whether the run lands,
    /// so it neither resumes nor holds the run.
    pub fn is_environmental(&self) -> bool {
        match self {
            Self::Conflict { .. } => false,
            Self::CheckFailed { failure, .. } => failure.class.is_environmental(),
            Self::ContainerRefused { .. } => true,
        }
    }

    /// A conflict is the landing's `rebase_conflict`, a failed command its
    /// `verification_failed` (ADR-0034's codes, ADR-0068 decision 3).
    pub const fn code(&self) -> ReasonCode {
        match self {
            Self::Conflict { .. } => ReasonCode::RebaseConflict,
            Self::CheckFailed { .. } | Self::ContainerRefused { .. } => {
                ReasonCode::VerificationFailed
            }
        }
    }

    /// The run's `last_error` once parked, and the reason of its resume.
    /// `landed` is `None` when main moved without a dagq landing.
    pub fn reason(&self, landed: Option<&Landed>, main: &CommitSha) -> String {
        let after = after(landed);
        match self {
            Self::Conflict { paths } => format!(
                "{after} that main {main} conflicts with the run in {}",
                paths.join(", ")
            ),
            Self::CheckFailed {
                command,
                exit_code,
                log_path,
                ..
            } => format!(
                "{after} that {command:?} exits with {exit_code} on main {main} with the run merged in (git merges it without a conflict); see {log_path}"
            ),
            Self::ContainerRefused { command, why } => format!(
                "{after} no container to run {command:?} in on main {main} with the run, whose worker works in a container, merged in: {why}; it did not run on the host instead"
            ),
        }
    }

    /// The payload of [`LANDING_RECHECK_FAILED`], without its `action`. A
    /// failed command's carries its `class` and `evidence` beside its log
    /// ([`VerifyFailure`]), which tell a host failure from a code failure.
    /// `landed_run_id` and `landed_task_id` are null when main moved
    /// without a dagq landing.
    pub fn payload(&self, landed: Option<&Landed>, main: &CommitSha, head: &CommitSha) -> Value {
        let mut payload = json!({
            "code": self.code(),
            "main": main,
            "head": head,
            "landed_run_id": landed.map(|l| &l.run_id),
            "landed_task_id": landed.map(|l| l.task_id),
        });
        match self {
            Self::Conflict { paths } => payload["conflicts"] = json!(paths),
            Self::CheckFailed {
                command,
                exit_code,
                log_path,
                output_tail,
                failure,
            } => {
                payload["command"] = json!(command);
                payload["exit_code"] = json!(exit_code);
                payload["log_path"] = json!(log_path);
                payload["output_tail"] = json!(output_tail);
                payload["class"] = json!(failure.class);
                payload["evidence"] = json!(failure.evidence);
            }
            Self::ContainerRefused { command, why } => {
                payload["command"] = json!(command);
                payload["container_refused"] = json!(why);
            }
        }
        payload
    }
}

/// What a recheck's finding starts with: the landing that moved main.
fn after(landed: Option<&Landed>) -> String {
    match landed {
        Some(landed) => format!(
            "after task {} (run {}) landed, the landing recheck found",
            landed.task_id, landed.run_id
        ),
        None => "after main moved without a dagq landing, the landing recheck found".to_owned(),
    }
}

/// The payload of [`LANDING_RECHECK_CLEAN`] (ADR-t1311-1). `command` is
/// null when only `git merge-tree` was looked at, and `landed_run_id` and
/// `landed_task_id` when main moved without a dagq landing. A run whose
/// diff touched none of `[recheck] paths` (`skipped_by_paths`) has a null
/// `command` and `command_skipped` [`SKIPPED_BY_PATHS`] (ADR-t2032-1), so
/// it is told apart from a recheck that had no command to run.
pub fn clean_payload(
    landed: Option<&Landed>,
    main: &CommitSha,
    head: &CommitSha,
    command: Option<&str>,
    skipped_by_paths: bool,
) -> Value {
    let mut payload = json!({
        "main": main,
        "head": head,
        "command": if skipped_by_paths { None } else { command },
        "landed_run_id": landed.map(|l| &l.run_id),
        "landed_task_id": landed.map(|l| l.task_id),
    });
    if skipped_by_paths {
        payload["command_skipped"] = json!(SKIPPED_BY_PATHS);
    }
    payload
}

/// How the recheck's paragraph in a run's open asks starts: the asks hold
/// one such paragraph, the latest finding's (ADR-t1311-1).
pub const ASK_NOTE_PREFIX: &str = "Landing recheck: ";

/// The paragraph a failure puts in the run's open asks (ADR-0068 decision
/// 4): what it found, and that the run is resumed without waiting for the
/// answer, which still applies once the run waits again.
pub fn ask_note(reason: &str, resumed: bool) -> String {
    let next = if resumed {
        "The supervisor resumes the run to bring it onto main without waiting for this answer; once it waits again, the answer applies to the rebased run."
    } else {
        "The supervisor parks the run for a resume instead of landing it once its session has exited."
    };
    format!("{ASK_NOTE_PREFIX}{reason}. {next}")
}

/// The paragraph a failure on the host puts in the run's open asks: what
/// the command met, and that the run keeps waiting to land as it was.
pub fn skipped_ask_note(reason: &str) -> String {
    format!(
        "{ASK_NOTE_PREFIX}{reason}. The host kept the command from judging the code: the run is not resumed and keeps waiting to land."
    )
}

/// The paragraph a command refused its container
/// ([`RecheckFailure::ContainerRefused`]) puts in the run's open asks: why
/// it did not run, and that the run keeps waiting to land as it was.
pub fn refused_ask_note(reason: &str) -> String {
    format!(
        "{ASK_NOTE_PREFIX}{reason}. Nothing judged the code: the run is not resumed and keeps waiting to land."
    )
}

/// The paragraph a clean finding puts in the run's open asks
/// (ADR-t1311-1): the main's short commit, that the run still lands there
/// cleanly, and whether a command was run on it.
pub fn clean_ask_note(landed: Option<&Landed>, main: &CommitSha, command: Option<&str>) -> String {
    let short = &main.as_str()[..main.as_str().len().min(12)];
    let checked = match command {
        Some(command) => format!(
            "git merges it without a conflict and {command:?} passes on main with the run merged in"
        ),
        None => "git merges it without a conflict (no command was run)".to_owned(),
    };
    format!(
        "{ASK_NOTE_PREFIX}{} that the run still lands cleanly on main {short}: {checked}.",
        after(landed)
    )
}

/// `question` with `note` as its recheck paragraph: any paragraph that
/// starts with [`ASK_NOTE_PREFIX`] is dropped and `note` ends the question,
/// so the asks show only the latest finding (ADR-t1311-1).
pub fn noted_question(question: &str, note: &str) -> String {
    let kept: Vec<&str> = question
        .split("\n\n")
        .filter(|paragraph| !paragraph.trim_start().starts_with(ASK_NOTE_PREFIX))
        .collect();
    format!("{}\n\n{note}", kept.join("\n\n").trim_end())
}

/// What a recorded [`LANDING_RECHECK_FAILED`] says followed the finding,
/// from its `action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailedAction {
    /// [`RESUMED`].
    Resumed,
    /// [`HELD`].
    Held,
    /// [`SKIPPED`].
    Skipped,
}

/// A recorded [`LANDING_RECHECK_FAILED`] restored as the typed values the
/// readers of other contexts decide by. It never refuses a payload: a
/// missing key, `null` or a value of another type (an older writer) reads
/// as absent, as `payload["key"]` reads it.
#[derive(Debug, Default, Deserialize)]
pub struct RecordedFailure<'a> {
    /// What followed the finding.
    #[serde(default, deserialize_with = "typed")]
    pub action: Option<FailedAction>,
    /// [`RecheckFailure::code`] as recorded.
    #[serde(default, borrow, deserialize_with = "typed")]
    pub code: Option<&'a str>,
    /// `true` on a held run's parking, which repeats its finding.
    #[serde(default, deserialize_with = "typed")]
    pub repeat: Option<bool>,
    /// `true` on a [`SKIPPED`] failure the command met on main alone too
    /// ([`kept_by_main`]): a failure of the code, not of the host.
    #[serde(default, deserialize_with = "typed")]
    pub main_failed: Option<bool>,
}

impl<'a> RecordedFailure<'a> {
    pub fn restore(event: &'a RunEvent) -> Self {
        restore(&event.payload)
    }

    /// Whether the host kept the command from judging the code
    /// ([`SKIPPED`]): the finding parked and held nothing.
    pub fn skipped(&self) -> bool {
        self.action == Some(FailedAction::Skipped)
    }

    /// Whether the host or a missing container kept the command from
    /// judging the code: [`Self::skipped`] but for main's failure
    /// ([`Self::main_failed`]), which judged it.
    pub fn environmental(&self) -> bool {
        self.skipped() && self.main_failed != Some(true)
    }
}

/// Whether the event parked its run for a session.
pub fn parks(event: &RunEvent) -> bool {
    event.kind == EventKind::LandingRecheckFailed && event.payload["action"] == RESUMED
}

/// The failure a recheck recorded as [`HELD`] on the run against `main`
/// with `head`, when it is the run's latest recheck failure: the run would
/// land a head known not to land there.
pub fn held_against<'a>(events: &'a [RunEvent], main: &str, head: &str) -> Option<&'a RunEvent> {
    events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::LandingRecheckFailed)
        .filter(|e| {
            e.payload["action"] == HELD && e.payload["main"] == main && e.payload["head"] == head
        })
}

/// The dagq landing whose commit `main` is, from the newest
/// `run_integrated` (`latest_integrated`): when it landed that commit
/// (`result_commit` or `commit`). `None` when main moved without a dagq
/// landing, or since one.
pub fn landing_at(latest_integrated: Option<&RunEvent>, main: &str) -> Option<Landed> {
    let event = latest_integrated?;
    let (Some(run_id), Some(task_id)) = (event.run_id.clone(), event.task_id) else {
        return None;
    };
    (event.payload["result_commit"] == main || event.payload["commit"] == main)
        .then_some(Landed { run_id, task_id })
}

/// Whether the run's `head` was judged against `main` already, from its
/// `events`: by its passed review's conflict precheck (ADR-0027 decision
/// 4), by its landing that parked it, or by a recheck, failed or clean.
/// What that found is handled, and is not judged again.
///
/// A clean recheck with a null `command` judged only the merge. When the
/// supervisor that asks would run a command (`runs_command`: `[recheck]
/// command` is set and its program is found), such a clean settles the
/// head only if it records the command was skipped by `[recheck] paths`
/// ([`SKIPPED_BY_PATHS`]); one recorded where the command could not run
/// (its program missing, or a record from before the paths) does not,
/// and the run goes through the recheck's paths decision again
/// ([`runs_command`]).
pub fn judged_against(events: &[RunEvent], main: &str, head: &str, runs_command: bool) -> bool {
    events.iter().any(|event| {
        let settles = match event.kind.known() {
            Some(
                EventKind::ConflictPrecheck
                | EventKind::IntegrationDeferred
                | EventKind::LandingRecheckFailed,
            ) => true,
            Some(EventKind::LandingRecheckClean) => {
                !runs_command
                    || !event.payload["command"].is_null()
                    || event.payload["command_skipped"] == SKIPPED_BY_PATHS
            }
            _ => false,
        };
        settles && event.payload["main"] == main && event.payload["head"] == head
    })
}

/// Who checked a main already.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckedBy {
    No,
    /// The supervisor that asks.
    This,
    /// Another supervisor: it could not see the runs this one holds to
    /// land.
    Other,
}

/// Who checked `main` already, from the recent `landing_recheck_finished`
/// (`finished`) and the token of the supervisor that asks.
pub fn checked_by(finished: &[RunEvent], main: &str, supervisor: &str) -> CheckedBy {
    let mut checked = CheckedBy::No;
    for event in finished
        .iter()
        .filter(|event| event.payload["main"] == main)
    {
        if event.payload["supervisor"] == supervisor {
            return CheckedBy::This;
        }
        checked = CheckedBy::Other;
    }
    checked
}

/// Recorded on the queue when a recheck could not write its build output
/// (`<queue dir>/recheck/<output>`, the Rust profile's `target`) and so ran
/// no command in that pass, after anything but this kind: an attention of
/// the inbox's ([`crate::domain::AttentionNext::FixRecheckTarget`]). Its
/// payload holds the `path` the check could not write, the OS's `error`,
/// the `main` checked and how many runs it left `unrecorded`. A command
/// that cannot write where it builds fails on every run whatever its code,
/// so the runs it would have judged get no record and stay awaiting
/// integration, as when [run.env] names a missing program (ADR-0068
/// decision 2, ADR-0049 decision 9).
pub const LANDING_RECHECK_TARGET_UNWRITABLE: &str =
    crate::domain::event_kind::EventKind::LandingRecheckTargetUnwritable.as_str();

/// Recorded when a recheck wrote its build output again after a
/// [`LANDING_RECHECK_TARGET_UNWRITABLE`] (`main`): no attention, and the
/// attention ends with it.
pub const LANDING_RECHECK_TARGET_WRITABLE: &str =
    crate::domain::event_kind::EventKind::LandingRecheckTargetWritable.as_str();

/// The two kinds, for reading the latest of them.
pub const TARGET_KINDS: [&str; 2] = [
    LANDING_RECHECK_TARGET_UNWRITABLE,
    LANDING_RECHECK_TARGET_WRITABLE,
];

/// `command_skipped_reason` of a `landing_recheck_finished` whose pass ran
/// no command because its build output could not be written; its count
/// `target_unwritable` is the runs that went unrecorded for it. Apart from
/// `command_skipped`, the runs whose diff missed `[recheck] paths`.
pub const TARGET_UNWRITABLE: &str = "target_unwritable";

/// What the write check of a recheck's build output found, made before the
/// first run whose command would run in the pass, and only then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetCheck {
    Writable,
    /// `path` could not be made or written for the OS's `error`.
    Unwritable {
        path: String,
        error: String,
    },
}

impl TargetCheck {
    /// The queue event a pass whose check found `self` records against
    /// `main`, given the latest of [`TARGET_KINDS`] on the queue (`last`)
    /// and the runs it left unrecorded: unwritable after anything but
    /// unwritable, writable after unwritable, else none, so main moving
    /// while it stays unwritable does not wake the inbox again.
    pub fn transition(
        &self,
        last: Option<&super::RecordedKind>,
        main: &CommitSha,
        unrecorded: u64,
    ) -> Option<(EventKind, Value)> {
        let was_unwritable =
            last.is_some_and(|kind| *kind == EventKind::LandingRecheckTargetUnwritable);
        match self {
            Self::Unwritable { path, error } if !was_unwritable => Some((
                EventKind::LandingRecheckTargetUnwritable,
                json!({
                    "path": path,
                    "error": error,
                    "main": main,
                    "unrecorded": unrecorded,
                }),
            )),
            Self::Writable if was_unwritable => Some((
                EventKind::LandingRecheckTargetWritable,
                json!({"main": main}),
            )),
            _ => None,
        }
    }
}

/// The landing recheck's command failed on main's tree alone, run once in
/// a pass whose command failed a run for the code (ADR-t2141-1), recorded
/// on the queue when the latest of [`LANDING_RECHECK_MAIN_KINDS`] is not
/// one already: `main`, `command`, `exit_code`, `class`, `evidence`,
/// `log_path` (`recheck/main-<main's first 12>.log` of the queue's
/// directory), `failed_runs` (how many runs it kept from a resume) and
/// `supervisor`. An attention (`fix main or the recheck environment`)
/// for the inbox: the runs' failures are main's or the recheck
/// environment's, and resuming the runs would not fix them. Its log is
/// kept while it is the latest of the two, and the log an open ask
/// points at while it does, which may be a later main's: each check of
/// main removes the other logs of main's checks ([`stale_main_logs`]).
pub const LANDING_RECHECK_MAIN_FAILED: &str =
    crate::domain::event_kind::EventKind::LandingRecheckMainFailed.as_str();

/// The command passed on main alone after a
/// [`LANDING_RECHECK_MAIN_FAILED`] (`main`, `command`, `log_path`,
/// `supervisor`); the attention ends with it, and it is no attention
/// itself. Not recorded after another pass: main's check passing is
/// the usual case.
pub const LANDING_RECHECK_MAIN_PASSED: &str =
    crate::domain::event_kind::EventKind::LandingRecheckMainPassed.as_str();

/// The two kinds, for reading the latest of them.
pub const LANDING_RECHECK_MAIN_KINDS: [&str; 2] =
    [LANDING_RECHECK_MAIN_FAILED, LANDING_RECHECK_MAIN_PASSED];

/// What the recheck's command found on main's tree alone, with no run
/// merged in (ADR-t2141-1). It runs once in a pass, only when the command
/// failed at least one run on the host for the code
/// ([`checks_main`]), or, while the latest record of it is
/// [`LANDING_RECHECK_MAIN_FAILED`], passed one, on the host in the
/// recheck's scratch worktree and build outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MainCheck {
    /// The command passed on main: the runs' failures are theirs.
    Passed { log_path: String },
    /// The command failed on main for the code: the runs' failures say
    /// nothing of the runs.
    Failed {
        exit_code: i32,
        log_path: String,
        failure: VerifyFailure,
    },
    /// Git or the command could not be run, or the command failed on the
    /// host ([`super::verify_failure::FailureClass::is_environmental`]):
    /// nothing is told of main, and the runs are handled as without it.
    Unjudged(String),
}

impl MainCheck {
    /// `main_check` of `landing_recheck_finished`.
    pub const fn outcome(&self) -> &'static str {
        match self {
            Self::Passed { .. } => "passed",
            Self::Failed { .. } => "failed",
            Self::Unjudged(_) => "unjudged",
        }
    }

    /// The log of main's check when main failed it.
    pub fn failed_log(&self) -> Option<&str> {
        match self {
            Self::Failed { log_path, .. } => Some(log_path),
            _ => None,
        }
    }

    /// The event to record on the queue after this check, given the
    /// latest of [`LANDING_RECHECK_MAIN_KINDS`] (`last`): the failure
    /// unless the latest is a failure already, the pass only after a
    /// failure, and nothing for a check that judged nothing. So a main
    /// that stays broken, or that passes as it usually does, does not
    /// wake the inbox on each recheck. `failed_runs` is how many runs the
    /// failure kept from a resume.
    pub fn transition(
        &self,
        last: Option<&RecordedKind>,
        main: &CommitSha,
        command: &str,
        failed_runs: u64,
    ) -> Option<(EventKind, Value)> {
        let was_failed = last.is_some_and(|kind| *kind == EventKind::LandingRecheckMainFailed);
        match self {
            Self::Failed {
                exit_code,
                log_path,
                failure,
            } if !was_failed => Some((
                EventKind::LandingRecheckMainFailed,
                json!({
                    "main": main,
                    "command": command,
                    "exit_code": exit_code,
                    "class": failure.class,
                    "evidence": failure.evidence,
                    "log_path": log_path,
                    "failed_runs": failed_runs,
                }),
            )),
            Self::Passed { log_path } if was_failed => Some((
                EventKind::LandingRecheckMainPassed,
                json!({"main": main, "command": command, "log_path": log_path}),
            )),
            _ => None,
        }
    }
}

/// Whether `failure`, of a run whose command ran on the host
/// (`on_host`), has the recheck check main alone (ADR-t2141-1): a
/// command that failed for the code. A conflict, a failure on the host
/// and a run in a container (whose command does not run where main's
/// check does) do not.
pub fn checks_main(failure: &RecheckFailure, on_host: bool) -> bool {
    on_host && matches!(failure, RecheckFailure::CheckFailed { .. }) && !failure.is_environmental()
}

/// Whether `failure` of a run is kept from a resume because the command
/// failed on main alone (`main`, the pass's check of main; ADR-t2141-1):
/// such a run is recorded [`SKIPPED`] with `main_failed` and keeps
/// waiting to land, using no resume. With main passing, unjudged or not
/// checked, the run is handled as ADR-0068 decision 3 says, however many
/// runs failed the same way.
pub fn kept_by_main(failure: &RecheckFailure, on_host: bool, main: Option<&MainCheck>) -> bool {
    checks_main(failure, on_host) && matches!(main, Some(MainCheck::Failed { .. }))
}

/// The paragraph a failure kept by main's failure ([`kept_by_main`])
/// puts in the run's open asks: what the run met, that main fails alone,
/// and that the run keeps waiting to land.
pub fn main_failed_ask_note(reason: &str, main_log: &str) -> String {
    format!("{ASK_NOTE_PREFIX}{reason}.{MAIN_FAILED_SEE}{main_log}{MAIN_FAILED_END}")
}

/// What precedes main's log in [`main_failed_ask_note`], by which
/// [`asked_main_log`] finds it again.
const MAIN_FAILED_SEE: &str = " The command fails on main alone too (see ";

/// What ends [`main_failed_ask_note`] after main's log: the log ends
/// where it starts, whatever the log's path holds.
const MAIN_FAILED_END: &str = "): the run is not resumed and keeps waiting to land.";

/// The name of the log of main's check of `main` in the recheck's
/// directory: `main-<main's first 12>.log`.
pub fn main_log_name(main: &CommitSha) -> String {
    format!("main-{}.log", &main.as_str()[..main.as_str().len().min(12)])
}

/// Whether `name`, an entry of the recheck's directory, is the log of a
/// check of main ([`main_log_name`]): only these are ever removed
/// ([`stale_main_logs`]), never the scratch worktree, a build output or
/// the lock.
pub fn is_main_log(name: &str) -> bool {
    name.len() > "main-.log".len() && name.starts_with("main-") && name.ends_with(".log")
}

/// The log of main's check an ask's `question` points at: the one its
/// recheck paragraph names when it is [`main_failed_ask_note`]'s.
pub fn asked_main_log(question: &str) -> Option<&str> {
    let paragraph = question
        .rsplit("\n\n")
        .find(|paragraph| paragraph.trim_start().starts_with(ASK_NOTE_PREFIX))?;
    let (_, log) = paragraph
        .trim_end()
        .strip_suffix(MAIN_FAILED_END)?
        .rsplit_once(MAIN_FAILED_SEE)?;
    Some(log)
}

/// The log [`LANDING_RECHECK_MAIN_FAILED`] points at when it is the
/// latest of [`LANDING_RECHECK_MAIN_KINDS`] (`latest`): the inbox's
/// attention shows it.
pub fn attention_main_log(latest: Option<&RunEvent>) -> Option<&str> {
    latest
        .filter(|event| event.kind == EventKind::LandingRecheckMainFailed)
        .and_then(|event| event.payload["log_path"].as_str())
}

/// The logs of main's checks to remove from the recheck's directory once
/// a check of main wrote `written` there: of its entries `names`, each
/// log of a check of main ([`is_main_log`]) that is none of these, which
/// are kept, compared by file name (the queue's directory may have moved
/// since a path was recorded):
/// (a) `written`, the log just written;
/// (b) `attention`, the log of [`LANDING_RECHECK_MAIN_FAILED`] while it
///     is the latest of [`LANDING_RECHECK_MAIN_KINDS`]
///     ([`attention_main_log`]), which the inbox's attention points at;
/// (c) `asked`, the logs the recheck paragraph of an ask nobody closed
///     (answered or not) points at ([`asked_main_log`]).
/// (b) alone is not enough: [`MainCheck::transition`] records no event
/// on a failure after a failure, so with main A, B and C failing one
/// after another the event still points at A while a run's asks, noted
/// on each pass, point at B; when C's check then passes and that run is
/// not recorded again (it moved on, or Git could not judge it), its asks
/// still point at B. So each check of main leaves at most the logs
/// somebody is pointed at, and they do not pile up as main moves.
pub fn stale_main_logs<'a>(
    names: &'a [String],
    written: &str,
    attention: Option<&str>,
    asked: &[&str],
) -> Vec<&'a str> {
    fn file_name(path: &str) -> &str {
        std::path::Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path)
    }
    let mut kept: Vec<&str> = vec![file_name(written)];
    kept.extend(attention.map(file_name));
    kept.extend(asked.iter().map(|log| file_name(log)));
    names
        .iter()
        .map(String::as_str)
        .filter(|name| is_main_log(name) && !kept.contains(name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;

    #[test]
    fn records_only_the_changes_between_unwritable_and_writable() {
        let main = CommitSha::parse("a".repeat(40), "commit").unwrap();
        let unwritable = TargetCheck::Unwritable {
            path: "/q/recheck/target".into(),
            error: "Operation not permitted (os error 1)".into(),
        };
        let writable_kind =
            crate::domain::RecordedKind::from(EventKind::LandingRecheckTargetWritable);
        let unwritable_kind =
            crate::domain::RecordedKind::from(EventKind::LandingRecheckTargetUnwritable);
        for last in [None, Some(&writable_kind)] {
            let (kind, payload) = unwritable.transition(last, &main, 3).unwrap();
            assert_eq!(kind, EventKind::LandingRecheckTargetUnwritable);
            assert_eq!(
                payload,
                json!({
                    "path": "/q/recheck/target",
                    "error": "Operation not permitted (os error 1)",
                    "main": "a".repeat(40),
                    "unrecorded": 3,
                })
            );
        }
        assert_eq!(
            unwritable.transition(Some(&unwritable_kind), &main, 3),
            None
        );
        let (kind, payload) = TargetCheck::Writable
            .transition(Some(&unwritable_kind), &main, 0)
            .unwrap();
        assert_eq!(kind, EventKind::LandingRecheckTargetWritable);
        assert_eq!(payload, json!({"main": "a".repeat(40)}));
        assert_eq!(TargetCheck::Writable.transition(None, &main, 0), None);
        assert_eq!(
            TargetCheck::Writable.transition(Some(&writable_kind), &main, 0),
            None
        );
    }

    fn landed() -> Landed {
        Landed {
            run_id: RunId::new("landed-run").unwrap(),
            task_id: TaskId::new(7),
        }
    }

    fn event(payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: LANDING_RECHECK_FAILED.into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    fn check_failed(class: super::super::verify_failure::FailureClass) -> RecheckFailure {
        RecheckFailure::CheckFailed {
            command: "cargo check".into(),
            exit_code: 101,
            log_path: "/runs/r/recheck-m.log".into(),
            output_tail: "error: could not compile".into(),
            failure: VerifyFailure {
                class,
                evidence: "error: could not compile".into(),
                profile: None,
            },
        }
    }

    fn main_failed() -> MainCheck {
        MainCheck::Failed {
            exit_code: 101,
            log_path: "/q/recheck/main-m.log".into(),
            failure: VerifyFailure {
                class: super::super::verify_failure::FailureClass::BuildError,
                evidence: "error: could not compile".into(),
                profile: None,
            },
        }
    }

    /// Main alone is checked only for a command that failed a run on the
    /// host for the code; a run whose failure it is is kept from a resume
    /// only when main failed too (ADR-t2141-1). A pass on main, a check
    /// that judged nothing, or none, leave the run to its resume.
    #[test]
    fn a_runs_code_failure_is_kept_from_its_resume_only_when_main_fails_alone_too() {
        use super::super::verify_failure::FailureClass::*;
        let code = check_failed(BuildError);
        let host = check_failed(DiskFull);
        let conflict = RecheckFailure::Conflict {
            paths: vec!["x.rs".into()],
        };
        let refused = RecheckFailure::ContainerRefused {
            command: "c".into(),
            why: "w".into(),
        };
        for class in [
            BuildError,
            Lint,
            TestFailure,
            Format,
            CoverageBelow,
            Unknown,
        ] {
            assert!(checks_main(&check_failed(class), true), "{class:?}");
        }
        assert!(!checks_main(&code, false));
        assert!(!checks_main(&host, true));
        assert!(!checks_main(&conflict, true));
        assert!(!checks_main(&refused, true));

        let failed = main_failed();
        let passed = MainCheck::Passed {
            log_path: "/q/recheck/main-m.log".into(),
        };
        let unjudged = MainCheck::Unjudged("git did not start".into());
        assert!(kept_by_main(&code, true, Some(&failed)));
        assert!(!kept_by_main(&code, true, Some(&passed)));
        assert!(!kept_by_main(&code, true, Some(&unjudged)));
        assert!(!kept_by_main(&code, true, None));
        assert!(!kept_by_main(&code, false, Some(&failed)));
        assert!(!kept_by_main(&conflict, true, Some(&failed)));
        assert!(!kept_by_main(&host, true, Some(&failed)));
        assert_eq!(failed.failed_log(), Some("/q/recheck/main-m.log"));
        assert_eq!(passed.failed_log(), None);
        assert_eq!(
            [failed.outcome(), passed.outcome(), unjudged.outcome()],
            ["failed", "passed", "unjudged"]
        );
        let note = main_failed_ask_note("it failed", "/q/recheck/main-m.log");
        assert!(note.starts_with(ASK_NOTE_PREFIX), "{note}");
        assert!(note.contains("fails on main alone too"), "{note}");
    }

    /// Each check of main keeps the log it wrote, the attention's while
    /// main's failure is the latest record, and those the open asks point
    /// at; it removes the other logs of main's checks and nothing else.
    #[test]
    fn a_check_of_main_keeps_only_the_logs_somebody_is_pointed_at() {
        let names = |list: &[&str]| list.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        let a = "/q/recheck/main-aaaaaaaaaaaa.log";
        let b = "/q/recheck/main-bbbbbbbbbbbb.log";
        let c = "/q/recheck/main-cccccccccccc.log";
        let entries = names(&[
            "main-aaaaaaaaaaaa.log",
            "main-bbbbbbbbbbbb.log",
            "main-cccccccccccc.log",
            "main-dddddddddddd.log",
            "lock",
            "target",
            "dagq-recheck-q",
            "main-.log",
            "main-notes.txt",
        ]);
        // Nobody points at an older log: only the one written is kept.
        assert_eq!(
            stale_main_logs(&entries, c, None, &[]),
            [
                "main-aaaaaaaaaaaa.log",
                "main-bbbbbbbbbbbb.log",
                "main-dddddddddddd.log"
            ]
        );
        // Main A, B and C fail one after another: the event points at A,
        // an open ask at B, and C was just written. A, B and C are kept.
        assert_eq!(
            stale_main_logs(&entries, c, Some(a), &[b]),
            ["main-dddddddddddd.log"]
        );
        // Once the ask closed, the next write lets B go.
        assert_eq!(
            stale_main_logs(&entries, c, Some(a), &[]),
            ["main-bbbbbbbbbbbb.log", "main-dddddddddddd.log"]
        );
        // Compared by name: a queue directory that moved keeps them too.
        assert_eq!(
            stale_main_logs(
                &entries,
                "/new/recheck/main-cccccccccccc.log",
                Some("/old/recheck/main-aaaaaaaaaaaa.log"),
                &[]
            ),
            ["main-bbbbbbbbbbbb.log", "main-dddddddddddd.log"]
        );
        assert!(stale_main_logs(&[], c, None, &[]).is_empty());
        assert!(is_main_log("main-aaaaaaaaaaaa.log"));
        assert!(!is_main_log("main-.log"));
        assert!(!is_main_log("lock"));
        let main = CommitSha::parse("c".repeat(40), "commit").unwrap();
        assert_eq!(main_log_name(&main), "main-cccccccccccc.log");

        // Only the latest record of main's failure points at a log.
        let failed = RunEvent {
            kind: LANDING_RECHECK_MAIN_FAILED.into(),
            ..event(json!({"log_path": a}))
        };
        let passed = RunEvent {
            kind: LANDING_RECHECK_MAIN_PASSED.into(),
            ..event(json!({"log_path": c}))
        };
        assert_eq!(attention_main_log(Some(&failed)), Some(a));
        assert_eq!(attention_main_log(Some(&passed)), None);
        assert_eq!(attention_main_log(None), None);

        // An ask points at the log its recheck paragraph names when main
        // failed alone too, not at a clean or another failure's paragraph.
        let question = noted_question(
            "Should it land?",
            &main_failed_ask_note("it failed (see /r/recheck-x.log)", b),
        );
        assert_eq!(asked_main_log(&question), Some(b));
        // A queue's path that holds the note's own punctuation is read
        // whole, so its log is kept.
        let odd = "/tmp/queue): data/recheck/main-bbbbbbbbbbbb.log";
        let question = noted_question(
            "Should it land?",
            &main_failed_ask_note("it failed (see /tmp/queue): data/r.log)", odd),
        );
        assert_eq!(asked_main_log(&question), Some(odd));
        assert_eq!(
            stale_main_logs(&entries, c, Some(a), &[asked_main_log(&question).unwrap()]),
            ["main-dddddddddddd.log"]
        );
        let cleaned = noted_question(&question, &clean_ask_note(None, &main, Some("c")));
        assert_eq!(asked_main_log(&cleaned), None);
        assert_eq!(
            asked_main_log(&noted_question("q", &ask_note("it failed", true))),
            None
        );
        assert_eq!(asked_main_log("Should it land?"), None);
    }

    /// Main's failure is recorded unless the latest record is a failure;
    /// its pass only after a failure; an unjudged check never (ADR-t2141-1).
    #[test]
    fn main_alone_is_told_once_when_it_fails_and_once_when_it_passes_again() {
        let main = CommitSha::parse("a".repeat(40), "commit").unwrap();
        let failed = main_failed();
        let passed = MainCheck::Passed {
            log_path: "/q/recheck/main-m.log".into(),
        };
        let unjudged = MainCheck::Unjudged("x".into());
        let was_failed: RecordedKind = EventKind::LandingRecheckMainFailed.into();
        let was_passed: RecordedKind = EventKind::LandingRecheckMainPassed.into();
        let (kind, payload) = failed.transition(None, &main, "cargo check", 3).unwrap();
        assert_eq!(kind, EventKind::LandingRecheckMainFailed);
        assert_eq!(
            payload,
            json!({
                "main": main,
                "command": "cargo check",
                "exit_code": 101,
                "class": "build_error",
                "evidence": "error: could not compile",
                "log_path": "/q/recheck/main-m.log",
                "failed_runs": 3,
            })
        );
        assert_eq!(
            failed
                .transition(Some(&was_passed), &main, "c", 1)
                .map(|(kind, _)| kind),
            Some(EventKind::LandingRecheckMainFailed)
        );
        assert!(
            failed
                .transition(Some(&was_failed), &main, "c", 1)
                .is_none()
        );
        let (kind, payload) = passed
            .transition(Some(&was_failed), &main, "cargo check", 0)
            .unwrap();
        assert_eq!(kind, EventKind::LandingRecheckMainPassed);
        assert_eq!(
            payload,
            json!({"main": main, "command": "cargo check", "log_path": "/q/recheck/main-m.log"})
        );
        assert!(passed.transition(None, &main, "c", 0).is_none());
        assert!(
            passed
                .transition(Some(&was_passed), &main, "c", 0)
                .is_none()
        );
        for last in [None, Some(&was_failed), Some(&was_passed)] {
            assert!(unjudged.transition(last, &main, "c", 0).is_none());
        }
    }

    #[test]
    fn a_conflict_names_the_landing_and_the_paths() {
        let main = CommitSha::parse("a".repeat(40), "commit").unwrap();
        let head = CommitSha::parse("b".repeat(40), "commit").unwrap();
        let failure = RecheckFailure::Conflict {
            paths: vec!["x.rs".into(), "y.rs".into()],
        };
        assert_eq!(failure.code(), ReasonCode::RebaseConflict);
        let reason = failure.reason(Some(&landed()), &main);
        assert_eq!(
            reason,
            format!(
                "after task 7 (run landed-run) landed, the landing recheck found that main {main} conflicts with the run in x.rs, y.rs"
            )
        );
        assert_eq!(
            failure.payload(Some(&landed()), &main, &head),
            json!({
                "code": "rebase_conflict",
                "main": main,
                "head": head,
                "landed_run_id": "landed-run",
                "landed_task_id": 7,
                "conflicts": ["x.rs", "y.rs"],
            })
        );
        assert!(ask_note(&reason, true).contains("without waiting for this answer"));
        assert!(ask_note(&reason, false).contains("once its session has exited"));
    }

    #[test]
    fn a_main_moved_without_a_landing_names_no_run() {
        let main = CommitSha::parse("a".repeat(40), "commit").unwrap();
        let head = CommitSha::parse("b".repeat(40), "commit").unwrap();
        let failure = RecheckFailure::Conflict {
            paths: vec!["x.rs".into()],
        };
        assert_eq!(
            failure.reason(None, &main),
            format!(
                "after main moved without a dagq landing, the landing recheck found that main {main} conflicts with the run in x.rs"
            )
        );
        let payload = failure.payload(None, &main, &head);
        assert_eq!(payload["landed_run_id"], Value::Null);
        assert_eq!(payload["landed_task_id"], Value::Null);
    }

    #[test]
    fn a_failed_check_names_the_command_and_its_log() {
        let main = CommitSha::parse("a".repeat(40), "commit").unwrap();
        let head = CommitSha::parse("b".repeat(40), "commit").unwrap();
        let failure = RecheckFailure::CheckFailed {
            command: "cargo check".into(),
            exit_code: 101,
            log_path: "/r/recheck.log".into(),
            output_tail: "error[E0063]".into(),
            failure: crate::domain::verify_failure::classify(
                &crate::domain::toolchain::Toolchains::this_repository(),
                "cargo check",
                Some(101),
                None,
                "error[E0063]: missing field",
            ),
        };
        assert_eq!(failure.code(), ReasonCode::VerificationFailed);
        let reason = failure.reason(Some(&landed()), &main);
        assert!(
            reason.contains("\"cargo check\" exits with 101"),
            "{reason}"
        );
        assert!(reason.ends_with("see /r/recheck.log"), "{reason}");
        let payload = failure.payload(Some(&landed()), &main, &head);
        assert_eq!(payload["code"], "verification_failed");
        assert_eq!(payload["exit_code"], 101);
        assert_eq!(payload["output_tail"], "error[E0063]");
        assert_eq!(payload["class"], "build_error");
        assert!(!failure.is_environmental());
    }

    /// A command of a run in a container with no container to run in
    /// judges nothing (ADR-t2424-1): it skips the resume and the hold like
    /// a failure on the host, and says why it did not run.
    #[test]
    fn a_command_refused_its_container_judges_nothing_and_says_why() {
        let main = CommitSha::parse("a".repeat(40), "commit").unwrap();
        let head = CommitSha::parse("b".repeat(40), "commit").unwrap();
        let failure = RecheckFailure::ContainerRefused {
            command: "cargo check".into(),
            why: "the dagq machine is not running".into(),
        };
        assert!(failure.is_environmental());
        assert_eq!(failure.code(), ReasonCode::VerificationFailed);
        let reason = failure.reason(Some(&landed()), &main);
        assert!(reason.contains("\"cargo check\""), "{reason}");
        assert!(
            reason.contains("the dagq machine is not running; it did not run on the host instead"),
            "{reason}"
        );
        let payload = failure.payload(Some(&landed()), &main, &head);
        assert_eq!(payload["code"], "verification_failed");
        assert_eq!(payload["command"], "cargo check");
        assert_eq!(
            payload["container_refused"],
            "the dagq machine is not running"
        );
        assert!(payload.get("exit_code").is_none());
        assert!(refused_ask_note(&reason).ends_with("keeps waiting to land."));
    }

    /// Only the host's classes skip the resume and the hold; a conflict
    /// never does.
    #[test]
    fn only_a_failure_on_the_host_is_skipped() {
        use crate::domain::verify_failure::FailureClass;
        let sha = CommitSha::parse("a".repeat(40), "commit").unwrap();
        for (class, environmental) in [
            (FailureClass::DiskFull, true),
            (FailureClass::Killed, true),
            (FailureClass::Timeout, true),
            (FailureClass::BuildError, false),
            (FailureClass::TestFailure, false),
            (FailureClass::Unknown, false),
        ] {
            let failure = RecheckFailure::CheckFailed {
                command: "check".into(),
                exit_code: 1,
                log_path: "/log".into(),
                output_tail: String::new(),
                failure: VerifyFailure {
                    class,
                    evidence: "evidence".into(),
                    profile: None,
                },
            };
            assert_eq!(failure.is_environmental(), environmental, "{class:?}");
            let payload = failure.payload(None, &sha, &sha);
            assert_eq!(payload["class"], json!(class));
            assert_eq!(payload["evidence"], "evidence");
        }
        assert!(!RecheckFailure::Conflict { paths: vec![] }.is_environmental());
        assert!(skipped_ask_note("found").contains("keeps waiting to land"));
    }

    #[test]
    fn a_clean_finding_names_the_main_and_whether_a_command_ran() {
        let main = CommitSha::parse("a".repeat(40), "commit").unwrap();
        let head = CommitSha::parse("b".repeat(40), "commit").unwrap();
        assert_eq!(
            clean_ask_note(Some(&landed()), &main, Some("cargo check")),
            "Landing recheck: after task 7 (run landed-run) landed, the landing recheck found that the run still lands cleanly on main aaaaaaaaaaaa: git merges it without a conflict and \"cargo check\" passes on main with the run merged in."
        );
        assert_eq!(
            clean_ask_note(None, &main, None),
            "Landing recheck: after main moved without a dagq landing, the landing recheck found that the run still lands cleanly on main aaaaaaaaaaaa: git merges it without a conflict (no command was run)."
        );
        assert_eq!(
            clean_payload(Some(&landed()), &main, &head, None, false),
            json!({
                "main": main,
                "head": head,
                "command": null,
                "landed_run_id": "landed-run",
                "landed_task_id": 7,
            })
        );
        assert_eq!(
            clean_payload(None, &main, &head, Some("cargo check"), false)["command"],
            "cargo check"
        );
        assert_eq!(
            clean_payload(None, &main, &head, Some("cargo check"), true),
            json!({
                "main": main,
                "head": head,
                "command": null,
                "command_skipped": "paths",
                "landed_run_id": null,
                "landed_task_id": null,
            })
        );
    }

    /// The command runs on every run without `[recheck] paths`, and with
    /// them only on a run whose diff touches one (ADR-t2032-1).
    #[test]
    fn the_command_runs_only_when_the_diff_touches_a_path() {
        let paths = ["**/*.rs".to_owned(), "Cargo.lock".to_owned()];
        let changed = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        assert!(runs_command(&[], &changed(&["docs/a.md"])));
        assert!(runs_command(&[], &[]));
        assert!(runs_command(&paths, &changed(&["docs/a.md", "src/lib.rs"])));
        assert!(runs_command(&paths, &changed(&["Cargo.lock"])));
        assert!(!runs_command(&paths, &changed(&["docs/a.md", "dagq.toml"])));
        assert!(!runs_command(&paths, &changed(&["sub/Cargo.lock"])));
        assert!(!runs_command(&paths, &[]));
    }

    #[test]
    fn the_latest_note_replaces_every_earlier_recheck_paragraph() {
        let question = "Land it?\n\nDetails.";
        let once = noted_question(question, "Landing recheck: first.");
        assert_eq!(once, "Land it?\n\nDetails.\n\nLanding recheck: first.");
        let twice = noted_question(&once, "Landing recheck: second.");
        assert_eq!(twice, "Land it?\n\nDetails.\n\nLanding recheck: second.");
        // Paragraphs an earlier binary stacked go too.
        let stacked = format!("{once}\n\nLanding recheck: older.");
        assert_eq!(noted_question(&stacked, "Landing recheck: second."), twice);
        assert_eq!(noted_question(&twice, "Landing recheck: second."), twice);
        // A question that ends in a newline does not stack them either.
        let trailing = noted_question("Land it?\n", "Landing recheck: first.");
        assert_eq!(trailing, "Land it?\n\nLanding recheck: first.");
        let stacked = "Land it?\n\n\nLanding recheck: older.";
        assert_eq!(noted_question(stacked, "Landing recheck: first."), trailing);
    }

    #[test]
    fn only_the_latest_held_failure_against_the_same_main_and_head_holds() {
        let held =
            |main: &str, head: &str| event(json!({"action": HELD, "main": main, "head": head}));
        let events = [held("m", "h")];
        assert!(held_against(&events, "m", "h").is_some());
        assert!(held_against(&events, "m2", "h").is_none());
        assert!(held_against(&events, "m", "h2").is_none());
        let resumed = event(json!({"action": RESUMED, "main": "m", "head": "h"}));
        assert!(parks(&resumed));
        assert!(!parks(&events[0]));
        assert!(held_against(&[held("m", "h"), resumed], "m", "h").is_none());
        // A failure on the host neither parks nor holds the run.
        let skipped = event(json!({"action": SKIPPED, "main": "m", "head": "h"}));
        assert!(!parks(&skipped));
        assert!(held_against(&[held("m", "h"), skipped], "m", "h").is_none());
    }

    /// The typed reading of a recorded failure names what followed it, and
    /// reads an older or broken payload's keys as absent.
    #[test]
    fn a_recorded_failure_is_restored_as_typed_values() {
        let recorded = event(json!({
            "action": SKIPPED,
            "code": "verification_failed",
            "repeat": true,
        }));
        let failure = RecordedFailure::restore(&recorded);
        assert_eq!(failure.action, Some(FailedAction::Skipped));
        assert!(failure.skipped());
        assert_eq!(failure.code, Some("verification_failed"));
        assert_eq!(failure.repeat, Some(true));
        for (action, typed) in [(RESUMED, FailedAction::Resumed), (HELD, FailedAction::Held)] {
            let failure = event(json!({"action": action}));
            let failure = RecordedFailure::restore(&failure);
            assert_eq!(failure.action, Some(typed));
            assert!(!failure.skipped());
        }
        for payload in [
            json!({}),
            json!({"action": "other", "code": 3, "repeat": "yes"}),
            json!(null),
        ] {
            let older = event(payload);
            let failure = RecordedFailure::restore(&older);
            assert_eq!(failure.action, None);
            assert_eq!(failure.code, None);
            assert_eq!(failure.repeat, None);
            assert!(!failure.skipped());
        }
    }

    fn integrated(run: Option<&str>, payload: Value) -> RunEvent {
        RunEvent {
            run_id: run.map(|run| RunId::new(run).unwrap()),
            task_id: run.map(|_| TaskId::new(7)),
            kind: "run_integrated".into(),
            ..event(payload)
        }
    }

    /// A recheck names the dagq landing main is at when it starts, whatever
    /// made it due (a direct `integrate` too), and none when main moved
    /// without one or since one.
    #[test]
    fn the_recheck_names_the_landing_main_is_at() {
        let main = "a".repeat(40);
        let landed = Some(Landed {
            run_id: RunId::new("landed-run").unwrap(),
            task_id: TaskId::new(7),
        });
        let by_result = integrated(Some("landed-run"), json!({"result_commit": main}));
        assert_eq!(landing_at(Some(&by_result), &main), landed);
        let by_commit = integrated(Some("landed-run"), json!({"commit": main}));
        assert_eq!(landing_at(Some(&by_commit), &main), landed);
        // Main moved since, outside dagq.
        assert_eq!(landing_at(Some(&by_result), &"b".repeat(40)), None);
        assert_eq!(landing_at(None, &main), None);
        let no_run = integrated(None, json!({"result_commit": main}));
        assert_eq!(landing_at(Some(&no_run), &main), None);
    }

    /// A run's head judged against main already, by its precheck, its
    /// landing's deferral or a recheck, is not checked again; one judged
    /// against another main or with another head is.
    #[test]
    fn a_head_judged_against_main_is_not_judged_again() {
        let (main, head) = ("m1", "h1");
        for kind in [
            "conflict_precheck",
            "integration_deferred",
            LANDING_RECHECK_FAILED,
            LANDING_RECHECK_CLEAN,
        ] {
            let judged = RunEvent {
                kind: kind.into(),
                ..event(json!({"main": main, "head": head}))
            };
            assert!(
                judged_against(std::slice::from_ref(&judged), main, head, false),
                "{kind}"
            );
            assert!(
                !judged_against(std::slice::from_ref(&judged), "m2", head, false),
                "{kind}"
            );
            assert!(!judged_against(&[judged], main, "h2", false), "{kind}");
        }
        let other = RunEvent {
            kind: "review_finished".into(),
            ..event(json!({"main": main, "head": head}))
        };
        assert!(!judged_against(&[other], main, head, false));
    }

    /// A clean recheck that checked only the merge settles the head for a
    /// supervisor that would run a command only when the command was
    /// skipped by `[recheck] paths`; for one that would not, it does. A
    /// clean that names a command and the other judgements settle it
    /// either way.
    #[test]
    fn a_clean_with_no_command_settles_a_supervisor_with_one_only_when_skipped_by_paths() {
        let (main, head) = ("m1", "h1");
        let clean = |payload: Value| RunEvent {
            kind: LANDING_RECHECK_CLEAN.into(),
            ..event(payload)
        };
        let unmarked = clean(json!({"main": main, "head": head, "command": null}));
        let unwritten = clean(json!({"main": main, "head": head}));
        let by_paths = clean(json!({
            "main": main, "head": head, "command": null, "command_skipped": SKIPPED_BY_PATHS,
        }));
        let with_command = clean(json!({"main": main, "head": head, "command": "make check"}));
        for event in [&unmarked, &unwritten] {
            assert!(!judged_against(
                std::slice::from_ref(event),
                main,
                head,
                true
            ));
            assert!(judged_against(
                std::slice::from_ref(event),
                main,
                head,
                false
            ));
        }
        for event in [&by_paths, &with_command] {
            assert!(judged_against(
                std::slice::from_ref(event),
                main,
                head,
                true
            ));
            assert!(judged_against(
                std::slice::from_ref(event),
                main,
                head,
                false
            ));
        }
        for kind in [
            "conflict_precheck",
            "integration_deferred",
            LANDING_RECHECK_FAILED,
        ] {
            let judged = RunEvent {
                kind: kind.into(),
                ..event(json!({"main": main, "head": head, "command": null}))
            };
            assert!(judged_against(&[judged], main, head, true), "{kind}");
        }
        // Another judgement of the same main and head still settles it.
        let deferred = RunEvent {
            kind: "integration_deferred".into(),
            ..event(json!({"main": main, "head": head}))
        };
        assert!(judged_against(&[unmarked, deferred], main, head, true));
    }

    /// The recent `landing_recheck_finished` of a main tell whether this
    /// supervisor checked it, another did, or nobody did; this one's own
    /// check wins over another's.
    #[test]
    fn who_checked_a_main_is_read_from_the_finished_rechecks() {
        let finished = |main: &str, supervisor: &str| RunEvent {
            kind: LANDING_RECHECK_FINISHED.into(),
            ..event(json!({"main": main, "supervisor": supervisor}))
        };
        assert_eq!(checked_by(&[], "m", "me"), CheckedBy::No);
        let events = [finished("other-main", "me")];
        assert_eq!(checked_by(&events, "m", "me"), CheckedBy::No);
        let events = [finished("m", "another")];
        assert_eq!(checked_by(&events, "m", "me"), CheckedBy::Other);
        let events = [finished("m", "another"), finished("m", "me")];
        assert_eq!(checked_by(&events, "m", "me"), CheckedBy::This);
    }
}
