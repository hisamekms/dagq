//! The runtime's detection of a task whose run takes long, and what it
//! makes of the read-only diagnosis that follows (ADR-t1521-1 decisions 1
//! to 4, docs/plans/task-replanning.md, "回数・抑制・上限と観測"): the turns of
//! a task counted by cause ([`count_turns`], which `status`, `timeline`
//! and `stats` show as the detection reads them), the window of review
//! verdicts a candidate is detected on ([`detect`]), the verdict of the
//! diagnosis job ([`DiagnosisVerdict`]) and the runtime's decision on it
//! ([`decide`]), which only continues, parks for the replanning or asks a
//! person. Nothing here splits or stops a run by its number of turns: only
//! the reviews that did not pass are counted towards a candidate, and a
//! candidate is only diagnosed.
//!
//! What a window is: the non-pass reviews (`concern` and `revise`) of the
//! task, across its runs whatever their provider, after its last `pass`
//! and after the last event an earlier diagnosis of the task read. A
//! retry, a move to the other provider or a restart of the supervisor
//! starts no new window, and a turn of a conflict, an e2e, a verification,
//! a provider's failure or a person's answer neither counts nor starts
//! one. Each window is diagnosed once: once its diagnosis chose to go on,
//! the next one is the [`ReplanConfig::continue_reviews`] non-pass reviews
//! after it and one more.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    DomainError, EventId, EventKind, RunEvent, RunId,
    task_replan::{DiagnosisChoice, ReplanCause, ReplanOperation, ReplanOrigin, ReplanStatus},
};

// What a turn of a run was sent for (docs/plans/task-replanning.md,
// "数え方"): the task's prompt, a review's send-back, a conflict, an e2e or
// a verification that failed, a receipt the runtime asked for again, a
// person's answer to the worker's question, a provider's or the host's
// failure, or none of these.
string_enum!(TurnCause {
    Initial => "initial",
    Review => "review",
    Conflict => "conflict",
    E2e => "e2e",
    Verification => "verification",
    Receipt => "receipt",
    WorkerAnswer => "worker_answer",
    ProviderHost => "provider_host",
    Unlabeled => "unlabeled",
});

impl TurnCause {
    pub const ALL: [Self; 9] = [
        Self::Initial,
        Self::Review,
        Self::Conflict,
        Self::E2e,
        Self::Verification,
        Self::Receipt,
        Self::WorkerAnswer,
        Self::ProviderHost,
        Self::Unlabeled,
    ];

    /// The cause of a turn by its request's `what` (`turn_started`), the
    /// cause of the latest trigger `trigger` for a resolution request.
    fn of_request(what: &str, trigger: Option<Self>) -> Self {
        match what {
            "the task's prompt" => Self::Initial,
            "revise request" => Self::Review,
            "conflict request" => Self::Conflict,
            "provider switch" | super::turn::PROVIDER_RETRY => Self::ProviderHost,
            "nudge" => Self::Receipt,
            "resolution request" => trigger.unwrap_or(Self::Unlabeled),
            _ if what.starts_with("answer of ask") => Self::WorkerAnswer,
            _ => Self::Unlabeled,
        }
    }

    /// The cause a resume's reason (`resume_started`) names.
    fn of_resume(reason: &str) -> Self {
        if reason.contains("review's findings were sent back") {
            Self::Review
        } else if reason.contains("e2e failed") {
            Self::E2e
        } else if reason.contains("conflict") {
            Self::Conflict
        } else if ["heartbeat", "environment failure", "stopped from outside"]
            .iter()
            .any(|word| reason.contains(word))
        {
            Self::ProviderHost
        } else if ["verification command", "exited with", "exits with"]
            .iter()
            .any(|word| reason.contains(word))
        {
            Self::Verification
        } else {
            Self::Unlabeled
        }
    }
}

/// The turns and reviews of a run, or of a task's runs together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TurnCounts {
    /// `turn_started`: the turns sent, the one that runs included.
    pub started: usize,
    /// `turn_finished`: the turns that ended.
    pub finished: usize,
    /// The turns sent, by [`TurnCause`]; every cause, 0 included.
    pub by_cause: BTreeMap<&'static str, usize>,
    /// `review_finished`, whatever the verdict.
    pub reviews: usize,
    /// The reviews that did not pass (`concern` and `revise`).
    pub non_pass: usize,
    pub passes: usize,
}

impl Default for TurnCounts {
    fn default() -> Self {
        Self {
            started: 0,
            finished: 0,
            by_cause: TurnCause::ALL.iter().map(|c| (c.as_str(), 0)).collect(),
            reviews: 0,
            non_pass: 0,
            passes: 0,
        }
    }
}

impl TurnCounts {
    fn add(&mut self, other: &Self) {
        self.started += other.started;
        self.finished += other.finished;
        for (cause, count) in &other.by_cause {
            *self.by_cause.entry(cause).or_default() += count;
        }
        self.reviews += other.reviews;
        self.non_pass += other.non_pass;
        self.passes += other.passes;
    }
}

/// One run's [`TurnCounts`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunTurns {
    pub run_id: RunId,
    #[serde(flatten)]
    pub counts: TurnCounts,
}

/// A task's [`TurnCounts`]: all of its runs together, and each run's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TaskTurns {
    pub total: TurnCounts,
    pub runs: Vec<RunTurns>,
}

/// Whether a `review_finished` did not pass: `Some(true)` for a `concern`
/// or a `revise`, `Some(false)` for a `pass`, `None` for anything else.
fn non_pass(event: &RunEvent) -> Option<bool> {
    if event.kind != EventKind::ReviewFinished {
        return None;
    }
    match event.payload["verdict"].as_str() {
        Some("pass") => Some(false),
        Some("concern" | "revise") => Some(true),
        _ => None,
    }
}

/// The turns of a task by cause, from `events` (its events, its runs' all
/// together, oldest first): a resolution request takes the cause of the
/// latest trigger of its run before it (a send-back, a conflict, a failed
/// e2e or verification, a receipt refused, the reason of its resume), and
/// the turn after a turn that failed is a provider's or the host's.
pub fn count_turns(events: &[RunEvent]) -> TaskTurns {
    count_turns_where(events, |_| true)
}

/// [`count_turns`] of only the events `counted` takes (a window of
/// `stats`), each turn's cause still read from every event before it.
pub fn count_turns_where(events: &[RunEvent], counted: impl Fn(&RunEvent) -> bool) -> TaskTurns {
    struct State {
        counts: TurnCounts,
        trigger: Option<TurnCause>,
        failed_before: bool,
    }
    let mut runs: Vec<(RunId, State)> = Vec::new();
    for event in events {
        let Some(run) = event.run_id.as_ref() else {
            continue;
        };
        let at = match runs.iter().position(|(id, _)| id == run) {
            Some(at) => at,
            None => {
                runs.push((
                    run.clone(),
                    State {
                        counts: TurnCounts::default(),
                        trigger: None,
                        failed_before: false,
                    },
                ));
                runs.len() - 1
            }
        };
        let state = &mut runs[at].1;
        let payload = &event.payload;
        let counts = usize::from(counted(event));
        if let Some(non_pass) = non_pass(event) {
            state.counts.reviews += counts;
            if non_pass {
                state.counts.non_pass += counts;
                state.trigger = Some(TurnCause::Review);
            } else {
                state.counts.passes += counts;
            }
        } else if event.kind == EventKind::LandingDecided {
            if payload["answer"].as_str() == Some("send_back") {
                state.trigger = Some(TurnCause::Review);
            }
        } else if event.kind == EventKind::ConflictPrecheck {
            state.trigger = Some(TurnCause::Conflict);
        } else if event.kind == EventKind::LandingRecheckFailed {
            state.trigger = Some(if payload["code"].as_str() == Some("rebase_conflict") {
                TurnCause::Conflict
            } else {
                TurnCause::Verification
            });
        } else if event.kind == EventKind::RunE2eFailed {
            state.trigger = Some(TurnCause::E2e);
        } else if event.kind == EventKind::ValidationFinished {
            if payload["accepted"] == Value::Bool(false) {
                state.trigger = Some(TurnCause::Receipt);
            }
        } else if event.kind == EventKind::ResumeStarted {
            let cause = TurnCause::of_resume(payload["reason"].as_str().unwrap_or_default());
            if cause != TurnCause::Unlabeled || state.trigger.is_none() {
                state.trigger = Some(cause);
            }
        } else if event.kind == EventKind::TurnStarted {
            state.counts.started += counts;
            let cause = if state.failed_before {
                TurnCause::ProviderHost
            } else {
                TurnCause::of_request(payload["what"].as_str().unwrap_or_default(), state.trigger)
            };
            state.failed_before = false;
            *state.counts.by_cause.entry(cause.as_str()).or_default() += counts;
        } else if event.kind == EventKind::TurnFinished {
            state.counts.finished += counts;
            if !payload["failure"].is_null() {
                state.failed_before = true;
            }
        }
    }
    let mut total = TurnCounts::default();
    let runs = runs
        .into_iter()
        .map(|(run_id, state)| {
            total.add(&state.counts);
            RunTurns {
                run_id,
                counts: state.counts,
            }
        })
        .collect();
    TaskTurns { total, runs }
}

/// `[replan]` of `dagq.toml`: whether the runtime detects a task to
/// diagnose, and its limits. Without the table (or without `enabled =
/// true`) nothing is detected, and an operation a person or a worker opens
/// waits as a `candidate` for none to diagnose it, as before the runtime
/// had a diagnosis; a queue that never had one reads the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ReplanConfig {
    /// `enabled`: whether the runtime detects candidates and diagnoses the
    /// operations opened (`false` without the key).
    pub enabled: bool,
    /// `non_pass_reviews`: the non-pass reviews in a row (no `pass`
    /// between them) that make a task a candidate.
    pub non_pass_reviews: usize,
    /// `continue_reviews`: the non-pass reviews a diagnosis that chose to
    /// go on lets the task have before the next one is a candidate again.
    pub continue_reviews: usize,
    /// `max_auto_parks`: how many times a diagnosis may park the tasks of
    /// one lineage (a task and the tasks that replaced it, and theirs)
    /// for a replanning without a person; past it a person decides.
    pub max_auto_parks: usize,
}

