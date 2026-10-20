//! Runtime tests: a send-back or resume of a worker whose previous turn's
//! context was above `[fresh_session] peak_context_above` goes on in a new
//! session in the same run, worktree and branch, with a handoff prompt as
//! its first request (ADR-t2080-1); at or below it, unmeasured or unset,
//! in the same session. Each path has one wiring case of each side here:
//! which peak and threshold renew is `fresh_session::next_session`'s, whose
//! unit tests hold every case, and a path without `[fresh_session]` is the
//! one its own tests (`runtime_review`, `runtime_resume`, `runtime_triage`)
//! run.
use crate::runtime_support;
use dagq::domain::EventKind;
use dagq::domain::turn::session_name;
use dagq::{
    application::{EntryKind, RunFiles},
    runtime::RunFilesPort,
};
use std::io;
use std::time::SystemTime;

use runtime_support::*;

/// A line of a stub `claude` turn whose model call takes `tokens` of
/// input: the turn's `peak_context`.
fn context(tokens: u64) -> String {
    format!(
        r#"printf '{{"type":"assistant","parent_tool_use_id":null,"message":{{"model":"stub","usage":{{"input_tokens":{tokens},"output_tokens":1}},"content":[]}}}}\n'; "#
    )
}

/// The first turn of a new session: the task's prompt of `run`, then the
/// handoff kept in `file` of its run directory.
fn new_session_turn(run: &TaskRun, file: &str) -> String {
    let run_dir = Path::new(run.run_dir().unwrap());
    let task = fs::read_to_string(run_dir.join("prompt.txt")).unwrap();
    let handoff = fs::read_to_string(run_dir.join(file)).unwrap();
    assert!(handoff.starts_with(HANDOFF_OPENING), "{handoff}");
    dagq::domain::turn::new_session_prompt(&task, &handoff)
}

/// The worker of `revising_agent(2)` whose first turn's context took
/// `peak` tokens; its later turns' are not measured.
fn revising_with_context(peak: u64) -> String {
    format!(
        "if [ \"$TURN\" -eq 1 ]; then {}fi; {}",
        context(peak),
        crate::runtime_review::revising_agent(2)
    )
}

