//! The snapshot of a run's work the trusted runtime saves before it parks
//! the run for a replanning (ADR-t1521-2 decision 2,
//! docs/plans/task-replanning.md, "snapshotと保存の
//! 順"): the run's base and head, its head pinned by a ref of the
//! repository, the staged and the unstaged diff, the untracked files, the
//! receipt, the run's logs and the task's conditions, each file with its
//! size and SHA-256 in a manifest. It is made in a staging directory under
//! the queue's `replans/`, which the worker does not write, checked against
//! its manifest and renamed whole into place; the queue records it only
//! then. A continuation compares the run's worktree with it before the run
//! resumes ([`check_restore`]).

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::task_replan::AcceptanceItem;
use super::{DomainError, ReplanId, RunId, TaskId};

/// The most bytes one saved file may hold: a larger one is not left out,
/// it fails the save.
pub const MAX_FILE_BYTES: u64 = 64 << 20;

/// The most bytes a snapshot's files may hold together.
pub const MAX_TOTAL_BYTES: u64 = 512 << 20;

/// The manifest's file in a snapshot's directory.
pub const MANIFEST: &str = "manifest.json";

/// The ID of the snapshot of operation `op` saved under `generation`: one
/// per generation, so a save tried again after a restart finds its own.
pub fn snapshot_id(op: ReplanId, generation: i64) -> String {
    format!("replan-{op}-g{generation}")
}

/// The ref that pins the head of operation `op`'s run: what the snapshot
/// names stays reachable whatever becomes of the run's branch.
pub fn pinned_ref(op: ReplanId) -> String {
    format!("refs/dagq/replans/{op}")
}

/// The directory of snapshot `id` under the queue's `root` (`replans/`).
pub fn snapshot_dir(root: &Path, id: &str) -> PathBuf {
    root.join(id)
}

/// Where the restore of the worktree `worktree` puts the work together
/// before it moves it into place: a scratch worktree beside it, named
/// after its run (the run directory it is in) as the run's worktree is
/// ([`super::worktree_name`]), so its record in `.git/worktrees`, which
/// the move keeps, has a name no other run's has, whichever layout the
/// run's worktree has.
pub fn scratch_worktree(worktree: &Path) -> PathBuf {
    let run = worktree
        .parent()
        .and_then(Path::file_name)
        .map_or_else(|| "run".into(), |run| run.to_string_lossy().into_owned());
    worktree.with_file_name(format!("{}.restoring", super::worktree_name(&run)))
}

/// The scratch worktree a restore of `worktree` made before the scratches
/// were named after their run (`<worktree name>.restoring`, so
/// `worktree.restoring` beside a worktree of the old layout), when it is
/// not [`scratch_worktree`]: a restore that stopped then may have left it
/// with the run's branch checked out, which the next restore discards
/// first.
pub fn legacy_scratch_worktree(worktree: &Path) -> Option<PathBuf> {
    let name = worktree.file_name()?.to_string_lossy().into_owned();
    let legacy = worktree.with_file_name(format!("{name}.restoring"));
    (legacy != scratch_worktree(worktree)).then_some(legacy)
}

/// The directory process `pid` makes snapshot `id` in before it renames it
/// into place: one per process, so two supervisors saving the same
/// generation do not write into one another's.
pub fn staging_dir(root: &Path, id: &str, pid: u32) -> PathBuf {
    root.join(format!("{id}.partial-{pid}"))
}

// What a saved file holds.
string_enum!(SavedKind {
    /// The staged diff against the head (`git diff --cached --binary`).
    Index => "index",
    /// The unstaged diff against the index (`git diff --binary`).
    Worktree => "worktree",
    /// An untracked file of the worktree, by its path there.
    Untracked => "untracked",
    /// The run's receipt as it was found, accepted or not.
    Receipt => "receipt",
    /// A log or a record of the run directory: the session's, the review's,
    /// the validation's.
    Log => "log",
    /// The task's conditions and what is left of them.
    Conditions => "conditions",
});

/// One file of a snapshot: its name in the snapshot's directory, what it
/// holds, where it came from, its size and its SHA-256, and, for an
/// untracked file, whether it was executable (its owner's execute bit),
/// which a restore gives back and compares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedFile {
    pub name: String,
    pub kind: SavedKind,
    pub source: String,
    pub bytes: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub executable: bool,
}

impl SavedFile {
    /// The record of `contents`, saved as `name` from `source`.
    pub fn of(
        name: impl Into<String>,
        kind: SavedKind,
        source: impl Into<String>,
        contents: &[u8],
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            source: source.into(),
            bytes: contents.len() as u64,
            sha256: sha256(contents),
            executable: false,
        }
    }

    /// The record, saying whether the file was executable.
    pub fn executable(self, executable: bool) -> Self {
        Self { executable, ..self }
    }
}

