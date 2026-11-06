//! Full-text search over tasks, goals, notes and the messages of landed
//! commits (ADR-0046 decision 1). The index is an FTS5 table with the
//! trigram tokenizer (migration 0026): it matches any substring of three or
//! more characters, so Japanese text, which does not separate words with
//! spaces, is found by any part of it. Shorter terms cannot be looked up in
//! a trigram index and are matched with `LIKE` instead.
use serde::{Deserialize, Serialize};

use super::{DomainError, GoalId, TaskStatus};

string_enum!(SearchKind {
    Task => "task",
    Goal => "goal",
    Note => "note",
    Commit => "commit",
});

/// The statuses of a goal as the index records them: its `status` while
/// it is not closed, its verdict once it is.
pub const GOAL_STATUSES: [&str; 4] = ["draft", "open", "achieved", "abandoned"];

/// The characters a term needs to be looked up in the trigram index.
pub const TRIGRAM: usize = 3;

/// Checks a `--status` value: a task's status or a goal's.
pub fn parse_status(value: &str) -> Result<String, DomainError> {
    let value = value.trim();
    if value.parse::<TaskStatus>().is_ok() || GOAL_STATUSES.contains(&value) {
        Ok(value.to_owned())
    } else {
        Err(DomainError::UnknownValue {
            kind: "status",
            value: value.to_owned(),
        })
    }
}

/// What `search` looks for: `terms` as [`parse_terms`] reads them, kept to
/// the given kinds and statuses (empty: all) and to one goal (its own
/// document, its tasks and their notes and commits).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SearchQuery {
    pub terms: String,
    pub kinds: Vec<SearchKind>,
    pub statuses: Vec<String>,
    pub goal_id: Option<GoalId>,
    pub limit: usize,
    /// Every indexed field in full and the bm25 score, besides the excerpt.
    pub full: bool,
}

/// A query split into what the index runs: an FTS5 expression of the terms
/// of at least [`TRIGRAM`] characters (`long`), and the shorter terms, each
/// of which a document must also contain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchTerms {
    pub fts: Option<String>,
    pub long: Vec<String>,
    pub short: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Token {
    Term(String),
    Operator(&'static str),
}

/// Reads a query: words separated by white space, `"..."` for a phrase
/// with spaces, and FTS5's `AND`, `OR`, `NOT` and parentheses. Every term is
/// quoted for FTS5, so a path such as `src/domain/task.rs` is one term that
/// matches as a substring. Terms under [`TRIGRAM`] characters must all be
/// present, so they cannot be combined with `OR`, `NOT` or parentheses.
pub fn parse_terms(query: &str) -> Result<SearchTerms, DomainError> {
    let tokens = tokens(query);
    let is_short = |term: &str| term.chars().count() < TRIGRAM;
    let (short, long): (Vec<String>, Vec<String>) = tokens
        .iter()
        .filter_map(|token| match token {
            Token::Term(term) => Some(term.clone()),
            Token::Operator(_) => None,
        })
        .partition(|term| is_short(term));
    if short.is_empty() && long.is_empty() {
        return Err(DomainError::SearchQuery {
            reason: "the query has no term".into(),
        });
    }
    let combines = tokens
        .iter()
        .any(|t| matches!(t, Token::Operator(op) if *op != "AND"));
    if !short.is_empty() && combines {
        return Err(DomainError::SearchQuery {
            reason: format!(
                "terms shorter than {TRIGRAM} characters ({}) cannot be combined with OR, NOT or \
                 parentheses",
                short.join(", ")
            ),
        });
    }
    // A term never contains `"`: it ends a word and delimits a phrase.
    let quote = |term: &str| format!("\"{term}\"");
    let parts: Vec<String> = if short.is_empty() {
        tokens
            .iter()
            .map(|token| match token {
                Token::Term(term) => quote(term),
                Token::Operator(op) => (*op).to_owned(),
            })
            .collect()
    } else {
        // Only AND can remain, which juxtaposition already means.
        tokens
            .iter()
            .filter_map(|token| match token {
                Token::Term(term) if !is_short(term) => Some(quote(term)),
                _ => None,
            })
            .collect()
    };
    Ok(SearchTerms {
        fts: (!parts.is_empty()).then(|| parts.join(" ")),
        long,
        short,
    })
}

/// An FTS5 boolean expression. Implicit AND binds more tightly than NOT,
/// explicit AND, then OR. Parentheses determine grouping, not a separate node.
/// Leaves retain phrase order (including duplicates) for bm25 accumulation.
/// AND and OR children are flattened as in FTS5, so arbitrarily long lists
/// do not produce recursive Drop/Clone/Eq chains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchExpr {
    Term(String),
    And(Vec<Self>),
    Or(Vec<Self>),
    Not(Box<Self>, Box<Self>),
}

