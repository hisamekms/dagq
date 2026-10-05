//! Waiting with the claim of a task until the supervisor's own build
//! contains the landings of the tasks it depends on (ADR-t1632-1). A task
//! that declares it (`add --wait-for-build`) is a candidate once its
//! dependencies are completed, like any other; the supervisor passes over
//! it while the commit of its build identifier lacks one of their landed
//! commits, and claims it from the first pass of a build that has them all
//! (after an automatic update hands the supervisor over to it). The wait is
//! recorded like the other deferrals of one task ([`super::claim_defer`]):
//! `claim_deferred` with the reason [`NOT_IN_BUILD`] when it starts, and
//! `claim_deferral_ended` when it ends. It has no limit.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

pub use super::views::Landing;
use super::{
    EventKind, LeaseToken, RunEvent, TaskId,
    claim_defer::{CLAIM_DEFERRED, Decision, Deferral, worker_deferral_ended},
    stats::timestamp_millis,
};

/// The reason of the `claim_deferred` of a task waiting for a build that
/// contains its dependencies' landings.
pub const NOT_IN_BUILD: &str = "not_in_build";

/// What the build says of a task that waits for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The build contains every landing: the task may be claimed.
    Contains,
    /// The build lacks these landings, or whether it contains them could
    /// not be read: the task waits.
    Lacks(Vec<Landing>),
    /// The build names no commit (a release, `+unknown`): nothing tells
    /// whether it contains them, and the task is claimed as if it declared
    /// nothing, rather than wait for a build that may never come.
    Unjudged,
}

/// Judge the landings of a task's dependencies against `build`, the
/// supervisor's build identifier, by the commit it names
/// ([`crate::build_id::named_commit`]: a dirty build by its commit, a
/// release or `+unknown` by none). `contains(landing, commit)` says whether
/// the build's commit has the landed commit among its ancestors; `None`
/// (it could not be read) counts as lacking it.
pub fn judge(
    build: &str,
    landings: &[Landing],
    mut contains: impl FnMut(&str, &str) -> Option<bool>,
) -> Verdict {
    let Some(commit) = crate::build_id::named_commit(build) else {
        return Verdict::Unjudged;
    };
    let missing: Vec<Landing> = landings
        .iter()
        .filter(|landing| contains(&landing.commit, commit) != Some(true))
        .cloned()
        .collect();
    if missing.is_empty() {
        Verdict::Contains
    } else {
        Verdict::Lacks(missing)
    }
}

/// What a pass does with a task that waits for the build, from its
/// `verdict`: the landings it waits for, `None` to claim it (the build
/// contains them all, or names no commit), and whether to warn that it is
/// claimed without the wait, the first time `warned` meets the task.
pub fn claim_step(
    verdict: Verdict,
    task: TaskId,
    warned: &mut HashSet<TaskId>,
) -> (Option<Vec<Landing>>, bool) {
    match verdict {
        Verdict::Contains => (None, false),
        Verdict::Lacks(missing) => (Some(missing), false),
        Verdict::Unjudged => (None, warned.insert(task)),
    }
}

/// Judge one candidate's wait for the build this pass. `lacking` is what
/// [`claim_step`] said (`None`: the build serves the task); `worker_cleared`
/// that its deferral for the worker ended this pass. `waits` holds the
/// waits in place (since when) and `hot` the deferrals on hotspots
/// ([`super::claim_defer`]); both are updated here so the task's latest
/// deferral event always describes the deferral in place, which `status`,
/// `show` and a restart read:
///
/// - a wait that starts records `claim_deferred` and replaces an open (not
///   expired) deferral on a hotspot, which the hotspot judgement records
///   again once the build serves the task;
/// - a wait in place records nothing more, unless the worker's deferral
///   just ended, whose end is then the latest: the wait is recorded again;
/// - a wait that the build now serves ends with `cleared`.
///
/// `Defer` passes over the task; `Claim` hands it on to the hotspot
/// judgement.
#[allow(clippy::too_many_arguments)]
pub fn decide(
    task: TaskId,
    lacking: Option<Vec<Landing>>,
    worker_cleared: bool,
    waits: &mut HashMap<TaskId, i64>,
    hot: &mut HashMap<TaskId, Deferral>,
    build: &str,
    now: i64,
    token: &LeaseToken,
) -> Decision {
    if worker_cleared {
        waits.remove(&task);
    }
    match (lacking, waits.get(&task).copied()) {
        (Some(_), Some(_)) => Decision::Defer { event: None },
        (Some(missing), None) => {
            if hot.get(&task).is_some_and(|deferral| !deferral.expired) {
                hot.remove(&task);
            }
            waits.insert(task, now);
            Decision::Defer {
                event: Some(deferred(build, &missing, token)),
            }
        }
        (None, Some(since)) => {
            waits.remove(&task);
            Decision::Claim {
                event: Some(ended("cleared", since, now, token)),
            }
        }
        (None, None) => Decision::Claim { event: None },
    }
}

