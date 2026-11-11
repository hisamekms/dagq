//! `delegate add|remove|list` (ADR-t3989-1 decision 3): the people who
//! answer for the caller's own person in the team version. Who the owner
//! is and whether the deployment has delegates at all is
//! [`crate::domain::delegate::delegating_owner`]'s; a refusal of the
//! policy is recorded as `authorization_denied`, one of the use case
//! (not the team version, a person not registered or disabled, the owner
//! itself) carries its reason and records nothing. The store checks the
//! registry and writes the row and its event in one transaction.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{DenialLog, record};
use crate::domain::delegate::{DelegateChange, DelegateDenial, DelegatesChanged, delegating_owner};
use crate::domain::goal_owner::Owner;
use crate::domain::oidc::OidcIdentity;
use crate::domain::queue_service::UseCase;
use crate::domain::{ActorContext, Resource};

/// The delegates of each owner, as the store keeps them.
pub trait DelegateStore {
    /// The delegates `owner` has, sorted by issuer and subject; none for an
    /// owner that chose none.
    fn delegates(&self, owner: &Owner) -> Result<Vec<OidcIdentity>>;
    /// Make `change` to `owner`'s delegates in one write transaction: the
    /// registry's row of its person read there and held to
    /// [`crate::domain::delegate::check_delegate`] (a refusal is the
    /// [`crate::domain::delegate::DelegateRefusal`], nothing written), then
    /// the row and its `delegates_changed` event together, or neither when
    /// the change changes nothing.
    fn change_delegate(
        &mut self,
        owner: &Owner,
        change: &DelegateChange,
    ) -> Result<DelegatesChanged>;
}

/// `delegate add` and `delegate remove`'s one option: the person, by its
/// OIDC subject under the owner's own issuer. Nothing else is read, so no
/// request names an owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegateMember {
    pub member: String,
}

/// `delegate list` has no option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoOptions {}

/// A `delegate` command as the service gets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegateRequest {
    Add(DelegateMember),
    Remove(DelegateMember),
    List,
}

/// Params of a `delegate` use case that do not read.
#[derive(Debug)]
pub struct BadDelegate(pub String);

impl std::fmt::Display for BadDelegate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadDelegate {}

impl DelegateRequest {
    /// The request of `use_case` with `params`; `None` for a use case that
    /// is no `delegate` command, and [`BadDelegate`] for params that do not
    /// read (an unknown field, such as one naming an owner, among them).
    pub fn parse(use_case: UseCase, params: &Value) -> Result<Option<Self>> {
        fn read<T: for<'de> Deserialize<'de>>(use_case: UseCase, params: &Value) -> Result<T> {
            let params = if params.is_null() {
                &Value::Object(serde_json::Map::new())
            } else {
                params
            };
            serde_json::from_value(params.clone()).map_err(|error| {
                BadDelegate(format!(
                    "the params of {} do not read: {error}",
                    use_case.as_str()
                ))
                .into()
            })
        }
        Ok(Some(match use_case {
            UseCase::DelegateAdd => Self::Add(read(use_case, params)?),
            UseCase::DelegateRemove => Self::Remove(read(use_case, params)?),
            UseCase::DelegateList => {
                read::<NoOptions>(use_case, params)?;
                Self::List
            }
            _ => return Ok(None),
        }))
    }

    /// The use case and the params the command line sends.
    pub fn request(&self) -> (UseCase, Value) {
        let params = |member: &DelegateMember| serde_json::to_value(member).expect("params JSON");
        match self {
            Self::Add(member) => (UseCase::DelegateAdd, params(member)),
            Self::Remove(member) => (UseCase::DelegateRemove, params(member)),
            Self::List => (UseCase::DelegateList, Value::Object(serde_json::Map::new())),
        }
    }
}

