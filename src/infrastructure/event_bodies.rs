//! The local body adapter and what an event write does with its body.

use anyhow::Result;
use serde_json::Value;

use super::sql_exec::{SqlResult, SqlWrite, sql_params};
use crate::application::{BodyPlacement, EventBodies, event_body_row, is_large_event_body};
use crate::domain::ids::EventId;

/// Local events retain their exact serialized body in `run_events.payload`.
/// No external object is created, so reads find no key and deletion is a no-op.
pub struct InlineEventBodies;

impl EventBodies for InlineEventBodies {
    fn put(&self, _key: &str, _body: &str) -> Result<BodyPlacement> {
        Ok(BodyPlacement::Inline)
    }

    fn read(&self, _key: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn delete(&self, _key: &str) -> Result<()> {
        Ok(())
    }
}

/// What `dagq_event_body()`, the function every production event INSERT calls
/// on its payload ([`super::sql_exec::sqlite::attach_event_bodies`]), returns
/// for `body`. The port receives neither connection nor transaction. A put
/// finishes before SQLite inserts the referencing row; its error aborts the
/// INSERT. A later rollback may leave an orphan. Each large write has a fresh
/// key.
pub(super) fn event_body_for_row(bodies: &dyn EventBodies, body: String) -> Result<String> {
    if !is_large_event_body(&body) {
        return Ok(body);
    }
    let key = uuid::Uuid::new_v4().to_string();
    let placement = bodies.put(&key, &body)?;
    Ok(event_body_row(body, &key, placement))
}

/// Set the top-level `field` of the recorded event `event_id`: the one way a
/// write adds to an event's body after its INSERT. A body the field takes over
/// the threshold goes through the port like a new one. A row that already
/// holds a key has no object for the field to go in, so it stays as it is and
/// the field is lost: see [`event_body_row`].
pub(super) fn set_event_body_field(
    tx: &impl SqlWrite,
    event_id: EventId,
    field: &str,
    value: &Value,
) -> SqlResult<()> {
    tx.execute(
        "UPDATE run_events SET payload=dagq_event_body(json_set(payload,?3,json(?2))) WHERE id=?1",
        sql_params![event_id.as_i64(), value.to_string(), format!("$.{field}")],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::{
        application::EVENT_BODY_THRESHOLD, domain::EventKind, infrastructure::sqlite::SqliteQueue,
    };
    use serde_json::json;

    #[derive(Default)]
    struct StoredBodies(Mutex<Vec<(String, String)>>);

    impl EventBodies for StoredBodies {
        fn put(&self, key: &str, body: &str) -> Result<BodyPlacement> {
            self.0.lock().unwrap().push((key.into(), body.into()));
            Ok(BodyPlacement::Stored)
        }
        fn read(&self, key: &str) -> Result<Option<String>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, b)| b.clone()))
        }
        fn delete(&self, key: &str) -> Result<()> {
            self.0.lock().unwrap().retain(|(k, _)| k != key);
            Ok(())
        }
    }

    #[test]
    fn local_large_events_keep_their_exact_payload_and_remain_readable() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("queue.db")).unwrap();
        let payload = json!({"message": "あ".repeat(EVENT_BODY_THRESHOLD)});
        queue
            .record_queue_event(EventKind::BackendCallFailed, payload.clone())
            .unwrap();
        let raw: String = queue
            .conn
            .query_row(
                "SELECT payload FROM run_events WHERE kind='backend_call_failed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, payload.to_string());
        assert_eq!(
            queue
                .latest_event_of("backend_call_failed")
                .unwrap()
                .unwrap()
                .payload,
            payload
        );
        assert_eq!(InlineEventBodies.read("absent").unwrap(), None);
        InlineEventBodies.delete("absent").unwrap();
    }

    #[test]
    fn only_large_new_writes_put_bodies_and_rollback_leaves_them_readable() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("queue.db")).unwrap();
        let payload = json!({"message": "x".repeat(EVENT_BODY_THRESHOLD)});
        queue
            .record_queue_event(EventKind::BackendCallFailed, payload.clone())
            .unwrap();
        let bodies = Arc::new(StoredBodies::default());
        let queue = queue.with_event_bodies(bodies.clone()).unwrap();
        // Reading an old large event does not migrate it through the port.
        assert_eq!(
            queue
                .latest_event_of("backend_call_failed")
                .unwrap()
                .unwrap()
                .payload,
            payload
        );
        queue
            .record_queue_event(EventKind::BackendCallFailed, json!({}))
            .unwrap();
        assert!(bodies.0.lock().unwrap().is_empty());
        let tx = queue.write_tx("event_body_rollback_test").unwrap();
        super::super::sqlite::record_queue_event_in(&tx, EventKind::BackendCallFailed, &payload)
            .unwrap();
        let row: String = tx
            .query_row(
                "SELECT payload FROM run_events ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let key: String = serde_json::from_str(&row).unwrap();
        assert_eq!(bodies.read(&key).unwrap(), Some(payload.to_string()));
        drop(tx);
        // No reference row survived, but the port was not rolled back.
        assert_eq!(
            queue
                .latest_event_of("backend_call_failed")
                .unwrap()
                .unwrap()
                .payload,
            json!({})
        );
        assert_eq!(bodies.read(&key).unwrap(), Some(payload.to_string()));
        bodies.delete(&key).unwrap();
        assert_eq!(bodies.read(&key).unwrap(), None);
    }

    /// A body one field short of the threshold, and that field's event.
    fn event_just_under_the_threshold(queue: &SqliteQueue) -> (String, EventId) {
        let payload = json!({"message": "x".repeat(EVENT_BODY_THRESHOLD - 20)});
        queue
            .record_queue_event(EventKind::BackendCallFailed, payload.clone())
            .unwrap();
        let id = queue
            .latest_event_of("backend_call_failed")
            .unwrap()
            .unwrap()
            .id;
        (payload.to_string(), id)
    }

    fn raw_payload(queue: &SqliteQueue, id: EventId) -> String {
        queue
            .conn
            .query_row(
                "SELECT payload FROM run_events WHERE id=?1",
                [id.as_i64()],
                |r| r.get(0),
            )
            .unwrap()
    }

    #[test]
    fn a_field_that_takes_a_body_over_the_threshold_puts_it_and_a_keyed_row_stays() {
        let dir = tempfile::tempdir().unwrap();
        let bodies = Arc::new(StoredBodies::default());
        let queue = SqliteQueue::init(dir.path().join("queue.db"))
            .unwrap()
            .with_event_bodies(bodies.clone())
            .unwrap();
        let (body, id) = event_just_under_the_threshold(&queue);
        assert!(!is_large_event_body(&body));
        assert_eq!(raw_payload(&queue, id), body);
        assert!(bodies.0.lock().unwrap().is_empty());

        let tx = queue.write_tx("event_body_field_test").unwrap();
        set_event_body_field(&tx, id, "finding_id", &json!(123)).unwrap();
        tx.commit().unwrap();
        let row = raw_payload(&queue, id);
        let key: String = serde_json::from_str(&row).unwrap();
        let stored: Value = serde_json::from_str(&bodies.read(&key).unwrap().unwrap()).unwrap();
        assert_eq!(stored["finding_id"], json!(123));

        // The field's place is the stored body, which the row cannot reach.
        let tx = queue.write_tx("event_body_keyed_field_test").unwrap();
        set_event_body_field(&tx, id, "tokens", &json!({"total": 1})).unwrap();
        tx.commit().unwrap();
        assert_eq!(raw_payload(&queue, id), row);
        assert_eq!(bodies.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_local_row_takes_each_field_in_its_own_column_over_the_threshold_too() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("queue.db")).unwrap();
        let (body, id) = event_just_under_the_threshold(&queue);
        let tx = queue.write_tx("event_body_local_field_test").unwrap();
        for (field, value) in [
            ("finding_id", json!(123)),
            ("finding_ids", json!([123])),
            ("tokens", json!({"total": 1})),
        ] {
            set_event_body_field(&tx, id, field, &value).unwrap();
        }
        tx.commit().unwrap();
        let open = body.strip_suffix('}').unwrap();
        assert_eq!(
            raw_payload(&queue, id),
            format!(r#"{open},"finding_id":123,"finding_ids":[123],"tokens":{{"total":1}}}}"#)
        );
    }

    struct FailedPut;

    impl EventBodies for FailedPut {
        fn put(&self, _key: &str, _body: &str) -> Result<BodyPlacement> {
            anyhow::bail!("body storage failed")
        }
        fn read(&self, _key: &str) -> Result<Option<String>> {
            Ok(None)
        }
        fn delete(&self, _key: &str) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failed_put_does_not_insert_a_reference() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("queue.db"))
            .unwrap()
            .with_event_bodies(Arc::new(FailedPut))
            .unwrap();
        let error = queue
            .record_queue_event(
                EventKind::BackendCallFailed,
                json!({"message": "x".repeat(EVENT_BODY_THRESHOLD)}),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("body storage failed"));
        assert!(
            queue
                .latest_event_of("backend_call_failed")
                .unwrap()
                .is_none()
        );
    }
}
