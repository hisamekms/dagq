//! Queue state shared by store adapters. The entries own the connection and
//! transaction policy; shared store implementations can only use their port.

use super::{event_actor::EventActors, sql_exec::TransactionEntries};
use crate::application::Generators;
use crate::domain::{ChangeSet, TagSet, TitlePrefixes};
use anyhow::Result;
use std::{path::PathBuf, sync::Arc};

/// A queue's shared state over an adapter's named transaction entries.
/// Only the concrete SQLite specialization exposes its connection through
/// dereferencing; generic store implementations cannot reach it.
pub struct StoreQueue<E: TransactionEntries> {
    pub(super) entries: E,
    /// The control side's run directory (next to the DB for SQLite). A run's directory, worktree,
    /// receipt and log are resolved under it by run ID, never read from the
    /// absolute paths stored at claim time, so a moved queue keeps its runs.
    pub(super) runs_dir: PathBuf,
    /// Where every time the queue writes and every run ID it creates come
    /// from; the system clock and random UUIDs unless a test fixes them.
    pub(super) generators: Generators,
    /// Who the events written through this connection record
    /// (ADR-t728-1 decision 4).
    pub(super) actors: EventActors,
    /// The repository's set of changes (`[tasks] changes` of dagq.toml,
    /// ADR-t980-1) that `add`, `edit`, `submit` and `lint` hold the tasks
    /// to; none accepts any change and a task without one.
    pub(super) changes: Option<ChangeSet>,
    /// The repository's prefixes of a task's title (`[tasks]
    /// title_prefixes` of dagq.toml) that `submit` and `lint` hold the
    /// titles to; none checks no title.
    pub(super) title_prefixes: Option<TitlePrefixes>,
    /// The repository's set of goal tags (`[goals] tags` of dagq.toml,
    /// ADR-t1639-1 decision 6) that `goal add`, `goal edit`, `submit` and
    /// `lint` hold the goals to; none accepts any tag and a goal without one.
    pub(super) goal_tags: Option<TagSet>,
    /// `[goals] max_spike_cycles` (ADR-t1487-1 decision 8), which `submit`
    /// holds a planner of the runtime's to; none limits nothing.
    pub(super) max_spike_cycles: Option<usize>,
    /// The replacement plan `submit --replacement` goes with
    /// ([`crate::domain::replan_plan`]); none for any other submit.
    pub(super) replacement: Option<crate::domain::replan_plan::ReplacementPlan>,
    /// Where a refusal of the policy is recorded instead of on this
    /// connection (the independent denial recorder).
    pub(super) denial_log: Option<DenialRecorder>,
}

/// What records a refusal of the policy, given its event's payload.
pub type DenialRecorder = Arc<dyn Fn(serde_json::Value) -> Result<()> + Send + Sync>;

impl<E: TransactionEntries> TransactionEntries for StoreQueue<E> {
    type Write<'store>
        = E::Write<'store>
    where
        Self: 'store;
    type Read<'store>
        = E::Read<'store>
    where
        Self: 'store;

    fn write_entry(&self, name: &'static str) -> Result<Self::Write<'_>> {
        self.entries.write_entry(name)
    }

    fn read_entry(&self, name: &'static str) -> Result<Self::Read<'_>> {
        self.entries.read_entry(name)
    }
}
