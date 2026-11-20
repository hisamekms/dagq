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
use dagq::application::TaskReads;
use dagq::{
    domain::{Priority, ProposalStatus, TaskId},
    infrastructure::sqlite::SqliteQueue,
};
use serde_json::{Value, json};
use std::process::Command;
use std::{
    fs, thread,
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
    crate::common::ledger::build_before(db, args);
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

    {
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
}

/// The supervisor opens Codex at a revise and delivers later revise and
/// answer requests to its thread. The stub runs the real CLI at the socket.
#[test]
fn a_codex_planner_resumes_revise_and_answer_and_writes_as_its_principal() {
    use crate::goal_review_codex::{codex_home, roles};
    use crate::planner_headless::{diagnose_planner, queue_events};
    use dagq::domain::EventKind;

    let fx = fixture();
    roles(
        &fx,
        "[language]\ntag = \"ja\"\n[roles.runtime_planner]\nprovider = \"codex\"\nmodel = \"gpt-requested\"\neffort = \"medium\"\n",
    );
    let dir = fx.db.parent().unwrap();
    let codex = dir.join("codex-planner");
    crate::common::template::script(
        &codex,
        concat!(
            r#"#!/bin/sh
set -eu
"#,
            crate::await_file_fn!(),
            r#"
[ "$1" != --version ] || { echo 'codex-cli 0.162.1'; exit 0; }
DIR="${0%/*}"
exec 3>> "$DIR/codex-calls"
printf '%s\n' "$*" >&3
exec 2>> "$DIR/codex-operations"
[ "$1" = exec ]
[ "$DAGQ_ROLE" = planner ]
[ "$DAGQ_ACTOR_ID" = planner:1 ]
[ -z "${DAGQ_QUEUE+x}" ]
[ -S "$DAGQ_SERVICE_SOCKET" ]
[ -r "$DAGQ_SERVICE_CREDENTIAL_FILE" ]
DAGQ="$DIR/planners/1/runner"
PROMPT=$(cat)
[ -r "$DIR/planners/1/references/skills/dagq/reference/register.md" ]
THREAD=planner-thread
printf '{"type":"thread.started","thread_id":"%s"}\n{"type":"turn.started"}\n' "$THREAD"
case "$PROMPT" in
  *'answer to ask '*'use approach A'*)
    [ "$2" = resume ]
    "$DAGQ" submit --proposal 1 ;;
  *'name the test'*)
    [ "$2" = resume ]
    "$DAGQ" ask --task 2 --kind planner_question --because scope --question 'Which approach?'
    await_file "$DIR/codex-answer-ready" ;;
  *'split it'*)
    [ "$2" = --json ]
    printf '%s' "$PROMPT" > "$DIR/codex-initial-prompt"
    "$DAGQ" add 'feat: follow-on' --description 'separate draft' --acceptance 'works' --verify true --depends-on 1
    "$DAGQ" edit 2 --context 'review change'
    "$DAGQ" note --task 2 --text 'planned with the service'
    "$DAGQ" submit --proposal 1
    await_file "$DIR/codex-revise-ready" ;;
  *) echo "$PROMPT" >&2; exit 1 ;;
