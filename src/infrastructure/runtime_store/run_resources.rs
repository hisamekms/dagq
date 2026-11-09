//! The runs' resources the supervisor measured from outside in queue.db
//! (`run_resources`, [`RunResourceStore`]). Its boundary on a real
//! queue.db is `tests/it/run_resources.rs`.
use crate::infrastructure::sql_exec::CommitTransaction as _;
use crate::infrastructure::sql_exec::TransactionEntries;
use crate::infrastructure::store_queue::StoreQueue;

use super::*;
use crate::application::RunResourceStore;
use crate::domain::environment::{Resources, RunResource};
use crate::infrastructure::sql_exec::{SqlRead, SqlResult, SqlRow, SqlWrite, sql_params};

/// The columns of a stored window, in the order [`resource_row`] reads
/// them.
const COLUMNS: &str = "id,run_id,attachment,node_id,from_ms,until_ms,cpu_seconds,cpu_limit,\
    memory_max_bytes,throttled_seconds,cpu_pressure,load_average,is_last";

/// A row of `run_resources` read with [`COLUMNS`].
fn resource_row(row: &SqlRow<'_>) -> SqlResult<RunResource> {
    Ok(RunResource {
        id: row.get(0)?,
        run_id: row.get(1)?,
        attachment: row.get(2)?,
        node_id: row.get(3)?,
        from_ms: row.get(4)?,
        until_ms: row.get(5)?,
        resources: Resources {
            cpu_seconds: row.get(6)?,
            cpu_limit: row.get(7)?,
            memory_max_bytes: row.get(8)?,
            throttled_seconds: row.get(9)?,
            cpu_pressure: row.get(10)?,
            load_average: row.get(11)?,
        },
        last: row.get(12)?,
    })
}

impl<E: TransactionEntries> RunResourceStore for StoreQueue<E> {
    fn record_run_resource(&self, row: &RunResource) -> Result<i64> {
        let tx = self.write_entry("run_resources_record")?;
        let id = SqlWrite::insert_id(
            &tx,
            "INSERT INTO run_resources(run_id,attachment,node_id,from_ms,until_ms,cpu_seconds,
                 cpu_limit,memory_max_bytes,throttled_seconds,cpu_pressure,load_average,is_last)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            sql_params![
                row.run_id,
                row.attachment,
                row.node_id,
                row.from_ms,
                row.until_ms,
                row.resources.cpu_seconds,
                row.resources.cpu_limit,
                row.resources.memory_max_bytes,
                row.resources.throttled_seconds,
                row.resources.cpu_pressure,
                row.resources.load_average,
                row.last,
            ],
        )?;
        tx.commit()?;
        Ok(id)
    }

    fn run_resources(&self, run: &RunId) -> Result<Vec<RunResource>> {
        let tx = self.read_entry("run_resources")?;
        let rows = SqlRead::query_map(
            &tx,
            &format!("SELECT {COLUMNS} FROM run_resources WHERE run_id=?1 ORDER BY id"),
            sql_params![run.as_str()],
            resource_row,
        )?;
        Ok(rows)
    }

    fn run_resources_after(&self, after: i64, limit: usize) -> Result<Vec<RunResource>> {
        let tx = self.read_entry("run_resources_after")?;
        let rows = SqlRead::query_map(
            &tx,
            &format!("SELECT {COLUMNS} FROM run_resources WHERE id > ?1 ORDER BY id LIMIT ?2"),
            sql_params![after, i64::try_from(limit)?],
            resource_row,
        )?;
        Ok(rows)
    }

    fn open_attachments(&self) -> Result<Vec<(RunId, i64)>> {
        let tx = self.read_entry("open_attachments")?;
        let open = SqlRead::query_map(
            &tx,
            "WITH attached AS (
                 SELECT run_id, MAX(id) AS attachment FROM run_events
                 WHERE kind='environment_attached' AND run_id IS NOT NULL
                 GROUP BY run_id)
             SELECT run_id, attachment FROM attached
             WHERE NOT EXISTS(SELECT 1 FROM run_resources r
                 WHERE r.attachment=attached.attachment AND r.is_last=TRUE)
             ORDER BY attachment",
            &[],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        open.into_iter()
            .map(|(run, attachment)| Ok((RunId::new(&run)?, attachment)))
            .collect()
    }
}
