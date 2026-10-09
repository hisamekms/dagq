//! `status`, `doctor` and `recover` (ADR-0016, ADR-0024 decision 3): how
//! the supervisors and the unfinished runs stand, what waits for a person
//! (`attention`), and the recovery of an orphaned run. A run's health is
//! its lease, its registered processes and its files. Each function reads
//! the queue through the store ports it needs; liveness comes through
//! [`ProcessControl`] and the files through [`RunFiles`].

use crate::domain::LeaseToken;
use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path};

use super::execution::{ActorExecution, actor_executions};
use super::{
    AskQuery, AskStore, Clock, DraftPlannerStore, GoalReviewStore, PlanReviewStore,
    PlannerAnswerRoute, ProcessControl, QueueRecords, RunCoordination, RunFiles, RunLog,
    RunRecovery, SessionRegistry, SupervisorRegistry, TRIAGE_ASKER, TaskStore,
};
use crate::domain::worker::ProviderCheck;
use crate::domain::{
    APPROVE_RELEASE_OPTIONS, AskId, AskKind, Attention, AttentionNext, HEARTBEAT_TIMEOUT_SECS,
    LandingAnswer, ReasonCode, RunEvent, RunHistory, RunId, RunLease, RunProcess, RunStatus,
    SessionRole, SupervisorMode, SupervisorPulse, SupervisorRegistration, TaskId, TaskRun,
    TaskStatus, UPDATE_FAILED_OPTIONS, event_attention, event_kind, heartbeat_stale,
    kpi::push::{KPI_PUSH_ABANDONED, KPI_PUSH_ATTENTION_KINDS},
    queue_hold::{self, HoldJob},
    reason, recheck, run_attention, run_attention_of,
    run_env::{RUN_ENV_PROGRAM_KINDS, RUN_ENV_PROGRAM_MISSING, RunEnvCheck},
    run_progress::{Lease, Progress},
    session_takes_answers,
    slot_limits::SettingSource,
    supervisor_attention,
    waiting::{WaitCount, WaitState},
};

mod attention;
mod host;
mod report;
mod runs;