/// Run `request` as `actor` on `store`, `team_version` the deployment the
/// policy holds: the owner is the caller's own person
/// ([`delegating_owner`]), a refusal of the policy recorded on the store, and
/// the person a change names is under the owner's issuer. `list` answers
/// the owner and its delegates; a change answers [`DelegatesChanged`].
pub fn answer<S: DelegateStore + DenialLog>(
    store: &mut S,
    actor: &ActorContext,
    team_version: Option<bool>,
    request: &DelegateRequest,
) -> Result<Value> {
    let owner = match delegating_owner(actor, team_version) {
        Ok(owner) => owner,
        Err(DelegateDenial::Denied(error)) => {
            record(store, &error, &Resource::Queue);
            return Err(error.into());
        }
        Err(DelegateDenial::Refused(why)) => return Err(why.into()),
    };
    let person = |member: &DelegateMember| -> Result<OidcIdentity> {
        // The subject is the person's identifier as the issuer gives it,
        // taken as it is: one with blanks around it is another person.
        if member.member.is_empty() {
            return Err(BadDelegate("a delegate is named by its subject".into()).into());
        }
        Ok(OidcIdentity {
            issuer: owner.identity().issuer.clone(),
            subject: member.member.clone(),
        })
    };
    let change = match request {
        DelegateRequest::List => {
            let delegates = store.delegates(&owner)?;
            return Ok(serde_json::json!({"owner": owner, "delegates": delegates}));
        }
        DelegateRequest::Add(member) => DelegateChange::Add(person(member)?),
        DelegateRequest::Remove(member) => DelegateChange::Remove(person(member)?),
    };
    Ok(serde_json::to_value(
        store.change_delegate(&owner, &change)?,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::actor::ActorRole;
    use crate::domain::authorization::DenyReason;
    use crate::domain::delegate::{DelegateRefusal, change_delegates};
    use crate::domain::queue_service::{Member, Principal, TeamRole};
    use crate::domain::{AuthorizationError, Capability};
    use serde_json::json;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Store {
        rows: Vec<OidcIdentity>,
        changes: Vec<(Owner, DelegateChange)>,
        denials: RefCell<Vec<Value>>,
    }

    impl DelegateStore for Store {
        fn delegates(&self, _: &Owner) -> Result<Vec<OidcIdentity>> {
            Ok(self.rows.clone())
        }
        fn change_delegate(
            &mut self,
            owner: &Owner,
            change: &DelegateChange,
        ) -> Result<DelegatesChanged> {
            self.changes.push((owner.clone(), change.clone()));
            Ok(change_delegates(owner, &self.rows, change))
        }
    }

    impl DenialLog for Store {
        fn record_denial(&self, payload: Value) -> Result<()> {
            self.denials.borrow_mut().push(payload);
            Ok(())
        }
    }

    fn alice() -> OidcIdentity {
        OidcIdentity {
            issuer: "https://issuer.example".into(),
            subject: "alice".into(),
        }
    }

    fn person() -> ActorContext {
        let row = Member {
            identity: alice(),
            role: TeamRole::Member,
            on_call: false,
            display_name: None,
            created_at: 1,
            disabled_at: None,
        };
        Principal::human(alice())
            .with_current_member(Some(100), Some(&row), 0)
            .unwrap()
            .actor()
    }

    /// The command line's requests read back as themselves, and a request
    /// that names anything but the member (an owner among them) does not
    /// read.
    #[test]
    fn the_requests_read_back_and_name_no_owner() {
        let member = DelegateMember {
            member: "bob".into(),
        };
        for request in [
            DelegateRequest::Add(member.clone()),
            DelegateRequest::Remove(member),
            DelegateRequest::List,
        ] {
            let (use_case, params) = request.request();
            assert_eq!(
                DelegateRequest::parse(use_case, &params).unwrap(),
                Some(request)
            );
            let mut named = params.clone();
            named["owner"] = json!({"issuer": "https://issuer.example", "subject": "carol"});
            let error = DelegateRequest::parse(use_case, &named).unwrap_err();
            assert!(error.downcast_ref::<BadDelegate>().is_some(), "{error}");
        }
        assert_eq!(
            DelegateRequest::parse(UseCase::GoalLandingMark, &json!({})).unwrap(),
            None
        );
    }

    /// A person's change reaches the store for that person under its
    /// issuer, and `list` answers its delegates; outside the team version
    /// it is refused with the reason and records nothing; an AI actor is
    /// refused as not granted and recorded.
    #[test]
    fn a_change_reaches_the_store_only_for_the_callers_own_person() {
        let bob = OidcIdentity {
            subject: "bob".into(),
            ..alice()
        };
        let add = DelegateRequest::Add(DelegateMember {
            member: "bob".into(),
        });
        let mut store = Store::default();
        let changed = answer(&mut store, &person(), Some(true), &add).unwrap();
        assert_eq!(
            store.changes,
            [(Owner::new(alice()), DelegateChange::Add(bob.clone()))]
        );
        assert_eq!(changed["after"], json!([bob]));
        store.rows = vec![bob.clone()];
        let listed = answer(&mut store, &person(), Some(true), &DelegateRequest::List).unwrap();
        assert_eq!(listed, json!({"owner": alice(), "delegates": [bob]}));

        for team in [Some(false), None] {
            let mut store = Store::default();
            let error = answer(&mut store, &person(), team, &add).unwrap_err();
            assert_eq!(
                error.downcast_ref::<DelegateRefusal>(),
                Some(&DelegateRefusal::NotTeamVersion)
            );
            assert!(store.changes.is_empty() && store.denials.into_inner().is_empty());
        }

        let planner = ActorContext::instance(ActorRole::Planner, 1);
        let mut store = Store::default();
        let error = answer(&mut store, &planner, Some(true), &add).unwrap_err();
        assert_eq!(
            error.downcast_ref::<AuthorizationError>(),
            Some(&AuthorizationError {
                role: ActorRole::Planner,
                capability: Capability::DelegateWrite,
                reason: DenyReason::NotGranted,
            })
        );
        assert!(store.changes.is_empty());
        assert_eq!(
            store.denials.into_inner(),
            [json!({
                "event": "authorization_denied",
                "role": "planner",
                "capability": "delegate.write",
                "reason": "not granted",
                "resource": {"kind": "queue"},
            })]
        );

        let empty = DelegateRequest::Remove(DelegateMember { member: "".into() });
        let mut store = Store::default();
        let error = answer(&mut store, &person(), Some(true), &empty).unwrap_err();
        assert!(error.downcast_ref::<BadDelegate>().is_some(), "{error}");
        assert!(store.changes.is_empty());
    }

    /// The subject reaches the store as it was given: one with blanks
    /// around it is another person than the one without, never taken for
    /// it.
    #[test]
    fn the_subject_is_taken_as_it_is_given() {
        for subject in [" bob ", "bob", "bob ", "\tbob"] {
            for request in [
                DelegateRequest::Add(DelegateMember {
                    member: subject.into(),
                }),
                DelegateRequest::Remove(DelegateMember {
                    member: subject.into(),
                }),
            ] {
                let mut store = Store::default();
                answer(&mut store, &person(), Some(true), &request).unwrap();
                let (_, change) = &store.changes[0];
                assert_eq!(change.delegate().subject, subject, "{request:?}");
                assert_eq!(change.delegate().issuer, alice().issuer);
            }
        }
    }
}
