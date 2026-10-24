//! The payloads of recorded run events that [`super::RunHistory`] and the
//! supervisor's stall and adoption decide by, restored as typed values
//! (task 1551). Each type names the keys it reads once; the readers read
//! the fields, not the keys.
//!
//! These restore what earlier versions wrote, so they never refuse a
//! payload: a missing key, an extra key, `null` or a value of another type
//! reads as the field's absence, the same as `payload["key"].as_str()` and
//! its kin read it. A payload that is not a JSON object restores to the
//! default (every field absent). Writing a payload is
//! [`super::recorded`]'s, except the stall watch's own records
//! ([`NewStallResolved`], [`NewStallNudged`]), whose keys it reads back.

use serde::de::IgnoredAny;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::domain::ReviewVerdict;

/// Restores `payload` as `T`; see the module doc for what it tolerates.
pub fn restore<'a, T>(payload: &'a Value) -> T
where
    T: Deserialize<'a> + Default,
{
    if payload.is_object() {
        T::deserialize(payload).unwrap_or_default()
    } else {
        T::default()
    }
}

/// A recorded value: of the type the reader expects, or anything else
/// (an older or broken writer), which reads as absent.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(untagged)]
enum Lenient<T> {
    Typed(T),
    Other(IgnoredAny),
}

impl<T> Lenient<T> {
    fn typed(self) -> Option<T> {
        match self {
            Self::Typed(value) => Some(value),
            Self::Other(_) => None,
        }
    }
}

/// A key whose value is of type `T`; absent when the key is missing,
/// `null` or of another type.
pub(crate) fn typed<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Lenient<T>>::deserialize(deserializer)?.and_then(Lenient::typed))
}

/// A key that may be `null` to clear a value: `None` (with
/// `#[serde(default)]`) when the key is missing, `Some(None)` when it is
/// `null` or of another type, `Some(Some(value))` when it is of type `T`.
pub(crate) fn nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    typed::<D, T>(deserializer).map(Some)
}

/// A key whose value is a JSON object, read as `T`; absent when it is
/// missing, of another type (an array too) or does not read as `T`.
pub(crate) fn object<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Ok(Option::<Value>::deserialize(deserializer)?.and_then(as_object))
}

/// `value` read as `T` when it is a JSON object.
fn as_object<T: serde::de::DeserializeOwned>(value: Value) -> Option<T> {
    value
        .is_object()
        .then(|| T::deserialize(value).ok())
        .flatten()
}

/// A key whose value is an array, each item read as `T` when it is an
/// object; absent when the key is missing or not an array.
pub(crate) fn objects<'de, D, T>(deserializer: D) -> Result<Option<Vec<Option<T>>>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    Ok(typed::<D, Vec<Value>>(deserializer)?
        .map(|items| items.into_iter().map(as_object).collect()))
}

/// Whether the key is there at all, whatever its value (even `null`).
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    IgnoredAny::deserialize(deserializer).map(|_| true)
}

/// `integration_approved`: the approval to land, recorded by `integrate`
/// when it is called (once per run, even on a run it then parks with
/// `defer_integration`) or by the `land` answer to `approve_landing`.
/// The supervisor landing an approved run pushes `main` as the first one
/// says (`RunHistory::landing_pushes`).
#[derive(Debug, Default, Deserialize)]
pub(super) struct IntegrationApproved {
    /// The `approve_landing` ask whose answer approved the run; absent
    /// when missing or `null`, kept (of whatever type) otherwise.
    #[serde(default)]
    ask_id: Option<Lenient<i64>>,
    /// `false` from `integrate --no-push`; `integrate` writes `true`
    /// otherwise.
    #[serde(default, deserialize_with = "typed")]
    pub(super) push: Option<bool>,
}

impl IntegrationApproved {
    /// Whether the event names an ask (`ask_id` present and not `null`).
    pub(super) fn names_ask(&self) -> bool {
        self.ask_id.is_some()
    }

    /// The ask it names, when `ask_id` is an integer.
    pub(super) fn ask_id(&self) -> Option<i64> {
        self.ask_id.and_then(Lenient::typed)
    }
}

