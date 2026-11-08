//! Test-only guard for reads reaching `visit_rows_on`. It cannot see
//! INSERT ... SELECT through execute, direct driver prepare calls, or SQL
//! never exercised by lib tests. No statistics are assumed in production.

#![cfg(test)]

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use rusqlite::Connection;
use sha2::{Digest, Sha256};

/// SQL tokens keep literals intact so whitespace inside a value changes its
/// identity. Comments are immaterial; quoted identifiers remain identifiers.
fn tokens(sql: &str) -> Vec<String> {
    let chars: Vec<_> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(chars.len());
            continue;
        }
        let start = i;
        i += 1;
        if matches!(c, '\'' | '"' | '`' | '[') {
            let end = if c == '[' { ']' } else { c };
            while i < chars.len() {
                if chars[i] == end {
                    i += 1;
                    if chars.get(i) == Some(&end) {
                        i += 1;
                    } else {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if c.is_alphanumeric() || matches!(c, '_' | '?' | ':' | '@' | '$') {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
        }
        out.push(chars[start..i].iter().collect());
    }
    out
}

/// Fold whitespace and variable-length lists of bind parameters, keeping
/// parameter numbers and literal contents elsewhere. SHA-256 of this UTF-8
/// text is the exception identity; edits outside those folds expire it.
fn normalized(sql: &str) -> String {
    let t = tokens(sql);
    let mut out = Vec::new();
    let mut i = 0;
    while i < t.len() {
        if t[i].eq_ignore_ascii_case("in") && t.get(i + 1).is_some_and(|s| s == "(") {
            let mut end = i + 2;
            while end < t.len() && (t[end].starts_with('?') || t[end] == ",") {
                end += 1;
            }
            let list = &t[i + 2..end];
            if !list.is_empty()
                && list.len() % 2 == 1
                && list.iter().enumerate().all(|(n, s)| {
                    if n % 2 == 0 {
                        s.starts_with('?')
                    } else {
                        s == ","
                    }
                })
                && t.get(end).is_some_and(|s| s == ")")
            {
                out.push("IN ( ?... )".to_owned());
                i = end + 1;
                continue;
            }
        }
        out.push(t[i].clone());
        i += 1;
    }
    out.join(" ")
}

fn hash(sql: &str) -> String {
    format!("{:x}", Sha256::digest(normalized(sql).as_bytes()))
}

fn identifier(s: &str) -> String {
    s.trim_matches(['"', '`', '[', ']']).to_ascii_lowercase()
}

/// SQLite's plan tree, retained so identical aliases in different SELECTs
/// are not confused. Synthetic decision cases can omit the tree with From.
#[derive(Debug)]
struct PlanStep {
    id: i64,
    parent: i64,
    detail: String,
}

impl PlanStep {
    fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            parent: row.get(1)?,
            detail: row.get(3)?,
        })
    }
}

