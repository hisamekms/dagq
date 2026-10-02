//! Runtime tests: a Codex worker's `dagq ask` goes to the queue service in
//! client mode (ADR-t1233-5 decision 5, amending ADR-t813-3 decision 3),
//! which opens it at once for the worker's principal. A request in the run
//! directory, which a turn an older binary started still writes, is opened
//! by its supervisor until no such turn is left: the tests of that path
//! write the requests where such a turn did.
use crate::common;
use crate::runtime_codex::{
    FINISH, TASK, codex_fixture, detail, finished, open_ask, supervise_thread,
};
use crate::runtime_support;

use dagq::domain::{
    AskReason, NewAsk,
    ask_request::{ASK_REQUESTS_DIR, pending_name, taken_name},
};
use runtime_support::*;

fn queue_events(db: &Path, kind: &str) -> Vec<Value> {
    SqliteQueue::open(db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| event.payload)
        .collect()
}

/// Wait, up to a step's limit, until `done`.
fn wait_for(what: &str, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(
            started.elapsed() < common::STEP_LIMIT,
            "{what}: not within {:?}",
            common::STEP_LIMIT
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn worker_questions(db: &Path) -> Vec<dagq::domain::Ask> {
    SqliteQueue::open(db)
        .unwrap()
        .asks(AskQuery {
            all: true,
            ..AskQuery::default()
        })
        .unwrap()
        .into_iter()
        .filter(|ask| ask.kind == AskKind::WorkerQuestion)
        .collect()
}

/// The names in the run's ask request directory, sorted.
fn request_files(run: &TaskRun) -> Vec<String> {
    let mut names: Vec<String> =
        fs::read_dir(Path::new(run.run_dir().unwrap()).join(ASK_REQUESTS_DIR))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
    names.sort();
    names
}

/// The worker's `dagq ask` opens the worker's `worker_question` through
/// the queue service, with no queue path in the turn's environment and no
/// request in the run directory; the inbox's answer is the next turn's
/// `codex exec resume` and the run lands. The turn's idle marker may be
/// seconds older than the ask (made so here): the answer still goes out,
/// the session being between turns.
#[test]
fn a_codex_workers_ask_goes_to_the_queue_service() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) env > "$RUN_DIR/turn-env.txt"; "$DAGQ" ask --run "$DAGQ_RUN_ID" --kind worker_question --because scope --topic design_choice --question "which file" --option a.txt --option b.txt > "$RUN_DIR/ask-output.json" 2>&1 || fail "ask failed: $(cat "$RUN_DIR/ask-output.json")"; say asked ;;
2) case "$PROMPT" in "answer to ask "*) {FINISH} ;; *) say lost ;; esac ;;
esac"#
        ),
    );
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        &codex,
        &[verdict("pass", &[], "meets the acceptance")],
    );
    let ask = open_ask(&db, |ask| ask.kind == AskKind::WorkerQuestion);
    // The turn that asked has ended and written its idle marker (the
    // run's first, through a rename).
    let marker = detail(&db).runs[0].idle_marker_path().unwrap();
    wait_for("the asking turn's idle marker", || marker.is_file());
    fs::File::options()
        .write(true)
        .open(&marker)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(10))
        .unwrap();
    assert_eq!(ask.question, "which file");
    assert_eq!(ask.options, ["a.txt", "b.txt"]);
    assert_eq!(ask.asked_by, "worker");
    assert_eq!(ask.reason_category, AskReason::Scope);
    assert_eq!(ask.topics, ["design_choice"]);
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "a.txt")
        .unwrap();
    let outcome = finished(&backend, supervisor);
    assert_eq!(
        outcome["runs"].as_array().unwrap().last().unwrap()["status"],
        "integrated",
        "{outcome}"
    );
    let detail = detail(&db);
    let run = &detail.runs[0];
    assert_eq!(ask.run_id.as_ref(), Some(run.id()));
    // On the run and told to the inbox, as an ask the command opens.
    let opened = payloads(&detail, "ask_opened");
    assert!(
        opened
            .iter()
            .any(|p| p["ask_id"] == json!(ask.id) && p["asked_by"] == "worker"),
        "{opened:?}"
    );
    // The service told the inbox, through its own cmux.
    let notified = fs::read_to_string(fake_cmux_dir(&db).join("calls")).unwrap_or_default();
    assert!(
        notified.contains("ask #1 worker_question --body which file")
            && notified.contains(&format!("run {}", run.id())),
        "{notified}"
    );

    // The command printed the ask the service opened, wrote no request,
    // and its turn was given the service, not the queue's path.
    let run_dir = Path::new(run.run_dir().unwrap());
    let output: Value =
        serde_json::from_slice(&fs::read(run_dir.join("ask-output.json")).unwrap()).unwrap();
    assert_eq!(output["id"], json!(ask.id), "{output}");
    assert!(!run_dir.join(ASK_REQUESTS_DIR).exists());
    assert!(payloads(&detail, "ask_request_taken").is_empty());
    let env = fs::read_to_string(run_dir.join("turn-env.txt")).unwrap();
    assert!(env.contains("DAGQ_SERVICE_SOCKET="), "{env}");
    assert!(env.contains("DAGQ_SERVICE_CREDENTIAL_FILE="), "{env}");
    for line in env.lines().filter(|line| !line.starts_with("DB=")) {
        assert!(
            !line.contains("queue's data.db")
                && !line.starts_with("DAGQ_QUEUE=")
                && !line.starts_with("DAGQ_ASK_REQUESTS="),
            "{line}"
        );
    }
    assert_eq!(worker_questions(&db).len(), 1);

    // The answer went as the next turn, a resume of the same thread.
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(
        calls[1],
        format!("resume codex-thread-1 answer to ask {}: a.txt", ask.id)
    );
}

