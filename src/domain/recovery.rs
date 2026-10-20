//! The recovery job (ADR-0047 decisions 39 and 40): the alerts it is
//! started for, the verdict it prints, and which processes belong to a run
//! so that `stop_processes` can touch only them.
use crate::domain::EventKind;
use crate::domain::run::{RecoveryRecord, StallResolved, restore_payload};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{
    AskId, AskKind, AskReason, DomainError, RunEvent, RunStatus, TRIAGE_OPTIONS, TaskId,
    parse_json_object,
};

// The alert a recovery job is started for (`recovery_requested`'s `alert`,
// ADR-0047 decision 39).
string_enum!(RecoveryAlert {
    Failed => "failed",
    Interrupted => "interrupted",
    ResumeExhausted => "resume_exhausted",
    StuckExit => "stuck_exit",
    PromptWaiting => "prompt_waiting",
    Stalled => "stalled",
    LongBackground => "long_background",
    IdleProcess => "idle_process",
});

string_enum!(RecoveryDecision {
    Repair => "repair",
    Escalate => "escalate",
});

string_enum!(RecoveryConfidence {
    High => "high",
    Low => "low",
});

/// How many recovery jobs one run gets per kind of alert; past it the
/// alert goes to the inbox as `recovery_failed` (ADR-0047 decision 39).
pub const MAX_RECOVERY_ATTEMPTS: usize = 3;

/// The longest `wait` a recovery job may ask for (ADR-0047 decision 40).
pub const MAX_RECHECK_SECS: u64 = 3600;

/// The actions a recovery job may choose for a run that ended `failed` or
/// `interrupted` (its session is gone): the ones that move the run or its
/// task on, and `wait`.
pub const ENDED_ACTIONS: [&str; 4] = ["retry", "retry_inherit", "resume", "wait"];

// The actions of the retired interactive worker's live alerts
// (`stuck_exit`, `prompt_waiting` and its `stalled`) went with it (task
// 1437); their past verdicts still parse below.

/// Why a session raised the `stalled` alert (`recovery_requested`'s
/// `reason`): idle without a receipt after its nudge (decision 30). Only
/// the retired interactive worker raised it; past records still carry it.
pub const IDLE_WITHOUT_RECEIPT: &str = "idle_without_receipt";

/// The other reason of the retired interactive worker's `stalled` alert:
/// a text the supervisor typed was not taken (`submit_unconfirmed` of a
/// text, `submit_not_started`, decision 31); past records still carry it.
pub const SEND_UNCONFIRMED: &str = "send_unconfirmed";

/// The reason of a headless session's `stalled` alert (ADR-t813-1
/// decision 9): its turn ended with neither a receipt nor an open
/// question, and the runtime's nudges were used up.
pub const TURN_WITHOUT_RECEIPT: &str = "turn_without_receipt";

/// The other reason of a headless session's `stalled` alert: its turn
/// ended with neither a receipt nor an open question, refused a
/// permission too many times to get on.
pub const PERMISSION_DENIED: &str = "permission_denied";

/// The actions for a headless session's `stalled` alert (ADR-t813-1
/// decision 9): an instruction (sent as the prompt of the session's next
/// turn), stopping processes of the run (a turn among them), `resume` (the
/// run parked for a session of its own) and `wait`. No dialog is ever
/// answered: a headless session has none.
pub const HEADLESS_STALLED_ACTIONS: [&str; 4] =
    ["send_instruction", "stop_processes", "resume", "wait"];

impl RecoveryAlert {
    /// The alert of a run that ended `failed` or `interrupted` without a
    /// pending request of its own (`resume_exhausted` records one).
    pub fn of_ended(status: RunStatus) -> Self {
        match status {
            RunStatus::Interrupted => Self::Interrupted,
            _ => Self::Failed,
        }
    }

    /// The kind of the ask an escalation of this alert opens (ADR-0047
    /// decision 40): `decide` for a run that ended, the kind the runtime
    /// raised before the recovery job for a live session.
    pub fn ask_kind(self) -> AskKind {
        match self {
            Self::Failed | Self::Interrupted | Self::ResumeExhausted => AskKind::Decide,
            Self::StuckExit => AskKind::StuckExit,
            Self::PromptWaiting => AskKind::AnswerPrompt,
            Self::Stalled | Self::LongBackground | Self::IdleProcess => AskKind::Stalled,
        }
    }
}

/// How many recovery jobs the run's `events` requested for `alert`.
pub fn attempts(events: &[RunEvent], alert: RecoveryAlert) -> usize {
    events
        .iter()
        .filter(|e| {
            e.kind == EventKind::RecoveryRequested
                && restore_payload::<RecoveryRecord>(&e.payload).alert == Some(alert.as_str())
        })
        .count()
}

/// Whether `event` is a `recovery_requested` for the alert of a run that
/// ended (`failed`, `interrupted`, `resume_exhausted`): a live session's
/// (`long_background` after the receipt of a run that then failed, task
/// 918) is not that run's triage.
fn ended_request(event: &RunEvent) -> bool {
    event.kind == EventKind::RecoveryRequested
        && restore_payload::<RecoveryRecord>(&event.payload)
            .alert
            .and_then(|alert| alert.parse::<RecoveryAlert>().ok())
            .is_some_and(|alert| alert.ask_kind() == AskKind::Decide)
}

/// The option the runtime adds to the `decide` ask of a run whose
/// `integrate` verification failed (ADR-t883-1): a person or the inbox
/// edits the task's verification commands first, and the answer sends the
/// run back to the recovery job, which can then choose `retry_inherit`.
pub const VERIFY_FIX_OPTION: &str = "edit the task's --verify, then retry_inherit";

/// Whether the run's `integrate` verification failed at least once
/// (`integration_deferred` with `code: verification_failed`): the failure
/// a broken verification command leaves, for which the `decide` ask offers
/// [`VERIFY_FIX_OPTION`].
pub fn verification_failed(run_events: &[RunEvent]) -> bool {
    run_events.iter().any(|e| {
        e.kind == EventKind::IntegrationDeferred && e.payload["code"] == "verification_failed"
    })
}

/// Whether the person's latest answer since the last round is
/// [`VERIFY_FIX_OPTION`] and the task's verification commands were edited
/// (`task_edited` with `verification_commands`) after the run began: that
/// answer gets a recovery round even when the alert's
/// [`MAX_RECOVERY_ATTEMPTS`] are used up, so the job reads the corrected
/// commands (ADR-t883-1). Without such an edit the answer counts as any
/// other, and once the attempts are used up the ask opens again.
pub fn verify_fix_round(run_events: &[RunEvent], task_events: &[RunEvent]) -> bool {
    let chosen = run_events
        .iter()
        .rev()
        .take_while(|e| e.kind != EventKind::TriageStarted)
        .find(|e| e.kind == EventKind::TriageDecided && e.payload["action"] == super::RECOVER_AGAIN)
        .is_some_and(|e| e.payload["answer"] == VERIFY_FIX_OPTION);
    let Some(began) = run_events.first().map(|e| e.id) else {
        return false;
    };
    chosen
        && task_events.iter().any(|e| {
            e.kind == EventKind::TaskEdited
                && e.id > began
                && e.payload["to"].get("verification_commands").is_some()
        })
}

