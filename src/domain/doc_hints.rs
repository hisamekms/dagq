//! The hints of `dagq lint` (ADR-t3321-1): for each task, the sections of
//! the repository's related documents (`[tasks] related_docs`) that name a
//! code identifier or file path the task names, while the task's
//! description and context name neither the document nor the section.
//!
//! A hint is a clue for the planner, not a rule: no task is refused for
//! one, and plan review's checks do not read them (ADR-0047 decision 10
//! holds). The documents to look in come from the repository's settings,
//! not from the runtime.

use std::collections::BTreeSet;

use serde::Serialize;

use super::{Task, TaskId};

/// At most this many sections are listed for one task; the rest are only
/// counted ([`DocHint::omitted`]).
pub const MAX_SECTIONS_PER_TASK: usize = 10;

/// One document a glob of `[tasks] related_docs` matched, as it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelatedDocument {
    /// The path from the repository root, with `/` between segments.
    pub path: String,
    /// The text, or why it could not be read.
    pub content: Result<String, String>,
}

/// The words of a task [`hints`] looks at.
#[derive(Debug, Clone, Copy)]
pub struct HintTask<'a> {
    pub id: TaskId,
    pub title: &'a str,
    pub description: &'a str,
    pub acceptance: &'a str,
    pub context: &'a str,
}

impl<'a> From<&'a Task> for HintTask<'a> {
    fn from(task: &'a Task) -> Self {
        Self {
            id: task.id(),
            title: task.title(),
            description: task.description(),
            acceptance: task.acceptance(),
            context: task.context(),
        }
    }
}

/// The hint of one task: the sections to look at, how many more there
/// were past [`MAX_SECTIONS_PER_TASK`], and the documents that could not
/// be read (looked at by no task).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocHint {
    pub task_id: TaskId,
    pub sections: Vec<SectionHint>,
    pub omitted: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unreadable: Vec<UnreadableDocument>,
}

/// A section of a related document and the task's words found in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SectionHint {
    pub path: String,
    /// The heading's text without its `#`s; empty for the text before the
    /// first heading.
    pub heading: String,
    /// The task's identifiers and paths the section names, sorted.
    pub terms: Vec<String>,
}

/// A related document that could not be read, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnreadableDocument {
    pub path: String,
    pub reason: String,
}

/// A section of a Markdown document: from a heading to the next heading of
/// any level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub heading: String,
    /// The section's lines, its heading among them.
    pub body: String,
}

