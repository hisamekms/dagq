pub fn answer() -> u32 {
    42
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected() -> u32 {
        40 + 2
    }

    #[test]
    fn checks_answer() {
        assert_eq!(answer(), expected());
    }
}
