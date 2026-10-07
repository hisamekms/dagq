//! The breakdown of `wait_to_land` by phase (goal 36, ADR-0049 decision
//! 5): the time from a run's first `validation_finished` to its
//! `run_integrated`, cut at the events that start each phase of the
//! landing, so that the phases add up to `wait_to_land`. Each event that
//! starts a phase closes the one before; an event that starts none leaves
//! the time with the current phase. The push, which follows
//! `run_integrated`, is measured on its own and is not part of the sum.
//! Each phase is cut to whole seconds, so the sum may fall a few seconds
//! short of `wait_to_land`.
//! The `verify` phase is also split by verification command (task 509),
//! and the `landing_queue` phase by what queued the run (`via`, task 949).
use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde::ser::SerializeMap;
use serde_json::Value;

use super::{RunEvent, Summary, median, payload_status};

/// The phases, in the order a landing goes through them.
pub const PHASES: [&str; 11] = [
    // The session's exit and the hand-offs between phases (from
    // `validation_finished`, `review_finished`, a precheck that asks
    // nothing): what is left until the run waits for the slot.
    "exit",
    // The headless review (`review_started` / `review_retried`).
    "review",
    // A revise sent to the live session, through its new receipt's
    // validation (`revise_requested`).
    "revise",
    // The live session resolving what `git merge-tree` found
    // (`conflict_precheck` with `requested: true`).
    "conflict",
    // A person answering an ask of the run, or integrating it by hand after
    // a landing error (`ask_opened`, `review_failed`, `integration_error`).
    "ask",
    // Parked `needs_session` and resumed (an event whose `status` is
    // `needs_session`).
    "resume",
    // Waiting for the single integration slot while another run lands
    // (`landing_queued`, an `approve_landing` answer the runtime applies).
    "landing_queue",
    // Waiting for the e2e of another run before its own (`run_e2e_waiting`,
    // ADR-t1233-2).
    "e2e_wait",
    // The runtime's e2e on the host after the review (`run_e2e_started`,
    // up to its `run_e2e_finished`, after which the run waits for the slot
    // again; a `run_e2e_failed` parks it for `resume`).
    "e2e",
    // `integrate` up to its rebase: the receipt checks and the rebase
    // (`integration_started`).
    "rebase",
    // `integrate` after the rebase: the scope check, `verification_commands`
    // and the commit to main (`integration_rebased`).
    "verify",
];

const EXIT: usize = 0;
const REVIEW: usize = 1;
const REVISE: usize = 2;
const CONFLICT: usize = 3;
const ASK: usize = 4;
const RESUME: usize = 5;
const LANDING_QUEUE: usize = 6;
/// The `via` of the `landing_queue` an `approve_landing` answer starts.
const APPROVE_VIA: &str = "approve";
/// The `via` of a `landing_queued` recorded without one.
const UNKNOWN_VIA: &str = "unknown";
const E2E_WAIT: usize = 7;
const E2E: usize = 8;
const REBASE: usize = 9;
const VERIFY: usize = 10;

/// The seconds a run spent in each phase of its wait to land, and in the
/// push after it (null when no push was recorded).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LandPhases {
    pub secs: [i64; PHASES.len()],
    pub push: Option<i64>,
    /// The `verify` phase per verification command, by command (task 509):
    /// all the run's `integrate` attempts, failed commands included. Their
    /// sum does not exceed `verify`.
    pub verify_commands: Vec<CommandSecs>,
    /// The `landing_queue` phase per what queued the run (task 949): the
    /// `via` of its `landing_queued` (`exit`, `resume`, `approve`), or
    /// `approve` from an `approve_landing` answer the runtime applies;
    /// `unknown` for a `landing_queued` without one. Their sum does not
    /// exceed `landing_queue` (each is cut to the second).
    pub landing_queue_via: Vec<ViaSecs>,
}

/// One `via`'s share of a run's `landing_queue` phase.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ViaSecs {
    pub via: String,
    pub secs: i64,
}

/// One verification command's share of a run's `verify` phase.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CommandSecs {
    pub command: String,
    /// How many times it ran over the run's attempts.
    pub count: usize,
    pub secs: i64,
}

impl LandPhases {
    /// The phase the most time went to; `None` when none took any.
    pub fn longest(&self) -> Option<&'static str> {
        let (index, secs) = self
            .secs
            .iter()
            .enumerate()
            .max_by_key(|&(index, secs)| (*secs, std::cmp::Reverse(index)))?;
        (*secs > 0).then_some(PHASES[index])
    }
}

impl Serialize for LandPhases {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(PHASES.len() + 1))?;
        for (name, secs) in PHASES.iter().zip(self.secs) {
            map.serialize_entry(name, &secs)?;
        }
        map.serialize_entry("push", &self.push)?;
        map.serialize_entry("verify_commands", &self.verify_commands)?;
        map.serialize_entry("landing_queue_via", &self.landing_queue_via)?;
        map.end()
    }
}

