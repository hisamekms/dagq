//! An additional human landing gate. This pure decision does not replace review
//! routing or any existing reason to ask for approval. No landing caller uses it
//! until the separate landing integration is installed.

use super::{AskConfidence, scope::glob_matches};

/// A goal's approval mark, shared by configuration and goal attribute storage.
/// Names must contain a non-whitespace character; names otherwise remain literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalMark(String);

impl GoalMark {
    pub fn new(name: String) -> Result<Self, &'static str> {
        if name.trim().is_empty() {
            Err("goal mark name must not be blank")
        } else {
            Ok(Self(name))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Parsed declarations are not trusted merely because they are valid TOML.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ApprovalSettings {
    pub paths: Vec<String>,
    pub low_confidence: bool,
    pub goal_marks: Vec<GoalMark>,
    pub two_person: bool,
}

/// Conditions from a commit-pinned main configuration. No public constructor,
/// Default or deserializer allows a run's parsed configuration to become trusted.
/// The infrastructure main reader is the sole production constructor caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainApprovalConditions {
    settings: ApprovalSettings,
    main_commit: String,
}

impl MainApprovalConditions {
    pub(crate) fn from_main(main_commit: String, settings: ApprovalSettings) -> Self {
        Self {
            settings,
            main_commit,
        }
    }

    pub fn main_commit(&self) -> &str {
        &self.main_commit
    }

    /// Requires the owner and a distinct eligible person when a gate opens.
    /// This changes approval requirements, never opens a gate by itself.
    pub fn two_person(&self) -> bool {
        self.settings.two_person
    }
}

/// A matching declaration, once per configured condition, in declaration-kind
/// order (path, confidence, mark). An empty result means no additional gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalMatch {
    Path(String),
    LowConfidence,
    GoalMark(GoalMark),
}

/// `changed_paths` includes additions, deletions, modifications and BOTH ends
/// of renames. Status does not alter matching. Conditions are ORed; all matches
/// are returned so an approval request can explain every applicable condition.
pub fn matching_conditions(
    conditions: &MainApprovalConditions,
    changed_paths: &[String],
    review_confidence: Option<AskConfidence>,
    goal_marks: &[GoalMark],
) -> Vec<ApprovalMatch> {
    let settings = &conditions.settings;
    let mut matched = Vec::new();
    for glob in &settings.paths {
        if changed_paths.iter().any(|path| glob_matches(glob, path)) {
            let condition = ApprovalMatch::Path(glob.clone());
            if !matched.contains(&condition) {
                matched.push(condition);
            }
        }
    }
    if settings.low_confidence && review_confidence == Some(AskConfidence::Low) {
        matched.push(ApprovalMatch::LowConfidence);
    }
    for mark in &settings.goal_marks {
        let condition = ApprovalMatch::GoalMark(mark.clone());
        if goal_marks.contains(mark) && !matched.contains(&condition) {
            matched.push(condition);
        }
    }
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_are_or_and_rename_matches_either_end() {
        let mark = GoalMark::new("sensitive".into()).unwrap();
        let conditions = MainApprovalConditions::from_main(
            "main-sha".into(),
            ApprovalSettings {
                paths: vec!["protected/**".into()],
                low_confidence: true,
                goal_marks: vec![mark.clone()],
                two_person: false,
            },
        );
        for (case, paths, confidence, marks, expected) in [
            (
                "addition",
                vec!["protected/new"],
                None,
                vec![],
                vec![ApprovalMatch::Path("protected/**".into())],
            ),
            (
                "deletion",
                vec!["protected/old"],
                None,
                vec![],
                vec![ApprovalMatch::Path("protected/**".into())],
            ),
            (
                "modification",
                vec!["protected/edit"],
                None,
                vec![],
                vec![ApprovalMatch::Path("protected/**".into())],
            ),
            (
                "rename from",
                vec!["protected/old", "other/new"],
                None,
                vec![],
                vec![ApprovalMatch::Path("protected/**".into())],
            ),
            (
                "rename to",
                vec!["other/old", "protected/new"],
                None,
                vec![],
                vec![ApprovalMatch::Path("protected/**".into())],
            ),
            (
                "low",
                vec![],
                Some(AskConfidence::Low),
                vec![],
                vec![ApprovalMatch::LowConfidence],
            ),
            (
                "mark",
                vec![],
                None,
                vec![mark.clone()],
                vec![ApprovalMatch::GoalMark(mark.clone())],
            ),
            (
                "all",
                vec!["protected/edit"],
                Some(AskConfidence::Low),
                vec![mark.clone()],
                vec![
                    ApprovalMatch::Path("protected/**".into()),
                    ApprovalMatch::LowConfidence,
                    ApprovalMatch::GoalMark(mark.clone()),
                ],
            ),
            (
                "none",
                vec!["other/edit"],
                Some(AskConfidence::High),
                vec![],
                vec![],
            ),
        ] {
            let paths = paths.into_iter().map(String::from).collect::<Vec<_>>();
            assert_eq!(
                matching_conditions(&conditions, &paths, confidence, &marks),
                expected,
                "{case}"
            );
        }
        let empty =
            MainApprovalConditions::from_main("main-sha".into(), ApprovalSettings::default());
        assert!(
            matching_conditions(
                &empty,
                &["protected/edit".into()],
                Some(AskConfidence::Low),
                &[mark]
            )
            .is_empty()
        );
    }

    #[test]
    fn paths_use_the_task_scope_matcher() {
        for glob in ["*.md", "docs/**", "**/*.md", "a?c", "literal[abc]"] {
            let conditions = MainApprovalConditions::from_main(
                "main-sha".into(),
                ApprovalSettings {
                    paths: vec![glob.into()],
                    ..Default::default()
                },
            );
            for path in [
                "README.md",
                "docs/a.md",
                "docs/deep/a.md",
                "aéc",
                "literal[abc]",
            ] {
                assert_eq!(
                    !matching_conditions(&conditions, &[path.into()], None, &[]).is_empty(),
                    glob_matches(glob, path),
                    "{glob}: {path}"
                );
            }
        }
    }
}
