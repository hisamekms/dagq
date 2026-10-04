//! Membership, immutable provenance, adoption and migration at the SQLite/CLI boundary.
use crate::common::{self, queue::*};
use dagq::domain::follow_up::{MembershipClassification as Class, MembershipJudgement};
use dagq::{
    application::TaskStore,
    domain::{
        ClaimOutcome, DraftOrigin, GoalEdit, GoalId, PlannerOrigin, PlannerOwner, Submission,
        TaskAction, TaskId,
    },
    infrastructure::{schema::MIGRATIONS, sqlite::SqliteQueue},
};
use rusqlite::{Connection, params};
use serde_json::json;

fn judgement(classification: Class, destination: Option<GoalId>) -> MembershipJudgement {
    MembershipJudgement {
        classification,
        acceptance_items: vec!["(1)".into()],
        reason: "receipt demonstrates the relation to (1)".into(),
        evidence: vec!["receipt:r".into()],
        destination_goal_id: destination,
        source_goal_id: None,
        corrects: None,
    }
}
fn submit(queue: &mut SqliteQueue, id: TaskId) -> anyhow::Result<dagq::domain::Proposal> {
    queue.submit(Submission {
        tasks: vec![id],
        goals: vec![],
        proposal: None,
        owner: PlannerOwner {
            origin: PlannerOrigin::Runtime,
            workspace_id: None,
        },
    })
}

