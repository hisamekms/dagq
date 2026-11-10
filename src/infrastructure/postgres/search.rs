//! LIKE candidates and application bm25 over one snapshot of the whole index.
//! Long queries read all matching rows before limiting; statistics are global,
//! including filtered-out kinds. Each phrase's count can use lower(doc)'s GIN.
//! Short-only queries sort and limit in SQL, without loading ranking statistics.
//!
//! Two permitted differences from SQLite: excerpt uses the domain helper and
//! falls back to first_line when it cannot locate a term (no FTS5 snippet).
//! For one/two-character non-ASCII terms SQLite LIKE folds ASCII only, whereas
//! Postgres lower() also folds É/é with non-C LC_CTYPE. Postgres therefore may
//! return extra matches; Japanese is unaffected. The locale limit is in the ADR.
use super::{PostgresQueue, PostgresSession, PostgresStore};
use crate::application::SearchIndex;
use crate::domain::{TaskId, related::RelatedPage, search::*, worker::Worker};
use crate::infrastructure::sql_exec::{SqlRead, SqlRow, SqlValue, TransactionEntries};
use anyhow::{Result, ensure};
use serde_json::json;

const COLUMNS: [&str; 5] = ["title", "description", "acceptance", "context", "text"];

impl<S: PostgresSession> SearchIndex for PostgresQueue<S> {
    fn search_documents(&self, query: &SearchQuery) -> Result<SearchPage> {
        self.entries.search(query)
    }
    fn related_landed_commits(&self, task: TaskId, limit: usize) -> Result<Vec<String>> {
        let tx = self.entries.read_entry("related_landed_commits")?;
        let page = related(&tx, task, &["completed".into()], limit)?;
        crate::infrastructure::related::landed_commits(&tx, page)
    }
    fn related_tasks(
        &self,
        task: TaskId,
        statuses: &[String],
        limit: usize,
    ) -> Result<RelatedPage> {
        related(&self.entries.read_entry("related")?, task, statuses, limit)
    }
}

impl<S: PostgresSession> PostgresStore<S> {
    /// Search on a repeatable-read snapshot; relatedness is a separate query.
    pub fn search(&self, query: &SearchQuery) -> Result<SearchPage> {
        search(&self.read_entry("search")?, query)
    }
}

