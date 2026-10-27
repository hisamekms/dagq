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
    AskQuery, AskStore, Clock, CoordinationReads, DraftPlannerStore, GoalReviewStore,
    PlanReviewStore, PlannerAnswerRoute, ProcessControl, QueueRecords, RunCoordination, RunFiles,
    RunLog, RunRecovery, SessionReads, SupervisorReads, SupervisorRegistry, TRIAGE_ASKER,
    TaskReads,
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

    /// `recurring` shows each key's schedule, the error of a file that
    /// cannot be read (how `doctor` tells of a mistake), and nothing
    /// without a table.
    #[test]
    fn the_recurring_view_shows_the_schedule_or_the_error() {
        use crate::domain::recurring::{RecurringDraft, RecurringSchedule};
        let item = RecurringDraft {
            key: "k".into(),
            interval_days: 1,
            title: "t".into(),
            description: "d".into(),
            acceptance: "a".into(),
            verification: Vec::new(),
            paths: Vec::new(),
            evidence: Vec::new(),
            goal: None,
        };
        let schedule = |items: &[RecurringDraft]| {
            Ok(items
                .iter()
                .map(|item| RecurringSchedule::of(&item.key, item.interval_days, None, None))
                .collect())
        };
        let shown = recurring(Ok(vec![item]), schedule).unwrap().unwrap();
        assert_eq!(shown["schedule"][0]["key"], "k");
        assert_eq!(shown["schedule"][0]["next_at"], serde_json::Value::Null);
        let error = recurring(Err(anyhow::anyhow!("dagq.toml:3: bad")), schedule)
            .unwrap()
            .unwrap();
        assert_eq!(error["error"], "dagq.toml:3: bad");
        assert!(recurring(Ok(Vec::new()), schedule).unwrap().is_none());
    }

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
        // The inbox's provider set in `[roles.inbox]` (ADR-t1181-1
        // decision 5), Claude's by default.
        assert_eq!(read["inbox"]["provider"], "claude");
        assert_eq!(read["inbox"]["source"], "default");
        let mut codex = RoleModels::default();
        codex.entry(ModelRole::Inbox).provider = Some(crate::domain::Provider::Codex);
        let configured = roles(Ok(codex));
        assert_eq!(configured["inbox"]["provider"], "codex");
        assert_eq!(configured["inbox"]["source"], "dagq.toml");
    }

    /// `requests` lists the open requests in the order given with their
    /// words cut, and tells why each is still open: an open planner first,
    /// then an unanswered `planner_question` (a closed or answered one or
    /// another kind of ask does not count), then whether a planner opens
    /// for it.
    #[test]
    fn requests_tell_why_each_open_request_waits() {
        use crate::domain::{PlannerId, RequestId, plan_request::PlanRequest};
        let request = |id: i64, text: &str| -> PlanRequest {
            serde_json::from_value(json!({
                "id": id, "text": text, "note": null, "refs": [], "priority": null,
                "requested_by": "inbox", "requested_by_id": "inbox", "status": "open",
                "status_reason": null, "proposals": [], "planners": 0,
                "created_at": 100 + id, "updated_at": 100 + id,
            }))
            .unwrap()
        };
        let ask = |id: i64, kind: &str, closed_at: Option<i64>| -> crate::domain::Ask {
            serde_json::from_value(json!({
                "id": id, "kind": kind, "task_id": null, "run_id": null, "question": "q",
                "options": [], "answer": null, "asked_by": "planner",
                "reason_category": "scope", "created_at": 1, "answered_at": null,
                "closed_at": closed_at,
            }))
            .unwrap()
        };
        let long = "x".repeat(200);
        let answered = crate::domain::Ask {
            answered_at: Some(6),
            ..ask(5, "planner_question", None)
        };
        let open = vec![
            (request(1, "working"), Some(PlannerId::new(7)), vec![]),
            (
                request(2, "asked"),
                None,
                vec![
                    ask(1, "planner_question", Some(5)),
                    ask(2, "planner_question", None),
                ],
            ),
            (
                request(3, &long),
                None,
                vec![ask(3, "planner_question", Some(5)), ask(4, "blocked", None)],
            ),
            (request(4, "held"), None, vec![answered]),
        ];
        let listed = requests(&open, &[RequestId::new(3)]);
        let of = |field: &str| -> Vec<Value> {
            listed
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r[field].clone())
                .collect()
        };
        assert_eq!(of("id"), [json!(1), json!(2), json!(3), json!(4)]);
        assert_eq!(
            of("waiting_for"),
            ["planner", "answer", "planner_start", "held"]
        );
        assert_eq!(
            of("planner_id"),
            [json!(7), json!(null), json!(null), json!(null)]
        );
        assert_eq!(
            of("planner_questions"),
            [json!([]), json!([2]), json!([]), json!([])]
        );
        assert_eq!(listed[0]["requested_by"], "inbox");
        assert_eq!(listed[0]["created_at"], 101);
        assert_eq!(listed[2]["text"], format!("{}…", "x".repeat(120)));
        assert_eq!(requests(&[], &[]), json!([]));
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

    /// `held` lists every open hold with its run; only an overdue one is
    /// attention, and only where the attention is for the role.
    #[test]
    fn held_tasks_list_every_hold_and_the_overdue_ones_join_the_roles_attention() {
        use crate::domain::HoldId;
        use crate::domain::task_hold::{HOLD_REMIND_SECS, TaskHold};
        use std::time::{Duration, UNIX_EPOCH};
        struct At(i64);
        impl Clock for At {
            fn system_time(&self) -> std::time::SystemTime {
                UNIX_EPOCH + Duration::from_secs(u64::try_from(self.0).unwrap())
            }
            fn monotonic(&self) -> std::time::Instant {
                std::time::Instant::now()
            }
        }
        let hold = |id: i64, task: i64, until: Option<i64>, settled: Option<i64>| TaskHold {
            id: HoldId::new(id),
            task_id: TaskId::new(task),
            run_id: None,
            reason: format!("reason {id}"),
            until,
            by_role: "user".into(),
            by_id: "user".into(),
            created_at: 1_000,
            settled_at: settled,
        };
        let run = RunId::new("11111111-1111-4111-8111-111111111111").unwrap();
        let holds = [
            (hold(1, 7, Some(2_000), Some(1_000)), None),
            (
                hold(2, 8, None, None),
                Some((run.clone(), RunStatus::Running)),
            ),
        ];
        let status = |now: i64, role| {
            let mut status = json!({"attention": [{"kind": "other"}]});
            add_held_tasks(&mut status, &holds, &At(now), role).unwrap();
            status
        };
        let early = status(1_500, Some(SessionRole::Inbox));
        assert_eq!(early["held"][0]["overdue"], Value::Null);
        assert_eq!(early["held"][1]["run_id"], json!(run));
        assert_eq!(early["held"][1]["run_status"], "running");
        assert_eq!(early["held"][1]["state"], "settling");
        assert_eq!(early["attention"].as_array().unwrap().len(), 1);
        let late = status(1_000 + HOLD_REMIND_SECS, Some(SessionRole::Inbox));
        assert_eq!(late["held"][0]["overdue"], "until_passed");
        assert_eq!(late["held"][1]["overdue"], "long_held");
        let attention = late["attention"].as_array().unwrap();
        assert_eq!(attention.len(), 3);
        assert_eq!(attention[1]["kind"], "task_hold_reminded");
        assert_eq!(attention[1]["next"], "release or keep the hold of task 7");
        assert_eq!(attention[2]["status"], "settling");
        assert_eq!(attention[2]["last_error"], "reason 2");
        assert_eq!(
            status(1_000 + HOLD_REMIND_SECS, None)["attention"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let planner = status(1_000 + HOLD_REMIND_SECS, Some(SessionRole::Planner));
        assert_eq!(planner["held"].as_array().unwrap().len(), 2);
        assert_eq!(planner["attention"].as_array().unwrap().len(), 1);
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
            max_spikes: None,
            max_spikes_source: None,
            class_aging: None,
            class_aging_source: None,
            max_load: None,
            max_load_source: None,
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

    /// Every pid below 100 is alive.
    struct Low;

    impl ProcessControl for Low {
        fn alive(&self, pid: u32) -> bool {
            pid < 100
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

    fn started(token: &str, pid: u32, started_at: i64, now: i64) -> SupervisorRegistration {
        SupervisorRegistration {
            started_at,
            ..registration(token, pid, now)
        }
    }

    fn agent(
        label: &str,
        loaded: bool,
        pid: Option<u32>,
    ) -> crate::application::lifecycle::QueueAgent {
        crate::application::lifecycle::QueueAgent {
            label: label.into(),
            plist: std::path::PathBuf::from(format!("/a/{label}.plist")),
            state: crate::application::AgentState { loaded, pid },
        }
    }

    /// Two live supervisors that are no handoff's pair are a duplicate,
    /// with each one's token, pid and mode and the fix; the pair of a
    /// handoff (the same pid registered again since) is one supervisor, and
    /// a dead or silent registration does not count.
    #[test]
    fn two_live_supervisors_but_not_a_handoff_s_pair_are_a_duplicate() {
        let now = 1_000;
        let mut by_hand = started("b", 2, 20, now);
        by_hand.mode = None;
        let found = duplicate_supervisors(&[started("a", 1, 10, now), by_hand], &[], now, &Low)
            .expect("two supervisors");
        assert_eq!(found.severity, "warning");
        let seen: Vec<_> = found
            .supervisors
            .iter()
            .map(|s| (s.token.as_str(), s.pid, s.mode))
            .collect();
        assert_eq!(
            seen,
            [("a", 1, Some(SupervisorMode::Launchd)), ("b", 2, None)]
        );
        assert!(found.fix[0].contains("dagq down --wait"), "{:?}", found.fix);
        assert!(found.agents.is_empty());

        // A handoff's pair: the successor the exec made under the same pid.
        let pair = [started("a", 1, 10, now), started("a2", 1, 40, now)];
        assert_eq!(duplicate_supervisors(&pair, &[], now, &Low), None);
        // Made in the same second, one of them stands for both.
        let same_second = [started("x", 1, 10, now), started("y", 1, 10, now)];
        assert_eq!(duplicate_supervisors(&same_second, &[], now, &Low), None);
        // A pair beside another supervisor is two.
        let three = [pair[0].clone(), pair[1].clone(), started("c", 3, 50, now)];
        let found = duplicate_supervisors(&three, &[], now, &Low).unwrap();
        let tokens: Vec<_> = found.supervisors.iter().map(|s| s.token.as_str()).collect();
        assert_eq!(tokens, ["a2", "c"]);

        // A dead pid and a silent heartbeat are reported as stale instead.
        let dead = [started("a", 1, 10, now), started("d", 200, 20, now)];
        assert_eq!(duplicate_supervisors(&dead, &[], now, &Low), None);
        let silent = [
            started("a", 1, 10, now),
            SupervisorRegistration {
                heartbeat_at: now - HEARTBEAT_TIMEOUT_SECS - 1,
                ..started("s", 2, 20, now)
            },
        ];
        assert_eq!(duplicate_supervisors(&silent, &[], now, &Low), None);
    }

    /// Two of the queue's agents loaded are a duplicate, naming each label
    /// with its bootout, even with one supervisor registered; one loaded
    /// and one only defined are not.
    #[test]
    fn two_loaded_agents_of_the_queue_are_a_duplicate() {
        let now = 1_000;
        let one = [started("a", 1, 10, now)];
        let agents = [
            agent("com.dagq.q", true, Some(1)),
            agent("com.dagq.h", true, None),
        ];
        let found = duplicate_supervisors(&one, &agents, now, &Low).expect("two agents");
        let labels: Vec<_> = found.agents.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(labels, ["com.dagq.q", "com.dagq.h"]);
        assert_eq!(found.agents[0].pid, Some(1));
        assert!(
            found
                .fix
                .iter()
                .any(|fix| fix.contains("launchctl bootout \"gui/$(id -u)/\"'com.dagq.h'")),
            "{:?}",
            found.fix
        );
        let unloaded = [
            agent("com.dagq.q", true, Some(1)),
            agent("com.dagq.h", false, None),
        ];
        assert_eq!(duplicate_supervisors(&one, &unloaded, now, &Low), None);
    }

    /// The fix quotes a label and a plist path for the shell: a `Label`
    /// with a `;` or a quote stays one argument of `launchctl bootout`.
    #[test]
    fn the_fix_quotes_each_label_and_plist_for_the_shell() {
        let now = 1_000;
        let agents = [
            agent("com.dagq.q", true, Some(1)),
            agent("com.dagq.x;id 'y'", true, None),
        ];
        let found = duplicate_supervisors(&[], &agents, now, &Low).unwrap();
        let fix = found.fix.iter().find(|fix| fix.contains("x;id")).unwrap();
        assert!(
            fix.contains(r#"`launchctl bootout "gui/$(id -u)/"'com.dagq.x;id '"'"'y'"'"''`"#),
            "{fix}"
        );
        assert!(
            fix.contains(r#"`rm '/a/com.dagq.x;id '"'"'y'"'"'.plist'`"#),
            "{fix}"
        );
    }

    /// `status` adds the duplicate to its attention for the inbox, and
    /// both add `duplicate_supervisors`, null when there is none.
    #[test]
    fn a_duplicate_is_added_to_the_report_and_to_the_attention() {
        let now = 1_000;
        let two = [started("a", 1, 10, now), started("b", 2, 20, now)];
        let mut report = json!({"attention": []});
        add_duplicate_supervisors(&mut report, &two, &[], now, &Low, true).unwrap();
        assert_eq!(report["duplicate_supervisors"]["severity"], "warning");
        let entry = &report["attention"][0];
        assert_eq!(entry["kind"], SUPERVISOR_DUPLICATE);
        assert_eq!(entry["next"], json!(AttentionNext::RestartSupervisor));
        let text = entry["last_error"].as_str().unwrap();
        assert!(text.contains("pids 1, 2"), "{text}");
        let mut doctor = json!({});
        add_duplicate_supervisors(&mut doctor, &two, &[], now, &Low, false).unwrap();
        assert!(doctor.get("attention").is_none());
        let mut none = json!({"attention": []});
        add_duplicate_supervisors(&mut none, &two[..1], &[], now, &Low, true).unwrap();
        assert_eq!(
            none,
            json!({"attention": [], "duplicate_supervisors": null})
        );
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

    fn run_at(id: &str, created_at: &str) -> TaskRun {
        TaskRun::restore(crate::domain::RunRecord {
            id: RunId::new(id).unwrap(),
            task_id: TaskId::new(7),
            status: RunStatus::Running,
            requested_provider: crate::domain::Provider::Claude,
            actual_provider: crate::domain::Provider::Claude,
            worker_mode: crate::domain::worker::Worker::default_mode(
                crate::domain::Provider::Claude,
            ),
            base_commit: crate::domain::CommitSha::parse("a".repeat(40), "commit").unwrap(),
            branch: None,
            worktree_path: None,
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: None,
            last_error: None,
            workspace_closed_at: None,
            created_at: created_at.to_owned(),
        })
        .unwrap()
    }

    /// `status`'s `runs` and `slots_and_waits` share each run's events:
    /// a listed run, a leased one and a run leased but not active are each
    /// read once. `attention`, which reads its own runs' events, is outside
    /// [`runs_slots_and_waits`] and not counted.
    #[test]
    fn status_runs_and_slots_read_each_runs_events_once() {
        let now = 1_000;
        let queue = crate::application::port_fakes::RunsAndEvents {
            active: vec![run_at("r1", "1"), run_at("r2", "2")],
            runs: vec![run_at("r3", "3")],
            ..Default::default()
        };
        let registrations = [registration("a", 1, now)];
        let leases = [lease("r1", "a", 1, now), lease("r3", "a", 1, now)];
        let (runs, supervisors, waiting) =
            runs_slots_and_waits(&queue, &OnlyOne, &registrations, &leases, now).unwrap();
        let ids: Vec<_> = runs.iter().map(|run| run["run_id"].clone()).collect();
        assert_eq!(ids, [json!("r1"), json!("r2"), json!("r3")]);
        assert_eq!(supervisors[0]["slots"]["used"], 2);
        assert!(waiting.is_empty());
        let reads = queue.event_reads.borrow();
        for id in ["r1", "r2", "r3"] {
            assert_eq!(reads.get(&RunId::new(id).unwrap()), Some(&1), "{id}");
        }
        assert_eq!(reads.len(), 3);
    }

    #[test]
    fn held_names_the_recorded_holds_and_a_waiting_spacing_after_no_supervisor() {
        let record = RunEvent {
            id: crate::domain::EventId::new(5),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: "claim_held".into(),
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
