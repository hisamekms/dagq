//! The throughput review on Codex (task 1220, as the goal and plan reviews
//! of ADR-t1063-1): `[roles.throughput_review]`'s `provider = "codex"`
//! starts the job as `codex exec --json` in the read-only sandbox (the
//! queue service's job profile), played by a stub `codex` that writes the
//! JSONL and the rollout codex-cli 0.155.1 writes, and its reply goes
//! through the same saving, weekly finding and `throughput_review_reported`
//! as Claude's. Under `--no-claude` it still starts, and a Codex that cannot
//! be used never moves it to Claude.
use crate::runtime_support;
use crate::runtime_throughput_review::{
    attentions, land, options, queue_events, review, review_claude_stub, utc,
};
use dagq::domain::{
    EventKind, LeaseToken, Provider,
    actor_model::{ActorLaunch, ModelRole, RoleModels},
    throughput_review::ReviewMode,
};
use dagq::infrastructure::codex::Codex;

use runtime_support::*;

/// The model the stub `codex` writes to its rollouts.
const CODEX_MODEL: &str = "gpt-6-astra";

/// The review the stub replies with: a conclusion, details and a next move.
const REPLY: &str = "## Conclusion\n- landings held on Codex\n\n## Details\nthe numbers\n\n```next_move\n{\"summary\": \"split the e2e\", \"why\": \"verify is the constraint\"}\n```";

