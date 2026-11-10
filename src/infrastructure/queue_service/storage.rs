//! The service's store seam. Callers use `db` to resolve the queue directory;
//! only the SQLite implementation interprets it as a database file.
//! Socket locks, tokens and the write line remain owned by the service.
use super::*;
use crate::infrastructure::sqlite::{MigrationReport, ReadOnlyQueue, SchemaState, SqliteEntries};

/// Opening and file administration belong to the backend, never to a use
/// case's queue. A deployment supplies this seam together with its entries;
/// the local service keeps SQLite's connection lending and migration policy.
pub trait QueueStorage: Clone + Send + Sync + 'static {
    type Entries: TransactionEntries + 'static;
    fn open(&self, db: &Path) -> Result<StoreQueue<Self::Entries>>;
    fn write_queue(
        &self,
        line: Option<&Arc<ServiceWriteLine>>,
        db: &Path,
    ) -> Result<StoreQueue<Self::Entries>>;
    fn open_read_only(&self, db: &Path) -> Result<StoreQueue<Self::Entries>>;
    fn open_watch(&self, db: &Path) -> Result<StoreQueue<Self::Entries>>;
    fn schema(&self, db: &Path) -> Result<SchemaState>;
    fn binding_at(&self, db: &Path) -> Result<Option<String>>;
    fn inspect_read_only(
        &self,
        db: &Path,
    ) -> Result<(SchemaState, ReadOnlyQueue<StoreQueue<Self::Entries>>)>;
    fn migrate_compatible(
        &self,
        db: &Path,
        alive: Option<&dyn Fn(u32) -> bool>,
        now: i64,
    ) -> Result<Option<MigrationReport>>;
    fn migrate_with_proof(
        &self,
        db: &Path,
        alive: Option<&dyn Fn(u32) -> bool>,
        now: i64,
        local_proof: bool,
    ) -> Result<MigrationReport>;
    fn refuse_breaking_in_use(&self, db: &Path, alive: &dyn Fn(u32) -> bool) -> Result<()>;
    fn serves(&self, db: &Path, own: &Path) -> bool;
    fn current_schema(&self, queue: &StoreQueue<Self::Entries>) -> Result<bool>;
    fn schema_version(&self, queue: &StoreQueue<Self::Entries>) -> Result<i64>;
    /// The store-specific atomic runner assignment, beyond credential reads.
    /// It retains the adapter's transaction and fencing boundary.
    fn runner_operation(
        &self,
        queue: StoreQueue<Self::Entries>,
        hash: &str,
        operation: &crate::domain::runner_assignment::Operation,
        claim: Option<&crate::domain::runner_assignment::ClaimInputs>,
        generators: &Generators,
    ) -> Result<crate::domain::runner_assignment::Outcome>;
}

/// The local store: fresh readers and the write line's lent connection.
#[derive(Clone, Copy)]
pub struct SqliteStorage;

impl QueueStorage for SqliteStorage {
    type Entries = SqliteEntries;
    fn open(&self, db: &Path) -> Result<SqliteQueue> {
        SqliteQueue::open(db)
    }
    fn write_queue(&self, line: Option<&Arc<ServiceWriteLine>>, db: &Path) -> Result<SqliteQueue> {
        match line {
            Some(line) => line.queue(),
            None => self.open(db),
        }
    }
    fn open_read_only(&self, db: &Path) -> Result<SqliteQueue> {
        SqliteQueue::open_read_only(db)
    }
    fn open_watch(&self, db: &Path) -> Result<SqliteQueue> {
        SqliteQueue::open_watch(db)
    }
    fn schema(&self, db: &Path) -> Result<SchemaState> {
        SqliteQueue::schema(db)
    }
    fn binding_at(&self, db: &Path) -> Result<Option<String>> {
        SqliteQueue::binding_at(db)
    }
    fn inspect_read_only(&self, db: &Path) -> Result<(SchemaState, ReadOnlyQueue)> {
        SqliteQueue::inspect_read_only(db)
    }
    fn migrate_compatible(
        &self,
        db: &Path,
        alive: Option<&dyn Fn(u32) -> bool>,
        now: i64,
    ) -> Result<Option<MigrationReport>> {
        SqliteQueue::migrate_compatible(db, alive, now)
    }
    fn migrate_with_proof(
        &self,
        db: &Path,
        alive: Option<&dyn Fn(u32) -> bool>,
        now: i64,
        local_proof: bool,
    ) -> Result<MigrationReport> {
        SqliteQueue::migrate_with_proof(db, alive, now, local_proof)
    }
    fn refuse_breaking_in_use(&self, db: &Path, alive: &dyn Fn(u32) -> bool) -> Result<()> {
        SqliteQueue::refuse_breaking_in_use(db, alive)
    }
    /// Both paths must resolve to the same file; an absent file is another queue.
    fn serves(&self, db: &Path, own: &Path) -> bool {
        match (fs::canonicalize(db), fs::canonicalize(own)) {
            (Ok(named), Ok(own)) => named == own,
            _ => false,
        }
    }
    fn current_schema(&self, queue: &SqliteQueue) -> Result<bool> {
        Ok(queue.schema_version()? >= SqliteQueue::SCHEMA_VERSION)
    }
    fn schema_version(&self, queue: &SqliteQueue) -> Result<i64> {
        queue.schema_version()
    }
    fn runner_operation(
        &self,
        queue: SqliteQueue,
        hash: &str,
        operation: &crate::domain::runner_assignment::Operation,
        claim: Option<&crate::domain::runner_assignment::ClaimInputs>,
        generators: &Generators,
    ) -> Result<crate::domain::runner_assignment::Outcome> {
        queue.runner_operation(hash, operation, claim, generators)
    }
}

impl SqliteStorage {
    pub(super) fn write_connection(&self, db: &Path) -> WriteConnection {
        SqliteQueue::write_connection(db)
    }
}
