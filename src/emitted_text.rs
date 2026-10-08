//! No string literal of the runtime cites dagq's own ADRs or task numbers
//! ([`crate::history_citation`]): the errors, warnings, refusals, event text,
//! prompts and generated files are built from the literals of `src/`, so the
//! test reads every `.rs` file there and checks each string literal outside
//! test code. Test code is what a `#[cfg(test)]` is put on (an item, or the
//! files a `#[cfg(test)] mod name;` declares). The help clap builds from the
//! doc comments of `src/main.rs` is checked by its own test there. This is
//! test code of the binary (declared by `src/main.rs`), outside the
//! library's layers, and reads the sources as files.

use crate::history_citation::first_citation;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
enum Token {
    Str(String),
    Punct(char),
    Word(String),
}

/// The tokens of a Rust source as far as the test needs them: string
/// literals (raw and byte ones too) with their text, punctuation, and words;
/// comments, character literals and lifetimes are dropped.
fn tokens(source: &str) -> Vec<Token> {
    let chars: Vec<char> = source.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        let next = chars.get(at + 1).copied();
        if c == '/' && next == Some('/') {
            while at < chars.len() && chars[at] != '\n' {
                at += 1;
            }
        } else if c == '/' && next == Some('*') {
            let mut depth = 0;
            while at < chars.len() {
                if chars[at] == '/' && chars.get(at + 1) == Some(&'*') {
                    depth += 1;
                    at += 2;
                } else if chars[at] == '*' && chars.get(at + 1) == Some(&'/') {
                    depth -= 1;
                    at += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    at += 1;
                }
            }
        } else if c == '"' {
            let mut text = String::new();
            at += 1;
            while at < chars.len() && chars[at] != '"' {
                // A line continuation is no text: Rust drops the newline
                // and the whitespace after it.
                if chars[at] == '\\' && chars.get(at + 1) == Some(&'\n') {
                    at += 1;
                    while chars.get(at).is_some_and(|c| c.is_whitespace()) {
                        at += 1;
                    }
                    continue;
                }
                if chars[at] == '\\' {
                    text.push(chars[at]);
                    at += 1;
                }
                if let Some(&escaped) = chars.get(at) {
                    text.push(escaped);
                }
                at += 1;
            }
            at += 1;
            out.push(Token::Str(text));
        } else if c == '\'' {
            if next == Some('\\') {
                at += 2;
                while at < chars.len() && chars[at] != '\'' {
                    at += 1;
                }
                at += 1;
            } else if chars.get(at + 2) == Some(&'\'') {
                at += 3;
            } else {
                at += 1;
            }
        } else if c.is_alphanumeric() || c == '_' {
            let start = at;
            while at < chars.len() && (chars[at].is_alphanumeric() || chars[at] == '_') {
                at += 1;
            }
            let word: String = chars[start..at].iter().collect();
            let hashes = chars[at..].iter().take_while(|&&c| c == '#').count();
            if (word == "r" || word == "br") && chars.get(at + hashes) == Some(&'"') {
                at += hashes + 1;
                let closing: Vec<char> = std::iter::once('"')
                    .chain(std::iter::repeat_n('#', hashes))
                    .collect();
                let start = at;
                while at < chars.len() && !chars[at..].starts_with(&closing) {
                    at += 1;
                }
                out.push(Token::Str(chars[start..at].iter().collect()));
                at += closing.len();
            } else if word != "b" || next != Some('"') {
                out.push(Token::Word(word));
            }
        } else {
            if !c.is_whitespace() {
                out.push(Token::Punct(c));
            }
            at += 1;
        }
    }
    out
}

