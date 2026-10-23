#[test]
fn marks_are_two() {
    let marks = ['é','}'];
    assert_eq!(marks.len(), 2);
    assert!(marks.contains(&'}'));
}
