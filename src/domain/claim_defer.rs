//! Deferring the claim of one task (ADR-0069): a candidate whose expected
//! files meet those of a run in flight on a file the landings keep
//! conflicting in (a hotspot of `stats`' `conflict_hotspots`) is passed
//! over in that pass, and the next candidate is claimed instead. Where
//! [`super::claim_hold`] holds every claim for the queue, this defers one
//! task; the two share the way they are recorded and shown: a task event
//! when the deferral starts (`claim_deferred`) and one when it ends
//! (`claim_deferral_ended`), which `status` and `stats` read.
//!
//! A task of interrupt priority is never deferred, and a deferral ends
//! once it has lasted `[conflicts] defer_max_secs`: the task is claimed
//! then even if the files still meet. A run in flight that only waits for
//! a person's answer stops counting once it has waited
//! `[conflicts] waiting_owner_grace_secs` (ADR-t1484-1), and a failed run
//! with no commit of its own never counts (ADR-t1634-1).

use super::EventKind;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;
use serde_json::{Value, json};

use super::{
    Ask, AskKind, EventId, LeaseToken, RunEvent, RunStatus, TaskId,
    event_kind::{LEASE_ACQUIRED, LEASE_RELEASED},
    scope::glob_matches,
    stats::timestamp_millis,
    waiting::WaitState,
};

/// Recorded on the task when its claim is first deferred (`reason`,
/// `files`, `runs`, `max_secs`, `message`, `supervisor`).
pub const CLAIM_DEFERRED: &str = crate::domain::event_kind::EventKind::ClaimDeferred.as_str();
/// Recorded on the task when its deferral ends (`reason`, `why`,
/// `deferred_secs`, `supervisor`): `cleared` (the files no longer meet or
/// the task became an interrupt), `owner_waiting` (they meet only runs
/// left out, one of them waiting for a person's answer past the grace,
/// ADR-t1484-1), `no_commit` (they meet only failed runs with no commit
/// of their own, ADR-t1634-1), `expired` (it lasted the limit) or `not_candidate` (it left the
/// candidates: claimed elsewhere, canceled, blocked again).
pub const CLAIM_DEFERRAL_ENDED: &str =
    crate::domain::event_kind::EventKind::ClaimDeferralEnded.as_str();
/// The kinds that say where a task's deferral stands; a claim of the task
/// ends whatever came before it.
pub const DEFERRAL_KINDS: [&str; 3] = [CLAIM_DEFERRED, CLAIM_DEFERRAL_ENDED, "run_claimed"];

/// The files meet on a hotspot. A task is also deferred while the
/// supervisor cannot run its worker ([`super::worker::PROVIDER_UNAVAILABLE`],
/// [`super::worker::MODE_UNAVAILABLE`]; see [`worker_deferred`]).
pub const HOT_FILES: &str = "hot_files";

/// The default of `[conflicts] defer_max_secs`: a deferral lasts at most an
/// hour. The runs of this queue work for 24 minutes at the median
/// (`dagq stats` of 2026-09-26), so an hour lets the run in the way work
/// and mostly land, while the task deferred never waits longer than about
/// two of them.
pub const DEFAULT_DEFER_MAX_SECS: i64 = 3600;

/// The default of `[conflicts] waiting_owner_grace_secs` (ADR-t1484-1): a
/// run that waits for a person's answer stops holding the claims back ten
/// minutes into its wait. An answer that comes at once (the inbox looks)
/// keeps the deferral; one that does not lets the tasks behind go.
pub const DEFAULT_WAITING_OWNER_GRACE_SECS: i64 = 600;

/// How a deferral ends when the files meet only runs that wait for a
/// person past the grace.
pub const OWNER_WAITING: &str = "owner_waiting";

/// How a deferral ends when the files meet only failed runs with no
/// commit of their own (ADR-t1634-1).
pub const NO_COMMIT: &str = "no_commit";

/// How many related landed tasks give the files of a task that declares no
/// concrete path.
pub const RELATED_TASKS: usize = 3;

/// The files a run in flight is expected to touch: its diff from its base
/// and the expected files of its task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InFlight {
    pub run_id: String,
    pub task_id: TaskId,
    pub files: Vec<String>,
    /// Since when (unix seconds) the run only waits for a person's answer
    /// ([`owner_waiting_since`]); `None` while it moves.
    pub owner_waiting_since: Option<i64>,
    /// The run failed with no commit of its own ([`failed_without_commit`]).
    pub no_commit: bool,
}

/// Whether a run of `status` has failed with no commit of its own
/// (ADR-t1634-1): `changed` is what its head (its result commit, or its
/// branch) changed from its base, `None` when it has no head. A failed run
/// that waits for its triage, its recovery or a person changed no file the
/// others could conflict with. A head that could not be read is not known
/// to be empty, so the caller passes such a run as changing something.
pub fn failed_without_commit(status: RunStatus, changed: Option<&[String]>) -> bool {
    status == RunStatus::Failed && changed.is_none_or(<[String]>::is_empty)
}

/// Whether a run's open ask of `kind` stops it on a person: the run does
/// nothing until it is answered (ADR-t1484-1).
pub fn waits_for_owner(kind: &AskKind) -> bool {
    matches!(
        kind,
        AskKind::WorkerQuestion
            | AskKind::ApproveLanding
            | AskKind::StuckExit
            | AskKind::AnswerPrompt
            | AskKind::Stalled
            | AskKind::Decide
    )
}