/// One phase over a set of runs: `Summary`'s count, sum and median, the
/// 90th percentile and the maximum, and the sum over the tail runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PhaseSummary {
    #[serde(flatten)]
    pub summary: Summary,
    pub p90: Option<i64>,
    pub max: Option<i64>,
    /// The sum over the runs whose `wait_to_land` is at least
    /// [`LandBreakdown::tail_threshold`].
    pub tail_total: i64,
}

/// The phases over a set of runs, with the long tail of `wait_to_land`
/// (the runs at or above its 90th percentile) and what each phase
/// contributed to it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LandBreakdown {
    /// Runs with a breakdown (those that landed).
    pub runs: usize,
    /// The 90th percentile of those runs' `wait_to_land`.
    pub tail_threshold: Option<i64>,
    pub tail_runs: usize,
    pub phases: [PhaseSummary; PHASES.len()],
    pub push: PhaseSummary,
    /// The `verify` phase per verification command, by command: each over
    /// the runs that ran it, with a run's attempts added up.
    pub verify_commands: Vec<CommandSummary>,
    /// The `landing_queue` phase per `via` (task 949): each over the runs
    /// that waited so, with a run's waits added up.
    pub landing_queue_via: Vec<ViaSummary>,
}

/// One `via`'s share of `landing_queue` over a set of runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ViaSummary {
    pub via: String,
    #[serde(flatten)]
    pub phase: PhaseSummary,
}

/// One verification command's share of `verify` over a set of runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CommandSummary {
    pub command: String,
    #[serde(flatten)]
    pub phase: PhaseSummary,
}

impl Serialize for LandBreakdown {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(PHASES.len() + 6))?;
        map.serialize_entry("runs", &self.runs)?;
        map.serialize_entry("tail_threshold", &self.tail_threshold)?;
        map.serialize_entry("tail_runs", &self.tail_runs)?;
        for (name, phase) in PHASES.iter().zip(&self.phases) {
            map.serialize_entry(name, phase)?;
        }
        map.serialize_entry("push", &self.push)?;
        map.serialize_entry("verify_commands", &self.verify_commands)?;
        map.serialize_entry("landing_queue_via", &self.landing_queue_via)?;
        map.end()
    }
}

/// The nearest-rank 90th percentile of `values` (sorted in place).
pub fn p90(values: &mut [i64]) -> Option<i64> {
    values.sort_unstable();
    let rank = (values.len() * 9).div_ceil(10);
    values.get(rank.checked_sub(1)?).copied()
}

fn phase_summary(values: &[(i64, bool)]) -> PhaseSummary {
    let mut all: Vec<i64> = values.iter().map(|(secs, _)| *secs).collect();
    PhaseSummary {
        summary: Summary {
            count: all.len(),
            total: all.iter().sum(),
            median: median(&mut all),
        },
        p90: p90(&mut all),
        max: all.iter().copied().max(),
        tail_total: values
            .iter()
            .filter(|(_, tail)| *tail)
            .map(|(secs, _)| secs)
            .sum(),
    }
}

/// Aggregate the breakdowns of runs, each with its `wait_to_land`.
pub fn breakdown<'a>(runs: impl Iterator<Item = (&'a LandPhases, i64)>) -> LandBreakdown {
    let runs: Vec<(&LandPhases, i64)> = runs.collect();
    let tail_threshold = p90(&mut runs.iter().map(|(_, wait)| *wait).collect::<Vec<_>>());
    let tail = |wait: i64| tail_threshold.is_some_and(|threshold| wait >= threshold);
    LandBreakdown {
        runs: runs.len(),
        tail_threshold,
        tail_runs: runs.iter().filter(|(_, wait)| tail(*wait)).count(),
        phases: std::array::from_fn(|index| {
            phase_summary(
                &runs
                    .iter()
                    .map(|(phases, wait)| (phases.secs[index], tail(*wait)))
                    .collect::<Vec<_>>(),
            )
        }),
        push: phase_summary(
            &runs
                .iter()
                .filter_map(|(phases, wait)| Some((phases.push?, tail(*wait))))
                .collect::<Vec<_>>(),
        ),
        verify_commands: {
            let mut by_command: BTreeMap<&str, Vec<(i64, bool)>> = BTreeMap::new();
            for (phases, wait) in &runs {
                for command in &phases.verify_commands {
                    by_command
                        .entry(&command.command)
                        .or_default()
                        .push((command.secs, tail(*wait)));
                }
            }
            by_command
                .into_iter()
                .map(|(command, values)| CommandSummary {
                    command: command.to_owned(),
                    phase: phase_summary(&values),
                })
                .collect()
        },
        landing_queue_via: {
            let mut by_via: BTreeMap<&str, Vec<(i64, bool)>> = BTreeMap::new();
            for (phases, wait) in &runs {
                for via in &phases.landing_queue_via {
                    by_via
                        .entry(&via.via)
                        .or_default()
                        .push((via.secs, tail(*wait)));
                }
            }
            by_via
                .into_iter()
                .map(|(via, values)| ViaSummary {
                    via: via.to_owned(),
                    phase: phase_summary(&values),
                })
                .collect()
        },
    }
}

