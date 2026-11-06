//! Postgres search adapter statements. These replace the search triggers,
//! leaving source mutations to their owners. No deletion entry exists: source
//! rows are retained. Writes and copies share the caller's fenced transaction.
use crate::infrastructure::search_writes::SearchWrite;
use crate::infrastructure::sql_exec::{SqlWrite, sql_params};
use anyhow::Result;

/// The offsets keep the inter-kind tie order of SQLite's search rowid.
#[derive(Clone, Copy)]
enum Kind {
    Task,
    Goal,
    Note,
    Commit,
}
impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Goal => "goal",
            Self::Note => "note",
            Self::Commit => "commit",
        }
    }
    fn order(self, id: i64) -> i64 {
        id * 4
            + match self {
                Self::Task => 0,
                Self::Goal => 1,
                Self::Note => 2,
                Self::Commit => 3,
            }
    }
    fn reference(self, id: i64, sha: &str) -> String {
        if matches!(self, Self::Commit) {
            sha.to_owned()
        } else {
            id.to_string()
        }
    }
}

/// FTS5 counts Unicode characters independently in each body column; neither
/// short columns nor separators contribute tokens to the document length.
fn trigrams(body: &[String; 5]) -> i64 {
    body.iter()
        .map(|s| s.chars().count().saturating_sub(2) as i64)
        .sum()
}
/// A task-owned note keeps the task state even when it names another goal.
fn note_status(task: Option<String>, goal: Option<String>) -> Option<String> {
    task.or(goal)
}
fn goal_status(
    status: Option<String>,
    closed: Option<String>,
    verdict: Option<String>,
) -> Option<String> {
    if closed.is_some() { verdict } else { status }
}

struct Document {
    kind: Kind,
    id: i64,
    sha: String,
    task: Option<i64>,
    goal: Option<i64>,
    run: Option<String>,
    status: Option<String>,
    updated: String,
    body: [String; 5],
}
impl Document {
    fn upsert_document(&self, tx: &impl SqlWrite) -> Result<()> {
        tx.execute("INSERT INTO search_index(doc_order, kind, ref, task_id, goal_id, run_id, status, updated_at, title, description, acceptance, context, text, trigram_count)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
            ON CONFLICT(doc_order) DO UPDATE SET kind=excluded.kind, ref=excluded.ref, task_id=excluded.task_id, goal_id=excluded.goal_id, run_id=excluded.run_id, status=excluded.status, updated_at=excluded.updated_at, title=excluded.title, description=excluded.description, acceptance=excluded.acceptance, context=excluded.context, text=excluded.text, trigram_count=excluded.trigram_count",
            sql_params![self.kind.order(self.id), self.kind.name(), self.kind.reference(self.id, &self.sha), self.task, self.goal, self.run.as_deref(), self.status.as_deref(), self.updated, self.body[0], self.body[1], self.body[2], self.body[3], self.body[4], trigrams(&self.body)])?;
        Ok(())
    }
}

