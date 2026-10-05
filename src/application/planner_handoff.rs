//! How a request's words reach a planner (ADR-t1228-1 decision 2,
//! ADR-t1394-1 decision 10): the words are written as a file under the
//! planner's directory, and the planner is pointed at that file with one
//! fixed sentence, never typed into its session as keys. The planner the
//! runtime opens for a planning request gets its request this way in its
//! first prompt; a later request to a planner already open (task 1533)
//! hands its file over the same way and sends the same sentence.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::RunFiles;

/// The directory under a planner's directory that holds the requests
/// handed to it.
pub const PLANNER_REQUESTS_DIR: &str = "requests";

/// A request handed to a planner: the file its words were written to, and
/// the sentence that points the planner at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandedRequest {
    pub path: PathBuf,
    pub sentence: String,
}

/// Write `words` as `<planner dir>/requests/<name>.md` and return the file
/// with the sentence that points at it ([`handed_request_sentence`]).
/// `name` names the request within the planner (`request-3`, a follow-up's
/// own name); it is one path component, and a file of that name already
/// there is replaced.
pub fn hand_request_to_planner(
    files: &dyn RunFiles,
    planner_dir: &Path,
    name: &str,
    words: &str,
) -> Result<HandedRequest> {
    ensure!(
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')),
        "a request handed to a planner is named by letters, digits, '-' and '_', not {name:?}"
    );
    let dir = planner_dir.join(PLANNER_REQUESTS_DIR);
    files
        .create_dir_all(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{name}.md"));
    files
        .write(&path, words.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    Ok(HandedRequest {
        sentence: handed_request_sentence(&path),
        path,
    })
}

/// The fixed sentence that points a planner at a request handed to it as
/// the file `path`.
pub fn handed_request_sentence(path: &Path) -> String {
    format!(
        "dagq: a request for you is in the file {}: read it and work on it as it says.",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::memory_files::MemoryFiles;

    #[test]
    fn the_words_are_written_under_the_planners_directory_and_pointed_at() {
        let files = MemoryFiles::default();
        let planner = Path::new("planners").join("4");
        let handed = hand_request_to_planner(&files, &planner, "request-2", "plan `it`\n").unwrap();
        assert_eq!(handed.path, planner.join("requests").join("request-2.md"));
        assert_eq!(files.read_to_string(&handed.path).unwrap(), "plan `it`\n");
        assert_eq!(
            handed.sentence,
            format!(
                "dagq: a request for you is in the file {}: read it and work on it as it says.",
                handed.path.display()
            )
        );
        // Handing it again replaces the file.
        hand_request_to_planner(&files, &planner, "request-2", "again").unwrap();
        assert_eq!(files.read_to_string(&handed.path).unwrap(), "again");
    }

    #[test]
    fn a_name_that_is_not_one_plain_component_is_refused() {
        let files = MemoryFiles::default();
        for name in ["", "../x", "a/b", "a b", "x.md"] {
            assert!(
                hand_request_to_planner(&files, Path::new("planner"), name, "w").is_err(),
                "{name:?}"
            );
        }
    }
}
