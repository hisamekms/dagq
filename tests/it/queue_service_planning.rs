//! The planning commands as the queue service's use cases (ADR-t3102-3
//! decisions 3 and 4, docs/design/queue-service.md): the host `dagq` of the
//! user, the inbox and a planner sends each to the service, which runs the
//! command line's own application call for the principal of the caller and
//! records its events as that principal, and prints what the command
//! prints on the DB. A worker's client-mode `dagq` is refused each one on
//! the service's side, the refusal recorded and nothing else written.

use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};

use crate::common::cli::invoke_with;
use crate::common::service::{Queue, queue};
use crate::common::{Bounded, WithoutActor};
use dagq::application::queue_writes::{self as writes, QueueWrite};
use dagq::domain::queue_service::{CREDENTIAL_FILE_ENV, Principal, SOCKET_ENV};
use dagq::domain::{ActorContext, ActorRole, EventId, PlannerOrigin, RequestId, TaskId};
use dagq::infrastructure::{queue_service as service, sqlite::SqliteQueue};

/// Who runs a step: the user, the inbox, or the runtime's planner `id`.
#[derive(Clone, Copy)]
enum Who {
    User,
    Inbox,
    Planner(i64),
}

impl Who {
    fn env(self) -> Vec<(&'static str, String)> {
        match self {
            Self::User => Vec::new(),
            Self::Inbox => vec![("DAGQ_ROLE", "inbox".to_owned())],
            Self::Planner(id) => vec![
                ("DAGQ_ROLE", "planner".to_owned()),
                ("DAGQ_ACTOR_ID", format!("planner:{id}")),
            ],
        }
    }

    fn actor(self) -> ActorContext {
        match self {
            Self::User => ActorContext::user(),
            Self::Inbox => ActorContext::new(ActorRole::Inbox, "inbox"),
            Self::Planner(id) => ActorContext::instance(ActorRole::Planner, id),
        }
    }
}

/// The served queue's `dagq args` as `who`, through its service.
fn served(queue: &Queue, who: Who, args: &[&str]) -> Output {
    let env = who.env();
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    invoke_with(&env, &queue.db, args)
}

/// What `write` prints when the command line runs it on the queue at `db`
/// itself as `who` (`compose::write_queue`, the call of its local path).
fn on_the_db(db: &Path, who: Who, write: &QueueWrite) -> Value {
    let actor = who.actor();
    let mut queue = SqliteQueue::open(db).unwrap().with_actor(actor.clone());
    dagq::compose::write_queue(&mut queue, &actor, write, |queue| {
        dagq::application::queue_writes::principal_owner(queue, &actor)
    })
    .unwrap()
}

/// The shape of `value`: its keys and the kinds of its values, the first
/// element standing for an array's.
fn shape(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), shape(value)))
                .collect(),
        ),
        Value::Array(items) => json!([items.first().map(shape)]),
        Value::String(_) => json!("string"),
        Value::Number(_) => json!("number"),
        Value::Bool(_) => json!("bool"),
        Value::Null => Value::Null,
    }
}

fn printed(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The actors of the served queue's events after `after`, in order.
fn actors_after(db: &Path, after: i64) -> Vec<Value> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.id > EventId::new(after))
        .map(|event| serde_json::to_value(event).unwrap()["actor"].clone())
        .collect()
}

fn last_event(db: &Path) -> i64 {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .last()
        .map_or(0, |event| event.id.as_i64())
}

/// Two queues made alike: one served, whose commands go through its
/// service, and one the test runs the same writes on itself.
fn twins() -> (Queue, Queue) {
    let pair = (queue(), queue());
    for queue in [&pair.0, &pair.1] {
        let opened = SqliteQueue::open(&queue.db).unwrap();
        // The planner each step names, the runtime's.
        assert_eq!(
            opened
                .open_planner(PlannerOrigin::Runtime, None)
                .unwrap()
                .id
                .as_i64(),
            1
        );
        // A follow-up of the fixture's run, whose task is in goal 1, for
        // judge-follow-up.
        let mut writable = SqliteQueue::open(&queue.db).unwrap();
        dagq::application::commands::planning::PlanningStore::add_goal(
            &mut writable,
            dagq::domain::NewGoal {
                title: "source".into(),
                description: String::new(),
                acceptance: "it works".into(),
                constraints: String::new(),
                doc: None,
                draft: false,
                priority: None,
                tags: Vec::new(),
            },
        )
        .unwrap();
        rusqlite::Connection::open(&queue.db)
            .unwrap()
            .execute(
                "UPDATE tasks SET goal_id=1 WHERE id=?1",
                [queue.run.task_id().as_i64()],
            )
            .unwrap();
        let task = dagq::application::TaskReads::show(&mut writable, queue.run.task_id())
            .unwrap()
            .task;
        let added = dagq::application::integrate::register_follow_ups(
            &mut writable,
            &task,
            queue.run.id(),
            Some(&json!([{"title": "follow", "description": "more"}])),
        );
        assert_eq!(added[0].task_id, TaskId::new(2));
    }
    pair
}

