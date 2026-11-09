//! SQL read as SQLite reads it, as far as the boundary tests need: the
//! tables a statement writes, the functions it calls, and the tables the
//! migrations leave.

use std::collections::BTreeSet;

#[derive(Debug, PartialEq)]
enum Kind {
    /// A word (a keyword or a bare name).
    Word,
    /// A quoted name: `"t"`, `` `t` ``, `[t]`.
    Name,
    /// A value in single quotes.
    Value,
    Number,
    /// Any other character.
    Punct,
}

#[derive(Debug)]
struct Token {
    kind: Kind,
    text: String,
}

fn word_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

fn word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// The tokens of `sql` in lower case, as [`spelled_tokens`] reads them.
fn tokens(sql: &str) -> Vec<Token> {
    spelled_tokens(&sql.to_lowercase())
}

/// The tokens of `sql` as spelled, read from the left: a quoted name or a
/// value is one token whose inside is never read as SQL (doubled quotes
/// inside are one), and comments (`/* ... */`, to the end when not closed,
/// and `--` to the end of the line) are dropped, so a comment marker inside
/// a name or a value is no comment.
fn spelled_tokens(sql: &str) -> Vec<Token> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    let take_until = |from: usize, close: char, doubled: bool| -> (String, usize) {
        let mut text = String::new();
        let mut i = from;
        while i < chars.len() {
            if chars[i] == close {
                if doubled && chars.get(i + 1) == Some(&close) {
                    text.push(close);
                    i += 2;
                    continue;
                }
                return (text, i + 1);
            }
            text.push(chars[i]);
            i += 1;
        }
        (text, chars.len())
    };
    while at < chars.len() {
        let c = chars[at];
        let next = chars.get(at + 1).copied();
        if c.is_whitespace() {
            at += 1;
        } else if c == '-' && next == Some('-') {
            match chars[at..].iter().position(|&c| c == '\n') {
                Some(end) => at += end + 1,
                None => break,
            }
        } else if c == '/' && next == Some('*') {
            match chars[at + 2..].windows(2).position(|w| w == ['*', '/']) {
                Some(end) => at += end + 4,
                None => break,
            }
        } else if c == '\'' || c == '"' || c == '`' || c == '[' {
            let close = if c == '[' { ']' } else { c };
            let (text, end) = take_until(at + 1, close, c == '\'' || c == '"');
            let kind = if c == '\'' { Kind::Value } else { Kind::Name };
            out.push(Token { kind, text });
            at = end;
        } else if word_start(c) {
            let len = chars[at..].iter().take_while(|&&c| word_char(c)).count();
            let text = chars[at..at + len].iter().collect();
            out.push(Token {
                kind: Kind::Word,
                text,
            });
            at += len;
        } else if c.is_ascii_digit() {
            let len = chars[at..]
                .iter()
                .take_while(|&&c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
                .count();
            let text = chars[at..at + len].iter().collect();
            out.push(Token {
                kind: Kind::Number,
                text,
            });
            at += len;
        } else {
            out.push(Token {
                kind: Kind::Punct,
                text: c.to_string(),
            });
            at += 1;
        }
    }
    out
}

fn is_word(tokens: &[Token], at: usize, word: &str) -> bool {
    tokens
        .get(at)
        .is_some_and(|t| t.kind == Kind::Word && t.text == word)
}

/// Whether a statement of `sql` is a `BEGIN` or a `SAVEPOINT` (which
/// begins a transaction when none is open): its first word, after
/// comments, at the start or after a `;` outside a name or a value.
pub(super) fn begins_transaction(sql: &str) -> bool {
    let tokens = tokens(sql);
    (0..tokens.len()).any(|at| {
        let start = at == 0 || (tokens[at - 1].kind == Kind::Punct && tokens[at - 1].text == ";");
        start && (is_word(&tokens, at, "begin") || is_word(&tokens, at, "savepoint"))
    })
}

