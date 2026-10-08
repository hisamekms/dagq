//! Shared fixtures and helpers of the KPI unit tests; no test lives here.
//! Put a new KPI's tests in the file of the group they fit (the `mod`s
//! below), or in a new file for a new group, not at the end of one file.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::config::{JudgedPeriod, KpiSettings, judge};
use super::*;
use crate::domain::{EventId, RunId};

/// 2026-09-21T00:00:00+09:00, a Monday, in unix seconds.
const MONDAY: i64 = 1_789_916_400;
const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;
const JST: i64 = 9 * HOUR;

mod comparisons;
mod cross_strata;
mod host_records;
mod periods_and_targets;
mod plan_kpis;
mod queue_kpis;
mod review_kpis;
mod strata;
mod tokens;
mod trial_strata;

/// The events of a queue, built in time order.
#[derive(Default)]
struct Queue {
    events: Vec<RunEvent>,
    changes: HashMap<TaskId, Option<TaskChange>>,
    goals: HashMap<TaskId, Option<GoalId>>,
    /// The registered supervisors' last heartbeats.
    heartbeats: HashMap<String, i64>,
    draft_origins: HashMap<TaskId, DraftOrigin>,
    /// The landed runs' areas; `None` without `[areas]`.
    areas: Option<crate::domain::areas::RunAreas>,
    /// The repository is not dagq's source (ADR-t614-1).
    not_source: bool,
}

/// How one run went.
#[derive(Clone)]
struct Run {
    task: i64,
    /// The change its task declares (ADR-t980-1).
    change: Option<TaskChange>,
    /// Unix seconds of the claim.
    claimed: i64,
    work: i64,
    parallel: i64,
    slots: i64,
    load: f64,
    build: &'static str,
    /// The worker's provider; a Codex run is headless, with Codex 0.46.0.
    provider: &'static str,
    /// The provider it moved to in the middle (`provider_switched`), if any.
    switched_to: Option<&'static str>,
    /// The worker session the claim recorded (model, effort, trial group);
    /// none for a claim before ADR-0079.
    session: Option<(&'static str, &'static str, Option<&'static str>)>,
    /// The `nature` of the task's weight prediction before the claim.
    nature: Option<&'static str>,
    /// `rustc`'s release the claim recorded, on aarch64-apple-darwin.
    rustc: Option<&'static str>,
    revise: bool,
    failed: bool,
}

impl Run {
    fn new(task: i64, change: Option<TaskChange>, claimed: i64, work: i64) -> Self {
        Self {
            task,
            change,
            claimed,
            work,
            parallel: 3,
            slots: 1,
            load: 2.0,
            build: "b1",
            provider: "claude",
            switched_to: None,
            session: None,
            nature: None,
            rustc: None,
            revise: false,
            failed: false,
        }
    }
}

impl Queue {
    fn push(
        &mut self,
        task: Option<i64>,
        run: Option<&str>,
        kind: &str,
        payload: Value,
        secs: i64,
    ) {
        self.events.push(RunEvent {
            id: EventId::new(self.events.len() as i64 + 1),
            task_id: task.map(TaskId::new),
            goal_id: None,
            run_id: run.map(|run| RunId::new(run).unwrap()),
            kind: kind.to_owned(),
            payload,
            created_at: marks::utc_text(secs * 1000),
            actor: None,
        });
    }

    fn queue_event(&mut self, kind: &str, payload: Value, secs: i64) {
        self.push(None, None, kind, payload, secs);
    }

