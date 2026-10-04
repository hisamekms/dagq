// Braces in strings, char literals and comments are not counted: { ' }
#[test]
fn an_unchanged_slow_test() {
    let s = "an unbalanced { in a string";
    let c = '{';
    /* and in a /* nested */ comment { */
    assert!(!s.is_empty() && c == '{');
}

#[test]
fn an_allowed_slow_test() {
    assert_eq!(1, 1);
    assert_eq!(2, 2);
}

fn helper() -> u32 {
    2
}

#[test]
fn a_new_slow_test() {
    assert_eq!(helper(), 2);
}

#[test]
fn a_new_fast_test() {
    assert_eq!(helper(), 2);
}

#[test]
fn a_test_the_log_does_not_time() {
    let s = r#"a raw string with "quotes" and } a brace"#;
    assert!(!s.is_empty());
}

#[test]
fn a_coloured_slow_test() {
    assert_eq!(helper(), 2);
}

#[test]
fn an_array_test() -> [u8; 2] {
    [2, 1]
}