/// The `recovery_requested` of a run that ended which no round took yet: the
/// latest one after the latest `triage_started` and `resume_started` (the
/// runtime records it when a run's resumes are used up). Requests of a
/// live session's alerts are not.
pub fn pending_request(events: &[RunEvent]) -> Option<&RunEvent> {
    events
        .iter()
        .rev()
        .take_while(|e| {
            !matches!(
                e.kind.known(),
                Some(EventKind::TriageStarted | EventKind::ResumeStarted)
            )
        })
        .find(|e| ended_request(e))
}

/// The alert of the run's recovery since its last `resume_started`: that
/// of its latest `recovery_requested`, so a round after a `wait` or a
/// handoff goes on with the alert the first one was requested for.
pub fn current_alert(events: &[RunEvent]) -> Option<RecoveryAlert> {
    events
        .iter()
        .rev()
        .take_while(|e| e.kind != EventKind::ResumeStarted)
        .find(|e| ended_request(e))
        .and_then(|e| restore_payload::<RecoveryRecord>(&e.payload).alert)
        .and_then(|alert| alert.parse().ok())
}

/// The events after which a live session's failed recovery job
/// (`recovery_failed`) no longer waits for a person. For every alert:
/// `session_exited`, `workspace_closed` (the session ended or was closed),
/// `supervision_finished`, `run_recovered` and `runtime_error` (the run was
/// given up or moved on). For its own alert, what raised it went away:
/// `prompt_cleared` for `prompt_waiting`, `receipt_observed` for
/// `long_background`, and for `stalled` either `receipt_observed` (the
/// session wrote its receipt) or `stall_resolved` of `detection: recovery`
/// (the supervisor recorded how the stalled detection ended, once the
/// session moved on).
fn clears(event: &RunEvent, alert: &str) -> bool {
    matches!(
        event.kind.known(),
        Some(
            EventKind::SessionExited
                | EventKind::WorkspaceClosed
                | EventKind::SupervisionFinished
                | EventKind::RunRecovered
                | EventKind::RuntimeError
        )
    ) || (alert == RecoveryAlert::PromptWaiting.as_str() && event.kind == EventKind::PromptCleared)
        || (alert == RecoveryAlert::LongBackground.as_str()
            && event.kind == EventKind::ReceiptObserved)
        || (alert == RecoveryAlert::Stalled.as_str()
            && (event.kind == EventKind::ReceiptObserved
                || (event.kind == EventKind::StallResolved
                    && restore_payload::<StallResolved>(&event.payload).detection
                        == Some("recovery"))))
}

/// The latest `recovery_failed` of a live session (of `alert`, or any)
/// that still waits for a person to recover the session by hand: nothing
/// after it cleared it (see [`clears`]). While it waits, the runtime starts
/// no other job for that alert. Only a runtime from before ADR-t609-1
/// records it for a live session (a failed job opens the alert's ask
/// now); it is still read for a run adopted from one.
pub fn failed_live(events: &[RunEvent], alert: Option<RecoveryAlert>) -> Option<&RunEvent> {
    let (index, failed) = events.iter().enumerate().rev().find(|(_, e)| {
        e.kind == EventKind::RecoveryFailed
            && alert.is_none_or(|alert| e.payload["alert"] == alert.as_str())
    })?;
    let failed_alert = failed.payload["alert"].as_str().unwrap_or_default();
    (!events[index + 1..].iter().any(|e| clears(e, failed_alert))).then_some(failed)
}

/// One operation of a `repair` verdict (ADR-0047 decision 40). The runtime
/// checks each one's preconditions again when it applies the verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecoveryAction {
    Retry,
    RetryInherit,
    Resume {
        #[serde(default)]
        instruction: String,
    },
    SendInstruction {
        instruction: String,
    },
    StopProcesses {
        pids: Vec<u32>,
    },
    AnswerKnownDialog {
        #[serde(default)]
        dialog: String,
    },
    CloseAndProceed,
    Wait {
        recheck_after_secs: u64,
    },
}

impl RecoveryAction {
    /// The name of every action, as [`Self::name`] gives it.
    pub const NAMES: [&'static str; 8] = [
        "retry",
        "retry_inherit",
        "resume",
        "send_instruction",
        "stop_processes",
        "answer_known_dialog",
        "close_and_proceed",
        "wait",
    ];

    pub const fn name(&self) -> &'static str {
        match self {
            Self::Retry => "retry",
            Self::RetryInherit => "retry_inherit",
            Self::Resume { .. } => "resume",
            Self::SendInstruction { .. } => "send_instruction",
            Self::StopProcesses { .. } => "stop_processes",
            Self::AnswerKnownDialog { .. } => "answer_known_dialog",
            Self::CloseAndProceed => "close_and_proceed",
            Self::Wait { .. } => "wait",
        }
    }
}

/// An answer to a recovery job's `decide` ask that is only the name of a
/// runtime operation and none of the ask's options (`answer` refuses it
/// and leaves the ask open): written as is, it would close the ask as a
/// free answer that nobody applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BareActionAnswer<'a> {
    /// The operation the answer names.
    pub action: &'static str,
    /// The ask's options that name the operation, as the ask offers them.
    pub options: Vec<&'a str>,
}

/// Whether `answer` to an ask is only the name of a runtime operation (a
/// recovery job's action or a [`TRIAGE_OPTIONS`] one; blanks, case and
/// quotes aside) that is none of its `options`, for a `decide` ask the
/// supervisor opened (`asked_by_supervisor`) on a run that is `failed` or
/// `interrupted`. Any other answer is taken as it was: an option is
/// applied, a person's own words are read by a person.
pub fn bare_action_answer<'a>(
    kind: &AskKind,
    asked_by_supervisor: bool,
    run: Option<RunStatus>,
    options: &'a [String],
    answer: &str,
) -> Option<BareActionAnswer<'a>> {
    if *kind != AskKind::Decide
        || !asked_by_supervisor
        || !matches!(run, Some(RunStatus::Failed | RunStatus::Interrupted))
        || options.iter().any(|option| option == answer.trim())
    {
        return None;
    }
    let quotes =
        |c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '“' | '”' | '‘' | '’');
    let bare = answer.trim_matches(quotes).to_lowercase();
    let action = RecoveryAction::NAMES
        .iter()
        .chain(TRIAGE_OPTIONS)
        .find(|name| **name == bare)?;
    let options = options
        .iter()
        .filter(|option| {
            option
                .to_lowercase()
                .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                .any(|word| word == *action)
        })
        .map(String::as_str)
        .collect();
    Some(BareActionAnswer { action, options })
}