/// The hints of `tasks` against `documents`, one per task that has a
/// section to look at or when a document could not be read, in the order
/// of `tasks`. No documents (no `[tasks] related_docs`) give no hints.
///
/// Sections come by the number of the task's terms they name, most first,
/// then by path and their order in the document. A document is left out
/// when the task's description or context names its path or its file
/// name, and a section when they name its heading (backquotes ignored).
pub fn hints(tasks: &[HintTask<'_>], documents: &[RelatedDocument]) -> Vec<DocHint> {
    let mut unreadable = Vec::new();
    let mut readable = Vec::new();
    for document in documents {
        match &document.content {
            Ok(text) => readable.push((document.path.as_str(), sections(text))),
            Err(reason) => unreadable.push(UnreadableDocument {
                path: document.path.clone(),
                reason: reason.clone(),
            }),
        }
    }
    let mut hints = Vec::new();
    for task in tasks {
        let terms = task_terms(task);
        let named = format!("{}\n{}", task.description, task.context).replace('`', "");
        let mut found: Vec<(usize, SectionHint)> = Vec::new();
        for (path, sections) in &readable {
            if names_document(&named, path) {
                continue;
            }
            for section in sections {
                if names_heading(&named, &section.heading) {
                    continue;
                }
                let hit: Vec<String> = terms
                    .iter()
                    .filter(|term| contains_term(&section.body, term))
                    .cloned()
                    .collect();
                if !hit.is_empty() {
                    found.push((
                        hit.len(),
                        SectionHint {
                            path: (*path).to_owned(),
                            heading: section.heading.clone(),
                            terms: hit,
                        },
                    ));
                }
            }
        }
        // Stable: equal counts keep the documents' and sections' order.
        found.sort_by_key(|(count, _)| std::cmp::Reverse(*count));
        let omitted = found.len().saturating_sub(MAX_SECTIONS_PER_TASK);
        let sections: Vec<SectionHint> = found
            .into_iter()
            .take(MAX_SECTIONS_PER_TASK)
            .map(|(_, section)| section)
            .collect();
        if !sections.is_empty() || !unreadable.is_empty() {
            hints.push(DocHint {
                task_id: task.id,
                sections,
                omitted,
                unreadable: unreadable.clone(),
            });
        }
    }
    hints
}

/// The terms of a task: the identifiers and paths of its title,
/// description, acceptance and context.
fn task_terms(task: &HintTask<'_>) -> BTreeSet<String> {
    [task.title, task.description, task.acceptance, task.context]
        .into_iter()
        .flat_map(identifiers)
        .collect()
}

/// The code identifiers and file paths `text` names. Inside backquotes
/// every token with a letter counts (`parse`, `--verify` as `verify`).
/// Outside them a token counts when it looks like code: it has `::` (a
/// Rust path), `_` (a snake_case function or key), a file extension (a
/// file path), or it is CamelCase (a type); a plain lowercase word
/// (`lint`, `submit`) is too common in prose to count there. A Rust
/// path's last segment is taken too when it looks like code on its own,
/// and a token shorter than 3 characters never counts.
pub fn identifiers(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    // The odd pieces between backquotes are code spans.
    for (index, piece) in text.split('`').enumerate() {
        let quoted = index % 2 == 1;
        for token in piece.split(|c: char| !is_token_char(c)) {
            let token = token.trim_matches(|c: char| matches!(c, '.' | ':' | '-' | '/'));
            let counts = if quoted {
                token.chars().count() >= 3 && token.chars().any(|c| c.is_ascii_alphabetic())
            } else {
                looks_like_code(token)
            };
            if counts {
                insert(&mut found, token);
            }
        }
    }
    found
}

/// Put `token` and, when it looks like code alone, a Rust path's last
/// segment into `found`.
fn insert(found: &mut BTreeSet<String>, token: &str) {
    found.insert(token.to_owned());
    if let Some((_, last)) = token.rsplit_once("::")
        && looks_like_code(last)
    {
        found.insert(last.to_owned());
    }
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '/' | '.' | '-')
}

fn looks_like_code(token: &str) -> bool {
    if token.chars().count() < 3 || !token.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    if token.contains("::") || token.contains('_') {
        return true;
    }
    if is_file_path(token) {
        return true;
    }
    // CamelCase: an upper-case start, then a lower-case letter and a later
    // upper-case one.
    let mut chars = token.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && token.chars().all(|c| c.is_ascii_alphanumeric())
        && token.char_indices().skip(1).any(|(i, c)| {
            c.is_ascii_lowercase() && token[i..].chars().any(|u| u.is_ascii_uppercase())
        })
}

/// A token whose last segment has a file extension: 1 to 5 alphanumerics
/// after a `.`, starting with a letter. Without a `/` the file name needs
/// at least 2 characters since the previous `.` before it, so an
/// abbreviation (`e.g.`, `U.S.`) is none; `src/a.rs` is one. A number
/// (`2.5GB`, `v1.2`) is none.
fn is_file_path(token: &str) -> bool {
    let (dir, name) = match token.rsplit_once('/') {
        Some((_, name)) => (true, name),
        None => (false, token),
    };
    let stem_len = if dir { 1 } else { 2 };
    match name.rsplit_once('.') {
        Some((stem, extension)) => {
            stem.rsplit('.')
                .next()
                .is_some_and(|last| last.len() >= stem_len)
                && (1..=5).contains(&extension.len())
                && extension.starts_with(|c: char| c.is_ascii_alphabetic())
                && extension.chars().all(|c| c.is_ascii_alphanumeric())
                && extension.chars().any(|c| c.is_ascii_alphabetic())
        }
        None => false,
    }
}