/// A stub `codex` next to the queue: `--version` answers; as codex-cli
/// does, `exec` in a directory outside any Git work tree without
/// `--skip-git-repo-check` says so on stderr and exits 1 (task 1378),
/// recording either way whether its directory was inside one in
/// `codex-cwd.txt`; otherwise `exec`
/// appends its arguments (each ended by `|`) to `codex-args.txt` and its
/// role and actor to `codex-actors.txt`, prints `thread.started` (thread
/// `codex-thread-<call>`) and `turn.started`, then, as `codex-mode` says,
/// fails at the usage limit (`limit`), or reads the KPIs through the
/// queue service, writes the thread's rollout with its model and prints
/// [`REPLY`] as its last message and `turn.completed`.
fn stub_codex(db: &Path, mode: &str) -> PathBuf {
    let dir = db.parent().unwrap();
    let stub = dir.join("codex");
    let reply = json!({"type": "item.completed", "item": {"id": "m1", "type": "agent_message", "text": REPLY}});
    fs::write(dir.join("codex-reply.jsonl"), format!("{reply}\n")).unwrap();
    fs::write(dir.join("codex-mode"), mode).unwrap();
    let script = format!(
        r#"#!/bin/sh
DIR="${{0%/*}}"
[ "$1" = --version ] && {{ echo "codex-cli 0.155.1"; exit 0; }}
if git rev-parse --is-inside-work-tree > /dev/null 2>&1; then
  echo "inside $PWD" >> "$DIR/codex-cwd.txt"
else
  echo "outside $PWD" >> "$DIR/codex-cwd.txt"
  SKIP=
  for arg in "$@"; do [ "$arg" = --skip-git-repo-check ] && SKIP=1; done
  if [ -z "$SKIP" ]; then
    echo "Reading additional input from stdin..." >&2
    echo "Not inside a trusted directory and --skip-git-repo-check was not specified." >&2
    exit 1
  fi
fi
for arg in "$@"; do printf '%s|' "$arg" | tr '\n' ' '; done >> "$DIR/codex-args.txt"
# The prompt comes on stdin, never as an argument (task 1560).
printf '<stdin>|' >> "$DIR/codex-args.txt"
tr '\n' ' ' >> "$DIR/codex-args.txt"
printf '\n' >> "$DIR/codex-args.txt"
printf '%s %s\n' "$DAGQ_ROLE" "$DAGQ_ACTOR_ID" >> "$DIR/codex-actors.txt"
THREAD="codex-thread-$(wc -l < "$DIR/codex-actors.txt" | tr -d ' ')"
echo "Reading additional input from stdin..." >&2
printf '{{"type":"thread.started","thread_id":"%s"}}\n{{"type":"turn.started"}}\n' "$THREAD"
if [ "$(cat "$DIR/codex-mode")" = limit ]; then
  printf '{{"type":"error","message":"You have hit your usage limit. Try again later."}}\n{{"type":"turn.failed","error":{{"message":"You have hit your usage limit."}}}}\n'
  exit 1
fi
env > "$DIR/codex-env.txt"
{dagq} kpi > "$DIR/codex-kpi.json" 2> "$DIR/codex-kpi.err"
SESSIONS="$DIR/codex-home/sessions/2026/09/29"
mkdir -p "$SESSIONS"
printf '{{"timestamp":"%s","type":"turn_context","payload":{{"model":"{CODEX_MODEL}","effort":"high"}}}}\n' "$(date -u +%Y-%m-%dT%H:%M:%S.000Z)" >> "$SESSIONS/rollout-2026-09-29T00-00-00-$THREAD.jsonl"
cat "$DIR/codex-reply.jsonl"
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

fn stub_lines(db: &Path, name: &str) -> Vec<String> {
    fs::read_to_string(db.parent().unwrap().join(name))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// `[roles.throughput_review]` naming Codex, with a model and an effort.
fn codex_launch() -> ActorLaunch {
    let mut models = RoleModels::default();
    let table = models.entry(ModelRole::ThroughputReview);
    table.provider = Some(Provider::Codex);
    table.model = Some(CODEX_MODEL.into());
    table.effort = Some("high".into());
    models.launch(ModelRole::ThroughputReview)
}

/// Commit `dagq.toml` with `text` to the fixture's checkout.
fn roles(repo: &Path, text: &str) {
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
}

/// The hour, the day and the week on Codex: each starts `codex exec --json`
/// in the job's read-only profile with the role's model and effort and the
/// job's role, and its last message is read as Claude's output is: saved,
/// the weekly next move a finding marked for a proposal, and
/// `throughput_review_reported` for the inbox. The start records Codex and
/// no session id; the finish and the span the thread and the model; `stats`
/// and `kpi` count the reviews under Codex.
#[test]
fn a_throughput_review_on_codex_reads_its_last_message_like_claude_s_output() {
    let (_dir, _repo, db) = fixture();
    let mut hours = vec![4; 27];
    hours.push(10);
    land(&db, &hours);
    let stub = stub_codex(&db, "ok");
    let codex = Codex {
        executable: stub,
        home: Some(codex_home(&db)),
    };
    for mode in ReviewMode::ALL {
        let done = review(
            &db,
            &codex,
            &dagq::application::throughput_review::ReviewOptions {
                launch: Some(codex_launch()),
                switchable: true,
                ..options(mode)
            },
        )
        .unwrap();
        assert_eq!(done["outcome"], "succeeded", "{done}");
    }

    let calls = stub_lines(&db, "codex-args.txt");
    assert_eq!(calls.len(), 3, "{calls:?}");
    // Each started in its job directory, outside any Git work tree, which
    // codex-cli refuses without `--skip-git-repo-check` (task 1378).
    let cwds = stub_lines(&db, "codex-cwd.txt");
    assert_eq!(cwds.len(), 3, "{cwds:?}");
    for cwd in &cwds {
        assert!(cwd.starts_with("outside "), "{cwd}");
        assert!(cwd.contains("/reports/reviews/"), "{cwd}");
    }
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
        assert!(
            call.contains(&format!(
                "|-m|{CODEX_MODEL}|-c|model_reasoning_effort=\"high\"|{profile}<stdin>|"
            )),
            "{call}"
        );
        assert!(call.contains("You are the throughput review job"), "{call}");
        for refused in ["dangerously", "bypass", "writable_roots", "--session-id"] {
            assert!(!call.contains(refused), "{refused}: {call}");
        }
    }
    assert_eq!(
        stub_lines(&db, "codex-actors.txt"),
        [
            "throughput-review-job throughput-review-job:hourly:2026-09-29T03",
            "throughput-review-job throughput-review-job:daily:2026-09-28",
            "throughput-review-job throughput-review-job:weekly:2026-W39",
        ]
    );
    // Its `dagq` reads through the queue service, not the queue's path.
    let env = fs::read_to_string(db.parent().unwrap().join("codex-env.txt")).unwrap();
    assert!(env.contains("DAGQ_SERVICE_SOCKET="), "{env}");
    assert!(
        !fs::read_to_string(db.parent().unwrap().join("codex-kpi.json"))
            .unwrap()
            .is_empty()
    );

    let started = queue_events(&db, "throughput_review_started");
    assert_eq!(started.len(), 3);
    for start in &started {
        assert_eq!(
            start["launch"],
            json!({"role": "throughput_review", "provider": "codex", "model": CODEX_MODEL,
                   "effort": "high", "source": "dagq.toml"})
        );
        assert!(start["session_id"].is_null(), "Codex names its thread");
    }
    let finished = queue_events(&db, "throughput_review_finished");
    for (finish, thread) in finished.iter().zip(1..) {
        assert_eq!(finish["session_id"], format!("codex-thread-{thread}"));
        assert_eq!(finish["model"], CODEX_MODEL);
        assert!(finish.get("provider_unusable").is_none(), "{finish}");
    }
    // Saved and told to the inbox as Claude's are.
    let reported = queue_events(&db, "throughput_review_reported");
    assert_eq!(reported.len(), 3);
    for notice in &reported {
        assert_eq!(notice["conclusion"], json!(["- landings held on Codex"]));
        let saved = fs::read_to_string(notice["path"].as_str().unwrap()).unwrap();
        assert!(saved.contains("## Details"), "{saved}");
        assert!(!saved.contains("split the e2e"), "{saved}");
    }
    let findings = SqliteQueue::open(&db)
        .unwrap()
        .findings(&dagq::domain::FindingQuery::default())
        .unwrap();
    assert_eq!(findings.len(), 1, "only the weekly one proposes");
    assert_eq!(findings[0].finding.kind, "throughput");
    assert_eq!(findings[0].finding.subject, "weekly/2026-W39");
    assert_eq!(findings[0].finding.summary, "split the e2e");
    assert_eq!(reported[2]["finding_id"], json!(findings[0].finding.id));
    assert!(
        attentions(&db)
            .iter()
            .all(|event| event["next"] != "check the failed review")
    );

    // Each span closes by its directory with the thread and the model, not
    // a Claude transcript.
    let closed = queue_events(&db, "session_closed");
    assert_eq!(closed.len(), 3, "{closed:?}");
    for (span, thread) in closed.iter().zip(1..) {
        assert_eq!(span["kind"], "throughput_review");
        assert_eq!(span["reason"], "job_finished");
        assert_eq!(span["session_id"], format!("codex-thread-{thread}"));
        assert_eq!(span["model"], CODEX_MODEL);
    }

    let stats = crate::common::cli::ok(&db, &["stats", "--full"]);
    let jobs = &stats["jobs"]["throughput_review"];
    assert_eq!(jobs["by_provider"]["codex"]["count"], 3, "{jobs}");
    assert!(jobs["by_provider"].get("claude").is_none(), "{jobs}");
    assert_eq!(jobs["by_model"][CODEX_MODEL]["count"], 3, "{jobs}");
    let kpi = crate::common::cli::ok(&db, &["kpi", "--last", "1"]);
    let count = &kpi["periods"][0]["kpis"]["job.count.throughput_review"];
    assert_eq!(count["provider=codex"]["value"], 3.0, "{count}");
}

/// A Codex review stopped at the usage limit: with a role that names its
/// provider, its finish says Codex could not be used and is no attention
/// (it starts again on the other provider); without, it is a failure as
/// any other, for the inbox.
#[test]
fn a_codex_review_at_the_usage_limit_says_codex_cannot_be_used() {
    for switchable in [true, false] {
        let (_dir, _repo, db) = fixture();
        let codex = Codex {
            executable: stub_codex(&db, "limit"),
            home: Some(codex_home(&db)),
        };
        let done = review(
            &db,
            &codex,
            &dagq::application::throughput_review::ReviewOptions {
                launch: Some(codex_launch()),
                switchable,
                ..options(ReviewMode::Daily)
            },
        )
        .unwrap();
        assert_eq!(done["outcome"], "failed", "{done}");
        assert_eq!(done["session_id"], "codex-thread-1");
        let checks = attentions(&db)
            .iter()
            .filter(|event| event["next"] == "check the failed review")
            .count();
        if switchable {
            assert_eq!(
                done["provider_unusable"],
                json!({"provider": "codex", "reason": "usage_limit"})
            );
            assert_eq!(checks, 0);
        } else {
            assert!(done.get("provider_unusable").is_none(), "{done}");
            assert_eq!(checks, 1);
        }
    }
}

/// A Codex that went away before the job started left no output: its
/// start's error says Codex cannot be used.
#[test]
fn a_codex_that_does_not_start_cannot_be_used() {
    let (_dir, _repo, db) = fixture();
    let codex = Codex {
        executable: db.parent().unwrap().join("codex-gone"),
        home: Some(codex_home(&db)),
    };
    let done = review(
        &db,
        &codex,
        &dagq::application::throughput_review::ReviewOptions {
            launch: Some(codex_launch()),
            switchable: true,
            ..options(ReviewMode::Daily)
        },
    )
    .unwrap();
    assert_eq!(done["outcome"], "error", "{done}");
    assert_eq!(
        done["provider_unusable"],
        json!({"provider": "codex", "reason": "executable_missing"})
    );
}

/// The command the supervisor starts, with a Codex gone since the
/// supervisor found it: it does not fail before the review, so the period
/// records its finish, which says Codex cannot be used, and the supervisor
/// can hold Codex and start the period on the other provider.
#[test]
fn the_command_with_a_codex_gone_records_that_codex_cannot_be_used() {
    let (_dir, _repo, db) = fixture();
    crate::common::service::serve(&db);
    let gone = db.parent().unwrap().join("codex-gone");
    let launch = codex_launch().to_value().to_string();
    let done = crate::common::cli::ok(
        &db,
        &[
            "throughput-review",
            "--mode",
            "daily",
            "--at",
            "1790655900",
            "--utc-offset",
            "0",
            "--codex",
            gone.to_str().unwrap(),
            "--launch",
            &launch,
            "--switchable",
        ],
    );
    assert_eq!(done["outcome"], "error", "{done}");
    assert_eq!(
        done["provider_unusable"],
        json!({"provider": "codex", "reason": "executable_missing"})
    );
    let finished = queue_events(&db, "throughput_review_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["provider_unusable"], done["provider_unusable"]);
}

/// A supervisor that took over by an exec reaps the reviews the process
/// before left running; one of them that found Codex unusable holds Codex,
/// so its period, due again, starts on Claude rather than on Codex once
/// more.
#[test]
fn a_handed_over_codex_review_that_found_codex_unusable_holds_codex() {
    let (_dir, repo, db) = fixture();
    roles(&repo, "[roles.throughput_review]\nprovider = \"codex\"\n");
    let codex = stub_codex(&db, "ok");
    let token = LeaseToken::new("throughput-handoff");
    let mut queue = SqliteQueue::open(&db).unwrap();
    queue
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    queue
        .register_supervisor(&token, std::process::id(), 1, "previous")
        .unwrap();
    // The process before the exec started the last hour's review on Codex,
    // which stopped at the usage limit after the exec.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let hour = dagq::domain::throughput_review::window(
        ReviewMode::Hourly,
        i64::try_from(now * 1000).unwrap(),
        0,
    )
    .label;
    queue
        .record_queue_event(
            EventKind::ThroughputReviewFinished,
            json!({"mode": "hourly", "period": hour, "outcome": "failed", "exit_code": 1,
                   "dir": "/nonexistent/reviews/hourly", "error": null,
                   "provider_unusable": {"provider": "codex", "reason": "usage_limit"},
                   "pid": 4_000_000_u32, "parent_pid": std::process::id()}),
        )
        .unwrap();
    drop(queue);
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    let options = SuperviseOptions {
        throughput_review: true,
        utc_offset: utc,
        codex,
        codex_home: Some(codex_home(&db)),
        handoff_token: Some(token),
        ..supervise_options(1, true)
    };
    runtime::supervise(
        &db,
        &repo,
        &backend,
        &review_claude_stub(&db, false),
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options,
    )
    .unwrap();
    let held = queue_events(&db, "provider_held");
    assert!(
        held.iter().any(|event| event["provider"] == "codex"),
        "{held:?}"
    );
    // Codex was not started again: every review went to Claude.
    assert!(stub_lines(&db, "codex-args.txt").is_empty());
    let started = queue_events(&db, "throughput_review_started");
    assert!(!started.is_empty());
    for start in &started {
        assert_eq!(start["launch"]["provider"], "claude", "{start}");
        assert_eq!(start["launch"]["switched_from"], "codex", "{start}");
    }
    assert!(
        started.iter().any(|start| start["mode"] == "hourly"),
        "the hour is due again: {started:?}"
    );
}

/// A review no provider can run (`--no-claude`, Codex not usable) records
/// its failure with why and starts no agent.
#[test]
fn a_review_no_provider_can_run_records_why() {
    let (_dir, _repo, db) = fixture();
    let codex = Codex {
        executable: stub_codex(&db, "ok"),
        home: Some(codex_home(&db)),
    };
    let done = review(
        &db,
        &codex,
        &dagq::application::throughput_review::ReviewOptions {
            launch: Some(codex_launch()),
            unavailable: Some("provider_disabled: codex cannot be used (usage_limit)".into()),
            ..options(ReviewMode::Weekly)
        },
    )
    .unwrap();
    assert_eq!(done["outcome"], "error", "{done}");
    assert!(
        done["error"]
            .as_str()
            .unwrap()
            .contains("provider_disabled: codex cannot be used"),
        "{done}"
    );
    assert!(stub_lines(&db, "codex-args.txt").is_empty());
    assert!(queue_events(&db, "throughput_review_started").is_empty());
    assert!(
        attentions(&db)
            .iter()
            .any(|event| event["next"] == "check the failed review")
    );
}

/// A Claude stub that leaves a mark when it is started at all.
fn marking_claude(db: &Path) -> PathBuf {
    let stub = db.parent().unwrap().join("claude-marking-stub");
    crate::common::template::script(
        &stub,
        "#!/bin/sh\ntouch \"${0%/*}/claude-started\"\nprintf 'test provider\\n'\n",
    );
    stub
}

fn supervise_no_claude(db: &Path, repo: &Path, codex: &Path) -> Value {
    SqliteQueue::open(db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let backend = TestWorkspace::new(db, false, VALID_AGENT);
    let options = SuperviseOptions {
        no_claude: true,
        throughput_review: true,
        utc_offset: utc,
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

/// `--no-claude` starts no review whose role names no provider (ADR-t1204-1
/// decision 2), but one set to Codex runs there: the hour, the day and the
/// week, and no Claude is started.
#[test]
fn a_no_claude_supervisor_runs_the_reviews_set_to_codex() {
    let (_dir, repo, db) = fixture();
    roles(&repo, "[roles.throughput_review]\nprovider = \"codex\"\n");
    let codex = stub_codex(&db, "ok");
    supervise_no_claude(&db, &repo, &codex);
    let finished = queue_events(&db, "throughput_review_finished");
    let modes: Vec<&str> = finished
        .iter()
        .map(|f| f["mode"].as_str().unwrap())
        .collect();
    for mode in ReviewMode::ALL {
        assert!(modes.contains(&mode.as_str()), "{finished:?}");
    }
    for finish in &finished {
        assert_eq!(finish["outcome"], "succeeded", "{finish}");
    }
    for start in queue_events(&db, "throughput_review_started") {
        assert_eq!(start["launch"]["provider"], "codex", "{start}");
    }
    assert_eq!(stub_lines(&db, "codex-args.txt").len(), finished.len());
    assert!(!db.parent().unwrap().join("claude-started").exists());
}

/// Under `--no-claude`, a Codex review that stops at the usage limit is not
/// moved to Claude: Codex is held, the period is due again, and with no
/// provider left it records why, a notice for the inbox.
#[test]
fn a_no_claude_supervisor_never_moves_a_failed_codex_review_to_claude() {
    let (_dir, repo, db) = fixture();
    roles(&repo, "[roles.throughput_review]\nprovider = \"codex\"\n");
    let codex = stub_codex(&db, "limit");
    supervise_no_claude(&db, &repo, &codex);
    let finished = queue_events(&db, "throughput_review_finished");
    let first = &finished[0];
    assert_eq!(first["mode"], "hourly", "{finished:?}");
    assert_eq!(
        first["provider_unusable"],
        json!({"provider": "codex", "reason": "usage_limit"})
    );
    // Codex ran once; the hour was due again and found no provider.
    assert_eq!(stub_lines(&db, "codex-args.txt").len(), 1);
    let again = finished
        .iter()
        .skip(1)
        .find(|finish| finish["mode"] == "hourly")
        .unwrap_or_else(|| panic!("{finished:?}"));
    assert_eq!(again["outcome"], "error", "{again}");
    assert!(
        again["error"]
            .as_str()
            .unwrap()
            .contains("codex cannot be used (usage_limit)"),
        "{again}"
    );
    assert!(!db.parent().unwrap().join("claude-started").exists());
    assert!(
        attentions(&db)
            .iter()
            .any(|event| event["next"] == "check the failed review")
    );
}

/// With `[provider_fallback] jobs = false` (ADR-t1857-1), a Codex review
/// that stops at the usage limit is not moved to Claude: Codex is held,
/// its period is not counted as reviewed and nothing starts while the hold
/// is in place; once the hold ends the same period starts again on Codex.
#[test]
fn with_the_fallback_off_a_codex_review_at_its_limit_waits_and_retries_codex() {
    let (_dir, repo, db) = fixture();
    roles(
        &repo,
        "[roles.throughput_review]\nprovider = \"codex\"\n\n[provider_fallback]\njobs = false\n",
    );
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let codex = stub_codex(&db, "limit");
    let supervise = |db: &Path| {
        let backend = TestWorkspace::new(db, false, VALID_AGENT);
        let options = SuperviseOptions {
            throughput_review: true,
            utc_offset: utc,
            codex: codex.clone(),
            codex_home: Some(codex_home(db)),
            // The weekly next move would open a finding planner; this test
            // is about the review alone.
            runtime_planners: Some(0),
            ..supervise_options(1, true)
        };
        runtime::supervise(
            db,
            &repo,
            &backend,
            &marking_claude(db),
            Path::new(env!("CARGO_BIN_EXE_dagq")),
            &options,
        )
        .unwrap();
    };
    supervise(&db);
    let finished = queue_events(&db, "throughput_review_finished");
    assert_eq!(finished.len(), 1, "{finished:?}");
    let hour = finished[0]["period"].clone();
    assert_eq!(finished[0]["mode"], "hourly", "{finished:?}");
    assert_eq!(
        finished[0]["provider_unusable"],
        json!({"provider": "codex", "reason": "usage_limit"})
    );
    let held = queue_events(&db, "provider_held");
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["provider"], "codex");
    assert_eq!(held[0]["reason"], "usage_limit");
    // Nothing went to Claude, and nothing started while Codex is held.
    let started = queue_events(&db, "throughput_review_started");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0]["launch"]["provider"], "codex");
    assert!(
        queue_events(&db, "ask_opened")
            .iter()
            .all(|p| p["kind"] != "queue_hold")
    );
    // A pass while Codex is held starts nothing: not on Claude, not on Codex.
    supervise(&db);
    assert_eq!(queue_events(&db, "throughput_review_started").len(), 1);
    assert_eq!(queue_events(&db, "throughput_review_finished").len(), 1);
    // Codex's hold ends (its time is up), and Codex can be used again.
    fs::write(db.parent().unwrap().join("codex-mode"), "ok").unwrap();
    SqliteQueue::open(&db)
        .unwrap()
        .record_queue_event(
            EventKind::ProviderReleased,
            json!({"provider": "codex", "reason": "usage_limit",
                   "since": held[0]["since"], "why": "retry_due"}),
        )
        .unwrap();
    supervise(&db);
    let finished = queue_events(&db, "throughput_review_finished");
    let again = finished
        .iter()
        .skip(1)
        .find(|finish| finish["mode"] == "hourly")
        .unwrap_or_else(|| panic!("{finished:?}"));
    assert_eq!(again["period"], hour, "{again}");
    assert_eq!(again["outcome"], "succeeded", "{again}");
    for start in queue_events(&db, "throughput_review_started") {
        assert_eq!(start["launch"]["provider"], "codex", "{start}");
        assert!(start["launch"].get("switched_from").is_none(), "{start}");
    }
}