/// The SHA-256 of `bytes`, in lowercase hex.
pub fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The manifest of a snapshot: whose work, which operation and generation
/// saved it, the run's base and head and the ref that pins it, whether the
/// worktree held uncommitted work, every saved file, what was left out on
/// purpose (regenerated build output) and the task's conditions it was
/// saved under, with the acceptance items still to meet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub id: String,
    pub operation: ReplanId,
    pub generation: i64,
    pub run_id: RunId,
    pub task_id: TaskId,
    pub base_commit: String,
    pub head: String,
    pub branch: Option<String>,
    pub pinned_ref: String,
    pub worktree: String,
    pub dirty: bool,
    pub files: Vec<SavedFile>,
    pub excluded: Vec<String>,
    pub conditions_version: String,
    pub remaining: Vec<AcceptanceItem>,
    pub saved_at: i64,
}

impl SnapshotManifest {
    /// Refuse the manifest when a path a restore joins is not a plain
    /// relative one ([`check_relative`]): each file's `name` in the
    /// snapshot's directory, and an untracked file's `source` in the
    /// worktree. The other sources are only recorded (a log's absolute
    /// path, a diff's command), never joined.
    pub fn check_paths(&self) -> Result<(), DomainError> {
        for file in &self.files {
            check_relative(&file.name)?;
            if file.kind == SavedKind::Untracked {
                check_relative(&file.source)?;
            }
        }
        Ok(())
    }

    /// The worktree as the snapshot saw it.
    pub fn state(&self) -> WorktreeState {
        let hash = |kind: SavedKind| {
            self.files
                .iter()
                .find(|file| file.kind == kind)
                .map_or_else(|| sha256(b""), |file| file.sha256.clone())
        };
        let mut untracked: Vec<UntrackedFile> = self
            .files
            .iter()
            .filter(|file| file.kind == SavedKind::Untracked)
            .map(|file| UntrackedFile {
                path: file.source.clone(),
                sha256: file.sha256.clone(),
                executable: file.executable,
            })
            .collect();
        untracked.sort();
        WorktreeState {
            head: self.head.clone(),
            index: hash(SavedKind::Index),
            worktree: hash(SavedKind::Worktree),
            untracked,
        }
    }
}

/// A worktree as the snapshot and the restore compare it: its head, the
/// SHA-256 of its staged and its unstaged diff (which carry the modes of
/// tracked files), and its untracked files, sorted by path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeState {
    pub head: String,
    pub index: String,
    pub worktree: String,
    pub untracked: Vec<UntrackedFile>,
}

/// An untracked file of a worktree: its path there, the SHA-256 of its
/// bytes and whether it is executable.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct UntrackedFile {
    pub path: String,
    pub sha256: String,
    pub executable: bool,
}

impl WorktreeState {
    /// Whether the worktree holds work that is not committed.
    pub fn dirty(&self) -> bool {
        let empty = sha256(b"");
        self.index != empty || self.worktree != empty || !self.untracked.is_empty()
    }
}

/// Whether an entry of the worktree may be saved: a regular file within
/// [`MAX_FILE_BYTES`] that keeps the snapshot within [`MAX_TOTAL_BYTES`]
/// (`total` counts what was saved before it). A link, a special file or
/// one too large fails the save rather than being left out.
pub fn check_entry(path: &str, regular: bool, bytes: u64, total: u64) -> Result<(), DomainError> {
    let refused = |why: String| DomainError::SnapshotRefused {
        path: path.to_owned(),
        why,
    };
    if !regular {
        return Err(refused("it is not a regular file".into()));
    }
    if bytes > MAX_FILE_BYTES {
        return Err(refused(format!(
            "it holds {bytes} bytes, past the {MAX_FILE_BYTES} a file may"
        )));
    }
    if total.saturating_add(bytes) > MAX_TOTAL_BYTES {
        return Err(refused(format!(
            "the snapshot would hold past the {MAX_TOTAL_BYTES} bytes it may"
        )));
    }
    Ok(())
}

/// Refuse a path a manifest names (a file's `name` in the snapshot's
/// directory, an untracked file's `source` in the worktree) unless it is a
/// relative path of plain names that stays inside the directory it is
/// joined to: an empty path or segment, an absolute one (which a join
/// would put in place of the directory), `..` and `.` are refused.
pub fn check_relative(path: &str) -> Result<(), DomainError> {
    let segments = path
        .split('/')
        .all(|segment| !matches!(segment, "" | "." | ".."));
    let plain = segments
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)));
    if plain {
        Ok(())
    } else {
        Err(DomainError::SnapshotPathRefused {
            path: path.to_owned(),
        })
    }
}

