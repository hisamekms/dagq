//! Immutable run facts shared by stats and its KPI period projections.
//! A page resolves event bounds and time cuts once. Event aggregates reuse
//! only identical cursors and end times within this input and goal; token
//! windows keep their own time cuts. Only selected finished runs are enriched.
use super::*;
use std::borrow::Cow;

mod kpi;
pub(super) use kpi::KpiOverall;
pub(in crate::domain) use kpi::KpiStats;

pub(in crate::domain) struct Prepared<'a> {
    events: &'a [RunEvent],
    goals: &'a HashMap<TaskId, Option<GoalId>>,
    now: i64,
    live: Cow<'a, LiveSnapshot>,
    goal: Option<GoalId>,
    event_answers: std::cell::RefCell<HashMap<(EventId, EventId, i64), kpi::EventStats>>,
    enriched_runs: std::cell::RefCell<HashMap<RunId, RunStats>>,
    pub(super) tracks: Vec<Track>,
    first_event: HashMap<&'a str, EventId>,
    spans: Vec<sessions::Span>,
    all_executions: Vec<executions::Execution>,
}

/// A stats page keeps the selected runs separate from the event and time
/// windows: paging limits runs, never the period aggregates.
pub(super) struct Page<'a> {
    pub(super) finished: Vec<&'a Track>,
    pub(super) open: Vec<&'a Track>,
    pub(super) next_cursor: EventId,
    pub(super) window: Window,
}

pub(super) struct Window {
    pub(super) start: EventId,
    pub(super) last: EventId,
    pub(super) end: i64,
    pub(super) from: i64,
    pub(super) until: i64,
    tokens_from: Option<i64>,
}

impl<'a> Prepared<'a> {
    pub(in crate::domain) fn new(
        events: &'a [RunEvent],
        goals: &'a HashMap<TaskId, Option<GoalId>>,
        now: i64,
        live: Cow<'a, LiveSnapshot>,
        goal: Option<GoalId>,
    ) -> Self {
        let mut tracks = runs(events, goals);
        if let Some(ledger) = &live.ledger {
            for track in &mut tracks {
                track.take_ledger(ledger);
            }
        }
        Self {
            events,
            goals,
            now,
            live,
            goal,
            event_answers: Default::default(),
            enriched_runs: Default::default(),
            tracks,
            first_event: first_events(events),
            spans: sessions::spans(events),
            all_executions: executions::executions(events),
        }
    }

    pub(super) fn in_goal(&self, task: TaskId) -> bool {
        self.goal
            .is_none_or(|goal| self.goals.get(&task).copied().flatten() == Some(goal))
    }

    pub(super) fn counts(&self, task: Option<TaskId>) -> bool {
        self.goal.is_none() || task.is_some_and(|task| self.in_goal(task))
    }

