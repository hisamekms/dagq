pub fn production() -> u32 {
    1
}

#[テスト::test]
fn under_a_unicode_path() {
    assert_eq!(1, production());
}
