//! The eval of a built-in agent through the supervisor loop (ADR-t2259-1
//! decision 10): `dagq agent eval builtin:<name>` measures the binary's
//! definition on the cases the binary carries, each case's tree a
//! repository of its own made from its baseline and patch, in a repository
//! with none of dagq's history, which the round neither reads nor changes.
//! The agent job is played by the eval's stub provider. Which cases are
//! carried and how a case's commits and digest are made are unit tests of
//! their modules; these are the boundary with the queue, Git and the
//! processes.

use crate::common::{self, shell_path};
use crate::plan_review::{fixture, git};
use crate::runtime_support::agent_eval::{EvalAgent, clean, events, supervise, violation};
use crate::runtime_support::git_out;

use dagq::domain::EventKind;
use dagq::domain::agent_eval::CaseBase;
use dagq::domain::review_builtins::{BuiltinAgent, evals};
use dagq::infrastructure::adapters::GitRepository;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

/// Register a worktree of `repo` whose directory is gone: one
/// `git worktree prune` would drop, so a round that ran Git in the
/// repository would change its worktrees' list.
fn stale_worktree(repo: &Path) {
    let dir = repo.parent().unwrap().join("stale-worktree");
    git(
        repo,
        &["worktree", "add", "-q", "--detach", dir.to_str().unwrap()],
    );
    fs::remove_dir_all(&dir).unwrap();
    assert!(
        git_out(repo, &["worktree", "list", "--porcelain"]).contains("prunable"),
        "a stale worktree is registered"
    );
}

/// What the round must leave as it was in the repository the eval runs
/// in: its refs, its worktrees and its `HEAD`.
fn repository_state(repo: &Path) -> (String, String, String) {
    (
        git_out(repo, &["for-each-ref", "--format=%(refname) %(objectname)"]),
        git_out(repo, &["worktree", "list", "--porcelain"]),
        git_out(repo, &["rev-parse", "HEAD"]),
    )
}

