//! The uncommitted work of a run in a container, as the files of its run
//! directory carry it between the container's executor and the control
//! side, which runs no git in the run's clone (ADR-t2179-1 decision 1):
//! the executor writes the work its turn's end left ([`CloneWork`],
//! [`RUN_WORK_FILE`]) for the snapshot of a replanning's park
//! (ADR-t1521-2 decision 2), and the control side hands back what a
//! continuation's restore needs ([`RestoreRequest`], [`RUN_RESTORE_FILE`]),
//! which the executor applies to the clone before the next turn's agent
//! runs and answers in [`RESTORE_RESULT_FILE`]. What is checked and decided
//! with them is here; the files and the git are the adapters' and the
//! executor's.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::replan_snapshot::{RestoreCheck, UntrackedFile, WorktreeState, compare, sha256};
use super::run_clone::RunState;
use super::{CommitSha, DomainError};

/// The work the executor read in the clone at the end of a turn, in the run
/// directory: written after the run branch's bundle and before the run's
/// state, to a temporary file renamed into place, with the generation of
/// the state written after it ([`CloneWork`]).
pub const RUN_WORK_FILE: &str = "run-work.pack";

/// What the control side hands the executor to restore a parked run's
/// work in its clone ([`RestoreRequest`]), in the run directory.
pub const RUN_RESTORE_FILE: &str = "run-restore.pack";

/// The executor's answer to a [`RestoreRequest`] ([`RestoreResult`]), in
/// the run directory.
pub const RESTORE_RESULT_FILE: &str = "run-restore-result.json";

// What a part of a packed file holds.
string_enum!(PartKind {
    /// The staged diff against the head (`git diff --cached --binary`).
    Index => "index",
    /// The unstaged diff against the index (`git diff --binary`).
    Worktree => "worktree",
    /// An untracked file Git does not ignore, by its path in the clone.
    Untracked => "untracked",
    /// A bundle of the head the restore starts from, on top of the run's
    /// base commit.
    Bundle => "bundle",
});

/// One part of a packed file: what it holds, its path in the clone for an
/// untracked file, whether that file is executable (its owner's execute
/// bit), and its size and SHA-256.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Part {
    pub kind: PartKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub executable: bool,
    pub bytes: u64,
    pub sha256: String,
}

impl Part {
    /// The record of `contents` as a part of `kind`.
    pub fn of(kind: PartKind, path: Option<String>, executable: bool, contents: &[u8]) -> Self {
        Self {
            kind,
            path,
            executable,
            bytes: contents.len() as u64,
            sha256: sha256(contents),
        }
    }
}

/// A header of a packed file: what names its parts.
pub trait Packed: Serialize + DeserializeOwned {
    fn parts(&self) -> &[Part];
}

/// A packed file as read: its header and the bytes of each part it names.
pub type Unpacked<H> = (H, Vec<Vec<u8>>);

/// A packed file: the header as one line of JSON, then the bytes of each
/// part it names, one after another in its order.
pub fn pack<H: Packed>(header: &H, contents: &[Vec<u8>]) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(header).expect("a header serializes");
    bytes.push(b'\n');
    for part in contents {
        bytes.extend_from_slice(part);
    }
    bytes
}

/// The header and the parts of a packed file ([`pack`]); `Err` says why
/// it does not read back as packed: no header, or a part whose bytes are
/// missing, more than the header says or not of its SHA-256.
pub fn unpack<H: Packed>(bytes: &[u8]) -> Result<Unpacked<H>, String> {
    let end = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or("it holds no header")?;
    let header: H = serde_json::from_slice(&bytes[..end])
        .map_err(|error| format!("its header is not valid: {error}"))?;
    let mut rest = &bytes[end + 1..];
    let mut contents = Vec::new();
    for part in header.parts() {
        let len = usize::try_from(part.bytes).map_err(|_| "a part is too large".to_owned())?;
        if rest.len() < len {
            return Err(format!("a {} part is cut short", part.kind.as_str()));
        }
        let (taken, left) = rest.split_at(len);
        if sha256(taken) != part.sha256 {
            return Err(format!(
                "a {} part does not read back as written",
                part.kind.as_str()
            ));
        }
        contents.push(taken.to_vec());
        rest = left;
    }
    if !rest.is_empty() {
        return Err("it holds more than its header names".into());
    }
    Ok((header, contents))
}

