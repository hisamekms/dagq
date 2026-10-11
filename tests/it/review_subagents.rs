//! Runtime tests: the review's agents (ADR-t1453-1, ADR-t1895-1). The
//! supervisor reads `[review.subagents.<agent>]` and the agents'
//! definitions from the landing branch's commit, never from the run's
//! worktree or the main checkout's files, selects the agents the reviewed
//! diff requires, and runs the whole review and each selected agent as
//! jobs of their own side by side; a review whose agents cannot be known,
//! or whose jobs do not all answer, does not pass, and the heaviest of the
//! jobs' judgments decides where the run goes. A repository without the
//! table reviews with the whole review's job alone.
use crate::runtime_support;

use runtime_support::*;
use sha2::{Digest, Sha256};

/// What the main checkout commits: `design` reviews `change.txt` (the
/// stand-in worker's change); `unused` matches nothing and has no
/// definition, which a review that does not select it never needs.
const CONFIG: &str = "[review.subagents.design]\npaths = [\"change.txt\", \"docs/**\"]\n\
                      [review.subagents.unused]\npaths = [\"nothing/**\"]\n";
const DEFINITION: &str = ".dagq/agents/design/AGENT.md";
const MAIN_DEFINITION: &str = "---\ndescription: main's design checks\n---\nCheck the design.\n";

