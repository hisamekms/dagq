use super::*;
use crate::application::role_snapshot::{self, *};

fn roundtrip(snapshot: &Snapshot) -> Snapshot {
    let json = serde_json::to_vec(snapshot).unwrap();
    let read: Snapshot = serde_json::from_slice(&json).unwrap();
    assert_eq!(
        serde_json::to_value(&read).unwrap(),
        serde_json::to_value(snapshot).unwrap()
    );
    read
}

#[test]
fn plan_inputs_roundtrip_and_saved_work_survives_its_removal() {
    let files = MemoryFiles::default();
    let case = plan_case(3, 2);
    let proposal: Proposal = serde_json::from_value(json!({
        "id": 1, "status": "submitted", "owner": {"origin": "person", "workspace_id": null},
        "submitted_at": "now", "revise_count": 0, "task_ids": [1], "goal_ids": [],
        "created_at": "", "updated_at": ""
    }))
    .unwrap();
    let brief = huge_replan_brief();
    let original = Path::new("/q/replans/saved");
    files.create_dir_all(&original.join("untracked")).unwrap();
    files
        .write(&original.join("manifest.json"), b"the saved manifest")
        .unwrap();
    files
        .write(&original.join("untracked/new.bin"), b"\0\xffsaved")
        .unwrap();
    let goals = vec![goal_of(1, 500)];
    let lint = vec![LintViolation {
        code: crate::domain::lint::LintCode::BlankAcceptance,
        task_id: TaskId::new(1),
        reason: "missing".into(),
    }];
    let others = vec![(
        proposal.clone(),
        vec![long_task(99, TaskStatus::Draft, &["src/**"])],
    )];
    let language = Some(crate::domain::language::Language {
        tag: "ja".into(),
        source: crate::domain::language::LanguageSource::Repository,
    });
    let material = PlanReviewMaterial {
        proposal: &proposal,
        tasks: &case.tasks,
        goals: &goals,
        lint: &lint,
        others: &others,
        queued: &case.queued,
        queued_left_out: case.left_out,
        expected: &case.expected,
        precedents: &case.precedents,
        hotspots: &case.hotspots,
        candidates: &case.candidates,
        repo_root: Path::new("/repo"),
        language: language.as_ref(),
        replacement: Some((&brief, Some(original))),
    };
    let production = plan_review_prompt(&material).unwrap().text;
    let snapshot = Snapshot::capture(
        &files,
        SHA.into(),
        Input::Plan(Box::new(PlanInput::capture(&material))),
        None,
    )
    .unwrap();
    let snapshot = roundtrip(&snapshot);
    let Input::Plan(input) = &snapshot.input else {
        panic!("plan")
    };
    assert_eq!(input.build().unwrap().text, production);
    files.remove_dir_all(original).unwrap();
    let replay = snapshot
        .for_case(&files, Path::new("/case/material"), Path::new("/case/tree"))
        .unwrap();
    assert_eq!(
        files
            .read(Path::new("/case/material/replacement/untracked/new.bin"))
            .unwrap(),
        b"\0\xffsaved"
    );
    assert_eq!(
        files
            .read(Path::new("/case/material/replacement/manifest.json"))
            .unwrap(),
        b"the saved manifest"
    );
    assert_eq!(
        replay.build().unwrap(),
        production
            .replace("/q/replans/saved", "/case/material/replacement")
            .replace("/repo", "/case/tree")
    );
}

#[test]
fn goal_inputs_roundtrip_with_the_same_production_prompt() {
    let language = Some(crate::domain::language::Language {
        tag: "ja".into(),
        source: crate::domain::language::LanguageSource::User,
    });
    let material = GoalReviewMaterial {
        goal: json!({"id": 42, "title": "goal", "acceptance": "works"}),
        tasks: vec![
            json!({"id": 1, "status": "completed", "title": "done", "landed": {"summary": "yes"}}),
        ],
        replacements: vec![json!({"original": 1, "children": [2]})],
        follow_ups: vec![json!({"task_id": 3})],
        events: vec![json!({"kind": "note", "payload": {"text": "a decision"}})],
        previous: vec![json!({"verdict": "gaps", "summary": "earlier"})],
        gaps_in_a_row: 2,
        repo_root: Path::new("/repo"),
    };
    let production = goal_review_prompt(&material)
        .with_language(language.as_ref())
        .text;
    let snapshot = roundtrip(
        &Snapshot::capture(
            &MemoryFiles::default(),
            SHA.into(),
            Input::Goal(Box::new(GoalInput::capture(material, language))),
            None,
        )
        .unwrap(),
    );
    let Input::Goal(input) = snapshot.input else {
        panic!("goal")
    };
    assert_eq!(input.build().text, production);
}