esac >&2
SESSIONS="$CODEX_HOME/sessions/2026/09/29"
mkdir -p "$SESSIONS"
printf '{"timestamp":"%s","type":"turn_context","payload":{"model":"gpt-observed"}}\n' "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" >> "$SESSIONS/rollout-2026-09-29T00-00-00-$THREAD.jsonl"
printf '{"type":"item.completed","item":{"type":"agent_message","text":"done"}}\n'
printf '{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":2}}\n'
"#,
        ),
    );

    let mut queue = SqliteQueue::open(&fx.db).unwrap();
    let task = add(
        &mut queue,
        "feat: change",
        &[TaskId::new(1)],
        Priority::Normal,
    );
    let proposal = submit(&mut queue, &[task], None);
    let reviewer = StubReviewer::new(&[
        json!({"verdict": "revise", "reasons": ["split it"], "summary": "not yet"}),
        json!({"verdict": "revise", "reasons": ["name the test"], "summary": "not yet"}),
        json!({"verdict": "pass", "reasons": [], "summary": "ok"}),
    ]);
    let backend = PlanWorkspace::running();
    let settings = dagq::compose::SuperviseOptions {
        codex,
        codex_home: Some(codex_home(&fx)),
        ..options(1, Duration::from_secs(3600))
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut revise_ready = false;
    let mut answered = false;
    loop {
        // Keep the initial turn at work until the second review sent the
        // proposal back. Keep the next turn at work until the answer is
        // recorded. An idle planner may otherwise close at either point.
        if !revise_ready
            && queue
                .latest_events_of(EventKind::PlanReviewFinished.as_str(), 2)
                .unwrap()
                .len()
                == 2
        {
            fs::write(dir.join("codex-revise-ready"), b"").unwrap();
            revise_ready = true;
        }
        if !answered
            && let Some(ask) = queue
                .asks(Default::default())
                .unwrap()
                .into_iter()
                .find(|ask| ask.task_id == Some(task))
        {
            queue.answer(ask.id, "use approach A").unwrap();
            fs::write(dir.join("codex-answer-ready"), b"").unwrap();
            answered = true;
        }
        let outcome = supervise_with(&fx, &backend, &reviewer, &settings);
        assert_eq!(outcome["errors"], json!([]), "{outcome}");
        if !queue_events(&fx.db, "planner_closed").is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{}\n{}",
            diagnose_planner(&fx.db),
            fs::read_to_string(dir.join("codex-operations")).unwrap_or_default()
        );
        thread::sleep(Duration::from_millis(20));
    }

    let planners = queue.planners(true).unwrap();
    assert_eq!(planners.len(), 1, "{}", diagnose_planner(&fx.db));
    let planner = &planners[0];
    let planner_dir = dagq::infrastructure::location::planners_dir(&fx.db).join("1");
    assert!(planner_dir.join("turns/exit").is_file());
    assert_eq!(planner.exit_code, Some(0), "{planner:?}");
    assert_eq!(
        queue.show_proposal(proposal).unwrap().status(),
        ProposalStatus::Accepted
    );
    assert!(answered);

    let prompt = fs::read_to_string(dir.join("codex-initial-prompt")).unwrap();
    assert!(prompt.contains("Basic policy"));
    assert!(prompt.contains("BCP 47 tag `ja`"));
    let calls = fs::read_to_string(dir.join("codex-calls")).unwrap();
    let calls: Vec<_> = calls.lines().collect();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(calls[0].starts_with("exec --json "), "{calls:?}");
    for call in &calls[1..] {
        assert!(
            call.starts_with("exec resume --json ") && call.ends_with("-- planner-thread -"),
            "{call}"
        );
    }
    assert!(
        calls
            .iter()
            .all(|call| call.contains(r#"model_reasoning_effort="high""#)
                && call.contains("-m gpt-requested"))
    );
    let started = queue_events(&fx.db, "turn_started");
    assert_eq!(started[1]["what"], "revise");
    assert!(
        started[2]["what"]
            .as_str()
            .unwrap()
            .starts_with("answer of ask ")
    );

    let events = queue.all_events().unwrap();
    for kind in [
        EventKind::TaskCreated,
        EventKind::TaskEdited,
        EventKind::Observation,
        EventKind::ProposalSubmitted,
        EventKind::AskOpened,
    ] {
        assert!(
            events.iter().any(|event| event.kind == kind
                && event.actor.as_ref().is_some_and(
                    |actor| actor.id == "planner:1" && actor.role.as_str() == "planner"
                )),
            "no {} from planner:1",
            kind.as_str()
        );
    }
    let opened = planner_spans(&fx.db, "session_opened");
    assert_eq!(opened[0]["provider"], "codex");
    assert_eq!(opened[0]["launch"]["model"], "gpt-requested");
    let closed = planner_spans(&fx.db, "session_closed");
    assert_eq!(closed[0]["session_id"], "planner-thread");
    assert_eq!(closed[0]["provider"], "codex");
    assert_eq!(closed[0]["model"], "gpt-observed");
    assert_eq!(closed[0]["requested_model"], "gpt-requested");

    let config = dir.join("config");
    fs::create_dir_all(&config).unwrap();
    let stats = read(&fx.db, &config, &["stats", "--full"]);
    let by_provider = &stats["sessions"]["by_provider"]["runtime_planner"];
    assert_eq!(by_provider["codex"]["count"], 1, "{by_provider}");
    assert_eq!(
        by_provider["codex"]["models"]["gpt-observed high"], 1,
        "{by_provider}"
    );
    assert!(by_provider.get("claude").is_none(), "{by_provider}");
    let kpi = read(&fx.db, &config, &["kpi", "--last", "1", "--by", "provider"]);
    let active = &kpi["periods"][0]["kpis"]["session_active.runtime_planner"];
    assert_eq!(active["provider=codex"]["n"], 1, "{active}");
}