/// The whole review's pass, and `design`'s.
fn design_passes() -> (String, Vec<(&'static str, Vec<String>)>) {
    (
        verdict("pass", &[], "meets the acceptance"),
        vec![("design", vec![agent_says("design", "pass", &[])])],
    )
}

/// Commit `config` as `dagq.toml` and, when given, the definition of
/// `design` on main; main's new head.
fn commit_on_main(repo: &Path, config: &str, definition: Option<&str>) -> String {
    fs::write(repo.join("dagq.toml"), config).unwrap();
    git(repo, &["add", "dagq.toml"]);
    if let Some(definition) = definition {
        fs::create_dir_all(repo.join(".dagq/agents/design")).unwrap();
        fs::write(repo.join(DEFINITION), definition).unwrap();
        git(repo, &["add", DEFINITION]);
    }
    git(repo, &["commit", "-q", "-m", "review subagents"]);
    git_out(repo, &["rev-parse", "main"])
}

/// Review the first run, whose worker runs `worker`, with the whole
/// review's `verdicts` and the agents' scripts `agents`.
fn reviewed(
    repo: &Path,
    db: &Path,
    worker: &str,
    (verdicts, agents): (&[String], &[(&str, Vec<String>)]),
) -> (TestReviewer, dagq::domain::TaskDetail) {
    let backend = TestWorkspace::new(db, false, worker);
    let reviewer = TestReviewer::new(verdicts).with_review_agents(agents);
    let outcome = supervise_reviewed(db, repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(db).unwrap().show(TaskId::new(1)).unwrap();
    (reviewer, detail)
}

/// [`reviewed`] with [`design_passes`].
fn reviewed_passing(
    repo: &Path,
    db: &Path,
    worker: &str,
) -> (TestReviewer, dagq::domain::TaskDetail) {
    let (whole, agents) = design_passes();
    reviewed(repo, db, worker, (&[whole], &agents))
}

/// The snapshot `review_started` records for main's commit `main`: the
/// run's range (its base, which is `main` here, to its head), and only
/// `design`, selected by `change.txt`, with main's definition's digest.
fn expected_snapshot(main: &str, detail: &dagq::domain::TaskDetail) -> Value {
    json!({
        "commit": main,
        "base": detail.runs[0].base_commit().as_str(),
        "head": reviewed_head(detail),
        "agents": [{
            "agent": "design",
            "paths": ["change.txt"],
            "definition": DEFINITION,
            "digest": format!("{:x}", Sha256::digest(MAIN_DEFINITION.as_bytes())),
        }],
    })
}

/// The commit the review read: the receipt's, not the landed one.
fn reviewed_head(detail: &dagq::domain::TaskDetail) -> String {
    let receipt: Value =
        serde_json::from_str(&fs::read_to_string(detail.runs[0].receipt_path().unwrap()).unwrap())
            .unwrap();
    receipt["commit"].as_str().unwrap().to_owned()
}

/// What the agent job of `design` was given: one job, whose prompt
/// carries main's definition as the snapshot returned it, whole, and no
/// path of a definition, beside the material whose range selected it;
/// the whole review's prompt says nothing of agents.
fn assert_job_input(reviewer: &TestReviewer, detail: &dagq::domain::TaskDetail, main: &str) {
    let run_dir = detail.runs[0].run_dir().unwrap();
    let launches = reviewer.launches();
    assert_eq!(launches.len(), 1, "{launches:?}");
    let prompt = &launches[0].prompt;
    for part in [
        "You are the review agent `design`".to_owned(),
        format!("<definition>\n{MAIN_DEFINITION}\n</definition>"),
        format!("{run_dir}/review.md"),
    ] {
        assert!(prompt.contains(&part), "{part} in {prompt}");
    }
    assert!(!prompt.contains(DEFINITION), "{prompt}");
    assert!(!prompt.contains(".dagq/review-agents"), "{prompt}");
    assert_eq!(
        fs::read_to_string(format!("{run_dir}/review-1-agents/design/prompt.txt")).unwrap(),
        *prompt
    );
    assert_eq!(
        launches[0].material,
        PathBuf::from(format!("{run_dir}/review.md"))
    );
    // review.md shows the range the agents were selected from.
    let expected = expected_snapshot(main, detail);
    let material = fs::read_to_string(format!("{run_dir}/review.md")).unwrap();
    let base = format!("- base: {}", expected["base"].as_str().unwrap());
    assert!(material.contains(&base), "{base} in {material}");
    let whole = &reviewer.prompts()[0];
    assert!(!whole.contains("subagent"), "{whole}");
    assert!(!Path::new(&format!("{run_dir}/review-subagents-1.json")).exists());
}

/// (a) A worker that rewrites or deletes `dagq.toml` and the definition
/// on its branch neither drops nor changes its required review: main's
/// commit decides, and the job reads main's definition (the Git boundary of ADR-t1453-1
/// decision 4; what the commit's files select is
/// `application::review::tests`).
#[test]
fn the_worker_cannot_change_or_drop_its_required_review() {
    for tamper in [
        // Narrow the agent's paths away and rewrite its definition.
        "printf '[review.subagents.design]\\npaths = [\"nothing/**\"]\\n' > dagq.toml; \
         printf 'the worker s checks\\n' > .dagq/agents/design/AGENT.md; \
         git commit -q -am tamper",
        // Delete the table and the definition.
        "printf '[run.env]\\n' > dagq.toml; git rm -q .dagq/agents/design/AGENT.md; \
         git commit -q -am tamper",
        // Delete dagq.toml itself and the definition.
        "git rm -q dagq.toml .dagq/agents/design/AGENT.md; git commit -q -m tamper",
    ] {
        let (_dir, repo, db) = fixture();
        let main = commit_on_main(&repo, CONFIG, Some(MAIN_DEFINITION));
        let worker =
            format!("{tamper}; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit");
        let (reviewer, detail) = reviewed_passing(&repo, &db, &worker);
        assert_eq!(detail.runs[0].status(), RunStatus::Integrated, "{tamper}");
        // The reviewed head has what the worker left of them, if anything.
        let head = reviewed_head(&detail);
        let left = git_out(
            &repo,
            &[
                "ls-tree",
                "--name-only",
                "-r",
                &head,
                "--",
                "dagq.toml",
                DEFINITION,
            ],
        );
        if tamper.starts_with("git rm") {
            assert_eq!(left.trim(), "", "{tamper}");
        } else if tamper.starts_with("printf '[run.env]") {
            assert_eq!(left.trim(), "dagq.toml", "{tamper}");
        }
        let started = payloads(&detail, "review_started");
        assert_eq!(started.len(), 1);
        assert_eq!(
            started[0]["subagents"],
            expected_snapshot(&main, &detail),
            "{tamper}"
        );
        assert_job_input(&reviewer, &detail, &main);
    }
}

/// (b) Uncommitted edits of the main checkout's `dagq.toml` and
/// definition are not used: the uncommitted agent `all` is not selected
/// (its definition is nowhere, which would fail the review), and the
/// committed definition is the one handed on. Its own fixture: the dirty
/// main checkout would stop a landing (the review's verdict keeps the run
/// from landing).
#[test]
fn uncommitted_main_checkout_edits_are_not_used() {
    let (_dir, repo, db) = fixture();
    let main = commit_on_main(&repo, CONFIG, Some(MAIN_DEFINITION));
    fs::write(
        repo.join("dagq.toml"),
        format!("{CONFIG}[review.subagents.all]\npaths = [\"**\"]\n"),
    )
    .unwrap();
    fs::write(repo.join(DEFINITION), "uncommitted checks\n").unwrap();
    let (reviewer, detail) = reviewed(
        &repo,
        &db,
        IDLE_AGENT,
        (
            &[verdict("concern", &["a finding"], "look")],
            &[("design", vec![agent_says("design", "pass", &[])])],
        ),
    );
    let started = payloads(&detail, "review_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["subagents"], expected_snapshot(&main, &detail));
    assert_job_input(&reviewer, &detail, &main);
    assert!(payloads(&detail, "review_failed").is_empty());
}

/// The reviewed range is Git's, without rename detection: the old path of
/// a rename out of `docs/` and a deleted path under it, in one commit,
/// both select `design`.
#[test]
fn the_old_path_of_a_rename_and_a_deleted_path_select_an_agent() {
    let (_dir, repo, db) = fixture();
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(repo.join("docs/old.md"), "old\n").unwrap();
    fs::write(repo.join("docs/gone.md"), "gone\n").unwrap();
    git(&repo, &["add", "docs/old.md", "docs/gone.md"]);
    let main = commit_on_main(&repo, CONFIG, Some(MAIN_DEFINITION));
    let worker = "git mv docs/old.md moved.md; git rm -q docs/gone.md; git commit -q -m change; \
                  receipt \"$(git rev-parse HEAD)\"; idle; await_exit";
    let (_reviewer, detail) = reviewed(
        &repo,
        &db,
        worker,
        (
            &[verdict("concern", &["a finding"], "look")],
            &[("design", vec![agent_says("design", "pass", &[])])],
        ),
    );
    let started = payloads(&detail, "review_started");
    assert_eq!(started.len(), 1);
    let subagents = &started[0]["subagents"];
    assert_eq!(subagents["commit"], main.as_str());
    assert_eq!(subagents["agents"][0]["agent"], "design");
    assert_eq!(
        subagents["agents"][0]["paths"],
        json!(["docs/gone.md", "docs/old.md"])
    );
}

/// (c) An agent the diff selects whose definition is not in main's
/// commit fails the review to the person with why: no job runs, the run
/// does not land, and `review_failed` and the ask say which definition.
/// The wiring of a review that cannot start to `review_failed` and the
/// ask; why it cannot (an unparsable config, a missing definition, no
/// provider that runs the agents) is `application::review::tests` and
/// `supervise::landing::tests`. `dagq doctor`'s `agents` reports the
/// same configuration's agents without a definition in `errors`
/// (ADR-t1728-1; the judgment is `domain::review_subagents::tests`).
#[test]
fn a_selected_agent_without_its_definition_fails_the_review() {
    let (_dir, repo, db) = fixture();
    let main = commit_on_main(&repo, CONFIG, None);
    let (reviewer, detail) = reviewed(
        &repo,
        &db,
        IDLE_AGENT,
        (&[verdict("pass", &[], "would pass")], &[]),
    );
    assert!(reviewer.prompts().is_empty());
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(payloads(&detail, "review_started").is_empty());
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1);
    let why = format!(
        "the review's required subagents could not be read: the review subagent design that dagq.toml names has no definition {DEFINITION} in the landing branch's commit {main}"
    );
    assert!(
        failed[0]["error"].as_str().unwrap().contains(&why),
        "{failed:?}"
    );
    let asks = SqliteQueue::open(&db)
        .unwrap()
        .asks(Default::default())
        .unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert!(asks[0].question.contains(&why), "{}", asks[0].question);
    // `doctor` reports the agent without a definition as a mistake of the
    // configuration (ADR-t1728-1); its judgment is
    // `domain::review_subagents::tests`.
    let doctor = crate::common::cli::ok(&db, &["doctor"]);
    assert_eq!(doctor["agents"]["commit"], main.as_str(), "{doctor}");
    assert_eq!(
        doctor["agents"]["errors"],
        json!([
            "[review.subagents.design] names an agent without a definition: .dagq/agents/design/AGENT.md is not committed",
            "[review.subagents.unused] names an agent without a definition: .dagq/agents/unused/AGENT.md is not committed",
        ]),
        "{doctor}"
    );
}

/// An agent the diff selects whose definition declares a tool beyond a
/// review's reads (ADR-t1728-2) fails the review as a missing definition
/// does: no job runs, the run does not land, and `review_failed` says
/// why; `dagq doctor`'s `agents` reports the same declaration in
/// `errors`. Which declarations are mistakes is
/// `domain::review_subagents::tests`.
#[test]
fn a_definition_declaring_a_tool_beyond_the_reads_fails_the_review() {
    let (_dir, repo, db) = fixture();
    let main = commit_on_main(
        &repo,
        CONFIG,
        Some("---\ndescription: d\ntools: [read, shell]\n---\nCheck.\n"),
    );
    let (reviewer, detail) = reviewed(
        &repo,
        &db,
        IDLE_AGENT,
        (&[verdict("pass", &[], "would pass")], &[]),
    );
    assert!(reviewer.prompts().is_empty());
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(payloads(&detail, "review_started").is_empty());
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1);
    let why = "the frontmatter's tools declares shell is beyond what an agent of [review.subagents] may use (read, grep, glob)";
    let error = failed[0]["error"].as_str().unwrap();
    assert!(
        error.contains(&format!(
            "the review subagent design's definition {DEFINITION} in the landing branch's commit {main} is a mistake: {why}"
        )),
        "{failed:?}"
    );
    let doctor = crate::common::cli::ok(&db, &["doctor"]);
    assert!(
        doctor["agents"]["errors"]
            .as_array()
            .unwrap()
            .contains(&json!(format!(
                "[review.subagents.design] {DEFINITION}: {why}"
            ))),
        "{doctor}"
    );
}

/// (d) With `[review.subagents]` but a diff it does not select, the
/// review is as before: the event records the empty selection, the whole
/// review's job alone runs, and no word of subagents is in the prompt or
/// the material (the supervisor's wiring;
/// without the table, `application::review::tests::
/// a_review_without_required_agents_reads_as_before`).
#[test]
fn a_review_without_required_agents_is_unchanged() {
    let (_dir, repo, db) = fixture();
    let main = commit_on_main(&repo, CONFIG, Some(MAIN_DEFINITION));
    // A worker whose change no glob matches.
    let worker = "printf 'x\\n' > other.txt; git add other.txt; git commit -q -m other; \
                  receipt \"$(git rev-parse HEAD)\"; idle; await_exit";
    let (reviewer, detail) = reviewed(
        &repo,
        &db,
        worker,
        (&[verdict("pass", &[], "meets the acceptance")], &[]),
    );
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let started = payloads(&detail, "review_started");
    assert_eq!(started.len(), 1);
    assert_eq!(
        started[0]["subagents"],
        json!({
            "commit": main,
            "base": detail.runs[0].base_commit().as_str(),
            "head": reviewed_head(&detail),
            "agents": [],
        })
    );
    let run_dir = detail.runs[0].run_dir().unwrap();
    assert!(!Path::new(&format!("{run_dir}/review-subagents-1.json")).exists());
    let prompt = &reviewer.prompts()[0];
    assert!(!prompt.contains("subagent"), "{prompt}");
    // No agent's job runs, and nothing of agents reaches review_finished.
    assert!(reviewer.launches().is_empty());
    assert_eq!(payloads(&detail, "review_job_started").len(), 1);
    let finished = payloads(&detail, "review_finished");
    assert!(finished[0].get("agents").is_none() && finished[0].get("route").is_none());
    let material = fs::read_to_string(format!("{run_dir}/review.md")).unwrap();
    assert!(!material.contains("subagent"), "{material}");
}

/// The attempt's range is fixed once, with the landing branch's commit
/// the agents are read from, and review.md shows that range: a session
/// rebased the run onto main B, which touched `late.txt`; the agents are
/// selected from B...head (only `design`), then main moves to C, which is
/// not an ancestor of the head. Found afresh, the material's range would
/// fall back to the run's base A and show `late.txt`, which `late` must
/// review but was not selected for.
#[test]
fn the_material_shows_the_range_its_agents_were_selected_from() {
    use dagq::application::review::{review_range_at, snapshot_subagents};
    use dagq::infrastructure::adapters::GitRepository;
    let (dir, repo, db) = fixture();
    let config = format!("{CONFIG}[review.subagents.late]\npaths = [\"late.txt\"]\n");
    let a = commit_on_main(&repo, &config, Some(MAIN_DEFINITION));
    let backend = TestWorkspace::new(&db, false, VALID_AGENT);
    supervise(&db, &repo, &backend).unwrap();
    backend.join();
    let mut queue = SqliteQueue::open(&db).unwrap();
    let run = queue.show(TaskId::new(1)).unwrap().runs[0].clone();
    assert_eq!(run.status(), RunStatus::AwaitingIntegration);
    assert_eq!(run.base_commit().as_str(), a);
    let head = run.result_commit().unwrap().to_string();
    // Main moves to B; the session rebases its head onto B.
    fs::write(repo.join("late.txt"), "late\n").unwrap();
    git(&repo, &["add", "late.txt"]);
    git(&repo, &["commit", "-q", "-m", "b"]);
    let b = git_out(&repo, &["rev-parse", "main"]);
    let rebasing = dir.path().join("rebasing");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            rebasing.to_str().unwrap(),
            &b,
        ],
    );
    git(&rebasing, &["cherry-pick", &head]);
    let rebased = git_out(&rebasing, &["rev-parse", "HEAD"]);
    write_receipt_json(
        &run,
        session_receipt(&run, &rebased, "succeeded", "rebased"),
    );
    let receipt =
        dagq::domain::Receipt::parse(&fs::read_to_string(run.receipt_path().unwrap()).unwrap())
            .unwrap();

    let repository = GitRepository::inspect(&repo).unwrap();
    let parse =
        |text: &str| Ok(dagq::infrastructure::run_env::parse_config(text)?.review_subagents);
    let range = |main: &CommitSha| review_range_at(&repository, &run, &receipt, main);
    let snapshot = snapshot_subagents(&repository, &parse, &range)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.commit, b);
    assert_eq!(snapshot.range.base, b);
    assert_eq!(snapshot.range.head, rebased);
    let selected: Vec<_> = snapshot.agents.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(selected, ["design"]);

    // Main moves on to C, which is not an ancestor of the head.
    fs::write(repo.join("c.txt"), "c\n").unwrap();
    git(&repo, &["add", "c.txt"]);
    git(&repo, &["commit", "-q", "-m", "c"]);
    let material = |outcome: Value| {
        let text = fs::read_to_string(outcome["path"].as_str().unwrap()).unwrap();
        (outcome["base"].as_str().unwrap().to_owned(), text)
    };
    let (base, text) = material(runtime::review(&db, TaskId::new(1)).unwrap());
    assert_eq!(
        base, a,
        "found afresh, the range falls back to the run's base"
    );
    assert!(text.contains("late.txt"), "{text}");
    let (base, text) =
        material(dagq::compose::review_in(&db, TaskId::new(1), Some(&snapshot.range)).unwrap());
    assert_eq!(base, b);
    assert!(text.contains(&format!("- base: {b}")), "{text}");
    assert!(!text.contains("late.txt"), "{text}");
    // A range that does not end at the receipt's commit is refused.
    let wrong = dagq::application::review::ReviewRange {
        base: b.clone(),
        head: head.clone(),
    };
    let error = format!(
        "{:#}",
        dagq::compose::review_in(&db, TaskId::new(1), Some(&wrong)).unwrap_err()
    );
    assert!(error.contains("not at the receipt's commit"), "{error}");
    git(
        &repo,
        &["worktree", "remove", "--force", rebasing.to_str().unwrap()],
    );
}