impl ReplanConfig {
    /// The keys of `[replan]`.
    pub const KEYS: [&'static str; 4] = [
        "enabled",
        "non_pass_reviews",
        "continue_reviews",
        "max_auto_parks",
    ];
}

impl Default for ReplanConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            non_pass_reviews: 3,
            continue_reviews: 2,
            max_auto_parks: 3,
        }
    }
}

// What made a task a candidate: the non-pass reviews of its window, the
// review's revise limit reached on them (`concern_decided`'s
// `escalated_because`), or the run's resumes used up while the window had
// one (`recovery_requested` of `resume_exhausted`). The latter two are the
// exits of the existing limits, which the diagnosis may turn into a
// replanning instead of a person's decision alone.
string_enum!(Trigger {
    NonPassReviews => "non_pass_reviews",
    ReviseLimit => "revise_limit",
    ResumeExhausted => "resume_exhausted",
});

/// A task's window of reviews now ([`window`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Window {
    /// The event it starts after: the task's last `pass`, or the last
    /// event a diagnosis of the task read, whichever is later.
    pub since: Option<EventId>,
    /// The non-pass reviews in it (`review_finished`).
    pub non_pass: Vec<EventId>,
    /// How many of them make a candidate: [`ReplanConfig::non_pass_reviews`],
    /// or [`ReplanConfig::continue_reviews`] and one more after a
    /// diagnosis that chose to go on.
    pub threshold: usize,
    /// Whether the window follows a diagnosis rather than a `pass`.
    pub after_diagnosis: bool,
    /// The exit of an existing limit in it, if any.
    pub limit: Option<(Trigger, EventId)>,
}

/// The latest event an earlier diagnosis of `ops` (the task's operations)
/// read.
fn diagnosed_until(ops: &[ReplanOperation]) -> Option<EventId> {
    ops.iter().filter_map(|op| op.diagnosis.window_end).max()
}

/// The window of the task whose events are `events` (oldest first) and
/// whose operations are `ops`, under `config`.
pub fn window(config: &ReplanConfig, events: &[RunEvent], ops: &[ReplanOperation]) -> Window {
    let passed = events
        .iter()
        .filter(|e| non_pass(e) == Some(false))
        .map(|e| e.id)
        .max();
    let diagnosed = diagnosed_until(ops);
    let after_diagnosis = match (diagnosed, passed) {
        (Some(diagnosed), Some(passed)) => diagnosed > passed,
        (Some(_), None) => true,
        (None, _) => false,
    };
    let since = passed.max(diagnosed);
    let after = |e: &&RunEvent| since.is_none_or(|since| e.id > since);
    let non_pass: Vec<EventId> = events
        .iter()
        .filter(after)
        .filter(|e| non_pass(e) == Some(true))
        .map(|e| e.id)
        .collect();
    let limit = events.iter().filter(after).find_map(|e| {
        if e.kind == EventKind::ConcernDecided
            && e.payload["escalated_because"].as_str() == Some("revise_limit")
        {
            Some((Trigger::ReviseLimit, e.id))
        } else if e.kind == EventKind::RecoveryRequested
            && e.payload["alert"].as_str() == Some("resume_exhausted")
        {
            Some((Trigger::ResumeExhausted, e.id))
        } else {
            None
        }
    });
    let threshold = if after_diagnosis {
        config.continue_reviews + 1
    } else {
        config.non_pass_reviews
    };
    Window {
        since,
        non_pass,
        threshold,
        after_diagnosis,
        limit,
    }
}

/// What the detection makes of a task now ([`detect`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Detection {
    /// `[replan]` does not enable it.
    Disabled,
    /// The task has an open operation already: no second one.
    Open { operation: super::ReplanId },
    /// Not a candidate yet.
    Below { window: Window },
    /// A candidate: the operation to open, on the evidence of its window.
    Candidate {
        trigger: Trigger,
        evidence: Vec<EventId>,
        window: Window,
    },
}

impl Detection {
    /// The reason of the operation a candidate opens.
    pub fn reason(&self) -> Option<String> {
        let Self::Candidate {
            trigger, window, ..
        } = self
        else {
            return None;
        };
        let since = if window.after_diagnosis {
            "since the task's last diagnosis"
        } else {
            "without a pass"
        };
        Some(match trigger {
            Trigger::NonPassReviews => format!(
                "{} reviews did not pass {since}: the task is diagnosed before more turns go on",
                window.non_pass.len()
            ),
            Trigger::ReviseLimit => format!(
                "the review's revise limit was reached with {} reviews that did not pass {since}",
                window.non_pass.len()
            ),
            Trigger::ResumeExhausted => format!(
                "the run's resumes were used up with {} reviews that did not pass {since}",
                window.non_pass.len()
            ),
        })
    }
}

/// Whether the task whose events are `events` and whose operations are
/// `ops` (every one, open or closed) is a candidate now: one with an open
/// operation is not (one operation per task), and one is when its window
/// holds [`Window::threshold`] non-pass reviews, or the exit of an
/// existing limit after one at least.
pub fn detect(config: &ReplanConfig, events: &[RunEvent], ops: &[ReplanOperation]) -> Detection {
    if !config.enabled {
        return Detection::Disabled;
    }
    if let Some(open) = ops.iter().find(|op| !op.status.is_closed()) {
        return Detection::Open { operation: open.id };
    }
    let window = window(config, events, ops);
    let trigger = if window.non_pass.len() >= window.threshold {
        Some(Trigger::NonPassReviews)
    } else if !window.non_pass.is_empty() {
        window.limit.map(|(trigger, _)| trigger)
    } else {
        None
    };
    match trigger {
        Some(trigger) => {
            let mut evidence = window.non_pass.clone();
            if trigger != Trigger::NonPassReviews
                && let Some((_, id)) = window.limit
            {
                evidence.push(id);
                evidence.sort();
            }
            Detection::Candidate {
                trigger,
                evidence,
                window,
            }
        }
        None => Detection::Below { window },
    }
}

/// Who moved an operation on from its diagnosis: the runtime, applying
/// the diagnosis, or a person, answering its ask.
pub const DECIDED_BY_RUNTIME: &str = "runtime";
pub const DECIDED_BY_PERSON: &str = "person";

/// How many times a diagnosis parked the tasks of `lineage` (the
/// operations of a task and of the tasks that replaced it, and theirs)
/// without a person: each operation the runtime moved past the pause's
/// fence on its diagnosis's `replan`, whatever happened to it after (an
/// operation closed as `continuing` from its diagnosis was never fenced).
pub fn auto_parks(lineage: &[ReplanOperation]) -> usize {
    lineage
        .iter()
        .filter(|op| {
            op.diagnosis.decided_by.as_deref() == Some(DECIDED_BY_RUNTIME)
                && op.diagnosis.choice == Some(DiagnosisChoice::Replan)
                && !matches!(
                    op.status,
                    ReplanStatus::Candidate | ReplanStatus::Diagnosing | ReplanStatus::Continuing
                )
        })
        .count()
}

/// How many diagnosis jobs one operation gets; past it a person decides.
pub const MAX_DIAGNOSIS_ATTEMPTS: usize = 3;

/// The asker of the `decide` ask a diagnosis escalates to.
pub const DIAGNOSIS_ASKER: &str = "replan-diagnosis";

/// The options of that ask: go on with the same conditions, or park the
/// run for the replanning.
pub const DIAGNOSIS_OPTIONS: [&str; 2] = ["continue", "replan"];

/// What the diagnosis job prints on stdout: one JSON object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosisVerdict {
    pub choice: DiagnosisChoice,
    /// The causes it found, the primary one first.
    #[serde(default)]
    pub causes: Vec<ReplanCause>,
    /// `high` or `low`.
    pub confidence: String,
    /// Why, from the evidence: what was compared and why this choice.
    pub reason: String,
    /// The progress the last turns made (what was fixed, what is left),
    /// which a choice to go on rests on.
    #[serde(default)]
    pub progress: String,
    /// How many acceptance items are not met yet.
    #[serde(default)]
    pub remaining: Option<usize>,
    /// What to check next.
    #[serde(default)]
    pub next_check: String,
    /// For a person, when it asks.
    #[serde(default)]
    pub question: String,
}

impl DiagnosisVerdict {
    /// The verdict in the job's stdout: the whole text, or else the
    /// outermost `{...}` in it.
    pub fn parse(stdout: &str) -> Result<Self, String> {
        super::parse_json_object(stdout)
            .map_err(|error| format!("the diagnosis job printed no verdict JSON: {error}"))
    }

    /// Whether it is of `high` confidence.
    pub fn sure(&self) -> bool {
        self.confidence.trim() == "high"
    }

    /// The operation's diagnosis it records, having read until
    /// `window_end`, `decided_by` who moved the operation on.
    pub fn diagnosis(
        &self,
        evidence: Vec<EventId>,
        window_end: Option<EventId>,
        decided_by: Option<&str>,
    ) -> super::task_replan::Diagnosis {
        super::task_replan::Diagnosis {
            evidence,
            causes: self.causes.clone(),
            choice: Some(self.choice),
            confidence: Some(self.confidence.trim().to_owned()),
            next_check: (!self.next_check.trim().is_empty()).then(|| self.next_check.clone()),
            window_end,
            remaining: self.remaining,
            decided_by: decided_by.map(str::to_owned),
            summary: Some(self.reason.clone()),
            premise: None,
        }
    }
}

/// The causes no smaller unit of work removes: a conflict, a verification
/// (an e2e among them) and the environment, provider or host.
pub const fn external(cause: ReplanCause) -> bool {
    matches!(
        cause,
        ReplanCause::Conflict | ReplanCause::Verification | ReplanCause::Environment
    )
}

