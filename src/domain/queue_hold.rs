//! The wait and the answer of the authentication and usage-limit
//! `queue_hold` asks (ADR-0047 decision 42, task 437). While one is open
//! the supervisor claims no new run and starts no headless job, since each
//! would stop at the same wall; the runs in flight keep their leases. The
//! runtime applies the answer: `done` ends the hold, types the fixed
//! [`CONTINUE_TEXT`] into the held sessions and starts again the jobs that
//! failed while the ask was open; `cancel_affected` gives the held runs up
//! as an abandon does. The disk's `cost` ask (`subject: disk`, task 377)
//! is not one: it holds the claims and landings its own way.

use super::EventKind;
use super::claim_hold::{HoldReason, QueueHold};
use super::{Ask, AskKind, AskReason, GoalId, ProposalId, RunId, disk::DISK_SUBJECT};

/// The `subject` of the `cost` ask about Claude Code's usage limit.
pub const USAGE_LIMIT_SUBJECT: &str = "usage_limit";

/// The answer that ends the hold: a person logged in, or the limit is back.
pub const DONE: &str = "done";
/// The answer that gives the held runs up.
pub const CANCEL_AFFECTED: &str = "cancel_affected";

/// Recorded on the queue when the supervisor applied the answer of a
/// `queue_hold` ask (`ask_id`, `answer`, `reason_category`, `subject`,
/// `continued`, `released`, `moved_on`, `restarted`, `elsewhere`,
/// `unwatched`, `runs`, `jobs`, `supervisor`).
pub const QUEUE_HOLD_APPLIED: &str = super::event_kind::QUEUE_HOLD_APPLIED;
/// Recorded on a held run by the supervisor whose slot has it when that
/// supervisor applied the answer of a `queue_hold` ask to it (task 754):
/// `ask_id`, `answer`, `outcome` (`continued`, `released` or `moved_on`)
/// and `supervisor`. Every supervisor applies an answer to the runs it
/// watches, once each, and the ask closes when every run in it was.
pub const HOLD_ANSWER_APPLIED: &str = super::event_kind::HOLD_ANSWER_APPLIED;
/// Recorded on a held run when the fixed text to go on was typed into its
/// session after `done` (`ask_id`, `workspace_id`, `submitted`).
pub const HOLD_CONTINUE_SENT: &str = super::event_kind::HOLD_CONTINUE_SENT;
/// Recorded on a `failed` or `interrupted` run whose recovery job (the
/// triage) failed while the ask was open, when the runtime has it recovered
/// again after `done` (`job: triage`, `ask_id`, the failure's `event_id`):
/// the run's triage is due again ([`super::triage_state`]).
pub const JOB_RESTARTED: &str = super::event_kind::JOB_RESTARTED;

/// The fixed text typed into a session held by the ask once a person
/// answered `done` (the send is checked as every typed text is: ADR-0047
/// decision 31).
pub const CONTINUE_TEXT: &str = "The login or the usage limit that stopped this session is fixed now (a person answered the queue's hold ask `done`). Continue the task from where you stopped: run again what failed with the authentication or usage-limit error, then commit and write the receipt as the task says.";

/// The question of the authentication ask; the runs and jobs it holds
/// follow it.
pub const AUTH_QUESTION: &str = "Claude Code's login ran out: worker sessions or headless jobs stopped at an authentication error (`Please run /login`, an API 401). Only a person can log in again: run `claude` in a terminal, `/login`, and answer `done`; the supervisor then tells each held session to go on and starts again the headless jobs that failed at the login. Answer `cancel_affected` to give the held runs up instead (the supervisor releases them as an abandon does, keeping their worktrees, and the inbox recovers them). Until then no new run is claimed and no headless job (review, recovery, plan review, goal review, observer) starts; the runs in flight keep their leases. More runs and jobs that stop at the login join this ask instead of opening another.";

/// The question of the usage-limit `cost` ask; the runs and jobs it holds
/// follow it.
pub const USAGE_LIMIT_QUESTION: &str = "Claude Code reached its usage limit: worker sessions or headless jobs stopped at it (`usage limit reached`, `limit reached · resets ...`). Only a person can decide on the cost: wait for the limit to reset or raise it, and answer `done`; the supervisor then tells each held session to go on and starts again the headless jobs that failed at the limit. Answer `cancel_affected` to give the held runs up instead (the supervisor releases them as an abandon does, keeping their worktrees, and the inbox recovers them). Until then no new run is claimed and no headless job (review, recovery, plan review, goal review, observer) starts; the runs in flight keep their leases. More runs and jobs that stop at the limit join this ask instead of opening another.";

/// A wall only a person moves that stopped a worker's session or a
/// headless job (ADR-0047 decision 42): a login that ran out, or Claude
/// Code's usage limit. The provider's adapter reads it from a screen or a
/// job's output (`infrastructure::claude`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wall {
    Authentication,
    UsageLimit,
}