impl BareActionAnswer<'_> {
    /// Why `answer` refuses it for `ask` of a run of `task`, and what to
    /// answer or run instead: the option that names the operation, and the
    /// retry by hand that carries the branch over for `retry_inherit`.
    pub fn refusal(&self, ask: AskId, task: TaskId) -> String {
        let mut message = format!(
            "`{}` is the name of a runtime operation and none of the options of ask {ask} (a recovery job's decide ask); nothing was written and the ask stays open.",
            self.action
        );
        if self.options.is_empty() {
            message.push_str(" No option names it: answer with an option's text as it is, or in your own words for a person to read.");
        } else {
            let quoted: Vec<String> = self
                .options
                .iter()
                .map(|option| format!("\"{option}\""))
                .collect();
            message.push_str(&format!(
                " Answer with the option's text as it is, and the runtime or the recovery job takes it: {}.",
                quoted.join(" or ")
            ));
        }
        if self.action == RecoveryAction::RetryInherit.name() {
            message.push_str(&format!(
                " To carry the run's branch over by hand now, user or inbox runs `dagq ready {task} --inherit --reason '...'`."
            ));
        }
        message
    }
}

/// What the recovery job prints on stdout: one JSON object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryVerdict {
    pub verdict: RecoveryDecision,
    pub confidence: RecoveryConfidence,
    pub diagnosis: String,
    #[serde(default)]
    pub actions: Vec<RecoveryAction>,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub options: Vec<JobOption>,
    #[serde(default)]
    pub reason_category: Option<AskReason>,
}

impl RecoveryVerdict {
    /// The verdict in the job's stdout, found the way the review's is: the
    /// whole text, or else the outermost `{...}` in it. A field it does not
    /// know is refused, and so is a `repair` that names no action.
    pub fn parse(stdout: &str) -> Result<Self, String> {
        let verdict: Self = parse_json_object(stdout)
            .map_err(|error| format!("the recovery job printed no verdict JSON: {error}"))?;
        if verdict.verdict == RecoveryDecision::Repair && verdict.actions.is_empty() {
            return Err("the recovery job's repair verdict names no action".to_owned());
        }
        Ok(verdict)
    }

    /// Whether the runtime applies it: a `repair` of high confidence. A
    /// `repair` of low confidence is an `escalate` with its actions as the
    /// recommendation (ADR-0047 decision 40).
    pub fn applies(&self) -> bool {
        self.verdict == RecoveryDecision::Repair && self.confidence == RecoveryConfidence::High
    }
}

/// One option a recovery job adds to the ask of its escalation, with the
/// kind it declares (ADR-t1818-2 decision 1): the runtime answer of the
/// ask it stands for, or one that goes back to the job ([`RECOVER_KIND`],
/// [`HAND_OFF_KIND`]).
/// A bare string declares no kind, as every job's option did before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RawJobOption", into = "RawJobOption")]
pub struct JobOption {
    pub text: String,
    pub kind: Option<String>,
}

/// An object with no `kind` (or a null one) declares none, and one whose
/// `kind` is not a string declares that value as an unknown kind: either is
/// left out of the ask, and the rest of the verdict still stands.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RawJobOption {
    Bare(String),
    Declared {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<serde_json::Value>,
        text: String,
    },
}

impl From<RawJobOption> for JobOption {
    fn from(raw: RawJobOption) -> Self {
        match raw {
            RawJobOption::Bare(text) => Self { text, kind: None },
            RawJobOption::Declared { kind, text } => Self {
                text,
                kind: match kind {
                    None | Some(serde_json::Value::Null) => None,
                    Some(serde_json::Value::String(kind)) => Some(kind),
                    Some(other) => Some(other.to_string()),
                },
            },
        }
    }
}

impl From<JobOption> for RawJobOption {
    fn from(option: JobOption) -> Self {
        match option.kind {
            None => Self::Bare(option.text),
            Some(kind) => Self::Declared {
                kind: Some(serde_json::Value::String(kind)),
                text: option.text,
            },
        }
    }
}

/// The kind a job declares for an option that goes back to it: choosing it
/// moves nothing, and the alert's next recovery job runs with the answer
/// (ADR-t1818-2 decision 2), on a run that ended and on a live one alike.
pub const RECOVER_KIND: &str = super::RECOVER_AGAIN;

/// The kind a job declares for an option that asks the next round to hand
/// the run to the check that the base already meets the acceptance
/// (ADR-t1818-2 decisions 1 and 3; ADR-t1818-1). Only a run that ended
/// offers it, and it is kept whatever its words say: it reads as the
/// task's end on purpose, and its label says what choosing it does.
pub const HAND_OFF_KIND: &str = "hand_off";

/// What the ask shows after each option of [`RECOVER_KIND`], so that each
/// one says where it goes.
pub const RECOVER_LABEL: &str = " [goes back to the recovery job, which runs again with this answer; the runtime moves nothing]";

/// What the ask shows after each option of [`HAND_OFF_KIND`].
pub const HAND_OFF_LABEL: &str = " [goes back to the recovery job, which runs again and may hand the run to the check that the base already meets the acceptance; the task closes only if that check and its review pass; the runtime moves nothing now]";

/// The text a person chose, without the label the ask showed after it.
pub fn without_label(answer: &str) -> &str {
    [RECOVER_LABEL, HAND_OFF_LABEL]
        .iter()
        .find_map(|label| answer.strip_suffix(label))
        .unwrap_or(answer)
}

/// Whether `option` is one a job added that goes back to it: it carries
/// one of the labels.
pub fn passes_back(option: &str) -> bool {
    without_label(option) != option
}

/// The option of `options` that `answer` chose: the one it is, or else the
/// one whose text it is without its label (a person may answer a job's
/// option without the label).
pub fn chosen_option<'a>(options: &'a [String], answer: &str) -> Option<&'a String> {
    let answer = answer.trim();
    options.iter().find(|option| *option == answer).or_else(|| {
        options
            .iter()
            .find(|option| passes_back(option) && without_label(option) == answer)
    })
}

/// The words for which a job's option of [`RECOVER_KIND`] is left out of
/// the ask (ADR-t1818-2 decision 1's fence): it reads as a runtime
/// operation, or as ending the task, which choosing it would not do. The
/// ask's own runtime answers are fenced too.
pub const CONFUSABLE_WORDS: [&str; 19] = [
    "retry",
    "retried",
    "retrying",
    "retry_inherit",
    "resume",
    "resumed",
    "cancel",
    "canceled",
    "cancelled",
    "canceling",
    "cancelling",
    "cancellation",
    "done",
    "complete",
    "completed",
    "completion",
    "close",
    "closed",
    "obsolete",
];

/// A job's option the runtime offers as its own answer instead
/// (ADR-t1818-2 decision 1): the job's text becomes the recommendation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MergedOption {
    pub text: String,
    pub into: String,
}

/// A job's option left out of the ask, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DroppedOption {
    pub text: String,
    pub kind: Option<String>,
    pub why: String,
}

/// What became of a job's options on its escalation's ask.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SortedOptions {
    /// The options that go back to the job, each with its label, in the
    /// job's order.
    pub kept: Vec<String>,
    pub merged: Vec<MergedOption>,
    pub dropped: Vec<DroppedOption>,
}