/// A revise whose worker's previous turn took more context than the
/// threshold goes to a new session of the same run: `session_renewed`
/// (the turn it follows, why, the provider kept, the new session's name)
/// is recorded before `revise_requested`, and the session wrapper starts
/// the request as the first turn of that session (`start`, not `resume`),
/// the task's prompt before a handoff that carries the commits, the
/// review's findings and the revise request. The revise's wait follows
/// the new session: its receipt and idle end the revise. The new
/// session's own turn is not measured, so the second revise stays in it
/// (`resume` of its name), and the earlier session's large peak renews
/// nothing again. The revise limit and the `/exit` after the pass are as
/// before.
#[test]
fn a_revise_past_the_threshold_goes_on_in_a_new_session_that_the_wait_follows() {
    let (_dir, repo, db) = fixture();
    set_fresh_session_threshold(&repo, 100);
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &revising_with_context(500));
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["add a line to change.txt"], "one gap"),
        verdict("revise", &["add another line"], "another gap"),
        verdict("pass", &[], "fixed"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    assert_landed(&repo, &run, "test task", &base);
    assert_eq!(
        fs::read_to_string(repo.join("change.txt")).unwrap(),
        format!("change by {}\nfix 1\nfix 2\n", run.id())
    );
    let name = session_name(run.id().as_str(), 1);
    assert_eq!(
        payloads(&detail, "session_renewed"),
        [
            &json!({"after_turn": 1, "peak_context": 500, "threshold": 100,
                 "provider": "claude", "session": name, "count": 1})
        ]
    );
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "review_finished") < position(&kinds, "session_renewed"));
    assert!(position(&kinds, "session_renewed") < position(&kinds, "revise_requested"));
    // The first revise in a new session, the second in the same one.
    let calls = stub_calls(&run);
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(
        calls[0].starts_with(&format!("start {} ", run.id())),
        "{calls:?}"
    );
    assert!(calls[1].starts_with(&format!("start {name} ")), "{calls:?}");
    assert!(
        calls[2].starts_with(&format!("resume {name} ")),
        "{calls:?}"
    );
    let texts = session_texts(&run);
    assert_eq!(texts[0], new_session_turn(&run, "revise-1.txt"));
    for expected in [
        format!(
            "{HANDOFF_OPENING}: its previous turn's context took 500 tokens, above the 100 tokens"
        ),
        format!(
            "Commits since the base {}, newest first:\n",
            run.base_commit()
        ),
        "seconds of this request (the resume timeout)".to_owned(),
        "as the project's scripts recorded them in the directory `$DAGQ_LOCAL_CHECKS_DIR` names"
            .to_owned(),
        "The earlier session would have been sent this next; it is yours now:".to_owned(),
        format!(
            "dagq: the supervisor's review of run {} (task 1) asks for changes (revise 1 of 2 of the run).",
            run.id()
        ),
    ] {
        assert!(
            texts[0].contains(&expected),
            "{expected:?} not in {}",
            texts[0]
        );
    }
    // The review's findings once, in the revise request.
    assert_eq!(
        texts[0].matches("add a line to change.txt").count(),
        1,
        "{}",
        texts[0]
    );
    assert!(
        texts[1].starts_with("dagq: this is the next turn of your session")
            && texts[1].contains(&format!(
                "dagq: the supervisor's review of run {} (task 1) asks for changes (revise 2 of 2 of the run).",
                run.id()
            ))
            && !texts[1].contains(HANDOFF_OPENING),
        "{}",
        texts[1]
    );
    let finished = payloads(&detail, "turn_finished");
    assert_eq!(finished[0]["peak_context"], 500);
    assert_eq!(finished[1]["session_id"], json!(name));
    // Each revise ended on what its own session committed after the
    // request, not on the receipt or idle of the session before it.
    let landed = format!("refs/dagq/runs/{}", run.id());
    let revised = payloads(&detail, "revise_finished");
    assert_eq!(revised.len(), 2);
    assert_eq!(
        revised[0]["head"],
        git_out(&repo, &["rev-parse", &format!("{landed}~1")])
    );
    assert_eq!(revised[1]["head"], git_out(&repo, &["rev-parse", &landed]));
    assert_exit_sent(&backend, &run, 1);
}