/// Acceptance (3): requests for another run, for a kind or a reason the
/// worker may not ask, or broken ones open no ask, and each refusal is on
/// the run (`ask_request_taken`, `refused`), a refusal of the policy as
/// `authorization_denied` as well (a request that names a finding too). A
/// request taken before the supervisor could mark its file (put back as if
/// it had stopped there) is not opened again.
#[test]
fn requests_the_worker_may_not_make_open_no_ask_and_none_opens_twice() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    let own = r#""run_id":"'"$DAGQ_RUN_ID"'""#;
    let request = |id: &str, body: &str| {
        format!(
            r#"printf '%s' '{body}' > "$R/.{id}.tmp"; mv "$R/.{id}.tmp" "$R/{id}.json"
"#
        )
        .replace("{id}", id)
    };
    let valid = |id: &str| {
        format!(
            r#"{{"id":"{id}","kind":"worker_question","because":"scope","question":"which file","topics":["other"],{own}}}"#
        )
    };
    let script = [
        "case \"$TURN\" in\n1) R=\"$RUN_DIR/ask-requests\"; mkdir -p \"$R\"\n".to_owned(),
        request(
            "other",
            r#"{"id":"other","kind":"worker_question","because":"scope","question":"q","topics":["other"],"run_id":"someone-elses-run"}"#,
        ),
        request(
            "landing",
            &format!(r#"{{"id":"landing","kind":"approve_landing","because":"scope","question":"q",{own}}}"#),
        ),
        request(
            "cost",
            &format!(r#"{{"id":"cost","kind":"worker_question","because":"cost","question":"q","topics":["other"],{own}}}"#),
        ),
        request("broken", r#"{"id":"#),
        request(
            "finding",
            &format!(
                r#"{{"id":"finding","kind":"worker_question","because":"scope","question":"q","topics":["other"],"finding_id":1,{own}}}"#
            ),
        ),
        request("named", &valid("elsewhere")),
        request("dup1", &valid("dup1")),
        "say asked ;;\n".to_owned(),
        format!("2) case \"$PROMPT\" in \"answer to ask \"*) {FINISH} ;; *) say lost ;; esac ;;\nesac"),
    ]
    .concat();
    set_turns(dir.path(), &script);
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        &codex,
        &[verdict("pass", &[], "meets the acceptance")],
    );
    let ask = open_ask(&db, |ask| ask.kind == AskKind::WorkerQuestion);
    let run = detail(&db).runs[0].clone();
    let requests = Path::new(run.run_dir().unwrap()).join(ASK_REQUESTS_DIR);
    // Put the taken request back, as a supervisor that stopped after the
    // ask opened and before it marked the file would leave it.
    let taken = requests.join(taken_name("dup1"));
    wait_for("the request dup1 to be marked taken", || taken.is_file());
    fs::copy(&taken, requests.join(".dup1.tmp")).unwrap();
    fs::rename(
        requests.join(".dup1.tmp"),
        requests.join(pending_name("dup1")),
    )
    .unwrap();
    // The run waits for the answer outside its slot; its watch takes the
    // request again once it returns, before the answer goes out.
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "a.txt")
        .unwrap();
    let outcome = finished(&backend, supervisor);
    assert_eq!(
        outcome["runs"].as_array().unwrap().last().unwrap()["status"],
        "integrated",
        "{outcome}"
    );

    // One ask, of the one valid request.
    let asks = worker_questions(&db);
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].id, ask.id);
    assert_eq!(asks[0].run_id.as_ref(), Some(run.id()));
    let detail = detail(&db);
    let mut taken: Vec<(String, String)> = payloads(&detail, "ask_request_taken")
        .into_iter()
        .map(|p| {
            (
                p["request"].as_str().unwrap().to_owned(),
                p["outcome"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    taken.sort();
    let expected: Vec<(String, String)> = [
        ("broken", "refused"),
        ("cost", "refused"),
        ("dup1", "opened"),
        ("finding", "refused"),
        ("landing", "refused"),
        ("named", "refused"),
        ("other", "refused"),
    ]
    .map(|(id, outcome)| (id.to_owned(), outcome.to_owned()))
    .to_vec();
    assert_eq!(taken, expected);
    let reason = |id: &str| {
        payloads(&detail, "ask_request_taken")
            .into_iter()
            .find(|p| p["request"] == id)
            .unwrap()["reason"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(
        reason("broken").contains("malformed"),
        "{}",
        reason("broken")
    );
    assert!(reason("named").contains("elsewhere"), "{}", reason("named"));
    assert!(
        reason("other").contains("may not ask.open"),
        "{}",
        reason("other")
    );
    // The policy's refusals, as the command's would be.
    let denied = queue_events(&db, "authorization_denied");
    assert_eq!(denied.len(), 2, "{denied:?}");
    for payload in &denied {
        assert_eq!(payload["role"], "worker");
        assert_eq!(payload["capability"], "ask.open");
    }
    assert!(
        denied
            .iter()
            .any(|p| p["resource"]["run"] == "someone-elses-run")
    );
    assert!(
        denied
            .iter()
            .any(|p| p["resource"]["ask_kind"] == "approve_landing")
    );
    let names = request_files(&run);
    assert!(
        names.iter().all(|name| name.ends_with(".taken")),
        "{names:?}"
    );
    assert_eq!(names.len(), 7, "{names:?}");
    assert_eq!(TASK, run.task_id());
}

/// Outside the run: what a link a worker puts in its request directory
/// points at, which the supervisor must neither read nor move (a person's
/// `~/.codex`, another run's directory).
fn victim(dir: &Path) -> PathBuf {
    let victim = dir.join("victim");
    fs::create_dir_all(&victim).unwrap();
    fs::write(victim.join("auth.json"), "keep").unwrap();
    victim
}

fn assert_untouched(victim: &Path) {
    let names: Vec<String> = fs::read_dir(victim)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, ["auth.json"]);
    assert_eq!(
        fs::read_to_string(victim.join("auth.json")).unwrap(),
        "keep"
    );
}

/// Supervise the fixture's Codex task to its landing; the task's detail.
fn landed(
    repo: &Path,
    db: &Path,
    backend: TestWorkspace,
    codex: &Path,
) -> dagq::domain::TaskDetail {
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(
        db,
        repo,
        backend.clone(),
        codex,
        &[verdict("pass", &[], "meets the acceptance")],
    );
    let outcome = finished(&backend, supervisor);
    assert_eq!(
        outcome["runs"].as_array().unwrap().last().unwrap()["status"],
        "integrated",
        "{outcome}"
    );
    detail(db)
}

/// The refusal reason of the request `id`, recorded once.
fn refused(detail: &dagq::domain::TaskDetail, id: &str) -> String {
    let taken: Vec<&Value> = payloads(detail, "ask_request_taken")
        .into_iter()
        .filter(|p| p["request"] == id)
        .collect();
    assert_eq!(taken.len(), 1, "{id}: {taken:?}");
    assert_eq!(taken[0]["outcome"], "refused", "{id}");
    taken[0]["reason"].as_str().unwrap().to_owned()
}

/// The supervisor runs outside the worker's sandbox and must not do for it
/// what the sandbox stops (ADR-t813-3 decisions 3 and 6): a link in place
/// of the request directory is not followed. Nothing where it points is
/// read, renamed or removed; the refusal is on the run once, whatever the
/// passes; and the turn, which left no ask, is nudged and the run lands.
#[test]
fn a_link_in_place_of_the_request_directory_is_not_followed() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    let victim = victim(dir.path());
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) R="$RUN_DIR/ask-requests"; rm -rf "$R"; ln -s '{}' "$R"; say working ;;
*) {FINISH} ;;
esac"#,
            victim.display()
        ),
    );
    let detail = landed(&repo, &db, backend, &codex);
    let run = &detail.runs[0];
    let reason = refused(&detail, &format!("{ASK_REQUESTS_DIR}/"));
    assert!(reason.contains("symbolic link"), "{reason}");
    assert_untouched(&victim);
    let requests = Path::new(run.run_dir().unwrap()).join(ASK_REQUESTS_DIR);
    assert!(fs::symlink_metadata(&requests).unwrap().is_symlink());
    assert!(worker_questions(&db).is_empty());
    assert!(!payloads(&detail, "stall_nudged").is_empty());
}