/// The table name at `at` (after a schema and a dot or not) and the index
/// after it; `values` also takes a value in single quotes, which SQLite may
/// take for a name.
fn name(tokens: &[Token], at: usize, values: bool) -> Option<(String, usize)> {
    let is_name = |t: &Token| {
        t.kind == Kind::Word || t.kind == Kind::Name || (values && t.kind == Kind::Value)
    };
    let first = tokens.get(at).filter(|t| is_name(t))?;
    let dotted = tokens
        .get(at + 1)
        .is_some_and(|t| t.kind == Kind::Punct && t.text == ".");
    match tokens.get(at + 2) {
        Some(table) if dotted && is_name(table) => Some((table.text.clone(), at + 3)),
        _ => Some((first.text.clone(), at + 1)),
    }
}

/// The functions of `functions` (in lower case) `sql` calls, in its order:
/// a word or a quoted name of the list followed by `(`, in any case and
/// with or without space between, so a value in quotes, a comment and a
/// longer word that spell one are no call.
pub(super) fn calls(sql: &str, functions: &[&str]) -> Vec<String> {
    let tokens = tokens(sql);
    (0..tokens.len())
        .filter(|&at| {
            matches!(tokens[at].kind, Kind::Word | Kind::Name)
                && functions.contains(&tokens[at].text.as_str())
                && tokens
                    .get(at + 1)
                    .is_some_and(|t| t.kind == Kind::Punct && t.text == "(")
        })
        .map(|at| tokens[at].text.clone())
        .collect()
}

/// Untyped positional parameters (`?`, `?N`, `$N`) tested with IS [NOT] NULL.
/// SQL comments and quoted text are ignored. Parentheses around a bare
/// parameter do not give it a type; CAST(? AS BIGINT) (or a numbered
/// parameter cast to its column's type) does, even
/// when NULL is the parameter's only use. Dynamically split SQL is unseen.
pub(super) fn untyped_null_parameters(sql: &str) -> Vec<String> {
    let tokens = tokens(sql);
    let mut out = Vec::new();
    for (at, token) in tokens.iter().enumerate() {
        if token.kind != Kind::Punct || !["?", "$"].contains(&token.text.as_str()) {
            continue;
        }
        let number = tokens
            .get(at + 1)
            .filter(|t| t.kind == Kind::Number && t.text.chars().all(|c| c.is_ascii_digit()));
        if number.is_none() && token.text != "?" {
            continue;
        }
        let mut next = at + 1 + usize::from(number.is_some());
        while tokens
            .get(next)
            .is_some_and(|t| t.kind == Kind::Punct && t.text == ")")
        {
            next += 1;
        }
        if is_word(&tokens, next, "is") {
            next += 1;
            if is_word(&tokens, next, "not") {
                next += 1;
            }
            if is_word(&tokens, next, "null") {
                out.push(format!(
                    "{}{}",
                    token.text,
                    number.map_or("", |n| n.text.as_str())
                ));
            }
        }
    }
    out
}

/// The words that end a conflict choice's `OR`.
const CONFLICT_CHOICES: [&str; 5] = ["ignore", "replace", "abort", "fail", "rollback"];

/// The keywords that, in capitals, make a string literal SQL, a piece of a
/// statement included. Not those a message writes in capitals as a placeholder
/// (`FROM..TO`).
const SQL_CAPITALS: [&str; 7] = ["SELECT", "INSERT", "UPDATE", "DELETE", "WHERE", "AND", "OR"];

/// The word at `at`, in any case.
fn spelled_word(tokens: &[Token], at: usize, word: &str) -> bool {
    tokens
        .get(at)
        .is_some_and(|t| t.kind == Kind::Word && t.text.eq_ignore_ascii_case(word))
}

/// Whether the word `word` is anywhere after `at`, in any case.
fn word_after(tokens: &[Token], at: usize, word: &str) -> bool {
    (at + 1..tokens.len()).any(|later| spelled_word(tokens, later, word))
}

/// The index after the parenthesized group that opens at `at`, or `at` when
/// none opens there.
fn after_group(tokens: &[Token], at: usize) -> usize {
    let is = |at: usize, text: &str| {
        tokens
            .get(at)
            .is_some_and(|t| t.kind == Kind::Punct && t.text == text)
    };
    if !is(at, "(") {
        return at;
    }
    let mut depth = 0;
    for later in at..tokens.len() {
        if is(later, "(") {
            depth += 1;
        } else if is(later, ")") {
            depth -= 1;
            if depth == 0 {
                return later + 1;
            }
        }
    }
    tokens.len()
}