/// End the waits of the tasks that left `candidates` (claimed elsewhere,
/// canceled, blocked again) with `not_candidate`, in task order.
pub fn left(
    waits: &mut HashMap<TaskId, i64>,
    candidates: &HashSet<TaskId>,
    now: i64,
    token: &LeaseToken,
) -> Vec<(TaskId, (EventKind, Value))> {
    let mut gone: Vec<(TaskId, i64)> = waits
        .iter()
        .filter(|(task, _)| !candidates.contains(task))
        .map(|(task, since)| (*task, *since))
        .collect();
    gone.sort_unstable();
    gone.into_iter()
        .map(|(task, since)| {
            waits.remove(&task);
            (task, ended("not_candidate", since, now, token))
        })
        .collect()
}

/// The `claim_deferred` of a task whose dependencies' landings `build`
/// lacks: `build`, the landings it lacks (`missing`), `message`,
/// `supervisor`.
pub fn deferred(build: &str, missing: &[Landing], token: &LeaseToken) -> (EventKind, Value) {
    let lacking: Vec<String> = missing
        .iter()
        .map(|landing| format!("task {} ({})", landing.task_id, landing.commit))
        .collect();
    let message = format!(
        "this supervisor's build {build} does not contain the landing of {}: not claimed until a build that does takes over",
        lacking.join(", ")
    );
    (
        EventKind::ClaimDeferred,
        json!({
            "reason": NOT_IN_BUILD,
            "build": build,
            "missing": missing,
            "message": message,
            "supervisor": token,
        }),
    )
}

/// The end of the wait: `why` is `cleared` (the build contains the
/// landings, or the task no longer declares the wait) or `not_candidate`.
pub fn ended(why: &str, since: i64, now: i64, token: &LeaseToken) -> (EventKind, Value) {
    worker_deferral_ended(NOT_IN_BUILD, why, since, now, token)
}