/// A dev and a holdout round of `builtin:security`, asked for in a
/// repository whose one commit has nothing of dagq: each run's job starts
/// on a tree that is a repository of its own, holding a carried baseline
/// with the case's patch committed on it, whose material's commits and
/// diff are that repository's; both rounds' scores are recorded with the
/// build and the definition's digest; the repository's refs, worktrees and
/// `HEAD` are the same after, its stale worktree still registered: the
/// trees are removed without Git in the repository (acceptance 2).
#[test]
fn a_builtin_is_measured_on_its_carried_cases_in_a_repository_of_their_own() {
    let fx = fixture();
    assert_eq!(git_out(&fx.repo, &["rev-list", "--count", "HEAD"]), "1");
    stale_worktree(&fx.repo);
    let before = repository_state(&fx.repo);
    // One case of each split, one on each carried baseline: a held-out
    // split runs a part of its cases only as a person's rerun.
    let dev = common::cli::ok(
        &fx.db,
        &[
            "agent",
            "eval",
            "builtin:security",
            "--cases",
            "sec-share-notes",
        ],
    );
    let holdout = common::cli::ok(
        &fx.db,
        &[
            "agent",
            "eval",
            "builtin:security",
            "--split",
            "holdout",
            "--cases",
            "sec-ledger-report",
            "--rerun",
            "--k",
            "1",
        ],
    );
    assert_eq!(dev["split"], "dev");
    assert_eq!(holdout["split"], "holdout");
    let out = tempfile::tempdir().unwrap();
    let agent = EvalAgent::new(out.path(), &[violation(), clean()]);
    let outcome = supervise(&fx, &agent);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let definition = BuiltinAgent::Security.definition();
    let digest = format!("{:x}", Sha256::digest(definition.as_bytes()));
    let builtin = json!({"builtin": "builtin:security", "build": dagq::VERSION, "digest": digest});
    let started = events(&fx.db, EventKind::AgentEvalStarted);
    assert_eq!(started.len(), 2, "{started:?}");
    for round in &started {
        assert_eq!(round["builtin"], builtin, "{round}");
        assert_eq!(round["definition_digest"], digest);
        assert_eq!(round["threshold"], 0.9);
    }
    assert_eq!(started[0]["cases"], json!(["sec-share-notes"]));
    assert_eq!(started[1]["cases"], json!(["sec-ledger-report"]));

    let launches = agent.launches();
    assert_eq!(launches.len(), 2);
    let trees = agent.trees_git.lock().unwrap().clone();
    for (n, (launch, tree)) in launches.iter().zip(&trees).enumerate() {
        let stdin = agent.stdin(n);
        assert!(stdin.contains("Sending out (`send-out`)"), "{stdin}");
        // The tree's Git is its own: two commits, the base on main and
        // the change at a detached HEAD, both those of the material.
        assert!(tree.own_git_dir && tree.links.is_empty(), "{tree:?}");
        assert!(tree.has_base && tree.has_head, "{tree:?}");
        assert_eq!(tree.commits.len(), 2, "{tree:?}");
        assert_eq!(tree.refs, ["refs/heads/main"], "{tree:?}");
        assert!(!tree.changed.is_empty(), "{tree:?}");
        assert!(!tree.objects.contains("seed"), "nothing of the repository");
        let material = fs::read_to_string(&launch.material).unwrap();
        assert!(material.contains(&tree.commits[0]), "{material}");
        assert!(material.contains("diff --git"), "{material}");
        let file = ["notes/api.py", "src/ledger.js"][n];
        assert!(
            tree.paths.contains(file),
            "{file} of a carried baseline: {tree:?}"
        );
        assert!(!tree.paths.contains("seed.txt"), "{tree:?}");
    }
    let scores = events(&fx.db, EventKind::AgentEvalFinished);
    assert_eq!(scores.len(), 2);
    for (scores, round) in scores.iter().zip(&started) {
        assert_eq!(scores["outcome"], "complete", "{scores}");
        assert_eq!(scores["builtin"], builtin, "{scores}");
        assert_eq!(scores["case_set_digest"], round["case_set_digest"]);
        assert_eq!(scores["program_stopped"]["count"], 0);
    }
    assert_eq!(repository_state(&fx.repo), before);
}

