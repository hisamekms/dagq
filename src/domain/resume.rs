//! How the resumes of a `needs_session` run are counted, and when a run
//! whose resumes are used up is retried with its branch carried over
//! (ADR-0047 decision 24). Both are read from the run's events alone.

use super::{
    CommitSha, MAX_RESUME_ATTEMPTS, ReasonCode, ReviewDecision, RunEvent, event_kind, recheck,
};

/// How many conflict-only attempts of one run the supervisor makes: the
/// resumes it starts while the run was parked only by a rebase conflict
/// after its review passed, and the conflict precheck's requests to the
/// live session of a passed run (ADR-0027 decision 4, task 511). Neither is
/// one of [`MAX_RESUME_ATTEMPTS`], but a conflict that never resolves stops
/// here (ADR-0047 decision 24's fence). The default of
/// [`ResumeConfig::conflict_only_limit`], which `[resume]` of `dagq.toml`
/// sets.
pub const CONFLICT_ONLY_RESUME_LIMIT: usize = 5;

/// `[resume]` of `dagq.toml` (ADR-0047 decision 24): the limit of the
/// conflict-only attempts of one run. The counted resumes'
/// [`MAX_RESUME_ATTEMPTS`] is not configurable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeConfig {
    /// The conflict-only attempts one run makes at most.
    pub conflict_only_limit: usize,
}

impl ResumeConfig {
    /// The keys `[resume]` may set.
    pub const KEYS: [&'static str; 1] = ["conflict_only_limit"];
}

impl Default for ResumeConfig {
    fn default() -> Self {
        Self {
            conflict_only_limit: CONFLICT_ONLY_RESUME_LIMIT,
        }
    }
}

/// `triage_finished`'s `action` for the automatic retry that carries the
/// run's branch over to the next run (ADR-0047 decisions 24 and 40).
pub const RETRY_INHERIT: &str = "retry_inherit";

/// The events that park a run for a session, or decide what it resumes
/// with: the latest of them before a resume is why it was resumed.
const PARKING: [&str; 7] = [
    "integration_deferred",
    "integration_error",
    "evidence_missing",
    "scope_violation",
    "landing_decided",
    "triage_finished",
    "triage_decided",
];

/// What the events so far say about the run: whether its landing was
/// approved (`integration_approved`, an `integrate` call or a person's
/// `land`), whether its latest review passed, whether the latest parking
/// event is a landing deferred for a rebase conflict or a landing recheck
/// that found one, and whether it is that recheck (ADR-0068 decision 5).
#[derive(Debug, Clone, Copy, Default)]
struct History {
    approved: bool,
    passed: bool,
    conflict: bool,
    rechecked: bool,
}

impl History {
    fn see(&mut self, event: &RunEvent) {
        match event.kind.as_str() {
            "integration_approved" => self.approved = true,
            "review_finished" => {
                self.passed = event.payload["verdict"] == ReviewDecision::Pass.as_str();
            }
            kind if PARKING.contains(&kind) || recheck::parks(event) => {
                self.conflict = matches!(
                    kind,
                    "integration_deferred" | recheck::LANDING_RECHECK_FAILED
                ) && event.payload["code"] == ReasonCode::RebaseConflict.as_str();
                self.rechecked = kind == recheck::LANDING_RECHECK_FAILED;
            }
            _ => {}
        }
    }

    /// The run waits for a session only because it conflicts with main,
    /// found by the landing's rebase after its review passed (or its
    /// landing was approved), or by the landing recheck while it waited
    /// (ADR-0068 decision 5: the wait, not the run, made the conflict,
    /// whatever its review said): not a failed verification or recheck
    /// command, `evidence_missing`, `scope_violation`, a person's
    /// `send_back` or a triage.
    fn conflict_only(self) -> bool {
        self.conflict && (self.approved || self.passed || self.rechecked)
    }