impl SearchTerms {
    /// Interprets only the FTS5 expression produced by [`parse_terms`]. Short
    /// terms remain separate mandatory filters; with no long terms this is None.
    /// Rejects invalid operator placement as [`DomainError::SearchQuery`]. SQLite
    /// deliberately keeps using FTS5 itself instead of this interpreter.
    /// Enforces the bundled FTS5 parser's fixed stack and flattened expression
    /// height limits before creating an unbounded recursive call or tree.
    pub fn expression(&self) -> Result<Option<SearchExpr>, DomainError> {
        let Some(fts) = &self.fts else {
            return Ok(None);
        };
        let tokens = tokens(fts);
        let mut parser = ExpressionParser {
            tokens: &tokens,
            at: 0,
        };
        let expr = parser.explicit(1, 0)?.expr;
        if parser.at != tokens.len() {
            return Err(expression_error());
        }
        Ok(Some(expr))
    }
}

fn expression_error() -> DomainError {
    DomainError::SearchQuery {
        reason: "invalid FTS5 operator placement".into(),
    }
}

struct ExpressionParser<'a> {
    tokens: &'a [Token],
    at: usize,
}

// The bundled FTS5 Lemon parser has 100 fixed stack entries (entry zero
// is the initial state). For parse_terms' grammar, an unresolved parenthesis
// occupies one entry and a pending binary operator occupies two. A STRING
// needs two temporary entries, including the empty star_opt reduction; an
// implicit-AND continuation or closing parenthesis needs three. Track these
// entries before recursion, rather than approximating a parenthesis limit.
const FTS5_PARSER_STACK: usize = 100;
// FTS5 leaves have height zero; AND/OR flatten children of the same kind.
const FTS5_EXPR_HEIGHT: usize = 256;

struct ParsedExpr {
    expr: SearchExpr,
    height: usize,
}

impl ParsedExpr {
    fn term(term: &str) -> Self {
        Self {
            expr: SearchExpr::Term(term.into()),
            height: 0,
        }
    }

    fn combine(op: &str, left: Self, right: Self) -> Result<Self, DomainError> {
        let flattened = |expr: &SearchExpr| {
            matches!(
                (op, expr),
                ("AND", SearchExpr::And(_)) | ("OR", SearchExpr::Or(_))
            )
        };
        let height = (left.height + usize::from(!flattened(&left.expr)))
            .max(right.height + usize::from(!flattened(&right.expr)));
        if height > FTS5_EXPR_HEIGHT {
            return Err(expression_error());
        }
        let expr = if op == "NOT" {
            SearchExpr::Not(Box::new(left.expr), Box::new(right.expr))
        } else {
            let mut children = match left.expr {
                SearchExpr::And(children) if op == "AND" => children,
                SearchExpr::Or(children) if op == "OR" => children,
                expr => vec![expr],
            };
            match right.expr {
                SearchExpr::And(other) if op == "AND" => children.extend(other),
                SearchExpr::Or(other) if op == "OR" => children.extend(other),
                expr => children.push(expr),
            }
            if op == "AND" {
                SearchExpr::And(children)
            } else {
                SearchExpr::Or(children)
            }
        };
        Ok(Self { expr, height })
    }
}