/// Acceptance (1) and (2): each planning command runs through the service
/// for the user, the inbox and a planner, prints what the command prints
/// on the DB, and records its events as that principal.
#[test]
fn the_planning_commands_go_through_the_service_as_their_caller_and_print_as_on_the_db() {
    let (served_queue, own) = twins();
    let planner = Who::Planner(1);
    let steps: Vec<(Who, Vec<&str>, QueueWrite)> = vec![
        (
            Who::User,
            vec!["goal", "add", "g"],
            QueueWrite::GoalAdd(writes::GoalAddWrite {
                title: "g".into(),
                ..Default::default()
            }),
        ),
        (
            Who::User,
            vec!["goal", "add", "d", "--draft", "--tag", "ops"],
            QueueWrite::GoalAdd(writes::GoalAddWrite {
                title: "d".into(),
                draft: true,
                tag: vec!["ops".into()],
                ..Default::default()
            }),
        ),
        (
            Who::Inbox,
            vec!["goal", "edit", "3", "--title", "d2", "--priority", "high"],
            QueueWrite::GoalEdit(writes::GoalEditWrite {
                id: 3,
                title: Some("d2".into()),
                priority: Some("high".into()),
                ..Default::default()
            }),
        ),
        (
            Who::User,
            vec!["goal", "ready", "3"],
            QueueWrite::GoalReady(writes::IdWrite { id: 3 }),
        ),
        (
            Who::User,
            vec!["add", "t", "--goal", "2", "--verify", "true"],
            QueueWrite::Add(writes::AddWrite {
                title: "t".into(),
                goal: Some(2),
                verify: vec!["true".into()],
                ..Default::default()
            }),
        ),
        (
            planner,
            vec!["add", "p", "--paths", "src/**"],
            QueueWrite::Add(writes::AddWrite {
                title: "p".into(),
                paths: vec!["src/**".into()],
                ..Default::default()
            }),
        ),
        (
            planner,
            vec!["edit", "4", "--title", "p2", "--no-verify"],
            QueueWrite::Edit(writes::EditWrite {
                task: 4,
                title: Some("p2".into()),
                no_verify: true,
                ..Default::default()
            }),
        ),
        (
            Who::User,
            vec!["dependency", "add", "4", "3"],
            QueueWrite::DependencyAdd(writes::DependencyWrite {
                task: 4,
                predecessor: Some(3),
                goal: None,
            }),
        ),
        (
            Who::User,
            vec!["dependency", "remove", "4", "3"],
            QueueWrite::DependencyRemove(writes::DependencyWrite {
                task: 4,
                predecessor: Some(3),
                goal: None,
            }),
        ),
        (
            planner,
            vec!["set-goal", "4", "2"],
            QueueWrite::SetGoal(writes::SetGoalWrite {
                task: 4,
                goal: Some(2),
            }),
        ),
        (
            planner,
            vec!["set-paths", "4", "--none"],
            QueueWrite::SetPaths(writes::SetPathsWrite {
                task: 4,
                paths: Vec::new(),
            }),
        ),
        (
            Who::Inbox,
            vec!["set-priority", "4", "high"],
            QueueWrite::SetPriority(writes::SetPriorityWrite {
                task: 4,
                level: Some("high".into()),
            }),
        ),
        (
            Who::User,
            vec![
                "revisit",
                "4",
                "--at",
                "2099-01-01T00:00:00Z",
                "--note",
                "n",
            ],
            QueueWrite::Revisit(writes::RevisitWrite {
                task: 4,
                at: Some("2099-01-01T00:00:00Z".into()),
                note: Some("n".into()),
            }),
        ),
        (
            planner,
            vec![
                "judge-follow-up",
                "2",
                "--classification",
                "undecided",
                "--reason",
                "not yet",
            ],
            QueueWrite::JudgeFollowUp(writes::JudgeFollowUpWrite {
                task: 2,
                classification: "undecided".into(),
                acceptance_item: Vec::new(),
                reason: "not yet".into(),
                evidence: Vec::new(),
                destination_goal: None,
                source_goal: None,
                corrects: None,
            }),
        ),
        (
            planner,
            vec!["submit", "4"],
            QueueWrite::Submit(writes::SubmitWrite {
                tasks: vec![4],
                ..Default::default()
            }),
        ),
        (
            planner,
            vec!["proposal", "withdraw", "1"],
            QueueWrite::ProposalWithdraw(writes::IdWrite { id: 1 }),
        ),
        (
            Who::User,
            vec!["ready", "3", "--bypass-review"],
            QueueWrite::Ready(writes::ReadyWrite {
                id: 3,
                bypass_review: true,
            }),
        ),
        (
            Who::User,
            vec!["draft", "3"],
            QueueWrite::Draft(writes::IdWrite { id: 3 }),
        ),
        (
            Who::Inbox,
            vec!["cancel", "3", "--reason", "stale"],
            QueueWrite::Cancel(writes::CancelWrite {
                id: 3,
                reason: Some("stale".into()),
                duplicate_of: None,
                note: None,
            }),
        ),
        (
            Who::User,
            vec!["goal", "review", "2"],
            QueueWrite::GoalReview(writes::IdWrite { id: 2 }),
        ),
        (
            Who::User,
            vec!["goal", "close", "3", "--verdict", "abandoned"],
            QueueWrite::GoalClose(writes::GoalCloseWrite {
                id: 3,
                verdict: "abandoned".into(),
            }),
        ),
        (
            Who::Inbox,
            vec!["request", "add", "--text", "plan it", "--ref", "task:4"],
            QueueWrite::RequestAdd(writes::RequestAddWrite {
                text: Some("plan it".into()),
                note: None,
                refs: vec!["task:4".into()],
                priority: None,
            }),
        ),
    ];
    for (who, args, write) in &steps {
        let before = last_event(&served_queue.db);
        let answered = printed(&served(&served_queue, *who, args));
        let local = on_the_db(&own.db, *who, write);
        assert_eq!(shape(&answered), shape(&local), "{args:?}");
        let actor = who.actor();
        let recorded = actors_after(&served_queue.db, before);
        assert!(!recorded.is_empty(), "{args:?} recorded nothing");
        for recorded in recorded {
            assert_eq!(recorded["role"], actor.role().as_str(), "{args:?}");
            // The inbox's actor id names its session.
            if !matches!(who, Who::Inbox) {
                assert_eq!(recorded["id"], actor.actor_id(), "{args:?}");
            }
        }
    }

    // The request's own planner declines it, and the user marks a change.
    for queue in [&served_queue, &own] {
        let mut opened = SqliteQueue::open(&queue.db).unwrap();
        let started = dagq::application::PlanRequestStore::open_request_planner(
            &mut opened,
            RequestId::new(1),
            None,
        )
        .unwrap();
        assert!(
            matches!(
                started,
                dagq::application::RequestPlannerStart::Opened { .. }
            ),
            "{started:?}"
        );
    }
    let decline = ["request", "decline", "1", "--reason", "done"];
    let before = last_event(&served_queue.db);
    let answered = printed(&served(&served_queue, Who::Planner(2), &decline));
    let local = on_the_db(
        &own.db,
        Who::Planner(2),
        &QueueWrite::RequestDecline(writes::RequestDeclineWrite {
            id: 1,
            reason: "done".into(),
            continue_run: false,
        }),
    );
    assert_eq!(shape(&answered), shape(&local));
    assert_eq!(answered["status"], "declined");
    assert!(
        actors_after(&served_queue.db, before)
            .iter()
            .any(|actor| *actor == json!({"role": "planner", "id": "planner:2"}))
    );
    let marked = printed(&served(
        &served_queue,
        Who::User,
        &["mark", "m", "--note", "n"],
    ));
    let retracted = printed(&served(
        &served_queue,
        Who::Inbox,
        &["mark", "--retract", &marked["id"].to_string()],
    ));
    assert_eq!(retracted["kind"], "mark_retracted", "{retracted}");
    assert_eq!(retracted["detail"]["mark"], marked["id"], "{retracted}");
    assert_eq!(retracted["detail"]["by"], "inbox", "{retracted}");
}

