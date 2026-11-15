//! The bin root of the fixture: its tests are binary dagq::bin/dagq.
#[cfg(test)]
mod bin_only;

fn main() {}

#[cfg(test)]
mod daemon_tests {
    #[test]
    fn an_old_bin_test() {}

    #[test]
    fn a_bin_inline_test() {
        assert_eq!(1, 1);
    }
}