/// A built-in's round gives its jobs neither the project's rules for it
/// (`.dagq/review/builtin-<name>.md`) nor the project's program reviews,
/// even one that would fail every case (acceptance 4).
#[test]
fn a_builtins_round_gets_neither_the_projects_rules_nor_its_programs() {
    let fx = fixture();
    let rules = fx.repo.join(".dagq/review");
    fs::create_dir_all(&rules).unwrap();
    fs::write(
        rules.join("builtin-tests.md"),
        "Only this project's rule: PROJECT-RULE-MARKER.\n",
    )
    .unwrap();
    let scripts = fx.repo.join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    let ran = fx.repo.join("program-ran");
    fs::write(
        scripts.join("mark.sh"),
        format!("#!/bin/sh\ntouch {}\nexit 1\n", shell_path(&ran)),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(scripts.join("mark.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(
        fx.repo.join("dagq.toml"),
        "[review.programs.mark]\nscript = \"scripts/mark.sh\"\npaths = [\"**\"]\n",
    )
    .unwrap();
    git(&fx.repo, &["add", "."]);
    git(&fx.repo, &["commit", "-q", "-m", "rules and a program"]);
    common::cli::ok(
        &fx.db,
        &[
            "agent",
            "eval",
            "builtin:tests",
            "--cases",
            "tests-readme-only",
        ],
    );
    let out = tempfile::tempdir().unwrap();
    let agent = EvalAgent::new(out.path(), &[clean()]);
    let outcome = supervise(&fx, &agent);
    assert_eq!(outcome["errors"], json!([]), "{outcome}");

    let launches = agent.launches();
    assert_eq!(launches.len(), 1);
    for n in 0..launches.len() {
        let stdin = agent.stdin(n);
        assert!(!stdin.contains("PROJECT-RULE-MARKER"), "{stdin}");
        assert!(stdin.contains("Stability (`unstable-test`)"), "{stdin}");
    }
    assert!(events(&fx.db, EventKind::AgentEvalCaseChecked).is_empty());
    assert!(!ran.exists(), "no program ran");
    let scores = &events(&fx.db, EventKind::AgentEvalFinished)[0];
    assert_eq!(scores["outcome"], "complete", "{scores}");
    assert_eq!(scores["builtin"]["builtin"], "builtin:tests");
}

/// A carried case's tree, made by the Git adapter, is a repository of its
/// own whose two commits the same baseline and patch make the same every
/// time and another baseline makes others; and the repository the eval
/// runs in keeps its refs, worktrees (a stale one too) and `HEAD`
/// (ADR-t2259-1 decision 10, acceptance 3).
#[test]
fn carried_case_trees_are_made_the_same_and_apart_from_the_repository() {
    let fx = fixture();
    stale_worktree(&fx.repo);
    let before = repository_state(&fx.repo);
    let repository = GitRepository::inspect(&fx.repo).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let patch = dir.path().join("change.patch");
    fs::write(
        &patch,
        "diff --git a/app/a.txt b/app/a.txt\n--- a/app/a.txt\n+++ b/app/a.txt\n@@ -1 +1,2 @@\n one\n+two\n",
    )
    .unwrap();
    let make = |name: &str, files: &[(&str, &str)], patch: &Path| {
        let diff = dir.path().join(format!("{name}.diff"));
        let tree = dir.path().join(name).join("tree");
        let made = repository
            .add_baseline_case_tree(&tree, files, patch, &diff)
            .unwrap_or_else(|error| panic!("{name}: {error:#}"));
        (made, tree, fs::read_to_string(diff).unwrap())
    };
    let files = [("app/a.txt", "one\n"), ("README.md", "a\n")];
    let (first, tree, diff) = make("first", &files, &patch);
    let (again, _, _) = make("again", &files, &patch);
    assert_eq!((&first.base, &first.head), (&again.base, &again.head));
    assert_ne!(first.base, first.head);
    assert!(diff.contains("+two"), "{diff}");
    assert_eq!(first.commits.lines().count(), 1, "{}", first.commits);
    assert_eq!(
        fs::read_to_string(tree.join("app/a.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert!(tree.join(".git").is_dir());
    let (other, _, _) = make(
        "other",
        &[("app/a.txt", "one\n"), ("README.md", "b\n")],
        &patch,
    );
    assert_ne!(other.base, first.base);
    // A tree left at the place is made again, the same.
    let (remade, _, _) = make("first", &files, &patch);
    assert_eq!(remade.head, first.head);

    assert_eq!(repository_state(&fx.repo), before);
}

/// Every case `agent` carries makes its tree: its patch applies to its
/// baseline and changes it (acceptance 1).
fn every_carried_case_makes_its_tree(agent: BuiltinAgent) {
    let fx = fixture();
    let repository = GitRepository::inspect(&fx.repo).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for list in evals::cases(agent).unwrap().files {
        for case in list.cases {
            let CaseBase::Baseline(name) = &case.base else {
                panic!("{} has a base commit", case.id)
            };
            let patch = dir.path().join(format!("{}.patch", case.id));
            fs::write(&patch, evals::patch(&case.patch).unwrap()).unwrap();
            let diff = dir.path().join(format!("{}.diff", case.id));
            let made = repository
                .add_baseline_case_tree(
                    &dir.path().join(&case.id),
                    evals::baseline(name).unwrap(),
                    &patch,
                    &diff,
                )
                .unwrap_or_else(|error| panic!("{}: {error:#}", case.id));
            assert_ne!(made.base, made.head, "{}", case.id);
            assert!(
                fs::read_to_string(&diff).unwrap().contains("diff --git"),
                "{}",
                case.id
            );
        }
    }
}

#[test]
fn every_carried_case_of_builtin_security_makes_its_tree() {
    every_carried_case_makes_its_tree(BuiltinAgent::Security);
}

#[test]
fn every_carried_case_of_builtin_tests_makes_its_tree() {
    every_carried_case_makes_its_tree(BuiltinAgent::Tests);
}
