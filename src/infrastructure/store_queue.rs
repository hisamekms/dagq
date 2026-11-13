//! Queue state shared by store adapters. The entries own the connection and
//! transaction policy; shared store implementations can only use their port.

use super::sqlite::PlanningRules;
use super::{
    event_actor::EventActors,
    sql_exec::{CommitTransaction, SqlValue, SqlWrite, TransactionEntries},
};
use crate::application::Generators;
use crate::domain::actor::ActorContext;
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
    /// The trusted dispatcher's concrete use case for authorization records.
    pub(super) audit_use_case: Option<crate::domain::queue_service::UseCase>,
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

impl<E: TransactionEntries> StoreQueue<E> {
    /// Attach the parsed dispatch entry, never a caller-provided audit field.
    pub fn set_audit_use_case(&mut self, use_case: crate::domain::queue_service::UseCase) {
        self.audit_use_case = Some(use_case);
    }

    /// The queue reading the time and creating IDs through `generators`
    /// instead of the system clock and random UUIDs.
    pub fn with_generators(mut self, generators: Generators) -> Self {
        self.generators = generators;
        self
    }

    /// The queue holding the tasks to the repository's set of changes
    /// (ADR-t980-1); `None` holds them to none.
    pub fn with_changes(mut self, changes: Option<ChangeSet>) -> Self {
        self.changes = changes;
        self
    }

    /// The queue holding the tasks' titles to the repository's prefixes;
    /// `None` holds them to none.
    pub fn with_title_prefixes(mut self, prefixes: Option<TitlePrefixes>) -> Self {
        self.title_prefixes = prefixes;
        self
    }

    /// The queue holding the goals to the repository's set of tags
    /// (ADR-t1639-1 decision 6); `None` holds them to none.
    /// With `[goals] max_spike_cycles` (ADR-t1487-1 decision 8).
    pub fn with_max_spike_cycles(mut self, max: Option<usize>) -> Self {
        self.max_spike_cycles = max;
        self
    }

    /// The queue whose next `submit` goes with the replacement plan `plan`
    /// of a replanning operation's request (ADR-t1521-2 decision 4).
    pub fn with_replacement_plan(
        mut self,
        plan: Option<crate::domain::replan_plan::ReplacementPlan>,
    ) -> Self {
        self.replacement = plan;
        self
    }

    pub fn with_goal_tags(mut self, tags: Option<TagSet>) -> Self {
        self.goal_tags = tags;
        self
    }

    /// [`Self::with_changes`], [`Self::with_goal_tags`],
    /// [`Self::with_max_spike_cycles`], [`Self::with_title_prefixes`] and
    /// [`Self::with_replacement_plan`] on
    /// a queue held by reference, each set only when given: the rules a
    /// planning command holds its tasks and goals to.
    pub fn hold_planning_to(&mut self, rules: PlanningRules) {
        if let Some(changes) = rules.changes {
            self.changes = changes;
        }
        if let Some(tags) = rules.goal_tags {
            self.goal_tags = tags;
        }
        if let Some(max) = rules.max_spike_cycles {
            self.max_spike_cycles = max;
        }
        if let Some(prefixes) = rules.title_prefixes {
            self.title_prefixes = prefixes;
        }
        if let Some(plan) = rules.replacement {
            self.replacement = plan;
        }
    }

    /// Record the refusals of the policy with `log`: for a read-only
    /// connection, which cannot write them.
    pub fn with_denial_log(mut self, log: DenialRecorder) -> Self {
        self.denial_log = Some(log);
        self
    }

    pub fn generators(&self) -> &Generators {
        &self.generators
    }

    /// The queue writing its events as `actor` instead of the process's
    /// actor ([`process_actor`]).
    pub fn with_actor(self, actor: ActorContext) -> Self {
        self.actors.set(actor);
        self
    }

    /// The actor this queue's events record.
    pub fn actor(&self) -> ActorContext {
        self.actors.get()
    }

    /// A single-statement write still uses the named, fenced transaction
    /// entry, with exactly the statement's affected-row result.
    pub(super) fn write_sql(
        &self,
        name: &'static str,
        sql: &str,
        params: &[SqlValue],
    ) -> Result<usize> {
        let tx = self.write_entry(name)?;
        let changed = SqlWrite::execute(&tx, sql, params)?;
        tx.commit()?;
        Ok(changed)
    }

    /// Read transcripts through the session execution port before a write
    /// entry holds its lock.
    pub(super) fn read_session_transcripts_before(
        &self,
        closing: super::sessions::Closing<'_>,
    ) -> Result<super::sessions::ReadBefore> {
        self.read_before(closing)
    }
}
