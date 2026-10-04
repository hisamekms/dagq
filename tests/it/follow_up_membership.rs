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

/// A goal `source` whose one task landed (completed) from run `run`, and
/// an open goal `other` to move follow-ups to.
struct Landed {
    dir: tempfile::TempDir,
    q: SqliteQueue,
    source: GoalId,
    other: GoalId,
    task: TaskId,
    run: dagq::domain::RunId,
    /// The receipt's follow_ups so far: `integrate` registers each index
    /// of a run once.
    follow_ups: Vec<serde_json::Value>,
}

fn landed() -> Landed {
    let (dir, mut q) = fixture();
    let source = q.add_goal(new_goal("source")).unwrap().id();
    let other = q.add_goal(new_goal("destination")).unwrap().id();
    let task = q
        .add(dagq::domain::NewTask {
            goal_id: Some(source),
            ..new_task("source task")
        })
        .unwrap()
        .id();
    q.transition(task, TaskAction::BypassReview).unwrap();
    let ClaimOutcome::Claimed { run, .. } = q.claim(&base()).unwrap() else {
        panic!("claim")
    };
    let mut landed = Landed {
        dir,
        q,
        source,
        other,
        task,
        run: run.id().clone(),
        follow_ups: Vec::new(),
    };
    landed.set_status(task, "completed");
    landed
        .q
        .register_supervisor(
            &dagq::domain::LeaseToken::new(REVIEW_OWNER),
            std::process::id(),
            2,
            "test",
        )
        .unwrap();
    landed
}

impl Landed {
    /// A follow-up the landed task's receipt names, registered as
    /// `integrate` registers it.
    fn follow_up(&mut self, title: &str) -> TaskId {
        self.follow_ups
            .push(json!({"title": title, "description": "new work"}));
        let added = dagq::application::integrate::register_follow_ups(
            &mut self.q,
            &q_task(self.task, self.source),
            &self.run,
            Some(&json!(self.follow_ups)),
        );
        assert_eq!(added.len(), 1);
        added[0].task_id
    }

    fn set_status(&self, task: TaskId, status: &str) {
        Connection::open(self.dir.path().join("queue.db"))
            .unwrap()
            .execute(
                "UPDATE tasks SET status=?2 WHERE id=?1",
                params![task.as_i64(), status],
            )
            .unwrap();
    }

    fn reviewable(&self) -> bool {
        use dagq::application::GoalReviewStore;
        self.q
            .goal_review_candidates()
            .unwrap()
            .contains(&self.source)
    }

    /// Why `goal close --verdict achieved` refuses the source goal, or
    /// `None` when it closed it.
    fn close_refused(&mut self) -> Option<String> {
        self.q
            .close_goal(self.source, dagq::domain::GoalVerdict::Achieved)
            .err()
            .map(|e| e.to_string())
    }

    fn change_acceptance(&mut self, text: &str) {
        self.q
            .edit_goal(
                self.source,
                GoalEdit {
                    acceptance: Some(text.into()),
                    ..Default::default()
                },
            )
            .unwrap();
    }

    fn out_of_scope(&mut self, task: TaskId) -> serde_json::Value {
        self.q
            .judge_follow_up(
                task,
                judgement(Class::OutOfScope, Some(self.other)),
                "planner",
            )
            .unwrap()
    }
}

/// ADR-t1504-2 decision 8: a goal whose follow-ups are judged out of scope
/// is reviewed and closes without waiting for their work; one not judged,
/// undecided, or judged before its acceptance changed keeps it from both
/// until it is judged against the current acceptance.
#[test]
fn out_of_scope_follow_ups_let_the_goal_close_and_unjudged_or_stale_ones_do_not() {
    let mut l = landed();
    let first = l.follow_up("first improvement");
    // A draft of the goal waits as one of its tasks.
    assert!(!l.reviewable());
    l.out_of_scope(first);
    assert_eq!(l.q.show(first).unwrap().task.goal_id(), Some(l.other));
    assert!(
        l.reviewable(),
        "an out-of-scope draft elsewhere does not hold it"
    );

    // Moved out without a judgement, then undecided: still the goal's.
    let second = l.follow_up("second improvement");
    l.q.set_goal(second, Some(l.other)).unwrap();
    assert!(!l.reviewable());
    let refused = l.close_refused().unwrap();
    assert!(
        refused.contains(&format!("{second} not judged")),
        "{refused}"
    );
    let mut undecided = judgement(Class::Undecided, None);
    undecided.acceptance_items.clear();
    l.q.judge_follow_up(second, undecided, "planner").unwrap();
    assert!(!l.reviewable());
    assert!(l.close_refused().unwrap().contains("undecided"));
    l.out_of_scope(second);
    assert!(l.reviewable());

    // A changed acceptance makes both judgements stale until rechecked.
    l.change_acceptance("a stricter acceptance");
    assert!(!l.reviewable());
    let refused = l.close_refused().unwrap();
    assert!(
        refused.contains(&format!(
            "{first} judged before the acceptance changed, {second} judged before the acceptance changed"
        )),
        "{refused}"
    );
    l.out_of_scope(first);
    assert!(!l.reviewable());
    l.out_of_scope(second);
    assert!(l.reviewable());
    // Their work never ran: it does not hold the achieved close.
    assert_eq!(l.close_refused(), None);
    assert!(l.q.show_goal(l.source).unwrap().closed);
}

