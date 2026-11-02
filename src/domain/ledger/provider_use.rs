//! What a run's row folds of the providers it could not use
//! (ADR-t4005-1 decisions 6 and 8): its worker's waits for a provider and
//! the jobs of its review that started on another provider than the one
//! planned for them. A breakdown beside the intervals: the intervals, their
//! blockers and holds stay as they are folded.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::resume::{self, RESUME_WAIT_PHASE};
use crate::domain::sessions::REVIEW;
use crate::domain::stats::asks::UNKNOWN;
use crate::domain::stats::{starts_a_turn, timestamp_millis};
use crate::domain::{EventKind, RunEvent};

/// The key of a run row's detail that holds its [`ProviderUse`], for a
/// run folded into intervals and a legacy one alike. A row without it
/// (one an older fold wrote) has no record of it, which
/// [`super::read::read`] tells from none used.
pub const DETAIL_KEY: &str = "provider_use";

/// A run's use of providers it could not use. Each part is none when the
/// run's events hold no record it could be read from (a run from before
/// it was recorded), and some, empty when nothing was used, otherwise.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUse {
    /// The waits of the run's worker for its provider, by the `reason` of
    /// each `provider_waiting` (a `SwitchReason`: `usage_limit`,
    /// `authentication`, ...; `unknown` when it names none). Each
    /// `provider_waiting` of the run is one wait. Recorded for a run with
    /// a `provider_waiting`, or with a headless turn (`turn_started`) and a
    /// claim that names its `requested_provider`: the claim's field came
    /// with the record of the waits, which a headless turn records, so a
    /// run claimed before (its turns recorded or not) or that recorded no
    /// turn has no record of its waits.
    pub worker_waits: Option<BTreeMap<String, Waits>>,
    /// The jobs of the run's review that started on another provider than
    /// the one planned for them, by their launch's `switch_reason`
    /// (`unknown` when it names none). A job's planned provider is the one
    /// its group and settings give it before a fallback, which its launch
    /// names in `switched_from` when it moved off it, and is the provider it
    /// started on when its launch names none. So a job that started where
    /// the settings or the run's group put it is no switch, though the
    /// review's jobs ran on different providers.
    ///
    /// A job is an `agent` of a review `attempt` (null for the whole
    /// review): its starts again and its moves are the one job. A review
    /// attempt with a `review_job_started` has its jobs there (the whole
    /// review's and each subagent's); one with none (a review from before
    /// its jobs were started apart, or whose jobs did not start) is the one
    /// job of the whole review, whose launch its `review_started` records,
    /// or else the review's `session_opened`, which copies it. Recorded for
    /// a run with a start of a review job that names its launch.
    pub review_switches: Option<BTreeMap<String, i64>>,
}

/// The waits of one reason.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Waits {
    /// How many.
    pub count: i64,
    /// Their time in milliseconds: each from its `provider_waiting` to the
    /// next start of a turn of the worker (a turn, a new session, a resume,
    /// a switch of provider) or the next wait, whichever comes first (for a
    /// wait before a resume, also a skipped resume or a park, which end it
    /// as [`resume::resume_wait_recorded`] reads it), and one none ends up
    /// to the row's end (`until`).
    pub millis: i64,
    /// Of them, those nothing ended before the row's end: cut there.
    pub open: i64,
}

impl ProviderUse {
    /// Fold the run's `events` (ascending id), a wait still open ending at
    /// `until` (unix milliseconds: the run's end, or the end of its last
    /// interval while it goes on; a legacy run's last event).
    pub fn of(events: &[RunEvent], until: i64) -> Self {
        let mut uses = Self::default();
        let claimed_with_request = events.iter().any(|event| {
            event.kind == EventKind::RunClaimed && event.payload.get("requested_provider").is_some()
        });
        let turned = events
            .iter()
            .any(|event| event.kind == EventKind::TurnStarted);
        if claimed_with_request && turned {
            uses.worker_waits = Some(BTreeMap::new());
        }
        for (index, event) in events.iter().enumerate() {
            if event.kind == EventKind::ProviderWaiting {
                uses.wait(event, &events[index + 1..], until);
            }
        }
        uses.review_switches = review_switches(events);
        uses
    }

    /// What a row's `detail` holds of it; none for a row an older fold
    /// wrote.
    pub fn of_detail(detail: &Value) -> Option<Self> {
        detail
            .get(DETAIL_KEY)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
    }

    fn wait(&mut self, waiting: &RunEvent, after: &[RunEvent], until: i64) {
        let waits = self
            .worker_waits
            .get_or_insert_default()
            .entry(reason(&waiting.payload, "reason"))
            .or_default();
        waits.count += 1;
        let Some(start) = timestamp_millis(&waiting.created_at) else {
            return;
        };
        let before_resume = waiting.payload["phase"] == RESUME_WAIT_PHASE;
        let ended = after
            .iter()
            .find(|event| {
                starts_a_turn(event)
                    || event.kind == EventKind::ProviderWaiting
                    || (before_resume
                        && (event.kind == EventKind::ResumeSkipped || resume::parks(event)))
            })
            .and_then(|event| timestamp_millis(&event.created_at));
        let end = match ended {
            Some(end) if end <= until => end,
            _ => {
                waits.open += 1;
                until
            }
        };
        waits.millis += (end - start).max(0);
    }
}

/// The switches of the run's review jobs by reason ([`ProviderUse`]):
/// each job's starts that name a launch, the first of them that switched
/// counted, by its reason.
fn review_switches(events: &[RunEvent]) -> Option<BTreeMap<String, i64>> {
    let attempt = |event: &RunEvent| event.payload["attempt"].to_string();
    let split: BTreeSet<String> = events
        .iter()
        .filter(|event| event.kind == EventKind::ReviewJobStarted)
        .map(attempt)
        .collect();
    // A start of a job and its launch: a job's own start, or, for an
    // attempt with none, the whole review's start or its span.
    let starts = events.iter().filter_map(|event| {
        let job = match event.kind.known() {
            Some(EventKind::ReviewJobStarted) => {
                (attempt(event), event.payload["agent"].to_string())
            }
            Some(EventKind::ReviewStarted) if !split.contains(&attempt(event)) => {
                (attempt(event), Value::Null.to_string())
            }
            Some(EventKind::SessionOpened)
                if event.payload["kind"] == REVIEW && !split.contains(&attempt(event)) =>
            {
                (attempt(event), Value::Null.to_string())
            }
            _ => return None,
        };
        Some((job, launch_of(event)?))
    });
    let mut switches: Option<BTreeMap<String, i64>> = None;
    let mut switched = BTreeSet::new();
    for (job, launch) in starts {
        let counted = switches.get_or_insert_default();
        if let Some(reason) = switch_of(launch)
            && switched.insert(job)
        {
            *counted.entry(reason).or_default() += 1;
        }
    }
    switches
}

/// The launch `event` (a start of a review job) records, if it names one.
fn launch_of(event: &RunEvent) -> Option<&Value> {
    event
        .payload
        .get("launch")
        .filter(|launch| launch.is_object())
}

/// The reason a job started with `launch` switched for: some when it
/// names a `switched_from` other than the provider it started on (Claude
/// when it names none, as [`crate::domain::actor_model::ActorLaunch`]
/// reads it).
fn switch_of(launch: &Value) -> Option<String> {
    let from = launch.get("switched_from")?.as_str()?;
    let provider = launch
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("claude");
    (from != provider).then(|| reason(launch, "switch_reason"))
}

fn reason(payload: &Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or(UNKNOWN)
        .to_owned()
}