#[test]
fn judgement_moves_membership_atomically_and_show_keeps_version_and_origin() {
    let (_dir, mut q) = fixture();
    let source = q.add_goal(new_goal("source")).unwrap().id();
    let other = q.add_goal(new_goal("destination")).unwrap().id();
    let source_task = q
        .add(dagq::domain::NewTask {
            goal_id: Some(source),
            ..new_task("source task")
        })
        .unwrap()
        .id();
    q.transition(source_task, TaskAction::BypassReview).unwrap();
    let ClaimOutcome::Claimed { run, .. } = q.claim(&base()).unwrap() else {
        panic!("claim")
    };
    // Registration reads the source task and goal in its transaction.
    let added = dagq::application::integrate::register_follow_ups(
        &mut q,
        &q_task(source_task, source),
        run.id(),
        Some(&json!([{"title":"follow up","description":"new work"}])),
    );
    let draft = added[0].task_id;
    let original = q.draft_origin(draft).unwrap().unwrap().1;
    assert_eq!(original["source_goal_id"], source.as_i64());
    assert_eq!(original["source_goal_state"], "open");
    let closed = q.add_goal(new_goal("closed destination")).unwrap().id();
    q.close_goal(closed, dagq::domain::GoalVerdict::Achieved)
        .unwrap();
    assert!(
        q.judge_follow_up(draft, judgement(Class::OutOfScope, Some(closed)), "planner")
            .is_err()
    );
    assert!(q.show(draft).unwrap().membership_judgements.is_empty());
    assert_eq!(q.show(draft).unwrap().task.goal_id(), Some(source));
    let row = q
        .judge_follow_up(draft, judgement(Class::OutOfScope, Some(other)), "planner")
        .unwrap();
    assert_eq!(q.show(draft).unwrap().task.goal_id(), Some(other));
    assert_eq!(q.follow_up_depth(draft).unwrap(), 1);
    assert_eq!(q.draft_origin(draft).unwrap().unwrap().1, original);
    assert_eq!(original["source_task_id"], source_task.as_i64());
    assert_eq!(original["source_run_id"], run.id().as_str());
    assert!(q.set_goal(draft, Some(source)).is_err());
    let detail = q.show(draft).unwrap();
    assert_eq!(detail.membership_judgements[0]["acceptance_version"], 1);
    assert_eq!(
        detail
            .events
            .iter()
            .filter(|e| e.kind.as_str() == "follow_up_judged")
            .count(),
        1
    );
    assert_eq!(
        q.show_goal(source).unwrap().follow_up_memberships[0]["judgements"][0]["id"],
        row["id"]
    );
    q.edit_goal(
        source,
        GoalEdit {
            title: Some("renamed".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(q.show_goal(source).unwrap().acceptance_version, 1);
    q.edit_goal(
        source,
        GoalEdit {
            acceptance: Some("changed criteria".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(q.show_goal(source).unwrap().acceptance_version, 2);
    assert_eq!(
        q.show(draft).unwrap().membership_judgements[0]["needs_recheck"],
        true
    );
    // A rejected move rolls back its judgement, and a correction is explicit.
    let invalid = judgement(Class::Required, None);
    assert!(q.judge_follow_up(draft, invalid, "planner").is_err());
    let mut correction = judgement(Class::Required, None);
    correction.corrects = row["id"].as_i64();
    q.judge_follow_up(draft, correction, "planner").unwrap();
    assert_eq!(q.show(draft).unwrap().task.goal_id(), Some(source));
    assert_eq!(
        q.show(draft).unwrap().membership_judgements[1]["acceptance_version"],
        2
    );
    assert_eq!(q.draft_origin(draft).unwrap().unwrap().1, original);
    submit(&mut q, draft).unwrap();
}

fn q_task(id: TaskId, goal: GoalId) -> dagq::domain::Task {
    dagq::domain::Task::new(
        id,
        dagq::domain::NewTask {
            goal_id: Some(goal),
            ..new_task("source task")
        },
        "2026-10-04T00:00:00.000Z".into(),
    )
    .unwrap()
}

#[test]
fn cli_records_judgements_and_denies_workers_and_jobs() {
    use common::cli::{invoke, invoke_as, ok};
    let (dir, mut q) = fixture();
    let db = dir.path().join("queue.db");
    let goal = q.add_goal(new_goal("source")).unwrap().id();
    let id = q
        .add(dagq::domain::NewTask {
            goal_id: Some(goal),
            ..new_task("follow up")
        })
        .unwrap()
        .id();
    q.record_draft_origin(id,DraftOrigin::FollowUp,&json!({"source_goal_id":goal,"source_goal_state":"open","source_goal_provenance":"recorded"})).unwrap();
    let args = [
        "judge-follow-up",
        "1",
        "--classification",
        "required",
        "--reason",
        "needed for acceptance (1)",
        "--acceptance-item",
        "(1)",
        "--evidence",
        "task:1",
    ];
    for role in [
        "worker",
        "review-job",
        "recovery-job",
        "plan-review-job",
        "goal-review-job",
        "throughput-review-job",
    ] {
        let out = invoke_as(Some(role), &db, &args);
        assert!(!out.status.success(), "{role}");
        let refusal: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(
            refusal["denied"]["capability"], "follow_up.judge",
            "{role}: {refusal}"
        );
    }
    assert!(q.show(id).unwrap().membership_judgements.is_empty());
    let row = ok(&db, &args);
    assert_eq!(row["classification"], "required");
    assert_eq!(
        ok(&db, &["show", "1"])["membership_judgements"][0]["acceptance_version"],
        1
    );
    assert_eq!(
        ok(&db, &["goal", "show", "1"])["follow_up_memberships"][0]["material"]["source_goal_id"],
        1
    );
    assert!(
        !invoke(
            &db,
            &[
                "judge-follow-up",
                "1",
                "--classification",
                "unknown",
                "--reason",
                "x"
            ]
        )
        .status
        .success()
    );
}

#[test]
fn migrated_histories_preserve_adopts_and_require_them_only_for_unsafe_registration_facts() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("old.db");
    let raw = Connection::open(&db).unwrap();
    // Locate the migration by its statement so later migrations or renumbering
    // cannot make this legacy fixture silently include the migration under test.
    let previous = MIGRATIONS
        .iter()
        .position(|migration| migration.contains("CREATE TABLE follow_up_judgements"))
        .expect("follow-up membership migration");
    for m in &MIGRATIONS[..previous] {
        raw.execute_batch(m).unwrap();
    }
    raw.pragma_update(None, "application_id", 0x43545131)
        .unwrap();
    raw.pragma_update(None, "user_version", previous as i64)
        .unwrap();
    raw.execute_batch("INSERT INTO goals(id,title,description,acceptance,constraints,status) VALUES (1,'source','','criteria','','open'),(2,'destination','','criteria','','open');
        INSERT INTO goals(id,title,description,acceptance,constraints,status,closed_at,verdict) VALUES (3,'closed','','criteria','','open','2026-10-01T00:00:00.000Z','achieved');").unwrap();
    // 0=open at registration, 1=closed, 2=no goal, 3=missing event,
    // 4=conflicting source chain, 5=depth 3, 6=already adopted,
    // 7=conflicting closed_at, 8=source and draft moved several times,
    // 9=conflicting draft chain, 10=closed only AFTER registration.
    for case in 0..11i64 {
        let source = 10 + case;
        let draft = 100 + case;
        let original = match case {
            1 | 6 | 7 | 10 => Some(3),
            2 => None,
            _ => Some(1),
        };
        raw.execute("INSERT INTO tasks(id,title,description,acceptance,verification_commands,status,goal_id) VALUES (?1,'source','','criteria','[]','completed',?2)",params![source,original]).unwrap();
        raw.execute("INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit) VALUES (?1,?2,'integrated','claude','claude',?3)",params![format!("r{case}"),source,BASE]).unwrap();
        let initial_draft = if case == 1 || case == 6 {
            None
        } else {
            original
        };
        raw.execute("INSERT INTO tasks(id,title,description,acceptance,verification_commands,status,goal_id,follow_up_depth) VALUES (?1,'follow up','work','criteria','[\"true\"]','draft',?2,?3)",params![draft,initial_draft,if case==5 {3} else if case==6 {0} else {2}]).unwrap();
        let material =
            json!({"source_task_id":source,"source_run_id":format!("r{case}"),"index":0});
        raw.execute("INSERT INTO draft_origins(task_id,origin,material,created_at) VALUES (?1,'follow_up',?2,1)",params![draft,material.to_string()]).unwrap();
        if case != 3 {
            let closed = case == 1 || case == 6;
            let date = if case == 10 {
                "2026-09-30T00:00:00.000Z"
            } else {
                "2026-10-02T00:00:00.000Z"
            };
            raw.execute("INSERT INTO run_events(task_id,run_id,kind,payload,created_at) VALUES (?1,?2,'follow_up_registered',?3,?4)",params![source,format!("r{case}"),json!({"task_id":draft,"index":0,"goal_closed":closed}).to_string(),date]).unwrap();
        }
        for (task, from, to) in if case == 8 {
            vec![
                (source, original, Some(2)),
                (source, Some(2), Some(1)),
                (source, Some(1), Some(2)),
                (draft, initial_draft, Some(1)),
                (draft, Some(1), Some(2)),
            ]
        } else {
            vec![
                (source, if case == 4 { None } else { original }, Some(2)),
                (draft, if case == 9 { None } else { initial_draft }, Some(2)),
            ]
        } {
            raw.execute("UPDATE tasks SET goal_id=?2 WHERE id=?1", params![task, to])
                .unwrap();
            raw.execute("INSERT INTO run_events(task_id,kind,payload,created_at) VALUES (?1,'task_goal_changed',?2,'2026-10-03T00:00:00.000Z')",params![task,json!({"from":from,"to":to}).to_string()]).unwrap();
        }
        if case == 6 {
            raw.execute("INSERT INTO run_events(task_id,kind,payload) VALUES (?1,'follow_up_adopted','{\"by\":\"person\"}')",[draft]).unwrap();
        }
    }
    drop(raw);
    let mut q = migrated(&db);
    for case in 0..11i64 {
        let id = TaskId::new(100 + case);
        let origin = q.draft_origin(id).unwrap().unwrap().1;
        let state = match case {
            1 | 6 => "closed",
            2 => "none",
            3 | 4 | 7 | 9 => "unknown",
            _ => "open",
        };
        assert_eq!(origin["source_goal_state"], state, "case {case}");
        assert_eq!(
            origin["source_goal_id"],
            match case {
                1 | 6 | 10 => json!(3),
                2 | 3 | 4 | 7 | 9 => json!(null),
                _ => json!(1),
            },
            "case {case}"
        );
        assert_eq!(
            origin["source_goal_provenance"],
            if state == "unknown" {
                "unknown"
            } else {
                "restored"
            }
        );
        assert_eq!(q.show(id).unwrap().task.goal_id(), Some(GoalId::new(2)));
        assert!(q.show(id).unwrap().membership_judgements.is_empty());
        if [0, 5, 6, 8, 10].contains(&case) {
            q.judge_follow_up(
                id,
                judgement(Class::OutOfScope, Some(GoalId::new(2))),
                "planner",
            )
            .unwrap();
        }
        if [0, 6, 8, 10].contains(&case) {
            submit(&mut q, id).unwrap();
            assert_eq!(
                q.follow_up_depth(id).unwrap(),
                if case == 6 { 0 } else { 2 }
            );
        } else {
            let error = submit(&mut q, id).unwrap_err().to_string();
            assert!(error.contains("without a person"), "case {case}: {error}");
            assert_eq!(
                q.show(id).unwrap().task.status(),
                dagq::domain::TaskStatus::Draft
            );
            // An adopt answer permits the same runtime submission.
            let raw = Connection::open(&db).unwrap();
            raw.execute("INSERT INTO asks(kind,task_id,question,options,answer,asked_by,reason_category,answered_at) VALUES ('planner_question',?1,'adopt?','[]','adopt','planner','scope',1)",[id.as_i64()]).unwrap();
            drop(raw);
            submit(&mut q, id).unwrap();
            assert_eq!(q.follow_up_depth(id).unwrap(), 0);
        }
    }
}

#[test]
fn membership_changes_never_adopt_missing_closed_unknown_or_deep_follow_ups() {
    let (dir, mut q) = fixture();
    let source = q.add_goal(new_goal("source")).unwrap().id();
    let destination = q.add_goal(new_goal("destination")).unwrap().id();
    for (index, state, depth) in [
        (0, "none", 1),
        (1, "closed", 1),
        (2, "unknown", 2),
        (3, "open", 3),
    ] {
        let id = q.add(new_task(&format!("follow {index}"))).unwrap().id();
        let material = json!({"source_task_id":99,"source_run_id":"r","source_goal_id":if state=="none" || state=="unknown" {None} else {Some(source)},"source_goal_state":state,"source_goal_provenance":if state=="unknown" {"unknown"} else {"recorded"}});
        q.record_draft_origin(id, DraftOrigin::FollowUp, &material)
            .unwrap();
        q.set_follow_up_depth(id, depth).unwrap();
        q.set_goal(id, Some(destination)).unwrap();
        assert!(
            submit(&mut q, id)
                .unwrap_err()
                .to_string()
                .contains("without a person")
        );
        if state != "none" {
            let mut judge = judgement(Class::OutOfScope, Some(destination));
            if state == "unknown" {
                judge.source_goal_id = Some(source);
            }
            let row = q.judge_follow_up(id, judge, "planner").unwrap();
            assert_eq!(
                row["source_kind"],
                if state == "unknown" {
                    "named_by_judge"
                } else {
                    "recorded"
                }
            );
            assert!(
                submit(&mut q, id)
                    .unwrap_err()
                    .to_string()
                    .contains("without a person")
            );
        }
        assert_eq!(q.draft_origin(id).unwrap().unwrap().1, material);
        assert_eq!(q.follow_up_depth(id).unwrap(), depth);
        let raw = Connection::open(dir.path().join("queue.db")).unwrap();
        raw.execute("INSERT INTO asks(kind,task_id,question,options,answer,asked_by,reason_category,answered_at) VALUES ('planner_question',?1,'adopt?','[]','adopt','planner','scope',1)",[id.as_i64()]).unwrap();
        drop(raw);
        submit(&mut q, id).unwrap();
        assert_eq!(q.follow_up_depth(id).unwrap(), 0);
    }
}
