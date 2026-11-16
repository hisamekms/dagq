//! Shared stats period aggregation, projected to the fields KPI consumes.
//! Ordinary stats adds its other summaries to this same answer. Run facts
//! are prepared once; every period applies its own time and goal bounds.
use super::*;

#[derive(Serialize)]
pub(in crate::domain) struct KpiStats {
    pub(in crate::domain) runs: Vec<RunStats>,
    pub(in crate::domain) overall: KpiOverall,
    pub(in crate::domain) backend_failures: BackendFailures,
    pub(in crate::domain) auto_repairs: AutoRepairStats,
    pub(in crate::domain) cancel_reasons: BTreeMap<String, cancel_reasons::CancelReason>,
    pub(in crate::domain) review_reasons: review_reasons::ReviewReasons,
    pub(in crate::domain) draft_flow: drafts::DraftFlow,
    pub(in crate::domain) execution_tokens: executions::ExecutionStats,
    pub(in crate::domain) follow_up_categories:
        BTreeMap<String, follow_up_categories::CategoryFlow>,
    pub(in crate::domain) jobs: BTreeMap<&'static str, jobs::JobStats>,
    pub(in crate::domain) planner_routes: BTreeMap<String, planner_routes::PlannerRouteHealth>,
    pub(in crate::domain) sessions: Sessions,
    pub(in crate::domain) worker_question_topics: worker_question_topics::WorkerQuestionTopics,
    pub(in crate::domain) worker_routes: BTreeMap<String, routes::RouteHealth>,
    pub(in crate::domain) asks: AskStats,
    pub(in crate::domain::stats) waiting: crate::domain::waiting::WaitingStats,
}

/// The run aggregates shared by stats intervals and KPI windows.
#[derive(Serialize)]
pub(in crate::domain) struct KpiOverall {
    pub(in crate::domain) already_satisfied: usize,
    pub(in crate::domain) land_phases: LandBreakdown,
    pub(in crate::domain) resume_outcomes: ResumeBreakdown,
}

impl KpiOverall {
    pub(in crate::domain::stats) fn of(runs: &[&RunStats]) -> Self {
        Self {
            already_satisfied: runs
                .iter()
                .filter(|run| run.status.as_deref() == Some(ALREADY_SATISFIED))
                .count(),
            land_phases: landing::breakdown(
                runs.iter()
                    .filter_map(|run| Some((run.land_phases.as_ref()?, run.wait_to_land?))),
            ),
            resume_outcomes: retries::resume_breakdown(
                runs.iter().flat_map(|run| &run.retries.resume_attempts),
            ),
        }
    }
}

/// Aggregates whose inputs are event cursors in a fixed snapshot and goal.
#[derive(Clone)]
pub(super) struct EventStats {
    backend_failures: BackendFailures,
    auto_repairs: AutoRepairStats,
    cancel_reasons: BTreeMap<String, cancel_reasons::CancelReason>,
    review_reasons: review_reasons::ReviewReasons,
    asks: AskStats,
    jobs: BTreeMap<&'static str, jobs::JobStats>,
    draft_flow: drafts::DraftFlow,
    follow_up_categories: BTreeMap<String, follow_up_categories::CategoryFlow>,
    worker_question_topics: worker_question_topics::WorkerQuestionTopics,
    worker_routes: BTreeMap<String, routes::RouteHealth>,
    sessions: Sessions,
    waiting: crate::domain::waiting::WaitingStats,
}

impl KpiStats {
    pub(in crate::domain) fn with_changes(
        &mut self,
        changes: &HashMap<TaskId, Option<TaskChange>>,
    ) {
        for run in &mut self.runs {
            run.change = changes.get(&run.task_id).cloned().flatten();
        }
        self.review_reasons.review.with_changes(changes);
        self.review_reasons.plan_review.with_changes(changes);
    }
}