/// Since when (unix seconds) a run in flight only waits for a person
/// (ADR-t1484-1): it has an ask of [`waits_for_owner`] that is neither
/// answered nor closed among `asks` (its unclosed asks), and it is either
/// out of its slot in a wait (ADR-0071, [`WaitState`] not ended; a
/// returning run moves) or `leased` by no supervisor (resting on the ask,
/// not on a resume). The wait counts from its start, the lease's absence
/// from the run's last lease event, neither before the oldest such ask.
/// `None` while the run moves: no such ask, or it is worked on.
pub fn owner_waiting_since(asks: &[Ask], leased: bool, events: &[RunEvent]) -> Option<i64> {
    let asked = asks
        .iter()
        .filter(|ask| ask.answered_at.is_none() && ask.closed_at.is_none())
        .filter(|ask| waits_for_owner(&ask.kind))
        .map(|ask| ask.created_at)
        .min()?;
    let seconds = |event: &RunEvent| timestamp_millis(&event.created_at).map(|ms| ms / 1000);
    let since = match WaitState::of(events) {
        Some(wait) if wait.ended.is_none() => Some(wait.since_ms / 1000),
        _ if leased => return None,
        _ => events
            .iter()
            .rev()
            .find(|event| event.kind == LEASE_ACQUIRED || event.kind == LEASE_RELEASED)
            .and_then(seconds),
    };
    Some(since.map_or(asked, |since| since.max(asked)))
}

/// The runs of `in_flight` that hold the claims back at `now`: all but
/// those that have only waited for a person for `grace_secs` or longer
/// (ADR-t1484-1) and those that failed with no commit of their own
/// (ADR-t1634-1).
pub fn counted(in_flight: &[InFlight], now: i64, grace_secs: i64) -> Vec<InFlight> {
    in_flight
        .iter()
        .filter(|run| {
            !run.no_commit
                && run
                    .owner_waiting_since
                    .is_none_or(|since| now - since < grace_secs)
        })
        .cloned()
        .collect()
}

/// Why a candidate's files meet no run that counts though they meet
/// `in_flight` on `hot`: [`NO_COMMIT`] when every run they meet failed
/// with no commit of its own, [`OWNER_WAITING`] when one of them waits for
/// a person past the grace instead; `None` when they meet a run
/// [`counted`] or none at all.
pub fn left_out(
    hot: &[String],
    candidate: &[String],
    in_flight: &[InFlight],
    counted: &[InFlight],
) -> Option<&'static str> {
    if overlap(hot, candidate, counted).is_some() {
        return None;
    }
    let met = overlap(hot, candidate, in_flight)?;
    let no_commit = met.runs.iter().all(|met| {
        in_flight
            .iter()
            .any(|run| run.run_id == met.run_id && run.no_commit)
    });
    Some(if no_commit { NO_COMMIT } else { OWNER_WAITING })
}

/// Where a candidate's expected files meet the runs in flight on hotspots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Overlap {
    /// The hotspots both sides touch, in the order of `hot`.
    pub files: Vec<String>,
    /// The runs in flight that touch them.
    pub runs: Vec<OverlapRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct OverlapRun {
    pub run_id: String,
    pub task_id: TaskId,
}

/// Whether `files` (paths, read as globs) touch `path`.
pub fn touches(files: &[String], path: &str) -> bool {
    files.iter().any(|file| glob_matches(file, path))
}

/// The wildcards of a `--paths` glob ([`glob_matches`] reads `*`, `**`
/// and `?`).
const WILDCARDS: [char; 2] = ['*', '?'];

/// The declared paths that name a file: those with no wildcard. A glob
/// declares the scope a task may change, not the files it means to touch,
/// so it is left out of the expected files (ADR-t1981-1, amending ADR-0080
/// decision 1).
pub fn concrete_paths(declared: &[String]) -> Vec<&String> {
    declared
        .iter()
        .filter(|path| !path.contains(WILDCARDS))
        .collect()
}

/// Whether the expected files of a task that declares `declared` come from
/// the landings of its most related tasks: it declares no concrete path.
pub fn needs_related_landings(declared: &[String]) -> bool {
    concrete_paths(declared).is_empty()
}

/// The expected files of a task: its declared concrete paths, or, when it
/// declares none (no paths, or only globs), the files the landings of its
/// most related tasks changed (ADR-t1981-1).
pub fn expected_files(declared: &[String], related_changes: &[String]) -> Vec<String> {
    let concrete = concrete_paths(declared);
    let files: Vec<&String> = if concrete.is_empty() {
        related_changes.iter().collect()
    } else {
        concrete
    };
    let mut unique: Vec<String> = Vec::new();
    for file in files {
        if !unique.contains(file) {
            unique.push(file.clone());
        }
    }
    unique
}

/// Where `candidate` (its expected files) meets `in_flight` on the files of
/// `hot`; `None` when it does not.
pub fn overlap(hot: &[String], candidate: &[String], in_flight: &[InFlight]) -> Option<Overlap> {
    let mut files = Vec::new();
    let mut runs = BTreeSet::new();
    for path in hot.iter().filter(|path| touches(candidate, path)) {
        let meeting: Vec<&InFlight> = in_flight
            .iter()
            .filter(|run| touches(&run.files, path))
            .collect();
        if meeting.is_empty() {
            continue;
        }
        files.push(path.clone());
        runs.extend(meeting.into_iter().map(|run| OverlapRun {
            run_id: run.run_id.clone(),
            task_id: run.task_id,
        }));
    }
    (!files.is_empty()).then(|| Overlap {
        files,
        runs: runs.into_iter().collect(),
    })
}

/// A task's deferral as the supervisor keeps it: since when (unix
/// seconds), and whether it ran out, which lets the task be claimed while
/// the files still meet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deferral {
    pub since: i64,
    pub expired: bool,
}

