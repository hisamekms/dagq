//! Postgres-only until source mutations invoke the entry in both adapters.
//! Source rows are written directly, then copied in the same transaction;
//! all rows roll back. Expected columns are SQLite search-trigger values.
use super::ContractStore;
use crate::infrastructure::search_writes::{SearchWrite, maintain};
use crate::infrastructure::sql_exec::{SqlDialect, SqlRead, SqlWrite, sql_params};
use anyhow::{Result, ensure};
use serde_json::{Value, json};

fn indexed(tx: &impl SqlRead, kind: &str, reference: &str, expected: Value) -> Result<()> {
    let row: String = tx.query_row(
        "SELECT jsonb_build_array(doc_order,kind,ref,task_id,goal_id,run_id,status,updated_at,title,description,acceptance,context,text,trigram_count)::text FROM search_index WHERE kind=?1 AND ref=?2",
        sql_params![kind, reference], |r| r.get(0),
    )?;
    ensure!(
        serde_json::from_str::<Value>(&row)? == expected,
        "{kind} {reference}: {row} != {expected}"
    );
    Ok(())
}

/// Covers all search/landing/version trigger equivalents against the real
/// schema. No source statement calls the maintenance entry implicitly yet.
pub fn postgres_search_writes<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_search_writes")?;
    ensure!(tx.dialect() == SqlDialect::Postgres);
    let goal = tx.insert_id("INSERT INTO goals(title,description,acceptance,constraints,updated_at) VALUES ('日本語検索','説明文','条件文','制約文','g1')", &[])?;
    let other = tx.insert_id("INSERT INTO goals(title) VALUES ('other')", &[])?;
    maintain(
        &tx,
        SearchWrite::Goal {
            id: goal,
            previous_acceptance: None,
        },
    )?;
    indexed(
        &tx,
        "goal",
        &goal.to_string(),
        json!([
            goal * 4 + 1,
            "goal",
            goal.to_string(),
            null,
            goal,
            null,
            "open",
            "g1",
            "日本語検索",
            "説明文",
            "条件文",
            "制約文",
            "",
            6
        ]),
    )?;
    let task = tx.insert_id("INSERT INTO tasks(title,description,acceptance,context,verification_commands,goal_id,updated_at) VALUES ('task title','body','accept','context','[]',?1,'t1')", sql_params![goal])?;
    maintain(&tx, SearchWrite::Task(task))?;
    indexed(
        &tx,
        "task",
        &task.to_string(),
        json!([
            task * 4,
            "task",
            task.to_string(),
            task,
            goal,
            null,
            "draft",
            "t1",
            "task title",
            "body",
            "accept",
            "context",
            "",
            19
        ]),
    )?;
    tx.execute("INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit) VALUES ('search-contract',?1,'integrated','codex','codex','base')", sql_params![task])?;
    let mut notes = Vec::new();
    for (task_id, goal_id) in [
        (Some(task), None),
        (Some(task), Some(other)),
        (None, Some(goal)),
    ] {
        let id = tx.insert_id("INSERT INTO run_events(task_id,goal_id,kind,payload,created_at) VALUES (?1,?2,'observation','{\"text\":\"観測の記録\"}','n1')", sql_params![task_id,goal_id])?;
        maintain(&tx, SearchWrite::Note(id))?;
        indexed(
            &tx,
            "note",
            &id.to_string(),
            json!([
                id * 4 + 2,
                "note",
                id.to_string(),
                task_id,
                goal_id.or(Some(goal)),
                null,
                if task_id.is_some() { "draft" } else { "open" },
                "n1",
                "",
                "",
                "",
                "",
                "観測の記録",
                3
            ]),
        )?;
        notes.push(id);
    }
    // Non-observation events have no note document.
    let ignored = tx.insert_id(
        "INSERT INTO run_events(kind,payload) VALUES ('other','{}')",
        &[],
    )?;
    maintain(&tx, SearchWrite::Note(ignored))?;
    ensure!(
        tx.query_row(
            "SELECT count(*) FROM search_index WHERE doc_order=?1",
            sql_params![ignored * 4 + 2],
            |r| r.get::<_, i64>(0)
        )? == 0
    );

    // SQLite INSERT OR IGNORE also ignores incomplete landing payloads.
    for source in ["{}", "{\"result_commit\":null}"] {
        let id = tx.insert_id("INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (?1,'search-contract','run_integrated',?2)", sql_params![task, crate::infrastructure::sql_exec::SqlValue::Json(source.into())])?;
        maintain(&tx, SearchWrite::Integrated(id))?;
    }
    ensure!(
        tx.query_row(
            "SELECT count(*) FROM landed_commits WHERE run_id='search-contract'",
            &[],
            |r| r.get::<_, i64>(0)
        )? == 0
    );
    let imported = tx.insert_id("INSERT INTO landed_commits(run_id,task_id,commit_sha,landed_at) VALUES ('imported-search',?1,'def456','imported')", sql_params![task])?;
    maintain(&tx, SearchWrite::Commit(imported))?;
    indexed(
        &tx,
        "commit",
        "def456",
        json!([
            imported * 4 + 3,
            "commit",
            "def456",
            task,
            goal,
            "imported-search",
            "draft",
            "imported",
            "",
            "",
            "",
            "",
            "",
            0
        ]),
    )?;
    let event = tx.insert_id("INSERT INTO run_events(task_id,run_id,kind,payload,created_at) VALUES (?1,'search-contract','run_integrated','{\"result_commit\":\"abc123\",\"message\":\"landed message\",\"git_common_dir\":\"/repo\"}','c1')", sql_params![task])?;
    maintain(&tx, SearchWrite::Integrated(event))?;
    // A second event for this run keeps the first landing, including its text.
    let duplicate = tx.insert_id("INSERT INTO run_events(task_id,run_id,kind,payload) VALUES (?1,'search-contract','run_integrated','{\"result_commit\":\"wrong\",\"message\":\"wrong\"}')", sql_params![task])?;
    maintain(&tx, SearchWrite::Integrated(duplicate))?;
    maintain(&tx, SearchWrite::Integrated(event))?;
    let (commit, sha, message, common, landed): (i64,String,String,String,String) = tx.query_row("SELECT id,commit_sha,message,git_common_dir,landed_at FROM landed_commits WHERE run_id='search-contract'", &[], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    ensure!(
        (
            sha.as_str(),
            message.as_str(),
            common.as_str(),
            landed.as_str()
        ) == ("abc123", "landed message", "/repo", "c1")
    );
    indexed(
        &tx,
        "commit",
        "abc123",
        json!([
            commit * 4 + 3,
            "commit",
            "abc123",
            task,
            goal,
            "search-contract",
            "draft",
            "c1",
            "",
            "",
            "",
            "",
            "landed message",
            12
        ]),
    )?;

    tx.execute(
        "UPDATE landed_commits SET message='updated' WHERE id=?1",
        sql_params![commit],
    )?;
    maintain(&tx, SearchWrite::Commit(commit))?;
    indexed(
        &tx,
        "commit",
        "abc123",
        json!([
            commit * 4 + 3,
            "commit",
            "abc123",
            task,
            goal,
            "search-contract",
            "draft",
            "c1",
            "",
            "",
            "",
            "",
            "updated",
            5
        ]),
    )?;
    // Task text updates replace the index row, while state/goal propagate
    // to commits and task notes, preserving an event's explicit goal.
    tx.execute(
        "UPDATE tasks SET title='new', status='completed', goal_id=?2, updated_at='t2' WHERE id=?1",
        sql_params![task, other],
    )?;
    maintain(&tx, SearchWrite::Task(task))?;
    indexed(
        &tx,
        "task",
        &task.to_string(),
        json!([
            task * 4,
            "task",
            task.to_string(),
            task,
            other,
            null,
            "completed",
            "t2",
            "new",
            "body",
            "accept",
            "context",
            "",
            12
        ]),
    )?;
    indexed(
        &tx,
        "commit",
        "abc123",
        json!([
            commit * 4 + 3,
            "commit",
            "abc123",
            task,
            other,
            "search-contract",
            "completed",
            "c1",
            "",
            "",
            "",
            "",
            "updated",
            5
        ]),
    )?;
    for id in &notes[..2] {
        indexed(
            &tx,
            "note",
            &id.to_string(),
            json!([
                id * 4 + 2,
                "note",
                id.to_string(),
                task,
                other,
                null,
                "completed",
                "n1",
                "",
                "",
                "",
                "",
                "観測の記録",
                3
            ]),
        )?;
    }
    // Moving again distinguishes explicit note ownership from inheritance.
    tx.execute(
        "UPDATE tasks SET goal_id=?2 WHERE id=?1",
        sql_params![task, goal],
    )?;
    maintain(&tx, SearchWrite::Task(task))?;
    for (id, owner) in [(notes[0], goal), (notes[1], other)] {
        indexed(
            &tx,
            "note",
            &id.to_string(),
            json!([
                id * 4 + 2,
                "note",
                id.to_string(),
                task,
                owner,
                null,
                "completed",
                "n1",
                "",
                "",
                "",
                "",
                "観測の記録",
                3
            ]),
        )?;
    }
    tx.execute("UPDATE goals SET title='closed', acceptance='changed', status='closed', closed_at='g2', verdict='achieved', updated_at='g2' WHERE id=?1", sql_params![goal])?;
    maintain(
        &tx,
        SearchWrite::Goal {
            id: goal,
            previous_acceptance: Some("条件文"),
        },
    )?;
    indexed(
        &tx,
        "goal",
        &goal.to_string(),
        json!([
            goal * 4 + 1,
            "goal",
            goal.to_string(),
            null,
            goal,
            null,
            "achieved",
            "g2",
            "closed",
            "説明文",
            "changed",
            "制約文",
            "",
            11
        ]),
    )?;
    indexed(
        &tx,
        "note",
        &notes[2].to_string(),
        json!([
            notes[2] * 4 + 2,
            "note",
            notes[2].to_string(),
            null,
            goal,
            null,
            "achieved",
            "n1",
            "",
            "",
            "",
            "",
            "観測の記録",
            3
        ]),
    )?;
    ensure!(
        tx.query_row(
            "SELECT acceptance_version FROM goals WHERE id=?1",
            sql_params![goal],
            |r| r.get::<_, i64>(0)
        )? == 2
    );
    maintain(
        &tx,
        SearchWrite::Goal {
            id: goal,
            previous_acceptance: Some("changed"),
        },
    )?;
    ensure!(
        tx.query_row(
            "SELECT acceptance_version FROM goals WHERE id=?1",
            sql_params![goal],
            |r| r.get::<_, i64>(0)
        )? == 2
    );
    let (doc, td): (String, String) = tx.query_row(
        "SELECT doc, td FROM search_index WHERE kind='task' AND task_id=?1",
        sql_params![task],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!(doc == "new\nbody\naccept\ncontext\n" && td == "new\nbody");
    let indexes = tx.query_map(
        "SELECT indexdef FROM pg_indexes WHERE indexname IN ('search_index_doc','search_index_td')",
        &[],
        |r| r.get::<_, String>(0),
    )?;
    ensure!(
        indexes.len() == 2
            && indexes
                .iter()
                .all(|s| s.contains("gin_bigm_ops") || s.contains("gin_trgm_ops"))
    );
    ensure!(
        tx.query_row(
            "SELECT count(*) FROM pg_trigger WHERE NOT tgisinternal",
            &[],
            |r| r.get::<_, i64>(0)
        )? == 0
    );
    Ok(())
}
