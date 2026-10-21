//! The build identifier a binary names itself by (ADR-0045 decision 2).
//! dagq's build script reads this file (`#[path]`) and calls [`emit`], which
//! embeds it as `DAGQ_BUILD_ID`; the runtime reads the commit an identifier
//! names through [`named_commit`].
//!
//! A release (a version without a pre-release) is its version alone, `X.Y.Z`.
//! A development version names the commit it was built from as SemVer build
//! metadata, `X.Y.Z-dev+<commit>`, with `.dirty` when the worktree had
//! uncommitted changes, and `+unknown` when it was built outside a Git
//! repository (from the crates.io source, say, or without `git`).

use std::path::{Path, PathBuf};
use std::process::Command;

/// The metadata of a build that could not name its commit.
pub const UNKNOWN_COMMIT: &str = "unknown";

/// The environment variable [`emit`] sets for the crate's compilation.
pub const ENV: &str = "DAGQ_BUILD_ID";

/// What the build depends on: a change to any of them reruns the build
/// script, relative to the repository root.
pub const SOURCES: [&str; 5] = ["src", "migrations", "build.rs", "Cargo.toml", "Cargo.lock"];

/// The build identifier of `version` built from `commit` (`None` when it is
/// not known), `dirty` when the worktree had uncommitted changes.
pub fn build_identifier(version: &str, commit: Option<&str>, dirty: bool) -> String {
    if !is_prerelease(version) {
        return version.to_owned();
    }
    match commit {
        Some(commit) if dirty => format!("{version}+{commit}.dirty"),
        Some(commit) => format!("{version}+{commit}"),
        None => format!("{version}+{UNKNOWN_COMMIT}"),
    }
}

/// The commit a build identifier names: `<commit>` of
/// `X.Y.Z-dev+<commit>[.dirty]`, a dirty build taken as its commit (it was
/// built from that commit with local changes on top). `None` for a release
/// (`X.Y.Z`) or a build that did not know its commit (`+unknown`).
pub fn named_commit(build: &str) -> Option<&str> {
    let (_, metadata) = build.split_once('+')?;
    let commit = metadata.strip_suffix(".dirty").unwrap_or(metadata);
    (commit != UNKNOWN_COMMIT && !commit.is_empty()).then_some(commit)
}

/// Whether `version` has a SemVer pre-release, such as `0.4.0-dev`. Build
/// metadata is not part of the version Cargo gives, so any `-` is one.
pub fn is_prerelease(version: &str) -> bool {
    version.split('+').next().unwrap_or(version).contains('-')
}

/// For a build script: print the directives that embed the build identifier
/// of this package as [`ENV`] and rerun the script when it may change. The
/// package's manifest directory is the repository root.
pub fn emit() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("cargo sets {name}"));
    let root = PathBuf::from(var("CARGO_MANIFEST_DIR"));
    let build = compute(&var("CARGO_PKG_NAME"), &var("CARGO_PKG_VERSION"), &root);
    for directive in build.directives {
        println!("{directive}");
    }
}

/// What [`emit`] prints for `package` at `version` under `root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub identifier: String,
    /// The `cargo:` lines, the identifier's last.
    pub directives: Vec<String>,
}

/// The build identifier of `package` at `version` whose repository root is
/// `root`, and the directives a build script prints for it.
///
/// For a pre-release, the reruns on [`SOURCES`] are registered before the
/// Git query, so a build that fell back to `unknown` is looked at again once
/// the sources change rather than kept until build.rs itself does.
///
/// `cargo package` and `cargo publish --dry-run` verify the copy unpacked in
/// `target/package/` with the checkout's own target directory, and cargo
/// takes that build for the checkout's (the unit hash does not depend on
/// where the package sits), so the copy's `unknown` output could be reused
/// by the checkout's next build. When the identifier is `unknown`, the
/// copy's missing `.git` at its repository root is watched too; a watched
/// path that is missing always reruns the script, so the checkout's next
/// build names its commit again.
pub fn compute(package: &str, version: &str, root: &Path) -> Build {
    let mut directives = vec!["cargo:rerun-if-changed=build.rs".to_owned()];
    let state = if is_prerelease(version) {
        for path in SOURCES {
            directives.push(rerun(&root.join(path)));
        }
        let state = git_state(root, &mut directives);
        if state.is_none() {
            directives.push(rerun(&root.join(".git")));
            directives.push(format!(
                "cargo:warning={package} {version} is built outside a Git worktree or without git; \
                 its build identifier is {version}+{UNKNOWN_COMMIT}"
            ));
        }
        state
    } else {
        None
    };
    let (commit, dirty) = match &state {
        Some((commit, dirty)) => (Some(commit.as_str()), *dirty),
        None => (None, false),
    };
    let identifier = build_identifier(version, commit, dirty);
    directives.push(format!("cargo:rustc-env={ENV}={identifier}"));
    Build {
        identifier,
        directives,
    }
}

