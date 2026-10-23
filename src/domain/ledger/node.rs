//! The node rows of the ledger (ADR-t1662-1 L4, ADR-t1662-2 decision 5,
//! docs/design/measurement.md "台帳の形"): the NodeSampleStore's samples of
//! one node over one [`BUCKET_SECS`] of unix seconds, folded by
//! [`node_rows`] from those samples alone (a `csv_import` row as a sampled
//! one, with no legacy mark), so that the host's load is read from the
//! ledger as every other statistic is ([`host_summary`]).
//!
//! A row keeps each sample of its bucket whole (its second, its local day,
//! where it came from and each value it has; a value the node could not
//! give stays missing), because the median and the p90 of a window, and the
//! seconds each sample stands for, are not made of sums: [`host_summary`]
//! gives the samples of the rows a window takes to
//! [`host_metrics::summarize`], so that any window, one across buckets
//! included, and a bucket's first sample (which stands for the seconds
//! since the window's start, never since the bucket's) read as the
//! samples themselves do.
//!
//! What a node row holds:
//!
//! | item | what it is |
//! | --- | --- |
//! | `kind`, `key` | `node`, and `<node id>/<the bucket's first unix second>` |
//! | `subject` | kind `node`, id the node's ([`host_metrics::HOST_NODE`] for the supervisor's host) |
//! | `started_at`, `ended_at` | the bucket's start and end, unix milliseconds (the end exclusive) |
//! | `inputs_through.node_samples` | the largest NodeSampleStore id folded (the order rows came in) |
//! | `detail.samples` | each sample: `id`, `unix`, `day` (its local day), `source` (`sampled`, `csv_import`) and `values` (column → the value's shortest text that reads back as it is) |
//!
//! Its events (`inputs_from`, `inputs_through.events`, `records`) are none.
//! The retention removes a sample with the NodeSampleStore's ([`retained`]),
//! so a window past the retention reads no host's load, as without the
//! ledger. Pure: the samples, the time and the retention's day are all.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{LedgerKind, LedgerRow, Subject};
use crate::domain::host_metrics::{
    self, COLUMNS, HostSample, HostSummary, NodeSample, SampleSource,
};

/// The length of a node row's bucket: an hour of unix seconds, so a day
/// is 24 rows and a sample that comes late folds again only its hour.
pub const BUCKET_SECS: i64 = 3_600;

/// The subject kind of a node row.
pub const SUBJECT: &str = "node";

/// The first unix second of the bucket `unix` falls in.
pub const fn bucket_of(unix: i64) -> i64 {
    unix.div_euclid(BUCKET_SECS) * BUCKET_SECS
}

/// The key of the row of `node` and the bucket from `start`.
pub fn key(node: &str, start: i64) -> String {
    format!("{node}/{start}")
}

/// A sample as a node row keeps it. Its values are text (Rust's shortest
/// text of the float, which parses back to the same float) and not JSON
/// numbers, which are read back only to the nearest float.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Kept {
    id: i64,
    unix: i64,
    day: i64,
    source: String,
    values: BTreeMap<String, String>,
}

impl Kept {
    fn of(sample: &NodeSample) -> Self {
        Self {
            id: sample.id,
            unix: sample.sample.unix,
            day: sample.local_day,
            source: sample.source.as_str().to_owned(),
            values: sample
                .sample
                .values
                .iter()
                .map(|(column, value)| ((*column).to_owned(), value.to_string()))
                .collect(),
        }
    }

    /// The sample again; a column or a value this binary does not know is
    /// left out, as a file's is.
    fn sample(&self) -> HostSample {
        let mut sample = HostSample::new(self.unix);
        for (name, text) in &self.values {
            if let Some(column) = COLUMNS[2..].iter().find(|column| **column == name) {
                sample.set(column, text.parse().ok());
            }
        }
        sample
    }

    fn node_sample(&self, node: &str) -> Option<NodeSample> {
        Some(NodeSample {
            resources: Default::default(),
            id: self.id,
            node_id: node.to_owned(),
            local_day: self.day,
            source: SampleSource::parse(&self.source)?,
            sample: self.sample(),
        })
    }
}

/// The samples a node row keeps; none for a row of another kind or one
/// whose samples do not read.
fn kept(row: &LedgerRow) -> Vec<Kept> {
    parsed(row).unwrap_or_default()
}

