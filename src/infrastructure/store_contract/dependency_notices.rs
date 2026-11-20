//! Dependency edges and their notices share the caller's transaction on
//! both adapters. Explicit deployment options avoid changing process env.
use super::ContractStore;
use crate::domain::actor::ActorContext;
use crate::domain::goal_owner::Owner;
use crate::domain::oidc::OidcIdentity;
use crate::domain::plan_request::Creator;
use crate::domain::{ActorRole, EventKind, GoalId, NewTask, RunEvent, TaskId};
use crate::infrastructure::sql_exec::{SqlRead, SqlWrite, sql_params};
use crate::infrastructure::store_rows::event_row;
use crate::infrastructure::store_writes::{
    DependencyRecommendation, insert_dependency, insert_goal_dependency, insert_task_recommended,
    set_goal_owner,
};
use anyhow::{Result, ensure};
use serde_json::json;

fn owner(subject: &str) -> Owner {
    Owner::new(OidcIdentity {
        issuer: "https://notice.example".into(),
        subject: subject.into(),
    })
}
fn options(reason: Option<&str>) -> DependencyRecommendation<'_> {
    DependencyRecommendation {
        reason,
        team: true,
        require_reason: true,
    }
}
fn fixture(tx: &impl SqlWrite) -> Result<(TaskId, TaskId, GoalId)> {
    let goal = GoalId::new(tx.insert_id(
        "INSERT INTO goals(title,priority) VALUES ('notice target',1)",
        sql_params![],
    )?);
    set_goal_owner(tx, goal, Some(&owner("bob")))?;
    let waiting_goal = GoalId::new(tx.insert_id(
        "INSERT INTO goals(title,priority) VALUES ('notice waiting',2)",
        sql_params![],
    )?);
    set_goal_owner(tx, waiting_goal, Some(&owner("alice")))?;
    let task = |goal: GoalId| -> Result<TaskId> {
        Ok(TaskId::new(tx.insert_id("INSERT INTO tasks(title,description,acceptance,verification_commands,status,goal_id) VALUES ('notice','','','[]','draft',?1)",sql_params![goal])?))
    };
    Ok((task(waiting_goal)?, task(goal)?, goal))
}
fn events(tx: &impl SqlRead, task: TaskId) -> Result<Vec<RunEvent>> {
    Ok(tx.query_map(
        "SELECT * FROM run_events WHERE task_id=?1 ORDER BY id",
        sql_params![task],
        event_row,
    )?)
}
fn notices(tx: &impl SqlRead, task: TaskId) -> Result<Vec<RunEvent>> {
    Ok(events(tx, task)?
        .into_iter()
        .filter(|e| e.kind == EventKind::CrossOwnerDependencyNoticed)
        .collect())
}
fn add(
    tx: &impl SqlWrite,
    task: TaskId,
    predecessor: TaskId,
    goal: GoalId,
    goal_edge: bool,
    options: DependencyRecommendation<'_>,
) -> Result<()> {
    if goal_edge {
        insert_goal_dependency(tx, task, goal, "now", options)
    } else {
        insert_dependency(tx, task, predecessor, "now", options)
    }
}
fn new_task(goal: GoalId, predecessor: TaskId, goal_edge: bool) -> NewTask {
    NewTask {
        title: "notice registration".into(),
        description: "d".into(),
        acceptance: "a".into(),
        verification_commands: vec!["true".into()],
        required_evidence: vec![],
        paths: vec![],
        priority: None,
        change: None,
        dependencies: if goal_edge { vec![] } else { vec![predecessor] },
        goal_dependencies: if goal_edge { vec![goal] } else { vec![] },
        goal_id: None,
        context: String::new(),
        provider: None,
        worker_mode: None,
        wait_for_build: false,
        execution_class: Default::default(),
    }
}

fn check_notice(
    tx: &impl SqlRead,
    task: TaskId,
    expected: serde_json::Value,
    role: ActorRole,
) -> Result<()> {
    let recorded = notices(tx, task)?;
    ensure!(recorded.len() == 1);
    let notice = &recorded[0];
    ensure!(notice.payload == expected);
    ensure!(notice.actor.as_ref().unwrap().role == role.as_str());
    ensure!(notice.actor.as_ref().unwrap().id == format!("{}:4384", role.as_str()));
    Ok(())
}

