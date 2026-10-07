//! The check that an agent's definition does not borrow a case's own words:
//! a definition fitted to its cases scores well on them
//! and not on the reviews it meets. The words a case owns are the paths its
//! patch changes, their file names, and the ADR ids and `backticked` names
//! of its changed lines; one the rules' documents (the documents the
//! definition links to) also use is the rule's, not the case's. The words
//! and the matching are the Spike's `runner/eval.py` `leak_terms` and
//! `leak_check` (dagq-agent-eval commit cfb8a4c).

use std::collections::BTreeSet;

/// A case's word that the definition uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leak {
    pub case: String,
    pub term: String,
}

/// The targets of the definition's Markdown links (`[text](target)`),
/// without the `#anchor`: the rules' documents, relative to the definition.
/// The caller reads them for [`leaks`]'s `rule_texts`.
pub fn linked_paths(definition: &str) -> Vec<String> {
    definition
        .match_indices("](")
        .filter_map(|(at, _)| {
            let rest = &definition[at + 2..];
            let end = rest.find([')', '#']).unwrap_or(rest.len());
            (end > 0).then(|| rest[..end].to_owned())
        })
        .collect()
}

/// The words of `definition` that a case owns: for each case (id, patch
/// text) the terms of [`case_terms`] that `definition` uses as a whole
/// word, except those the rules use: a term inside a link's target or its
/// file name, or one of `rule_texts` (the linked documents' texts).
pub fn leaks<'a>(
    definition: &str,
    rule_texts: &[&str],
    cases: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<Leak> {
    let links = linked_paths(definition);
    let linked: Vec<&str> = links
        .iter()
        .flat_map(|link| [link.as_str(), file_name(link)])
        .collect();
    let mut found = Vec::new();
    for (case, patch) in cases {
        for term in case_terms(patch) {
            let the_rules = linked.iter().any(|link| link.contains(term.as_str()))
                || rule_texts.iter().any(|text| text.contains(term.as_str()));
            if !the_rules && uses_word(definition, &term) {
                found.push(Leak {
                    case: case.to_owned(),
                    term,
                });
            }
        }
    }
    found
}

/// The words a case's patch owns: each changed path (`--- a/` and
/// `+++ b/`) and its file name without the extension when that is
/// [`specific`] and 6 characters or more, and on the changed lines the ADR
/// ids (`ADR-t<task>-<n>`, `ADR-<4 digits>`, either case of `adr`) and the
/// [`specific`] names between backticks (4 to 60 characters).
pub fn case_terms(patch: &str) -> BTreeSet<String> {
    let mut terms = BTreeSet::new();
    for line in patch.lines() {
        if let Some(path) = line
            .strip_prefix("+++ b/")
            .or_else(|| line.strip_prefix("--- a/"))
        {
            let path: String = path.chars().take_while(|c| !c.is_whitespace()).collect();
            if !path.is_empty() {
                let stem = stem(&path);
                if stem.chars().count() >= 6 && specific(stem) {
                    terms.insert(stem.to_owned());
                }
                terms.insert(path);
            }
        }
        let changed = (line.starts_with('+') || line.starts_with('-'))
            && !line.starts_with("+++")
            && !line.starts_with("---");
        if changed {
            terms.extend(adr_ids(line));
            terms.extend(backticked(line).into_iter().filter(|term| specific(term)));
        }
    }
    terms
}