/// The agents `dagq.toml` in `commit`'s tree names whose definition is
/// not in that tree: read from Git's objects, so a definition only added
/// to the index or left in the working files does not count.
fn undefined_agents(root: &Path, commit: &str) -> Vec<String> {
    let repository = dagq::infrastructure::adapters::GitRepository::inspect(root).unwrap();
    let Some(text) = repository.file_in(commit, "dagq.toml").unwrap() else {
        return Vec::new();
    };
    let config = dagq::infrastructure::run_env::parse_config(&text).unwrap();
    config
        .review_subagents
        .iter()
        .filter(|agent| {
            dagq::domain::review_subagents::find_definition(&agent.name, |path| {
                repository.file_in(commit, path)
            })
            .unwrap()
            .is_none()
        })
        .map(|agent| agent.name.clone())
        .collect()
}

/// The check below finds a definition that is only in the index, or only
/// committed after the commit it looks at.
#[test]
fn a_definition_in_the_index_only_is_not_in_the_tree() {
    let (_dir, repo, _db) = fixture();
    let main = commit_on_main(&repo, CONFIG, None);
    fs::create_dir_all(repo.join(".dagq/agents/design")).unwrap();
    fs::write(repo.join(DEFINITION), MAIN_DEFINITION).unwrap();
    git(&repo, &["add", DEFINITION]);
    assert_eq!(undefined_agents(&repo, "HEAD"), ["design", "unused"]);
    git(&repo, &["commit", "-q", "-m", "define design"]);
    assert_eq!(undefined_agents(&repo, "HEAD"), ["unused"]);
    assert_eq!(undefined_agents(&repo, &main), ["design", "unused"]);
}

