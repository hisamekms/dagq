//! A person's retry that carries a run's branch over (`ready --inherit`,
//! ADR-t1962-1): whether its preconditions hold, judged from what the
//! store read in the transaction that applies it.

use std::fmt;

use super::{RunStatus, TaskStatus, actor::ActorRole};

/// What the store read about the task and its latest run, in the
/// transaction that would carry the run over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InheritFacts {
    /// Who asks for it.
    pub role: ActorRole,
    pub task: TaskStatus,
    /// The status of the task's latest run; `None` when it has none.
    pub latest_run: Option<RunStatus>,
    /// The run has a lease that is not stale: a supervisor or a recovery
    /// job holds it.
    pub leased: bool,
    /// A process of the run is still registered and heartbeating.
    pub process_alive: bool,
    /// The run's branch holds a commit on top of its base (the commit
    /// `inherited_head` would keep).
    pub own_commits: bool,
}

/// Why a retry by hand that carries the branch over is refused; nothing is
/// changed then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InheritRefusal {
    /// Only a person (user) or the inbox at their word carries a run over
    /// by hand.
    Actor(ActorRole),
    /// The task is not `in_progress`.
    Task(TaskStatus),
    /// The task has no run.
    NoRun,
    /// The latest run is neither `failed` nor `interrupted`.
    Run(RunStatus),
    /// A lease that is not stale holds the run (a recovery job's round).
    Leased,
    /// A process of the run is still alive.
    ProcessAlive,
    /// Nothing on the branch to carry over: a plain `ready` retries.
    NoOwnCommits,
}

impl fmt::Display for InheritRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Actor(role) => write!(
                f,
                "only user or inbox carries a run over by hand, not {}",
                role.as_str()
            ),
            Self::Task(status) => write!(
                f,
                "the task is {}; only an in_progress task whose latest run failed or was interrupted is carried over",
                status.as_str()
            ),
            Self::NoRun => f.write_str("the task has no run to carry over"),
            Self::Run(status) => write!(
                f,
                "its latest run is {}, not failed or interrupted",
                status.as_str()
            ),
            Self::Leased => f.write_str(
                "its latest run is leased (a recovery job's round holds it); wait for the round to end and try again",
            ),
            Self::ProcessAlive => f.write_str("a process of its latest run is still alive"),
            Self::NoOwnCommits => f.write_str(
                "its latest run's branch holds no commit of its own to carry over; use a plain `dagq ready` to retry from the current main",
            ),
        }
    }
}

impl std::error::Error for InheritRefusal {}

/// Whether the retry by hand that carries the latest run's branch over may
/// go on: the first precondition that does not hold, in the order the
/// variants of [`InheritRefusal`] list them. It has no once-per-task limit:
/// the automatic `retry_inherit`'s does not apply to a person's call.
pub fn check(facts: InheritFacts) -> Result<(), InheritRefusal> {
    if !matches!(facts.role, ActorRole::User | ActorRole::Inbox) {
        return Err(InheritRefusal::Actor(facts.role));
    }
    if facts.task != TaskStatus::InProgress {
        return Err(InheritRefusal::Task(facts.task));
    }
    let run = facts.latest_run.ok_or(InheritRefusal::NoRun)?;
    if !matches!(run, RunStatus::Failed | RunStatus::Interrupted) {
        return Err(InheritRefusal::Run(run));
    }
    if facts.leased {
        return Err(InheritRefusal::Leased);
    }
    if facts.process_alive {
        return Err(InheritRefusal::ProcessAlive);
    }
    if !facts.own_commits {
        return Err(InheritRefusal::NoOwnCommits);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn carried() -> InheritFacts {
        InheritFacts {
            role: ActorRole::Inbox,
            task: TaskStatus::InProgress,
            latest_run: Some(RunStatus::Failed),
            leased: false,
            process_alive: false,
            own_commits: true,
        }
    }

    #[test]
    fn a_failed_or_interrupted_run_with_commits_is_carried_over_by_user_or_inbox() {
        for role in [ActorRole::User, ActorRole::Inbox] {
            for run in [RunStatus::Failed, RunStatus::Interrupted] {
                let facts = InheritFacts {
                    role,
                    latest_run: Some(run),
                    ..carried()
                };
                assert_eq!(check(facts), Ok(()), "{role:?} {run:?}");
            }
        }
    }

    #[test]
    fn another_actor_is_refused() {
        for role in [
            ActorRole::Worker,
            ActorRole::Planner,
            ActorRole::Supervisor,
            ActorRole::RecoveryJob,
            ActorRole::Observer,
        ] {
            let facts = InheritFacts { role, ..carried() };
            assert_eq!(check(facts), Err(InheritRefusal::Actor(role)));
        }
    }

    #[test]
    fn a_task_not_in_progress_or_without_a_run_is_refused() {
        for task in [TaskStatus::Ready, TaskStatus::Draft, TaskStatus::Completed] {
            let facts = InheritFacts { task, ..carried() };
            assert_eq!(check(facts), Err(InheritRefusal::Task(task)));
        }
        let facts = InheritFacts {
            latest_run: None,
            ..carried()
        };
        assert_eq!(check(facts), Err(InheritRefusal::NoRun));
    }

    #[test]
    fn a_latest_run_that_did_not_fail_is_refused() {
        for run in [
            RunStatus::Running,
            RunStatus::NeedsSession,
            RunStatus::AwaitingIntegration,
            RunStatus::Integrated,
        ] {
            let facts = InheritFacts {
                latest_run: Some(run),
                ..carried()
            };
            assert_eq!(check(facts), Err(InheritRefusal::Run(run)));
        }
    }

    #[test]
    fn a_live_lease_or_process_or_a_branch_without_commits_is_refused() {
        let leased = InheritFacts {
            leased: true,
            ..carried()
        };
        assert_eq!(check(leased), Err(InheritRefusal::Leased));
        let alive = InheritFacts {
            process_alive: true,
            ..carried()
        };
        assert_eq!(check(alive), Err(InheritRefusal::ProcessAlive));
        let empty = InheritFacts {
            own_commits: false,
            ..carried()
        };
        assert_eq!(check(empty), Err(InheritRefusal::NoOwnCommits));
        assert!(
            InheritRefusal::NoOwnCommits
                .to_string()
                .contains("plain `dagq ready`")
        );
    }
}