pub(crate) fn maintain(tx: &impl SqlWrite, change: SearchWrite<'_>) -> Result<()> {
    match change {
        SearchWrite::Task(id) => {
            let doc = tx.query_row("SELECT goal_id, status, updated_at, title, description, acceptance, context FROM tasks WHERE id=?1", sql_params![id], |r| Ok(Document {
                kind: Kind::Task, id, sha: String::new(), task: Some(id), goal: r.get(0)?, run: None, status: r.get(1)?, updated: r.get(2)?, body: [r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, String::new()]
            }))?;
            doc.upsert_document(tx)?;
            tx.execute(
                "UPDATE search_index SET status=?2, goal_id=?3 WHERE task_id=?1 AND kind='commit'",
                sql_params![id, doc.status.as_deref(), doc.goal],
            )?;
            tx.execute("UPDATE search_index SET status=?2, goal_id=coalesce((SELECT e.goal_id FROM run_events e WHERE e.id=CAST(search_index.ref AS bigint)), ?3) WHERE task_id=?1 AND kind='note'", sql_params![id, doc.status.as_deref(), doc.goal])?;
        }
        SearchWrite::Goal {
            id,
            previous_acceptance,
        } => {
            let doc = tx.query_row("SELECT status, closed_at, verdict, updated_at, title, description, acceptance, constraints FROM goals WHERE id=?1", sql_params![id], |r| Ok(Document {
                kind: Kind::Goal, id, sha: String::new(), task: None, goal: Some(id), run: None, status: goal_status(r.get(0)?, r.get(1)?, r.get(2)?), updated: r.get(3)?, body: [r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, String::new()]
            }))?;
            if previous_acceptance.is_some_and(|old| old != doc.body[2]) {
                tx.execute(
                    "UPDATE goals SET acceptance_version=acceptance_version+1 WHERE id=?1",
                    sql_params![id],
                )?;
            }
            doc.upsert_document(tx)?;
            tx.execute("UPDATE search_index SET status=?2 WHERE kind='note' AND task_id IS NULL AND goal_id=?1", sql_params![id, doc.status.as_deref()])?;
        }
        SearchWrite::Note(id) => {
            let docs = tx.query_map("SELECT e.task_id, coalesce(e.goal_id,t.goal_id), e.run_id, t.status, g.status, g.closed_at, g.verdict, e.created_at, coalesce(e.payload::jsonb ->> 'text','') FROM run_events e LEFT JOIN tasks t ON t.id=e.task_id LEFT JOIN goals g ON g.id=e.goal_id WHERE e.id=?1 AND e.kind='observation'", sql_params![id], |r| Ok(Document {
                kind: Kind::Note, id, sha: String::new(), task: r.get(0)?, goal: r.get(1)?, run: r.get(2)?, status: note_status(r.get(3)?, goal_status(r.get(4)?, r.get(5)?, r.get(6)?)), updated: r.get(7)?, body: [String::new(), String::new(), String::new(), String::new(), r.get(8)?]
            }))?;
            for doc in docs {
                doc.upsert_document(tx)?;
            }
        }
        SearchWrite::Integrated(id) => {
            let inserted = tx.insert_id_if_inserted("INSERT INTO landed_commits(run_id, task_id, commit_sha, message, git_common_dir, landed_at)
                SELECT run_id, task_id, payload::jsonb ->> 'result_commit', payload::jsonb ->> 'message', payload::jsonb ->> 'git_common_dir', created_at FROM run_events WHERE id=?1 AND kind='run_integrated' AND run_id IS NOT NULL AND task_id IS NOT NULL AND payload::jsonb ->> 'result_commit' IS NOT NULL
                ON CONFLICT(run_id) DO NOTHING", sql_params![id])?;
            if let Some(id) = inserted {
                maintain(tx, SearchWrite::Commit(id))?;
            }
        }
        SearchWrite::Commit(id) => {
            let doc = tx.query_row("SELECT c.commit_sha, c.task_id, t.goal_id, c.run_id, t.status, c.landed_at, coalesce(c.message,'') FROM landed_commits c LEFT JOIN tasks t ON t.id=c.task_id WHERE c.id=?1", sql_params![id], |r| Ok(Document {
                kind: Kind::Commit, id, sha: r.get(0)?, task: r.get(1)?, goal: r.get(2)?, run: r.get(3)?, status: r.get(4)?, updated: r.get(5)?, body: [String::new(), String::new(), String::new(), String::new(), r.get(6)?]
            }))?;
            doc.upsert_document(tx)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn order_and_reference_keep_the_sqlite_interleaving() {
        for (kind, offset) in [
            (Kind::Task, 0),
            (Kind::Goal, 1),
            (Kind::Note, 2),
            (Kind::Commit, 3),
        ] {
            for id in [1, 27, 1_000_000] {
                assert_eq!(kind.order(id), id * 4 + offset);
                assert_eq!(
                    kind.reference(id, "abc123"),
                    if offset == 3 {
                        "abc123".to_owned()
                    } else {
                        id.to_string()
                    }
                );
            }
        }
    }
    #[test]
    fn length_counts_characters_per_column_without_crossing_columns() {
        assert_eq!(
            trigrams(&[
                "日本語検索".into(),
                "ab".into(),
                "".into(),
                "東京駅".into(),
                "abcd".into()
            ]),
            6
        );
        assert_eq!(
            trigrams(&["a".into(), "b".into(), "c".into(), "d".into(), "e".into()]),
            0
        );
    }
    #[test]
    fn notes_prefer_task_state_then_fall_back_to_goal_state() {
        assert_eq!(
            note_status(Some("ready".into()), Some("achieved".into())),
            Some("ready".into())
        );
        assert_eq!(
            note_status(None, Some("abandoned".into())),
            Some("abandoned".into())
        );
        assert_eq!(note_status(None, None), None);
    }
    #[test]
    fn closed_goals_use_the_verdict_including_null() {
        assert_eq!(
            goal_status(Some("open".into()), None, Some("achieved".into())),
            Some("open".into())
        );
        assert_eq!(
            goal_status(
                Some("open".into()),
                Some("at".into()),
                Some("abandoned".into())
            ),
            Some("abandoned".into())
        );
        assert_eq!(
            goal_status(Some("open".into()), Some("at".into()), None),
            None
        );
    }
}
