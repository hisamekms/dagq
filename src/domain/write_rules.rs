//! The rules a SQLite CHECK held that no domain type expresses
//! (ADR-t876-1): the write port checks them before it writes, and a broken
//! one is an error instead of a row. Rules a type holds (a `string_enum!`
//! column, a validated input) are not repeated here; the list of every
//! CHECK and where its rule lives is in `docs/design/persistence.md`.

use super::{DomainError, RunId, TaskId, error::require};

/// A row about a run names its task too (`asks`, `run_events`): a run
/// belongs to one task.
pub fn check_run_has_task(
    task_id: Option<TaskId>,
    run_id: Option<&RunId>,
) -> Result<(), DomainError> {
    match run_id {
        Some(run_id) => require(task_id.is_some(), || DomainError::RunWithoutTask {
            run_id: run_id.clone(),
        }),
        None => Ok(()),
    }
}

/// A text column that must hold something other than whitespace.
pub fn check_non_blank(field: &'static str, value: &str) -> Result<(), DomainError> {
    require(!value.trim().is_empty(), || DomainError::Blank { field })
}

/// A number column with a least value (an attempt, a count, a floor).
pub fn check_at_least(field: &'static str, value: i64, min: i64) -> Result<(), DomainError> {
    require(value >= min, || DomainError::BelowMinimum {
        field,
        min,
        value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        AskReason, FindingStatus, GoalStatus, GoalVerdict, Impact, PlannerOrigin, Priority,
        ProposalStatus, SupervisorMode, TaskStatus,
        follow_up::DraftOrigin,
        plan_review::{PlanReviewOutcome, ReviewHold},
        worker::WorkerMode,
    };

    fn run() -> RunId {
        RunId::new("run-1").unwrap()
    }

    #[test]
    fn a_row_about_a_run_is_refused_without_its_task() {
        let run = run();
        assert_eq!(
            check_run_has_task(None, Some(&run)),
            Err(DomainError::RunWithoutTask {
                run_id: run.clone()
            })
        );
        assert!(check_run_has_task(Some(TaskId::new(1)), Some(&run)).is_ok());
        assert!(check_run_has_task(Some(TaskId::new(1)), None).is_ok());
        assert!(check_run_has_task(None, None).is_ok());
    }

    #[test]
    fn a_blank_text_is_refused() {
        for blank in ["", " ", "\t\n"] {
            assert_eq!(
                check_non_blank("outcome", blank),
                Err(DomainError::Blank { field: "outcome" })
            );
        }
        assert!(check_non_blank("outcome", "ended").is_ok());
    }

    #[test]
    fn a_number_below_its_least_value_is_refused() {
        assert_eq!(
            check_at_least("attempt", 0, 1),
            Err(DomainError::BelowMinimum {
                field: "attempt",
                min: 1,
                value: 0
            })
        );
        assert_eq!(
            check_at_least("attempt", 0, 1).unwrap_err().to_string(),
            "attempt must be at least 1, not 0"
        );
        assert!(check_at_least("attempt", 1, 1).is_ok());
        assert!(check_at_least("attempt", 0, 0).is_ok());
    }

    /// Every column whose CHECK listed its values is written from a
    /// `string_enum!`, whose parse refuses a value outside the list, so the
    /// port cannot write one.
    #[test]
    fn each_listed_column_refuses_a_value_outside_its_list() {
        fn refuses<T: std::str::FromStr<Err = DomainError>>(known: &[&str]) {
            for value in known {
                assert!(value.parse::<T>().is_ok(), "{value}");
            }
            for value in ["", "bogus", "Draft"] {
                assert!(
                    matches!(value.parse::<T>(), Err(DomainError::UnknownValue { .. })),
                    "{value}"
                );
            }
        }
        refuses::<TaskStatus>(&[
            "draft",
            "submitted",
            "ready",
            "in_progress",
            "completed",
            "canceled",
        ]);
        refuses::<crate::domain::Provider>(&["claude", "codex"]);
        refuses::<WorkerMode>(&["interactive", "headless"]);
        refuses::<GoalStatus>(&["draft", "open"]);
        refuses::<GoalVerdict>(&["achieved", "abandoned"]);
        refuses::<ProposalStatus>(&["submitted", "revising", "accepted", "canceled"]);
        refuses::<PlannerOrigin>(&["person", "runtime"]);
        refuses::<ReviewHold>(&["failed", "concern"]);
        refuses::<PlanReviewOutcome>(&["pass", "revise", "concern", "failed", "interrupted"]);
        refuses::<AskReason>(&[
            "authentication",
            "cost",
            "scope",
            "discard",
            "recovery_failed",
        ]);
        refuses::<Impact>(&["high", "normal", "low"]);
        refuses::<FindingStatus>(&["open", "proposed", "resolved", "dismissed"]);
        refuses::<SupervisorMode>(&["launchd", "in_cmux"]);
        // `reopened` is a draft origin the table does not take: the port
        // refuses it before the write (`draft_planners::record_draft_origin`).
        refuses::<DraftOrigin>(&["follow_up", "goal_gap", "reopened"]);
        // A priority is stored as 0 to 4 and read back only from that range.
        assert!(Priority::from_i64(-1).is_err());
        assert!(Priority::from_i64(5).is_err());
        for n in 0..=4 {
            assert_eq!(Priority::from_i64(n).unwrap().as_i64(), n);
        }
    }
}