/// The deferrals in place from each task's latest [`DEFERRAL_KINDS`]
/// event: an open `claim_deferred`, or a deferral that expired and whose
/// task was not claimed since. What a supervisor starts from, so the limit
/// holds across restarts.
pub fn deferrals_in_place(latest: &[RunEvent]) -> HashMap<TaskId, Deferral> {
    latest
        .iter()
        // A deferral for the worker is kept apart (`worker_deferrals_in_place`).
        .filter(|event| text(event, "reason").is_none_or(|reason| reason == HOT_FILES))
        .filter_map(|event| {
            let task = event.task_id?;
            let since = || timestamp_millis(&event.created_at).map(|ms| ms / 1000);
            match event.kind.as_str() {
                CLAIM_DEFERRED => Some((
                    task,
                    Deferral {
                        since: since()?,
                        expired: false,
                    },
                )),
                CLAIM_DEFERRAL_ENDED if text(event, "why") == Some("expired") => Some((
                    task,
                    Deferral {
                        since: since()?,
                        expired: true,
                    },
                )),
                _ => None,
            }
        })
        .collect()
}

/// What to do with one candidate this pass.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Claim it (or pass it on to the claim); `event` ends a deferral.
    Claim { event: Option<(EventKind, Value)> },
    /// Pass over it; `event` starts the deferral.
    Defer { event: Option<(EventKind, Value)> },
}

/// Judge one candidate: `interrupt` for a task of interrupt priority,
/// `overlap` of its files with the runs [`counted`], `left_out` why they
/// meet only runs left out ([`left_out`]), `deferral` the one in place
/// (updated here), `now` in unix seconds.
pub fn decide(
    interrupt: bool,
    overlap: Option<Overlap>,
    left_out: Option<&'static str>,
    deferral: &mut Option<Deferral>,
    now: i64,
    max_secs: i64,
    token: &LeaseToken,
) -> Decision {
    let ended = |why: &str, since: i64| {
        Some((
            EventKind::ClaimDeferralEnded,
            json!({
                "reason": HOT_FILES,
                "why": why,
                "deferred_secs": (now - since).max(0),
                "supervisor": token,
            }),
        ))
    };
    match (overlap.filter(|_| !interrupt), *deferral) {
        (Some(_), Some(open)) if open.expired => Decision::Claim { event: None },
        (Some(_), Some(open)) if now - open.since >= max_secs => {
            *deferral = Some(Deferral {
                since: open.since,
                expired: true,
            });
            Decision::Claim {
                event: ended("expired", open.since),
            }
        }
        (Some(_), Some(_)) => Decision::Defer { event: None },
        // No limit left to wait for: nothing is deferred.
        (Some(_), None) if max_secs <= 0 => Decision::Claim { event: None },
        (Some(overlap), None) => {
            *deferral = Some(Deferral {
                since: now,
                expired: false,
            });
            let message = message(&overlap, max_secs);
            Decision::Defer {
                event: Some((
                    EventKind::ClaimDeferred,
                    json!({
                        "reason": HOT_FILES,
                        "files": overlap.files,
                        "runs": overlap.runs,
                        "max_secs": max_secs,
                        "message": message,
                        "supervisor": token,
                    }),
                )),
            }
        }
        // An expired deferral stays expired until the task leaves the
        // candidates (it is claimed), so it is not deferred anew.
        (None, Some(open)) if open.expired => Decision::Claim { event: None },
        (None, Some(open)) => {
            *deferral = None;
            let why = left_out.filter(|_| !interrupt).unwrap_or("cleared");
            Decision::Claim {
                event: ended(why, open.since),
            }
        }
        (None, None) => Decision::Claim { event: None },
    }
}

/// The event that ends `deferral` of a task that left the candidates, if
/// it was still open.
pub fn left(deferral: Deferral, now: i64, token: &LeaseToken) -> Option<(EventKind, Value)> {
    (!deferral.expired).then(|| {
        (
            EventKind::ClaimDeferralEnded,
            json!({
                "reason": HOT_FILES,
                "why": "not_candidate",
                "deferred_secs": (now - deferral.since).max(0),
                "supervisor": token,
            }),
        )
    })
}

/// The deferrals for the worker in place from each task's latest
/// [`DEFERRAL_KINDS`] event: an open `claim_deferred` of such a reason, with
/// its reason and since when (unix seconds).
pub fn worker_deferrals_in_place(latest: &[RunEvent]) -> HashMap<TaskId, (String, i64)> {
    latest
        .iter()
        .filter(|event| event.kind == CLAIM_DEFERRED)
        .filter_map(|event| {
            let reason = text(event, "reason")?;
            // The wait for a build is kept apart too
            // ([`super::build_wait::deferrals_in_place`]).
            if reason == HOT_FILES || reason == super::build_wait::NOT_IN_BUILD {
                return None;
            }
            let since = timestamp_millis(&event.created_at)? / 1000;
            Some((event.task_id?, (reason.to_owned(), since)))
        })
        .collect()
}