/// Entries of the request directory that are no regular file (a link out
/// of the run, a link to a directory, a directory, a FIFO) are refused and
/// left as they are, never read through nor renamed. They count as no
/// pending request, so the turn that left only them is read as one without
/// an ask and nudged, and the run lands.
#[test]
fn entries_that_are_no_regular_file_are_refused_untouched_and_hold_nothing() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    let victim = victim(dir.path());
    let v = victim.display();
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) R="$RUN_DIR/ask-requests"; mkdir -p "$R"; ln -s '{v}/auth.json' "$R/leak.json"; ln -s '{v}' "$R/linkdir.json"; mkdir "$R/dir.json"; mkfifo "$R/fifo.json"; say working ;;
*) {FINISH} ;;
esac"#
        ),
    );
    let detail = landed(&repo, &db, backend, &codex);
    let run = &detail.runs[0];
    for (id, what) in [
        ("leak", "symbolic link"),
        ("linkdir", "symbolic link"),
        ("dir", "directory"),
        ("fifo", "special file"),
    ] {
        let reason = refused(&detail, id);
        assert!(
            reason.contains(what) && reason.contains("not a regular file"),
            "{id}: {reason}"
        );
    }
    assert_untouched(&victim);
    assert_eq!(
        request_files(run),
        ["dir.json", "fifo.json", "leak.json", "linkdir.json"]
    );
    let requests = Path::new(run.run_dir().unwrap()).join(ASK_REQUESTS_DIR);
    assert!(
        fs::symlink_metadata(requests.join("leak.json"))
            .unwrap()
            .is_symlink()
    );
    assert!(worker_questions(&db).is_empty());
    assert!(!payloads(&detail, "stall_nudged").is_empty());
}

