//! The build identifier a binary names itself by (ADR-0045 decision 2), the
//! one rule `dagq`, `dagq-broker` and `dagq-broker-client` share (ADR-t827-1
//! decisions 5 and 7): each build script calls [`emit`], which embeds it as
//! `DAGQ_BUILD_ID`, so the three binaries of one checkout name the same
//! build.
//!
//! A release (a version without a pre-release) is its version alone, `X.Y.Z`.
//! A development version names the commit it was built from as SemVer build
//! metadata, `X.Y.Z-dev+<commit>`, with `.dirty` when the worktree had
//! uncommitted changes, and `+unknown` when it was built outside a Git
//! repository (from the crates.io source, say, or without `git`).

use std::path::Path;
use std::process::Command;

/// The metadata of a build that could not name its commit.
pub const UNKNOWN_COMMIT: &str = "unknown";

/// The environment variable [`emit`] sets for the crate's compilation.
pub const ENV: &str = "DAGQ_BUILD_ID";

/// The environment variable that gives a build without Git the identifier
/// of the dagq it goes with: the broker's image is built in a container
/// that has no checkout, and dagq passes its own build here (a build
/// argument of the image), so the server's health names dagq's build
/// (ADR-t827-1 decisions 6 and 7).
pub const GIVEN_ENV: &str = "DAGQ_BROKER_IMAGE_BUILD";

/// The identifier `given` ([`GIVEN_ENV`]) names for a package at
/// `version`: only one of that version (`version` itself or
/// `version+<metadata>`), so a mistaken value cannot name another release.
pub fn given_identifier(version: &str, given: Option<&str>) -> Option<String> {
    let given = given?.trim();
    let of_version = given == version
        || given
            .strip_prefix(version)
            .and_then(|rest| rest.strip_prefix('+'))
            .is_some_and(|metadata| {
                !metadata.is_empty()
                    && metadata
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            });
    of_version.then(|| given.to_owned())
}

/// What the build of a package under the repository root depends on: a
/// change to any of them reruns the build script, relative to the root.
/// crates/ is part of what a build of dagq ships (ADR-t827-1), and every
/// package watches the same set, so an edit to any of them marks all the
/// binaries of the checkout dirty together.
pub const SOURCES: [&str; 6] = [
    "src",
    "crates",
    "migrations",
    "build.rs",
    "Cargo.toml",
    "Cargo.lock",
];

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

/// Whether `version` has a SemVer pre-release, such as `0.4.0-dev`. Build
/// metadata is not part of the version Cargo gives, so any `-` is one.
pub fn is_prerelease(version: &str) -> bool {
    version.split('+').next().unwrap_or(version).contains('-')
}