/// The `claim_deferred` of a task whose worker this supervisor cannot run
/// (ADR-t813-2): `reason` from [`super::worker::unavailable`]. It lasts
/// until the supervisor can run the worker or the task leaves the
/// candidates; it has no limit.
pub fn worker_deferred(
    reason: &str,
    worker: super::worker::Worker,
    token: &LeaseToken,
) -> (EventKind, Value) {
    let message = if reason == super::worker::FALLBACK_OFF {
        format!(
            "{} cannot be used now and [provider_fallback] workers is false, so the {} {} worker does not start on the other provider ({reason}): not claimed until {} can be used again",
            worker.provider.as_str(),
            worker.provider.as_str(),
            worker.mode.as_str(),
            worker.provider.as_str()
        )
    } else {
        format!(
            "this supervisor cannot run a {} {} worker ({reason}): not claimed until one can",
            worker.provider.as_str(),
            worker.mode.as_str()
        )
    };
    (
        EventKind::ClaimDeferred,
        json!({
            "reason": reason,
            "provider": worker.provider,
            "worker_mode": worker.mode,
            "message": message,
            "supervisor": token,
        }),
    )
}

/// The end of a deferral for the worker: `why` is `cleared` (the
/// supervisor can run it now) or `not_candidate`. The wait for a build
/// ends the same way ([`super::build_wait::ended`]).
pub fn worker_deferral_ended(
    reason: &str,
    why: &str,
    since: i64,
    now: i64,
    token: &LeaseToken,
) -> (EventKind, Value) {
    (
        EventKind::ClaimDeferralEnded,
        json!({
            "reason": reason,
            "why": why,
            "deferred_secs": (now - since).max(0),
            "supervisor": token,
        }),
    )
}

/// Why the claim is deferred, for the log and the event.
pub fn message(overlap: &Overlap, max_secs: i64) -> String {
    let runs: Vec<String> = overlap
        .runs
        .iter()
        .map(|run| format!("task {} (run {})", run.task_id, run.run_id))
        .collect();
    format!(
        "the task's files meet those of {} on the conflict hotspots {}: not claimed until they land or for {max_secs} seconds",
        runs.join(", "),
        overlap.files.join(", ")
    )
}

/// A deferral in progress, for `status` and `stats`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenDeferral {
    pub task_id: TaskId,
    pub reason: String,
    pub since: String,
    pub files: Value,
    pub runs: Value,
    pub supervisor: Option<String>,
    /// For a wait for the build (ADR-t1632-1): the build that lacks the
    /// landings, and those it lacks (`[{task_id, commit}]`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing: Option<Value>,
}

impl OpenDeferral {
    pub fn of(event: &RunEvent) -> Option<Self> {
        (event.kind == CLAIM_DEFERRED).then(|| Self {
            task_id: event.task_id.unwrap_or(TaskId::new(0)),
            reason: text(event, "reason").unwrap_or("unknown").to_owned(),
            since: event.created_at.clone(),
            files: event.payload.get("files").cloned().unwrap_or(Value::Null),
            runs: event.payload.get("runs").cloned().unwrap_or(Value::Null),
            supervisor: text(event, "supervisor").map(str::to_owned),
            build: text(event, "build").map(str::to_owned),
            missing: event.payload.get("missing").cloned(),
        })
    }
}

/// One way deferrals ended.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WhyEnded {
    pub count: i64,
    pub secs: i64,
}

/// The deferred claims (`stats`' `claim_deferrals`): those that started in
/// the window, the seconds they lasted (one still open lasts to the
/// window's end), how they ended, the hotspots they were deferred on, and
/// the deferrals in progress now.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ClaimDeferrals {
    pub count: i64,
    pub secs: i64,
    pub by_end: BTreeMap<String, WhyEnded>,
    pub by_file: BTreeMap<String, i64>,
    pub deferred: Vec<OpenDeferral>,
}

/// Aggregate the deferrals of `events` (ascending id) that started with
/// `after < id <= upto` on a task `counts` accepts; `end_ms` ends one still
/// open. A deferral ends at the next `claim_deferral_ended` of its task or
/// at its `run_claimed`; `deferred` is the queue's now either way.
pub fn claim_deferrals(
    events: &[RunEvent],
    after: EventId,
    upto: EventId,
    end_ms: i64,
    counts: impl Fn(Option<TaskId>) -> bool,
) -> ClaimDeferrals {
    let mut stats = ClaimDeferrals::default();
    let mut open: BTreeMap<TaskId, (&RunEvent, i64)> = BTreeMap::new();
    let close = |stats: &mut ClaimDeferrals, (event, start): (&RunEvent, i64), end, why: &str| {
        if event.id <= after || event.id > upto || !counts(event.task_id) {
            return;
        }
        let secs = (i64::min(end, end_ms) - start).max(0) / 1000;
        stats.count += 1;
        stats.secs += secs;
        let entry = stats.by_end.entry(why.to_owned()).or_default();
        entry.count += 1;
        entry.secs += secs;
        for file in event
            .payload
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            *stats.by_file.entry(file.to_owned()).or_default() += 1;
        }
    };
    for event in events {
        let Some(task) = event.task_id else {
            continue;
        };
        let at = || timestamp_millis(&event.created_at).unwrap_or(end_ms);
        match event.kind.as_str() {
            CLAIM_DEFERRED => {
                if let Some(held) = open.remove(&task) {
                    close(&mut stats, held, at(), "superseded");
                }
                open.insert(task, (event, at()));
            }
            CLAIM_DEFERRAL_ENDED => {
                if let Some(held) = open.remove(&task) {
                    close(
                        &mut stats,
                        held,
                        at(),
                        text(event, "why").unwrap_or("unknown"),
                    );
                }
            }
            "run_claimed" => {
                if let Some(held) = open.remove(&task) {
                    close(&mut stats, held, at(), "claimed");
                }
            }
            _ => {}
        }
    }
    for (_, held) in open {
        stats.deferred.extend(OpenDeferral::of(held.0));
        close(&mut stats, held, end_ms, "open");
    }
    stats
}

