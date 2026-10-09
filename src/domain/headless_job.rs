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
/// `headless_jobs.kind` of a program job of a run's review stage
/// (ADR-t1895-1 decision 1); its `label` names the program.
pub const REVIEW_PROGRAM: &str = "review_program";
/// `headless_jobs.provider` of a job no provider runs (a program job): the
/// column is the provider of an agent job only.
pub const NO_PROVIDER: &str = "none";

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

string_enum!(JobKind {
    Agent => "agent",
    Program => "program",
});

/// What runs a job of a run's review stage (ADR-t1895-1 decision 1): an
/// `agent` job is a provider's headless session, a `program` job a child
/// process of a set program in a process group of its own. Either starts,
/// is waited for under its kind's timeout, is stopped with its descendants
/// at the timeout, is recorded in `headless_jobs` and is taken over by
/// another supervisor the same way. Every other headless job (the
/// recovery job, the plan review, the goal review) is an agent job.
impl JobKind {
    /// The `headless_jobs.kind` of this kind's job of a run's review.
    pub const fn review_kind(self) -> &'static str {
        match self {
            Self::Agent => REVIEW,
            Self::Program => REVIEW_PROGRAM,
        }
    }

    /// Whether a job of this kind that ended as `stop` is worth one more
    /// job with the same input. An agent job that exited non-zero is a
    /// passing failure (task 328); a program's exit is its check's result
    /// (ADR-t1895-2 decision 3), not a failure to try again. A timeout is
    /// retried for neither: another job would spend the timeout again, and
    /// a program's is a review failure (ADR-t1895-2 decision 4).
    pub const fn retries(self, stop: JobStop) -> bool {
        matches!((self, stop), (Self::Agent, JobStop::Exited))
    }
}

/// How a job ended without a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStop {
    /// Its process exited non-zero.
    Exited,
    /// It did not end within its timeout and was stopped.
    TimedOut,
}

/// `[review.jobs]` of `dagq.toml`: the timeout of each kind of job of a
/// run's review stage, in seconds. A kind without a key keeps the
/// timeout of the provider's review ([`JobTimeouts::of`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobTimeouts {
    /// `agent_timeout_secs`, a whole number above 0.
    pub agent: Option<u64>,
    /// `program_timeout_secs`, a whole number above 0.
    pub program: Option<u64>,
}

impl JobTimeouts {
    /// The keys of `[review.jobs]`, the agent's first.
    pub const KEYS: [&'static str; 2] = ["agent_timeout_secs", "program_timeout_secs"];

    /// Set the key `key` (one of [`Self::KEYS`]) to `secs`.
    pub fn set(&mut self, key: &str, secs: u64) {
        if key == Self::KEYS[0] {
            self.agent = Some(secs);
        } else {
            self.program = Some(secs);
        }
    }

    /// The timeout of a `kind` job of a run's review: its key's, else
    /// `default`, the provider's review timeout the review had before the
    /// kinds.
    pub fn of(self, kind: JobKind, default: std::time::Duration) -> std::time::Duration {
        match kind {
            JobKind::Agent => self.agent,
            JobKind::Program => self.program,
        }
        .map_or(default, std::time::Duration::from_secs)
    }

    /// How long a job may run. One outside a run's review stage (`stage`
    /// `None`: the recovery job, a plan review, a goal review) gets
    /// `reviewer`'s, the review timeout of the supervisor's reviewer,
    /// whatever `[review.jobs]` says. A `kind` job of the stage gets its
    /// key's ([`Self::of`]), else `provider`'s (the review timeout of the
    /// provider that runs it) for an agent job and `reviewer`'s for a
    /// program job, which no provider runs.
    pub fn job_timeout(
        self,
        stage: Option<JobKind>,
        provider: std::time::Duration,
        reviewer: std::time::Duration,
    ) -> std::time::Duration {
        match stage {
            None => reviewer,
            Some(JobKind::Agent) => self.of(JobKind::Agent, provider),
            Some(JobKind::Program) => self.of(JobKind::Program, reviewer),
        }
    }
}

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

/// The job of a gone supervisor another one stops: what it ran on, to
/// tell the events that ended it already.
#[derive(Debug, Clone, Copy)]
pub struct JobOn<'a> {
    /// Its `headless_jobs.kind`.
    pub kind: &'a str,
    pub run_id: Option<&'a super::RunId>,
    pub proposal_id: Option<super::ProposalId>,
    pub goal_id: Option<super::GoalId>,
    /// Its attempt: an end that names another is another job's.
    pub attempt: usize,
    /// When its row was written, unix seconds.
    pub started_at: i64,
}

