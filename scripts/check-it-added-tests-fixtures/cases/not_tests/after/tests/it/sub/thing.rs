// #[test]
// fn in_a_line_comment() {}

/* #[test] fn in_a_block_comment() { /* nested */ } */

const SCRIPT: &str = "#[test] fn in_a_string() { \" } \" }";
const RAW: &str = r#"#[test]
fn in_a_raw_string() { "}" }
"#;
const BRACE: char = '{';

fn lifetime<'a>(s: &'a str) -> &'a str {
    s
}

#[test]
fn thing_case() {}

#[test]
#[ignore = "a } in an attribute"]
fn after_the_strings() {
    let _ = (SCRIPT, RAW, BRACE, lifetime("x"), '}', '\'');
}