/// Refuse a save while a Git operation (`rebase`, `merge`, `cherry-pick`,
/// `revert`) is half done in the worktree: its state is more than the head
/// and the diffs, and the save keeps the run as it is rather than calling
/// the head its work.
pub fn check_git_operation(operation: Option<&str>) -> Result<(), DomainError> {
    match operation {
        Some(operation) => Err(DomainError::SnapshotRefused {
            path: ".git".into(),
            why: format!("a {operation} is in progress in the worktree"),
        }),
        None => Ok(()),
    }
}

/// Whether `saved` (each file of the directory as read back: its name,
/// size and SHA-256) is the whole of `manifest`: every file the manifest
/// names, with its size and hash.
pub fn check_saved(
    manifest: &SnapshotManifest,
    saved: &[(String, u64, String)],
) -> Result<(), DomainError> {
    for file in &manifest.files {
        let found = saved.iter().find(|(name, _, _)| *name == file.name);
        match found {
            Some((_, bytes, hash)) if *bytes == file.bytes && *hash == file.sha256 => {}
            Some(_) => {
                return Err(DomainError::SnapshotRefused {
                    path: file.name.clone(),
                    why: "it does not read back as saved".into(),
                });
            }
            None => {
                return Err(DomainError::SnapshotRefused {
                    path: file.name.clone(),
                    why: "it is missing from the snapshot".into(),
                });
            }
        }
    }
    Ok(())
}

/// Where a supervisor's start of the restore of a parked run stands: the
/// restore is one owner's from its start to its recorded result, the
/// run's lease being that ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreStart {
    /// The run is leased to this supervisor for the restore: only it
    /// touches the scratch worktree and records the result, while it still
    /// holds the lease.
    Owned,
    /// The operation's work is restored already: the run may resume.
    Done,
    /// Another process holds the run, or it or its operation changed: it is
    /// not restored here.
    Elsewhere,
}

/// What a continuation finds of the run's work before it resumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreCheck {
    /// The worktree is as the snapshot saved it: the run resumes there.
    Intact,
    /// The worktree is gone: it is made again from the snapshot.
    Missing,
    /// The worktree changed since the snapshot: neither is thrown away,
    /// and the run does not resume.
    Diverged(String),
}

