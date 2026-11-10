//! The per-request service builds without a driver feature.
use super::*;
use crate::infrastructure::postgres::{PostgresQueue, PostgresSession, exec::PostgresRow};
use crate::infrastructure::sql_exec::{SqlResult, SqlValue};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Clone, Default)]
struct Session(Rc<Recorded>);
#[derive(Default)]
struct Recorded {
    transaction: Cell<bool>,
    statements: RefCell<Vec<String>>,
}

fn row(names: &[&str], values: Vec<SqlValue>) -> PostgresRow {
    PostgresRow::new(
        names.iter().map(|name| (*name).to_owned()).collect(),
        values,
    )
}

impl PostgresSession for Session {
    fn run(&self, sql: &str) -> Result<()> {
        if sql.starts_with("BEGIN") {
            assert!(!self.0.transaction.replace(true), "nested BEGIN");
        } else if matches!(sql, "COMMIT" | "ROLLBACK") {
            assert!(self.0.transaction.replace(false));
        }
        self.0.statements.borrow_mut().push(sql.to_owned());
        Ok(())
    }
    fn query(&self, sql: &str, _: &[SqlValue]) -> SqlResult<Vec<PostgresRow>> {
        self.0.statements.borrow_mut().push(sql.to_owned());
        // Only opening the store reads outside issuing's transaction.
        if sql.contains("FROM pg_catalog.pg_class") {
            return Ok(vec![row(&["count"], vec![SqlValue::Integer(1)])]);
        }
        if sql == "SELECT version, floor FROM dagq_schema" {
            return Ok(vec![row(
                &["version", "floor"],
                vec![
                    SqlValue::Integer(crate::infrastructure::schema::BINARY_SCHEMA),
                    SqlValue::Integer(1),
                ],
            )]);
        }
        if sql == crate::infrastructure::write_leader::IDENTITY_SQL {
            return Ok(vec![row(
                &["holder", "generation"],
                vec![SqlValue::Text("service".into()), SqlValue::Integer(1)],
            )]);
        }
        assert!(self.0.transaction.get(), "read outside issuing: {sql}");
        Ok(vec![])
    }
    fn execute(&self, sql: &str, _: &[SqlValue]) -> SqlResult<u64> {
        assert!(self.0.transaction.get(), "write outside issuing: {sql}");
        self.0.statements.borrow_mut().push(sql.to_owned());
        Ok(1)
    }
}

#[test]
fn postgres_issuing_reads_inside_one_committed_transaction() {
    fn supervisor_store(_: &mut impl crate::application::supervisor_service::SupervisorStore) {}
    let session = Session::default();
    let mut queue = PostgresQueue::open(session.clone(), "runs".into()).unwrap();
    supervisor_store(&mut queue);
    let mut request = ServiceSqlite {
        queue,
        db: PathBuf::from("queue/data.db"),
        reads: Arc::new(|_, _, _| Ok(Value::Null)),
        writes: Arc::new(|_, _, _| Ok(Value::Null)),
        commands: Arc::new(|_, _, _| Ok(Value::Null)),
        detail: |_, _| Value::Null,
    };
    let request: &mut dyn ServiceQueue = &mut request;
    let _ = request.supervisor_store();
    session.0.statements.borrow_mut().clear();
    let answer = request
        .issuing(
            &crate::domain::queue_service::IssueHeld::Nothing,
            1_800_000_000_000,
            &mut |facts| {
                assert!(
                    facts
                        .member(&crate::domain::oidc::OidcIdentity {
                            issuer: "issuer".into(),
                            subject: "subject".into(),
                        })?
                        .is_none()
                );
                assert!(
                    facts
                        .target_start(&StepTarget::of(
                            crate::domain::session_step::TargetKind::PlanReview,
                            1,
                        ))?
                        .is_none()
                );
                Ok(json!({"issued": true}))
            },
        )
        .unwrap();
    assert_eq!(answer, json!({"issued": true}));
    let statements = session.0.statements.borrow();
    assert_eq!(
        statements.iter().filter(|s| s.starts_with("BEGIN")).count(),
        1
    );
    assert_eq!(statements.iter().filter(|s| *s == "COMMIT").count(), 1);
    assert!(!statements.iter().any(|s| s == "ROLLBACK"));
    assert!(!session.0.transaction.get());
    assert!(statements.iter().any(|s| s.contains("FROM members")));
    assert!(statements.iter().any(|s| s.contains("FROM supervisors")));
    assert!(statements.iter().any(|s| s.contains("FROM plan_reviews")));
}