impl JobOn<'_> {
    /// The kinds of the events besides `headless_job_stopped` that end a
    /// job of this kind with its Execution (ADR-t1486-1); none for a kind
    /// that starts no agent.
    fn end_kinds(&self) -> &'static [&'static str] {
        use super::event_kind as k;
        match self.kind {
            REVIEW => &[k::REVIEW_FINISHED, k::REVIEW_FAILED, k::REVIEW_RETRIED],
            RECOVERY => &[k::RECOVERY_FINISHED],
            PLAN_REVIEW => &[
                k::PLAN_REVIEW_FINISHED,
                k::PLAN_REVIEW_FAILED,
                k::PLAN_REVIEW_DISCARDED,
            ],
            GOAL_REVIEW => &[k::GOAL_REVIEW_FINISHED, k::GOAL_REVIEW_FAILED],
            _ => &[],
        }
    }

    /// The kinds of event [`Self::execution_recorded`] looks among: the
    /// ends of this kind of job and `headless_job_stopped`.
    pub fn ending_kinds(&self) -> Vec<&'static str> {
        let mut kinds = self.end_kinds().to_vec();
        kinds.push(super::event_kind::HEADLESS_JOB_STOPPED);
        kinds
    }

    /// Whether one of `events` ended this job with its Execution recorded
    /// (`tokens_source`), so that the supervisor that stops it does not
    /// count it again: an end of its kind, on its run (else its proposal,
    /// else its goal), of its attempt when the end names one, written no
    /// earlier than the job started.
    pub fn execution_recorded(&self, events: &[super::RunEvent]) -> bool {
        events.iter().any(|event| {
            let end: super::run::JobEnd<'_> = super::run::restore_payload(&event.payload);
            let ends = if event.kind == super::event_kind::HEADLESS_JOB_STOPPED {
                end.kind == Some(self.kind)
            } else {
                self.end_kinds().contains(&event.kind.as_str())
            };
            let on = match (self.run_id, self.proposal_id, self.goal_id) {
                (Some(run), _, _) => event.run_id.as_ref() == Some(run),
                (None, Some(proposal), _) => end.proposal_id == Some(proposal.as_i64()),
                (None, None, Some(goal)) => {
                    event.goal_id == Some(goal) || end.goal_id == Some(goal.as_i64())
                }
                (None, None, None) => true,
            };
            let attempt = end
                .attempt
                .is_none_or(|attempt| attempt == self.attempt as u64);
            let since = super::stats::timestamp_millis(&event.created_at)
                .is_some_and(|at| at >= self.started_at * 1000);
            ends && on && attempt && since && end.execution
        })
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
    fn each_kind_is_recorded_retried_and_bounded_its_own_way() {
        assert_eq!(JobKind::Agent.review_kind(), REVIEW);
        assert_eq!(JobKind::Program.review_kind(), REVIEW_PROGRAM);
        for kind in [JobKind::Agent, JobKind::Program] {
            assert_eq!(kind.as_str().parse::<JobKind>().unwrap(), kind);
            assert!(!kind.retries(JobStop::TimedOut));
        }
        assert!(JobKind::Agent.retries(JobStop::Exited));
        assert!(!JobKind::Program.retries(JobStop::Exited));
        let default = std::time::Duration::from_secs(600);
        let none = JobTimeouts::default();
        assert_eq!(none.of(JobKind::Agent, default), default);
        assert_eq!(none.of(JobKind::Program, default), default);
        let mut set = JobTimeouts::default();
        set.set("program_timeout_secs", 30);
        assert_eq!(set.of(JobKind::Program, default).as_secs(), 30);
        assert_eq!(set.of(JobKind::Agent, default), default);
        set.set("agent_timeout_secs", 900);
        assert_eq!(set.of(JobKind::Agent, default).as_secs(), 900);
    }

    #[test]
    fn a_jobs_timeout_follows_its_stage_and_kind() {
        let secs = std::time::Duration::from_secs;
        let (provider, reviewer) = (secs(120), secs(600));
        // Without `[review.jobs]`: the review's agent job keeps its
        // provider's, a program job and any other job the reviewer's.
        let none = JobTimeouts::default();
        assert_eq!(
            none.job_timeout(Some(JobKind::Agent), provider, reviewer),
            provider
        );
        assert_eq!(
            none.job_timeout(Some(JobKind::Program), provider, reviewer),
            reviewer
        );
        assert_eq!(none.job_timeout(None, provider, reviewer), reviewer);
        // With it: each kind of the stage gets its key's; a job outside
        // the stage (the recovery job, a plan or goal review) does not.
        let set = JobTimeouts {
            agent: Some(900),
            program: Some(30),
        };
        assert_eq!(
            set.job_timeout(Some(JobKind::Agent), provider, reviewer),
            secs(900)
        );
        assert_eq!(
            set.job_timeout(Some(JobKind::Program), provider, reviewer),
            secs(30)
        );
        assert_eq!(set.job_timeout(None, provider, reviewer), reviewer);
        // One key alone leaves the other kind at its default.
        let program = JobTimeouts {
            agent: None,
            program: Some(30),
        };
        assert_eq!(
            program.job_timeout(Some(JobKind::Agent), provider, reviewer),
            provider
        );
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

    fn ending(
        id: i64,
        run: Option<&str>,
        kind: &str,
        payload: serde_json::Value,
        at: &str,
    ) -> crate::domain::RunEvent {
        crate::domain::RunEvent {
            id: crate::domain::EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: run.map(|run| crate::domain::RunId::new(run).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: at.to_owned(),
            actor: None,
        }
    }

    /// A job a supervisor stops after its own is gone is counted again
    /// only when no end of it recorded its Execution: an end of its kind
    /// on its run (or proposal) since it started, with `tokens_source`.
    /// An end before its start, on another run or proposal, of another
    /// kind or attempt, or with no Execution leaves it to be counted.
    #[test]
    fn a_taken_over_job_is_counted_once() {
        use serde_json::json;
        let run = crate::domain::RunId::new("r1").unwrap();
        // 2026-10-01T09:00:00Z
        let started_at = 1_790_845_200;
        let review = JobOn {
            kind: REVIEW,
            run_id: Some(&run),
            proposal_id: None,
            goal_id: None,
            attempt: 2,
            started_at,
        };
        let after = "2026-10-01T09:05:00Z";
        let counted = json!({"tokens": null, "tokens_source": null});
        assert!(review.execution_recorded(&[ending(
            1,
            Some("r1"),
            "review_failed",
            counted.clone(),
            after
        )]));
        assert!(review.execution_recorded(&[ending(
            1,
            Some("r1"),
            "headless_job_stopped",
            json!({"kind": "review", "tokens_source": "model_usage"}),
            after
        )]));
        for left in [
            ending(
                1,
                Some("r1"),
                "review_failed",
                counted.clone(),
                "2026-10-01T08:59:59Z",
            ),
            ending(1, Some("r2"), "review_failed", counted.clone(), after),
            ending(
                1,
                Some("r1"),
                "review_failed",
                json!({"code": "job_failed"}),
                after,
            ),
            ending(1, Some("r1"), "recovery_finished", counted.clone(), after),
            // The end of the attempt before, written in the second the job
            // started.
            ending(
                1,
                Some("r1"),
                "review_retried",
                json!({"attempt": 1, "tokens_source": null}),
                "2026-10-01T09:00:00.300Z",
            ),
            ending(
                1,
                Some("r1"),
                "headless_job_stopped",
                json!({"kind": "recovery", "tokens_source": null}),
                after,
            ),
        ] {
            assert!(
                !review.execution_recorded(std::slice::from_ref(&left)),
                "{left:?}"
            );
        }
        let plan_review = JobOn {
            kind: PLAN_REVIEW,
            run_id: None,
            proposal_id: Some(crate::domain::ProposalId::new(4)),
            goal_id: None,
            attempt: 1,
            started_at,
        };
        let on = |proposal: i64| {
            ending(
                1,
                None,
                "plan_review_discarded",
                json!({"proposal_id": proposal, "tokens_source": null}),
                after,
            )
        };
        assert!(plan_review.execution_recorded(&[on(4)]));
        assert!(!plan_review.execution_recorded(&[on(5)]));
        assert_eq!(
            plan_review.ending_kinds(),
            [
                "plan_review_finished",
                "plan_review_failed",
                "plan_review_discarded",
                "headless_job_stopped"
            ]
        );
    }
}