    /// A run that lands (or fails) `work` seconds after its claim, the task
    /// ready an hour before; returns the landing time.
    fn run(&mut self, run: &Run) -> i64 {
        let id = format!("{:08x}-0000-4000-8000-{:012x}", run.task, run.claimed);
        let (task, id) = (Some(run.task), Some(id.as_str()));
        self.changes
            .insert(TaskId::new(run.task), run.change.clone());
        self.goals.insert(TaskId::new(run.task), None);
        self.push(
            task,
            None,
            "task_status_changed",
            json!({"from": "submitted", "to": "ready"}),
            run.claimed - HOUR,
        );
        if let Some(nature) = run.nature {
            self.push(
                task,
                None,
                "task_weight_predicted",
                json!({"prediction": {"nature": nature, "expected_output_tokens": 1000}}),
                run.claimed - 2 * HOUR,
            );
        }
        self.push(
            task,
            id,
            "run_claimed",
            {
                let mut claimed = json!({"parallel": run.parallel, "slots": run.slots, "load_avg": run.load,
                                         "dagq_version": run.build, "provider": run.provider,
                                         "worker_mode": "interactive"});
                if run.provider == "codex" {
                    claimed["worker_mode"] = json!("headless");
                    claimed["codex_version"] = json!("0.46.0");
                }
                if let Some(release) = run.rustc {
                    claimed["rustc_release"] = json!(release);
                    claimed["rustc_host"] = json!("aarch64-apple-darwin");
                }
                if let Some((model, effort, group)) = run.session {
                    claimed["claude_version"] = json!("2.1.0");
                    claimed["model"] = json!(model);
                    claimed["effort"] = json!(effort);
                    claimed["group"] = json!(group);
                    // Codex's claim keeps the step, not a model (task 892).
                    if run.provider == "codex" {
                        claimed["model"] = json!(null);
                        claimed["ladder_model"] = json!(model);
                        claimed["group"] = json!(null);
                    }
                }
                claimed
            },
            run.claimed,
        );
        if run.provider == "codex" && run.session.is_some() {
            self.push(
                task,
                id,
                "turn_finished",
                json!({"turn": 1, "outcome": "succeeded", "provider": "codex", "model": "gpt-6-astra"}),
                run.claimed + 30,
            );
        }
        self.push(task, id, "agent_started", json!({}), run.claimed + 10);
        self.push(
            task,
            id,
            "first_commit_observed",
            json!({}),
            run.claimed + 60,
        );
        if let Some(to) = run.switched_to {
            self.push(
                task,
                id,
                "provider_switched",
                json!({"from": run.provider, "to": to, "reason": "usage_limit", "phase": "nudge"}),
                run.claimed + 70,
            );
        }
        let receipt = run.claimed + run.work;
        if run.failed {
            self.push(task, id, "run_failed", json!({"status": "failed"}), receipt);
            return receipt;
        }
        self.push(task, id, "receipt_observed", json!({}), receipt);
        self.push(
            task,
            id,
            "validation_finished",
            json!({"status": "awaiting_integration"}),
            receipt + 20,
        );
        if run.revise {
            self.push(task, id, "revise_requested", json!({}), receipt + 30);
        }
        self.push(task, id, "integration_started", json!({}), receipt + 40);
        self.push(task, id, "integration_rebased", json!({}), receipt + 50);
        self.push(
            task,
            id,
            "verification_command",
            json!({"phase": "integration", "attempt": 1, "index": 1, "exit_code": 0}),
            receipt + 60,
        );
        self.push(
            task,
            id,
            "run_integrated",
            json!({"status": "integrated"}),
            receipt + 100,
        );
        receipt + 100
    }

    /// The events in time order, renumbered, as a queue records them; a
    /// retraction keeps naming its mark.
    fn sort(&mut self) {
        self.events
            .sort_by_key(|event| timestamp_millis(&event.created_at));
        let renumbered: HashMap<i64, i64> = self
            .events
            .iter()
            .enumerate()
            .map(|(index, event)| (event.id.as_i64(), index as i64 + 1))
            .collect();
        for event in &mut self.events {
            event.id = EventId::new(renumbered[&event.id.as_i64()]);
            if let Some(mark) = event.payload.get("mark").and_then(Value::as_i64) {
                event.payload["mark"] = json!(renumbered[&mark]);
            }
        }
    }

    /// The id of the mark labeled `label`, once sorted.
    fn mark_id(&mut self, label: &str) -> EventId {
        self.sort();
        self.events
            .iter()
            .find(|event| event.payload["label"] == label)
            .unwrap()
            .id
    }

    fn kpi(&mut self, now: i64, config: &KpiConfig, query: &KpiQuery) -> Kpi {
        self.kpi_with_host(now, config, query, None)
    }

    fn kpi_with_host(
        &mut self,
        now: i64,
        config: &KpiConfig,
        query: &KpiQuery,
        host: Option<HostReader<'_>>,
    ) -> Kpi {
        self.sort();
        kpi(
            &KpiInput {
                events: &self.events,
                goals: &self.goals,
                changes: &self.changes,
                areas: self.areas.as_ref(),
                heartbeats: &self.heartbeats,
                draft_origins: &self.draft_origins,
                now,
                utc_offset_secs: JST,
                cores: Some(4),
                dagq_source: !self.not_source,
                config,
                host,
            },
            query,
        )
        .unwrap()
    }
}

fn measure<'a>(period: &'a PeriodKpis, name: &str, stratum: &str) -> &'a Measure {
    &period.window.kpis[name][stratum]
}

fn marks_ms(text: &str) -> i64 {
    timestamp_millis(text).unwrap()
}

impl PeriodKpis {
    fn end_ms(&self) -> i64 {
        timestamp_millis(&self.end).unwrap()
    }
}

fn start(queue: &mut Queue, parallel: i64, secs: i64) {
    queue.queue_event(
        marks::SUPERVISOR_STARTED,
        json!({"supervisor": "s", "parallel": parallel, "dagq_version": "b1"}),
        secs,
    );
}

/// The run id [`Queue::run`] gives a run.
fn run_id(run: &Run) -> RunId {
    RunId::new(format!(
        "{:08x}-0000-4000-8000-{:012x}",
        run.task, run.claimed
    ))
    .unwrap()
}