impl From<&str> for PlanStep {
    fn from(detail: &str) -> Self {
        Self {
            id: -1,
            parent: -1,
            detail: detail.to_owned(),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct PlanContext {
    scope: Option<usize>,
    blocking: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum IdPredicate {
    Range,
    Point,
    Other,
}

/// One SELECT branch, excluding sibling compound branches and nested SELECTs.
/// A compound's final LIMIT governs all its branches; a nested LIMIT does not.
struct SelectScope {
    start: usize,
    end: usize,
    level: usize,
    query_end: usize,
    from_parent: Option<usize>,
    limit: bool,
}

struct EventRead {
    scope: usize,
    /// Qualifiers that may name this read's columns: its alias, or the
    /// table name with and without the schema it was written with.
    names: Vec<String>,
    /// SQLite's plan names a table by its alias, otherwise as written,
    /// schema included.
    plan_name: String,
    index: Option<String>,
}

/// The labels under which a query passes on some columns of an event read.
type Columns = HashSet<String>;

fn column_set(names: &[&str]) -> Columns {
    names.iter().map(|&c| c.to_owned()).collect()
}

/// Words that end a table reference instead of naming it.
const NOT_ALIASES: &[&str] = &[
    "where",
    "indexed",
    "not",
    "order",
    "group",
    "limit",
    "join",
    "left",
    "right",
    "full",
    "cross",
    "inner",
    "outer",
    "on",
    "using",
    "having",
    "window",
    "natural",
    "offset",
    "returning",
    "union",
    "except",
    "intersect",
    "from",
];

/// Words that continue an expression, so they neither end one nor label it.
const EXPRESSION_WORDS: &[&str] = &[
    "not", "is", "and", "or", "collate", "between", "like", "glob", "regexp", "match", "in",
    "escape", "case", "when", "then", "else", "distinct", "all", "as", "isnull", "notnull", "null",
    "end", "true", "false", "exists", "cast", "over", "filter",
];

/// One entry of a SELECT's result list.
enum ResultColumn {
    /// `*` or `qualifier.*`.
    Star(Option<String>),
    /// The column a bare column reference reads, and the entry's label.
    Expr {
        column: Option<(Option<String>, String)>,
        label: Option<String>,
    },
}

fn qualifies(qualifier: Option<&str>, names: &[String]) -> bool {
    qualifier.is_none_or(|q| names.iter().any(|name| name == q))
}

/// The small amount of SQL structure the plan rules need: SELECT ownership,
/// table aliases and column operands. Raw tokens distinguish keywords from
/// quoted identifiers and literals; the exception normalization stays separate.
struct ReadSql {
    raw: Vec<String>,
    words: Vec<String>,
    levels: Vec<usize>,
    pairs: HashMap<usize, usize>,
    scopes: Vec<SelectScope>,
    reads: Vec<EventRead>,
}

impl ReadSql {
    fn new(sql: &str) -> Self {
        let raw = tokens(sql);
        let words = raw.iter().map(|s| identifier(s)).collect();
        let mut parsed = Self {
            raw,
            words,
            levels: Vec::new(),
            pairs: HashMap::new(),
            scopes: Vec::new(),
            reads: Vec::new(),
        };
        let mut stack = Vec::new();
        for (i, token) in parsed.raw.iter().enumerate() {
            parsed.levels.push(stack.len());
            if token == "(" {
                stack.push(i);
            }
            if token == ")"
                && let Some(open) = stack.pop()
            {
                parsed.pairs.insert(open, i);
                parsed.pairs.insert(i, open);
            }
        }
        for i in 0..parsed.raw.len() {
            if !parsed.keyword(i, "select") {
                continue;
            }
            let level = parsed.levels[i];
            let query_end = (i + 1..parsed.raw.len())
                .find(|&j| {
                    parsed.levels[j] < level
                        || (parsed.levels[j] == level
                            && matches!(parsed.raw[j].as_str(), ")" | ";"))
                })
                .unwrap_or(parsed.raw.len());
            let end = (i + 1..query_end)
                .find(|&j| {
                    parsed.levels[j] == level
                        && ["union", "except", "intersect"]
                            .iter()
                            .any(|k| parsed.keyword(j, k))
                })
                .unwrap_or(query_end);
            let limit =
                (i + 1..query_end).any(|j| parsed.levels[j] == level && parsed.keyword(j, "limit"));
            parsed.scopes.push(SelectScope {
                start: i,
                end,
                level,
                query_end,
                from_parent: None,
                limit,
            });
        }
        for scope in 0..parsed.scopes.len() {
            let end = parsed.scopes[scope].query_end;
            let Some(&open) = parsed.pairs.get(&end) else {
                continue;
            };
            // A subquery after a comma of the result list is a value.
            if parsed.table_reference(open) {
                parsed.scopes[scope].from_parent = parsed.owner(open);
            }
        }
        for i in 0..parsed.raw.len() {
            if parsed.words[i] != "run_events" {
                continue;
            }
            let Some(scope) = parsed.owner(i) else {
                continue;
            };
            let schema = (i >= 2 && parsed.raw[i - 1] == ".").then(|| parsed.words[i - 2].clone());
            if !parsed.table_reference(if schema.is_some() { i - 2 } else { i }) {
                continue;
            }
            let mut next = i + 1;
            // SQLite also permits an alias outside a parenthesized table.
            while parsed.raw.get(next).is_some_and(|s| s == ")")
                && parsed
                    .pairs
                    .get(&next)
                    .is_some_and(|&open| open > parsed.scopes[scope].start)
            {
                next += 1;
            }
            let explicit = parsed.keyword(next, "as");
            if explicit {
                next += 1;
            }
            let alias = parsed.raw.get(next).is_some_and(|s| {
                explicit
                    || s.starts_with(['"', '`', '['])
                    || (s
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_alphabetic() || c == '_')
                        && !NOT_ALIASES.contains(&parsed.words[next].as_str()))
            });
            // An unqualified name also resolves as main.run_events.
            let (names, plan_name) = if alias {
                let alias = parsed.words[next].clone();
                next += 1;
                (vec![alias.clone()], alias)
            } else if let Some(schema) = schema {
                let qualified = format!("{schema}.run_events");
                (vec!["run_events".to_owned(), qualified.clone()], qualified)
            } else {
                (
                    vec!["run_events".to_owned(), "main.run_events".to_owned()],
                    "run_events".to_owned(),
                )
            };
            let index = if parsed.keyword(next, "indexed") && parsed.keyword(next + 1, "by") {
                parsed.words.get(next + 2).cloned()
            } else {
                None
            };
            parsed.reads.push(EventRead {
                scope,
                names,
                plan_name,
                index,
            });
        }
        parsed
    }

    fn keyword(&self, i: usize, word: &str) -> bool {
        self.raw
            .get(i)
            .is_some_and(|s| s.eq_ignore_ascii_case(word))
    }

    fn owner(&self, i: usize) -> Option<usize> {
        self.scopes
            .iter()
            .enumerate()
            .filter(|(_, scope)| scope.start <= i && i < scope.end)
            .max_by_key(|(_, scope)| (scope.level, scope.start))
            .map(|(n, _)| n)
    }

    /// Only filter clauses select an index. A NULL expression in a SELECT
    /// list, or a condition inside another SELECT, does not constrain this read.
    fn is_filter(&self, scope: usize, i: usize) -> bool {
        let select = &self.scopes[scope];
        let mut filter = false;
        for j in select.start..i {
            if self.owner(j) != Some(scope) {
                continue;
            }
            if self.keyword(j, "on")
                || (self.levels[j] == select.level
                    && (self.keyword(j, "where") || self.keyword(j, "having")))
            {
                filter = true;
            }
            if self.levels[j] == select.level
                && ["select", "from", "join", "group", "order", "limit"]
                    .iter()
                    .any(|k| self.keyword(j, k))
            {
                filter = false;
            }
        }
        filter
    }

    /// A bare, table- or schema-qualified column in `[start, end)`, with any
    /// number of redundant parentheses. The qualifier keeps its dots.
    fn column_ref(&self, start: usize, end: usize) -> Option<(Option<String>, &str)> {
        let (start, end) = self.strip_parens(start, end);
        let column = match &self.words[start..end] {
            [column] => (None, column.as_str()),
            [table, dot, column] if dot == "." => (Some(table.clone()), column.as_str()),
            [schema, d1, table, d2, column] if d1 == "." && d2 == "." => {
                (Some(format!("{schema}.{table}")), column.as_str())
            }
            _ => return None,
        };
        // A quoted string, bind parameter or number is not a column operand.
        if self.raw[end - 1].starts_with(['\'', '?', ':', '@', '$'])
            || self.raw[end - 1]
                .chars()
                .next()
                .is_none_or(|c| c.is_ascii_digit())
        {
            return None;
        }
        Some(column)
    }

    /// The operand of the `IS NULL` or `ISNULL` at `is`.
    /// Function calls and unary + are expressions rather than index operands.
    fn null_column(&self, is: usize) -> Option<(Option<String>, &str)> {
        let mut start = is.checked_sub(1)?;
        if self.raw[start] == ")" {
            start = *self.pairs.get(&start)?;
        } else {
            while start >= 2 && self.raw[start - 1] == "." {
                start -= 2;
            }
        }
        if start > 0
            && (self.raw[start - 1] == "+"
                || self.keyword(start - 1, "not")
                || (self.raw[start] == "("
                    && self.raw[start - 1]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_alphabetic())
                    && !["where", "and", "or", "on", "having", "when", "then"]
                        .iter()
                        .any(|k| self.keyword(start - 1, k))))
        {
            return None;
        }
        self.column_ref(start, is)
    }

    /// A filter of `scope` tests a column the read passes on for NULL.
    fn null_filter(&self, scope: usize, names: &[String], columns: &Columns) -> bool {
        (1..self.raw.len()).any(|i| {
            (self.keyword(i, "isnull") || (self.keyword(i, "is") && self.keyword(i + 1, "null")))
                && self.owner(i) == Some(scope)
                && self.is_filter(scope, i)
                && self.null_column(i).is_some_and(|(qualifier, name)| {
                    columns.contains(name) && qualifies(qualifier.as_deref(), names)
                })
        })
    }

    /// SQLite flattens FROM subqueries and CTEs into the query reading them,
    /// or pushes that query's WHERE terms down into them, so an outer test
    /// also reaches the read. Neither crosses a LIMIT or a CTE kept apart.
    fn null_can_select(&self, read: &EventRead, column: &str, index: &str) -> bool {
        if read.index.as_deref().is_some_and(|hint| hint != index) {
            return false;
        }
        let mut pending = vec![(read.scope, read.names.clone(), column_set(&[column]))];
        let mut visited = HashSet::new();
        while let Some((scope, names, columns)) = pending.pop() {
            if !visited.insert(scope) {
                continue;
            }
            if self.null_filter(scope, &names, &columns) {
                return true;
            }
            if self.scopes[scope].limit || self.cte_kept_apart(scope) {
                continue;
            }
            let Some(columns) = self.output_columns(scope, &names, &columns) else {
                continue;
            };
            for parent in self.parents(scope) {
                let names = self.source_alias(scope, parent).into_iter().collect();
                pending.push((parent, names, columns.clone()));
            }
        }
        false
    }

    fn group(&self, scope: usize) -> Vec<usize> {
        let query = &self.scopes[scope];
        self.scopes
            .iter()
            .enumerate()
            .filter(|(_, s)| s.level == query.level && s.query_end == query.query_end)
            .map(|(i, _)| i)
            .collect()
    }

    /// The name and the closing parenthesis of the column list of the CTE
    /// whose body is `scope`, optionally AS MATERIALIZED / AS NOT MATERIALIZED.
    fn cte_definition(&self, scope: usize) -> Option<(usize, Option<usize>)> {
        let end = self.scopes[scope].query_end;
        let &open = self.pairs.get(&end)?;
        let mut before = open;
        if before > 0 && self.keyword(before - 1, "materialized") {
            before -= 1;
        }
        if before > 0 && self.keyword(before - 1, "not") {
            before -= 1;
        }
        if before < 2 || !self.keyword(before - 1, "as") {
            return None;
        }
        let name = before - 2;
        if self.raw[name] == ")" {
            return Some((self.pairs.get(&name)?.checked_sub(1)?, Some(name)));
        }
        Some((name, None))
    }

    fn cte_name(&self, scope: usize) -> Option<String> {
        self.cte_definition(scope)
            .map(|(name, _)| self.words[name].clone())
    }

    fn cte_columns(&self, scope: usize) -> Option<Vec<String>> {
        let (name, close) = self.cte_definition(scope)?;
        let close = close?;
        Some(
            self.words[name + 2..close]
                .iter()
                .filter(|w| *w != ",")
                .cloned()
                .collect(),
        )
    }

    /// SQLite neither flattens nor pushes WHERE terms into a CTE that is
    /// MATERIALIZED, recursive, or read more than once without NOT MATERIALIZED.
    fn cte_kept_apart(&self, scope: usize) -> bool {
        let Some((name, _)) = self.cte_definition(scope) else {
            return false;
        };
        let close = self.scopes[scope].query_end;
        let Some(&open) = self.pairs.get(&close) else {
            return false;
        };
        let materialized = self.keyword(open - 1, "materialized");
        let not = materialized && open >= 2 && self.keyword(open - 2, "not");
        let reads: Vec<_> = (0..self.raw.len())
            .filter(|&i| i != name && self.words[i] == self.words[name] && self.table_reference(i))
            .collect();
        let recursive = reads.iter().any(|&i| open < i && i < close);
        recursive || (materialized && !not) || (reads.len() > 1 && !not)
    }

    /// The entries of `scope`'s result list, up to its FROM or later clauses.
    fn result_columns(&self, scope: usize) -> Vec<ResultColumn> {
        let select = &self.scopes[scope];
        let at_level = |j: usize| self.levels[j] == select.level && self.owner(j) == Some(scope);
        let mut start = select.start + 1;
        if self.keyword(start, "distinct") || self.keyword(start, "all") {
            start += 1;
        }
        let end = (start..select.end)
            .find(|&j| {
                at_level(j)
                    && [
                        "from", "where", "group", "having", "window", "order", "limit",
                    ]
                    .iter()
                    .any(|k| self.keyword(j, k))
            })
            .unwrap_or(select.end);
        let mut entries = Vec::new();
        let mut from = start;
        for j in start..=end {
            if j < end && !(at_level(j) && self.raw[j] == ",") {
                continue;
            }
            if from < j {
                entries.push(self.result_column(from, j));
            }
            from = j + 1;
        }
        entries
    }

    fn result_column(&self, start: usize, end: usize) -> ResultColumn {
        match &self.raw[start..end] {
            [star] if star == "*" => return ResultColumn::Star(None),
            [_, dot, star] if dot == "." && star == "*" => {
                return ResultColumn::Star(Some(self.words[start].clone()));
            }
            _ => {}
        }
        let column = |end| {
            self.column_ref(start, end)
                .map(|(qualifier, column)| (qualifier, column.to_owned()))
        };
        let word = |i: usize| {
            self.raw[i]
                .starts_with(|c: char| c.is_alphabetic() || matches!(c, '_' | '"' | '`' | '['))
        };
        let continues = |i: usize| {
            !self.raw[i].starts_with(['"', '`', '['])
                && EXPRESSION_WORDS.iter().any(|k| self.keyword(i, k))
        };
        // The last token of an expression: a value, a column or END.
        let ends = |i: usize| {
            self.raw[i] == ")"
                || self.raw[i].starts_with(|c: char| c.is_ascii_digit() || "'?:@$".contains(c))
                || (word(i)
                    && (!continues(i)
                        || ["end", "null", "true", "false"]
                            .iter()
                            .any(|k| self.keyword(i, k))))
        };
        // A label after AS, a column's own name, or a label without AS.
        let (column, label) = if end - start >= 3 && self.keyword(end - 2, "as") {
            (column(end - 2), Some(self.words[end - 1].clone()))
        } else if let Some(read) = column(end) {
            let label = Some(read.1.clone());
            (Some(read), label)
        } else if end - start >= 2 && word(end - 1) && !continues(end - 1) && ends(end - 2) {
            (column(end - 1), Some(self.words[end - 1].clone()))
        } else {
            (None, None)
        };
        ResultColumn::Expr { column, label }
    }

    /// The labels under which a reader of `scope` sees the tracked columns,
    /// or None when none is passed on. A CTE column list and the first
    /// compound branch name columns by position. When `*` hides positions,
    /// a CTE's columns may take any label of its list, and a compound
    /// branch's columns keep the labels this branch gives them.
    fn output_columns(&self, scope: usize, names: &[String], tracked: &Columns) -> Option<Columns> {
        let entries = self.result_columns(scope);
        let starred = entries
            .iter()
            .any(|entry| matches!(entry, ResultColumn::Star(_)));
        let first = self.group(scope)[0];
        let list = self.cte_columns(scope);
        let positions = match &list {
            Some(list) => Some(list.iter().cloned().map(Some).collect::<Vec<_>>()),
            None if first != scope => self
                .result_columns(first)
                .into_iter()
                .map(|entry| match entry {
                    ResultColumn::Star(_) => None,
                    ResultColumn::Expr { label, .. } => Some(label),
                })
                .collect::<Option<Vec<_>>>(),
            None => None,
        }
        .filter(|_| !starred);
        let mut out = HashSet::new();
        for (k, entry) in entries.iter().enumerate() {
            let label = match entry {
                ResultColumn::Star(qualifier) if qualifies(qualifier.as_deref(), names) => None,
                ResultColumn::Expr {
                    column: Some((qualifier, column)),
                    label,
                } if qualifies(qualifier.as_deref(), names) && tracked.contains(column) => {
                    Some(label)
                }
                _ => continue,
            };
            match (&positions, &list, label) {
                (Some(labels), _, _) => out.extend(labels.get(k).cloned().flatten()),
                (None, Some(list), _) => out.extend(list.iter().cloned()),
                (None, None, None) => out.extend(tracked.iter().cloned()),
                (None, None, Some(label)) => out.extend(label.clone()),
            }
        }
        (!out.is_empty()).then_some(out)
    }

    fn table_reference(&self, i: usize) -> bool {
        let mut before = i;
        while before > 0 && self.raw[before - 1] == "(" {
            before -= 1;
        }
        if before == 0 {
            return false;
        }
        if self.keyword(before - 1, "from") || self.keyword(before - 1, "join") {
            return true;
        }
        if self.raw[before - 1] != "," {
            return false;
        }
        let Some(scope) = self.owner(i) else {
            return false;
        };
        let mut from = false;
        for j in self.scopes[scope].start..before {
            if self.owner(j) != Some(scope) || self.levels[j] != self.scopes[scope].level {
                continue;
            }
            if self.keyword(j, "from") {
                from = true;
            }
            if ["where", "group", "having", "order", "limit"]
                .iter()
                .any(|k| self.keyword(j, k))
            {
                from = false;
            }
        }
        from
    }

    fn parents(&self, scope: usize) -> Vec<usize> {
        if let Some(parent) = self.scopes[scope].from_parent {
            return vec![parent];
        }
        let Some(name) = self.cte_name(scope) else {
            return Vec::new();
        };
        let mut parents: Vec<_> = (0..self.raw.len())
            .filter(|&i| self.words[i] == name && self.table_reference(i))
            .filter_map(|i| self.owner(i))
            .filter(|&parent| parent != scope)
            .collect();
        parents.sort_unstable();
        parents.dedup();
        parents
    }

    /// Flattened FROM subqueries and CTE consumers remain visible in their
    /// parent's plan. Scalar subqueries and sibling compound branches do not.
    fn visible_from(&self, scope: usize, context: usize) -> bool {
        let mut pending = vec![scope];
        let mut visited = HashSet::new();
        while let Some(scope) = pending.pop() {
            if scope == context {
                return true;
            }
            if visited.insert(scope) {
                pending.extend(self.parents(scope));
            }
        }
        false
    }

    fn parent_on_path(&self, scope: usize, target: Option<usize>) -> Option<usize> {
        let parents: Vec<_> = self
            .parents(scope)
            .into_iter()
            .filter(|&parent| target.is_none_or(|target| self.visible_from(parent, target)))
            .collect();
        if parents.len() == 1 {
            Some(parents[0])
        } else {
            None
        }
    }

    fn bounded(&self, mut scope: usize, context: PlanContext) -> bool {
        let target = context.scope.filter(|&target| target != scope);
        let mut visited = HashSet::new();
        while visited.insert(scope) {
            if self.scopes[scope].limit {
                return true;
            }
            if context.blocking {
                return false;
            }
            let Some(parent) = self.parent_on_path(scope, target) else {
                return false;
            };
            scope = parent;
        }
        false
    }

    /// SQLite numbers SELECTs as they finish parsing: nested queries before
    /// their enclosing SELECT, compound branches from left to right.
    fn subquery_scope(&self, detail: &str) -> Option<usize> {
        let upper = detail.to_ascii_uppercase();
        let number = upper
            .split_once("SUBQUERY ")
            .map(|(_, n)| n)
            .or_else(|| upper.split_once("SUBQUERY-").map(|(_, n)| n))
            .and_then(|n| {
                n.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<usize>()
                    .ok()
            });
        if let Some(number) = number {
            let mut order: Vec<_> = self
                .scopes
                .iter()
                .enumerate()
                .map(|(i, scope)| (scope.end, Some(i)))
                .collect();
            // Each VALUES row is also a SQLite Select and occupies an ID.
            for i in 0..self.raw.len() {
                if !self.keyword(i, "values") {
                    continue;
                }
                let mut row = i + 1;
                while self.raw.get(row).is_some_and(|s| s == "(") {
                    let Some(&end) = self.pairs.get(&row) else {
                        break;
                    };
                    order.push((end, None));
                    if self.raw.get(end + 1).is_none_or(|s| s != ",") {
                        break;
                    }
                    row = end + 2;
                }
            }
            order.sort_by_key(|&(end, _)| end);
            return number
                .checked_sub(1)
                .and_then(|n| order.get(n).and_then(|&(_, scope)| scope));
        }
        if !(upper.starts_with("MATERIALIZE ") || upper.starts_with("CO-ROUTINE ")) {
            return None;
        }
        let name = identifier(detail.split_once(' ')?.1);
        (0..self.scopes.len()).find(|&i| {
            let end = self.scopes[i].query_end;
            if !self.pairs.contains_key(&end) {
                return false;
            }
            if self.cte_name(i).as_ref() == Some(&name) {
                return true;
            }
            let mut next = end + 1;
            if self.keyword(next, "as") {
                next += 1;
            }
            self.words.get(next) == Some(&name)
        })
    }

    fn compound_leaves(node: usize, plan: &[PlanStep]) -> Vec<usize> {
        let children: Vec<_> = plan
            .iter()
            .enumerate()
            .filter(|(i, row)| *i != node && row.parent == plan[node].id)
            .map(|(i, _)| i)
            .collect();
        if plan[node].detail.starts_with("COMPOUND QUERY") {
            return children;
        }
        if plan[node].detail.starts_with("MERGE ") {
            return children
                .into_iter()
                .flat_map(|i| Self::compound_leaves(i, plan))
                .collect();
        }
        if children.len() == 1 && plan[children[0]].detail.starts_with("MERGE ") {
            return Self::compound_leaves(children[0], plan);
        }
        vec![node]
    }

    fn assign_plan(
        &self,
        node: usize,
        mut context: PlanContext,
        plan: &[PlanStep],
        result: &mut [PlanContext],
    ) {
        let detail = &plan[node].detail;
        if detail.contains("SUBQUERY ")
            || detail.contains("subquery-")
            || detail.starts_with("MATERIALIZE ")
            || detail.starts_with("CO-ROUTINE ")
        {
            // Unknown subquery wrappers must not inherit an unrelated scope.
            context.scope = self.subquery_scope(detail);
        }
        context.blocking |= plan[node].detail.starts_with("MATERIALIZE ");
        result[node] = context;
        if (plan[node].detail.starts_with("COMPOUND QUERY")
            || plan[node].detail.starts_with("MERGE "))
            && let Some(mut scope) = context.scope
        {
            let mut group = self.group(scope);
            // A flattened UNION ALL in FROM can supply the compound root.
            if group.len() == 1
                && let Some(child) = (0..self.scopes.len())
                    .find(|&i| self.scopes[i].from_parent == Some(scope) && self.group(i).len() > 1)
            {
                scope = child;
                group = self.group(scope);
            }
            let leaves = Self::compound_leaves(node, plan);
            if leaves.len() == group.len() && group.len() > 1 {
                for (leaf, branch) in leaves.into_iter().zip(group) {
                    self.assign_plan(
                        leaf,
                        PlanContext {
                            scope: Some(branch),
                            ..context
                        },
                        plan,
                        result,
                    );
                }
                return;
            }
        }
        for child in 0..plan.len() {
            if plan[child].parent == plan[node].id && child != node {
                self.assign_plan(child, context, plan, result);
            }
        }
    }

    fn plan_contexts(&self, plan: &[PlanStep]) -> Vec<PlanContext> {
        let mut result = vec![PlanContext::default(); plan.len()];
        let root =
            (0..self.scopes.len()).min_by_key(|&i| (self.scopes[i].level, self.scopes[i].start));
        for i in 0..plan.len() {
            if plan[i].id >= 0 && plan[i].parent == 0 {
                self.assign_plan(
                    i,
                    PlanContext {
                        scope: root,
                        blocking: false,
                    },
                    plan,
                    &mut result,
                );
            }
        }
        // Sorting/materializing a query's output must finish its reads before
        // an outer LIMIT can stop it. A local explicit LIMIT keeps its rule.
        for (i, row) in plan.iter().enumerate() {
            if row.detail.contains("TEMP B-TREE")
                && let Some(scope) = result[i].scope
            {
                let blocked = if row.detail.starts_with("USE TEMP B-TREE") {
                    vec![scope]
                } else {
                    self.group(scope)
                };
                for context in &mut result {
                    if context.scope.is_some_and(|s| blocked.contains(&s)) {
                        context.blocking = true;
                    }
                }
            }
        }
        result
    }

    fn plan_barriers(&self, plan: &[PlanStep]) -> Vec<Vec<usize>> {
        plan.iter()
            .filter(|row| {
                row.detail.starts_with("MATERIALIZE ")
                    || row.detail.starts_with("CO-ROUTINE ")
                    || row.detail.contains("SUBQUERY ")
            })
            .filter_map(|row| self.subquery_scope(&row.detail))
            .map(|scope| self.group(scope))
            .collect()
    }

    fn read_in_context(
        &self,
        read: &EventRead,
        context: PlanContext,
        barriers: &[Vec<usize>],
    ) -> bool {
        let Some(scope) = context.scope else {
            return true;
        };
        self.visible_from(read.scope, scope)
            && !barriers.iter().any(|group| {
                group
                    .iter()
                    .any(|&inner| self.visible_from(read.scope, inner))
                    && !group.iter().any(|&inner| self.visible_from(scope, inner))
            })
    }

    fn strip_parens(&self, mut start: usize, mut end: usize) -> (usize, usize) {
        while start < end && self.raw[start] == "(" && self.pairs.get(&start) == Some(&(end - 1)) {
            start += 1;
            end -= 1;
        }
        (start, end)
    }

    fn is_id(&self, names: &[String], ids: &Columns, start: usize, end: usize) -> bool {
        self.column_ref(start, end)
            .is_some_and(|(qualifier, column)| {
                ids.contains(column) && qualifies(qualifier.as_deref(), names)
            })
    }

    /// Boolean structure, rather than the tokens of a bound expression,
    /// determines whether there is an additional predicate. Subqueries,
    /// functions and arithmetic inside a range boundary are opaque values.
    fn id_predicates(
        &self,
        names: &[String],
        ids: &Columns,
        start: usize,
        end: usize,
    ) -> Vec<IdPredicate> {
        let (start, end) = self.strip_parens(start, end);
        let mut i = start;
        let mut case = 0usize;
        let mut between = false;
        while i < end {
            if self.raw[i] == "("
                && let Some(&close) = self.pairs.get(&i)
            {
                i = close + 1;
                continue;
            }
            if self.keyword(i, "case") {
                case += 1;
            }
            if self.keyword(i, "end") {
                case = case.saturating_sub(1);
            }
            if case == 0 {
                if self.keyword(i, "between") {
                    between = true;
                }
                if self.keyword(i, "and") && between {
                    between = false;
                } else if self.keyword(i, "and") || self.keyword(i, "or") {
                    let mut predicates = self.id_predicates(names, ids, start, i);
                    predicates.extend(self.id_predicates(names, ids, i + 1, end));
                    return predicates;
                }
            }
            i += 1;
        }
        i = start;
        while i < end {
            if self.raw[i] == "("
                && let Some(&close) = self.pairs.get(&i)
            {
                i = close + 1;
                continue;
            }
            let range = matches!(self.raw[i].as_str(), ">" | "<") || self.keyword(i, "between");
            let comparison =
                range || self.raw[i] == "=" || self.keyword(i, "in") || self.keyword(i, "is");
            if comparison {
                let mut after = i + 1;
                if self.raw.get(after).is_some_and(|s| s == "=") {
                    after += 1;
                }
                let id = self.is_id(names, ids, start, i)
                    || (range && self.is_id(names, ids, after, end));
                return vec![if !id {
                    IdPredicate::Other
                } else if range {
                    IdPredicate::Range
                } else {
                    IdPredicate::Point
                }];
            }
            i += 1;
        }
        vec![IdPredicate::Other]
    }

    /// The alias, with or without AS, that may follow a table or subquery
    /// whose last token comes before `at`.
    fn alias_at(&self, at: usize) -> Option<String> {
        let explicit = self.keyword(at, "as");
        let at = if explicit { at + 1 } else { at };
        let token = self.raw.get(at)?;
        (explicit
            || token.starts_with(['"', '`', '['])
            || (token
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
                && !NOT_ALIASES.contains(&self.words[at].as_str())))
        .then(|| self.words[at].clone())
    }

    fn derived_alias(&self, scope: usize) -> Option<String> {
        let mut next = self.scopes[scope].query_end + 1;
        while self.raw.get(next).is_some_and(|s| s == ")") {
            next += 1;
        }
        self.alias_at(next)
    }

    fn source_alias(&self, child: usize, parent: usize) -> Option<String> {
        let Some(name) = self.cte_name(child) else {
            return self.derived_alias(child);
        };
        let table = (0..self.raw.len()).find(|&i| {
            self.words[i] == name && self.owner(i) == Some(parent) && self.table_reference(i)
        })?;
        Some(self.alias_at(table + 1).unwrap_or(name))
    }

    fn id_range_only(&self, read: &EventRead, context: PlanContext) -> bool {
        if self.bounded(read.scope, context) {
            return false;
        }
        let mut scope_id = read.scope;
        let mut names = read.names.clone();
        let mut ids = column_set(&["id", "rowid"]);
        let mut predicates = Vec::new();
        let target = context.scope.filter(|&target| target != read.scope);
        let mut visited = HashSet::new();
        while visited.insert(scope_id) {
            let scope = &self.scopes[scope_id];
            if let Some(start) = (scope.start..scope.end)
                .find(|&i| self.levels[i] == scope.level && self.keyword(i, "where"))
            {
                let end = (start + 1..scope.end)
                    .find(|&i| {
                        self.levels[i] == scope.level
                            && ["order", "group", "having", "window", "limit"]
                                .iter()
                                .any(|k| self.keyword(i, k))
                    })
                    .unwrap_or(scope.end);
                predicates.extend(self.id_predicates(&names, &ids, start + 1, end));
            }
            if context.blocking {
                break;
            }
            let Some(parent) = self.parent_on_path(scope_id, target) else {
                break;
            };
            ids = self
                .output_columns(scope_id, &names, &ids)
                .unwrap_or_default();
            names = self.source_alias(scope_id, parent).into_iter().collect();
            scope_id = parent;
        }
        predicates.is_empty()
            || (predicates.contains(&IdPredicate::Range)
                && !predicates.contains(&IdPredicate::Other))
    }
}

/// Classify each plan read against its SELECT branch and table reference.
fn violation(sql: &str, plan: &[PlanStep]) -> Option<&'static str> {
    let parsed = ReadSql::new(sql);
    let contexts = parsed.plan_contexts(plan);
    let barriers = parsed.plan_barriers(plan);
    for (step, row) in plan.iter().enumerate() {
        let detail = &row.detail;
        let p: Vec<_> = detail.split_whitespace().collect();
        let Some((operation, table_read)) = detail.split_once(' ') else {
            continue;
        };
        if !matches!(operation, "SCAN" | "SEARCH") {
            continue;
        }
        let table_read = table_read.to_ascii_lowercase();
        for read in &parsed.reads {
            if !parsed.read_in_context(read, contexts[step], &barriers) {
                continue;
            }
            if !(table_read == read.plan_name
                || table_read
                    .strip_prefix(&read.plan_name)
                    .is_some_and(|rest| rest.starts_with(' ')))
            {
                continue;
            }
            if operation == "SCAN" {
                return Some("SCAN of run_events");
            }
            for (column, index) in [
                ("run_id", "events_by_run"),
                ("task_id", "events_by_task"),
                ("goal_id", "events_by_goal"),
            ] {
                if p.contains(&index) && parsed.null_can_select(read, column, index) {
                    return Some("IS NULL selects a subject index");
                }
            }
            if detail.contains("INTEGER PRIMARY KEY")
                && (detail.contains("rowid>") || detail.contains("rowid<"))
                && parsed.id_range_only(read, contexts[step])
            {
                return Some("id range without LIMIT or another predicate");
            }
        }
    }
    None
}

#[derive(Default)]
struct Exception {
    hash: String,
    head: String,
    reason: String,
}
#[derive(Default)]
struct Registry {
    budget: usize,
    entries: Vec<Exception>,
}

impl Registry {
    /// A deliberately small TOML subset: decimal budget, [[exception]], and
    /// double-quoted JSON-compatible strings. Unknown/duplicate fields fail.
    fn append(&mut self, text: &str, base: bool) -> Result<(), String> {
        let mut entry: Option<Exception> = None;
        let mut fields = HashSet::new();
        for line in text
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty() && !s.starts_with('#'))
        {
            if line == "[[exception]]" {
                if let Some(e) = entry.take() {
                    self.entries.push(e);
                }
                entry = Some(Exception::default());
                fields.clear();
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| format!("invalid registry line: {line}"))?;
            let key = key.trim();
            if !fields.insert(key.to_owned()) {
                return Err(format!("duplicate {key}"));
            }
            if entry.is_none() && base && key == "budget" {
                self.budget = value.trim().parse().map_err(|_| "invalid budget")?;
                continue;
            }
            let e = entry.as_mut().ok_or("expected [[exception]]")?;
            let value: String = serde_json::from_str(value.trim()).map_err(|e| e.to_string())?;
            match key {
                "hash" => e.hash = value,
                "head" => e.head = value,
                "reason" => e.reason = value,
                _ => return Err(format!("unknown {key}")),
            }
        }
        if let Some(e) = entry {
            self.entries.push(e);
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        if self.entries.len() > self.budget {
            return Err(format!(
                "{} exceptions exceed budget {}",
                self.entries.len(),
                self.budget
            ));
        }
        let mut hashes = HashSet::new();
        for e in &self.entries {
            if e.reason.trim().is_empty()
                || e.head.trim().is_empty()
                || e.hash.len() != 64
                || !e.hash.bytes().all(|b| b.is_ascii_hexdigit())
                || !hashes.insert(&e.hash)
            {
                return Err("invalid exception: unique SHA-256, head and reason required".into());
            }
        }
        Ok(())
    }