fn pattern(term: &str) -> String {
    format!(
        "%{}%",
        term.replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
}
fn term_sql(term: &str, values: &mut Vec<SqlValue>) -> String {
    values.push(SqlValue::Text(pattern(term)));
    format!("lower(doc) LIKE lower(?{}) ESCAPE '\\'", values.len())
}
fn expression_sql(expr: &SearchExpr, values: &mut Vec<SqlValue>) -> String {
    match expr {
        SearchExpr::Term(term) => term_sql(term, values),
        SearchExpr::And(children) | SearchExpr::Or(children) => {
            let op = if matches!(expr, SearchExpr::And(_)) {
                " AND "
            } else {
                " OR "
            };
            format!(
                "({})",
                children
                    .iter()
                    .map(|e| expression_sql(e, values))
                    .collect::<Vec<_>>()
                    .join(op)
            )
        }
        SearchExpr::Not(left, right) => format!(
            "({} AND NOT ({}))",
            expression_sql(left, values),
            expression_sql(right, values)
        ),
    }
}

/// Counts overlapping instances separately within each column. Unicode simple
/// lowercase preserves CAFÉ/café; no trigrams cross the column separators.
fn occurrences(texts: &[String], term: &str) -> u64 {
    let needle: Vec<_> = term.chars().flat_map(char::to_lowercase).collect();
    texts
        .iter()
        .map(|text| {
            let folded: Vec<_> = text.chars().flat_map(char::to_lowercase).collect();
            folded
                .windows(needle.len())
                .filter(|part| *part == needle)
                .count() as u64
        })
        .sum()
}

/// FTS5 only exposes instances in satisfied boolean branches. Keep phrase
/// order and zero inactive branches, including the right side of NOT.
fn active(expr: &SearchExpr, texts: &[String]) -> (bool, Vec<u64>) {
    match expr {
        SearchExpr::Term(term) => {
            let n = occurrences(texts, term);
            (n > 0, vec![n])
        }
        SearchExpr::And(children) | SearchExpr::Or(children) => {
            let parts: Vec<_> = children.iter().map(|e| active(e, texts)).collect();
            let matched = if matches!(expr, SearchExpr::And(_)) {
                parts.iter().all(|p| p.0)
            } else {
                parts.iter().any(|p| p.0)
            };
            let counts = parts
                .into_iter()
                .flat_map(|(_, counts)| counts)
                .map(|n| if matched { n } else { 0 })
                .collect();
            (matched, counts)
        }
        SearchExpr::Not(left, right) => {
            let (yes, mut counts) = active(left, texts);
            let (no, excluded) = active(right, texts);
            if !yes || no {
                counts.fill(0);
            }
            counts.extend(excluded.iter().map(|_| 0));
            (yes && !no, counts)
        }
    }
}

pub(crate) fn search(tx: &impl SqlRead, query: &SearchQuery) -> Result<SearchPage> {
    ensure!(query.limit > 0, "limit must be at least 1");
    let terms = parse_terms(&query.terms)?;
    let expr = terms.expression()?;
    let mut values = Vec::new();
    let mut filters = Vec::new();
    if let Some(expr) = &expr {
        filters.push(expression_sql(expr, &mut values));
    }
    for term in &terms.short {
        filters.push(term_sql(term, &mut values));
    }
    for (column, entries) in [
        (
            "kind",
            query
                .kinds
                .iter()
                .map(|k| k.as_str().to_owned())
                .collect::<Vec<_>>(),
        ),
        ("status", query.statuses.clone()),
    ] {
        if !entries.is_empty() {
            let marks = entries
                .into_iter()
                .map(|value| {
                    values.push(SqlValue::Text(value));
                    format!("?{}", values.len())
                })
                .collect::<Vec<_>>()
                .join(",");
            filters.push(format!("s.{column} IN ({marks})"));
        }
    }
    if let Some(goal) = query.goal_id {
        values.push(SqlValue::Integer(goal.as_i64()));
        filters.push(format!("s.goal_id=?{}", values.len()));
    }
    let filter = filters.join(" AND ");
    let total: i64 = tx.query_row(
        &format!("SELECT count(*) FROM search_index s WHERE {filter}"),
        &values,
        |r| r.get(0),
    )?;
    let mut statistics = Vec::new();
    let (rows, trigrams) = if expr.is_some() {
        for term in &terms.long {
            let mut parameter = Vec::new();
            let condition = term_sql(term, &mut parameter);
            let n: i64 = tx.query_row(
                &format!("SELECT count(*) FROM search_index WHERE {condition}"),
                &parameter,
                |r| r.get(0),
            )?;
            statistics.push(n as u64);
        }
        tx.query_row(
            "SELECT count(*), CAST(coalesce(sum(trigram_count),0) AS bigint) FROM search_index",
            &[],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)? as u64)),
        )?
    } else {
        (0, 0)
    };
    let order = if expr.is_some() {
        String::new()
    } else {
        values.push(SqlValue::Integer(i64::try_from(query.limit)?));
        format!(
            " ORDER BY s.updated_at DESC, doc_order DESC LIMIT ?{}",
            values.len()
        )
    };
    let sql = format!("SELECT s.doc_order, s.kind, s.ref, s.task_id, s.goal_id, s.run_id, s.status, s.title, s.description, s.acceptance, s.context, s.text, s.trigram_count, t.worker_provider, t.worker_mode, t.change, CAST(g.tags AS text) AS goal_tags FROM search_index s
        LEFT JOIN tasks t ON s.kind='task' AND CAST(t.id AS text)=s.ref
        LEFT JOIN goals g ON s.kind='goal' AND CAST(g.id AS text)=s.ref
        WHERE {filter}{order}");
    let mut candidates = tx.query_map(&sql, &values, |r| {
        let texts = COLUMNS
            .iter()
            .map(|c| r.get::<_, String>(*c))
            .collect::<crate::infrastructure::sql_exec::SqlResult<Vec<_>>>()?;
        Ok((
            r.get::<_, i64>("doc_order")?,
            r.get::<_, i64>("trigram_count")?,
            texts,
            read_hit(r)?,
        ))
    })?;
    for (_, size, texts, hit) in &mut candidates {
        if let Some(expr) = &expr {
            let (_, counts) = active(expr, texts);
            let phrases: Vec<_> = counts
                .into_iter()
                .zip(&statistics)
                .map(|(occurrences, &matching_rows)| Bm25Phrase {
                    occurrences,
                    matching_rows,
                })
                .collect();
            hit.score = Some(bm25(rows, trigrams, *size as u64, &phrases));
        }
    }
    if expr.is_some() {
        candidates.sort_by(|a, b| {
            a.3.score
                .unwrap()
                .total_cmp(&b.3.score.unwrap())
                .then_with(|| b.0.cmp(&a.0))
        });
    }
    candidates.truncate(query.limit);
    let words: Vec<_> = terms.long.iter().chain(&terms.short).collect();
    for (_, _, texts, hit) in &mut candidates {
        let (column, preview) = COLUMNS
            .iter()
            .zip(texts.iter())
            .find_map(|(column, text)| {
                words
                    .iter()
                    .find_map(|word| excerpt(text, word, 24))
                    .map(|s| (*column, s))
            })
            .unwrap_or_else(|| {
                let i = texts.iter().position(|s| !s.is_empty()).unwrap_or(0);
                (COLUMNS[i], first_line(&texts[i], 80))
            });
        hit.field = field_name(hit.kind, column).to_owned();
        hit.excerpt = preview;
        hit.title = if matches!(hit.kind, SearchKind::Task | SearchKind::Goal) {
            texts[0].clone()
        } else {
            first_line(&texts[4], 80)
        };
        hit.fields = query.full.then(|| {
            COLUMNS
                .iter()
                .zip(texts.iter())
                .filter(|(name, text)| match hit.kind {
                    SearchKind::Task | SearchKind::Goal => **name != "text",
                    _ => **name == "text" || !text.is_empty(),
                })
                .map(|(name, text)| (field_name(hit.kind, name).to_owned(), json!(text)))
                .collect()
        });
    }
    let hits = candidates
        .into_iter()
        .take(query.limit)
        .map(|(_, _, _, mut hit)| {
            if !query.full {
                hit.score = None;
            }
            hit
        })
        .collect();
    Ok(SearchPage {
        hits,
        total: usize::try_from(total)?,
    })
}