/// Whether a statement begins at `at`, in any case: `select`, `values (`,
/// `with [recursive] name [(columns)] as [not] [materialized] (`, `insert ..
/// into`, `replace into .. values` or `.. select`, `update .. set`, `delete
/// from`, `create [temp | temporary] [unique] table`, `index`, `view` or
/// `trigger`, `create virtual table`, `alter table`, or `explain [query plan]`
/// before any of these.
fn begins_statement(tokens: &[Token], at: usize) -> bool {
    if spelled_word(tokens, at, "explain") {
        let mut next = at + 1;
        if spelled_word(tokens, next, "query") && spelled_word(tokens, next + 1, "plan") {
            next += 2;
        }
        return begins_statement(tokens, next);
    }
    let create = || {
        let mut next = at + 1;
        if spelled_word(tokens, next, "virtual") {
            return spelled_word(tokens, next + 1, "table");
        }
        if spelled_word(tokens, next, "temp") || spelled_word(tokens, next, "temporary") {
            next += 1;
        }
        if spelled_word(tokens, next, "unique") {
            next += 1;
        }
        ["table", "index", "view", "trigger"]
            .iter()
            .any(|object| spelled_word(tokens, next, object))
    };
    let open = |at: usize| {
        tokens
            .get(at)
            .is_some_and(|t| t.kind == Kind::Punct && t.text == "(")
    };
    let with = || {
        let mut next = at + 1;
        if spelled_word(tokens, next, "recursive") {
            next += 1;
        }
        if !tokens
            .get(next)
            .is_some_and(|t| matches!(t.kind, Kind::Word | Kind::Name))
        {
            return false;
        }
        next = after_group(tokens, next + 1);
        if !spelled_word(tokens, next, "as") {
            return false;
        }
        next += 1;
        if spelled_word(tokens, next, "not") {
            next += 1;
        }
        if spelled_word(tokens, next, "materialized") {
            next += 1;
        }
        open(next)
    };
    spelled_word(tokens, at, "select")
        || (spelled_word(tokens, at, "values") && open(at + 1))
        || (spelled_word(tokens, at, "with") && with())
        || (spelled_word(tokens, at, "insert") && word_after(tokens, at, "into"))
        || (spelled_word(tokens, at, "replace")
            && spelled_word(tokens, at + 1, "into")
            && (word_after(tokens, at + 1, "values") || word_after(tokens, at + 1, "select")))
        || (spelled_word(tokens, at, "update") && word_after(tokens, at, "set"))
        || (spelled_word(tokens, at, "delete") && spelled_word(tokens, at + 1, "from"))
        || (spelled_word(tokens, at, "create") && create())
        || (spelled_word(tokens, at, "alter") && spelled_word(tokens, at + 1, "table"))
}

/// Whether a string literal reads as SQL and not as prose: it has one of
/// [`SQL_CAPITALS`], or a statement begins ([`begins_statement`]) at its
/// start or after a `;`.
fn reads_as_sql(tokens: &[Token]) -> bool {
    let capitals = tokens
        .iter()
        .any(|t| t.kind == Kind::Word && SQL_CAPITALS.contains(&t.text.as_str()));
    capitals
        || (0..tokens.len()).any(|at| {
            let start = at == 0
                || tokens
                    .get(at - 1)
                    .is_some_and(|t| t.kind == Kind::Punct && t.text == ";");
            start && begins_statement(tokens, at)
        })
}