/// `ask_opened`.
#[derive(Debug, Default, Deserialize)]
pub struct AskOpened<'a> {
    #[serde(default, borrow, deserialize_with = "typed")]
    pub kind: Option<&'a str>,
    /// The role that opened it (`supervisor`, ...).
    #[serde(default, borrow, deserialize_with = "typed")]
    pub asked_by: Option<&'a str>,
    #[serde(default, deserialize_with = "typed")]
    pub ask_id: Option<i64>,
}

/// `run_recovered`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct RunRecovered<'a> {
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) previous_status: Option<&'a str>,
}

/// `integration_rebased` and `migration_renumbered`: a landing moved the
/// run's head.
#[derive(Debug, Default, Deserialize)]
pub(super) struct HeadRewritten<'a> {
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) head_before: Option<&'a str>,
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) head_after: Option<&'a str>,
}

/// The `status` an event moved the run to (`landing_decided`,
/// `resume_finished`, any event that parked it for a session).
#[derive(Debug, Default, Deserialize)]
pub(super) struct StatusOf<'a> {
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) status: Option<&'a str>,
}

/// `resume_finished`.
#[derive(Debug, Default, Deserialize)]
pub struct ResumeFinished<'a> {
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) status: Option<&'a str>,
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) outcome: Option<&'a str>,
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) workspace_id: Option<&'a str>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) attempt: Option<u64>,
    /// The resume ended provider-only (ADR-t3255-1,
    /// `domain::resume::finish_provider_only`); absent on any other
    /// resume and on an event older than the field.
    #[serde(default, deserialize_with = "typed")]
    provider_only: Option<bool>,
}

impl ResumeFinished<'_> {
    /// Whether the resume was marked provider-only as it finished, and so
    /// not counted however it started: only those recorded
    /// `provider_only: true`.
    pub fn provider_only(&self) -> bool {
        self.provider_only == Some(true)
    }
}

/// `workspace_closed`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct WorkspaceClosed<'a> {
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) workspace_id: Option<&'a str>,
}

/// An event that parked the run for a session (see `last_park`).
#[derive(Debug, Default, Deserialize)]
pub(super) struct Parking<'a> {
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) reason: Option<&'a str>,
    /// The recovery job's (`triage_finished`, `recovery_parked`) in place
    /// of a `reason`.
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) instruction: Option<&'a str>,
    /// A landing deferred for missing evidence names its `checks`.
    #[serde(default, rename = "checks", deserialize_with = "present")]
    pub(super) names_checks: bool,
    /// A landing deferred for its scope names the paths.
    #[serde(default, rename = "scope_violation", deserialize_with = "present")]
    pub(super) names_scope_violation: bool,
}

/// An event that names an ask by `ask_id` (`ask_delivery_failed`).
#[derive(Debug, Default, Deserialize)]
pub(super) struct AskOf {
    #[serde(default, deserialize_with = "typed")]
    pub(super) ask_id: Option<i64>,
}

/// `follow_up_registered`: a receipt's entry names its `index`, a
/// review's (`from` `review`) its document's `path` and `section`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct FollowUpRegistered<'a> {
    #[serde(default, deserialize_with = "typed")]
    pub(super) index: Option<u64>,
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) from: Option<&'a str>,
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) path: Option<&'a str>,
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) section: Option<&'a str>,
}

/// `push_failed`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct PushFailed<'a> {
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) error: Option<&'a str>,
}

/// The `attempt` an event names (`review_started`, `review_finished`,
/// `recovery_requested`, ...), when it is an unsigned integer.
#[derive(Debug, Default, Deserialize)]
pub struct AttemptOf {
    #[serde(default, deserialize_with = "typed")]
    pub attempt: Option<u64>,
}

/// The event that ends a headless job (`review_finished`,
/// `plan_review_discarded`, `headless_job_stopped`, ...), as a supervisor
/// that stops a gone one's job reads it to tell whether that job's
/// Execution was recorded already.
#[derive(Debug, Default, Deserialize)]
pub struct JobEnd<'a> {
    /// The `headless_jobs.kind` a `headless_job_stopped` names.
    #[serde(default, borrow, deserialize_with = "typed")]
    pub kind: Option<&'a str>,
    /// The `headless_jobs.label` a `headless_job_stopped` names.
    #[serde(default, borrow, deserialize_with = "typed")]
    pub label: Option<&'a str>,
    /// The agent a `review_job_finished` names (none for the whole
    /// review's job).
    #[serde(default, borrow, deserialize_with = "typed")]
    pub agent: Option<&'a str>,
    /// Whether a `review_job_finished` starts its job once more.
    #[serde(default, deserialize_with = "typed")]
    pub runs_again: Option<bool>,
    /// A `review_job_finished`'s outcome (`held` starts the job again).
    #[serde(default, borrow, deserialize_with = "typed")]
    pub outcome: Option<&'a str>,
    #[serde(default, deserialize_with = "typed")]
    pub attempt: Option<u64>,
    #[serde(default, deserialize_with = "typed")]
    pub proposal_id: Option<i64>,
    #[serde(default, deserialize_with = "typed")]
    pub goal_id: Option<i64>,
    /// Whether it records the Execution (ADR-t1486-1): `tokens_source` is
    /// there, even `null` (not measured).
    #[serde(default, rename = "tokens_source", deserialize_with = "present")]
    pub execution: bool,
}

