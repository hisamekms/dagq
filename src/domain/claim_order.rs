//! The claim order across the execution classes (ADR-t1487-2, its
//! decision 2 amended by ADR-t1489-1): the candidates the class limits
//! leave out (ADR-t1487-2 decision 3(b)), and the aging that, within one
//! effective priority, lets the class (implementation or Spike) that
//! waited past the threshold go first. The supervisor's claims and the
//! order `candidates` and `graph` show both go through [`arrange`]: the
//! supervisor with the candidates its other deferrals left and the Spikes
//! in its slots, the command line with the deferrals the supervisor
//! recorded as the excluded set and no limit of its own.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::{
    EventKind, ExecutionClass, LeaseToken, Priority, RunEvent, TaskId,
    claim_defer::OpenDeferral,
    investigation::{INVESTIGATION_LIMIT, InvestigationLimit, investigation_message},
    pre_claim::heavy_slots_free,
    stats::timestamp_millis,
};

/// The default of `[supervisor] class_aging`, in seconds: a class whose
/// candidate waited an hour since it became claimable goes first within
/// its effective priority. A generic value of the runtime, not a
/// repository's.
pub const DEFAULT_CLASS_AGING_SECS: usize = 3600;

/// The `claim_deferred` reason of a Spike left out while as many Spikes
/// as `[supervisor] max_spikes` hold a slot.
pub const SPIKE_LIMIT: &str = "spike_limit";

/// The reasons of the class limits: [`ClassLimits::limit`] gives one of
/// them.
pub const CLASS_LIMITS: [&str; 1] = [SPIKE_LIMIT];

/// The `claim_deferred` reason of a candidate the room the landing queue
/// leaves cannot take (ADR-t1591-1): recorded while the claim arranges
/// only what that room takes, so the order `candidates` shows leaves it
/// out as the claim does.
pub const LIGHT_ROOM: &str = "light_room";

/// The reasons the claim order's step records (the class limits, the
/// investigation limit and the light room): a deferral of one is the
/// order's, not a worker's.
pub const ORDER_DEFERRALS: [&str; 3] = [SPIKE_LIMIT, INVESTIGATION_LIMIT, LIGHT_ROOM];

/// What a claim's transaction holds whatever task it reads, not only those
/// of the supervisor's order, nor when it falls back to the claim order:
/// the classes a class limit holds, and the Spikes that would open an
/// investigation past `[supervisor] max_investigations` (ADR-t1487-1
/// decision 7), judged again in the transaction from the queue as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClaimHolds {
    pub classes: Vec<ExecutionClass>,
    pub investigations: InvestigationLimit,
}

/// A claimable task as the order compares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    pub id: TaskId,
    pub class: ExecutionClass,
    /// Its effective priority: aging never moves a task out of it.
    pub priority: Priority,
    /// When it last became claimable (`claim_facts::claimable_since`), in Unix
    /// milliseconds; `None` never ages.
    pub ready_at_ms: Option<i64>,
}

/// The class limits in force for a claim: `max_spikes` (`None` limits
/// nothing) against the Spikes holding a slot now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassLimits {
    pub max_spikes: Option<usize>,
    pub running_spikes: usize,
}

impl ClassLimits {
    /// Why a task of `class` may not be claimed now ([`CLASS_LIMITS`]),
    /// `None` when it may. An interrupt is held to it too.
    pub fn limit(self, class: ExecutionClass) -> Option<&'static str> {
        match class {
            ExecutionClass::Spike => self
                .max_spikes
                .filter(|max| self.running_spikes >= *max)
                .map(|_| SPIKE_LIMIT),
            ExecutionClass::Implementation => None,
        }
    }

    /// The classes a claim may not take now, whatever task it reads: the
    /// claim's fallback to the claim order holds them too.
    pub fn held(self) -> Vec<ExecutionClass> {
        [ExecutionClass::Implementation, ExecutionClass::Spike]
            .into_iter()
            .filter(|class| self.limit(*class).is_some())
            .collect()
    }
}

