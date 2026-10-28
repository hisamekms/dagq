//! The git of a run in a container (ADR-t2113-2, ADR-t2179-1): the run
//! works in a clone of its own, which the worker writes and the control
//! side never runs git or a program in (decision 1). What the control side
//! knows of it comes from two files the container's executor writes in the
//! run directory, the run branch's bundle ([`RUN_BUNDLE_FILE`]) and the
//! run's state ([`RunState`], decision 2); the landing works in a worktree
//! of the control side's own ([`landing_path`], decision 5); main reaches
//! the clone as a bundle the control side writes ([`MAIN_BUNDLE_FILE`],
//! decision 6). The control side's git that handles a container run's
//! commits runs no hook and no driver the commits' tracked content could
//! choose ([`guard_overrides`], decision 8). The files and the commands
//! are the adapters'; this is what they decide with.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{CommitSha, RunId};

/// The directory of a queue that holds its container runs' clones,
/// `<queue dir>/clones/<run id>` (one of the directories the dagq machine
/// mounts, ADR-t2173-1 decision 1).
pub const CLONES_DIR: &str = "clones";

/// The directory of a queue that holds the landing worktrees of its
/// container runs, `<queue dir>/landing/<run id>`: the control side's
/// only, 0700, mounted into no machine and no container (ADR-t2179-1
/// decision 5).
pub const LANDING_DIR: &str = "landing";

/// The run branch's bundle in the run directory: a `git bundle` whose one
/// ref is `refs/heads/<run branch>` (`dagq/<run id>`) at the clone's head,
/// with the run's base commit as its prerequisite. The container's
/// executor writes it at the end of each turn that left the branch on a
/// commit other than the base, and removes it while the branch has no new
/// commit, writing a temporary file and renaming it so no reader sees a
/// part of one; the control side imports it ([`bundle_head`]) and reads it
/// never as a path git opens in place.
pub const RUN_BUNDLE_FILE: &str = "run-branch.bundle";

/// The run's state in the run directory ([`RunState`]), written by the
/// container's executor with the bundle, at the end of each turn, after the
/// bundle and the work ([`super::clone_work::RUN_WORK_FILE`]) and by a
/// rename like them; marked by a rename at the start of each turn, before
/// its agent runs ([`RunState::turn_ended`]).
pub const RUN_STATE_FILE: &str = "run-state.json";

/// Main's bundle in the run directory: a `git bundle` whose one ref is the
/// landing branch's (`refs/heads/<landing branch>`) at main's commit when
/// it was written, with the run's base commit as its prerequisite. The
/// control side writes it before it hands the session a request that may
/// ask for a rebase (a resume, a revise, a conflict); the container's
/// executor fetches it into [`CLONE_MAIN_REF`] at the start of the next
/// turn. None is written while main is still the base.
pub const MAIN_BUNDLE_FILE: &str = "main.bundle";

/// The ref of the clone that holds main as the control side last handed it
/// ([`MAIN_BUNDLE_FILE`]); the clone is made with it at the base commit.
/// A request that asks a container run to rebase names it, since the clone
/// has no remote and no other ref of the control side.
pub const CLONE_MAIN_REF: &str = "refs/remotes/dagq/main";

/// The hooks directory the guarded git is pointed at (`core.hooksPath`):
/// no hook is found there, whatever the repository or its tracked content
/// names.
pub const NO_HOOKS: &str = "/dev/null";

/// The largest run bundle or state file the control side reads: a run's
/// change, not a repository's history.
pub const RUN_FILE_LIMIT: u64 = 1 << 30;

/// The clone of the run `run` of the queue whose runs' directories are
/// `runs_dir` (`<queue dir>/runs`).
pub fn clone_path(runs_dir: &Path, run: &RunId) -> PathBuf {
    queue_dir(runs_dir).join(CLONES_DIR).join(run.as_str())
}