/// `stall_resolved`: how one detection of the stall watch ended.
#[derive(Debug, Default, Deserialize)]
pub struct StallResolved<'a> {
    /// The watch's phase (`session`).
    #[serde(default, borrow, deserialize_with = "typed")]
    pub phase: Option<&'a str>,
    /// `nudge`, `recovery` or `ask`.
    #[serde(default, borrow, deserialize_with = "typed")]
    pub detection: Option<&'a str>,
    #[serde(default, borrow, deserialize_with = "typed")]
    pub outcome: Option<&'a str>,
    /// The `stalled` ask of an `ask` detection.
    #[serde(default, deserialize_with = "typed")]
    pub ask_id: Option<i64>,
    /// The recovery job of a `recovery` detection.
    #[serde(default, deserialize_with = "typed")]
    pub attempt: Option<u64>,
    /// An ask answered `intervene`, which is opened again.
    #[serde(default, deserialize_with = "typed")]
    pub reopened: Option<bool>,
    #[serde(default, deserialize_with = "typed")]
    pub detected_after_secs: Option<i64>,
}

/// The `stall_resolved` the stall watch records.
#[derive(Debug, Clone, Serialize)]
pub struct NewStallResolved<'a> {
    pub phase: &'a str,
    pub detection: &'a str,
    /// The setting the detection was judged by.
    pub threshold: &'a str,
    pub threshold_secs: i64,
    pub detected_after_secs: i64,
    pub outcome: &'a str,
    pub resolved_after_secs: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<usize>,
    /// The alert's reason of a `recovery` detection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reopened: Option<bool>,
}

/// `stall_nudged`.
#[derive(Debug, Default, Deserialize)]
pub struct StallNudged<'a> {
    #[serde(default, borrow, deserialize_with = "typed")]
    pub phase: Option<&'a str>,
    /// How long the session had been idle.
    #[serde(default, deserialize_with = "typed")]
    pub idle_secs: Option<i64>,
}

/// The `stall_nudged` the stall watch records.
#[derive(Debug, Clone, Serialize)]
pub struct NewStallNudged<'a, B: Serialize> {
    pub phase: &'a str,
    pub idle_secs: i64,
    pub threshold_secs: i64,
    pub background_running: bool,
    pub background_tasks: B,
    pub workspace_id: &'a str,
    /// The `worker_question` closed without its answer that the nudge
    /// told of, in its place.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_ask: Option<i64>,
}

/// `recovery_requested` and `recovery_finished`: a recovery job of an
/// alert of the run.
#[derive(Debug, Default, Deserialize)]
pub struct RecoveryRecord<'a> {
    #[serde(default, borrow, deserialize_with = "typed")]
    pub alert: Option<&'a str>,
    #[serde(default, borrow, deserialize_with = "typed")]
    pub reason: Option<&'a str>,
    #[serde(default, deserialize_with = "typed")]
    pub attempt: Option<u64>,
    /// The ask a `recovery_finished` escalated to.
    #[serde(default, deserialize_with = "typed")]
    pub ask_id: Option<i64>,
    /// How long the session had been idle when it was requested.
    #[serde(default, deserialize_with = "typed")]
    pub idle_secs: Option<i64>,
    /// The repairs a `recovery_finished` applied.
    #[serde(default, borrow, deserialize_with = "typed")]
    applied: Option<Vec<Lenient<&'a str>>>,
}

impl RecoveryRecord<'_> {
    /// Whether it applied a repair other than `wait` (any item that is not
    /// the text `wait`).
    pub fn repaired(&self) -> bool {
        self.applied
            .as_ref()
            .is_some_and(|applied| applied.iter().any(|a| a.typed() != Some("wait")))
    }
}

