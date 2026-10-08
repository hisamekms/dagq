//! The observation and analysis context's reads of the queue (its
//! operations in docs/design/architecture.md): `status`, `events`,
//! `timeline`, `stats`, `kpi`, `forecast`, `notes`, `marks`, `findings` and
//! `observe --history|--input`.
//!
//! Each read takes the observation context's ports ([`EventReads`],
//! [`ObserverLog`], [`QueueRecords`]) and only the reads the other
//! contexts open to every context: the execution context's [`RunLog`]
//! (the events and the marks) and the planning context's [`TaskStore`]
//! (the notes).

use anyhow::Result;
use serde_json::{Value, json};

use super::{
    EventsRead, FindingsRead, ForecastRead, KpiRead, MarksRead, NotesRead, ObserveHistoryRead,
    ObserveInputRead, RoleRead, StatsRead, TimelineRead,
};
use crate::application::forecast::ForecastQuery;
use crate::application::{
    Clock, EventReads, ObserverLog, QueueRecords, RunLog, TaskStore, observer, watch,
};
use crate::domain::kpi::KpiQuery;
use crate::domain::stats::StatsQuery;
use crate::domain::{
    EventFilter, EventId, FindingId, FindingQuery, FindingTarget, GoalId, NoteQuery, RunId,
    SessionRole, TaskId,
};

/// What the observation reads take beyond the queue's records that the
/// composition root assembles: the reads composed with the host
/// (`status`, `stats`, `kpi`, `forecast`, the improvements), the
/// observation's input files and the clock.
pub trait ObservationSources<Q: ?Sized> {
    fn status(&self, queue: &Q, role: Option<SessionRole>) -> Result<Value>;
    fn stats(&self, queue: &Q, query: &StatsQuery) -> Result<Value>;
    fn kpi(&self, queue: &Q, query: &KpiQuery) -> Result<Value>;
    fn forecast(&self, queue: &Q, query: &ForecastQuery) -> Result<Value>;
    /// The limit of the improvement proposals and the ones waiting
    /// (`findings`' `improvements`).
    fn improvements(&self, queue: &Q) -> Result<Value>;
    /// A page of an observation's input (`observe --input`).
    fn observe_input(&self, read: &ObserveInputRead) -> Result<Value>;
    /// The clock of `queue` (`timeline`'s time since the last event).
    fn clock<'q>(&self, queue: &'q Q) -> &'q dyn Clock;
}

/// `status`: the attention of the role named, every role's without one.
pub fn status<Q: ?Sized>(
    queue: &Q,
    sources: &(impl ObservationSources<Q> + ?Sized),
    read: &RoleRead,
) -> Result<Value> {
    sources.status(queue, super::session_role(read.role.as_ref())?)
}

/// `events`.
pub fn events(queue: &(impl RunLog + EventReads), read: &EventsRead) -> Result<Value> {
    watch::events_in(queue, &events_query(read)?)
}

/// `timeline`: one run's events in spans, its gaps measured on the
/// queue's clock.
pub fn timeline<Q: RunLog + EventReads>(
    queue: &Q,
    sources: &(impl ObservationSources<Q> + ?Sized),
    read: &TimelineRead,
) -> Result<Value> {
    watch::timeline_in(
        queue,
        &RunId::new(read.run.clone())?,
        read.gap,
        read.full,
        sources.clock(queue),
    )
}

/// `stats`.
pub fn stats<Q: ?Sized>(
    queue: &Q,
    sources: &(impl ObservationSources<Q> + ?Sized),
    read: &StatsRead,
) -> Result<Value> {
    sources.stats(
        queue,
        &StatsQuery {
            since: read.since,
            until: read.until,
            goal_id: read.goal.map(GoalId::new),
            full: read.full,
        },
    )
}

/// `kpi`.
pub fn kpi<Q: ?Sized>(
    queue: &Q,
    sources: &(impl ObservationSources<Q> + ?Sized),
    read: &KpiRead,
) -> Result<Value> {
    sources.kpi(queue, &kpi_query(read)?)
}

/// `forecast`.
pub fn forecast<Q: ?Sized>(
    queue: &Q,
    sources: &(impl ObservationSources<Q> + ?Sized),
    read: &ForecastRead,
) -> Result<Value> {
    sources.forecast(
        queue,
        &ForecastQuery {
            task_id: read.task.map(TaskId::new),
            goal_id: read.goal.map(GoalId::new),
            parallel: read.parallel.map(usize::from),
            trials: read.trials as usize,
        },
    )
}

/// `notes`.
pub fn notes(queue: &(impl TaskStore + ?Sized), read: &NotesRead) -> Result<Value> {
    Ok(serde_json::to_value(queue.notes(&NoteQuery {
        goal_id: read.goal.map(GoalId::new),
        task_id: read.task.map(TaskId::new),
        since: read.since.map(EventId::new),
        limit: usize::try_from(read.limit)?,
    })?)?)
}

/// `marks`: the recorded and the derived marks between the cursors.
pub fn marks(queue: &(impl RunLog + ?Sized), read: &MarksRead) -> Result<Value> {
    let marks = crate::domain::marks::marks(&queue.all_events()?, read.since, read.until);
    Ok(json!({ "marks": marks }))
}

