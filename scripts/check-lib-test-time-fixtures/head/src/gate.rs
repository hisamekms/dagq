// Braces in strings, char literals and comments are not counted: { ' }
pub fn outside_the_tests() -> u32 {
    2
}

#[cfg(test)]
mod tests {
    fn helper() -> u32 {
        1
    }

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

    #[test]
    fn a_test_whose_body_changed() {
        assert_eq!(helper(), 1);
    }

    fn a_helper_of_one_test() -> u32 {
        let s = r#"a raw string with "quotes" and } a brace"#;
        s.len() as u32 - 40
    }

    #[test]
    fn a_test_whose_helper_changed() {
        assert_eq!(a_helper_of_one_test(), 1);
    }

    #[test]
    fn a_new_slow_test() {
        assert_eq!(helper(), 1);
    }

    #[test]
    fn a_new_fast_test() {
        assert_eq!(helper(), 1);
    }

    #[test]
    fn a_test_at_the_threshold() {
        assert_eq!(helper(), 1);
    }

    #[test]
    fn a_coloured_slow_test() {
        assert_eq!(helper(), 1);
    }

    #[test]
    fn a_test_only_dagq_it_times() {
        assert_eq!(helper(), 1);
    }

    #[test]
    fn an_array_test() -> [u8; 2] {
        [2, 1]
    }
}

#[cfg(test)]
mod more {
    #[test]
    fn a_test_with_a_plus_plus_line() {
        let s = "
++ a line of a string";
        assert!(!s.is_empty());
    }

    #[test]
    fn a_test_after_a_plus_plus_line() {
        assert_eq!(2, 2);
    }

    macro_rules! tests_of {
        ($name:ident) => {
            mod $name {
                #[test]
                fn a_macro_pattern_test() {}
            }
        };
    }

    const _C: &core::ffi::CStr = cr#"a"b{"#;

    #[test]
    fn a_test_after_a_c_raw_string() {}
}