/// Sort a job's `options` for an ask whose runtime answers are `kinds`, of
/// which it offers `offered` now (with any other of the runtime's own
/// options); `hand_off` says whether the ask may offer [`HAND_OFF_KIND`]
/// (ADR-t1818-2 decision 1). In order: an option whose text is one the
/// ask offers is that one; one of a runtime answer's kind is that answer
/// when offered and left out when not; one of [`HAND_OFF_KIND`] is kept
/// with [`HAND_OFF_LABEL`] whatever its words; one of [`RECOVER_KIND`] is
/// kept with [`RECOVER_LABEL`] unless a word of [`CONFUSABLE_WORDS`] or of
/// `kinds` is in it; any other (no kind, an unknown one) is left out.
pub fn sort_job_options(
    offered: &[String],
    kinds: &[&str],
    hand_off: bool,
    options: &[JobOption],
) -> SortedOptions {
    let mut sorted = SortedOptions::default();
    let keep = |sorted: &mut SortedOptions, labeled: String| {
        if !sorted.kept.contains(&labeled) {
            sorted.kept.push(labeled);
        }
    };
    for option in options {
        let text = option.text.trim();
        if text.is_empty() {
            continue;
        }
        let kind = option.kind.as_deref().map(str::trim);
        let drop = |why: String| DroppedOption {
            text: text.to_owned(),
            kind: kind.map(str::to_owned),
            why,
        };
        if let Some(into) = offered.iter().find(|offered| *offered == text) {
            sorted.merged.push(MergedOption {
                text: text.to_owned(),
                into: into.clone(),
            });
            continue;
        }
        match kind {
            None => sorted.dropped.push(drop("it declares no kind".to_owned())),
            Some(kind) if kinds.contains(&kind) => {
                if offered.iter().any(|offered| offered == kind) {
                    sorted.merged.push(MergedOption {
                        text: text.to_owned(),
                        into: kind.to_owned(),
                    });
                } else {
                    sorted.dropped.push(drop(format!(
                        "`{kind}` is not one of this ask's answers now"
                    )));
                }
            }
            Some(HAND_OFF_KIND) if hand_off => keep(&mut sorted, format!("{text}{HAND_OFF_LABEL}")),
            Some(RECOVER_KIND) => match confusable(text, kinds) {
                Some(word) => sorted.dropped.push(drop(format!(
                    "it reads as `{word}`, a runtime operation or the task's end, which choosing it would not do"
                ))),
                None => keep(&mut sorted, format!("{text}{RECOVER_LABEL}")),
            },
            Some(kind) => {
                let mut known = kinds.to_vec();
                known.push(RECOVER_KIND);
                if hand_off {
                    known.push(HAND_OFF_KIND);
                }
                sorted.dropped.push(drop(format!(
                    "its kind `{kind}` is none of this ask's ({})",
                    known.join(", ")
                )));
            }
        }
    }
    sorted
}

/// The first word of `text` (lowercased, split at anything but letters,
/// digits and `_`) that is in [`CONFUSABLE_WORDS`] or `kinds`.
fn confusable(text: &str, kinds: &[&str]) -> Option<String> {
    text.to_lowercase()
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .find(|word| CONFUSABLE_WORDS.contains(word) || kinds.contains(word))
        .map(str::to_owned)
}

impl SortedOptions {
    /// The lines the ask's question carries for them: the job's text of
    /// each option the runtime offers instead, each one left out with why,
    /// and where the kept ones go. Empty when there is nothing to say.
    pub fn question(&self) -> String {
        let mut lines = Vec::new();
        for merged in &self.merged {
            if merged.text != merged.into {
                lines.push(format!(
                    "The recovery job recommends `{}`: {}",
                    merged.into, merged.text
                ));
            }
        }
        for dropped in &self.dropped {
            lines.push(format!(
                "Left out of the options (the recovery job's, not one the runtime can apply): \"{}\": {}.",
                dropped.text, dropped.why
            ));
        }
        if self
            .kept
            .iter()
            .any(|option| option.ends_with(RECOVER_LABEL))
        {
            lines.push("Each option marked [goes back to the recovery job, which runs again with this answer ...] is the recovery job's own: the runtime moves nothing for it, and the recovery job runs again with your choice.".to_owned());
        }
        if self
            .kept
            .iter()
            .any(|option| option.ends_with(HAND_OFF_LABEL))
        {
            lines.push("The option marked [... may hand the run to the check that the base already meets the acceptance ...] is the recovery job's own too: the runtime moves nothing for it now; the recovery job runs again and may hand the run to that check, which closes the task only if the verification and its review pass.".to_owned());
        }
        lines.join("\n")
    }

    /// Record the merged and the left-out options in `payload`, when any.
    pub fn record(&self, payload: &mut serde_json::Value) {
        if !self.merged.is_empty() {
            payload["options_merged"] = serde_json::json!(self.merged);
        }
        if !self.dropped.is_empty() {
            payload["options_dropped"] = serde_json::json!(self.dropped);
        }
    }
}

/// The ask whose answer a live session's next recovery job of the alert
/// judged by `threshold` gets (ADR-t1818-2 decision 2): the latest
/// `stall_resolved` of `outcome: answered_recover` and that threshold since
/// the alert's latest `recovery_requested`. `None` when no person chose a
/// job's option since. The runtime writes it into the next job's
/// `recovery_requested` (and so its alert facts) as `person_answer`:
/// `{ask_id, answer}`, the answer without its label, as a run that ended
/// carries a person's choice into its next round.
pub fn live_pass_back(events: &[RunEvent], alert: RecoveryAlert, threshold: &str) -> Option<AskId> {
    events
        .iter()
        .rev()
        .take_while(|e| {
            !(e.kind == EventKind::RecoveryRequested
                && restore_payload::<RecoveryRecord>(&e.payload).alert == Some(alert.as_str()))
        })
        .find(|e| {
            e.kind == EventKind::StallResolved
                && restore_payload::<StallResolved>(&e.payload).outcome == Some(ANSWERED_RECOVER)
                && e.payload["threshold"] == threshold
        })
        .and_then(|e| restore_payload::<StallResolved>(&e.payload).ask_id)
        .map(AskId::new)
}

/// `stall_resolved`'s `outcome` when a person chose a job's own option of a
/// live session's `stalled` ask: the ask is closed and the alert's next
/// recovery job reads the answer ([`live_pass_back`]).
pub const ANSWERED_RECOVER: &str = "answered_recover";

/// A process as the runtime lists it for the recovery job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessInfo {
    pub pid: u32,
    pub ppid: u32,
    pub elapsed_secs: u64,
    pub command: String,
    /// The working directory; `None` when it could not be read.
    pub cwd: Option<String>,
    /// The CPU time it used so far (user and system, `ps`'s `time`), in
    /// milliseconds; `None` when it could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_ms: Option<u64>,
}

/// Whether `path` is `root` or under it.
fn under(path: &str, root: &Path) -> bool {
    Path::new(path).starts_with(root)
}