/// The order a claim takes the candidates in, the tasks aging moved
/// ahead, and those a class limit left out, with its reason.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Arranged {
    pub order: Vec<TaskId>,
    pub aged: Vec<TaskId>,
    pub limited: Vec<(TaskId, &'static str)>,
}

/// Whether a wait of `waited_ms` is past the threshold of `aging_secs`:
/// strictly more, so a wait of exactly the threshold does not age.
pub fn overdue(waited_ms: i64, aging_secs: usize) -> bool {
    let threshold_ms = i64::try_from(aging_secs)
        .unwrap_or(i64::MAX)
        .saturating_mul(1000);
    waited_ms > threshold_ms
}

/// Arrange `candidates` (in the rule's order: effective priority, then
/// unblocks, then ID) for a claim at `now_ms`: leave out the `excluded`
/// and those a class limit of `limits` holds, then age each effective
/// priority but `interrupt` on its own (ADR-t1489-1). A class's wait
/// there is the longest of its candidates' waits past `aging_secs` since
/// they became claimable. When one class waited past it and longer than
/// the other, its candidates go first within that priority, in the
/// rule's order among themselves, and the other class's follow in theirs.
/// Otherwise, and with one class there, the order is the rule's. No task
/// leaves its effective priority, so aging passes no higher one, and an
/// interrupt is never reordered.
pub fn arrange(
    candidates: &[Candidate],
    excluded: &HashSet<TaskId>,
    limits: ClassLimits,
    now_ms: i64,
    aging_secs: usize,
) -> Arranged {
    let mut limited = Vec::new();
    let mut kept: Vec<&Candidate> = Vec::new();
    for candidate in candidates {
        if excluded.contains(&candidate.id) {
            continue;
        }
        match limits.limit(candidate.class) {
            Some(reason) => limited.push((candidate.id, reason)),
            None => kept.push(candidate),
        }
    }
    let wait = |candidate: &Candidate| {
        candidate
            .ready_at_ms
            .map(|at| now_ms.saturating_sub(at))
            .filter(|waited| overdue(*waited, aging_secs))
    };
    let mut order = Vec::with_capacity(kept.len());
    let mut aged = Vec::new();
    let mut tier_start = 0;
    while tier_start < kept.len() {
        let priority = kept[tier_start].priority;
        let tier_end = kept[tier_start..]
            .iter()
            .position(|candidate| candidate.priority != priority)
            .map_or(kept.len(), |offset| tier_start + offset);
        let tier = &kept[tier_start..tier_end];
        let class_wait = |class: ExecutionClass| {
            tier.iter()
                .filter(|candidate| candidate.class == class)
                .filter_map(|candidate| wait(candidate))
                .max()
        };
        let first = (priority != Priority::Interrupt)
            .then(|| {
                match (
                    class_wait(ExecutionClass::Implementation),
                    class_wait(ExecutionClass::Spike),
                ) {
                    (Some(implementation), Some(spike)) if implementation > spike => {
                        Some(ExecutionClass::Implementation)
                    }
                    (Some(implementation), Some(spike)) if spike > implementation => {
                        Some(ExecutionClass::Spike)
                    }
                    (Some(_), None) => Some(ExecutionClass::Implementation),
                    (None, Some(_)) => Some(ExecutionClass::Spike),
                    _ => None,
                }
            })
            .flatten();
        match first {
            Some(class) => {
                let arranged: Vec<&Candidate> = tier
                    .iter()
                    .filter(|candidate| candidate.class == class)
                    .chain(tier.iter().filter(|candidate| candidate.class != class))
                    .copied()
                    .collect();
                for (index, candidate) in arranged.iter().enumerate() {
                    let was = tier.iter().position(|c| c.id == candidate.id);
                    if was.is_some_and(|was| index < was) {
                        aged.push(candidate.id);
                    }
                }
                order.extend(arranged.iter().map(|candidate| candidate.id));
            }
            None => order.extend(tier.iter().map(|candidate| candidate.id)),
        }
        tier_start = tier_end;
    }
    Arranged {
        order,
        aged,
        limited,
    }
}