fn rerun(path: &Path) -> String {
    format!("cargo:rerun-if-changed={}", path.display())
}

/// The commit `HEAD` names and whether the worktree has uncommitted changes,
/// or `None` when `root` is not the root of a Git worktree (a crates.io
/// source unpacked somewhere, possibly inside another repository) or `git`
/// cannot answer. It watches the files [`git_watched`] picks, so the
/// identifier follows commits, checkouts and staging.
fn git_state(root: &Path, directives: &mut Vec<String>) -> Option<(String, bool)> {
    let toplevel = git(root, &["rev-parse", "--show-toplevel"])?;
    if !same_path(Path::new(&toplevel), root) {
        return None;
    }
    let branch = git(root, &["symbolic-ref", "-q", "HEAD"]);
    let candidates: Vec<(PathBuf, bool)> = git_candidates(branch.as_deref())
        .into_iter()
        .filter_map(|name| git(root, &["rev-parse", "--git-path", &name]))
        .map(|path| {
            let path = root.join(path);
            let exists = path.is_file();
            (path, exists)
        })
        .collect();
    for path in git_watched(candidates) {
        directives.push(rerun(&path));
    }
    let commit = git(root, &["rev-parse", "--verify", "HEAD"])?;
    let status = git(root, &["status", "--porcelain"])?;
    Some((commit, !status.is_empty()))
}

/// The Git paths (names for `git rev-parse --git-path`) that may tell this
/// worktree moved to another commit or staged something, when `branch` is
/// the ref `HEAD` is on (`None` when detached).
///
/// `HEAD` changes on a checkout to another branch or commit, and the index
/// on staging. `logs/HEAD`, this worktree's reflog, gains a line on every
/// commit and checkout, also when the branch's ref is only in
/// `packed-refs` and the commit writes a loose ref that was not there to be
/// watched. `packed-refs` and the branch's loose ref name the commit
/// directly. Only this branch's ref is a candidate, not all of `refs`: the
/// refs directory is shared by every worktree, and a commit in another one
/// must not rebuild this one.
fn git_candidates(branch: Option<&str>) -> Vec<String> {
    let mut names: Vec<String> = ["HEAD", "logs/HEAD", "index", "packed-refs"]
        .map(str::to_owned)
        .into();
    names.extend(branch.map(str::to_owned));
    names
}

/// The paths to watch of `candidates`, each a path [`git_candidates`] names
/// and whether it is a file now. A missing one is left out: cargo takes a
/// watched path that is missing as changed every time, so it would rerun
/// the script and rebuild the crate on every cargo command (a fresh
/// repository has no `packed-refs`, and a packed branch has no loose ref).
/// With the reflog Git keeps by default (`core.logAllRefUpdates`) and refs
/// in files, the reflog, `HEAD` and the index are there in a worktree whose
/// commit was made by `git add` and `git commit`, so the identifier still
/// follows commits, checkouts and staging; without the reflog, a commit
/// that leaves the index alone on a branch only in `packed-refs` does not
/// rerun the script until something else does.
fn git_watched(candidates: Vec<(PathBuf, bool)>) -> Vec<PathBuf> {
    candidates
        .into_iter()
        .filter_map(|(path, exists)| exists.then_some(path))
        .collect()
}