impl Prepared<'_> {
    pub(in crate::domain) fn kpi_answer(
        &self,
        answers: Option<&crate::domain::ledger::window::bounds::Answers>,
        start: i64,
        end: i64,
    ) -> KpiStats {
        let query = StatsQuery {
            since: Some(Cursor::Time(start)),
            until: Some(Cursor::Time(end)),
            goal_id: self.goal,
            full: true,
        };
        self.answer(&self.page(answers, &query))
    }

    /// The stats period projection consumed by both readers. It derives
    /// run, event and time-bounded values once under the same window rules.
    pub(in crate::domain::stats) fn answer(&self, page: &Page<'_>) -> KpiStats {
        let events = self.events;
        let live = &self.live;
        let window = &page.window;
        let window_start = window.start;
        let window_last = window.last;
        let window_until = window.until;
        let counts = |task| self.counts(task);
        let runs = self.page_runs(page);
        let overall = KpiOverall::of(&runs.iter().collect::<Vec<_>>());
        let EventStats {
            backend_failures,
            auto_repairs,
            cancel_reasons,
            review_reasons,
            asks,
            jobs,
            draft_flow,
            follow_up_categories,
            worker_question_topics,
            worker_routes,
            sessions,
            waiting,
        } = self.event_answer(window);
        let mut sessions = sessions;
        let all_executions = &self.all_executions;
        // The tokens per Execution (ADR-t1486-1), by when each ended (a cut by
        // its time): from since, else the window start event, to its end
        // capped at now.
        // The sessions per kind and route
        // read theirs from them, so that a span open for days spreads its
        // tokens over the days it used them.
        let tokens_from = window.tokens_from;
        let execution_tokens = executions::window_after(
            all_executions,
            &live.earlier_executions,
            tokens_from,
            window_until,
            |execution| match (execution.task_id, execution.goal_id) {
                (None, Some(goal)) => self.goal.is_none_or(|only| only == goal),
                (task_id, _) => counts(task_id),
            },
        );
        let mut kind_contexts = context::by_kind(
            &context::records(&execution_tokens.executions),
            tokens_from,
            window_until,
        );
        for (kind, kind_sessions) in &mut sessions.by_kind {
            if let Some(actor) = execution_tokens.by_actor.get(kind) {
                kind_sessions.tokens = actor.totals.clone();
            }
            kind_sessions.context = kind_contexts.remove(*kind).unwrap_or_default();
        }
        // No inbox is of a goal.
        if self.goal.is_some() {
            sessions.inbox_launches.clear();
        }
        for (kind, providers) in &mut sessions.by_provider {
            for (provider, provider_sessions) in providers.iter_mut() {
                provider_sessions.tokens = execution_tokens
                    .by_actor
                    .get(kind)
                    .and_then(|actor| actor.by_provider.get(provider))
                    .map(|tokens| tokens.totals.clone())
                    .unwrap_or_default();
            }
        }
        for (kind, routes) in &mut sessions.by_route {
            for (route, route_sessions) in routes.iter_mut() {
                route_sessions.tokens = execution_tokens
                    .by_route
                    .iter()
                    .find(|((of, on), _)| of == kind && on == route)
                    .map(|(_, totals)| totals.clone())
                    .unwrap_or_default();
                let route_records =
                    context::records(execution_tokens.executions.iter().filter(|execution| {
                        execution.actor == *kind && execution.route == Some(route)
                    }));
                route_sessions.context =
                    context::by_kind(&route_records, tokens_from, window_until)
                        .remove(*kind)
                        .unwrap_or_default();
            }
        }
        // The planners' tokens are those of `sessions`, whose window is theirs.
        let planner_routes = planner_routes::planner_routes(
            events,
            window_start,
            window_last,
            counts,
            sessions
                .by_route
                .get(crate::domain::sessions::RUNTIME_PLANNER),
        );
        KpiStats {
            runs,
            overall,
            backend_failures,
            auto_repairs,
            cancel_reasons,
            review_reasons,
            draft_flow,
            execution_tokens,
            follow_up_categories,
            jobs,
            planner_routes,
            sessions,
            worker_question_topics,
            worker_routes,
            asks,
            waiting,
        }
    }

    /// Reuse only within this immutable input and goal. Time-clipped token
    /// counts and contexts are still derived separately for every period.
    fn event_answer(&self, window: &Window) -> EventStats {
        let (window_start, window_last, window_end) = (window.start, window.last, window.end);
        if let Some(answer) =
            self.event_answers
                .borrow()
                .get(&(window_start, window_last, window_end))
        {
            return answer.clone();
        }
        let events = self.events;
        let live = &self.live;
        let goal = self.goal;
        let in_goal = |task| self.in_goal(task);
        let counts = |task| self.counts(task);
        let backend_failures = backend_failures(events, window_start, window_last, counts);
        let auto_repairs = auto_repairs::auto_repairs(events, window_start, window_last, counts);
        let cancel_reasons =
            cancel_reasons::cancel_reasons(events, window_start, window_last, counts);
        let review_reasons =
            review_reasons::review_reasons(events, window_start, window_last, counts);
        let asks = asks::asks(events, window_start, window_last, window_end, counts);
        let jobs = jobs::jobs(events, window_start, window_last, |event| {
            match (event.task_id, event.goal_id) {
                (None, Some(g)) => goal.is_none_or(|only| only == g),
                (task, _) => counts(task),
            }
        });
        let draft_flow = drafts::draft_flow(
            events,
            &live.draft_origins,
            window_start,
            window_last,
            window_end,
            counts,
        );
        let follow_up_categories = follow_up_categories::follow_up_categories(
            events,
            window_start,
            window_last,
            window_end,
            counts,
        );
        let worker_question_topics = worker_question_topics::worker_question_topics(
            events,
            window_start,
            window_last,
            live.utc_offset_secs,
            counts,
        );
        let waiting =
            super::super::super::waiting::waiting_stats(events, window_start, window_last, counts);
        let worker_routes =
            routes::route_health(events, window_start, window_last, counts, &waiting.by_route);
        let spans = &self.spans;
        let sessions = sessions::by_kind(
            spans,
            events,
            SessionWindow {
                after: window_start,
                upto: window_last,
            },
            window_end,
            |span| match goal {
                None => true,
                Some(_) if span.run_id.is_some() => span.task_id.is_some_and(in_goal),
                Some(goal) => span.goal_ids.contains(&goal),
            },
        );
        let answer = EventStats {
            backend_failures,
            auto_repairs,
            cancel_reasons,
            review_reasons,
            asks,
            jobs,
            draft_flow,
            follow_up_categories,
            worker_question_topics,
            worker_routes,
            sessions,
            waiting,
        };
        self.event_answers
            .borrow_mut()
            .insert((window_start, window_last, window_end), answer.clone());
        answer
    }
}

#[cfg(test)]
impl From<Stats> for KpiStats {
    fn from(stats: Stats) -> Self {
        Self {
            runs: stats.runs,
            overall: KpiOverall {
                already_satisfied: stats.overall.already_satisfied,
                land_phases: stats.overall.land_phases,
                resume_outcomes: stats.overall.resume_outcomes,
            },
            backend_failures: stats.backend_failures,
            auto_repairs: stats.auto_repairs,
            cancel_reasons: stats.cancel_reasons,
            review_reasons: stats.review_reasons,
            draft_flow: stats.draft_flow,
            execution_tokens: stats.execution_tokens,
            follow_up_categories: stats.follow_up_categories,
            jobs: stats.jobs,
            planner_routes: stats.planner_routes,
            sessions: stats.sessions,
            worker_question_topics: stats.worker_question_topics,
            worker_routes: stats.worker_routes,
            asks: stats.asks,
            waiting: stats.waiting,
        }
    }
}