/// The `claim_deferred` of a task a class limit holds (`reason` from
/// [`ClassLimits::limit`]). It lasts until the limit lets it go or the
/// task leaves the candidates; it has no limit of its own.
pub fn class_deferred(reason: &str, limits: ClassLimits, token: &LeaseToken) -> (EventKind, Value) {
    (
        EventKind::ClaimDeferred,
        json!({
            "reason": reason,
            "limit": limits.max_spikes,
            "running": limits.running_spikes,
            "message": format!(
                "{} Spikes hold a slot, the limit of [supervisor] max_spikes: not claimed until one leaves its slot",
                limits.running_spikes
            ),
            "supervisor": token,
        }),
    )
}

/// The `claim_deferred` of a Spike the investigation limit holds
/// ([`INVESTIGATION_LIMIT`]): `counted` investigations open against `max`.
/// It lasts until one ends, or the task leaves the candidates.
pub fn investigation_deferred(
    counted: usize,
    max: usize,
    token: &LeaseToken,
) -> (EventKind, Value) {
    (
        EventKind::ClaimDeferred,
        json!({
            "reason": INVESTIGATION_LIMIT,
            "limit": max,
            "running": counted,
            "message": investigation_message(counted, max),
            "supervisor": token,
        }),
    )
}

/// How long passes with a free normal slot keep a [`LIGHT_ROOM`]
/// deferral open, in seconds, counted from the first of them after the
/// last pass that was not one. The landing queue's room and a free slot
/// take turns as runs land and claims fill the slots: a deferral ended at
/// each turn would write an end and a new deferral for every heavy
/// candidate each time. Longer than the default claim spacing, so a free
/// slot that only waits for the spacing keeps it too.
pub const LIGHT_ROOM_KEPT_SECS: i64 = 300;

/// The `claim_deferred` of a candidate the room the landing queue leaves
/// cannot take ([`LIGHT_ROOM`]). It lasts while the task stays a
/// candidate that the light room's passes defer and no other deferral
/// holds, through the passes with a free normal slot in between for
/// [`LIGHT_ROOM_KEPT_SECS`] (see [`class_deferral_step`]).
pub fn light_room_deferred(token: &LeaseToken) -> (EventKind, Value) {
    (
        EventKind::ClaimDeferred,
        json!({
            "reason": LIGHT_ROOM,
            "message": "only the room the landing queue leaves is free, which takes the light changes that declare their paths: not claimed until a slot is free",
            "supervisor": token,
        }),
    )
}

/// The deferrals of `open` that hold their task out of the claim now,
/// with `live` the supervisors alive now and `slots_full` the newest
/// `slots_full_*` of each of them: a [`LIGHT_ROOM`] one only while none of
/// them has a heavy slot free ([`heavy_slots_free`]), every other one as
/// recorded. Over a free normal slot the light room's record stays open
/// ([`class_deferral_step`]), whichever supervisor recorded it, while the
/// claim of the supervisor with that slot may take its task: `candidates`
/// reads its order from these, so it shows the task where the claim takes
/// it. With no supervisor alive the records hold as recorded, as no claim
/// is made.
pub fn in_effect(
    open: Vec<OpenDeferral>,
    live: &[String],
    slots_full: &[RunEvent],
) -> Vec<OpenDeferral> {
    let free = live.iter().any(|token| heavy_slots_free(slots_full, token));
    open.into_iter()
        .filter(|deferral| !free || deferral.reason != LIGHT_ROOM)
        .collect()
}

/// What a pass makes of an order's deferral in place of `why`, from the
/// reason the order defers the task for this pass (`now`), whether it is
/// still a candidate, whether it passed the deferrals before the order's
/// (worker, build, hotspot), and, in a pass with a free normal slot, for
/// how long the passes have had one (`normal_secs`; `None` in a pass of
/// the light room or with no free slot).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassDeferralStep {
    /// Deferred for the same reason: the record stays open.
    Open,
    /// A candidate another deferral held before the order: that one's
    /// `claim_deferred` is the task's latest record now, so this one is
    /// dropped without an end of its own.
    Superseded,
    /// It ends with this `why` (`cleared` or `not_candidate`).
    Ended(&'static str),
}

