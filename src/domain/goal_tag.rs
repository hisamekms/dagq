//! The tags of a goal (ADR-t1639-1 decision 6): labels of the repository's
//! own naming what the goal is about, zero or more per goal, each once. The
//! runtime holds no set of values; a repository names its own in `[goals]
//! tags` of `dagq.toml`, and then `goal add` and `goal edit` refuse a tag
//! outside it and `lint` and `submit` refuse a draft goal without one.
//! Without a set any tag of the right form is accepted and a goal may have
//! none. `goal list --tag` narrows the goals by them.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::{DomainError, GoalId, require};

/// A goal's tag: a lowercase slug of letters, digits, '-' and '_' (at most
/// 64 bytes), the form of a task's change.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GoalTag(String);

impl GoalTag {
    /// The longest tag, in bytes.
    pub const MAX_LEN: usize = 64;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for GoalTag {
    type Err = DomainError;
    fn from_str(value: &str) -> Result<Self, DomainError> {
        require(
            !value.is_empty()
                && value.len() <= Self::MAX_LEN
                && value.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'
                }),
            || DomainError::InvalidGoalTag {
                tag: value.to_owned(),
            },
        )?;
        Ok(Self(value.to_owned()))
    }
}

impl TryFrom<String> for GoalTag {
    type Error = DomainError;
    fn try_from(value: String) -> Result<Self, DomainError> {
        value.parse()
    }
}

impl From<GoalTag> for String {
    fn from(tag: GoalTag) -> Self {
        tag.0
    }
}

impl fmt::Display for GoalTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A goal's tags as given, refused when one is given twice.
pub fn check_distinct(tags: &[GoalTag]) -> Result<(), DomainError> {
    for (index, tag) in tags.iter().enumerate() {
        require(!tags[..index].contains(tag), || {
            DomainError::GoalTagRepeated {
                tag: tag.as_str().to_owned(),
            }
        })?;
    }
    Ok(())
}

/// The repository's set of goal tags, `[goals] tags` of `dagq.toml`, in
/// the order written, each once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagSet(Vec<GoalTag>);

impl TagSet {
    /// The set of `tags`; an empty set or a value twice is refused.
    pub fn new(tags: Vec<GoalTag>) -> Result<Self, String> {
        if tags.is_empty() {
            return Err("names no tag".to_owned());
        }
        for (index, tag) in tags.iter().enumerate() {
            if tags[..index].contains(tag) {
                return Err(format!("names {tag} twice"));
            }
        }
        Ok(Self(tags))
    }

    pub fn values(&self) -> &[GoalTag] {
        &self.0
    }

    fn names(&self) -> Vec<String> {
        self.0.iter().map(|tag| tag.as_str().to_owned()).collect()
    }

    /// Each of `tags` when it is one of the set (`goal add` and `goal
    /// edit`); none is fine.
    pub fn check(&self, tags: &[GoalTag]) -> Result<(), DomainError> {
        for tag in tags {
            require(self.0.contains(tag), || DomainError::GoalTagNotInSet {
                tag: tag.as_str().to_owned(),
                tags: self.names(),
            })?;
        }
        Ok(())
    }

    /// A draft goal's tags on its way to plan review: at least one, each
    /// of the set (`lint` and `submit`).
    pub fn check_declared(&self, goal_id: GoalId, tags: &[GoalTag]) -> Result<(), DomainError> {
        require(!tags.is_empty(), || DomainError::GoalTagMissing {
            goal_id,
            tags: self.names(),
        })?;
        self.check(tags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(value: &str) -> GoalTag {
        value.parse().unwrap()
    }

    #[test]
    fn a_tag_is_a_lowercase_slug() {
        for label in ["codex", "throughput", "a-b_1", "unknown"] {
            assert_eq!(tag(label).as_str(), label);
            let read: GoalTag = serde_json::from_value(serde_json::json!(label)).unwrap();
            assert_eq!(read.to_string(), label);
        }
        let long = "a".repeat(GoalTag::MAX_LEN + 1);
        for bad in ["", "Codex", "a b", long.as_str()] {
            assert!(
                matches!(
                    bad.parse::<GoalTag>(),
                    Err(DomainError::InvalidGoalTag { .. })
                ),
                "{bad:?}"
            );
        }
        assert!(serde_json::from_value::<GoalTag>(serde_json::json!("Upper")).is_err());
        assert_eq!(String::from(tag("cmux")), "cmux");
        check_distinct(&[tag("a"), tag("b")]).unwrap();
        assert_eq!(
            check_distinct(&[tag("a"), tag("b"), tag("a")]).unwrap_err(),
            DomainError::GoalTagRepeated { tag: "a".into() }
        );
    }

    #[test]
    fn a_set_checks_values_and_declarations() {
        let set = TagSet::new(vec![tag("codex"), tag("throughput")]).unwrap();
        assert_eq!(set.values().len(), 2);
        set.check(&[]).unwrap();
        set.check(&[tag("codex"), tag("throughput")]).unwrap();
        let refused = set.check(&[tag("codex"), tag("cmux")]).unwrap_err();
        assert!(matches!(refused, DomainError::GoalTagNotInSet { ref tag, .. } if tag == "cmux"));
        assert!(
            refused.to_string().contains("codex, throughput"),
            "{refused}"
        );
        set.check_declared(GoalId::new(3), &[tag("codex")]).unwrap();
        let missing = set.check_declared(GoalId::new(3), &[]).unwrap_err();
        assert!(matches!(missing, DomainError::GoalTagMissing { .. }));
        assert!(missing.to_string().contains("draft goal 3"), "{missing}");
        assert!(matches!(
            set.check_declared(GoalId::new(3), &[tag("cmux")]),
            Err(DomainError::GoalTagNotInSet { .. })
        ));
        assert!(TagSet::new(Vec::new()).is_err());
        assert!(TagSet::new(vec![tag("a"), tag("a")]).is_err());
    }
}
