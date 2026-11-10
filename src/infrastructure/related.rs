//! `dagq related TASK` on the queue (ADR-0063 decision 4): reads every
//! task with its landed commits' messages, follow-up origins and duplicate
//! mark, asks the search index how strongly the task's title matches each
//! other task, and leaves the scoring to [`crate::domain::related`].
use super::sql_dialect::JsonType;
use super::sql_exec::{OptionalRow, SqlRead, sql_params};
use crate::domain::{cancel_reason, event_kind};
use std::collections::HashMap;

use anyhow::{Context, Result, ensure};
use rusqlite::params;

use super::sqlite::SqliteQueue;
use crate::domain::TaskId;
use crate::domain::related::trigram_query;
use crate::domain::related::{RelatedDoc, RelatedPage, rank};

impl SqliteQueue {
    /// The tasks most related to `task_id`, best first, kept to `statuses`
    /// (empty: all) and cut to `limit`.
    pub fn related(&self, task_id: i64, statuses: &[String], limit: usize) -> Result<RelatedPage> {
        ensure!(limit > 0, "limit must be at least 1");
        related_in(&*self.conn, task_id, statuses, limit, |title| {
            self.title_matches(task_id, title)
        })
    }

    /// The commits that landed the best related completed tasks, in rank order.
    pub fn related_landed_commits(&self, task_id: TaskId, limit: usize) -> Result<Vec<String>> {
        let page = self.related(task_id.as_i64(), &["completed".to_owned()], limit)?;
        landed_commits(&*self.conn, page)
    }

    /// How strongly the index matches the trigrams of `title` against each
    /// other task's title and description, as the ratio of its bm25 to that
    /// of the task itself (1: as strongly as the task matches itself).
    fn title_matches(&self, task_id: i64, title: &str) -> Result<HashMap<i64, f64>> {
        let Some(query) = trigram_query(title) else {
            return Ok(HashMap::new());
        };
        let scores: Vec<(i64, f64)> = self
            .conn
            .prepare(
                "SELECT ref, bm25(search_index) FROM search_index
                 WHERE search_index MATCH ?1 AND kind = 'task'",
            )?
            .query_map(params![query], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let own = scores
            .iter()
            .find(|(id, _)| *id == task_id)
            .map(|(_, score)| *score)
            .filter(|score| *score < 0.0);
        let Some(own) = own else {
            return Ok(HashMap::new());
        };
        Ok(scores
            .into_iter()
            .filter(|(id, _)| *id != task_id)
            .map(|(id, score)| (id, (score / own).clamp(0.0, 1.0)))
            .collect())
    }
}

/// Shared structure reads; the adapter alone supplies the title's search clue.
pub(crate) fn related_in(
    tx: &impl SqlRead,
    task_id: i64,
    statuses: &[String],
    limit: usize,
    matches: impl FnOnce(&str) -> Result<HashMap<i64, f64>>,
) -> Result<RelatedPage> {
    ensure!(limit > 0, "limit must be at least 1");
    let title: String = tx
        .query_row(
            "SELECT title FROM tasks WHERE id = ?1",
            sql_params![task_id],
            |row| row.get(0),
        )
        .optional()?
        .with_context(|| format!("task {task_id} does not exist"))?;
    let docs = related_docs(tx)?;
    rank(task_id, &docs, &matches(&title)?, statuses, limit)
        .with_context(|| format!("task {task_id} does not exist"))
}

pub(crate) fn landed_commits(tx: &impl SqlRead, page: RelatedPage) -> Result<Vec<String>> {
    let mut commits = Vec::new();
    for task in page.related {
        commits.extend(tx.query_map(
            "SELECT commit_sha FROM landed_commits WHERE task_id = ?1 ORDER BY id",
            sql_params![task.id],
            |row| row.get::<_, String>(0),
        )?);
    }
    Ok(commits)
}

pub(crate) fn related_docs(tx: &impl SqlRead) -> Result<Vec<RelatedDoc>> {
    let mut docs: Vec<RelatedDoc> = tx.query_map(
                "SELECT id, status, title, description, acceptance, context, goal_id, CAST(paths AS text) AS paths
                 FROM tasks ORDER BY id",
            &[], |row| {
                let title: String = row.get("title")?;
                let paths: String = row.get("paths")?;
                Ok(RelatedDoc {
                    id: row.get("id")?,
                    status: row.get("status")?,
                    goal_id: row.get("goal_id")?,
                    paths: serde_json::from_str(&paths).unwrap_or_default(),
                    texts: vec![
                        title.clone(),
                        row.get("description")?,
                        row.get("acceptance")?,
                        row.get("context")?,
                    ],
                    title,
                    ..RelatedDoc::default()
                })
            })?;
    let position: HashMap<i64, usize> = docs
        .iter()
        .enumerate()
        .map(|(i, doc)| (doc.id, i))
        .collect();
    let messages: Vec<(i64, String)> = tx.query_map(
        "SELECT task_id, message FROM landed_commits
                 WHERE message IS NOT NULL ORDER BY id",
        &[],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    for (task_id, message) in messages {
        if let Some(&i) = position.get(&task_id) {
            docs[i].texts.push(message);
        }
    }
    // The run whose receipt proposed a follow-up is on the event, with
    // the task it ran; the registered task is in the payload.
    let follow_ups: Vec<(i64, String, i64)> = tx.query_map(
        &format!(
            "SELECT CAST({task_id} AS bigint), run_id, task_id
                 FROM run_events
                 WHERE kind = '{}' AND run_id IS NOT NULL
                   AND task_id IS NOT NULL
                   AND {task_id_type} = 'integer'
                 ORDER BY id",
            event_kind::FOLLOW_UP_REGISTERED,
            task_id = tx
                .dialect()
                .json_field_spaced("payload", "$.task_id", JsonType::Integer),
            task_id_type = tx.dialect().json_type_of("payload", "$.task_id"),
        ),
        &[],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    for (registered, run_id, source) in follow_ups {
        if let Some(&i) = position.get(&registered) {
            docs[i].follow_up_of.push((run_id, source));
        }
    }
    // A cancel records what the task duplicates (decision 5); the last
    // cancel of a task still canceled is the one that holds, and only
    // its reasons of a duplicate pair it (ADR-t947-4 decision 2).
    let duplicates: Vec<(i64, Option<i64>, Option<String>)> = tx.query_map(
        &format!(
            "SELECT e.task_id,
                        CASE WHEN {duplicate_of_type} = 'integer'
                             THEN {duplicate_of} END,
                        {reason}
                 FROM run_events e JOIN tasks t ON t.id = e.task_id
                 WHERE e.kind = '{}' AND t.status = 'canceled'
                   AND {to} = 'canceled'
                 ORDER BY e.id",
            event_kind::TASK_STATUS_CHANGED,
            duplicate_of_type = tx.dialect().json_type_of("e.payload", "$.duplicate_of"),
            duplicate_of =
                tx.dialect()
                    .json_field_spaced("e.payload", "$.duplicate_of", JsonType::Integer),
            reason = tx
                .dialect()
                .json_field_spaced("e.payload", "$.reason", JsonType::Text),
            to = tx
                .dialect()
                .json_field_spaced("e.payload", "$.to", JsonType::Text),
        ),
        &[],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    for (task_id, duplicate_of, reason) in duplicates {
        if let Some(&i) = position.get(&task_id) {
            docs[i].duplicate_of =
                duplicate_of.filter(|_| cancel_reason::pairs_as_duplicate(reason.as_deref()));
        }
    }
    Ok(docs)
}