/// Whether `a` and `b` are the same existing directory.
fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// The trimmed stdout of a successful `git` in `dir`.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_names_its_commit_dirty_or_not_and_a_release_names_none() {
        assert_eq!(named_commit("0.4.0-dev+abc123"), Some("abc123"));
        assert_eq!(named_commit("0.4.0-dev+abc123.dirty"), Some("abc123"));
        for build in [
            "0.4.0",
            "0.4.0-dev+unknown",
            "0.4.0-dev+",
            "0.4.0-dev+.dirty",
        ] {
            assert_eq!(named_commit(build), None, "{build}");
        }
    }

    #[test]
    fn a_release_names_its_version_alone() {
        assert_eq!(build_identifier("0.4.0", Some("abc"), true), "0.4.0");
        assert_eq!(build_identifier("0.4.0", None, false), "0.4.0");
    }

    #[test]
    fn a_development_version_names_its_commit_and_dirtiness() {
        let commit = "89e8c54ed2a851920a44435bf3868683d33f6a45";
        assert_eq!(
            build_identifier("0.4.0-dev", Some(commit), false),
            format!("0.4.0-dev+{commit}")
        );
        assert_eq!(
            build_identifier("0.4.0-dev", Some(commit), true),
            format!("0.4.0-dev+{commit}.dirty")
        );
    }

    #[test]
    fn a_development_version_outside_git_is_unknown() {
        assert_eq!(
            build_identifier("0.4.0-dev", None, true),
            "0.4.0-dev+unknown"
        );
    }

    #[test]
    fn only_a_pre_release_is_one() {
        assert!(is_prerelease("0.4.0-dev"));
        assert!(is_prerelease("1.0.0-rc.1+abc"));
        assert!(!is_prerelease("0.4.0"));
        assert!(!is_prerelease("0.4.0+abc-def"));
    }

    #[test]
    fn the_candidates_are_this_worktrees_files_and_its_branchs_ref_not_all_refs() {
        let on_branch = git_candidates(Some("refs/heads/dagq/run"));
        assert_eq!(
            on_branch,
            [
                "HEAD",
                "logs/HEAD",
                "index",
                "packed-refs",
                "refs/heads/dagq/run"
            ]
        );
        assert_eq!(
            git_candidates(None),
            ["HEAD", "logs/HEAD", "index", "packed-refs"]
        );
        for name in on_branch {
            assert!(!["refs", "refs/heads", "refs/heads/dagq"].contains(&name.as_str()));
        }
    }

    fn candidates(missing: &[&str]) -> Vec<(PathBuf, bool)> {
        git_candidates(Some("refs/heads/main"))
            .into_iter()
            .map(|name| {
                let exists = !missing.contains(&name.as_str());
                (PathBuf::from(".git").join(name), exists)
            })
            .collect()
    }

    fn watched(missing: &[&str]) -> Vec<String> {
        git_watched(candidates(missing))
            .into_iter()
            .map(|path| path.display().to_string())
            .collect()
    }

    #[test]
    fn a_missing_file_is_not_watched_and_the_reflog_head_and_index_still_are() {
        assert_eq!(
            watched(&[]),
            [
                ".git/HEAD",
                ".git/logs/HEAD",
                ".git/index",
                ".git/packed-refs",
                ".git/refs/heads/main"
            ]
        );
        // The branch's ref only in packed-refs.
        assert_eq!(
            watched(&["refs/heads/main"]),
            [
                ".git/HEAD",
                ".git/logs/HEAD",
                ".git/index",
                ".git/packed-refs"
            ]
        );
        // A repository without packed-refs.
        assert_eq!(
            watched(&["packed-refs"]),
            [
                ".git/HEAD",
                ".git/logs/HEAD",
                ".git/index",
                ".git/refs/heads/main"
            ]
        );
        assert_eq!(
            watched(&[
                "HEAD",
                "logs/HEAD",
                "index",
                "packed-refs",
                "refs/heads/main"
            ]),
            Vec::<String>::new()
        );
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn identifier_line(build: &Build) -> &str {
        build.directives.last().unwrap()
    }

    #[test]
    fn a_release_does_not_ask_git() {
        let dir = tempfile::tempdir().unwrap();
        let build = compute("dagq", "0.4.0", dir.path());
        assert_eq!(build.identifier, "0.4.0");
        assert_eq!(
            build.directives,
            [
                "cargo:rerun-if-changed=build.rs".to_owned(),
                "cargo:rustc-env=DAGQ_BUILD_ID=0.4.0".to_owned()
            ]
        );
    }

    #[test]
    fn a_development_build_names_the_commit_of_its_root_and_whether_it_is_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("Cargo.toml"), "").unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "first"]);
        let commit = git(root, &["rev-parse", "HEAD"]).unwrap();

        let build = compute("dagq", "0.4.0-dev", root);
        assert_eq!(build.identifier, format!("0.4.0-dev+{commit}"));
        assert_eq!(
            identifier_line(&build),
            format!("cargo:rustc-env=DAGQ_BUILD_ID=0.4.0-dev+{commit}")
        );
        for path in SOURCES {
            let line = rerun(&root.join(path));
            assert!(build.directives.contains(&line), "{line}");
        }
        // The branch's ref is watched, so a commit reruns the script.
        assert!(
            build
                .directives
                .iter()
                .any(|line| line.ends_with("refs/heads/main")),
            "{:?}",
            build.directives
        );
        assert!(!build.directives.iter().any(|l| l.contains("warning")));

        std::fs::write(root.join("Cargo.toml"), "# edited\n").unwrap();
        let build = compute("dagq", "0.4.0-dev", root);
        assert_eq!(build.identifier, format!("0.4.0-dev+{commit}.dirty"));

        // A directory that is not itself the root is not the checkout's (a
        // copy unpacked in target/package/, say).
        let nested = root.join("target/package/dagq");
        std::fs::create_dir_all(&nested).unwrap();
        let build = compute("dagq", "0.4.0-dev", &nested);
        assert_eq!(build.identifier, "0.4.0-dev+unknown");
    }

    #[test]
    fn a_development_build_outside_git_is_unknown_and_reruns() {
        let dir = tempfile::tempdir().unwrap();
        let build = compute("dagq", "0.4.0-dev", dir.path());
        assert_eq!(build.identifier, "0.4.0-dev+unknown");
        assert!(build.directives.contains(&rerun(&dir.path().join(".git"))));
        assert!(
            build.directives.iter().any(|line| line
                .starts_with("cargo:warning=dagq 0.4.0-dev is built outside a Git worktree")),
            "{:?}",
            build.directives
        );
    }
}