/// Makes the request directory writable again when dropped, so a test that
/// failed while it was read-only leaves nothing its tempdir cannot remove.
struct Writable(PathBuf);

impl Drop for Writable {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

/// A taken request its supervisor can neither mark taken nor remove (a
/// directory at its taken name, its directory made read-only) holds
/// nothing: the ask opens once, and the turn after the answer, with neither
/// receipt nor ask, is nudged.
#[test]
fn a_taken_request_that_cannot_be_moved_holds_nothing() {
    let (dir, repo, db, backend, codex) = codex_fixture();
    let own = r#""run_id":"'"$DAGQ_RUN_ID"'""#;
    set_turns(
        dir.path(),
        &format!(
            r#"case "$TURN" in
1) R="$RUN_DIR/ask-requests"; mkdir -p "$R/stuck.taken/x"; printf '%s' '{{"id":"stuck","kind":"worker_question","because":"scope","question":"which file","topics":["other"],{own}}}' > "$R/.stuck.tmp"; mv "$R/.stuck.tmp" "$R/stuck.json"; chmod 555 "$R"; say asked ;;
2) say working ;;
*) chmod 755 "$RUN_DIR/ask-requests"; {FINISH} ;;
esac"#
        ),
    );
    let backend = Arc::new(backend);
    let supervisor = supervise_thread(
        &db,
        &repo,
        backend.clone(),
        &codex,
        &[verdict("pass", &[], "meets the acceptance")],
    );
    let ask = open_ask(&db, |ask| ask.kind == AskKind::WorkerQuestion);
    let _writable =
        Writable(Path::new(detail(&db).runs[0].run_dir().unwrap()).join(ASK_REQUESTS_DIR));
    SqliteQueue::open(&db)
        .unwrap()
        .answer(ask.id, "a.txt")
        .unwrap();
    let outcome = finished(&backend, supervisor);
    assert_eq!(
        outcome["runs"].as_array().unwrap().last().unwrap()["status"],
        "integrated",
        "{outcome}"
    );
    let detail = detail(&db);
    let run = &detail.runs[0];
    let calls = stub_calls(run);
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(calls[1].contains("answer to ask"), "{calls:?}");
    assert_eq!(payloads(&detail, "stall_nudged").len(), 1);
    let taken = payloads(&detail, "ask_request_taken");
    assert_eq!(taken.len(), 1, "{taken:?}");
    assert_eq!(taken[0]["outcome"], "opened");
    assert_eq!(worker_questions(&db).len(), 1);
}

