//! The shared SQL's actor functions read these custom settings. `set_config`
//! with `true` scopes every value to the transaction; the empty string clears
//! an absent requester (Postgres custom settings reset to empty, not NULL).
//! Values are bound as text, never interpolated into SQL. Refreshing before
//! each statement also reflects changes made during a transaction.
use crate::infrastructure::{event_actor::EventActors, sql_exec::SqlValue};

/// Pure mapping of connection state to the statement and its bound values.
/// Keep the setting names paired with the functions in the migration.
pub(super) fn settings(actors: &EventActors) -> (String, Vec<SqlValue>) {
    let names = ["dagq.actor_role", "dagq.actor_id", "dagq.requested_by"];
    let expressions: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, name)| format!("set_config('{name}', ${}, true)", i + 1))
        .collect();
    (
        format!("SELECT {}", expressions.join(", ")),
        vec![
            SqlValue::Text(actors.role()),
            SqlValue::Text(actors.actor_id()),
            SqlValue::Text(actors.requested_by().unwrap_or_default()),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::actor::{ActorContext, ActorRole};

    #[test]
    fn settings_bind_the_actor_and_clear_or_restore_the_requester() {
        let actors = EventActors::new(ActorContext::instance(ActorRole::Supervisor, 42));
        let (sql, values) = settings(&actors);
        assert_eq!(
            sql,
            "SELECT set_config('dagq.actor_role', $1, true), set_config('dagq.actor_id', $2, true), set_config('dagq.requested_by', $3, true)"
        );
        assert_eq!(
            values,
            vec![
                SqlValue::Text("supervisor".into()),
                SqlValue::Text("supervisor:42".into()),
                SqlValue::Text(String::new())
            ]
        );
        actors.request(Some("outer".into()));
        let previous = actors.request(Some("inner'日本語".into()));
        assert_eq!(
            settings(&actors).1[2],
            SqlValue::Text("inner'日本語".into())
        );
        actors.request(previous);
        assert_eq!(settings(&actors).1[2], SqlValue::Text("outer".into()));
        actors.request(None);
        actors.set(ActorContext::user());
        assert_eq!(
            settings(&actors).1,
            vec![
                SqlValue::Text("user".into()),
                SqlValue::Text("user".into()),
                SqlValue::Text(String::new())
            ]
        );
    }
}