    /// [`Self::conflict_only`] with a review that passed or an approved
    /// landing: what the automatic retry with the branch carried over asks.
    fn reviewed_conflict(self) -> bool {
        self.conflict && (self.approved || self.passed)
    }
}

fn history(events: &[RunEvent]) -> History {
    let mut history = History::default();
    for event in events {
        history.see(event);
    }
    history
}

/// Whether the run, as its events stand, waits for a session only because
/// of a rebase conflict after its review passed: its next resume is not
/// counted toward [`MAX_RESUME_ATTEMPTS`].
pub fn parked_for_conflict_only(events: &[RunEvent]) -> bool {
    history(events).conflict_only()
}

/// Why a run parked only by a conflict counts as such
/// ([`parked_for_conflict_only`]): its latest review passed, its landing was
/// approved, or the landing recheck found the conflict. `None` when it is
/// not parked only by a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictOnly {
    pub passed: bool,
    pub approved: bool,
    pub rechecked: bool,
}

/// [`parked_for_conflict_only`] with what made it so.
pub fn conflict_only_basis(events: &[RunEvent]) -> Option<ConflictOnly> {
    let history = history(events);
    history.conflict_only().then_some(ConflictOnly {
        passed: history.passed,
        approved: history.approved,
        rechecked: history.rechecked,
    })
}

/// Whether the latest event that parked the run for a session is the
/// landing recheck's (ADR-0068 decision 3).
pub fn parked_by_recheck(events: &[RunEvent]) -> bool {
    history(events).rechecked
}

/// The resumes of one run (`resume_started` events), split by whether
/// each counts toward [`MAX_RESUME_ATTEMPTS`], and the conflict precheck's
/// requests to its live session, which share the conflict-only limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResumeCount {
    /// Resumes that count toward [`MAX_RESUME_ATTEMPTS`].
    pub counted: usize,
    /// Resumes of a run parked only by a rebase conflict after its review
    /// passed, fenced by [`ResumeConfig::conflict_only_limit`].
    pub conflict_only: usize,
    /// The conflict precheck's resolution requests sent to the live
    /// session of a passed run (`conflict_precheck` with `requested: true`,
    /// but those a later one with `unsent: true` withdrew): conflicts only
    /// after a passed review, fenced with [`Self::conflict_only`] by
    /// [`ResumeConfig::conflict_only_limit`].
    pub conflict_requests: usize,
    /// Whether the run, as its events end, is parked only by a conflict
    /// ([`parked_for_conflict_only`]): its next resume would be a
    /// conflict-only one, which the precheck's requests fence too.
    pub parked_for_conflict: bool,
}

impl ResumeCount {
    pub fn of(events: &[RunEvent]) -> Self {
        let mut history = History::default();
        let mut count = Self::default();
        let mut withdrawn = 0;
        for event in events {
            if event.kind == "resume_started" {
                if history.conflict_only() {
                    count.conflict_only += 1;
                } else {
                    count.counted += 1;
                }
            } else if event.kind == event_kind::CONFLICT_PRECHECK {
                if event.payload["requested"] == true {
                    count.conflict_requests += 1;
                }
                if event.payload["unsent"] == true {
                    withdrawn += 1;
                }
            }
            history.see(event);
        }
        count.conflict_requests = count.conflict_requests.saturating_sub(withdrawn);
        count.parked_for_conflict = history.conflict_only();
        count
    }

    /// The attempts fenced by [`ResumeConfig::conflict_only_limit`]: the
    /// conflict-only resumes and the precheck's conflict requests.
    pub const fn conflict_attempts(self) -> usize {
        self.conflict_only + self.conflict_requests
    }

    /// Every resume started, counted or not: the number of the last one.
    pub const fn total(self) -> usize {
        self.counted + self.conflict_only
    }

    /// How many counted resumes are left.
    pub const fn left(self) -> usize {
        MAX_RESUME_ATTEMPTS.saturating_sub(self.counted)
    }

