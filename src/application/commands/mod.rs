//! The state-changing commands of the CLI as use cases (ADR-t728-1
//! decision 5): each names the capability and the resource it acts on,
//! reads from the store what the policy's rules need (a task's status, a
//! proposal's owner), asks the [`crate::domain::Authorizer`] as the actor
//! that runs it, and only then changes the queue. A refusal is recorded as
//! `authorization_denied` and returned as the typed
//! [`crate::domain::AuthorizationError`]. The CLI parses and prints only.

pub mod planning;