/// Exercise the distinct SQL entry paths: both dependency writers after
/// planner registration, both registration flags, and plan-review application.
/// These read inherited priorities, an explicit task priority and the actor;
/// the role/reason decision table lives in domain::dependency_notice.
pub fn dependency_notices_records<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    store.set_actor(ActorContext::instance(ActorRole::Planner, 4384));
    for goal_edge in [false, true] {
        let tx = store.write_entry("contract_dependency_notice")?;
        let (task, predecessor, goal) = fixture(&tx)?;
        let mut new = new_task(goal, predecessor, goal_edge);
        new.goal_id = crate::infrastructure::store_rows::read_task(&tx, task)?.goal_id();
        new.dependencies.clear();
        new.goal_dependencies.clear();
        let added = insert_task_recommended(&tx, new, "now", Creator::Other, options(None))?;
        let task = added.id();
        let recorded = events(&tx, task)?;
        ensure!(recorded.len() == 1 && recorded[0].kind == EventKind::TaskCreated);
        add(
            &tx,
            task,
            predecessor,
            goal,
            goal_edge,
            options(Some("content prerequisite")),
        )?;
        ensure!(if goal_edge {
            tx.exists(
                "SELECT 1 FROM task_goal_dependencies WHERE task_id=?1 AND goal_id=?2",
                sql_params![task, goal],
            )?
        } else {
            tx.exists(
                "SELECT 1 FROM task_dependencies WHERE task_id=?1 AND predecessor_id=?2",
                sql_params![task, predecessor],
            )?
        });
        let expected = json!({"task_id":task,"depends_on":if goal_edge {json!({"goal_id":goal})}else{json!({"task_id":predecessor})},"owner":owner("bob"),"priority_inversion":true,"reason":"content prerequisite"});
        check_notice(&tx, task, expected.clone(), ActorRole::Planner)?;
        // Each physical edge table suppresses a duplicate's notice and reason.
        add(
            &tx,
            task,
            predecessor,
            goal,
            goal_edge,
            options(Some("discard this duplicate reason")),
        )?;
        add(&tx, task, predecessor, goal, goal_edge, options(None))?;
        check_notice(&tx, task, expected, ActorRole::Planner)?;
        let added = events(&tx, task)?
            .into_iter()
            .find(|e| {
                e.kind
                    == if goal_edge {
                        EventKind::GoalDependencyAdded
                    } else {
                        EventKind::DependencyAdded
                    }
            })
            .unwrap();
        ensure!(
            added.payload
                == if goal_edge {
                    json!({"goal_id":goal})
                } else {
                    json!({"predecessor_id":predecessor})
                }
        );
    }
    store.set_actor(ActorContext::instance(ActorRole::User, 4384));
    for goal_edge in [false, true] {
        let tx = store.write_entry("contract_dependency_registration")?;
        let (task, predecessor, goal) = fixture(&tx)?;
        let mut new = new_task(goal, predecessor, goal_edge);
        new.goal_id = crate::infrastructure::store_rows::read_task(&tx, task)?.goal_id();
        new.priority = Some(crate::domain::Priority::Low);
        let added = insert_task_recommended(&tx, new, "now", Creator::Person, options(None))?;
        check_notice(
            &tx,
            added.id(),
            json!({"task_id":added.id(),"depends_on":if goal_edge {json!({"goal_id":goal})}else{json!({"task_id":predecessor})},"owner":owner("bob"),"priority_inversion":false,"reason":null}),
            ActorRole::User,
        )?;
    }
    store.set_actor(ActorContext::instance(ActorRole::Planner, 4384));
    let tx = store.write_entry("contract_dependency_plan_review")?;
    let (task, predecessor, _) = fixture(&tx)?;
    crate::infrastructure::plan_reviews::apply_action(
        &tx,
        &crate::domain::plan_review::PlanReviewAction::AddDependency {
            task_id: task,
            depends_on: predecessor,
        },
        crate::domain::ProposalId::new(1),
        "now",
        options(None),
    )?;
    check_notice(
        &tx,
        task,
        json!({"task_id":task,"depends_on":{"task_id":predecessor},"owner":owner("bob"),"priority_inversion":true,"reason":null}),
        ActorRole::Planner,
    )
}

