// Braces in strings, char literals and comments are not counted: { ' }
#[test]
fn an_unchanged_slow_test() {
    let s = "an unbalanced { in a string";
    let c = '{';
    /* and in a /* nested */ comment { */
    assert!(!s.is_empty() && c == '{');
}
fn a_helper_the_head_removes() -> u32 {
    3
}

#[test]
fn an_allowed_slow_test() {
    assert_eq!(1, 1);
}

fn helper() -> u32 {
    1
}

#[test]
fn an_array_test() -> [u8; 2] {
    [1, 2]
}
