//! The state-changing commands of the CLI as use cases (ADR-t728-1
//! decision 5): each names the capability and the resource it acts on,
//! reads from the store what the policy's rules need (a task's status, a
//! proposal's owner), asks the [`crate::domain::Authorizer`] as the actor
//! that runs it, and only then changes the queue. A refusal is recorded as
//! `authorization_denied` and returned as the typed
//! [`crate::domain::AuthorizationError`]. The CLI parses and prints only.

pub mod dialogue;
pub mod planning;

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
