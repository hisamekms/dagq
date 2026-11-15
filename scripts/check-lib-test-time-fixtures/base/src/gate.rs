// Braces in strings, char literals and comments are not counted: { ' }
pub fn outside_the_tests() -> u32 {
    1
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
    }

    #[test]
    fn a_test_whose_body_changed() {
        assert_eq!(1, 1);
    }

    fn a_helper_of_one_test() -> u32 {
        1
    }

    #[test]
    fn a_test_whose_helper_changed() {
        assert_eq!(a_helper_of_one_test(), 1);
    }
}

#[cfg(test)]
mod more {
    #[test]
    fn a_test_with_a_plus_plus_line() {
        let s = "
a line of a string";
        assert!(!s.is_empty());
    }

    #[test]
    fn a_test_after_a_plus_plus_line() {
        assert_eq!(1, 1);
    }
}
