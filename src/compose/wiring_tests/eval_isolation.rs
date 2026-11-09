//! A case reaches the production executor from the supervisor without
//! the queue service variables given to a production review job.
use super::{agent::Script, harness::Wiring};
use crate::domain::EventKind;
use serde_json::json;

#[test]
fn an_eval_case_reaches_the_executor_without_queue_credentials() {
    let wiring = Wiring::new(
        "eval_case_isolation",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    let patch = "diff --git a/notes.txt b/notes.txt\nnew file mode 100644\n--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1 @@\n+hello\n";
    let hash = crate::infrastructure::queue_service::sha256_hex(patch.as_bytes());
    let cases = json!({"agent":"design", "role":"review", "codes":["D-1"], "k":1, "cases":[{
        "id":"one", "source":"handmade", "made_by":"test", "base_commit":wiring.git.main(),
        "patch":hash, "review":{"input":{}, "expected":{"verdict":"clean", "codes":[]}}
    }]})
    .to_string();
    wiring.git.commit_files(&[
        ("dagq.toml", "[review.subagents.design]\npaths = ['change.txt']\n[review.programs.\"builtin:secrets\"]\nenabled = false\n"),
        (".dagq/agents/design/AGENT.md", "---\ndescription: design checks\n---\nCheck design.\n"),
        (".dagq/agents/design/evals/dev.json", &cases),
        (&format!(".dagq/agent-cases/patches/{hash}.patch"), patch),
    ], &[]);
    wiring
        .queue()
        .record_queue_event(
            EventKind::AgentEvalRequested,
            json!({
                "agent":"design", "split":"dev", "k":1, "rerun":false,
                "requested_by":"user", "requested_by_id":"user", "before_landing":false
            }),
        )
        .unwrap();
    wiring.supervise_once();
    let commands = wiring.agent.case_commands.lock().unwrap();
    assert_eq!(commands.len(), 1, "{commands:?}");
    let command = &commands[0];
    assert!(command.get_current_dir().unwrap().ends_with("tree"));
    let env: std::collections::BTreeMap<_, _> = command.get_envs().collect();
    assert_eq!(
        env[std::ffi::OsStr::new("DAGQ_ROLE")],
        Some(std::ffi::OsStr::new("review-job"))
    );
    for name in crate::domain::queue_service::CLIENT_ENV {
        assert_eq!(env[std::ffi::OsStr::new(name)], None, "{env:?}");
    }
}
