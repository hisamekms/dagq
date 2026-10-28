//! The node samples in queue.db (`node_samples` and
//! `node_sample_imports`, [`NodeSampleStore`]).

use super::*;
use crate::application::NodeSampleStore;
use crate::domain::environment::{NodeRate, Resources};
use crate::domain::host_metrics::{
    HostSample, ImportMark, NodeSample, SampleSource, value_columns,
};

/// The columns of the sample in the common units, in the order [`values`]
/// gives them after the sample's.
const RESOURCE_COLUMNS: [&str; 5] = [
    "cpu_seconds",
    "cpu_limit",
    "memory_max_bytes",
    "throttled_seconds",
    "cpu_pressure",
];

/// Every column of a sample's values, the common units' after them.
fn sample_columns() -> String {
    value_columns()
        .iter()
        .chain(RESOURCE_COLUMNS.iter())
        .copied()
        .collect::<Vec<_>>()
        .join(",")
}

/// `INSERT` of a row of `node_samples` that adds nothing when the node has
/// a row at its second, or on its local day less than `apart` from it:
/// ?1 node, ?2 unix, ?3 local day, ?4 source, ?5 apart, then the values
/// and the common units.
fn insert() -> String {
    let columns = sample_columns();
    let marks: Vec<String> = (6..6 + value_columns().len() + RESOURCE_COLUMNS.len())
        .map(|n| format!("?{n}"))
        .collect();
    // The WHERE also keeps the upsert's ON from reading as a join's.
    format!(
        "INSERT INTO node_samples(node_id,unix,local_day,source,{columns})
         SELECT ?1,?2,?3,?4,{}
         WHERE NOT EXISTS (SELECT 1 FROM node_samples
             WHERE node_id=?1 AND local_day=?3 AND unix > ?2 - ?5 AND unix < ?2 + ?5)
         ON CONFLICT(node_id, unix) DO NOTHING",
        marks.join(",")
    )
}

fn values(
    node: &str,
    day: i64,
    sample: &HostSample,
    resources: &Resources,
    source: SampleSource,
    apart: i64,
) -> Vec<rusqlite::types::Value> {
    let mut values: Vec<rusqlite::types::Value> = vec![
        node.to_owned().into(),
        sample.unix.into(),
        day.into(),
        source.as_str().to_owned().into(),
        apart.into(),
    ];
    values.extend(
        value_columns()
            .iter()
            .map(|column| sample.values.get(column).copied().into()),
    );
    values.extend([
        resources.cpu_seconds.into(),
        resources.cpu_limit.into(),
        resources.memory_max_bytes.into(),
        resources.throttled_seconds.into(),
        resources.cpu_pressure.into(),
    ]);
    values
}