/// A revise whose worker's previous turn took context at the threshold
/// goes to the session the worker has, as before: no `session_renewed`,
/// and the request is a resume of the run's session with no handoff.
#[test]
fn a_revise_at_the_threshold_stays_in_the_session() {
    let (_dir, repo, db) = fixture();
    set_fresh_session_threshold(&repo, 500);
    let backend = TestWorkspace::new(&db, false, &revising_with_context(500));
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["add a line to change.txt"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let run = detail.runs[0].clone();
    assert_eq!(run.status(), RunStatus::Integrated);
    assert!(payloads(&detail, "session_renewed").is_empty());
    assert_eq!(payloads(&detail, "turn_finished")[0]["peak_context"], 500);
    let calls = stub_calls(&run);
    assert!(
        calls[1].starts_with(&format!("resume {} ", run.id())),
        "{calls:?}"
    );
    let text = &session_texts(&run)[0];
    assert!(
        text.starts_with("dagq: this is the next turn of your session")
            && text.contains("dagq: the supervisor's review of run")
            && !text.contains(HANDOFF_OPENING),
        "{text}"
    );
}

/// Park task 2's run for a resume with `dagq.toml` setting `threshold`,
/// its previous turn's context recorded as `peak` tokens: the resumed
/// session resolves the conflict on its first turn.
fn parked_with_context(
    repo: &Path,
    db: &Path,
    threshold: Option<u64>,
    peak: u64,
) -> (TestWorkspace, TaskRun, String) {
    let backend = TestWorkspace::new(db, false, VALID_AGENT);
    let (run, _) = parked_conflict(repo, db, &backend);
    if let Some(threshold) = threshold {
        set_fresh_session_threshold(repo, threshold);
    }
    let main = git_out(repo, &["rev-parse", "main"]);
    record_previous_peak(db, &run, peak);
    backend.resume_script_for(
        2,
        "await_message; resolve; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    (backend, run, main)
}

/// A `needs_session` resume whose worker's previous turn took more
/// context than the threshold opens a new session rather than reopening
/// the run's: `session_renewed` is recorded after `resume_started` and
/// before the resumed wrapper starts, whose first turn starts that
/// session with the task's prompt and a handoff carrying the resolution
/// request (`resume-1.txt` holds it, sent once). The run lands.
#[test]
fn a_resume_past_the_threshold_opens_a_new_session() {
    let (_dir, repo, db) = fixture();
    let (backend, run, main) = parked_with_context(&repo, &db, Some(100), 500);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    let landed = detail.runs[0].clone();
    assert_landed(&repo, &landed, "second", &main);
    let name = session_name(run.id().as_str(), 1);
    let renewed = payloads(&detail, "session_renewed");
    assert_eq!(renewed.len(), 1, "{:?}", event_kinds(&detail));
    assert_eq!(
        (
            &renewed[0]["peak_context"],
            &renewed[0]["threshold"],
            &renewed[0]["session"]
        ),
        (&json!(500), &json!(100), &json!(name))
    );
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "resume_started") < position(&kinds, "session_renewed"));
    let resumed = kinds
        .iter()
        .rposition(|k| *k == "workspace_created")
        .unwrap();
    assert!(position(&kinds, "session_renewed") < resumed, "{kinds:?}");
    assert_eq!(payloads(&detail, "resume_request_sent").len(), 1);
    let calls = stub_calls(&landed);
    assert!(
        calls.last().unwrap().starts_with(&format!("start {name} ")),
        "{calls:?}"
    );
    let text = session_texts(&landed).pop().unwrap();
    assert_eq!(text, new_session_turn(&landed, "resume-1.txt"));
    assert!(text.contains("The earlier session would have been sent this next"));
    assert!(text.contains(&format!("main is now {main} ")), "{text}");
    // The claim's session and the one the resume opened are the run's
    // telemetry target's (ADR-t1662-3 decision 10).
    for session in [run.id().as_str(), name.as_str()] {
        assert_eq!(
            crate::common::service::session_owner(&db, session),
            Some(format!("run:{}", run.id()))
        );
    }
}

/// A `needs_session` resume whose worker's previous turn took context at
/// the threshold reopens the run's session, as before: no
/// `session_renewed`, the resumed wrapper resumes the run's session, and
/// its request is the resolution request with no handoff.
#[test]
fn a_resume_at_the_threshold_reopens_the_runs_session() {
    let (_dir, repo, db) = fixture();
    let (backend, run, main) = parked_with_context(&repo, &db, Some(500), 500);
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    let landed = detail.runs[0].clone();
    assert_landed(&repo, &landed, "second", &main);
    assert!(payloads(&detail, "session_renewed").is_empty());
    let calls = stub_calls(&landed);
    assert!(
        calls
            .last()
            .unwrap()
            .starts_with(&format!("resume {} ", run.id())),
        "{calls:?}"
    );
    let run_dir = Path::new(landed.run_dir().unwrap());
    let request = fs::read_to_string(run_dir.join("resume-1.txt")).unwrap();
    assert!(!request.contains(HANDOFF_OPENING), "{request}");
    let text = session_texts(&landed).pop().unwrap();
    assert!(text.ends_with(&request), "{text}");
    assert!(text.contains(&format!("main is now {main} ")), "{text}");
}