/// The landing worktree of the run `run` of that queue.
pub fn landing_path(runs_dir: &Path, run: &RunId) -> PathBuf {
    queue_dir(runs_dir).join(LANDING_DIR).join(run.as_str())
}

/// Whether `workplace`, a run's stored worktree path, is a clone
/// (`<queue dir>/clones/<run id>`) rather than a worktree
/// (`<queue dir>/runs/<run id>/worktree-<run id>`, or `.../worktree` of a
/// run made before): what tells a container run from
/// a host one for as long as the run is kept.
pub fn is_clone_path(workplace: &Path) -> bool {
    workplace
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == CLONES_DIR)
}

/// The landing worktree of the run whose clone is `clone`.
pub fn landing_of(clone: &Path) -> Option<PathBuf> {
    beside(clone, LANDING_DIR)
}

/// The directory of the run whose clone is `clone`.
pub fn run_dir_of(clone: &Path) -> Option<PathBuf> {
    beside(clone, "runs")
}

/// `<queue dir>/<dir>/<run id>` for the clone `<queue dir>/clones/<run id>`.
fn beside(clone: &Path, dir: &str) -> Option<PathBuf> {
    if !is_clone_path(clone) {
        return None;
    }
    Some(clone.parent()?.parent()?.join(dir).join(clone.file_name()?))
}

fn queue_dir(runs_dir: &Path) -> &Path {
    runs_dir.parent().unwrap_or(runs_dir)
}

/// The run's state as the container's executor read it in the clone at the
/// end of a turn ([`RUN_STATE_FILE`]), and marked at the start of the next
/// ([`Self::turn_ended`]), as JSON:
///
/// ```json
/// {"branch": "refs/heads/dagq/<run id>", "head": "<full commit id>",
///  "status": "<git status --porcelain --untracked-files=all>",
///  "rebase_in_progress": false, "turn_ended": true, "generation": 3}
/// ```
///
/// `branch` is the ref `HEAD` points at (`null` when detached), `head` the
/// commit of `HEAD`, `status` the porcelain output as it is (blank when
/// clean). It is the worker's word: a false one only keeps uncommitted
/// work from landing, since what lands is the imported commit the control
/// side checks itself (ADR-t2179-1 decision 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunState {
    pub branch: Option<String>,
    pub head: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub rebase_in_progress: bool,
    /// Whether the state is of the end of the last turn the executor
    /// started: `false` from that turn's start, before its agent runs,
    /// until its end writes the state again, so the state a turn cut off
    /// (its container gone with it) left is not taken for that turn's
    /// work; absent from an executor that marks no turn, whose state
    /// cannot tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_ended: Option<bool>,
    /// Which turn's end wrote it, counted up from 1 by each end
    /// ([`next_generation`]) and kept by the mark of a turn's start: the
    /// work written before it ([`super::clone_work::CloneWork::generation`])
    /// is of that end only when it names the same; absent from an executor
    /// that writes no work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
}

impl RunState {
    /// The state written as `text`; `Err` says why it cannot be read.
    pub fn parse(text: &str) -> Result<Self, String> {
        let state: Self = serde_json::from_str(text)
            .map_err(|error| format!("the run's state file is not valid: {error}"))?;
        CommitSha::parse(state.head.to_ascii_lowercase(), "head").map_err(|_| {
            format!(
                "the run's state names no commit as its head: {:?}",
                state.head
            )
        })?;
        Ok(state)
    }

    /// Its head, as a commit.
    pub fn head(&self) -> CommitSha {
        CommitSha::parse(self.head.to_ascii_lowercase(), "head")
            .expect("checked when the state was parsed")
    }

    /// Whether it says what the last turn the executor started left: only
    /// when that turn's end wrote it ([`Self::turn_ended`]).
    pub fn of_the_last_turn(&self) -> bool {
        self.turn_ended == Some(true)
    }
}