    /// No further resume starts: the counted ones reached
    /// [`MAX_RESUME_ATTEMPTS`], the conflict-only resumes
    /// `config`'s [`ResumeConfig::conflict_only_limit`], or, while the run is parked only by a
    /// conflict, the conflict-only attempts with the precheck's requests
    /// ([`Self::conflict_attempts`]) reached it. A run the requests brought
    /// to the limit that is parked for another reason (a failed
    /// verification, missing evidence) keeps its counted resumes.
    pub const fn exhausted(self, config: ResumeConfig) -> bool {
        let limit = config.conflict_only_limit;
        self.counted >= MAX_RESUME_ATTEMPTS
            || self.conflict_only >= limit
            || (self.parked_for_conflict && self.conflict_attempts() >= limit)
    }
}

/// How a run whose resumes are used up ends (ADR-0047 decision 24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exhaustion {
    /// The run becomes `failed` and its `resume_exhausted` alert goes to
    /// the recovery job (ADR-0047 decision 39).
    Recover,
    /// The task is ready again, and its next run carries this run's
    /// `branch` over from `head` (its review passed; only conflicts kept
    /// it from landing).
    Inherit {
        branch: Option<String>,
        head: CommitSha,
    },
}

/// Whether the event is the automatic retry that carried a run's branch
/// over.
pub fn is_inherit_retry(event: &RunEvent) -> bool {
    event.kind == "triage_finished" && event.payload["action"] == RETRY_INHERIT
}

/// Whether a run whose resumes are used up is retried with its branch
/// carried over, without a person (ADR-0047 decision 24): its review
/// passed and the last reason it waited for a session is a rebase conflict
/// only, and no run of its task (`task_events`) was retried that way
/// before. Otherwise a person decides.
pub fn inherits_on_exhaustion(run_events: &[RunEvent], task_events: &[RunEvent]) -> bool {
    history(run_events).reviewed_conflict() && !task_events.iter().any(is_inherit_retry)
}