/// `auto_repaired`.
#[derive(Debug, Default, Deserialize)]
pub struct AutoRepaired<'a> {
    #[serde(default, borrow, deserialize_with = "typed")]
    pub repair: Option<&'a str>,
}

/// `turn_requested`.
#[derive(Debug, Default, Deserialize)]
pub struct TurnRequested {
    /// The request's number in the session's `turns/`.
    #[serde(default, deserialize_with = "typed")]
    pub seq: Option<u64>,
}

/// `provider_waiting`.
#[derive(Debug, Default, Deserialize)]
pub struct ProviderWaiting {
    /// The turn that ended on the provider's wall; absent on an event
    /// without it.
    #[serde(default, deserialize_with = "typed")]
    pub turn: Option<u64>,
}

/// `revise_requested`.
#[derive(Debug, Default, Deserialize)]
pub struct ReviseRequested {
    #[serde(default, deserialize_with = "typed")]
    pub attempt: Option<u64>,
    #[serde(default, deserialize_with = "typed")]
    pub reasons: Option<Vec<String>>,
}

/// `conflict_precheck`.
#[derive(Debug, Default, Deserialize)]
pub struct ConflictPrecheck<'a> {
    /// The conflict request was sent to the session.
    #[serde(default, deserialize_with = "typed")]
    pub requested: Option<bool>,
    #[serde(default, deserialize_with = "typed")]
    pub attempt: Option<u64>,
    /// Why a person was asked instead.
    #[serde(default, borrow, deserialize_with = "typed")]
    pub asked: Option<&'a str>,
}

/// `resume_started`.
#[derive(Debug, Default, Deserialize)]
pub struct ResumeStarted {
    /// Whether the resume counts toward `MAX_RESUME_ATTEMPTS`; absent on
    /// an event older than the field.
    #[serde(default, deserialize_with = "typed")]
    counted: Option<bool>,
}

impl ResumeStarted {
    /// Whether the resume counts toward `MAX_RESUME_ATTEMPTS`: all but
    /// those recorded `counted: false`, so an older event counts.
    pub fn counted(&self) -> bool {
        self.counted != Some(false)
    }
}

/// `revise_unsent`.
#[derive(Debug, Default, Deserialize)]
pub struct ReviseUnsent<'a> {
    #[serde(default, borrow, deserialize_with = "typed")]
    pub error: Option<&'a str>,
}

/// `concern_decided`.
#[derive(Debug, Default, Deserialize)]
pub struct ConcernDecided {
    #[serde(default, deserialize_with = "typed")]
    pub attempt: Option<u64>,
    /// The `send_back` was applied.
    #[serde(default, deserialize_with = "typed")]
    pub applied: Option<bool>,
}

/// `review_finished`, besides its verdict ([`review_verdict`]).
#[derive(Debug, Default, Deserialize)]
pub struct ReviewFinished<'a> {
    #[serde(default, borrow, deserialize_with = "typed")]
    pub verdict: Option<&'a str>,
    #[serde(default, deserialize_with = "typed")]
    pub attempt: Option<u64>,
    #[serde(default, deserialize_with = "object")]
    route: Option<ReviewRoute>,
}

/// Where a review sent the run (ADR-t1453-1 decision 7).
#[derive(Debug, Default, Deserialize)]
struct ReviewRoute {
    #[serde(default, deserialize_with = "typed")]
    destination: Option<String>,
    /// Where the verdict's own judgment went.
    #[serde(default, deserialize_with = "typed")]
    parent: Option<String>,
    #[serde(default, deserialize_with = "objects")]
    agents: Option<Vec<Option<RouteAgent>>>,
}

/// Where one subagent's judgment went.
#[derive(Debug, Clone, Default, Deserialize)]
struct RouteAgent {
    #[serde(default, deserialize_with = "typed")]
    destination: Option<String>,
}

