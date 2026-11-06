//! `dagq search` on the queue's FTS5 index (ADR-0063 decisions 1–3). The
//! index and the triggers that keep it current are migration 0026's; this
//! module only reads it, and fills the landed commits whose message the
//! `run_integrated` payload lacked.
use anyhow::{Result, ensure};
use rusqlite::{Row, params, params_from_iter, types::Value};
use serde_json::{Map, json};

use crate::application::SearchIndex;
use crate::domain::{TaskId, related::RelatedPage};

use super::sqlite::{SqliteQueue, optional_enum_col};
use crate::domain::search::{
    SearchHit, SearchKind, SearchPage, SearchQuery, SearchRef, excerpt, field_name, first_line,
    parse_terms,
};
use crate::domain::{DomainError, worker::Worker};

/// The indexed columns, in the order of the table, and their index in it.
const COLUMNS: [(&str, usize); 5] = [
    ("title", 7),
    ("description", 8),
    ("acceptance", 9),
    ("context", 10),
    ("text", 11),
];
/// Around the match in an excerpt: characters on each side, and FTS5
/// tokens (trigrams, the most `snippet` takes) for the fallback.
const SNIPPET_TOKENS: usize = 64;
const EXCERPT_CHARS: usize = 24;
/// What `snippet` marks a match with, replaced by `«` and `»` once read, so
/// a column containing those characters is not taken for a match.
const OPEN: &str = "\u{2}";
const CLOSE: &str = "\u{3}";
const TITLE_CHARS: usize = 80;

/// SQLite's search adapter: the index reads and relatedness joins stay
/// here and in `related`, behind [`SearchIndex`] (ADR-0063 decisions 1–4).
impl SearchIndex for SqliteQueue {
    fn related_landed_commits(&self, task: TaskId, limit: usize) -> Result<Vec<String>> {
        SqliteQueue::related_landed_commits(self, task, limit)
    }
    fn related_tasks(
        &self,
        task: TaskId,
        statuses: &[String],
        limit: usize,
    ) -> Result<RelatedPage> {
        SqliteQueue::related(self, task.as_i64(), statuses, limit)
    }
    fn search_documents(&self, query: &SearchQuery) -> Result<SearchPage> {
        SqliteQueue::search(self, query)
    }
}