/// (e) This repository's `dagq.toml` parses, and every agent it names has
/// its definition committed. With no agent configured yet, there is
/// nothing to find; the task that enables them verifies with this test.
#[test]
fn this_repository_names_only_agents_it_defines() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    // The commit being verified: its `dagq.toml` and definitions as
    // committed, not the working files or the index.
    let head = git_out(root, &["rev-parse", "HEAD"]);
    assert_eq!(
        undefined_agents(root, head.trim()),
        Vec::<String>::new(),
        "dagq.toml of {head} names agents without a committed definition"
    );
}

/// `design` and `tests` both review `change.txt`, the stand-in worker's
/// change.
const TWO_AGENTS: &str = "[review.subagents.design]\npaths = [\"change.txt\"]\n\
                          [review.subagents.tests]\npaths = [\"change.txt\"]\n";
const TESTS_DEFINITION: &str = "---\ndescription: main's test checks\n---\nCheck the tests.\n";

/// Main with [`TWO_AGENTS`] and both definitions.
fn two_agents_on_main(repo: &Path, config: &str) {
    fs::create_dir_all(repo.join(".dagq/agents/tests")).unwrap();
    fs::write(repo.join(".dagq/agents/tests/AGENT.md"), TESTS_DEFINITION).unwrap();
    git(repo, &["add", ".dagq/agents/tests/AGENT.md"]);
    commit_on_main(repo, config, Some(MAIN_DEFINITION));
}

/// The one open `approve_landing` ask of the queue.
fn landing_ask(db: &Path) -> dagq::domain::Ask {
    let asks = SqliteQueue::open(db)
        .unwrap()
        .asks(Default::default())
        .unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    assert_eq!(asks[0].kind.as_str(), "approve_landing");
    asks[0].clone()
}

/// The `headless_jobs` rows of the queue: kind and label, in order.
fn job_rows(db: &Path) -> Vec<(String, Option<String>)> {
    rusqlite::Connection::open(db)
        .unwrap()
        .prepare("SELECT kind, label FROM headless_jobs ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// (b) Two selected agents: the whole review's job and each agent's start
/// side by side, before any ends, each in a `headless_jobs` row named by
/// its agent (none for the whole review), each agent's prompt carrying
/// the definition the landing branch's snapshot returned; all pass, the
/// run lands, and `review_finished` records each agent's result, in their
/// configured order, and the route the jobs' judgments gave.
#[test]
fn a_review_runs_its_whole_review_and_each_agent_as_jobs_side_by_side() {
    let (_dir, repo, db) = fixture();
    two_agents_on_main(&repo, TWO_AGENTS);
    let (reviewer, detail) = reviewed(
        &repo,
        &db,
        IDLE_AGENT,
        (
            &[verdict("pass", &[], "meets the acceptance")],
            &[
                ("design", vec![agent_says("design", "pass", &[])]),
                ("tests", vec![agent_says("tests", "pass", &[])]),
            ],
        ),
    );
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let kinds = event_kinds(&detail);
    let first_end = position(&kinds, "review_job_finished");
    let starts: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter(|(_, kind)| **kind == "review_job_started")
        .map(|(at, _)| at)
        .collect();
    assert_eq!(starts.len(), 3, "{kinds:?}");
    assert!(starts.iter().all(|at| *at < first_end), "{kinds:?}");
    let agents: Vec<&Value> = payloads(&detail, "review_job_started")
        .iter()
        .map(|started| &started["agent"])
        .collect();
    assert_eq!(agents, [&Value::Null, &json!("design"), &json!("tests")]);
    assert_eq!(
        job_rows(&db),
        [
            ("review".to_owned(), None),
            ("review".to_owned(), Some("design".to_owned())),
            ("review".to_owned(), Some("tests".to_owned())),
        ]
    );
    let prompts: Vec<String> = reviewer.launches().into_iter().map(|l| l.prompt).collect();
    assert_eq!(prompts.len(), 2);
    assert!(prompts[0].contains(&format!("<definition>\n{MAIN_DEFINITION}\n</definition>")));
    assert!(prompts[1].contains(&format!("<definition>\n{TESTS_DEFINITION}\n</definition>")));
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 1);
    assert_eq!(
        finished[0]["agents"],
        json!([
            {"agent": "design", "status": "completed", "verdict": "pass", "reasons": [], "summary": "design looked"},
            {"agent": "tests", "status": "completed", "verdict": "pass", "reasons": [], "summary": "tests looked"},
        ])
    );
    assert_eq!(
        finished[0]["route"],
        json!({"destination": "land", "parent": "land", "parent_lighter": false, "agents": [
            {"agent": "design", "destination": "land", "escalated_because": null},
            {"agent": "tests", "destination": "land", "escalated_because": null},
        ]})
    );
    // Every job's Execution is on its own end.
    for end in payloads(&detail, "review_job_finished") {
        assert!(end.get("tokens_source").is_some(), "{end}");
    }
    assert!(payloads(&detail, "review_retried").is_empty());
}

/// An agent whose job gives no readable answer runs once more on its own,
/// the other jobs' answers kept; when that gives none either, the review
/// is not judged but fails to the person (`review_failed` and the
/// `approve_landing` ask), naming the agent and its output, and the run
/// does not land.
#[test]
fn an_agent_that_never_answers_fails_the_review_and_no_other_job_runs_again() {
    let (_dir, repo, db) = fixture();
    two_agents_on_main(&repo, TWO_AGENTS);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")])
        .with_review_agents(&[
            ("design", vec![agent_says("design", "pass", &[])]),
            ("tests", vec!["echo 'no verdict'".to_owned()]),
        ]);
    let outcome = supervise_reviewed_with(
        &db,
        &repo,
        &backend,
        &reviewer,
        &SuperviseOptions {
            retry_unreadable_review: true,
            ..supervise_options(4, true)
        },
    );
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db)
        .unwrap()
        .show(TaskId::new(1))
        .unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    assert!(payloads(&detail, "review_finished").is_empty());
    let ends: Vec<(Value, Value, Value)> = payloads(&detail, "review_job_finished")
        .iter()
        .map(|end| {
            (
                end["agent"].clone(),
                end["outcome"].clone(),
                end["runs_again"].clone(),
            )
        })
        .collect();
    assert_eq!(
        ends.iter().filter(|end| end.0 == "tests").count(),
        2,
        "{ends:?}"
    );
    assert!(ends.contains(&(json!("tests"), json!("unreadable"), json!(true))));
    assert!(ends.contains(&(json!("tests"), json!("unreadable"), json!(false))));
    assert_eq!(ends.iter().filter(|end| end.0 == "design").count(), 1);
    assert_eq!(ends.iter().filter(|end| end.0.is_null()).count(), 1);
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1);
    let run_dir = detail.runs[0].run_dir().unwrap();
    let error = failed[0]["error"].as_str().unwrap();
    for part in [
        "the review's jobs did not all give an answer: the agent tests: ".to_owned(),
        format!("its output is in {run_dir}/review-1-agent-tests.again.out and .err"),
    ] {
        assert!(error.contains(&part), "{part} in {error}");
    }
    assert_eq!(landing_ask(&db).run_id.as_ref(), Some(detail.runs[0].id()));
}

