//! The related documents of `lint`'s hints (ADR-t3321-1): the files of a
//! checkout the globs of `[tasks] related_docs` match, each read as text.
//! Matching and judging are [`crate::domain::doc_hints`]'s.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, ensure};

use super::git_binary::git_executable;
use crate::domain::doc_hints::RelatedDocument;
use crate::domain::scope::glob_matches;

/// The files of the checkout at `root` some glob of `globs` matches,
/// sorted by path, each with its text or why it could not be read. The
/// files are those Git lists there, tracked or untracked but not ignored,
/// under each glob's literal leading segments, so a build's output and
/// what no glob names are never looked at; a symbolic link is passed over,
/// and a listed file that cannot be read is kept with why.
/// No globs read nothing; a Git that cannot list the files is an error.
pub fn read_related_docs(root: &Path, globs: &[String]) -> Result<Vec<RelatedDocument>> {
    if globs.is_empty() {
        return Ok(Vec::new());
    }
    let mut paths = list_files(root, globs)?;
    paths.retain(|path| globs.iter().any(|glob| glob_matches(glob, path)));
    paths.sort();
    paths.dedup();
    Ok(paths
        .into_iter()
        .filter_map(|path| read(root, path))
        .collect())
}

/// The document at `path` under `root`: none for a symbolic link, else its
/// text, or why its metadata or text could not be read (a tracked file
/// deleted from the checkout among them).
fn read(root: &Path, path: String) -> Option<RelatedDocument> {
    let full = root.join(&path);
    let content = match std::fs::symlink_metadata(&full) {
        Ok(metadata) if metadata.is_symlink() => return None,
        Ok(_) => std::fs::read_to_string(&full).map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    Some(RelatedDocument { path, content })
}

/// `git ls-files` of the files tracked or untracked and not ignored under
/// the globs' literal leading segments (the root for a glob that starts
/// with a pattern).
fn list_files(root: &Path, globs: &[String]) -> Result<Vec<String>> {
    let mut command = Command::new(git_executable()?);
    command.arg("-C").arg(root).args([
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
        "--",
    ]);
    for glob in globs {
        let literal: Vec<&str> = glob
            .split('/')
            .take_while(|segment| !segment.contains(['*', '?']))
            .collect();
        let literal = literal.join("/");
        if literal.is_empty() {
            command.arg(".");
        } else {
            command.arg(format!(":(literal){literal}"));
        }
    }
    let output = command
        .output()
        .with_context(|| format!("list the related documents in {}", root.display()))?;
    ensure!(
        output.status.success(),
        "git ls-files in {} failed: {}",
        root.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|path| std::str::from_utf8(path).ok())
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}
