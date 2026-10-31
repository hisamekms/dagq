fn helper() -> u32 {
    2
}

fn another_helper() {}

#[test]
fn keeps_a_boundary() {
    another_helper();
    assert_eq!(helper(), 2);
}

mod inner {
    #[test]
    fn inner_case() {
        let x: [u8; 3] = [0; 3];
        assert_eq!(x.len(), 3);
    }
}