/// Whether the run was ended by the automatic retry that carries its
/// branch over: its latest `triage_finished` / `triage_decided` is one.
/// The next run of its task inherits it.
pub fn retried_with_inheritance(run_events: &[RunEvent]) -> bool {
    run_events
        .iter()
        .rev()
        .find(|e| matches!(e.kind.as_str(), "triage_finished" | "triage_decided"))
        .is_some_and(is_inherit_retry)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::domain::EventId;

    fn event(kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(0),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.to_owned(),
            payload,
            created_at: String::new(),
        }
    }

    fn conflict() -> RunEvent {
        event(
            "integration_deferred",
            json!({"code": "rebase_conflict", "conflicts": ["src/lib.rs"]}),
        )
    }

    fn resume() -> RunEvent {
        event("resume_started", json!({}))
    }

    fn pass() -> RunEvent {
        event("review_finished", json!({"verdict": "pass"}))
    }

    #[test]
    fn a_conflict_after_a_passed_review_is_not_counted() {
        let events = [pass(), conflict(), resume(), conflict(), resume()];
        let count = ResumeCount::of(&events);
        assert_eq!(
            count,
            ResumeCount {
                counted: 0,
                conflict_only: 2,
                conflict_requests: 0,
                parked_for_conflict: true,
            }
        );
        assert_eq!(count.total(), 2);
        assert_eq!(count.left(), MAX_RESUME_ATTEMPTS);
        assert!(!count.exhausted(ResumeConfig::default()));
        assert!(parked_for_conflict_only(&events));
        assert_eq!(
            conflict_only_basis(&events),
            Some(ConflictOnly {
                passed: true,
                approved: false,
                rechecked: false,
            })
        );
    }

    #[test]
    fn an_approved_landing_counts_as_a_passed_review() {
        let events = [
            event("integration_approved", json!({})),
            conflict(),
            resume(),
        ];
        assert_eq!(ResumeCount::of(&events).conflict_only, 1);
    }

    #[test]
    fn a_conflict_without_a_passed_review_is_counted() {
        let events = [conflict(), resume()];
        assert_eq!(ResumeCount::of(&events).counted, 1);
        // A later review that did not pass takes the pass back.
        let events = [
            pass(),
            event("review_finished", json!({"verdict": "revise"})),
            conflict(),
            resume(),
        ];
        assert_eq!(ResumeCount::of(&events).counted, 1);
        assert!(!parked_for_conflict_only(&events));
        assert_eq!(conflict_only_basis(&events), None);
    }

    #[test]
    fn other_reasons_after_a_passed_review_are_counted() {
        for parking in [
            event(
                "integration_deferred",
                json!({"code": "verification_failed"}),
            ),
            event("integration_error", json!({"code": "rebase_conflict"})),
            event("evidence_missing", json!({})),
            event("scope_violation", json!({})),
            event("landing_decided", json!({"answer": "send_back"})),
            event("triage_finished", json!({"action": "resume"})),
            event("triage_decided", json!({"answer": "resume"})),
        ] {
            let events = [pass(), conflict(), parking.clone(), resume()];
            assert_eq!(ResumeCount::of(&events).counted, 1, "{}", parking.kind);
        }
    }

    #[test]
    fn a_resume_is_judged_by_what_parked_it_before_it() {
        // A conflict recorded after the resume started does not change it.
        let events = [
            pass(),
            event("evidence_missing", json!({})),
            resume(),
            conflict(),
            resume(),
        ];
        assert_eq!(
            ResumeCount::of(&events),
            ResumeCount {
                counted: 1,
                conflict_only: 1,
                conflict_requests: 0,
                parked_for_conflict: true,
            }
        );
    }

    #[test]
    fn either_limit_uses_the_resumes_up() {
        let counted = ResumeCount {
            counted: MAX_RESUME_ATTEMPTS,
            conflict_only: 0,
            conflict_requests: 0,
            ..Default::default()
        };
        assert!(counted.exhausted(ResumeConfig::default()));
        assert_eq!(counted.left(), 0);
        let conflicts = ResumeCount {
            counted: 0,
            conflict_only: CONFLICT_ONLY_RESUME_LIMIT,
            conflict_requests: 0,
            ..Default::default()
        };
        assert!(conflicts.exhausted(ResumeConfig::default()));
        assert!(
            !ResumeCount {
                counted: MAX_RESUME_ATTEMPTS - 1,
                conflict_only: CONFLICT_ONLY_RESUME_LIMIT - 1,
                conflict_requests: 0,
                ..Default::default()
            }
            .exhausted(ResumeConfig::default())
        );
    }

    #[test]
    fn the_conflict_only_limit_is_the_configured_one() {
        let two = ResumeConfig {
            conflict_only_limit: 2,
        };
        let one_conflict = ResumeCount {
            conflict_only: 1,
            ..Default::default()
        };
        assert!(!one_conflict.exhausted(two));
        let two_conflicts = ResumeCount {
            conflict_only: 2,
            ..Default::default()
        };
        assert!(two_conflicts.exhausted(two));
        assert!(!two_conflicts.exhausted(ResumeConfig::default()));
        // The precheck's requests count toward it while parked for a
        // conflict; the counted resumes' limit stays.
        let requests = ResumeCount {
            conflict_only: 1,
            conflict_requests: 1,
            parked_for_conflict: true,
            ..Default::default()
        };
        assert!(requests.exhausted(two));
        let counted = ResumeCount {
            counted: MAX_RESUME_ATTEMPTS,
            ..Default::default()
        };
        assert!(counted.exhausted(ResumeConfig {
            conflict_only_limit: 100,
        }));
        assert_eq!(
            ResumeConfig::default().conflict_only_limit,
            CONFLICT_ONLY_RESUME_LIMIT
        );
    }

    #[test]
    fn precheck_requests_share_the_conflict_only_limit() {
        let precheck = |payload| event("conflict_precheck", payload);
        let mut events = vec![
            pass(),
            conflict(),
            resume(),
            precheck(json!({"requested": true})),
            precheck(json!({"requested": false})),
            precheck(json!({"requested": true})),
            // Withdrawn before it was typed.
            precheck(json!({"requested": false, "unsent": true})),
        ];
        let count = ResumeCount::of(&events);
        assert_eq!(
            count,
            ResumeCount {
                counted: 0,
                conflict_only: 1,
                conflict_requests: 1,
                parked_for_conflict: true,
            }
        );
        assert_eq!(count.total(), 1);
        assert_eq!(count.conflict_attempts(), 2);
        assert_eq!(count.left(), MAX_RESUME_ATTEMPTS);
        events.extend(
            std::iter::repeat_with(|| precheck(json!({"requested": true})))
                .take(CONFLICT_ONLY_RESUME_LIMIT - 2),
        );
        let count = ResumeCount::of(&events);
        assert_eq!(count.conflict_attempts(), CONFLICT_ONLY_RESUME_LIMIT);
        assert_eq!(count.counted, 0);
        assert!(count.exhausted(ResumeConfig::default()));
        // A conflict found by the landing after the requests is retried
        // with the branch carried over.
        events.push(conflict());
        assert!(inherits_on_exhaustion(&events, &events));
        // Parked for another reason, the run keeps its counted resumes: the
        // requests fence conflict-only attempts only.
        let mut failed = events.clone();
        failed.push(event(
            "integration_deferred",
            json!({"code": "verification_failed"}),
        ));
        let count = ResumeCount::of(&failed);
        assert!(!count.parked_for_conflict);
        assert_eq!(count.conflict_attempts(), CONFLICT_ONLY_RESUME_LIMIT);
        assert!(!count.exhausted(ResumeConfig::default()));
        failed.push(resume());
        assert_eq!(ResumeCount::of(&failed).counted, 1);
    }

    #[test]
    fn a_used_up_conflict_run_inherits_once_per_task() {
        let run = [pass(), conflict(), resume()];
        assert!(inherits_on_exhaustion(&run, &run));
        let inherited = event("triage_finished", json!({"action": RETRY_INHERIT}));
        let task: Vec<RunEvent> = run.iter().cloned().chain([inherited.clone()]).collect();
        assert!(!inherits_on_exhaustion(&run, &task));
        // Not after a reason other than the conflict.
        let failed = [pass(), conflict(), event("evidence_missing", json!({}))];
        assert!(!inherits_on_exhaustion(&failed, &failed));
        assert!(retried_with_inheritance(std::slice::from_ref(&inherited)));
        assert!(!retried_with_inheritance(&[
            inherited,
            event("triage_decided", json!({"answer": "retry"}))
        ]));
        assert!(!retried_with_inheritance(&run));
    }

    fn recheck(code: &str, action: &str) -> RunEvent {
        event(
            recheck::LANDING_RECHECK_FAILED,
            json!({"code": code, "action": action}),
        )
    }

    #[test]
    fn a_conflict_the_landing_recheck_found_is_not_counted_whatever_the_review() {
        let concern = event("review_finished", json!({"verdict": "concern"}));
        let events = [
            concern.clone(),
            recheck("rebase_conflict", recheck::RESUMED),
            resume(),
        ];
        assert_eq!(
            ResumeCount::of(&events),
            ResumeCount {
                counted: 0,
                conflict_only: 1,
                conflict_requests: 0,
                parked_for_conflict: true,
            }
        );
        // The retry that carries the branch over still needs a review that
        // passed.
        assert!(!inherits_on_exhaustion(&events, &events));
        let passed = [pass(), recheck("rebase_conflict", recheck::RESUMED)];
        assert!(inherits_on_exhaustion(&passed, &passed));
        assert!(parked_by_recheck(&passed));
        assert!(!parked_by_recheck(&[pass(), conflict()]));
        // A failed recheck command is counted, and a held failure parks
        // nothing.
        let failed = [
            pass(),
            recheck("verification_failed", recheck::RESUMED),
            resume(),
        ];
        assert_eq!(ResumeCount::of(&failed).counted, 1);
        let held = [concern, recheck("rebase_conflict", recheck::HELD), resume()];
        assert_eq!(ResumeCount::of(&held).counted, 1);
    }
}
