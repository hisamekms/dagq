//! One role round from the authorized request through the production
//! executor to the score event, using only port fakes.
use super::{agent::Script, harness::Wiring};
use crate::application::agent_eval::{AgentEvals, EvalRequest, EvalStore, LaunchSettings};
use crate::application::commands::DenialLog;
use crate::domain::actor_model::AgentModel;
use crate::domain::agent_eval::Split;
use crate::domain::{ActorContext, EventId, EventKind, RunEvent, StaticPolicy};
use anyhow::Result;
use serde_json::{Value, json};
use std::path::Path;

struct Requested<'a> {
    store: &'a dyn EvalStore,
    settings: LaunchSettings,
}
impl DenialLog for Requested<'_> {
    fn record_denial(&self, value: Value) -> Result<()> {
        self.store.record_denial(value)
    }
}
impl EvalStore for Requested<'_> {
    fn record_eval_event(&self, k: EventKind, p: Value) -> Result<EventId> {
        self.store.record_eval_event(k, p)
    }
    fn eval_events(&self) -> Result<Vec<RunEvent>> {
        self.store.eval_events()
    }
    fn launch_settings(&self, _: &str) -> Result<Option<LaunchSettings>> {
        Ok(Some(self.settings.clone()))
    }
}

#[test]
fn plan_round_uses_its_recorded_main_production_job_and_records_launch_and_scores() {
    role_round("plan_review");
}

#[test]
fn goal_round_uses_its_recorded_main_production_job_and_records_launch_and_scores() {
    role_round("goal_review");
}