/// `findings`, with the limit of the improvements beside them.
pub fn findings<Q: QueueRecords + ?Sized>(
    queue: &Q,
    sources: &(impl ObservationSources<Q> + ?Sized),
    read: &FindingsRead,
) -> Result<Value> {
    let findings = queue.findings(&finding_query(read)?)?;
    // The limit's settings not reading does not hide the findings.
    let improvements = sources
        .improvements(queue)
        .unwrap_or_else(|error| json!({"error": format!("{error:#}")}));
    Ok(json!({"findings": findings, "improvements": improvements}))
}

/// `observe --history`.
pub fn observe_history(queue: &dyn ObserverLog, read: &ObserveHistoryRead) -> Result<Value> {
    observer::history(queue, read.limit)
}

/// `observe --input`.
pub fn observe_input<Q: ?Sized, S: ObservationSources<Q> + ?Sized>(
    sources: &S,
    read: &ObserveInputRead,
) -> Result<Value> {
    sources.observe_input(read)
}

/// `events`' filters as the query of [`watch::events_in`].
pub(super) fn events_query(read: &EventsRead) -> Result<watch::EventsQuery> {
    Ok(watch::EventsQuery {
        after: EventId::new(read.after),
        limit: read.limit as usize,
        all: read.all,
        full: read.full,
        filter: EventFilter {
            kinds: (!read.kind.is_empty()).then(|| read.kind.clone()),
            run: read.run.clone().map(RunId::new).transpose()?,
            task: read.task.map(TaskId::new),
            goal: read.goal.map(GoalId::new),
            since: read.since.as_deref().map(watch::event_time).transpose()?,
            until: read.until.as_deref().map(watch::event_time).transpose()?,
        },
    })
}

pub(super) fn kpi_query(read: &KpiRead) -> Result<KpiQuery> {
    Ok(KpiQuery {
        period: read.period.parse().map_err(anyhow::Error::msg)?,
        last: usize::from(read.last),
        at: read.at,
        since: read.since,
        until: read.until,
        changes: read.changes.clone(),
        areas: read.areas.clone(),
        by: read
            .by
            .iter()
            .map(|axis| axis.parse())
            .collect::<Result<_, String>>()
            .map_err(anyhow::Error::msg)?,
        cross: read.cross,
        compare: read.compare,
        window_days: read.window,
        goal_id: read.goal.map(GoalId::new),
    })
}

/// `findings`' query: the first of `--task`, `--run`, `--goal` and
/// `--queue` given is its target.
pub(super) fn finding_query(read: &FindingsRead) -> Result<FindingQuery> {
    let target = match (read.task, &read.run, read.goal) {
        (Some(task), _, _) => Some(FindingTarget::Task(TaskId::new(task))),
        (_, Some(run), _) => Some(FindingTarget::Run(RunId::new(run.clone())?)),
        (_, _, Some(id)) => Some(FindingTarget::Goal(GoalId::new(id))),
        _ => read.queue.then_some(FindingTarget::Queue),
    };
    Ok(FindingQuery {
        id: read.id.map(FindingId::new),
        all: read.all,
        statuses: read
            .status
            .iter()
            .map(|value| value.parse())
            .collect::<Result<_, _>>()?,
        kinds: read.kinds.clone(),
        target,
        full: read.full,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::application::queue_reads::fakes::Records;

    /// The host's sources with only the improvements, which read as
    /// `improvements` says.
    struct Improvements(Result<Value, &'static str>);

    impl ObservationSources<Records> for Improvements {
        fn status(&self, _: &Records, _: Option<SessionRole>) -> Result<Value> {
            unimplemented!()
        }
        fn stats(&self, _: &Records, _: &StatsQuery) -> Result<Value> {
            unimplemented!()
        }
        fn kpi(&self, _: &Records, _: &KpiQuery) -> Result<Value> {
            unimplemented!()
        }
        fn forecast(&self, _: &Records, _: &ForecastQuery) -> Result<Value> {
            unimplemented!()
        }
        fn improvements(&self, _: &Records) -> Result<Value> {
            self.0.clone().map_err(anyhow::Error::msg)
        }
        fn observe_input(&self, _: &ObserveInputRead) -> Result<Value> {
            unimplemented!()
        }
        fn clock<'q>(&self, _: &'q Records) -> &'q dyn Clock {
            unimplemented!()
        }
    }

    fn read(params: Value) -> FindingsRead {
        serde_json::from_value(params).unwrap()
    }

    #[test]
    fn findings_list_the_records_findings_of_the_target_with_the_improvements_beside() {
        let records = Records::default();
        let sources = Improvements(Ok(json!({"limit": 2})));
        let value = findings(&records, &sources, &read(json!({"goal": 7, "all": true}))).unwrap();
        assert_eq!(value, json!({"findings": [], "improvements": {"limit": 2}}));
        let queries = records.findings.borrow();
        assert_eq!(queries.len(), 1);
        assert_eq!(queries[0].target, Some(FindingTarget::Goal(GoalId::new(7))));
        assert!(queries[0].all);
    }

    #[test]
    fn findings_still_show_when_the_improvements_do_not_read() {
        let records = Records::default();
        let sources = Improvements(Err("the limit's settings do not read"));
        let value = findings(&records, &sources, &read(json!({}))).unwrap();
        assert_eq!(
            value,
            json!({"findings": [],
                   "improvements": {"error": "the limit's settings do not read"}})
        );
    }
}
