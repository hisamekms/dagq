//! The watch of how old a declared workflow's latest success is: whether
//! its latest successful run on the watched branch is too old, so the
//! inbox is told and the workflow is started once by `workflow_dispatch`,
//! and whether a newer success ends that. The age is measured from the
//! creation time of the latest run that concluded `success`: whatever the
//! run makes is made inside it, so is newer, and the runtime reads no
//! format of what it makes (ADR-t2268-1). Runs that end otherwise
//! (cancelled, failure, timed out, or never started on a runner that went
//! away) leave the age to grow, so they are caught by it. The records are
//! [`CI_WORKFLOW_STALE`] and [`CI_WORKFLOW_FRESH`], apart from the push
//! watch's and the scheduled workflows' records.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::CiRun;
use crate::domain::RunEvent;
use crate::domain::event_kind::EventKind;
use crate::domain::stats::rfc3339_millis;

/// A declared workflow's latest success was found too old (or there was
/// none) and the workflow is started once for it: one per workflow and
/// latest success (`run_id`, null when there was none), which the queue
/// keeps unique.
pub const CI_WORKFLOW_STALE: &str = EventKind::CiWorkflowStale.as_str();
/// A success newer than the one a [`CI_WORKFLOW_STALE`] recorded was read:
/// that record is closed.
pub const CI_WORKFLOW_FRESH: &str = EventKind::CiWorkflowFresh.as_str();
/// The two kinds of the records, for reading them from the queue.
pub const FRESHNESS_KINDS: [EventKind; 2] =
    [EventKind::CiWorkflowStale, EventKind::CiWorkflowFresh];

/// One workflow whose latest success is watched for its age.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessWatch {
    /// The workflow file name or name (`gh run list --workflow`), started
    /// with `gh workflow run` on the watched branch.
    pub workflow: String,
    /// Seconds after its latest success's creation that the workflow's
    /// output may no longer be used.
    pub max_age_secs: u64,
    /// Seconds before [`Self::max_age_secs`] that the inbox is told and the
    /// workflow started, so a new run can finish before the limit.
    pub lead_secs: u64,
}

impl FreshnessWatch {
    /// The age in seconds from which the latest success counts as stale:
    /// the limit less the lead (0 when the lead is longer).
    pub const fn alert_after_secs(&self) -> u64 {
        self.max_age_secs.saturating_sub(self.lead_secs)
    }
}

/// The latest record of one workflow that is still open: a
/// [`CI_WORKFLOW_STALE`] no [`CI_WORKFLOW_FRESH`] closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenStale {
    /// The record's event ID.
    pub event: i64,
    /// The run ID of the latest success it recorded; `None` when there
    /// was none.
    pub run_id: Option<i64>,
    /// That run's creation time (RFC 3339); `None` with no run.
    pub created_at: Option<String>,
}

/// One workflow as its freshness records leave it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FreshnessState {
    /// The open record, if any: the attention stands while it is open.
    pub open: Option<OpenStale>,
    /// The event ID of the workflow's latest record of either kind: a
    /// record decided after it is written only while it is still the
    /// latest, so a supervisor that read the records before another wrote
    /// does not put back what it read.
    pub last: Option<i64>,
}

impl FreshnessState {
    /// Fold `events` (oldest first) for `workflow`; the records of the
    /// other workflows and the other kinds are skipped.
    pub fn fold(events: &[RunEvent], workflow: &str) -> Self {
        let mut state = Self::default();
        for event in events {
            if event.payload["workflow"].as_str() != Some(workflow) {
                continue;
            }
            if FRESHNESS_KINDS.iter().any(|kind| event.kind == *kind) {
                state.last = Some(event.id.as_i64());
            }
            if event.kind == EventKind::CiWorkflowStale {
                state.open = Some(OpenStale {
                    event: event.id.as_i64(),
                    run_id: event.payload["run_id"].as_i64(),
                    created_at: event.payload["created_at"].as_str().map(str::to_owned),
                });
            } else if event.kind == EventKind::CiWorkflowFresh {
                state.open = None;
            }
        }
        state
    }
}