/// Compare the worktree as it is now (`None` when it is gone) with the one
/// the snapshot saved.
pub fn check_restore(manifest: &SnapshotManifest, current: Option<&WorktreeState>) -> RestoreCheck {
    let Some(current) = current else {
        return RestoreCheck::Missing;
    };
    let saved = manifest.state();
    if current.head != saved.head {
        return RestoreCheck::Diverged(format!(
            "its head is {}, the snapshot's {}",
            current.head, saved.head
        ));
    }
    if current.index != saved.index || current.worktree != saved.worktree {
        return RestoreCheck::Diverged("its uncommitted changes differ from the snapshot's".into());
    }
    if current.untracked != saved.untracked {
        return RestoreCheck::Diverged("its untracked files differ from the snapshot's".into());
    }
    RestoreCheck::Intact
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(files: Vec<SavedFile>) -> SnapshotManifest {
        SnapshotManifest {
            id: snapshot_id(ReplanId::new(3), 2),
            operation: ReplanId::new(3),
            generation: 2,
            run_id: RunId::new("run-1").unwrap(),
            task_id: TaskId::new(7),
            base_commit: "b".into(),
            head: "h".into(),
            branch: Some("dagq/run-1".into()),
            pinned_ref: pinned_ref(ReplanId::new(3)),
            worktree: "/w".into(),
            dirty: true,
            files,
            excluded: Vec::new(),
            conditions_version: "v".into(),
            remaining: Vec::new(),
            saved_at: 0,
        }
    }

    #[test]
    fn the_names_follow_the_operation_and_its_generation() {
        assert_eq!(snapshot_id(ReplanId::new(3), 2), "replan-3-g2");
        assert_eq!(pinned_ref(ReplanId::new(3)), "refs/dagq/replans/3");
        let root = Path::new("/q/replans");
        assert_eq!(snapshot_dir(root, "replan-3-g2"), root.join("replan-3-g2"));
        for worktree in ["/q/runs/r1/worktree", "/q/runs/r1/worktree-r1"] {
            assert_eq!(
                scratch_worktree(Path::new(worktree)),
                Path::new("/q/runs/r1/worktree-r1.restoring")
            );
        }
        assert_eq!(
            legacy_scratch_worktree(Path::new("/q/runs/r1/worktree")),
            Some(PathBuf::from("/q/runs/r1/worktree.restoring"))
        );
        assert_eq!(
            legacy_scratch_worktree(Path::new("/q/runs/r1/worktree-r1")),
            None
        );
        assert_eq!(
            staging_dir(root, "replan-3-g2", 42),
            root.join("replan-3-g2.partial-42")
        );
    }

    #[test]
    fn an_entry_that_cannot_be_saved_whole_fails_the_save() {
        assert!(check_entry("a", true, 10, 0).is_ok());
        let link = check_entry("a", false, 10, 0).unwrap_err().to_string();
        assert!(link.contains("not a regular file"), "{link}");
        assert!(check_entry("a", true, MAX_FILE_BYTES, 0).is_ok());
        assert!(check_entry("a", true, MAX_FILE_BYTES + 1, 0).is_err());
        assert!(check_entry("a", true, 10, MAX_TOTAL_BYTES - 10).is_ok());
        assert!(check_entry("a", true, 10, MAX_TOTAL_BYTES - 5).is_err());
        assert!(check_git_operation(None).is_ok());
        let rebase = check_git_operation(Some("rebase")).unwrap_err().to_string();
        assert!(rebase.contains("rebase is in progress"), "{rebase}");
    }

    #[test]
    fn a_manifest_path_is_restored_only_as_plain_names_inside_its_directory() {
        for path in [
            "notes.txt",
            "untracked/0",
            "src/a b/c.rs",
            "..notes",
            "a..b/c",
        ] {
            assert!(check_relative(path).is_ok(), "{path}");
        }
        for path in [
            "",
            "/etc/passwd",
            "..",
            "../outside",
            "a/../../outside",
            "a/..",
            "./notes.txt",
            "a/./b",
            "a//b",
            "a/",
        ] {
            let refused = check_relative(path).unwrap_err().to_string();
            assert!(
                refused.contains("not a relative path inside"),
                "{path}: {refused}"
            );
        }
        // A manifest: an untracked file's source and every name are
        // checked; a log's absolute source is only recorded.
        let untracked =
            |source: &str| SavedFile::of("untracked/0", SavedKind::Untracked, source, b"x");
        let log = SavedFile::of(
            "logs/session.log",
            SavedKind::Log,
            "/q/runs/r1/session.log",
            b"l",
        );
        assert!(
            manifest(vec![untracked("notes.txt"), log.clone()])
                .check_paths()
                .is_ok()
        );
        for files in [
            vec![untracked("../outside.txt")],
            vec![untracked("/etc/passwd")],
            vec![SavedFile {
                name: "../manifest-of-another.json".into(),
                ..log.clone()
            }],
        ] {
            assert!(matches!(
                manifest(files).check_paths(),
                Err(DomainError::SnapshotPathRefused { .. })
            ));
        }
    }

    #[test]
    fn the_snapshot_reads_back_whole_or_not_at_all() {
        let file = SavedFile::of("index.patch", SavedKind::Index, "git diff --cached", b"x");
        let m = manifest(vec![file.clone()]);
        assert!(check_saved(&m, &[(file.name.clone(), 1, file.sha256.clone())]).is_ok());
        assert!(check_saved(&m, &[]).is_err());
        assert!(check_saved(&m, &[(file.name.clone(), 1, sha256(b"y"))]).is_err());
    }

    #[test]
    fn a_continuation_resumes_only_on_the_saved_work() {
        let m = manifest(vec![
            SavedFile::of("index.patch", SavedKind::Index, "git diff --cached", b"i"),
            SavedFile::of("worktree.patch", SavedKind::Worktree, "git diff", b"w"),
            SavedFile::of("untracked/0", SavedKind::Untracked, "notes.txt", b"n"),
            SavedFile::of("untracked/1", SavedKind::Untracked, "run.sh", b"r").executable(true),
        ]);
        let state = m.state();
        assert!(state.dirty());
        assert_eq!(check_restore(&m, Some(&state)), RestoreCheck::Intact);
        assert_eq!(check_restore(&m, None), RestoreCheck::Missing);
        let moved = WorktreeState {
            head: "other".into(),
            ..state.clone()
        };
        assert!(
            matches!(check_restore(&m, Some(&moved)), RestoreCheck::Diverged(why) if why.contains("other"))
        );
        let edited = WorktreeState {
            worktree: sha256(b"changed"),
            ..state.clone()
        };
        assert!(matches!(
            check_restore(&m, Some(&edited)),
            RestoreCheck::Diverged(_)
        ));
        // A script that lost its execute bit is not the saved work.
        let mut lost_mode = state.clone();
        lost_mode
            .untracked
            .iter_mut()
            .filter(|file| file.path == "run.sh")
            .for_each(|file| file.executable = false);
        assert!(matches!(
            check_restore(&m, Some(&lost_mode)),
            RestoreCheck::Diverged(_)
        ));
        let added = WorktreeState {
            untracked: vec![UntrackedFile {
                path: "new.txt".into(),
                sha256: sha256(b""),
                executable: false,
            }],
            ..state
        };
        assert!(matches!(
            check_restore(&m, Some(&added)),
            RestoreCheck::Diverged(_)
        ));
        let clean = manifest(Vec::new()).state();
        assert!(!clean.dirty());
    }
}
