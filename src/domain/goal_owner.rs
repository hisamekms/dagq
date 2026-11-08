//! Who owns a goal and a task in the team version (ADR-t3989-1 decisions 1
//! and 2): a goal records one person, a task in a goal records none and
//! inherits its goal's, and only a task without a goal records its own.
//! Where no OIDC issuer is configured nothing is recorded and the host's user
//! is everyone's owner. The rules here read and move the record; who becomes
//! an owner at creation and who may change one are the callers'.

use serde::{Deserialize, Serialize};

use super::DomainError;
use super::oidc::OidcIdentity;

/// One registered person as an owner: the OIDC (issuer, subject) pair of
/// the members registry, never a display name. Stored as the pair of
/// `owner_issuer` and `owner_subject`, both set or both NULL; printed as
/// `{"issuer", "subject"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Owner(OidcIdentity);

impl Owner {
    pub fn new(identity: OidcIdentity) -> Self {
        Self(identity)
    }

    pub fn identity(&self) -> &OidcIdentity {
        &self.0
    }

    /// The owner a stored pair records: none when both are NULL, refused
    /// when only one is.
    pub fn from_columns(
        issuer: Option<String>,
        subject: Option<String>,
    ) -> Result<Option<Self>, DomainError> {
        match (issuer, subject) {
            (Some(issuer), Some(subject)) => Ok(Some(Self(OidcIdentity { issuer, subject }))),
            (None, None) => Ok(None),
            _ => Err(DomainError::OwnerIncomplete),
        }
    }

    /// The pair to store.
    pub fn columns(owner: Option<&Self>) -> (Option<&str>, Option<&str>) {
        match owner {
            Some(owner) => (Some(&owner.0.issuer), Some(&owner.0.subject)),
            None => (None, None),
        }
    }
}

/// Where a task belongs, with what its goal records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Membership<'a> {
    /// In a goal, with the goal's recorded owner (none in the local default).
    Goal(Option<&'a Owner>),
    /// In no goal.
    NoGoal,
}

/// Whom a task belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOwner<'a> {
    /// A recorded person.
    Person(&'a Owner),
    /// Nobody is recorded: the host's user, the one owner of the local
    /// default (ADR-t3989-1 decision 2).
    HostUser,
}

impl<'a> TaskOwner<'a> {
    /// The recorded person; none for the host's user.
    pub fn person(self) -> Option<&'a Owner> {
        match self {
            Self::Person(owner) => Some(owner),
            Self::HostUser => None,
        }
    }
}

/// A task's owner: its goal's when it is in one, whatever the task's own
/// record holds; its own without a goal; the host's user when neither is
/// recorded.
pub fn task_owner<'a>(membership: Membership<'a>, own: Option<&'a Owner>) -> TaskOwner<'a> {
    let recorded = match membership {
        Membership::Goal(goal) => goal,
        Membership::NoGoal => own,
    };
    recorded.map_or(TaskOwner::HostUser, TaskOwner::Person)
}

/// The task's own owner record after it moves from `from` into a goal
/// (`into_goal`) or out of any: none in a goal, which it reads from; out of
/// a goal, the owner that goal recorded, so the task keeps whom it belonged
/// to. A goal without a recorded owner copies nothing and the record stays.
pub fn own_owner_after_move(
    from: Membership<'_>,
    own: Option<&Owner>,
    into_goal: bool,
) -> Option<Owner> {
    if into_goal {
        return None;
    }
    match from {
        Membership::Goal(Some(goal)) => Some(goal.clone()),
        Membership::Goal(None) | Membership::NoGoal => own.cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(subject: &str) -> Owner {
        Owner::new(OidcIdentity {
            issuer: "https://issuer.example".into(),
            subject: subject.into(),
        })
    }

    #[test]
    fn a_task_reads_its_goals_owner_its_own_without_a_goal_and_else_the_host_user() {
        let (goal, own) = (person("goal"), person("own"));
        assert_eq!(
            task_owner(Membership::Goal(Some(&goal)), Some(&own)),
            TaskOwner::Person(&goal)
        );
        assert_eq!(
            task_owner(Membership::Goal(Some(&goal)), None),
            TaskOwner::Person(&goal)
        );
        // The goal decides even when it records nobody.
        assert_eq!(
            task_owner(Membership::Goal(None), Some(&own)),
            TaskOwner::HostUser
        );
        assert_eq!(
            task_owner(Membership::NoGoal, Some(&own)),
            TaskOwner::Person(&own)
        );
        assert_eq!(task_owner(Membership::NoGoal, None), TaskOwner::HostUser);
        assert_eq!(task_owner(Membership::NoGoal, None).person(), None);
    }

    #[test]
    fn moving_clears_into_a_goal_and_copies_the_goals_owner_out_of_it() {
        let (from, to, own) = (person("from"), person("to"), person("own"));
        // Into a goal: the own record goes, from no goal or another goal.
        assert_eq!(
            own_owner_after_move(Membership::NoGoal, Some(&own), true),
            None
        );
        assert_eq!(
            own_owner_after_move(Membership::Goal(Some(&from)), None, true),
            None
        );
        // Moved between goals, it reads the goal it moved to.
        let after = own_owner_after_move(Membership::Goal(Some(&from)), None, true);
        assert_eq!(
            task_owner(Membership::Goal(Some(&to)), after.as_ref()),
            TaskOwner::Person(&to)
        );
        // Out of a goal: the goal's owner becomes its own.
        assert_eq!(
            own_owner_after_move(Membership::Goal(Some(&from)), None, false),
            Some(from.clone())
        );
        // A goal recording nobody copies nothing.
        assert_eq!(
            own_owner_after_move(Membership::Goal(None), None, false),
            None
        );
        assert_eq!(
            own_owner_after_move(Membership::Goal(None), Some(&own), false),
            Some(own.clone())
        );
        assert_eq!(
            own_owner_after_move(Membership::NoGoal, Some(&own), false),
            Some(own)
        );
    }

    #[test]
    fn a_pair_is_both_or_neither_and_prints_as_issuer_and_subject() {
        assert_eq!(Owner::from_columns(None, None), Ok(None));
        let owner = Owner::from_columns(Some("i".into()), Some("s".into()))
            .unwrap()
            .unwrap();
        assert_eq!(Owner::columns(Some(&owner)), (Some("i"), Some("s")));
        assert_eq!(Owner::columns(None), (None, None));
        assert_eq!(
            Owner::from_columns(Some("i".into()), None),
            Err(DomainError::OwnerIncomplete)
        );
        assert_eq!(
            Owner::from_columns(None, Some("s".into())),
            Err(DomainError::OwnerIncomplete)
        );
        assert_eq!(
            serde_json::to_value(&owner).unwrap(),
            serde_json::json!({"issuer": "i", "subject": "s"})
        );
    }
}
