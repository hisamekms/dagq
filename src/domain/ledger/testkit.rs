//! The lives of runs as the tests of what builds and compares the ledger
//! lay them down: events with ids in order, at seconds from [`T0`].

use serde_json::{Value, json};

use crate::domain::marks::utc_text;
use crate::domain::run_phase::{Attempt, Phase, PhaseChange};
use crate::domain::{EventId, RunEvent, RunId, TaskId};

pub(crate) const T0: i64 = 1_900_000_000;

/// The unix milliseconds of `secs` after [`T0`].
pub(crate) const fn ms(secs: i64) -> i64 {
    (T0 + secs) * 1000
}

/// A run id of `n`.
pub(crate) fn run_id(n: u8) -> String {
    format!("{n:08x}-1111-4111-8111-111111111111")
}

/// The events of a queue, ascending id.
#[derive(Debug, Clone, Default)]
pub(crate) struct Log {
    pub(crate) events: Vec<RunEvent>,
}

impl Log {
    /// Add an event of `kind` on `task` and `run` at `secs`.
    pub(crate) fn add(
        &mut self,
        task: Option<i64>,
        run: Option<&str>,
        kind: &str,
        payload: Value,
        secs: i64,
    ) -> &mut Self {
        let id = EventId::new(i64::try_from(self.events.len()).unwrap() + 1);
        self.events.push(RunEvent {
            id,
            task_id: task.map(TaskId::new),
            goal_id: None,
            run_id: run.map(|run| RunId::new(run).unwrap()),
            kind: kind.into(),
            payload,
            created_at: utc_text(ms(secs)),
            actor: None,
        });
        self
    }

    pub(crate) fn last(&self) -> EventId {
        self.events.last().map_or(EventId::new(0), |event| event.id)
    }

    /// The events after `after`.
    pub(crate) fn after(&self, after: EventId) -> Vec<RunEvent> {
        self.events
            .iter()
            .filter(|event| event.id > after)
            .cloned()
            .collect()
    }

    fn phase(&mut self, task: i64, run: &str, secs: i64, phase: Phase, cause: &str) -> &mut Self {
        self.add(
            Some(task),
            Some(run),
            "run_phase_changed",
            PhaseChange::new(phase, Attempt::FIRST, cause).payload(),
            secs,
        )
    }

    /// `task` created at `secs` and `run` of it claimed, worked and
    /// validated from `secs` to `secs + 660`.
    pub(crate) fn worked(&mut self, task: i64, run: &str, secs: i64) -> &mut Self {
        self.add(Some(task), None, "task_created", json!({}), secs)
            .add(
                Some(task),
                Some(run),
                "run_claimed",
                json!({"from": "ready", "to": "in_progress", "provider": "claude"}),
                secs,
            )
            .phase(task, run, secs, Phase::Provisioning, "x")
            .add(Some(task), Some(run), "agent_started", json!({}), secs + 5)
            .phase(task, run, secs + 10, Phase::Worker, "x")
            .add(
                Some(task),
                Some(run),
                "first_commit_observed",
                json!({}),
                secs + 70,
            )
            .add(
                Some(task),
                Some(run),
                "receipt_observed",
                json!({}),
                secs + 600,
            )
            .phase(task, run, secs + 600, Phase::Validating, "x")
            .add(
                Some(task),
                Some(run),
                "validation_finished",
                json!({"status": "awaiting_integration"}),
                secs + 660,
            )
            .phase(task, run, secs + 660, Phase::AwaitingSlot, "x")
    }

    /// `run` of `task` lands from `secs`: the landing, `run_integrated` at
    /// `secs + 200` and its push.
    pub(crate) fn integrated(&mut self, task: i64, run: &str, secs: i64) -> &mut Self {
        self.phase(task, run, secs, Phase::Landing, "x")
            .add(
                Some(task),
                Some(run),
                "integration_started",
                json!({}),
                secs,
            )
            .add(
                Some(task),
                Some(run),
                "integration_rebased",
                json!({}),
                secs,
            )
            .add(
                Some(task),
                Some(run),
                "run_integrated",
                json!({"from": "validating", "to": "integrated", "status": "integrated"}),
                secs + 200,
            )
            .phase(task, run, secs + 200, Phase::Push { in_slot: true }, "x")
    }

    /// The push of `run` of `task` finished at `secs`, the run and the task
    /// ended with it.
    pub(crate) fn pushed(&mut self, task: i64, run: &str, secs: i64) -> &mut Self {
        self.add(
            Some(task),
            Some(run),
            "push_finished",
            json!({"remote": "origin", "branch": "main"}),
            secs,
        )
        .phase(task, run, secs, Phase::Ended, "pushed")
        .add(
            Some(task),
            None,
            "task_status_changed",
            json!({"from": "in_progress", "to": "completed"}),
            secs,
        )
    }

    /// The push of `run` of `task` failed at `secs`: pending.
    pub(crate) fn push_failed(&mut self, task: i64, run: &str, secs: i64) -> &mut Self {
        self.add(
            Some(task),
            Some(run),
            "push_failed",
            json!({"remote": "origin", "branch": "main"}),
            secs,
        )
        .phase(task, run, secs, Phase::PushPending, "push_failed")
    }

    /// A run that lands: worked from `secs`, landing at `secs + 700`,
    /// pushed at `secs + 930`.
    pub(crate) fn landed(&mut self, task: i64, run: &str, secs: i64) -> &mut Self {
        self.worked(task, run, secs)
            .integrated(task, run, secs + 700)
            .pushed(task, run, secs + 930)
    }

    /// A run of `task` from before the switch: no `run_phase_changed`,
    /// landed at `secs + 1200`.
    pub(crate) fn legacy(&mut self, task: i64, run: &str, secs: i64) -> &mut Self {
        let at = |log: &mut Self, offset: i64, kind: &str, payload: Value| {
            log.add(Some(task), Some(run), kind, payload, secs + offset);
        };
        at(self, 0, "run_claimed", json!({"provider": "claude"}));
        at(self, 5, "agent_started", json!({}));
        at(self, 70, "first_commit_observed", json!({}));
        at(self, 600, "receipt_observed", json!({}));
        at(
            self,
            660,
            "validation_finished",
            json!({"status": "awaiting_integration"}),
        );
        at(self, 1000, "integration_started", json!({}));
        at(
            self,
            1200,
            "run_integrated",
            json!({"status": "integrated"}),
        );
        at(self, 1230, "push_finished", json!({}));
        self
    }
}