    /// The same cursor, paging and window rules serve ordinary stats and
    /// KPI's full time-bounded questions, including ledger-supplied bounds.
    pub(super) fn page(
        &self,
        answers: Option<&crate::domain::ledger::window::bounds::Answers>,
        query: &StatsQuery,
    ) -> Page<'_> {
        debug_assert_eq!(query.goal_id, self.goal);
        let events = self.events;
        let resolve = |cursor: Cursor| match cursor {
            Cursor::Time(ms) => answers
                .and_then(|answers| answers.last_at(ms))
                .unwrap_or_else(|| cursor.event_id(events)),
            Cursor::Event(_) => cursor.event_id(events),
        };
        let since = query.since.map(resolve);
        let until = query.until.map(resolve);
        let latest_event = events.iter().map(|e| e.id).max().unwrap_or(EventId::new(0));
        // A since past until leaves no runs and never moves the cursor back.
        let latest = until.map_or(latest_event, |until| {
            let capped = until.min(latest_event);
            since.map_or(capped, |since| capped.max(since.min(latest_event)))
        });
        let (mut finished, open): (Vec<_>, Vec<_>) = self
            .tracks
            .iter()
            .filter(|track| self.in_goal(track.stats.task_id))
            .partition(|track| track.stats.finished_event_id.is_some());
        finished.retain(|track| {
            since.is_none_or(|since| track.stats.finished_event_id > Some(since))
                && until.is_none_or(|until| track.stats.finished_event_id <= Some(until))
        });
        finished.sort_by_key(|track| track.stats.finished_event_id);
        let mut next_cursor = latest;
        if !query.full && finished.len() > DEFAULT_RUNS {
            if since.is_some() {
                finished.truncate(DEFAULT_RUNS);
                next_cursor = finished
                    .last()
                    .and_then(|track| track.stats.finished_event_id)
                    .unwrap_or(latest);
            } else {
                finished.drain(..finished.len() - DEFAULT_RUNS);
            }
        }
        let start = match since {
            Some(since) => since,
            None if query.full => EventId::new(0),
            None => finished
                .iter()
                .filter_map(|track| self.first_event.get(track.stats.run_id.as_str()))
                .min()
                .map_or(EventId::new(0), |id| EventId::new(id.as_i64() - 1)),
        };
        let end = match events.iter().find(|event| event.id == latest) {
            Some(event) if until.is_some() || latest < latest_event => {
                timestamp_millis(&event.created_at).unwrap_or(self.now * 1000)
            }
            _ => self.now * 1000,
        };
        let cursor_ms = |cursor: Option<Cursor>| match cursor {
            Some(Cursor::Time(millis)) => Some(millis),
            Some(Cursor::Event(id)) => events
                .iter()
                .find(|event| event.id == id)
                .and_then(|event| timestamp_millis(&event.created_at)),
            None => None,
        };
        let window_until = match query.until {
            Some(Cursor::Time(millis)) => millis.min(self.now * 1000),
            _ => end,
        };
        let first_of_events = || {
            events
                .iter()
                .filter(|event| event.id > start && event.id <= latest)
                .filter_map(|event| timestamp_millis(&event.created_at))
                .min()
        };
        let from = cursor_ms(query.since)
            .or_else(|| match answers {
                Some(answers) if !query.full => {
                    answers.first_in(start, latest).unwrap_or_else(|| {
                        crate::domain::ledger::window::bounds::missing(
                            crate::domain::ledger::window::bounds::Bound::FirstIn(start, latest),
                        );
                        first_of_events()
                    })
                }
                _ => first_of_events(),
            })
            .unwrap_or(window_until)
            .min(window_until);
        let tokens_from = cursor_ms(query.since).or_else(|| {
            events
                .iter()
                .find(|event| event.id == start)
                .and_then(|event| timestamp_millis(&event.created_at))
        });
        Page {
            finished,
            open,
            next_cursor,
            window: Window {
                start,
                last: latest,
                end,
                from,
                until: window_until,
                tokens_from,
            },
        }
    }

    fn page_runs(&self, page: &Page<'_>) -> Vec<RunStats> {
        let mut cache = self.enriched_runs.borrow_mut();
        let mut missing: Vec<_> = page
            .finished
            .iter()
            .filter(|track| !cache.contains_key(&track.stats.run_id))
            .map(|track| track.stats.clone())
            .collect();
        if !missing.is_empty() {
            enrich(
                self.events,
                self.now,
                &self.first_event,
                &self.spans,
                &self.all_executions,
                &mut missing,
            );
            cache.extend(missing.into_iter().map(|run| (run.run_id.clone(), run)));
        }
        page.finished
            .iter()
            .map(|track| cache[&track.stats.run_id].clone())
            .collect()
    }

    /// KPI classification needs the full finished run facts, with no
    /// whole-history period aggregates. Later periods reuse their enrichment.
    pub(in crate::domain) fn finished_runs(&self) -> Vec<RunStats> {
        self.page_runs(&self.page(
            None,
            &StatsQuery {
                full: true,
                goal_id: self.goal,
                ..StatsQuery::default()
            },
        ))
    }
}