fn role_round(role: &str) {
    let goal = role == "goal_review";
    let pass = json!({"verdict":"pass","reasons":[],"summary":"sound"});
    let revise =
        json!({"verdict":"revise","reasons":[{"text":"fix","codes":["P-1"]}],"summary":"fix"});
    let concern =
        json!({"verdict":"concern","reasons":[{"text":"ask","codes":["P-1"]}],"summary":"ask"});
    let (pass, revise, concern) = if goal {
        (
            json!({"verdict":"achieved","criteria":[{"criterion":"G-1","met":true}]}),
            json!({"verdict":"gaps","criteria":[{"criterion":"G-1","met":false}],"gaps":[{"title":"fix"}]}),
            json!({"verdict":"ask","criteria":[{"criterion":"G-1","met":false}],"question":"scope?"}),
        )
    } else {
        (pass, revise, concern)
    };
    let w = Wiring::new(
        "role_eval",
        Script {
            receipts: vec![],
            verdicts: vec![
                pass.clone(),
                pass,
                revise.clone(),
                revise,
                concern.clone(),
                concern,
            ],
        },
    )
    // Not a run's review stage: the reviewer's limit, whatever this says.
    .with_review_agent_timeout(321);
    let main = w.git.main().to_string();
    let snapshot = if goal {
        crate::application::role_snapshot::Snapshot {
            main_commit: main.clone(),
            change: None,
            materials: vec![],
            input: crate::application::role_snapshot::Input::Goal(Box::new(
                crate::application::role_snapshot::GoalInput {
                    goal: json!({"id":42,"title":"goal","acceptance":"G-1"}),
                    tasks: vec![json!({"id":1,"status":"completed","result":"done"})],
                    follow_ups: vec![json!({"task_id":3,"judgement":"out_of_scope"})],
                    replacements: vec![],
                    events: vec![],
                    previous: vec![],
                    gaps_in_a_row: 0,
                    repo_root: "/production/repository".into(),
                    language: None,
                },
            )),
        }
    } else {
        crate::application::role_fixtures::plan_snapshot(&main)
    };
    let values = if goal {
        ["achieved", "gaps", "ask"]
    } else {
        ["pass", "revise", "concern"]
    };
    let code = if goal { "G-1" } else { "P-1" };
    let members = values.map(|verdict| {
        json!({"id":verdict,"source":"handmade","made_by":"test","snapshot":snapshot,
            "expected":{"verdict":verdict,"codes":if verdict==values[0] {vec![]}else{vec![code]}}})
    });
    let cases = json!({"role":role,"codes":[code],"k":1,"cases": members});
    // Main moves after the record. The case must still request the old main.
    w.git.move_main();
    let config = format!("[roles.{role}]\nprovider='claude'\n[roles.review]\nprovider='codex'\n");
    w.verifier.set_config(&config);
    w.git.commit_files(
        &[
            ("dagq.toml", config.as_str()),
            (
                &format!(".dagq/role-evals/{role}/dev.json"),
                &cases.to_string(),
            ),
        ],
        &[],
    );
    let queue = w.queue();
    let store = Requested {
        store: &queue,
        settings: LaunchSettings {
            roles: crate::infrastructure::run_env::parse_config(&config)
                .unwrap()
                .roles,
            ..Default::default()
        },
    };
    let actor = ActorContext::instance(crate::domain::actor::ActorRole::User, "user");
    let evals = AgentEvals::new(&store, &actor, &StaticPolicy);
    let requested = evals
        .request(&EvalRequest {
            agent: format!("role:{role}"),
            split: Split::Dev,
            k: Some(2),
            cases: None,
            set: None,
            rerun: false,
            launch: AgentModel {
                provider: Some(crate::domain::Provider::Claude),
                model: Some("claude-haiku-5-5".into()),
                effort: Some("low".into()),
            },
        })
        .unwrap();
    w.supervise_once();
    let result = evals.show(requested["eval_id"].as_i64().unwrap()).unwrap();
    assert_eq!(result["status"], "finished", "{result}");
    let errors = store
        .eval_events()
        .unwrap()
        .into_iter()
        .filter_map(|e| e.payload.get("error").cloned())
        .collect::<Vec<_>>();
    assert_eq!(result["passed"], true, "{result}; job errors {errors:?}");
    assert_eq!(result["scores"]["runs"], 6);
    assert_eq!(result["scores"]["per_verdict"][values[2]]["recall"], 1.0);
    assert_eq!(
        result["launch"],
        json!({"provider":"claude","model":"claude-haiku-5-5","effort":"low"})
    );
    assert_eq!(
        evals.list(None, 10, true).unwrap()["by_launch"][0]["rounds"][0]["scores"],
        result["scores"]
    );
    let events = store.eval_events().unwrap();
    let started: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::AgentEvalRunStarted)
        .collect();
    assert_eq!(started.len(), 6);
    for event in started {
        assert_eq!(event.payload["provider"], "claude");
        assert_eq!(event.payload["model"], "claude-haiku-5-5");
        assert_eq!(event.payload["effort"], "low");
        assert_eq!(event.payload["timeout_secs"], 600);
        assert_eq!(
            event.payload["prompt_bytes"]["limit"],
            if goal {
                crate::application::prompt::GOAL_REVIEW_PROMPT_LIMIT
            } else {
                crate::application::prompt::PLAN_REVIEW_PROMPT_LIMIT
            }
        );
    }
    let trees = w.git.case_bases();
    assert_eq!(trees.len(), 3);
    for (_, base, hidden) in trees {
        assert_eq!(base, main);
        assert_eq!(hidden, crate::domain::agent_eval::verdict::HIDDEN);
    }
    let commands = w.agent.case_commands.lock().unwrap();
    assert_eq!(commands.len(), 6);
    for command in commands.iter() {
        let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(
            env[std::ffi::OsStr::new("DAGQ_ROLE")],
            Some(std::ffi::OsStr::new(if goal {
                "goal-review-job"
            } else {
                "plan-review-job"
            }))
        );
        for key in crate::domain::queue_service::CLIENT_ENV {
            assert_eq!(env[std::ffi::OsStr::new(key)], None);
        }
    }
    let reads = w.agent.case_reads.lock().unwrap();
    assert_eq!(reads.len(), 6);
    for boundary in reads.iter() {
        assert_eq!(boundary.readable.len(), 2);
        assert!(boundary.readable[0].ends_with("tree"));
        assert!(boundary.readable[1].ends_with("material"));
        assert!(!boundary.readable.iter().any(|p| p == &w.db));
        assert!(
            boundary
                .unreadable
                .contains(&w.db.parent().unwrap().to_owned())
        );
    }
    let prompts = w.agent.recovery_prompts();
    assert_eq!(prompts.len(), 6);
    for (prompt, cwd) in prompts {
        assert!(cwd.ends_with("tree"));
        assert!(!prompt.contains("\"expected\""));
        assert!(!prompt.contains("/production/repository"));
        let files = crate::application::memory_files::MemoryFiles::default();
        let harness =
            crate::application::role_eval::RoleHarness::target(&format!("role:{role}")).unwrap();
        assert_eq!(
            prompt,
            harness
                .prompt(
                    &snapshot,
                    &files,
                    &cwd.parent().unwrap().join("material"),
                    &cwd
                )
                .unwrap()
        );
    }
}

