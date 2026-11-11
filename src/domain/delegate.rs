//! The people who answer for an owner in the team version (ADR-t3989-1
//! decision 3): each owner keeps zero or more registered people, its
//! delegates, whom the asks of its goals and tasks reach as well. Only the
//! owner chooses them: the caller is always the person its token names (an
//! inbox the person its human parent is, ADR-t3985-1 decision 6), never one
//! a request names, and no AI actor or runner changes them. Where no OIDC
//! issuer is configured there are no delegates (decision 2), and the use
//! case refuses with its reason and records nothing. The decisions here
//! are pure; the store checks the registry and writes in one transaction.

use std::fmt;

use serde::Serialize;

use super::authorization::{
    AuthorizationError, Authorizer, Capability, DenyReason, Resource, StaticPolicy,
};
use super::goal_owner::Owner;
use super::oidc::OidcIdentity;
use super::queue_service::Member;
use super::{ActorContext, ActorRole};

/// A change of an owner's delegates: one person added or removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegateChange {
    Add(OidcIdentity),
    Remove(OidcIdentity),
}

impl DelegateChange {
    /// The person the change adds or removes.
    pub fn delegate(&self) -> &OidcIdentity {
        match self {
            Self::Add(delegate) | Self::Remove(delegate) => delegate,
        }
    }

    /// The change as its event and its answer name it.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Add(_) => "add",
            Self::Remove(_) => "remove",
        }
    }
}

/// What a change did to an owner's delegates, as the use case answers it
/// and its `delegates_changed` event records it (the event's actor is who
/// changed them): the owner, the change and its person, whether it changed
/// anything, and the delegates before and after, each sorted by issuer and
/// subject. A change that changes nothing (an add of one there already, a
/// remove of one not there) writes neither a row nor an event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DelegatesChanged {
    pub owner: Owner,
    pub change: &'static str,
    pub delegate: OidcIdentity,
    pub changed: bool,
    pub before: Vec<OidcIdentity>,
    pub after: Vec<OidcIdentity>,
}

/// Apply `change` to `before`, the delegates `owner` has.
pub fn change_delegates(
    owner: &Owner,
    before: &[OidcIdentity],
    change: &DelegateChange,
) -> DelegatesChanged {
    let mut before = before.to_vec();
    before.sort_by(order);
    before.dedup();
    let delegate = change.delegate();
    let mut after = before.clone();
    match change {
        DelegateChange::Add(_) if !after.contains(delegate) => {
            after.push(delegate.clone());
            after.sort_by(order);
        }
        DelegateChange::Remove(_) => after.retain(|kept| kept != delegate),
        DelegateChange::Add(_) => {}
    }
    DelegatesChanged {
        owner: owner.clone(),
        change: change.as_str(),
        delegate: delegate.clone(),
        changed: after != before,
        before,
        after,
    }
}

/// The order delegates are read and recorded in: by issuer, then subject.
pub fn order(a: &OidcIdentity, b: &OidcIdentity) -> std::cmp::Ordering {
    (&a.issuer, &a.subject).cmp(&(&b.issuer, &b.subject))
}

/// Why a change of delegates is refused, with nothing written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegateRefusal {
    /// The deployment is not the team version (no OIDC issuer configured),
    /// or the policy does not know it.
    NotTeamVersion,
    /// The person to add or remove is not in the registry.
    Unregistered,
    /// The person to add or remove is in the registry but disabled.
    Disabled,
    /// The person to add or remove is the owner.
    Oneself,
}

impl fmt::Display for DelegateRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotTeamVersion => {
                "delegates exist only in the team version (a deployment with an OIDC issuer); \
                 here every ask reaches the one inbox and nothing was recorded"
            }
            Self::Unregistered => "the delegate is not a registered person; nothing changed",
            Self::Disabled => "the delegate's registration is disabled; nothing changed",
            Self::Oneself => "an owner answers its own asks and is no delegate of its own",
        })
    }
}

impl std::error::Error for DelegateRefusal {}

