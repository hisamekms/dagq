//! Runtime tests: the review's subagents (ADR-t1453-1). The supervisor
//! reads `[review.subagents.<agent>]` and the agents' definitions from the
//! landing branch's commit, never from the run's worktree or the main
//! checkout's files, selects the agents the reviewed diff requires, and
//! hands them to the review job; a review whose required agents cannot be
//! known does not pass. A repository without the table reviews as before.
use crate::runtime_support;

use runtime_support::*;
use sha2::{Digest, Sha256};

/// What the main checkout commits: `design` reviews `change.txt` (the
/// stand-in worker's change); `unused` matches nothing and has no
/// definition, which a review that does not select it never needs.
const CONFIG: &str = "[review.subagents.design]\npaths = [\"change.txt\", \"docs/**\"]\n\
                      [review.subagents.unused]\npaths = [\"nothing/**\"]\n";
const DEFINITION: &str = ".dagq/review-agents/design.md";
const MAIN_DEFINITION: &str = "---\ndescription: main's design checks\n---\nCheck the design.\n";

/// Commit `config` as `dagq.toml` and, when given, the definition of
/// `design` on main; main's new head.
fn commit_on_main(repo: &Path, config: &str, definition: Option<&str>) -> String {
    fs::write(repo.join("dagq.toml"), config).unwrap();
    git(repo, &["add", "dagq.toml"]);
    if let Some(definition) = definition {
        fs::create_dir_all(repo.join(".dagq/review-agents")).unwrap();
        fs::write(repo.join(DEFINITION), definition).unwrap();
        git(repo, &["add", DEFINITION]);
    }
    git(repo, &["commit", "-q", "-m", "review subagents"]);
    git_out(repo, &["rev-parse", "main"])
}