/// (f) An agent's concern that needs a person (`scope`) beside a whole
/// review that would land on high confidence asks a person, naming the
/// agent and why; the run does not land. The wiring of a route that asks;
/// the routes of each judgment are `supervise::landing::tests::
/// an_agents_heavier_judgment_under_a_lighter_verdict_decides_where_the_run_goes`.
#[test]
fn an_agents_scope_concern_under_a_landing_concern_asks_a_person() {
    let (_dir, repo, db) = fixture();
    two_agents_on_main(&repo, TWO_AGENTS);
    let scope = json!({"agent": "design", "status": "completed", "verdict": "concern",
        "reasons": ["departs from the ADR"], "summary": "s",
        "recommendation": "land", "confidence": "high", "reason_category": "scope"});
    let whole = json!({"verdict": "concern", "reasons": [], "summary": "lands",
        "recommendation": "land", "confidence": "high"});
    let (_reviewer, detail) = reviewed(
        &repo,
        &db,
        IDLE_AGENT,
        (
            &[format!("printf '%s\\n' '{whole}'")],
            &[
                ("design", vec![agent_answers(scope)]),
                ("tests", vec![agent_says("tests", "pass", &[])]),
            ],
        ),
    );
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished[0]["route"]["destination"], "ask");
    assert_eq!(finished[0]["route"]["parent"], "land");
    assert!(payloads(&detail, "integration_approved").is_empty());
    let ask = landing_ask(&db);
    for part in [
        "the subagent design returned concern recommending land (high confidence, scope)",
        "the review's own verdict concern was lighter",
        "- design: departs from the ADR",
    ] {
        assert!(ask.question.contains(part), "{part} in {}", ask.question);
    }
}

/// (f) The whole review passes, but `design` recommends `send_back` with
/// high confidence and `tests` asks for a change: the run goes back to the
/// session in one revise with each agent's reasons, and lands once every
/// job passes. The wiring of a route that sends the run back.
#[test]
fn the_agents_send_backs_go_back_in_one_revise_with_each_agents_reasons() {
    let (_dir, repo, db) = fixture();
    two_agents_on_main(&repo, TWO_AGENTS);
    let send_back = json!({"agent": "design", "status": "completed", "verdict": "concern",
        "reasons": ["departs"], "summary": "s", "recommendation": "send_back", "confidence": "high"});
    let (_reviewer, detail) = reviewed(
        &repo,
        &db,
        &crate::runtime_review::revising_agent(1),
        (
            &[
                verdict("pass", &[], "looks fine"),
                verdict("pass", &[], "fixed"),
            ],
            &[
                (
                    "design",
                    vec![agent_answers(send_back), agent_says("design", "pass", &[])],
                ),
                (
                    "tests",
                    vec![
                        agent_says("tests", "revise", &["add a test"]),
                        agent_says("tests", "pass", &[]),
                    ],
                ),
            ],
        ),
    );
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let requested = payloads(&detail, "revise_requested");
    assert_eq!(requested.len(), 1);
    assert_eq!(
        requested[0]["reasons"],
        json!(["design: departs", "tests: add a test"])
    );
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished[0]["route"]["destination"], "send_back");
    assert_eq!(finished[0]["route"]["parent_lighter"], true);
    assert_eq!(finished[1]["route"]["destination"], "land");
    // The whole review's own pass is no concern: nothing to decide.
    assert!(payloads(&detail, "concern_decided").is_empty());
}

/// With `[review.jobs] concurrency = 1` the jobs of two runs' reviews,
/// each a whole review and two agents, run one at a time all together: no
/// job starts while another of either review runs, and every job runs.
/// How many start is `domain::review_jobs::tests::
/// jobs_past_the_limit_wait_and_the_reviews_share_it`.
#[test]
fn the_reviews_of_two_runs_share_the_limit_of_jobs_at_once() {
    let (_dir, repo, db) = fixture();
    two_agents_on_main(
        &repo,
        &format!("{TWO_AGENTS}[review.jobs]\nconcurrency = 1\n"),
    );
    add_ready_task(&mut SqliteQueue::open(&db).unwrap(), "second task", &[]);
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    // Each job takes a moment, so that a job past the limit would overlap.
    let slow = |script: String| format!("sleep 0.3; {script}");
    let reviewer = TestReviewer::new(&[slow(verdict("concern", &["look"], "a person looks"))])
        .with_review_agents(&[
            ("design", vec![slow(agent_says("design", "pass", &[]))]),
            ("tests", vec![slow(agent_says("tests", "pass", &[]))]),
        ]);
    let outcome =
        supervise_reviewed_with(&db, &repo, &backend, &reviewer, &supervise_options(2, true));
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let events = SqliteQueue::open(&db).unwrap().all_events().unwrap();
    let mut running = 0;
    let mut most = 0;
    let mut started = 0;
    for event in &events {
        match event.kind.as_str() {
            "review_job_started" => {
                running += 1;
                started += 1;
            }
            "review_job_finished" => running -= 1,
            _ => continue,
        }
        most = most.max(running);
    }
    assert_eq!(started, 6, "both reviews ran all their jobs");
    assert_eq!(most, 1, "no two jobs ran at once");
    let reviews = events
        .iter()
        .filter(|e| e.kind == "review_finished")
        .count();
    assert_eq!(reviews, 2);
}

