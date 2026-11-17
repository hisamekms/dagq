//! The landing entry point pairs cleanup with the environment of both kinds
//! of command. Fake compiler locks leave a session for a running compiler.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::{
    agent::{Script, succeeded},
    harness::Wiring,
};
use crate::application::{RunFiles, TaskReads};
use serde_json::json;

#[test]
fn landing_incremental_cleanup_and_environment_are_opt_in() {
    for (enabled, busy) in [(false, false), (true, false), (true, true)] {
        let wiring = Wiring::new(
            "landing_incremental_cleanup_and_environment_are_opt_in",
            Script {
                receipts: vec![succeeded],
                verdicts: vec![json!({"verdict": "pass", "reasons": [], "summary": "fine"})],
            },
        );
        wiring.verifier.set_config(&format!("[run.env]\nCARGO_INCREMENTAL = '1'\n[toolchains.rust]\nprofile = 'rust'\n[landing_verification]\nreplaces = ['gate']\ncommand = 'replacement'\nnon_incremental = {enabled}\n"));
        let task = wiring.ready_task(&["registered", "gate"]);
        let root = Arc::new(Mutex::new(None::<PathBuf>));
        let remembered = root.clone();
        let files = wiring.files.clone();
        let db = wiring.db.clone();
        wiring.agent.on_review(Box::new(move || {
            let mut queue = crate::infrastructure::sqlite::SqliteQueue::open(&db).unwrap();
            let detail = queue.show(task).unwrap();
            let dir = PathBuf::from(detail.runs[0].worktree_path().unwrap())
                .join("target/debug/incremental");
            files.create_dir_all(&dir.join("crate/s-a-r-done")).unwrap();
            files
                .write(&dir.join("crate/s-a-r-done/data"), b"previous build")
                .unwrap();
            files.write(&dir.join("crate/s-a-r.lock"), b"").unwrap();
            if busy {
                files
                    .create_dir_all(&dir.join("crate/s-b-r-working"))
                    .unwrap();
                files
                    .write(&dir.join("crate/s-b-r-working/data"), b"active build")
                    .unwrap();
                files.hold_lock(&dir.join("crate/s-b-r.lock"));
            }
            files.set_tree_size(&dir, 4096);
            *remembered.lock().unwrap() = Some(dir);
        }));
        let check_root = root.clone();
        let files = wiring.files.clone();
        wiring.verifier.on_command(
            "registered",
            Box::new(move || {
                let root = check_root.lock().unwrap();
                let dir = root.as_ref().unwrap();
                assert_eq!(files.exists(&dir.join("crate/s-a-r-done/data")), !enabled);
                assert_eq!(files.exists(&dir.join("crate/s-b-r-working/data")), busy);
            }),
        );
        wiring.supervise_once();
        let detail = wiring.queue().show(task).unwrap();
        assert_eq!(
            detail.task.status(),
            crate::domain::TaskStatus::Completed,
            "{:?}",
            detail
                .events
                .iter()
                .filter(|e| matches!(e.kind.as_str(), "review_failed" | "integration_deferred"))
                .map(|e| &e.payload)
                .collect::<Vec<_>>()
        );
        let events: Vec<_> = detail
            .events
            .iter()
            .filter(|e| e.kind.as_str() == "landing_incremental_removed")
            .collect();
        assert_eq!(events.len(), usize::from(enabled));
        if enabled {
            assert_eq!(
                events[0].payload["outputs"][0]["dir"],
                json!(root.lock().unwrap().as_ref().unwrap())
            );
            assert_eq!(
                events[0].payload["outputs"][0]["bytes"],
                if busy { 0 } else { 4096 }
            );
            if busy {
                assert_eq!(
                    events[0].payload["outputs"][0]["retained"][0]["reason"],
                    "locked"
                );
            }
        }
        let envs = wiring.verifier.environments.lock().unwrap();
        for command in ["registered", "replacement"] {
            let (_, env) = envs
                .iter()
                .find(|(c, _)| c == command)
                .unwrap_or_else(|| panic!("missing {command}: {envs:?}"));
            assert_eq!(
                env.iter()
                    .rev()
                    .find(|(k, _)| k == "CARGO_INCREMENTAL")
                    .unwrap()
                    .1,
                if enabled { "0" } else { "1" }
            );
            assert_eq!(
                env.iter()
                    .any(|(k, v)| k == "CARGO_INCREMENTAL" && v == "0"),
                enabled
            );
        }
    }
}

#[test]
fn landing_cleanup_keeps_compiler_locked_sessions() {
    let files = crate::application::memory_files::MemoryFiles::default();
    let root = std::path::Path::new("/run/worktree");
    let dir = root.join("target/debug/incremental");
    for session in ["s-a-r-done", "s-b-r-working"] {
        files
            .create_dir_all(&dir.join("crate").join(session))
            .unwrap();
        files
            .write(&dir.join("crate").join(session).join("data"), b"build")
            .unwrap();
    }
    files.hold_lock(&dir.join("crate/s-b-r.lock"));
    files.create_dir_all(&dir.join("crate/s-c-r-done")).unwrap();
    files
        .write(&dir.join("crate/s-c-r-done/data"), b"failed cleanup")
        .unwrap();
    files.put_dir(&dir.join("crate/s-c-r.lock"));
    let build = crate::domain::toolchain::ToolchainProfile::non_incremental_build(
        &crate::domain::toolchain::rust::RUST,
    )
    .unwrap();
    let outputs = crate::application::landing_build_cleanup::remove(&files, root, &[build]);
    assert!(!files.exists(&dir.join("crate/s-a-r-done/data")));
    assert!(files.exists(&dir.join("crate/s-b-r-working/data")));
    assert_eq!(outputs[0]["retained"][0]["reason"], "locked");
    assert!(files.exists(&dir.join("crate/s-c-r-done/data")));
    assert!(
        outputs[0]["failures"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["error"] == "compiler lock is not a regular file")
    );
}
