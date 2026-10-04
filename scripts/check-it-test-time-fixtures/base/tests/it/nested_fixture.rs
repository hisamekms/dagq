mod review_verdicts {
    mod inner {
        #[test]
        fn a_nested_test_whose_body_changed() {
            let x = r#"a raw string with } and {"#;
            assert!(!x.is_empty());
        }
    }
}

#[test]
fn a_nested_test_whose_body_changed() {
    assert_eq!(1, 1);
}