/// The generation of the state a turn's end writes, after the state
/// `last` found at its start (`None` when none could be read): one past
/// its generation, 1 for the first.
pub fn next_generation(last: Option<&RunState>) -> u64 {
    last.and_then(|state| state.generation).unwrap_or(0) + 1
}

/// What a container's executor finds where the run's state goes when a
/// turn starts ([`RUN_STATE_FILE`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateFound {
    Nothing,
    /// A plain file whose bytes read as a state.
    State(RunState),
    /// A plain file whose bytes are no state, or a symlink in its place.
    Unreadable,
    /// Something else in its place (a directory), which no reader takes
    /// for a state and the turn's end cannot write over either.
    Other,
}

/// What the executor does with the state it found before its agent runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStart {
    Leave,
    /// Write it back as of a turn that has not ended.
    Mark(RunState),
    Remove,
}

/// What a turn's start does with the state `found`, so that no facts an
/// earlier turn left stand as the last turn's while the agent runs: a
/// state is marked as of a turn that has not ended ([`RunState::turn_ended`]
/// `false`), its other facts as they were; what cannot be read as one is
/// removed; nothing, or something no reader takes for a state, is left.
pub fn turn_start(found: StateFound) -> TurnStart {
    match found {
        StateFound::State(state) => TurnStart::Mark(RunState {
            turn_ended: Some(false),
            ..state
        }),
        StateFound::Unreadable => TurnStart::Remove,
        StateFound::Nothing | StateFound::Other => TurnStart::Leave,
    }
}

/// The one head of a run bundle, from `git bundle list-heads` of it
/// (`<commit> <ref>` per line): `Err` unless it holds exactly one ref and
/// that ref is the run branch `branch` (ADR-t2179-1 decision 3). No other
/// ref of the clone is taken in.
pub fn bundle_head(list_heads: &str, branch: &str) -> Result<CommitSha, String> {
    let expected = format!("refs/heads/{branch}");
    let heads: Vec<&str> = list_heads.lines().filter(|line| !line.is_empty()).collect();
    let [line] = heads.as_slice() else {
        return Err(format!(
            "the run bundle holds {} refs, not just {expected}",
            heads.len()
        ));
    };
    let Some((commit, name)) = line.split_once(' ') else {
        return Err(format!("the run bundle lists an unreadable head: {line:?}"));
    };
    if name != expected {
        return Err(format!(
            "the run bundle holds {name}, not the run branch {expected}"
        ));
    }
    CommitSha::parse(commit.to_ascii_lowercase(), "bundle head")
        .map_err(|_| format!("the run bundle names no commit: {line:?}"))
}

/// What the control side reads of the landing directory ([`LANDING_DIR`])
/// that holds a run's landing worktree, without following a symlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LandingDirFacts {
    pub is_symlink: bool,
    pub is_dir: bool,
    pub uid: u32,
    /// The permission bits (`st_mode`).
    pub mode: u32,
}

/// Why no landing worktree is made in the landing directory `dir`: `Err`
/// unless it is a directory, not a symlink, owned by the control side's
/// effective user `own_uid` and writable by neither its group nor others
/// (the control side's only, ADR-t2179-1 decision 5). The run is held with
/// the reason.
pub fn landing_dir_check(facts: &LandingDirFacts, own_uid: u32, dir: &Path) -> Result<(), String> {
    if facts.is_symlink || !facts.is_dir {
        return Err(format!(
            "{} is a symlink or no directory, so no landing worktree is made in it",
            dir.display()
        ));
    }
    if facts.uid != own_uid || facts.mode & 0o022 != 0 {
        return Err(format!(
            "{} is not the control side's alone (another owner, or writable by its group or others), so no landing worktree is made in it",
            dir.display()
        ));
    }
    Ok(())
}

/// The driver keys whose program a tracked `.gitattributes` can choose by
/// naming the driver: a filter's clean, smudge and process, a diff's
/// textconv and command, a merge's driver.
const DRIVER_KEYS: [(&str, &str); 6] = [
    ("filter", "clean"),
    ("filter", "smudge"),
    ("filter", "process"),
    ("diff", "textconv"),
    ("diff", "command"),
    ("merge", "driver"),
];