/// A run's wait to land as its events go by, from its first
/// `validation_finished` (unix milliseconds throughout).
#[derive(Debug, Clone)]
pub struct LandClock {
    phase: usize,
    since: i64,
    spent: [i64; PHASES.len()],
    /// The phase an open ask interrupted, to go back to once it is answered.
    before_ask: Option<usize>,
    /// The run's open asks, by `ask_id`, with their kind.
    asks: HashMap<String, Option<String>>,
    integrated: Option<i64>,
    push: Option<i64>,
    /// Per verification command: the times it ran and its milliseconds.
    commands: BTreeMap<String, (usize, i64)>,
    /// The last `verification_command` counted, where the next one's
    /// interval starts when it is still in the same `verify`.
    command_mark: i64,
    /// What queued the run in the current `landing_queue` phase.
    queue_via: String,
    /// Per `via`, the milliseconds in `landing_queue`.
    via_spent: BTreeMap<String, i64>,
}

fn ask_key(payload: &Value) -> Option<String> {
    payload
        .get("ask_id")
        .or_else(|| payload.get("id"))
        .map(|id| id.as_str().map_or_else(|| id.to_string(), str::to_owned))
}

impl LandClock {
    /// Start at the run's first `validation_finished`, in the phase that
    /// event starts (`resume` when it parked the run).
    pub fn start(event: &RunEvent, at: i64) -> Self {
        let mut clock = Self {
            phase: EXIT,
            since: at,
            spent: [0; PHASES.len()],
            before_ask: None,
            asks: HashMap::new(),
            integrated: None,
            push: None,
            commands: BTreeMap::new(),
            command_mark: at,
            queue_via: UNKNOWN_VIA.to_owned(),
            via_spent: BTreeMap::new(),
        };
        clock.observe(event, at);
        clock
    }