/// ADR-t1504-2 decision 8: a follow-up judged required holds its source
/// goal until it ends, in the goal or (past ready, where the judgement
/// cannot move it) outside it.
#[test]
fn a_required_follow_up_holds_its_goal_until_it_ends() {
    let mut l = landed();
    let inside = l.follow_up("required inside");
    let outside = l.follow_up("required outside");
    l.q.set_goal(outside, Some(l.other)).unwrap();
    l.set_status(outside, "submitted");
    for task in [inside, outside] {
        l.q.judge_follow_up(task, judgement(Class::Required, None), "planner")
            .unwrap();
    }
    assert_eq!(l.q.show(inside).unwrap().task.goal_id(), Some(l.source));
    assert_eq!(l.q.show(outside).unwrap().task.goal_id(), Some(l.other));
    assert!(!l.reviewable());
    let refused = l.close_refused().unwrap();
    assert!(refused.contains("1 task(s) draft"), "{refused}");
    l.set_status(inside, "completed");
    assert!(!l.reviewable());
    let refused = l.close_refused().unwrap();
    assert!(
        refused.contains(&format!("{outside} required but outside the goal")),
        "{refused}"
    );
    l.set_status(outside, "completed");
    assert!(l.reviewable());
    assert_eq!(l.close_refused(), None);
}

const REVIEW_OWNER: &str = "goal-review-owner";

impl Landed {
    /// Start a goal review of the source goal as the supervisor
    /// `REVIEW_OWNER` (registered by [`landed`]) does.
    fn begin_review(&mut self) -> dagq::application::GoalReviewJob {
        use dagq::application::GoalReviewStore;
        use dagq::domain::{
            LeaseToken,
            actor_model::{ActorLaunch, ModelRole},
        };
        let token = LeaseToken::new(REVIEW_OWNER);
        let dir = self.dir.path().to_path_buf();
        self.q
            .begin_goal_review(
                self.source,
                &token,
                &dir.join("goal-reviews"),
                &dir,
                &ActorLaunch::default_of(ModelRole::GoalReview),
            )
            .unwrap()
            .expect("the goal is reviewed")
    }