/// What `sql` spells that only SQLite takes where the store's SQL has a
/// spelling both dialects take, one entry for each, in its order: `INSERT OR`
/// and `UPDATE OR` with a conflict choice (or a `{}` of a format string in its
/// place), `REPLACE INTO`, a call of `ifnull`,
/// and an `IS` or `IS NOT` that compares with a value or a column. `IS [NOT]
/// NULL`, `IS [NOT] TRUE`, `IS [NOT] FALSE` and `IS [NOT] DISTINCT FROM` pass.
///
/// A string literal may be prose: in one that reads as SQL ([`reads_as_sql`])
/// all of them count in any case, whatever follows `IS` (a `{}` of a format
/// string and the end of a piece of a statement too). In prose, `IS` counts in
/// capitals or before a `?` or a value in quotes, and a call of `ifnull` counts:
/// "the run is closed" and "insert or replace the row" are no SQL.
pub(super) fn unshared_spellings(sql: &str) -> Vec<String> {
    let tokens = spelled_tokens(sql);
    let is_sql = reads_as_sql(&tokens);
    let is_choice = |at: usize| {
        CONFLICT_CHOICES
            .iter()
            .any(|choice| spelled_word(&tokens, at, choice))
    };
    let mut out = Vec::new();
    for at in 0..tokens.len() {
        let or_choice = spelled_word(&tokens, at + 1, "or")
            && (is_choice(at + 2)
                || tokens
                    .get(at + 2)
                    .is_some_and(|t| t.kind == Kind::Punct && t.text == "{"));
        if is_sql && spelled_word(&tokens, at, "insert") && or_choice {
            out.push("INSERT OR".to_string());
        } else if is_sql && spelled_word(&tokens, at, "update") && or_choice {
            out.push("UPDATE OR".to_string());
        } else if is_sql
            && spelled_word(&tokens, at, "replace")
            && spelled_word(&tokens, at + 1, "into")
            && !(at > 0 && spelled_word(&tokens, at - 1, "or"))
        {
            out.push("REPLACE INTO".to_string());
        } else if matches!(tokens[at].kind, Kind::Word | Kind::Name)
            && tokens[at].text.eq_ignore_ascii_case("ifnull")
            && tokens
                .get(at + 1)
                .is_some_and(|t| t.kind == Kind::Punct && t.text == "(")
        {
            out.push("ifnull(".to_string());
        } else if spelled_word(&tokens, at, "is") {
            let negated = spelled_word(&tokens, at + 1, "not");
            let right = if negated { at + 2 } else { at + 1 };
            let shared = ["null", "true", "false"]
                .iter()
                .any(|word| spelled_word(&tokens, right, word))
                || (spelled_word(&tokens, right, "distinct")
                    && spelled_word(&tokens, right + 1, "from"));
            let capitals = tokens[at].text == "IS";
            let value = tokens
                .get(right)
                .is_some_and(|t| t.kind == Kind::Value || t.text == "?");
            if !shared && (is_sql || capitals || value) {
                out.push(if negated { "IS NOT" } else { "IS" }.to_string());
            }
        }
    }
    out
}

/// The tables `sql` writes: `INSERT [OR choice] INTO t`, `REPLACE INTO t`,
/// `DELETE FROM t`, and `UPDATE [OR choice] t [[AS] alias] [INDEXED BY i |
/// NOT INDEXED] SET`, so a word update in prose, `DO UPDATE SET` and
/// `UPDATE OF` are no write.
pub(super) fn writes(sql: &str) -> Vec<String> {
    let lower = sql.to_lowercase();
    if !["insert", "replace", "update", "delete"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return Vec::new();
    }
    let tokens = tokens(sql);
    let mut out = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        if token.kind != Kind::Word {
            continue;
        }
        let table_at = match token.text.as_str() {
            "insert" => {
                let mut j = i + 1;
                if is_word(&tokens, j, "or") {
                    j += 2;
                }
                is_word(&tokens, j, "into").then_some(j + 1)
            }
            // The REPLACE of INSERT OR REPLACE is counted by its INSERT.
            "replace" if !(i > 0 && is_word(&tokens, i - 1, "or")) => {
                is_word(&tokens, i + 1, "into").then_some(i + 2)
            }
            "delete" => is_word(&tokens, i + 1, "from").then_some(i + 2),
            "update" => {
                let mut j = i + 1;
                if is_word(&tokens, j, "or") {
                    j += 2;
                }
                if is_word(&tokens, j, "set") || is_word(&tokens, j, "of") {
                    continue;
                }
                let Some((table, mut k)) = name(&tokens, j, true) else {
                    continue;
                };
                if is_word(&tokens, k, "as") {
                    k += 2;
                } else if tokens.get(k).is_some_and(|t| {
                    t.kind == Kind::Name
                        || (t.kind == Kind::Word
                            && !["set", "indexed", "not"].contains(&t.text.as_str()))
                }) {
                    k += 1;
                }
                if is_word(&tokens, k, "indexed") && is_word(&tokens, k + 1, "by") {
                    k += 3;
                } else if is_word(&tokens, k, "not") && is_word(&tokens, k + 1, "indexed") {
                    k += 2;
                }
                if is_word(&tokens, k, "set") {
                    out.push(table);
                }
                continue;
            }
            _ => None,
        };
        if let Some((table, _)) = table_at.and_then(|j| name(&tokens, j, true)) {
            out.push(table);
        }
    }
    out
}