/// Claim the fixture's task as a supervisor with every worker would.
fn claimed(repo: &Path, db: &Path) -> dagq::domain::RunId {
    use dagq::{
        domain::{ClaimOutcome, provider_switch::WorkerRoute, worker::Worker},
        infrastructure::adapters::{GitRepository, path_text},
    };
    let repository = GitRepository::inspect(repo).unwrap();
    let mut queue = SqliteQueue::open(db).unwrap();
    queue
        .bind_repository(&path_text(&repository.common_dir).unwrap())
        .unwrap();
    let ClaimOutcome::Claimed { run } = queue
        .claim_for_supervisor_in_order(
            &repository.main_head().unwrap(),
            &dagq::domain::LeaseToken::new("t"),
            &[],
            None,
            &Default::default(),
            &WorkerRoute::direct(&Worker::ALL),
        )
        .unwrap()
    else {
        panic!("no candidate to claim")
    };
    run.id().clone()
}

/// The queue side of the once-only rule: a request opens its ask once in
/// the same transaction that records it taken, so one taken before stays
/// taken after its ask was answered and closed, and is refused no more.
#[test]
fn a_request_is_opened_once_even_after_its_ask_closed() {
    let (_dir, repo, db, _backend, _codex) = codex_fixture();
    let run_id = claimed(&repo, &db);
    let ask = || NewAsk {
        kind: AskKind::WorkerQuestion,
        task_id: None,
        run_id: Some(run_id.clone()),
        question: "which file".to_owned(),
        options: vec![],
        asked_by: "worker".to_owned(),
        reason_category: AskReason::Scope,
        topics: vec!["other".to_owned()],
        finding_id: None,
    };
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert!(!queue.ask_request_taken(&run_id, "r1").unwrap());
    let first = queue.ask_on_request(&run_id, "r1", ask()).unwrap().unwrap();
    assert!(first.created);
    assert!(queue.ask_request_taken(&run_id, "r1").unwrap());
    queue.answer(first.ask.id, "a").unwrap();
    queue.close_ask(first.ask.id).unwrap();
    let again = queue.ask_on_request(&run_id, "r1", ask()).unwrap().unwrap();
    assert!(!again.created);
    assert_eq!(again.ask.id, first.ask.id);
    assert!(!queue.refuse_ask_request(&run_id, "r1", "late").unwrap());
    // A refused request stays refused, and opens nothing later.
    assert!(queue.refuse_ask_request(&run_id, "r2", "broken").unwrap());
    assert!(!queue.refuse_ask_request(&run_id, "r2", "broken").unwrap());
    assert!(
        queue
            .ask_on_request(&run_id, "r2", ask())
            .unwrap()
            .is_none()
    );
    assert_eq!(worker_questions(&db).len(), 1);
    let taken = queue_events(&db, "ask_request_taken");
    assert_eq!(taken.len(), 2, "{taken:?}");
}
