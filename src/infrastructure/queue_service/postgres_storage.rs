//! Deployment storage for the service. Fresh readers run independently; the
//! write line lends one driver connection and pins its startup fence identity.
use super::*;
use crate::domain::store_setting::ConnectionString;
use crate::infrastructure::postgres::exec::PostgresRow;
use crate::infrastructure::postgres::{PostgresQueue, PostgresSession, PostgresStore, client};
use crate::infrastructure::sql_exec::{SqlResult, SqlValue};
use crate::infrastructure::sqlite::{MigrationReport, ReadOnlyQueue, SchemaState};

#[derive(Clone)]
pub struct PostgresStorage {
    connection: ConnectionString,
    writing: Arc<Mutex<Slot>>,
}
#[derive(Default)]
enum Slot {
    #[default]
    Empty,
    Idle(Box<client::PostgresClient>),
    Lent,
}

/// An owned session, returned to the line when the request's queue drops.
/// A closed driver or a transaction that could not roll back is discarded.
pub struct ServiceSession {
    client: Option<Box<client::PostgresClient>>,
    transaction: std::cell::Cell<bool>,
    back: Option<Arc<Mutex<Slot>>>,
}
impl Drop for ServiceSession {
    fn drop(&mut self) {
        if let Some(back) = &self.back {
            let client = self.client.take().expect("owned session");
            *back.lock().expect("Postgres write connection") =
                if self.transaction.get() || client.is_closed() {
                    Slot::Empty
                } else {
                    Slot::Idle(client)
                };
        }
    }
}
impl PostgresSession for ServiceSession {
    fn run(&self, sql: &str) -> Result<()> {
        self.client.as_ref().expect("owned session").run(sql)?;
        if sql.starts_with("BEGIN") {
            self.transaction.set(true);
        } else if matches!(sql, "COMMIT" | "ROLLBACK") {
            self.transaction.set(false);
        }
        Ok(())
    }
    fn query(&self, sql: &str, params: &[SqlValue]) -> SqlResult<Vec<PostgresRow>> {
        self.client
            .as_ref()
            .expect("owned session")
            .query(sql, params)
    }
    fn execute(&self, sql: &str, params: &[SqlValue]) -> SqlResult<u64> {
        self.client
            .as_ref()
            .expect("owned session")
            .execute(sql, params)
    }
}

impl PostgresStorage {
    pub fn new(connection: ConnectionString) -> Self {
        Self {
            connection,
            writing: Arc::new(Mutex::new(Slot::Empty)),
        }
    }
    fn session(&self) -> Result<ServiceSession> {
        Ok(ServiceSession {
            client: Some(Box::new(client::session(&self.connection)?)),
            transaction: std::cell::Cell::new(false),
            back: None,
        })
    }
    fn lend(&self) -> Result<ServiceSession> {
        let mut slot = self.writing.lock().expect("Postgres write connection");
        ensure!(
            !matches!(*slot, Slot::Lent),
            "Postgres write connection is already lent"
        );
        let client = match std::mem::take(&mut *slot) {
            Slot::Idle(client) => client,
            Slot::Empty => Box::new(client::session(&self.connection)?),
            Slot::Lent => unreachable!(),
        };
        *slot = Slot::Lent;
        Ok(ServiceSession {
            client: Some(client),
            transaction: std::cell::Cell::new(false),
            back: Some(self.writing.clone()),
        })
    }
    fn queue(&self, session: ServiceSession, db: &Path) -> Result<PostgresQueue<ServiceSession>> {
        storage::postgres_queue(session, db)
    }
}

impl QueueStorage for PostgresStorage {
    type Entries = PostgresStore<ServiceSession>;
    fn refuse_service_migrate(&self) -> Result<()> {
        storage::refuse_postgres_service_migrate()
    }
    fn local(&self) -> bool {
        false
    }
    fn write_line(&self, db: &Path) -> Result<ServiceWriteLine> {
        let mut line = ServiceWriteLine::waiting(db)?;
        line.postgres = Some(self.clone());
        Ok(line)
    }
    fn open(&self, db: &Path) -> Result<PostgresQueue<ServiceSession>> {
        self.queue(self.session()?, db)
    }
    fn write_queue(
        &self,
        line: Option<&Arc<ServiceWriteLine>>,
        db: &Path,
    ) -> Result<PostgresQueue<ServiceSession>> {
        let Some(line) = line else {
            return self.open(db);
        };
        ensure!(
            line.line.here(),
            "a Postgres write off the service's write line"
        );
        let identity = line
            .identity
            .lock()
            .unwrap()
            .clone()
            .ok_or(crate::application::NotLeader)?;
        Ok(self.queue(self.lend()?, db)?.with_write_identity(identity))
    }
    fn open_read_only(&self, db: &Path) -> Result<PostgresQueue<ServiceSession>> {
        self.open(db)
    }
    fn open_watch(&self, db: &Path) -> Result<PostgresQueue<ServiceSession>> {
        self.open(db)
    }
    fn schema(&self, _: &Path) -> Result<SchemaState> {
        crate::infrastructure::postgres::migrate::schema(&self.session()?)
    }
    fn binding_at(&self, db: &Path) -> Result<Option<String>> {
        self.open(db)?.repository_binding()
    }
    fn inspect_read_only(
        &self,
        db: &Path,
    ) -> Result<(SchemaState, ReadOnlyQueue<PostgresQueue<ServiceSession>>)> {
        Ok((self.schema(db)?, ReadOnlyQueue::Readable(self.open(db)?)))
    }
    fn migrate_compatible(
        &self,
        _: &Path,
        _: Option<&dyn Fn(u32) -> bool>,
        _: i64,
    ) -> Result<Option<MigrationReport>> {
        storage::refuse_postgres_service_migrate()
    }
    fn migrate_with_proof(
        &self,
        _: &Path,
        _: Option<&dyn Fn(u32) -> bool>,
        _: i64,
        _: bool,
    ) -> Result<MigrationReport> {
        storage::refuse_postgres_service_migrate()
    }
    fn refuse_breaking_in_use(&self, _: &Path, _: &dyn Fn(u32) -> bool) -> Result<()> {
        storage::refuse_postgres_service_migrate()
    }
    fn serves(&self, db: &Path, own: &Path) -> bool {
        db == own
    }
    fn current_schema(&self, _: &PostgresQueue<ServiceSession>) -> Result<bool> {
        Ok(self.schema(Path::new(""))?.opens)
    }
    fn schema_version(&self, _: &PostgresQueue<ServiceSession>) -> Result<i64> {
        Ok(self.schema(Path::new(""))?.schema_version)
    }
    fn runner_operation(
        &self,
        queue: PostgresQueue<ServiceSession>,
        hash: &str,
        operation: &crate::domain::runner_assignment::Operation,
        claim: Option<&crate::domain::runner_assignment::ClaimInputs>,
        generators: &Generators,
    ) -> Result<crate::domain::runner_assignment::Outcome> {
        queue.runner_operation(hash, operation, claim, generators)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn postgres_migration_administration_never_opens_a_driver() {
        let storage = PostgresStorage::new(ConnectionString::new("invalid connection"));
        assert!(!storage.local());
        for error in [
            storage.refuse_service_migrate().unwrap_err(),
            storage
                .migrate_compatible(Path::new("unused"), None, 0)
                .unwrap_err(),
            storage
                .migrate_with_proof(Path::new("unused"), None, 0, true)
                .unwrap_err(),
        ] {
            assert!(
                error
                    .to_string()
                    .contains("deployment's separate migrate command")
            );
        }
    }
}
