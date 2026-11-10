//! Related and plan-review duplicate candidates on one fixed corpus. These
//! expectations are shared with Postgres CI and anchored by SQLite FTS5.
use super::ContractStore;
use crate::domain::{
    TaskId,
    search::{SearchKind, SearchQuery, SearchRef, any_word_query},
};
use crate::infrastructure::{
    related::landed_commits,
    search_writes::{SearchWrite, maintain},
    sql_exec::{SqlWrite, sql_params},
};
use anyhow::{Result, ensure};

pub fn related_cases<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    let store = open()?;
    let tx = store.write_entry("contract_related")?;
    // a/b paraphrase the same change; c repeats only part of the vocabulary.
    // Extra acceptance text changes D without adding title/description hits.
    for (id, title, description, acceptance, status) in [
        (710001, "検索候補を日本語で探す", "", "", "draft"),
        (710002, "日本語の検索候補を抽出する", "", "", "completed"),
        (
            710003,
            "検索候補を日本語にする",
            "",
            "padding padding padding padding padding padding padding padding ",
            "completed",
        ),
        (710004, "ab", "", "", "draft"),
        (710005, "unrelated", "", "", "draft"),
    ] {
        tx.execute("INSERT INTO tasks(id,title,description,acceptance,status,verification_commands,created_at,updated_at) VALUES (?1,?2,?3,?4,?5,'[]','2026-01-01','2026-01-01')", sql_params![id, title, description, acceptance, status])?;
        maintain(&tx, SearchWrite::Task(id))?;
    }
    // Non-task rows affect N, n and avgdl, but cannot be returned as tasks.
    tx.execute("INSERT INTO goals(id,title,created_at,updated_at) VALUES (710006,'検索候補','2026-01-01','2026-01-01')", &[])?;
    maintain(
        &tx,
        SearchWrite::Goal {
            id: 710006,
            previous_acceptance: None,
        },
    )?;
    for (id, task, sha) in [
        (710007, 710002, "bbbb"),
        (710008, 710003, "cccc"),
        (710009, 710002, "bbcc"),
    ] {
        tx.execute("INSERT INTO landed_commits(id,run_id,task_id,commit_sha,message,landed_at) VALUES (?1,?4,?2,?3,'','2026-01-01')", sql_params![id, task, sha, format!("contract-related-{id}")])?;
        maintain(&tx, SearchWrite::Commit(id))?;
    }
    let a = TaskId::new(710001);
    let page = store.related_in(&tx, a, &[], 10)?;
    let actual: Vec<_> = page.related.iter().map(|r| r.id).collect();
    ensure!(actual == vec![710002, 710003], "related: {actual:?}");
    let page = store.related_in(&tx, a, &["completed".into()], 1)?;
    ensure!(page.related[0].id == 710002);
    ensure!(landed_commits(&tx, page)? == vec!["bbbb", "bbcc"]);
    ensure!(
        store
            .related_in(&tx, TaskId::new(710004), &[], 10)?
            .related
            .is_empty()
    );
    // An unindexed own title has a zero own bm25: no search clue, even if
    // other documents match the title. Structural clues remain independent.
    tx.execute("UPDATE tasks SET title='検索候補' WHERE id=710005", &[])?;
    maintain(&tx, SearchWrite::Task(710005))?;
    tx.execute(
        "DELETE FROM search_index WHERE kind='task' AND CAST(ref AS text)='710005'",
        &[],
    )?;
    ensure!(
        store
            .related_in(&tx, TaskId::new(710005), &[], 10)?
            .related
            .is_empty()
    );
    let terms = any_word_query("検索候補を日本語で探す").unwrap();
    let page = store.search_in(
        &tx,
        &SearchQuery {
            terms,
            kinds: vec![SearchKind::Task],
            statuses: vec![],
            goal_id: None,
            limit: 10,
            full: true,
        },
    )?;
    let actual: Vec<_> = page.hits.into_iter().map(|h| h.id).collect();
    ensure!(
        actual
            == vec![
                SearchRef::Id(710001),
                SearchRef::Id(710002),
                SearchRef::Id(710003)
            ],
        "duplicate candidates: {actual:?}"
    );
    tx.execute(
        "UPDATE tasks SET paths='[\"src/related.rs\"]' WHERE id=710002",
        &[],
    )?;
    tx.execute("UPDATE tasks SET status='canceled' WHERE id=710003", &[])?;
    tx.execute("INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit) VALUES ('origin',710001,'failed','codex','codex','base')", &[])?;
    for (id, kind, run, payload) in [
        (
            710010,
            "follow_up_registered",
            Some("origin"),
            r#"{"task_id":710002}"#,
        ),
        (
            710011,
            "follow_up_registered",
            Some("origin"),
            r#"{"task_id":"710002"}"#,
        ),
        (
            710012,
            "task_status_changed",
            None,
            r#"{"to":"canceled","duplicate_of":710001,"reason":"duplicate"}"#,
        ),
        (
            710013,
            "task_status_changed",
            None,
            r#"{"to":"canceled","duplicate_of":710002,"reason":"already_done"}"#,
        ),
    ] {
        let task = if kind == "follow_up_registered" {
            710001
        } else {
            710003
        };
        tx.execute("INSERT INTO run_events(id,task_id,run_id,kind,payload,created_at) VALUES (?1,?2,?3,?4,?5,'2026-01-01')", sql_params![id,task,run,kind,payload])?;
    }
    let docs = crate::infrastructure::related::related_docs(&tx)?;
    let b = docs.iter().find(|d| d.id == 710002).unwrap();
    ensure!(b.paths == vec!["src/related.rs"]);
    ensure!(b.follow_up_of == vec![("origin".into(), 710001)]);
    ensure!(b.texts.len() == 6); // four task columns, two landed messages
    ensure!(docs.iter().find(|d| d.id == 710003).unwrap().duplicate_of == Some(710002));
    Ok(())
}
