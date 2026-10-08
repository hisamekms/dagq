//! The processes of the supervisor's headless jobs (task 443): each job's
//! pid is recorded when it starts, so a supervisor that takes over after
//! the one that started it died can stop it before it starts its own, and
//! a job's timeout stops the processes the job started too.

use serde::{Deserialize, Serialize};

use super::{
    DomainError, provider_switch::SwitchReason, queue_hold::Wall, recovery::ProcessInfo,
    tokens::ExecutionTokens,
};

/// `headless_jobs.kind` of a headless review of a run (ADR-0027).
pub const REVIEW: &str = "review";
/// `headless_jobs.kind` of a recovery job, of a run that ended (the
/// triage) or of a live session (ADR-0047 decisions 39 and 40).
pub const RECOVERY: &str = "recovery";
/// `headless_jobs.kind` of a plan review of a proposal (ADR-0041).
pub const PLAN_REVIEW: &str = "plan_review";
/// `headless_jobs.kind` of a goal review (ADR-0047 decision 43).
pub const GOAL_REVIEW: &str = "goal_review";

/// `headless_jobs.outcome` of a job that ended by itself (its exit read).
pub const ENDED: &str = "ended";
/// `headless_jobs.outcome` of a job its own supervisor stopped (its
/// timeout, or the supervisor stopped watching it).
pub const STOPPED: &str = "stopped";
/// `headless_jobs.outcome` of a job of a gone supervisor that another
/// supervisor stopped (`headless_job_stopped`).
pub const TAKEN_OVER: &str = "taken_over";
/// `headless_jobs.outcome` of a job of a gone supervisor whose process was
/// found gone already.
pub const GONE: &str = "gone";
/// `headless_jobs.outcome` of a job of a gone supervisor whose pid runs
/// another process now (or whose start could not be told): it is not
/// touched.
pub const NOT_THE_JOB: &str = "not_the_job";

string_enum!(JobAccess {
    ReadFiles => "read_files",
    ReadFilesAndQueueCli => "read_files_and_queue_cli",
    QueueCli => "queue_cli",
});

/// What a headless job may do, as an intent rather than a provider's tool
/// names (ADR-t1063-1 decision 2): the job names it, and the provider's
/// implementation turns it into its own mechanism (Claude Code's allowed
/// tools, a sandbox). Beyond it the job may do only what needs no
/// permission (reading its prompt and answering).
impl JobAccess {
    /// Whether the job may read the files of its directory (and, for the
    /// review, the run's).
    pub const fn reads_files(self) -> bool {
        matches!(self, Self::ReadFiles | Self::ReadFilesAndQueueCli)
    }

    /// Whether the job may run the `dagq` CLI; what it may change through
    /// it is its role's policy (ADR-t728-1), not this.
    pub const fn runs_queue_cli(self) -> bool {
        matches!(self, Self::ReadFilesAndQueueCli | Self::QueueCli)
    }
}

string_enum!(JobFailure {
    ExecutableMissing => "executable_missing",
    LaunchFailed => "launch_failed",
    Authentication => "authentication",
    UsageLimit => "usage_limit",
    Other => "other",
});

/// Why a headless job did not start or failed, whatever its provider
/// (ADR-t1063-1 decision 4): the provider's implementation reads its own
/// output into this. The first four are the worker's reasons for a
/// provider that cannot be used ([`SwitchReason`], ADR-t813-2 decision 2),
/// by the same values; `other` is any other failure (a non-zero exit, the
/// timeout, a verdict that does not parse), which moves no job.
impl JobFailure {
    /// The reason a provider cannot be used that this failure is, if any.
    pub const fn switch_reason(self) -> Option<SwitchReason> {
        match self {
            Self::ExecutableMissing => Some(SwitchReason::ExecutableMissing),
            Self::LaunchFailed => Some(SwitchReason::LaunchFailed),
            Self::Authentication => Some(SwitchReason::Authentication),
            Self::UsageLimit => Some(SwitchReason::UsageLimit),
            Self::Other => None,
        }
    }

    /// The wall only a person moves that the job stopped at (task 438): a
    /// login that ran out, or the usage limit.
    pub const fn wall(self) -> Option<Wall> {
        match self {
            Self::Authentication => Some(Wall::Authentication),
            Self::UsageLimit => Some(Wall::UsageLimit),
            Self::ExecutableMissing | Self::LaunchFailed | Self::Other => None,
        }
    }

    /// The failure a wall is.
    pub const fn of_wall(wall: Wall) -> Self {
        match wall {
            Wall::Authentication => Self::Authentication,
            Wall::UsageLimit => Self::UsageLimit,
        }
    }
}

