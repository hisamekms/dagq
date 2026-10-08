//! `planner request` (ADR-t1533-1): a person, or the inbox at a person's
//! word, hands a follow-up request to a planner of the runtime's that is
//! open, naming it by its planner id. The words are handed over as a file
//! under the planner's directory with the fixed sentence that points at it
//! ([`hand_request_to_planner`]), and the sentence is written as the
//! headless planner's next turn request: a turn at work is not refused,
//! the request is taken after it. Nothing is typed into a terminal: an
//! interactive planner, a person's, one that is closed, lost, exited or
//! asked to exit, and one that waits for Claude after its turn failed at
//! the login, the usage limit or the start (its `provider retry` carries
//! the failed call first, ADR-t1394-2 decision 5) are refused with the
//! reason. The caller authorizes the
//! command first (`planner.request`; `docs/design/authorization.md`).

use std::path::Path;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::planner::{PlannerProbes, PlannerView, planner_view};
use super::planner_handoff::{PLANNER_REQUESTS_DIR, hand_request_to_planner};
use super::supervise::{lock_waiting, provider_failure, write_request};
use super::{Queue, planner_idle_marker};
use crate::domain::turn::{TurnFailure, TurnMark, exit_path, request_path};
use crate::domain::{
    EventKind, PlannerId, PlannerOrigin, PlannerRoute, PlannerSession, PlannerState,
};

/// The lock in a planner's directory that a follow-up request is named,
/// written and requested under, so two at once get their own files.
const FOLLOW_UP_LOCK: &str = "requests.lock";

/// What the name of a follow-up request handed to a planner starts with:
/// `followup-1`, `followup-2`, ... under its `requests/`.
pub const FOLLOW_UP_PREFIX: &str = "followup-";

/// Why `planner` takes no follow-up request, from its row alone: only a
/// headless planner of the runtime's, not closed, takes one.
pub fn refused_by_row(planner: &PlannerSession) -> Option<String> {
    let id = planner.id;
    if planner.origin != PlannerOrigin::Runtime {
        return Some(format!(
            "planner {id} was opened by a person: a follow-up request goes only to a planner of the runtime's (record a new request with `request add`)"
        ));
    }
    if planner.route != PlannerRoute::Headless {
        return Some(format!(
            "planner {id} is interactive: a follow-up request goes only to a headless planner, as its next turn, and nothing is typed into a terminal"
        ));
    }
    if planner.closed_at.is_some() {
        return Some(format!("planner {id} is closed"));
    }
    None
}

/// Why the headless planner `id`, judged `state`, takes no follow-up
/// request now: it is closed, lost or exited, its exit is requested, or
/// it is idle at `wall`, the provider failure its last turn stopped at.
/// One opening, at work or idle takes it.
pub fn refused_by_state(
    id: PlannerId,
    state: PlannerState,
    exit_requested: bool,
    wall: Option<TurnFailure>,
) -> Option<String> {
    if !state.alive() {
        return Some(format!(
            "planner {id} is {}: a follow-up request goes only to a live planner (record a new request with `request add`)",
            state.as_str()
        ));
    }
    if exit_requested {
        return Some(format!(
            "planner {id} is asked to exit: a follow-up request goes only to a planner that goes on (record a new request with `request add`)"
        ));
    }
    if let Some(failure) = wall.filter(|_| state == PlannerState::Idle) {
        return Some(format!(
            "planner {id} waits for Claude ({}): hand it the request once its turn goes on",
            failure.as_str()
        ));
    }
    None
}

/// The name of the next follow-up request among the files `names` of a
/// planner's `requests/`: one past the highest `followup-N.md`.
pub fn next_follow_up_name<'a>(names: impl IntoIterator<Item = &'a str>) -> String {
    let last = names
        .into_iter()
        .filter_map(|name| {
            name.strip_prefix(FOLLOW_UP_PREFIX)?
                .strip_suffix(".md")?
                .parse::<u64>()
                .ok()
        })
        .max()
        .unwrap_or(0);
    format!("{FOLLOW_UP_PREFIX}{}", last + 1)
}

