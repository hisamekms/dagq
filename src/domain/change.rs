//! The change a task declares (ADR-t980-1): the kind of change it makes, a
//! label of the repository's own, one per task. The runtime holds no set of
//! values; a repository names its own in `[tasks] changes` of `dagq.toml`,
//! and then `add` and `edit` refuse a value outside it and `lint` and
//! `submit` refuse a task without one. Without a set any label is accepted
//! and a task may declare none. `stats`, `kpi` and `forecast` group the
//! runs by it, `unknown` for a task without one.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{DomainError, TaskId, require};

/// A task's change: a lowercase slug of letters, digits, '-' and '_' (at
/// most 64 bytes), neither `unknown` (the name of the tasks without one)
/// nor `all` (every task together), like a task's kind (ADR-t980-1
/// decision 6 (a)).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TaskChange(String);

impl TaskChange {
    /// The name `stats`, `kpi` and `forecast` give the tasks without a change.
    pub const NONE: &'static str = "unknown";
    /// The name of every task together.
    pub const ALL: &'static str = "all";
    /// The longest label, in bytes.
    pub const MAX_LEN: usize = 64;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for TaskChange {
    type Err = DomainError;
    fn from_str(value: &str) -> Result<Self, DomainError> {
        require(
            !value.is_empty()
                && value.len() <= Self::MAX_LEN
                && value != Self::NONE
                && value != Self::ALL
                && value.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'
                }),
            || DomainError::InvalidTaskChange {
                change: value.to_owned(),
            },
        )?;
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for TaskChange {
    type Error = DomainError;
    fn try_from(value: String) -> Result<Self, DomainError> {
        value.parse()
    }
}

impl From<TaskChange> for String {
    fn from(change: TaskChange) -> Self {
        change.0
    }
}

impl fmt::Display for TaskChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The repository's set of changes, `[tasks] changes` of `dagq.toml`, in
/// the order written, each once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSet(Vec<TaskChange>);

impl ChangeSet {
    /// The set of `changes`; an empty set or a value twice is refused.
    pub fn new(changes: Vec<TaskChange>) -> Result<Self, String> {
        if changes.is_empty() {
            return Err("names no change".to_owned());
        }
        for (index, change) in changes.iter().enumerate() {
            if changes[..index].contains(change) {
                return Err(format!("names {change} twice"));
            }
        }
        Ok(Self(changes))
    }

    pub fn values(&self) -> &[TaskChange] {
        &self.0
    }

    fn names(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|change| change.as_str().to_owned())
            .collect()
    }

    /// `change` when it is one of the set (`add` and `edit`).
    pub fn check(&self, change: &TaskChange) -> Result<(), DomainError> {
        require(self.0.contains(change), || DomainError::ChangeNotInSet {
            change: change.as_str().to_owned(),
            changes: self.names(),
        })
    }

    /// A task's declared change: one of the set, never none (`lint` and
    /// `submit`).
    pub fn check_declared(
        &self,
        task_id: TaskId,
        change: Option<&TaskChange>,
    ) -> Result<(), DomainError> {
        match change {
            Some(change) => self.check(change),
            None => Err(DomainError::ChangeMissing {
                task_id,
                changes: self.names(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(value: &str) -> TaskChange {
        value.parse().unwrap()
    }

    #[test]
    fn a_change_is_a_lowercase_slug_other_than_unknown_and_all() {
        for label in ["feature", "fix", "a-b_1"] {
            assert_eq!(change(label).as_str(), label);
            let read: TaskChange = serde_json::from_value(serde_json::json!(label)).unwrap();
            assert_eq!(read.to_string(), label);
        }
        let long = "a".repeat(TaskChange::MAX_LEN + 1);
        for bad in ["", "Fix", "a b", "unknown", "all", long.as_str()] {
            assert!(
                matches!(
                    bad.parse::<TaskChange>(),
                    Err(DomainError::InvalidTaskChange { .. })
                ),
                "{bad:?}"
            );
        }
        assert!(serde_json::from_value::<TaskChange>(serde_json::json!("Upper")).is_err());
        assert_eq!(String::from(change("fix")), "fix");
    }

    #[test]
    fn a_set_checks_values_and_declarations() {
        let set = ChangeSet::new(vec![change("feature"), change("fix")]).unwrap();
        assert_eq!(set.values().len(), 2);
        set.check(&change("fix")).unwrap();
        let refused = set.check(&change("docs")).unwrap_err();
        assert!(refused.to_string().contains("feature, fix"), "{refused}");
        set.check_declared(TaskId::new(3), Some(&change("feature")))
            .unwrap();
        let missing = set.check_declared(TaskId::new(3), None).unwrap_err();
        assert!(matches!(missing, DomainError::ChangeMissing { .. }));
        assert!(missing.to_string().contains("task 3"), "{missing}");
        assert!(ChangeSet::new(Vec::new()).is_err());
        assert!(ChangeSet::new(vec![change("fix"), change("fix")]).is_err());
    }
}