/// What the output of a headless job that ended says of its session: for
/// a provider that names the session itself and whose model the runtime
/// cannot read from a transcript (Codex, ADR-t1063-1 decision 6), the id
/// the provider named it by (Codex's thread), the model it ran on, and why
/// none was read when it was not; and the tokens of the job, one
/// Execution (ADR-t1486-1), for a provider whose output gives them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobSession {
    /// The provider named the session itself: `session_id`, `model` and
    /// `model_unknown` are recorded. A provider whose session the runtime
    /// names ahead (Claude Code) leaves them to the job's start and
    /// transcript.
    pub named: bool,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub model_unknown: Option<String>,
    pub tokens: Option<ExecutionTokens>,
}

impl JobSession {
    /// Put the session into the payload of the event that ends the job:
    /// `session_id`, `model` and `model_unknown` (only when set) when the
    /// provider named it, and the tokens ([`ExecutionTokens::record`])
    /// when its output gave them.
    pub fn record(&self, payload: &mut serde_json::Value) {
        if self.named {
            payload["session_id"] = serde_json::json!(self.session_id);
            payload["model"] = serde_json::json!(self.model);
            if let Some(why) = &self.model_unknown {
                payload["model_unknown"] = serde_json::json!(why);
            }
        }
        if let Some(tokens) = &self.tokens {
            tokens.record(payload);
        }
    }

    /// Put the session of a job that started an agent into the payload of
    /// the event that ends it, as [`Self::record`], and record its
    /// Execution (ADR-t1486-1) whatever its output gave: not measured
    /// ([`TOKENS_NOT_READ`](crate::domain::tokens::TOKENS_NOT_READ)) when
    /// it gave no tokens, so that the end counts among the Executions.
    pub fn record_execution(session: Option<&Self>, payload: &mut serde_json::Value) {
        if let Some(session) = session {
            session.record(payload);
        }
        if session.is_none_or(|session| session.tokens.is_none()) {
            crate::domain::tokens::ExecutionTokens::unmeasured(
                crate::domain::tokens::TOKENS_NOT_READ,
            )
            .record(payload);
        }
    }
}

/// What a supervisor that takes over does with the process of a job of a
/// gone supervisor, from what `ps` says of the pid now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Takeover {
    /// The pid runs no process: the row is only closed.
    Gone,
    /// The pid runs the job's process (the same start): it and its
    /// descendants are stopped.
    Stop,
    /// The pid runs another process, or the start of either could not be
    /// read: nothing is signalled.
    NotTheJob,
}

/// Judge the pid of a job: `recorded` is the process's start read when the
/// job started, `now` the start of the pid's process read now (`None` when
/// it could not be read), `alive` whether the pid runs.
pub fn takeover(alive: bool, recorded: Option<&str>, now: Option<&str>) -> Takeover {
    if !alive {
        return Takeover::Gone;
    }
    match (recorded, now) {
        (Some(recorded), Some(now)) if recorded == now => Takeover::Stop,
        _ => Takeover::NotTheJob,
    }
}