#[test]
fn whole_run_inputs_and_each_attempts_full_material_survive_overwrites() {
    let files = MemoryFiles::default();
    let task = verified_task(7, "work", TaskStatus::InProgress, vec!["make gate".into()]);
    let run = run_on(Provider::Codex, WorkerMode::Headless);
    let answers = PersonAnswers {
        answers: vec![PersonAnswer {
            ask: crate::domain::AskId::new(1),
            round: Some(1),
            answer: "accepted concern".into(),
        }],
        older: 2,
    };
    let rules = "rules\n".repeat(RUN_REVIEW_RULES_BYTES);
    let input = RunInput {
        task: Whole(task.clone()),
        run: run.clone(),
        review_path: "/run/review.md".into(),
        known_failures: Some(KnownFailures {
            others: vec![KnownFailure {
                name: "test".into(),
                url: None,
                fix_tasks: vec![],
            }],
            fixing: vec![],
            file: "/run/known.json".into(),
        }),
        rules: Some(RulesInput {
            path: ".dagq/review/builtin-acceptance.md".into(),
            commit: SHA.into(),
            text: rules.clone(),
            whole_at: "/run/rules.md".into(),
        }),
        answers: answers.clone(),
        language: None,
    };
    files
        .write(Path::new("/run/rules.md"), rules.as_bytes())
        .unwrap();
    files
        .write(Path::new("/run/known.json"), b"the whole list")
        .unwrap();
    let production = review_prompt(
        &task,
        &run,
        &input.review_path,
        input.known_failures.as_ref(),
        Some(AcceptanceRules {
            path: &input.rules.as_ref().unwrap().path,
            commit: SHA,
            text: &rules,
            whole_at: "/run/rules.md",
        }),
        &answers,
    )
    .text;
    let mut saved = Vec::new();
    for bytes in [b"attempt one\0\xff".as_slice(), b"attempt two".as_slice()] {
        files.write(Path::new("/run/review.md"), bytes).unwrap();
        let snapshot = roundtrip(
            &Snapshot::capture(
                &files,
                SHA.into(),
                Input::Run(Box::new(input.clone())),
                None,
            )
            .unwrap(),
        );
        let Input::Run(read) = &snapshot.input else {
            panic!("run")
        };
        assert_eq!(read.build().text, production);
        saved.push((snapshot, bytes));
    }
    files.remove_file(Path::new("/run/review.md")).unwrap();
    files.remove_file(Path::new("/run/rules.md")).unwrap();
    files.remove_file(Path::new("/run/known.json")).unwrap();
    for (snapshot, bytes) in saved {
        let replay = snapshot
            .for_case(&files, Path::new("/case/material"), Path::new("/case/tree"))
            .unwrap();
        assert_eq!(
            files.read(Path::new("/case/material/review.md")).unwrap(),
            bytes
        );
        assert_eq!(
            files
                .read(Path::new("/case/material/builtin-acceptance.md"))
                .unwrap(),
            rules.as_bytes()
        );
        assert_eq!(
            files
                .read(Path::new("/case/material/known-failures.json"))
                .unwrap(),
            b"the whole list"
        );
        assert_eq!(
            replay.build().unwrap(),
            production
                .replace("/run/review.md", "/case/material/review.md")
                .replace("/run/rules.md", "/case/material/builtin-acceptance.md")
                .replace("/run/known.json", "/case/material/known-failures.json")
        );
    }
}

#[test]
fn change_refusals_are_chosen_before_a_case_can_run() {
    let change = ReviewChange {
        base: "base".into(),
        head: "head".into(),
        tree_commit: "head".into(),
        tree: Some("tree".into()),
        patch: "change.patch".into(),
    };
    assert_eq!(
        role_snapshot::refusal(&change, true, Some(b"patch"), Some("tree")),
        None
    );
    for (reachable, patch, rebuilt, reason) in [
        (
            false,
            Some(b"patch".as_slice()),
            None,
            "base is not reachable",
        ),
        (true, None, None, "missing or empty"),
        (true, Some(b"".as_slice()), None, "missing or empty"),
        (
            true,
            Some(b"patch".as_slice()),
            Some("wrong"),
            "differs from recorded tree",
        ),
    ] {
        assert!(
            role_snapshot::refusal(&change, reachable, patch, rebuilt)
                .unwrap()
                .contains(reason)
        );
    }
    let mut mismatch = change;
    mismatch.tree_commit = "other".into();
    assert!(
        role_snapshot::refusal(&mismatch, true, Some(b"patch"), None)
            .unwrap()
            .contains("differs from range head")
    );
}