impl ExpressionParser<'_> {
    fn stack_room(base: usize, temporary: usize) -> Result<(), DomainError> {
        if base + temporary >= FTS5_PARSER_STACK {
            Err(expression_error())
        } else {
            Ok(())
        }
    }

    fn explicit(&mut self, minimum: u8, base: usize) -> Result<ParsedExpr, DomainError> {
        let mut left = self.primary(base)?;
        loop {
            let (op, precedence) = match self.tokens.get(self.at) {
                Some(Token::Operator("OR")) => ("OR", 1),
                Some(Token::Operator("AND")) => ("AND", 2),
                Some(Token::Operator("NOT")) => ("NOT", 3),
                _ => break,
            };
            if precedence < minimum {
                break;
            }
            self.at += 1;
            let right = self.explicit(precedence + 1, base + 2)?;
            left = ParsedExpr::combine(op, left, right)?;
        }
        Ok(left)
    }

    fn primary(&mut self, base: usize) -> Result<ParsedExpr, DomainError> {
        Self::stack_room(base, 2)?;
        match self.tokens.get(self.at) {
            Some(Token::Term(term)) => {
                let mut expr = ParsedExpr::term(term);
                self.at += 1;
                // FTS5 allows implicit AND only between phrases, never beside
                // a parenthesized expression: `foo (bar)` is a syntax error.
                while let Some(Token::Term(term)) = self.tokens.get(self.at) {
                    Self::stack_room(base, 3)?;
                    expr = ParsedExpr::combine("AND", expr, ParsedExpr::term(term))?;
                    self.at += 1;
                }
                Ok(expr)
            }
            Some(Token::Operator("(")) => {
                self.at += 1;
                let expr = self.explicit(1, base + 1)?;
                if self.tokens.get(self.at) != Some(&Token::Operator(")")) {
                    return Err(expression_error());
                }
                Self::stack_room(base, 3)?;
                self.at += 1;
                Ok(expr)
            }
            _ => Err(expression_error()),
        }
    }
}

/// One query phrase's statistics. `matching_rows` counts rows in the entire
/// index containing the phrase in the query's selected columns, before other
/// filters. `occurrences` counts overlapping phrase instances in those columns
/// of the scored row, with column weights all one. For boolean queries the
/// caller supplies FTS5's active instances (e.g. an excluded NOT branch has zero).
#[derive(Debug, Clone, Copy)]
pub struct Bm25Phrase {
    pub matching_rows: u64,
    pub occurrences: u64,
}

/// FTS5's bm25 from `fts5_aux.c`, without reading an index. Smaller is better.
/// `rows` is N over the whole table, including other kinds and filtered-out rows.
/// `total_trigrams` and `row_trigrams` sum `max(chars - 2, 0)` separately over
/// all five indexed columns, even when matching only title and description.
/// Phrases must be in FTS5's query order, including repeated phrases; counts
/// use the tokenizer's case folding. The caller owns folding and counting.
/// Keep the floating-point operation order: sum positive contributions in phrase
/// order, then negate once. Empty/tokenless indexes have no ranked matches and
/// return zero. Counts must describe a consistent index (`matching_rows <= rows`).
pub fn bm25(rows: u64, total_trigrams: u64, row_trigrams: u64, phrases: &[Bm25Phrase]) -> f64 {
    if rows == 0 || total_trigrams == 0 {
        return 0.0;
    }
    let n = rows as f64;
    let avgdl = total_trigrams as f64 / n;
    let mut score = 0.0;
    for phrase in phrases {
        let hits = phrase.matching_rows as f64;
        let mut idf = (((rows - phrase.matching_rows) as f64 + 0.5) / (hits + 0.5)).ln();
        if idf <= 0.0 {
            idf = 1e-6;
        }
        let frequency = phrase.occurrences as f64;
        score += idf
            * ((frequency * (1.2 + 1.0))
                / (frequency + 1.2 * (1.0 - 0.75 + 0.75 * row_trigrams as f64 / avgdl)));
    }
    -score
}

fn tokens(query: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut chars = query.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '(' || c == ')' {
            chars.next();
            tokens.push(Token::Operator(if c == '(' { "(" } else { ")" }));
        } else if c == '"' {
            chars.next();
            let phrase: String = chars.by_ref().take_while(|&c| c != '"').collect();
            if !phrase.trim().is_empty() {
                tokens.push(Token::Term(phrase));
            }
        } else {
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() || c == '(' || c == ')' || c == '"' {
                    break;
                }
                word.push(c);
                chars.next();
            }
            tokens.push(match word.as_str() {
                "AND" => Token::Operator("AND"),
                "OR" => Token::Operator("OR"),
                "NOT" => Token::Operator("NOT"),
                _ => Token::Term(word),
            });
        }
    }
    tokens
}

