//! The planning request commands (ADR-t1394-1 decisions 3 and 6):
//! `request add`, which only the user and the inbox run (at a person's
//! word, the record keeping which), and `request decline`, which only the
//! planner of the runtime's open for the request runs. Each is authorized
//! as the caller before it reaches the store, and a refusal is recorded as
//! `authorization_denied`. The policy is the
//! [`crate::domain::StaticPolicy`]'s (`docs/design/authorization.md`).

use anyhow::Result;

use super::{DenialLog, Gate};
use crate::domain::{
    ActorContext, Authorizer, Capability, PlannerId, RequestId, Resource,
    plan_request::{NewPlanRequest, PlanRequest},
};

/// What the request commands read and change of the queue.
pub trait RequestStore: DenialLog {
    /// Record `request` as `by_role` with actor id `by_id`.
    fn record_request(
        &mut self,
        request: &NewPlanRequest,
        by_role: &str,
        by_id: &str,
    ) -> Result<PlanRequest>;
    /// The planner of the runtime's open for `request`; a missing request
    /// is an error.
    fn request_planner(&self, request: RequestId) -> Result<Option<PlannerId>>;
    /// Decline `request`, unless the planner open for it is no longer
    /// `planner`, the one the decline was authorized for.
    fn decline_request(
        &mut self,
        request: RequestId,
        reason: &str,
        by: &str,
        planner: Option<PlannerId>,
    ) -> Result<PlanRequest>;
}

/// The request commands of one actor on one store.
pub struct Requests<'a, S> {
    store: &'a mut S,
    gate: Gate<'a>,
}

impl<'a, S: RequestStore> Requests<'a, S> {
    pub fn new(store: &'a mut S, actor: &'a ActorContext, authorizer: &'a dyn Authorizer) -> Self {
        Self {
            store,
            gate: Gate { actor, authorizer },
        }
    }

    /// `request add`: the request, recorded as the actor (`inbox` for the
    /// inbox at a person's word, `user` for a person at a plain terminal).
    pub fn record(&mut self, request: &NewPlanRequest) -> Result<PlanRequest> {
        self.gate
            .authorize(&*self.store, Capability::RequestRecord, &Resource::Queue)?;
        let actor = self.gate.actor;
        self.store
            .record_request(request, actor.role().as_str(), actor.actor_id())
    }

    /// `request decline`: a role without the capability is refused before
    /// the request is read; a planner, unless it is the one open for the
    /// request.
    pub fn decline(&mut self, id: RequestId, reason: &str) -> Result<PlanRequest> {
        self.gate.refuse_ungranted(
            &*self.store,
            Capability::RequestDecline,
            &Resource::Request { id, planner: None },
        )?;
        let planner = self.store.request_planner(id)?;
        self.gate.authorize(
            &*self.store,
            Capability::RequestDecline,
            &Resource::Request { id, planner },
        )?;
        let by = self.gate.actor.written_by();
        self.store.decline_request(id, reason, by, planner)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::Value;

    use super::*;
    use crate::domain::{
        ActorRole, StaticPolicy,
        plan_request::{RequestStatus, check_decline},
    };

    #[derive(Default)]
    struct Store {
        denials: RefCell<Vec<Value>>,
        recorded: Vec<(String, String)>,
        planner: Option<PlannerId>,
        declined: Vec<String>,
    }

    fn request(status: RequestStatus) -> PlanRequest {
        PlanRequest {
            id: RequestId::new(1),
            text: "plan it".into(),
            note: None,
            refs: Vec::new(),
            priority: None,
            requested_by: "inbox".into(),
            requested_by_id: "inbox".into(),
            status,
            status_reason: None,
            proposals: Vec::new(),
            planners: 1,
            created_at: 0,
            updated_at: 0,
        }
    }

    impl DenialLog for Store {
        fn record_denial(&self, payload: Value) -> Result<()> {
            self.denials.borrow_mut().push(payload);
            Ok(())
        }
    }

    impl RequestStore for Store {
        fn record_request(
            &mut self,
            _: &NewPlanRequest,
            by_role: &str,
            by_id: &str,
        ) -> Result<PlanRequest> {
            self.recorded.push((by_role.into(), by_id.into()));
            Ok(request(RequestStatus::Open))
        }

        fn request_planner(&self, _: RequestId) -> Result<Option<PlannerId>> {
            Ok(self.planner)
        }

        fn decline_request(
            &mut self,
            _: RequestId,
            reason: &str,
            by: &str,
            _: Option<PlannerId>,
        ) -> Result<PlanRequest> {
            check_decline(&request(RequestStatus::Open), reason)?;
            self.declined.push(by.into());
            Ok(request(RequestStatus::Declined))
        }
    }

    fn new() -> NewPlanRequest {
        NewPlanRequest {
            text: "plan it".into(),
            note: None,
            refs: Vec::new(),
            priority: None,
        }
    }

    #[test]
    fn the_inbox_and_the_user_record_a_request_as_themselves_and_others_are_refused() {
        for (actor, role) in [
            (ActorContext::user(), "user"),
            (ActorContext::instance(ActorRole::Inbox, "inbox"), "inbox"),
        ] {
            let mut store = Store::default();
            Requests::new(&mut store, &actor, &StaticPolicy)
                .record(&new())
                .unwrap();
            assert_eq!(
                store.recorded,
                vec![(role.to_owned(), actor.actor_id().to_owned())]
            );
        }
        for role in [
            ActorRole::Planner,
            ActorRole::Worker,
            ActorRole::Observer,
            ActorRole::ReviewJob,
            ActorRole::PlanReviewJob,
            ActorRole::Supervisor,
        ] {
            let actor = ActorContext::instance(role, 1);
            let mut store = Store::default();
            let error = Requests::new(&mut store, &actor, &StaticPolicy)
                .record(&new())
                .unwrap_err();
            assert!(error.to_string().contains("request.record"), "{error}");
            assert!(store.recorded.is_empty());
            let denials = store.denials.borrow();
            assert_eq!(denials.len(), 1, "{role:?}");
            assert_eq!(denials[0]["capability"], "request.record");
        }
    }

    #[test]
    fn only_the_requests_own_planner_declines_it() {
        let own = ActorContext::instance(ActorRole::Planner, 4);
        let mut store = Store {
            planner: Some(PlannerId::new(4)),
            ..Store::default()
        };
        Requests::new(&mut store, &own, &StaticPolicy)
            .decline(RequestId::new(1), "done already")
            .unwrap();
        assert_eq!(store.declined, vec!["planner".to_owned()]);
        for actor in [
            ActorContext::instance(ActorRole::Planner, 5),
            ActorContext::instance(ActorRole::Inbox, "inbox"),
            ActorContext::user(),
            ActorContext::instance(ActorRole::Observer, 1),
        ] {
            let mut store = Store {
                planner: Some(PlannerId::new(4)),
                ..Store::default()
            };
            assert!(
                Requests::new(&mut store, &actor, &StaticPolicy)
                    .decline(RequestId::new(1), "no")
                    .is_err()
            );
            assert!(store.declined.is_empty());
            assert_eq!(store.denials.borrow().len(), 1);
        }
    }
}
