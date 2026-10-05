//! The payloads of recorded run events that [`super::RunHistory`] decides
//! by, restored as typed values (task 1551). Each type names the keys it
//! reads once; history reads the fields, not the keys.
//!
//! These restore what earlier versions wrote, so they never refuse a
//! payload: a missing key, an extra key, `null` or a value of another type
//! reads as the field's absence, the same as `payload["key"].as_str()` and
//! its kin read it. A payload that is not a JSON object restores to the
//! default (every field absent). Writing a payload is
//! [`super::recorded`]'s, not this module's.

use serde::de::IgnoredAny;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// Restores `payload` as `T`; see the module doc for what it tolerates.
pub(super) fn restore<'a, T>(payload: &'a Value) -> T
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
pub(super) struct AskOpened<'a> {
    #[serde(default, borrow, deserialize_with = "typed")]
    pub(super) kind: Option<&'a str>,
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
