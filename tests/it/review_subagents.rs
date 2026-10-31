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
use dagq::domain::EventKind;

use runtime_support::*;
use sha2::{Digest, Sha256};

/// What the main checkout commits: `design` reviews `change.txt` (the
/// stand-in worker's change); `unused` matches nothing and has no
/// definition, which a review that does not select it never needs.
const CONFIG: &str = "[review.subagents.design]\npaths = [\"change.txt\", \"docs/**\"]\n\
                      [review.subagents.unused]\npaths = [\"nothing/**\"]\n";
const DEFINITION: &str = ".dagq/agents/design/AGENT.md";
const MAIN_DEFINITION: &str = "---\ndescription: main's design checks\n---\nCheck the design.\n";

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

/// What the agent job of `design` was given for the run of `detail`: one
/// job, whose prompt carries main's definition as the snapshot returned
/// it, whole, and no path of a definition, beside the material whose range
/// selected it; no whole review's prompt says anything of agents.
fn assert_job_input(reviewer: &TestReviewer, detail: &dagq::domain::TaskDetail, main: &str) {
    let run_dir = detail.runs[0].run_dir().unwrap();
    let material = PathBuf::from(format!("{run_dir}/review.md"));
    let launches: Vec<_> = reviewer
        .launches()
        .into_iter()
        .filter(|launch| launch.material == material)
        .collect();
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
    // review.md shows the range the agents were selected from.
    let expected = expected_snapshot(main, detail);
    let material = fs::read_to_string(&material).unwrap();
    let base = format!("- base: {}", expected["base"].as_str().unwrap());
    assert!(material.contains(&base), "{base} in {material}");
    for whole in reviewer.prompts() {
        assert!(!whole.contains("subagent"), "{whole}");
    }
    assert!(!Path::new(&format!("{run_dir}/review-subagents-1.json")).exists());
}