/// Whether `text` has `term` as a whole token: not right after or before
/// an ASCII alphanumeric or `_`. Other characters (Japanese prose right
/// against an unquoted path) end a token.
fn contains_term(text: &str, term: &str) -> bool {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    text.match_indices(term).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + term.len()..].chars().next();
        !before.is_some_and(is_word) && !after.is_some_and(is_word)
    })
}

/// Whether `named` names the document at `path`: its path or its file
/// name.
fn names_document(named: &str, path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    contains_term(named, path) || contains_term(named, file)
}

/// Whether `named` names a section by its heading, as a whole token
/// ([`contains_term`]: `Lint` is not named by `LintInput`).
fn names_heading(named: &str, heading: &str) -> bool {
    let heading = heading.replace('`', "");
    let heading = heading.trim();
    !heading.is_empty() && contains_term(named, heading)
}

/// The sections of the Markdown `text`: a YAML front matter at the top is
/// skipped, and a `#` line inside a fenced code block is no heading. The
/// text before the first heading is a section with an empty heading when
/// it has anything but blank lines.
pub fn sections(text: &str) -> Vec<Section> {
    let mut lines = text.lines().peekable();
    if lines.peek() == Some(&"---") {
        lines.next();
        for line in lines.by_ref() {
            if line == "---" {
                break;
            }
        }
    }
    let mut sections = Vec::new();
    let mut current = Section {
        heading: String::new(),
        body: String::new(),
    };
    let mut fence: Option<&str> = None;
    for line in lines {
        let trimmed = line.trim_start();
        match fence {
            Some(marker) => {
                if trimmed.starts_with(marker) {
                    fence = None;
                }
            }
            None if trimmed.starts_with("```") => fence = Some("```"),
            None if trimmed.starts_with("~~~") => fence = Some("~~~"),
            None => {
                if let Some(heading) = heading(line) {
                    let done = std::mem::replace(
                        &mut current,
                        Section {
                            heading,
                            body: String::new(),
                        },
                    );
                    if !done.heading.is_empty() || !done.body.trim().is_empty() {
                        sections.push(done);
                    }
                }
            }
        }
        current.body.push_str(line);
        current.body.push('\n');
    }
    if !current.heading.is_empty() || !current.body.trim().is_empty() {
        sections.push(current);
    }
    sections
}

