//! How the runtime names and quotes what it hands to the shell: the
//! startup command of a session and the repository's name. Pure text; the
//! adapters re-export these under their own path.

use std::path::Path;

/// Shell boundaries are a session's startup command and Claude's hook command.
/// Quote every argument independently, including paths containing apostrophes.
pub fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|s| shell_quote(s))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\"'\"'"))
}

/// The repository's directory name, or the whole path when it has none.
pub fn repository_name(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}