/// What a diff's external command is replaced with: a plain `git diff`
/// of the two files git hands it (`$2` and `$5` of the seven arguments),
/// without any driver, so a person's or an agent's `git diff` still shows
/// the change.
const PLAIN_EXTERNAL_DIFF: &str =
    "git diff --no-index --no-ext-diff --no-textconv -- \"$2\" \"$5\" || :";

/// The control side's git configuration cannot be kept from running a hook
/// or a driver (ADR-t2179-1 decision 8): what the configuration is, unlike
/// a git that could not be run to read it, so it stays until a person sees
/// to it.
#[derive(Debug)]
pub struct GuardDoesNotHold(pub String);

impl std::fmt::Display for GuardDoesNotHold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GuardDoesNotHold {}

/// The configuration that keeps the control side's git from running a hook
/// or a driver the tracked content of a container run's commit could
/// choose (ADR-t2179-1 decision 8), for the configuration whose names are
/// `names` (`git config --list --name-only -z` of every scope, NUL
/// separated): `core.hooksPath` at [`NO_HOOKS`], and every driver some
/// scope defines made one that runs no program of the repository (a filter
/// with no command and not required, a textconv that copies, an external
/// diff that is git's own, a merge driver that always conflicts). Passed
/// as the command scope (`GIT_CONFIG_COUNT`), which wins over the system,
/// global, repository and worktree scopes; a driver no scope defines is
/// never run by git, whatever the attributes name. `Err` names a key that
/// cannot be overridden, and nothing is to run then.
pub fn guard_overrides(names: &str) -> Result<Vec<(String, String)>, String> {
    let mut overrides = vec![("core.hooksPath".to_owned(), NO_HOOKS.to_owned())];
    let mut push = |key: String, value: &str| {
        if !overrides.iter().any(|(k, _)| *k == key) {
            overrides.push((key, value.to_owned()));
        }
    };
    for name in names.split('\0').map(str::trim).filter(|n| !n.is_empty()) {
        let Some((section, rest)) = name.split_once('.') else {
            continue;
        };
        let Some((driver, key)) = rest.rsplit_once('.') else {
            continue;
        };
        let (section, key) = (section.to_ascii_lowercase(), key.to_ascii_lowercase());
        if !DRIVER_KEYS.iter().any(|(s, k)| *s == section && *k == key) {
            continue;
        }
        if driver.contains(['\n', '\0']) {
            return Err(format!(
                "the {section} driver {driver:?} cannot be overridden"
            ));
        }
        match section.as_str() {
            "filter" => {
                for key in ["clean", "smudge", "process"] {
                    push(format!("filter.{driver}.{key}"), "");
                }
                push(format!("filter.{driver}.required"), "false");
            }
            "diff" => {
                push(format!("diff.{driver}.textconv"), "cat");
                push(format!("diff.{driver}.command"), PLAIN_EXTERNAL_DIFF);
            }
            _ => push(format!("merge.{driver}.driver"), "false"),
        }
    }
    Ok(overrides)
}

/// Whether `effective`, the configuration (`git config --list -z`, each
/// entry `key\nvalue`) read with `overrides` in place, keeps every
/// override as the last value of its key: what proves the guard holds
/// before a container run's commit is handled. `Err` names a key that
/// does not.
pub fn guard_holds(effective: &str, overrides: &[(String, String)]) -> Result<(), String> {
    for (key, value) in overrides {
        let last = effective
            .split('\0')
            .filter_map(|entry| entry.split_once('\n').or(Some((entry, ""))))
            .filter(|(name, _)| name.eq_ignore_ascii_case(key) || same_key(name, key))
            .map(|(_, value)| value)
            .next_back();
        if last != Some(value.as_str()) {
            return Err(format!(
                "{key} is {} with the guard in place, not {value:?}",
                last.map_or("unset".to_owned(), |last| format!("{last:?}"))
            ));
        }
    }
    Ok(())
}

