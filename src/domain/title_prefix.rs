//! The prefixes a task's title starts with. A landing's squash commit on
//! main takes the task's title as its subject, and the controlling git runs
//! no hook, so a repository's rule of commit prefixes reaches main only
//! through the titles. The runtime holds no prefix (ADR-t2257-1); a
//! repository names its own in `[tasks] title_prefixes` of `dagq.toml`, and
//! then `lint` reports and `submit` refuses a task whose title starts with
//! none of them. `add` and `edit` do not check, so the drafts the runtime
//! makes from a receipt or a review keep their titles until a planner
//! edits them before the submit. Without the key no title is checked.

use super::{DomainError, TaskId, require};

/// The repository's prefixes, `[tasks] title_prefixes` of `dagq.toml`, in
/// the order written, each once. A title passes when it is
/// `<prefix>[(<scope>)][!]: <subject>`: one of the prefixes, an optional
/// non-empty scope in parentheses, an optional `!`, a `:`, whitespace and a
/// subject that is not blank.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitlePrefixes(Vec<String>);

impl TitlePrefixes {
    /// The set of `prefixes`; an empty set, an empty prefix, one with a
    /// `:` or whitespace, or one twice is refused.
    pub fn new(prefixes: Vec<String>) -> Result<Self, String> {
        if prefixes.is_empty() {
            return Err("names no prefix".to_owned());
        }
        for (index, prefix) in prefixes.iter().enumerate() {
            if prefix.is_empty() {
                return Err("names an empty prefix".to_owned());
            }
            if prefix.contains(':') || prefix.contains(char::is_whitespace) {
                return Err(format!(
                    "prefix {prefix:?} has a ':' or whitespace; name the prefix alone, like \"feat\""
                ));
            }
            if prefixes[..index].contains(prefix) {
                return Err(format!("names {prefix} twice"));
            }
        }
        Ok(Self(prefixes))
    }

    pub fn values(&self) -> &[String] {
        &self.0
    }

    /// Whether `title` starts with one of the prefixes in the form above.
    pub fn accepts(&self, title: &str) -> bool {
        self.0.iter().any(|prefix| {
            title
                .strip_prefix(prefix.as_str())
                .is_some_and(after_prefix)
        })
    }

    /// The title of task `task_id` (`lint` and `submit`).
    pub fn check(&self, task_id: TaskId, title: &str) -> Result<(), DomainError> {
        require(self.accepts(title), || DomainError::TitlePrefixOutsideSet {
            task_id,
            title: title.to_owned(),
            prefixes: self.0.clone(),
        })
    }
}

/// What follows the prefix: `[(<scope>)][!]: <subject>`.
fn after_prefix(rest: &str) -> bool {
    let rest = match rest.strip_prefix('(') {
        Some(scoped) => match scoped.split_once(')') {
            Some((scope, after)) if !scope.is_empty() && !scope.contains('(') => after,
            _ => return false,
        },
        None => rest,
    };
    let rest = rest.strip_prefix('!').unwrap_or(rest);
    rest.strip_prefix(':').is_some_and(|subject| {
        subject.starts_with(char::is_whitespace) && !subject.trim().is_empty()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefixes(values: &[&str]) -> Result<TitlePrefixes, String> {
        TitlePrefixes::new(values.iter().map(|v| (*v).to_owned()).collect())
    }

    #[test]
    fn a_title_starts_with_a_prefix_an_optional_scope_and_bang_and_a_subject() {
        let set = prefixes(&["feat", "fix"]).unwrap();
        for title in [
            "feat: x",
            "feat(cli): x",
            "feat!: x",
            "feat(cli)!: x",
            "fix:  two words",
        ] {
            assert!(set.accepts(title), "{title:?}");
        }
        for title in [
            "runtime: x",
            "feat:x",
            "feat: ",
            "feat:",
            "feat",
            "no prefix",
            "",
            "feature: x",
            "Feat: x",
            "feat(): x",
            "feat(cli: x",
            "feat(a(b)): x",
            "feat!!: x",
            " feat: x",
        ] {
            assert!(!set.accepts(title), "{title:?}");
        }
    }

    #[test]
    fn a_refused_title_names_the_task_and_the_prefixes() {
        let set = prefixes(&["feat", "fix"]).unwrap();
        set.check(TaskId::new(4), "fix: y").unwrap();
        let refused = set.check(TaskId::new(4), "runtime: y").unwrap_err();
        assert!(matches!(refused, DomainError::TitlePrefixOutsideSet { .. }));
        let message = refused.to_string();
        assert!(message.contains("task 4"), "{message}");
        assert!(message.contains("feat, fix"), "{message}");
        assert_eq!(set.values(), ["feat", "fix"]);
    }

    #[test]
    fn an_empty_set_an_empty_prefix_a_colon_or_a_repeat_is_refused() {
        assert_eq!(prefixes(&[]).unwrap_err(), "names no prefix");
        assert_eq!(
            prefixes(&["feat", ""]).unwrap_err(),
            "names an empty prefix"
        );
        assert!(prefixes(&["feat:"]).unwrap_err().contains("':'"));
        assert!(prefixes(&["a b"]).unwrap_err().contains("whitespace"));
        assert_eq!(prefixes(&["fix", "fix"]).unwrap_err(), "names fix twice");
    }
}