fn read_hit(r: &SqlRow<'_>) -> crate::infrastructure::sql_exec::SqlResult<SearchHit> {
    // Stored kinds are constrained by the writer; use the same typed reader
    // conversion as other execution-port enum columns.
    let kind: SearchKind = r.get::<_, String>("kind")?.parse().map_err(|e| {
        crate::infrastructure::sql_exec::SqlError::Conversion {
            index: 0,
            stored: crate::infrastructure::sql_exec::ValueKind::Text,
            cause: Box::new(e),
        }
    })?;
    let reference: String = r.get("ref")?;
    let id = if kind == SearchKind::Commit {
        SearchRef::Commit(reference)
    } else {
        SearchRef::Id(reference.parse().map_err(|e| {
            crate::infrastructure::sql_exec::SqlError::Conversion {
                index: 0,
                stored: crate::infrastructure::sql_exec::ValueKind::Text,
                cause: Box::new(e),
            }
        })?)
    };
    let worker = if kind == SearchKind::Task {
        Some(
            Worker::resolve(
                r.get::<_, Option<String>>("worker_provider")?
                    .and_then(|s| s.parse().ok()),
                r.get::<_, Option<String>>("worker_mode")?
                    .and_then(|s| s.parse().ok()),
            )
            .unwrap_or(Worker::DEFAULT),
        )
    } else {
        None
    };
    Ok(SearchHit {
        kind,
        id,
        status: r.get("status")?,
        title: String::new(),
        field: String::new(),
        excerpt: String::new(),
        task_id: r
            .get::<_, Option<i64>>("task_id")?
            .filter(|_| kind != SearchKind::Task),
        goal_id: r
            .get::<_, Option<i64>>("goal_id")?
            .filter(|_| kind != SearchKind::Goal),
        run_id: r.get("run_id")?,
        provider: worker.map(|w| w.provider),
        worker_mode: worker.map(|w| w.mode),
        change: r
            .get::<_, Option<String>>("change")?
            .and_then(|s| s.parse().ok()),
        tags: r
            .get::<_, Option<String>>("goal_tags")?
            .map(|s| serde_json::from_str(&s).unwrap_or_default()),
        score: None,
        fields: None,
    })
}