/// The descendants of `root` in `all` (children first, then theirs), not
/// `root` itself.
pub fn descendants(all: &[ProcessInfo], root: u32) -> Vec<u32> {
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for process in all {
            if process.ppid == parent
                && process.pid != root
                && process.pid > 1
                && !found.contains(&process.pid)
            {
                found.push(process.pid);
                frontier.push(process.pid);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, ppid: u32) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            elapsed_secs: 0,
            command: String::new(),
            cwd: None,
            cpu_ms: None,
        }
    }

    #[test]
    fn descendants_follow_every_generation_and_leave_the_rest() {
        let all = [
            process(10, 1),
            process(11, 10),
            process(12, 11),
            process(13, 10),
            process(20, 1),
            process(21, 20),
        ];
        let mut found = descendants(&all, 10);
        found.sort();
        assert_eq!(found, [11, 12, 13]);
        assert!(descendants(&all, 12).is_empty());
        assert!(descendants(&all, 99).is_empty());
    }

    #[test]
    fn a_cycle_in_the_listing_ends() {
        let all = [process(5, 6), process(6, 5)];
        assert_eq!(descendants(&all, 5), [6]);
    }

    #[test]
    fn only_the_same_process_is_stopped() {
        assert_eq!(takeover(false, Some("a"), None), Takeover::Gone);
        assert_eq!(takeover(true, Some("a"), Some("a")), Takeover::Stop);
        assert_eq!(takeover(true, Some("a"), Some("b")), Takeover::NotTheJob);
        assert_eq!(takeover(true, None, Some("b")), Takeover::NotTheJob);
        assert_eq!(takeover(true, Some("a"), None), Takeover::NotTheJob);
    }

    #[test]
    fn job_access_says_what_the_job_may_do() {
        assert!(JobAccess::ReadFiles.reads_files());
        assert!(!JobAccess::ReadFiles.runs_queue_cli());
        assert!(JobAccess::ReadFilesAndQueueCli.reads_files());
        assert!(JobAccess::ReadFilesAndQueueCli.runs_queue_cli());
        assert!(!JobAccess::QueueCli.reads_files());
        assert!(JobAccess::QueueCli.runs_queue_cli());
        for access in [
            JobAccess::ReadFiles,
            JobAccess::ReadFilesAndQueueCli,
            JobAccess::QueueCli,
        ] {
            assert_eq!(access.as_str().parse::<JobAccess>().unwrap(), access);
        }
    }

    #[test]
    fn a_job_session_is_recorded_in_the_end_of_the_job() {
        let mut payload = serde_json::json!({"goal_review_id": 3});
        JobSession {
            named: true,
            session_id: Some("thread-1".into()),
            model: Some("gpt-6-astra".into()),
            ..JobSession::default()
        }
        .record(&mut payload);
        assert_eq!(payload["session_id"], "thread-1");
        assert_eq!(payload["model"], "gpt-6-astra");
        assert!(payload.get("model_unknown").is_none());
        let mut payload = serde_json::json!({});
        JobSession {
            named: true,
            model_unknown: Some("no rollout".into()),
            ..JobSession::default()
        }
        .record(&mut payload);
        assert!(payload["session_id"].is_null());
        assert_eq!(payload["model_unknown"], "no rollout");
        assert!(payload.get("tokens").is_none(), "{payload}");
        // A session the runtime named ahead (Claude's) records the job's
        // tokens only.
        let mut payload = serde_json::json!({"session_id": "ahead"});
        JobSession {
            tokens: Some(super::super::tokens::ExecutionTokens::unmeasured(
                super::super::tokens::NO_USAGE,
            )),
            ..JobSession::default()
        }
        .record(&mut payload);
        assert_eq!(payload["session_id"], "ahead");
        assert!(payload.get("model").is_none(), "{payload}");
        assert_eq!(payload["tokens_reason"], "no_usage");
    }

    #[test]
    fn a_job_that_started_an_agent_always_records_its_execution() {
        // No tokens from its output, or no session read: not measured.
        for session in [None, Some(JobSession::default())] {
            let mut payload = serde_json::json!({});
            JobSession::record_execution(session.as_ref(), &mut payload);
            assert!(payload["tokens"].is_null(), "{payload}");
            assert!(payload.get("tokens_source").is_some(), "{payload}");
            assert_eq!(payload["tokens_reason"], "tokens_not_read");
        }
        // Tokens counted: they are recorded as they are.
        let tokens = super::super::tokens::ExecutionTokens {
            tokens: Some(super::super::tokens::TokenUsage {
                input: 3,
                output: 4,
                ..Default::default()
            }),
            source: Some(super::super::tokens::TokenSource::ModelUsage),
            ..Default::default()
        };
        let mut payload = serde_json::json!({});
        JobSession::record_execution(
            Some(&JobSession {
                tokens: Some(tokens),
                ..JobSession::default()
            }),
            &mut payload,
        );
        assert_eq!(payload["tokens"]["input"], 3);
        assert_eq!(payload["tokens_source"], "model_usage");
        assert!(payload["tokens_reason"].is_null(), "{payload}");
    }

    #[test]
    fn job_failures_share_the_workers_reasons() {
        for (failure, reason) in [
            (
                JobFailure::ExecutableMissing,
                SwitchReason::ExecutableMissing,
            ),
            (JobFailure::LaunchFailed, SwitchReason::LaunchFailed),
            (JobFailure::Authentication, SwitchReason::Authentication),
            (JobFailure::UsageLimit, SwitchReason::UsageLimit),
        ] {
            assert_eq!(failure.switch_reason(), Some(reason));
            assert_eq!(failure.as_str(), reason.as_str());
        }
        assert_eq!(JobFailure::Other.switch_reason(), None);
        assert_eq!(
            JobFailure::Authentication.wall(),
            Some(Wall::Authentication)
        );
        assert_eq!(JobFailure::UsageLimit.wall(), Some(Wall::UsageLimit));
        assert_eq!(JobFailure::LaunchFailed.wall(), None);
        assert_eq!(JobFailure::Other.wall(), None);
        for wall in [Wall::Authentication, Wall::UsageLimit] {
            assert_eq!(JobFailure::of_wall(wall).wall(), Some(wall));
        }
    }
}
