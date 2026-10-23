use crate::support::value;

fn twice(x: u32) -> u32 {
    x * 2
}

#[test]
fn a_one() {
    assert_eq!(value() + 0, 1);
}

#[test]
fn a_two() {
    assert_eq!(twice(value()) + 0, 2);
}