/// A worker's client-mode `dagq`, as the principal of its run's token.
fn worker(queue: &Queue, credential: &Path, args: &[&str]) -> Output {
    let elsewhere = queue.dir().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .current_dir(elsewhere)
        .env(SOCKET_ENV, service::socket_path(queue.dir()))
        .env(CREDENTIAL_FILE_ENV, credential)
        .env("DAGQ_ROLE", "worker")
        .args(args)
        .bounded_output()
        .unwrap()
}

/// Acceptance (1) and (3): every planning command is refused on the
/// service's side to a principal its role does not grant it (a worker's,
/// and a planner's for what the command line refuses a planner), each
/// refusal recorded as that principal and nothing else written.
#[test]
fn the_service_refuses_each_planning_command_its_caller_may_not_run_and_records_it() {
    let (queue, _) = twins();
    // The service runs for the user's first command.
    printed(&served(&queue, Who::User, &["goal", "add", "g"]));
    let credential = service::issue(
        queue.dir(),
        &Principal::worker(queue.run.id(), queue.run.task_id()),
        1,
    )
    .unwrap()
    .file;
    let task = "2";
    let commands: Vec<Vec<&str>> = vec![
        vec!["add", "x"],
        vec!["edit", task, "--title", "x"],
        vec!["submit", task],
        vec!["cancel", task, "--reason", "stale"],
        vec!["dependency", "add", task, "--goal", "1"],
        vec!["dependency", "remove", task, "--goal", "1"],
        vec!["set-goal", task, "1"],
        vec!["set-paths", task, "--none"],
        vec!["set-priority", task, "high"],
        vec!["revisit", task, "--clear"],
        vec![
            "judge-follow-up",
            task,
            "--classification",
            "undecided",
            "--reason",
            "r",
        ],
        vec!["draft", task],
        vec!["ready", task],
        vec!["ready", task, "--bypass-review"],
        vec!["goal", "add", "x"],
        vec!["goal", "edit", "1", "--title", "x"],
        vec!["goal", "close", "1", "--verdict", "abandoned"],
        vec!["goal", "ready", "1"],
        vec!["goal", "review", "1"],
        vec!["proposal", "withdraw", "1"],
        vec!["request", "add", "--text", "x"],
        vec!["request", "decline", "1", "--reason", "r"],
        vec!["mark", "m"],
    ];
    let worker_id = format!("worker:{}", queue.run.id());
    for args in &commands {
        let before = last_event(&queue.db);
        let output = worker(&queue, &credential, args);
        assert!(!output.status.success(), "{args:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(
            error["queue_service"]["code"], "authorization_denied",
            "{args:?}: {error}"
        );
        assert_eq!(error["denied"]["role"], "worker", "{args:?}");
        let recorded = SqliteQueue::open(&queue.db)
            .unwrap()
            .all_events()
            .unwrap()
            .into_iter()
            .filter(|event| event.id > EventId::new(before))
            .map(|event| serde_json::to_value(event).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(recorded.len(), 1, "{args:?}: {recorded:?}");
        assert_eq!(recorded[0]["kind"], "authorization_denied", "{args:?}");
        assert_eq!(recorded[0]["actor"]["id"], worker_id.as_str(), "{args:?}");
    }
    // What the command line refuses a planner, the service refuses the
    // planner's principal.
    for args in [
        vec!["ready", task, "--bypass-review"],
        vec!["goal", "ready", "1"],
        vec!["goal", "review", "1"],
        vec!["request", "add", "--text", "x"],
    ] {
        let output = served(&queue, Who::Planner(1), &args);
        assert!(!output.status.success(), "{args:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["queue_service"]["code"], "authorization_denied");
        assert_eq!(error["denied"]["role"], "planner", "{args:?}");
        assert_eq!(error["denied"]["reason"], "not granted", "{args:?}");
    }
}
