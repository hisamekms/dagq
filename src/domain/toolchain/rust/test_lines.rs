//! Which lines of a Rust source are in a test ([`super::Rust`]'s
//! [`ToolchainProfile::test_lines`](super::ToolchainProfile::test_lines)):
//! the items a test's attribute is put on, read from the tokens with the
//! comments and the string and character literals left out.
//!
//! A test's attribute is `#[test]`, an attribute whose path ends in `test`
//! (`#[tokio::test]`, with or without arguments), or a `#[cfg(…)]` whose
//! predicate names `test` anywhere (`cfg(not(test))` too, which counts its
//! item as a test: a line counted wrongly so only keeps a gate). Its item
//! runs from the attribute to the `;` or the closing `}` of the first
//! brace that ends it (the commas of its generic arguments and its
//! `where` clause end none, nor do the braces of a macro's call in its
//! signature), or to the `,` or the enclosing `}` of a
//! field, a variant or an argument. `#![cfg(test)]` (or another of these as an inner
//! attribute) puts the whole file in a test.

use super::TestLines;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Punct(char),
}

/// The tokens of `text`, each with its line from 1, and the last line.
fn tokens(text: &str) -> (Vec<(Token, usize)>, usize) {
    let chars: Vec<char> = text.chars().collect();
    let at = |index: usize| chars.get(index).copied();
    let mut out = Vec::new();
    let mut line = 1;
    let mut index = 0;
    // Skip to past the closing `"` of a string whose text starts at
    // `index`, followed by `hashes` `#` for a raw one.
    let skip_string = |index: &mut usize, line: &mut usize, raw: Option<usize>| {
        while let Some(c) = at(*index) {
            *index += 1;
            match c {
                '\n' => *line += 1,
                '\\' if raw.is_none() => {
                    if at(*index) == Some('\n') {
                        *line += 1;
                    }
                    *index += 1;
                }
                '"' => {
                    let hashes = raw.unwrap_or(0);
                    if (0..hashes).all(|n| at(*index + n) == Some('#')) {
                        *index += hashes;
                        return;
                    }
                }
                _ => {}
            }
        }
    };
    while let Some(c) = at(index) {
        match c {
            '\n' => {
                line += 1;
                index += 1;
            }
            c if c.is_whitespace() => index += 1,
            '/' if at(index + 1) == Some('/') => {
                while at(index).is_some_and(|c| c != '\n') {
                    index += 1;
                }
            }
            '/' if at(index + 1) == Some('*') => {
                let mut depth = 0;
                while let Some(c) = at(index) {
                    if c == '/' && at(index + 1) == Some('*') {
                        depth += 1;
                        index += 2;
                    } else if c == '*' && at(index + 1) == Some('/') {
                        depth -= 1;
                        index += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        if c == '\n' {
                            line += 1;
                        }
                        index += 1;
                    }
                }
            }
            '"' => {
                index += 1;
                skip_string(&mut index, &mut line, None);
            }
            '\'' => {
                if at(index + 1) == Some('\\') {
                    // An escaped character: to its closing quote.
                    index += 3;
                    while at(index).is_some_and(|c| c != '\'' && c != '\n') {
                        index += 1;
                    }
                    index += 1;
                } else if at(index + 2) == Some('\'') {
                    index += 3;
                } else {
                    // A lifetime or a label: its name is a word.
                    index += 1;
                }
            }
            c if c.is_alphanumeric() || c == '_' => {
                // A literal's prefix (`b"`, `br#"`, `c"`, `r"`), else a word.
                let mut prefix = index;
                if matches!(c, 'b' | 'c') {
                    prefix += 1;
                }
                if matches!(at(prefix), Some('"')) && prefix > index {
                    index = prefix + 1;
                    skip_string(&mut index, &mut line, None);
                    continue;
                }
                if c == 'b' && at(index + 1) == Some('\'') {
                    index += 1;
                    continue;
                }
                if at(prefix) == Some('r') {
                    let hashes = (prefix + 1..).take_while(|&n| at(n) == Some('#')).count();
                    if at(prefix + 1 + hashes) == Some('"') {
                        index = prefix + 2 + hashes;
                        skip_string(&mut index, &mut line, Some(hashes));
                        continue;
                    }
                }
                let start = index;
                while at(index).is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    index += 1;
                }
                out.push((Token::Word(chars[start..index].iter().collect()), line));
            }
            c => {
                out.push((Token::Punct(c), line));
                index += 1;
            }
        }
    }
    (out, line)
}