/// The most words of a text [`any_word_query`] asks the index for.
const MAX_QUERY_WORDS: usize = 20;

/// A query for documents holding any word of `text` (a task's title, for
/// the plan review's candidates): its runs of letters, digits and `_ - . /`
/// of at least [`TRIGRAM`] characters, and the trigrams of its runs of
/// Japanese (kanji, kana), which are not separated by spaces, except those
/// of hiragana alone (particles and endings such as `にする`), each once
/// (ASCII case ignored), each quoted and joined with `OR`, at most
/// [`MAX_QUERY_WORDS`] in order; `None` when it has none. It does not
/// change how `dagq search` reads the query a person types.
pub fn any_word_query(text: &str) -> Option<String> {
    let mut words: Vec<String> = Vec::new();
    let mut take = |word: &str| {
        if word.chars().count() < TRIGRAM
            || words.len() == MAX_QUERY_WORDS
            || words.iter().any(|seen| seen.eq_ignore_ascii_case(word))
        {
            return;
        }
        words.push(word.to_owned());
    };
    for word in text.split(|c: char| !(c.is_alphanumeric() || "_-./".contains(c))) {
        let chars: Vec<char> = word.chars().collect();
        for run in chars.chunk_by(|a, b| is_unspaced(*a) == is_unspaced(*b)) {
            if is_unspaced(run[0]) {
                for gram in run.windows(TRIGRAM) {
                    if !gram.iter().all(|&c| is_hiragana(c)) {
                        take(&gram.iter().collect::<String>());
                    }
                }
            } else {
                let run: String = run.iter().collect();
                take(run.trim_matches(|c: char| ".-/".contains(c)));
            }
        }
    }
    (!words.is_empty()).then(|| {
        words
            .iter()
            .map(|word| format!("\"{word}\""))
            .collect::<Vec<_>>()
            .join(" OR ")
    })
}

/// Whether `c` is written without spaces between words: kana, kanji and
/// the iteration mark `々`.
fn is_unspaced(c: char) -> bool {
    matches!(c,
        '\u{3005}'
        | '\u{3040}'..='\u{30FF}'
        | '\u{31F0}'..='\u{31FF}'
        | '\u{3400}'..='\u{4DBF}'
        | '\u{4E00}'..='\u{9FFF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FF66}'..='\u{FF9F}')
}

fn is_hiragana(c: char) -> bool {
    matches!(c, '\u{3040}'..='\u{309F}')
}

/// What a hit is: a task or goal ID, a note's event ID or a commit's SHA.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SearchRef {
    Id(i64),
    Commit(String),
}

/// One document `search` found. `field` names where the excerpt comes
/// from, with the match between `«` and `»`. A note or a commit carries the
/// task (and run) it belongs to; `fields` and `score` come with `--full`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub kind: SearchKind,
    pub id: SearchRef,
    pub status: Option<String>,
    pub title: String,
    pub field: String,
    pub excerpt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<i64>,
    /// A task's worker provider and mode (ADR-t813-2); none for the other
    /// kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<super::Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_mode: Option<super::worker::WorkerMode>,
    /// A task's change (ADR-t980-1); none for a task without one and for
    /// the other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<super::TaskChange>,
    /// A goal's tags (ADR-t1639-1 decision 6), empty for a goal without
    /// one; none for the other kinds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<super::GoalTag>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<serde_json::Map<String, serde_json::Value>>,
}

/// The hits, best first (bm25, or the most recently updated when the
/// query has only short terms), and how many documents match in all.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub total: usize,
}

/// The name a document's indexed column has in its kind: a goal keeps its
/// constraints in `context`, a note its text and a commit its message in
/// `text`.
pub fn field_name(kind: SearchKind, column: &str) -> &str {
    match (kind, column) {
        (SearchKind::Goal, "context") => "constraints",
        (SearchKind::Commit, "text") => "message",
        _ => column,
    }
}