/// The processes of a run that `stop_processes` may stop (ADR-0047
/// decision 40): those whose working directory is in the run's worktree or
/// that descend from its session wrapper, but neither the wrapper nor the
/// agent, nor anything they run under (the terminal that started the
/// wrapper), nor `except` (the supervisor itself), its ancestors or its
/// descendants, nor pid 1.
pub fn run_processes<'a>(
    all: &'a [ProcessInfo],
    worktree: &Path,
    wrapper: Option<u32>,
    agent: Option<u32>,
    except: u32,
) -> Vec<&'a ProcessInfo> {
    let parent = |pid: u32| all.iter().find(|p| p.pid == pid).map(|p| p.ppid);
    // The chain of parents from `pid` up, `pid` included.
    let chain = |pid: u32| {
        let mut chain = vec![pid];
        let mut current = pid;
        while let Some(next) = parent(current) {
            if next <= 1 || chain.contains(&next) {
                break;
            }
            chain.push(next);
            current = next;
        }
        chain
    };
    let supervisor = chain(except);
    // A wrapper that is the supervisor or runs above it (as in a process
    // that hosts both) makes nothing the run's by descent.
    let wrapper = wrapper.filter(|pid| !supervisor.contains(pid));
    let session: Vec<u32> = [wrapper, agent].into_iter().flatten().collect();
    let protected: Vec<u32> = session
        .iter()
        .flat_map(|pid| chain(*pid))
        .chain(supervisor)
        .collect();
    all.iter()
        // The supervisor's own children (its review job runs in the
        // worktree) are never the run's.
        .filter(|p| p.pid > 1 && !protected.contains(&p.pid) && !chain(p.pid).contains(&except))
        .filter(|p| {
            p.cwd.as_deref().is_some_and(|cwd| under(cwd, worktree))
                || wrapper.is_some_and(|wrapper| chain(p.pid).contains(&wrapper))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One of each action: the match makes a new variant fail to build
    /// until it is listed here and in [`RecoveryAction::NAMES`].
    fn every_action() -> Vec<RecoveryAction> {
        let all = vec![
            RecoveryAction::Retry,
            RecoveryAction::RetryInherit,
            RecoveryAction::Resume {
                instruction: String::new(),
            },
            RecoveryAction::SendInstruction {
                instruction: String::new(),
            },
            RecoveryAction::StopProcesses { pids: Vec::new() },
            RecoveryAction::AnswerKnownDialog {
                dialog: String::new(),
            },
            RecoveryAction::CloseAndProceed,
            RecoveryAction::Wait {
                recheck_after_secs: 0,
            },
        ];
        for action in &all {
            match action {
                RecoveryAction::Retry
                | RecoveryAction::RetryInherit
                | RecoveryAction::Resume { .. }
                | RecoveryAction::SendInstruction { .. }
                | RecoveryAction::StopProcesses { .. }
                | RecoveryAction::AnswerKnownDialog { .. }
                | RecoveryAction::CloseAndProceed
                | RecoveryAction::Wait { .. } => {}
            }
        }
        all
    }

    /// A job's option is a bare string (no kind) or `{kind, text}`, and
    /// is written back the way it was read (a verdict kept for a hold is
    /// read again).
    #[test]
    fn a_jobs_option_reads_and_writes_with_or_without_its_kind() {
        let verdict = RecoveryVerdict::parse(
            r#"{"verdict":"escalate","confidence":"high","diagnosis":"x","options":["bare",{"kind":"recover","text":"declared"}]}"#,
        )
        .unwrap();
        assert_eq!(
            verdict.options,
            [
                JobOption {
                    text: "bare".into(),
                    kind: None
                },
                JobOption {
                    text: "declared".into(),
                    kind: Some("recover".into())
                },
            ]
        );
        let written = serde_json::to_value(&verdict.options).unwrap();
        assert_eq!(
            written,
            serde_json::json!(["bare", {"kind": "recover", "text": "declared"}])
        );
        assert_eq!(
            serde_json::from_value::<Vec<JobOption>>(written).unwrap(),
            verdict.options
        );
        // An object without a kind, or with a null or non-string one, is
        // read (and left out of the ask later), not a broken verdict.
        let loose = RecoveryVerdict::parse(
            r#"{"verdict":"escalate","confidence":"high","diagnosis":"x","options":[{"text":"a"},{"kind":null,"text":"b"},{"kind":3,"text":"c"}]}"#,
        )
        .unwrap();
        let kinds: Vec<Option<&str>> = loose.options.iter().map(|o| o.kind.as_deref()).collect();
        assert_eq!(kinds, [None, None, Some("3")]);
    }

    /// A job's option is chosen by its whole text, or by its text without
    /// the label; a runtime option only by itself.
    #[test]
    fn an_answer_chooses_an_option_with_or_without_its_label() {
        let labeled = format!("ask for a split{RECOVER_LABEL}");
        let hand_off = format!("the base already meets it{HAND_OFF_LABEL}");
        let options = vec!["retry".to_owned(), labeled.clone(), hand_off.clone()];
        assert_eq!(chosen_option(&options, " retry "), Some(&options[0]));
        assert_eq!(chosen_option(&options, &labeled), Some(&labeled));
        assert_eq!(chosen_option(&options, "ask for a split"), Some(&labeled));
        assert_eq!(
            chosen_option(&options, "the base already meets it"),
            Some(&hand_off)
        );
        assert_eq!(chosen_option(&options, "ask for"), None);
        assert_eq!(chosen_option(&options, ""), None);
        assert!(passes_back(&labeled) && passes_back(&hand_off));
        assert!(!passes_back("retry"));
        assert_eq!(without_label(&hand_off), "the base already meets it");
        assert_eq!(without_label("free words"), "free words");
    }

    fn declared(kind: &str, text: &str) -> JobOption {
        JobOption {
            text: text.into(),
            kind: Some(kind.into()),
        }
    }

    /// ADR-t1818-2 decision 1, in its order: an option whose text the ask
    /// offers is that one whatever its kind; a runtime answer's kind is
    /// that answer when offered and left out when not; `recover` is kept
    /// with its label unless a fenced word (or one of the ask's answers) is
    /// in it; anything else is left out. Kept ones are once. The labels say
    /// each option goes back to the recovery job and the runtime moves
    /// nothing.
    #[test]
    fn a_jobs_options_are_merged_kept_or_left_out() {
        let offered: Vec<String> = ["retry", "cancel"].map(String::from).to_vec();
        let options = [
            JobOption {
                text: " cancel ".into(),
                kind: None,
            },
            declared("cancel", "Cancel: the task is obsolete"),
            declared("resume", "resume it"),
            declared("recover", "Mark task 1217 done (or cancel it as obsolete)"),
            declared("recover", "Ask for a split"),
            declared("recover", "Ask for a split"),
            declared("recover", "wait_a_bit"),
            declared("instruction", "go on"),
            declared("recover", "   "),
            JobOption {
                text: "no kind".into(),
                kind: None,
            },
        ];
        let sorted = sort_job_options(&offered, TRIAGE_OPTIONS, true, &options);
        assert_eq!(
            sorted.kept,
            [
                format!("Ask for a split{RECOVER_LABEL}"),
                format!("wait_a_bit{RECOVER_LABEL}"),
            ]
        );
        assert_eq!(
            sorted.merged,
            [
                MergedOption {
                    text: "cancel".into(),
                    into: "cancel".into()
                },
                MergedOption {
                    text: "Cancel: the task is obsolete".into(),
                    into: "cancel".into()
                },
            ]
        );
        let dropped: Vec<(&str, &str)> = sorted
            .dropped
            .iter()
            .map(|d| (d.text.as_str(), d.why.as_str()))
            .collect();
        assert_eq!(
            dropped,
            [
                ("resume it", "`resume` is not one of this ask's answers now"),
                (
                    "Mark task 1217 done (or cancel it as obsolete)",
                    "it reads as `done`, a runtime operation or the task's end, which choosing it would not do"
                ),
                (
                    "go on",
                    "its kind `instruction` is none of this ask's (retry, resume, cancel, recover, hand_off)"
                ),
                ("no kind", "it declares no kind"),
            ]
        );
        // A live ask fences its own answers too: `wait` is a word of it.
        let live = sort_job_options(
            &["wait".to_owned(), "stop".to_owned()],
            &["wait", "stop"],
            false,
            &[
                declared("recover", "wait for the build"),
                declared("recover", "look at the test's log"),
            ],
        );
        assert_eq!(
            live.kept,
            [format!("look at the test's log{RECOVER_LABEL}")]
        );
        assert_eq!(
            live.dropped[0].why,
            "it reads as `wait`, a runtime operation or the task's end, which choosing it would not do"
        );
        assert!(RECOVER_LABEL.contains("goes back to the recovery job"));
        assert!(RECOVER_LABEL.contains("the runtime moves nothing"));
        assert!(
            sorted
                .question()
                .contains("the runtime moves nothing for it, and the recovery job runs again"),
            "{}",
            sorted.question()
        );
    }

    /// ADR-t1818-2 decision 1's exception: an option that asks the next
    /// round to hand the run to the already-satisfied check is kept with
    /// its own label though its words read as the task's end, on an ask
    /// that may offer it (a run that ended); a live ask leaves it out.
    #[test]
    fn a_hand_off_option_is_kept_with_its_own_label_where_it_may_be_offered() {
        let offered: Vec<String> = TRIAGE_OPTIONS.iter().map(|o| (*o).to_owned()).collect();
        let text =
            "The base already meets the acceptance: hand it to the check that closes it as done";
        let sorted = sort_job_options(
            &offered,
            TRIAGE_OPTIONS,
            true,
            &[declared(HAND_OFF_KIND, text), declared(HAND_OFF_KIND, text)],
        );
        assert_eq!(sorted.kept, [format!("{text}{HAND_OFF_LABEL}")]);
        assert!(sorted.dropped.is_empty(), "{sorted:?}");
        for part in [
            "goes back to the recovery job",
            "may hand the run to the check that the base already meets the acceptance",
            "the task closes only if that check and its review pass",
            "the runtime moves nothing now",
        ] {
            assert!(HAND_OFF_LABEL.contains(part), "{part}");
        }
        let question = sorted.question();
        assert!(
            question.contains("closes the task only if the verification and its review pass"),
            "{question}"
        );
        assert!(
            !question.contains("Each option marked [goes back"),
            "{question}"
        );
        let live = sort_job_options(
            &["wait".to_owned(), "stop".to_owned()],
            &["wait", "stop"],
            false,
            &[declared(HAND_OFF_KIND, text)],
        );
        assert!(live.kept.is_empty());
        assert_eq!(
            live.dropped[0].why,
            "its kind `hand_off` is none of this ask's (wait, stop, recover)"
        );
    }

    /// ADR-t1818-2 decision 2 on a live session: the next job of the alert
    /// gets the ask whose job option a person chose since its last
    /// request, of its own threshold only.
    #[test]
    fn a_live_alerts_next_job_reads_the_ask_whose_job_option_was_chosen() {
        let requested = |id| {
            numbered(
                id,
                "recovery_requested",
                serde_json::json!({"alert": "stalled"}),
            )
        };
        let resolved = |id, outcome, threshold, ask| {
            numbered(
                id,
                "stall_resolved",
                serde_json::json!({"outcome": outcome, "threshold": threshold, "ask_id": ask}),
            )
        };
        let turn = "idle_without_receipt_secs";
        let events = vec![
            requested(1),
            resolved(2, ANSWERED_RECOVER, turn, 7),
            requested(3),
        ];
        assert_eq!(live_pass_back(&events, RecoveryAlert::Stalled, turn), None);
        let mut events = events;
        events.push(resolved(4, "answered_wait", turn, 8));
        events.push(resolved(5, ANSWERED_RECOVER, turn, 9));
        assert_eq!(
            live_pass_back(&events, RecoveryAlert::Stalled, turn),
            Some(AskId::new(9))
        );
        assert_eq!(
            live_pass_back(&events, RecoveryAlert::IdleProcess, "idle_process_secs"),
            None
        );
    }

    #[test]
    fn the_names_are_every_actions_name() {
        let names: Vec<&str> = every_action().iter().map(RecoveryAction::name).collect();
        assert_eq!(names, RecoveryAction::NAMES);
    }

    const OPTION: &str = "retry_inherit once process inspection is allowed";

    fn decide_options() -> Vec<String> {
        ["retry", "resume", "cancel", OPTION, "Retry anyway"]
            .map(String::from)
            .to_vec()
    }

    fn bare(answer: &str) -> Option<BareActionAnswer<'static>> {
        let options: &'static [String] = decide_options().leak();
        bare_action_answer(
            &AskKind::Decide,
            true,
            Some(RunStatus::Failed),
            options,
            answer,
        )
    }

    #[test]
    fn a_bare_operation_name_that_is_no_option_is_refused_with_the_options_naming_it() {
        for answer in [
            "retry_inherit",
            "  retry_inherit\n",
            "RETRY_INHERIT",
            "\"retry_inherit\"",
            "`Retry_Inherit`",
            "'retry_inherit'",
        ] {
            assert_eq!(
                bare(answer),
                Some(BareActionAnswer {
                    action: "retry_inherit",
                    options: vec![OPTION],
                }),
                "{answer:?}"
            );
        }
        // An option's own word in another case names every option with it.
        assert_eq!(
            bare("Retry"),
            Some(BareActionAnswer {
                action: "retry",
                options: vec!["retry", "Retry anyway"],
            })
        );
        // No option names it: refused all the same.
        assert_eq!(
            bare("wait"),
            Some(BareActionAnswer {
                action: "wait",
                options: vec![],
            })
        );
        let interrupted = decide_options();
        assert!(
            bare_action_answer(
                &AskKind::Decide,
                true,
                Some(RunStatus::Interrupted),
                &interrupted,
                "stop_processes",
            )
            .is_some()
        );
    }

    #[test]
    fn options_free_answers_and_other_asks_are_taken_as_they_were() {
        let options = decide_options();
        // An option as it is, and a person's own words.
        for answer in [
            "retry",
            " cancel ",
            OPTION,
            "retry_inherit please, the work is done",
            "try again",
        ] {
            assert_eq!(bare(answer), None, "{answer:?}");
        }
        // Another kind, another asker, a run that is not failed or
        // interrupted, or no run.
        for (kind, by_supervisor, run) in [
            (AskKind::WorkerQuestion, true, Some(RunStatus::Failed)),
            (AskKind::Stalled, true, Some(RunStatus::Failed)),
            (AskKind::Decide, false, Some(RunStatus::Failed)),
            (AskKind::Decide, true, Some(RunStatus::Running)),
            (AskKind::Decide, true, Some(RunStatus::NeedsSession)),
            (AskKind::Decide, true, None),
        ] {
            assert_eq!(
                bare_action_answer(&kind, by_supervisor, run, &options, "retry_inherit"),
                None,
                "{kind:?} {by_supervisor} {run:?}"
            );
        }
    }

    #[test]
    fn the_refusal_names_the_option_and_the_retry_by_hand() {
        let refusal = bare("retry_inherit")
            .unwrap()
            .refusal(AskId::new(531), TaskId::new(661));
        assert!(refusal.contains("ask 531"), "{refusal}");
        assert!(refusal.contains("the ask stays open"), "{refusal}");
        assert!(refusal.contains(&format!("\"{OPTION}\"")), "{refusal}");
        assert!(
            refusal.contains("dagq ready 661 --inherit --reason"),
            "{refusal}"
        );
        let wait = bare("wait").unwrap().refusal(AskId::new(1), TaskId::new(2));
        assert!(wait.contains("No option names it"), "{wait}");
        assert!(!wait.contains("--inherit"), "{wait}");
    }

    fn process(pid: u32, ppid: u32, cwd: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            ppid,
            elapsed_secs: 5,
            command: format!("cmd {pid}"),
            cwd: (!cwd.is_empty()).then(|| cwd.to_owned()),
            cpu_ms: None,
        }
    }

    #[test]
    fn a_verdict_is_parsed_with_its_actions() {
        let verdict = RecoveryVerdict::parse(
            r#"Here: {"verdict": "repair", "confidence": "high", "diagnosis": "orphan", "actions": [{"action": "stop_processes", "pids": [7, 8]}, {"action": "wait", "recheck_after_secs": 60}, {"action": "retry"}]}"#,
        )
        .unwrap();
        assert!(verdict.applies());
        assert_eq!(
            verdict.actions,
            [
                RecoveryAction::StopProcesses { pids: vec![7, 8] },
                RecoveryAction::Wait {
                    recheck_after_secs: 60
                },
                RecoveryAction::Retry,
            ]
        );
        assert_eq!(
            verdict
                .actions
                .iter()
                .map(RecoveryAction::name)
                .collect::<Vec<_>>(),
            ["stop_processes", "wait", "retry"]
        );
        let low = RecoveryVerdict::parse(
            r#"{"verdict": "repair", "confidence": "low", "diagnosis": "?", "actions": [{"action": "resume"}], "reason_category": "recovery_failed"}"#,
        )
        .unwrap();
        assert!(!low.applies());
        assert_eq!(low.reason_category, Some(AskReason::RecoveryFailed));
    }

    #[test]
    fn a_verdict_with_unknown_fields_or_no_action_is_refused() {
        for text in [
            r#"{"verdict": "repair", "confidence": "high", "diagnosis": "x", "actions": []}"#,
            r#"{"verdict": "repair", "confidence": "high", "diagnosis": "x", "actions": [{"action": "cancel"}]}"#,
            r#"{"verdict": "escalate", "confidence": "high", "diagnosis": "x", "extra": 1}"#,
            r#"{"verdict": "repair", "confidence": "high", "diagnosis": "x", "actions": [{"action": "stop_processes", "pids": [1], "signal": 9}]}"#,
            "no json",
        ] {
            assert!(RecoveryVerdict::parse(text).is_err(), "{text}");
        }
        let escalate = RecoveryVerdict::parse(
            r#"{"verdict": "escalate", "confidence": "high", "diagnosis": "x"}"#,
        )
        .unwrap();
        assert!(!escalate.applies());
    }

    /// Each broken output a recovery job may print, of a run that ended
    /// and of a live session's alert, is refused with the error the
    /// failed job's `triage_failed` and `recovery_finished` (and its ask)
    /// carry: no JSON, no verdict, a field or an action the runtime does
    /// not know, and a repair without an action (moved from
    /// `runtime_job_verdicts`, task 1415).
    #[test]
    fn a_broken_verdict_is_refused_with_what_is_wrong() {
        for (text, parts) in [
            (
                "no verdict here\n",
                &["the recovery job printed no verdict JSON"][..],
            ),
            (
                r#"{"confidence": "high", "diagnosis": "x", "actions": [{"action": "stop_processes", "pids": [1]}]}"#,
                &[
                    "the recovery job printed no verdict JSON",
                    "missing field `verdict`",
                ],
            ),
            (
                r#"{"verdict": "repair", "confidence": "high", "diagnosis": "x", "actions": [{"action": "retry"}], "force": true}"#,
                &[
                    "the recovery job printed no verdict JSON",
                    "unknown field `force`",
                ],
            ),
            (
                r#"{"verdict": "repair", "confidence": "high", "diagnosis": "x", "actions": [{"action": "close_and_proceed"}], "force": true}"#,
                &["unknown field `force`"],
            ),
            (
                r#"{"verdict": "repair", "confidence": "high", "diagnosis": "x", "actions": [{"action": "integrate"}]}"#,
                &[
                    "the recovery job printed no verdict JSON",
                    "unknown variant `integrate`",
                ],
            ),
            (
                r#"{"verdict": "repair", "confidence": "high", "diagnosis": "x", "actions": [{"action": "kill_session"}]}"#,
                &["unknown variant `kill_session`"],
            ),
            (
                r#"{"verdict": "repair", "confidence": "high", "diagnosis": "x", "actions": []}"#,
                &["the recovery job's repair verdict names no action"],
            ),
        ] {
            let error = RecoveryVerdict::parse(text).unwrap_err();
            for part in parts {
                assert!(error.contains(part), "{text}: {part} in {error}");
            }
        }
    }

    #[test]
    fn only_the_runs_own_processes_may_be_stopped() {
        let worktree = Path::new("/runs/r/worktree");
        let all = [
            process(1, 0, "/"),
            // The terminal that runs the wrapper, in the worktree.
            process(10, 1, "/runs/r/worktree"),
            process(11, 10, "/runs/r"),          // wrapper
            process(12, 11, "/runs/r/worktree"), // agent
            process(13, 12, "/runs/r/worktree/src"),
            process(14, 13, ""), // a descendant whose cwd is unreadable
            process(20, 1, "/runs/r/worktree"), // orphan
            process(21, 1, "/runs/other/worktree"),
            process(22, 1, "/runs/r/worktree-2"),
            process(30, 1, "/runs/r/worktree"), // the supervisor
            process(31, 30, "/repo"),
        ];
        let pids: Vec<u32> = run_processes(&all, worktree, Some(11), Some(12), 31)
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(pids, [13, 14, 20]);
        // Without a wrapper, only the working directory counts.
        let pids: Vec<u32> = run_processes(&all, worktree, None, None, 31)
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(pids, [10, 12, 13, 20]);
        // A wrapper hosted by the supervisor's own process: none of the
        // supervisor's children are the run's, even in the worktree (its
        // review job), while an orphan there is.
        let hosted = [
            process(30, 1, "/repo"),
            process(31, 30, "/tmp"),
            process(32, 30, "/runs/r/worktree"),
            process(33, 1, "/runs/r/worktree"),
        ];
        let pids: Vec<u32> = run_processes(&hosted, worktree, Some(30), None, 30)
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(pids, [33]);
    }

    fn event(kind: &str, payload: serde_json::Value) -> RunEvent {
        RunEvent {
            id: super::super::EventId::new(1),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: String::new(),
            actor: None,
        }
    }

    fn numbered(id: i64, kind: &str, payload: serde_json::Value) -> RunEvent {
        RunEvent {
            id: super::super::EventId::new(id),
            ..event(kind, payload)
        }
    }

    #[test]
    fn a_verify_fix_answer_after_a_verification_edit_gets_a_round() {
        let claimed = numbered(10, "run_claimed", serde_json::json!({}));
        let deferred = numbered(
            11,
            "integration_deferred",
            serde_json::json!({"code": "verification_failed"}),
        );
        assert!(!verification_failed(std::slice::from_ref(&claimed)));
        assert!(verification_failed(&[claimed.clone(), deferred.clone()]));
        let decided = |answer: &str| {
            numbered(
                14,
                "triage_decided",
                serde_json::json!({"action": "recover", "answer": answer}),
            )
        };
        let edited = |id: i64, field: &str| {
            numbered(
                id,
                "task_edited",
                serde_json::json!({"from": {field: []}, "to": {field: ["true"]}}),
            )
        };
        let run = [
            claimed.clone(),
            deferred.clone(),
            decided(VERIFY_FIX_OPTION),
        ];
        assert!(verify_fix_round(
            &run,
            &[edited(13, "verification_commands")]
        ));
        // No edit, an edit of another field, or one from before the run.
        assert!(!verify_fix_round(&run, &[]));
        assert!(!verify_fix_round(&run, &[edited(13, "paths")]));
        assert!(!verify_fix_round(
            &run,
            &[edited(5, "verification_commands")]
        ));
        // Another answer, or a round already taken since.
        let other = [claimed.clone(), decided("split the task")];
        assert!(!verify_fix_round(
            &other,
            &[edited(13, "verification_commands")]
        ));
        let taken = [
            claimed,
            decided(VERIFY_FIX_OPTION),
            numbered(15, "triage_started", serde_json::json!({})),
        ];
        assert!(!verify_fix_round(
            &taken,
            &[edited(13, "verification_commands")]
        ));
    }

    #[test]
    fn requests_are_counted_by_alert_and_one_is_pending_until_a_round_takes_it() {
        let exhausted = event(
            "recovery_requested",
            serde_json::json!({"alert": "resume_exhausted"}),
        );
        let failed = event("recovery_requested", serde_json::json!({"alert": "failed"}));
        let events = vec![
            failed.clone(),
            event("triage_started", serde_json::json!({})),
            event("resume_started", serde_json::json!({})),
            exhausted.clone(),
        ];
        assert_eq!(attempts(&events, RecoveryAlert::Failed), 1);
        assert_eq!(attempts(&events, RecoveryAlert::ResumeExhausted), 1);
        assert_eq!(attempts(&events, RecoveryAlert::StuckExit), 0);
        let alert = |events: &[RunEvent]| {
            pending_request(events).map(|e| e.payload["alert"].as_str().unwrap().to_owned())
        };
        assert_eq!(alert(&events).as_deref(), Some("resume_exhausted"));
        let taken = [exhausted, event("triage_started", serde_json::json!({}))];
        assert_eq!(alert(&taken), None);
        assert_eq!(
            alert(std::slice::from_ref(&failed)).as_deref(),
            Some("failed")
        );
        // A round after a wait keeps the alert; a resume starts afresh.
        assert_eq!(current_alert(&taken), Some(RecoveryAlert::ResumeExhausted));
        let resumed = [
            taken[0].clone(),
            event("resume_started", serde_json::json!({})),
        ];
        assert_eq!(current_alert(&resumed), None);
        // A live session's request (task 918: long_background after the
        // receipt of a run that then failed) is not the ended run's.
        let live = [event(
            "recovery_requested",
            serde_json::json!({"alert": "long_background"}),
        )];
        assert_eq!(alert(&live), None);
        assert_eq!(current_alert(&live), None);
        let after = [failed.clone(), live[0].clone()];
        assert_eq!(alert(&after).as_deref(), Some("failed"));
        assert_eq!(current_alert(&after), Some(RecoveryAlert::Failed));
    }

    #[test]
    fn a_failed_live_job_waits_until_the_session_moves_on() {
        let failed = |alert: &str| {
            event(
                "recovery_failed",
                serde_json::json!({"alert": alert, "attempt": 1}),
            )
        };
        let stuck = vec![failed("stuck_exit")];
        assert!(failed_live(&stuck, None).is_some());
        assert!(failed_live(&stuck, Some(RecoveryAlert::StuckExit)).is_some());
        assert!(failed_live(&stuck, Some(RecoveryAlert::PromptWaiting)).is_none());
        let mut exited = stuck.clone();
        exited.push(event("session_exited", serde_json::json!({})));
        assert!(failed_live(&exited, None).is_none());
        // A cleared dialog clears only the dialog's alert.
        let mut cleared = vec![failed("prompt_waiting")];
        cleared.push(event("prompt_cleared", serde_json::json!({})));
        assert!(failed_live(&cleared, None).is_none());
        let mut other = stuck;
        other.push(event("prompt_cleared", serde_json::json!({})));
        assert!(failed_live(&other, None).is_some());
        let mut received = vec![failed("long_background")];
        received.push(event("receipt_observed", serde_json::json!({})));
        assert!(failed_live(&received, None).is_none());
        // A stalled session that moved on clears its failed job; the end
        // of another detection does not.
        let mut stalled = vec![failed("stalled")];
        stalled.push(event(
            "stall_resolved",
            serde_json::json!({"detection": "nudge"}),
        ));
        assert!(failed_live(&stalled, None).is_some());
        stalled.push(event(
            "stall_resolved",
            serde_json::json!({"detection": "recovery"}),
        ));
        assert!(failed_live(&stalled, None).is_none());
        let mut received = vec![failed("stalled")];
        received.push(event("receipt_observed", serde_json::json!({})));
        assert!(failed_live(&received, None).is_none());
    }

    #[test]
    fn each_alert_escalates_to_its_kind() {
        assert_eq!(
            RecoveryAlert::of_ended(RunStatus::Interrupted),
            RecoveryAlert::Interrupted
        );
        assert_eq!(
            RecoveryAlert::of_ended(RunStatus::Failed),
            RecoveryAlert::Failed
        );
        for (alert, kind) in [
            (RecoveryAlert::Failed, AskKind::Decide),
            (RecoveryAlert::Interrupted, AskKind::Decide),
            (RecoveryAlert::ResumeExhausted, AskKind::Decide),
            (RecoveryAlert::StuckExit, AskKind::StuckExit),
            (RecoveryAlert::PromptWaiting, AskKind::AnswerPrompt),
            (RecoveryAlert::Stalled, AskKind::Stalled),
            (RecoveryAlert::LongBackground, AskKind::Stalled),
        ] {
            assert_eq!(alert.ask_kind(), kind);
        }
    }

    #[test]
    fn alerts_and_categories_read_as_their_names() {
        assert_eq!(RecoveryAlert::LongBackground.as_str(), "long_background");
        assert_eq!(
            "stalled".parse::<RecoveryAlert>().unwrap(),
            RecoveryAlert::Stalled
        );
    }
}