/// The argv dagq gives a review with required subagents, and without
/// them, run on the real Claude Code (ADR-t1453-1 decision 8,
/// ADR-t1470-1). Real Claude is not run by the
/// automated tests (AGENTS.md: manual smoke), so this is ignored and runs
/// only when a person names the executable:
/// `DAGQ_REAL_CLAUDE=~/.local/bin/claude cargo test --locked --test it -- --ignored --exact review_subagents::the_real_claude_review_runs_its_subagents_without_the_worktrees_settings`.
/// It builds the command with `ClaudeCode::review_command` and
/// `review_subagents` as the supervisor does, then checks with Haiku:
/// (1) the worktree's `.claude/settings.json` is not loaded, with or
/// without subagents (its `SessionStart` hook writes no marker, which it
/// does for the same review with the default setting sources, the
/// control); (2) the worktree's `.claude/agents`
/// is not offered and the handed agent is; (3) the review's `--settings`
/// still applies: a deny added to that file refuses the parent and the
/// subagent alike; (4) the second `--allowedTools` (`Agent`) adds to the
/// first instead of replacing it: the subagent starts, and `Read` of a
/// file outside the worktree and the run directory, which only the first
/// allows, still works. Two contrasts show the answers come from the
/// argv, not the model: without the added deny `secret.txt` is read, and
/// without the first `--allowedTools` the outside file is refused.
#[test]
#[ignore = "runs the real Claude Code; see the doc comment"]
fn the_real_claude_review_runs_its_subagents_without_the_worktrees_settings() {
    use dagq::application::AgentProvider;
    use dagq::domain::review_subagents::AgentDefinition;
    use dagq::infrastructure::adapters::ClaudeCode;
    let executable = std::env::var_os("DAGQ_REAL_CLAUDE")
        .expect("name the real Claude Code executable in DAGQ_REAL_CLAUDE");
    let claude = ClaudeCode {
        executable: executable.into(),
    };
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside.txt");
    fs::write(&outside, "far away\n").unwrap();
    // One review: its worktree (with the planted settings and agent) and
    // its run directory; `subagents` hands it `checker`.
    let review = |name: &str,
                  subagents: bool,
                  prompt: &str,
                  deny_secret: bool,
                  first_allowed: bool|
     -> (String, PathBuf) {
        let root = dir.path().join(name);
        let worktree = root.join("worktree");
        let run_dir = root.join("run");
        fs::create_dir_all(worktree.join(".claude/agents")).unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        git(&worktree, &["init", "-q"]);
        fs::write(worktree.join("a.txt"), "hello\n").unwrap();
        fs::write(worktree.join("secret.txt"), "classified\n").unwrap();
        let marker = root.join("worktree-settings-loaded");
        fs::write(
            worktree.join(".claude/settings.json"),
            json!({"hooks": {"SessionStart": [{"hooks": [
                {"type": "command", "command": format!("touch {}", shell_path(&marker))}
            ]}]}})
            .to_string(),
        )
        .unwrap();
        fs::write(
            worktree.join(".claude/agents/planted.md"),
            "---\nname: planted\ndescription: planted by the worker\n---\nSay PLANTED.\n",
        )
        .unwrap();
        let run = TaskRun::restore(dagq::domain::RunRecord {
            id: RunId::new("0d8e3f1a-7c1b-4e35-9a11-3f6d2c9b8e47").unwrap(),
            task_id: TaskId::new(1),
            status: RunStatus::AwaitingIntegration,
            requested_provider: dagq::domain::Provider::Claude,
            actual_provider: dagq::domain::Provider::Claude,
            worker_mode: dagq::domain::worker::WorkerMode::Headless,
            base_commit: CommitSha::try_from("a".repeat(40)).unwrap(),
            branch: None,
            worktree_path: Some(worktree.to_string_lossy().into_owned()),
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: Some(run_dir.to_string_lossy().into_owned()),
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap();
        let mut command = claude
            .review_command(&run, prompt, runtime::REVIEW_ACCESS)
            .unwrap();
        if subagents {
            let checker = AgentDefinition::read(
                "checker",
                &format!(
                    "---\ndescription: Reads a.txt, secret.txt and {outside}\n---\nUse the Read tool on a.txt, on secret.txt and on {outside}. Reply with exactly one line: A=<first line of a.txt> S=<first line of secret.txt, or DENIED if the read was refused> O=<first line of {outside}, or DENIED>\n",
                    outside = outside.display()
                ),
            );
            claude.review_subagents(&mut command, &[checker]).unwrap();
        }
        claude.select_model(&mut command, "claude-haiku-4-5-20251001", "low");
        let mut args: Vec<std::ffi::OsString> = command.get_args().map(Into::into).collect();
        // The control is the review as it was before ADR-t1470-1: with
        // the default setting sources.
        if name == "control" {
            let at = args.iter().position(|a| a == "--setting-sources").unwrap();
            assert_eq!(args[at + 1], "");
            args.drain(at..=at + 1);
        }
        if !first_allowed {
            let at = args.iter().position(|a| a == "Read,Grep,Glob").unwrap();
            assert_eq!(args[at - 1], "--allowedTools");
            args.drain(at - 1..=at);
        }
        // The review's settings as dagq wrote them, with one more deny.
        let settings = run_dir.join("claude-review-settings.json");
        let mut written: Value =
            serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        if deny_secret {
            written["permissions"]["deny"]
                .as_array_mut()
                .unwrap()
                .push(json!("Read(./secret.txt)"));
        }
        fs::write(&settings, written.to_string()).unwrap();
        // The prompt is the review's standard input (task 1560).
        let input = run_dir.join(format!("{name}-prompt.txt"));
        fs::write(&input, command.get_stdin().unwrap()).unwrap();
        let mut process = std::process::Command::new(command.get_program());
        process
            .args(&args)
            .current_dir(command.get_current_dir().unwrap())
            .env_remove("CLAUDECODE")
            .stdin(fs::File::open(&input).unwrap());
        let output = {
            let _waiting =
                crate::common::within(Duration::from_secs(300), "the real Claude review");
            process.output().unwrap()
        };
        assert!(output.status.success(), "{output:?}");
        // The review prints its result as JSON: the reply is its `result`.
        (
            claude.job_reply(&String::from_utf8_lossy(&output.stdout)),
            marker,
        )
    };

    let (control, marker) = review(
        "control",
        false,
        "Reply with the single word OK.",
        true,
        true,
    );
    assert!(
        marker.is_file(),
        "with the default setting sources the worktree's settings load (the control): {control}"
    );
    // A review without subagents loads them no more (ADR-t1470-1).
    let (plain, marker) = review("plain", false, "Reply with the single word OK.", true, true);
    assert!(
        !marker.is_file(),
        "the worktree's settings were loaded without subagents: {plain}"
    );

    let prompt = format!(
        "Do these steps and print one line for each, nothing else.\n\
         1. TYPES: the subagent types your Agent tool accepts, comma separated.\n\
         2. READ: the first line of a.txt, read with the Read tool.\n\
         3. SECRET: the first line of secret.txt read with the Read tool, or DENIED if the read is refused.\n\
         4. OUTSIDE: the first line of {}, read with the Read tool, or DENIED if the read is refused.\n\
         5. CHECKER: call the checker subagent with the Agent tool and copy its whole reply verbatim, the A=, S= and O= parts included; do not summarize it.",
        outside.display()
    );
    let line = |said: &str, label: &str| {
        said.lines()
            .find_map(|l| {
                l.trim()
                    .trim_start_matches(|c: char| c.is_ascii_digit() || "*-. ".contains(c))
                    .strip_prefix(label)
            })
            .unwrap_or_else(|| panic!("no {label} in {said}"))
            .trim()
            .to_owned()
    };
    let (said, marker) = review("subagents", true, &prompt, true, true);
    println!("{said}");
    assert!(
        !marker.is_file(),
        "the worktree's settings were loaded: {said}"
    );
    let types = line(&said, "TYPES:");
    assert!(types.contains("checker"), "{said}");
    assert!(!types.contains("planted"), "{said}");
    assert!(line(&said, "READ:").contains("hello"), "{said}");
    assert!(line(&said, "SECRET:").contains("DENIED"), "{said}");
    assert!(line(&said, "OUTSIDE:").contains("far away"), "{said}");
    let checker = line(&said, "CHECKER:");
    assert!(
        checker.contains("A=hello")
            && checker.contains("S=DENIED")
            && checker.contains("O=far away"),
        "{said}"
    );
    assert!(!said.contains("classified"), "{said}");

    // Without the added deny, the same review reads secret.txt.
    let (open, _) = review("no-deny", true, &prompt, false, true);
    println!("{open}");
    assert!(line(&open, "SECRET:").contains("classified"), "{open}");
    // Without the first --allowedTools, the outside file is refused.
    let (narrow, _) = review("no-reads", true, &prompt, true, false);
    println!("{narrow}");
    assert!(line(&narrow, "OUTSIDE:").contains("DENIED"), "{narrow}");
    assert!(!narrow.contains("far away"), "{narrow}");
}

/// Main with `[roles.review]` naming Codex, `extra` after it, and
/// `design` required for `change.txt`.
fn codex_review_with_design(repo: &Path, extra: &str) {
    commit_on_main(
        repo,
        &format!("[roles.review]\nprovider = 'codex'\n{extra}{CONFIG}"),
        Some(MAIN_DEFINITION),
    );
}

/// A Codex review that requires `design` runs the whole review's job and
/// the agent's on Codex, without moving to Claude for what a provider can
/// run, even under `--no-claude`; the agent's job is Codex's read-only job
/// with the narrowing of its declared tools, and the run lands.
#[test]
fn a_codex_review_runs_its_whole_review_and_its_agents_on_codex() {
    use crate::runtime_codex::{FINISH, TASK, codex_fixture};
    let (dir, repo, db, backend, codex) = codex_fixture();
    codex_review_with_design(&repo, "");
    set_turns(dir.path(), FINISH);
    let options = SuperviseOptions {
        codex,
        no_claude: true,
        codex_home: Some(dir.path().join(CODEX_HOME)),
        ..supervise_options(1, true)
    };
    let reviewer = TestReviewer::new(&[]);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db).unwrap().show(TASK).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let started = payloads(&detail, "review_job_started");
    assert_eq!(started.len(), 2, "{started:?}");
    for job in &started {
        assert_eq!(job["launch"]["provider"], "codex", "{job}");
        assert!(job["launch"]["switch_reason"].is_null(), "{job}");
    }
    assert_eq!(started[1]["agent"], "design");
    assert!(reviewer.prompts().is_empty() && reviewer.launches().is_empty());
    let args = fs::read_to_string(dir.path().join("codex-agent-args.log")).unwrap();
    assert!(args.starts_with("design "), "{args}");
    assert!(
        args.contains("permissions.dagq_job") || args.contains(" --sandbox| read-only|"),
        "{args}"
    );
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished[0]["agents"][0]["summary"], "codex agent passed");
}