// Why a diagnosis goes to a person instead of being applied.
string_enum!(AskWhy {
    // The job asked itself.
    JobAsked => "job_asked",
    LowConfidence => "low_confidence",
    // A choice to go on that names no progress to rest on.
    NoEvidence => "no_evidence",
    // The unmet items did not go down over two diagnoses in a row.
    NoProgress => "no_progress",
    // The lineage's parks without a person are used up.
    ParkLimit => "park_limit",
    // A person asked for the replanning; going on is theirs to accept.
    ManualRequest => "manual_request",
    // The job failed, or printed no verdict, MAX_DIAGNOSIS_ATTEMPTS times.
    JobFailed => "job_failed",
    // The run has no run directory for the job to work in.
    NoRunDir => "no_run_dir",
});

impl AskWhy {
    /// Why, as a sentence for the ask.
    pub const fn sentence(self) -> &'static str {
        match self {
            Self::JobAsked => "the diagnosis could not decide and asks a person",
            Self::LowConfidence => "the diagnosis is not sure of its choice",
            Self::NoEvidence => "the diagnosis would go on without naming the progress it rests on",
            Self::NoProgress => {
                "the unmet acceptance items did not go down over two diagnoses in a row"
            }
            Self::ParkLimit => {
                "the parks for a replanning without a person are used up for this task's lineage"
            }
            Self::ManualRequest => {
                "a person asked for this replanning, and the diagnosis would go on instead"
            }
            Self::JobFailed => "the diagnosis job did not give a verdict",
            Self::NoRunDir => "the run has no run directory, so no diagnosis job starts for it",
        }
    }
}

/// Where a diagnosis job of a run works ([`job_place`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobPlace {
    /// Its own directory under the run's directory.
    Dir(std::path::PathBuf),
    /// Not yet: the run is being set up and gets its directory soon, or
    /// it was interrupted before it had one and the task's next run takes
    /// the operation over.
    Wait,
    /// Never: a person decides without a diagnosis ([`AskWhy::NoRunDir`]).
    Ask,
}

/// Where the diagnosis job of a run in `status` with `run_dir` works,
/// `label` naming its directory: under the run's directory only, never the
/// main checkout or anywhere else, as a recovery job and the park.
pub fn job_place(run_dir: Option<&str>, status: super::RunStatus, label: &str) -> JobPlace {
    match run_dir {
        Some(dir) => JobPlace::Dir(std::path::Path::new(dir).join(label)),
        None if matches!(
            status,
            super::RunStatus::Claimed | super::RunStatus::Starting | super::RunStatus::Interrupted
        ) =>
        {
            JobPlace::Wait
        }
        None => JobPlace::Ask,
    }
}

/// What the runtime does with a diagnosis ([`decide`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decided {
    /// Close the operation as `continuing`: the run goes on with the same
    /// conditions, its revises and resumes counted as before.
    Continue(String),
    /// Move the operation past the pause's fence: the run is parked and
    /// its work saved for the planner.
    Park(String),
    /// Ask a person in a `decide` ask, recommending the choice, if any.
    Ask {
        why: AskWhy,
        recommendation: Option<DiagnosisChoice>,
    },
}

/// What [`decide`] reads beyond the verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecideFacts {
    pub origin: ReplanOrigin,
    /// [`auto_parks`] of the task's lineage.
    pub auto_parks: usize,
    pub config: ReplanConfig,
    /// The unmet items the task's earlier diagnoses found, oldest first.
    pub remaining_before: Vec<usize>,
}

/// Whether `remaining`, after `before` (oldest first), did not go down
/// twice in a row.
fn no_progress(before: &[usize], remaining: Option<usize>) -> bool {
    let Some(now) = remaining else {
        return false;
    };
    match before {
        [.., older, last] => last >= older && now >= *last,
        _ => false,
    }
}

/// The runtime's decision on `verdict` (ADR-t1521-1 decisions 2 and 3,
/// ADR-t451-1): a sure choice it applies itself, any other to a person.
/// A replanning only for causes outside the unit of work (a conflict, a
/// verification, the environment) goes on instead, since a smaller unit
/// would meet the same; one past the lineage's parks goes to a person. A
/// choice to go on needs the progress it rests on, goes to a person when
/// the unmet items did not go down twice in a row, and to the person who
/// asked for the replanning.
pub fn decide(verdict: &DiagnosisVerdict, facts: &DecideFacts) -> Decided {
    let ask = |why, recommendation| Decided::Ask {
        why,
        recommendation,
    };
    if verdict.choice == DiagnosisChoice::Ask {
        return ask(AskWhy::JobAsked, None);
    }
    if !verdict.sure() {
        return ask(AskWhy::LowConfidence, Some(verdict.choice));
    }
    // A replanning only for external causes is a choice to go on, held
    // to the same checks as the job's own.
    let external_only = verdict.choice == DiagnosisChoice::Replan
        && verdict.causes.iter().all(|cause| external(*cause));
    if verdict.choice == DiagnosisChoice::Replan && !external_only {
        if facts.auto_parks >= facts.config.max_auto_parks {
            return ask(AskWhy::ParkLimit, Some(DiagnosisChoice::Replan));
        }
        return Decided::Park(format!("diagnosed: {}", verdict.reason));
    }
    // Going on without a count of the unmet items cannot be compared with
    // the earlier diagnoses, so it rests on nothing the runtime can check.
    if verdict.reason.trim().is_empty()
        || verdict.progress.trim().is_empty()
        || verdict.remaining.is_none()
    {
        return ask(AskWhy::NoEvidence, Some(DiagnosisChoice::Continue));
    }
    if no_progress(&facts.remaining_before, verdict.remaining) {
        return ask(AskWhy::NoProgress, Some(DiagnosisChoice::Replan));
    }
    if facts.origin == ReplanOrigin::Manual {
        return ask(AskWhy::ManualRequest, Some(DiagnosisChoice::Continue));
    }
    if external_only {
        let causes: Vec<&str> = verdict.causes.iter().map(|c| c.as_str()).collect();
        return Decided::Continue(format!(
            "the diagnosis found only causes a smaller unit of work would meet as well ({}): the run goes on, and the existing recovery deals with them (progress: {})",
            if causes.is_empty() {
                "none named".to_owned()
            } else {
                causes.join(", ")
            },
            verdict.progress
        ));
    }
    Decided::Continue(format!(
        "diagnosed: {} (progress: {})",
        verdict.reason, verdict.progress
    ))
}

/// The events of a task that are progress of its run, after which a
/// diagnosis that read up to before them no longer holds: a review, a
/// turn requested, started or ended, a receipt seen or validated.
pub const PROGRESS: [EventKind; 6] = [
    EventKind::ReviewFinished,
    EventKind::TurnRequested,
    EventKind::TurnStarted,
    EventKind::TurnFinished,
    EventKind::ReceiptObserved,
    EventKind::ValidationFinished,
];

/// Whether a turn of `run`'s headless session is requested, waits or runs:
/// of the run's turn events in `events` the latest is not `turn_finished`,
/// or a request `listed` in its `turns/` has not had its turn end
/// ([`super::turn::turn_awaited`] against the turn its idle marker names,
/// `mark`). A request written while a turn ran is recorded before that
/// turn's `turn_finished`, so the events alone miss it. A diagnosis starts
/// only between the turns of a run at work, so that the turn's work does
/// not make its verdict stale.
pub fn turn_pending(
    events: &[RunEvent],
    run: &RunId,
    listed: &[super::turn::ListedRequest],
    mark: Option<&super::turn::TurnMark>,
) -> bool {
    let own: Vec<RunEvent> = events
        .iter()
        .filter(|e| e.run_id.as_ref() == Some(run))
        .cloned()
        .collect();
    turn_in_flight(&own, run) || super::turn::turn_awaited(listed, &own, mark)
}

/// Whether a turn of `run` is requested or runs in `events`: of its turn
/// events the latest is not `turn_finished`.
fn turn_in_flight(events: &[RunEvent], run: &RunId) -> bool {
    events
        .iter()
        .filter(|e| {
            e.run_id.as_ref() == Some(run)
                && e.kind.is_in(&[
                    EventKind::TurnRequested,
                    EventKind::TurnStarted,
                    EventKind::TurnFinished,
                ])
        })
        .max_by_key(|e| e.id)
        .is_some_and(|e| e.kind != EventKind::TurnFinished)
}

/// `payload` of the `replan_diagnosed` that opens a diagnosis's ask, once
/// the queue opened `asked` for it under the operation's `generation`: the
/// ask's id, or, when the queue gave the open ask of another asker about
/// the task instead, the diagnosis deferred until it closes.
pub fn asked_payload(payload: &mut Value, asked: &super::Ask, generation: i64) {
    payload["generation"] = serde_json::json!(generation);
    if asked.asked_by == DIAGNOSIS_ASKER {
        payload["outcome"] = serde_json::json!("ask");
        payload["ask_id"] = serde_json::json!(asked.id);
    } else {
        payload["outcome"] = serde_json::json!("deferred");
        payload["reason"] = serde_json::json!(format!("ask {} of the task is open", asked.id));
    }
}

/// `replan_diagnosed`'s `outcome` of a verdict its task's hold kept
/// instead of applying (ADR-t1879-1 decision 6): the operation stays
/// `diagnosing` under the job's generation, and the verdict is applied,
/// its premise checked again, once the hold is released.
pub const HELD_OUTCOME: &str = "held";

/// The key of a `replan_diagnosed` that settles a verdict a hold kept: the
/// id of the [`HELD_OUTCOME`] record it applied, discarded or kept again.
/// Only a record naming it settles it, so a late record of an earlier
/// generation (a supervisor that applied an older kept verdict after it
/// was replaced) leaves the current one to apply.
pub const KEPT_ID: &str = "kept_id";