/// Whether `change` may be made to `owner`'s delegates, `member` the
/// registry's row of its person as the writing transaction reads it. Both
/// an add and a remove need a registered person who is not disabled, other
/// than the owner.
pub fn check_delegate(
    owner: &Owner,
    change: &DelegateChange,
    member: Option<&Member>,
) -> Result<(), DelegateRefusal> {
    if change.delegate() == owner.identity() {
        return Err(DelegateRefusal::Oneself);
    }
    match member {
        None => Err(DelegateRefusal::Unregistered),
        Some(member) if member.disabled_at.is_some() => Err(DelegateRefusal::Disabled),
        Some(_) => Ok(()),
    }
}

/// Why a caller does not get to an owner's delegates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegateDenial {
    /// The policy refused the caller, recorded as `authorization_denied`.
    Denied(AuthorizationError),
    /// The use case refused, with its reason and no record.
    Refused(DelegateRefusal),
}

/// The owner whose delegates `actor` reads and changes, `team_version` the
/// policy's deployment ([`Authorizer::team_version`]). A role without
/// `delegate.write` (every AI actor, a runner) is refused first; then a
/// deployment that is not known to be the team version; then a caller with
/// no person: the person an HTTP-authenticated user is, or an inbox's human
/// parent. A local user and an inbox whose parent is not known have none
/// and are refused as the policy refuses an unknown owner. Nothing a
/// request carries names the owner.
pub fn delegating_owner(
    actor: &ActorContext,
    team_version: Option<bool>,
) -> Result<Owner, DelegateDenial> {
    StaticPolicy
        .authorize(actor, Capability::DelegateWrite, &Resource::Queue)
        .map_err(DelegateDenial::Denied)?;
    if team_version != Some(true) {
        return Err(DelegateDenial::Refused(DelegateRefusal::NotTeamVersion));
    }
    let person = match actor.role() {
        ActorRole::User => actor.human_identity(),
        ActorRole::Inbox => actor.parent_identity().cloned(),
        _ => None,
    };
    person
        .map(Owner::new)
        .ok_or(DelegateDenial::Denied(AuthorizationError {
            role: actor.role(),
            capability: Capability::DelegateWrite,
            reason: DenyReason::Resource,
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::actor::HumanRole;
    use crate::domain::queue_service::{Principal, TeamRole};

    fn person(subject: &str) -> OidcIdentity {
        OidcIdentity {
            issuer: "https://issuer.example".into(),
            subject: subject.into(),
        }
    }

    fn member(subject: &str, role: TeamRole, disabled_at: Option<i64>) -> Member {
        Member {
            identity: person(subject),
            role,
            on_call: false,
            display_name: None,
            created_at: 1,
            disabled_at,
        }
    }

    /// The actor the service authenticates a principal as, with the
    /// registry row it reads on the call.
    fn authenticated(principal: Principal, row: &Member) -> ActorContext {
        principal
            .with_current_member(Some(100), Some(row), 0)
            .expect("a live registered person")
            .actor()
    }

    fn denied(role: ActorRole, reason: DenyReason) -> Result<Owner, DelegateDenial> {
        Err(DelegateDenial::Denied(AuthorizationError {
            role,
            capability: Capability::DelegateWrite,
            reason,
        }))
    }

    /// A person changes its own delegates, of either registry role; an
    /// inbox with a human parent acts as that parent; an inbox whose parent
    /// is not known, a local user, and every AI actor and the runner are
    /// refused as `authorization_denied`. The owner comes from the actor
    /// alone: the function has no input a request could name another with.
    #[test]
    fn only_a_person_or_its_inbox_changes_that_persons_delegates() {
        for role in [TeamRole::Admin, TeamRole::Member] {
            let alice = member("alice", role, None);
            let user = authenticated(Principal::human(person("alice")), &alice);
            assert_eq!(
                delegating_owner(&user, Some(true)),
                Ok(Owner::new(person("alice")))
            );
            let inbox = authenticated(Principal::human_inbox(person("alice")), &alice);
            assert_eq!(inbox.role(), ActorRole::Inbox);
            assert_eq!(
                delegating_owner(&inbox, Some(true)),
                Ok(Owner::new(person("alice")))
            );
        }
        // An inbox of a host session has no parent, nor has the local user.
        let orphan = ActorContext::new(ActorRole::Inbox, "inbox");
        assert_eq!(
            delegating_owner(&orphan, Some(true)),
            denied(ActorRole::Inbox, DenyReason::Resource)
        );
        assert_eq!(
            delegating_owner(&ActorContext::user(), Some(true)),
            denied(ActorRole::User, DenyReason::Resource)
        );
        // Registry attributes alone, with no person to name, are no owner.
        let attributed = ActorContext::new(ActorRole::Inbox, "inbox").with_human_role(HumanRole {
            role: TeamRole::Admin,
            on_call: true,
        });
        assert_eq!(
            delegating_owner(&attributed, Some(true)),
            denied(ActorRole::Inbox, DenyReason::Resource)
        );
        for role in ActorRole::ALL {
            if matches!(role, ActorRole::User | ActorRole::Inbox) {
                continue;
            }
            let actor = ActorContext::new(role, format!("{}:1", role.as_str()));
            for team in [Some(true), Some(false), None] {
                assert_eq!(
                    delegating_owner(&actor, team),
                    denied(role, DenyReason::NotGranted),
                    "{role:?}"
                );
            }
        }
    }

    /// Where the deployment is local or unknown, the person and the inbox
    /// are refused with the reason, which records nothing, before any
    /// person is read.
    #[test]
    fn outside_the_team_version_the_use_case_refuses_with_its_reason() {
        let alice = member("alice", TeamRole::Member, None);
        for actor in [
            ActorContext::user(),
            ActorContext::new(ActorRole::Inbox, "inbox"),
            authenticated(Principal::human(person("alice")), &alice),
        ] {
            for team in [Some(false), None] {
                assert_eq!(
                    delegating_owner(&actor, team),
                    Err(DelegateDenial::Refused(DelegateRefusal::NotTeamVersion))
                );
            }
        }
        assert!(
            DelegateRefusal::NotTeamVersion
                .to_string()
                .contains("team version")
        );
    }

    /// An add and a remove both need a registered, enabled person other
    /// than the owner.
    #[test]
    fn a_delegate_is_a_registered_enabled_person_other_than_the_owner() {
        let owner = Owner::new(person("alice"));
        let bob = person("bob");
        let enabled = member("bob", TeamRole::Member, None);
        let disabled = member("bob", TeamRole::Admin, Some(5));
        for change in [
            DelegateChange::Add(bob.clone()),
            DelegateChange::Remove(bob),
        ] {
            assert_eq!(check_delegate(&owner, &change, Some(&enabled)), Ok(()));
            assert_eq!(
                check_delegate(&owner, &change, None),
                Err(DelegateRefusal::Unregistered)
            );
            assert_eq!(
                check_delegate(&owner, &change, Some(&disabled)),
                Err(DelegateRefusal::Disabled)
            );
        }
        let me = member("alice", TeamRole::Admin, None);
        for change in [
            DelegateChange::Add(person("alice")),
            DelegateChange::Remove(person("alice")),
        ] {
            assert_eq!(
                check_delegate(&owner, &change, Some(&me)),
                Err(DelegateRefusal::Oneself)
            );
        }
    }

    /// An add puts the person in, sorted, once; a remove takes it out; an
    /// add of one there and a remove of one not there change nothing.
    #[test]
    fn a_change_reports_the_delegates_before_and_after() {
        let owner = Owner::new(person("alice"));
        let (bob, carol) = (person("bob"), person("carol"));
        let added = change_delegates(
            &owner,
            &[person("carol")],
            &DelegateChange::Add(bob.clone()),
        );
        assert!(added.changed);
        assert_eq!(added.before, [person("carol")]);
        assert_eq!(added.after, [bob.clone(), carol.clone()]);
        assert_eq!((added.change, &added.delegate), ("add", &bob));
        let again = change_delegates(&owner, &added.after, &DelegateChange::Add(bob.clone()));
        assert!(!again.changed);
        assert_eq!(again.after, again.before);
        let removed = change_delegates(&owner, &added.after, &DelegateChange::Remove(bob.clone()));
        assert!(removed.changed);
        assert_eq!(removed.after, [person("carol")]);
        let absent = change_delegates(&owner, &[carol], &DelegateChange::Remove(bob));
        assert!(!absent.changed);
        assert_eq!(
            serde_json::to_value(&removed).unwrap()["owner"],
            serde_json::json!({"issuer": "https://issuer.example", "subject": "alice"})
        );
    }
}