/// Reads structural clues and search statistics from the same repeatable-read
/// snapshot. Ranking and all clue weights remain in the domain.
pub(crate) fn related(
    tx: &impl SqlRead,
    task: TaskId,
    statuses: &[String],
    limit: usize,
) -> Result<RelatedPage> {
    crate::infrastructure::related::related_in(tx, task.as_i64(), statuses, limit, |title| {
        title_matches(tx, task, title)
    })
}

/// At most 200 OR-ed trigrams select the union of title/description matches
/// across ALL kinds. Every phrase occurrence lies in this union, so its n is
/// counted here without one query per phrase. N and the sum of D are global;
/// D includes all five columns. Only task rows become candidates. This reads
/// C rows (two text columns) and holds C * K counts, K <= 200, before reducing
/// scores to ratios. It intentionally never truncates the union before bm25.
/// The td newline cannot match a gram (grams contain no whitespace).
pub(crate) fn title_matches(
    tx: &impl SqlRead,
    task: TaskId,
    title: &str,
) -> Result<std::collections::HashMap<i64, f64>> {
    use crate::domain::related::title_trigrams;
    let grams = title_trigrams(title);
    if grams.is_empty() {
        return Ok(Default::default());
    }
    let mut values = Vec::new();
    let filter = grams
        .iter()
        .map(|g| {
            values.push(SqlValue::Text(pattern(g)));
            format!("lower(td) LIKE lower(?{}) ESCAPE '\\'", values.len())
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    let (rows, total): (i64, i64) = tx.query_row(
        "SELECT count(*), CAST(coalesce(sum(trigram_count),0) AS bigint) FROM search_index",
        &[],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let candidates = tx.query_map(
        &format!(
            "SELECT kind, ref, title, description, trigram_count FROM search_index WHERE {filter}"
        ),
        &values,
        |r| {
            let texts = vec![
                r.get::<_, String>("title")?,
                r.get::<_, String>("description")?,
            ];
            Ok((
                r.get::<_, String>("kind")?,
                r.get::<_, String>("ref")?,
                r.get::<_, i64>("trigram_count")?,
                grams
                    .iter()
                    .map(|g| occurrences(&texts, g))
                    .collect::<Vec<_>>(),
            ))
        },
    )?;
    let statistics: Vec<_> = (0..grams.len())
        .map(|i| candidates.iter().filter(|r| r.3[i] > 0).count() as u64)
        .collect();
    let scores = candidates
        .into_iter()
        .filter(|r| r.0 == "task")
        .map(|(_, id, size, counts)| {
            let phrases = counts
                .into_iter()
                .zip(&statistics)
                .map(|(occurrences, &matching_rows)| Bm25Phrase {
                    occurrences,
                    matching_rows,
                })
                .collect::<Vec<_>>();
            Ok((
                id.parse::<i64>()?,
                bm25(rows as u64, total as u64, size as u64, &phrases),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let Some(own) = scores
        .iter()
        .find(|r| r.0 == task.as_i64())
        .map(|r| r.1)
        .filter(|s| *s < 0.0)
    else {
        return Ok(Default::default());
    };
    Ok(scores
        .into_iter()
        .filter(|r| r.0 != task.as_i64())
        .map(|(id, score)| (id, (score / own).clamp(0.0, 1.0)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::sql_exec::{SqlDialect, SqlResult};

    /// Runs the adapter's read statements on an ordinary memdb table; FTS5
    /// independently supplies the expected scores, never the candidate filter.
    struct Index(rusqlite::Connection);
    impl SqlRead for Index {
        fn dialect(&self) -> SqlDialect {
            SqlDialect::Postgres
        }
        fn visit_rows(
            &self,
            sql: &str,
            params: &[SqlValue],
            visit: &mut dyn FnMut(&SqlRow<'_>) -> SqlResult<bool>,
        ) -> SqlResult<()> {
            crate::infrastructure::sql_exec::sqlite::port_tx(&self.0).visit_rows(
                &sql.replace("search_index", "pg_index"),
                params,
                visit,
            )
        }
    }

    #[test]
    fn title_union_ratios_match_fts5() {
        use crate::domain::related::trigram_query;
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE VIRTUAL TABLE search_index USING fts5(kind UNINDEXED, ref UNINDEXED, title,description,acceptance,context,text,tokenize='trigram');
            CREATE TABLE pg_index(kind TEXT,ref TEXT,title TEXT,description TEXT,trigram_count INTEGER,td TEXT);").unwrap();
        for (i, (kind, title, body, other)) in [
            ("task", "検索候補を日本語で探す", "", "extra text changes D"),
            ("task", "日本語の検索候補を抽出する", "", ""),
            ("task", "検索候補", "", ""),
            (
                "goal",
                "検索候補",
                "日本語",
                "large global statistics contribution",
            ),
            ("task", "100% a_b c\\d", "alpha ALPHA aaaaa", ""),
            ("note", "alpha", "", ""),
            ("task", "ab", "", ""),
            ("task", "100X aXb cXd", "", ""),
            ("task", "aaaaa", "", "alpha"),
            ("commit", "unrelated", "", "検索候補"),
        ]
        .into_iter()
        .enumerate()
        {
            let id = i as i64 + 1;
            db.execute("INSERT INTO search_index(rowid,kind,ref,title,description,acceptance,context,text) VALUES (?1,?2,?3,?4,?5,?6,'','')", rusqlite::params![id,kind,id.to_string(),title,body,other]).unwrap();
            let size: usize = [title, body, other]
                .iter()
                .map(|s| s.chars().count().saturating_sub(2))
                .sum();
            db.execute(
                "INSERT INTO pg_index VALUES (?1,?2,?3,?4,?5,?6)",
                rusqlite::params![
                    kind,
                    id.to_string(),
                    title,
                    body,
                    size as i64,
                    format!("{title}\n{body}")
                ],
            )
            .unwrap();
        }
        let index = Index(db);
        for (id, title) in [
            (1, "検索候補を日本語で探す"),
            (5, "100% a_b c\\d"),
            (5, "ALPHA"),
            (7, "ab"),
            (9, "aaaaa"),
            (7, "検索候補"),
            (99, "alpha"),
        ] {
            let actual = title_matches(&index, TaskId::new(id), title).unwrap();
            let Some(query) = trigram_query(title) else {
                assert!(actual.is_empty());
                continue;
            };
            let scores = index.0.prepare("SELECT ref,bm25(search_index) FROM search_index WHERE search_index MATCH ?1 AND kind='task'").unwrap().query_map([query], |r| Ok((r.get::<_,String>(0)?.parse::<i64>().unwrap(),r.get::<_,f64>(1)?))).unwrap().map(Result::unwrap).collect::<Vec<_>>();
            let own = scores
                .iter()
                .find(|s| s.0 == id)
                .map(|s| s.1)
                .filter(|s| *s < 0.0);
            let expected: std::collections::HashMap<_, _> = own
                .map(|own| {
                    scores
                        .into_iter()
                        .filter(|r| r.0 != id)
                        .map(|(id, s)| (id, (s / own).clamp(0.0, 1.0)))
                        .collect()
                })
                .unwrap_or_default();
            assert_eq!(actual.len(), expected.len(), "{title}");
            for (id, expected) in expected {
                assert!(
                    (actual[&id] - expected).abs() < 1e-12,
                    "{title}: {id}: {} != {expected}",
                    actual[&id]
                );
            }
        }
    }

    #[test]
    fn boolean_instances_scores_and_candidate_sql_match_fts5() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE VIRTUAL TABLE search_index USING fts5(title,description,acceptance,context,text,tokenize='trigram');
            CREATE TABLE tasks(id INTEGER,worker_provider TEXT,worker_mode TEXT,change TEXT);
            CREATE TABLE goals(id INTEGER,tags TEXT);
            CREATE TABLE pg_index(doc_order INTEGER,kind TEXT,ref TEXT,task_id INTEGER,goal_id INTEGER,run_id TEXT,status TEXT,updated_at TEXT,title TEXT,description TEXT,acceptance TEXT,context TEXT,text TEXT,trigram_count INTEGER,doc TEXT);").unwrap();
        for (i, texts) in [
            ["alpha", "beta gamma", "", "", ""],
            ["alpha alpha", "", "", "", ""],
            ["beta", "alpha", "", "", ""],
            ["gamma", "", "", "", ""],
            ["other unrelated", "100% a_b c\\d", "", "", ""],
            ["alpha alpha", "", "", "", ""],
            ["aaaaa", "", "", "", ""],
        ]
        .iter()
        .enumerate()
        {
            let id = i as i64 + 1;
            db.execute("INSERT INTO search_index(rowid,title,description,acceptance,context,text) VALUES (?1,?2,?3,?4,?5,?6)",rusqlite::params![id,texts[0],texts[1],texts[2],texts[3],texts[4]]).unwrap();
            let size: usize = texts
                .iter()
                .map(|s| s.chars().count().saturating_sub(2))
                .sum();
            db.execute("INSERT INTO pg_index SELECT rowid,'task',CAST(rowid AS text),rowid,NULL,NULL,'draft','fixed',title,description,acceptance,context,text,?2,?3 FROM search_index WHERE rowid=?1",rusqlite::params![id,size as i64,texts.join("\n")]).unwrap();
        }
        let index = Index(db);
        for terms in [
            "alpha",
            "alpha OR beta",
            "alpha NOT beta",
            "alpha OR (beta AND gamma)",
            "(alpha NOT beta) OR gamma",
            "alpha NOT (beta OR gamma)",
            "alpha OR alpha",
            "alpha alpha",
            "aaa",
            "100%",
            "a_b",
            "c\\d",
        ] {
            let query = SearchQuery {
                terms: terms.into(),
                kinds: vec![],
                statuses: vec![],
                goal_id: None,
                limit: 100,
                full: true,
            };
            let actual = search(&index, &query).unwrap();
            let fts = parse_terms(terms).unwrap().fts.unwrap();
            let expected: Vec<(i64,f64)>=index.0.prepare("SELECT rowid,bm25(search_index) FROM search_index WHERE search_index MATCH ?1 ORDER BY bm25(search_index),rowid DESC").unwrap().query_map([fts],|r|Ok((r.get(0)?,r.get(1)?))).unwrap().map(Result::unwrap).collect();
            assert_eq!(actual.total, expected.len(), "{terms}");
            for (hit, (id, score)) in actual.hits.iter().zip(expected) {
                assert_eq!(hit.id, SearchRef::Id(id), "{terms}");
                assert!(
                    (hit.score.unwrap() - score).abs() < 1e-12,
                    "{terms}: {:?} != {score}",
                    hit.score
                );
            }
        }
        let short = search(
            &index,
            &SearchQuery {
                terms: "al".into(),
                kinds: vec![SearchKind::Task],
                statuses: vec!["draft".into()],
                goal_id: None,
                limit: 1,
                full: false,
            },
        )
        .unwrap();
        assert_eq!(short.total, 4);
        assert_eq!(short.hits[0].id, SearchRef::Id(6));
        assert!(short.hits[0].score.is_none());
    }
}