/// The lines of `text`, a Rust source, in a test: see the module's doc.
pub(super) fn test_lines(text: &str) -> Vec<TestLines> {
    let (tokens, last) = tokens(text);
    let punct = |index: usize, c: char| {
        tokens
            .get(index)
            .is_some_and(|(t, _)| *t == Token::Punct(c))
    };
    let mut found = Vec::new();
    for (index, (token, line)) in tokens.iter().enumerate() {
        if *token != Token::Punct('#') {
            continue;
        }
        let inner = punct(index + 1, '!');
        let open = index + 1 + usize::from(inner);
        if !punct(open, '[') {
            continue;
        }
        let Some(close) = closing(&tokens, open, '[', ']') else {
            continue;
        };
        if !is_test_attribute(&tokens[open + 1..close]) {
            continue;
        }
        if inner {
            return std::iter::once(1..=last).collect();
        }
        let end = item_end(&tokens, close + 1);
        found.push(*line..=tokens[end].1);
    }
    found
}

/// The index of the token that closes the `open` at `index`.
fn closing(tokens: &[(Token, usize)], index: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (at, (token, _)) in tokens.iter().enumerate().skip(index) {
        if *token == Token::Punct(open) {
            depth += 1;
        } else if *token == Token::Punct(close) {
            depth -= 1;
            if depth == 0 {
                return Some(at);
            }
        }
    }
    None
}

/// Whether an attribute's tokens (between its brackets) are a test's.
fn is_test_attribute(tokens: &[(Token, usize)]) -> bool {
    let word = |index: usize| match tokens.get(index) {
        Some((Token::Word(word), _)) => Some(word.as_str()),
        _ => None,
    };
    if word(0) == Some("cfg") {
        return tokens
            .iter()
            .skip(1)
            .any(|(token, _)| *token == Token::Word("test".into()));
    }
    // The attribute's path: words joined by `::`, up to its arguments.
    let path_end = tokens
        .iter()
        .position(|(token, _)| !matches!(token, Token::Word(_) | Token::Punct(':')))
        .unwrap_or(tokens.len());
    path_end > 0 && word(path_end - 1) == Some("test")
}

/// The index of the last token of the item that starts at `index`, after
/// its attribute: see the module's doc. A `,` ends only a field, a
/// variant or an argument, never an item a keyword starts ([`is_item`]),
/// whose `where` clause has commas; and a `,` inside a type's generic
/// arguments (`Result<(), E>`) does not end it: the `<` and `>` of the
/// signature are counted, but the `>` of `->` and `=>`. A `<` miscounted
/// (a comparison) only makes the item longer, which keeps a gate. The
/// braces of a macro's call (`R!{}` as a return type) are its own and end
/// no item, but for an item that is that call alone (`thread_local! {…}`).
fn item_end(tokens: &[(Token, usize)], index: usize) -> usize {
    let commas_end = !is_item(tokens, index);
    let mut depth = 0usize;
    let mut angles = 0usize;
    let mut at = index;
    while let Some((token, _)) = tokens.get(at) {
        let after_arrow = at > 0 && matches!(tokens[at - 1].0, Token::Punct('-' | '='));
        let macro_call = at > index
            && tokens[at - 1].0 == Token::Punct('!')
            && !tokens[index..at - 1]
                .iter()
                .all(|(token, _)| matches!(token, Token::Word(_) | Token::Punct(':')));
        match token {
            Token::Punct('<') => angles += 1,
            Token::Punct('>') if angles > 0 && !after_arrow => angles -= 1,
            // A brace inside brackets (a closure's body in an argument)
            // or generic arguments (a const's block) is closed in them.
            Token::Punct('(' | '[') => depth += 1,
            Token::Punct('{') if depth > 0 || angles > 0 || macro_call => depth += 1,
            Token::Punct(')' | ']' | '}') if depth > 0 => depth -= 1,
            Token::Punct('{') => {
                return closing(tokens, at, '{', '}').unwrap_or(tokens.len() - 1);
            }
            Token::Punct(';') if depth == 0 => return at,
            Token::Punct(',') if commas_end && depth == 0 && angles == 0 => return at,
            // The `}` or `)` of what holds it: the token before.
            Token::Punct('}' | ')' | ']') => return at - 1,
            _ => {}
        }
        at += 1;
    }
    tokens.len() - 1
}

/// The words that start an item (after its visibility): a `,` does not
/// end what they start.
const ITEM_WORDS: [&str; 17] = [
    "fn",
    "mod",
    "impl",
    "struct",
    "enum",
    "union",
    "static",
    "const",
    "trait",
    "type",
    "use",
    "extern",
    "unsafe",
    "async",
    "macro_rules",
    "let",
    "default",
];