fn text<'e>(event: &'e RunEvent, key: &str) -> Option<&'e str> {
    event.payload.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: i64, task: i64, kind: &str, payload: Value, at: &str) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(task)),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: format!("2026-09-26T01:{at}.000Z"),
            actor: None,
        }
    }

    fn run(id: &str, task: i64, files: &[&str]) -> InFlight {
        InFlight {
            run_id: id.to_owned(),
            task_id: TaskId::new(task),
            files: files.iter().map(|file| (*file).to_owned()).collect(),
            owner_waiting_since: None,
            no_commit: false,
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn files_meet_only_on_a_hotspot_both_sides_touch() {
        let hot = strings(&["docs/plans/current.md", "src/schema.rs"]);
        let in_flight = [
            run("r1", 1, &["docs/**", "src/a.rs"]),
            run("r2", 2, &["src/schema.rs"]),
        ];
        let found = overlap(&hot, &strings(&["docs/plans/*.md"]), &in_flight).unwrap();
        assert_eq!(found.files, strings(&["docs/plans/current.md"]));
        assert_eq!(
            found.runs,
            vec![OverlapRun {
                run_id: "r1".into(),
                task_id: TaskId::new(1)
            }]
        );
        let both = overlap(&hot, &strings(&["**"]), &in_flight).unwrap();
        assert_eq!(both.files.len(), 2);
        assert_eq!(both.runs.len(), 2);
        // A file both touch that is no hotspot, or a hotspot only one side
        // touches, does not defer.
        assert_eq!(overlap(&hot, &strings(&["src/a.rs"]), &in_flight), None);
        assert_eq!(overlap(&hot, &strings(&["src/schema.rs"]), &[]), None);
        assert_eq!(overlap(&[], &strings(&["**"]), &in_flight), None);
        assert_eq!(overlap(&hot, &[], &in_flight), None);
    }

    #[test]
    fn declared_paths_come_before_the_related_landings() {
        assert_eq!(
            expected_files(&strings(&["src/b.rs"]), &strings(&["src/a.rs"])),
            strings(&["src/b.rs"])
        );
        assert_eq!(
            expected_files(&[], &strings(&["src/a.rs", "src/b.rs", "src/a.rs"])),
            strings(&["src/a.rs", "src/b.rs"])
        );
    }

    #[test]
    fn globs_are_left_out_of_the_expected_files() {
        assert_eq!(
            expected_files(&strings(&["docs/**", "src/a.rs"]), &strings(&["x.md"])),
            strings(&["src/a.rs"])
        );
        assert_eq!(
            expected_files(&strings(&["src/*.rs", "a?.md", "b.md"]), &[]),
            strings(&["b.md"])
        );
        assert!(!needs_related_landings(&strings(&["docs/**", "src/a.rs"])));
    }

    #[test]
    fn only_globs_fall_back_to_the_related_landings() {
        let declared = strings(&["docs/**", "*.md"]);
        assert!(needs_related_landings(&declared));
        assert!(needs_related_landings(&[]));
        assert_eq!(
            expected_files(&declared, &strings(&["src/a.rs", "src/a.rs"])),
            strings(&["src/a.rs"])
        );
        assert_eq!(expected_files(&declared, &[]), Vec::<String>::new());
    }

    fn hot() -> Option<Overlap> {
        Some(Overlap {
            files: strings(&["x.md"]),
            runs: vec![OverlapRun {
                run_id: "r1".into(),
                task_id: TaskId::new(1),
            }],
        })
    }

    #[test]
    fn a_deferral_starts_holds_and_ends_once() {
        let mut deferral = None;
        let Decision::Defer {
            event: Some((kind, payload)),
        } = decide(
            false,
            hot(),
            None,
            &mut deferral,
            100,
            60,
            &LeaseToken::new("s"),
        )
        else {
            panic!("deferred with its event")
        };
        assert_eq!(kind, EventKind::ClaimDeferred);
        assert_eq!(payload["reason"], "hot_files");
        assert_eq!(payload["files"], json!(["x.md"]));
        assert_eq!(payload["runs"][0]["run_id"], "r1");
        assert_eq!(payload["max_secs"], 60);
        assert!(payload["message"].as_str().unwrap().contains("task 1"));
        assert_eq!(
            deferral,
            Some(Deferral {
                since: 100,
                expired: false
            })
        );
        // Still meeting: deferred again, not recorded again.
        assert_eq!(
            decide(
                false,
                hot(),
                None,
                &mut deferral,
                130,
                60,
                &LeaseToken::new("s")
            ),
            Decision::Defer { event: None }
        );
        // The files no longer meet: claimed, and the deferral ends.
        let Decision::Claim {
            event: Some((kind, payload)),
        } = decide(
            false,
            None,
            None,
            &mut deferral,
            150,
            60,
            &LeaseToken::new("s"),
        )
        else {
            panic!("claimed with the end")
        };
        assert_eq!(kind, EventKind::ClaimDeferralEnded);
        assert_eq!(payload["why"], "cleared");
        assert_eq!(payload["deferred_secs"], 50);
        assert_eq!(deferral, None);
        assert_eq!(
            decide(
                false,
                None,
                None,
                &mut deferral,
                160,
                60,
                &LeaseToken::new("s")
            ),
            Decision::Claim { event: None }
        );
    }

    #[test]
    fn an_interrupt_is_never_deferred_and_ends_a_deferral() {
        let mut deferral = None;
        assert_eq!(
            decide(
                true,
                hot(),
                None,
                &mut deferral,
                100,
                60,
                &LeaseToken::new("s")
            ),
            Decision::Claim { event: None }
        );
        let mut deferral = Some(Deferral {
            since: 100,
            expired: false,
        });
        let Decision::Claim {
            event: Some((_, payload)),
        } = decide(
            true,
            hot(),
            None,
            &mut deferral,
            110,
            60,
            &LeaseToken::new("s"),
        )
        else {
            panic!("an interrupt is claimed")
        };
        assert_eq!(payload["why"], "cleared");
    }

    #[test]
    fn a_deferral_past_its_limit_is_claimed_and_not_deferred_again() {
        let mut deferral = Some(Deferral {
            since: 100,
            expired: false,
        });
        let Decision::Claim {
            event: Some((kind, payload)),
        } = decide(
            false,
            hot(),
            None,
            &mut deferral,
            160,
            60,
            &LeaseToken::new("s"),
        )
        else {
            panic!("expired")
        };
        assert_eq!(kind, EventKind::ClaimDeferralEnded);
        assert_eq!(payload["why"], "expired");
        assert_eq!(payload["deferred_secs"], 60);
        assert!(deferral.unwrap().expired);
        // Not claimed in that pass (no slot): the next one claims it, and
        // records nothing more.
        assert_eq!(
            decide(
                false,
                hot(),
                None,
                &mut deferral,
                170,
                60,
                &LeaseToken::new("s")
            ),
            Decision::Claim { event: None }
        );
        // The files stop meeting and meet again: still expired, not
        // deferred anew until the task is claimed.
        assert_eq!(
            decide(
                false,
                None,
                None,
                &mut deferral,
                180,
                60,
                &LeaseToken::new("s")
            ),
            Decision::Claim { event: None }
        );
        assert!(deferral.unwrap().expired);
        assert_eq!(
            decide(
                false,
                hot(),
                None,
                &mut deferral,
                190,
                60,
                &LeaseToken::new("s")
            ),
            Decision::Claim { event: None }
        );
        // A limit of 0 defers nothing.
        let mut none = None;
        assert_eq!(
            decide(false, hot(), None, &mut none, 100, 0, &LeaseToken::new("s")),
            Decision::Claim { event: None }
        );
        assert_eq!(none, None);
    }

    fn ask(kind: AskKind, created_at: i64) -> Ask {
        Ask {
            recommendation: None,
            confidence: None,
            topics: Vec::new(),
            id: super::super::AskId::new(7),
            kind,
            task_id: None,
            run_id: None,
            question: "q".into(),
            options: Vec::new(),
            answer: None,
            asked_by: "worker".into(),
            reason_category: super::super::AskReason::Scope,
            subject: None,
            affected: Vec::new(),
            created_at,
            answered_at: None,
            closed_at: None,
            finding_id: None,
            request_id: None,
            answered_by: None,
            option_index: None,
            answer_authority: None,
            answer_approval: None,
        }
    }

    fn second(at: &str) -> i64 {
        timestamp_millis(&format!("2026-09-26T01:{at}.000Z")).unwrap() / 1000
    }

    #[test]
    fn a_run_waits_for_its_owner_on_an_open_ask_in_a_wait_or_without_a_lease() {
        let asked = second("00:10");
        let question = [ask(AskKind::WorkerQuestion, asked)];
        let waiting = event(
            1,
            1,
            "run_waiting_started",
            json!({"ask_id": 7, "ask_kind": "worker_question", "phase": "session", "status": "running"}),
            "00:20",
        );
        // In a wait (ADR-0071): from its start, even with the lease.
        assert_eq!(
            owner_waiting_since(&question, true, std::slice::from_ref(&waiting)),
            Some(second("00:20"))
        );
        // Returning to a slot, the run moves.
        let ended = event(
            2,
            1,
            "run_waiting_ended",
            json!({"cause": "answered"}),
            "00:30",
        );
        assert_eq!(
            owner_waiting_since(&question, true, &[waiting.clone(), ended.clone()]),
            None
        );
        // Leased and out of a wait: worked on (validating, review, resume).
        assert_eq!(owner_waiting_since(&question, true, &[]), None);
        // No lease: from the ask, or from the lease's end after it.
        assert_eq!(owner_waiting_since(&question, false, &[]), Some(asked));
        let released = event(3, 1, "lease_released", json!({}), "00:40");
        assert_eq!(
            owner_waiting_since(&question, false, &[waiting, ended, released]),
            Some(second("00:40"))
        );
        let before = event(4, 1, "lease_released", json!({}), "00:05");
        assert_eq!(
            owner_waiting_since(&question, false, &[before]),
            Some(asked)
        );
        // An answered or closed ask, or one of a kind that does not stop
        // the run on a person, waits for nobody.
        let mut answered = ask(AskKind::ApproveLanding, asked);
        answered.answered_at = Some(asked + 1);
        let mut closed = ask(AskKind::StuckExit, asked);
        closed.closed_at = Some(asked + 1);
        let held = ask(AskKind::QueueHold, asked);
        assert_eq!(
            owner_waiting_since(&[answered, closed, held], false, &[]),
            None
        );
        for kind in [
            AskKind::WorkerQuestion,
            AskKind::ApproveLanding,
            AskKind::StuckExit,
            AskKind::AnswerPrompt,
            AskKind::Stalled,
            AskKind::Decide,
        ] {
            assert!(waits_for_owner(&kind), "{kind}");
        }
        assert!(!waits_for_owner(&AskKind::Blocked));
    }

    #[test]
    fn a_run_waiting_for_its_owner_past_the_grace_holds_no_claim_back() {
        let hot_files = strings(&["x.md"]);
        let waiting = InFlight {
            owner_waiting_since: Some(100),
            ..run("r1", 1, &["x.md"])
        };
        let working = run("r2", 2, &["x.md"]);
        // Within the grace, the waiting run still counts.
        assert_eq!(counted(std::slice::from_ref(&waiting), 699, 600).len(), 1);
        assert!(counted(std::slice::from_ref(&waiting), 700, 600).is_empty());
        let both = [waiting.clone(), working.clone()];
        let left = counted(&both, 700, 600);
        assert_eq!(left, [working]);
        assert!(overlap(&hot_files, &hot_files, &left).is_some());
        // Only the waiting run in the way: the deferral ends as
        // owner_waiting and the task is claimed before the limit.
        let mut deferral = Some(Deferral {
            since: 100,
            expired: false,
        });
        let Decision::Claim {
            event: Some((kind, payload)),
        } = decide(
            false,
            None,
            Some(OWNER_WAITING),
            &mut deferral,
            700,
            3600,
            &LeaseToken::new("s"),
        )
        else {
            panic!("claimed past the grace")
        };
        assert_eq!(kind, EventKind::ClaimDeferralEnded);
        assert_eq!(payload["why"], OWNER_WAITING);
        assert_eq!(payload["deferred_secs"], 600);
        assert_eq!(deferral, None);
        // A new deferral is not started on such a run alone.
        assert_eq!(
            decide(
                false,
                None,
                Some(OWNER_WAITING),
                &mut deferral,
                710,
                3600,
                &LeaseToken::new("s")
            ),
            Decision::Claim { event: None }
        );
        // Moving again, the run defers anew.
        assert!(matches!(
            decide(
                false,
                hot(),
                None,
                &mut deferral,
                720,
                3600,
                &LeaseToken::new("s")
            ),
            Decision::Defer { event: Some(_) }
        ));
    }

    #[test]
    fn a_failed_run_with_no_commit_of_its_own_holds_no_claim_back() {
        let changed = strings(&["x.md"]);
        assert!(failed_without_commit(RunStatus::Failed, None));
        assert!(failed_without_commit(RunStatus::Failed, Some(&[])));
        // A commit of its own, or a run still worked on, is in the way.
        assert!(!failed_without_commit(RunStatus::Failed, Some(&changed)));
        for status in [
            RunStatus::Running,
            RunStatus::Validating,
            RunStatus::NeedsSession,
            RunStatus::AwaitingIntegration,
        ] {
            assert!(!failed_without_commit(status, Some(&[])), "{status:?}");
        }
        let hot_files = strings(&["x.md"]);
        let failed = InFlight {
            no_commit: true,
            ..run("r1", 1, &["x.md"])
        };
        let committed = run("r2", 2, &["x.md"]);
        let waiting = InFlight {
            owner_waiting_since: Some(100),
            ..run("r3", 3, &["x.md"])
        };
        // Left out at once, without the grace.
        assert!(counted(std::slice::from_ref(&failed), 100, 600).is_empty());
        let both = [failed.clone(), committed.clone()];
        let left = counted(&both, 100, 600);
        assert_eq!(left, [committed]);
        assert_eq!(left_out(&hot_files, &hot_files, &both, &left), None);
        let alone = [failed.clone()];
        assert_eq!(
            left_out(&hot_files, &hot_files, &alone, &[]),
            Some(NO_COMMIT)
        );
        // With a run waiting for a person past the grace, it is that wait.
        let with_waiting = [failed, waiting];
        let left = counted(&with_waiting, 700, 600);
        assert!(left.is_empty());
        assert_eq!(
            left_out(&hot_files, &hot_files, &with_waiting, &left),
            Some(OWNER_WAITING)
        );
        assert_eq!(left_out(&hot_files, &[], &with_waiting, &left), None);
        // The deferral ends as no_commit, long before the limit.
        let mut deferral = Some(Deferral {
            since: 100,
            expired: false,
        });
        let Decision::Claim {
            event: Some((kind, payload)),
        } = decide(
            false,
            None,
            Some(NO_COMMIT),
            &mut deferral,
            130,
            3600,
            &LeaseToken::new("s"),
        )
        else {
            panic!("claimed past the failed run")
        };
        assert_eq!(kind, EventKind::ClaimDeferralEnded);
        assert_eq!(payload["why"], NO_COMMIT);
        assert_eq!(payload["deferred_secs"], 30);
        assert_eq!(deferral, None);
    }

    #[test]
    fn a_task_that_leaves_the_candidates_ends_its_open_deferral() {
        let open = Deferral {
            since: 100,
            expired: false,
        };
        let (kind, payload) = left(open, 130, &LeaseToken::new("s")).unwrap();
        assert_eq!(kind, EventKind::ClaimDeferralEnded);
        assert_eq!(payload["why"], "not_candidate");
        assert_eq!(payload["deferred_secs"], 30);
        assert_eq!(
            left(
                Deferral {
                    expired: true,
                    ..open
                },
                130,
                &LeaseToken::new("s")
            ),
            None
        );
    }

    #[test]
    fn the_deferrals_in_place_come_from_each_tasks_latest_event() {
        let latest = [
            event(1, 1, CLAIM_DEFERRED, json!({}), "00:10"),
            event(
                2,
                2,
                CLAIM_DEFERRAL_ENDED,
                json!({"why": "expired"}),
                "00:20",
            ),
            event(
                3,
                3,
                CLAIM_DEFERRAL_ENDED,
                json!({"why": "cleared"}),
                "00:30",
            ),
            event(4, 4, "run_claimed", json!({}), "00:40"),
        ];
        let place = deferrals_in_place(&latest);
        assert_eq!(place.len(), 2);
        let first = place[&TaskId::new(1)];
        assert!(!first.expired);
        assert_eq!(
            first.since,
            timestamp_millis("2026-09-26T01:00:10.000Z").unwrap() / 1000
        );
        assert!(place[&TaskId::new(2)].expired);
    }

    #[test]
    fn stats_count_the_deferrals_by_how_they_ended_and_show_the_open_ones() {
        let deferred = |id, task, at| {
            event(
                id,
                task,
                CLAIM_DEFERRED,
                json!({"reason": "hot_files", "files": ["x.md"], "runs": [], "supervisor": "s"}),
                at,
            )
        };
        let events = [
            deferred(1, 1, "00:00"),
            deferred(2, 2, "00:00"),
            deferred(3, 3, "00:00"),
            event(
                4,
                1,
                CLAIM_DEFERRAL_ENDED,
                json!({"why": "cleared"}),
                "00:30",
            ),
            event(5, 2, "run_claimed", json!({}), "00:40"),
            event(6, 9, "run_claimed", json!({}), "00:40"),
        ];
        let end = timestamp_millis("2026-09-26T01:01:00.000Z").unwrap();
        let stats = claim_deferrals(&events, EventId::new(0), EventId::new(6), end, |_| true);
        assert_eq!(stats.count, 3);
        assert_eq!(stats.secs, 30 + 40 + 60);
        assert_eq!(stats.by_end["cleared"], WhyEnded { count: 1, secs: 30 });
        assert_eq!(stats.by_end["claimed"], WhyEnded { count: 1, secs: 40 });
        assert_eq!(stats.by_end["open"], WhyEnded { count: 1, secs: 60 });
        assert_eq!(stats.by_file["x.md"], 3);
        assert_eq!(stats.deferred.len(), 1);
        assert_eq!(stats.deferred[0].task_id, TaskId::new(3));
        assert_eq!(stats.deferred[0].files, json!(["x.md"]));
        assert_eq!(stats.deferred[0].supervisor.as_deref(), Some("s"));
        // Out of the window or of the goal: not counted, still shown.
        let later = claim_deferrals(&events, EventId::new(3), EventId::new(6), end, |_| true);
        assert_eq!(later.count, 0);
        assert_eq!(later.deferred.len(), 1);
        let none = claim_deferrals(&events, EventId::new(0), EventId::new(6), end, |_| false);
        assert_eq!(none.count, 0);
        // A deferral recorded again replaces the one before.
        let again = [deferred(1, 1, "00:00"), deferred(2, 1, "00:10")];
        let stats = claim_deferrals(&again, EventId::new(0), EventId::new(2), end, |_| true);
        assert_eq!(stats.by_end["superseded"].count, 1);
        assert_eq!(stats.deferred.len(), 1);
    }

    /// A claim the fallback turned off keeps here says so (ADR-t1857-1),
    /// and is a deferral for the worker like any other.
    #[test]
    fn a_deferral_for_the_fallback_off_says_its_provider_waits() {
        let token = LeaseToken::new("s");
        let worker = super::super::worker::Worker::ALL[1];
        let (_, payload) = worker_deferred(super::super::worker::FALLBACK_OFF, worker, &token);
        assert_eq!(payload["reason"], "provider_fallback_off");
        assert_eq!(payload["provider"], "claude");
        let message = payload["message"].as_str().unwrap();
        assert!(
            message.contains("claude cannot be used now")
                && message.contains("[provider_fallback] workers is false"),
            "{message}"
        );
        let latest = [event(1, 1, CLAIM_DEFERRED, payload, "00:00")];
        assert_eq!(
            worker_deferrals_in_place(&latest)[&TaskId::new(1)].0,
            "provider_fallback_off"
        );
    }

    #[test]
    fn a_deferral_for_the_worker_is_kept_apart_from_those_on_hotspots() {
        let token = LeaseToken::new("s");
        let worker = super::super::worker::Worker::ALL[2];
        let (kind, payload) = worker_deferred("provider_unavailable", worker, &token);
        assert_eq!(kind, EventKind::ClaimDeferred);
        assert_eq!(payload["provider"], "codex");
        assert_eq!(payload["worker_mode"], "headless");
        assert!(
            payload["message"]
                .as_str()
                .unwrap()
                .contains("codex headless")
        );
        let latest = [
            event(1, 1, CLAIM_DEFERRED, payload, "00:00"),
            event(2, 2, CLAIM_DEFERRED, json!({"reason": HOT_FILES}), "00:00"),
            // A deferral from before the reason existed is on hotspots.
            event(3, 3, CLAIM_DEFERRED, json!({}), "00:00"),
        ];
        let workers = worker_deferrals_in_place(&latest);
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[&TaskId::new(1)].0, "provider_unavailable");
        let hot = deferrals_in_place(&latest);
        let mut tasks: Vec<i64> = hot.keys().map(|task| task.as_i64()).collect();
        tasks.sort_unstable();
        assert_eq!(tasks, [2, 3]);
        let since = workers[&TaskId::new(1)].1;
        let (kind, ended) =
            worker_deferral_ended("provider_unavailable", "cleared", since, since + 5, &token);
        assert_eq!(kind, EventKind::ClaimDeferralEnded);
        assert_eq!(ended["why"], "cleared");
        assert_eq!(ended["deferred_secs"], 5);
        // Ended, it is no longer in place.
        let latest = [event(4, 1, CLAIM_DEFERRAL_ENDED, ended, "00:05")];
        assert!(worker_deferrals_in_place(&latest).is_empty());
        assert!(deferrals_in_place(&latest).is_empty());
    }
}
