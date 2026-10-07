//! The observer on Codex (task 1223, ADR-t1222-1): `[roles.observer]`'s
//! `provider = "codex"` starts the job as `codex exec --json` in the
//! read-only sandbox (the queue service's job profile), played by a stub
//! `codex` that writes the JSONL and the rollout codex-cli 0.155.1 writes.
//! Its `dagq` runs in client mode and writes the findings, their updates and
//! resolves and the finding's `blocked` ask through the queue service, as
//! the observer, as Claude's does. Under `--no-claude` it still starts, and
//! a Codex that cannot be used never moves it to Claude.
use crate::runtime_observer::{observe, observe_options, queue_events};
use crate::runtime_support;
use dagq::application::observer::ObserveMode;
use dagq::domain::{
    FindingTarget, NewFinding, Provider,
    actor_model::{ActorLaunch, ModelRole, RoleModels},
};
use dagq::infrastructure::codex::Codex;

use runtime_support::*;

/// The model the stub `codex` writes to its rollouts.
const CODEX_MODEL: &str = "gpt-6-astra";

/// A stub `codex` next to the queue: `--version` answers; `exec` appends
/// its arguments (each ended by `|`, the prompt from stdin last) to
/// `codex-args.txt`, its role and actor to `codex-actors.txt` and its
/// environment to `codex-env-<call>.txt`, prints `thread.started` (thread
/// `codex-thread-<call>`) and `turn.started`, then, as `codex-mode` says,
/// fails at the usage limit (`limit`), or runs the observer's `dagq`
/// commands of its call (the first, when `codex-writes` is there: record,
/// update and record findings, resolve finding 1, raise a blocked ask on
/// finding 3, try a note; else nothing), writes the thread's rollout with its model (not
/// for `norollout`), and prints its last message and `turn.completed`.
fn stub_codex(db: &Path, mode: &str) -> PathBuf {
    let dir = db.parent().unwrap();
    let stub = dir.join("codex");
    fs::write(dir.join("codex-mode"), mode).unwrap();
    let script = format!(
        r#"#!/bin/sh
DIR="${{0%/*}}"
[ "$1" = --version ] && {{ echo "codex-cli 0.155.1"; exit 0; }}
for arg in "$@"; do printf '%s|' "$arg" | tr '\n' ' '; done >> "$DIR/codex-args.txt"
# The prompt comes on stdin, never as an argument (task 1560).
printf '<stdin>|' >> "$DIR/codex-args.txt"
tr '\n' ' ' >> "$DIR/codex-args.txt"
printf '\n' >> "$DIR/codex-args.txt"
printf '%s %s\n' "$DAGQ_ROLE" "$DAGQ_ACTOR_ID" >> "$DIR/codex-actors.txt"
CALL="$(wc -l < "$DIR/codex-actors.txt" | tr -d ' ')"
THREAD="codex-thread-$CALL"
env > "$DIR/codex-env-$CALL.txt"
printf '{{"type":"thread.started","thread_id":"%s"}}\n{{"type":"turn.started"}}\n' "$THREAD"
MODE="$(cat "$DIR/codex-mode")"
if [ "$MODE" = limit ]; then
  printf '{{"type":"error","message":"You have hit your usage limit. Try again later."}}\n{{"type":"turn.failed","error":{{"message":"You have hit your usage limit."}}}}\n'
  exit 1
fi
q() {{ {dagq} "$@" > /dev/null; }}
if [ "$CALL" = 1 ] && [ -e "$DIR/codex-writes" ]; then
  q finding record --kind stall --task 1 --summary 'task 1 waits for a slot' --evidence 1 || exit 2
  q finding record --kind stall --task 1 --summary 'task 1 waits for a slot' --evidence 1 --evidence 2 || exit 3
  q finding record --kind capacity --queue --subject idle_slots --summary 'slots idle' || exit 4
  q finding resolve 1 --reason 'slots are busy again' || exit 5
  q ask --kind blocked --because recovery_failed --finding 3 --question 'slots idle while task 1 is ready' --option 'restart the supervisor' --recommend 'restart the supervisor' --confidence high --cmux /usr/bin/true || exit 6
  if {dagq} note --task 1 --text 'seen' 2> "$DIR/codex-note.err"; then exit 7; fi
fi
if [ "$MODE" != norollout ]; then
  SESSIONS="$DIR/codex-home/sessions/2026/09/29"
  mkdir -p "$SESSIONS"
  printf '{{"timestamp":"%s","type":"turn_context","payload":{{"model":"{CODEX_MODEL}","effort":"high"}}}}\n' "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" >> "$SESSIONS/rollout-2026-09-29T00-00-00-$THREAD.jsonl"
fi
printf '{{"type":"item.completed","item":{{"id":"m1","type":"agent_message","text":"recorded 2 findings, updated 1, closed 1, 1 ask"}}}}\n'
printf '{{"type":"turn.completed","usage":{{"input_tokens":10,"output_tokens":2}}}}\n'
"#,
        dagq = crate::common::shell_path(env!("CARGO_BIN_EXE_dagq")),
    );
    crate::common::template::script(&stub, script);
    stub
}