/// The part of `text` around the first occurrence of `term` (ASCII case
/// ignored, as `LIKE` does), with the occurrence between `«` and `»` and
/// `…` where text is cut; `None` when `text` does not contain it.
pub fn excerpt(text: &str, term: &str, context: usize) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let needle: Vec<char> = term.chars().map(|c| c.to_ascii_lowercase()).collect();
    if needle.is_empty() || needle.len() > chars.len() {
        return None;
    }
    let start = (0..=chars.len() - needle.len()).find(|&i| {
        chars[i..i + needle.len()]
            .iter()
            .zip(&needle)
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
    })?;
    let end = start + needle.len();
    let from = start.saturating_sub(context);
    let to = (end + context).min(chars.len());
    let slice = |a: usize, b: usize| chars[a..b].iter().collect::<String>();
    Some(format!(
        "{}{}«{}»{}{}",
        if from > 0 { "…" } else { "" },
        slice(from, start),
        slice(start, end),
        slice(end, to),
        if to < chars.len() { "…" } else { "" },
    ))
}

/// The first line of `text`, cut to `max` characters with `…`.
pub fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    if line.chars().count() > max {
        format!("{}…", line.chars().take(max).collect::<String>())
    } else {
        line.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(query: &str) -> SearchTerms {
        parse_terms(query).unwrap()
    }

    #[test]
    fn expression_keeps_fts5_precedence_and_associativity() {
        use SearchExpr::{And, Not, Or, Term};
        let term = |s: &str| Term(s.into());
        assert_eq!(
            terms("foo OR bar AND baz NOT qux quux")
                .expression()
                .unwrap(),
            Some(Or(vec![
                term("foo"),
                And(vec![
                    term("bar"),
                    Not(
                        Box::new(term("baz")),
                        Box::new(And(vec![term("qux"), term("quux")]))
                    )
                ])
            ]))
        );
        assert_eq!(
            terms("foo NOT bar NOT baz").expression().unwrap(),
            Some(Not(
                Box::new(Not(Box::new(term("foo")), Box::new(term("bar")))),
                Box::new(term("baz"))
            ))
        );
        assert_eq!(
            terms("(foo OR bar) AND baz").expression().unwrap(),
            Some(And(vec![Or(vec![term("foo"), term("bar")]), term("baz")]))
        );
        assert_eq!(terms("重複").expression().unwrap(), None);
        // parse_terms already discards explicit ANDs beside short terms;
        // interpret its output, preserving that local normalization.
        assert_eq!(
            terms("AND 重複 AND foo AND").expression().unwrap(),
            Some(Term("foo".into()))
        );
    }

    #[test]
    fn long_flat_expressions_clone_compare_and_drop_without_deep_recursion() {
        for separator in [" ", " AND ", " OR "] {
            let query = vec!["foo"; 50_000].join(separator);
            let expr = terms(&query).expression().unwrap().unwrap();
            let children = match &expr {
                SearchExpr::And(children) | SearchExpr::Or(children) => children,
                _ => panic!("expected a flat list"),
            };
            assert_eq!(children.len(), 50_000);
            assert!(
                children
                    .iter()
                    .all(|child| matches!(child, SearchExpr::Term(_)))
            );
            let cloned = expr.clone();
            assert_eq!(expr, cloned);
            drop(cloned);
            drop(expr);
        }
    }

    #[test]
    fn bm25_without_ranked_instances_is_zero() {
        assert_eq!(bm25(0, 0, 0, &[]), 0.0);
        assert_eq!(bm25(2, 0, 0, &[]), 0.0);
        assert_eq!(bm25(2, 10, 5, &[]), 0.0);
        assert_eq!(
            bm25(
                2,
                10,
                5,
                &[Bm25Phrase {
                    matching_rows: 1,
                    occurrences: 0
                }]
            ),
            0.0
        );
    }

    #[test]
    fn long_terms_are_quoted_for_fts5_with_their_operators() {
        assert_eq!(
            terms("src/domain/task.rs 全文検索"),
            SearchTerms {
                fts: Some("\"src/domain/task.rs\" \"全文検索\"".into()),
                long: vec!["src/domain/task.rs".into(), "全文検索".into()],
                short: vec![],
            }
        );
        assert_eq!(
            terms("(search OR \"related task\") NOT embed AND fts5").fts,
            Some("( \"search\" OR \"related task\" ) NOT \"embed\" AND \"fts5\"".into())
        );
        assert_eq!(terms("a\"b c\"").short, vec!["a".to_owned()]);
    }

    #[test]
    fn short_terms_are_matched_apart_and_only_with_and() {
        assert_eq!(
            terms("重複 AND search"),
            SearchTerms {
                fts: Some("\"search\"".into()),
                long: vec!["search".into()],
                short: vec!["重複".into()],
            }
        );
        assert_eq!(terms("重複 id").fts, None);
        assert!(matches!(
            parse_terms("重複 OR search"),
            Err(DomainError::SearchQuery { .. })
        ));
        assert!(matches!(
            parse_terms("  AND \"\" "),
            Err(DomainError::SearchQuery { .. })
        ));
    }

    #[test]
    fn any_word_query_takes_each_long_word_once() {
        let query =
            any_word_query("runtime: plan review の prompt に、Plan AND (src/domain/task.rs).")
                .unwrap();
        assert_eq!(
            query,
            "\"runtime\" OR \"plan\" OR \"review\" OR \"prompt\" OR \"AND\" OR \"src/domain/task.rs\""
        );
        assert_eq!(
            parse_terms(&query).unwrap().fts.as_deref(),
            Some(query.as_str())
        );
        assert_eq!(any_word_query("の に 、 ab"), None);
        let many: String = (100..200).map(|n| format!("w{n} ")).collect();
        assert_eq!(
            any_word_query(&many).unwrap().matches(" OR ").count(),
            MAX_QUERY_WORDS - 1
        );
    }

    #[test]
    fn any_word_query_splits_japanese_into_trigrams() {
        let query =
            any_word_query("plan review の search 候補を日本語の title でも引けるようにする")
                .unwrap();
        assert_eq!(
            query,
            "\"plan\" OR \"review\" OR \"search\" OR \"候補を\" OR \"補を日\" OR \"を日本\" OR \
             \"日本語\" OR \"本語の\" OR \"title\" OR \"でも引\" OR \"も引け\" OR \"引ける\""
        );
        assert_eq!(
            parse_terms(&query).unwrap().fts.as_deref(),
            Some(query.as_str())
        );
        // A word of ASCII and Japanese is split where the script changes.
        assert_eq!(
            any_word_query("task591の計画").as_deref(),
            Some("\"task591\" OR \"の計画\"")
        );
        // Runs under three characters and hiragana alone give nothing.
        assert_eq!(any_word_query("計画 にする ab"), None);
        let long: String = "計画を検査して準備完了にする仕組みを作り直す".repeat(3);
        assert_eq!(
            any_word_query(&long).unwrap().matches(" OR ").count(),
            MAX_QUERY_WORDS - 1
        );
    }

    #[test]
    fn statuses_are_those_of_tasks_and_goals() {
        assert_eq!(parse_status(" completed").unwrap(), "completed");
        assert_eq!(parse_status("achieved").unwrap(), "achieved");
        assert!(parse_status("closed").is_err());
    }

    #[test]
    fn excerpts_mark_the_first_occurrence() {
        assert_eq!(
            excerpt("計画の重複を見つける", "重複", 2).as_deref(),
            Some("…画の«重複»を見…")
        );
        assert_eq!(excerpt("Rust ID", "id", 10).as_deref(), Some("Rust «ID»"));
        assert_eq!(excerpt("abc", "abcd", 3), None);
        assert_eq!(excerpt("abc", "", 3), None);
        assert_eq!(excerpt("abc", "x", 3), None);
    }

    #[test]
    fn titles_and_field_names() {
        assert_eq!(
            first_line("\n  runtime: search\nbody", 80),
            "runtime: search"
        );
        assert_eq!(first_line("abcdef", 3), "abc…");
        assert_eq!(field_name(SearchKind::Goal, "context"), "constraints");
        assert_eq!(field_name(SearchKind::Commit, "text"), "message");
        assert_eq!(field_name(SearchKind::Note, "text"), "text");
    }
}