/// The worktree state of `head` and `parts`, as a snapshot compares it:
/// the SHA-256 of the staged and the unstaged diff (of nothing when a part
/// is missing) and the untracked files, sorted by path.
fn state_of(head: &str, parts: &[Part]) -> WorktreeState {
    let hash = |kind: PartKind| {
        parts
            .iter()
            .find(|part| part.kind == kind)
            .map_or_else(|| sha256(b""), |part| part.sha256.clone())
    };
    let mut untracked: Vec<UntrackedFile> = parts
        .iter()
        .filter(|part| part.kind == PartKind::Untracked)
        .map(|part| UntrackedFile {
            path: part.path.clone().unwrap_or_default(),
            sha256: part.sha256.clone(),
            executable: part.executable,
        })
        .collect();
    untracked.sort();
    WorktreeState {
        head: head.to_owned(),
        index: hash(PartKind::Index),
        worktree: hash(PartKind::Worktree),
        untracked,
    }
}

/// The work the executor read in the clone at the end of a turn
/// ([`RUN_WORK_FILE`]): the generation of the run's state written with it
/// ([`RunState::generation`]), the head, the Git operation half done there
/// (`rebase`, `merge`, `cherry-pick`, `revert`), why the work could not be
/// written whole (a link or a special file untracked, a file past a
/// snapshot's limits), and its parts: the staged and the unstaged diff and
/// each untracked file. It is the worker's word as the state is: the
/// snapshot saves it as it reads, and only a continuation restores it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloneWork {
    pub generation: u64,
    pub head: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    pub parts: Vec<Part>,
}

impl Packed for CloneWork {
    fn parts(&self) -> &[Part] {
        &self.parts
    }
}

impl CloneWork {
    /// The clone as this work says it is.
    pub fn state(&self) -> WorktreeState {
        state_of(&self.head, &self.parts)
    }
}

/// Whether `work`, as unpacked, is the work of the end of the last turn
/// whose state `state` is, on the head `imported` the control side took
/// from the run's bundle: the state is of the last turn's end
/// ([`RunState::of_the_last_turn`]: not one a turn cut off left) and has a
/// generation, the work is of that generation (not an earlier turn's) and
/// of the state's head, the head is the imported one, no Git operation is
/// half done and the work was written whole. `Err` says why the snapshot
/// cannot be saved from it.
pub fn check_work(state: &RunState, imported: &CommitSha, work: &CloneWork) -> Result<(), String> {
    if !state.of_the_last_turn() {
        return Err(
            "the run's state is not of the end of its last turn (that turn was cut off)".into(),
        );
    }
    let Some(generation) = state.generation else {
        return Err("the run's state has no generation to match its work with".into());
    };
    if work.generation != generation {
        return Err(format!(
            "the work written is of generation {}, the run's state of {generation}",
            work.generation
        ));
    }
    if work.head != state.head {
        return Err(format!(
            "the work written is on {}, the run's state on {}",
            work.head, state.head
        ));
    }
    if state.head() != *imported {
        return Err(format!(
            "the clone's HEAD {} is not the run branch's head {imported}",
            state.head
        ));
    }
    if let Some(operation) = work
        .git_operation
        .as_deref()
        .or_else(|| state.rebase_in_progress.then_some("rebase"))
    {
        return Err(format!("a {operation} is in progress in the clone"));
    }
    if let Some(why) = &work.refused {
        return Err(format!("the work could not be written whole: {why}"));
    }
    Ok(())
}

/// Why an untracked entry of the clone cannot be written into the work,
/// `None` when it can: it is to be a regular file within a snapshot's
/// limits ([`super::replan_snapshot::check_entry`]), `total` counting the
/// bytes written before it.
pub fn refused_entry(path: &str, regular: bool, bytes: u64, total: u64) -> Option<String> {
    super::replan_snapshot::check_entry(path, regular, bytes, total)
        .err()
        .map(|error| error.to_string())
}

