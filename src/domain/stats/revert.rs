//! The evidence of a revert a commit on main carries (ADR-t3957-1
//! decision 5, docs/design/main-history.md "記録"): the commit `git revert`
//! made names the commit it reverts in the line it writes into the
//! message's body by default, and the record of main keeps that commit.
//!
//! Limits: only the line `git revert` writes by default is read. A revert
//! written by hand (without the line, or with it reworded), one made with
//! `--no-edit` replaced by an edited message that drops the line, and a
//! change that undoes only part of a commit are not told from any other
//! commit: they read as [`RevertOf::NotRevert`].

/// What a recorded commit on main says it reverts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RevertOf {
    /// Recorded before the record carried the evidence (a
    /// `main_commits_recorded` commit without the field): unknown.
    #[default]
    Unrecorded,
    /// Its message has no line of `git revert`'s.
    NotRevert,
    /// It reverts this commit (its full ID, lowercase).
    Commit(String),
}

impl RevertOf {
    /// What `message` (a commit's whole message) says the commit reverts:
    /// the commit of the first line `This reverts commit <full ID>.` (or
    /// `,` where `git revert` of a merge goes on with `reversing`), the ID
    /// 40 hexadecimal digits (or 64 in a SHA-256 repository).
    pub fn of_message(message: &str) -> Self {
        message
            .lines()
            .find_map(|line| {
                let rest = line.trim().strip_prefix("This reverts commit ")?;
                let end = rest.find(['.', ','])?;
                let sha = &rest[..end];
                (matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()))
                    .then(|| Self::Commit(sha.to_ascii_lowercase()))
            })
            .unwrap_or(Self::NotRevert)
    }

    /// The commit it reverts, if it is a revert.
    pub fn target(&self) -> Option<&str> {
        match self {
            Self::Commit(sha) => Some(sha),
            Self::Unrecorded | Self::NotRevert => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha() -> String {
        "b0d6f36275c444c5fd6b3f2b127be1948604c61b".to_owned()
    }

    #[test]
    fn git_reverts_default_line_names_the_reverted_commit() {
        let message = format!("Revert \"second\"\n\nThis reverts commit {}.\n", sha());
        assert_eq!(RevertOf::of_message(&message), RevertOf::Commit(sha()));
        assert_eq!(
            RevertOf::of_message(&message).target(),
            Some(sha().as_str())
        );
    }

    #[test]
    fn a_merges_revert_line_and_upper_case_read_alike() {
        let message = format!(
            "Revert \"merge\"\n\nThis reverts commit {}, reversing\nchanges made to x.\n",
            sha().to_ascii_uppercase()
        );
        assert_eq!(RevertOf::of_message(&message), RevertOf::Commit(sha()));
    }

    #[test]
    fn a_commit_without_the_line_is_no_revert() {
        assert_eq!(
            RevertOf::of_message("feat: a change\n\nbody\n"),
            RevertOf::NotRevert
        );
        assert_eq!(RevertOf::of_message(""), RevertOf::NotRevert);
        assert_eq!(RevertOf::NotRevert.target(), None);
    }

    /// A revert written by hand, or a line naming a short or broken ID, is
    /// not told from another commit: the limit the module states.
    #[test]
    fn a_hand_written_revert_is_not_identified() {
        let short = format!("Revert x\n\nThis reverts commit {}.\n", &sha()[..12]);
        assert_eq!(RevertOf::of_message(&short), RevertOf::NotRevert);
        let reworded = format!("Revert x\n\nThis undoes {}.\n", sha());
        assert_eq!(RevertOf::of_message(&reworded), RevertOf::NotRevert);
        let unended = format!("This reverts commit {}", sha());
        assert_eq!(RevertOf::of_message(&unended), RevertOf::NotRevert);
        let not_hex = format!("This reverts commit {}.", "g".repeat(40));
        assert_eq!(RevertOf::of_message(&not_hex), RevertOf::NotRevert);
    }

    #[test]
    fn an_old_record_is_unrecorded() {
        assert_eq!(RevertOf::default(), RevertOf::Unrecorded);
        assert_eq!(RevertOf::Unrecorded.target(), None);
    }
}
