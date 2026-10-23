pub fn answer() -> u32 {
    42
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expected() -> u32 {
        42
    }

    #[test]
    fn checks_answer() {
        assert_eq!(answer(), expected());
    }
}