/// What a continuation's restore hands the executor before the turn that
/// resumes the run ([`RUN_RESTORE_FILE`]): the snapshot it restores and the
/// attempt at it ([`restore_attempt`]), the run's base and branch, the head
/// the snapshot saved, whether the control side made the clone again (gone
/// since the park), which only then may be filled, and the parts: the
/// head's bundle on top of the base when the head is not the base, the
/// staged and the unstaged diff and the untracked files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreRequest {
    pub snapshot_id: String,
    pub attempt: u32,
    pub base: String,
    pub branch: String,
    pub head: String,
    pub remade: bool,
    pub parts: Vec<Part>,
}

impl Packed for RestoreRequest {
    fn parts(&self) -> &[Part] {
        &self.parts
    }
}

impl RestoreRequest {
    /// The clone as the snapshot saved it.
    pub fn state(&self) -> WorktreeState {
        state_of(&self.head, &self.parts)
    }
}

/// The executor's answer to the restore of `snapshot_id` at `attempt`
/// ([`RESTORE_RESULT_FILE`]): what it did (`intact`, `recreated`) or why
/// it did not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreResult {
    pub snapshot_id: String,
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
}

impl RestoreResult {
    /// Whether this answers the restore of `snapshot` at `attempt`: an
    /// answer to another snapshot's, or to an earlier attempt whose failure
    /// was recorded, ends none of this one.
    pub fn answers(&self, snapshot: &str, attempt: u32) -> bool {
        self.snapshot_id == snapshot && self.attempt == attempt
    }

    /// The result for the control side to record: the action, or why the
    /// restore failed.
    pub fn outcome(&self) -> Result<String, String> {
        match (&self.failed, &self.action) {
            (Some(why), _) => Err(why.clone()),
            (None, Some(action)) => Ok(action.clone()),
            (None, None) => Err("the executor's result names no action".into()),
        }
    }
}

/// What the executor does with a [`RestoreRequest`] before the turn's
/// agent runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutorRestore {
    /// It answered this restore before, and the clone was restored: the
    /// turn goes on, its clone not compared again (the turns since changed
    /// it).
    Done,
    /// The clone is as the snapshot saved it.
    Intact,
    /// The clone is the one the control side made again, still at the base
    /// with nothing uncommitted: it is filled from the request.
    Fill,
    /// The clone is neither: nothing in it is touched, and no agent runs.
    Refuse(String),
}

/// What the executor does with `request` for the clone as it reads it now
/// (`current`, with the Git operation half done there), `answered` being
/// its earlier answer in the run directory, if any: one to an earlier
/// attempt is not taken, the clone being compared again.
pub fn executor_restore(
    request: &RestoreRequest,
    answered: Option<&RestoreResult>,
    current: &WorktreeState,
    git_operation: Option<&str>,
) -> ExecutorRestore {
    if let Some(answered) = answered.filter(|a| a.answers(&request.snapshot_id, request.attempt)) {
        return match answered.outcome() {
            Ok(_) => ExecutorRestore::Done,
            Err(why) => ExecutorRestore::Refuse(why),
        };
    }
    if let Some(operation) = git_operation {
        return ExecutorRestore::Refuse(format!("a {operation} is in progress in the clone"));
    }
    match compare(&request.state(), current) {
        RestoreCheck::Intact => ExecutorRestore::Intact,
        RestoreCheck::Diverged(_) if request.remade && fresh(request, current) => {
            ExecutorRestore::Fill
        }
        RestoreCheck::Diverged(why) => {
            ExecutorRestore::Refuse(format!("the clone changed since the park ({why})"))
        }
        RestoreCheck::Missing => ExecutorRestore::Refuse("the clone is gone".into()),
    }
}

/// Whether `current` is a clone as the control side makes one: at the
/// base, nothing uncommitted.
fn fresh(request: &RestoreRequest, current: &WorktreeState) -> bool {
    current.head == request.base && !current.dirty()
}