/// The string literals of `tokens` outside test code, and the names of the
/// modules a `#[cfg(test)] mod name;` declares.
fn production_strings(tokens: &[Token]) -> (Vec<&str>, Vec<&str>) {
    let cfg_test = [
        Token::Punct('#'),
        Token::Punct('['),
        Token::Word("cfg".into()),
        Token::Punct('('),
        Token::Word("test".into()),
        Token::Punct(')'),
        Token::Punct(']'),
    ];
    let mut strings = Vec::new();
    let mut test_modules = Vec::new();
    let mut at = 0;
    while at < tokens.len() {
        if !tokens[at..].starts_with(&cfg_test) {
            if let Token::Str(text) = &tokens[at] {
                strings.push(text.as_str());
            }
            at += 1;
            continue;
        }
        // Skip the item the cfg is put on: to its `;`, or its body's end.
        at += cfg_test.len();
        let start = at;
        let mut depth = 0;
        while at < tokens.len() {
            match &tokens[at] {
                Token::Punct('(' | '[') => depth += 1,
                Token::Punct(')' | ']') => depth -= 1,
                // A field, variant or arm ends at its `,`, or at the `}` of
                // what holds it.
                Token::Punct(',' | '}') if depth == 0 => break,
                Token::Punct(';') if depth == 0 => {
                    if let [.., Token::Word(keyword), Token::Word(name)] = &tokens[start..at]
                        && keyword == "mod"
                    {
                        test_modules.push(name.as_str());
                    }
                    break;
                }
                Token::Punct('{') if depth == 0 => {
                    let mut braces = 0;
                    while at < tokens.len() {
                        match tokens[at] {
                            Token::Punct('{') => braces += 1,
                            Token::Punct('}') => {
                                braces -= 1;
                                if braces == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        at += 1;
                    }
                    break;
                }
                _ => {}
            }
            at += 1;
        }
        at += 1;
    }
    (strings, test_modules)
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

/// The directory that holds the files of the modules `file` declares.
fn children_dir(file: &Path) -> PathBuf {
    let stem = file.file_stem().unwrap();
    let parent = file.parent().unwrap();
    if ["mod", "lib", "main"].iter().any(|name| stem == *name) {
        parent.to_path_buf()
    } else {
        parent.join(stem)
    }
}

#[test]
fn no_runtime_string_literal_cites_a_dagq_adr_or_task_number() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    let mut test_paths = HashSet::new();
    let mut parsed = Vec::new();
    for file in &files {
        let source = std::fs::read_to_string(file).unwrap();
        let tokens = tokens(&source);
        let (strings, modules) = production_strings(&tokens);
        for module in modules {
            let dir = children_dir(file);
            test_paths.insert(dir.join(format!("{module}.rs")));
            test_paths.insert(dir.join(module));
        }
        let strings: Vec<String> = strings.into_iter().map(str::to_owned).collect();
        parsed.push((file.clone(), strings));
    }
    let mut found = Vec::new();
    for (file, strings) in &parsed {
        if test_paths.iter().any(|test| file.starts_with(test)) {
            continue;
        }
        for text in strings {
            if let Some(citation) = first_citation(text) {
                found.push(format!(
                    "{}: {citation:?} in {text:?}",
                    file.strip_prefix(&src).unwrap().display()
                ));
            }
        }
    }
    assert!(
        found.is_empty(),
        "string literals the runtime emits cite dagq's ADRs or task numbers; state the rule in plain words:\n{}",
        found.join("\n")
    );
}

#[test]
fn the_scan_reads_literals_and_skips_comments_and_test_items() {
    let source = r##"
        // "ADR-0001 in a comment"
        /* "task 1001" /* nested */ */
        const A: &str = "kept";
        const B: &str = r#"raw "kept""#;
        const C: char = '"';
        fn f<'a>(x: &'a str) -> &'a str { x }
        #[cfg(test)]
        mod tests { fn t() { let _ = "inside a test { brace"; } }
        #[cfg(test)]
        mod more;
        struct S { #[cfg(test)] field: u8, other: u8 }
        const D: &[u8] = b"bytes";
        const E: &str = "joined \
                         line";
    "##;
    let tokens = tokens(source);
    let (strings, modules) = production_strings(&tokens);
    assert_eq!(strings, ["kept", "raw \"kept\"", "bytes", "joined line"]);
    assert_eq!(modules, ["more"]);
}
