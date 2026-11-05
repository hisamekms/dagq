//! The one read of a ledger row (ADR-t1662-1 decisions 12 and 18): the
//! values under today's names (`work`, `validate`, `wait_to_land`,
//! `land_phases`, `startup`) and the new breakdown, from a new row's
//! intervals or a legacy row's frozen values, in the same names and
//! shapes, with the legacy mark; and what a run used of providers it could
//! not use, or that its row has no record of it. Readers of the ledger go through the
//! `LedgerStore` and this function only.

use serde::Serialize;
use serde_json::{Map, Value};

use super::failure_cause;
use super::fold::{Interval, Milestones};
use super::provider_use::ProviderUse;
use super::{LedgerKind, LedgerRow, Totals};
use crate::domain::after_claim::AfterClaim;
use crate::domain::recovery::FailureCause;
use crate::domain::stats::landing::{LandPhases, PHASES};
use crate::domain::stats::ledger_times::RowTimes;

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
    /// A legacy row's is the split its frozen values hold, as they were
    /// written (none when they hold none, as a legacy row written now does).
    pub land_phases: Option<LandPhases>,
    pub startup: Option<i64>,
    /// The time and the slot time, per blocker, holds and attempt, and the
    /// unattributed time; none for a legacy row.
    pub breakdown: Option<Totals>,
    /// The claim's attributes as recorded, whatever their keys, and the
    /// facts after the claim (a run's).
    pub attributes: Map<String, Value>,
    pub after_claim: Option<AfterClaim>,
    /// A run's worker's waits for a provider and its review's jobs that
    /// moved off their planned provider ([`ProviderUse`]), a legacy run's
    /// too. None when the row has no record of them (one an older fold
    /// wrote, a task), and each part none when the run's events hold none,
    /// which is not a run that used none (an empty map).
    pub provider_use: Option<ProviderUse>,
    /// What a run's recovery jobs judged caused its failure
    /// ([`failure_cause`]), a legacy run's too. Unknown when the row holds
    /// none (one an older fold wrote, a task), which does not say the
    /// environment was not the cause.
    pub failure_cause: FailureCause,
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
        provider_use: (row.kind == LedgerKind::Run)
            .then(|| ProviderUse::of_detail(&row.detail))
            .flatten(),
        failure_cause: failure_cause::of_detail(&row.detail),
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
///
/// Where the meaning differs from the values `stats` made of the events
/// before the ledger: `work` starts at the row's start (its first
/// interval, the claim) and ends where `validating` starts, not at the
/// receipt; `validate` is the `validating` interval; `wait_to_land` ends
/// where `push` starts, at `run_integrated`, as before. In `land_phases`
/// a person's wait (`waiting`, `landing_answer`, an `approve_landing`
/// answer among them) is `ask`, never `exit`, and `held` is `held`.
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
    let push_millis: i64 = row
        .intervals
        .iter()
        .filter(|i| i.phase == "push")
        .map(Interval::millis)
        .sum();
    values.land_phases = Some(LandPhases {
        secs: split(&row.intervals, validated, push.start),
        push: Some(push_millis / 1000),
        verify_commands: Vec::new(),
        landing_queue_via: Vec::new(),
    });
}

/// The whole seconds of `intervals` from `from` to `to` (unix
/// milliseconds) under each phase of `land_phases`.
fn split(intervals: &[Interval], from: i64, to: i64) -> [i64; PHASES.len()] {
    let mut millis = [0_i64; PHASES.len()];
    for interval in intervals {
        let (start, end) = (interval.start.max(from), interval.end.min(to));
        if end > start
            && let Some(index) = PHASES.iter().position(|name| *name == land_phase(interval))
        {
            millis[index] += end - start;
        }
    }
    millis.map(|ms| ms / 1000)
}

/// A run row's times under today's names as `stats` and `kpi` take them
/// ([`RowTimes`]): [`read`]'s, and the phase its wait to land spent most
/// in so far, for a run still waiting up to its last interval's end.
pub fn times(row: &LedgerRow) -> RowTimes {
    let values = read(row);
    let waiting_phase = match &values.land_phases {
        Some(phases) => phases.longest(),
        None if !values.legacy => validation(&row.intervals).and_then(|validating| {
            let end = row.intervals.last()?.end;
            let secs = split(&row.intervals, validating.end, end);
            let phases = LandPhases {
                secs,
                ..LandPhases::default()
            };
            phases.longest()
        }),
        None => None,
    };
    RowTimes {
        legacy: values.legacy,
        work: values.work,
        validate: values.validate,
        wait_to_land: values.wait_to_land,
        land_phases: values.land_phases,
        startup: values.startup,
        waiting_phase,
    }
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
/// the e2e (`awaiting_slot`) is `landing_queue`, as it is today; a
/// person's wait for an answer (`waiting`, `landing_answer`) is `ask` and
/// the hold's (`held`) is `held`; what the phases do not name (the
/// session's exit, the hand-offs, unattributed time) is `exit`.
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
        "held" => "held",
        _ => "exit",
    }
}