/// For a build script: print the directives that embed the build identifier
/// of this package as [`ENV`] and rerun the script when it may change.
/// `root` is the repository root relative to the package's manifest
/// directory (`.` for dagq, `../..` for a crate under crates/).
pub fn emit(root: &str) {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("cargo sets {name}"));
    let manifest_dir = std::path::PathBuf::from(var("CARGO_MANIFEST_DIR"));
    let package = var("CARGO_PKG_NAME");
    let root = match root {
        "." => manifest_dir,
        // Only a crate that sits at crates/<its name> of the root is the
        // checkout's; one unpacked elsewhere (vendored inside another
        // repository, say) looks at itself, which no Git toplevel is.
        relative => {
            let root = manifest_dir.join(relative);
            if same_path(&root.join("crates").join(&package), &manifest_dir) {
                root
            } else {
                manifest_dir
            }
        }
    };
    let version = var("CARGO_PKG_VERSION");
    println!("cargo:rerun-if-env-changed={GIVEN_ENV}");
    if let Some(given) = given_identifier(&version, std::env::var(GIVEN_ENV).ok().as_deref()) {
        println!("cargo:rustc-env={ENV}={given}");
        return;
    }
    let build = compute(&package, &version, &root);
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
pub fn compute(package: &str, version: &str, root: &Path) -> Build {
    let mut directives = vec!["cargo:rerun-if-changed=build.rs".to_owned()];
    let state = if is_prerelease(version) {
        // Registered before anything can fail, so a build that fell back to
        // `unknown` is looked at again once the sources change rather than
        // kept until build.rs itself does.
        for path in SOURCES {
            directives.push(rerun(&root.join(path)));
        }
        let state = git_state(root, &mut directives);
        if state.is_none() {
            // `cargo package` verifies the unpacked copy in
            // target/package/ with the checkout's own target directory, and
            // cargo takes that build for the checkout's (the unit hash does
            // not depend on where the package sits). The copy has no `.git`,
            // and a watched path that is missing always reruns the script,
            // so the checkout's next build names its commit again.
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
/// cannot answer.
fn git_state(root: &Path, directives: &mut Vec<String>) -> Option<(String, bool)> {
    let toplevel = git(root, &["rev-parse", "--show-toplevel"])?;
    if !same_path(Path::new(&toplevel), root) {
        return None;
    }
    // Rerun when this worktree moves to another commit (HEAD, and the ref of
    // the branch it is on) or its index changes, so the identifier follows
    // commits and checkouts. Only this branch's ref is watched: the refs
    // directory is shared by every worktree, and a commit in another one
    // must not rebuild this one. An edit elsewhere in the worktree (docs,
    // say) marks the next build dirty only once something else reruns this
    // script.
    let mut watched = vec![
        "HEAD".to_owned(),
        "index".to_owned(),
        "packed-refs".to_owned(),
    ];
    watched.extend(git(root, &["symbolic-ref", "-q", "HEAD"]));
    for name in watched {
        if let Some(path) = git(root, &["rev-parse", "--git-path", &name]) {
            directives.push(rerun(&root.join(path)));
        }
    }
    let commit = git(root, &["rev-parse", "--verify", "HEAD"])?;
    let status = git(root, &["status", "--porcelain"])?;
    Some((commit, !status.is_empty()))
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
    fn a_given_identifier_is_taken_only_for_its_own_version() {
        assert_eq!(
            given_identifier("0.4.0-dev", Some("0.4.0-dev+abc.dirty")).as_deref(),
            Some("0.4.0-dev+abc.dirty")
        );
        assert_eq!(
            given_identifier("0.4.0", Some("0.4.0")).as_deref(),
            Some("0.4.0")
        );
        for other in ["", "0.3.0", "0.4.0", "0.4.0-dev+", "0.4.0-dev+a b"] {
            assert_eq!(given_identifier("0.4.0-dev", Some(other)), None, "{other}");
        }
        assert_eq!(given_identifier("0.4.0", None), None);
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

        let build = compute("dagq-broker", "0.4.0-dev", root);
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
        let build = compute("dagq-broker", "0.4.0-dev", root);
        assert_eq!(build.identifier, format!("0.4.0-dev+{commit}.dirty"));

        // A crate under crates/ reads the same root, so it names the same
        // build.
        let nested = root.join("crates/dagq-broker");
        std::fs::create_dir_all(&nested).unwrap();
        let build = compute("dagq-broker", "0.4.0-dev", &nested.join("../.."));
        assert_eq!(build.identifier, format!("0.4.0-dev+{commit}.dirty"));
        // But a package that is not itself the root is not the checkout's
        // (a copy unpacked in target/package/, say).
        let build = compute("dagq-broker", "0.4.0-dev", &nested);
        assert_eq!(build.identifier, "0.4.0-dev+unknown");
    }

    #[test]
    fn a_development_build_outside_git_is_unknown_and_reruns() {
        let dir = tempfile::tempdir().unwrap();
        let build = compute("dagq-broker-client", "0.4.0-dev", dir.path());
        assert_eq!(build.identifier, "0.4.0-dev+unknown");
        assert!(build.directives.contains(&rerun(&dir.path().join(".git"))));
        assert!(
            build.directives.iter().any(|line| line.starts_with(
                "cargo:warning=dagq-broker-client 0.4.0-dev is built outside a Git worktree"
            )),
            "{:?}",
            build.directives
        );
    }
}