/// (a) A worker that rewrites or deletes `dagq.toml` and the definition
/// on its branch neither drops nor changes its required review, and the
/// main checkout's uncommitted edits of them are not used: main's commit
/// decides, and the job reads main's definition (the Git boundary of
/// ADR-t1453-1 decision 4; what the commit's files select is
/// `application::review::tests`). The range is Git's, without rename
/// detection: the old path of a rename out of `docs/` and a deleted path
/// under it select `design`; and a diff the table does not select reviews
/// with the whole review's job alone, no word of subagents in the prompt,
/// the material or `review_finished`. One task's run each, in one fixture
/// and one supervisor's pass: the whole review's `concern` keeps each run
/// from landing, so main's commit stays the one each review reads.
#[test]
fn the_worker_cannot_change_or_drop_its_required_review() {
    // Each tamper, and what the reviewed head keeps of `dagq.toml` and the
    // definition.
    let tampers = [
        // Narrow the agent's paths away and rewrite its definition.
        (
            "printf '[review.subagents.design]\\npaths = [\"nothing/**\"]\\n' > dagq.toml; \
             printf 'the worker s checks\\n' > .dagq/agents/design/AGENT.md; \
             git commit -q -am tamper",
            format!("{DEFINITION}\ndagq.toml"),
        ),
        // Delete the table and the definition.
        (
            "printf '[run.env]\\n' > dagq.toml; git rm -q .dagq/agents/design/AGENT.md; \
             git commit -q -am tamper",
            "dagq.toml".to_owned(),
        ),
        // Delete dagq.toml itself and the definition.
        (
            "git rm -q dagq.toml .dagq/agents/design/AGENT.md; git commit -q -m tamper",
            String::new(),
        ),
    ];
    // After the tampers: a change no glob matches, and a rename and a
    // deletion under `docs/`.
    let unselected = TaskId::new(tampers.len() as i64 + 1);
    let renamed = TaskId::new(tampers.len() as i64 + 2);
    let (_dir, repo, db) = fixture();
    let mut queue = SqliteQueue::open(&db).unwrap();
    for _ in 1..renamed.as_i64() {
        add_ready_task(&mut queue, "test task", &[]);
    }
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(repo.join("docs/old.md"), "old\n").unwrap();
    fs::write(repo.join("docs/gone.md"), "gone\n").unwrap();
    git(&repo, &["add", "docs/old.md", "docs/gone.md"]);
    let main = commit_on_main(&repo, CONFIG, Some(MAIN_DEFINITION));
    // The main checkout's uncommitted edits: the agent `all` would select
    // every run (its definition is nowhere, which would fail the review).
    fs::write(
        repo.join("dagq.toml"),
        format!("{CONFIG}[review.subagents.all]\npaths = [\"**\"]\n"),
    )
    .unwrap();
    fs::write(repo.join(DEFINITION), "uncommitted checks\n").unwrap();
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    for (task, (tamper, _)) in (1..).zip(&tampers) {
        backend.script_for(
            task,
            &format!("{tamper}; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit"),
        );
    }
    backend.script_for(
        unselected.as_i64(),
        "printf 'x\\n' > other.txt; git add other.txt; git commit -q -m other; \
         receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    backend.script_for(
        renamed.as_i64(),
        "git mv docs/old.md moved.md; git rm -q docs/gone.md; git commit -q -m change; \
         receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let reviewer = TestReviewer::new(&[verdict("concern", &["a finding"], "look")])
        .with_review_agents(&[("design", vec![agent_says("design", "pass", &[])])]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    // design's job for each tamper and the rename, none for the unselected.
    assert_eq!(reviewer.launches().len(), tampers.len() + 1);
    for (task, (tamper, kept)) in (1..).zip(&tampers) {
        let detail = queue.show(TaskId::new(task)).unwrap();
        assert_eq!(
            detail.runs[0].status(),
            RunStatus::AwaitingIntegration,
            "{tamper}"
        );
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
        assert_eq!(left.trim(), kept, "{tamper}");
        let started = payloads(&detail, "review_started");
        assert_eq!(started.len(), 1, "{tamper}");
        assert_eq!(
            started[0]["subagents"],
            expected_snapshot(&main, &detail),
            "{tamper}"
        );
        assert_job_input(&reviewer, &detail, &main);
        assert!(payloads(&detail, "review_failed").is_empty(), "{tamper}");
    }

    // The rename's old path and the deleted path select design.
    let detail = queue.show(renamed).unwrap();
    let started = payloads(&detail, "review_started");
    assert_eq!(started.len(), 1);
    let subagents = &started[0]["subagents"];
    assert_eq!(subagents["commit"], main.as_str());
    assert_eq!(subagents["agents"].as_array().unwrap().len(), 1);
    assert_eq!(subagents["agents"][0]["agent"], "design");
    assert_eq!(
        subagents["agents"][0]["paths"],
        json!(["docs/gone.md", "docs/old.md"])
    );
    assert!(payloads(&detail, "review_failed").is_empty());

    // A diff the table does not select: the event records the empty
    // selection, the whole review's job alone runs, and nothing of agents
    // is in the material or review_finished (without the table,
    // `application::review::tests::a_review_without_required_agents_reads_as_before`).
    let detail = queue.show(unselected).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
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
    let material = PathBuf::from(format!("{run_dir}/review.md"));
    assert!(
        reviewer
            .launches()
            .iter()
            .all(|launch| launch.material != material)
    );
    assert_eq!(payloads(&detail, "review_job_started").len(), 1);
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 1);
    assert!(finished[0].get("agents").is_none() && finished[0].get("route").is_none());
    let material = fs::read_to_string(&material).unwrap();
    assert!(!material.contains("subagent"), "{material}");
    assert!(payloads(&detail, "review_failed").is_empty());
}

/// (c) An agent the diff selects whose definition is not in main's
/// commit fails the review to the person with why: no job runs, the run
/// does not land, and `review_failed` and the ask say which definition.
/// The wiring of a review that cannot start to `review_failed` and the
/// ask; why it cannot (an unparsable config, a missing definition, a
/// definition declaring a tool beyond a review's reads (ADR-t1728-2), no
/// provider that runs the agents) is `application::review::tests` (the
/// tools: `a_definitions_tools_are_checked_by_the_snapshot_and_doctor`)
/// and `supervise::landing::tests`. `dagq doctor`'s `agents` reports the
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

/// The range starts at the merge base of the current main and the
/// receipt's commit: a session rebased the run from main A onto main B,
/// which touched `late.txt`, and main then moved on to C, which touched
/// `c.txt` and configured the program reviews. Main is no ancestor of the
/// head, yet the range is B...head: review.md's commits and diff, the
/// agents and the programs selected and the range `review_started`
/// records and the agent's job is told hold the run's change only, not
/// B's or C's. The range fixed with the snapshot is the one review.md
/// shows, and one that does not end at the receipt's commit is refused.
#[test]
fn a_run_rebased_onto_a_main_that_moved_on_is_reviewed_from_their_merge_base() {
    use dagq::application::review::{review_range_at, snapshot_subagents};
    use dagq::application::review_programs::snapshot_programs;
    use dagq::infrastructure::adapters::GitRepository;
    let (dir, repo, db) = fixture();
    let config = format!("{CONFIG}[review.subagents.late]\npaths = [\"late.txt\", \"c.txt\"]\n");
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
    git(&repo, &["commit", "-q", "-m", "other task b"]);
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
    // Main moves on to C, which is not an ancestor of the head.
    fs::create_dir_all(repo.join("scripts")).unwrap();
    for name in ["late", "change"] {
        let script = format!("scripts/{name}.sh");
        fs::write(repo.join(&script), "#!/bin/sh\nexit 0\n").unwrap();
        git(&repo, &["add", &script]);
    }
    fs::write(repo.join("c.txt"), "c\n").unwrap();
    git(&repo, &["add", "c.txt"]);
    let c = commit_on_main(
        &repo,
        &format!(
            "{config}[review.programs.late]\nscript = \"scripts/late.sh\"\npaths = [\"late.txt\", \"c.txt\"]\n\
             [review.programs.change]\nscript = \"scripts/change.sh\"\npaths = [\"change.txt\"]\n"
        ),
        None,
    );

    let repository = GitRepository::inspect(&repo).unwrap();
    let range = |main: &CommitSha| review_range_at(&repository, &run, &receipt, main);
    let parse = |text: &str| Ok(dagq::infrastructure::run_env::parse_config(text)?.review_agents());
    let snapshot = snapshot_subagents(&repository, &parse, &range)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.commit, c);
    assert_eq!(snapshot.range.base, b);
    assert_eq!(snapshot.range.head, rebased);
    assert_eq!(snapshot.event_value()["base"], json!(b));
    let selected: Vec<_> = snapshot.agents.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(selected, ["design"]);
    assert_eq!(snapshot.agents[0].matched, ["change.txt"]);
    // The agent's job is told the change is B to the receipt's commit, as
    // the review builds it from the snapshot.
    let agent = &snapshot.agents[0];
    let job_dir = dir.path().join("agent-job");
    let built = dagq::application::agent_job::build(&dagq::application::agent_job::AgentJob {
        agent: &agent.name,
        definition: &agent.definition,
        tools: &agent.tools,
        rules: None,
        base: &snapshot.range.base,
        head: &snapshot.range.head,
        material: &job_dir.join("review.md"),
        cwd: &rebasing,
        dir: &job_dir,
        language: None,
        reads: None,
        answers: None,
    })
    .unwrap();
    let prompt = &built.prompt.text;
    assert!(
        prompt.contains(&format!("the commits from {b} to {rebased},")),
        "{prompt}"
    );
    let parse =
        |text: &str| Ok(dagq::infrastructure::run_env::parse_config(text)?.review_programs());
    let programs = snapshot_programs(&repository, &parse, &range)
        .unwrap()
        .unwrap();
    assert_eq!(programs.range, snapshot.range);
    let selected: Vec<_> = programs
        .programs
        .iter()
        .map(|p| (p.program.name.as_str(), p.matched.clone()))
        .collect();
    assert_eq!(
        selected,
        [
            ("builtin:secrets", vec!["change.txt".to_owned()]),
            ("change", vec!["change.txt".to_owned()])
        ]
    );

    let material = |outcome: Value| {
        let text = fs::read_to_string(outcome["path"].as_str().unwrap()).unwrap();
        (outcome["base"].as_str().unwrap().to_owned(), text)
    };
    let (base, text) = material(runtime::review(&db, TaskId::new(1)).unwrap());
    assert_eq!(base, b, "found afresh, the range starts at the merge base");
    assert!(text.contains(&format!("- base: {b}")), "{text}");
    assert!(text.contains("change.txt"), "{text}");
    for other in ["late.txt", "c.txt", "other task b", "review subagents"] {
        assert!(!text.contains(other), "{other}: {text}");
    }
    let (base, fixed) =
        material(dagq::compose::review_in(&db, TaskId::new(1), Some(&snapshot.range)).unwrap());
    assert_eq!((base, fixed), (b.clone(), text));
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
            // builtin:secrets, on by default, before the agents.
            (
                "review_program".to_owned(),
                Some("builtin:secrets".to_owned())
            ),
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

/// (f) The agents' judgments decide where the run goes, over two rounds.
/// Round 1: the whole review passes, but `design` recommends `send_back`
/// with high confidence and `tests` asks for a change: the run goes back
/// to the session in one revise with each agent's reasons. Round 2: an
/// agent's concern that needs a person (`scope`) beside a whole review
/// that would land on high confidence asks a person, naming the agent and
/// why; the run does not land. The wiring of a route that sends the run
/// back and of one that asks; the routes and the ask's words of each
/// judgment are `supervise::landing::tests::
/// an_agents_heavier_judgment_under_a_lighter_verdict_decides_where_the_run_goes`.
#[test]
fn the_agents_judgments_send_the_run_back_and_then_ask_a_person() {
    let (_dir, repo, db) = fixture();
    two_agents_on_main(&repo, TWO_AGENTS);
    let send_back = json!({"agent": "design", "status": "completed", "verdict": "concern",
        "reasons": ["departs"], "summary": "s", "recommendation": "send_back", "confidence": "high"});
    let scope = json!({"agent": "design", "status": "completed", "verdict": "concern",
        "reasons": ["departs from the ADR"], "summary": "s",
        "recommendation": "land", "confidence": "high", "reason_category": "scope"});
    let landing = json!({"verdict": "concern", "reasons": [], "summary": "lands",
        "recommendation": "land", "confidence": "high"});
    let (_reviewer, detail) = reviewed(
        &repo,
        &db,
        &crate::runtime_review::revising_agent(1),
        (
            &[
                verdict("pass", &[], "looks fine"),
                format!("printf '%s\\n' '{landing}'"),
            ],
            &[
                (
                    "design",
                    vec![agent_answers(send_back), agent_answers(scope)],
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
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    let requested = payloads(&detail, "revise_requested");
    assert_eq!(requested.len(), 1);
    assert_eq!(
        requested[0]["reasons"],
        json!(["design: departs", "tests: add a test"])
    );
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished.len(), 2);
    assert_eq!(finished[0]["route"]["destination"], "send_back");
    assert_eq!(finished[0]["route"]["parent_lighter"], true);
    assert_eq!(finished[1]["route"]["destination"], "ask");
    assert_eq!(finished[1]["route"]["parent"], "land");
    // Neither whole review's own judgment decided: nothing to decide.
    assert!(payloads(&detail, "concern_decided").is_empty());
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
        .filter(|e| e.kind == EventKind::ReviewFinished)
        .count();
    assert_eq!(reviews, 2);
}

/// The argv dagq gives the run's review jobs, run on the real Claude
/// Code: the whole review's (`ClaudeCode::review_command`) and an agent's
/// (`ClaudeCode::agent_job_command`, with its declared tools turned into
/// Claude Code's, and without MCP servers), each built as the supervisor
/// builds it but for the model and the session id. Real Claude is
/// not run by the automated tests (AGENTS.md: manual smoke), so this is
/// ignored and runs only when a person names the executable:
/// `DAGQ_REAL_CLAUDE=~/.local/bin/claude cargo test --locked --test it -- --ignored --exact review_subagents::the_real_claude_review_jobs_keep_their_limits`.
/// With Haiku, in a worktree whose `.claude/settings.json` has a
/// `SessionStart` hook that writes a marker and whose `.claude/agents`
/// defines an agent, it checks that
/// 1. neither job loads the worktree's settings: no marker
///    (ADR-t1470-1);
/// 2. the agent's job is given none of the worktree's agents: its
///    `init` event lists none;
/// 3. the `permissions.deny` of the job's `--settings` holds in the
///    agent's job: given the shell, the command the deny names is
///    refused (its `result` event lists the denial) and does not run;
/// 4. the agent's job has only its declared tools, here the review's
///    default reads: Claude Code lists no writing tool and the file it is
///    told to write is not there (ADR-t1728-2).
///
/// Each has a control the property fails in, so a check that passes
/// proves the argv and not the test: for 1 the whole review and for 1, 2
/// and 4 the agent's job with the default setting sources (and, for 4,
/// `write` declared) load the marker, list the agent and write the file;
/// for 3 the same job with the deny emptied runs the command.
#[test]
#[ignore = "runs the real Claude Code; see the doc comment"]
fn the_real_claude_review_jobs_keep_their_limits() {
    use dagq::application::{AgentJobLaunch, AgentProvider, CommandSpec};
    use dagq::domain::review_subagents::{AgentRole, AgentTool, AgentTools};
    use dagq::infrastructure::adapters::ClaudeCode;
    const PLANTED_AGENT: &str = "planted-reviewer";
    let executable = std::env::var_os("DAGQ_REAL_CLAUDE")
        .expect("name the real Claude Code executable in DAGQ_REAL_CLAUDE");
    let claude = ClaudeCode {
        executable: executable.into(),
    };
    let dir = tempfile::tempdir().unwrap();
    // One review's worktree, with the planted settings and agent, and its
    // run directory; the marker the settings' hook writes.
    let review = |name: &str| -> (PathBuf, PathBuf, PathBuf) {
        let root = dir.path().join(name);
        let worktree = root.join("worktree");
        let run_dir = root.join("run");
        fs::create_dir_all(worktree.join(".claude/agents")).unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        git(&worktree, &["init", "-q"]);
        fs::write(worktree.join("a.txt"), "hello\n").unwrap();
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
            worktree.join(format!(".claude/agents/{PLANTED_AGENT}.md")),
            format!(
                "---\nname: {PLANTED_AGENT}\ndescription: planted by the worktree\n---\nReview.\n"
            ),
        )
        .unwrap();
        (worktree, run_dir, marker)
    };
    // Run `command` with Haiku, `--setting-sources` dropped for a
    // control; its reply, its `init` event and its `result` event.
    let run = |mut command: CommandSpec, control: bool, input: &Path| -> (String, Value, Value) {
        claude.select_model(&mut command, "claude-haiku-4-5-20251001", "low");
        let mut args: Vec<std::ffi::OsString> = command.get_args().map(Into::into).collect();
        let at = args.iter().position(|a| a == "--setting-sources").unwrap();
        assert_eq!(args[at + 1], "");
        if control {
            args.drain(at..=at + 1);
        }
        // The prompt is the job's standard input.
        fs::write(input, command.get_stdin().unwrap()).unwrap();
        let mut process = std::process::Command::new(command.get_program());
        process
            .args(&args)
            .current_dir(command.get_current_dir().unwrap())
            .env_remove("CLAUDECODE")
            .stdin(fs::File::open(input).unwrap())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Killed when the test ends early, and by its pid when the wait
        // below times out and the process exits without the drops.
        let job = crate::common::KillOnDrop::new(process.spawn().unwrap(), "the real Claude job");
        let output = {
            let _waiting = crate::common::within(Duration::from_secs(300), "the real Claude job");
            job.wait_with_output().unwrap()
        };
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let events: Vec<Value> = stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect();
        let event = |kind: &str| {
            events
                .iter()
                .rfind(|event| {
                    event["type"] == kind && (kind != "system" || event["subtype"] == "init")
                })
                .unwrap_or_else(|| panic!("no {kind} event in {stdout}"))
                .clone()
        };
        (claude.job_reply(&stdout), event("system"), event("result"))
    };
    let names = |init: &Value, key: &str| -> Vec<String> {
        init[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|name| name.as_str().map(str::to_owned))
            .collect()
    };

    // The whole review.
    let whole = |name: &str, control: bool| {
        let (worktree, run_dir, marker) = review(name);
        let task_run = TaskRun::restore(dagq::domain::RunRecord {
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
        let command = claude
            .review_command(
                &task_run,
                Path::new(task_run.worktree_path().unwrap()),
                "Reply with the single word OK.",
                runtime::REVIEW_ACCESS,
            )
            .unwrap();
        let (reply, ..) = run(command, control, &run_dir.join("prompt.txt"));
        (reply, marker)
    };
    let (reply, marker) = whole("whole-control", true);
    assert!(
        marker.is_file(),
        "with the default setting sources the worktree's settings load (the control): {reply}"
    );
    let (reply, marker) = whole("whole", false);
    assert!(
        !marker.is_file(),
        "the whole review loaded the worktree's settings: {reply}"
    );

    // An agent's job, as the run's review launches it: in the worktree,
    // its directory and the material in the run directory.
    let agent = |name: &str, tools: AgentTools, prompt: &str| {
        let (worktree, run_dir, marker) = review(name);
        let job_dir = run_dir.join("review-1-agents/design");
        fs::create_dir_all(&job_dir).unwrap();
        let material = run_dir.join("review.md");
        fs::write(&material, "# Review\n").unwrap();
        let launch = AgentJobLaunch {
            cwd: worktree.clone(),
            dir: job_dir.clone(),
            material,
            prompt: prompt.to_owned(),
            tools,
            reads: None,
        };
        let mut command = claude.agent_job_command(&launch).unwrap();
        claude.without_mcp(&mut command);
        (command, worktree, job_dir, marker)
    };
    let written = "written.txt";
    let write_prompt = format!(
        "Use the Write tool once to create the file `{written}` in the current directory \
         containing `x`. If it is refused or missing, do not try another way. Then reply OK."
    );
    // 1, 2 and 4.
    let reads = AgentRole::Review.default_tools();
    let (command, worktree, job_dir, marker) = agent("agent", reads, &write_prompt);
    let (reply, init, _) = run(command, false, &job_dir.join("prompt.txt"));
    assert!(
        !marker.is_file(),
        "the agent's job loaded the worktree's settings: {reply}"
    );
    assert!(
        !names(&init, "agents").contains(&PLANTED_AGENT.to_owned()),
        "the worktree's agent was given: {init}"
    );
    let tools = names(&init, "tools");
    for tool in ["Write", "Edit", "NotebookEdit", "Bash"] {
        assert!(!tools.contains(&tool.to_owned()), "{tool} in {tools:?}");
    }
    assert!(
        !worktree.join(written).exists(),
        "the agent's job wrote: {reply}"
    );
    // Their control.
    let writes = AgentTools::new([AgentTool::Read, AgentTool::Write]);
    let (command, worktree, job_dir, marker) = agent("agent-control", writes, &write_prompt);
    let (reply, init, _) = run(command, true, &job_dir.join("prompt.txt"));
    assert!(
        marker.is_file(),
        "the control loads the worktree's settings: {reply}"
    );
    assert!(
        names(&init, "agents").contains(&PLANTED_AGENT.to_owned()),
        "the control is given the worktree's agent: {init}"
    );
    assert!(
        worktree.join(written).is_file(),
        "the control writes: {reply}"
    );

    // 3: a command `permissions.deny` refuses (the identity rules), with
    // the shell declared so that only the deny can refuse it.
    let ran = "shell-ran";
    let shell_prompt = format!(
        "Run exactly this one command once with the Bash tool: \
         `export DAGQ_ROLE=worker && touch {}`. If it is refused, do not retry it \
         or run anything else. Then reply OK.",
        shell_path(ran)
    );
    let shell = || AgentTools::new([AgentTool::Read, AgentTool::Shell]);
    let (command, worktree, job_dir, _) = agent("deny", shell(), &shell_prompt);
    let settings: Value = serde_json::from_str(
        &fs::read_to_string(job_dir.join("claude-agent-job-settings.json")).unwrap(),
    )
    .unwrap();
    assert!(
        names(&settings["permissions"], "deny").contains(&"Bash(export DAGQ_ROLE*)".to_owned()),
        "{settings}"
    );
    let (reply, _, result) = run(command, false, &job_dir.join("prompt.txt"));
    assert!(
        !worktree.join(ran).exists(),
        "the deny did not hold: {reply}"
    );
    // The job did try the command and was refused, not just left it.
    let denials = result["permission_denials"]
        .as_array()
        .into_iter()
        .flatten();
    assert!(
        denials
            .into_iter()
            .any(|denial| denial["tool_name"] == "Bash"),
        "the command was not refused: {result}"
    );
    // Its control: the same job with the deny emptied.
    let (command, worktree, job_dir, _) = agent("deny-control", shell(), &shell_prompt);
    let settings = job_dir.join("claude-agent-job-settings.json");
    let mut emptied: Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
    emptied["permissions"]["deny"] = json!([]);
    fs::write(&settings, emptied.to_string()).unwrap();
    let (reply, ..) = run(command, false, &job_dir.join("prompt.txt"));
    assert!(
        worktree.join(ran).is_file(),
        "the control runs the command: {reply}"
    );
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
    use crate::runtime_support::codex::{FINISH, TASK, codex_fixture};
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
    assert!(!args.contains(".filesystem="), "{args}");
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished[0]["agents"][0]["summary"], "codex agent passed");
}

/// `[roles.review]` names no provider (the whole review runs on Claude);
/// `design` names Codex for its job and `tests` names none, both
/// required for `change.txt`; `extra` goes before them.
fn design_on_codex(repo: &Path, extra: &str) {
    fs::create_dir_all(repo.join(".dagq/agents/tests")).unwrap();
    fs::write(repo.join(".dagq/agents/tests/AGENT.md"), TESTS_DEFINITION).unwrap();
    git(repo, &["add", ".dagq/agents/tests/AGENT.md"]);
    commit_on_main(
        repo,
        &format!(
            "{extra}[review.subagents.design]\npaths = [\"change.txt\"]\nprovider = \"codex\"\n\
             [review.subagents.tests]\npaths = [\"change.txt\"]\n"
        ),
        Some(MAIN_DEFINITION),
    );
}

/// Each `review_job_started`'s agent with its launch's provider, source
/// and `switched_from`, in order.
fn job_launches(detail: &dagq::domain::TaskDetail) -> Vec<(Value, Value, Value, Value)> {
    payloads(detail, "review_job_started")
        .iter()
        .map(|job| {
            (
                job["agent"].clone(),
                job["launch"]["provider"].clone(),
                job["launch"]["source"].clone(),
                job["launch"]["switched_from"].clone(),
            )
        })
        .collect()
}

/// With the fallback on, the job of the agent that names Codex stops at
/// Codex's usage limit: Codex alone is held and that job alone starts again
/// on Claude, its launch saying it moved from Codex and still that the
/// agent's table named its provider; the run lands.
#[test]
fn an_agent_that_names_codex_moves_alone_to_claude_and_keeps_where_its_provider_came_from() {
    use crate::runtime_support::codex::{FINISH, TASK, codex_fixture};
    let (dir, repo, db, backend, codex) = codex_fixture();
    design_on_codex(&repo, "");
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
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")])
        .with_review_agents(&[
            ("tests", vec![agent_says("tests", "pass", &[])]),
            ("design", vec![agent_says("design", "pass", &[])]),
        ]);
    let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = SqliteQueue::open(&db).unwrap().show(TASK).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let subagent = || json!("review.subagents");
    assert_eq!(
        job_launches(&detail),
        [
            (Value::Null, json!("claude"), json!("default"), Value::Null),
            (json!("design"), json!("codex"), subagent(), Value::Null),
            (
                json!("tests"),
                json!("claude"),
                json!("default"),
                Value::Null
            ),
            (json!("design"), json!("claude"), subagent(), json!("codex")),
        ]
    );
    let moved = payloads(&detail, "review_job_started")[3]["launch"].clone();
    assert_eq!(moved["switch_reason"], "usage_limit", "{moved}");
    let held: Vec<Value> = SqliteQueue::open(&db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == EventKind::ProviderHeld)
        .map(|event| event.payload["provider"].clone())
        .collect();
    assert_eq!(held, [json!("codex")]);
    assert!(payloads(&detail, "review_failed").is_empty());
}

/// An agent whose `[review.subagents.<agent>]` names Codex runs its job on
/// Codex while the whole review's job and the agent that names no provider
/// run where `[roles.review]` sends the review (Claude), each job's start
/// recording its launch's provider and where it came from. With
/// `[provider_fallback] jobs = false`, the job of `design`, on Codex,
/// stops where Codex's login's refresh token ran out, said only on its
/// stderr: Codex alone is held at its login, not asked of per run, and the
/// job waits for it, not moving to Claude, while the whole review's job and
/// the other agent's answer on Claude; the review is not judged until the
/// hold ends (the login fixed) and the job starts on Codex again in a later
/// pass and answers, and the run lands. How a job moves or waits as its
/// provider and the fallback say, and which failure holds which provider,
/// is `supervise::landing::tests::
/// an_agent_that_names_its_provider_moves_or_waits_alone_as_the_fallback_says`;
/// which output is a usage limit or an expired login is
/// `infrastructure::codex_turns`' tests.
#[test]
fn with_the_fallback_off_an_agent_whose_codex_login_ran_out_waits_for_it_and_the_review_lands() {
    use crate::runtime_support::codex::{FINISH, TASK, codex_fixture};
    let stdout = "{\"type\":\"turn.failed\",\"error\":{\"message\":\"stream disconnected before completion\"}}\n";
    let stderr = "ERROR codex_login::auth::manager: Failed to refresh token: Your access token could not be refreshed because your refresh token has expired. Please log out and sign in again.\n";
    let reason = "authentication";
    let (dir, repo, db, backend, codex) = codex_fixture();
    design_on_codex(&repo, "[provider_fallback]\njobs = false\n");
    set_turns(dir.path(), FINISH);
    fs::write(dir.path().join("codex-agent-design-failure.jsonl"), stdout).unwrap();
    fs::write(dir.path().join("codex-agent-design-failure.err"), stderr).unwrap();
    let options = SuperviseOptions {
        codex,
        codex_home: Some(dir.path().join(CODEX_HOME)),
        ..supervise_options(1, true)
    };
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")])
        .with_review_agents(&[("tests", vec![agent_says("tests", "pass", &[])])]);
    let supervisor = {
        let (db, repo) = (db.clone(), repo.clone());
        thread::spawn(move || {
            let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
            let prompts: Vec<String> = reviewer.launches().into_iter().map(|l| l.prompt).collect();
            (outcome, prompts)
        })
    };
    // The others answer while Codex is held for design's job.
    let answered = |detail: &dagq::domain::TaskDetail| {
        payloads(detail, "review_job_finished")
            .iter()
            .filter(|end| end["outcome"] == "answered")
            .map(|end| end["agent"].clone())
            .collect::<Vec<_>>()
    };
    wait_until(&db, Duration::from_secs(60), |queue| {
        let detail = queue.show(TASK).unwrap();
        let held = queue
            .all_events()
            .unwrap()
            .iter()
            .any(|event| event.kind == EventKind::ProviderHeld);
        held && answered(&detail).len() == 2
    });
    let detail = SqliteQueue::open(&db).unwrap().show(TASK).unwrap();
    assert_eq!(answered(&detail), [Value::Null, json!("tests")]);
    assert!(payloads(&detail, "review_finished").is_empty());
    let held = SqliteQueue::open(&db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .find(|event| event.kind == EventKind::ProviderHeld)
        .unwrap();
    assert_eq!(held.payload["provider"], "codex");
    assert_eq!(held.payload["reason"], reason);
    SqliteQueue::open(&db)
        .unwrap()
        .record_queue_event(
            dagq::domain::EventKind::ProviderReleased,
            json!({"provider": "codex", "reason": reason,
                   "since": held.payload["since"], "why": "retry_due"}),
        )
        .unwrap();
    let (outcome, claude_agent_jobs) = joined(supervisor, "the supervisor to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(claude_agent_jobs.len(), 1, "only tests' job ran on Claude");
    assert!(claude_agent_jobs[0].contains(TESTS_DEFINITION));
    let args = fs::read_to_string(dir.path().join("codex-agent-args.log")).unwrap();
    assert!(args.starts_with("design "), "{args}");
    assert!(!args.contains("\ntests "), "{args}");
    let detail = SqliteQueue::open(&db).unwrap().show(TASK).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    let subagent = || json!("review.subagents");
    assert_eq!(
        job_launches(&detail),
        [
            (Value::Null, json!("claude"), json!("default"), Value::Null),
            (json!("design"), json!("codex"), subagent(), Value::Null),
            (
                json!("tests"),
                json!("claude"),
                json!("default"),
                Value::Null
            ),
            (json!("design"), json!("codex"), subagent(), Value::Null),
        ]
    );
    let ends: Vec<(Value, Value)> = payloads(&detail, "review_job_finished")
        .iter()
        .map(|end| (end["agent"].clone(), end["outcome"].clone()))
        .collect();
    assert!(ends.contains(&(json!("design"), json!("held"))), "{ends:?}");
    assert_eq!(
        ends.last(),
        Some(&(json!("design"), json!("answered"))),
        "{ends:?}"
    );
    assert!(payloads(&detail, "review_failed").is_empty());
    let finished = payloads(&detail, "review_finished");
    assert_eq!(finished[0]["agents"][0]["summary"], "codex agent passed");
    assert_eq!(finished[0]["agents"][1]["summary"], "tests looked");
    let held_providers: Vec<Value> = SqliteQueue::open(&db)
        .unwrap()
        .all_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == EventKind::ProviderHeld)
        .map(|event| event.payload["provider"].clone())
        .collect();
    assert!(
        held_providers.iter().all(|p| p == "codex"),
        "{held_providers:?}"
    );
}

/// A hold that is there before the review begins holds only the jobs that
/// need its provider, and the whole review that waited takes the session
/// its span names. `[roles.review]` names Claude, `[provider_fallback]
/// jobs = false`, and Claude is held (its agent did not start) while the
/// worker works; `design` names Codex. The review begins anyway:
/// `design`'s job runs on Codex and answers while the whole review's job
/// waits for Claude, the review is not judged meanwhile, and once
/// Claude's hold ends the whole review's job starts on Claude, not moved,
/// its launch saying `dagq.toml` named its provider, in the session id
/// `review_started` named: the review's span names that session from its
/// open to its close, and the run lands. Whether a review waits as a whole
/// is `supervise::landing::tests::
/// a_review_waits_as_a_whole_only_while_none_of_its_jobs_can_start`.
#[test]
fn a_whole_review_that_waited_for_claude_starts_in_the_session_its_span_names() {
    use crate::runtime_support::codex::{FINISH, TASK, codex_fixture};
    use dagq::domain::provider_switch::{ProviderHold, SwitchReason};
    let (dir, repo, db, backend, codex) = codex_fixture();
    commit_on_main(
        &repo,
        "[roles.review]\nprovider = 'claude'\n[provider_fallback]\njobs = false\n\
         [review.subagents.design]\npaths = [\"change.txt\"]\nprovider = \"codex\"\n",
        Some(MAIN_DEFINITION),
    );
    // The worker's turn finishes only once Claude is held.
    let gate = dir.path().join("claude-held");
    set_turns(
        dir.path(),
        &format!("{}; {FINISH}", crate::common::await_path(&gate)),
    );
    let options = SuperviseOptions {
        codex,
        codex_home: Some(dir.path().join(CODEX_HOME)),
        ..supervise_options(1, true)
    };
    let reviewer = TestReviewer::new(&[verdict("pass", &[], "meets the acceptance")]);
    let supervisor = {
        let (db, repo) = (db.clone(), repo.clone());
        thread::spawn(move || {
            let outcome = supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options);
            (outcome, reviewer.prompts().len())
        })
    };
    wait_until(&db, Duration::from_secs(60), |queue| {
        queue
            .show(TASK)
            .unwrap()
            .runs
            .first()
            .is_some_and(|run| run.status() == RunStatus::Running)
    });
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let hold = ProviderHold::new(
        dagq::domain::Provider::Claude,
        SwitchReason::LaunchFailed,
        now,
    );
    SqliteQueue::open(&db)
        .unwrap()
        .record_queue_event(
            dagq::domain::EventKind::ProviderHeld,
            hold.held_payload(None),
        )
        .unwrap();
    fs::write(&gate, "").unwrap();
    // design answers on Codex while the whole review waits for Claude.
    wait_until(&db, Duration::from_secs(60), |queue| {
        payloads(&queue.show(TASK).unwrap(), "review_job_finished")
            .iter()
            .any(|end| end["agent"] == "design" && end["outcome"] == "answered")
    });
    let waiting = SqliteQueue::open(&db).unwrap().show(TASK).unwrap();
    assert_eq!(payloads(&waiting, "review_started").len(), 1);
    assert_eq!(
        job_launches(&waiting),
        [(
            json!("design"),
            json!("codex"),
            json!("review.subagents"),
            Value::Null
        )]
    );
    assert!(payloads(&waiting, "review_finished").is_empty());
    let session_id = payloads(&waiting, "review_started")[0]["session_id"]
        .as_str()
        .expect("a review whose whole job will run on Claude names its session")
        .to_owned();
    SqliteQueue::open(&db)
        .unwrap()
        .record_queue_event(
            dagq::domain::EventKind::ProviderReleased,
            hold.released_payload("retry_due"),
        )
        .unwrap();
    let (outcome, claude_reviews) = joined(supervisor, "the supervisor to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    assert_eq!(claude_reviews, 1, "the whole review ran on Claude once");
    let detail = SqliteQueue::open(&db).unwrap().show(TASK).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
    assert_eq!(
        job_launches(&detail),
        [
            (
                json!("design"),
                json!("codex"),
                json!("review.subagents"),
                Value::Null
            ),
            (
                Value::Null,
                json!("claude"),
                json!("dagq.toml"),
                Value::Null
            ),
        ]
    );
    let started = payloads(&detail, "review_job_started");
    assert_eq!(started[1]["session_id"], json!(session_id));
    assert!(payloads(&detail, "review_failed").is_empty());
    // The review's one span names the session the whole job ran in.
    let reviews = |kind: &str| -> Vec<Value> {
        payloads(&detail, kind)
            .into_iter()
            .filter(|span| span["kind"] == "review")
            .cloned()
            .collect()
    };
    let opened = reviews("session_opened");
    assert_eq!(opened.len(), 1, "{opened:?}");
    assert_eq!(opened[0]["session_id"], json!(session_id));
    let closed = reviews("session_closed");
    assert_eq!(closed.len(), 1, "{closed:?}");
    assert_eq!(closed[0]["reason"], "job_finished");
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
/// eval's job and of the review's agent job are the same argv but for the
/// read restriction the eval's case gives its job (its tree and material
/// read, never the repository nor the queue's data directory): Codex
/// takes the narrowed profile of `agent_reads_config` for `--sandbox
/// read-only`, the review's agent none, and Claude Code's argv is the
/// same; their prompts (on standard input) differ only by the material's
/// lines. (b)
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
        .filter(|event| event.kind == EventKind::AgentEvalRunFinished)
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
    // The eval's case names where its job reads, the review's agent
    // nothing.
    let reads = eval.reads.as_ref().expect("the eval's job names its reads");
    assert_eq!(
        reads.readable,
        [eval.cwd.clone(), eval.material.parent().unwrap().to_owned()]
    );
    let real = |path: &Path| path.canonicalize().unwrap();
    for kept in [repo.clone(), db.parent().unwrap().to_owned()] {
        assert!(
            reads
                .unreadable
                .iter()
                .any(|path| real(path) == real(&kept)),
            "{kept:?} in {reads:?}"
        );
    }
    assert_eq!(review.reads, None);
    let unrestricted = dagq::application::AgentJobLaunch {
        reads: None,
        ..eval.clone()
    };
    let search = std::env::var_os("PATH");
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let restriction =
        dagq::infrastructure::codex::agent_reads_config(reads, search.as_deref(), home.as_deref())
            .unwrap();
    for provider in [&claude as &dyn AgentProvider, &codex] {
        let args = argv(provider, eval);
        // Without its reads the eval's argv is the review's.
        assert_eq!(argv(provider, &unrestricted), argv(provider, review));
        assert!(
            !args
                .iter()
                .any(|arg| arg.contains("You are the review agent"))
        );
        // Nothing of an agent run inside a parent job: no `--agents`, no
        // `Agent` among the allowed tools.
        assert!(!args.iter().any(|arg| arg == "--agents"), "{args:?}");
        assert!(
            !args
                .iter()
                .any(|arg| arg.split(',').any(|tool| tool == "Agent")),
            "{args:?}"
        );
    }
    assert_eq!(argv(&claude, eval), argv(&claude, review));
    let mut narrowed = argv(&codex, review);
    let at = narrowed
        .windows(2)
        .position(|pair| pair == ["--sandbox", "read-only"])
        .expect("the review's agent job is in the read-only sandbox");
    narrowed.drain(at..at + 2);
    narrowed.extend(
        restriction
            .iter()
            .flat_map(|config| ["-c".to_owned(), config.clone()]),
    );
    assert_eq!(argv(&codex, eval), narrowed);
    assert!(
        !argv(&codex, review)
            .iter()
            .any(|arg| arg.contains(".filesystem=")),
        "the review's agent job reads as its sandbox lets it"
    );
    for launch in [eval, review] {
        for word in ["Required review subagents", "\"agents\" array"] {
            assert!(!launch.prompt.contains(word), "{word}");
        }
    }
}

/// Open a replanning operation on `run` and move it past the pause's fence,
/// as the trusted runtime's diagnosis would (ADR-t1521-2).
fn pause_replan(db: &Path, run: &RunId) -> dagq::domain::task_replan::ReplanOperation {
    use dagq::domain::task_replan::{NewReplan, ReplanMove, ReplanOrigin, ReplanStatus};
    let mut queue = SqliteQueue::open(db).unwrap();
    let op = queue
        .record_replan(
            &NewReplan {
                run: run.clone(),
                reason: "two independent parts".into(),
                evidence: Vec::new(),
            },
            ReplanOrigin::Runtime,
            "supervisor",
            "supervisor",
        )
        .unwrap()
        .operation;
    queue
        .move_replan(
            op.id,
            op.generation,
            &ReplanMove {
                to: ReplanStatus::PauseRequested,
                reason: "the test pauses it".into(),
                snapshot_id: None,
                diagnosis: None,
            },
        )
        .unwrap()
}

/// Acceptance (ADR-t1521-2, the park): an agent's job that waits for its
/// provider (Codex held at its usage limit, the fallback off) does not keep
/// a run a replanning parks in its slot. Once the operation passes its
/// fence, the job that ran ends, the waiting one starts no more, the review
/// is withheld (`replan_result_withheld`), and the run is parked with Codex
/// still held.
#[test]
fn a_review_whose_agent_waits_for_its_provider_is_parked_without_starting_it() {
    use crate::runtime_support::codex::{FINISH, TASK, codex_fixture};
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
        thread::spawn(move || supervise_reviewed_with(&db, &repo, &backend, &reviewer, &options))
    };
    wait_until(&db, Duration::from_secs(60), |queue| {
        queue
            .all_events()
            .unwrap()
            .iter()
            .any(|event| event.kind == EventKind::ProviderHeld)
    });
    let run = SqliteQueue::open(&db).unwrap().show(TASK).unwrap().runs[0].clone();
    let op = pause_replan(&db, run.id());
    let outcome = joined(supervisor, "the supervisor to return");
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    assert_eq!(
        queue.replan_operation(op.id).unwrap().status,
        dagq::domain::task_replan::ReplanStatus::Parked
    );
    let detail = queue.show(TASK).unwrap();
    assert_eq!(detail.runs[0].status(), RunStatus::NeedsSession);
    let withheld = payloads(&detail, "replan_result_withheld");
    assert_eq!(withheld.len(), 1, "{withheld:?}");
    assert_eq!(withheld[0]["what"], "review");
    assert_eq!(withheld[0]["waiting"], 1);
    // The whole review's job and the agent's first run, not its run once
    // Codex is back.
    assert_eq!(payloads(&detail, "review_job_started").len(), 2);
    assert!(payloads(&detail, "review_finished").is_empty());
}

/// A person's answer to the run's `approve_landing` ask reaches the next
/// round's review across the queue: review.md, the whole review's prompt
/// and the agent's job carry it, with the ask and the round that opened
/// it, and the rule on the concerns it accepted once; the first round,
/// which no person had answered, carries none of it.
#[test]
fn a_persons_landing_answer_reaches_the_next_rounds_review_and_its_agents() {
    const ACCEPTED: &str = "A person already decided the concerns these answers accept";
    let (_dir, repo, db) = fixture();
    commit_on_main(&repo, CONFIG, Some(MAIN_DEFINITION));
    let backend = TestWorkspace::new(&db, false, IDLE_AGENT);
    let reviewer = TestReviewer::new(&[
        verdict(
            "concern",
            &["adds a permission the task did not name"],
            "scope",
        ),
        verdict("pass", &[], "fixed"),
    ])
    .with_review_agents(&[("design", vec![agent_says("design", "pass", &[])])]);
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let mut queue = SqliteQueue::open(&db).unwrap();
    let asks = queue.asks(Default::default()).unwrap();
    assert_eq!(asks.len(), 1, "{asks:?}");
    let answer = "send_back: the added permission is as designed; narrow the change";
    queue.answer(asks[0].id, answer).unwrap();
    backend.resume_script_for(
        1,
        "await_message; printf 'narrowed\\n' > change.txt; unlocked git commit -q -am narrowed; receipt \"$(git rev-parse HEAD)\"; idle; await_exit",
    );
    let outcome = supervise_reviewed(&db, &repo, &backend, &reviewer);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");
    let detail = queue.show(TaskId::new(1)).unwrap();
    assert_eq!(detail.task.status(), TaskStatus::Completed);
    let line = format!("- ask {}, after review 1: {answer}\n", asks[0].id);

    let whole = reviewer.prompts();
    assert_eq!(whole.len(), 2);
    assert!(
        !whole[0].contains(&line) && !whole[0].contains(ACCEPTED),
        "{}",
        whole[0]
    );
    assert_eq!(whole[1].matches(&line).count(), 1, "{}", whole[1]);
    assert_eq!(whole[1].matches(ACCEPTED).count(), 1, "{}", whole[1]);

    let run_dir = detail.runs[0].run_dir().unwrap();
    let material = PathBuf::from(format!("{run_dir}/review.md"));
    let agents: Vec<String> = reviewer
        .launches()
        .into_iter()
        .filter(|launch| launch.material == material)
        .map(|launch| launch.prompt)
        .collect();
    assert_eq!(agents.len(), 2, "{agents:?}");
    assert!(
        !agents[0].contains(&line) && !agents[0].contains(ACCEPTED),
        "{}",
        agents[0]
    );
    assert_eq!(agents[1].matches(&line).count(), 1, "{}", agents[1]);
    assert_eq!(agents[1].matches(ACCEPTED).count(), 1, "{}", agents[1]);
    assert_eq!(
        fs::read_to_string(format!("{run_dir}/review-2-agents/design/prompt.txt")).unwrap(),
        agents[1]
    );

    // review.md is the last round's: it carries the answer before the
    // commits, and none of the first round's findings.
    let material = fs::read_to_string(&material).unwrap();
    let section = material
        .split("## A person's answers to this run's landing asks")
        .nth(1)
        .and_then(|rest| rest.split("## Commits").next())
        .unwrap_or_else(|| panic!("no answers in {material}"));
    assert!(section.contains(&line), "{section}");
    assert!(
        !section.contains("adds a permission the task did not name"),
        "{section}"
    );
}