/// The names of the files under `dir`'s `requests/` (none when it does not
/// exist).
fn request_names(probes: &PlannerProbes<'_>, dir: &Path) -> Result<Vec<String>> {
    match probes.files.read_dir(&dir.join(PLANNER_REQUESTS_DIR)) {
        Ok(paths) => Ok(paths
            .iter()
            .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
            .collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

/// `planner request ID`: hand `words` to planner `id` as its next turn.
/// Returns what was handed: the file, the sentence, the request's number
/// and the planner's state when it was handed. Recorded as
/// `turn_requested` (as the supervisor's requests) and
/// `planner_request_handed`, both with the caller as the actor.
pub fn request_planner(
    queue: &mut dyn Queue,
    probes: &PlannerProbes<'_>,
    id: PlannerId,
    words: &str,
) -> Result<Value> {
    let planner = queue.planner(id)?;
    // Judged before anything is probed: an interactive planner's terminal
    // is not even looked at.
    if let Some(reason) = refused_by_row(&planner) {
        bail!(reason);
    }
    let view: PlannerView = planner_view(probes, planner)?;
    let exit_requested = probes.files.is_file(&exit_path(&view.dir));
    let wall = provider_failure(
        probes
            .files
            .read(&planner_idle_marker(&view.dir))
            .ok()
            .and_then(|bytes| TurnMark::parse(&bytes)),
    );
    if let Some(reason) = refused_by_state(id, view.state, exit_requested, wall) {
        bail!(reason);
    }
    probes.files.create_dir_all(&view.dir)?;
    let _guard = lock_waiting(probes.files, &view.dir.join(FOLLOW_UP_LOCK))?;
    let names = request_names(probes, &view.dir)?;
    let name = next_follow_up_name(names.iter().map(String::as_str));
    let handed = hand_request_to_planner(probes.files, &view.dir, &name, words)?;
    // Not stamped as input: the planner is not idle while the request
    // waits in its `turns/` (unless its last turn met Claude's wall, when
    // the wrapper takes nothing before the `provider retry` and the planner
    // stays idle at the wall, task 1596), and its wrapper stamps the input
    // as it takes it.
    let what = format!("follow-up request {name}");
    let seq = write_request(probes.files, &view.dir, &handed.sentence, &what)?;
    // The supervisor may have asked it to exit meanwhile: a request still
    // waiting would never be taken, so it is taken back and refused.
    if probes.files.is_file(&exit_path(&view.dir))
        && probes
            .files
            .remove_file(&request_path(&view.dir, seq))
            .is_ok()
    {
        bail!(
            "planner {id} was asked to exit as the request was handed: record a new request with `request add`"
        );
    }
    let file = handed.path.display().to_string();
    queue.record_queue_event(
        EventKind::TurnRequested,
        json!({"planner_id": id, "seq": seq, "what": what, "workspace_id": view.planner.workspace_id}),
    )?;
    queue.record_queue_event(
        EventKind::PlannerRequestHanded,
        json!({"planner_id": id, "seq": seq, "name": name, "file": file}),
    )?;
    Ok(json!({
        "planner_id": id,
        "state": view.state,
        "name": name,
        "file": file,
        "sentence": handed.sentence,
        "seq": seq,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::RequestId;

    fn planner(origin: PlannerOrigin, route: PlannerRoute) -> PlannerSession {
        PlannerSession {
            id: PlannerId::new(4),
            origin,
            proposal_id: None,
            draft_task_id: None,
            finding_id: None,
            request_id: Some(RequestId::new(2)),
            workspace_id: Some("background:1:x".to_owned()),
            wrapper_pid: Some(1),
            agent_pid: Some(2),
            heartbeat_at: Some(0),
            exit_code: None,
            exited_at: None,
            closed_at: None,
            error: None,
            created_at: 0,
            route,
            answer_wait_at: None,
        }
    }

    #[test]
    fn only_a_headless_planner_of_the_runtimes_not_closed_takes_a_request() {
        assert_eq!(
            refused_by_row(&planner(PlannerOrigin::Runtime, PlannerRoute::Headless)),
            None
        );
        let interactive =
            refused_by_row(&planner(PlannerOrigin::Runtime, PlannerRoute::Interactive)).unwrap();
        assert!(interactive.contains("is interactive"), "{interactive}");
        assert!(interactive.contains("nothing is typed into a terminal"));
        let person =
            refused_by_row(&planner(PlannerOrigin::Person, PlannerRoute::Interactive)).unwrap();
        assert!(person.contains("opened by a person"), "{person}");
        let mut closed = planner(PlannerOrigin::Runtime, PlannerRoute::Headless);
        closed.closed_at = Some(5);
        assert_eq!(refused_by_row(&closed).unwrap(), "planner 4 is closed");
    }

    #[test]
    fn a_live_planner_takes_a_request_at_work_too_but_not_one_asked_to_exit() {
        let id = PlannerId::new(4);
        for state in [
            PlannerState::Opening,
            PlannerState::Working,
            PlannerState::Idle,
        ] {
            assert_eq!(refused_by_state(id, state, false, None), None, "{state:?}");
            assert!(
                refused_by_state(id, state, true, None)
                    .unwrap()
                    .contains("is asked to exit")
            );
        }
        for (state, name) in [
            (PlannerState::Closed, "closed"),
            (PlannerState::Lost, "lost"),
            (PlannerState::Exited, "exited"),
        ] {
            let reason = refused_by_state(id, state, false, None).unwrap();
            assert!(reason.contains(&format!("planner 4 is {name}")), "{reason}");
        }
    }

    #[test]
    fn an_idle_planner_that_waits_for_claude_takes_no_request() {
        let id = PlannerId::new(4);
        for failure in [
            TurnFailure::Authentication,
            TurnFailure::UsageLimit,
            TurnFailure::Launch,
        ] {
            let reason = refused_by_state(id, PlannerState::Idle, false, Some(failure)).unwrap();
            assert!(reason.contains("waits for Claude"), "{reason}");
            // At work, the wall is of a turn before: the request waits.
            assert_eq!(
                refused_by_state(id, PlannerState::Working, false, Some(failure)),
                None
            );
        }
    }

    #[test]
    fn a_follow_up_is_named_one_past_the_highest() {
        assert_eq!(next_follow_up_name([]), "followup-1");
        assert_eq!(
            next_follow_up_name([
                "request-3.md",
                "followup-2.md",
                "followup-10.md",
                "followup-x.md"
            ]),
            "followup-11"
        );
        assert_eq!(next_follow_up_name(["followup-1"]), "followup-1");
    }
}