impl SqliteQueue {
    /// Documents matching `query`, best first. Terms of three or more
    /// characters are looked up in the index and ranked by bm25; shorter
    /// ones are `LIKE` filters, and a query with only those is ordered by
    /// the most recent update.
    pub fn search(&self, query: &SearchQuery) -> Result<SearchPage> {
        ensure!(query.limit > 0, "limit must be at least 1");
        let terms = parse_terms(&query.terms)?;
        let mut filters = Vec::new();
        let mut values: Vec<Value> = Vec::new();
        if let Some(fts) = &terms.fts {
            filters.push("search_index MATCH ?".to_owned());
            values.push(Value::from(fts.clone()));
        }
        let all = COLUMNS
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(" || char(10) || ");
        for short in &terms.short {
            filters.push(format!("({all}) LIKE ? ESCAPE '\\'"));
            let escaped = short
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            values.push(Value::from(format!("%{escaped}%")));
        }
        if !query.kinds.is_empty() {
            filters.push(format!("kind IN ({})", marks(query.kinds.len())));
            values.extend(
                query
                    .kinds
                    .iter()
                    .map(|kind| Value::from(kind.as_str().to_owned())),
            );
        }
        if !query.statuses.is_empty() {
            filters.push(format!("status IN ({})", marks(query.statuses.len())));
            values.extend(query.statuses.iter().cloned().map(Value::from));
        }
        if let Some(goal_id) = query.goal_id {
            filters.push("goal_id = ?".into());
            values.push(Value::from(goal_id.as_i64()));
        }
        let filter = filters.join(" AND ");
        // FTS5 judges the operators' placement (`foo OR`, `NOT foo`, `()`).
        let total: i64 = self
            .conn
            .query_row(
                &format!("SELECT count(*) FROM search_index WHERE {filter}"),
                params_from_iter(&values),
                |row| row.get(0),
            )
            .map_err(|error| match error.to_string() {
                message if message.contains("fts5") => {
                    anyhow::Error::from(DomainError::SearchQuery { reason: message })
                }
                _ => error.into(),
            })?;
        let ranked = terms.fts.is_some();
        let (score, order) = if ranked {
            ("bm25(search_index)", "score, rowid DESC")
        } else {
            ("NULL", "updated_at DESC, rowid DESC")
        };
        let snippets: String = COLUMNS
            .iter()
            .map(|(name, index)| {
                if ranked {
                    format!(
                        ", snippet(search_index, {index}, char(2), char(3), '…', {SNIPPET_TOKENS}) \
                         AS snippet_{name}"
                    )
                } else {
                    format!(", NULL AS snippet_{name}")
                }
            })
            .collect();
        values.push(Value::from(i64::try_from(query.limit)?));
        let sql = format!(
            "SELECT kind, ref, task_id, goal_id, run_id, status, title, description, acceptance, \
                    context, text, {score} AS score{snippets},
                    (SELECT t.worker_provider FROM tasks t
                     WHERE search_index.kind = 'task' AND t.id = search_index.ref) AS worker_provider,
                    (SELECT t.worker_mode FROM tasks t
                     WHERE search_index.kind = 'task' AND t.id = search_index.ref) AS worker_mode,
                    (SELECT t.change FROM tasks t
                     WHERE search_index.kind = 'task' AND t.id = search_index.ref) AS change,
                    (SELECT g.tags FROM goals g
                     WHERE search_index.kind = 'goal' AND g.id = search_index.ref) AS goal_tags
             FROM search_index WHERE {filter} ORDER BY {order} LIMIT ?"
        );
        let words: Vec<String> = terms.long.iter().chain(&terms.short).cloned().collect();
        let hits = self
            .conn
            .prepare(&sql)?
            .query_map(params_from_iter(&values), |row| {
                hit(row, &words, query.full)
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(SearchPage {
            hits,
            total: usize::try_from(total)?,
        })
    }

    /// Fill the message of each landed commit recorded without one, read by
    /// `read(git_common_dir, commit)` (the Git directory the landing
    /// recorded, if any). Returns how many were filled; a commit `read`
    /// cannot find keeps waiting.
    pub fn fill_commit_messages(
        &mut self,
        read: impl Fn(Option<&str>, &str) -> Option<String>,
    ) -> Result<usize> {
        let mut filled = 0;
        for missing in self.missing_commit_messages()? {
            if let Some(message) = read(missing.git_common_dir.as_deref(), &missing.commit) {
                filled += self.fill_commit_message(missing.id, &message)?;
            }
        }
        Ok(filled)
    }

    /// The landed commits recorded without their message (ADR-0063
    /// decision 3), oldest first: what `migrate`'s client reads with Git
    /// and sends back ([`Self::fill_commit_message`]).
    pub fn missing_commit_messages(&self) -> Result<Vec<MissingCommitMessage>> {
        Ok(self
            .conn
            .prepare(
                "SELECT id, commit_sha, git_common_dir FROM landed_commits
                 WHERE message IS NULL ORDER BY id",
            )?
            .query_map([], |row| {
                Ok(MissingCommitMessage {
                    id: row.get(0)?,
                    commit: row.get(1)?,
                    git_common_dir: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Fill the message of the landed commit `id` when it still has none:
    /// how many were filled (0 or 1).
    pub fn fill_commit_message(&mut self, id: i64, message: &str) -> Result<usize> {
        self.write_statement(
            "fill_commit_message",
            "UPDATE landed_commits SET message = ?2 WHERE id = ?1 AND message IS NULL",
            params![id, message],
        )
    }
}

/// A landed commit recorded without its message
/// ([`SqliteQueue::missing_commit_messages`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MissingCommitMessage {
    pub id: i64,
    pub commit: String,
    pub git_common_dir: Option<String>,
}

fn marks(n: usize) -> String {
    vec!["?"; n].join(", ")
}

fn hit(row: &Row<'_>, terms: &[String], full: bool) -> rusqlite::Result<SearchHit> {
    let kind: String = row.get("kind")?;
    let kind: SearchKind = kind.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(format!("{error}"))),
        )
    })?;
    let texts: Vec<(&str, String)> = COLUMNS
        .iter()
        .map(|(name, _)| Ok((*name, row.get::<_, String>(*name)?)))
        .collect::<rusqlite::Result<_>>()?;
    let snippets: Vec<Option<String>> = COLUMNS
        .iter()
        .map(|(name, _)| row.get(format!("snippet_{name}").as_str()))
        .collect::<rusqlite::Result<_>>()?;
    // The first column holding a term, around its first occurrence; else
    // the column FTS5 matched, as its snippet (a trigram snippet can cut a
    // match short, so it is only the fallback); else the first text.
    let found = texts.iter().find_map(|(name, text)| {
        terms
            .iter()
            .find_map(|term| excerpt(text, term, EXCERPT_CHARS))
            .map(|excerpt| (*name, excerpt))
    });
    let (column, excerpt) = found
        .or_else(|| {
            snippets
                .iter()
                .zip(&texts)
                .find_map(|(snippet, (name, _))| {
                    snippet
                        .as_deref()
                        .filter(|s| s.contains(OPEN))
                        .map(|s| (*name, s.replace(OPEN, "«").replace(CLOSE, "»")))
                })
        })
        .unwrap_or_else(|| {
            let (name, text) = texts
                .iter()
                .find(|(_, text)| !text.is_empty())
                .unwrap_or(&texts[0]);
            (*name, first_line(text, TITLE_CHARS))
        });
    let text_of = |name: &str| {
        texts
            .iter()
            .find(|(column, _)| *column == name)
            .map(|(_, text)| text.as_str())
            .unwrap_or("")
    };
    let title = match kind {
        SearchKind::Task | SearchKind::Goal => text_of("title").to_owned(),
        SearchKind::Note | SearchKind::Commit => first_line(text_of("text"), TITLE_CHARS),
    };
    let id = match kind {
        SearchKind::Commit => SearchRef::Commit(row.get("ref")?),
        _ => SearchRef::Id(row.get("ref")?),
    };
    let fields = full.then(|| {
        texts
            .iter()
            .filter(|(name, text)| match kind {
                SearchKind::Task | SearchKind::Goal => *name != "text",
                SearchKind::Note | SearchKind::Commit => *name == "text" || !text.is_empty(),
            })
            .map(|(name, text)| (field_name(kind, name).to_owned(), json!(text)))
            .collect::<Map<_, _>>()
    });
    // A task's worker, NULL columns read as the provider's default.
    let worker = match kind {
        SearchKind::Task => Some(
            Worker::resolve(
                optional_enum_col(row, "worker_provider")?,
                optional_enum_col(row, "worker_mode")?,
            )
            .unwrap_or(Worker::DEFAULT),
        ),
        _ => None,
    };
    Ok(SearchHit {
        kind,
        id,
        status: row.get("status")?,
        title,
        field: field_name(kind, column).to_owned(),
        excerpt,
        task_id: row
            .get::<_, Option<i64>>("task_id")?
            .filter(|_| kind != SearchKind::Task),
        run_id: row.get("run_id")?,
        goal_id: row
            .get::<_, Option<i64>>("goal_id")?
            .filter(|_| kind != SearchKind::Goal),
        provider: worker.map(|worker| worker.provider),
        worker_mode: worker.map(|worker| worker.mode),
        // Any label reads as written; one that is not reads as none.
        change: row
            .get::<_, Option<String>>("change")?
            .and_then(|change| change.parse().ok()),
        // A goal's tags as stored; none for the other kinds.
        tags: row
            .get::<_, Option<String>>("goal_tags")?
            .map(|tags| serde_json::from_str(&tags).unwrap_or_default()),
        score: if full { row.get("score")? } else { None },
        fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::TaskStore;
    use crate::domain::search::any_word_query;
    use crate::domain::{NewTask, Priority, TaskId};

    // Keep the complete migration's FTS declaration, including UNINDEXED
    // metadata: kind must not affect either document length or term frequency.
    fn search_memdb() -> rusqlite::Connection {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        let migration = include_str!("../../migrations/0026_search.sql");
        let start = migration.find("CREATE VIRTUAL TABLE search_index").unwrap();
        let end = start + migration[start..].find(';').unwrap() + 1;
        db.execute_batch(&migration[start..end]).unwrap();
        db
    }

    #[test]
    fn pure_bm25_matches_fts5_values_and_order_with_column_filters() {
        use crate::domain::search::{Bm25Phrase, bm25};
        let db = search_memdb();
        let rows = [
            [
                "全文検索検索",
                "aaaaa search",
                "done",
                "src/domain/search.rs",
                "",
            ],
            ["SEARCH", "全文検索 search", "日本語の条件", "", "aaaa"],
            [
                "src/domain/search.rs",
                "aaaa search",
                "全文検索",
                "context search",
                "message search",
            ],
            ["その他の資料", "different", "", "", "全文検索 search"],
            ["稀な検索語", "unique", "", "", ""],
            ["SEARCH", "全文検索 search", "日本語の条件", "", "aaaa"],
            ["", "", "", "", ""],
            ["無関係", "unrelated", "criteria", "context", "message"],
        ];
        for (id, row) in rows.iter().enumerate() {
            db.execute(
                "INSERT INTO search_index(rowid,kind,title,description,acceptance,context,text)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    id as i64,
                    ["task", "goal", "note", "commit"][id % 4],
                    row[0],
                    row[1],
                    row[2],
                    row[3],
                    row[4]
                ],
            )
            .unwrap();
        }
        let sizes: Vec<u64> = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|text| text.chars().count().saturating_sub(2) as u64)
                    .sum()
            })
            .collect();
        let total = sizes.iter().sum();
        // ASCII folding is sufficient for this fixture; no tokenizer or DB
        // statistics feed the expected counts. Windows include overlaps.
        let count = |text: &str, term: &str| -> u64 {
            let text: Vec<char> = text.to_ascii_lowercase().chars().collect();
            let term: Vec<char> = term.to_ascii_lowercase().chars().collect();
            text.windows(term.len())
                .filter(|part| *part == term)
                .count() as u64
        };
        for columns in [5, 2] {
            for terms in [
                vec!["search"],
                vec!["全文検索"],
                vec!["aaa"],
                vec!["src/domain/search.rs"],
                vec!["unique"],
                vec!["search", "全文検索", "aaa"],
                vec!["aaa", "search", "search"],
            ] {
                let expression = terms
                    .iter()
                    .map(|t| format!("\"{t}\""))
                    .collect::<Vec<_>>()
                    .join(" OR ");
                let query = if columns == 2 {
                    format!("{{title description}} : ({expression})")
                } else {
                    expression
                };
                let frequencies: Vec<Vec<u64>> = rows
                    .iter()
                    .map(|row| {
                        terms
                            .iter()
                            .map(|term| row[..columns].iter().map(|text| count(text, term)).sum())
                            .collect()
                    })
                    .collect();
                let matching: Vec<u64> = (0..terms.len())
                    .map(|i| frequencies.iter().filter(|row| row[i] > 0).count() as u64)
                    .collect();
                let mut expected: Vec<(i64, f64)> = frequencies
                    .iter()
                    .enumerate()
                    .filter(|(_, fs)| fs.iter().any(|f| *f > 0))
                    .map(|(id, fs)| {
                        let phrases: Vec<_> = fs
                            .iter()
                            .zip(&matching)
                            .map(|(&occurrences, &matching_rows)| Bm25Phrase {
                                occurrences,
                                matching_rows,
                            })
                            .collect();
                        (
                            id as i64,
                            bm25(rows.len() as u64, total, sizes[id], &phrases),
                        )
                    })
                    .collect();
                expected.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(&a.0)));
                let mut stmt = db
                    .prepare(
                        "SELECT rowid, bm25(search_index) FROM search_index
                    WHERE search_index MATCH ?1 ORDER BY bm25(search_index), rowid DESC",
                    )
                    .unwrap();
                let actual: Vec<(i64, f64)> = stmt
                    .query_map([&query], |row| Ok((row.get(0)?, row.get(1)?)))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                assert_eq!(actual.len(), expected.len(), "{query}");
                for (actual, expected) in actual.iter().zip(&expected) {
                    assert_eq!(actual.0, expected.0, "rank: {query}");
                    assert!(
                        (actual.1 - expected.1).abs() < 1e-9,
                        "{query}: {actual:?} != {expected:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn expression_rejects_the_same_operator_placements_as_fts5() {
        let db = search_memdb();
        for query in [
            "foo",
            "foo bar",
            "foo AND bar",
            "foo OR bar NOT baz",
            "foo NOT bar baz",
            "(foo OR bar) AND baz",
            "foo AND (bar OR baz)",
            "((foo))",
            "foo NOT bar NOT baz",
            "\"AND\" OR foo",
            "foo OR",
            "NOT foo",
            "foo AND",
            "AND foo",
            "foo NOT",
            "foo OR OR bar",
            "()",
            "foo ()",
            "(foo",
            "foo)",
            "foo (bar)",
            "(foo) bar",
            "(foo)(bar)",
            "foo AND NOT bar",
            "foo NOT (bar OR baz)",
            "foo OR (bar baz)",
            "foo OR (bar) baz",
            "foo NOT bar AND baz",
            "\"foo bar\"",
            "foo AND AND bar",
        ] {
            let terms = parse_terms(query);
            // A termless expression such as () is rejected by parse_terms
            // before the tree interpreter; FTS5 must reject it too.
            let fts = terms
                .as_ref()
                .ok()
                .and_then(|terms| terms.fts.as_deref())
                .unwrap_or(query);
            let actual = db.query_row(
                "SELECT count(*) FROM search_index WHERE search_index MATCH ?1",
                [fts],
                |row| row.get::<_, i64>(0),
            );
            let interpreted = terms.and_then(|terms| terms.expression());
            assert_eq!(
                interpreted.is_ok(),
                actual.is_ok(),
                "{query}: {actual:?}, {interpreted:?}"
            );
            if let Err(error) = interpreted {
                assert!(matches!(error, DomainError::SearchQuery { .. }));
            }
        }
    }

    #[test]
    fn expression_limits_match_fts5_stack_and_tree_boundaries() {
        let db = search_memdb();
        let compare = |query: &str| {
            let terms = parse_terms(query).unwrap();
            let sqlite = db.query_row(
                "SELECT count(*) FROM search_index WHERE search_index MATCH ?1",
                [terms.fts.as_ref().unwrap()],
                |row| row.get::<_, i64>(0),
            );
            let interpreted = terms.expression();
            assert_eq!(
                interpreted.is_ok(),
                sqlite.is_ok(),
                "length {}, prefix {:?}: SQLite {sqlite:?}",
                query.len(),
                &query[..query.len().min(50)]
            );
            if let Err(error) = interpreted {
                assert!(matches!(error, DomainError::SearchQuery { .. }));
            }
        };
        // Exercise both sides of the fixed parser-stack boundary. Pending
        // operators and implicit AND consume different numbers of entries.
        for depth in 0..=105 {
            for phrase in [
                "foo",
                "foo bar",
                "foo AND bar",
                "foo OR bar AND baz NOT qux",
            ] {
                compare(&format!(
                    "{}{}{}",
                    "(".repeat(depth),
                    phrase,
                    ")".repeat(depth)
                ));
                compare(&format!(
                    "{}{}{}",
                    "foo OR (".repeat(depth),
                    phrase,
                    ")".repeat(depth)
                ));
            }
        }
        // NOT is left associative and cannot flatten like AND and OR. Its
        // expression height, rather than the parser stack, is the limit.
        for count in 240..=265 {
            compare(&format!("foo{}", " NOT foo".repeat(count)));
        }
        for operator in [" ", " AND ", " OR "] {
            compare(&vec!["foo"; 2048].join(operator));
        }
        compare(&format!("{}foo{}", "(".repeat(50_000), ")".repeat(50_000)));
        compare(&format!("foo{}", " NOT foo".repeat(50_000)));
    }

    fn add(queue: &mut SqliteQueue, title: &str, description: &str) -> TaskId {
        queue
            .add(NewTask {
                change: None,
                title: title.into(),
                description: description.into(),
                acceptance: "done".into(),
                verification_commands: Vec::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                priority: Some(Priority::Normal),
                dependencies: Vec::new(),
                goal_dependencies: Vec::new(),
                goal_id: None,
                context: String::new(),
                provider: None,
                worker_mode: None,
                wait_for_build: false,
                execution_class: Default::default(),
            })
            .unwrap()
            .id()
    }

    #[test]
    fn a_japanese_title_finds_tasks_sharing_its_words() {
        let dir = tempfile::tempdir().unwrap();
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let by_title = add(&mut queue, "計画の重複を検出する", "plan");
        let by_description = add(&mut queue, "検索", "候補を並べて見直す");
        let other = add(&mut queue, "着地の順番", "衝突を避ける");
        // The whole title appears nowhere else, so the old one-phrase
        // query found nothing.
        let terms = any_word_query("重複した計画の候補を一覧にする").unwrap();
        let page = queue
            .search(&SearchQuery {
                terms,
                kinds: vec![SearchKind::Task],
                limit: 10,
                ..SearchQuery::default()
            })
            .unwrap();
        let ids: Vec<SearchRef> = page.hits.into_iter().map(|hit| hit.id).collect();
        assert!(ids.contains(&SearchRef::Id(by_title.as_i64())), "{ids:?}");
        assert!(
            ids.contains(&SearchRef::Id(by_description.as_i64())),
            "{ids:?}"
        );
        assert!(!ids.contains(&SearchRef::Id(other.as_i64())), "{ids:?}");
    }
}
