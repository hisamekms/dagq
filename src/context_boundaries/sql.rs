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
    c.is_ascii_lowercase() || c == '_' || !c.is_ascii()
}

fn word_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '$' || !c.is_ascii()
}

/// The tokens of `sql` in lower case, read from the left: a quoted name or
/// a value is one token whose inside is never read as SQL (doubled quotes
/// inside are one), and comments (`/* ... */`, to the end when not closed,
/// and `--` to the end of the line) are dropped, so a comment marker inside
/// a name or a value is no comment.
fn tokens(sql: &str) -> Vec<Token> {
    let chars: Vec<char> = sql.to_lowercase().chars().collect();
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
                .take_while(|&&c| {
                    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.'
                })
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