/// The waits in place from each task's latest deferral event (an open
/// `claim_deferred` of the reason [`NOT_IN_BUILD`]), with since when (unix
/// seconds): what a supervisor starts from, after a handoff too.
pub fn deferrals_in_place(latest: &[RunEvent]) -> HashMap<TaskId, i64> {
    latest
        .iter()
        .filter(|event| {
            event.kind == CLAIM_DEFERRED
                && event.payload.get("reason").and_then(Value::as_str) == Some(NOT_IN_BUILD)
        })
        .filter_map(|event| {
            let since = timestamp_millis(&event.created_at)? / 1000;
            Some((event.task_id?, since))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, claim_defer::worker_deferrals_in_place};

    const LANDED: &str = "1111111111111111111111111111111111111111";
    const OTHER: &str = "2222222222222222222222222222222222222222";
    const BUILD: &str = "3333333333333333333333333333333333333333";

    fn landings() -> Vec<Landing> {
        vec![
            Landing {
                task_id: TaskId::new(1),
                commit: LANDED.into(),
            },
            Landing {
                task_id: TaskId::new(2),
                commit: OTHER.into(),
            },
        ]
    }

    /// The build's history as a set of the commits it contains.
    fn history<'h>(contained: &'h [&str]) -> impl FnMut(&str, &str) -> Option<bool> + 'h {
        move |landing, commit| {
            assert_eq!(commit, BUILD, "judged against the build's commit");
            Some(contained.contains(&landing))
        }
    }

    #[test]
    fn a_build_that_contains_every_landing_lets_the_task_be_claimed() {
        let build = format!("0.4.0-dev+{BUILD}");
        assert_eq!(
            judge(&build, &landings(), history(&[LANDED, OTHER])),
            Verdict::Contains
        );
        // A task with no landing to wait for is claimed.
        assert_eq!(judge(&build, &[], history(&[])), Verdict::Contains);
    }

    #[test]
    fn a_build_that_lacks_a_landing_or_cannot_tell_holds_the_task() {
        let build = format!("0.4.0-dev+{BUILD}");
        assert_eq!(
            judge(&build, &landings(), history(&[LANDED])),
            Verdict::Lacks(vec![landings()[1].clone()])
        );
        // A landing the repository cannot place is not taken as contained.
        assert_eq!(
            judge(&build, &landings(), |landing, _| (landing == LANDED)
                .then_some(true)),
            Verdict::Lacks(vec![landings()[1].clone()])
        );
    }

    #[test]
    fn a_dirty_build_is_judged_by_its_commit_and_one_without_a_commit_is_not_judged() {
        let dirty = format!("0.4.0-dev+{BUILD}.dirty");
        assert_eq!(
            judge(&dirty, &landings(), history(&[LANDED, OTHER])),
            Verdict::Contains
        );
        assert_eq!(
            judge(&dirty, &landings(), history(&[OTHER])),
            Verdict::Lacks(vec![landings()[0].clone()])
        );
        for build in ["0.4.0", "0.4.0-dev+unknown", "0.4.0-dev+"] {
            assert_eq!(
                judge(build, &landings(), |_, _| unreachable!()),
                Verdict::Unjudged
            );
        }
    }

    #[test]
    fn a_task_is_claimed_unless_the_build_lacks_a_landing_and_warned_of_once_without_a_commit() {
        let mut warned = HashSet::new();
        let task = TaskId::new(7);
        assert_eq!(
            claim_step(Verdict::Contains, task, &mut warned),
            (None, false)
        );
        let missing = vec![landings()[0].clone()];
        assert_eq!(
            claim_step(Verdict::Lacks(missing.clone()), task, &mut warned),
            (Some(missing), false)
        );
        // A build with no commit claims it; the warning is the first pass's.
        let unjudged = judge("0.4.0", &landings(), |_, _| unreachable!());
        assert_eq!(
            claim_step(unjudged.clone(), task, &mut warned),
            (None, true)
        );
        assert_eq!(
            claim_step(unjudged.clone(), task, &mut warned),
            (None, false)
        );
        assert_eq!(
            claim_step(unjudged, TaskId::new(8), &mut warned),
            (None, true)
        );
    }

    fn why(decision: &Decision) -> Option<(&str, &str)> {
        let (Decision::Claim { event } | Decision::Defer { event }) = decision;
        event.as_ref().map(|(_, payload)| {
            (
                payload["reason"].as_str().unwrap(),
                payload["why"].as_str().unwrap_or("deferred"),
            )
        })
    }

    #[test]
    fn a_wait_starts_once_holds_and_ends_when_the_build_serves_the_task() {
        let token = LeaseToken::new("s");
        let task = TaskId::new(7);
        let (mut waits, mut hot) = (HashMap::new(), HashMap::new());
        let missing = || Some(vec![landings()[0].clone()]);
        let mut pass = |lacking, worker_cleared, now| {
            decide(
                task,
                lacking,
                worker_cleared,
                &mut waits,
                &mut hot,
                "b",
                now,
                &token,
            )
        };
        let started = pass(missing(), false, 10);
        assert!(matches!(started, Decision::Defer { .. }));
        assert_eq!(why(&started), Some((NOT_IN_BUILD, "deferred")));
        // In place: nothing more is recorded.
        assert_eq!(pass(missing(), false, 20), Decision::Defer { event: None });
        let served = pass(None, false, 30);
        assert!(matches!(served, Decision::Claim { .. }));
        assert_eq!(why(&served), Some((NOT_IN_BUILD, "cleared")));
        let Decision::Claim {
            event: Some((_, end)),
        } = served
        else {
            unreachable!()
        };
        assert_eq!(end["deferred_secs"], 20, "since the wait started");
        // A task that waits for nothing passes on with nothing recorded.
        assert_eq!(pass(None, false, 40), Decision::Claim { event: None });
        assert!(waits.is_empty());
    }

    #[test]
    fn the_latest_deferral_event_describes_the_wait_when_deferrals_overlap() {
        let token = LeaseToken::new("s");
        let task = TaskId::new(7);
        let missing = || Some(vec![landings()[0].clone()]);
        // The worker's deferral ended this pass over a wait in place: the
        // wait is recorded again, from now.
        let mut waits = HashMap::from([(task, 10)]);
        let mut hot = HashMap::new();
        let again = decide(task, missing(), true, &mut waits, &mut hot, "b", 50, &token);
        assert_eq!(why(&again), Some((NOT_IN_BUILD, "deferred")));
        assert_eq!(waits[&task], 50);
        // Served at once, it ends no wait the worker's end already hid.
        let mut waits = HashMap::from([(task, 10)]);
        assert_eq!(
            decide(task, None, true, &mut waits, &mut hot, "b", 50, &token),
            Decision::Claim { event: None }
        );
        // A wait that starts replaces an open deferral on a hotspot; one
        // past its limit stays, so the task is not deferred on it again.
        let open = Deferral {
            since: 5,
            expired: false,
        };
        let mut hot = HashMap::from([(task, open)]);
        let mut waits = HashMap::new();
        decide(
            task,
            missing(),
            false,
            &mut waits,
            &mut hot,
            "b",
            50,
            &token,
        );
        assert!(hot.is_empty());
        let expired = Deferral {
            since: 5,
            expired: true,
        };
        let mut hot = HashMap::from([(task, expired)]);
        let mut waits = HashMap::new();
        decide(
            task,
            missing(),
            false,
            &mut waits,
            &mut hot,
            "b",
            50,
            &token,
        );
        assert_eq!(hot[&task], expired);
    }

    #[test]
    fn a_wait_whose_task_left_the_candidates_ends_as_not_candidate() {
        let token = LeaseToken::new("s");
        let mut waits = HashMap::from([
            (TaskId::new(9), 10),
            (TaskId::new(7), 20),
            (TaskId::new(8), 30),
        ]);
        let candidates = HashSet::from([TaskId::new(8)]);
        let ended = left(&mut waits, &candidates, 40, &token);
        let tasks: Vec<i64> = ended.iter().map(|(task, _)| task.as_i64()).collect();
        assert_eq!(tasks, [7, 9]);
        assert!(ended.iter().all(|(_, (kind, payload))| {
            *kind == EventKind::ClaimDeferralEnded
                && payload["why"] == "not_candidate"
                && payload["reason"] == NOT_IN_BUILD
        }));
        assert_eq!(ended[0].1.1["deferred_secs"], 20);
        assert_eq!(waits.keys().copied().collect::<Vec<_>>(), [TaskId::new(8)]);
    }

    #[test]
    fn the_wait_is_recorded_apart_from_the_deferrals_for_the_worker() {
        let token = LeaseToken::new("s");
        let missing = vec![landings()[1].clone()];
        let (kind, payload) = deferred("0.4.0-dev+abc", &missing, &token);
        assert_eq!(kind, EventKind::ClaimDeferred);
        assert_eq!(payload["reason"], NOT_IN_BUILD);
        assert_eq!(payload["build"], "0.4.0-dev+abc");
        assert_eq!(payload["missing"], json!([{"task_id": 2, "commit": OTHER}]));
        assert!(payload["message"].as_str().unwrap().contains("task 2"));
        let event = |id, task, kind: &str, payload: Value| RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(task)),
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: "2026-10-05T01:00:00.000Z".into(),
            actor: None,
        };
        let latest = [
            event(1, 7, CLAIM_DEFERRED, payload),
            event(
                2,
                8,
                CLAIM_DEFERRED,
                json!({"reason": "provider_unavailable"}),
            ),
        ];
        let place = deferrals_in_place(&latest);
        assert_eq!(place.keys().copied().collect::<Vec<_>>(), [TaskId::new(7)]);
        // Not taken for a deferral for the worker.
        let workers = worker_deferrals_in_place(&latest);
        assert_eq!(
            workers.keys().copied().collect::<Vec<_>>(),
            [TaskId::new(8)]
        );
        let since = place[&TaskId::new(7)];
        let (kind, end) = ended("cleared", since, since + 9, &token);
        assert_eq!(kind, EventKind::ClaimDeferralEnded);
        assert_eq!(
            (&end["reason"], &end["why"], &end["deferred_secs"]),
            (&json!(NOT_IN_BUILD), &json!("cleared"), &json!(9))
        );
        let latest = [event(3, 7, "claim_deferral_ended", end)];
        assert!(deferrals_in_place(&latest).is_empty());
    }
}