    /// Apply an `achieved` verdict of `job`, as the supervisor does for a
    /// verdict it reaped or collected across a handoff (task 1425).
    fn finish_achieved(
        &mut self,
        job: &dagq::application::GoalReviewJob,
    ) -> dagq::application::GoalReviewApplied {
        use dagq::application::{GoalReviewApply, GoalReviewStore};
        use dagq::domain::goal_review::{GoalReviewDecision, GoalReviewVerdict};
        let apply = GoalReviewApply {
            verdict: GoalReviewVerdict::parse(r#"{"verdict":"achieved","summary":"done"}"#)
                .unwrap(),
            decision: GoalReviewDecision::Achieved,
            overridden: None,
            ask: None,
            duration_secs: 1,
            session: None,
        };
        self.q
            .finish_goal_review(job, &dagq::domain::LeaseToken::new(REVIEW_OWNER), &apply)
            .unwrap()
    }

    fn review_outcome(&self, job: &dagq::application::GoalReviewJob) -> String {
        Connection::open(self.dir.path().join("queue.db"))
            .unwrap()
            .query_row(
                "SELECT outcome FROM goal_reviews WHERE id=?1",
                [job.id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn held(&self) -> bool {
        use dagq::application::GoalReviewStore;
        self.q
            .goal_review_holds()
            .unwrap()
            .iter()
            .any(|hold| hold.goal_id == self.source)
    }
}

/// ADR-t1504-2 decision 8: a review started before a follow-up was
/// registered, a membership changed or the acceptance changed is not
/// applied while the change leaves the goal unsettled: `finish_goal_review`
/// (the one apply path, for a verdict reaped live or collected across a
/// handoff, task 1425) leaves the goal open and the row `interrupted`, and
/// `goal close --verdict achieved` refuses the goal too, until the input is
/// settled and the review is taken again.
#[test]
fn a_review_whose_follow_ups_or_acceptance_changed_does_not_close_the_goal() {
    for change in ["registered", "corrected", "acceptance"] {
        let mut l = landed();
        let first = l.follow_up("first improvement");
        let row = l.out_of_scope(first);
        assert!(l.reviewable(), "{change}");
        let job = l.begin_review();
        let second = match change {
            "registered" => Some(l.follow_up("found meanwhile")),
            "corrected" => {
                let mut correction = judgement(Class::Required, None);
                correction.corrects = row["id"].as_i64();
                l.q.judge_follow_up(first, correction, "planner").unwrap();
                None
            }
            _ => {
                l.change_acceptance("a stricter acceptance");
                None
            }
        };
        let applied = l.finish_achieved(&job);
        assert!(applied.stale && !applied.closed, "{change}");
        assert!(!l.q.show_goal(l.source).unwrap().closed, "{change}");
        assert_eq!(l.review_outcome(&job), "interrupted", "{change}");
        assert!(l.close_refused().is_some(), "{change}");
        assert!(!l.reviewable(), "{change}");
        // Settled again, the goal is reviewed again.
        match change {
            "registered" => {
                l.out_of_scope(second.unwrap());
            }
            "corrected" => l.set_status(first, "completed"),
            _ => {
                l.out_of_scope(first);
            }
        }
        assert!(l.reviewable(), "{change}");
    }
}

/// ADR-t1504-2 decision 8: a verdict whose input changed during the review
/// is not used even when the change left the goal closable again before the
/// verdict came: a follow-up registered and judged out of scope, an
/// out-of-scope judgement recorded again, a required judgement of an ended
/// follow-up corrected to out of scope, or the acceptance changed and the
/// follow-up rejudged. The goal's own tasks are the same in each, so only
/// the acceptance version and the judgements in the fingerprint tell the
/// verdict is stale; the goal is reviewed again and may close.
#[test]
fn a_review_whose_input_changed_and_settled_again_is_not_applied() {
    for change in ["registered", "rechecked", "corrected", "acceptance"] {
        let mut l = landed();
        let first = l.follow_up("first improvement");
        l.out_of_scope(first);
        // A required follow-up that already landed in the goal.
        let ended = (change == "corrected").then(|| {
            let ended = l.follow_up("required and done");
            let row =
                l.q.judge_follow_up(ended, judgement(Class::Required, None), "planner")
                    .unwrap();
            l.set_status(ended, "completed");
            (ended, row)
        });
        assert!(l.reviewable(), "{change}");
        let tasks = l.q.show_goal(l.source).unwrap().tasks.len();
        let job = l.begin_review();
        match change {
            "registered" => {
                let found = l.follow_up("found meanwhile");
                l.out_of_scope(found);
            }
            "rechecked" => {
                l.out_of_scope(first);
            }
            "corrected" => {
                let (ended, row) = ended.unwrap();
                let mut correction = judgement(Class::OutOfScope, Some(l.other));
                correction.corrects = row["id"].as_i64();
                l.q.judge_follow_up(ended, correction, "planner").unwrap();
                // An ended task stays where it is.
                assert_eq!(l.q.show(ended).unwrap().task.goal_id(), Some(l.source));
            }
            _ => {
                l.change_acceptance("a stricter acceptance");
                l.out_of_scope(first);
            }
        }
        // The goal is closable again, with the same tasks as the review saw.
        assert!(l.reviewable(), "{change}");
        assert_eq!(
            l.q.show_goal(l.source).unwrap().tasks.len(),
            tasks,
            "{change}"
        );
        let applied = l.finish_achieved(&job);
        assert!(applied.stale && !applied.closed, "{change}");
        assert!(!l.q.show_goal(l.source).unwrap().closed, "{change}");
        assert_eq!(l.review_outcome(&job), "interrupted", "{change}");
        // The interrupted review does not count: the goal is reviewed
        // again, and its next verdict closes it.
        assert!(l.reviewable(), "{change}");
        let again = l.begin_review();
        assert!(l.finish_achieved(&again).closed, "{change}");
        assert!(l.q.show_goal(l.source).unwrap().closed, "{change}");
    }
}

/// ADR-t1504-2 decision 8: a review that ended (here `failed`, which holds
/// the goal for a person) is taken again by itself when only a follow-up's
/// judgement or the goal's acceptance changes, as when its tasks change.
#[test]
fn a_failed_review_is_taken_again_when_a_judgement_or_the_acceptance_changes() {
    use dagq::application::{GoalReviewFailure, GoalReviewStore};
    let mut l = landed();
    let first = l.follow_up("first improvement");
    l.out_of_scope(first);
    let fail = |l: &mut Landed| {
        let job = l.begin_review();
        l.q.fail_goal_review(
            &job,
            &dagq::domain::LeaseToken::new(REVIEW_OWNER),
            &GoalReviewFailure {
                error: "no verdict".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(l.review_outcome(&job), "failed");
        assert!(!l.reviewable());
        assert!(l.held());
    };
    fail(&mut l);
    // The judgement recorded again (a recheck) is new input.
    l.out_of_scope(first);
    assert!(l.reviewable());
    assert!(!l.held());
    fail(&mut l);
    // A changed acceptance waits for the rechecks, then is new input.
    l.change_acceptance("a stricter acceptance");
    assert!(!l.reviewable());
    assert!(!l.held());
    l.out_of_scope(first);
    assert!(l.reviewable());
}
