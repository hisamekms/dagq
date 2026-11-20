//! Representative queue ports, including the filesystem effect of a committed
//! span close. Both adapters call this on an otherwise unused queue and files.
use crate::application::{AskStore, SearchIndex, TaskStore};
use crate::domain::{
    AskKind, AskReason, ClaimOutcome, CommitSha, EventKind, NewAsk, NewTask, TaskAction, TaskStatus,
};
use crate::infrastructure::{
    sessions,
    sql_exec::{CommitTransaction, SqlRead, SqlWrite, TransactionEntries, sql_params},
    store_queue::StoreQueue,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::Path;

/// Run through the same queue ports the service dispatches, on the supplied
/// adapter. Fixture files and committed records belong to this case alone.
pub fn queue_ports<E: TransactionEntries>(queue: &mut StoreQueue<E>, files: &Path) -> Result<()>
where
    StoreQueue<E>: SearchIndex,
{
    queue.runs_dir = files.join("runs");
    let task = TaskStore::add(
        queue,
        NewTask {
            title: "portcontract日本語".into(),
            description: "description".into(),
            acceptance: "acceptance".into(),
            verification_commands: vec![],
            required_evidence: vec![],
            paths: vec![],
            priority: None,
            change: None,
            dependencies: vec![],
            goal_dependencies: vec![],
            goal_id: None,
            context: String::new(),
            provider: Some(crate::domain::Provider::Codex),
            worker_mode: Some(crate::domain::worker::WorkerMode::Headless),
            wait_for_build: false,
            execution_class: Default::default(),
        },
    )?;
    ensure!(task.status() == TaskStatus::Draft);
    let ready = TaskStore::transition(queue, task.id(), TaskAction::BypassReview)?;
    ensure!(ready.status() == TaskStatus::Ready);
    let token = crate::domain::LeaseToken::new("queue-port-contract");
    let ClaimOutcome::Claimed { run } = queue.claim_for_supervisor_in_order(
        &CommitSha::try_from("a".repeat(40))?,
        &token,
        &[task.id()],
        None,
        &crate::domain::worker_model::WorkerTrial::default(),
        &crate::domain::provider_switch::WorkerRoute::direct(&crate::domain::worker::Worker::ALL),
    )?
    else {
        anyhow::bail!("the ready contract task was not claimed");
    };
    ensure!(run.task_id() == task.id());
    let read = queue.read_entry("contract_claim_read")?;
    let status: String = read.query_row(
        "SELECT status FROM tasks WHERE id=?1",
        sql_params![task.id()],
        |r| r.get(0),
    )?;
    ensure!(status == "in_progress");
    let kinds = read.query_map(
        "SELECT kind FROM run_events WHERE task_id=?1 ORDER BY id",
        sql_params![task.id()],
        |r| r.get::<_, String>(0),
    )?;
    ensure!(kinds.iter().any(|k| k == "task_created"));
    ensure!(kinds.iter().any(|k| k == "task_status_changed"));
    ensure!(kinds.iter().any(|k| k == "run_claimed"));
    drop(read);
    let page = SearchIndex::search_documents(
        queue,
        &crate::domain::search::SearchQuery {
            terms: "portcontract日本語".into(),
            limit: 10,
            ..Default::default()
        },
    )?;
    ensure!(
        page.hits
            .iter()
            .any(|hit| hit.kind == crate::domain::search::SearchKind::Task && matches!(&hit.id, crate::domain::search::SearchRef::Id(id) if *id == task.id().as_i64()))
    );
    let ask = AskStore::ask(
        queue,
        NewAsk {
            kind: AskKind::Blocked,
            task_id: Some(task.id()),
            run_id: Some(run.id().clone()),
            question: "contract question".into(),
            options: vec!["continue".into()],
            asked_by: "contract".into(),
            reason_category: AskReason::Scope,
            topics: vec![],
            recommendation: None,
            confidence: None,
            finding_id: None,
            request_id: None,
        },
    )?;
    ensure!(ask.created && ask.ask.is_open());
    AskStore::answer_as(
        queue,
        ask.ask.id,
        "continue",
        crate::domain::Answerer::PERSON,
    )?;
    let closed = AskStore::close_ask(queue, ask.ask.id)?;
    ensure!(!closed.is_open());

    let run_dir = queue.runs_dir.join(run.id().as_str());
    let text = |name: &str| run_dir.join(name).to_string_lossy().into_owned();
    queue.plan_run(
        run.id(),
        &token,
        &crate::domain::RunPlan {
            repo_path: text("repo"),
            run_dir: run_dir.to_string_lossy().into_owned(),
            branch: "contract".into(),
            worktree_path: text("worktree"),
            receipt_path: text("receipt.json"),
            log_path: text("run.log"),
        },
    )?;
    ensure!(queue.run(run.id())?.status() == crate::domain::RunStatus::Starting);
    std::fs::create_dir_all(run_dir.join("turns"))?;
    let commands = json!({"id":"command", "tool":"command_execution", "command":"git status", "started":1_735_689_601_000i64,"ended":1_735_689_602_000i64,"exit_code":0,"status":"completed"});
    std::fs::write(
        crate::domain::turn::commands_path(&run_dir, 1),
        format!("{commands}\n"),
    )?;
    let tx = queue.write_entry("contract_span_fixture")?;
    for (kind, payload, at) in [
        (
            EventKind::AgentStarted,
            json!({"session_id":null}),
            "2025-01-01T00:00:00.000Z",
        ),
        (
            EventKind::TurnStarted,
            json!({"turn":1,"provider":"codex"}),
            "2025-01-01T00:00:01.000Z",
        ),
        (
            EventKind::TurnFinished,
            json!({"turn":1,"provider":"codex","outcome":"succeeded"}),
            "2025-01-01T00:00:03.000Z",
        ),
    ] {
        record(&tx, task.id(), run.id(), kind, payload, at)?;
    }
    tx.commit()?;
    let path = run_dir.join(sessions::WORKTIME_FILE);
    for commit in [false, true] {
        let payload = json!({"exit_code":0});
        let _read = queue.read_before(sessions::Closing::Event(
            run.id(),
            EventKind::SessionExited,
            &payload,
        ))?;
        let tx = queue.write_entry("contract_span_close")?;
        record(
            &tx,
            task.id(),
            run.id(),
            EventKind::SessionExited,
            payload,
            "2025-01-01T00:00:05.000Z",
        )?;
        ensure!(!path.exists(), "worktime was written before commit");
        if commit {
            tx.commit()?;
        } else {
            drop(tx);
        }
        crate::infrastructure::sql_exec::flush_after_commit();
        ensure!(
            path.exists() == commit,
            "worktime did not follow commit/rollback"
        );
    }
    let lines = std::fs::read_to_string(path)?;
    ensure!(lines.lines().count() == 1, "{lines}");
    let line: Value = serde_json::from_str(lines.lines().next().context("worktime line")?)?;
    ensure!(line["command"] == "git status", "{line}");
    Ok(())
}

fn record(
    tx: &impl SqlWrite,
    task: crate::domain::TaskId,
    run: &crate::domain::RunId,
    kind: EventKind,
    payload: Value,
    at: &str,
) -> Result<()> {
    let id = sessions::insert_at(tx, Some(task), Some(run), kind, &payload, at)?;
    sessions::follow(tx, id, Some(task), Some(run), kind, &payload)
}