/// The [`CI_WORKFLOW_STALE`] records still open in `events` (oldest
/// first), one per workflow, oldest first: what the inbox is told stands.
/// With `declared` (`[[ci_watch.freshness]]` as read), only the declared
/// workflows' count, so taking a declaration out ends its attention; with
/// `None` (the file cannot be read) every open one does.
pub fn standing_stale(events: &[RunEvent], declared: Option<&[FreshnessWatch]>) -> Vec<RunEvent> {
    let mut workflows: Vec<&str> = Vec::new();
    for event in events {
        if let Some(workflow) = event.payload["workflow"].as_str()
            && !workflows.contains(&workflow)
            && declared
                .is_none_or(|declared| declared.iter().any(|watch| watch.workflow == workflow))
        {
            workflows.push(workflow);
        }
    }
    let mut open: Vec<RunEvent> = workflows
        .into_iter()
        .filter_map(|workflow| {
            let id = FreshnessState::fold(events, workflow).open?.event;
            events.iter().find(|event| event.id.as_i64() == id).cloned()
        })
        .collect();
    open.sort_by_key(|event| event.id.as_i64());
    open
}

/// What the inbox reads of an open [`CI_WORKFLOW_STALE`] `payload`: the
/// workflow and branch, its latest success (or none) and how old it was
/// against the limit when it was found.
pub fn stale_summary(payload: &Value) -> String {
    let hours = |key: &str| payload[key].as_u64().map(|secs| secs / 3600);
    let workflow = payload["workflow"].as_str().unwrap_or("?");
    let branch = payload["branch"].as_str().unwrap_or("?");
    let latest = match (payload["run_id"].as_i64(), payload["created_at"].as_str()) {
        (Some(run), Some(at)) => format!("latest success run {run} created {at}"),
        (Some(run), None) => format!("latest success run {run}"),
        _ => "no success".to_owned(),
    };
    let age = hours("age_secs").map_or_else(|| "age unknown".to_owned(), |h| format!("{h}h old"));
    let limit = hours("max_age_secs").map_or_else(String::new, |h| format!(", limit {h}h"));
    format!("{workflow} on {branch}: {latest}, {age}{limit}; workflow_dispatch tried once")
}

/// What one look at a workflow's latest success decides.
#[derive(Debug, Clone, PartialEq)]
pub enum FreshnessDecision {
    /// Fresh enough, and nothing open: nothing to do.
    Nothing,
    /// Too old (or no success at all), and nothing recorded for it: record
    /// the [`CI_WORKFLOW_STALE`] payload, tell the inbox and start the
    /// workflow once by `workflow_dispatch`.
    Alert(Value),
    /// The open record is about the same latest success: the attention
    /// stays and the workflow is not started again.
    Hold,
    /// A success newer than the open record's was read: record the
    /// [`CI_WORKFLOW_FRESH`] payload, which closes it.
    Close(Value),
}

/// What `watch` decides for `latest`, the latest successful run of the
/// workflow on `branch` (`None` when it has none), at `now_millis` (Unix
/// milliseconds), against `state`: with an open record, a success created
/// after the one it recorded (any success when it recorded none) closes
/// it, and otherwise it holds; with none open, a success at least
/// [`FreshnessWatch::alert_after_secs`] old, one whose creation time does
/// not read (taken as old, the safe side), and no success at all alert.
/// Only the latest success is read: how the runs after it ended does not
/// matter.
pub fn decide_freshness(
    watch: &FreshnessWatch,
    branch: &str,
    latest: Option<&CiRun>,
    now_millis: i64,
    state: &FreshnessState,
) -> FreshnessDecision {
    if let Some(open) = &state.open {
        return match latest {
            Some(run) if is_newer(run, open) => FreshnessDecision::Close(json!({
                "workflow": watch.workflow,
                "branch": branch,
                "closes": open.event,
                "stale_run_id": open.run_id,
                "run_id": run.run_id,
                "created_at": run.created_at,
                "url": run.url,
            })),
            _ => FreshnessDecision::Hold,
        };
    }
    let age_secs = latest.and_then(|run| {
        rfc3339_millis(&run.created_at).map(|at| now_millis.saturating_sub(at).max(0) / 1000)
    });
    let threshold = watch.alert_after_secs();
    if age_secs.is_some_and(|age| u64::try_from(age).unwrap_or(u64::MAX) < threshold) {
        return FreshnessDecision::Nothing;
    }
    FreshnessDecision::Alert(json!({
        "workflow": watch.workflow,
        "branch": branch,
        "run_id": latest.map(|run| run.run_id),
        "created_at": latest.map(|run| run.created_at.as_str()),
        "url": latest.map(|run| run.url.as_str()),
        "age_secs": age_secs,
        "max_age_secs": watch.max_age_secs,
        "lead_secs": watch.lead_secs,
    }))
}