/// A name a case could own: it has a separator (`_./:-`), a digit or a
/// capital after a small letter, unlike a plain word (`review`, `task`)
/// that any definition may use.
pub fn specific(term: &str) -> bool {
    term.chars()
        .any(|c| matches!(c, '_' | '.' | '/' | ':' | '-') || c.is_numeric())
        || term
            .as_bytes()
            .windows(2)
            .any(|pair| pair[0].is_ascii_lowercase() && pair[1].is_ascii_uppercase())
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn file_name(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

/// The file name without its last extension; a name with only a leading
/// dot keeps it.
fn stem(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    }
}

/// Whether `text` has `term` with neither a word character nor `-` on
/// either side, trying every start (occurrences may overlap).
fn uses_word(text: &str, term: &str) -> bool {
    if term.is_empty() {
        return false;
    }
    text.char_indices().any(|(at, _)| {
        text[at..].starts_with(term) && {
            let before = text[..at].chars().next_back();
            let after = text[at + term.len()..].chars().next();
            [before, after]
                .into_iter()
                .all(|c| c.is_none_or(|c| !is_word(c) && c != '-'))
        }
    })
}

/// The ADR ids of `line`: `ADR-` or `adr-` after a non-word character,
/// then `t<digits>-<digits>` or 4 digits, then a non-word character.
fn adr_ids(line: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut from = 0;
    while let Some(found) = ["ADR-", "adr-"]
        .iter()
        .filter_map(|prefix| line[from..].find(prefix))
        .min()
    {
        let at = from + found;
        from = at + 4;
        if line[..at].chars().next_back().is_some_and(is_word) {
            continue;
        }
        let rest = &line[at + 4..];
        let length = adr_number(rest);
        if let Some(length) = length
            && !rest[length..].chars().next().is_some_and(is_word)
        {
            ids.push(line[at..at + 4 + length].to_owned());
            from = at + 4 + length;
        }
    }
    ids
}

/// The length of `t<digits>-<digits>` or of 4 digits at the start of
/// `rest`.
fn adr_number(rest: &str) -> Option<usize> {
    let digits = |text: &str| text.bytes().take_while(u8::is_ascii_digit).count();
    if let Some(task) = rest.strip_prefix('t') {
        let first = digits(task);
        if first > 0
            && let Some(number) = task[first..].strip_prefix('-')
        {
            let second = digits(number);
            if second > 0 {
                return Some(1 + first + 1 + second);
            }
        }
    }
    (rest.len() >= 4 && rest.as_bytes()[..4].iter().all(u8::is_ascii_digit)).then_some(4)
}

/// The texts between a pair of backticks on `line`, 4 to 60 characters
/// without a backtick, read from the left as pairs.
fn backticked(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(open) = line[from..].find('`') {
        let start = from + open + 1;
        let rest = &line[start..];
        match rest.find('`') {
            Some(close) if (4..=60).contains(&rest[..close].chars().count()) => {
                found.push(rest[..close].to_owned());
                from = start + close + 1;
            }
            Some(_) => from = start,
            None => break,
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "\
diff --git a/src/infrastructure/schema.rs b/src/infrastructure/schema.rs
--- a/src/infrastructure/schema.rs
+++ b/src/infrastructure/schema.rs
@@ -1,2 +1,2 @@
-pub const SCHEMA_VERSION: u32 = 64; // ADR-t1453-1
+pub const SCHEMA_VERSION: u32 = 65; // see adr-0004 and `applied_count` and `review`
 context `not_changed_here`
";

    #[test]
    fn a_patch_owns_its_paths_specific_stems_adr_ids_and_backticked_names() {
        let terms: Vec<String> = case_terms(PATCH).into_iter().collect();
        assert_eq!(
            terms,
            [
                "ADR-t1453-1",
                "adr-0004",
                "applied_count",
                "src/infrastructure/schema.rs"
            ]
        );
        // `schema` is a plain word, `release_update` a specific stem.
        let terms = case_terms("+++ b/src/domain/release_update.rs\n");
        assert!(terms.contains("release_update"));
        assert!(specific("runNow") && specific("A-150") && !specific("review"));
    }

    #[test]
    fn adr_ids_and_backticks_follow_the_spikes_patterns() {
        assert_eq!(
            adr_ids("+(ADR-t12-3) xADR-0001 ADR-00011 adr-t1-2x ADR-0004."),
            ["ADR-t12-3", "ADR-0004"]
        );
        assert_eq!(
            backticked("+`ab` then `long_name` and `x`y`z_z_z`"),
            [" then ", " and ", "z_z_z"]
        );
    }

    #[test]
    fn the_check_finds_a_cases_word_in_the_definition() {
        let definition = "Check that `applied_count` is derived. See [rules](../../docs/m.md#a).";
        assert_eq!(
            leaks(definition, &[], [("gen-a", PATCH)]),
            [Leak {
                case: "gen-a".to_owned(),
                term: "applied_count".to_owned()
            }]
        );
        // Part of a longer word is not the word.
        assert!(leaks("`applied_count_total`", &[], [("gen-a", PATCH)]).is_empty());
        // An occurrence overlapping one that fails the boundary still counts.
        assert!(uses_word("xa.a.a", "a.a"));
    }

    #[test]
    fn the_check_does_not_react_to_the_rules_words() {
        let definition = "Never bump src/infrastructure/schema.rs by hand (ADR-t1453-1); \
                          read [the rules](../../../docs/development/migrations.md).";
        assert_eq!(
            linked_paths(definition),
            ["../../../docs/development/migrations.md"]
        );
        let rules = "The schema is src/infrastructure/schema.rs.";
        assert_eq!(
            leaks(definition, &[rules], [("gen-a", PATCH)]),
            [Leak {
                case: "gen-a".to_owned(),
                term: "ADR-t1453-1".to_owned()
            }]
        );
        let linked = "See [migrations](../../../docs/development/migrations.md): migrations.md";
        let patch = "+++ b/docs/development/migrations.md\n";
        assert!(leaks(linked, &[], [("gen-b", patch)]).is_empty());
    }
}
