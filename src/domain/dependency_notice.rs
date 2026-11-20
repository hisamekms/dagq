//! Decisions for a newly added dependency between recorded owners.
use super::{ActorRole, Priority, goal_owner::Owner};

/// Whether another owner needs a notice, and whether the waiting task has
/// a higher effective priority. Callers resolve task/goal inheritance first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DependencyNotice {
    pub notify: bool,
    pub priority_inversion: bool,
}

/// A planner must attach a recommendation to a new cross-owner edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecommendationRequired;

impl std::fmt::Display for RecommendationRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a planner must recommend a cross-owner dependency: create the task without this dependency, then use dagq dependency add TASK PREDECESSOR (or --goal GOAL) --reason 'why this dependency is needed'")
    }
}
impl std::error::Error for RecommendationRequired {}

impl DependencyNotice {
    /// The reason to retain in this notice, or none when no notice is due.
    /// Whitespace alone is missing. Only planners need a reason; applying
    /// a plan-review verdict (`require_reason = false`) is exempt.
    pub fn recommendation(
        self,
        role: ActorRole,
        reason: Option<&str>,
        require_reason: bool,
    ) -> Result<Option<&str>, RecommendationRequired> {
        if !self.notify {
            return Ok(None);
        }
        let reason = reason.filter(|reason| !reason.trim().is_empty());
        if require_reason && role == ActorRole::Planner && reason.is_none() {
            return Err(RecommendationRequired);
        }
        Ok(reason)
    }
}

pub fn dependency_notice(
    waiting: Option<&Owner>,
    priority: Priority,
    waited: Option<&Owner>,
    waited_priority: Priority,
) -> DependencyNotice {
    DependencyNotice {
        notify: matches!((waiting, waited), (Some(a), Some(b)) if a != b),
        priority_inversion: priority > waited_priority,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::oidc::OidcIdentity;

    #[test]
    fn only_a_planner_adding_a_noticed_edge_needs_a_nonblank_reason() {
        let notice = DependencyNotice {
            notify: true,
            priority_inversion: false,
        };
        for (role, reason, require_reason, expected) in [
            (ActorRole::Planner, None, true, Err(RecommendationRequired)),
            (
                ActorRole::Planner,
                Some(" \n\t\u{2003}"),
                true,
                Err(RecommendationRequired),
            ),
            (
                ActorRole::Planner,
                Some(" prerequisite "),
                true,
                Ok(Some(" prerequisite ")),
            ),
            (ActorRole::User, None, true, Ok(None)),
            (ActorRole::Inbox, None, true, Ok(None)),
            (ActorRole::User, Some(" \n\t"), true, Ok(None)),
            (
                ActorRole::User,
                Some("prerequisite"),
                true,
                Ok(Some("prerequisite")),
            ),
            (ActorRole::Planner, None, false, Ok(None)),
            (ActorRole::Planner, Some(" \n\t"), false, Ok(None)),
        ] {
            assert_eq!(
                notice.recommendation(role, reason, require_reason),
                expected
            );
        }
        assert_eq!(
            RecommendationRequired.to_string(),
            "a planner must recommend a cross-owner dependency: create the task without this dependency, then use dagq dependency add TASK PREDECESSOR (or --goal GOAL) --reason 'why this dependency is needed'"
        );
    }

    #[test]
    fn unnoticed_edges_discard_reasons_without_requiring_them() {
        let notice = DependencyNotice {
            notify: false,
            priority_inversion: true,
        };
        for role in [ActorRole::User, ActorRole::Inbox, ActorRole::Planner] {
            for reason in [None, Some(" \n\t"), Some("discarded")] {
                for require_reason in [false, true] {
                    assert_eq!(
                        notice.recommendation(role, reason, require_reason),
                        Ok(None)
                    );
                }
            }
        }
    }

    #[test]
    fn recorded_owners_and_effective_priorities_decide_the_notice() {
        let owner = |subject: &str| {
            Owner::new(OidcIdentity {
                issuer: "issuer".into(),
                subject: subject.into(),
            })
        };
        let (a, b) = (owner("a"), owner("b"));
        for (waiting, waited, notify) in [
            (Some(&a), Some(&b), true),
            (Some(&a), Some(&a), false),
            (None, Some(&b), false),
            (Some(&a), None, false),
            (None, None, false),
        ] {
            for (priority, inversion) in [
                (Priority::High, true),
                (Priority::Normal, false),
                (Priority::Low, false),
            ] {
                assert_eq!(
                    dependency_notice(waiting, priority, waited, Priority::Normal),
                    DependencyNotice {
                        notify,
                        priority_inversion: inversion
                    }
                );
            }
        }
    }
}