/// Review the first run, whose worker runs `worker`, with `verdicts`.
fn reviewed(
    repo: &Path,
    db: &Path,
    worker: &str,
    verdicts: &[String],
) -> (TestReviewer, dagq::domain::TaskDetail) {
    let backend = TestWorkspace::new(db, false, worker);
    let reviewer = TestReviewer::new(verdicts);
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

/// What the job reads beside review.md, and what its prompt says of it.
fn assert_job_input(reviewer: &TestReviewer, detail: &dagq::domain::TaskDetail, main: &str) {
    let run_dir = detail.runs[0].run_dir().unwrap();
    let input = format!("{run_dir}/review-subagents-1.json");
    let written: Value = serde_json::from_str(&fs::read_to_string(&input).unwrap()).unwrap();
    let mut expected = expected_snapshot(main, detail);
    expected["agents"][0]["text"] = json!(MAIN_DEFINITION);
    assert_eq!(written, expected);
    // review.md shows the range the agents were selected from.
    let material = fs::read_to_string(format!("{run_dir}/review.md")).unwrap();
    let base = format!("- base: {}", expected["base"].as_str().unwrap());
    assert!(material.contains(&base), "{base} in {material}");
    let prompt = &reviewer.prompts()[0];
    for part in [
        "Required review subagents: ".to_owned(),
        format!("committed on the landing branch at {main}, are in {input}"),
        "- design (changed: change.txt)\n".to_owned(),
    ] {
        assert!(prompt.contains(&part), "{part} in {prompt}");
    }
}

/// (a) A worker that rewrites or deletes `dagq.toml` and the definition
/// on its branch neither drops nor changes its required review: main's
/// commit decides, and the job reads main's definition.
#[test]
fn the_worker_cannot_change_or_drop_its_required_review() {
    for tamper in [
        // Narrow the agent's paths away and rewrite its definition.
        "printf '[review.subagents.design]\\npaths = [\"nothing/**\"]\\n' > dagq.toml; \
         printf 'the worker s checks\\n' > .dagq/review-agents/design.md; \
         git commit -q -am tamper",
        // Delete the table and the definition.
        "printf '[run.env]\\n' > dagq.toml; git rm -q .dagq/review-agents/design.md; \
         git commit -q -am tamper",
        // Delete dagq.toml itself and the definition.
        "git rm -q dagq.toml .dagq/review-agents/design.md; git commit -q -m tamper",
    ] {
        let (_dir, repo, db) = fixture();
        let main = commit_on_main(&repo, CONFIG, Some(MAIN_DEFINITION));
        let worker =
            format!("{tamper}; commit work; receipt \"$(git rev-parse HEAD)\"; idle; await_exit");
        let (reviewer, detail) = reviewed(
            &repo,
            &db,
            &worker,
            &[verdict("pass", &[], "meets the acceptance")],
        );
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
/// committed definition is the one handed on.
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
        &[verdict("concern", &["a finding"], "look")],
    );
    let started = payloads(&detail, "review_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["subagents"], expected_snapshot(&main, &detail));
    assert_job_input(&reviewer, &detail, &main);
    assert!(payloads(&detail, "review_failed").is_empty());
}

/// The reviewed range is Git's, without rename detection: the old path of
/// a rename out of `docs/` and a deleted path under it select `design`.
#[test]
fn the_old_path_of_a_rename_and_a_deleted_path_select_an_agent() {
    for (change, matched) in [
        ("git mv docs/old.md moved.md", "docs/old.md"),
        ("git rm -q docs/old.md", "docs/old.md"),
    ] {
        let (_dir, repo, db) = fixture();
        fs::create_dir_all(repo.join("docs")).unwrap();
        fs::write(repo.join("docs/old.md"), "old\n").unwrap();
        git(&repo, &["add", "docs/old.md"]);
        let main = commit_on_main(&repo, CONFIG, Some(MAIN_DEFINITION));
        let worker = format!(
            "{change}; git commit -q -m change; receipt \"$(git rev-parse HEAD)\"; idle; await_exit"
        );
        let (_reviewer, detail) = reviewed(
            &repo,
            &db,
            &worker,
            &[verdict("concern", &["a finding"], "look")],
        );
        let started = payloads(&detail, "review_started");
        assert_eq!(started.len(), 1, "{change}");
        let subagents = &started[0]["subagents"];
        assert_eq!(subagents["commit"], main.as_str());
        assert_eq!(subagents["agents"][0]["agent"], "design", "{change}");
        assert_eq!(
            subagents["agents"][0]["paths"],
            json!([matched]),
            "{change}"
        );
    }
}

/// (c) An agent the diff selects whose definition is not in main's
/// commit fails the review to the person with why: no job runs, the run
/// does not land, and `review_failed` and the ask say which definition.
#[test]
fn a_selected_agent_without_its_definition_fails_the_review() {
    let (_dir, repo, db) = fixture();
    let main = commit_on_main(&repo, CONFIG, None);
    let (reviewer, detail) = reviewed(
        &repo,
        &db,
        IDLE_AGENT,
        &[verdict("pass", &[], "would pass")],
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
}

/// A `dagq.toml` on main that cannot be parsed fails the review too: the
/// required checks are not known.
#[test]
fn an_unparsable_committed_config_fails_the_review() {
    let (_dir, repo, db) = fixture();
    commit_on_main(&repo, "[review.subagents.design]\n", None);
    // The main checkout's own file parses, so the supervisor starts.
    fs::write(repo.join("dagq.toml"), CONFIG).unwrap();
    let (reviewer, detail) = reviewed(
        &repo,
        &db,
        IDLE_AGENT,
        &[verdict("pass", &[], "would pass")],
    );
    assert!(reviewer.prompts().is_empty());
    assert_eq!(detail.runs[0].status(), RunStatus::AwaitingIntegration);
    let failed = payloads(&detail, "review_failed");
    assert_eq!(failed.len(), 1);
    let error = failed[0]["error"].as_str().unwrap();
    assert!(
        error.contains("parse dagq.toml in the landing branch's commit")
            && error.contains("[review.subagents.design] has no paths"),
        "{error}"
    );
}

/// (d) Without `[review.subagents]` the review is as before: no
/// `subagents` in `review_started`, no job input beside review.md, no
/// word of subagents in the prompt. With the table but a diff it does not
/// select, the event records the empty selection and the rest is as
/// before.
#[test]
fn a_review_without_required_agents_is_unchanged() {
    for config in [None, Some(CONFIG)] {
        let (_dir, repo, db) = fixture();
        let main = config.map(|config| commit_on_main(&repo, config, Some(MAIN_DEFINITION)));
        // A worker whose change no glob matches.
        let worker = "printf 'x\\n' > other.txt; git add other.txt; git commit -q -m other; \
                      receipt \"$(git rev-parse HEAD)\"; idle; await_exit";
        let (reviewer, detail) = reviewed(
            &repo,
            &db,
            worker,
            &[verdict("pass", &[], "meets the acceptance")],
        );
        assert_eq!(detail.runs[0].status(), RunStatus::Integrated);
        let started = payloads(&detail, "review_started");
        assert_eq!(started.len(), 1);
        match &main {
            None => assert!(started[0].get("subagents").is_none(), "{started:?}"),
            Some(main) => assert_eq!(
                started[0]["subagents"],
                json!({
                    "commit": main,
                    "base": detail.runs[0].base_commit().as_str(),
                    "head": reviewed_head(&detail),
                    "agents": [],
                })
            ),
        }
        let run_dir = detail.runs[0].run_dir().unwrap();
        assert!(!Path::new(&format!("{run_dir}/review-subagents-1.json")).exists());
        let prompt = &reviewer.prompts()[0];
        assert!(!prompt.contains("subagent"), "{prompt}");
        let material = fs::read_to_string(format!("{run_dir}/review.md")).unwrap();
        assert!(!material.contains("subagent"), "{material}");
    }
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
            let path = dagq::domain::review_subagents::definition_path(&agent.name);
            repository.file_in(commit, &path).unwrap().is_none()
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
    fs::create_dir_all(repo.join(".dagq/review-agents")).unwrap();
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