/// [`ClassDeferralStep`] of a deferral of `why`. A [`LIGHT_ROOM`]
/// deferral stays open in a pass with a free normal slot that the order
/// passes the task in (the claim may take it there), until such passes
/// last [`LIGHT_ROOM_KEPT_SECS`]: it ends when the task is claimed or
/// leaves the candidates (`not_candidate`), when another deferral holds it
/// (superseded), when the order defers it for another reason or the light
/// room takes it (`cleared`), or when the free slot lasted that long
/// (`cleared`).
pub fn class_deferral_step(
    why: &str,
    now: Option<&str>,
    candidate: bool,
    passed: bool,
    normal_secs: Option<i64>,
) -> ClassDeferralStep {
    let kept = why == LIGHT_ROOM
        && now.is_none()
        && normal_secs.is_some_and(|secs| secs < LIGHT_ROOM_KEPT_SECS);
    if now == Some(why) || (candidate && passed && kept) {
        ClassDeferralStep::Open
    } else if candidate && !passed {
        ClassDeferralStep::Superseded
    } else if candidate {
        ClassDeferralStep::Ended("cleared")
    } else {
        ClassDeferralStep::Ended("not_candidate")
    }
}

/// The claim order's deferrals in place from each task's latest
/// `claim_defer::DEFERRAL_KINDS` event: an open `claim_deferred` of an
/// [`ORDER_DEFERRALS`] reason, with its reason and since when (unix
/// seconds).
pub fn class_deferrals_in_place(latest: &[RunEvent]) -> HashMap<TaskId, (String, i64)> {
    latest
        .iter()
        .filter(|event| event.kind == EventKind::ClaimDeferred)
        .filter_map(|event| {
            let reason = event.payload.get("reason").and_then(Value::as_str)?;
            if !ORDER_DEFERRALS.contains(&reason) {
                return None;
            }
            let since = timestamp_millis(&event.created_at)? / 1000;
            Some((event.task_id?, (reason.to_owned(), since)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::claim_defer::CLAIM_DEFERRED;

    const MIN: i64 = 60_000;
    const NOW: i64 = 1_000 * MIN;
    /// The threshold the tests age by: ten minutes.
    const AGING: usize = 600;

    fn implementation(id: i64, priority: Priority, waited_min: i64) -> Candidate {
        Candidate {
            id: TaskId::new(id),
            class: ExecutionClass::Implementation,
            priority,
            ready_at_ms: Some(NOW - waited_min * MIN),
        }
    }

    fn spike(id: i64, priority: Priority, waited_min: i64) -> Candidate {
        Candidate {
            class: ExecutionClass::Spike,
            ..implementation(id, priority, waited_min)
        }
    }

    fn ids(order: &[TaskId]) -> Vec<i64> {
        order.iter().map(|id| id.as_i64()).collect()
    }

    fn arranged(candidates: &[Candidate], limits: ClassLimits) -> Arranged {
        arrange(candidates, &HashSet::new(), limits, NOW, AGING)
    }

    const NO_LIMIT: ClassLimits = ClassLimits {
        max_spikes: None,
        running_spikes: 0,
    };

    #[test]
    fn without_a_spike_the_order_is_the_rule_s() {
        let candidates = [
            implementation(3, Priority::High, 90),
            implementation(1, Priority::Normal, 120),
            implementation(2, Priority::Normal, 5),
        ];
        let result = arranged(
            &candidates,
            ClassLimits {
                max_spikes: Some(1),
                running_spikes: 1,
            },
        );
        assert_eq!(ids(&result.order), [3, 1, 2]);
        assert!(result.aged.is_empty());
        assert!(result.limited.is_empty());
        // A candidate that waited far past the threshold behind a fresh
        // head of its class stays behind it, and so do Spikes alone.
        let candidates = [
            implementation(1, Priority::High, 1),
            implementation(2, Priority::Normal, 60),
        ];
        assert_eq!(ids(&arranged(&candidates, NO_LIMIT).order), [1, 2]);
        let candidates = [spike(1, Priority::High, 1), spike(2, Priority::Low, 60)];
        assert_eq!(ids(&arranged(&candidates, NO_LIMIT).order), [1, 2]);
    }

    #[test]
    fn the_spike_limit_leaves_every_spike_out_and_the_room_to_implementations() {
        let candidates = [
            spike(1, Priority::Interrupt, 1),
            spike(2, Priority::High, 1),
            implementation(3, Priority::Normal, 1),
        ];
        let full = ClassLimits {
            max_spikes: Some(1),
            running_spikes: 1,
        };
        let result = arranged(&candidates, full);
        // An interrupt Spike does not pass the limit.
        assert_eq!(ids(&result.order), [3]);
        assert_eq!(
            result.limited,
            [(TaskId::new(1), SPIKE_LIMIT), (TaskId::new(2), SPIKE_LIMIT)]
        );
        assert_eq!(full.held(), [ExecutionClass::Spike]);
        // Under the limit, or with none, the Spikes are claimable: no slot
        // is kept for either class.
        for limits in [
            ClassLimits {
                max_spikes: Some(2),
                running_spikes: 1,
            },
            NO_LIMIT,
        ] {
            let result = arranged(&candidates, limits);
            assert_eq!(ids(&result.order), [1, 2, 3]);
            assert!(result.limited.is_empty());
            assert!(limits.held().is_empty());
        }
    }

    /// Within one priority the Spikes that waited past the threshold go
    /// ahead of fresh implementations, in their own order: Spike 4 does
    /// not pass Spike 3, though Spike 4 waited longer.
    #[test]
    fn a_spike_class_past_the_threshold_goes_first_within_its_priority() {
        let candidates = [
            implementation(1, Priority::Normal, 5),
            implementation(2, Priority::Normal, 5),
            spike(3, Priority::Normal, 5),
            spike(4, Priority::Normal, 30),
        ];
        let result = arranged(&candidates, NO_LIMIT);
        assert_eq!(ids(&result.order), [3, 4, 1, 2]);
        assert_eq!(ids(&result.aged), [3, 4]);
        // Not past the threshold yet: the rule's order.
        let result = arrange(&candidates, &HashSet::new(), NO_LIMIT, NOW, 3600);
        assert_eq!(ids(&result.order), [1, 2, 3, 4]);
        assert!(result.aged.is_empty());
    }

    /// Aging passes no higher priority, whatever the wait: a low Spike past
    /// the threshold stays behind high and normal implementations, and goes
    /// first only among the low ones.
    #[test]
    fn aging_never_passes_a_higher_priority() {
        let candidates = [
            implementation(1, Priority::High, 1),
            implementation(2, Priority::Normal, 1),
            implementation(3, Priority::Low, 1),
            spike(4, Priority::Low, 120),
        ];
        let result = arranged(&candidates, NO_LIMIT);
        assert_eq!(ids(&result.order), [1, 2, 4, 3]);
        assert_eq!(ids(&result.aged), [4]);
    }

    /// Implementations that waited longer than the Spikes go first again:
    /// Spikes that keep arriving within the priority do not starve them.
    #[test]
    fn the_class_that_waited_longer_goes_first() {
        let candidates = [
            spike(1, Priority::Normal, 20),
            implementation(2, Priority::Normal, 30),
            spike(3, Priority::Normal, 11),
        ];
        let result = arranged(&candidates, NO_LIMIT);
        assert_eq!(ids(&result.order), [2, 1, 3]);
        assert_eq!(ids(&result.aged), [2]);
        // The Spikes waited longer: they go first, the head already one.
        let candidates = [
            implementation(1, Priority::Normal, 20),
            spike(2, Priority::Normal, 30),
            implementation(3, Priority::Normal, 11),
        ];
        assert_eq!(ids(&arranged(&candidates, NO_LIMIT).order), [2, 1, 3]);
        // The same wait: the rule's order.
        let candidates = [
            implementation(1, Priority::Normal, 30),
            spike(2, Priority::Normal, 30),
        ];
        let result = arranged(&candidates, NO_LIMIT);
        assert_eq!(ids(&result.order), [1, 2]);
        assert!(result.aged.is_empty());
    }

    /// The threshold is strict: a wait of exactly `class_aging` does not
    /// age, one millisecond more does.
    #[test]
    fn a_wait_of_exactly_the_threshold_does_not_age() {
        assert!(!overdue(600_000, AGING));
        assert!(overdue(600_001, AGING));
        assert!(!overdue(599_999, AGING));
        let at = |waited_ms: i64| {
            [
                implementation(1, Priority::Normal, 0),
                Candidate {
                    ready_at_ms: Some(NOW - waited_ms),
                    ..spike(2, Priority::Normal, 0)
                },
            ]
        };
        assert_eq!(ids(&arranged(&at(600_000), NO_LIMIT).order), [1, 2]);
        assert_eq!(ids(&arranged(&at(600_001), NO_LIMIT).order), [2, 1]);
    }

    /// The interrupts keep the rule's order, aging or not, and nothing
    /// passes them.
    #[test]
    fn aging_never_reorders_or_passes_an_interrupt() {
        let candidates = [
            implementation(1, Priority::Interrupt, 1),
            spike(2, Priority::Interrupt, 120),
            implementation(3, Priority::High, 1),
            spike(4, Priority::High, 60),
        ];
        let result = arranged(&candidates, NO_LIMIT);
        assert_eq!(ids(&result.order), [1, 2, 4, 3]);
        assert_eq!(ids(&result.aged), [4]);
    }

    #[test]
    fn a_candidate_without_its_time_does_not_age() {
        let candidates = [
            implementation(1, Priority::High, 1),
            Candidate {
                ready_at_ms: None,
                ..spike(2, Priority::High, 0)
            },
        ];
        let result = arranged(&candidates, NO_LIMIT);
        assert_eq!(ids(&result.order), [1, 2]);
        assert!(result.aged.is_empty());
    }

    #[test]
    fn an_excluded_or_limited_candidate_is_not_aged_and_the_next_one_takes_the_room() {
        let candidates = [
            implementation(1, Priority::Normal, 1),
            spike(2, Priority::Normal, 60),
            spike(3, Priority::Normal, 30),
            implementation(4, Priority::Normal, 1),
        ];
        // The deferred Spike (a hotspot, a provider, a record of the
        // supervisor's) is out; the class ages by the one left.
        let excluded = HashSet::from([TaskId::new(2)]);
        let result = arrange(&candidates, &excluded, NO_LIMIT, NOW, AGING);
        assert_eq!(ids(&result.order), [3, 1, 4]);
        assert_eq!(ids(&result.aged), [3]);
        // At the Spike limit no Spike ages: the room goes to the next
        // claimable implementation.
        let result = arranged(
            &candidates,
            ClassLimits {
                max_spikes: Some(1),
                running_spikes: 1,
            },
        );
        assert_eq!(ids(&result.order), [1, 4]);
        assert!(result.aged.is_empty());
        assert_eq!(result.limited.len(), 2);
    }

    /// The light room passes the tasks it cannot take as the excluded: a
    /// heavy implementation that waited longest does not hold back the
    /// aging of a light Spike behind a fresh light implementation.
    #[test]
    fn the_tasks_the_room_cannot_take_do_not_hold_the_aging_back() {
        let candidates = [
            implementation(1, Priority::High, 120),
            implementation(2, Priority::High, 1),
            spike(3, Priority::High, 30),
        ];
        // Over every candidate the heavy one's wait keeps the
        // implementations first.
        assert_eq!(ids(&arranged(&candidates, NO_LIMIT).order), [1, 2, 3]);
        let heavy = HashSet::from([TaskId::new(1)]);
        let result = arrange(&candidates, &heavy, NO_LIMIT, NOW, AGING);
        assert_eq!(ids(&result.order), [3, 2]);
        assert_eq!(ids(&result.aged), [3]);
    }

    #[test]
    fn the_order_s_deferrals_are_read_back_and_a_worker_s_is_not() {
        let event = |task: i64, reason: &str| RunEvent {
            id: crate::domain::EventId::new(task),
            task_id: Some(TaskId::new(task)),
            goal_id: None,
            run_id: None,
            kind: CLAIM_DEFERRED.into(),
            payload: json!({"reason": reason}),
            created_at: "2026-10-11 10:00:00".to_owned(),
            actor: None,
        };
        let token = LeaseToken::new("tok");
        let (kind, payload) = class_deferred(
            SPIKE_LIMIT,
            ClassLimits {
                max_spikes: Some(1),
                running_spikes: 1,
            },
            &token,
        );
        assert_eq!(kind, EventKind::ClaimDeferred);
        assert_eq!(payload["reason"], SPIKE_LIMIT);
        assert_eq!(
            (payload["limit"].clone(), payload["running"].clone()),
            (json!(1), json!(1))
        );
        let (kind, payload) = light_room_deferred(&token);
        assert_eq!(kind, EventKind::ClaimDeferred);
        assert_eq!(payload["reason"], LIGHT_ROOM);
        let latest = [
            event(1, SPIKE_LIMIT),
            event(2, "provider_unavailable"),
            event(3, LIGHT_ROOM),
        ];
        let in_place = class_deferrals_in_place(&latest);
        assert_eq!(in_place.len(), 2);
        assert_eq!(in_place[&TaskId::new(1)].0, SPIKE_LIMIT);
        assert_eq!(in_place[&TaskId::new(3)].0, LIGHT_ROOM);
        let workers = crate::domain::claim_defer::worker_deferrals_in_place(&latest);
        assert_eq!(workers.keys().collect::<Vec<_>>(), [&TaskId::new(2)]);
    }

    #[test]
    fn an_order_s_deferral_superseded_by_another_ends_without_a_record_of_its_own() {
        use ClassDeferralStep::*;
        let step =
            |why, now, candidate, passed| class_deferral_step(why, now, candidate, passed, None);
        // Held by the same reason, whether or not it passed before.
        assert_eq!(step(SPIKE_LIMIT, Some(SPIKE_LIMIT), true, true), Open);
        // A worker, build or hotspot deferral held it before the order.
        assert_eq!(step(SPIKE_LIMIT, None, true, false), Superseded);
        // The order passed it, or holds it for another of its reasons.
        assert_eq!(step(SPIKE_LIMIT, None, true, true), Ended("cleared"));
        assert_eq!(
            step(SPIKE_LIMIT, Some(LIGHT_ROOM), true, true),
            Ended("cleared")
        );
        assert_eq!(
            step(SPIKE_LIMIT, None, false, false),
            Ended("not_candidate")
        );
    }

    /// The landing queue's room and a free normal slot taking turns: the
    /// light room's deferral of a heavy candidate stays one record, however
    /// many turns, until the free slot lasts [`LIGHT_ROOM_KEPT_SECS`].
    #[test]
    fn a_light_room_deferral_stays_open_across_brief_free_slots() {
        use ClassDeferralStep::*;
        let heavy = TaskId::new(1);
        let mut open: HashMap<TaskId, &str> = [(heavy, LIGHT_ROOM)].into();
        let mut records = 1;
        // (the reason the pass defers it for, seconds of free normal slot)
        let passes = [
            (None, Some(0)),
            (Some(LIGHT_ROOM), None),
            (None, Some(0)),
            (None, Some(LIGHT_ROOM_KEPT_SECS - 1)),
            (Some(LIGHT_ROOM), None),
            (None, Some(10)),
            (Some(LIGHT_ROOM), None),
        ];
        for (now, normal_secs) in passes {
            if let Some(why) = open.get(&heavy).copied() {
                match class_deferral_step(why, now, true, true, normal_secs) {
                    Open => continue,
                    _ => {
                        open.remove(&heavy);
                    }
                }
            }
            if let Some(now) = now {
                open.insert(heavy, now);
                records += 1;
            }
        }
        assert_eq!((records, open.get(&heavy)), (1, Some(&LIGHT_ROOM)));
        let step = class_deferral_step;
        // A free slot that lasted: the task is in the order, so it ends.
        assert_eq!(
            step(LIGHT_ROOM, None, true, true, Some(LIGHT_ROOM_KEPT_SECS)),
            Ended("cleared")
        );
        // Claimed, or no candidate any more.
        assert_eq!(
            step(LIGHT_ROOM, None, false, false, Some(0)),
            Ended("not_candidate")
        );
        // Held by a worker, build or hotspot deferral.
        assert_eq!(step(LIGHT_ROOM, None, true, false, Some(0)), Superseded);
        // Deferred by the order for another reason.
        assert_eq!(
            step(LIGHT_ROOM, Some(SPIKE_LIMIT), true, true, Some(0)),
            Ended("cleared")
        );
        // Taken by the light room (a light task now): no free slot to keep it.
        assert_eq!(step(LIGHT_ROOM, None, true, true, None), Ended("cleared"));
        // A class limit's deferral is not kept by a free slot.
        assert_eq!(
            step(SPIKE_LIMIT, None, true, true, Some(0)),
            Ended("cleared")
        );
    }

    /// A light room's record holds its task out of `candidates` only while
    /// no live supervisor has a heavy slot free, whichever supervisor
    /// recorded it: over a free slot the claim may take the task, and the
    /// order shows it there.
    #[test]
    fn a_light_room_deferral_holds_only_while_no_live_supervisor_has_a_heavy_slot_free() {
        let deferral = |task, reason: &str, supervisor: Option<&str>| OpenDeferral {
            task_id: TaskId::new(task),
            reason: reason.to_owned(),
            since: "2026-10-21 10:00:00".to_owned(),
            files: Value::Null,
            runs: Value::Null,
            supervisor: supervisor.map(str::to_owned),
            build: None,
            missing: None,
        };
        let slots = |id, kind: EventKind, token: &str, class: &str| RunEvent {
            id: crate::domain::EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.as_str().into(),
            payload: json!({"supervisor": token, "class": class}),
            created_at: "2026-10-21 10:00:00".to_owned(),
            actor: None,
        };
        let open = vec![
            deferral(1, LIGHT_ROOM, Some("gone")),
            deferral(2, SPIKE_LIMIT, Some("a")),
            deferral(3, LIGHT_ROOM, None),
        ];
        let held = |live: &[&str], records: &[RunEvent]| -> Vec<i64> {
            let live: Vec<String> = live.iter().map(|token| (*token).to_owned()).collect();
            in_effect(open.clone(), &live, records)
                .iter()
                .map(|deferral| deferral.task_id.as_i64())
                .collect()
        };
        // The supervisor that recorded task 1's deferral is gone, its heavy
        // slots left full; `a` judges it now, in the light room.
        let light_room = [
            slots(10, EventKind::SlotsFullStarted, "gone", "heavy"),
            slots(11, EventKind::SlotsFullStarted, "a", "heavy"),
        ];
        assert_eq!(held(&["a"], &light_room), [1, 2, 3]);
        // `a` has a free slot: its claim may take tasks 1 and 3.
        let free = [
            slots(10, EventKind::SlotsFullStarted, "gone", "heavy"),
            slots(12, EventKind::SlotsFullEnded, "a", "heavy"),
        ];
        assert_eq!(held(&["a"], &free), [2]);
        // A supervisor that never recorded its slots full has them free.
        assert_eq!(held(&["a", "b"], &light_room), [2]);
        // With none alive, the records hold as recorded.
        assert_eq!(held(&[], &free), [1, 2, 3]);
    }
}