/// Whether two config keys name the same key: the section and the last
/// part case-insensitively, the subsection as it is.
fn same_key(a: &str, b: &str) -> bool {
    let split = |key: &str| -> Option<(String, String, String)> {
        let (section, rest) = key.split_once('.')?;
        let (sub, name) = rest.rsplit_once('.').unwrap_or(("", rest));
        Some((
            section.to_ascii_lowercase(),
            sub.to_owned(),
            name.to_ascii_lowercase(),
        ))
    };
    split(a).is_some_and(|a| split(b) == Some(a))
}

/// The environment that carries `overrides` to a git a process runs
/// (`GIT_CONFIG_COUNT`, `GIT_CONFIG_KEY_<n>`, `GIT_CONFIG_VALUE_<n>`), with
/// an inherited `GIT_CONFIG_PARAMETERS` (a parent's `-c`, which git reads
/// after them) emptied.
pub fn guard_env(overrides: &[(String, String)]) -> Vec<(String, String)> {
    let mut env = vec![
        ("GIT_CONFIG_PARAMETERS".to_owned(), String::new()),
        ("GIT_CONFIG_COUNT".to_owned(), overrides.len().to_string()),
    ];
    for (index, (key, value)) in overrides.iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{index}"), key.clone()));
        env.push((format!("GIT_CONFIG_VALUE_{index}"), value.clone()));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn a_runs_clone_and_landing_worktree_sit_beside_its_runs_directory() {
        let runs = Path::new("/q/runs");
        let run = RunId::new("r1").unwrap();
        assert_eq!(clone_path(runs, &run), Path::new("/q/clones/r1"));
        assert_eq!(landing_path(runs, &run), Path::new("/q/landing/r1"));
        assert!(is_clone_path(&clone_path(runs, &run)));
        assert!(!is_clone_path(Path::new("/q/runs/r1/worktree")));
        let clone = clone_path(runs, &run);
        assert_eq!(landing_of(&clone).unwrap(), landing_path(runs, &run));
        assert_eq!(run_dir_of(&clone).unwrap(), Path::new("/q/runs/r1"));
        assert_eq!(landing_of(Path::new("/q/runs/r1/worktree")), None);
    }

    #[test]
    fn the_state_is_read_and_a_head_that_is_no_commit_is_refused() {
        let state = RunState::parse(&format!(
            r#"{{"branch":"refs/heads/dagq/r1","head":"{HEAD}","status":" M a\n","rebase_in_progress":true}}"#
        ))
        .unwrap();
        assert_eq!(state.branch.as_deref(), Some("refs/heads/dagq/r1"));
        assert_eq!(state.head().as_str(), HEAD);
        assert!(state.rebase_in_progress);
        let detached = RunState::parse(&format!(r#"{{"branch":null,"head":"{HEAD}"}}"#)).unwrap();
        assert_eq!(detached.branch, None);
        assert_eq!(detached.status, "");
        assert!(RunState::parse(r#"{"branch":null,"head":"main"}"#).is_err());
        assert!(RunState::parse("not json").is_err());
        assert!(RunState::parse(&format!(r#"{{"head":"{HEAD}","extra":1}}"#)).is_err());
    }

    /// Only a state the last turn's end wrote is of that turn: one marked
    /// at a turn's start, or one with no mark at all, is not.
    #[test]
    fn a_state_is_of_the_last_turn_only_once_its_end_wrote_it() {
        let read = |mark: &str| RunState::parse(&format!(r#"{{"head":"{HEAD}"{mark}}}"#)).unwrap();
        let ended = read(r#","turn_ended":true"#);
        assert_eq!(ended.turn_ended, Some(true));
        assert!(ended.of_the_last_turn());
        let started = read(r#","turn_ended":false"#);
        assert_eq!(started.turn_ended, Some(false));
        assert!(!started.of_the_last_turn());
        let unmarked = read("");
        assert_eq!(unmarked.turn_ended, None);
        assert!(!unmarked.of_the_last_turn());
        assert!(
            !serde_json::to_string(&unmarked)
                .unwrap()
                .contains("turn_ended")
        );
        assert_eq!(
            RunState::parse(&serde_json::to_string(&ended).unwrap()).unwrap(),
            ended
        );
    }

    /// Each turn's end counts the generation up from the state it found,
    /// one with none (or none found) being of no generation yet.
    #[test]
    fn each_turns_end_writes_the_next_generation() {
        let read = |mark: &str| RunState::parse(&format!(r#"{{"head":"{HEAD}"{mark}}}"#)).unwrap();
        assert_eq!(next_generation(None), 1);
        assert_eq!(next_generation(Some(&read(""))), 1);
        let third = read(r#","turn_ended":false,"generation":3"#);
        assert_eq!(third.generation, Some(3));
        assert_eq!(next_generation(Some(&third)), 4);
        // The mark of a turn's start keeps it.
        assert_eq!(
            turn_start(StateFound::State(third.clone())),
            TurnStart::Mark(third)
        );
    }

    /// A turn's start marks a state, the last turn's end's or one with no
    /// mark, keeping its other facts; removes what is no state; and leaves
    /// nothing, or something no reader takes for a state, as it is.
    #[test]
    fn a_turn_s_start_marks_a_state_removes_what_is_no_state_and_leaves_the_rest() {
        let ended = RunState::parse(&format!(
            r#"{{"branch":null,"head":"{HEAD}","status":" M a\n","turn_ended":true}}"#
        ))
        .unwrap();
        let unmarked = RunState {
            turn_ended: None,
            ..ended.clone()
        };
        let marked = RunState {
            turn_ended: Some(false),
            ..ended.clone()
        };
        for state in [ended, unmarked] {
            assert_eq!(
                turn_start(StateFound::State(state)),
                TurnStart::Mark(marked.clone())
            );
        }
        assert_eq!(turn_start(StateFound::Unreadable), TurnStart::Remove);
        assert_eq!(turn_start(StateFound::Nothing), TurnStart::Leave);
        assert_eq!(turn_start(StateFound::Other), TurnStart::Leave);
    }

    #[test]
    fn a_bundle_lands_only_its_one_run_branch() {
        assert_eq!(
            bundle_head(&format!("{HEAD} refs/heads/dagq/r1\n"), "dagq/r1")
                .unwrap()
                .as_str(),
            HEAD
        );
        let two = format!("{HEAD} refs/heads/dagq/r1\n{HEAD} refs/heads/main\n");
        assert!(bundle_head(&two, "dagq/r1").unwrap_err().contains("2 refs"));
        assert!(
            bundle_head(&format!("{HEAD} refs/heads/main\n"), "dagq/r1")
                .unwrap_err()
                .contains("not the run branch")
        );
        assert!(bundle_head("", "dagq/r1").is_err());
        assert!(bundle_head("nonsense refs/heads/dagq/r1\n", "dagq/r1").is_err());
    }

    #[test]
    fn every_driver_a_scope_defines_is_made_harmless_and_hooks_are_off() {
        let names = "core.hookspath\0filter.lfs.process\0filter.lfs.required\0diff.pdf.textconv\0\
merge.ours.driver\0diff.Mixed.Case.command\0user.name\0";
        let overrides = guard_overrides(names).unwrap();
        let get = |key: &str| {
            overrides
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("core.hooksPath"), Some(NO_HOOKS));
        assert_eq!(get("filter.lfs.clean"), Some(""));
        assert_eq!(get("filter.lfs.smudge"), Some(""));
        assert_eq!(get("filter.lfs.process"), Some(""));
        assert_eq!(get("filter.lfs.required"), Some("false"));
        assert_eq!(get("diff.pdf.textconv"), Some("cat"));
        assert_eq!(get("diff.pdf.command"), Some(PLAIN_EXTERNAL_DIFF));
        assert_eq!(get("diff.Mixed.Case.command"), Some(PLAIN_EXTERNAL_DIFF));
        assert_eq!(get("merge.ours.driver"), Some("false"));
        assert_eq!(get("user.name"), None);
        assert!(guard_overrides("filter.a\nb.clean\0").is_err());
    }

    #[test]
    fn the_guard_holds_only_when_each_override_is_the_last_value() {
        let overrides = guard_overrides("filter.lfs.process\0").unwrap();
        let held = "core.hookspath\n.githooks\0filter.lfs.process\ngit-lfs filter-process\0\
core.hookspath\n/dev/null\0filter.lfs.clean\n\0filter.lfs.smudge\n\0filter.lfs.process\n\0\
filter.lfs.required\nfalse\0";
        assert_eq!(guard_holds(held, &overrides), Ok(()));
        let broken = "core.hookspath\n/dev/null\0filter.lfs.clean\n\0filter.lfs.smudge\n\0\
filter.lfs.process\n\0filter.lfs.required\nfalse\0filter.lfs.process\ngit-lfs filter-process\0";
        assert!(
            guard_holds(broken, &overrides)
                .unwrap_err()
                .contains("filter.lfs.process")
        );
    }

    #[test]
    fn the_overrides_reach_a_process_as_the_command_scope() {
        let env = guard_env(&[("core.hooksPath".into(), NO_HOOKS.into())]);
        assert_eq!(
            env,
            [
                ("GIT_CONFIG_PARAMETERS".to_owned(), String::new()),
                ("GIT_CONFIG_COUNT".to_owned(), "1".to_owned()),
                ("GIT_CONFIG_KEY_0".to_owned(), "core.hooksPath".to_owned()),
                ("GIT_CONFIG_VALUE_0".to_owned(), NO_HOOKS.to_owned()),
            ]
        );
    }

    fn landing_dir(uid: u32, mode: u32) -> LandingDirFacts {
        LandingDirFacts {
            is_symlink: false,
            is_dir: true,
            uid,
            mode,
        }
    }

    #[test]
    fn a_landing_directory_of_another_owner_is_refused() {
        let dir = Path::new("/q/landing");
        let why = landing_dir_check(&landing_dir(1001, 0o40700), 501, dir).unwrap_err();
        assert!(why.contains("another owner"), "{why}");
        assert!(why.contains("/q/landing"), "{why}");
    }

    #[test]
    fn a_landing_directory_is_taken_only_as_the_control_sides_own_directory() {
        let dir = Path::new("/q/landing");
        assert_eq!(
            landing_dir_check(&landing_dir(501, 0o40700), 501, dir),
            Ok(())
        );
        assert_eq!(
            landing_dir_check(&landing_dir(501, 0o40755), 501, dir),
            Ok(())
        );
        for mode in [0o40770, 0o40707, 0o40720, 0o40702, 0o40777] {
            let why = landing_dir_check(&landing_dir(501, mode), 501, dir).unwrap_err();
            assert!(
                why.contains("writable by its group or others"),
                "{mode:o}: {why}"
            );
        }
        let symlink = LandingDirFacts {
            is_symlink: true,
            is_dir: false,
            ..landing_dir(501, 0o120700)
        };
        let file = LandingDirFacts {
            is_dir: false,
            ..landing_dir(501, 0o100600)
        };
        // What it is is judged before whose it is.
        let foreign_symlink = LandingDirFacts {
            uid: 1001,
            mode: 0o120777,
            ..symlink
        };
        for facts in [symlink, file, foreign_symlink] {
            let why = landing_dir_check(&facts, 501, dir).unwrap_err();
            assert!(
                why.contains("a symlink or no directory"),
                "{facts:?}: {why}"
            );
        }
    }
}
