//! [`postgres_search_writes`] is Postgres-only: it writes source rows directly
//! and copies them with the entry, checking every column of the trigger's
//! values. [`source_search_writes`] runs in both adapters through the source
//! writes themselves. All rows roll back.
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
/// schema, the entry called directly.
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

/// What `dagq search` reads of a document, in both adapters' index.
#[derive(Debug, PartialEq)]
struct Document {
    task: Option<i64>,
    goal: Option<i64>,
    status: Option<String>,
    title: String,
    acceptance: String,
    text: String,
}

/// The document `kind` `reference`; FTS5 keeps a ref as an integer or a
/// string, Postgres as text, so both are read as text.
fn document(tx: &impl SqlRead, kind: &str, reference: &str) -> Result<Option<Document>> {
    use crate::infrastructure::sql_exec::OptionalRow;
    Ok(tx
        .query_row(
            "SELECT task_id, goal_id, status, title, acceptance, text FROM search_index
             WHERE kind = ?1 AND CAST(ref AS TEXT) = ?2",
            sql_params![kind, reference],
            |r| {
                Ok(Document {
                    task: r.get(0)?,
                    goal: r.get(1)?,
                    status: r.get(2)?,
                    title: r.get(3)?,
                    acceptance: r.get(4)?,
                    text: r.get(5)?,
                })
            },
        )
        .optional()?)
}

fn expect(
    tx: &impl SqlRead,
    kind: &str,
    reference: impl ToString,
    expected: (Option<i64>, Option<i64>, &str),
    texts: (&str, &str, &str),
) -> Result<()> {
    let reference = reference.to_string();
    let found = document(tx, kind, &reference)?;
    let (task, goal, status) = expected;
    let (title, acceptance, text) = texts;
    let wanted = Document {
        task,
        goal,
        status: Some(status.to_owned()),
        title: title.to_owned(),
        acceptance: acceptance.to_owned(),
        text: text.to_owned(),
    };
    ensure!(
        found.as_ref() == Some(&wanted),
        "{kind} {reference}: {found:?} != {wanted:?}"
    );
    Ok(())
}

