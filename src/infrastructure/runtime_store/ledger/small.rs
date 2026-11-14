//! Indexed event inputs for a ledger window. Shared by SQLite and Postgres;
//! selection order is independent of the visibility predicate's saved rows.
use super::*;
use crate::application::{
    CursorRecordTime, CursorTimePath, RunFinish, WINDOW_READ_CANDIDATES, WindowRead,
};
use crate::domain::{marks, stats::timestamp_millis};
use std::collections::BTreeSet;

enum Failure {
    Sql(anyhow::Error),
    Unavailable,
    Exhausted,
    OffTimeForm,
}
impl From<anyhow::Error> for Failure {
    fn from(error: anyhow::Error) -> Self {
        Self::Sql(error)
    }
}
type Read<T> = std::result::Result<T, Failure>;
fn answer<T>(read: Read<T>) -> Result<WindowRead<T>> {
    match read {
        Ok(value) => Ok(WindowRead::Complete(value)),
        Err(Failure::Sql(error)) => Err(error),
        Err(Failure::Unavailable) => Ok(WindowRead::Unavailable),
        Err(Failure::Exhausted) => Ok(WindowRead::Exhausted),
        Err(Failure::OffTimeForm) => Ok(WindowRead::OffTimeForm),
    }
}
fn dedup(mut events: Vec<RunEvent>) -> Vec<RunEvent> {
    events.sort_by_key(|e| e.id);
    events.dedup_by_key(|e| e.id);
    events
}
fn ms(event: &RunEvent) -> i64 {
    timestamp_millis(&event.created_at).unwrap_or(i64::MIN)
}
/// Borrows the snapshot's named read transaction for selection and visibility.
struct Reader<'a, T> {
    tx: &'a T,
}
impl<T: SqlRead> Reader<'_, T> {
    fn field(&self, key: &str, ty: JsonType) -> String {
        self.tx
            .dialect()
            .json_field("payload", &format!("$.{key}"), ty)
    }
    fn table(&self, index: &str) -> String {
        format!("run_events{}", self.tx.dialect().indexed_by(index))
    }
    fn ready(&self) -> Read<()> {
        self.visible(&[])?;
        // The complete INTEGER key domain, including imported negative ids:
        // an index seek over the off-form entries alone, with no table walk.
        let sql = format!(
            "SELECT EXISTS(SELECT 1 FROM {} WHERE {} AND id >= -9223372036854775808)",
            self.table("events_off_the_record_form"),
            self.tx.dialect().off_the_record_form("created_at")
        );
        if SqlRead::query_row(self.tx, &sql, sql_params![], |r| r.get::<_, bool>(0))
            .map_err(anyhow::Error::from)?
        {
            return Err(Failure::OffTimeForm);
        }
        Ok(())
    }
    fn visible(&self, events: &[RunEvent]) -> Read<Vec<RunEvent>> {
        visibility::visible_in(self.tx, events)?.ok_or(Failure::Unavailable)
    }
    fn query(
        &self,
        table: &str,
        condition: &str,
        order: &str,
        params: &[SqlValue],
        limit: Option<usize>,
    ) -> Read<Vec<RunEvent>> {
        let limit = limit.map(|n| format!(" LIMIT {n}")).unwrap_or_default();
        let sql = format!("SELECT * FROM {table} WHERE {condition} ORDER BY {order}{limit}");
        Ok(SqlRead::query_map(self.tx, &sql, params, event_row).map_err(anyhow::Error::from)?)
    }
    fn all(&self, index: &str, condition: &str, params: &[SqlValue]) -> Read<Vec<RunEvent>> {
        self.visible(&self.query(&self.table(index), condition, "id", params, None)?)
    }
    /// Limit BEFORE visibility; never select a single invisible candidate and
    /// call it absent. The extra probe returns only a scalar, not an event.
    fn first(
        &self,
        index: &str,
        condition: &str,
        order: &str,
        params: &[SqlValue],
    ) -> Read<Option<RunEvent>> {
        let table = self.table(index);
        let (found, walked) = self.walk(&table, condition, order, params, |_| true)?;
        if found.is_some() {
            return Ok(found);
        }
        if walked == WINDOW_READ_CANDIDATES {
            let sql = format!(
                "SELECT EXISTS(SELECT 1 FROM {table} WHERE {condition} ORDER BY {order} LIMIT 1 OFFSET {WINDOW_READ_CANDIDATES})"
            );
            if SqlRead::query_row(self.tx, &sql, params, |r| r.get::<_, bool>(0))
                .map_err(anyhow::Error::from)?
            {
                return Err(Failure::Exhausted);
            }
        }
        Ok(None)
    }
    /// Count every event visited, including entries rejected by eligibility,
    /// but stop immediately at the first visible match.
    fn walk(
        &self,
        table: &str,
        condition: &str,
        order: &str,
        params: &[SqlValue],
        eligible: impl Fn(&RunEvent) -> bool,
    ) -> Read<(Option<RunEvent>, usize)> {
        let sql = format!(
            "SELECT * FROM {table} WHERE {condition} ORDER BY {order} LIMIT {WINDOW_READ_CANDIDATES}"
        );
        let mut walked = 0;
        let mut found = None;
        let mut failure = None;
        SqlRead::visit_rows(self.tx, &sql, params, &mut |row| {
            walked += 1;
            let event = event_row(row)?;
            if !eligible(&event) {
                return Ok(true);
            }
            match self.visible(&[event]) {
                Ok(events) => found = events.into_iter().next(),
                Err(error) => failure = Some(error),
            }
            Ok(found.is_none() && failure.is_none())
        })
        .map_err(anyhow::Error::from)?;
        if let Some(error) = failure {
            return Err(error);
        }
        Ok((found, walked))
    }
    fn by_time(&self, index: &str, condition: &str, start: i64, end: i64) -> Read<Vec<RunEvent>> {
        self.all(
            index,
            &format!("{condition} AND created_at > ?1 AND created_at <= ?2"),
            sql_params![marks::utc_text(start), marks::utc_text(end)],
        )
    }
    fn recorded(&self, start: i64, end: i64) -> Read<Vec<RunEvent>> {
        let mut events = Vec::new();
        for (kind, index) in RECORDED_TIME {
            events.extend(self.by_time(index, &format!("kind = '{kind}'"), start, end)?);
        }
        let at = self.field("at", JsonType::Text);
        events.extend(self.all(
            "events_by_mark_at",
            &format!("kind = 'mark_recorded' AND {at} > ?1 AND {at} <= ?2"),
            sql_params![marks::utc_text(start), marks::utc_text(end)],
        )?);
        let heartbeat = self.field("last_heartbeat_at", JsonType::Integer);
        events.extend(self.all(
            "events_by_pruned_heartbeat",
            &format!("kind = 'supervisor_stopped' AND {heartbeat} >= ?1 AND {heartbeat} <= ?2"),
            sql_params![start.div_euclid(1000), end.div_euclid(1000)],
        )?);
        Ok(dedup(events))
    }
    fn retractions(&self, events: &[RunEvent]) -> Read<Vec<RunEvent>> {
        let mut retractions = Vec::new();
        for event in events
            .iter()
            .filter(|e| e.kind.is_in(&marks::RETRACTABLE_KINDS))
        {
            let mark = self.field("mark", JsonType::Integer);
            retractions.extend(self.all(
                "events_by_retracted_mark",
                &format!("kind = 'mark_retracted' AND {mark} = ?1"),
                sql_params![event.id],
            )?);
        }
        Ok(retractions)
    }
    fn claims(&self, start: i64, end: i64) -> Read<Vec<RunEvent>> {
        let claims = self.by_time(
            "events_by_time",
            "kind = 'run_claimed' AND run_id IS NOT NULL",
            start,
            end,
        )?;
        let mut events = claims.clone();
        for claim in &claims {
            for (attribute, start_key) in marks::DERIVED {
                let Some(value) = marks::claim_value(&claim.payload, attribute) else {
                    continue;
                };
                let there = |key: &str| format!("{} IS NOT NULL", self.field(key, JsonType::Text));
                let condition = if attribute == "toolchain" {
                    format!("{} AND {}", there("rustc_release"), there("rustc_host"))
                } else {
                    there(attribute)
                };
                let Some(previous) = self.first(
                    &format!("claims_with_{attribute}"),
                    &format!(
                        "kind = 'run_claimed' AND {condition} AND run_id IS NOT NULL AND id < ?1"
                    ),
                    "id DESC",
                    sql_params![claim.id],
                )?
                else {
                    continue;
                };
                if let Some(key) = start_key
                    && marks::claim_value(&previous.payload, attribute).as_ref() != Some(&value)
                {
                    let ty = if key == "parallel" {
                        JsonType::Integer
                    } else {
                        JsonType::Text
                    };
                    let field = self.field(key, ty);
                    let parameter = if key == "parallel" {
                        // The index is numeric; unusual legacy values cannot be
                        // safely equated through a numeric cast.
                        match claim.payload[key].as_i64() {
                            Some(n) => n.to_sql_value(),
                            None => return Err(Failure::Unavailable),
                        }
                    } else {
                        value.to_sql_value()
                    };
                    if let Some(start) = self.first(&format!("starts_by_{key}"),
                        &format!("kind = 'supervisor_started' AND task_id IS NULL AND goal_id IS NULL AND run_id IS NULL AND {field} = ?1 AND id > ?2 AND id < ?3"),
                        "id", &[parameter, previous.id.to_sql_value(), claim.id.to_sql_value()])? { events.push(start); }
                }
                events.push(previous);
            }
        }
        Ok(dedup(events))
    }
    fn marks(&self, start: i64, end: i64) -> Read<Vec<RunEvent>> {
        let mut events = self.recorded(start, end)?;
        events.extend(self.retractions(&events)?);
        events.extend(self.claims(start, end)?);
        events.extend(self.first("starts_by_time", "kind = 'supervisor_started' AND task_id IS NULL AND goal_id IS NULL AND run_id IS NULL AND created_at <= ?1",
            "created_at DESC, id DESC", sql_params![marks::utc_text(start)])?);
        Ok(dedup(events))
    }
    fn cursor(&self, cursor: EventId) -> Read<CursorRecordTime> {
        // All non-null text values are >= the empty text, including the
        // off-form values rejected above; this is the time index's low edge.
        let (recent, _) = self.walk(
            &self.table("events_by_time"),
            "created_at >= ''",
            "created_at DESC, id DESC",
            sql_params![],
            |e| e.id <= cursor,
        )?;
        if let Some(e) = recent {
            return Ok(CursorRecordTime {
                millis: ms(&e),
                path: CursorTimePath::Recent,
            });
        }
        let at: Option<String> = SqlRead::query_row(
            self.tx,
            "SELECT MAX(created_at) FROM run_events WHERE id <= ?1",
            sql_params![cursor],
            |r| r.get(0),
        )
        .map_err(anyhow::Error::from)?;
        let Some(at) = at else {
            return Ok(CursorRecordTime {
                millis: i64::MIN,
                path: CursorTimePath::Aggregate,
            });
        };
        // Equal-time events can have different owners. The bounded walk checks
        // all of those ahead of older times and records its fallback too.
        let event = self.first(
            "events_by_time",
            "created_at >= '' AND id <= ?1",
            "created_at DESC, id DESC",
            sql_params![cursor],
        )?;
        Ok(CursorRecordTime {
            millis: event.as_ref().map_or(i64::MIN, ms),
            path: if event.as_ref().is_some_and(|e| e.created_at == at) {
                CursorTimePath::Aggregate
            } else {
                CursorTimePath::VisibleWalk
            },
        })
    }
    /// [`Self::cursor`] of the events of a time at or before `end` alone.
    fn cursor_through(&self, cursor: EventId, end: i64) -> Read<CursorRecordTime> {
        let end = marks::utc_text(end);
        let (recent, _) = self.walk(
            &self.table("events_by_time"),
            "created_at >= '' AND created_at <= ?1",
            "created_at DESC, id DESC",
            sql_params![end.clone()],
            |e| e.id <= cursor,
        )?;
        if let Some(e) = recent {
            return Ok(CursorRecordTime {
                millis: ms(&e),
                path: CursorTimePath::Recent,
            });
        }
        let at: Option<String> = SqlRead::query_row(
            self.tx,
            "SELECT MAX(created_at) FROM run_events WHERE id <= ?1 AND created_at <= ?2",
            sql_params![cursor, end.clone()],
            |r| r.get(0),
        )
        .map_err(anyhow::Error::from)?;
        let Some(at) = at else {
            return Ok(CursorRecordTime {
                millis: i64::MIN,
                path: CursorTimePath::Aggregate,
            });
        };
        let event = self.first(
            "events_by_time",
            "created_at >= '' AND id <= ?1 AND created_at <= ?2",
            "created_at DESC, id DESC",
            sql_params![cursor, end],
        )?;
        Ok(CursorRecordTime {
            millis: event.as_ref().map_or(i64::MIN, ms),
            path: if event.as_ref().is_some_and(|e| e.created_at == at) {
                CursorTimePath::Aggregate
            } else {
                CursorTimePath::VisibleWalk
            },
        })
    }
    fn task_end(&self) -> String {
        format!(
            "kind = 'task_status_changed' AND {} IN ('completed', 'canceled')",
            self.field("to", JsonType::Text)
        )
    }
    /// Snapshots must all be read for scoring within the open period, but
    /// their extrema still obey the bounded visibility walk. Do not compute
    /// an invisible minimum and drop it, nor walk arbitrarily many hidden
    /// snapshots to repair it. Equal values are walked by id.
    fn snapshot_min(&self, snapshots: &[RunEvent], key: &str, positive: bool) -> Read<Option<i64>> {
        let mut candidates: Vec<_> = snapshots
            .iter()
            .filter_map(|e| {
                let value = e.payload[key].as_i64()?;
                (!positive || value > 0).then_some((value, e))
            })
            .collect();
        candidates.sort_by_key(|(value, e)| (*value, e.id));
        for (value, event) in candidates.iter().take(WINDOW_READ_CANDIDATES) {
            if !self.visible(std::slice::from_ref(*event))?.is_empty() {
                return Ok(Some(*value));
            }
        }
        if candidates.len() > WINDOW_READ_CANDIDATES {
            return Err(Failure::Exhausted);
        }
        Ok(None)
    }
    fn last_visible_id(&self, candidates: &[RunEvent]) -> Read<Option<EventId>> {
        let mut by_id: Vec<_> = candidates.iter().collect();
        by_id.sort_by_key(|event| std::cmp::Reverse(event.id));
        for event in by_id.iter().take(WINDOW_READ_CANDIDATES) {
            if !self.visible(std::slice::from_ref(*event))?.is_empty() {
                return Ok(Some(event.id));
            }
        }
        if by_id.len() > WINDOW_READ_CANDIDATES {
            return Err(Failure::Exhausted);
        }
        Ok(None)
    }
    fn forecast(&self, start: i64, end: i64) -> Read<Vec<RunEvent>> {
        let time_range = "created_at > ?1 AND created_at <= ?2";
        let mut candidates = self.query(
            &self.table("task_ends_by_time"),
            &format!(
                "{} AND task_id IS NOT NULL AND {time_range}",
                self.task_end()
            ),
            "id",
            sql_params![marks::utc_text(start), marks::utc_text(end)],
            None,
        )?;
        candidates.extend(self.query(
            &self.table("goal_ends_by_time"),
            &format!("kind = 'goal_closed' AND goal_id IS NOT NULL AND {time_range}"),
            "id",
            sql_params![marks::utc_text(start), marks::utc_text(end)],
            None,
        )?);
        let Some(high) = self.last_visible_id(&candidates)? else {
            return Ok(Vec::new());
        };
        let finishes = self.visible(&candidates)?;
        let mut low = high;
        let mut events = finishes.clone();
        for finish in &finishes {
            let (column, target, index, condition) = if finish.kind == EventKind::GoalClosed {
                let Some(goal) = finish.goal_id else {
                    continue;
                };
                (
                    "goal_id",
                    goal.to_sql_value(),
                    "goal_ends_by_goal",
                    "kind = 'goal_closed'".to_owned(),
                )
            } else {
                let Some(task) = finish.task_id else {
                    continue;
                };
                (
                    "task_id",
                    task.to_sql_value(),
                    "task_ends_by_task",
                    self.task_end(),
                )
            };
            let previous = self.first(
                index,
                &format!("{column} = ?1 AND {condition} AND id < ?2"),
                "id DESC",
                &[target.clone(), finish.id.to_sql_value()],
            )?;
            let bound = match previous {
                Some(previous) => {
                    let id = previous.id;
                    events.push(previous);
                    id
                }
                None => {
                    let index = if column == "task_id" {
                        "events_by_task"
                    } else {
                        "events_by_goal"
                    };
                    self.first(index, &format!("{column} = ?1"), "id", &[target])?
                        .map_or(EventId::new(0), |e| EventId::new(e.id.as_i64() - 1))
                }
            };
            low = low.min(bound);
        }
        let snapshots = self.query(
            &self.table("events_by_kind"),
            "kind = 'forecast_recorded' AND id > ?1 AND id <= ?2",
            "id",
            sql_params![low, high],
            None,
        )?;
        events.extend(self.forecast_of(&snapshots, end)?);
        Ok(dedup(events))
    }
    /// Each visible finish of a target at or after `from` in time, with
    /// the period of the snapshots its score takes (a score takes each
    /// target's first finish after a snapshot, by id): after the target's
    /// end before that finish by id, through the finish. With no end
    /// before it, a task's period has no start (its snapshots are sought by
    /// the view of the snapshots' tasks) and a goal's starts at its first
    /// event, as [`Self::forecast`]'s. As many as the finishes from `from`,
    /// each one's previous end and a goal's first event.
    fn finish_periods(&self, from: i64) -> Read<Vec<FinishPeriod>> {
        let after = "created_at >= ?1";
        let from = marks::utc_text(from);
        let mut candidates = self.query(
            &self.table("task_ends_by_time"),
            &format!("{} AND task_id IS NOT NULL AND {after}", self.task_end()),
            "id",
            sql_params![from.clone()],
            None,
        )?;
        candidates.extend(self.query(
            &self.table("goal_ends_by_time"),
            &format!("kind = 'goal_closed' AND goal_id IS NOT NULL AND {after}"),
            "id",
            sql_params![from],
            None,
        )?);
        let mut periods = Vec::new();
        for finish in self.visible(&candidates)? {
            let (target, column, value, index, condition) = match (finish.goal_id, finish.task_id) {
                (Some(goal), _) if finish.kind == EventKind::GoalClosed => (
                    SnapshotTarget::Goal(goal.as_i64()),
                    "goal_id",
                    goal.to_sql_value(),
                    "goal_ends_by_goal",
                    "kind = 'goal_closed'".to_owned(),
                ),
                (_, Some(task)) if finish.kind != EventKind::GoalClosed => (
                    SnapshotTarget::Task(task.as_i64()),
                    "task_id",
                    task.to_sql_value(),
                    "task_ends_by_task",
                    self.task_end(),
                ),
                _ => continue,
            };
            let previous = self.first(
                index,
                &format!("{column} = ?1 AND {condition} AND id < ?2"),
                "id DESC",
                &[value.clone(), finish.id.to_sql_value()],
            )?;
            let after = match (previous, target) {
                (Some(previous), _) => previous.id.as_i64(),
                (None, SnapshotTarget::Task(_)) => i64::MIN,
                (None, SnapshotTarget::Goal(_)) => self
                    .first("events_by_goal", "goal_id = ?1", "id", &[value])?
                    .map_or(i64::MIN, |first| first.id.as_i64() - 1),
            };
            periods.push(FinishPeriod {
                target,
                after,
                through: finish.id.as_i64(),
            });
        }
        Ok(periods)
    }
    /// The second stage, from snapshots already sought by their targets.
    fn forecast_of(&self, snapshots: &[RunEvent], end: i64) -> Read<Vec<RunEvent>> {
        let mut events = Vec::new();
        let at = self.snapshot_min(snapshots, "at_secs", false)?;
        let seen = self.snapshot_min(snapshots, "triggers_through", true)?;
        let snapshots = self.visible(snapshots)?;
        events.extend(snapshots);
        let Some(at) = at.and_then(|at| at.checked_mul(1000)) else {
            return Ok(dedup(events));
        };
        let mut recorded = self.recorded(at, end)?;
        let mut task_marks = Vec::new();
        for kind in crate::domain::forecast::score::TASK_MARK_KINDS {
            task_marks.extend(self.by_time(
                "events_by_time",
                &format!("kind = '{}' AND task_id IS NOT NULL", kind.as_str()),
                at,
                end,
            )?);
            if let Some(seen) = seen {
                task_marks.extend(self.all(
                    "events_by_kind",
                    &format!("kind = '{}' AND id > ?1", kind.as_str()),
                    sql_params![seen],
                )?);
            }
        }
        if let Some(seen) = seen {
            for kind in marks::RECORDED_KINDS {
                recorded.extend(self.all("events_by_kind", &format!("kind = '{}' AND id > ?1 AND task_id IS NULL AND goal_id IS NULL AND run_id IS NULL", kind.as_str()), sql_params![seen])?);
            }
        }
        let recorded = dedup(recorded);
        events.extend(self.retractions(&recorded)?);
        events.extend(recorded);
        events.extend(task_marks);
        events.extend(self.claims(at, end)?);
        Ok(dedup(events))
    }
    fn end_condition(&self) -> String {
        format!(
            "run_id IS NOT NULL AND task_id IS NOT NULL AND (kind IN ('run_integrated', 'run_satisfied_closed') OR {} IN ('failed', 'interrupted'))",
            self.field("status", JsonType::Text)
        )
    }
    fn finishes(&self, range: Option<(i64, i64)>) -> Read<Vec<RunFinish>> {
        let condition = self.end_condition();
        let candidates = match range {
            Some((start, end)) => self.query(
                &self.table("run_ends_by_time"),
                &format!("{condition} AND created_at > ?1 AND created_at <= ?2"),
                "created_at, id",
                sql_params![marks::utc_text(start), marks::utc_text(end)],
                None,
            )?,
            None => {
                // Aggregate only ids, not ledger records; repair an invisible
                // MIN(id) by walking this run's end index below.
                let sql = format!(
                    "SELECT e.* FROM run_events e JOIN (SELECT MIN(id) AS first FROM {} WHERE {condition} AND run_id >= '' GROUP BY run_id) f ON e.id=f.first",
                    self.table("run_ends_by_run")
                );
                SqlRead::query_map(self.tx, &sql, sql_params![], event_row)
                    .map_err(anyhow::Error::from)?
            }
        };
        // In the cohort read the aggregate already is each run's first end.
        // Keep a visible minimum directly: only invisible minima need repair.
        let mut known: std::collections::BTreeMap<RunId, RunEvent> = if range.is_none() {
            self.visible(&candidates)?
                .into_iter()
                .filter_map(|e| Some((e.run_id.clone()?, e)))
                .collect()
        } else {
            Default::default()
        };
        let runs: BTreeSet<_> = candidates.into_iter().filter_map(|e| e.run_id).collect();
        let mut finishes = Vec::new();
        for run in runs {
            let first = match known.remove(&run) {
                Some(first) => Some(first),
                None => self.first(
                    "run_ends_by_run",
                    &format!("{condition} AND run_id = ?1"),
                    "id",
                    sql_params![run.as_str()],
                )?,
            };
            if let Some(first) = first {
                let millis = ms(&first);
                if range.is_none_or(|(s, e)| millis > s && millis <= e) {
                    finishes.push(RunFinish {
                        run,
                        task: first.task_id.unwrap(),
                        event: first.id,
                        millis,
                    });
                }
            }
        }
        Ok(finishes)
    }
}
const RECORDED_TIME: [(&str, &str); 5] = [
    ("supervisor_started", "starts_by_time"),
    ("supervisor_stopped", "stops_by_time"),
    ("run_env_changed", "run_env_changes_by_time"),
    ("mark_recorded", "marks_by_time"),
    ("mark_retracted", "retractions_by_time"),
];
fn read<T: SqlRead, U>(
    tx: &T,
    action: impl FnOnce(&Reader<'_, T>) -> Read<U>,
) -> Result<WindowRead<U>> {
    let reader = Reader { tx };
    answer((|| {
        reader.ready()?;
        action(&reader)
    })())
}
pub(super) fn mark_events(
    tx: &impl SqlRead,
    start: i64,
    end: i64,
) -> Result<WindowRead<Vec<RunEvent>>> {
    read(tx, |r| r.marks(start, end))
}
pub(super) fn cursor_time(
    tx: &impl SqlRead,
    cursor: EventId,
) -> Result<WindowRead<CursorRecordTime>> {
    read(tx, |r| r.cursor(cursor))
}
pub(super) fn cursor_time_through(
    tx: &impl SqlRead,
    cursor: EventId,
    end_ms: i64,
) -> Result<WindowRead<CursorRecordTime>> {
    read(tx, |r| r.cursor_through(cursor, end_ms))
}
pub(super) fn forecast_events(
    tx: &impl SqlRead,
    start: i64,
    end: i64,
) -> Result<WindowRead<Vec<RunEvent>>> {
    read(tx, |r| r.forecast(start, end))
}
/// What a forecast's snapshot names and a finish ends: a task or a goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SnapshotTarget {
    Task(i64),
    Goal(i64),
}