/// A new session recorded and asked nothing yet (here a supervisor
/// recorded it and could not send its request) is the one the next resume
/// opens, though the repository sets no threshold any more: its handoff
/// goes there, and no second `session_renewed` is recorded.
#[test]
fn a_new_session_recorded_without_its_request_is_the_one_the_resume_opens() {
    let (_dir, repo, db) = fixture();
    let (backend, run, main) = parked_with_context(&repo, &db, None, 500);
    let events = SqliteQueue::open(&db)
        .unwrap()
        .run_events(run.id())
        .unwrap();
    let recorded = dagq::domain::fresh_session::renewed_payload(
        run.id().as_str(),
        run.actual_provider(),
        &events,
        (500, 100),
    );
    SqliteQueue::open(&db)
        .unwrap()
        .record_runtime_event(run.id(), EventKind::SessionRenewed, recorded.clone())
        .unwrap();
    let outcome = supervise(&db, &repo, &backend).unwrap();
    backend.join();
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(2))
        .unwrap();
    let landed = detail.runs[0].clone();
    assert_landed(&repo, &landed, "second", &main);
    assert_eq!(payloads(&detail, "session_renewed"), [&recorded]);
    let name = session_name(run.id().as_str(), 1);
    let calls = stub_calls(&landed);
    assert!(
        calls.last().unwrap().starts_with(&format!("start {name} ")),
        "{calls:?}"
    );
    let text = session_texts(&landed).pop().unwrap();
    assert_eq!(text, new_session_turn(&landed, "resume-1.txt"));
    assert!(
        text.contains(&format!(
            "{HANDOFF_OPENING}: its previous turn's context took 500 tokens, above the 100 tokens"
        )),
        "{text}"
    );
}

/// Run task 1 with `[fresh_session]` at 100 tokens: its worker's first
/// turn runs `script`, commits and fails, the recovery job resumes it, and
/// the resumed session writes the receipt and the run lands. The fixture,
/// kept for the run's files, the run's detail and the run.
fn triage_resumed(script: &str) -> (Fixture, dagq::domain::TaskDetail, TaskRun) {
    let (dir, repo, db) = fixture();
    set_fresh_session_threshold(&repo, 100);
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(&db, false, &format!("{script}commit work; exit 7"));
    backend.resume_script_for(
        1,
        "await_message; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let reviewer =
        TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]).with_triages(&[repair(
            json!({"action": "resume", "instruction": "write the receipt for your commit"}),
            "the work is committed but the receipt is missing",
        )]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    let run = detail.runs[0].clone();
    assert_landed_run(&run, &repo, &base);
    let text = session_texts(&run).pop().unwrap();
    for expected in [
        "the supervisor's triage sent it back to this session to finish",
        "Reason: write the receipt for your commit",
    ] {
        assert!(text.contains(expected), "{expected:?} in {text}");
    }
    (dir, detail, run)
}

/// A triage's `resume` of a failed run whose worker's last turn took more
/// context than the threshold resumes it in a new session: the handoff
/// carries the job's instruction as the resolution request's reason, and
/// the run lands.
#[test]
fn a_triage_resume_past_the_threshold_goes_on_in_a_new_session() {
    let (_dir, detail, run) = triage_resumed(&context(500));
    let kinds = event_kinds(&detail);
    assert!(position(&kinds, "triage_finished") < position(&kinds, "session_renewed"));
    let renewed = payloads(&detail, "session_renewed");
    assert_eq!(renewed.len(), 1, "{kinds:?}");
    assert_eq!(renewed[0]["peak_context"], 500);
    let name = session_name(run.id().as_str(), 1);
    let calls = stub_calls(&run);
    assert!(
        calls.last().unwrap().starts_with(&format!("start {name} ")),
        "{calls:?}"
    );
    let text = session_texts(&run).pop().unwrap();
    assert_eq!(text, new_session_turn(&run, "resume-1.txt"));
}