/// Whether what starts at `index`, after its attribute, past any further
/// attribute and its visibility (`pub`, `pub(crate)`), is an item a
/// keyword starts rather than a field, a variant or an argument.
fn is_item(tokens: &[(Token, usize)], mut index: usize) -> bool {
    loop {
        match tokens.get(index) {
            Some((Token::Punct('#'), _)) => {
                let open = index
                    + 1
                    + usize::from(matches!(
                        tokens.get(index + 1),
                        Some((Token::Punct('!'), _))
                    ));
                match closing(tokens, open, '[', ']') {
                    Some(close) if matches!(tokens.get(open), Some((Token::Punct('['), _))) => {
                        index = close + 1;
                    }
                    _ => return false,
                }
            }
            Some((Token::Word(word), _)) if word == "pub" => {
                index += 1;
                if matches!(tokens.get(index), Some((Token::Punct('('), _))) {
                    match closing(tokens, index, '(', ')') {
                        Some(close) => index = close + 1,
                        None => return false,
                    }
                }
            }
            Some((Token::Word(word), _)) => return ITEM_WORDS.contains(&word.as_str()),
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`test_lines`] as the first and last line of each.
    fn spans(text: &str) -> Vec<(usize, usize)> {
        test_lines(text)
            .into_iter()
            .map(|lines| (*lines.start(), *lines.end()))
            .collect()
    }

    #[test]
    fn a_test_function_and_a_test_module_are_tests_and_the_rest_is_not() {
        let text = "\
fn production() {
    let x = 1;
}

#[test]
fn alone() {
    assert!(true);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn helper() {}
}

fn after() {}
";
        assert_eq!(spans(text), [(5, 8), (10, 15)]);
    }

    #[test]
    fn attributes_of_a_path_ending_in_test_and_cfgs_naming_test_count() {
        let text = "\
#[tokio::test(flavor = \"multi_thread\")]
async fn a() {}
#[cfg(all(test, unix))]
fn b() {}
#[cfg(not(test))]
fn c() {}
#[cfg(unix)]
fn d() {}
#[testing]
fn e() {}
#[derive(Debug)]
struct F;
";
        assert_eq!(spans(text), [(1, 2), (3, 4), (5, 6)]);
    }

    #[test]
    fn an_item_ends_at_its_semicolon_or_at_its_field_s_comma() {
        let text = "\
#[cfg(test)]
mod tests;
struct S {
    #[cfg(test)]
    seen: u32,
    kept: u32,
}
fn f(#[cfg(test)] a: u32) {}
";
        assert_eq!(spans(text), [(1, 2), (4, 5), (8, 8)]);
        let text = "#[cfg(test)]\nstatic X: L = L::new(|| {\n    1\n});\nfn a() {}\n";
        assert_eq!(spans(text), [(1, 4)]);
    }

    /// The braces of a macro's call in a signature end no item; an item
    /// that is a macro's call alone ends at its own braces.
    #[test]
    fn a_macro_s_braces_in_a_signature_do_not_end_the_item() {
        let text = "\
macro_rules! R { () => { Result<(), std::io::Error> }; }
#[test]
fn t() -> R!{} {
    Ok(())
}
#[cfg(test)]
thread_local! {
    static X: u32 = 1;
}
fn after() {}
";
        assert_eq!(spans(text), [(2, 5), (6, 9)]);
    }

    /// The commas of a signature's generic arguments and `where` clause
    /// end no item; those of a field still end it, visibility and all.
    #[test]
    fn a_comma_in_generic_arguments_does_not_end_the_item() {
        let text = "\
#[test]
fn t() -> Result<(), std::io::Error> {
    Ok(())
}
fn after() {}
";
        assert_eq!(spans(text), [(1, 4)]);
        let text = "\
struct S {
    #[cfg(test)]
    pub(crate) seen: HashMap<String, Vec<u32>>,
    kept: u32,
}
#[test]
fn f() where F: Fn(u32) -> Option<u32>, G: Fn() {
    x();
}
#[test]
fn g() -> Foo<{ 1 }, u8> {
    y();
}
";
        assert_eq!(spans(text), [(2, 3), (6, 9), (10, 13)]);
    }

    #[test]
    fn braces_and_marks_in_comments_and_literals_are_not_code() {
        let text = r####"
#[test]
fn a() {
    let s = "}"; // }
    let r = r#"}"
#[test]"#;
    /* } /* nested } */ */
    let c = '}';
    let e = '\'';
    let b = b'}';
    let l: &'static str = br"}";
}
fn after() { let t = "#[test]"; }
"####;
        assert_eq!(spans(text), [(2, 12)]);
    }

    #[test]
    fn an_inner_test_attribute_puts_the_whole_file_in_a_test() {
        let text = "//! doc\n#![cfg(test)]\n\nfn a() {}\n";
        assert_eq!(spans(text), [(1, 5)]);
        assert!(spans("#![allow(dead_code)]\nfn a() {}\n").is_empty());
    }

    #[test]
    fn an_unfinished_source_ends_its_item_at_its_last_token() {
        assert_eq!(spans("#[test]\nfn a() {\n  x(\n"), [(1, 3)]);
        assert_eq!(spans("#[test]\nfn a()"), [(1, 2)]);
        assert!(spans("#[test").is_empty());
        assert_eq!(spans("#[test]"), [(1, 1)]);
    }
}
