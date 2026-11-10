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
    let pass = json!({"verdict":"pass","reasons":[],"summary":"sound"});
    let revise =
        json!({"verdict":"revise","reasons":[{"text":"fix","codes":["P-1"]}],"summary":"fix"});
    let concern =
        json!({"verdict":"concern","reasons":[{"text":"ask","codes":["P-1"]}],"summary":"ask"});
    let w = Wiring::new(
        "plan_role_eval",
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
    );
    let main = w.git.main().to_string();
    let snapshot = crate::application::role_eval::tests::snapshot(&main);
    let members = ["pass", "revise", "concern"].map(|verdict| {
        json!({"id":verdict,"source":"handmade","made_by":"test","snapshot":snapshot,
            "expected":{"verdict":verdict,"codes":if verdict=="pass" {vec![]}else{vec!["P-1"]}}})
    });
    let cases = json!({"role":"plan_review","codes":["P-1"],"k":1,"cases": members});
    // Main moves after the record. The case must still request the old main.
    w.git.move_main();
    let config = "[roles.plan_review]\nprovider='claude'\n[roles.review]\nprovider='codex'\n";
    w.verifier.set_config(config);
    w.git.commit_files(
        &[
            ("dagq.toml", config),
            (".dagq/role-evals/plan_review/dev.json", &cases.to_string()),
        ],
        &[],
    );
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
            agent: "role:plan_review".into(),
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
    assert_eq!(result["scores"]["per_verdict"]["concern"]["recall"], 1.0);
    assert_eq!(
        result["launch"],
        json!({"provider":"claude","model":"claude-haiku-5-5","effort":"low"})
    );
    assert_eq!(
        evals.list(None, 10, true).unwrap()["by_launch"][0]["rounds"][0]["scores"],
        result["scores"]
    );
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
            Some(std::ffi::OsStr::new("plan-review-job"))
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
    for (prompt, cwd) in w.agent.recovery_prompts() {
        assert!(cwd.ends_with("tree"));
        assert!(!prompt.contains("\"expected\""));
        assert!(!prompt.contains("/production/repository"));
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