/// The verdict a hold kept for operation `op`, still `diagnosing` under
/// `generation`, that is neither applied nor discarded yet: of `events`
/// (its task's), the op's last [`HELD_OUTCOME`] record under that
/// generation, unless a later `replan_diagnosed` of the op names it in
/// [`KEPT_ID`]. A claim since (a job or an ask) moved the generation on.
pub fn kept_verdict(
    events: &[RunEvent],
    op: super::ReplanId,
    generation: i64,
) -> Option<&RunEvent> {
    let diagnosed =
        |e: &&RunEvent| e.kind == EventKind::ReplanDiagnosed && PlanningPayload::of(e).is_of(op);
    let kept = events.iter().filter(diagnosed).rfind(|e| {
        e.payload["outcome"] == HELD_OUTCOME
            && PlanningPayload::of(e).generation == Some(generation)
    })?;
    let settled = events.iter().filter(diagnosed).any(|e| {
        e.id > kept.id
            && e.payload
                .get(KEPT_ID)
                .and_then(|id| serde_json::from_value::<EventId>(id.clone()).ok())
                == Some(kept.id)
    });
    (!settled).then_some(kept)
}

/// The fields of a planning event's payload (`replan_requested`,
/// `replan_diagnosis_started`, `replan_diagnosis_failed`,
/// `replan_diagnosed`) the diagnosis decides on, restored typed: a field
/// an older binary did not write, or one it cannot read, is `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct PlanningPayload {
    pub operation_id: Option<super::ReplanId>,
    /// `replan_requested`: who asked.
    pub origin: Option<ReplanOrigin>,
    /// `replan_diagnosed`: the ask it opened.
    pub ask_id: Option<super::AskId>,
    /// The operation's generation the job or the ask claimed it under.
    pub generation: Option<i64>,
    pub window_end: Option<EventId>,
    /// `replan_diagnosis_started`: the supervisor that runs the job.
    pub supervisor_pid: Option<u32>,
    /// `replan_diagnosis_started`: that supervisor's registration (none of
    /// an older binary's start).
    pub supervisor_token: Option<String>,
    /// `replan_diagnosis_started`: when that supervisor's process started,
    /// as [`crate::application::ProcessControl::start_identity`] reads it
    /// (none of an older binary's start, or when it could not be read).
    pub supervisor_process_start: Option<String>,
    /// `replan_diagnosed` and `replan_diagnosis_failed` of a job: the
    /// generation its start claimed the operation under, which ties the
    /// end to that start (an ask's record or another attempt's names
    /// another one, or none).
    pub started_generation: Option<i64>,
    /// The job's attempt, which ties an older binary's end, which names no
    /// generation, to its start.
    pub attempt: Option<usize>,
    /// `replan_diagnosed`: the job's verdict.
    pub verdict: Option<DiagnosisVerdict>,
}

impl PlanningPayload {
    /// `event`'s payload, each field read on its own.
    pub fn of(event: &RunEvent) -> Self {
        Self::from_value(&event.payload)
    }

    /// The fields of `payload`, each read on its own.
    pub fn from_value(payload: &Value) -> Self {
        let field = |key: &str| payload.get(key).cloned().unwrap_or(Value::Null);
        fn typed<T: serde::de::DeserializeOwned>(value: Value) -> Option<T> {
            serde_json::from_value(value).ok()
        }
        Self {
            operation_id: typed(field("operation_id")),
            origin: typed(field("origin")),
            ask_id: typed(field("ask_id")),
            generation: typed(field("generation")),
            window_end: typed(field("window_end")),
            supervisor_pid: typed(field("supervisor_pid")),
            supervisor_token: typed(field("supervisor_token")),
            supervisor_process_start: typed(field("supervisor_process_start")),
            started_generation: typed(field("started_generation")),
            attempt: typed(field("attempt")),
            verdict: typed(field("verdict")),
        }
    }

    /// Whether it is of operation `op`.
    pub fn is_of(&self, op: super::ReplanId) -> bool {
        self.operation_id == Some(op)
    }

    /// The supervisor a `replan_diagnosis_started` names as its job's
    /// owner, `None` when it names no pid.
    pub fn owner(&self) -> Option<JobOwner> {
        Some(JobOwner {
            pid: self.supervisor_pid?,
            token: self.supervisor_token.clone(),
            process_start: self.supervisor_process_start.clone(),
        })
    }
}

/// The supervisor that owns a diagnosis job, as its start names it: its
/// pid, and the token of its registration and its process's start, which
/// tell it from another process that took the pid later or from a
/// supervisor of another pid namespace that has the same pid. A start of
/// an older binary names the pid only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JobOwner {
    pub pid: u32,
    pub token: Option<String>,
    pub process_start: Option<String>,
}

/// The owner of a diagnosis job the queue records as running, with the
/// `supervisors` row of its token: the row's `heartbeat_at` (unix
/// seconds), `None` when no row has its token or it names none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningOwner {
    pub owner: JobOwner,
    pub heartbeat_at: Option<i64>,
}

/// What a supervisor sees of the owner of a diagnosis job besides the
/// queue's records: itself (its token and pid), and the process that has
/// the owner's pid now: whether one does and when it started (`None` when
/// it cannot be read).
#[derive(Debug, Clone, Copy)]
pub struct OwnerSeen<'a> {
    pub me: (&'a str, u32),
    pub pid_alive: bool,
    pub pid_start: Option<&'a str>,
}

/// Whether `owner`, with `heartbeat_at` its registration's heartbeat
/// ([`RunningOwner`], `None` when no `supervisors` row has its token),
/// still runs its diagnosis job for the supervisor that sees it as `seen`.
/// An owner that names a token runs when it is not this supervisor's (a
/// job of it before an exec, which keeps its token, runs no more) and
/// either its registration is there, whatever its heartbeat (an old one,
/// as every one is right after the host wakes from sleep, is not read
/// here: the registration of a supervisor that is gone is pruned by a live
/// one, [`super::supervisor_prune::left_behind`]), or, its registration
/// gone, its pid runs the process that started when it did, whichever
/// process that is here: a pid taken by another process later, or whose
/// start cannot be told, runs no job. Its pid is not compared with this
/// process's, so another pid namespace's supervisor with the same pid is
/// not taken for this one. A start of an older binary, which names no
/// token, is judged by its pid alone, as that binary did, since its
/// supervisor may still run: it runs when the pid is not this process's
/// and a process has it (one that started at another time than a recorded
/// start runs none).
pub fn owner_runs(owner: &JobOwner, heartbeat_at: Option<i64>, seen: &OwnerSeen<'_>) -> bool {
    let (token, pid) = seen.me;
    let started_alike = |recorded: &str| seen.pid_start == Some(recorded);
    match &owner.token {
        Some(owner_token) => {
            owner_token != token
                && (heartbeat_at.is_some()
                    || seen.pid_alive && owner.process_start.as_deref().is_some_and(started_alike))
        }
        None => {
            owner.pid != pid
                && seen.pid_alive
                && owner.process_start.as_deref().is_none_or(started_alike)
        }
    }
}

/// The owners of the diagnosis jobs the queue records as running, split
/// by [`owner_runs`] (`seen` gives what the caller sees of each): those
/// that run theirs, whose job is not started again, and those gone, whose
/// job is taken as ended so that the next attempt starts.
pub fn split_owners<'a>(
    owners: &[RunningOwner],
    seen: impl Fn(&JobOwner) -> OwnerSeen<'a>,
) -> (Vec<JobOwner>, Vec<JobOwner>) {
    let (live, gone): (Vec<&RunningOwner>, Vec<&RunningOwner>) = owners
        .iter()
        .partition(|o| owner_runs(&o.owner, o.heartbeat_at, &seen(&o.owner)));
    let owners = |of: Vec<&RunningOwner>| of.into_iter().map(|o| o.owner.clone()).collect();
    (owners(live), owners(gone))
}

/// Who runs one diagnosis job at a time: a registration's token, or the
/// pid of a start that names none.
type OwnerKey<'a> = (Option<&'a str>, Option<u32>);

/// A diagnosis job that runs: its operation, the generation its start
/// claimed it under and the supervisor that owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningJob {
    pub operation: super::ReplanId,
    pub generation: i64,
    pub owner: JobOwner,
}

/// A planning event read as a record of a diagnosis job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobRecord {
    /// `replan_diagnosis_started`.
    Started(PlanningPayload),
    /// The end of a job: every `replan_diagnosis_failed` (the job did not
    /// start, or gave no verdict), and a `replan_diagnosed` of the job's
    /// verdict (applied, discarded or asked about).
    Ended(PlanningPayload),
}

impl JobRecord {
    /// Event `kind` with `payload` as a record of a job, `None` for one that
    /// is none: a `replan_diagnosed` of an ask without a verdict ends no
    /// job. A `replan_diagnosed` is a job's end when it names its start's
    /// generation or, written by an older binary that named none, when it
    /// carries the job's verdict or its duration, which an ask's own
    /// record never does.
    pub fn of(kind: EventKind, payload: &Value) -> Option<Self> {
        let read = PlanningPayload::from_value(payload);
        match kind {
            EventKind::ReplanDiagnosisStarted => Some(Self::Started(read)),
            EventKind::ReplanDiagnosisFailed => Some(Self::Ended(read)),
            EventKind::ReplanDiagnosed
                if read.started_generation.is_some()
                    || payload.get("verdict").is_some_and(|v| !v.is_null())
                    || payload.get("duration_secs").is_some() =>
            {
                Some(Self::Ended(read))
            }
            _ => None,
        }
    }
}

impl PlanningPayload {
    /// Whether this end of a job of its operation ends `start`, a start of
    /// the same operation before it: the one whose generation it names, or,
    /// for an older binary's end that names none, the one of its attempt,
    /// or, naming neither, any.
    pub fn ends(&self, start: &PlanningPayload) -> bool {
        match (self.started_generation, self.attempt) {
            (Some(generation), _) => start.generation == Some(generation),
            (None, Some(attempt)) => start.attempt == Some(attempt),
            (None, None) => true,
        }
    }
}

