//! The state-changing commands of the CLI as use cases (ADR-t728-1
//! decision 5): each names the capability and the resource it acts on,
//! reads from the store what the policy's rules need (a task's status, a
//! proposal's owner), asks the [`crate::domain::Authorizer`] as the actor
//! that runs it, and only then changes the queue. A refusal is recorded as
//! `authorization_denied` and returned as the typed
//! [`crate::domain::AuthorizationError`]. The CLI parses and prints only.

pub mod delegates;
pub mod dialogue;
pub mod holds;
pub mod operations;
pub mod planning;
pub mod replans;
pub mod requests;

use anyhow::Result;
use serde_json::{Value, json};
use tracing::warn;

use crate::domain::{
    ActorContext, AuthorizationError, Authorizer, Capability, Resource, authorization::DenyReason,
    event_kind,
};

/// Where a command records what it refused.
pub trait DenialLog {
    /// Record a refusal as the queue event `authorization_denied`.
    fn record_denial(&self, payload: Value) -> Result<()>;
    /// An authorization grant of a privileged operation. Adapters without a
    /// queue (unit fakes) need not retain it.
    fn record_privileged(&self, _payload: Value) -> Result<()> {
        Ok(())
    }
}

/// The actor a command runs as and the policy it is held to.
#[derive(Clone, Copy)]
pub struct Gate<'a> {
    pub actor: &'a ActorContext,
    pub authorizer: &'a dyn Authorizer,
}

impl Gate<'_> {
    /// Refuse, before the store is read, a capability the actor has on no
    /// resource: `named` is the resource as the command names it, its
    /// owner unknown; what the owner decides is left to [`Self::authorize`].
    /// When this passes, a missing task or proposal is a store error rather
    /// than a denial. It does not manufacture a resource to authorize.
    pub fn refuse_ungranted(
        &self,
        log: &dyn DenialLog,
        capability: Capability,
        named: &Resource,
    ) -> Result<()> {
        match self.authorizer.authorize(self.actor, capability, named) {
            Err(error) if error.reason != DenyReason::Resource => {
                record(log, &error, named);
                Err(error.into())
            }
            _ => Ok(()),
        }
    }

    /// Ask the authorizer; a refusal is recorded (on the queue, as the
    /// refused actor) and returned as the [`AuthorizationError`]. A record
    /// that cannot be written does not turn the refusal into another error.
    pub fn authorize(
        &self,
        log: &dyn DenialLog,
        capability: Capability,
        resource: &Resource,
    ) -> Result<()> {
        let Err(error) = self.authorizer.authorize(self.actor, capability, resource) else {
            if super::audit::privileged(capability) {
                log.record_privileged(json!({"capability": capability, "reason": "allowed", "resource": resource.record()}))?;
            }
            return Ok(());
        };
        record(log, &error, resource);
        Err(error.into())
    }
}

pub(crate) fn record(log: &dyn DenialLog, error: &AuthorizationError, resource: &Resource) {
    if let Err(record) = log.record_denial(denial(error, resource)) {
        warn!("could not record the refusal ({error}): {record:#}");
    }
}

/// The payload of `authorization_denied`.
pub(crate) fn denial(error: &AuthorizationError, resource: &Resource) -> Value {
    json!({
        "event": event_kind::AUTHORIZATION_DENIED,
        "role": error.role,
        "capability": error.capability,
        "reason": error.reason.as_str(),
        "resource": resource.record(),
    })
}

#[cfg(test)]
mod audit_tests {
    use super::*;
    use crate::domain::{ActorRole, StaticPolicy};
    use std::cell::RefCell;

    #[derive(Default)]
    struct Recorder {
        denied: RefCell<Vec<Value>>,
        grants: RefCell<Vec<Value>>,
        fail: bool,
        lost: RefCell<u64>,
        calls: RefCell<u64>,
    }
    impl DenialLog for Recorder {
        fn record_denial(&self, payload: Value) -> Result<()> {
            *self.calls.borrow_mut() += 1;
            crate::application::audit::write_refusal(
                || {
                    if self.fail {
                        anyhow::bail!("audit unavailable");
                    }
                    self.denied.borrow_mut().push(payload);
                    Ok(())
                },
                &mut self.lost.borrow_mut(),
            );
            Ok(())
        }
        fn record_privileged(&self, payload: Value) -> Result<()> {
            self.grants.borrow_mut().push(payload);
            Ok(())
        }
    }

    #[test]
    fn worker_denial_is_one_fact_and_a_failed_sink_preserves_the_refusal() {
        let actor = ActorContext::new(ActorRole::Worker, "worker-a");
        let gate = Gate {
            actor: &actor,
            authorizer: &StaticPolicy,
        };
        for fail in [false, true] {
            let log = Recorder {
                fail,
                ..Recorder::default()
            };
            let error = gate
                .authorize(&log, Capability::AskAnswer, &Resource::Queue)
                .unwrap_err();
            let refusal = error.downcast_ref::<AuthorizationError>().unwrap();
            assert_eq!(refusal.role, ActorRole::Worker);
            assert_eq!(refusal.capability, Capability::AskAnswer);
            assert_eq!(refusal.reason, DenyReason::NotGranted);
            assert_eq!(*log.calls.borrow(), 1);
            assert_eq!(log.denied.borrow().len(), usize::from(!fail));
            if !fail {
                let input =
                    crate::application::audit::authorization_fact(&log.denied.borrow()[0], false);
                assert_eq!(input.authority.rule.as_deref(), Some("not granted"));
                assert_eq!(input.authority.capability.as_deref(), Some("ask.answer"));
                assert_eq!(input.authority.policy_version.unwrap().len(), 64);
            }
            assert_eq!(*log.lost.borrow(), u64::from(fail));
        }
    }

    #[test]
    fn bypass_grant_is_retained_but_an_ordinary_read_is_not() {
        let log = Recorder::default();
        let actor = ActorContext::user();
        let gate = Gate {
            actor: &actor,
            authorizer: &StaticPolicy,
        };
        gate.authorize(&log, Capability::TaskReadyBypassReview, &Resource::Queue)
            .unwrap();
        gate.authorize(&log, Capability::QueueRead, &Resource::Queue)
            .unwrap();
        assert_eq!(log.grants.borrow().len(), 1);
        let input = crate::application::audit::authorization_fact(&log.grants.borrow()[0], true);
        assert_eq!(
            input.action,
            crate::domain::audit::AuditAction::AuthzPrivileged
        );
        assert_eq!(
            input.authority.capability.as_deref(),
            Some("task.ready_bypass_review")
        );
        assert_eq!(input.authority.rule.as_deref(), Some("allowed"));
        assert_eq!(
            crate::application::audit::lost_refusal_count(u64::MAX, true),
            u64::MAX
        );
    }
}