impl NodeSampleStore for SqliteQueue {
    fn record_node_sample(
        &self,
        node: &str,
        day: i64,
        sample: &HostSample,
        resources: &NodeRate,
        source: SampleSource,
        apart_secs: i64,
    ) -> Result<bool> {
        // Immediate: the rows around are read in the write that admits the
        // sample, so another supervisor's sample comes before or after both.
        let tx = self.write_tx("node_samples_record", TransactionBehavior::Immediate)?;
        let (before, later): (Option<i64>, bool) = tx.query_row(
            "SELECT (SELECT max(unix) FROM node_samples WHERE node_id=?1 AND unix < ?2),
                    EXISTS(SELECT 1 FROM node_samples WHERE node_id=?1 AND unix > ?2)",
            params![node, sample.unix],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        // A sample written after a later row of the node (it waited on the
        // queue's lock) stands for no time: the later row counted it.
        let previous = if later { Some(sample.unix) } else { before };
        let written = tx.execute(
            &insert(),
            rusqlite::params_from_iter(values(
                node,
                day,
                sample,
                &resources.over(previous, sample.unix),
                source,
                apart_secs,
            )),
        )?;
        tx.commit()?;
        Ok(written == 1)
    }

    fn import_node_sample_part(
        &self,
        node: &str,
        file: &str,
        day: i64,
        samples: &[HostSample],
        mark: Option<ImportMark>,
    ) -> Result<usize> {
        let mut added = 0;
        in_batches(
            self,
            "node_samples_csv_import",
            samples,
            |tx, batch, last| {
                let mut statement = tx.prepare_cached(&insert())?;
                for sample in batch {
                    // Only the second: every row a file kept is kept.
                    let rows = statement.execute(rusqlite::params_from_iter(values(
                        node,
                        day,
                        sample,
                        &Resources::default(),
                        SampleSource::CsvImport,
                        0,
                    )))?;
                    tx.wrote(rows);
                    added += rows;
                }
                // The mark goes with the last batch: a file whose import
                // stopped before it keeps its mark of before (none, or bytes
                // shorter than the file), so the next import reads it again
                // whole, and its rows committed already are kept out by
                // their second.
                if last && let Some(mark) = mark {
                    tx.execute(
                    "INSERT INTO node_sample_imports(node_id,file,day,bytes,rows,imported_at)
                     VALUES (?1,?2,?3,?4,?5,?6)
                     ON CONFLICT(node_id, file) DO UPDATE SET
                         bytes=excluded.bytes, rows=excluded.rows, imported_at=excluded.imported_at",
                    params![
                        node,
                        file,
                        day,
                        i64::try_from(mark.bytes)?,
                        i64::try_from(mark.rows)?,
                        self.generators.clock.now()
                    ],
                )?;
                }
                Ok(())
            },
        )?;
        Ok(added)
    }

    fn imported_files(&self, node: &str) -> Result<std::collections::BTreeMap<String, u64>> {
        let mut statement = self
            .conn
            .prepare("SELECT file, bytes FROM node_sample_imports WHERE node_id=?1")?;
        let files = statement.query_map([node], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        files
            .map(|file| {
                let (name, bytes) = file?;
                Ok((name, u64::try_from(bytes)?))
            })
            .collect()
    }

    fn remove_node_samples(&self, before_day: i64) -> Result<usize> {
        // Immediate: its first DELETE is the long one, and a deferred
        // transaction's hold starts after its first write.
        let tx = self.write_tx("node_samples_retention", TransactionBehavior::Immediate)?;
        let removed = tx.execute(
            "DELETE FROM node_samples WHERE local_day < ?1",
            [before_day],
        )?;
        tx.execute(
            "DELETE FROM node_sample_imports WHERE day < ?1",
            [before_day],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    fn node_samples(&self, node: &str, from: i64, until: i64) -> Result<Vec<NodeSample>> {
        self.read_node_samples(
            "node_id=?1 AND unix BETWEEN ?2 AND ?3 ORDER BY unix",
            params![node, from, until],
        )
    }

    fn node_samples_after(&self, after: i64) -> Result<Vec<NodeSample>> {
        self.read_node_samples("id > ?1 ORDER BY id", params![after])
    }
}

impl SqliteQueue {
    /// The rows of `node_samples` `filter` (a `WHERE` clause and its order)
    /// keeps.
    fn read_node_samples(
        &self,
        filter: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<NodeSample>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT id,node_id,unix,local_day,source,{} FROM node_samples WHERE {filter}",
            sample_columns()
        ))?;
        let rows = statement.query_map(params, |row| {
            let source: String = row.get(4)?;
            let mut sample = HostSample::new(row.get(2)?);
            for (index, column) in value_columns().iter().enumerate() {
                sample.set(column, row.get(5 + index)?);
            }
            let at = 5 + value_columns().len();
            let mut resources = Resources {
                cpu_seconds: row.get(at)?,
                cpu_limit: row.get(at + 1)?,
                memory_max_bytes: row.get(at + 2)?,
                throttled_seconds: row.get(at + 3)?,
                cpu_pressure: row.get(at + 4)?,
                load_average: None,
            };
            // The load average is the `load1` column's, in the rows the
            // measure of the node wrote.
            if !resources.is_empty() {
                resources.load_average = sample.values.get("load1").copied();
            }
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(3)?,
                source,
                sample,
                resources,
            ))
        })?;
        rows.map(|row| {
            let (id, node_id, local_day, source, sample, resources) = row?;
            Ok(NodeSample {
                id,
                node_id,
                local_day,
                source: SampleSource::parse(&source)
                    .ok_or_else(|| anyhow!("unknown node sample source {source:?}"))?,
                sample,
                resources,
            })
        })
        .collect()
    }
}