/// The diagnosis jobs that run now, queue-wide, from the records of every
/// operation's jobs in their order ([`JobRecord`]): each owner's latest
/// start that no end of a job tied to it came after
/// ([`PlanningPayload::ends`]), its owner not among `gone` (ended, or the
/// caller itself, which runs none). A supervisor (one registration, or
/// one pid for a start that names none: [`JobOwner`]) runs one job at a
/// time, so its later start ends its earlier one. A job runs on whatever its
/// operation does meanwhile (a later claim, a new run that takes the
/// operation over, its close) until its end is recorded; a start that
/// names no owner or generation has none to wait for.
pub fn running_jobs(records: &[JobRecord], gone: &[JobOwner]) -> Vec<RunningJob> {
    let mut by_owner: BTreeMap<OwnerKey<'_>, (RunningJob, &PlanningPayload)> = BTreeMap::new();
    for record in records {
        match record {
            JobRecord::Started(read) => {
                if let (Some(operation), Some(owner), Some(generation)) =
                    (read.operation_id, read.owner(), read.generation)
                {
                    let job = RunningJob {
                        operation,
                        generation,
                        owner,
                    };
                    let key = match &read.supervisor_token {
                        Some(token) => (Some(token.as_str()), None),
                        None => (None, Some(job.owner.pid)),
                    };
                    by_owner.insert(key, (job, read));
                }
            }
            JobRecord::Ended(end) => {
                by_owner.retain(|_, (job, start)| {
                    end.operation_id != Some(job.operation) || !end.ends(start)
                });
            }
        }
    }
    by_owner
        .into_values()
        .map(|(job, _)| job)
        .filter(|job| !gone.contains(&job.owner))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::task_replan::{Diagnosis, OriginalConditions};
    use crate::domain::{ReplanId, TaskId};
    use serde_json::json;

    fn run(id: &str) -> RunId {
        RunId::new(id).unwrap()
    }

    fn event(id: i64, run_id: &str, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(7)),
            goal_id: None,
            run_id: Some(run(run_id)),
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    /// A verdict kept for its task's hold is the operation's to apply
    /// while it is the op's last kept record under the generation it was
    /// kept at and no later record of the op names it; a claim since (a
    /// new generation) or another operation's records leave none of it.
    #[test]
    fn a_kept_verdict_is_the_operations_last_record_under_its_generation() {
        let op = ReplanId::new(3);
        let kept = |id: i64, generation: i64| {
            event(
                id,
                "r1",
                "replan_diagnosed",
                json!({"operation_id": 3, "outcome": HELD_OUTCOME, "generation": generation}),
            )
        };
        let settling = |id: i64, outcome: &str, kept: i64| {
            event(
                id,
                "r1",
                "replan_diagnosed",
                json!({"operation_id": 3, "outcome": outcome, KEPT_ID: kept}),
            )
        };
        let started = event(
            1,
            "r1",
            "replan_diagnosis_started",
            json!({"operation_id": 3}),
        );
        let events = vec![started.clone(), kept(2, 4)];
        assert_eq!(
            kept_verdict(&events, op, 4).map(|e| e.id),
            Some(EventId::new(2))
        );
        assert!(kept_verdict(&events, op, 5).is_none(), "claimed since");
        assert!(kept_verdict(&events, ReplanId::new(9), 4).is_none());
        let other = event(
            3,
            "r1",
            "replan_diagnosed",
            json!({"operation_id": 9, "outcome": "continue", KEPT_ID: 2}),
        );
        assert!(
            kept_verdict(&[started.clone(), kept(2, 4), other], op, 4).is_some(),
            "another op's records"
        );
        for outcome in ["continue", "park", "discarded", "ask"] {
            let applied = settling(3, outcome, 2);
            assert!(
                kept_verdict(&[started.clone(), kept(2, 4), applied], op, 4).is_none(),
                "{outcome}"
            );
        }
        // Kept again (a hold came back before the apply): the new record is
        // the one to apply, the old one settled by it.
        let again = event(
            3,
            "r1",
            "replan_diagnosed",
            json!({"operation_id": 3, "outcome": HELD_OUTCOME, "generation": 4, KEPT_ID: 2}),
        );
        assert_eq!(
            kept_verdict(&[started, kept(2, 4), again], op, 4).map(|e| e.id),
            Some(EventId::new(3))
        );
    }

    /// The write order of two supervisors: B discards the verdict kept at
    /// generation 4, diagnoses again under 5 and keeps that verdict for a
    /// new hold; then A, which read the generation-4 record before, writes
    /// its late discard of it. The generation-5 verdict is still the one
    /// to apply, not hidden by that last record.
    #[test]
    fn a_late_record_of_an_earlier_generation_leaves_the_current_kept_verdict() {
        let op = ReplanId::new(3);
        let events = vec![
            event(
                1,
                "r1",
                "replan_diagnosis_started",
                json!({"operation_id": 3, "generation": 4}),
            ),
            event(
                2,
                "r1",
                "replan_diagnosed",
                json!({"operation_id": 3, "outcome": HELD_OUTCOME, "generation": 4}),
            ),
            event(
                3,
                "r1",
                "replan_diagnosed",
                json!({"operation_id": 3, "outcome": "discarded", KEPT_ID: 2}),
            ),
            event(
                4,
                "r1",
                "replan_diagnosis_started",
                json!({"operation_id": 3, "generation": 5}),
            ),
            event(
                5,
                "r1",
                "replan_diagnosed",
                json!({"operation_id": 3, "outcome": HELD_OUTCOME, "generation": 5}),
            ),
            event(
                6,
                "r1",
                "replan_diagnosed",
                json!({"operation_id": 3, "outcome": "discarded", KEPT_ID: 2}),
            ),
        ];
        assert_eq!(
            kept_verdict(&events, op, 5).map(|e| e.id),
            Some(EventId::new(5))
        );
        assert!(kept_verdict(&events, op, 4).is_none());
    }

    fn review(id: i64, run_id: &str, verdict: &str) -> RunEvent {
        event(id, run_id, "review_finished", json!({"verdict": verdict}))
    }

    fn turn(id: i64, run_id: &str, what: &str) -> RunEvent {
        event(id, run_id, "turn_started", json!({"what": what}))
    }

    fn op(status: ReplanStatus, diagnosis: Diagnosis) -> ReplanOperation {
        ReplanOperation {
            id: ReplanId::new(1),
            task_id: TaskId::new(7),
            run_id: run("r1"),
            goal_id: None,
            origin: ReplanOrigin::Runtime,
            status,
            status_reason: None,
            generation: 1,
            return_phase: None,
            reason: "r".into(),
            conditions: OriginalConditions {
                title: String::new(),
                description: String::new(),
                acceptance: Vec::new(),
                paths: Vec::new(),
                verify: Vec::new(),
                evidence: Value::Null,
                predecessors: Vec::new(),
                successors: Vec::new(),
                goal: None,
            },
            task_version: String::new(),
            goal_acceptance_version: None,
            diagnosis,
            snapshot_id: None,
            request_id: None,
            request_status: None,
            proposal_id: None,
            requested_by: "supervisor".into(),
            requested_by_id: "supervisor".into(),
            mappings: Vec::new(),
            replaced_by: Vec::new(),
            created_at: 0,
            updated_at: 0,
            closed_at: None,
        }
    }

    fn diagnosed(until: i64, choice: DiagnosisChoice) -> Diagnosis {
        Diagnosis {
            choice: Some(choice),
            window_end: Some(EventId::new(until)),
            decided_by: Some(DECIDED_BY_RUNTIME.into()),
            ..Diagnosis::default()
        }
    }

    fn enabled() -> ReplanConfig {
        ReplanConfig {
            enabled: true,
            ..ReplanConfig::default()
        }
    }

    fn trigger(detection: &Detection) -> Option<Trigger> {
        match detection {
            Detection::Candidate { trigger, .. } => Some(*trigger),
            _ => None,
        }
    }

    /// The turns are told apart by cause across runs: a resolution request
    /// takes its run's latest trigger, the turn after a failed one is a
    /// provider's, and a request of no known kind is `unlabeled`.
    #[test]
    fn turns_are_counted_by_cause_per_run_and_for_the_task() {
        let events = vec![
            turn(1, "r1", "the task's prompt"),
            event(2, "r1", "turn_finished", json!({})),
            review(3, "r1", "revise"),
            turn(4, "r1", "revise request"),
            event(5, "r1", "turn_finished", json!({"failure": "usage_limit"})),
            turn(6, "r1", "provider switch"),
            event(7, "r1", "conflict_precheck", json!({})),
            turn(8, "r1", "resolution request"),
            event(9, "r1", "run_e2e_failed", json!({})),
            turn(10, "r1", "resolution request"),
            event(
                11,
                "r1",
                "landing_recheck_failed",
                json!({"code": "verification_failed"}),
            ),
            turn(12, "r1", "resolution request"),
            event(13, "r1", "validation_finished", json!({"accepted": false})),
            turn(14, "r1", "nudge"),
            turn(15, "r1", "answer of ask 9"),
            review(16, "r1", "pass"),
            turn(17, "r2", "the task's prompt"),
            turn(18, "r2", "something new"),
            event(
                19,
                "r2",
                "resume_started",
                json!({"reason": "the review's findings were sent back by ask 3: x"}),
            ),
            turn(20, "r2", "resolution request"),
            review(21, "r2", "concern"),
        ];
        let turns = count_turns(&events);
        assert_eq!(turns.runs.len(), 2);
        let first = &turns.runs[0].counts;
        assert_eq!((first.started, first.finished), (8, 2));
        assert_eq!((first.reviews, first.non_pass, first.passes), (2, 1, 1));
        let by = |counts: &TurnCounts, cause: TurnCause| counts.by_cause[cause.as_str()];
        assert_eq!(by(first, TurnCause::Initial), 1);
        assert_eq!(by(first, TurnCause::Review), 1);
        assert_eq!(by(first, TurnCause::ProviderHost), 1);
        assert_eq!(by(first, TurnCause::Conflict), 1);
        assert_eq!(by(first, TurnCause::E2e), 1);
        assert_eq!(by(first, TurnCause::Verification), 1);
        assert_eq!(by(first, TurnCause::Receipt), 1);
        assert_eq!(by(first, TurnCause::WorkerAnswer), 1);
        let second = &turns.runs[1].counts;
        assert_eq!(by(second, TurnCause::Unlabeled), 1);
        assert_eq!(by(second, TurnCause::Review), 1);
        assert_eq!(turns.total.started, 11);
        assert_eq!(turns.total.non_pass, 2);
        assert_eq!(by(&turns.total, TurnCause::Initial), 2);
        assert_eq!(turns.total.by_cause.len(), TurnCause::ALL.len());
    }

    /// A window counts only its own events, each turn still of the cause
    /// the events before it give.
    #[test]
    fn a_window_counts_its_turns_by_the_causes_before_it() {
        let events = vec![
            event(1, "r1", "run_e2e_failed", json!({})),
            turn(2, "r1", "resolution request"),
            review(3, "r1", "revise"),
        ];
        let turns = count_turns_where(&events, |e| e.id > EventId::new(1));
        assert_eq!(turns.total.by_cause[TurnCause::E2e.as_str()], 1);
        assert_eq!((turns.total.started, turns.total.non_pass), (1, 1));
        let none = count_turns_where(&events, |e| e.id > EventId::new(3));
        assert_eq!((none.total.started, none.total.reviews), (0, 0));
    }

    /// The threshold is reached on the non-pass reviews alone: one short
    /// of it is no candidate, whatever the turns between them (conflicts,
    /// answers, a provider's failure), and a pass starts over.
    #[test]
    fn a_candidate_needs_the_non_pass_reviews_in_a_row_whatever_the_other_turns() {
        let config = enabled();
        let mut events = vec![
            review(1, "r1", "revise"),
            turn(2, "r1", "conflict request"),
            turn(3, "r1", "answer of ask 4"),
            event(4, "r1", "turn_finished", json!({"failure": "other"})),
            turn(5, "r1", "provider switch"),
            review(6, "r1", "concern"),
        ];
        let below = detect(&config, &events, &[]);
        assert_eq!(trigger(&below), None, "{below:?}");
        let Detection::Below { window } = below else {
            unreachable!()
        };
        assert_eq!(window.non_pass.len(), 2);
        assert_eq!(window.threshold, 3);
        events.push(review(7, "r1", "revise"));
        let detected = detect(&config, &events, &[]);
        assert_eq!(trigger(&detected), Some(Trigger::NonPassReviews));
        let Detection::Candidate { evidence, .. } = &detected else {
            unreachable!()
        };
        assert_eq!(evidence, &[1, 6, 7].map(EventId::new));
        assert!(
            detected
                .reason()
                .unwrap()
                .contains("3 reviews did not pass")
        );
        // A pass starts the window over.
        events.push(review(8, "r1", "pass"));
        events.push(review(9, "r1", "revise"));
        assert_eq!(trigger(&detect(&config, &events, &[])), None);
    }

    /// The window runs across the task's runs (a retry, the other
    /// provider) and is not started over by them.
    #[test]
    fn the_window_spans_the_tasks_runs_and_providers() {
        let events = vec![
            review(1, "r1", "revise"),
            review(2, "r1", "revise"),
            event(3, "r2", "run_claimed", json!({"provider": "codex"})),
            review(4, "r2", "revise"),
        ];
        let detected = detect(&enabled(), &events, &[]);
        assert_eq!(trigger(&detected), Some(Trigger::NonPassReviews));
    }

    /// Nothing is detected without `enabled`, nor beside an open operation.
    #[test]
    fn disabled_or_an_open_operation_detects_nothing() {
        let events: Vec<RunEvent> = (1..=5).map(|id| review(id, "r1", "revise")).collect();
        assert_eq!(
            detect(&ReplanConfig::default(), &events, &[]),
            Detection::Disabled
        );
        let open = op(ReplanStatus::Diagnosing, Diagnosis::default());
        assert_eq!(
            detect(&enabled(), &events, &[open]),
            Detection::Open {
                operation: ReplanId::new(1)
            }
        );
    }

    /// A window is diagnosed once: after a diagnosis that went on, the
    /// next candidate is the third non-pass review after the events it
    /// read, the reviews it read do not count again, and the window does
    /// not reopen on a restart (the diagnosis is the queue's record).
    #[test]
    fn after_a_diagnosis_went_on_the_next_candidate_is_past_the_continue_reviews() {
        let config = enabled();
        let mut events: Vec<RunEvent> = (1..=3).map(|id| review(id, "r1", "revise")).collect();
        let done = op(
            ReplanStatus::Continuing,
            diagnosed(4, DiagnosisChoice::Continue),
        );
        let ops = [done];
        let Detection::Below { window } = detect(&config, &events, &ops) else {
            panic!("the diagnosed window is a candidate again");
        };
        assert!(window.after_diagnosis);
        assert_eq!(window.threshold, 3);
        events.push(review(5, "r1", "revise"));
        events.push(review(6, "r1", "concern"));
        assert_eq!(trigger(&detect(&config, &events, &ops)), None);
        events.push(review(7, "r1", "revise"));
        assert_eq!(
            trigger(&detect(&config, &events, &ops)),
            Some(Trigger::NonPassReviews)
        );
        // `continue_reviews` sets how many go on.
        let one = ReplanConfig {
            continue_reviews: 1,
            ..config
        };
        assert_eq!(
            trigger(&detect(&one, &events[..4], &ops)),
            None,
            "one review after the diagnosis is within the one let go on"
        );
        assert_eq!(
            trigger(&detect(&one, &events[..5], &ops)),
            Some(Trigger::NonPassReviews)
        );
    }

    /// The exits of the existing limits make a candidate of a window that
    /// has a non-pass review: the revise limit and the resumes used up;
    /// without one (only conflicts, say) they do not.
    #[test]
    fn the_revise_limit_and_the_resumes_used_up_make_a_candidate_of_a_reviewed_window() {
        let config = enabled();
        let limit = event(
            3,
            "r1",
            "concern_decided",
            json!({"escalated_because": "revise_limit"}),
        );
        let events = vec![review(1, "r1", "revise"), limit];
        let detected = detect(&config, &events, &[]);
        assert_eq!(trigger(&detected), Some(Trigger::ReviseLimit));
        let Detection::Candidate { evidence, .. } = &detected else {
            unreachable!()
        };
        assert_eq!(evidence, &[1, 3].map(EventId::new));
        let exhausted = event(
            2,
            "r1",
            "recovery_requested",
            json!({"alert": "resume_exhausted"}),
        );
        assert_eq!(
            trigger(&detect(
                &config,
                &[review(1, "r1", "concern"), exhausted.clone()],
                &[]
            )),
            Some(Trigger::ResumeExhausted)
        );
        // Only conflicts used the resumes up: no review to diagnose.
        assert_eq!(
            trigger(&detect(
                &config,
                &[turn(1, "r1", "conflict request"), exhausted],
                &[]
            )),
            None
        );
    }

    fn verdict(
        choice: DiagnosisChoice,
        causes: &[ReplanCause],
        confidence: &str,
    ) -> DiagnosisVerdict {
        DiagnosisVerdict {
            choice,
            causes: causes.to_vec(),
            confidence: confidence.into(),
            reason: "the findings repeat on one mechanism".into(),
            progress: "two of three findings fixed".into(),
            remaining: Some(2),
            next_check: String::new(),
            question: String::new(),
        }
    }

    fn facts() -> DecideFacts {
        DecideFacts {
            origin: ReplanOrigin::Runtime,
            auto_parks: 0,
            config: enabled(),
            remaining_before: Vec::new(),
        }
    }

    /// A sure replanning for a cause of the unit of work parks; one only
    /// for causes outside it goes on instead, never split.
    #[test]
    fn only_a_cause_of_the_unit_of_work_parks_the_run() {
        let parks = decide(
            &verdict(
                DiagnosisChoice::Replan,
                &[ReplanCause::OversizedUnit, ReplanCause::Conflict],
                "high",
            ),
            &facts(),
        );
        assert!(matches!(parks, Decided::Park(_)), "{parks:?}");
        for causes in [
            &[ReplanCause::Conflict][..],
            &[ReplanCause::Verification, ReplanCause::Environment],
            &[],
        ] {
            let decided = decide(&verdict(DiagnosisChoice::Replan, causes, "high"), &facts());
            assert!(
                matches!(decided, Decided::Continue(_)),
                "{causes:?}: {decided:?}"
            );
        }
    }

    /// Past the lineage's parks, a low confidence, the job's own ask and a
    /// continuation without progress go to a person.
    #[test]
    fn what_the_runtime_does_not_decide_goes_to_a_person() {
        let at_limit = DecideFacts {
            auto_parks: 3,
            ..facts()
        };
        let replan = verdict(
            DiagnosisChoice::Replan,
            &[ReplanCause::OversizedUnit],
            "high",
        );
        assert_eq!(
            decide(&replan, &at_limit),
            Decided::Ask {
                why: AskWhy::ParkLimit,
                recommendation: Some(DiagnosisChoice::Replan)
            }
        );
        assert_eq!(
            decide(
                &verdict(
                    DiagnosisChoice::Replan,
                    &[ReplanCause::OversizedUnit],
                    "low"
                ),
                &facts()
            ),
            Decided::Ask {
                why: AskWhy::LowConfidence,
                recommendation: Some(DiagnosisChoice::Replan)
            }
        );
        assert_eq!(
            decide(&verdict(DiagnosisChoice::Ask, &[], "high"), &facts()),
            Decided::Ask {
                why: AskWhy::JobAsked,
                recommendation: None
            }
        );
        let mut bare = verdict(
            DiagnosisChoice::Continue,
            &[ReplanCause::LocalDefect],
            "high",
        );
        bare.progress = " ".into();
        assert_eq!(
            decide(&bare, &facts()),
            Decided::Ask {
                why: AskWhy::NoEvidence,
                recommendation: Some(DiagnosisChoice::Continue)
            }
        );
        let manual = DecideFacts {
            origin: ReplanOrigin::Manual,
            ..facts()
        };
        let goes_on = verdict(
            DiagnosisChoice::Continue,
            &[ReplanCause::LocalDefect],
            "high",
        );
        assert_eq!(
            decide(&goes_on, &manual),
            Decided::Ask {
                why: AskWhy::ManualRequest,
                recommendation: Some(DiagnosisChoice::Continue)
            }
        );
        assert!(matches!(decide(&goes_on, &facts()), Decided::Continue(_)));
    }

    /// The continuation is capped by progress: the unmet items not going
    /// down twice in a row sends it to a person, recommending the
    /// replanning; going down once lets it go on.
    #[test]
    fn going_on_twice_without_fewer_unmet_items_goes_to_a_person() {
        let goes_on = verdict(
            DiagnosisChoice::Continue,
            &[ReplanCause::LocalDefect],
            "high",
        );
        let stuck = DecideFacts {
            remaining_before: vec![2, 2],
            ..facts()
        };
        assert_eq!(
            decide(&goes_on, &stuck),
            Decided::Ask {
                why: AskWhy::NoProgress,
                recommendation: Some(DiagnosisChoice::Replan)
            }
        );
        for before in [vec![3, 3], vec![3], vec![]] {
            let facts = DecideFacts {
                remaining_before: before.clone(),
                ..facts()
            };
            assert!(
                matches!(decide(&goes_on, &facts), Decided::Continue(_)),
                "{before:?}"
            );
        }
    }

    /// A replanning only for external causes goes on under the checks of
    /// any choice to go on: a person's request asks them, unmet items that
    /// did not go down ask, and so does a verdict with no count of them.
    #[test]
    fn going_on_for_external_causes_meets_the_checks_of_any_continuation() {
        let external = verdict(DiagnosisChoice::Replan, &[ReplanCause::Conflict], "high");
        let manual = DecideFacts {
            origin: ReplanOrigin::Manual,
            ..facts()
        };
        assert_eq!(
            decide(&external, &manual),
            Decided::Ask {
                why: AskWhy::ManualRequest,
                recommendation: Some(DiagnosisChoice::Continue)
            }
        );
        let stuck = DecideFacts {
            remaining_before: vec![2, 2],
            ..facts()
        };
        assert_eq!(
            decide(&external, &stuck),
            Decided::Ask {
                why: AskWhy::NoProgress,
                recommendation: Some(DiagnosisChoice::Replan)
            }
        );
        for choice in [DiagnosisChoice::Replan, DiagnosisChoice::Continue] {
            let mut uncounted = verdict(choice, &[ReplanCause::Environment], "high");
            uncounted.remaining = None;
            assert_eq!(
                decide(&uncounted, &facts()),
                Decided::Ask {
                    why: AskWhy::NoEvidence,
                    recommendation: Some(DiagnosisChoice::Continue)
                },
                "{choice:?}"
            );
        }
    }

    /// A planning event's fields are read typed, each on its own: one an
    /// older binary did not write, or of another type, is none.
    #[test]
    fn a_planning_events_fields_are_restored_typed() {
        let read = PlanningPayload::of(&event(
            1,
            "run-1",
            "replan_requested",
            json!({"operation_id": 4, "origin": "manual", "generation": "x"}),
        ));
        assert!(read.is_of(crate::domain::ReplanId::new(4)));
        assert!(!read.is_of(crate::domain::ReplanId::new(5)));
        assert_eq!(read.origin, Some(ReplanOrigin::Manual));
        assert_eq!(read.generation, None);
        assert_eq!(read.ask_id, None);
    }

    /// A job works under the run's directory only: without one it waits
    /// while the run is set up or after it was interrupted before it had
    /// one, and a person is asked otherwise.
    #[test]
    fn a_job_works_only_under_the_runs_directory() {
        use crate::domain::RunStatus;
        assert_eq!(
            job_place(Some("/runs/r1"), RunStatus::Running, "replan_diagnosis"),
            JobPlace::Dir("/runs/r1/replan_diagnosis".into())
        );
        for status in [
            RunStatus::Claimed,
            RunStatus::Starting,
            RunStatus::Interrupted,
        ] {
            assert_eq!(job_place(None, status, "replan_diagnosis"), JobPlace::Wait);
        }
        for status in [
            RunStatus::Running,
            RunStatus::NeedsSession,
            RunStatus::AwaitingIntegration,
        ] {
            assert_eq!(job_place(None, status, "replan_diagnosis"), JobPlace::Ask);
        }
    }

    /// A turn requested or started and not ended holds a diagnosis back;
    /// one ended, or another run's, does not.
    #[test]
    fn a_diagnosis_waits_for_the_runs_turn_to_end() {
        let started = |id| event(id, "run-1", "turn_started", json!({}));
        let finished = |id| event(id, "run-1", "turn_finished", json!({}));
        let other = event(5, "run-2", "turn_started", json!({}));
        assert!(!turn_in_flight(&[], &run("run-1")));
        assert!(turn_in_flight(&[started(1)], &run("run-1")));
        assert!(!turn_in_flight(&[started(1), finished(2)], &run("run-1")));
        assert!(turn_in_flight(
            &[started(1), finished(2), started(3)],
            &run("run-1")
        ));
        assert!(!turn_in_flight(
            &[started(1), finished(2), other],
            &run("run-1")
        ));
        let requested = event(4, "run-1", "turn_requested", json!({}));
        assert!(turn_in_flight(
            &[started(1), finished(2), requested],
            &run("run-1")
        ));
    }

    /// A request written while a turn ran is recorded before that turn's
    /// end, so the latest turn event is `turn_finished` while the request
    /// still waits: the session's `turns/` and idle marker hold the
    /// diagnosis back until a turn taken from it, or a later one, ended.
    #[test]
    fn a_diagnosis_waits_for_a_request_written_during_the_last_turn() {
        use crate::domain::turn::{
            ListedRequest, RequestState, TurnMark, TurnOutcome, TurnRequest,
        };
        let started = |id, turn: u64, request: Option<u64>| {
            event(
                id,
                "run-1",
                "turn_started",
                json!({"turn": turn, "request": request}),
            )
        };
        let requested = |id, seq: u64| event(id, "run-1", "turn_requested", json!({"seq": seq}));
        let finished = |id, turn: u64| event(id, "run-1", "turn_finished", json!({"turn": turn}));
        let mark = |turn| TurnMark {
            turn,
            outcome: TurnOutcome::Succeeded,
            failure: None,
            permission_denials: 0,
        };
        let listed = |state| {
            vec![ListedRequest {
                state,
                request: TurnRequest {
                    seq: 1,
                    what: "continue".to_owned(),
                    prompt: "go on".to_owned(),
                },
            }]
        };
        let run1 = run("run-1");
        // Turn 1 runs, request 1 is written, then turn 1 ends.
        let written = [started(1, 1, None), requested(2, 1), finished(3, 1)];
        assert!(!turn_in_flight(&written, &run1), "the events alone miss it");
        assert!(turn_pending(
            &written,
            &run1,
            &listed(RequestState::Pending),
            Some(&mark(1))
        ));
        // Taken, its turn not started yet.
        assert!(turn_pending(
            &written,
            &run1,
            &listed(RequestState::Taken),
            Some(&mark(1))
        ));
        // Its turn runs.
        let running = [
            started(1, 1, None),
            requested(2, 1),
            finished(3, 1),
            started(4, 2, Some(1)),
        ];
        assert!(turn_pending(
            &running,
            &run1,
            &listed(RequestState::Taken),
            Some(&mark(1))
        ));
        // Its turn ended: between turns.
        let ended = [
            started(1, 1, None),
            requested(2, 1),
            finished(3, 1),
            started(4, 2, Some(1)),
            finished(5, 2),
        ];
        assert!(!turn_pending(
            &ended,
            &run1,
            &listed(RequestState::Taken),
            Some(&mark(2))
        ));
        // Another run's turn neither holds it back nor counts as its own.
        let mut other = ended.to_vec();
        other.push(event(6, "run-2", "turn_started", json!({"turn": 9})));
        assert!(!turn_pending(
            &other,
            &run1,
            &listed(RequestState::Taken),
            Some(&mark(2))
        ));
        // No request and the last turn ended: between turns.
        assert!(!turn_pending(
            &[started(1, 1, None), finished(2, 1)],
            &run1,
            &[],
            Some(&mark(1))
        ));
    }

    /// The HEAD a diagnosis read is compared only when it was read both
    /// times.
    #[test]
    fn a_moved_head_makes_the_premise_stale() {
        let premise = |head: Option<&str>| crate::domain::task_replan::DiagnosisPremise {
            run_status: crate::domain::RunStatus::Running,
            read_until: None,
            head: head.map(str::to_owned),
            manual: None,
            max_auto_parks: None,
        };
        assert!(premise(Some("aaa")).head_moved(Some("bbb")));
        assert!(!premise(Some("aaa")).head_moved(Some("aaa")));
        assert!(!premise(Some("aaa")).head_moved(None));
        assert!(!premise(None).head_moved(Some("bbb")));
    }

    /// The parks counted are the runtime's on a diagnosis, whatever
    /// happened after; a person's and those never fenced are not.
    #[test]
    fn the_lineages_parks_are_the_runtimes_past_the_fence() {
        let mut person = diagnosed(5, DiagnosisChoice::Replan);
        person.decided_by = Some(DECIDED_BY_PERSON.into());
        let lineage = [
            op(ReplanStatus::Applied, diagnosed(5, DiagnosisChoice::Replan)),
            op(ReplanStatus::Parked, diagnosed(9, DiagnosisChoice::Replan)),
            op(ReplanStatus::Parked, person),
            op(
                ReplanStatus::Continuing,
                diagnosed(12, DiagnosisChoice::Continue),
            ),
            // Never fenced, whatever the job chose.
            op(
                ReplanStatus::Continuing,
                diagnosed(14, DiagnosisChoice::Replan),
            ),
            op(ReplanStatus::Diagnosing, Diagnosis::default()),
        ];
        assert_eq!(auto_parks(&lineage), 2);
    }

    #[test]
    fn a_verdict_is_read_from_the_jobs_output_and_unknown_fields_are_refused() {
        let text = r#"Here it is: {"choice": "replan", "causes": ["oversized_unit"], "confidence": "high", "reason": "two parts", "remaining": 2}"#;
        let parsed = DiagnosisVerdict::parse(text).unwrap();
        assert_eq!(parsed.choice, DiagnosisChoice::Replan);
        assert_eq!(parsed.remaining, Some(2));
        let recorded = parsed.diagnosis(
            vec![EventId::new(3)],
            Some(EventId::new(9)),
            Some(DECIDED_BY_RUNTIME),
        );
        assert_eq!(recorded.window_end, Some(EventId::new(9)));
        assert_eq!(recorded.summary.as_deref(), Some("two parts"));
        assert!(DiagnosisVerdict::parse(r#"{"choice": "split"}"#).is_err());
        assert!(
            DiagnosisVerdict::parse(
                r#"{"choice": "continue", "confidence": "high", "reason": "x", "extra": 1}"#
            )
            .is_err()
        );
    }

    #[test]
    fn a_diagnosis_job_runs_until_an_end_tied_to_its_start() {
        let record = |kind: EventKind, payload: Value| JobRecord::of(kind, &payload);
        let started = |op: i64, generation: i64, owner: u32| {
            record(
                EventKind::ReplanDiagnosisStarted,
                json!({"operation_id": op, "attempt": generation, "generation": generation,
                       "supervisor_pid": owner}),
            )
            .unwrap()
        };
        let failed = |payload: Value| record(EventKind::ReplanDiagnosisFailed, payload).unwrap();
        let pid = |pid: u32| JobOwner {
            pid,
            token: None,
            process_start: None,
        };
        let job = |op: i64, generation: i64, owner: u32| RunningJob {
            operation: ReplanId::new(op),
            generation,
            owner: pid(owner),
        };
        // An ask's own record, of a verdict-less ask whatever binary wrote
        // it, is no job's end; other events are no job's records.
        for ask in [
            json!({"operation_id": 1, "attempt": 2, "window_end": 9, "outcome": "ask",
                   "ask_id": 4, "generation": 3}),
            json!({"operation_id": 1, "attempt": 2, "window_end": 9, "outcome": "discarded"}),
        ] {
            assert_eq!(record(EventKind::ReplanDiagnosed, ask), None);
        }
        assert_eq!(
            record(EventKind::ReplanRequested, json!({"operation_id": 1})),
            None
        );
        // A start runs, whatever claims its operation meanwhile, until its
        // own end: another attempt's, or another operation's, ends it not.
        let records = vec![
            started(1, 2, 10),
            failed(json!({"operation_id": 1, "started_generation": 1})),
            failed(json!({"operation_id": 1, "attempt": 1})),
            failed(json!({"operation_id": 2, "started_generation": 2})),
        ];
        assert_eq!(running_jobs(&records, &[]), [job(1, 2, 10)]);
        assert_eq!(running_jobs(&records, &[pid(11)]), [job(1, 2, 10)]);
        // Its owner gone, or the caller itself.
        assert!(running_jobs(&records, &[pid(10)]).is_empty());
        // Its end: one that names its generation, one of an older binary
        // of its attempt (a failure, or the verdict of the job, applied or
        // asked about), or one that names neither.
        for end in [
            failed(json!({"operation_id": 1, "started_generation": 2})),
            failed(json!({"operation_id": 1, "attempt": 2, "error": "the job could not start"})),
            record(
                EventKind::ReplanDiagnosed,
                json!({"operation_id": 1, "attempt": 2, "verdict": {"choice": "continue"},
                       "duration_secs": 3, "outcome": "ask", "ask_id": 4, "generation": 3}),
            )
            .unwrap(),
            record(
                EventKind::ReplanDiagnosed,
                json!({"operation_id": 1, "attempt": 2, "duration_secs": 3,
                       "outcome": "discarded"}),
            )
            .unwrap(),
            failed(json!({"operation_id": 1})),
        ] {
            let mut ended = records.clone();
            ended.push(end.clone());
            assert!(running_jobs(&ended, &[]).is_empty(), "{end:?}");
        }
        // An owner's later start ends its earlier one; owners apart.
        let records = vec![started(1, 2, 10), started(2, 5, 10), started(3, 1, 11)];
        assert_eq!(running_jobs(&records, &[]), [job(2, 5, 10), job(3, 1, 11)]);
        // No owner named: none to wait for.
        let ownerless = record(
            EventKind::ReplanDiagnosisStarted,
            json!({"operation_id": 1, "generation": 2}),
        )
        .unwrap();
        assert!(running_jobs(&[ownerless], &[]).is_empty());
    }

    fn owner(pid: u32, token: Option<&str>, start: Option<&str>) -> JobOwner {
        JobOwner {
            pid,
            token: token.map(str::to_owned),
            process_start: start.map(str::to_owned),
        }
    }

    /// This supervisor is `me` (pid 7); the owner's pid is alive and
    /// started at `start`, or not alive.
    fn seen(pid_start: Option<&str>) -> OwnerSeen<'_> {
        OwnerSeen {
            me: ("me", 7),
            pid_alive: pid_start.is_some(),
            pid_start,
        }
    }

    #[test]
    fn an_owner_runs_by_its_registration_or_its_pid_and_its_start() {
        const NOW: i64 = 10_000;
        let fresh = Some(NOW - 5);
        let stale = Some(NOW - crate::domain::HEARTBEAT_TIMEOUT_SECS - 1);
        let started = owner(10, Some("other"), Some("Mon 10:00"));
        // Its registration there, its heartbeat fresh or old (a host just
        // woken): it runs, whatever its pid runs here (another pid
        // namespace's supervisor's pid is not ours).
        for heartbeat in [fresh, stale] {
            assert!(owner_runs(&started, heartbeat, &seen(None)));
            assert!(owner_runs(&started, heartbeat, &seen(Some("Tue 09:00"))));
        }
        // Its registration gone: its pid running the process that started
        // when it did.
        assert!(owner_runs(&started, None, &seen(Some("Mon 10:00"))));
        // Its pid alive in a process that started at another time, or of
        // an unknown start, or gone: the owner is gone.
        assert!(!owner_runs(&started, None, &seen(Some("Tue 09:00"))));
        assert!(!owner_runs(&started, None, &seen(None)));
        let unknown = OwnerSeen {
            pid_start: None,
            ..seen(Some("x"))
        };
        assert!(!owner_runs(&started, None, &unknown));
        let no_start = owner(10, Some("other"), None);
        assert!(!owner_runs(&no_start, None, &seen(Some("Mon 10:00"))));
        assert!(owner_runs(&no_start, stale, &seen(None)));
        // This supervisor's own token (a job of it before an exec): gone,
        // even registered with its pid and start this process's.
        let mine = owner(7, Some("me"), Some("Mon 10:00"));
        assert!(!owner_runs(&mine, fresh, &seen(Some("Mon 10:00"))));
        // Another registration with this process's pid (another pid
        // namespace's): not this one's job. It runs while registered,
        // whatever its heartbeat, and once its registration is gone while
        // the pid runs the process of its start.
        let twin = owner(7, Some("other"), Some("Mon 10:00"));
        for heartbeat in [fresh, stale] {
            assert!(owner_runs(&twin, heartbeat, &seen(Some("Tue 09:00"))));
        }
        assert!(owner_runs(&twin, None, &seen(Some("Mon 10:00"))));
        assert!(!owner_runs(&twin, None, &seen(Some("Tue 09:00"))));
    }

    #[test]
    fn an_older_binarys_start_is_judged_by_its_pid_alone() {
        let legacy = owner(10, None, None);
        // Any process with its pid runs it; none, or this process, not.
        assert!(owner_runs(&legacy, None, &seen(Some("Tue 09:00"))));
        assert!(!owner_runs(&legacy, None, &seen(None)));
        assert!(!owner_runs(&owner(7, None, None), None, &seen(Some("Mon"))));
        // A start it names is compared, a heartbeat is not read.
        let timed = owner(10, None, Some("Mon 10:00"));
        assert!(owner_runs(&timed, None, &seen(Some("Mon 10:00"))));
        assert!(!owner_runs(&timed, Some(1), &seen(Some("Tue 09:00"))));
    }

    #[test]
    fn the_jobs_of_gone_owners_are_started_again_and_the_others_not() {
        let start = |op: i64, owner: &JobOwner| {
            JobRecord::of(
                EventKind::ReplanDiagnosisStarted,
                &json!({"operation_id": op, "attempt": 1, "generation": 1,
                        "supervisor_pid": owner.pid, "supervisor_token": owner.token,
                        "supervisor_process_start": owner.process_start}),
            )
            .unwrap()
        };
        let live = owner(10, Some("live"), Some("Mon 10:00"));
        // Its pid taken by a process that started later.
        let reused = owner(11, Some("gone"), Some("Mon 10:00"));
        // The same pid as `live`'s, of another registration (another pid
        // namespace): a job of its own beside `live`'s.
        let twin = owner(10, Some("twin"), Some("Mon 10:00"));
        let mine = owner(7, Some("me"), None);
        let records = [
            start(1, &live),
            start(2, &reused),
            start(3, &twin),
            start(4, &mine),
        ];
        let running: Vec<RunningOwner> = running_jobs(&records, &[])
            .into_iter()
            .map(|job| RunningOwner {
                heartbeat_at: matches!(job.owner.token.as_deref(), Some("live" | "twin"))
                    .then_some(1),
                owner: job.owner,
            })
            .collect();
        assert_eq!(running.len(), 4, "{running:?}");
        let (runs, gone) = split_owners(&running, |o| {
            seen(if o.pid == 11 {
                Some("Tue 09:00")
            } else {
                Some("Mon 10:00")
            })
        });
        assert_eq!(runs, [live.clone(), twin.clone()]);
        assert_eq!(gone, [reused, mine]);
        let jobs: Vec<i64> = running_jobs(&records, &gone)
            .into_iter()
            .map(|job| job.operation.as_i64())
            .collect();
        assert_eq!(jobs, [1, 3]);
    }
}
