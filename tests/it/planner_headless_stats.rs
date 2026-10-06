//! The session of a headless planner of the runtime's and its route in
//! `stats` and `kpi` (ADR-t1394-2 decision 4): the plugin's hook does not
//! run for it, so its span opens on its first `turn_started`, takes its
//! active time and tokens from its turns, closes with its
//! `planner_closed`, and is read per route next to the interactive
//! planners' spans the hook records.

use crate::common::{Bounded, WithoutActor};
use crate::plan_review::{
    PlanWorkspace, StubReviewer, add, fixture, git, options, submit, supervise_with,
};
use crate::runtime_support::{headless_claude, set_turns};
use dagq::application::TaskStore;
use dagq::{
    domain::{Priority, ProposalStatus, TaskId},
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::{Value, json};
use std::{
    fs,
    process::Command,
    thread,
    time::{Duration, Instant},
};

/// A fixture whose runtime's planners run headless in the background, the
/// stub agent of [`headless_claude`] running `turns`.
fn headless_fixture(turns: &str) -> crate::plan_review::Fixture {
    let mut fx = fixture();
    fs::write(
        fx.repo.join("dagq.toml"),
        "[roles.runtime_planner]\nroute = \"headless\"\n\n[headless]\nwrapper = \"background\"\n",
    )
    .unwrap();
    git(&fx.repo, &["add", "dagq.toml"]);
    git(
        &fx.repo,
        &["commit", "-qm", "route of the runtime's planners"],
    );
    let stub = headless_claude(fx.db.parent().unwrap(), &fx.db);
    set_turns(fx.db.parent().unwrap(), turns);
    let claude = fx.db.parent().unwrap().join("claude-planner");
    crate::common::template::script(
        &claude,
        format!(
            "#!/bin/sh\ncase \"$*\" in *--version*) echo '2.1.0 (Claude Code)'; exit 0 ;; esac\nexec {} \"$@\"\n",
            stub.display()
        ),
    );
    fx.claude = claude;
    fx
}

/// The queue's `session_opened` / `session_closed` of kind
/// `runtime_planner`, oldest first.
fn planner_spans(db: &std::path::Path, kind: &str) -> Vec<Value> {
    let mut events: Vec<Value> = SqliteQueue::open(db)
        .unwrap()
        .latest_events_of(kind, 50)
        .unwrap()
        .into_iter()
        .map(|event| {
            let mut payload = event.payload;
            payload["task_id"] = json!(event.task_id);
            payload
        })
        .filter(|payload| payload["kind"] == "runtime_planner")
        .collect();
    events.reverse();
    events
}

/// `dagq <args>` in UTC, without the host's settings.
fn read(db: &std::path::Path, config: &std::path::Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
        .without_actor_env()
        .env("TZ", "UTC")
        .env("XDG_CONFIG_HOME", config)
        .arg("--db")
        .arg(db)
        .args(args)
        .bounded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Acceptance: a headless planner opened for a revise has a
/// `runtime_planner` span from its first turn to its close, on the
/// proposal's first task, with the route, its turns' active time, tokens
/// and model; `stats` counts it in `sessions.by_kind.runtime_planner`, in
/// `sessions.by_route.runtime_planner.headless` and in
/// `planner_routes.headless` (the planner opened for the revise, the time
/// to the next plan review, its turn and its tokens); `kpi` has the
/// `session_*` of `runtime_planner` with a `route=headless` stratum and
/// the planners in `health.planner_routes`.
#[test]
fn a_headless_planners_session_and_route_are_read_by_stats_and_kpi() {
    let fx = headless_fixture(
        "\"$DAGQ\" --db \"$DB\" submit --proposal 1 >> \"$RUN_DIR/submit.log\" 2>&1; say \"turn $TURN submitted\"",
    );
    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(&mut queue, "change", &[TaskId::new(1)], Priority::Normal);
    let proposal = submit(&mut queue, &[task], None);
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "revise", "reasons": ["split it"], "summary": "not yet"}),
        json!({"verdict": "pass", "reasons": [], "summary": "ok"}),
    ]);
    let backend = PlanWorkspace::running();
    let settings = options(1, Duration::from_secs(3600));
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        supervise_with(&fx, &backend, &reviewer, &settings);
        if !planner_spans(&fx.db, "session_closed").is_empty()
            && queue.show_proposal(proposal).unwrap().status() == ProposalStatus::Accepted
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{:?}\n{:?}",
            queue.planners(true).unwrap(),
            planner_spans(&fx.db, "session_opened")
        );
        thread::sleep(Duration::from_millis(50));
    }

    // One span, opened by its turn on the proposal's first task.
    let opened = planner_spans(&fx.db, "session_opened");
    assert_eq!(opened.len(), 1, "{opened:?}");
    let span = &opened[0];
    assert_eq!(span["route"], "headless", "{span}");
    assert_eq!(span["planner_id"], 1);
    assert_eq!(span["provider"], "claude");
    assert_eq!(span["proposal_id"], proposal.as_i64());
    assert_eq!(span["task_id"], task.as_i64());
    assert!(span["launch"]["effort"].is_string(), "{span}");
    let closed = planner_spans(&fx.db, "session_closed");
    assert_eq!(closed.len(), 1, "{closed:?}");
    let close = &closed[0];
    assert_eq!(close["reason"], "exited", "{close}");
    assert_eq!(close["active"], "recorded", "{close}");
    assert_eq!(close["tokens"]["input"], 7, "{close}");
    assert_eq!(close["tokens"]["output"], 3, "{close}");
    assert!(close["model"].is_string(), "{close}");
    assert_eq!(close["effort"], span["launch"]["effort"]);

    let config = fx.db.parent().unwrap().join("config");
    fs::create_dir_all(&config).unwrap();
    let stats = read(&fx.db, &config, &["stats", "--full"]);
    let sessions = &stats["sessions"];
    let all = &sessions["by_kind"]["runtime_planner"];
    assert_eq!(all["count"], 1, "{sessions}");
    assert_eq!(all["open_now"], 0, "{sessions}");
    assert_eq!(all["active"]["count"], 1, "{sessions}");
    assert_eq!(all["tokens"]["input"], 7, "{sessions}");
    assert_eq!(all["models"].as_object().unwrap().len(), 1, "{sessions}");
    let headless = &sessions["by_route"]["runtime_planner"]["headless"];
    assert_eq!(headless["count"], 1, "{sessions}");
    assert_eq!(headless["tokens"]["input"], 7, "{sessions}");
    assert!(
        sessions["by_route"]["runtime_planner"]
            .get("interactive")
            .is_none()
    );
    let planners = &stats["planner_routes"]["headless"];
    assert_eq!(planners["opened"]["revise"], 1, "{planners}");
    assert_eq!(planners["revise_to_review"]["count"], 1, "{planners}");
    assert_eq!(planners["turns"], 1, "{planners}");
    assert_eq!(planners["turn_outcomes"]["succeeded"], 1, "{planners}");
    assert_eq!(planners["tokens"]["input"], 7, "{planners}");
    assert_eq!(planners["planner_questions"], 0, "{planners}");

    let kpi = read(&fx.db, &config, &["kpi", "--last", "1"]);
    let period = &kpi["periods"][0];
    let open = &period["kpis"]["session_open.runtime_planner"];
    assert_eq!(open["all"]["n"], 1, "{open}");
    assert_eq!(open["route=headless"]["n"], 1, "{open}");
    assert_eq!(
        period["kpis"]["session_active.runtime_planner"]["route=headless"]["n"],
        1
    );
    assert!(
        period["kpis"]["session_active_ratio.runtime_planner"]
            .get("route=headless")
            .is_some()
    );
    assert_eq!(
        period["health"]["planner_routes"]["headless"]["opened"]["revise"], 1,
        "{}",
        period["health"]
    );
}