fn codex_home(db: &Path) -> PathBuf {
    db.parent().unwrap().join("codex-home")
}

fn codex(db: &Path, mode: &str) -> Codex {
    Codex {
        executable: stub_codex(db, mode),
        home: Some(codex_home(db)),
    }
}

fn stub_lines(db: &Path, name: &str) -> Vec<String> {
    fs::read_to_string(db.parent().unwrap().join(name))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// `[roles.observer]` naming Codex, with a model and an effort.
fn codex_launch() -> ActorLaunch {
    let mut models = RoleModels::default();
    let table = models.entry(ModelRole::Observer);
    table.provider = Some(Provider::Codex);
    table.model = Some(CODEX_MODEL.into());
    table.effort = Some("high".into());
    models.launch(ModelRole::Observer)
}

/// `observe_options(mode)` on the Codex launch of a role that names it.
fn codex_options(mode: ObserveMode) -> dagq::application::observer::ObserveOptions {
    dagq::application::observer::ObserveOptions {
        launch: Some(codex_launch()),
        switchable: true,
        ..observe_options(mode)
    }
}

/// Commit `dagq.toml` with `text` to the fixture's checkout and bind the
/// queue to it, so the supervisor reads its role table.
pub(crate) fn roles(db: &Path, repo: &Path, text: &str) {
    fs::write(repo.join("dagq.toml"), text).unwrap();
    for args in [
        &["add", "dagq.toml"][..],
        &["commit", "-q", "-m", "roles"][..],
    ] {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
    let common = repo.join(".git").canonicalize().unwrap();
    SqliteQueue::open(db)
        .unwrap()
        .bind_repository(common.to_str().unwrap())
        .unwrap();
}

/// The hourly and the daily observation on Codex: each starts `codex exec
/// --json` in the job's read-only profile (no writable root, no bypass of
/// the sandbox) with the role's model and effort, as the observer with a
/// token of the queue service and no queue path. Its `dagq` records a
/// finding, updates it, records another, resolves an old one and raises a
/// blocked ask on the new one through the service, attributed to the
/// observer; a note is refused. `observe_finished` counts them with their
/// ids, records the thread and the model as the span does, and the next
/// hourly observation, with nothing new but the observer's own events,
/// is skipped. `stats` and `kpi` count the observations under Codex.
#[test]
fn a_codex_observer_writes_findings_and_a_blocked_ask_through_the_queue_service() {
    let (_dir, _repo, db) = fixture();
    let old = SqliteQueue::open(&db)
        .unwrap()
        .record_finding(NewFinding {
            kind: "capacity".into(),
            target: FindingTarget::Queue,
            subject: "busy".into(),
            summary: "slots were busy".into(),
            detail: None,
            impact: None,
            evidence: Vec::new(),
            propose: None,
            by: "observer".into(),
        })
        .unwrap()
        .finding
        .id;
    assert_eq!(old.as_i64(), 1);
    let codex = codex(&db, "ok");
    fs::write(db.parent().unwrap().join("codex-writes"), "").unwrap();
    let hourly = observe(&db, &codex, &codex_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(hourly["outcome"], "succeeded", "{hourly}");
    assert_eq!(
        (
            &hourly["findings_recorded"],
            &hourly["findings_updated"],
            &hourly["findings_closed"],
            &hourly["asks"],
            &hourly["findings_without_ask"],
        ),
        (&json!(2), &json!(1), &json!(1), &json!(1), &json!(1)),
        "{hourly}"
    );
    assert_eq!(hourly["recorded_finding_ids"], json!([2, 3]));
    assert_eq!(hourly["updated_finding_ids"], json!([2]));
    assert_eq!(hourly["closed_finding_ids"], json!([1]));
    assert_eq!(hourly["without_ask_finding_ids"], json!([2]));
    assert_eq!(hourly["session_id"], "codex-thread-1");
    assert_eq!(hourly["model"], CODEX_MODEL);
    assert!(hourly.get("provider_unusable").is_none(), "{hourly}");
    assert!(hourly["wall"].is_null(), "{hourly}");
    let daily = observe(&db, &codex, &codex_options(ObserveMode::Daily)).unwrap();
    assert_eq!(daily["outcome"], "succeeded", "{daily}");
    assert_eq!(daily["findings_recorded"], 0, "{daily}");
    assert_eq!(daily["session_id"], "codex-thread-2");

    // Started in the job's read-only profile, which reaches the socket
    // only, with the role's model and effort and the prompt on stdin.
    let calls = stub_lines(&db, "codex-args.txt");
    assert_eq!(calls.len(), 2, "{calls:?}");
    let socket = dagq::infrastructure::queue_service::socket_path(
        db.canonicalize().unwrap().parent().unwrap(),
    );
    let profile: String = dagq::infrastructure::codex::job_service_config(&socket)
        .unwrap()
        .iter()
        .map(|config| format!("-c|{config}|"))
        .collect();
    assert!(profile.contains(r#"permissions.dagq_job.extends=":read-only""#));
    for call in &calls {
        assert!(
            call.starts_with("exec|--json|--skip-git-repo-check|-C|"),
            "{call}"
        );
        assert!(call.contains("/observer/"), "{call}");
        assert!(
            call.contains(&format!(
                "|-m|{CODEX_MODEL}|-c|model_reasoning_effort=\"high\"|{profile}<stdin>|"
            )),
            "{call}"
        );
        assert!(call.contains("You are the observer"), "{call}");
        for refused in [
            "dangerously",
            "bypass",
            "writable_roots",
            "workspace-write",
            "--session-id",
        ] {
            assert!(!call.contains(refused), "{refused}: {call}");
        }
    }
    // As the observer, one actor per observation, with the service's
    // socket and token and no queue path.
    let actors = stub_lines(&db, "codex-actors.txt");
    assert_eq!(actors.len(), 2);
    for actor in &actors {
        assert!(actor.starts_with("observer observer:"), "{actor}");
    }
    assert_ne!(actors[0], actors[1]);
    let env = fs::read_to_string(db.parent().unwrap().join("codex-env-1.txt")).unwrap();
    assert!(env.contains("DAGQ_SERVICE_SOCKET="), "{env}");
    assert!(env.contains("DAGQ_SERVICE_CREDENTIAL_FILE="), "{env}");
    assert!(!env.contains("DAGQ_QUEUE="), "{env}");
    // The note is refused for the observer's principal by the service.
    let note: Value = serde_json::from_str(
        &fs::read_to_string(db.parent().unwrap().join("codex-note.err")).unwrap(),
    )
    .unwrap();
    assert_eq!(note["queue_service"]["code"], "authorization_denied");

    // The writes are the observer's.
    let queue = SqliteQueue::open(&db).unwrap();
    let findings = queue
        .findings(&dagq::domain::FindingQuery::default())
        .unwrap();
    let stall = findings.iter().find(|f| f.finding.kind == "stall").unwrap();
    assert_eq!(stall.finding.occurrences, 2);
    assert_eq!(stall.finding.recorded_by, "observer");
    for kind in ["finding_recorded", "finding_updated"] {
        let events = queue_events(&db, kind);
        assert!(!events.is_empty(), "{kind}");
        assert!(events.iter().all(|e| e["by"] == "observer"), "{events:?}");
    }
    assert!(
        queue_events(&db, "finding_status_changed")
            .iter()
            .any(|e| e["by"] == "observer" && e["to"] == "resolved")
    );
    let asks = queue
        .asks(dagq::infrastructure::asks::AskQuery::default())
        .unwrap();
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0].kind.as_str(), "blocked");
    assert_eq!(asks[0].asked_by, "observer");
    assert_eq!(asks[0].finding_id, Some(dagq::domain::FindingId::new(3)));
    assert_eq!(hourly["ask_ids"], json!([asks[0].id]));
    let opened = queue_events(&db, "ask_opened");
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0]["asked_by"], "observer");
    // The events' actor is the observer's job (its role and actor id).
    let written: Vec<(String, String)> = Connection::open(&db)
        .unwrap()
        .prepare(
            "SELECT actor_role, actor_id FROM run_events
             WHERE kind IN ('finding_recorded', 'finding_updated', 'ask_opened')
               AND id > (SELECT min(id) FROM run_events WHERE kind = 'observe_started')",
        )
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(written.len(), 4, "{written:?}");
    let first_actor = actors[0].split_once(' ').unwrap().1;
    for (role, id) in &written {
        assert_eq!(role, "observer", "{written:?}");
        assert_eq!(id, first_actor, "{written:?}");
    }

    // The start records Codex and no session id; the finish and the span
    // the thread and the model.
    let started = queue_events(&db, "observe_started");
    assert_eq!(started.len(), 2);
    for start in &started {
        assert_eq!(
            start["launch"],
            json!({"role": "observer", "provider": "codex", "model": CODEX_MODEL,
                   "effort": "high", "source": "dagq.toml"})
        );
        assert!(start["session_id"].is_null(), "Codex names its thread");
    }
    let closed: Vec<Value> = queue_events(&db, "session_closed")
        .into_iter()
        .filter(|span| span["kind"] == "observer")
        .collect();
    assert_eq!(closed.len(), 2, "{closed:?}");
    for (span, thread) in closed.iter().zip(1..) {
        assert_eq!(span["reason"], "job_finished");
        assert_eq!(span["session_id"], format!("codex-thread-{thread}"));
        assert_eq!(span["model"], CODEX_MODEL);
    }

    // Nothing new but the observer's own events: the next hourly
    // observation starts no agent.
    let skipped = observe(&db, &codex, &codex_options(ObserveMode::Hourly)).unwrap();
    assert_eq!(skipped["outcome"], "skipped", "{skipped}");
    assert_eq!(stub_lines(&db, "codex-args.txt").len(), 2);

    let stats = crate::common::cli::ok(&db, &["stats", "--full"]);
    let jobs = &stats["jobs"]["observer"];
    assert_eq!(jobs["by_provider"]["codex"]["count"], 2, "{jobs}");
    assert!(jobs["by_provider"].get("claude").is_none(), "{jobs}");
    assert_eq!(jobs["by_model"][CODEX_MODEL]["count"], 2, "{jobs}");
    let kpi = crate::common::cli::ok(&db, &["kpi", "--last", "1"]);
    let count = &kpi["periods"][0]["kpis"]["job.count.observer"];
    assert_eq!(count["provider=codex"]["value"], 2.0, "{count}");
}

/// A rollout that names no model: the finish says why with
/// `model_unknown`.
#[test]
fn a_codex_observation_without_its_rollout_says_the_model_is_unknown() {
    let (_dir, _repo, db) = fixture();
    let done = observe(
        &db,
        &codex(&db, "norollout"),
        &codex_options(ObserveMode::Daily),
    )
    .unwrap();
    assert_eq!(done["outcome"], "succeeded", "{done}");
    assert_eq!(done["session_id"], "codex-thread-1");
    assert!(done["model"].is_null(), "{done}");
    assert!(done["model_unknown"].is_string(), "{done}");
}

/// A Codex observation stopped at the usage limit, of a role that names
/// its provider: Codex's own output, read through the real `Codex`, says
/// it could not be used, and it joins no hold ask of Claude's. Which
/// failures say so, and that a role naming no provider says nothing, are
/// `application::observer`'s unit test
/// `a_failure_holds_claude_or_says_codex_cannot_be_used`.
#[test]
fn a_codex_observation_at_the_usage_limit_says_codex_cannot_be_used() {
    let (_dir, _repo, db) = fixture();
    let done = observe(
        &db,
        &codex(&db, "limit"),
        &codex_options(ObserveMode::Daily),
    )
    .unwrap();
    assert_eq!(done["outcome"], "failed", "{done}");
    assert_eq!(done["session_id"], "codex-thread-1");
    assert!(done["wall"].is_null(), "{done}");
    assert!(done["hold_ask_id"].is_null(), "{done}");
    assert_eq!(
        done["provider_unusable"],
        json!({"provider": "codex", "reason": "usage_limit"})
    );
}

/// A Codex that went away before the job started left no output: its
/// start's error says Codex cannot be used.
#[test]
fn a_codex_observer_that_does_not_start_cannot_be_used() {
    let (_dir, _repo, db) = fixture();
    let gone = Codex {
        executable: db.parent().unwrap().join("codex-gone"),
        home: Some(codex_home(&db)),
    };
    let done = observe(&db, &gone, &codex_options(ObserveMode::Daily)).unwrap();
    assert_eq!(done["outcome"], "error", "{done}");
    assert_eq!(
        done["provider_unusable"],
        json!({"provider": "codex", "reason": "executable_missing"})
    );
}

/// An observation no provider can run (`--no-claude`, Codex not usable)
/// records its finish with why and starts no agent; `stats` counts no job.
#[test]
fn an_observation_no_provider_can_run_records_why() {
    let (_dir, _repo, db) = fixture();
    let done = observe(
        &db,
        &codex(&db, "ok"),
        &dagq::application::observer::ObserveOptions {
            unavailable: Some("provider_disabled: codex cannot be used (usage_limit)".into()),
            ..codex_options(ObserveMode::Hourly)
        },
    )
    .unwrap();
    assert_eq!(done["outcome"], "error", "{done}");
    assert_eq!(done["unavailable"], true, "{done}");
    assert!(
        done["error"]
            .as_str()
            .unwrap()
            .contains("provider_disabled: codex cannot be used"),
        "{done}"
    );
    assert!(done["dir"].is_null());
    assert_eq!(done["cursor_saved"], false);
    assert!(stub_lines(&db, "codex-args.txt").is_empty());
    assert!(queue_events(&db, "observe_started").is_empty());
    let stats = crate::common::cli::ok(&db, &["stats", "--full"]);
    assert_eq!(stats["jobs"]["observer"]["count"], 0, "{stats}");
}

/// A Claude stub that leaves a mark when it is started at all, and in
/// print mode (`-p`) records a finding through the queue CLI as the
/// observer on Claude does.
fn marking_claude(db: &Path) -> PathBuf {
    let stub = db.parent().unwrap().join("claude-marking-stub");
    crate::common::template::script(
        &stub,
        r#"#!/bin/sh
if [ "$1" = "-p" ]; then
  touch "${0%/*}/claude-started"
  cat > /dev/null
  exec dagq finding record --queue --kind observed --subject claude --summary "observed on claude"
fi
printf 'test provider\n'
"#,
    );
    stub
}

/// A supervisor of the fixture whose task is cancelled, observing hourly
/// and daily, with the stub `codex`; `--no-claude` as `no_claude` says.
fn supervise_observing(db: &Path, repo: &Path, codex: &Path, no_claude: bool) -> Value {
    crate::common::service::serve(db);
    SqliteQueue::open(db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let backend = TestWorkspace::new(db, false, VALID_AGENT);
    let options = SuperviseOptions {
        no_claude,
        observe_interval: Duration::from_secs(3600),
        observe_daily: true,
        codex: codex.to_owned(),
        codex_home: Some(codex_home(db)),
        ..supervise_options(1, true)
    };
    runtime::supervise(
        db,
        repo,
        &backend,
        &marking_claude(db),
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap()
}

/// `--no-claude` starts no observer whose role names no provider
/// (ADR-t1204-1 decision 2), but one set to Codex runs there: the daily
/// and the hourly observation, through the child `observe` the supervisor
/// gives the Codex executable and the routed launch, and no Claude is
/// started.
#[test]
fn a_no_claude_supervisor_runs_the_observer_set_to_codex() {
    let (_dir, repo, db) = fixture();
    roles(&db, &repo, "[roles.observer]\nprovider = \"codex\"\n");
    let codex = stub_codex(&db, "ok");
    supervise_observing(&db, &repo, &codex, true);
    let finished = queue_events(&db, "observe_finished");
    assert_eq!(
        finished
            .iter()
            .map(|f| (f["mode"].as_str().unwrap(), f["outcome"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        [("daily", "succeeded"), ("hourly", "succeeded")],
        "{finished:?}"
    );
    for start in queue_events(&db, "observe_started") {
        assert_eq!(start["launch"]["provider"], "codex", "{start}");
    }
    assert_eq!(stub_lines(&db, "codex-args.txt").len(), 2);
    assert!(!db.parent().unwrap().join("claude-started").exists());
}

/// Without `--no-claude` and with no role table, the observer runs on
/// Claude as before, and Codex is not started.
#[test]
fn a_supervisor_runs_the_observer_without_a_role_table_on_claude() {
    let (_dir, repo, db) = fixture();
    let codex = stub_codex(&db, "ok");
    supervise_observing(&db, &repo, &codex, false);
    let finished = queue_events(&db, "observe_finished");
    assert_eq!(finished.len(), 2, "{finished:?}");
    for start in queue_events(&db, "observe_started") {
        assert_eq!(start["launch"]["provider"], "claude", "{start}");
    }
    assert!(stub_lines(&db, "codex-args.txt").is_empty());
    assert!(db.parent().unwrap().join("claude-started").exists());
}

/// Under `--no-claude`, a Codex observation that stops at the usage limit
/// is not moved to Claude: Codex is held, the observation is due again,
/// and with no provider left it records why.
#[test]
fn a_no_claude_supervisor_never_moves_a_failed_codex_observer_to_claude() {
    let (_dir, repo, db) = fixture();
    roles(&db, &repo, "[roles.observer]\nprovider = \"codex\"\n");
    let codex = stub_codex(&db, "limit");
    supervise_observing(&db, &repo, &codex, true);
    let finished = queue_events(&db, "observe_finished");
    let first = &finished[0];
    assert_eq!(first["mode"], "daily", "{finished:?}");
    assert_eq!(
        first["provider_unusable"],
        json!({"provider": "codex", "reason": "usage_limit"})
    );
    let held = queue_events(&db, "provider_held");
    assert!(
        held.iter().any(|event| event["provider"] == "codex"),
        "{held:?}"
    );
    // Codex ran once; the daily observation was due again and found no
    // provider.
    assert_eq!(stub_lines(&db, "codex-args.txt").len(), 1);
    let again = &finished[1];
    assert_eq!(again["mode"], "daily", "{finished:?}");
    assert_eq!(again["outcome"], "error", "{again}");
    assert_eq!(again["unavailable"], true, "{again}");
    assert!(
        again["error"]
            .as_str()
            .unwrap()
            .contains("codex cannot be used (usage_limit)"),
        "{again}"
    );
    assert!(!db.parent().unwrap().join("claude-started").exists());
}

/// With Claude usable, a Codex observation that stops at the usage limit
/// holds Codex and the observation starts again on Claude.
#[test]
fn a_codex_observer_at_the_usage_limit_starts_again_on_claude() {
    let (_dir, repo, db) = fixture();
    roles(&db, &repo, "[roles.observer]\nprovider = \"codex\"\n");
    let codex = stub_codex(&db, "limit");
    supervise_observing(&db, &repo, &codex, false);
    let started = queue_events(&db, "observe_started");
    assert_eq!(started[0]["launch"]["provider"], "codex", "{started:?}");
    let again = &started[1];
    assert_eq!(again["mode"], "daily", "{started:?}");
    assert_eq!(again["launch"]["provider"], "claude", "{again}");
    assert_eq!(again["launch"]["switched_from"], "codex", "{again}");
    assert_eq!(stub_lines(&db, "codex-args.txt").len(), 1);
    assert!(db.parent().unwrap().join("claude-started").exists());
}
