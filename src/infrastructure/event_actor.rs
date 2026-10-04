//! The actor each event records (ADR-t728-1 decision 4, task 730): every
//! connection of a [`super::sqlite::SqliteQueue`] carries the actor its
//! process acts as, and every `INSERT INTO run_events` writes it through the
//! SQL functions this module registers (`dagq_actor_role()`,
//! `dagq_actor_id()`, `dagq_requested_by()`), so no write site has to be
//! handed the actor and none can leave it out: an insert on a connection
//! without them fails. The actor is the process's ([`set_process_actor`],
//! which `main` sets once from `DAGQ_ROLE`), the user when none was set, or
//! the one a caller sets on the queue ([`EventActors::set`]); while the
//! supervisor applies a headless job's verdict, the job's id is recorded as
//! `requested_by`.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use anyhow::Result;
use rusqlite::{Connection, Row, functions::FunctionFlags};

use crate::domain::{EventActor, actor::ActorContext};

static PROCESS_ACTOR: OnceLock<ActorContext> = OnceLock::new();

/// The actor this process acts as on every queue it opens from now on,
/// unless a queue is given another; only the first call counts.
pub fn set_process_actor(actor: ActorContext) {
    let _ = PROCESS_ACTOR.set(actor);
}

/// The actor a queue opened now writes as: the process's, or the user.
pub fn process_actor() -> ActorContext {
    PROCESS_ACTOR
        .get()
        .cloned()
        .unwrap_or_else(ActorContext::user)
}

#[derive(Debug, Clone)]
struct State {
    actor: ActorContext,
    requested_by: Option<String>,
}

/// The actor of one connection, shared with the SQL functions registered
/// on it.
#[derive(Debug, Clone)]
pub(super) struct EventActors(Arc<Mutex<State>>);

impl EventActors {
    /// Register the actor functions on `conn`, writing as `actor`.
    pub(super) fn attach(conn: &Connection, actor: ActorContext) -> Result<Self> {
        let actors = Self(Arc::new(Mutex::new(State {
            actor,
            requested_by: None,
        })));
        // Not deterministic: the value changes with `set`.
        let flags = FunctionFlags::SQLITE_UTF8;
        let state = actors.0.clone();
        conn.create_scalar_function("dagq_actor_role", 0, flags, move |_| {
            Ok(lock(&state).actor.role().as_str().to_owned())
        })?;
        let state = actors.0.clone();
        conn.create_scalar_function("dagq_actor_id", 0, flags, move |_| {
            Ok(lock(&state).actor.actor_id().to_owned())
        })?;
        let state = actors.0.clone();
        conn.create_scalar_function("dagq_requested_by", 0, flags, move |_| {
            Ok(lock(&state).requested_by.clone())
        })?;
        Ok(actors)
    }

    pub(super) fn set(&self, actor: ActorContext) {
        lock(&self.0).actor = actor;
    }

    pub(super) fn get(&self) -> ActorContext {
        lock(&self.0).actor.clone()
    }

    /// Record `requested_by` (the id of a headless job whose verdict is
    /// being applied) on the events written until it is replaced; the one
    /// it replaces, to be put back when the request ends (task 783).
    pub(super) fn request(&self, requested_by: Option<String>) -> Option<String> {
        std::mem::replace(&mut lock(&self.0).requested_by, requested_by)
    }
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The actor of an event row; `None` for a row written before the queue
/// recorded actors, or a query that did not select the columns.
pub(super) fn event_actor(row: &Row<'_>) -> rusqlite::Result<Option<EventActor>> {
    if row.as_ref().column_index("actor_role").is_err() {
        return Ok(None);
    }
    let role: Option<String> = row.get("actor_role")?;
    let id: Option<String> = row.get("actor_id")?;
    Ok(match (role, id) {
        (Some(role), Some(id)) => Some(EventActor {
            role,
            id,
            requested_by: row.get("requested_by")?,
        }),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use crate::domain::EventKind;
    use serde_json::json;

    use crate::{
        application::RunLog,
        domain::actor::{ActorContext, ActorRole},
        infrastructure::sqlite::SqliteQueue,
    };

    #[test]
    fn events_record_the_actor_of_their_connection_and_the_job_they_came_of() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        // No process actor was set in the tests: the user.
        assert_eq!(queue.actor(), ActorContext::user());
        let latest = |queue: &SqliteQueue, kind: &str| {
            queue.latest_event_of(kind).unwrap().unwrap().actor.unwrap()
        };
        queue
            .record_queue_event(EventKind::ObserveStarted, json!({}))
            .unwrap();
        let user = latest(&queue, "observe_started");
        assert_eq!((user.role.as_str(), user.id.as_str()), ("user", "user"));
        assert_eq!(user.requested_by, None);

        let supervisor = ActorContext::instance(ActorRole::Supervisor, 42);
        let queue = queue.with_actor(supervisor.clone());
        assert_eq!(queue.actor(), supervisor);
        let job = ActorContext::review_job(&crate::domain::RunId::new("r1").unwrap(), 2);
        queue.request_as(Some(&job));
        queue
            .record_queue_event(EventKind::ObserveFinished, json!({}))
            .unwrap();
        queue.request_as(None);
        queue
            .record_queue_event(EventKind::BackendCallFailed, json!({}))
            .unwrap();
        let applied = latest(&queue, "observe_finished");
        assert_eq!(
            (applied.role.as_str(), applied.id.as_str()),
            ("supervisor", "supervisor:42")
        );
        assert_eq!(applied.requested_by.as_deref(), Some("review-job:r1:2"));
        assert_eq!(latest(&queue, "backend_call_failed").requested_by, None);
    }

    #[test]
    fn a_restored_request_is_written_as_the_requester_of_later_events() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db"))
            .unwrap()
            .with_actor(ActorContext::instance(ActorRole::Supervisor, 42));
        let requested_by = |kind: &str| {
            queue
                .latest_event_of(kind)
                .unwrap()
                .unwrap()
                .actor
                .unwrap()
                .requested_by
        };
        let outer = ActorContext::review_job(&crate::domain::RunId::new("r1").unwrap(), 1);
        let inner = ActorContext::review_job(&crate::domain::RunId::new("r2").unwrap(), 3);
        assert_eq!(queue.request_as(Some(&outer)), None);
        let previous = queue.request_as(Some(&inner));
        assert_eq!(previous.as_deref(), Some("review-job:r1:1"));
        queue
            .record_queue_event(EventKind::ObserveStarted, json!({}))
            .unwrap();
        queue.restore_request(previous);
        queue
            .record_queue_event(EventKind::ObserveFinished, json!({}))
            .unwrap();
        queue.restore_request(None);
        queue
            .record_queue_event(EventKind::BackendCallFailed, json!({}))
            .unwrap();
        assert_eq!(
            requested_by("observe_started").as_deref(),
            Some("review-job:r2:3")
        );
        assert_eq!(
            requested_by("observe_finished").as_deref(),
            Some("review-job:r1:1")
        );
        assert_eq!(requested_by("backend_call_failed"), None);
    }
}