/// Where the restore of a parked run in a container stands for the control
/// side, from what is there (whether the clone's directory is, the restore
/// handed last, the executor's answer), the attempt under way
/// ([`restore_attempt`]) and whether the run resumes: each step is read
/// again from these, so a restart or an adopt goes on where the last one
/// stopped and nothing is handed or recorded twice, while a continuation
/// after a recorded failure starts a new attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneRestore {
    /// The executor answered this attempt at the snapshot's restore: record
    /// it.
    Record(Result<String, String>),
    /// Hand the restore to the executor ([`RestoreRequest`], `remade` when
    /// the clone is or was made again for this attempt), making the clone
    /// again after when
    /// `remake`, so that a clone made again is never taken for the one the
    /// park left; the run resumes, and the executor compares the clone as
    /// it is with the snapshot before its agent runs.
    Hand { remake: bool, remade: bool },
    /// The run is handed on without a resume (its resumes used up): no turn
    /// of its clone follows, whose executor alone sees the clone as it is
    /// now (the work it wrote before the park cannot show a change made
    /// since), so the restore fails, why.
    Fail(String),
}

/// A restore handed before, as the control side reads it back: the
/// snapshot it names, the attempt and whether the clone was made again for
/// it; `None` for one that does not read back.
pub type Handed = Option<(String, u32, bool)>;

/// The attempt at the restore of `snapshot` for operation `operation`: one
/// past the failures recorded for them, `failures` being the payloads of
/// the run's `replan_restore_failed`. A restart or an adopt reads the same
/// and goes on with the attempt under way; once its failure is recorded,
/// the continuation a person moves the operation to after mending the
/// clone is a new attempt, which no answer to the earlier one ends.
pub fn restore_attempt<'a>(
    failures: impl IntoIterator<Item = &'a serde_json::Value>,
    operation: i64,
    snapshot: &str,
) -> u32 {
    let failed = failures
        .into_iter()
        .filter(|payload| {
            payload["operation_id"].as_i64() == Some(operation)
                && payload["snapshot_id"].as_str() == Some(snapshot)
        })
        .count();
    u32::try_from(failed).map_or(u32::MAX, |failed| failed.saturating_add(1))
}

/// The step of attempt `attempt` at the restore of `snapshot`
/// ([`CloneRestore`]): `handed` is the restore found in the run directory
/// (`Some(None)` for one that does not read back, taken for this attempt's
/// cut short, not made again), `resuming` whether a turn of the run's
/// clone follows. What an earlier attempt handed or answered is not this
/// one's: a clone it made again is not taken for made again now.
pub fn clone_restore(
    snapshot: &str,
    attempt: u32,
    clone_present: bool,
    handed: Option<Handed>,
    answered: Option<&RestoreResult>,
    resuming: bool,
) -> CloneRestore {
    if let Some(answered) = answered.filter(|a| a.answers(snapshot, attempt)) {
        return CloneRestore::Record(answered.outcome());
    }
    let handed =
        handed.map(|handed| handed.unwrap_or_else(|| (snapshot.to_owned(), attempt, false)));
    let ours = handed.filter(|(id, at, _)| id == snapshot && *at == attempt);
    if resuming {
        let remake = !clone_present;
        let remade = remake || ours.is_some_and(|(_, _, remade)| remade);
        return CloneRestore::Hand { remake, remade };
    }
    CloneRestore::Fail(
        "the run's resumes are used up, so no turn of its clone follows, in which alone its executor compares the clone as it is with the snapshot".into(),
    )
}

/// A file of the run directory as read: the bytes of `name` (`None` when
/// it is not there), `Err` when it is no regular file or too large.
pub type RunFileRead = Result<Option<Vec<u8>>, String>;