/// A triage's `resume` of a failed run whose worker's last turn's context
/// was not measured resumes the run's own session, though the repository
/// sets a threshold: no `session_renewed`, and the request carries no
/// handoff.
#[test]
fn a_triage_resume_after_an_unmeasured_turn_stays_in_the_session() {
    let (_dir, detail, run) = triage_resumed("");
    assert_eq!(
        payloads(&detail, "turn_finished")[0]["peak_context"],
        Value::Null
    );
    assert!(payloads(&detail, "session_renewed").is_empty());
    let calls = stub_calls(&run);
    assert!(
        calls
            .last()
            .unwrap()
            .starts_with(&format!("resume {} ", run.id())),
        "{calls:?}"
    );
    let text = session_texts(&run).pop().unwrap();
    assert!(!text.contains(HANDOFF_OPENING), "{text}");
}

/// Run files whose first write of a request carrying a handoff into a
/// session's `turns/` fails, as a full disk would: the supervisor recorded
/// the new session and could not write its first request.
#[derive(Default)]
struct HandoffUnwritten {
    failed: Mutex<bool>,
}

impl RunFiles for HandoffUnwritten {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.create_dir_all(dir)
    }
    fn create_new_dir(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.create_new_dir(dir)
    }
    fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        let request = path.parent().is_some_and(|dir| dir.ends_with("turns"))
            && String::from_utf8_lossy(contents).contains(HANDOFF_OPENING);
        let mut failed = self.failed.lock().unwrap();
        if request && !*failed {
            *failed = true;
            return Err(io::Error::other("no space left on device"));
        }
        LocalRunFiles.write(path, contents)
    }
    fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        LocalRunFiles.copy(from, to)
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        LocalRunFiles.read(path)
    }
    fn read_from(&self, path: &Path, offset: u64) -> io::Result<Vec<u8>> {
        LocalRunFiles.read_from(path, offset)
    }
    fn read_range(&self, path: &Path, offset: u64, len: u64) -> io::Result<Vec<u8>> {
        LocalRunFiles.read_range(path, offset, len)
    }
    fn read_tail(&self, path: &Path, bytes: u64) -> io::Result<Vec<u8>> {
        LocalRunFiles.read_tail(path, bytes)
    }
    fn size(&self, path: &Path) -> io::Result<u64> {
        LocalRunFiles.size(path)
    }
    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        LocalRunFiles.read_to_string(path)
    }
    fn try_lock(&self, path: &Path) -> io::Result<Option<Box<dyn std::any::Any + Send>>> {
        LocalRunFiles.try_lock(path)
    }
    fn try_lock_shared(&self, path: &Path) -> io::Result<Option<Box<dyn std::any::Any + Send>>> {
        LocalRunFiles.try_lock_shared(path)
    }
    fn lock(&self, path: &Path) -> io::Result<Box<dyn std::any::Any + Send>> {
        LocalRunFiles.lock(path)
    }
    fn modified(&self, path: &Path) -> io::Result<SystemTime> {
        LocalRunFiles.modified(path)
    }
    fn read_stamped(&self, path: &Path) -> Result<Option<(SystemTime, Vec<u8>)>> {
        LocalRunFiles.read_stamped(path)
    }
    fn is_file(&self, path: &Path) -> bool {
        LocalRunFiles.is_file(path)
    }
    fn is_dir(&self, path: &Path) -> bool {
        LocalRunFiles.is_dir(path)
    }
    fn executable(&self, path: &Path) -> io::Result<bool> {
        LocalRunFiles.executable(path)
    }
    fn set_executable(&self, path: &Path, executable: bool) -> io::Result<()> {
        LocalRunFiles.set_executable(path, executable)
    }
    fn entry_kind(&self, path: &Path) -> io::Result<EntryKind> {
        LocalRunFiles.entry_kind(path)
    }
    fn exists(&self, path: &Path) -> bool {
        LocalRunFiles.exists(path)
    }
    fn read_dir(&self, dir: &Path) -> io::Result<Vec<PathBuf>> {
        LocalRunFiles.read_dir(dir)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        LocalRunFiles.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        LocalRunFiles.remove_file(path)
    }
    fn tree_size(&self, dir: &Path) -> io::Result<Option<u64>> {
        LocalRunFiles.tree_size(dir)
    }
    fn remove_dir_all(&self, dir: &Path) -> io::Result<()> {
        LocalRunFiles.remove_dir_all(dir)
    }
    fn append_line(&self, path: &Path, line: &str) -> io::Result<()> {
        LocalRunFiles.append_line(path, line)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        LocalRunFiles.canonicalize(path)
    }
    fn write_fenced(&self, path: &Path, text: &str, info: &str, body: &Path) -> Result<()> {
        LocalRunFiles.write_fenced(path, text, info, body)
    }
    fn now(&self) -> SystemTime {
        LocalRunFiles.now()
    }
}