/// A finish of `target` and the event ids of the snapshots its score takes:
/// after `after` through `through` (the finish).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct FinishPeriod {
    pub(super) target: SnapshotTarget,
    pub(super) after: i64,
    pub(super) through: i64,
}

/// The finishes at or after `from_ms` and their snapshots' periods
/// ([`Reader::finish_periods`]).
pub(super) fn finish_periods(
    tx: &impl SqlRead,
    from_ms: i64,
) -> Result<WindowRead<Vec<FinishPeriod>>> {
    read(tx, |r| r.finish_periods(from_ms))
}
/// Marks after the supplied snapshots, without scanning unrelated snapshots
/// between a target's first and last events.
pub(super) fn forecast_of(
    tx: &impl SqlRead,
    snapshots: &[RunEvent],
    end: i64,
) -> Result<WindowRead<Vec<RunEvent>>> {
    read(tx, |r| r.forecast_of(snapshots, end))
}
pub(super) fn run_finishes(
    tx: &impl SqlRead,
    range: Option<(i64, i64)>,
) -> Result<WindowRead<Vec<RunFinish>>> {
    read(tx, |r| r.finishes(range))
}
pub(super) fn comparison_events(tx: &impl SqlRead) -> Result<WindowRead<Vec<RunEvent>>> {
    read(tx, |r| {
        let mut events = Vec::new();
        for kind in marks::READ_KINDS {
            events.extend(r.all(
                "events_by_kind",
                &format!("kind = '{}'", kind.as_str()),
                sql_params![],
            )?);
        }
        Ok(dedup(events))
    })
}

#[cfg(test)]
mod tests;