/// The text of an ATX heading line (`#` to `######`, then a space).
fn heading(line: &str) -> Option<String> {
    let hashes = line.len() - line.trim_start_matches('#').len();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &line[hashes..];
    (rest.is_empty() || rest.starts_with([' ', '\t']))
        .then(|| rest.trim().trim_end_matches('#').trim().to_owned())
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task<'a>(id: i64, description: &'a str, context: &'a str) -> HintTask<'a> {
        HintTask {
            id: TaskId::new(id),
            title: "feat: x",
            description,
            acceptance: "",
            context,
        }
    }

    fn doc(path: &str, text: &str) -> RelatedDocument {
        RelatedDocument {
            path: path.into(),
            content: Ok(text.into()),
        }
    }

    fn set(words: &[&str]) -> BTreeSet<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn identifiers_take_code_like_tokens_and_leave_prose_words() {
        let text = "変える: `crate::domain::lint::LintInput` と `load_title_prefixes`、\
                    src/domain/lint.rs と dagq.toml の [tasks] related_docs。lint は submit を拒まない。\
                    PlanningSources を読む。`--verify` と `x_`。";
        assert_eq!(
            identifiers(text),
            set(&[
                "LintInput",
                "PlanningSources",
                "crate::domain::lint::LintInput",
                "dagq.toml",
                "load_title_prefixes",
                "related_docs",
                "src/domain/lint.rs",
                "verify",
            ])
        );
        // Inside backquotes a plain lowercase word counts, outside it
        // does not; a path with a directory may have a 1-letter stem.
        assert_eq!(identifiers("`parse` を変える。parse"), set(&["parse"]));
        assert_eq!(identifiers("src/a.rs を変える"), set(&["src/a.rs"]));
        // A Rust path's last segment is taken only when it counts alone.
        assert_eq!(
            identifiers("`crate::domain::lint`"),
            set(&["crate::domain::lint"])
        );
        // No letter, all upper-case, an abbreviation or a number is no
        // identifier.
        assert!(identifiers("1.2.3 ADR HTTP Plain e.g. i.e. U.S. 2.5GB v1.2").is_empty());
        assert_eq!(identifiers("a.test.ts"), set(&["a.test.ts"]));
    }

    #[test]
    fn sections_split_at_headings_outside_code_and_skip_front_matter() {
        let text = "---\ntitle: x\n---\n\nintro\n# Top\none\n```\n# not a heading\n```\n## Sub ##\ntwo\n#nospace\n";
        let found = sections(text);
        let headings: Vec<&str> = found.iter().map(|s| s.heading.as_str()).collect();
        assert_eq!(headings, ["", "Top", "Sub"]);
        assert_eq!(found[0].body, "\nintro\n");
        assert!(found[1].body.contains("# not a heading"));
        assert!(found[2].body.ends_with("two\n#nospace\n"));
        // Blank lines before the first heading make no section.
        let headings: Vec<String> = sections("\n# A\n").into_iter().map(|s| s.heading).collect();
        assert_eq!(headings, ["A"]);
    }

    #[test]
    fn a_term_matches_only_as_a_whole_token() {
        assert!(contains_term(
            "calls `load_title_prefixes`.",
            "load_title_prefixes"
        ));
        assert!(!contains_term(
            "load_title_prefixes_v2",
            "load_title_prefixes"
        ));
        assert!(!contains_term(
            "my_load_title_prefixes",
            "load_title_prefixes"
        ));
        assert!(contains_term("読むのはsrc/a.rsの関数", "src/a.rs"));
        // A heading is named as a whole token too.
        assert!(!names_heading("change LintInput", "`Lint`"));
        assert!(names_heading("「責務と境界」に足す", "責務と境界"));
    }

    #[test]
    fn hints_list_the_sections_naming_the_tasks_terms_most_first() {
        let documents = [
            doc(
                "docs/a.md",
                "# One\nuses `related_docs`\n# Two\n`related_docs` and LintInput\n# Three\nnothing\n",
            ),
            doc("docs/b.md", "# Other\nLintInput\n"),
        ];
        let tasks = [task(7, "add `related_docs` to LintInput", "")];
        let hints = hints(&tasks, &documents);
        assert_eq!(hints.len(), 1);
        let found: Vec<(&str, &str, Vec<String>)> = hints[0]
            .sections
            .iter()
            .map(|s| (s.path.as_str(), s.heading.as_str(), s.terms.clone()))
            .collect();
        assert_eq!(
            found,
            [
                (
                    "docs/a.md",
                    "Two",
                    vec!["LintInput".into(), "related_docs".into()]
                ),
                ("docs/a.md", "One", vec!["related_docs".into()]),
                ("docs/b.md", "Other", vec!["LintInput".into()]),
            ]
        );
        assert_eq!(hints[0].omitted, 0);
        assert_eq!(
            serde_json::to_value(&hints[0]).unwrap(),
            serde_json::json!({
                "task_id": 7,
                "sections": [
                    {"path": "docs/a.md", "heading": "Two", "terms": ["LintInput", "related_docs"]},
                    {"path": "docs/a.md", "heading": "One", "terms": ["related_docs"]},
                    {"path": "docs/b.md", "heading": "Other", "terms": ["LintInput"]},
                ],
                "omitted": 0,
            })
        );
    }

    #[test]
    fn a_quoted_word_and_a_short_path_give_hints() {
        let documents = [doc(
            "docs/flow.md",
            "# Parse\n`parse` reads it\n# Files\nsee src/a.rs\n# Other\nparse in prose\n",
        )];
        let quoted = hints(&[task(1, "`parse` を変える", "")], &documents);
        let headings: Vec<&str> = quoted[0]
            .sections
            .iter()
            .map(|s| s.heading.as_str())
            .collect();
        assert_eq!(headings, ["Parse", "Other"]);
        let path = hints(&[task(2, "src/a.rs を変える", "")], &documents);
        assert_eq!(path[0].sections.len(), 1);
        assert_eq!(path[0].sections[0].heading, "Files");
        assert_eq!(path[0].sections[0].terms, ["src/a.rs"]);
    }

    #[test]
    fn a_document_or_section_the_description_or_context_names_is_left_out() {
        let documents = [
            doc("docs/design/a.md", "# One\n`related_docs`\n"),
            doc(
                "docs/design/b.md",
                "# `up`のコマンド\n`related_docs`\n# Kept\n`related_docs`\n",
            ),
        ];
        // By path in the description.
        let by_path = [task(1, "`related_docs`; docs/design/a.md も直す", "")];
        let headings: Vec<String> = hints(&by_path, &documents)[0]
            .sections
            .iter()
            .map(|s| format!("{}#{}", s.path, s.heading))
            .collect();
        assert_eq!(
            headings,
            ["docs/design/b.md#`up`のコマンド", "docs/design/b.md#Kept"]
        );
        // By file name and by heading (backquotes ignored) in the context.
        let by_name = [task(2, "`related_docs`", "b.md ではなく a.md")];
        assert!(hints(&by_name, &documents).is_empty());
        let by_heading = [task(3, "`related_docs`", "a.md と「upのコマンド」")];
        let headings: Vec<String> = hints(&by_heading, &documents)[0]
            .sections
            .iter()
            .map(|s| s.heading.clone())
            .collect();
        assert_eq!(headings, ["Kept"]);
        // The title and the acceptance do not leave a document out.
        let mut in_title = task(4, "`related_docs`", "");
        in_title.title = "docs/design/a.md";
        in_title.acceptance = "b.md";
        assert_eq!(hints(&[in_title], &documents)[0].sections.len(), 3);
    }

    #[test]
    fn sections_past_the_limit_are_counted_and_unreadable_documents_reported() {
        let text: String = (0..MAX_SECTIONS_PER_TASK + 3)
            .map(|i| format!("# S{i}\n`related_docs`\n"))
            .collect();
        let documents = [
            doc("docs/a.md", &text),
            RelatedDocument {
                path: "docs/bad.md".into(),
                content: Err("stream did not contain valid UTF-8".into()),
            },
        ];
        let hints = hints(
            &[task(1, "`related_docs`", ""), task(2, "nothing", "")],
            &documents,
        );
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].sections.len(), MAX_SECTIONS_PER_TASK);
        assert_eq!(hints[0].sections[0].heading, "S0");
        assert_eq!(hints[0].omitted, 3);
        // A task with no section still carries the unreadable documents.
        assert!(hints[1].sections.is_empty());
        assert_eq!(
            hints[1].unreadable,
            [UnreadableDocument {
                path: "docs/bad.md".into(),
                reason: "stream did not contain valid UTF-8".into(),
            }]
        );
    }

    #[test]
    fn no_documents_give_no_hints() {
        assert!(hints(&[task(1, "`related_docs`", "")], &[]).is_empty());
        let documents = [doc("docs/a.md", "# One\nother\n")];
        assert!(hints(&[task(1, "`related_docs`", "")], &documents).is_empty());
    }
}