pub use attention::*;
pub use host::*;
pub use report::*;
pub use runs::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// `doctor`'s `roles` keys every role but the worker's, gives the
    /// runtime's planner the headless route only, and keeps the defaults
    /// with `error` when `[roles.*]` could not be read.
    #[test]
    fn doctor_roles_report_each_role_and_a_read_error() {
        use crate::domain::actor_model::{ModelRole, RoleModels};
        let read = roles(Ok(RoleModels::default()));
        for role in ModelRole::ALL {
            assert!(read[role.as_str()]["provider"].is_string(), "{read}");
        }
        assert_eq!(
            read[ModelRole::RuntimePlanner.as_str()]["route"],
            json!(crate::domain::PlannerRoute::Headless)
        );
        assert!(read.get("error").is_none());
        let failed = roles(Err(anyhow::anyhow!("bad toml")));
        assert_eq!(failed["error"], "bad toml");
        for role in ModelRole::ALL {
            assert_eq!(failed[role.as_str()], read[role.as_str()]);
        }
    }

    #[test]
    fn status_sections_follow_the_role() {
        let of = |role| {
            let sections = status_sections(role);
            (sections.language, sections.inbox)
        };
        assert_eq!(of(None), (false, true));
        assert_eq!(of(Some(SessionRole::Inbox)), (true, true));
        assert_eq!(of(Some(SessionRole::Planner)), (true, false));
    }

    #[test]
    fn a_refused_queue_reports_when_and_why() {
        let report = refused(42, &anyhow::anyhow!("floor 9"));
        assert_eq!(report, json!({"checked_at": 42, "error": "floor 9"}));
    }
    use anyhow::Result;

    /// Pid 1 is alive, every other pid is dead.
    struct OnlyOne;

    impl ProcessControl for OnlyOne {
        fn alive(&self, pid: u32) -> bool {
            pid == 1
        }
        fn terminate(&self, _: u32) -> Result<()> {
            Ok(())
        }
        fn interrupt(&self, _: u32) -> Result<()> {
            Ok(())
        }
        fn kill(&self, _: u32) -> Result<()> {
            Ok(())
        }
    }

    fn registration(token: &str, pid: u32, heartbeat_at: i64) -> SupervisorRegistration {
        SupervisorRegistration {
            token: LeaseToken::new(token),
            pid,
            parallel: 2,
            started_at: 0,
            heartbeat_at,
            mode: Some(SupervisorMode::Launchd),
            workspace_id: None,
            handoff_accepted: false,
            handoff_binary: None,
            auto_update: false,
            max_waiting: None,
            parallel_source: None,
            max_waiting_source: None,
            runtime_planners: None,
            runtime_planners_source: None,
            claim_spacing: None,
            claim_spacing_source: None,
            max_load: None,
            providers: None,
            binary_version: Some("1.0.0".into()),
        }
    }

    fn lease(run: &str, token: &str, pid: u32, heartbeat_at: i64) -> RunLease {
        RunLease {
            run_id: RunId::new(run).unwrap(),
            token: LeaseToken::new(token),
            pid,
            heartbeat_at,
        }
    }

    #[test]
    fn supervisors_join_leases_by_token_and_take_liveness_from_the_port() {
        let now = 1_000;
        let health = supervisors(
            &[registration("a", 1, now - 5), registration("b", 2, now - 5)],
            &[
                lease("r1", "a", 1, now - 5),
                // An `integrate` process: two leases, the freshest counts.
                lease("r2", "c", 3, now - 100),
                lease("r3", "c", 3, now - 10),
            ],
            now,
            &OnlyOne,
        );
        assert_eq!(health.len(), 3);
        assert!(health[0].alive && !health[0].stale && health[0].registered);
        assert_eq!(health[0].run_ids, vec![RunId::new("r1").unwrap()]);
        // Dead pid: stale whatever its heartbeat.
        assert!(!health[1].alive && health[1].stale);
        assert!(!health[2].registered);
        assert_eq!(health[2].heartbeat_age_secs, 10);
        assert_eq!(health[2].run_ids.len(), 2);
        assert!(health[2].stale, "a dead lease holder is stale");
    }

    #[test]
    fn a_lease_is_stale_by_age_and_its_holder_is_judged_by_the_port() {
        let fresh = lease_health(&lease("r", "a", 1, 90), 100, &OnlyOne);
        assert!(fresh.alive && !fresh.stale);
        let old = lease_health(
            &lease("r", "a", 2, 100 - HEARTBEAT_TIMEOUT_SECS - 1),
            100,
            &OnlyOne,
        );
        assert!(!old.alive && old.stale);
        assert!(lease_is_stale(&lease("r", "a", 2, 100), 100, &OnlyOne));
        assert!(!lease_is_stale(&lease("r", "a", 1, 100), 100, &OnlyOne));
    }

    #[test]
    fn attention_is_the_inboxs_and_reasons_are_cut() {
        assert!(for_role(None));
        assert!(for_role(Some(SessionRole::Inbox)));
        assert!(!for_role(Some(SessionRole::Planner)));
        assert_eq!(truncate_reason("short"), "short");
        let long = "x".repeat(REASON_CHARS + 5);
        assert_eq!(
            truncate_reason(&long),
            format!("{}…", "x".repeat(REASON_CHARS))
        );
    }

    #[test]
    fn held_names_the_recorded_holds_and_a_waiting_spacing_after_no_supervisor() {
        let record = RunEvent {
            id: crate::domain::EventId::new(5),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: "claim_held".to_owned(),
            payload: json!({"reason": "load", "supervisor": "tok"}),
            created_at: "2026-10-06 22:40:00".to_owned(),
            actor: None,
        };
        let waiting = json!({"waiting": true, "last_claim_at": "2026-10-06 22:41:00"});
        let idle = json!({"waiting": false, "last_claim_at": "2026-10-06 22:30:00"});
        assert_eq!(
            held(true, [record.clone()], [waiting.clone(), idle.clone()]),
            [
                json!({"reason": "no_supervisor"}),
                json!({"reason": "claim_held", "since": "2026-10-06 22:40:00",
                       "record": {"reason": "load", "supervisor": "tok"}}),
                json!({"reason": "claim_spacing", "since": "2026-10-06 22:41:00",
                       "record": waiting}),
            ]
        );
        assert!(held(false, [], [idle]).is_empty());
    }
}
