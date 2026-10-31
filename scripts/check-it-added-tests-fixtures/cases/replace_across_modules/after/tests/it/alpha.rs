fn helper() -> u32 {
    1
}

mod inner {
    #[test]
    fn inner_case() {
        let x: [u8; 2] = [0; 2];
        assert_eq!(x.len(), 2);
    }
}