impl Wall {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::UsageLimit => "usage_limit",
        }
    }
    /// The `reason_category` of its `queue_hold` ask.
    pub fn reason(self) -> AskReason {
        match self {
            Self::Authentication => AskReason::Authentication,
            Self::UsageLimit => AskReason::Cost,
        }
    }
    /// The `subject` of its `queue_hold` ask.
    pub fn subject(self) -> Option<&'static str> {
        match self {
            Self::Authentication => None,
            Self::UsageLimit => Some(USAGE_LIMIT_SUBJECT),
        }
    }
    /// The event recorded on what hit it: `auth_required` or
    /// `usage_limited`.
    pub fn event_kind(self) -> EventKind {
        match self {
            Self::Authentication => EventKind::AuthRequired,
            Self::UsageLimit => EventKind::UsageLimited,
        }
    }
    pub fn question(self) -> &'static str {
        match self {
            Self::Authentication => AUTH_QUESTION,
            Self::UsageLimit => USAGE_LIMIT_QUESTION,
        }
    }
}

/// A headless job a hold ask lists in its `affected` next to the runs
/// (ADR-0047 decision 42, task 438): its entry names its kind and what it
/// is about, and has a space, which no run ID has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldJob {
    Review(RunId),
    Recovery(RunId),
    PlanReview(ProposalId),
    GoalReview(GoalId),
    Observer,
}

impl HoldJob {
    /// Its kind, as `headless_jobs.kind` names it.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Review(_) => "review",
            Self::Recovery(_) => "recovery",
            Self::PlanReview(_) => "plan_review",
            Self::GoalReview(_) => "goal_review",
            Self::Observer => "observer",
        }
    }
    /// The run it is about, if any.
    pub fn run_id(&self) -> Option<&RunId> {
        match self {
            Self::Review(run) | Self::Recovery(run) => Some(run),
            _ => None,
        }
    }
    /// Its entry in the ask's `affected` and question.
    pub fn entry(&self) -> String {
        match self {
            Self::Review(run) => format!("review job of run {run}"),
            Self::Recovery(run) => format!("recovery job of run {run}"),
            Self::PlanReview(proposal) => format!("plan_review job of proposal {proposal}"),
            Self::GoalReview(goal) => format!("goal_review job of goal {goal}"),
            Self::Observer => "observer job".to_owned(),
        }
    }
}

/// Whether an entry of a hold ask's `affected` is a run (not a job).
pub fn is_run_entry(entry: &str) -> bool {
    !entry.contains(' ')
}

/// The runs of a hold ask's `affected`, without its jobs.
pub fn affected_runs(ask: &Ask) -> impl Iterator<Item = &String> {
    ask.affected.iter().filter(|entry| is_run_entry(entry))
}

/// The hold of `ask`, when it is an open authentication or usage-limit
/// `queue_hold` ask; the disk's `cost` ask is none.
pub fn hold_of(ask: &Ask) -> Option<QueueHold> {
    let reason = reason_of(ask)?;
    ask.is_open().then(|| QueueHold {
        reason,
        ask_id: ask.id.as_i64(),
        affected: affected_runs(ask).count(),
    })
}

/// Whether one of the unclosed (open, or answered and not applied yet)
/// authentication or usage-limit asks in `asks` lists `job`: its failure
/// waits for that ask, and is no attention of its own meanwhile.
pub fn job_held(asks: &[Ask], job: &HoldJob) -> bool {
    let entry = job.entry();
    asks.iter().any(|ask| {
        ask.closed_at.is_none() && reason_of(ask).is_some() && ask.affected.contains(&entry)
    })
}

/// Whether `ask` is an authentication or usage-limit `queue_hold` ask,
/// open or not, and which hold it is.
pub fn reason_of(ask: &Ask) -> Option<HoldReason> {
    if ask.kind != AskKind::QueueHold {
        return None;
    }
    match (ask.reason_category, ask.subject.as_deref()) {
        (AskReason::Authentication, _) => Some(HoldReason::Authentication),
        (AskReason::Cost, Some(USAGE_LIMIT_SUBJECT)) => Some(HoldReason::UsageLimit),
        (AskReason::Cost, Some(DISK_SUBJECT)) => None,
        // A cost ask about something else holds like the usage limit:
        // its runs stopped at a wall only a person moves.
        (AskReason::Cost, _) => Some(HoldReason::UsageLimit),
        _ => None,
    }
}