    fn error(&self, sql: &str, plan: &[PlanStep]) -> Option<String> {
        let reason = violation(sql, plan)?;
        let hash = hash(sql);
        if self.entries.iter().any(|e| e.hash == hash) {
            return None;
        }
        Some(format!(
            "run_events read plan violation: {reason}\nSQL: {sql}\nplan: {plan:#?}\nexception identity: {}",
            serde_json::json!({"hash": hash, "head": normalized(sql).chars().take(160).collect::<String>(), "reason": reason})
        ))
    }

    fn check(&self, sql: &str, plan: &[PlanStep]) {
        if let Some(error) = self.error(sql, plan) {
            panic!("{error}");
        }
    }
}

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".config");
        let mut r = Registry::default();
        r.append(
            &std::fs::read_to_string(root.join("run-events-read-plans.toml")).unwrap(),
            true,
        )
        .unwrap();
        let dir = root.join("run-events-read-plans.d");
        if dir.exists() {
            let mut paths: Vec<_> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|e| e == "toml"))
                .collect();
            paths.sort();
            for path in paths {
                r.append(&std::fs::read_to_string(path).unwrap(), false)
                    .unwrap();
            }
        }
        r.validate().unwrap();
        r
    })
}

/// Check eligible schemas before consulting the process-wide statement set:
/// hand-built tables and ANALYZE fixtures must never suppress a later check.
fn eligible(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='events_by_kind') AND NOT EXISTS(SELECT 1 FROM sqlite_master WHERE name='sqlite_stat1')", [], |r| r.get(0))
}

