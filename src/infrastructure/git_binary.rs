//! Resolve Git once per process, bypassing Apple's xcrun shim when possible.
use super::adapters::{capture, executable};
use anyhow::Result;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::LazyLock,
    time::Duration,
};

static GIT: LazyLock<Result<PathBuf, String>> = LazyLock::new(|| {
    executable(Path::new("git"))
        .map(|path| resolve_shim(path, cfg!(target_os = "macos"), find_developer_git))
        .map_err(|error| format!("{error:#}"))
});

/// Uses the first PATH resolution for this process. A failed shim lookup keeps
/// that Git; it does not make a working Git installation a startup error.
pub fn git_executable() -> Result<PathBuf> {
    GIT.clone().map_err(anyhow::Error::msg)
}

fn find_developer_git() -> Option<PathBuf> {
    let (status, stdout, _) = capture(
        Command::new("/usr/bin/xcrun").args(["--find", "git"]),
        Duration::from_secs(30),
    )
    .ok()?;
    status.success().then(|| PathBuf::from(stdout.trim()))
}

fn resolve_shim(original: PathBuf, macos: bool, find: impl FnOnce() -> Option<PathBuf>) -> PathBuf {
    if !macos || original != Path::new("/usr/bin/git") {
        return original;
    }
    let resolved = find().filter(|path| path.is_absolute()).and_then(|path| {
        let path = path.canonicalize().ok()?;
        let metadata = path.metadata().ok()?;
        (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(path)
    });
    resolved.unwrap_or(original)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn only_the_macos_system_git_needs_a_lookup() {
        for (macos, path) in [
            (false, "/usr/bin/git"),
            (true, "/opt/homebrew/bin/git"),
            (true, "/usr/local/bin/git"),
            (true, "/Library/Developer/CommandLineTools/usr/bin/git"),
            (
                true,
                "/Applications/Xcode.app/Contents/Developer/usr/bin/git",
            ),
        ] {
            assert_eq!(
                resolve_shim(path.into(), macos, || panic!("not a shim")),
                Path::new(path)
            );
        }
    }

    #[test]
    fn the_shim_uses_the_developer_git_including_paths_with_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join("Xcode Preview.app/git");
        fs::create_dir_all(git.parent().unwrap()).unwrap();
        fs::write(&git, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            resolve_shim("/usr/bin/git".into(), true, || Some(git.clone())),
            git.canonicalize().unwrap()
        );
    }

    #[test]
    fn failed_or_unusable_lookups_keep_the_original_git() {
        let dir = tempfile::tempdir().unwrap();
        let non_executable = dir.path().join("git");
        fs::write(&non_executable, "not executable").unwrap();
        fs::set_permissions(&non_executable, fs::Permissions::from_mode(0o644)).unwrap();
        for result in [
            None,
            Some(PathBuf::new()),
            Some("relative/git".into()),
            Some(dir.path().join("missing")),
            Some(dir.path().to_owned()),
            Some(non_executable),
            Some("/usr/bin/git".into()),
        ] {
            assert_eq!(
                resolve_shim("/usr/bin/git".into(), true, || result),
                Path::new("/usr/bin/git")
            );
        }
    }

    #[test]
    fn process_resolution_is_shared_by_callers() {
        let first = git_executable().unwrap();
        assert!(first.is_absolute());
        for _ in 0..3 {
            assert_eq!(git_executable().unwrap(), first);
        }
    }
}
