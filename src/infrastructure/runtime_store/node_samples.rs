//! The node samples in queue.db (`node_samples` and
//! `node_sample_imports`, [`NodeSampleStore`]).

use super::*;
use crate::application::NodeSampleStore;
use crate::domain::host_metrics::{HostSample, NodeSample, SampleSource, value_columns};

/// `INSERT` of a row of `node_samples` that adds nothing when the node has
/// a row at its second, or on its local day less than `apart` from it:
/// ?1 node, ?2 unix, ?3 local day, ?4 source, ?5 apart, then the values.
fn insert() -> String {
    let columns = value_columns().join(",");
    let marks: Vec<String> = (6..6 + value_columns().len())
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
    values
}

impl NodeSampleStore for SqliteQueue {
    fn record_node_sample(
        &self,
        node: &str,
        day: i64,
        sample: &HostSample,
        source: SampleSource,
        apart_secs: i64,
    ) -> Result<bool> {
        let written = self.conn.execute(
            &insert(),
            rusqlite::params_from_iter(values(node, day, sample, source, apart_secs)),
        )?;
        Ok(written == 1)
    }

    fn import_node_samples(
        &self,
        node: &str,
        file: &str,
        day: i64,
        samples: &[HostSample],
        bytes: u64,
    ) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut added = 0;
        {
            let mut statement = tx.prepare(&insert())?;
            for sample in samples {
                // Only the second: every row a file kept is kept.
                added += statement.execute(rusqlite::params_from_iter(values(
                    node,
                    day,
                    sample,
                    SampleSource::CsvImport,
                    0,
                )))?;
            }
        }
        tx.execute(
            "INSERT INTO node_sample_imports(node_id,file,day,bytes,rows,imported_at)
             VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(node_id, file) DO UPDATE SET
                 bytes=excluded.bytes, rows=excluded.rows, imported_at=excluded.imported_at",
            params![
                node,
                file,
                day,
                i64::try_from(bytes)?,
                i64::try_from(samples.len())?,
                self.generators.clock.now()
            ],
        )?;
        tx.commit()?;
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
        let tx = self.conn.unchecked_transaction()?;
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
            value_columns().join(",")
        ))?;
        let rows = statement.query_map(params, |row| {
            let source: String = row.get(4)?;
            let mut sample = HostSample::new(row.get(2)?);
            for (index, column) in value_columns().iter().enumerate() {
                sample.set(column, row.get(5 + index)?);
            }
            Ok((row.get(0)?, row.get(1)?, row.get(3)?, source, sample))
        })?;
        rows.map(|row| {
            let (id, node_id, local_day, source, sample) = row?;
            Ok(NodeSample {
                id,
                node_id,
                local_day,
                source: SampleSource::parse(&source)
                    .ok_or_else(|| anyhow!("unknown node sample source {source:?}"))?,
                sample,
            })
        })
        .collect()
    }
}