/// The tables the migrations create at any point, and those they leave
/// after the last one, from the text of the migrations in their order:
/// `CREATE [VIRTUAL] TABLE [IF NOT EXISTS]` adds a table, `DROP TABLE [IF
/// EXISTS]` removes it and `ALTER TABLE x RENAME TO y` moves it to the new
/// name, so a table rebuilt under a temporary name or dropped is not left.
pub(super) fn migrated_tables(migrations: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let tokens = tokens(migrations);
    let mut created = BTreeSet::new();
    let mut kept = BTreeSet::new();
    for i in 0..tokens.len() {
        if is_word(&tokens, i, "create") {
            let mut j = i + 1;
            if is_word(&tokens, j, "virtual") {
                j += 1;
            }
            if !is_word(&tokens, j, "table") {
                continue;
            }
            j += 1;
            if is_word(&tokens, j, "if")
                && is_word(&tokens, j + 1, "not")
                && is_word(&tokens, j + 2, "exists")
            {
                j += 3;
            }
            if let Some((table, _)) = name(&tokens, j, false) {
                created.insert(table.clone());
                kept.insert(table);
            }
        } else if is_word(&tokens, i, "drop") && is_word(&tokens, i + 1, "table") {
            let mut j = i + 2;
            if is_word(&tokens, j, "if") && is_word(&tokens, j + 1, "exists") {
                j += 2;
            }
            if let Some((table, _)) = name(&tokens, j, false) {
                kept.remove(&table);
            }
        } else if is_word(&tokens, i, "alter") && is_word(&tokens, i + 1, "table") {
            let Some((from, j)) = name(&tokens, i + 2, false) else {
                continue;
            };
            if !(is_word(&tokens, j, "rename") && is_word(&tokens, j + 1, "to")) {
                continue;
            }
            if let Some((to, _)) = name(&tokens, j + 2, false)
                && kept.remove(&from)
            {
                kept.insert(to);
            }
        }
    }
    (created, kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(sql: &str) -> Vec<String> {
        writes(sql)
    }

    #[test]
    fn the_write_statements_name_their_tables() {
        assert_eq!(written("INSERT INTO tasks(id) VALUES (1)"), ["tasks"]);
        assert_eq!(
            written("insert or replace into main.task_runs(id) values (1)"),
            ["task_runs"]
        );
        assert_eq!(
            written("REPLACE INTO [run_leases] VALUES (1)"),
            ["run_leases"]
        );
        assert_eq!(
            written("DELETE FROM \"task_runs\" WHERE id = 1"),
            ["task_runs"]
        );
        assert_eq!(written("UPDATE 'task_runs' SET id = 1"), ["task_runs"]);
        assert_eq!(written("UPDATE task_runs AS r SET id = 1"), ["task_runs"]);
        assert_eq!(written("UPDATE task_runs r SET id = 1"), ["task_runs"]);
        assert_eq!(
            written("UPDATE task_runs NOT INDEXED SET id = 1"),
            ["task_runs"]
        );
        assert_eq!(
            written("UPDATE OR IGNORE main.task_runs AS r INDEXED BY i SET id = 1"),
            ["task_runs"]
        );
    }

    #[test]
    fn a_call_is_a_listed_word_before_a_parenthesis() {
        let listed = ["json_extract", "strftime"];
        assert_eq!(
            calls(
                "SELECT JSON_EXTRACT(p,'$.a'), strftime ('%s', x), json_each(p), \"strftime\"(x)",
                &listed
            ),
            ["json_extract", "strftime", "strftime"]
        );
        assert!(
            calls(
                "SELECT 'json_extract(p)', strftime, my_strftime(x) -- json_extract(p)\n/* strftime(x) */",
                &listed
            )
            .is_empty()
        );
    }

    #[test]
    fn the_spellings_only_sqlite_takes_are_named_in_their_order() {
        assert_eq!(
            unshared_spellings(
                "INSERT OR IGNORE INTO t SELECT IFNULL (a,0) FROM u WHERE a IS ?1 AND b IS NOT 'x'
                 AND c IS d AND e IS NOT f AND g IS {} AND h IS NOT 1 AND insert or replace"
            ),
            [
                "INSERT OR",
                "ifnull(",
                "IS",
                "IS NOT",
                "IS",
                "IS NOT",
                "IS",
                "IS NOT",
                "INSERT OR"
            ]
        );
        assert_eq!(
            unshared_spellings("a is ?1 AND b is not 'x'"),
            ["IS", "IS NOT"]
        );
        assert_eq!(unshared_spellings("AND r.label IS NOT"), ["IS NOT"]);
        assert_eq!(unshared_spellings("IS"), ["IS"]);
        assert_eq!(unshared_spellings("SELECT \"ifnull\"(a, 0)"), ["ifnull("]);
        assert_eq!(unshared_spellings("the ifnull(a, 0)"), ["ifnull("]);
    }

    #[test]
    fn in_a_statement_a_comparison_counts_in_any_case() {
        for (sql, spelled) in [
            ("SELECT a FROM t WHERE a is b", "IS"),
            ("SELECT a FROM t WHERE a is not 1", "IS NOT"),
            ("select a from t where a is b", "IS"),
            ("select a from t where a is not 1", "IS NOT"),
            ("Select a From t Where a Is b", "IS"),
            ("update t set a = 1 where a is b", "IS"),
            ("delete from t where a is not b", "IS NOT"),
            ("AND a is b", "IS"),
            ("WHERE a is not {}", "IS NOT"),
            ("select 1 is 1", "IS"),
            ("select 1 is not 1", "IS NOT"),
            ("with t(a,b) as (values(1,2)) select a is b from t", "IS"),
            (
                "with recursive t as materialized (select 1) select a is b from t",
                "IS",
            ),
            ("values (1 is 2)", "IS"),
            ("PRAGMA x; select 1 is not a", "IS NOT"),
            ("explain select 1 is 1", "IS"),
            ("explain query plan select 1 is not 1", "IS NOT"),
            (
                "explain query plan with t as (select 1) select a is b from t",
                "IS",
            ),
            ("create table t (a, check (a is not 1))", "IS NOT"),
            ("create temp view v as select a is b from t", "IS"),
            ("create unique index i on t (a) where a is not 1", "IS NOT"),
            (
                "create trigger r after insert on t when new.a is 1 begin select 1; end",
                "IS",
            ),
            ("alter table t add column b default (1 is 1)", "IS"),
        ] {
            assert_eq!(unshared_spellings(sql), [spelled], "{sql}");
        }
    }

    #[test]
    fn the_other_conflict_spellings_are_named_once() {
        for (sql, spelled) in [
            ("INSERT OR REPLACE INTO t VALUES (1)", "INSERT OR"),
            ("insert or abort into t values (1)", "INSERT OR"),
            ("REPLACE INTO t VALUES (1)", "REPLACE INTO"),
            ("replace into t values (1)", "REPLACE INTO"),
            ("UPDATE OR IGNORE t SET a = 1", "UPDATE OR"),
            ("update or replace t set a = 1", "UPDATE OR"),
            ("explain insert or ignore into t values (1)", "INSERT OR"),
            (
                "explain query plan replace into t values (1)",
                "REPLACE INTO",
            ),
            ("explain update or ignore t set a = 1", "UPDATE OR"),
            ("INSERT OR {} INTO t VALUES (1)", "INSERT OR"),
            ("update or {choice} t set a = 1", "UPDATE OR"),
        ] {
            assert_eq!(unshared_spellings(sql), [spelled], "{sql}");
        }
    }

    #[test]
    fn the_spellings_both_dialects_take_and_prose_are_not_named() {
        for sql in [
            "SELECT 1 WHERE a IS NULL AND b IS NOT NULL AND c is null",
            "WHERE a IS NOT DISTINCT FROM ?1 AND b IS DISTINCT FROM 'x'",
            "select a from t where a is not distinct from ?1 and b is distinct from c",
            "WHERE a IS TRUE AND b IS NOT FALSE",
            "INSERT INTO t VALUES (1) ON CONFLICT DO NOTHING",
            "SELECT COALESCE(a, 0), ifnull, my_ifnull(a), 'ifnull(a)' -- ifnull(a)\n/* a IS ?1 */",
            "SELECT 'a IS ?1', 'INSERT OR IGNORE', \"IS\"",
            "SELECT replace(a, 'x', 'y') FROM t",
            "the run is closed and it is not ready",
            "insert or update the row",
            "insert or replace the row",
            "update or replace the row",
            "replace into the file",
            "with no run, the task is closed",
            "with it as a guide the task is closed",
            "the values (a, b) it is not",
            "where the run is closed",
            "You read and explain; you change nothing, as it is",
            "explain the run, which is closed",
            "create the table, which is new",
            "alter the plan if it is late",
        ] {
            assert_eq!(unshared_spellings(sql), Vec::<String>::new(), "{sql}");
        }
    }

    #[test]
    fn prose_upserts_and_triggers_are_no_writes() {
        assert!(written("the update of a task").is_empty());
        assert!(
            written("INSERT INTO tasks(id) VALUES (1) ON CONFLICT(id) DO UPDATE SET id = 1")
                == ["tasks"]
        );
        assert!(
            written("CREATE TRIGGER t AFTER UPDATE OF status ON tasks BEGIN SELECT 1; END")
                .is_empty()
        );
        assert!(written("SELECT \"update task_runs set x = 1\" FROM tasks").is_empty());
    }

    #[test]
    fn comments_and_values_are_read_as_sqlite_reads_them() {
        assert!(written("-- UPDATE task_runs SET id = 1\nSELECT 1").is_empty());
        assert!(written("/* DELETE FROM task_runs */ SELECT 1").is_empty());
        assert!(written("SELECT 'INSERT INTO task_runs VALUES (1)'").is_empty());
        assert_eq!(
            written("UPDATE /* c */ task_runs SET id = 1"),
            ["task_runs"]
        );
        assert_eq!(
            written("UPDATE task_runs /* c */ SET id = 1"),
            ["task_runs"]
        );
        assert_eq!(
            written("INSERT /* c */ INTO task_runs VALUES (1)"),
            ["task_runs"]
        );
        assert_eq!(
            written("SELECT 1 -- c\nDELETE FROM run_leases"),
            ["run_leases"]
        );
        assert_eq!(
            written("SELECT '/*'; UPDATE task_runs SET id = 1; SELECT '*/'"),
            ["task_runs"]
        );
        assert_eq!(
            written("SELECT '--'; UPDATE task_runs SET id = 1"),
            ["task_runs"]
        );
        assert_eq!(
            written("SELECT \"--\", [/*]; DELETE FROM run_leases"),
            ["run_leases"]
        );
    }

    #[test]
    fn the_migrations_leave_what_they_create_and_do_not_drop_or_rename() {
        let (created, kept) = migrated_tables(
            "CREATE TABLE tasks (id INTEGER);
             CREATE TABLE IF NOT EXISTS old_runs (id TEXT);
             ALTER TABLE old_runs RENAME TO task_runs;
             CREATE VIRTUAL TABLE search_index USING fts5(x);
             CREATE TABLE tasks_new (id INTEGER); DROP TABLE tasks; ALTER TABLE tasks_new RENAME TO tasks;
             CREATE TABLE ghosts (id INTEGER); DROP TABLE IF EXISTS main.ghosts;
             -- DROP TABLE tasks
             INSERT INTO tasks VALUES ('DROP TABLE task_runs');
             CREATE TABLE \"shades\" (id INTEGER);
             CREATE TABLE main.wraiths (id INTEGER);
             CREATE TABLE haunts (id INTEGER); SELECT \"drop table haunts\";",
        );
        let kept: Vec<&str> = kept.iter().map(String::as_str).collect();
        assert_eq!(
            kept,
            [
                "haunts",
                "search_index",
                "shades",
                "task_runs",
                "tasks",
                "wraiths"
            ]
        );
        assert!(
            created.contains("ghosts")
                && created.contains("old_runs")
                && created.contains("tasks_new")
        );
    }
}

/// The judgement-facing part of PlanningStore has exactly the append entry.
/// Inspect names and argument types as Rust syntax: methods mentioning a
/// judgement or membership cannot add an update/delete operation. Disguised
/// names, dynamic dispatch outside this port and SQL built at runtime remain
/// review concerns; deployment also withholds UPDATE/DELETE on the table.
fn judgement_entries(text: &str) -> Vec<String> {
    use syn::visit::{self, Visit};
    struct Mentions(bool);
    impl<'ast> Visit<'ast> for Mentions {
        fn visit_ident(&mut self, ident: &'ast syn::Ident) {
            let name = ident.to_string().to_lowercase();
            self.0 |= ["judgement", "membership", "follow_up"]
                .iter()
                .any(|s| name.contains(s));
            visit::visit_ident(self, ident);
        }
    }
    let file = syn::parse_file(text).expect("planning port parses");
    let port = file
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Trait(t) if t.ident == "PlanningStore" => Some(t),
            _ => None,
        })
        .expect("PlanningStore exists");
    port.items
        .iter()
        .filter_map(|item| {
            let syn::TraitItem::Fn(method) = item else {
                return None;
            };
            let mut mentions = Mentions(false);
            mentions.visit_signature(&method.sig);
            mentions.0.then(|| method.sig.ident.to_string())
        })
        .collect()
}

