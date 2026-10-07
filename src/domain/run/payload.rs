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
fn typed<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Lenient<T>>::deserialize(deserializer)?.and_then(Lenient::typed))
}

/// A key whose value is a JSON object, read as `T`; absent when it is
/// missing, of another type (an array too) or does not read as `T`.
fn object<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
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
fn objects<'de, D, T>(deserializer: D) -> Result<Option<Vec<Option<T>>>, D::Error>
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

/// `integration_approved`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct IntegrationApproved {
    /// The `approve_landing` ask whose answer approved the run; absent
    /// when missing or `null`, kept (of whatever type) otherwise.
    #[serde(default)]
    ask_id: Option<Lenient<i64>>,
    /// `false` from `integrate --no-push`.
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
pub(super) struct ResumeFinished<'a> {
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) status: Option<&'a str>,
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) outcome: Option<&'a str>,
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) workspace_id: Option<&'a str>,
    #[serde(default, deserialize_with = "typed")]
    pub(super) attempt: Option<u64>,
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

/// `follow_up_registered`.
#[derive(Debug, Default, Deserialize)]
pub(super) struct FollowUpRegistered {
    #[serde(default, deserialize_with = "typed")]
    pub(super) index: Option<u64>,
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