fn first_events(events: &[RunEvent]) -> HashMap<&str, EventId> {
    let mut first_event: HashMap<&str, EventId> = HashMap::new();
    for event in events {
        if let Some(run_id) = &event.run_id {
            first_event.entry(run_id.as_str()).or_insert(event.id);
        }
    }
    first_event
}

fn enrich(
    events: &[RunEvent],
    now: i64,
    first_event: &HashMap<&str, EventId>,
    spans: &[sessions::Span],
    all_executions: &[executions::Execution],
    finished: &mut [RunStats],
) {
    let run_contexts = context::per_run(&context::records(all_executions));
    let mut reviews: HashMap<&str, Vec<RunEvent>> = HashMap::new();
    for event in events.iter().filter(|event| {
        matches!(
            event.kind.known(),
            Some(
                EventKind::ReviewFinished
                    | EventKind::ReviseRequested
                    | EventKind::ReviseFinished
                    | EventKind::ReviewOutcome
            )
        )
    }) {
        if let Some(run_id) = &event.run_id {
            reviews
                .entry(run_id.as_str())
                .or_default()
                .push(event.clone());
        }
    }
    for track in finished.iter_mut() {
        if let Some(run_events) = reviews.get(track.run_id.as_str()) {
            track.review_reasons = review_reasons::per_run(run_events);
        }
        let run_spans = sessions::run_spans(spans, &track.run_id, now * 1000);
        track.sessions = sessions::per_run(&run_spans);
        track.work_breakdown = work::per_run(run_spans.iter().filter_map(|s| s.work.as_ref()));
        track.tokens = tokens::per_run(
            run_spans
                .iter()
                .filter_map(|s| Some((s.kind.as_str(), s.tokens.as_ref()?))),
        );
        track.session_spans = run_spans;
        track.context = run_contexts.get(&track.run_id).cloned();
    }
    predictions::attach(
        events,
        first_event,
        &mut finished.iter_mut().collect::<Vec<_>>(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// KPI periods reuse run facts, and every field they consume still
    /// matches the ordinary full stats answer for time and goal bounds,
    /// including distinct time cuts that resolve to the same event cursors.
    /// Each answer is read again to exercise reuse against fresh stats.
    /// Both readers share aggregation; independent field assertions and the
    /// application frozen goldens separately guard the values themselves.
    #[test]
    fn repeated_windows_keep_uncached_answers() {
        for input in crate::domain::ledger::testkit::frozen::inputs()
            .into_iter()
            .filter(|input| matches!(input.name, "reached" | "clock back" | "no r"))
        {
            let events = &input.log.events;
            let goals = HashMap::from([(TaskId::new(1), Some(GoalId::new(1)))]);
            let live = LiveSnapshot::default();
            let now = crate::domain::ledger::testkit::T0 + input.now;
            for goal_id in [None, Some(GoalId::new(1))] {
                let prepared = Prepared::new(events, &goals, now, Cow::Borrowed(&live), goal_id);
                let expected = super::super::stats_answering(
                    None,
                    events,
                    &goals,
                    now,
                    SlotSnapshot::default(),
                    &StatsQuery {
                        goal_id,
                        full: true,
                        ..StatsQuery::default()
                    },
                    &live,
                );
                assert_eq!(
                    serde_json::to_value(prepared.finished_runs()).unwrap(),
                    serde_json::to_value(expected.runs).unwrap(),
                    "{} goal {goal_id:?}",
                    input.name
                );
                for (start, end) in [
                    (
                        crate::domain::ledger::testkit::ms(input.times[0]),
                        now * 1000,
                    ),
                    (now * 1000 + 1000, now * 1000 + 10000),
                    (now * 1000 + 2000, now * 1000 + 9000),
                    (
                        crate::domain::ledger::testkit::ms(input.times[0]) - 172800000,
                        crate::domain::ledger::testkit::ms(input.times[0]) - 86400000,
                    ),
                ] {
                    let expected = super::super::stats_answering(
                        None,
                        events,
                        &goals,
                        now,
                        SlotSnapshot::default(),
                        &StatsQuery {
                            since: Some(Cursor::Time(start)),
                            until: Some(Cursor::Time(end)),
                            goal_id,
                            full: true,
                        },
                        &live,
                    );
                    let expected = serde_json::to_value(KpiStats::from(expected)).unwrap();
                    for _ in 0..2 {
                        assert_eq!(
                            serde_json::to_value(prepared.kpi_answer(None, start, end)).unwrap(),
                            expected,
                            "{} {start}..{end} goal {goal_id:?}",
                            input.name
                        );
                    }
                }
            }
        }
    }

    /// A late token record has one event cursor in both questions, but its
    /// payload's time belongs to only one. Event reuse must not reuse tokens.
    #[test]
    fn identical_event_bounds_keep_distinct_token_time_cuts() {
        use crate::domain::ledger::testkit::{Log, T0, ms};
        let mut log = Log::default();
        log.add(
            None,
            None,
            "session_tokens",
            serde_json::json!({
                "kind": "inbox", "at": crate::domain::marks::utc_text(ms(100)),
                "tokens": {"input": 10, "output": 1}, "tokens_source": "transcript",
                "tokens_by_model": []
            }),
            2000,
        );
        let goals = HashMap::new();
        let live = LiveSnapshot::default();
        let prepared = Prepared::new(&log.events, &goals, T0 + 3000, Cow::Borrowed(&live), None);
        for (start, end, total) in [(50, 150, 11), (150, 250, 0), (50, 150, 11)] {
            let actual = prepared.kpi_answer(None, ms(start), ms(end));
            assert_eq!(actual.execution_tokens.totals.total, total);
            let expected = super::super::stats_answering(
                None,
                &log.events,
                &goals,
                T0 + 3000,
                SlotSnapshot::default(),
                &StatsQuery {
                    since: Some(Cursor::Time(ms(start))),
                    until: Some(Cursor::Time(ms(end))),
                    full: true,
                    ..StatsQuery::default()
                },
                &live,
            );
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(KpiStats::from(expected)).unwrap()
            );
            assert_eq!(prepared.event_answers.borrow().len(), 1);
        }
    }

    /// A full query measures an open ask to now; an explicit until measures
    /// it to the last event. Equal event cursors must not merge those ends.
    #[test]
    fn equal_event_bounds_keep_the_open_waits_end() {
        use crate::domain::ledger::testkit::{Log, T0, ms};
        let mut log = Log::default();
        log.add(
            None,
            None,
            "ask_opened",
            serde_json::json!({
                "ask_id": 1, "kind": "worker_question", "asked_by": "worker"
            }),
            10,
        );
        let goals = HashMap::new();
        let live = LiveSnapshot::default();
        let prepared = Prepared::new(&log.events, &goals, T0 + 100, Cow::Borrowed(&live), None);
        for (until, waited) in [(None, 90), (Some(Cursor::Time(ms(100))), 0), (None, 90)] {
            let query = StatsQuery {
                full: true,
                until,
                ..StatsQuery::default()
            };
            let page = prepared.page(None, &query);
            assert_eq!(
                (page.window.start, page.window.last),
                (EventId::new(0), EventId::new(1))
            );
            let reused = prepared.answer(&page);
            let fresh = super::super::stats_answering(
                None,
                &log.events,
                &goals,
                T0 + 100,
                SlotSnapshot::default(),
                &query,
                &live,
            );
            for asks in [&reused.asks, &fresh.asks] {
                assert_eq!(asks.times.by_kind["worker_question"].open.count, 1);
                assert_eq!(asks.times.by_kind["worker_question"].open.max, Some(waited));
            }
        }
        assert_eq!(prepared.event_answers.borrow().len(), 2);
    }
}