#[test]
fn role_round_refuses_extra_or_missing_case_files_before_launch() {
    let w = Wiring::new(
        "role_case_files",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    w.git
        .commit_files(&[(".dagq/role-evals/plan_review/extra.json", "{}")], &[]);
    let queue = w.queue();
    let actor = ActorContext::instance(crate::domain::actor::ActorRole::User, "user");
    let evals = AgentEvals::new(&queue, &actor, &StaticPolicy);
    let request = EvalRequest {
        agent: "role:plan_review".into(),
        split: Split::Dev,
        k: None,
        cases: None,
        set: None,
        rerun: false,
        launch: Default::default(),
    };
    let id = evals.request(&request).unwrap()["eval_id"]
        .as_i64()
        .unwrap();
    w.supervise_once();
    let result = evals.show(id).unwrap();
    assert_eq!(result["status"], "refused", "{result}");
    assert!(
        result["refusal"]["detail"]
            .as_str()
            .unwrap()
            .contains("unknown role case file"),
        "{result}"
    );
    assert!(w.agent.case_commands.lock().unwrap().is_empty());
}

#[test]
fn destinations_are_checked_again_when_the_role_round_starts() {
    let w = Wiring::new(
        "role_destination_recheck",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    let queue = w.queue();
    let actor = ActorContext::instance(crate::domain::actor::ActorRole::User, "user");
    let allowed = "[roles.plan_review]\nprovider='claude'\n[roles.review]\nprovider='codex'\n";
    let store = Requested {
        store: &queue,
        settings: LaunchSettings {
            roles: crate::infrastructure::run_env::parse_config(allowed)
                .unwrap()
                .roles,
            ..Default::default()
        },
    };
    let evals = AgentEvals::new(&store, &actor, &StaticPolicy);
    let mut request = EvalRequest {
        agent: "role:plan_review".into(),
        split: Split::Dev,
        k: None,
        cases: None,
        set: None,
        rerun: false,
        launch: AgentModel {
            provider: Some(crate::domain::Provider::Codex),
            ..Default::default()
        },
    };
    assert!(
        evals.request(&request).is_err(),
        "another role's provider must not authorize this role"
    );
    request.launch.provider = Some(crate::domain::Provider::Claude);
    let id = evals.request(&request).unwrap()["eval_id"]
        .as_i64()
        .unwrap();
    w.verifier
        .set_config("[roles.plan_review]\nprovider='codex'\n");
    w.supervise_once();
    let result = evals.show(id).unwrap();
    assert_eq!(result["status"], "refused", "{result}");
    assert!(
        result["refusal"]["detail"]
            .as_str()
            .unwrap()
            .contains("eval.providers.claude"),
        "{result}"
    );
    assert!(w.agent.case_commands.lock().unwrap().is_empty());
}

/// The run review's setting when a test names none.
const RUN_CONFIG: &str = "[roles.review]\nprovider='claude'\n";

/// Commit `members` (id, expected verdict, snapshot, change copy) as the
/// run review's dev cases with `config` and the `extra` files, every range
/// changing `changed`, and request a round of `k` on Claude.
fn request_run_round(
    w: &Wiring,
    (config, extra, changed): (&str, &[(&str, &str)], &[&str]),
    members: Vec<(&str, &str, Value, Option<&[u8]>)>,
    k: u32,
) -> (Value, Vec<RunEvent>) {
    use sha2::{Digest, Sha256};
    // Production's whole review on Codex is asked for with Codex's model.
    let (provider, model) = if config.contains("provider='codex'") {
        (crate::domain::Provider::Codex, "gpt-5.5")
    } else {
        (crate::domain::Provider::Claude, "claude-haiku-5-5")
    };
    w.verifier.set_config(config);
    let mut files = vec![("dagq.toml".to_owned(), config.to_owned())];
    files.extend(
        extra
            .iter()
            .map(|(p, t)| ((*p).to_owned(), (*t).to_owned())),
    );
    let mut cases = Vec::new();
    for (id, verdict, snapshot, patch) in members {
        let mut case = json!({"id":id,"source":"production","made_by":"codex","snapshot":snapshot,
            "expected":{"verdict":verdict,"codes":if verdict=="pass" {vec![]} else {vec!["acceptance_unmet"]}}});
        if let Some(patch) = patch {
            let hash = format!("{:x}", Sha256::digest(patch));
            let at = format!("{}/{hash}.patch", crate::domain::agent_eval::PATCH_DIR);
            match std::str::from_utf8(patch) {
                Ok(text) => files.push((at, text.to_owned())),
                Err(_) => w.git.commit_blob(&at, patch),
            }
            case["patch"] = json!(hash);
        } else {
            case["patch"] = json!("0".repeat(64));
        }
        cases.push(case);
    }
    let list = json!({"role":"review","codes":["acceptance_unmet"],"k":1,"cases":cases});
    files.push((".dagq/role-evals/review/dev.json".into(), list.to_string()));
    let named: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    w.git.commit_files(&named, changed);
    let queue = w.queue();
    let store = Requested {
        store: &queue,
        settings: LaunchSettings {
            roles: crate::infrastructure::run_env::parse_config(config)
                .unwrap()
                .roles,
            ..Default::default()
        },
    };
    let actor = ActorContext::instance(crate::domain::actor::ActorRole::User, "user");
    let evals = AgentEvals::new(&store, &actor, &StaticPolicy);
    let requested = evals
        .request(&EvalRequest {
            agent: "role:review".into(),
            split: Split::Dev,
            k: Some(k),
            cases: None,
            set: None,
            rerun: false,
            launch: AgentModel {
                provider: Some(provider),
                model: Some(model.into()),
                effort: Some("low".into()),
            },
        })
        .unwrap();
    w.supervise_once();
    let result = evals.show(requested["eval_id"].as_i64().unwrap()).unwrap();
    (result, store.eval_events().unwrap())
}

#[test]
fn run_review_round_reads_the_rebuilt_change_tree_and_records_launch_and_scores() {
    let pass = json!({"verdict":"pass","reasons":[],"summary":"sound"});
    let revise = json!({"verdict":"revise","reasons":[{"text":"fix","codes":["acceptance_unmet"]}],"summary":"fix"});
    let concern = json!({"verdict":"concern","reasons":[{"text":"ask","codes":["acceptance_unmet"]}],"summary":"ask"});
    let w = Wiring::new(
        "role_eval_run",
        Script {
            receipts: vec![],
            verdicts: vec![
                pass.clone(),
                pass,
                revise.clone(),
                revise,
                concern.clone(),
                concern,
            ],
        },
    )
    .with_codex()
    .with_review_agent_timeout(321);
    // The recorded base, the job's tree its change on it. Its head (the
    // run's branch) is no commit the repository has, and main moves on.
    let base = w.git.main().to_string();
    let snapshot =
        crate::application::role_fixtures::run_snapshot(&base, Some(&format!("tree-{base}")));
    let value = serde_json::to_value(&snapshot).unwrap();
    w.git.move_main();
    // Production's whole review is set on Codex, another provider than the
    // supervisor's reviewer (Claude), with its stage's time limit
    // (`[review.jobs]`, read at the start). The change copy is not UTF-8
    // (a Shift-JIS file's text diff).
    let config = "[roles.review]\nprovider='codex'\n[eval.providers.codex]\ndefault_run_usd = 0.1\ninput_usd_per_mtok = 1.0\noutput_usd_per_mtok = 2.0\n";
    let copy = b"diff --git a/sjis.txt b/sjis.txt\n+\x82\xa0\x82\xa2\n".as_slice();
    let (result, events) = request_run_round(
        &w,
        (config, &[], &[]),
        ["pass", "revise", "concern"]
            .into_iter()
            .map(|v| (v, v, value.clone(), Some(copy)))
            .collect(),
        2,
    );
    assert_eq!(result["status"], "finished", "{result}");
    let errors: Vec<_> = events
        .iter()
        .filter_map(|e| e.payload.get("error").cloned())
        .collect();
    assert_eq!(result["passed"], true, "{result}; job errors {errors:?}");
    assert_eq!(result["scores"]["runs"], 6);
    assert_eq!(result["scores"]["per_verdict"]["concern"]["recall"], 1.0);
    assert_eq!(
        result["launch"],
        json!({"provider":"codex","model":"gpt-5.5","effort":"low"})
    );
    for event in events
        .iter()
        .filter(|e| e.kind == EventKind::AgentEvalRunStarted)
    {
        assert_eq!(event.payload["model"], "gpt-5.5");
        assert_eq!(event.payload["effort"], "low");
        assert_eq!(
            event.payload["prompt_bytes"]["limit"],
            crate::application::prompt::RUN_REVIEW_PROMPT_LIMIT
        );
        // `[review.jobs]`'s limit, as production's whole review job has.
        assert_eq!(event.payload["timeout_secs"], 321, "{}", event.payload);
    }
    // Three trees, one per case, each the change copy on the recorded base
    // in a Git of its own with every expectation store hidden; none of
    // main's.
    assert!(w.git.case_bases().is_empty());
    let trees = w.git.case_trees();
    assert_eq!(trees.len(), 3);
    for (path, made_on, patch, hidden) in &trees {
        assert_eq!(made_on, &base);
        assert_eq!(patch.as_slice(), copy);
        assert_eq!(hidden, &crate::domain::agent_eval::verdict::HIDDEN);
        assert!(path.ends_with("tree"));
    }
    let commands = w.agent.case_commands.lock().unwrap();
    assert_eq!(commands.len(), 6);
    let prompts = w.agent.review_prompts();
    let dirs = w.agent.review_run_dirs.lock().unwrap();
    for ((command, prompt), run_dir) in commands.iter().zip(&prompts).zip(dirs.iter()) {
        let cwd = command.get_current_dir().unwrap().to_owned();
        assert!(
            trees.iter().any(|(path, ..)| *path == cwd),
            "{}",
            cwd.display()
        );
        let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        assert_eq!(
            env[std::ffi::OsStr::new("DAGQ_ROLE")],
            Some(std::ffi::OsStr::new("review-job"))
        );
        for key in crate::domain::queue_service::CLIENT_ENV {
            assert_eq!(env[std::ffi::OsStr::new(key)], None);
        }
        // Its settings and logs go below the case, not the production run.
        let case = cwd.parent().unwrap();
        assert!(Path::new(run_dir.as_deref().unwrap()).starts_with(case));
        // The material is the saved copy, and the prompt production's.
        let material = case.join("material");
        assert_eq!(
            crate::application::RunFiles::read(&*w.files, &material.join("review.md")).unwrap(),
            b"the material\0\xff"
        );
        let files = crate::application::memory_files::MemoryFiles::default();
        let harness = crate::application::role_eval::RoleHarness::target("role:review").unwrap();
        assert_eq!(
            prompt,
            &harness.prompt(&snapshot, &files, &material, &cwd).unwrap()
        );
        assert!(!prompt.contains("/production/"));
        assert!(!prompt.contains("\"expected\""));
    }
    let reads = w.agent.case_reads.lock().unwrap();
    assert_eq!(reads.len(), 6);
    for boundary in reads.iter() {
        assert!(boundary.readable[0].ends_with("tree"));
        assert!(boundary.readable[1].ends_with("material"));
        assert!(
            boundary
                .unreadable
                .contains(&w.db.parent().unwrap().to_owned())
        );
    }
}

#[test]
fn run_review_cases_that_cannot_rebuild_start_no_job_and_keep_why() {
    let w = Wiring::new(
        "role_eval_run_refused",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    let base = w.git.main().to_string();
    let snapshot = |base: &str, tree: &str| {
        serde_json::to_value(crate::application::role_fixtures::run_snapshot(
            base,
            Some(tree),
        ))
        .unwrap()
    };
    let tree = format!("tree-{base}");
    let (result, _) = request_run_round(
        &w,
        (RUN_CONFIG, &[], &[]),
        vec![
            (
                "mismatch",
                "pass",
                snapshot(&base, "tree-other"),
                Some(b"patch\n".as_slice()),
            ),
            (
                "unreachable",
                "pass",
                snapshot(&"f".repeat(40), &tree),
                Some(b"patch\n".as_slice()),
            ),
            (
                "empty",
                "pass",
                snapshot(&base, &tree),
                Some(b"".as_slice()),
            ),
            ("missing", "pass", snapshot(&base, &tree), None),
        ],
        1,
    );
    assert_eq!(result["status"], "finished", "{result}");
    assert_eq!(result["passed"], false);
    assert_eq!(result["scores"]["errors"], 4, "{result}");
    let why: std::collections::BTreeMap<String, String> = result["scores"]["failed_cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["id"].as_str().unwrap().into(),
                c["error"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert!(
        why["mismatch"].contains("differs from recorded tree"),
        "{why:?}"
    );
    assert!(why["unreachable"].contains("not reachable"), "{why:?}");
    assert!(why["empty"].contains("missing or empty"), "{why:?}");
    assert!(why["missing"].contains("missing or empty"), "{why:?}");
    assert!(w.agent.case_commands.lock().unwrap().is_empty());
    assert!(w.agent.review_prompts().is_empty());
    // Committing the copy it lacked makes another case set, not the same.
    let (empty, _) = request_run_round(
        &w,
        (RUN_CONFIG, &[], &[]),
        vec![("missing", "pass", snapshot(&base, &tree), None)],
        1,
    );
    let (copied, _) = request_run_round(
        &w,
        (RUN_CONFIG, &[], &[]),
        vec![(
            "missing",
            "pass",
            snapshot(&base, &tree),
            Some(b"patch\n".as_slice()),
        )],
        1,
    );
    assert!(empty["finished"]["case_set_digest"].is_string(), "{empty}");
    assert_ne!(
        empty["finished"]["case_set_digest"],
        copied["finished"]["case_set_digest"]
    );
}

/// One run review case whose change touches `new.txt`, its program review
/// `program` configured on that path, on a wiring that runs programs.
fn program_round(program: &str, verdicts: Vec<Value>) -> (Wiring, Value, Vec<RunEvent>) {
    let w = Wiring::new(
        &format!("role_eval_run_{program}"),
        Script {
            receipts: vec![],
            verdicts,
        },
    )
    .with_programs();
    let base = w.git.main().to_string();
    let snapshot = serde_json::to_value(crate::application::role_fixtures::run_snapshot(
        &base,
        Some(&format!("tree-{base}")),
    ))
    .unwrap();
    let config = format!(
        "{RUN_CONFIG}[review.programs.{program}]\nscript = \"scripts/{program}.sh\"\npaths = [\"new.txt\"]\n[review.programs.\"builtin:secrets\"]\nenabled = false\n"
    );
    let script = format!("scripts/{program}.sh");
    let (result, events) = request_run_round(
        &w,
        (&config, &[(&script, "exit 0\n")], &["new.txt"]),
        vec![(
            "pass",
            "pass",
            snapshot,
            Some(b"diff --git a/new.txt b/new.txt\n".as_slice()),
        )],
        1,
    );
    (w, result, events)
}

#[test]
fn run_review_cases_run_the_programs_production_runs_before_their_job() {
    let (w, result, events) = program_round(
        "check",
        vec![json!({"verdict":"pass","reasons":[],"summary":"sound"})],
    );
    assert_eq!(result["status"], "finished", "{result}");
    // Against the recorded base's tree first, then the rebuilt change's.
    let runs = w.agent.program_runs.lock().unwrap().clone();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(runs[0].0, "check");
    assert!(runs[0].1.ends_with("base"), "{runs:?}");
    assert!(runs[1].1.ends_with("tree"), "{runs:?}");
    let (_, base, _) = &w.git.case_bases()[0];
    assert_eq!(base, &w.git.case_trees()[0].1);
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::AgentEvalCaseChecked
                && e.payload["programs"] == json!(["check"])),
        "{events:?}"
    );
    assert_eq!(w.agent.case_commands.lock().unwrap().len(), 1);
    assert_eq!(result["scores"]["runs"], 1, "{result}");
}

#[test]
fn a_program_that_rejects_a_run_review_case_stops_it_before_its_job() {
    let (w, result, _) = program_round("reject", vec![]);
    assert_eq!(result["status"], "finished", "{result}");
    assert_eq!(
        result["finished"]["program_stopped"]["count"], 1,
        "{result}"
    );
    assert_eq!(result["scores"]["runs"], 0, "{result}");
    assert!(w.agent.case_commands.lock().unwrap().is_empty());
    assert_eq!(w.agent.program_runs.lock().unwrap().len(), 2);
}