/// The header of a packed file read as `H`, `None` when there is none.
pub fn read_packed<H: Packed>(read: RunFileRead) -> Result<Option<Unpacked<H>>, String> {
    match read? {
        Some(bytes) => unpack(&bytes).map(Some),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::replan_snapshot::{MAX_FILE_BYTES, MAX_TOTAL_BYTES};
    use serde_json::json;

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
    const BASE: &str = "89abcdef0123456789abcdef0123456789abcdef";

    fn work(generation: u64) -> (CloneWork, Vec<Vec<u8>>) {
        let contents = vec![
            b"staged".to_vec(),
            b"unstaged".to_vec(),
            b"#!/bin/sh\n".to_vec(),
        ];
        let work = CloneWork {
            generation,
            head: HEAD.into(),
            git_operation: None,
            refused: None,
            parts: vec![
                Part::of(PartKind::Index, None, false, &contents[0]),
                Part::of(PartKind::Worktree, None, false, &contents[1]),
                Part::of(
                    PartKind::Untracked,
                    Some("run.sh".into()),
                    true,
                    &contents[2],
                ),
            ],
        };
        (work, contents)
    }

    fn state(generation: Option<u64>, ended: bool) -> RunState {
        RunState {
            branch: Some("refs/heads/dagq/r1".into()),
            head: HEAD.into(),
            status: "M  a\n".into(),
            rebase_in_progress: false,
            turn_ended: Some(ended),
            generation,
        }
    }

    fn head() -> CommitSha {
        CommitSha::try_from(HEAD.to_owned()).unwrap()
    }

    /// A packed file reads back whole, and one cut short, longer than its
    /// header says or with a part changed does not.
    #[test]
    fn a_packed_file_reads_back_only_as_it_was_written() {
        let (written, contents) = work(3);
        let bytes = pack(&written, &contents);
        let (read, parts): (CloneWork, _) = unpack(&bytes).unwrap();
        assert_eq!(read, written);
        assert_eq!(parts, contents);
        assert!(unpack::<CloneWork>(&bytes[..bytes.len() - 1]).is_err());
        let mut longer = bytes.clone();
        longer.push(b'x');
        assert!(unpack::<CloneWork>(&longer).is_err());
        let mut changed = bytes.clone();
        let last = changed.len() - 1;
        changed[last] ^= 1;
        assert!(unpack::<CloneWork>(&changed).is_err());
        assert!(unpack::<CloneWork>(b"no header").is_err());
        assert_eq!(read_packed::<CloneWork>(Ok(None)), Ok(None));
        assert!(read_packed::<CloneWork>(Err("a symlink".into())).is_err());
    }

    /// The snapshot is saved only from the work of the last turn's end:
    /// of the state's generation and head, on the imported head, with no
    /// Git operation half done and written whole.
    #[test]
    fn the_work_is_saved_only_when_it_is_of_the_last_turns_end() {
        let (written, _) = work(3);
        assert_eq!(check_work(&state(Some(3), true), &head(), &written), Ok(()));
        let refused = |state: &RunState, imported: &CommitSha, work: &CloneWork| {
            check_work(state, imported, work).unwrap_err()
        };
        // An older turn's work, a state a cut-off turn left, a state that
        // names no generation.
        assert!(refused(&state(Some(4), true), &head(), &written).contains("generation 3"));
        assert!(refused(&state(Some(3), false), &head(), &written).contains("cut off"));
        assert!(refused(&state(None, true), &head(), &written).contains("no generation"));
        let elsewhere = CloneWork {
            head: BASE.into(),
            ..written.clone()
        };
        assert!(refused(&state(Some(3), true), &head(), &elsewhere).contains("is on"));
        let other = CommitSha::try_from(BASE.to_owned()).unwrap();
        assert!(refused(&state(Some(3), true), &other, &written).contains("not the run branch"));
        let rebasing = RunState {
            rebase_in_progress: true,
            ..state(Some(3), true)
        };
        assert!(refused(&rebasing, &head(), &written).contains("rebase"));
        let merging = CloneWork {
            git_operation: Some("merge".into()),
            ..written.clone()
        };
        assert!(refused(&state(Some(3), true), &head(), &merging).contains("merge"));
        let cut = CloneWork {
            refused: Some("a.bin is not a regular file".into()),
            ..written
        };
        assert!(refused(&state(Some(3), true), &head(), &cut).contains("a.bin"));
        assert!(refused_entry("a", false, 1, 0).is_some());
        assert!(refused_entry("a", true, MAX_FILE_BYTES + 1, 0).is_some());
        assert!(refused_entry("a", true, 1, MAX_TOTAL_BYTES).is_some());
        assert_eq!(refused_entry("a", true, 1, 0), None);
    }

    fn request(remade: bool) -> RestoreRequest {
        let (written, _) = work(3);
        RestoreRequest {
            snapshot_id: "replan-3-g2".into(),
            attempt: 1,
            base: BASE.into(),
            branch: "dagq/r1".into(),
            head: HEAD.into(),
            remade,
            parts: written.parts,
        }
    }

    fn fresh_clone() -> WorktreeState {
        WorktreeState {
            head: BASE.into(),
            index: sha256(b""),
            worktree: sha256(b""),
            untracked: Vec::new(),
        }
    }

    fn answer(failed: Option<&str>) -> RestoreResult {
        RestoreResult {
            snapshot_id: "replan-3-g2".into(),
            attempt: 1,
            action: failed.is_none().then(|| "recreated".to_owned()),
            failed: failed.map(str::to_owned),
        }
    }

    /// The executor leaves a clone as saved, fills only one the control
    /// side made again (at the base, nothing uncommitted), and refuses one
    /// that changed, a clone left in place at the base included, touching
    /// nothing; a restore it answered is not done again.
    #[test]
    fn the_executor_fills_only_a_clone_made_again_and_never_overwrites_one_that_changed() {
        let saved = request(false).state();
        assert_eq!(
            executor_restore(&request(true), None, &saved, None),
            ExecutorRestore::Intact
        );
        assert_eq!(
            executor_restore(&request(true), None, &fresh_clone(), None),
            ExecutorRestore::Fill
        );
        // A clone left in place at the base is not filled over.
        assert!(matches!(
            executor_restore(&request(false), None, &fresh_clone(), None),
            ExecutorRestore::Refuse(_)
        ));
        let changed = WorktreeState {
            worktree: sha256(b"edited"),
            ..saved.clone()
        };
        let at_base_with_a_file = WorktreeState {
            untracked: saved.untracked.clone(),
            ..fresh_clone()
        };
        for (current, remade) in [
            (&changed, false),
            (&changed, true),
            (&at_base_with_a_file, true),
        ] {
            assert!(matches!(
                executor_restore(&request(remade), None, current, None),
                ExecutorRestore::Refuse(why) if why.contains("changed since the park")
            ));
        }
        assert!(matches!(
            executor_restore(&request(true), None, &fresh_clone(), Some("rebase")),
            ExecutorRestore::Refuse(_)
        ));
        // Answered before: done, whatever the turns since did; or refused
        // again, the clone still untouched.
        assert_eq!(
            executor_restore(&request(true), Some(&answer(None)), &changed, None),
            ExecutorRestore::Done
        );
        assert_eq!(
            executor_restore(&request(true), Some(&answer(Some("no"))), &saved, None),
            ExecutorRestore::Refuse("no".into())
        );
        // An answer to another snapshot's restore is not this one's.
        let other = RestoreResult {
            snapshot_id: "replan-3-g1".into(),
            ..answer(None)
        };
        assert_eq!(
            executor_restore(&request(true), Some(&other), &fresh_clone(), None),
            ExecutorRestore::Fill
        );
    }

    /// The control side records the executor's answer to this snapshot; a
    /// resume hands the restore, the clone made again only while it is
    /// gone and named made again after a restart that found it made; a run
    /// handed on without a resume gets [`CloneRestore::Fail`] unless the
    /// executor answered this snapshot already, and nothing compares its
    /// clone. A restart reads the same and neither hands nor records twice.
    #[test]
    fn the_control_side_goes_on_from_what_the_restore_left() {
        let snapshot = "replan-3-g2";
        let ours = |remade| Some(Some((snapshot.to_owned(), 1, remade)));
        let hand = |remake, remade| CloneRestore::Hand { remake, remade };
        assert_eq!(
            clone_restore(snapshot, 1, true, None, None, true),
            hand(false, false)
        );
        assert_eq!(
            clone_restore(snapshot, 1, false, None, None, true),
            hand(true, true)
        );
        assert_eq!(
            clone_restore(snapshot, 1, true, ours(true), None, true),
            hand(false, true)
        );
        assert_eq!(
            clone_restore(snapshot, 1, true, ours(false), None, true),
            hand(false, false)
        );
        // One that does not read back is taken for this one's, not made.
        assert_eq!(
            clone_restore(snapshot, 1, true, Some(None), None, true),
            hand(false, false)
        );
        assert_eq!(
            clone_restore(snapshot, 1, true, ours(true), Some(&answer(None)), true),
            CloneRestore::Record(Ok("recreated".into()))
        );
        assert_eq!(
            clone_restore(
                snapshot,
                1,
                true,
                ours(false),
                Some(&answer(Some("changed"))),
                false
            ),
            CloneRestore::Record(Err("changed".into()))
        );
        // Handed on without a resume and not answered: a failure, whether
        // the clone is there or not and whether a restore was handed.
        for (present, handed) in [
            (true, None),
            (false, None),
            (true, ours(true)),
            (true, Some(None)),
        ] {
            assert!(matches!(
                clone_restore(snapshot, 1, present, handed, None, false),
                CloneRestore::Fail(_)
            ));
        }
        // What an earlier park's restore left is not this one's.
        let older = RestoreResult {
            snapshot_id: "replan-3-g1".into(),
            ..answer(None)
        };
        let earlier = Some(Some(("replan-3-g1".to_owned(), 1, true)));
        assert_eq!(
            clone_restore(snapshot, 1, true, earlier.clone(), Some(&older), true),
            hand(false, false)
        );
        assert!(matches!(
            clone_restore(snapshot, 1, true, earlier, Some(&older), false),
            CloneRestore::Fail(_)
        ));
        assert_eq!(
            RestoreResult {
                snapshot_id: snapshot.into(),
                attempt: 1,
                action: None,
                failed: None
            }
            .outcome(),
            Err("the executor's result names no action".into())
        );
    }

    /// A continuation after a recorded failure is a new attempt: the
    /// failures recorded for this operation and snapshot number it, the same
    /// for a restart or an adopt; the earlier attempt's answer neither
    /// records nor refuses it, a clone made again for it is not taken for
    /// made again now, and the executor compares the clone again.
    #[test]
    fn a_restore_after_a_recorded_failure_is_a_new_attempt() {
        let snapshot = "replan-3-g2";
        let failed =
            |op: i64, id: &str| json!({"operation_id": op, "snapshot_id": id, "reason": "no"});
        assert_eq!(restore_attempt(std::iter::empty(), 3, snapshot), 1);
        let events = [
            failed(3, snapshot),
            failed(4, snapshot),
            failed(3, "replan-3-g1"),
        ];
        assert_eq!(restore_attempt(&events, 3, snapshot), 2);
        assert_eq!(restore_attempt(&events[..1], 3, snapshot), 2);
        let refused = answer(Some("the clone changed since the park"));
        let handed = Some(Some((snapshot.to_owned(), 1, true)));
        let hand = |remake, remade| CloneRestore::Hand { remake, remade };
        // The first attempt's refusal, and the clone it made again, are not
        // the second's.
        assert_eq!(
            clone_restore(snapshot, 2, true, handed.clone(), Some(&refused), true),
            hand(false, false)
        );
        assert_eq!(
            clone_restore(snapshot, 2, false, handed, Some(&refused), true),
            hand(true, true)
        );
        // Within the second attempt, a restart goes on with what it left.
        let second = Some(Some((snapshot.to_owned(), 2, true)));
        assert_eq!(
            clone_restore(snapshot, 2, true, second.clone(), Some(&refused), true),
            hand(false, true)
        );
        let answered = RestoreResult {
            attempt: 2,
            ..answer(None)
        };
        assert_eq!(
            clone_restore(snapshot, 2, true, second, Some(&answered), true),
            CloneRestore::Record(Ok("recreated".into()))
        );
        // The executor compares the clone again for the second attempt.
        let retry = |remade| RestoreRequest {
            attempt: 2,
            ..request(remade)
        };
        let saved = request(false).state();
        assert_eq!(
            executor_restore(&retry(false), Some(&refused), &saved, None),
            ExecutorRestore::Intact
        );
        assert_eq!(
            executor_restore(&retry(true), Some(&refused), &fresh_clone(), None),
            ExecutorRestore::Fill
        );
        let stray = WorktreeState {
            untracked: vec![UntrackedFile {
                path: "stray.txt".into(),
                sha256: sha256(b"stray"),
                executable: false,
            }],
            ..saved
        };
        assert!(matches!(
            executor_restore(&retry(false), Some(&refused), &stray, None),
            ExecutorRestore::Refuse(why) if why.contains("changed since the park")
        ));
    }
}