/// Whether the runtime applies `answer` to the `queue_hold` ask with
/// `options`: one of them, as offered (the disk's `done` / `wait` too).
pub fn applies(options: &[String], answer: &str) -> bool {
    options.iter().any(|option| option == answer.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::AskId;

    fn ask(reason: AskReason, subject: Option<&str>) -> Ask {
        Ask {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            id: AskId::new(4),
            kind: AskKind::QueueHold,
            task_id: None,
            run_id: None,
            question: "q".into(),
            options: vec![DONE.into(), CANCEL_AFFECTED.into()],
            answer: None,
            asked_by: "supervisor".into(),
            reason_category: reason,
            subject: subject.map(str::to_owned),
            affected: vec!["r1".into(), "r2".into()],
            created_at: 0,
            answered_at: None,
            closed_at: None,
            finding_id: None,
            request_id: None,
            answered_by: None,
            option_index: None,
            answer_authority: None,
            answer_approval: None,
        }
    }

    #[test]
    fn authentication_and_usage_limit_asks_hold_while_open_and_the_disk_does_not() {
        let auth = ask(AskReason::Authentication, None);
        assert_eq!(
            hold_of(&auth),
            Some(QueueHold {
                reason: HoldReason::Authentication,
                ask_id: 4,
                affected: 2,
            })
        );
        let limit = ask(AskReason::Cost, Some(USAGE_LIMIT_SUBJECT));
        assert_eq!(hold_of(&limit).unwrap().reason, HoldReason::UsageLimit);
        assert_eq!(
            hold_of(&ask(AskReason::Cost, None)).unwrap().reason,
            HoldReason::UsageLimit
        );
        assert_eq!(hold_of(&ask(AskReason::Cost, Some(DISK_SUBJECT))), None);
        let answered = Ask {
            answered_at: Some(1),
            answer: Some("done".into()),
            ..auth.clone()
        };
        assert_eq!(hold_of(&answered), None);
        assert_eq!(reason_of(&answered), Some(HoldReason::Authentication));
        let other = Ask {
            kind: AskKind::Blocked,
            ..auth
        };
        assert_eq!(reason_of(&other), None);
    }

    #[test]
    fn jobs_are_listed_next_to_the_runs_and_held_until_the_ask_closes() {
        let run = RunId::new("r9").unwrap();
        let jobs = [
            HoldJob::Review(run.clone()),
            HoldJob::Recovery(run.clone()),
            HoldJob::PlanReview(ProposalId::new(3)),
            HoldJob::GoalReview(GoalId::new(5)),
            HoldJob::Observer,
        ];
        let entries: Vec<String> = jobs.iter().map(HoldJob::entry).collect();
        assert_eq!(
            entries,
            [
                "review job of run r9",
                "recovery job of run r9",
                "plan_review job of proposal 3",
                "goal_review job of goal 5",
                "observer job",
            ]
        );
        assert!(entries.iter().all(|entry| !is_run_entry(entry)));
        assert_eq!(jobs[0].run_id(), Some(&run));
        assert_eq!(jobs[2].run_id(), None);
        assert_eq!(
            jobs.iter().map(HoldJob::kind).collect::<Vec<_>>(),
            [
                "review",
                "recovery",
                "plan_review",
                "goal_review",
                "observer"
            ]
        );
        let mut held = ask(AskReason::Cost, Some(USAGE_LIMIT_SUBJECT));
        held.affected.push(entries[2].clone());
        assert_eq!(affected_runs(&held).collect::<Vec<_>>(), ["r1", "r2"]);
        assert_eq!(hold_of(&held).unwrap().affected, 2);
        let asks = vec![held.clone()];
        assert!(job_held(&asks, &jobs[2]));
        assert!(!job_held(&asks, &jobs[3]));
        let answered = Ask {
            answered_at: Some(1),
            ..held.clone()
        };
        assert!(job_held(&[answered], &jobs[2]));
        let closed = Ask {
            closed_at: Some(2),
            ..held.clone()
        };
        assert!(!job_held(&[closed], &jobs[2]));
        let disk = Ask {
            subject: Some(DISK_SUBJECT.into()),
            ..held
        };
        assert!(!job_held(&[disk], &jobs[2]));
    }

    #[test]
    fn a_wall_names_its_ask_and_its_event() {
        assert_eq!(Wall::Authentication.reason(), AskReason::Authentication);
        assert_eq!(Wall::Authentication.subject(), None);
        assert_eq!(Wall::Authentication.event_kind(), EventKind::AuthRequired);
        assert_eq!(Wall::Authentication.question(), AUTH_QUESTION);
        assert_eq!(Wall::UsageLimit.reason(), AskReason::Cost);
        assert_eq!(Wall::UsageLimit.subject(), Some(USAGE_LIMIT_SUBJECT));
        assert_eq!(Wall::UsageLimit.event_kind(), EventKind::UsageLimited);
        assert_eq!(Wall::UsageLimit.question(), USAGE_LIMIT_QUESTION);
        assert_eq!(Wall::UsageLimit.as_str(), "usage_limit");
        assert_eq!(Wall::Authentication.as_str(), "authentication");
    }

    #[test]
    fn only_an_offered_option_is_applied() {
        let options = vec![DONE.to_owned(), CANCEL_AFFECTED.to_owned()];
        assert!(applies(&options, "done"));
        assert!(applies(&options, " cancel_affected\n"));
        assert!(!applies(&options, "logged in, go on"));
    }
}
