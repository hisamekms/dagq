//! Whether a text the runtime emits cites dagq's own history: an ADR of
//! this repository (`ADR-0077`, `ADR-t1433-2`) or a task by its number
//! (`task 1437`). The runtime's messages state their rules in plain words,
//! since dagq runs in repositories whose ADRs and tasks are other ones.
//! Test code of the binary (declared by `src/main.rs`, outside the
//! library's layers), shared by the test of the string literals of `src/`
//! (`emitted_text`) and the test of the CLI's help.

/// The first citation in `text`, if any: `ADR-` followed by a digit or by
/// `t` and a digit, or `task ` (any case) followed by three or more digits.
pub fn first_citation(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let digit = |at: usize| bytes.get(at).is_some_and(u8::is_ascii_digit);
    for at in 0..bytes.len() {
        let rest = &bytes[at..];
        if rest.starts_with(b"ADR-")
            && (digit(at + 4) || (bytes.get(at + 4) == Some(&b't') && digit(at + 5)))
        {
            let end = (at + 4..bytes.len())
                .find(|&end| !(bytes[end].is_ascii_alphanumeric() || bytes[end] == b'-'))
                .unwrap_or(bytes.len());
            return Some(&text[at..end]);
        }
        if rest.len() >= 5
            && rest[..5].eq_ignore_ascii_case(b"task ")
            && (0..3).all(|offset| digit(at + 5 + offset))
        {
            return Some(&text[at..at + 8]);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::first_citation;

    #[test]
    fn citations_of_adrs_and_task_numbers_are_found_and_other_text_is_not() {
        for text in [
            "retired (ADR-t1433-2)",
            "near-term dependencies (ADR-0077)",
            "the interactive worker was retired, task 1437",
            "Task 1561 and more",
            "ADR-1あ",
        ] {
            assert!(first_citation(text).is_some(), "{text}");
        }
        for text in [
            "ADR-",
            "an ADR of the repository",
            "ADR-tN-N",
            "task {id}",
            "task 12",
            "a fix task is needed",
            "the task's kind",
        ] {
            assert_eq!(first_citation(text), None, "{text}");
        }
    }
}
