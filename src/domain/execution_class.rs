//! A task's execution class (ADR-t1487-1 decision 1): whether its run
//! implements (`implementation`, the default) or investigates a premise of
//! the plan by experiment, prototype, build or measurement (`spike`). The
//! runtime holds the set and what it means, independent of the task's
//! change (ADR-t980-1). Like the priority, it changes only while the task
//! has not started (`add`, `edit` of a draft or submitted task).
//!
//! A Spike's run reports its result in the receipt's `spike_result`
//! (decision 3), which validation requires of a Spike's run only:
//! [`missing_spike_result`] names what is missing or malformed. A negative
//! verdict and an unresolved one are results, not gaps.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::DomainError;

/// A task's execution class, `implementation` unless it names `spike`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionClass {
    #[default]
    Implementation,
    Spike,
}

impl ExecutionClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Implementation => "implementation",
            Self::Spike => "spike",
        }
    }

    pub fn is_spike(self) -> bool {
        self == Self::Spike
    }
}

impl std::str::FromStr for ExecutionClass {
    type Err = DomainError;
    fn from_str(value: &str) -> Result<Self, DomainError> {
        match value {
            "implementation" => Ok(Self::Implementation),
            "spike" => Ok(Self::Spike),
            _ => Err(DomainError::UnknownValue {
                kind: "ExecutionClass",
                value: value.to_owned(),
            }),
        }
    }
}

/// The receipt's field that carries a Spike's result.
pub const SPIKE_RESULT_FIELD: &str = "spike_result";

// What the Spike found of its question: it holds, it does not hold, or the
// investigation could not tell within its limits (ADR-t1487-1 decision 3).
string_enum!(SpikeVerdict {
    Holds => "holds",
    DoesNotHold => "does_not_hold",
    Unresolved => "unresolved",
});

/// The text fields of `spike_result` that must not be blank: the grounds
/// of the verdict and where the evidence is.
const RESULT_TEXTS: [&str; 2] = ["grounds", "evidence"];

/// The conditions the evidence was taken under, each a non-blank text in
/// `spike_result.conditions`: the commit examined, the versions of the
/// tools, the provider and the route the agent ran on (a worker's own,
/// not a planner's, a subagent's or a person's terminal), and the
/// environment.
const CONDITIONS: [&str; 4] = ["commit", "tools", "provider", "environment"];

/// The fields of a Spike's result `spike_result` lacks or holds in the
/// wrong shape, as dotted paths from the receipt (`spike_result` itself
/// when it is absent or not an object); empty when it is complete. The
/// runtime checks the shape only, never whether the grounds convince.
pub fn missing_spike_result(result: Option<&Value>) -> Vec<String> {
    let Some(result) = result.and_then(Value::as_object) else {
        return vec![SPIKE_RESULT_FIELD.to_owned()];
    };
    let mut missing = Vec::new();
    if result
        .get("verdict")
        .and_then(Value::as_str)
        .and_then(|verdict| verdict.parse::<SpikeVerdict>().ok())
        .is_none()
    {
        missing.push(format!("{SPIKE_RESULT_FIELD}.verdict"));
    }
    let blank = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .is_none_or(|text| text.trim().is_empty())
    };
    for field in RESULT_TEXTS {
        if blank(result.get(field)) {
            missing.push(format!("{SPIKE_RESULT_FIELD}.{field}"));
        }
    }
    let conditions = result.get("conditions").and_then(Value::as_object);
    for field in CONDITIONS {
        if blank(conditions.and_then(|conditions| conditions.get(field))) {
            missing.push(format!("{SPIKE_RESULT_FIELD}.conditions.{field}"));
        }
    }
    missing
}

/// The reason a run parked for its Spike result carries: the fields
/// missing, in the order [`missing_spike_result`] names them.
pub fn spike_result_missing_reason(missing: &[String]) -> String {
    format!("spike result missing: {}", missing.join(", "))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn complete(verdict: &str) -> Value {
        json!({
            "verdict": verdict,
            "grounds": "the build failed on the worker's route",
            "evidence": "docs/plans/x.md",
            "conditions": {
                "commit": "0123abc",
                "tools": "cargo 1.90",
                "provider": "claude headless worker",
                "environment": "macOS 14 arm64",
            },
            "spent": "40 minutes",
        })
    }

    #[test]
    fn the_default_class_is_implementation_and_both_round_trip() {
        assert_eq!(ExecutionClass::default(), ExecutionClass::Implementation);
        for class in [ExecutionClass::Implementation, ExecutionClass::Spike] {
            assert_eq!(class.as_str().parse::<ExecutionClass>().unwrap(), class);
        }
        assert!("measure".parse::<ExecutionClass>().is_err());
        assert!(ExecutionClass::Spike.is_spike());
        assert!(!ExecutionClass::Implementation.is_spike());
    }

    #[test]
    fn every_verdict_with_its_fields_is_complete() {
        for verdict in ["holds", "does_not_hold", "unresolved"] {
            assert!(
                missing_spike_result(Some(&complete(verdict))).is_empty(),
                "{verdict}"
            );
        }
    }

    #[test]
    fn an_absent_or_malformed_result_names_what_is_missing() {
        assert_eq!(missing_spike_result(None), ["spike_result"]);
        assert_eq!(
            missing_spike_result(Some(&json!("holds"))),
            ["spike_result"]
        );
        let mut result = complete("maybe");
        result["grounds"] = json!("  ");
        result["conditions"]
            .as_object_mut()
            .unwrap()
            .remove("tools");
        result["conditions"]["environment"] = json!(3);
        assert_eq!(
            missing_spike_result(Some(&result)),
            [
                "spike_result.verdict",
                "spike_result.grounds",
                "spike_result.conditions.tools",
                "spike_result.conditions.environment",
            ]
        );
        let mut result = complete("holds");
        result.as_object_mut().unwrap().remove("conditions");
        assert_eq!(
            missing_spike_result(Some(&result)),
            CONDITIONS.map(|field| format!("spike_result.conditions.{field}"))
        );
        assert_eq!(
            spike_result_missing_reason(&["spike_result.verdict".to_owned()]),
            "spike result missing: spike_result.verdict"
        );
    }
}