pub(super) fn check(conn: &Connection, sql: &str) -> rusqlite::Result<()> {
    if !tokens(sql).iter().any(|t| identifier(t) == "run_events")
        || sql.trim_start().to_ascii_uppercase().starts_with("EXPLAIN")
        || !eligible(conn)?
    {
        return Ok(());
    }
    let registry = registry();
    static CHECKED: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    // Different IN arities can choose different plans even though their
    // exception identity is shared, so memoize the exact statement.
    let key = sql.to_owned();
    // Serialize first checks as well as insertions, so concurrent reads do
    // not explain the same statement twice. Never panic with this lock held.
    let mut checked = CHECKED.get_or_init(Default::default).lock().unwrap();
    if let Some(error) = checked.get(&key).cloned() {
        drop(checked);
        if let Some(error) = error {
            panic!("{error}");
        }
        return Ok(());
    }
    let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
    let nulls = vec![rusqlite::types::Null; stmt.parameter_count()];
    let plan = stmt
        .query_map(rusqlite::params_from_iter(nulls), PlanStep::read)?
        .collect::<rusqlite::Result<Vec<PlanStep>>>()?;
    let error = registry.error(sql, &plan);
    checked.insert(key, error.clone());
    drop(checked);
    if let Some(error) = error {
        panic!("{error}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_event_read_plans() {
        for (sql, plan, bad) in [
            ("SELECT * FROM run_events", "SCAN run_events", true),
            (
                "SELECT * FROM run_events AS \"event rows\"",
                "SCAN event rows",
                true,
            ),
            (
                "SELECT * FROM run_events AS e",
                "SCAN e USING INDEX events_by_kind",
                true,
            ),
            (
                "SELECT * FROM run_events WHERE run_id IS NULL",
                "SEARCH run_events USING INDEX events_by_run (run_id=?)",
                true,
            ),
            (
                "SELECT * FROM run_events e WHERE e.task_id IS NULL",
                "SEARCH e USING INDEX events_by_task (task_id=?)",
                true,
            ),
            (
                "SELECT * FROM run_events WHERE goal_id IS NULL",
                "SEARCH run_events USING INDEX events_by_goal (goal_id=?)",
                true,
            ),
            (
                "SELECT * FROM run_events INDEXED BY events_by_kind WHERE kind=?",
                "SEARCH run_events USING INDEX events_by_kind (kind=?)",
                false,
            ),
            (
                "SELECT * FROM run_events WHERE task_id=?",
                "SEARCH run_events USING INDEX events_by_task (task_id=?)",
                false,
            ),
            (
                "SELECT * FROM run_events WHERE id>? LIMIT ?",
                "SEARCH run_events USING INTEGER PRIMARY KEY (rowid>?)",
                false,
            ),
            (
                "SELECT * FROM run_events WHERE id>?",
                "SEARCH run_events USING INTEGER PRIMARY KEY (rowid>?)",
                true,
            ),
            (
                "SELECT * FROM run_events e WHERE e.id BETWEEN ? AND ? ORDER BY id",
                "SEARCH e USING INTEGER PRIMARY KEY (rowid>? AND rowid<?)",
                true,
            ),
            (
                "SELECT * FROM run_events WHERE id>? AND kind=?",
                "SEARCH run_events USING INTEGER PRIMARY KEY (rowid>?)",
                false,
            ),
            (
                "SELECT * FROM tasks t JOIN run_events e ON e.task_id=t.id",
                "SCAN t",
                false,
            ),
        ] {
            assert_eq!(violation(sql, &[plan.into()]).is_some(), bad, "{sql}");
        }
        for predicate in ["task_id", "+task_id"] {
            let sql = format!(
                "SELECT * FROM run_events WHERE task_id=? UNION SELECT * FROM run_events INDEXED BY events_by_kind WHERE {predicate} IS NULL AND kind=?"
            );
            assert_eq!(
                violation(
                    &sql,
                    &[
                        "SEARCH run_events USING INDEX events_by_task (task_id=?)".into(),
                        "SEARCH run_events USING INDEX events_by_kind (kind=?)".into()
                    ]
                ),
                None
            );
        }
        assert_eq!(
            violation(
                "SELECT * FROM run_events WHERE +task_id IS NULL",
                &["SEARCH run_events USING INDEX events_by_kind (kind=?)".into()]
            ),
            None
        );
    }

    fn indexed_schema() -> Connection {
        let conn = schema();
        conn.execute_batch(
            "ALTER TABLE run_events ADD COLUMN run_id TEXT;
            ALTER TABLE run_events ADD COLUMN task_id INTEGER;
            ALTER TABLE run_events ADD COLUMN goal_id INTEGER;
            CREATE INDEX events_by_run ON run_events(run_id, id);
            CREATE INDEX events_by_task ON run_events(task_id, id);
            CREATE INDEX events_by_goal ON run_events(goal_id, id);
            CREATE TABLE tasks(id INTEGER PRIMARY KEY, goal_id INTEGER);",
        )
        .unwrap();
        conn
    }

    fn actual_plan(conn: &Connection, sql: &str) -> Vec<PlanStep> {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let params = vec![rusqlite::types::Null; statement.parameter_count()];
        statement
            .query_map(rusqlite::params_from_iter(params), PlanStep::read)
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn id_ranges_use_their_own_select_predicates_and_limit() {
        let conn = indexed_schema();
        for (sql, expected) in [
            (
                "SELECT id FROM run_events WHERE id>? UNION ALL SELECT id FROM run_events WHERE task_id=?",
                Some("id range without LIMIT or another predicate"),
            ),
            (
                "SELECT id FROM run_events WHERE task_id=? UNION ALL SELECT id FROM run_events WHERE id>?",
                Some("id range without LIMIT or another predicate"),
            ),
            (
                "SELECT (SELECT 1 LIMIT 1), id FROM run_events WHERE id>?",
                Some("id range without LIMIT or another predicate"),
            ),
            (
                "SELECT (SELECT 1 LIMIT 1), id FROM run_events WHERE id>? LIMIT 2",
                None,
            ),
            (
                "SELECT id FROM run_events WHERE id>? UNION ALL SELECT id FROM run_events WHERE task_id=? LIMIT 2",
                None,
            ),
            ("SELECT id FROM run_events WHERE id>? AND task_id=?", None),
            (
                "SELECT id FROM run_events WHERE id=? UNION ALL SELECT id FROM run_events NOT INDEXED WHERE id>? AND kind=?",
                None,
            ),
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(violation(sql, &plan), expected, "{sql}\n{plan:#?}");
        }
    }

    #[test]
    fn id_range_boundaries_are_values_not_additional_predicates() {
        let conn = indexed_schema();
        for sql in [
            "SELECT id FROM run_events WHERE id>(SELECT 1)",
            "SELECT id FROM run_events WHERE (id > (SELECT 1 LIMIT 1))",
            "SELECT id FROM run_events WHERE id > abs(?) + 1",
            "SELECT id FROM run_events WHERE id > CAST(? AS INTEGER)",
            "SELECT id FROM run_events WHERE id > CASE WHEN ? AND ? THEN 1 ELSE 2 END",
            "SELECT id FROM run_events WHERE id BETWEEN (SELECT 1) AND abs(?) + 2",
            "SELECT id FROM run_events WHERE (SELECT 1) < id",
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(
                violation(sql, &plan),
                Some("id range without LIMIT or another predicate"),
                "{sql}\n{plan:#?}"
            );
        }
        let sql = "SELECT id FROM run_events NOT INDEXED WHERE id>(SELECT 1) AND kind=?";
        let plan = actual_plan(&conn, sql);
        assert_eq!(violation(sql, &plan), None, "{sql}\n{plan:#?}");
    }

    #[test]
    fn plan_scopes_distinguish_reused_aliases_on_other_tables() {
        let conn = indexed_schema();
        for sql in [
            "SELECT e.id FROM run_events e WHERE task_id=? UNION ALL SELECT e.id FROM tasks e",
            "SELECT e.id FROM tasks e UNION ALL SELECT e.id FROM run_events e WHERE task_id=?",
            "SELECT e.id FROM run_events e WHERE task_id=? UNION SELECT e.id FROM tasks e ORDER BY id",
            "SELECT e.id FROM tasks e WHERE id=(SELECT e.id FROM run_events e WHERE task_id=? LIMIT 1)",
            "SELECT e.id FROM run_events e WHERE task_id=(SELECT e.id FROM tasks e LIMIT 1)",
            "WITH p(x) AS (VALUES(1),(2)) SELECT e.id FROM run_events e WHERE task_id=(SELECT e.id FROM tasks e LIMIT 1)",
            "WITH q AS MATERIALIZED (SELECT q.id FROM run_events q WHERE task_id=?) SELECT q.id FROM q",
            "SELECT e.id FROM (SELECT DISTINCT e.id FROM run_events e WHERE task_id=?) e",
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(violation(sql, &plan), None, "{sql}\n{plan:#?}");
        }
        for sql in [
            "SELECT e.id FROM tasks e UNION ALL SELECT e.id FROM run_events e",
            "SELECT e.id FROM tasks e WHERE id=(SELECT e.id FROM run_events e LIMIT 1)",
            "WITH q AS MATERIALIZED (SELECT q.id FROM run_events q) SELECT q.id FROM q",
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(
                violation(sql, &plan),
                Some("SCAN of run_events"),
                "{sql}\n{plan:#?}"
            );
        }
    }

    #[test]
    fn outer_limits_bound_from_reads_but_not_scalar_or_materialized_reads() {
        let conn = indexed_schema();
        for sql in [
            "SELECT id FROM (SELECT id FROM run_events WHERE id>?) LIMIT 2",
            "SELECT id FROM (SELECT DISTINCT id FROM run_events WHERE id>?) LIMIT 2",
            "SELECT id FROM (SELECT id FROM (SELECT id FROM run_events WHERE id>?)) LIMIT 2",
            "SELECT d.id FROM (SELECT id FROM run_events WHERE id>?) d LIMIT 2",
            "SELECT d.id FROM (SELECT id,kind FROM run_events WHERE id>?) d WHERE d.kind=?",
            "WITH q AS (SELECT id FROM run_events WHERE id>?) SELECT id FROM q LIMIT 2",
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(violation(sql, &plan), None, "{sql}\n{plan:#?}");
        }
        for sql in [
            "SELECT (SELECT 1 LIMIT 1), id FROM run_events WHERE id>?",
            "WITH q AS (SELECT id FROM run_events WHERE id>?) SELECT id FROM q",
            "WITH q AS (SELECT id FROM run_events) SELECT q.id FROM q WHERE q.id>(SELECT 1)",
            "WITH q AS (SELECT id FROM run_events) SELECT d.id FROM q AS d WHERE d.id>(SELECT 1)",
            "SELECT (SELECT id FROM run_events WHERE id>?) FROM tasks LIMIT 2",
            "SELECT id FROM (SELECT id FROM run_events NOT INDEXED WHERE id>? ORDER BY kind) LIMIT 2",
            "WITH q AS MATERIALIZED (SELECT id FROM run_events WHERE id>?) SELECT id FROM q LIMIT 2",
            "WITH q(id) AS MATERIALIZED (SELECT id FROM run_events WHERE id>?) SELECT id FROM q LIMIT 2",
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(
                violation(sql, &plan),
                Some("id range without LIMIT or another predicate"),
                "{sql}\n{plan:#?}"
            );
        }
    }

    #[test]
    fn parenthesized_event_tables_keep_their_aliases() {
        let conn = indexed_schema();
        for sql in [
            "SELECT id FROM (run_events AS e)",
            "SELECT id FROM ((run_events)) AS e",
            "SELECT id FROM (SELECT id FROM run_events)",
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(
                violation(sql, &plan),
                Some("SCAN of run_events"),
                "{sql}\n{plan:#?}"
            );
        }
    }

    #[test]
    fn parenthesized_null_operands_select_subject_indexes() {
        let conn = indexed_schema();
        for sql in [
            "SELECT id FROM run_events WHERE (task_id) IS NULL",
            "SELECT e.id FROM run_events e WHERE ((e.goal_id)) IS NULL",
            "SELECT id FROM (run_events AS e) WHERE ((e.run_id)) IS NULL",
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(
                violation(sql, &plan),
                Some("IS NULL selects a subject index"),
                "{sql}\n{plan:#?}"
            );
        }
    }

    #[test]
    fn schema_qualified_event_tables_are_named_as_their_plans_name_them() {
        let conn = indexed_schema();
        for (sql, expected) in [
            ("SELECT id FROM main.run_events", "SCAN of run_events"),
            ("SELECT id FROM \"main\".run_events", "SCAN of run_events"),
            (
                "SELECT id FROM main.run_events WHERE task_id IS NULL",
                "IS NULL selects a subject index",
            ),
            (
                "SELECT id FROM main.run_events WHERE main.run_events.goal_id IS NULL",
                "IS NULL selects a subject index",
            ),
            (
                "SELECT id FROM main.run_events WHERE run_events.run_id IS NULL",
                "IS NULL selects a subject index",
            ),
            (
                "SELECT id FROM main.run_events WHERE id>?",
                "id range without LIMIT or another predicate",
            ),
            (
                "SELECT id FROM main.run_events WHERE main.run_events.id>?",
                "id range without LIMIT or another predicate",
            ),
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(violation(sql, &plan), Some(expected), "{sql}\n{plan:#?}");
        }
        for sql in [
            "SELECT id FROM main.run_events WHERE task_id=?",
            "SELECT id FROM main.run_events WHERE id>? LIMIT 2",
            "SELECT e.id FROM main.run_events e WHERE e.task_id=?",
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(violation(sql, &plan), None, "{sql}\n{plan:#?}");
        }
    }

    #[test]
    fn outer_null_tests_reach_flattened_and_pushed_down_reads() {
        let conn = indexed_schema();
        for sql in [
            "SELECT e.id FROM (SELECT * FROM run_events) e WHERE e.task_id IS NULL",
            "WITH q AS (SELECT * FROM run_events) SELECT id FROM q WHERE task_id IS NULL",
            "WITH q AS (SELECT * FROM run_events) SELECT d.id FROM q d WHERE d.task_id ISNULL",
            "SELECT t FROM (SELECT task_id AS t FROM run_events) WHERE t IS NULL",
            "SELECT t FROM (SELECT e.task_id t FROM run_events e) WHERE (t) IS NULL",
            "WITH q(a, b) AS (SELECT id, task_id FROM run_events) SELECT a FROM q WHERE b IS NULL",
            "SELECT id FROM (SELECT * FROM (SELECT * FROM run_events)) WHERE goal_id IS NULL",
            "SELECT x FROM (SELECT id AS x, goal_id AS g FROM tasks UNION ALL SELECT id, goal_id FROM run_events) WHERE g IS NULL",
            "SELECT k FROM (SELECT task_id AS k, count(*) AS c FROM run_events GROUP BY task_id) WHERE k IS NULL",
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(
                violation(sql, &plan),
                Some("IS NULL selects a subject index"),
                "{sql}\n{plan:#?}"
            );
        }
        for sql in [
            // A LIMIT keeps the outer test out of the subquery.
            "SELECT id FROM (SELECT * FROM run_events WHERE task_id=? LIMIT 5) WHERE task_id IS NULL",
            "SELECT d.id FROM (SELECT id, task_id FROM run_events WHERE task_id=?) d WHERE d.id IS NULL",
            "SELECT d.id FROM (SELECT id, kind AS task_id FROM run_events WHERE task_id=?) d WHERE d.task_id IS NULL",
            "SELECT d.id FROM (SELECT e.id FROM run_events e WHERE e.task_id=?) d JOIN tasks t ON t.id=d.id WHERE t.goal_id IS NULL",
            "SELECT n FROM (SELECT count(*) AS n FROM run_events WHERE task_id=?) WHERE n IS NULL",
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(violation(sql, &plan), None, "{sql}\n{plan:#?}");
        }
    }

    #[test]
    fn joins_after_unaliased_sources_are_not_aliases() {
        let conn = indexed_schema();
        for join in ["LEFT", "INNER", "CROSS", "NATURAL", "LEFT OUTER"] {
            let on = if join == "NATURAL" || join == "CROSS" {
                ""
            } else {
                " ON t.id = q.task_id"
            };
            for (sql, marker, expected) in [
                (
                    format!(
                        "WITH q AS (SELECT * FROM run_events) SELECT q.id FROM q {join} JOIN tasks t{on} WHERE q.task_id IS NULL"
                    ),
                    "events_by_task",
                    "IS NULL selects a subject index",
                ),
                (
                    format!(
                        "SELECT q.id FROM (SELECT * FROM run_events) q {join} JOIN tasks t{on} WHERE q.task_id IS NULL"
                    ),
                    "events_by_task",
                    "IS NULL selects a subject index",
                ),
                (
                    format!(
                        "WITH q AS (SELECT id, task_id FROM run_events) SELECT q.id FROM q {join} JOIN tasks t{on} WHERE q.id > ?"
                    ),
                    "rowid>",
                    "id range without LIMIT or another predicate",
                ),
            ] {
                let plan = actual_plan(&conn, &sql);
                let events: Vec<_> = plan
                    .iter()
                    .filter(|row| row.detail.split_whitespace().nth(1) == Some("run_events"))
                    .collect();
                // A join SQLite may reorder can drive the events another way;
                // LEFT and CROSS keep them first.
                let want = if events.iter().any(|row| row.detail.starts_with("SCAN")) {
                    Some("SCAN of run_events")
                } else if events.iter().any(|row| row.detail.contains(marker)) {
                    Some(expected)
                } else {
                    None
                };
                if join.starts_with("LEFT") || join == "CROSS" {
                    assert_eq!(want, Some(expected), "{sql}\n{plan:#?}");
                }
                assert_eq!(violation(&sql, &plan), want, "{sql}\n{plan:#?}");
            }
        }
    }

    #[test]
    fn unqualified_reads_accept_main_qualified_columns() {
        let conn = indexed_schema();
        for sql in [
            "SELECT id FROM run_events WHERE main.run_events.task_id IS NULL",
            "SELECT id FROM (run_events) WHERE ((main.run_events.goal_id)) IS NULL",
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(
                violation(sql, &plan),
                Some("IS NULL selects a subject index"),
                "{sql}\n{plan:#?}"
            );
        }
        let sql = "SELECT id FROM run_events WHERE main.run_events.id > ?";
        let plan = actual_plan(&conn, sql);
        assert_eq!(
            violation(sql, &plan),
            Some("id range without LIMIT or another predicate"),
            "{sql}\n{plan:#?}"
        );
    }

    #[test]
    fn null_tests_stay_out_of_sources_sqlite_keeps_apart() {
        let conn = indexed_schema();
        for sql in [
            // A star in a compound arm keeps the labels the arm gives.
            "SELECT id FROM (SELECT * FROM run_events WHERE task_id = ? UNION ALL SELECT * FROM run_events WHERE goal_id = ?) WHERE kind IS NULL",
            "SELECT id FROM (SELECT * FROM tasks UNION ALL SELECT id, goal_id FROM run_events WHERE goal_id = ?) WHERE id IS NULL",
            "WITH q AS MATERIALIZED (SELECT * FROM run_events WHERE task_id = ? OR goal_id = ?) SELECT id FROM q WHERE task_id IS NULL",
            "WITH q AS (SELECT * FROM run_events WHERE task_id = ?) SELECT a.id FROM q a JOIN q b ON b.id = a.id WHERE a.task_id IS NULL",
            "WITH RECURSIVE r(id, task_id) AS (SELECT id, task_id FROM run_events WHERE task_id = ? UNION ALL SELECT e.id, e.task_id FROM run_events e JOIN r ON e.id = r.id + 1 WHERE e.task_id = ?) SELECT id FROM r WHERE task_id IS NULL",
            // A subquery after a comma of the result list is a value.
            "SELECT t.id, (SELECT e.goal_id FROM run_events e WHERE e.goal_id = t.goal_id) FROM tasks t WHERE goal_id IS NULL",
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(violation(sql, &plan), None, "{sql}\n{plan:#?}");
        }
        for sql in [
            "WITH q AS NOT MATERIALIZED (SELECT * FROM run_events) SELECT id FROM q WHERE task_id IS NULL",
            "SELECT x FROM (SELECT 0 g, id AS x FROM tasks UNION ALL SELECT goal_id, id FROM run_events) WHERE g IS NULL",
            "SELECT x FROM (SELECT count(*) g, 1 x FROM tasks UNION ALL SELECT goal_id, id FROM run_events) WHERE g IS NULL",
            "WITH q(a, b, c, d, e) AS (SELECT * FROM run_events) SELECT a FROM q WHERE e IS NULL",
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(
                violation(sql, &plan),
                Some("IS NULL selects a subject index"),
                "{sql}\n{plan:#?}"
            );
        }
    }

    #[test]
    fn renamed_ids_keep_their_range_checks() {
        let conn = indexed_schema();
        let sql = "SELECT x FROM (SELECT id AS x FROM run_events) WHERE x>?";
        let plan = actual_plan(&conn, sql);
        assert_eq!(
            violation(sql, &plan),
            Some("id range without LIMIT or another predicate"),
            "{sql}\n{plan:#?}"
        );
        let sql =
            "SELECT x FROM (SELECT id AS x, kind FROM run_events NOT INDEXED) WHERE x>? AND kind=?";
        let plan = actual_plan(&conn, sql);
        assert_eq!(violation(sql, &plan), None, "{sql}\n{plan:#?}");
    }

    #[test]
    fn null_predicates_on_other_tables_do_not_select_event_indexes() {
        let conn = indexed_schema();
        for sql in [
            "SELECT e.id FROM run_events AS e INDEXED BY events_by_goal JOIN tasks AS t ON t.id=e.task_id WHERE e.goal_id=? AND t.goal_id IS NULL",
            "SELECT e.id FROM run_events AS e INDEXED BY events_by_goal JOIN tasks AS t ON t.id=e.task_id WHERE e.goal_id=? AND ((t.goal_id)) IS NULL",
            "SELECT e.goal_id IS NULL FROM run_events AS e WHERE e.goal_id=?",
            "SELECT e.id FROM run_events AS e INDEXED BY events_by_kind WHERE kind=? AND +(task_id) IS NULL",
            "SELECT e.id FROM run_events AS e INDEXED BY events_by_kind WHERE kind=? AND abs(task_id) IS NULL",
        ] {
            let plan = actual_plan(&conn, sql);
            assert_eq!(violation(sql, &plan), None, "{sql}\n{plan:#?}");
        }
    }

    #[test]
    fn unlisted_read_panics_with_sql_and_plan() {
        let conn = schema();
        let sql = "SELECT id FROM run_events AS unlisted_guard_fixture";
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::infrastructure::sql_exec::SqlRead::visit_rows(&conn, sql, &[], &mut |_| {
                Ok(true)
            })
            .unwrap();
        }))
        .unwrap_err();
        let text = panic.downcast_ref::<String>().unwrap();
        assert!(text.contains(sql), "{text}");
        assert!(text.contains("SCAN unlisted_guard_fixture"), "{text}");
    }

    #[test]
    fn exception_identity_expires_on_sql_change() {
        let sql = "SELECT id FROM run_events WHERE id IN (?,?)";
        let r = Registry {
            budget: 1,
            entries: vec![Exception {
                hash: hash(sql),
                head: sql.into(),
                reason: "fixture".into(),
            }],
        };
        r.validate().unwrap();
        r.check(sql, &["SCAN run_events".into()]);
        assert_eq!(
            hash(sql),
            hash(" SELECT  id FROM run_events WHERE id IN (?1, ?2, ?3) ")
        );
        assert_ne!(
            hash(sql),
            hash("SELECT kind FROM run_events WHERE id IN (?,?)")
        );
        assert!(
            std::panic::catch_unwind(|| r.check(
                "SELECT kind FROM run_events WHERE id IN (?,?)",
                &["SCAN run_events".into()]
            ))
            .is_err()
        );
        assert_ne!(hash("SELECT 'a b'"), hash("SELECT 'a  b'"));
    }

    #[test]
    fn registry_requires_reasons_and_enforces_budget() {
        let mut r = Registry::default();
        r.append("budget = 1", true).unwrap();
        r.append(
            &format!("[[exception]]\nhash = {:?}\nhead = \"SELECT\"", hash("sql")),
            false,
        )
        .unwrap();
        assert!(r.validate().is_err());
        r.entries[0].reason = "existing read".into();
        r.validate().unwrap();
        r.budget = 0;
        assert!(r.validate().unwrap_err().contains("budget"));
    }

    #[test]
    fn checked_in_registry_respects_budget() {
        registry().validate().unwrap();
    }

    fn schema() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE run_events(id INTEGER PRIMARY KEY, kind TEXT); CREATE INDEX events_by_kind ON run_events(kind);").unwrap();
        conn
    }

    #[test]
    fn memoizes_exact_statements_but_checks_each_in_arity() {
        let conn = schema();
        conn.execute_batch("ALTER TABLE run_events ADD COLUMN task_id INTEGER; CREATE INDEX events_by_task ON run_events(task_id);").unwrap();
        let sql = "SELECT id FROM run_events AS arity_guard_fixture WHERE task_id IN (?1, ?2)";
        check(&conn, sql).unwrap();
        conn.execute_batch("DROP INDEX events_by_task").unwrap();
        // Rechecking this exact statement would now see a SCAN. Its first
        // successful plan is deliberately retained for this process.
        check(&conn, sql).unwrap();
        let expanded =
            "SELECT id FROM run_events AS arity_guard_fixture WHERE task_id IN (?1, ?2, ?3)";
        assert_eq!(hash(sql), hash(expanded));
        // A different arity shares the exception identity, but must still
        // get its own plan; no exception covers this fixture's scan.
        let panic =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&conn, expanded)))
                .unwrap_err();
        assert!(
            panic
                .downcast_ref::<String>()
                .unwrap()
                .contains("SCAN arity_guard_fixture")
        );
    }

    #[test]
    fn skips_hand_built_and_statistics_fixtures() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE run_events(id INTEGER PRIMARY KEY, kind TEXT)")
            .unwrap();
        let sql = "SELECT id FROM run_events AS skipped_guard_fixture";
        assert!(!eligible(&conn).unwrap());
        check(&conn, sql).unwrap();
        conn.execute_batch("CREATE INDEX events_by_kind ON run_events(kind); ANALYZE")
            .unwrap();
        assert!(!eligible(&conn).unwrap());
        check(&conn, sql).unwrap();
        let eligible_conn = schema();
        assert!(eligible(&eligible_conn).unwrap());
        // Skipping did not memoize the SQL and hide a later violation.
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&eligible_conn, sql)))
                .is_err()
        );
    }
}
