fn helper() -> u32 {
    1
}

#[test]
fn keeps_a_boundary_with_a_reopened_file() {
    assert_eq!(helper(), 1);
}

mod inner {
    #[test]
    fn inner_case() {
        let x: [u8; 2] = [0; 2];
        assert_eq!(x.len(), 2);
    }
}
