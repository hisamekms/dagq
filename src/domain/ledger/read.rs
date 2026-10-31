//! The one read of a ledger row (ADR-t1662-1 decisions 12 and 18): the
//! values under today's names (`work`, `validate`, `wait_to_land`,
//! `land_phases`, `startup`) and the new breakdown, from a new row's
//! intervals or a legacy row's frozen values, in the same names and
//! shapes, with the legacy mark. Readers of the ledger go through the
//! `LedgerStore` and this function only.

use serde::Serialize;
use serde_json::{Map, Value};

use super::fold::{Interval, Milestones};
use super::{LedgerKind, LedgerRow, Totals};
use crate::domain::after_claim::AfterClaim;
use crate::domain::stats::landing::{LandPhases, PHASES};

/// What a row reads as. Times under today's names are whole seconds, as
/// `stats` gives them; the breakdown is in milliseconds, so that its parts
/// add up to its whole.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LedgerValues {
    pub kind: LedgerKind,
    pub key: String,
    /// A legacy row: today's names come from its frozen values and it has
    /// no breakdown.
    pub legacy: bool,
    /// The run landed (`run_integrated`): a legacy row whose `stats`
    /// status was `integrated`, a new one with that milestone. False for
    /// a task.
    pub landed: bool,
    pub work: Option<i64>,
    pub validate: Option<i64>,
    pub wait_to_land: Option<i64>,
    pub land_phases: Option<LandPhases>,
    pub startup: Option<i64>,
    /// The time and the slot time, per blocker, holds and attempt, and the
    /// unattributed time; none for a legacy row.
    pub breakdown: Option<Totals>,
    /// The claim's attributes as recorded, whatever their keys, and the
    /// facts after the claim (a run's).
    pub attributes: Map<String, Value>,
    pub after_claim: Option<AfterClaim>,
}

/// Read `row`.
pub fn read(row: &LedgerRow) -> LedgerValues {
    let mut values = LedgerValues {
        kind: row.kind,
        key: row.key.clone(),
        legacy: row.is_legacy(),
        landed: false,
        work: None,
        validate: None,
        wait_to_land: None,
        land_phases: None,
        startup: None,
        breakdown: None,
        attributes: row.attributes.clone(),
        after_claim: row.after_claim.clone(),
    };
    if let Some(legacy) = &row.legacy {
        values.landed = row.kind == LedgerKind::Run && row.outcome.as_deref() == Some("integrated");
        let secs = |key: &str| legacy.get(key).and_then(Value::as_i64);
        values.work = secs("work");
        values.validate = secs("validate");
        values.wait_to_land = secs("wait_to_land");
        values.startup = secs("startup");
        values.land_phases = legacy
            .get("land_phases")
            .filter(|phases| !phases.is_null())
            .and_then(|phases| serde_json::from_value(phases.clone()).ok());
        return values;
    }
    values.breakdown = Some(row.totals.clone());
    if row.kind == LedgerKind::Run {
        let milestones: Milestones = row
            .detail
            .get("milestones")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default();
        values.landed = milestones.integrated.is_some();
        run_names(&mut values, row, &milestones);
    }
    values
}

/// Today's names of a new run row, from its intervals: `work` to its first
/// `validating`, `validate` that interval, `wait_to_land` from its end to
/// the first `push` and `land_phases` its intervals between, by the phase
/// of the landing each falls under; `startup` between the milestones it
/// names.
fn run_names(values: &mut LedgerValues, row: &LedgerRow, milestones: &Milestones) {
    let secs = |from: Option<i64>, to: Option<i64>| Some((to? - from?) / 1000);
    let validating = validation(&row.intervals);
    let push = row.intervals.iter().find(|i| i.phase == "push");
    values.work = secs(row.started_at, validating.map(|i| i.start));
    values.validate = validating.map(|i| i.millis() / 1000);
    values.startup = secs(milestones.agent_started, milestones.first_commit);
    let (Some(validated), Some(push)) = (validating.map(|i| i.end), push) else {
        return;
    };
    values.wait_to_land = secs(Some(validated), Some(push.start));
    let mut millis = [0_i64; PHASES.len()];
    for interval in &row.intervals {
        let (from, to) = (interval.start.max(validated), interval.end.min(push.start));
        if to > from
            && let Some(index) = PHASES.iter().position(|name| *name == land_phase(interval))
        {
            millis[index] += to - from;
        }
    }
    let push_millis: i64 = row
        .intervals
        .iter()
        .filter(|i| i.phase == "push")
        .map(Interval::millis)
        .sum();
    values.land_phases = Some(LandPhases {
        secs: millis.map(|ms| ms / 1000),
        push: Some(push_millis / 1000),
        verify_commands: Vec::new(),
        landing_queue_via: Vec::new(),
    });
}

/// The interval that ends `work` and is `validate` under today's names:
/// a run's first `validating`. Before it the run is in its `work`, after
/// it in its `wait_to_land` (to its first `push`); the forecast places a
/// run in flight by the same rule.
pub fn validation(intervals: &[Interval]) -> Option<&Interval> {
    intervals.iter().find(|i| i.phase == "validating")
}

/// The phase of today's `land_phases` an interval of the wait to land
/// falls under. The landing is one phase now, so it is all `verify` and
/// `rebase` stays 0; the wait for the landing slot after the review and
/// the e2e (`awaiting_slot`) is `landing_queue`, as it is today; what the
/// phases do not name (the session's exit, the hand-offs, unattributed
/// time) is `exit`, as it is today.
fn land_phase(interval: &Interval) -> &'static str {
    match interval.phase.as_str() {
        "review" | "review_held" => "review",
        "revise"
            if interval
                .attempt
                .is_some_and(|a| a.kind == crate::domain::run_phase::AttemptKind::Conflict) =>
        {
            "conflict"
        }
        "revise" => "revise",
        "waiting" | "landing_answer" => "ask",
        "needs_session" | "resume" | "recovery" => "resume",
        "landing_queue" | "awaiting_slot" => "landing_queue",
        "awaiting_e2e" => "e2e_wait",
        "e2e" => "e2e",
        "landing" => "verify",
        _ => "exit",
    }
}