/// A revise past the threshold whose handoff cannot be written into
/// `turns/` leaves `session_renewed` with no request after it: the revise
/// is withdrawn (`revise_unsent`), the earlier session exits without a
/// turn, and a person is asked. The person's `send_back` resumes the run in
/// the session recorded, though that session has finished no turn, so
/// judging the peak alone would keep it: no second `session_renewed`, and
/// the resumed wrapper's first turn starts that session with the task's
/// prompt and the handoff, sent once. No turn of either session ran a
/// request without the handoff in between.
#[test]
fn a_new_session_whose_handoff_could_not_be_written_starts_with_the_next_handoff() {
    let (_dir, repo, db) = fixture();
    set_fresh_session_threshold(&repo, 100);
    let base = git_out(&repo, &["rev-parse", "main"]);
    let backend = TestWorkspace::new(
        &db,
        false,
        &format!(
            "if [ \"$TURN\" -eq 1 ]; then {}fi; {IDLE_AGENT}",
            context(500)
        ),
    );
    let reviewer = TestReviewer::new(&[
        verdict("revise", &["add a line to change.txt"], "one gap"),
        verdict("pass", &[], "fixed"),
    ]);
    let options = SuperviseOptions {
        files: Some(RunFilesPort(Arc::new(HandoffUnwritten::default()))),
        ..supervise_options(4, true)
    };
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let detail = queue.show(TaskId::new(1)).unwrap();
    let run = detail.runs[0].clone();
    let name = session_name(run.id().as_str(), 1);
    let recorded = json!({"after_turn": 1, "peak_context": 500, "threshold": 100,
                          "provider": "claude", "session": name, "count": 1});
    assert_eq!(payloads(&detail, "session_renewed"), [&recorded]);
    let unsent = payloads(&detail, "revise_unsent");
    assert_eq!(unsent.len(), 1, "{unsent:?}");
    assert!(
        unsent[0]["error"]
            .as_str()
            .unwrap()
            .contains("no space left on device"),
        "{unsent:?}"
    );
    assert!(payloads(&detail, "turn_requested").is_empty());
    assert_eq!(stub_calls(&run).len(), 1);
    assert_exit_sent(&backend, &run, 1);
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let ask = asks[0].clone();
    assert_eq!(ask.kind, AskKind::ApproveLanding);

    queue.answer(ask.id, "send_back").unwrap();
    backend.resume_script_for(
        1,
        "await_message; printf 'fixed\\n' >> change.txt; unlocked git commit -q -am fixed; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    let landed = detail.runs[0].clone();
    assert_landed_run(&landed, &repo, &base);
    assert_eq!(payloads(&detail, "session_renewed"), [&recorded]);
    let requested = payloads(&detail, "turn_requested");
    assert_eq!(requested.len(), 1, "{requested:?}");
    assert_eq!(requested[0]["what"], "resolution request");
    let calls = stub_calls(&landed);
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(
        calls[0].starts_with(&format!("start {} ", run.id())),
        "{calls:?}"
    );
    assert!(calls[1].starts_with(&format!("start {name} ")), "{calls:?}");
    let texts = session_texts(&landed);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert_eq!(texts[0], new_session_turn(&landed, "resume-1.txt"));
    assert!(
        texts[0].contains(
            "dagq: this run's worker goes on in a new session: its previous turn's context took 500 tokens, above the 100 tokens"
        ),
        "{}",
        texts[0]
    );
    assert!(texts[0].contains("The earlier session would have been sent this next"));
}