/// The adapter's own source writes (tasks, goals, notes, landings and
/// commit messages) keep the index in their transaction: SQLite's triggers,
/// Postgres's search writes. Everything rolls back with the transaction.
pub fn source_search_writes<S: ContractStore, F: Fn() -> Result<S>>(open: &F) -> Result<()> {
    use crate::domain::task::MovedBy;
    use crate::domain::{
        EventKind, GoalEdit, GoalVerdict, NewGoal, NewNote, NewTask, NoteTarget, Priority, RunId,
        TaskAction,
    };
    use crate::infrastructure::{landed_commits, store_writes, task_store};

    let store = open()?;
    // A fixed time: the case compares no timestamps, and reads no clock.
    struct At(std::time::SystemTime);
    impl crate::application::Clock for At {
        fn system_time(&self) -> std::time::SystemTime {
            self.0
        }
        fn monotonic(&self) -> std::time::Instant {
            unreachable!("the source writes read no monotonic time")
        }
    }
    let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_791_072_000);
    let generators = crate::application::Generators {
        clock: std::sync::Arc::new(At(at)),
        ids: std::sync::Arc::new(crate::infrastructure::clock::UuidGenerator),
    };
    let now = crate::application::timestamp(at);
    let refs = {
        let tx = store.write_entry("contract_source_search_writes")?;
        let mut writes = task_store::Store {
            tx: &tx,
            runs_dir: std::path::Path::new("runs"),
            generators: &generators,
            changes: &None,
            title_prefixes: &None,
            goal_tags: &None,
        };
        let goal = |title: &str, draft| NewGoal {
            title: title.into(),
            acceptance: "条件文".into(),
            draft,
            ..NewGoal::default()
        };
        let source = writes.add_goal(goal("検索の目標", false))?.id();
        let other = writes.add_goal(goal("other", true))?.id();
        let (g, o) = (source.as_i64(), other.as_i64());
        expect(
            &tx,
            "goal",
            g,
            (None, Some(g), "open"),
            ("検索の目標", "条件文", ""),
        )?;
        expect(
            &tx,
            "goal",
            o,
            (None, Some(o), "draft"),
            ("other", "条件文", ""),
        )?;
        writes.ready_goal(other)?;
        expect(
            &tx,
            "goal",
            o,
            (None, Some(o), "open"),
            ("other", "条件文", ""),
        )?;

        let task = writes
            .add(NewTask {
                change: None,
                title: "検索のタスク".into(),
                description: String::new(),
                acceptance: "accept".into(),
                verification_commands: Vec::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                priority: Some(Priority::Normal),
                dependencies: Vec::new(),
                goal_dependencies: Vec::new(),
                goal_id: Some(source),
                context: String::new(),
                provider: None,
                worker_mode: None,
                wait_for_build: false,
                execution_class: Default::default(),
            })?
            .id();
        let t = task.as_i64();
        expect(
            &tx,
            "task",
            t,
            (Some(t), Some(g), "draft"),
            ("検索のタスク", "accept", ""),
        )?;

        let note = |writes: &mut task_store::Store<'_, _>, target| -> Result<i64> {
            Ok(writes
                .add_note(NewNote {
                    target,
                    text: "観測の記録".into(),
                    kind: None,
                    by: "human".into(),
                })?
                .id
                .as_i64())
        };
        let task_note = note(&mut writes, NoteTarget::Task(task))?;
        let goal_note = note(&mut writes, NoteTarget::Goal(source))?;
        // A note of the task recorded with a goal of its own keeps it.
        let owned_note = tx.insert_id(
            "INSERT INTO run_events(task_id, goal_id, kind, payload) VALUES (?1, ?2, 'observation', '{\"text\":\"観測の記録\"}')",
            sql_params![task, other],
        )?;
        crate::infrastructure::search_writes::maintain(&tx, SearchWrite::Note(owned_note))?;
        let noted = ("", "", "観測の記録");
        expect(&tx, "note", task_note, (Some(t), Some(g), "draft"), noted)?;
        expect(&tx, "note", goal_note, (None, Some(g), "open"), noted)?;
        expect(&tx, "note", owned_note, (Some(t), Some(o), "draft"), noted)?;

        // A landing without its message; a second one of the run keeps it.
        let run = RunId::new("contract-source-search".to_owned())?;
        tx.execute(
            "INSERT INTO task_runs(id, task_id, status, requested_provider, actual_provider, base_commit) VALUES (?1, ?2, 'integrated', 'codex', 'codex', 'base')",
            sql_params![run, task],
        )?;
        for sha in ["abc123", "wrong"] {
            let event = store_writes::event_id(
                &tx,
                task,
                Some(&run),
                EventKind::RunIntegrated,
                serde_json::json!({"result_commit": sha, "git_common_dir": "/repo"}),
            )?;
            crate::infrastructure::search_writes::maintain(
                &tx,
                SearchWrite::Integrated(event.as_i64()),
            )?;
        }
        let missing = landed_commits::missing(&tx)?;
        ensure!(
            missing.len() == 1 && missing[0].commit == "abc123",
            "{missing:?}"
        );
        expect(
            &tx,
            "commit",
            "abc123",
            (Some(t), Some(g), "draft"),
            ("", "", ""),
        )?;
        ensure!(document(&tx, "commit", "wrong")?.is_none());
        ensure!(landed_commits::fill(&tx, missing[0].id, "landed message")? == 1);
        ensure!(landed_commits::fill(&tx, missing[0].id, "again")? == 0);
        expect(
            &tx,
            "commit",
            "abc123",
            (Some(t), Some(g), "draft"),
            ("", "", "landed message"),
        )?;

        // The task's state and goal reach its notes and commit.
        task_store::set_goal_in(&tx, task, Some(other), &now, MovedBy::Person)?;
        task_store::transition_task(&tx, task, TaskAction::Submit, &now)?;
        let moved = (Some(t), Some(o), "submitted");
        expect(&tx, "task", t, moved, ("検索のタスク", "accept", ""))?;
        expect(&tx, "note", task_note, moved, noted)?;
        expect(&tx, "note", owned_note, moved, noted)?;
        expect(&tx, "commit", "abc123", moved, ("", "", "landed message"))?;
        task_store::set_goal_in(&tx, task, Some(source), &now, MovedBy::Person)?;
        expect(
            &tx,
            "note",
            task_note,
            (Some(t), Some(g), "submitted"),
            noted,
        )?;
        expect(
            &tx,
            "note",
            owned_note,
            (Some(t), Some(o), "submitted"),
            noted,
        )?;
        task_store::set_goal_in(&tx, task, Some(other), &now, MovedBy::Person)?;

        // An acceptance changed advances its version once; the same does not.
        let edit = |acceptance: &str| GoalEdit {
            acceptance: Some(acceptance.into()),
            ..GoalEdit::default()
        };
        let version = |tx: &S::Write<'_>| -> Result<i64> {
            Ok(tx.query_row(
                "SELECT acceptance_version FROM goals WHERE id = ?1",
                sql_params![source],
                |r| r.get(0),
            )?)
        };
        let before = version(&tx)?;
        writes.edit_goal(source, edit("変えた条件"))?;
        ensure!(version(&tx)? == before + 1);
        writes.edit_goal(source, edit("変えた条件"))?;
        ensure!(version(&tx)? == before + 1);
        expect(
            &tx,
            "goal",
            g,
            (None, Some(g), "open"),
            ("検索の目標", "変えた条件", ""),
        )?;

        // A closed goal reads as its verdict, as do its own notes.
        task_store::close_goal_in(
            &tx,
            source,
            GoalVerdict::Abandoned,
            &now,
            serde_json::json!({}),
        )?;
        expect(
            &tx,
            "goal",
            g,
            (None, Some(g), "abandoned"),
            ("検索の目標", "変えた条件", ""),
        )?;
        expect(&tx, "note", goal_note, (None, Some(g), "abandoned"), noted)?;
        // Dropped without commit: the transaction rolls back.
        [
            ("goal", g.to_string()),
            ("goal", o.to_string()),
            ("task", t.to_string()),
            ("note", task_note.to_string()),
            ("note", goal_note.to_string()),
            ("note", owned_note.to_string()),
            ("commit", "abc123".to_owned()),
        ]
    };
    let tx = store.read_entry("contract_source_search_writes_rolled_back")?;
    for (kind, reference) in &refs {
        ensure!(
            document(&tx, kind, reference)?.is_none(),
            "{kind} {reference} stayed after the rollback"
        );
    }
    ensure!(
        tx.query_row(
            "SELECT count(*) FROM landed_commits WHERE run_id = 'contract-source-search'",
            &[],
            |r| r.get::<_, i64>(0)
        )? == 0
    );
    Ok(())
}