impl ReviewFinished<'_> {
    /// Where the route went, when it names it.
    pub fn destination(&self) -> Option<&str> {
        self.route.as_ref()?.destination.as_deref()
    }

    /// Whether the route was decided by one of the review's subagents:
    /// one went further than landing, to where the review went.
    pub fn agents_decided(&self) -> bool {
        let Some(route) = &self.route else {
            return false;
        };
        let Some(destination) = route.destination.as_deref() else {
            return false;
        };
        destination != "land"
            && route.agents.as_ref().is_some_and(|agents| {
                agents
                    .iter()
                    .flatten()
                    .any(|agent| agent.destination.as_deref() == Some(destination))
            })
    }

    /// Whether the verdict's own judgment went where the route went.
    pub fn parent_decided(&self) -> bool {
        self.route
            .as_ref()
            .is_some_and(|route| route.parent.is_some() && route.parent == route.destination)
    }
}

/// The verdict a `review_finished` recorded, with a concern's
/// recommendation, confidence and reason when it gave them, and the
/// required subagents' results when the review had them (ADR-t1453-1).
pub fn review_verdict(payload: &Value) -> serde_json::Result<ReviewVerdict> {
    let mut verdict = json!({
        "verdict": payload["verdict"],
        "reasons": payload["reasons"],
        "summary": payload["summary"],
        "recommendation": payload["recommendation"],
        "confidence": payload["confidence"],
        "reason_category": payload["reason_category"],
    });
    if let Some(agents) = payload.get("agents").filter(|a| a.is_array()) {
        verdict["agents"] = agents.clone();
    }
    serde_json::from_value(verdict)
}

/// The route of a `review_finished` (`payload`) whose agents' judgments
/// decide it: recorded as `route(true)` gave it, so `None` for a verdict
/// recorded without one, which is judged by its own reasons.
fn deciding_route(
    payload: &Value,
    verdict: &ReviewVerdict,
) -> Option<crate::domain::review_subagents::VerdictRoute> {
    payload
        .get("route")
        .filter(|route| route.is_object())
        .map(|_| verdict.route(true))
        .filter(|route| !route.deciding().is_empty())
}

/// The findings a `review_finished` (`payload`) sent the run back with, as
/// its `approve_landing` ask showed them: when it recorded a route that its
/// agents' judgments decide (ADR-t1453-1 decision 7), each deciding
/// agent's under its name and the verdict's own only when its judgment
/// went there too; else the verdict's reasons, or its summary when it gave
/// none ([`ReviewVerdict::sent_back_findings`]). `None` when the payload is
/// no verdict.
pub fn review_findings(payload: &Value) -> Option<Vec<String>> {
    let verdict = review_verdict(payload).ok()?;
    let route = deciding_route(payload, &verdict);
    Some(
        verdict
            .sent_back_findings(route.as_ref())
            .into_iter()
            .map(|finding| finding.text)
            .collect(),
    )
}

/// A review that sent the run back, as a reader of the run's reviews sees
/// it ([`review_sent_back`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentBackReview {
    /// The verdicts of the judgments that sent it back, each once: the
    /// review's own when its judgment went where the route went, then each
    /// deciding agent's.
    pub verdicts: Vec<crate::domain::ReviewDecision>,
    /// Its findings ([`review_findings`]).
    pub findings: Vec<String>,
}

/// What a `review_finished` (`payload`) sent the run back with: `None`
/// when it did not send it back (its recorded route went to `land`, or it
/// recorded none and passed) or is no verdict. A review whose own verdict
/// passed but whose agents sent the run back is one.
pub fn review_sent_back(payload: &Value) -> Option<SentBackReview> {
    let verdict = review_verdict(payload).ok()?;
    let finished: ReviewFinished = restore(payload);
    let landed = match finished.destination() {
        Some(destination) => destination == "land",
        None => verdict.verdict == crate::domain::ReviewDecision::Pass,
    };
    if landed {
        return None;
    }
    let route = deciding_route(payload, &verdict);
    let mut verdicts = Vec::new();
    if route
        .as_ref()
        .is_none_or(|route| route.parent == route.destination)
    {
        verdicts.push(verdict.verdict);
    }
    for deciding in route.iter().flat_map(|route| route.deciding()) {
        let agent = verdict
            .agents
            .iter()
            .find(|result| result.agent == deciding.agent)
            .and_then(|result| result.verdict);
        if let Some(agent) = agent
            && !verdicts.contains(&agent)
        {
            verdicts.push(agent);
        }
    }
    Some(SentBackReview {
        verdicts,
        findings: verdict
            .sent_back_findings(route.as_ref())
            .into_iter()
            .map(|finding| finding.text)
            .collect(),
    })
}