    /// Take the run's next event, recorded at `at`.
    pub fn observe(&mut self, event: &RunEvent, at: i64) {
        let kind = event.kind.as_str();
        if let Some(integrated) = self.integrated {
            if self.push.is_none()
                && matches!(kind, "push_finished" | "push_failed" | "push_skipped")
            {
                self.push = Some((at - integrated).max(0) / 1000);
            }
            return;
        }
        if kind == "verification_command" {
            self.command(&event.payload, at);
        }
        let next = if kind == "run_integrated" {
            self.enter(self.phase, at);
            self.integrated = Some(at);
            return;
        } else if payload_status(&event.payload) == Some("needs_session") {
            Some(RESUME)
        } else {
            match kind {
                "review_started" | "review_retried" => Some(REVIEW),
                "review_finished" | "validation_finished" => Some(EXIT),
                "review_failed" => Some(ASK),
                "revise_requested" => Some(REVISE),
                // The request could not be sent: the session is asked to exit.
                "revise_unsent" => Some(EXIT),
                "conflict_precheck" if event.payload["requested"] == true => Some(CONFLICT),
                "conflict_precheck" => Some(EXIT),
                "landing_queued" => Some(LANDING_QUEUE),
                "run_e2e_waiting" => Some(E2E_WAIT),
                "run_e2e_started" => Some(E2E),
                "run_e2e_finished" => Some(LANDING_QUEUE),
                // The landing gave the lease back: the run waits for a
                // person's `review and integrate`, not for the slot.
                "integration_error" | "integration_held" => Some(ASK),
                "integration_started" => Some(REBASE),
                "integration_rebased" => Some(VERIFY),
                // The observer's `blocked` and a planner's question are
                // about the run but do not hold it (like the timeline's gaps).
                "ask_opened"
                    if matches!(
                        event.payload["kind"].as_str(),
                        Some("blocked" | "planner_question")
                    ) =>
                {
                    None
                }
                "ask_opened" => {
                    let opened = event.payload.get("kind").and_then(Value::as_str);
                    if let Some(key) = ask_key(&event.payload) {
                        self.asks.insert(key, opened.map(str::to_owned));
                    }
                    if self.phase != ASK {
                        self.before_ask = Some(self.phase);
                    }
                    Some(ASK)
                }
                "ask_answered" => {
                    let opened = ask_key(&event.payload).and_then(|key| self.asks.remove(&key));
                    let answered = event
                        .payload
                        .get("kind")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .or(opened.flatten());
                    if answered.as_deref() == Some("approve_landing") {
                        // `land` waits for the slot; `send_back` and
                        // `cancel` record their own status next. Another
                        // answer is the inbox's to read: still a person's
                        // wait.
                        self.asks.clear();
                        self.before_ask = None;
                        Some(if event.payload["runtime_delivers"] == false {
                            ASK
                        } else {
                            LANDING_QUEUE
                        })
                    } else if self.asks.is_empty() {
                        self.before_ask.take()
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        // What queued the run, for the `landing_queue` it enters now.
        let via = match (kind, next) {
            ("landing_queued", _) => Some(
                event.payload["via"]
                    .as_str()
                    .unwrap_or(UNKNOWN_VIA)
                    .to_owned(),
            ),
            ("ask_answered", Some(LANDING_QUEUE)) => Some(APPROVE_VIA.to_owned()),
            _ => None,
        };
        if let Some(next) = next {
            if next != ASK && kind != "ask_answered" {
                self.asks.clear();
                self.before_ask = None;
            }
            self.enter(next, at);
            if let Some(via) = via {
                self.queue_via = via;
            }
        }
    }

    /// Count a `verification_command` of `integrate` recorded at `at`
    /// towards its command: its `duration_secs`, or without one (before
    /// task 197) the time since the `verify` phase began or the last
    /// command. Only commands in `verify` count, and none for more than
    /// that time, so the commands never add up to more than the phase.
    fn command(&mut self, payload: &Value, at: i64) {
        let integration = payload["phase"]
            .as_str()
            .is_none_or(|phase| phase == "integration");
        let Some(command) = payload["command"].as_str() else {
            return;
        };
        if self.phase != VERIFY || !integration {
            return;
        }
        let elapsed = (at - self.since.max(self.command_mark)).max(0);
        let ms = payload["duration_secs"].as_f64().map_or(elapsed, |secs| {
            ((secs * 1000.0).round() as i64).clamp(0, elapsed)
        });
        let entry = self.commands.entry(command.to_owned()).or_default();
        entry.0 += 1;
        entry.1 += ms;
        self.command_mark = at;
    }

    fn enter(&mut self, phase: usize, at: i64) {
        self.spend(at);
        self.since = at.max(self.since);
        self.phase = phase;
    }

    /// Count the time since the current phase began towards it, and
    /// towards its `via` in `landing_queue`.
    fn spend(&mut self, at: i64) {
        let ms = (at - self.since).max(0);
        self.spent[self.phase] += ms;
        if self.phase == LANDING_QUEUE {
            *self.via_spent.entry(self.queue_via.clone()).or_default() += ms;
        }
    }

    /// Whether `run_integrated` ended the wait.
    pub fn landed(&self) -> bool {
        self.integrated.is_some()
    }

    /// The seconds per phase: up to `run_integrated` for a run that landed,
    /// up to `now` for one still waiting.
    pub fn phases(&self, now: i64) -> LandPhases {
        let mut clock = self.clone();
        if clock.integrated.is_none() {
            clock.spend(now);
        }
        let (spent, via_spent) = (clock.spent, clock.via_spent);
        LandPhases {
            secs: spent.map(|ms| ms / 1000),
            push: self.push,
            verify_commands: self
                .commands
                .iter()
                .map(|(command, (count, ms))| CommandSecs {
                    command: command.clone(),
                    count: *count,
                    secs: ms / 1000,
                })
                .collect(),
            landing_queue_via: via_spent
                .into_iter()
                .map(|(via, ms)| ViaSecs {
                    via,
                    secs: ms / 1000,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EventId, TaskId};
    use serde_json::json;

    fn event(index: usize, kind: &str, payload: &Value) -> RunEvent {
        RunEvent {
            id: EventId::new(index as i64 + 1),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload: payload.clone(),
            created_at: String::new(),
            actor: None,
        }
    }

    /// Feed `(kind, payload, second)` to a clock started at second 0 by the
    /// first event when it is a `validation_finished`, by one that left the
    /// run awaiting integration otherwise, and return the phases, keyed by
    /// name, at second `now`.
    fn phases(events: &[(&str, Value, i64)], now: i64) -> (HashMap<&'static str, i64>, LandPhases) {
        let awaiting = (
            "validation_finished",
            json!({"status": "awaiting_integration"}),
            0,
        );
        let (first, rest) = match events.split_first() {
            Some((first, rest)) if first.0 == "validation_finished" => (first, rest),
            _ => (&awaiting, events),
        };
        let mut clock = LandClock::start(&event(0, first.0, &first.1), first.2 * 1000);
        for (index, (kind, payload, secs)) in rest.iter().enumerate() {
            clock.observe(&event(index + 1, kind, payload), secs * 1000);
        }
        let phases = clock.phases(now * 1000);
        (
            PHASES
                .iter()
                .zip(phases.secs)
                .filter(|(_, secs)| *secs > 0)
                .map(|(name, secs)| (*name, secs))
                .collect(),
            phases,
        )
    }

    fn map(pairs: &[(&'static str, i64)]) -> HashMap<&'static str, i64> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn a_passed_review_splits_into_review_exit_queue_integrate_and_push() {
        let (spent, phases) = phases(
            &[
                ("review_started", json!({"attempt": 1}), 2),
                ("review_finished", json!({"verdict": "pass"}), 62),
                ("exit_requested", json!({}), 63),
                ("landing_queued", json!({}), 70),
                ("integration_started", json!({}), 170),
                ("integration_rebased", json!({}), 175),
                ("verification_command", json!({"exit_code": 0}), 400),
                ("run_integrated", json!({}), 410),
                ("worktree_removed", json!({}), 411),
                ("push_finished", json!({}), 414),
            ],
            9999,
        );
        assert_eq!(
            spent,
            map(&[
                ("exit", 2 + 8),
                ("review", 60),
                ("landing_queue", 100),
                ("rebase", 5),
                ("verify", 235),
            ])
        );
        assert_eq!(phases.secs.iter().sum::<i64>(), 410);
        assert_eq!(phases.push, Some(4));
        assert_eq!(phases.longest(), Some("verify"));
    }

    /// The runtime's e2e after the review (ADR-t1233-2) is a phase of its
    /// own, apart from the wait for another run's e2e; a failed one parks
    /// the run, which counts as `resume`.
    #[test]
    fn the_e2e_after_the_review_has_its_own_phases() {
        let (spent, _) = phases(
            &[
                ("landing_queued", json!({"via": "exit"}), 10),
                ("run_e2e_waiting", json!({}), 12),
                ("run_e2e_started", json!({}), 40),
                ("run_e2e_finished", json!({"outcome": "passed"}), 340),
                ("integration_started", json!({}), 345),
                ("integration_rebased", json!({}), 350),
                ("run_integrated", json!({}), 400),
            ],
            9999,
        );
        assert_eq!(
            spent,
            map(&[
                ("exit", 10),
                ("landing_queue", 2 + 5),
                ("e2e_wait", 28),
                ("e2e", 300),
                ("rebase", 5),
                ("verify", 50),
            ])
        );
        let (spent, _) = phases(
            &[
                ("run_e2e_started", json!({}), 10),
                ("run_e2e_failed", json!({"status": "needs_session"}), 100),
            ],
            160,
        );
        assert_eq!(spent, map(&[("exit", 10), ("e2e", 90), ("resume", 60)]));
    }

    #[test]
    fn a_revise_counts_until_the_next_review() {
        let (spent, _) = phases(
            &[
                ("review_started", json!({}), 0),
                ("review_finished", json!({"verdict": "revise"}), 50),
                ("revise_requested", json!({"attempt": 1}), 51),
                ("revise_finished", json!({"attempt": 1}), 300),
                ("receipt_observed", json!({}), 301),
                (
                    "validation_finished",
                    json!({"status": "awaiting_integration"}),
                    311,
                ),
                ("review_started", json!({"attempt": 2}), 312),
                ("review_finished", json!({"verdict": "pass"}), 342),
                ("landing_queued", json!({}), 350),
                ("integration_started", json!({}), 350),
                ("integration_rebased", json!({}), 351),
                ("run_integrated", json!({}), 400),
            ],
            9999,
        );
        assert_eq!(
            spent,
            map(&[
                ("review", 80),
                ("exit", 1 + 1 + 8),
                ("revise", 260),
                ("rebase", 1),
                ("verify", 49),
            ])
        );
    }

    #[test]
    fn a_concern_waits_for_the_person_then_for_the_slot() {
        let (spent, phases) = phases(
            &[
                ("review_started", json!({}), 0),
                ("review_finished", json!({"verdict": "concern"}), 30),
                (
                    "ask_opened",
                    json!({"ask_id": 9, "kind": "approve_landing"}),
                    40,
                ),
                (
                    "ask_answered",
                    json!({"ask_id": 9, "kind": "approve_landing"}),
                    3640,
                ),
                ("integration_approved", json!({}), 3700),
                ("integration_started", json!({}), 3700),
                ("integration_rebased", json!({}), 3701),
                ("run_integrated", json!({}), 3800),
            ],
            9999,
        );
        assert_eq!(
            spent,
            map(&[
                ("review", 30),
                ("exit", 10),
                ("ask", 3600),
                ("landing_queue", 60),
                ("rebase", 1),
                ("verify", 99),
            ])
        );
        assert_eq!(phases.longest(), Some("ask"));
        assert_eq!(phases.push, None);
    }

    /// Task 949: the `landing_queue` phase split by what queued the run;
    /// an answer the runtime applies queues it as `approve`, whether or not
    /// it recorded its own `landing_queued`.
    #[test]
    fn the_landing_queue_splits_by_what_queued_the_run() {
        let via = |phases: &LandPhases| -> Vec<(String, i64)> {
            phases
                .landing_queue_via
                .iter()
                .map(|v| (v.via.clone(), v.secs))
                .collect()
        };
        let asked = |queued: Option<i64>| {
            let mut events = vec![
                (
                    "ask_opened",
                    json!({"ask_id": 9, "kind": "approve_landing"}),
                    10,
                ),
                (
                    "ask_answered",
                    json!({"ask_id": 9, "kind": "approve_landing"}),
                    100,
                ),
                ("integration_approved", json!({"ask_id": 9}), 101),
            ];
            if let Some(at) = queued {
                events.push(("landing_queued", json!({"via": "approve", "ask_id": 9}), at));
            }
            events.extend([
                ("integration_started", json!({}), 700),
                ("integration_rebased", json!({}), 701),
                ("run_integrated", json!({}), 800),
            ]);
            events
        };
        for queued in [None, Some(101)] {
            let (spent, phases) = phases(&asked(queued), 9999);
            assert_eq!(spent["landing_queue"], 600);
            assert_eq!(via(&phases), [("approve".to_owned(), 600)]);
        }
        // A resume's, then an exit's, then one without a `via`; a run
        // still waiting counts up to now.
        let (spent, phases) = phases(
            &[
                ("landing_queued", json!({"via": "resume"}), 0),
                ("integration_started", json!({}), 30),
                (
                    "integration_error",
                    json!({"status": "awaiting_integration"}),
                    40,
                ),
                ("landing_queued", json!({"via": "exit"}), 50),
                ("integration_started", json!({}), 60),
                (
                    "integration_error",
                    json!({"status": "awaiting_integration"}),
                    70,
                ),
                ("landing_queued", json!({}), 80),
            ],
            100,
        );
        assert_eq!(spent["landing_queue"], 30 + 10 + 20);
        assert_eq!(
            via(&phases),
            [
                ("exit".to_owned(), 10),
                ("resume".to_owned(), 30),
                ("unknown".to_owned(), 20),
            ]
        );
        let json = serde_json::to_value(&phases).unwrap();
        assert_eq!(
            json["landing_queue_via"][0],
            json!({"via": "exit", "secs": 10})
        );
        let result = breakdown([(&phases, 100)].into_iter());
        assert_eq!(
            serde_json::to_value(&result).unwrap()["landing_queue_via"][1],
            json!({"via": "resume", "count": 1, "total": 30, "median": 30, "p90": 30, "max": 30, "tail_total": 30})
        );
    }

    #[test]
    fn a_question_during_a_revise_returns_to_the_revise() {
        let (spent, _) = phases(
            &[
                ("revise_requested", json!({}), 0),
                (
                    "ask_opened",
                    json!({"ask_id": "a", "kind": "worker_question"}),
                    100,
                ),
                ("ask_opened", json!({"ask_id": "b", "kind": "stalled"}), 150),
                ("ask_answered", json!({"ask_id": "a"}), 200),
                ("ask_answered", json!({"ask_id": "b"}), 250),
                ("run_integrated", json!({}), 300),
            ],
            9999,
        );
        assert_eq!(spent, map(&[("revise", 150), ("ask", 150)]));
    }

    #[test]
    fn a_resume_and_a_retried_landing_are_their_own_phases() {
        let (spent, _) = phases(
            &[
                ("landing_queued", json!({}), 0),
                ("integration_started", json!({}), 10),
                (
                    "integration_deferred",
                    json!({"status": "needs_session", "code": "verification_failed"}),
                    20,
                ),
                ("resume_started", json!({}), 80),
                ("resume_finished", json!({"status": "validating"}), 500),
                (
                    "validation_finished",
                    json!({"status": "awaiting_integration"}),
                    505,
                ),
                ("landing_queued", json!({}), 510),
                ("integration_started", json!({}), 610),
                (
                    "integration_error",
                    json!({"status": "awaiting_integration"}),
                    615,
                ),
                ("integration_started", json!({}), 700),
                ("integration_rebased", json!({}), 720),
                ("run_integrated", json!({}), 900),
            ],
            9999,
        );
        assert_eq!(
            spent,
            map(&[
                ("landing_queue", 10 + 100),
                ("ask", 85),
                ("rebase", 10 + 5 + 20),
                ("resume", 485),
                ("exit", 5),
                ("verify", 180),
            ])
        );
    }

    #[test]
    fn a_first_validation_that_parks_the_run_starts_in_resume() {
        let (spent, _) = phases(
            &[
                ("validation_finished", json!({"status": "needs_session"}), 0),
                ("evidence_missing", json!({"checks": ["e2e"]}), 0),
                ("resume_started", json!({}), 60),
                ("resume_finished", json!({"status": "validating"}), 600),
                (
                    "validation_finished",
                    json!({"status": "awaiting_integration"}),
                    610,
                ),
                ("review_started", json!({}), 611),
                ("review_finished", json!({"verdict": "pass"}), 641),
                ("run_integrated", json!({}), 700),
            ],
            9999,
        );
        assert_eq!(
            spent,
            map(&[("resume", 610), ("exit", 1 + 59), ("review", 30)])
        );
    }

    #[test]
    fn asks_that_do_not_hold_the_run_or_go_to_the_inbox() {
        let (spent, _) = phases(
            &[
                ("landing_queued", json!({}), 0),
                ("ask_opened", json!({"ask_id": 1, "kind": "blocked"}), 10),
                ("ask_answered", json!({"ask_id": 1, "kind": "blocked"}), 20),
                (
                    "ask_opened",
                    json!({"ask_id": 2, "kind": "planner_question"}),
                    30,
                ),
                ("integration_started", json!({}), 40),
                ("integration_rebased", json!({}), 41),
                (
                    "integration_deferred",
                    json!({"status": "needs_session"}),
                    50,
                ),
                ("resume_started", json!({}), 51),
                (
                    "ask_opened",
                    json!({"ask_id": 3, "kind": "approve_landing"}),
                    100,
                ),
                (
                    "ask_answered",
                    json!({"ask_id": 3, "kind": "approve_landing", "runtime_delivers": false}),
                    200,
                ),
                ("integration_approved", json!({}), 300),
                ("integration_started", json!({}), 300),
                ("integration_rebased", json!({}), 301),
                ("run_integrated", json!({}), 310),
            ],
            9999,
        );
        assert_eq!(
            spent,
            map(&[
                ("landing_queue", 40),
                ("rebase", 2),
                ("verify", 9 + 9),
                ("resume", 50),
                ("ask", 200),
            ])
        );
    }

    #[test]
    fn a_conflict_precheck_that_asks_the_session_is_the_conflict_phase() {
        let (spent, _) = phases(
            &[
                ("review_started", json!({}), 0),
                ("review_finished", json!({"verdict": "pass"}), 10),
                ("conflict_precheck", json!({"requested": true}), 11),
                ("conflict_resolved", json!({}), 200),
                (
                    "validation_finished",
                    json!({"status": "awaiting_integration"}),
                    210,
                ),
                ("review_started", json!({}), 211),
                ("review_finished", json!({"verdict": "pass"}), 221),
                ("conflict_precheck", json!({"requested": false}), 221),
                ("run_integrated", json!({}), 230),
            ],
            9999,
        );
        assert_eq!(
            spent,
            map(&[("review", 20), ("exit", 1 + 1 + 9), ("conflict", 199)])
        );
    }

    #[test]
    fn a_failed_review_and_a_run_still_waiting_count_up_to_now() {
        let (spent, phases) = phases(
            &[
                ("review_started", json!({}), 0),
                ("ask_opened", json!({"ask_id": 3}), 40),
                ("review_failed", json!({"ask_id": 3}), 41),
            ],
            1000,
        );
        assert_eq!(spent, map(&[("review", 40), ("ask", 960)]));
        assert_eq!(phases.longest(), Some("ask"));
        assert_eq!(LandPhases::default().longest(), None);
    }

    /// The commands of `phases`' `verify_commands` as `(command, count,
    /// secs)`.
    fn commands(phases: &LandPhases) -> Vec<(&str, usize, i64)> {
        phases
            .verify_commands
            .iter()
            .map(|c| (c.command.as_str(), c.count, c.secs))
            .collect()
    }

    fn verified(command: &str, secs: Option<f64>, exit_code: i64) -> Value {
        json!({
            "phase": "integration", "command": command, "exit_code": exit_code,
            "duration_secs": secs,
        })
    }

    #[test]
    fn verify_splits_by_command_that_passed() {
        let (spent, phases) = phases(
            &[
                ("landing_queued", json!({}), 0),
                ("integration_started", json!({}), 10),
                ("integration_rebased", json!({}), 12),
                ("verification_command", verified("fmt", Some(3.2), 0), 15),
                (
                    "verification_command",
                    verified("clippy", Some(60.4), 0),
                    76,
                ),
                // Another phase's command is not the landing's.
                (
                    "verification_command",
                    json!({"phase": "recheck", "command": "llvm-cov", "duration_secs": 5.0}),
                    77,
                ),
                (
                    "verification_command",
                    verified("llvm-cov", Some(300.9), 0),
                    377,
                ),
                ("run_integrated", json!({}), 380),
            ],
            9999,
        );
        assert_eq!(spent["verify"], 368);
        assert_eq!(
            commands(&phases),
            [("clippy", 1, 60), ("fmt", 1, 3), ("llvm-cov", 1, 300)]
        );
        let json = serde_json::to_value(&phases).unwrap();
        assert_eq!(
            json["verify_commands"][0],
            json!({"command": "clippy", "count": 1, "secs": 60})
        );
    }

    #[test]
    fn verify_adds_up_every_attempt_and_its_failed_commands() {
        let (spent, phases) = phases(
            &[
                ("integration_started", json!({}), 0),
                ("integration_rebased", json!({}), 5),
                ("verification_command", verified("fmt", Some(2.0), 0), 7),
                (
                    "verification_command",
                    verified("llvm-cov", Some(200.0), 101),
                    208,
                ),
                (
                    "integration_deferred",
                    json!({"status": "needs_session", "code": "verification_failed"}),
                    209,
                ),
                ("resume_started", json!({}), 210),
                ("resume_finished", json!({"status": "validating"}), 600),
                (
                    "validation_finished",
                    json!({"status": "awaiting_integration"}),
                    605,
                ),
                ("integration_started", json!({}), 610),
                ("integration_rebased", json!({}), 612),
                ("verification_command", verified("fmt", Some(2.0), 0), 614),
                (
                    "verification_command",
                    verified("llvm-cov", Some(250.0), 0),
                    865,
                ),
                ("run_integrated", json!({}), 870),
            ],
            9999,
        );
        assert_eq!(spent["verify"], 204 + 258);
        assert_eq!(commands(&phases), [("fmt", 2, 4), ("llvm-cov", 2, 450)]);
        let sum: i64 = phases.verify_commands.iter().map(|c| c.secs).sum();
        assert!(sum <= phases.secs[VERIFY]);
    }

    #[test]
    fn verify_without_durations_takes_the_intervals_and_never_exceeds_the_phase() {
        let (spent, land) = phases(
            &[
                ("integration_started", json!({}), 0),
                ("integration_rebased", json!({}), 10),
                // Before task 197: no duration, and no phase either.
                (
                    "verification_command",
                    json!({"command": "fmt", "exit_code": 0}),
                    14,
                ),
                ("verification_command", verified("clippy", None, 0), 74),
                // A duration longer than the time since the last command is
                // cut to it.
                (
                    "verification_command",
                    verified("test", Some(9999.0), 0),
                    374,
                ),
                // A command without its name is not counted.
                ("verification_command", json!({"exit_code": 0}), 375),
                ("run_integrated", json!({}), 380),
            ],
            9999,
        );
        assert_eq!(spent["verify"], 370);
        assert_eq!(
            commands(&land),
            [("clippy", 1, 60), ("fmt", 1, 4), ("test", 1, 300)]
        );
        // A command outside `verify` (no rebase recorded) is not counted.
        let (_, outside) = phases(
            &[
                ("verification_command", verified("fmt", Some(1.0), 0), 3),
                ("run_integrated", json!({}), 10),
            ],
            9999,
        );
        assert!(outside.verify_commands.is_empty());
    }

    #[test]
    fn breakdowns_split_verify_by_command_over_the_runs_that_ran_it() {
        let run = |verify: i64, commands: &[(&str, i64)]| LandPhases {
            secs: std::array::from_fn(|index| if index == VERIFY { verify } else { 0 }),
            push: None,
            verify_commands: commands
                .iter()
                .map(|(command, secs)| CommandSecs {
                    command: (*command).to_owned(),
                    count: 1,
                    secs: *secs,
                })
                .collect(),
            landing_queue_via: Vec::new(),
        };
        let runs = [
            run(100, &[("fmt", 2), ("llvm-cov", 90)]),
            run(200, &[("fmt", 3), ("llvm-cov", 190)]),
            run(10, &[("fmt", 1)]),
        ];
        let result = breakdown(runs.iter().map(|phases| (phases, phases.secs[VERIFY])));
        assert_eq!(result.tail_threshold, Some(200));
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(
            json["verify_commands"],
            json!([
                {"command": "fmt", "count": 3, "total": 6, "median": 2, "p90": 3, "max": 3, "tail_total": 3},
                {"command": "llvm-cov", "count": 2, "total": 280, "median": 140, "p90": 190, "max": 190, "tail_total": 190},
            ])
        );
        assert_eq!(json["verify"]["total"], 310);
    }

    #[test]
    fn breakdowns_sum_and_mark_the_tail() {
        let run = |exit: i64, verify: i64, push: Option<i64>| LandPhases {
            secs: std::array::from_fn(|index| match index {
                EXIT => exit,
                VERIFY => verify,
                _ => 0,
            }),
            push,
            verify_commands: Vec::new(),
            landing_queue_via: Vec::new(),
        };
        let runs: Vec<LandPhases> = (1..=10)
            .map(|n| {
                run(
                    n,
                    if n == 10 { 1000 } else { 10 },
                    (n % 2 == 0).then_some(n),
                )
            })
            .collect();
        let result = breakdown(runs.iter().map(|phases| (phases, phases.secs.iter().sum())));
        assert_eq!(result.runs, 10);
        assert_eq!(result.tail_threshold, Some(19));
        assert_eq!(result.tail_runs, 2);
        let verify = &result.phases[VERIFY];
        assert_eq!(verify.summary.total, 1090);
        assert_eq!(verify.summary.median, Some(10));
        assert_eq!((verify.p90, verify.max), (Some(10), Some(1000)));
        assert_eq!(verify.tail_total, 1010);
        assert_eq!(result.push.summary.count, 5);
        assert_eq!(result.push.tail_total, 10);
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["verify"]["count"], 10);
        assert_eq!(json["verify"]["p90"], 10);
        assert_eq!(json["tail_runs"], 2);
        assert_eq!(json["review"]["median"], 0);
        assert_eq!(serde_json::to_value(&runs[0]).unwrap()["push"], Value::Null);
        assert_eq!(breakdown(std::iter::empty()).tail_threshold, None);
        assert_eq!(p90(&mut []), None);
    }
}
