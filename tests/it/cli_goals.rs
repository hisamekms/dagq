use crate::common;
use dagq::domain::LeaseToken;

use common::cli::*;

use std::path::Path;

use dagq::infrastructure::sqlite::SqliteQueue;
use serde_json::Value;

#[test]
fn goals_group_tasks_and_report_counts_by_status() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("goals.db");
    common::template::queue(&db);
    assert_eq!(ok(&db, &["goal", "list"]), serde_json::json!([]));
    let goal = ok(
        &db,
        &[
            "goal",
            "add",
            "Goal groups",
            "--description",
            "ADR-0009",
            "--acceptance",
            "four tasks landed",
            "--constraints",
            "no new crates",
            "--doc",
            "docs/adr/0009-goal-groups-tasks.md",
        ],
    );
    assert_eq!(goal["id"], 1);
    assert_eq!(goal["doc"], "docs/adr/0009-goal-groups-tasks.md");
    assert!(goal["closed_at"].is_null());
    let first = ok(
        &db,
        &[
            "add",
            "prompt",
            "--goal",
            "1",
            "--context",
            "Stage 1 of the ADR",
        ],
    );
    assert_eq!(first["goal_id"], 1);
    assert_eq!(first["context"], "Stage 1 of the ADR");
    let second = ok(&db, &["add", "entity", "--goal", "1", "--depends-on", "1"]);
    assert_eq!(second["context"], "");
    let alone = ok(&db, &["add", "stands alone"]);
    assert!(alone["goal_id"].is_null());
    ok(&db, &["ready", "2", "--bypass-review"]);
    let detail = ok(&db, &["show", "1"]);
    assert_eq!(detail["task"]["goal_id"], 1);
    assert_eq!(detail["task"]["context"], "Stage 1 of the ADR");
    let shown = ok(&db, &["goal", "show", "1"]);
    assert_eq!(shown["goal"]["title"], "Goal groups");
    assert_eq!(shown["closed"], false);
    assert_eq!(
        shown["tasks"],
        serde_json::json!([
            {"id": 1, "title": "prompt", "status": "draft", "priority": "normal",
                "priority_source": "goal", "priority_by": "human"},
            {"id": 2, "title": "entity", "status": "ready", "priority": "normal",
                "priority_source": "goal", "priority_by": "human"}
        ])
    );
    assert_eq!(shown["events"][0]["kind"], "goal_created");
    assert!(shown["events"][0]["run_id"].is_null());
    let listed = ok(&db, &["goal", "list"]);
    assert_eq!(listed[0]["id"], 1);
    assert_eq!(listed[0]["closed"], false);
    assert!(listed[0]["verdict"].is_null());
    assert_eq!(
        listed[0]["tasks"],
        serde_json::json!({"total": 2, "draft": 1, "submitted": 0, "ready": 1, "in_progress": 0,
                           "completed": 0, "canceled": 0})
    );
    // Moving tasks and editing the goal.
    assert_eq!(ok(&db, &["set-goal", "3", "1"])["goal_id"], 1);
    assert!(ok(&db, &["set-goal", "3", "--none"])["goal_id"].is_null());
    assert!(!invoke(&db, &["set-goal", "3"]).status.success());
    assert!(
        !invoke(&db, &["set-goal", "3", "1", "--none"])
            .status
            .success()
    );
    assert!(!invoke(&db, &["goal", "edit", "1"]).status.success());
    let edited = ok(
        &db,
        &[
            "goal",
            "edit",
            "1",
            "--title",
            "Goal groups (ADR-0009)",
            "--doc",
            "",
        ],
    );
    assert_eq!(edited["title"], "Goal groups (ADR-0009)");
    assert!(edited["doc"].is_null());
    // Closing follows the task statuses and blocks further membership.
    let output = invoke(&db, &["goal", "close", "1", "--verdict", "achieved"]);
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("cannot be closed")
    );
    assert!(
        !invoke(&db, &["goal", "close", "1", "--verdict", "done"])
            .status
            .success()
    );
    ok(&db, &["cancel", "1"]);
    ok(&db, &["cancel", "2"]);
    let closed = ok(&db, &["goal", "close", "1", "--verdict", "achieved"]);
    assert_eq!(closed["verdict"], "achieved");
    assert!(closed["closed_at"].is_string());
    assert!(
        !invoke(&db, &["add", "late", "--goal", "1"])
            .status
            .success()
    );
    assert!(!invoke(&db, &["set-goal", "3", "1"]).status.success());
    assert!(!invoke(&db, &["goal", "show", "2"]).status.success());
    let listed = ok(&db, &["goal", "list"]);
    assert_eq!(listed[0]["closed"], true);
    assert_eq!(listed[0]["verdict"], "achieved");
    assert_eq!(listed[0]["tasks"]["canceled"], 2);
    assert_eq!(
        ok(&db, &["goal", "show", "1"])["events"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn graph_reports_unfinished_dependencies_releases_and_the_critical_chain() {
    let (_dir, db) = queue();
    let goal = ok(&db, &["goal", "add", "goal"])["id"].to_string();
    ok(&db, &["add", "in goal", "--goal", &goal]);
    ok(&db, &["add", "root"]);
    ok(&db, &["add", "middle", "--depends-on", "2"]);
    ok(&db, &["add", "leaf", "--depends-on", "3"]);
    ok(&db, &["add", "canceled", "--depends-on", "2"]);
    ok(
        &db,
        &["add", "after goal", "--goal", &goal, "--depends-on", "1"],
    );
    for id in ["1", "2", "3"] {
        ok(&db, &["ready", id, "--bypass-review"]);
    }
    ok(&db, &["cancel", "5"]);

    let graph = ok(&db, &["graph"]);
    let tasks = graph["tasks"].as_array().unwrap();
    let ids: Vec<i64> = tasks.iter().map(|t| t["id"].as_i64().unwrap()).collect();
    assert_eq!(ids, [1, 2, 3, 4, 6]);
    assert_eq!(
        tasks[1],
        serde_json::json!({
            "id": 2, "status": "ready", "priority": "normal", "priority_source": "default",
            "priority_by": "ai", "effective_priority": "normal",
            "execution_class": "implementation", "title": "root", "goal_id": null,
            "depends_on": [], "goal_dependencies": [], "blocks": [3], "unblocks": 2,
            "ready_after": [],
        })
    );
    assert_eq!(tasks[2]["ready_after"], serde_json::json!([2]));
    assert_eq!(tasks[0]["unblocks"], 1);
    assert_eq!(graph["candidates"], serde_json::json!([2, 1]));
    assert_eq!(graph["critical"], serde_json::json!([2, 3, 4]));

    let in_goal = ok(&db, &["graph", "--goal", &goal]);
    let ids: Vec<i64> = in_goal["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, [1, 6]);
    assert_eq!(in_goal["candidates"], serde_json::json!([1]));
    assert_eq!(in_goal["critical"], serde_json::json!([1, 6]));
}

#[test]
fn a_task_waits_for_its_goal_dependency_until_the_goal_is_achieved() {
    let (_dir, db) = queue();
    // Goal 1 holds task 1; task 2 (no goal) waits for goal 1.
    ok(&db, &["goal", "add", "upstream"]);
    ok(&db, &["add", "upstream work", "--goal", "1"]);
    let waiting = ok(&db, &["add", "downstream", "--depends-on-goal", "1"]);
    assert_eq!(waiting["id"], 2);
    for id in ["1", "2"] {
        ok(&db, &["ready", id, "--bypass-review"]);
    }
    let shown = ok(&db, &["show", "2"]);
    assert_eq!(shown["goal_dependencies"], serde_json::json!([1]));
    assert_eq!(shown["dependencies"], serde_json::json!([]));
    assert_eq!(
        ok(&db, &["show", "2", "--full"])["goal_dependencies"],
        serde_json::json!([1])
    );
    let listed = ok(&db, &["list"]);
    assert_eq!(listed["tasks"][0]["id"], 2);
    assert_eq!(
        listed["tasks"][0]["goal_dependencies"],
        serde_json::json!([1])
    );
    assert_eq!(
        listed["tasks"][1]["goal_dependencies"],
        serde_json::json!([])
    );
    let goal = ok(&db, &["goal", "show", "1"]);
    assert_eq!(
        goal["dependents"],
        serde_json::json!([{"id": 2, "title": "downstream", "status": "ready",
            "priority": "normal", "priority_source": "default", "priority_by": "ai"}])
    );
    let candidates = |db: &Path| -> Vec<i64> {
        ok(db, &["candidates"])["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_i64().unwrap())
            .collect()
    };
    assert_eq!(candidates(&db), [1]);
    let graph = ok(&db, &["graph"]);
    assert_eq!(graph["tasks"][0]["blocks"], serde_json::json!([2]));
    assert_eq!(graph["tasks"][0]["unblocks"], 1);
    assert_eq!(
        graph["tasks"][1]["ready_after"],
        serde_json::json!([{"goal": 1}])
    );
    assert_eq!(
        graph["tasks"][1]["goal_dependencies"],
        serde_json::json!([1])
    );
    assert_eq!(graph["critical"], serde_json::json!([1, 2]));

    // The goal's only task is done, yet the goal is open: still waiting.
    // Canceling stands in for completion; both are terminal for the goal.
    ok(&db, &["cancel", "1"]);
    assert!(candidates(&db).is_empty());
    assert_eq!(
        ok(&db, &["graph"])["tasks"][0]["ready_after"],
        serde_json::json!([{"goal": 1}])
    );
    ok(&db, &["goal", "close", "1", "--verdict", "achieved"]);
    assert_eq!(candidates(&db), [2]);
    assert_eq!(
        ok(&db, &["graph"])["tasks"][0]["ready_after"],
        serde_json::json!([])
    );

    // An abandoned goal never releases its dependents.
    ok(&db, &["goal", "add", "dropped"]);
    ok(&db, &["add", "stuck", "--depends-on-goal", "2"]);
    ok(&db, &["ready", "3", "--bypass-review"]);
    ok(&db, &["goal", "close", "2", "--verdict", "abandoned"]);
    assert_eq!(candidates(&db), [2]);
    let graph = ok(&db, &["graph"]);
    let stuck = graph["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == 3)
        .unwrap();
    assert_eq!(stuck["ready_after"], serde_json::json!([{"goal": 2}]));

    // dependency add / remove --goal.
    ok(&db, &["goal", "add", "third"]);
    let added = ok(&db, &["dependency", "add", "3", "--goal", "3"]);
    assert_eq!(added["goal_dependencies"], serde_json::json!([2, 3]));
    let removed = ok(&db, &["dependency", "remove", "3", "--goal", "2"]);
    assert_eq!(removed["goal_dependencies"], serde_json::json!([3]));
    assert!(refused(&db, &["dependency", "remove", "3", "--goal", "2"]).contains("does not exist"));
    assert!(!invoke(&db, &["dependency", "add", "3"]).status.success());
    assert!(
        !invoke(&db, &["dependency", "add", "3", "1", "--goal", "3"])
            .status
            .success()
    );
    let kinds: Vec<String> = ok(&db, &["show", "3", "--full"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .collect();
    assert!(kinds.contains(&"goal_dependency_added".to_owned()));
    assert!(kinds.contains(&"goal_dependency_removed".to_owned()));
}

#[test]
fn goal_dependencies_on_the_own_goal_or_through_membership_are_refused() {
    let (_dir, db) = queue();
    ok(&db, &["goal", "add", "a"]);
    ok(&db, &["goal", "add", "b"]);
    // Task 1 in goal 1; task 2 in goal 2 waits for goal 1.
    ok(&db, &["add", "in a", "--goal", "1"]);
    ok(
        &db,
        &["add", "in b", "--goal", "2", "--depends-on-goal", "1"],
    );
    let own = "a task cannot depend on its own goal 1; the goal already waits for it";
    assert_eq!(
        refused(&db, &["dependency", "add", "1", "--goal", "1"]),
        own
    );
    assert_eq!(
        refused(&db, &["add", "x", "--goal", "1", "--depends-on-goal", "1"]),
        own
    );
    // Goal 1 waiting for goal 2 through task 1 closes 2 -> 1 -> 1's task.
    assert_eq!(
        refused(&db, &["dependency", "add", "1", "--goal", "2"]),
        "dependency 1 -> goal 2 would create a cycle"
    );
    // A task of goal 1 waiting for goal 2 is refused at registration too.
    assert_eq!(
        refused(&db, &["add", "y", "--goal", "1", "--depends-on-goal", "2"]),
        "dependency 3 -> goal 2 would create a cycle"
    );
    // A task dependency that closes the loop through the goals. The
    // refused registrations took no ID, so this is task 3.
    assert_eq!(ok(&db, &["add", "free"])["id"], 3);
    assert_eq!(
        refused(&db, &["dependency", "add", "1", "2"]),
        "dependency 1 -> 2 would create a cycle"
    );
    // set-goal: task 2 into goal 1, which it waits for.
    assert_eq!(refused(&db, &["set-goal", "2", "1"]), own);
    // set-goal: task 3 waits for task 2 (which waits for goal 1); moving it
    // into goal 1 makes goal 1 wait for it.
    ok(&db, &["dependency", "add", "3", "2"]);
    assert_eq!(
        refused(&db, &["set-goal", "3", "1"]),
        "moving task 3 to goal 1 would create a cycle: the task already waits for the goal"
    );
    // Nothing refused was written; the unrelated moves still work.
    assert_eq!(
        ok(&db, &["show", "1"])["goal_dependencies"],
        serde_json::json!([])
    );
    assert_eq!(ok(&db, &["set-goal", "3", "2"])["goal_id"], 2);
    assert_eq!(ok(&db, &["set-goal", "2", "2"])["goal_id"], 2);
}

#[test]
fn draft_goal_tasks_wait_for_goal_ready() {
    let (_dir, db) = queue();
    let goal = ok(&db, &["goal", "add", "proposal", "--draft"]);
    assert_eq!(goal["status"], "draft");
    let id = goal["id"].to_string();
    ok(&db, &["add", "proposed", "--goal", &id]);
    ok(&db, &["ready", "1", "--bypass-review"]);
    assert_eq!(
        ok(&db, &["candidates"])["candidates"],
        serde_json::json!([])
    );
    assert_eq!(ok(&db, &["goal", "list"])[0]["status"], "draft");
    assert_eq!(ok(&db, &["goal", "show", &id])["goal"]["status"], "draft");
    let graph = ok(&db, &["graph"]);
    assert_eq!(graph["tasks"][0]["goal_status"], "draft");
    assert_eq!(graph["candidates"], serde_json::json!([]));

    let opened = ok(&db, &["goal", "ready", &id]);
    assert_eq!(opened["status"], "open");
    let candidates = ok(&db, &["candidates"]);
    assert_eq!(candidates["candidates"][0]["id"], 1);
    assert_eq!(ok(&db, &["graph"])["tasks"][0]["goal_status"], "open");
    let again = invoke(&db, &["goal", "ready", &id]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("is not a draft"));
    // A goal registered without --draft is open.
    assert_eq!(ok(&db, &["goal", "add", "plain"])["status"], "open");
}

#[test]
fn ready_tasks_of_a_draft_goal_do_not_raise_idle_slots() {
    let (_dir, db) = queue();
    SqliteQueue::open(&db)
        .unwrap()
        .register_supervisor(&LeaseToken::new("live"), std::process::id(), 2, "0.0.1")
        .unwrap();
    let idle = |db: &Path| {
        ok(db, &["stats"])["alerts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|alert| alert["kind"] == "idle_slots")
    };
    ok(&db, &["goal", "add", "proposal", "--draft"]);
    ok(&db, &["add", "proposed", "--goal", "1"]);
    ok(&db, &["ready", "1", "--bypass-review"]);
    assert!(!idle(&db));
    // A ready task blocked by a predecessor still raises it.
    ok(&db, &["add", "first"]);
    ok(&db, &["add", "blocked", "--depends-on", "2"]);
    ok(&db, &["ready", "3", "--bypass-review"]);
    assert!(idle(&db));
}

#[test]
fn a_dependency_on_an_abandoned_goal_is_refused() {
    let (_dir, db) = queue();
    ok(&db, &["goal", "add", "dropped"]);
    ok(&db, &["goal", "add", "done"]);
    ok(&db, &["goal", "close", "1", "--verdict", "abandoned"]);
    ok(&db, &["goal", "close", "2", "--verdict", "achieved"]);
    let abandoned = "goal 1 is closed as abandoned and never releases a task that depends on it";
    assert_eq!(
        refused(&db, &["add", "stuck", "--depends-on-goal", "1"]),
        abandoned
    );
    ok(&db, &["add", "free"]);
    assert_eq!(
        refused(&db, &["dependency", "add", "1", "--goal", "1"]),
        abandoned
    );
    // An achieved goal still takes dependents: it has released them.
    let added = ok(&db, &["dependency", "add", "1", "--goal", "2"]);
    assert_eq!(added["goal_dependencies"], serde_json::json!([2]));
}

#[test]
fn a_canceled_member_does_not_make_its_goal_wait() {
    let (_dir, db) = queue();
    ok(&db, &["goal", "add", "g"]);
    // Task 2 in goal 1 depends on task 1, then is canceled: goal 1 no
    // longer waits for task 1 through it.
    ok(&db, &["add", "x"]);
    ok(&db, &["add", "c", "--goal", "1", "--depends-on", "1"]);
    let cycle = "dependency 1 -> goal 1 would create a cycle";
    assert_eq!(
        refused(&db, &["dependency", "add", "1", "--goal", "1"]),
        cycle
    );
    ok(&db, &["cancel", "2"]);
    let added = ok(&db, &["dependency", "add", "1", "--goal", "1"]);
    assert_eq!(added["goal_dependencies"], serde_json::json!([1]));
}

/// ADR-t1639-1 decision 1: a goal takes one of the five priority names
/// (normal unless given), changes it while it is a draft or open, refuses
/// it once closed, and `goal_updated` keeps the old and the new.
#[test]
fn a_goal_takes_a_priority_by_name_while_it_is_not_closed() {
    let (_dir, db) = queue();
    assert_eq!(
        ok(&db, &["goal", "add", "urgent", "--priority", "urgent"])["priority"],
        "urgent"
    );
    assert_eq!(ok(&db, &["goal", "add", "plain"])["priority"], "normal");
    let draft = ok(&db, &["goal", "add", "drafted", "--draft"])["id"].to_string();
    for bad in ["2", "Urgent", "top"] {
        for args in [
            vec!["goal", "add", "bad", "--priority", bad],
            vec!["goal", "edit", "1", "--priority", bad],
        ] {
            let output = invoke(&db, &args);
            assert!(!output.status.success(), "{args:?}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("invalid value"),
                "{args:?}"
            );
        }
    }
    assert_eq!(
        ok(&db, &["goal", "show", "1"])["goal"]["priority"],
        "urgent"
    );

    let edited = ok(&db, &["goal", "edit", "1", "--priority", "low"]);
    assert_eq!(edited["priority"], "low");
    assert_eq!(edited["title"], "urgent");
    let shown = ok(&db, &["goal", "show", "1", "--full"]);
    assert_eq!(shown["goal"]["priority"], "low");
    let updated = shown["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "goal_updated")
        .unwrap();
    assert_eq!(
        (
            &updated["payload"]["old"]["priority"],
            &updated["payload"]["new"]["priority"]
        ),
        (&serde_json::json!("urgent"), &serde_json::json!("low"))
    );
    assert_eq!(
        ok(&db, &["goal", "edit", &draft, "--priority", "high"])["priority"],
        "high"
    );

    ok(&db, &["goal", "close", "2", "--verdict", "abandoned"]);
    assert_eq!(
        refused(&db, &["goal", "edit", "2", "--priority", "high"]),
        "goal 2 is closed as abandoned; create a new goal for further work"
    );
    assert_eq!(
        ok(&db, &["goal", "show", "2"])["goal"]["priority"],
        "normal"
    );
}

/// The queue at `db` bound to `repo`, so the commands read the `dagq.toml`
/// of `repo` (ADR-t980-1, ADR-t1639-1 decision 6).
fn bind(db: &Path, repo: &Path) {
    let repository = dagq::infrastructure::adapters::GitRepository::inspect(repo).unwrap();
    SqliteQueue::open(db)
        .unwrap()
        .bind_repository(
            &dagq::infrastructure::adapters::path_text(&repository.common_dir).unwrap(),
        )
        .unwrap();
}

/// ADR-t1639-1 decision 6 without a set of tags: `goal add --tag` and
/// `goal edit --tag` take any tag of the form, `--tag` replaces the whole
/// list, `--no-tags` removes it, `goal_updated` records both, and `goal
/// show` and `search` print them.
#[test]
fn a_goal_takes_tags_that_an_edit_replaces_or_removes() {
    let (_dir, db) = queue();
    let added = ok(
        &db,
        &[
            "goal",
            "add",
            "tagged",
            "--tag",
            "codex",
            "--tag",
            "front-end_2",
        ],
    );
    assert_eq!(added["tags"], serde_json::json!(["codex", "front-end_2"]));
    assert_eq!(
        ok(&db, &["goal", "add", "plain"])["tags"],
        serde_json::json!([])
    );
    let long = "t".repeat(65);
    for args in [
        vec!["goal", "add", "bad", "--tag", "Codex"],
        vec!["goal", "add", "bad", "--tag", "two words"],
        vec!["goal", "add", "bad", "--tag", &long],
        vec!["goal", "edit", "1", "--tag", "Codex"],
    ] {
        let error = refused(&db, &args);
        assert!(error.contains("must be a slug"), "{args:?}: {error}");
    }
    assert_eq!(
        refused(&db, &["goal", "add", "bad", "--tag", "a", "--tag", "a"]),
        "goal tag \"a\" is given twice"
    );
    assert!(
        !invoke(&db, &["goal", "edit", "1", "--tag", "a", "--no-tags"])
            .status
            .success()
    );
    assert_eq!(
        ok(&db, &["goal", "show", "1"])["goal"]["tags"],
        serde_json::json!(["codex", "front-end_2"])
    );

    let edited = ok(&db, &["goal", "edit", "1", "--tag", "cmux"]);
    assert_eq!(edited["tags"], serde_json::json!(["cmux"]));
    assert_eq!(edited["title"], "tagged");
    let cleared = ok(&db, &["goal", "edit", "1", "--no-tags"]);
    assert_eq!(cleared["tags"], serde_json::json!([]));
    let updated: Vec<Value> = ok(&db, &["goal", "show", "1", "--full"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "goal_updated")
        .map(|event| {
            serde_json::json!([
                event["payload"]["old"]["tags"],
                event["payload"]["new"]["tags"]
            ])
        })
        .collect();
    assert_eq!(
        updated,
        [
            serde_json::json!([["codex", "front-end_2"], ["cmux"]]),
            serde_json::json!([["cmux"], []])
        ]
    );

    // A closed goal's tags change too, and a search hit of a goal shows them.
    ok(&db, &["goal", "edit", "2", "--tag", "throughput"]);
    ok(&db, &["goal", "close", "2", "--verdict", "abandoned"]);
    ok(
        &db,
        &["goal", "edit", "2", "--tag", "throughput", "--tag", "codex"],
    );
    let found = ok(&db, &["search", "plain"]);
    assert_eq!(found["hits"][0]["kind"], "goal", "{found}");
    assert_eq!(
        found["hits"][0]["tags"],
        serde_json::json!(["throughput", "codex"])
    );
    let untagged = ok(&db, &["search", "tagged"]);
    assert_eq!(untagged["hits"][0]["tags"], serde_json::json!([]));
    ok(&db, &["add", "tagged task"]);
    let task_hit = ok(&db, &["search", "task", "--kind", "task"]);
    assert!(task_hit["hits"][0].get("tags").is_none(), "{task_hit}");
}

/// ADR-t1639-1 decision 6 with `[goals] tags` in the bound checkout's
/// dagq.toml: `goal add` and `goal edit` refuse a tag outside it, and
/// `lint` and `submit` refuse a draft goal without a tag, moving nothing,
/// until it has one.
#[test]
fn the_set_of_goal_tags_of_dagq_toml_holds_goal_add_edit_lint_and_submit() {
    let (_fixture, repo, db) = crate::runtime_support::fixture();
    bind(&db, &repo);
    std::fs::write(
        repo.join("dagq.toml"),
        "[goals]\ntags = [\"codex\", \"throughput\"]\n",
    )
    .unwrap();
    let outside = refused(&db, &["goal", "add", "outside", "--tag", "cmux"]);
    assert!(outside.contains("not one of [goals] tags"), "{outside}");
    assert!(outside.contains("codex, throughput"), "{outside}");
    let draft = ok(&db, &["goal", "add", "untagged", "--draft"])["id"].to_string();
    let refused_edit = refused(&db, &["goal", "edit", &draft, "--tag", "cmux"]);
    assert!(
        refused_edit.contains("not one of [goals] tags"),
        "{refused_edit}"
    );
    let open = ok(&db, &["goal", "add", "open goal"])["id"].to_string();
    let sound = ["--acceptance", "works", "--verify", "x"];
    let member = ok(
        &db,
        &[&["add", "member", "--goal", &draft][..], &sound].concat(),
    )["id"]
        .to_string();
    let other = ok(
        &db,
        &[&["add", "other", "--goal", &open][..], &sound].concat(),
    )["id"]
        .to_string();

    // Only the task of the draft goal without a tag is linted.
    let linted = ok(&db, &["lint", &member, &other]);
    let violations = linted["violations"].as_array().unwrap();
    assert_eq!(violations.len(), 1, "{linted}");
    assert_eq!(violations[0]["code"], "missing_goal_tag");
    assert_eq!(violations[0]["task_id"].to_string(), member);
    let submitted = submit_from(&db, None, None, &["--goal", &draft]);
    assert!(!submitted.status.success());
    let stderr = String::from_utf8_lossy(&submitted.stderr);
    assert!(
        stderr.contains(&format!("draft goal {draft} has no tag")),
        "{stderr}"
    );
    // By task ID too: the task's draft goal holds the whole submit.
    let by_task = submit_from(&db, None, None, &[&member, &other]);
    assert!(!by_task.status.success());
    let stderr = String::from_utf8_lossy(&by_task.stderr);
    assert!(
        stderr.contains(&format!("draft goal {draft} has no tag")),
        "{stderr}"
    );
    for id in [&member, &other] {
        assert_eq!(ok(&db, &["show", id])["task"]["status"], "draft");
    }
    assert_eq!(
        ok(&db, &["proposal", "list", "--all"])["proposals"],
        serde_json::json!([])
    );
    // A task of an open goal is not held by the tags.
    assert!(submit_from(&db, None, None, &[&other]).status.success());
    assert_eq!(ok(&db, &["show", &other])["task"]["status"], "submitted");

    ok(&db, &["goal", "edit", &draft, "--tag", "codex"]);
    assert_eq!(
        ok(&db, &["lint", &member, &other])["violations"],
        serde_json::json!([])
    );
    // A tag the set no longer names is linted as outside it.
    std::fs::write(repo.join("dagq.toml"), "[goals]\ntags = [\"throughput\"]\n").unwrap();
    assert_eq!(
        ok(&db, &["lint", &member])["violations"][0]["code"],
        "goal_tag_outside_set"
    );
    std::fs::write(
        repo.join("dagq.toml"),
        "[goals]\ntags = [\"codex\", \"throughput\"]\n",
    )
    .unwrap();
    assert!(
        submit_from(&db, None, None, &["--goal", &draft])
            .status
            .success()
    );
    assert_eq!(ok(&db, &["show", &member])["task"]["status"], "submitted");

    // Without the key, any tag of the form again, and a draft goal may
    // have none.
    std::fs::write(repo.join("dagq.toml"), "[goals]\n").unwrap();
    assert_eq!(
        ok(&db, &["goal", "add", "free", "--tag", "cmux"])["tags"],
        serde_json::json!(["cmux"])
    );
    let bare = ok(&db, &["goal", "add", "bare", "--draft"])["id"].to_string();
    let bare_task = ok(
        &db,
        &[&["add", "bare task", "--goal", &bare][..], &sound].concat(),
    )["id"]
        .to_string();
    assert_eq!(
        ok(&db, &["lint", &bare_task])["violations"],
        serde_json::json!([])
    );
    assert!(
        submit_from(&db, None, None, &["--goal", &bare])
            .status
            .success()
    );
}

/// ADR-t1639-1 decision 7: `goal list` puts the open and draft goals
/// before the closed ones, each part by priority (highest first) then ID,
/// prints each goal's priority and tags, keeps a draft goal without tasks,
/// and `--tag` keeps the goals with any of the tags given.
#[test]
fn goal_list_orders_by_priority_and_narrows_by_tag() {
    let (_dir, db) = queue();
    for args in [
        &[
            "goal",
            "add",
            "low codex",
            "--priority",
            "low",
            "--tag",
            "codex",
        ][..],
        &[
            "goal",
            "add",
            "closed interrupt",
            "--priority",
            "interrupt",
            "--tag",
            "codex",
        ][..],
        &["goal", "add", "high plain", "--priority", "high"][..],
        &[
            "goal",
            "add",
            "normal two",
            "--tag",
            "cmux",
            "--tag",
            "throughput",
        ][..],
        &[
            "goal",
            "add",
            "empty draft",
            "--draft",
            "--priority",
            "high",
            "--tag",
            "throughput",
        ][..],
    ] {
        ok(&db, args);
    }
    ok(&db, &["goal", "close", "2", "--verdict", "abandoned"]);
    ok(&db, &["add", "a task", "--goal", "1"]);
    let ids = |listed: &Value| -> Vec<i64> {
        listed
            .as_array()
            .unwrap()
            .iter()
            .map(|goal| goal["id"].as_i64().unwrap())
            .collect()
    };
    let listed = ok(&db, &["goal", "list"]);
    assert_eq!(ids(&listed), [3, 5, 4, 1, 2]);
    let draft = &listed[1];
    assert_eq!(
        (&draft["status"], &draft["priority"], &draft["tags"]),
        (
            &serde_json::json!("draft"),
            &serde_json::json!("high"),
            &serde_json::json!(["throughput"])
        )
    );
    assert_eq!(draft["tasks"]["total"], 0);
    assert_eq!(listed[0]["tags"], serde_json::json!([]));
    assert_eq!(listed[4]["priority"], "interrupt");
    assert_eq!(listed[4]["closed"], true);

    assert_eq!(ids(&ok(&db, &["goal", "list", "--tag", "codex"])), [1, 2]);
    assert_eq!(
        ids(&ok(
            &db,
            &["goal", "list", "--tag", "throughput", "--tag", "codex"]
        )),
        [5, 4, 1, 2]
    );
    assert_eq!(
        ok(&db, &["goal", "list", "--tag", "enterprise"]),
        serde_json::json!([])
    );
    let error = refused(&db, &["goal", "list", "--tag", "Codex"]);
    assert!(error.contains("must be a slug"), "{error}");
}