/// Whether `run` is a success after the one `open` recorded: any success
/// when it recorded none; otherwise another run created no earlier.
fn is_newer(run: &CiRun, open: &OpenStale) -> bool {
    match (open.run_id, open.created_at.as_deref()) {
        (None, _) => true,
        (Some(id), _) if id == run.run_id => false,
        (Some(_), Some(at)) => match (rfc3339_millis(&run.created_at), rfc3339_millis(at)) {
            (Some(new), Some(old)) => new >= old,
            _ => run.created_at.as_str() >= at,
        },
        (Some(_), None) => true,
    }
}

/// What the store writes for one decision, in one transaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum FreshnessRecord {
    /// A [`CI_WORKFLOW_STALE`] payload: written only while the
    /// workflow's latest record is still `previous` (its
    /// [`FreshnessState::last`] when decided), so a late write does not
    /// open an older success over a newer record, and unless one of the
    /// same `workflow` and `run_id` is there, so the workflow is started
    /// once per latest success across supervisors and restarts.
    Stale {
        previous: Option<i64>,
        payload: Value,
    },
    /// A [`CI_WORKFLOW_FRESH`] payload: written only while the
    /// workflow's latest record is the open one it `closes`.
    Fresh { payload: Value },
}

impl FreshnessRecord {
    /// The record of `decision`, decided against `state`, or `None` for
    /// one that writes nothing.
    pub fn of(decision: &FreshnessDecision, state: &FreshnessState) -> Option<Self> {
        match decision {
            FreshnessDecision::Alert(payload) => Some(Self::Stale {
                previous: state.last,
                payload: payload.clone(),
            }),
            FreshnessDecision::Close(payload) => Some(Self::Fresh {
                payload: payload.clone(),
            }),
            FreshnessDecision::Nothing | FreshnessDecision::Hold => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;

    const HOUR: i64 = 3600 * 1000;

    fn watch() -> FreshnessWatch {
        FreshnessWatch {
            workflow: "table.yml".into(),
            max_age_secs: 48 * 3600,
            lead_secs: 6 * 3600,
        }
    }

    /// 2026-10-30T00:00:00Z in Unix milliseconds.
    fn base() -> i64 {
        rfc3339_millis("2026-10-30T00:00:00Z").unwrap()
    }

    fn success(id: i64, created_at: &str) -> CiRun {
        CiRun {
            run_id: id,
            run_number: id,
            sha: format!("sha{id}"),
            conclusion: "success".into(),
            url: format!("https://github.com/o/n/actions/runs/{id}"),
            created_at: created_at.into(),
            attempt: 1,
        }
    }

    fn event(id: i64, kind: EventKind, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.as_str().into(),
            payload,
            created_at: "2026-11-01T00:00:00Z".into(),
            actor: None,
        }
    }

    fn recorded(id: i64, decision: &FreshnessDecision) -> RunEvent {
        match FreshnessRecord::of(decision, &FreshnessState::default()).unwrap() {
            FreshnessRecord::Stale { payload, .. } => {
                event(id, EventKind::CiWorkflowStale, payload)
            }
            FreshnessRecord::Fresh { payload } => event(id, EventKind::CiWorkflowFresh, payload),
        }
    }

    /// A success younger than the limit less the lead does nothing; one
    /// at it alerts with its run, age and limit.
    #[test]
    fn a_success_younger_than_the_limit_less_the_lead_does_nothing() {
        let latest = success(7, "2026-10-30T00:00:00Z");
        let empty = FreshnessState::default();
        assert_eq!(
            decide_freshness(&watch(), "main", Some(&latest), base() + 41 * HOUR, &empty),
            FreshnessDecision::Nothing
        );
        let FreshnessDecision::Alert(payload) =
            decide_freshness(&watch(), "main", Some(&latest), base() + 42 * HOUR, &empty)
        else {
            panic!("a success 42 hours old alerts at a limit of 48 with a lead of 6");
        };
        assert_eq!(payload["workflow"], "table.yml");
        assert_eq!(payload["branch"], "main");
        assert_eq!(payload["run_id"], 7);
        assert_eq!(payload["age_secs"], 42 * 3600);
        assert_eq!(payload["max_age_secs"], 48 * 3600);
    }

    /// Runs that keep ending cancelled leave the latest success where it
    /// was: only its age decides, so the workflow is caught as stale.
    #[test]
    fn runs_that_only_end_cancelled_are_caught_by_the_age_of_the_last_success() {
        let latest = success(3, "2026-10-30T00:13:49Z");
        let decision = decide_freshness(
            &watch(),
            "main",
            Some(&latest),
            base() + 47 * HOUR,
            &FreshnessState::default(),
        );
        assert!(matches!(&decision, FreshnessDecision::Alert(p) if p["run_id"] == 3));
    }

    /// A workflow with no success at all alerts, with no run.
    #[test]
    fn no_success_at_all_alerts() {
        let FreshnessDecision::Alert(payload) =
            decide_freshness(&watch(), "main", None, base(), &FreshnessState::default())
        else {
            panic!("no success alerts");
        };
        assert_eq!(payload["run_id"], Value::Null);
        assert_eq!(payload["age_secs"], Value::Null);
        // Recorded with no run, the next look with still none holds, and
        // the first success closes it.
        let state = FreshnessState::fold(
            &[recorded(1, &FreshnessDecision::Alert(payload))],
            "table.yml",
        );
        assert_eq!(state.open.as_ref().map(|o| o.run_id), Some(None));
        assert_eq!(
            decide_freshness(&watch(), "main", None, base() + HOUR, &state),
            FreshnessDecision::Hold
        );
        let first = success(1, "2026-10-30T02:00:00Z");
        assert!(matches!(
            decide_freshness(&watch(), "main", Some(&first), base() + 3 * HOUR, &state),
            FreshnessDecision::Close(_)
        ));
    }

    /// The second look at the same stale success holds: the attention
    /// stays and the workflow is not started again, however old it gets.
    #[test]
    fn the_second_look_at_the_same_success_does_not_start_it_again() {
        let latest = success(5, "2026-10-30T00:00:00Z");
        let alert = decide_freshness(
            &watch(),
            "main",
            Some(&latest),
            base() + 43 * HOUR,
            &FreshnessState::default(),
        );
        let state = FreshnessState::fold(&[recorded(4, &alert)], "table.yml");
        assert_eq!(
            state.open,
            Some(OpenStale {
                event: 4,
                run_id: Some(5),
                created_at: Some("2026-10-30T00:00:00Z".into()),
            })
        );
        for hours in [44, 49, 72] {
            assert_eq!(
                decide_freshness(
                    &watch(),
                    "main",
                    Some(&latest),
                    base() + hours * HOUR,
                    &state
                ),
                FreshnessDecision::Hold
            );
        }
        // Another workflow's record is not this one's.
        assert_eq!(
            FreshnessState::fold(&[recorded(4, &alert)], "other.yml"),
            FreshnessState::default()
        );
    }

    /// A success newer than the recorded one closes the record, naming
    /// it; once closed, a fresh success does nothing and an old one
    /// alerts anew for itself.
    #[test]
    fn a_newer_success_closes_the_record() {
        let old = success(5, "2026-10-30T00:00:00Z");
        let alert = decide_freshness(
            &watch(),
            "main",
            Some(&old),
            base() + 43 * HOUR,
            &FreshnessState::default(),
        );
        let state = FreshnessState::fold(&[recorded(4, &alert)], "table.yml");
        let new = success(9, "2026-10-31T19:30:00Z");
        let FreshnessDecision::Close(payload) =
            decide_freshness(&watch(), "main", Some(&new), base() + 44 * HOUR, &state)
        else {
            panic!("a newer success closes");
        };
        assert_eq!(payload["closes"], 4);
        assert_eq!(payload["stale_run_id"], 5);
        assert_eq!(payload["run_id"], 9);
        let closed = FreshnessState::fold(
            &[
                recorded(4, &alert),
                recorded(6, &FreshnessDecision::Close(payload)),
            ],
            "table.yml",
        );
        assert_eq!((closed.open.clone(), closed.last), (None, Some(6)));
        assert_eq!(
            decide_freshness(&watch(), "main", Some(&new), base() + 45 * HOUR, &closed),
            FreshnessDecision::Nothing
        );
        assert!(matches!(
            decide_freshness(&watch(), "main", Some(&new), base() + 90 * HOUR, &closed),
            FreshnessDecision::Alert(p) if p["run_id"] == 9
        ));
    }

    /// A success created before the recorded one (read from a list that
    /// lags) does not close it.
    #[test]
    fn an_older_success_does_not_close_the_record() {
        let open = FreshnessState {
            open: Some(OpenStale {
                event: 2,
                run_id: Some(5),
                created_at: Some("2026-10-30T00:00:00Z".into()),
            }),
            last: Some(2),
        };
        let older = success(4, "2026-10-29T00:00:00Z");
        assert_eq!(
            decide_freshness(&watch(), "main", Some(&older), base() + 50 * HOUR, &open),
            FreshnessDecision::Hold
        );
    }

    /// A creation time that does not read counts as old (the safe side);
    /// a lead longer than the limit alerts at once.
    #[test]
    fn an_unreadable_time_and_a_long_lead_alert() {
        let odd = success(1, "yesterday");
        assert!(matches!(
            decide_freshness(&watch(), "main", Some(&odd), base(), &FreshnessState::default()),
            FreshnessDecision::Alert(p) if p["age_secs"].is_null()
        ));
        let eager = FreshnessWatch {
            lead_secs: 100 * 3600,
            ..watch()
        };
        assert_eq!(eager.alert_after_secs(), 0);
        let now = success(2, "2026-10-30T00:00:00Z");
        assert!(matches!(
            decide_freshness(
                &eager,
                "main",
                Some(&now),
                base(),
                &FreshnessState::default()
            ),
            FreshnessDecision::Alert(_)
        ));
    }

    /// An alert's record carries the workflow's latest record it was
    /// decided after, and a close's names the open record.
    #[test]
    fn a_record_names_the_latest_record_it_was_decided_after() {
        let latest = success(5, "2026-10-30T00:00:00Z");
        let empty = FreshnessState::default();
        let alert = decide_freshness(&watch(), "main", Some(&latest), base() + 43 * HOUR, &empty);
        assert!(matches!(
            FreshnessRecord::of(&alert, &empty),
            Some(FreshnessRecord::Stale { previous: None, .. })
        ));
        let other = event(
            3,
            EventKind::CiWorkflowStale,
            json!({"workflow": "other.yml", "run_id": 1}),
        );
        let state = FreshnessState::fold(&[other, recorded(4, &alert)], "table.yml");
        assert_eq!(state.last, Some(4));
        let closed = FreshnessState::fold(
            &[
                recorded(4, &alert),
                event(
                    7,
                    EventKind::CiWorkflowFresh,
                    json!({"workflow": "table.yml", "closes": 4}),
                ),
            ],
            "table.yml",
        );
        let again = decide_freshness(&watch(), "main", Some(&latest), base() + 90 * HOUR, &closed);
        assert!(matches!(
            FreshnessRecord::of(&again, &closed),
            Some(FreshnessRecord::Stale {
                previous: Some(7),
                ..
            })
        ));
    }

    /// Nothing and Hold write nothing; the record goes through the
    /// queue service's JSON unchanged.
    #[test]
    fn only_alert_and_close_write_a_record() {
        let state = FreshnessState::default();
        assert_eq!(
            FreshnessRecord::of(&FreshnessDecision::Nothing, &state),
            None
        );
        assert_eq!(FreshnessRecord::of(&FreshnessDecision::Hold, &state), None);
        let record = FreshnessRecord::Stale {
            previous: Some(3),
            payload: json!({"workflow": "table.yml", "run_id": 1}),
        };
        let wire = serde_json::to_value(&record).unwrap();
        assert_eq!(wire["record"], "stale");
        assert_eq!(
            serde_json::from_value::<FreshnessRecord>(wire).unwrap(),
            record
        );
    }

    /// The open stale record of each workflow stands, a closed one does
    /// not, and with the declarations read only a declared workflow's
    /// stands; its summary names the workflow, its success, age and limit.
    #[test]
    fn the_open_stale_records_of_the_declared_workflows_stand() {
        let stale = |id, workflow: &str, run: Option<i64>| {
            event(
                id,
                EventKind::CiWorkflowStale,
                json!({"workflow": workflow, "branch": "main", "run_id": run,
                       "created_at": run.map(|_| "2026-10-30T00:13:49Z"),
                       "age_secs": 151_200, "max_age_secs": 172_800}),
            )
        };
        let events = vec![
            stale(1, "table.yml", Some(9)),
            stale(2, "gone.yml", None),
            stale(3, "closed.yml", Some(4)),
            event(
                4,
                EventKind::CiWorkflowFresh,
                json!({"workflow": "closed.yml", "closes": 3}),
            ),
        ];
        let ids = |open: Vec<RunEvent>| open.iter().map(|e| e.id.as_i64()).collect::<Vec<_>>();
        assert_eq!(ids(standing_stale(&events, None)), vec![1, 2]);
        assert_eq!(ids(standing_stale(&events, Some(&[watch()]))), vec![1]);
        assert_eq!(ids(standing_stale(&events, Some(&[]))), Vec::<i64>::new());
        assert_eq!(
            stale_summary(&events[0].payload),
            "table.yml on main: latest success run 9 created 2026-10-30T00:13:49Z, 42h old, limit 48h; workflow_dispatch tried once"
        );
        assert_eq!(
            stale_summary(
                &json!({"workflow": "gone.yml", "branch": "main", "run_id": null, "age_secs": null})
            ),
            "gone.yml on main: no success, age unknown; workflow_dispatch tried once"
        );
    }
}