/// The samples of a node row, `None` when it is of another kind or they
/// do not read (a newer binary's shape, a broken row).
fn parsed(row: &LedgerRow) -> Option<Vec<Kept>> {
    if row.kind != LedgerKind::Node {
        return None;
    }
    serde_json::from_value(row.detail.get("samples")?.clone()).ok()
}

/// The node a node row is of.
fn node_of(row: &LedgerRow) -> &str {
    row.subject
        .as_ref()
        .map_or("", |subject| subject.id.as_str())
}

/// The row of `node` over the bucket from `start`, of `samples` (those of
/// `node` in the bucket, any order), built at `built_at`.
pub fn node_row(node: &str, start: i64, samples: &[&NodeSample], built_at: &str) -> LedgerRow {
    let mut kept: Vec<Kept> = samples.iter().map(|sample| Kept::of(sample)).collect();
    kept.sort_by_key(|sample| (sample.unix, sample.id));
    let mut row = LedgerRow::new(LedgerKind::Node, key(node, start), None, built_at);
    row.inputs_through.node_samples = kept.iter().map(|sample| sample.id).max();
    row.started_at = Some(start * 1000);
    row.ended_at = Some((start + BUCKET_SECS) * 1000);
    row.subject = Some(Subject {
        kind: SUBJECT.to_owned(),
        id: node.to_owned(),
        ..Subject::default()
    });
    row.detail = json!({ "samples": kept });
    row
}

/// The rows of `samples` (the NodeSampleStore's, any node, any order),
/// one per node and bucket that has a sample, by node and bucket.
pub fn node_rows(samples: &[NodeSample], built_at: &str) -> Vec<LedgerRow> {
    let mut buckets: BTreeMap<(&str, i64), Vec<&NodeSample>> = BTreeMap::new();
    for sample in samples {
        buckets
            .entry((sample.node_id.as_str(), bucket_of(sample.sample.unix)))
            .or_default()
            .push(sample);
    }
    buckets
        .into_iter()
        .map(|((node, start), samples)| node_row(node, start, &samples, built_at))
        .collect()
}

/// The samples a node row keeps, as the NodeSampleStore gave them.
pub fn samples(row: &LedgerRow) -> Vec<NodeSample> {
    let node = node_of(row);
    kept(row)
        .iter()
        .filter_map(|sample| sample.node_sample(node))
        .collect()
}

/// Whether the node row of `[start_ms, end_ms)` may hold a sample of the
/// unix seconds from `from` to `until` (both inclusive).
pub const fn covers(row_start_ms: i64, row_end_ms: i64, from: i64, until: i64) -> bool {
    row_start_ms <= until * 1000 && row_end_ms > from * 1000
}

/// The summary of the host's load over the unix seconds `from` to `until`
/// (both inclusive) of `rows`' samples of the supervisor's host
/// ([`host_metrics::HOST_NODE`]): what `infrastructure::host_metrics::summary`
/// gives of the same samples. A row of another kind or node adds nothing.
pub fn host_summary<'a>(
    rows: impl IntoIterator<Item = &'a LedgerRow>,
    from: i64,
    until: i64,
) -> HostSummary {
    let samples: Vec<HostSample> = rows
        .into_iter()
        .filter(|row| row.kind == LedgerKind::Node && node_of(row) == host_metrics::HOST_NODE)
        .filter(|row| {
            row.started_at
                .zip(row.ended_at)
                .is_some_and(|(start, end)| covers(start, end, from, until))
        })
        .flat_map(|row| kept(row).iter().map(Kept::sample).collect::<Vec<_>>())
        .collect();
    host_metrics::summarize(&samples, from, until)
}

/// The node row `row` as the retention leaves it once the NodeSampleStore
/// removed the samples of a local day before `before_day`: without them,
/// and `None` when it keeps none (the store then removes the row). A row
/// whose samples do not read is left as it is, not removed for a day it
/// cannot tell.
pub fn retained(row: &LedgerRow, before_day: i64) -> Option<LedgerRow> {
    let Some(samples) = parsed(row) else {
        return Some(row.clone());
    };
    let kept: Vec<Kept> = samples
        .into_iter()
        .filter(|sample| sample.day >= before_day)
        .collect();
    if kept.is_empty() {
        return None;
    }
    let mut row = row.clone();
    if let Value::Object(detail) = &mut row.detail {
        detail.insert("samples".to_owned(), json!(kept));
    }
    Some(row)
}

#[cfg(test)]
mod tests;