#[test]
fn membership_port_only_appends_judgements() {
    assert_eq!(
        judgement_entries(include_str!("../application/commands/planning.rs")),
        ["judge_follow_up"]
    );
}

#[test]
fn membership_port_rejects_update_and_delete_entries() {
    for operation in [
        "update_judgement",
        "delete_membership",
        "rewrite_follow_up",
        "remove",
    ] {
        let text = format!(
            "trait PlanningStore {{ fn judge_follow_up(&self); fn {operation}(&self, value: MembershipJudgement); }}"
        );
        assert_ne!(judgement_entries(&text), ["judge_follow_up"]);
    }
}

/// SQLite-only row identity, scalar extrema, and numeric boolean columns.
/// Names and comments are tokenized just as for calls; nested commas do
/// not turn a one-argument aggregate into a scalar function.
pub(super) fn nonportable_values(sql: &str, allow_rowid: bool) -> Vec<String> {
    let tokens = tokens(sql);
    let mut found = BTreeSet::new();
    for (at, token) in tokens.iter().enumerate() {
        if !matches!(token.kind, Kind::Word | Kind::Name) {
            continue;
        }
        if !allow_rowid && token.text == "rowid" {
            found.insert("rowid outside insertion_order".to_owned());
        }
        if [
            "final",
            "legacy",
            "is_last",
            "handoff_accepted",
            "wait_for_build",
            "answer_approval",
            "auto_update",
            "background",
        ]
        .contains(&token.text.as_str())
        {
            let mut next = at + 1;
            while tokens.get(next).is_some_and(|t| {
                t.kind == Kind::Punct && ["=", "!", "<", ">"].contains(&t.text.as_str())
            }) {
                next += 1;
            }
            if next > at + 1
                && tokens.get(next).is_some_and(|t| {
                    t.kind == Kind::Number && ["0", "1"].contains(&t.text.as_str())
                })
            {
                found.insert(format!("numeric boolean {}", token.text));
            }
        }
        if !["max", "min"].contains(&token.text.as_str())
            || !tokens.get(at + 1).is_some_and(|t| t.text == "(")
        {
            continue;
        }
        let mut depth = 0;
        let mut scalar = false;
        let mut predicate = false;
        for inner in &tokens[at + 1..] {
            match inner.text.as_str() {
                "(" if inner.kind == Kind::Punct => depth += 1,
                ")" if inner.kind == Kind::Punct => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                "," if inner.kind == Kind::Punct && depth == 1 => scalar = true,
                "=" | "<" | ">" if inner.kind == Kind::Punct => predicate = true,
                "is" | "in" | "like" | "not" if inner.kind == Kind::Word => predicate = true,
                _ => (),
            }
        }
        if scalar || predicate {
            found.insert(format!(
                "{} of {}",
                token.text,
                if scalar {
                    "two scalar arguments"
                } else {
                    "a predicate"
                }
            ));
        }
    }
    found.into_iter().collect()
}