/// One representative for each SQL read/gate: local deployment, a task's
/// inherited matching owner, an unrecorded waiting owner and a goal with no
/// owner. Reasons are fixed here; domain tests enumerate their policy cases.
pub fn dependency_notices_ignored<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    store.set_actor(ActorContext::instance(ActorRole::Planner, 4384));
    for (mode, goal_edge) in [
        ("local", false),
        ("same", false),
        ("missing_waiting", false),
        ("missing_target", true),
    ] {
        let tx = store.write_entry("contract_dependency_notice_ignored")?;
        let (task, predecessor, goal) = fixture(&tx)?;
        let waiting_goal = crate::infrastructure::store_rows::read_task(&tx, task)?
            .goal_id()
            .unwrap();
        match mode {
            "same" => {
                set_goal_owner(&tx, goal, Some(&owner("alice")))?;
                ensure!(
                    crate::infrastructure::store_rows::read_task(&tx, predecessor)?
                        .owner()
                        .person()
                        == Some(&owner("alice"))
                );
            }
            "missing_waiting" => {
                set_goal_owner(&tx, waiting_goal, None)?;
                ensure!(
                    crate::infrastructure::store_rows::read_task(&tx, task)?
                        .owner()
                        .person()
                        .is_none()
                );
            }
            "missing_target" => {
                set_goal_owner(&tx, goal, None)?;
                ensure!(
                    crate::infrastructure::store_rows::read_goal(&tx, goal)?
                        .owner()
                        .is_none()
                );
            }
            _ => {}
        }
        add(
            &tx,
            task,
            predecessor,
            goal,
            goal_edge,
            DependencyRecommendation {
                team: mode != "local",
                ..options(Some("not retained"))
            },
        )?;
        let recorded = events(&tx, task)?;
        ensure!(recorded.len() == 1);
        ensure!(
            recorded[0].payload
                == if goal_edge {
                    json!({"goal_id":goal})
                } else {
                    json!({"predecessor_id":predecessor})
                }
        );
    }
    Ok(())
}

/// One representative refusal proves the SQL writer reaches the domain
/// policy before inserting the edge or either event.
pub fn dependency_notices_refused<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    store.set_actor(ActorContext::instance(ActorRole::Planner, 4384));
    let tx = store.write_entry("contract_dependency_notice_refused")?;
    let (task, predecessor, _) = fixture(&tx)?;
    let error = insert_dependency(&tx, task, predecessor, "now", options(None)).unwrap_err();
    ensure!(
        error
            .downcast_ref::<crate::domain::dependency_notice::RecommendationRequired>()
            .is_some()
    );
    ensure!(events(&tx, task)?.is_empty());
    ensure!(!tx.exists(
        "SELECT 1 FROM task_dependencies WHERE task_id=?1",
        sql_params![task]
    )?);
    Ok(())
}

/// Dropping the write transaction leaves neither its edge nor its event.
/// A committed fixture survives, proving the test is reading the same DB.
pub fn dependency_notices_undone<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    store.set_actor(ActorContext::user());
    let tx = store.write_entry("contract_dependency_fixture")?;
    let (task, predecessor, goal) = fixture(&tx)?;
    let waiting_goal = crate::infrastructure::store_rows::read_task(&tx, task)?
        .goal_id()
        .unwrap();
    S::commit(tx)?;
    for goal_edge in [false, true] {
        let tx = store.write_entry("contract_dependency_undone")?;
        add(&tx, task, predecessor, goal, goal_edge, options(None))?;
        ensure!(notices(&tx, task)?.len() == 1);
        drop(tx);
        let tx = store.write_entry("contract_dependency_read_undone")?;
        ensure!(notices(&tx, task)?.is_empty());
        ensure!(!tx.exists(
            "SELECT 1 FROM task_dependencies WHERE task_id=?1",
            sql_params![task]
        )?);
        ensure!(!tx.exists(
            "SELECT 1 FROM task_goal_dependencies WHERE task_id=?1",
            sql_params![task]
        )?);
        ensure!(crate::infrastructure::store_rows::read_task(&tx, task)?.id() == task);
    }
    store.set_actor(ActorContext::instance(ActorRole::Planner, 4384));
    let tx = store.write_entry("contract_dependency_registration_undone")?;
    let mut new = new_task(goal, predecessor, false);
    new.goal_id = Some(waiting_goal);
    ensure!(insert_task_recommended(&tx, new, "now", Creator::Other, options(None)).is_err());
    let attempted = TaskId::new(tx.query_row(
        "SELECT id FROM tasks WHERE title='notice registration'",
        sql_params![],
        |r| r.get(0),
    )?);
    let recorded = events(&tx, attempted)?;
    ensure!(recorded.len() == 1 && recorded[0].kind == EventKind::TaskCreated);
    drop(tx);
    let tx = store.write_entry("contract_dependency_registration_absent")?;
    ensure!(!tx.exists(
        "SELECT 1 FROM tasks WHERE title='notice registration'",
        sql_params![]
    )?);
    ensure!(events(&tx, attempted)?.is_empty());
    drop(tx);
    let tx = store.write_entry("contract_dependency_cleanup")?;
    tx.execute(
        "DELETE FROM tasks WHERE id IN (?1,?2)",
        sql_params![task, predecessor],
    )?;
    tx.execute(
        "DELETE FROM goals WHERE id IN (?1,?2)",
        sql_params![goal, waiting_goal],
    )?;
    S::commit(tx)
}