/// With `[provider_fallback] jobs = false`, the agent's job of a Codex
/// review stops at Codex's usage limit: Codex is held, the job does not
/// move to Claude but waits, the whole review's answer kept, and once the
/// hold ends it starts on Codex again and answers; the run lands. Claude
/// is never held nor started.
#[test]
fn with_the_fallback_off_an_agents_job_waits_for_its_provider_and_lands() {
    use crate::runtime_codex::{FINISH, TASK, codex_fixture};
    let (dir, repo, db, backend, codex) = codex_fixture();
    codex_review_with_design(&repo, "[provider_fallback]\njobs = false\n");
    set_turns(dir.path(), FINISH);
    fs::write(
        dir.path().join("codex-agent-design-failure.jsonl"),
        "{\"type\":\"error\",\"message\":\"unexpected status 429 Too Many Requests: You have hit your usage limit. Try again later.\"}\n",
    )
    .unwrap();
    let options = SuperviseOptions {
        codex,
        codex_home: Some(dir.path().join(CODEX_HOME)),
        ..supervise_options(1, true)
    };
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "from Claude")]);
    let supervisor = {
        let (db, repo) = (db.clone(), repo.clone());
        thread::spawn(move || {
            let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
            (outcome, reviewer.prompts().len())
        })
    };
    // Codex is held for the agent's job; its hold is lifted by hand.
    wait_until(&db, Duration::from_secs(60), |queue| {
        queue
            .all_events()
            .unwrap()
            .iter()
            .any(|event| event.kind == "provider_held")
    });
    let held = SqliteQueue::open(&db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .find(|event| event.kind == "provider_held")
        .unwrap();
    assert_eq!(held.payload["provider"], "codex");
    SqliteQueue::open(&db)
        .unwrap()
        .record_queue_event(
            dagq::domain::EventKind::ProviderReleased,
            json!({"provider": "codex", "reason": "usage_limit",
                   "since": held.payload["since"], "why": "retry_due"}),
        )
        .unwrap();
    let (outcome, claude_prompts) = joined(supervisor, "the supervisor to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(claude_prompts, 0, "Claude reviewed nothing");
    let detail = SqliteQueue::open(&db).unwrap().show(TASK).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let starts: Vec<(Value, Value, Value)> = payloads(&detail, "review_job_started")
        .iter()
        .map(|job| {
            (
                job["agent"].clone(),
                job["launch"]["provider"].clone(),
                job["launch"]["switched_from"].clone(),
            )
        })
        .collect();
    assert_eq!(
        starts,
        [
            (Value::Null, json!("codex"), Value::Null),
            (json!("design"), json!("codex"), Value::Null),
            (json!("design"), json!("codex"), Value::Null),
        ]
    );
    let ends: Vec<(Value, Value)> = payloads(&detail, "review_job_finished")
        .iter()
        .map(|end| (end["agent"].clone(), end["outcome"].clone()))
        .collect();
    assert!(ends.contains(&(json!("design"), json!("held"))), "{ends:?}");
    assert!(
        ends.contains(&(json!("design"), json!("answered"))),
        "{ends:?}"
    );
    assert!(payloads(&detail, "review_failed").is_empty());
    let held_providers: Vec<Value> = SqliteQueue::open(&db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "provider_held")
        .map(|event| event.payload["provider"].clone())
        .collect();
    assert!(
        held_providers.iter().all(|p| p == "codex"),
        "{held_providers:?}"
    );
}

/// Commit an eval of `design` on main: one dev case adding `notes.txt`,
/// its patch, and the case list, at a base commit that holds them.
fn commit_design_eval(repo: &Path) {
    let patch = "diff --git a/notes.txt b/notes.txt\nnew file mode 100644\n--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1 @@\n+hello\n";
    let hash = format!("{:x}", Sha256::digest(patch.as_bytes()));
    let patches = repo.join(".dagq/agent-cases/patches");
    fs::create_dir_all(&patches).unwrap();
    fs::write(patches.join(format!("{hash}.patch")), patch).unwrap();
    let write = |base: &str| {
        let dir = repo.join(".dagq/agents/design/evals");
        fs::create_dir_all(&dir).unwrap();
        let list = json!({"agent": "design", "role": "review", "codes": ["D-1"], "k": 1, "cases": [
            {"id": "adds-notes", "source": "handmade", "made_by": "test", "base_commit": base,
             "patch": hash, "review": {"input": {}, "expected": {"verdict": "clean", "codes": []}}},
        ]});
        fs::write(dir.join("dev.json"), list.to_string()).unwrap();
        git(repo, &["add", "."]);
        git(repo, &["commit", "-q", "-m", "design's eval"]);
    };
    write(&git_out(repo, &["rev-parse", "HEAD"]));
    let base = git_out(repo, &["rev-parse", "HEAD"]);
    write(&base);
}

/// `launch` with its paths set to `root`'s, so that two jobs' commands
/// can be told apart only by what they are given.
fn at_root(
    launch: &dagq::application::AgentJobLaunch,
    root: &Path,
) -> dagq::application::AgentJobLaunch {
    dagq::application::AgentJobLaunch {
        cwd: root.join("tree"),
        dir: root.join("job"),
        material: root.join("material/material.md"),
        ..launch.clone()
    }
}

/// The prompt without its material lines (the change's commits and the
/// material file), which are all an eval's and a review's job of one
/// agent differ by.
fn without_material(prompt: &str) -> String {
    prompt
        .lines()
        .filter(|line| {
            !line.starts_with("The change: ")
                && !line.starts_with("The material file ")
                && !line.contains("read the whole change in the material file")
        })
        .map(|line| {
            line.split(" in the material file ")
                .next()
                .unwrap_or(line)
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The eval of an agent and a run's review start the agent's job the one
/// way (ADR-t1728-1 (e), ADR-t1895-1). (a) With the same definition,
/// provider and declared tools, Claude Code's and Codex's commands of the
/// eval's job and of the review's agent job are the same argv, and their
/// prompts (on standard input) differ only by the material's lines. (b)
/// The eval's case starts the job of the agent it measures alone: no whole
/// review's job and no other agent's. (c) Their limits are apart: with
/// `[review.jobs] concurrency = 1` the review's whole job runs until the
/// eval's job started (the review's full limit does not keep the eval
/// from starting), and the review's agent job then starts while the
/// eval's still runs (the eval's job takes no room of the review's).
#[test]
fn the_eval_and_a_review_start_an_agents_job_the_one_way_within_their_own_limits() {
    use dagq::application::AgentProvider;
    let (dir, repo, db) = fixture();
    commit_on_main(
        &repo,
        &format!("{CONFIG}[review.jobs]\nconcurrency = 1\n"),
        Some(MAIN_DEFINITION),
    );
    commit_design_eval(&repo);
    let requested = crate::common::cli::ok(&db, &["agent", "eval", "design"]);
    assert_eq!(requested["status"], "waiting", "{requested}");
    let marks = dir.path().join("marks");
    fs::create_dir_all(&marks).unwrap();
    let mark = |name: &str| shell_path(marks.join(name));
    // A wait that ends with the test, whose job would otherwise wait on.
    let await_mark = |name: &str| crate::common::await_path(marks.join(name));
    let whole = format!(
        "{}; {}",
        await_mark("eval-started"),
        verdict("pass", &[], "meets the acceptance")
    );
    // The eval's job runs in its case's tree, the review's in the worktree.
    let design = format!(
        "case \"$PWD\" in */tree) touch {eval}; {wait}; {says} ;; *) touch {review}; {says} ;; esac",
        eval = mark("eval-started"),
        wait = await_mark("review-agent-started"),
        review = mark("review-agent-started"),
        says = agent_says("design", "pass", &[]),
    );
    let (reviewer, detail) = reviewed(
        &repo,
        &db,
        IDLE_AGENT,
        (&[whole], &[("design", vec![design])]),
    );
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    // (c) Neither waited for the other: every job answered before its time.
    assert!(payloads(&detail, "review_failed").is_empty());
    let ends = payloads(&detail, "review_job_finished");
    assert!(
        ends.iter().all(|end| end["outcome"] == "answered"),
        "{ends:?}"
    );
    let eval_runs = SqliteQueue::open(&db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == "agent_eval_run_finished")
        .map(|event| event.payload)
        .collect::<Vec<_>>();
    assert_eq!(eval_runs.len(), 1, "{eval_runs:?}");
    assert!(eval_runs[0]["error"].is_null(), "{eval_runs:?}");
    // (b) One job for the eval's one run, of the agent it measures; the
    // run's review alone started a whole review's job.
    let launches = reviewer.launches();
    let (eval, review): (Vec<_>, Vec<_>) = launches
        .iter()
        .partition(|launch| launch.cwd.ends_with("tree"));
    assert_eq!((eval.len(), review.len()), (1, 1), "{launches:?}");
    assert_eq!(reviewer.prompts().len(), 1, "one whole review, the run's");
    let rows = job_rows(&db);
    assert_eq!(
        rows.iter().filter(|(kind, _)| kind == "agent_eval").count(),
        1,
        "{rows:?}"
    );
    // (a) The same argv on either provider, and the prompts differ only by
    // the material's lines.
    let (eval, review) = (eval[0], review[0]);
    assert_eq!(eval.tools, review.tools);
    assert_eq!(
        without_material(&eval.prompt),
        without_material(&review.prompt)
    );
    assert_ne!(eval.prompt, review.prompt);
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("job")).unwrap();
    let claude = dagq::infrastructure::adapters::ClaudeCode {
        executable: "claude".into(),
    };
    let codex = dagq::infrastructure::codex::Codex {
        executable: "codex".into(),
        home: None,
    };
    let argv = |provider: &dyn AgentProvider, launch: &dagq::application::AgentJobLaunch| {
        let command = provider
            .agent_job_command(&at_root(launch, root.path()))
            .unwrap();
        assert_eq!(command.get_stdin(), Some(launch.prompt.as_str()));
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    };
    for provider in [&claude as &dyn AgentProvider, &codex] {
        let args = argv(provider, eval);
        assert_eq!(args, argv(provider, review));
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("You are the review agent"))
        );
        assert!(!args.iter().any(|arg| arg == "--agents"), "{args:?}");
    }
}
