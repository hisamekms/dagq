//! Admission of new work from the supervisor's current observations.

use super::{
    Priority,
    claim_hold::{ClaimHold, HoldInputs, HoldReason},
    claim_spacing,
    disk::{ClaimsWaitReason, DiskDecision, DiskRun, DiskStage, DiskUsage},
    slot_order::{SlotCandidate, SlotKind, slot_order},
};

#[derive(Clone, Copy, Debug, Default)]
pub struct Gates {
    pub ci: bool,
    pub env: bool,
    pub branch: bool,
}

impl Gates {
    fn blocked(self) -> Option<Blocked> {
        if self.branch {
            Some(Blocked::LandingBranch)
        } else if self.env {
            Some(Blocked::Environment)
        } else if self.ci {
            Some(Blocked::Ci)
        } else {
            None
        }
    }
}

/// Stops that keep a run from advancing to its next step.
#[derive(Clone, Copy, Debug, Default)]
pub struct Progress {
    pub held: bool,
    pub unanswered: bool,
    pub gates: Gates,
}

impl Progress {
    pub fn can_progress(self) -> bool {
        self.blocked().is_none()
    }

    fn blocked(self) -> Option<Blocked> {
        if self.held {
            Some(Blocked::TaskHold)
        } else if self.unanswered {
            Some(Blocked::Answer)
        } else {
            self.gates.blocked()
        }
    }

    /// Count moving and reserved bytes only for runs that can progress.
    /// An in-flight step still finishes under its own lifecycle rules.
    pub fn disk_usage(self, usage: DiskUsage) -> DiskUsage {
        if self.can_progress() {
            usage
        } else {
            DiskUsage {
                stage: DiskStage::Waiting,
                active: false,
            }
        }
    }

    /// Measure stopped runs at their actual size, with a waiting-stage budget.
    pub fn disk_run(self, usage: DiskUsage, current: Option<u64>) -> DiskRun {
        DiskRun {
            stage: self.disk_usage(usage).stage,
            current,
        }
    }
}

/// Whether a run contributes to the disk snapshot's landing wait count.
pub fn landing_waiting(
    container_held: bool,
    candidates: impl IntoIterator<Item = Progress>,
) -> bool {
    !container_held && candidates.into_iter().any(Progress::can_progress)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Landing { needs_worker: bool },
    Slot(SlotKind),
}

#[derive(Clone, Debug)]
pub struct Candidate<T> {
    pub item: T,
    pub kind: Kind,
    pub priority: Priority,
    pub progress: Progress,
}

impl<T> Candidate<T> {
    pub fn slot(kind: SlotKind, priority: Priority, item: T) -> Self {
        Self {
            item,
            kind: Kind::Slot(kind),
            priority,
            progress: Progress::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocked {
    TaskHold,
    Answer,
    Ci,
    Environment,
    LandingBranch,
    WorkerSlot,
    /// An eligible returning run takes precedence over a fresh claim.
    Returning,
    Disk(Option<ClaimsWaitReason>),
    Load,
    Authentication,
    ClaimSpacing,
    NoCandidates,
    CandidatesDeferred,
    NoProvider,
    ProvisioningFailed,
    Draining,
    Handoff,
    RepositoryConfig,
    QueueService,
    LandingHeld,
    LandingInProgress,
    LandingTurn,
    /// This pass's disk budget can admit only one of the competing starts.
    DiskCompetition,
}

impl Blocked {
    /// Stable names in the claim observation records. Disk ordering waits
    /// keep their specific reason as the name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Disk(Some(ClaimsWaitReason::NoSamples)) => "NoSamples",
            Self::Disk(None) => "Disk",
            Self::TaskHold => "TaskHold",
            Self::Answer => "Answer",
            Self::Ci => "Ci",
            Self::Environment => "Environment",
            Self::LandingBranch => "LandingBranch",
            Self::WorkerSlot => "WorkerSlot",
            Self::Returning => "Returning",
            Self::Load => "Load",
            Self::Authentication => "Authentication",
            Self::ClaimSpacing => "ClaimSpacing",
            Self::NoCandidates => "NoCandidates",
            Self::CandidatesDeferred => "CandidatesDeferred",
            Self::NoProvider => "NoProvider",
            Self::ProvisioningFailed => "ProvisioningFailed",
            Self::Draining => "Draining",
            Self::Handoff => "Handoff",
            Self::RepositoryConfig => "RepositoryConfig",
            Self::QueueService => "QueueService",
            Self::LandingHeld => "LandingHeld",
            Self::LandingInProgress => "LandingInProgress",
            Self::LandingTurn => "LandingTurn",
            Self::DiskCompetition => "DiskCompetition",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Resources {
    pub free_slots: usize,
    pub gates: Gates,
    pub repository_config_missing: bool,
    pub service_down: bool,
    pub claim_hold: Option<Blocked>,
    pub disk_claims: Option<usize>,
    /// The reason for a zero claim budget when it comes from ordering.
    pub disk_wait: Option<Blocked>,
    /// Claim capacity after starting the queue's selected landing. No outer
    /// value means there is no competing disk reservation in the candidates.
    pub landing_disk: Option<LandingDisk>,
    pub spacing_secs: Option<usize>,
    pub last_claim_ms: Option<i64>,
    pub now_ms: i64,
    pub container_held: bool,
    pub integrating: bool,
    pub landing_turn_busy: bool,
}

/// The disk snapshot's claim capacity when the selected landing starts.
#[derive(Clone, Copy, Debug)]
pub struct LandingDisk {
    /// None means the worker budget imposes no numeric limit.
    pub claims: Option<usize>,
}

/// The existing hold record and its admission reason share the load and
/// authentication inputs. Disk ordering waits come from the disk budget.
pub fn claim_hold(
    disk: Option<ClaimHold>,
    authentication: &HoldInputs,
    load: &HoldInputs,
) -> Option<ClaimHold> {
    ClaimHold::judge(authentication)
        .or(disk)
        .or_else(|| ClaimHold::judge(load))
}

pub fn hold_reason(reason: HoldReason) -> Blocked {
    match reason {
        HoldReason::Authentication | HoldReason::UsageLimit => Blocked::Authentication,
        HoldReason::DiskSpace => Blocked::Disk(None),
        HoldReason::LoadAverage => Blocked::Load,
        HoldReason::RepositoryConfigUnavailable => Blocked::RepositoryConfig,
    }
}

/// Probe one potential slot start before reading its candidate list.
pub fn probe(kind: SlotKind, resources: Resources) -> Option<Blocked> {
    admit(vec![Candidate::slot(kind, Priority::Normal, ())], resources)
        .blocked
        .first()
        .map(|(_, reason)| *reason)
}

/// Apply the worker line's return precedence when a claim group rereads
/// the ready order before a transaction.
pub fn claim_waits_for_return(priority: Priority, highest_return: Option<Priority>) -> bool {
    highest_return.is_some_and(|returning| returning >= priority)
}

/// Capacity for the existing slots-full observations, before other holds.
pub fn claim_room(resources: Resources) -> super::light_slots::ClaimRoom {
    use super::light_slots::ClaimRoom;
    match probe(SlotKind::Claim, resources) {
        Some(_) => ClaimRoom::None,
        None => ClaimRoom::Any,
    }
}

#[derive(Debug)]
pub struct Decision<T> {
    pub starts: Vec<Candidate<T>>,
    pub blocked: Vec<(Candidate<T>, Blocked)>,
}

impl<T> Decision<T> {
    pub fn starts_work(&self) -> bool {
        !self.starts.is_empty()
    }
}

impl Resources {
    /// Keep the disk calculation's claim budget and named ordering wait.
    pub fn with_disk(mut self, decision: &DiskDecision) -> Self {
        self.disk_claims = decision.claims;
        self.disk_wait = decision
            .needs
            .claims_wait_reason
            .map(|reason| Blocked::Disk(Some(reason)));
        self
    }

    /// Retain the chosen landing's reservation for the rest of this pass.
    pub fn after_landing(mut self) -> Self {
        if let Some(landing) = self.landing_disk.take() {
            self.disk_claims = landing.claims;
            if landing.claims == Some(0) {
                self.disk_wait.get_or_insert(Blocked::DiskCompetition);
            } else {
                self.disk_wait = None;
            }
        }
        self
    }

    fn claim_block(self) -> Option<Blocked> {
        self.gates.blocked().or_else(|| {
            if self.repository_config_missing {
                Some(Blocked::RepositoryConfig)
            } else if self.service_down {
                Some(Blocked::QueueService)
            } else if let Some(hold) = self.claim_hold {
                Some(hold)
            } else if self.free_slots == 0 {
                Some(Blocked::WorkerSlot)
            } else if self.disk_claims == Some(0) {
                Some(self.disk_wait.unwrap_or(Blocked::Disk(None)))
            } else if claim_spacing::waits(
                claim_spacing::next_claim_ms(self.spacing_secs, self.last_claim_ms),
                self.now_ms,
            ) {
                Some(Blocked::ClaimSpacing)
            } else {
                None
            }
        })
    }

    fn blocked(self, kind: Kind) -> Option<Blocked> {
        match kind {
            Kind::Slot(SlotKind::Claim) => self.claim_block(),
            Kind::Slot(SlotKind::Resume) => self.gates.blocked().or(self.worker_block()),
            Kind::Slot(SlotKind::Recovery) if self.gates.branch => Some(Blocked::LandingBranch),
            Kind::Slot(SlotKind::Recovery | SlotKind::Return { .. }) => self.worker_block(),
            Kind::Landing { .. } => self.gates.blocked().or({
                if self.container_held {
                    Some(Blocked::LandingHeld)
                } else if self.integrating {
                    Some(Blocked::LandingInProgress)
                } else if self.landing_turn_busy {
                    Some(Blocked::LandingTurn)
                } else {
                    None
                }
            }),
        }
    }

    fn worker_block(self) -> Option<Blocked> {
        (self.free_slots == 0).then_some(Blocked::WorkerSlot)
    }
}

/// Admit the landing selected by LandingQueue and the worker priority
/// line, which includes returns from human waits.
/// A runnable landing and claim share the disk budget when it fits. When
/// they compete for disk, give the higher priority the first start and
/// give the landing a tie. Call again with refreshed facts after a start
/// or a failed attempt. A queued e2e has already passed LandingQueue's
/// worker replacement check.
pub fn admit<T>(candidates: Vec<Candidate<T>>, mut resources: Resources) -> Decision<T> {
    let mut landings = Vec::new();
    let mut slots = Vec::new();
    for candidate in candidates {
        match candidate.kind {
            Kind::Landing { .. } => landings.push(candidate),
            Kind::Slot(kind) => slots.push(SlotCandidate {
                kind,
                priority: candidate.priority,
                item: candidate,
            }),
        }
    }
    let mut decision = Decision {
        starts: Vec::new(),
        blocked: Vec::new(),
    };
    let slots = slot_order(slots);
    for landing in landings {
        let claim_first = resources
            .landing_disk
            .is_some_and(|disk| disk.claims == Some(0))
            && resources.blocked(landing.kind).is_none()
            && landing.progress.can_progress()
            && slots
                .iter()
                .filter(|slot| {
                    slot.item.progress.can_progress() && resources.blocked(slot.item.kind).is_none()
                })
                .take(resources.free_slots)
                .find(|slot| slot.kind == SlotKind::Claim)
                .is_some_and(|claim| claim.priority > landing.priority);
        if claim_first {
            resources.disk_claims = Some(1);
            decision.blocked.push((landing, Blocked::DiskCompetition));
        } else {
            decision.consider(landing, &mut resources);
        }
    }
    for slot in slots {
        decision.consider(slot.item, &mut resources);
    }
    decision
}

impl<T> Decision<T> {
    fn consider(&mut self, candidate: Candidate<T>, resources: &mut Resources) {
        let blocked = candidate
            .progress
            .blocked()
            .or_else(|| resources.blocked(candidate.kind));
        if let Some(reason) = blocked {
            self.blocked.push((candidate, reason));
            return;
        }
        match candidate.kind {
            Kind::Landing { needs_worker } => {
                *resources = resources.after_landing();
                resources.integrating = true;
                if needs_worker {
                    resources.free_slots = resources.free_slots.saturating_sub(1);
                }
            }
            Kind::Slot(kind) => {
                resources.free_slots -= 1;
                if kind == SlotKind::Claim {
                    resources.last_claim_ms = Some(resources.now_ms);
                    if let Some(left) = &mut resources.disk_claims {
                        *left -= 1;
                    }
                }
            }
        }
        self.starts.push(candidate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::disk::{DEFAULT_RESERVE_SHARE, DiskState, MIN_RESERVE_BYTES};

    #[test]
    fn the_existing_claim_gates_name_the_wait() {
        let open = Resources {
            free_slots: 3,
            ..Resources::default()
        };
        let load = claim_hold(
            None,
            &HoldInputs::default(),
            &HoldInputs {
                load_average: Some(5.0),
                max_load: Some(4.0),
                ..HoldInputs::default()
            },
        )
        .unwrap();
        for (resources, expected) in [
            (open, None),
            (
                Resources {
                    free_slots: 0,
                    ..open
                },
                Some(Blocked::WorkerSlot),
            ),
            (
                Resources {
                    claim_hold: Some(hold_reason(load.reason)),
                    ..open
                },
                Some(Blocked::Load),
            ),
            (
                Resources {
                    disk_claims: Some(0),
                    ..open
                },
                Some(Blocked::Disk(None)),
            ),
            (
                Resources {
                    disk_claims: Some(0),
                    disk_wait: Some(Blocked::Disk(Some(ClaimsWaitReason::NoSamples))),
                    ..open
                },
                Some(Blocked::Disk(Some(ClaimsWaitReason::NoSamples))),
            ),
            (
                Resources {
                    spacing_secs: Some(180),
                    last_claim_ms: Some(0),
                    now_ms: 179_000,
                    ..open
                },
                Some(Blocked::ClaimSpacing),
            ),
            (
                Resources {
                    spacing_secs: Some(180),
                    last_claim_ms: Some(0),
                    now_ms: 180_000,
                    ..open
                },
                None,
            ),
            (
                Resources {
                    gates: Gates {
                        ci: true,
                        ..Gates::default()
                    },
                    ..open
                },
                Some(Blocked::Ci),
            ),
            (
                Resources {
                    gates: Gates {
                        env: true,
                        ..Gates::default()
                    },
                    ..open
                },
                Some(Blocked::Environment),
            ),
            (
                Resources {
                    gates: Gates {
                        branch: true,
                        ..Gates::default()
                    },
                    ..open
                },
                Some(Blocked::LandingBranch),
            ),
        ] {
            assert_eq!(probe(SlotKind::Claim, resources), expected, "{resources:?}");
        }
    }

    fn returning(item: &'static str, priority: Priority, ended_at: i64) -> Candidate<&'static str> {
        Candidate::slot(SlotKind::Return { ended_at }, priority, item)
    }

    #[test]
    fn returns_compete_by_effective_priority_then_wait_end() {
        use Priority::{High, Low, Normal};

        let decision = admit(
            vec![
                returning("oldest low return", Low, 0),
                returning("later normal return", Normal, 3),
                Candidate::slot(SlotKind::Recovery, Normal, "normal recovery"),
                Candidate::slot(SlotKind::Resume, Normal, "normal resume"),
                returning("earlier normal return", Normal, 1),
                returning("latest high return", High, 4),
            ],
            Resources {
                free_slots: 5,
                ..Resources::default()
            },
        );
        assert_eq!(
            decision
                .starts
                .into_iter()
                .map(|start| start.item)
                .collect::<Vec<_>>(),
            [
                "latest high return",
                "earlier normal return",
                "later normal return",
                "normal resume",
                "normal recovery",
            ],
        );
        assert_eq!(decision.blocked.len(), 1);
        assert_eq!(decision.blocked[0].0.item, "oldest low return");
        assert_eq!(decision.blocked[0].1, Blocked::WorkerSlot);
    }

    #[test]
    fn higher_priority_claims_and_resumes_take_the_slot_before_a_low_return() {
        use Priority::{High, Interrupt, Low, Urgent};

        for priority in [Interrupt, Urgent, High] {
            assert!(!claim_waits_for_return(priority, Some(Low)));
            for kind in [SlotKind::Claim, SlotKind::Resume] {
                let result = admit(
                    vec![
                        returning("return", Low, 1),
                        Candidate::slot(kind, priority, "higher"),
                    ],
                    Resources {
                        free_slots: 1,
                        ..Resources::default()
                    },
                );
                assert_eq!(result.starts[0].item, "higher", "{priority:?} {kind:?}");
                assert_eq!(result.blocked[0].0.item, "return");
                assert_eq!(result.blocked[0].1, Blocked::WorkerSlot);
            }
        }
        assert!(claim_waits_for_return(Low, Some(Low)));
        assert!(claim_waits_for_return(Low, Some(High)));
        assert!(!claim_waits_for_return(Low, None));
        for held in [false, true] {
            let mut returning = returning("return", Low, 1);
            returning.progress.held = held;
            let result = admit(
                vec![Candidate::slot(SlotKind::Claim, Low, "claim"), returning],
                Resources {
                    free_slots: 1,
                    ..Resources::default()
                },
            );
            assert_eq!(result.starts[0].item, if held { "claim" } else { "return" });
            assert_eq!(
                result.blocked[0].1,
                if held {
                    Blocked::TaskHold
                } else {
                    Blocked::WorkerSlot
                }
            );
        }
    }

    #[test]
    fn a_claim_hold_leaves_the_slot_available_for_a_return() {
        let result = admit(
            vec![
                Candidate::slot(SlotKind::Claim, Priority::Interrupt, "claim"),
                returning("return", Priority::Low, 1),
            ],
            Resources {
                free_slots: 1,
                claim_hold: Some(Blocked::Load),
                ..Resources::default()
            },
        );
        assert_eq!(result.starts[0].item, "return");
        assert_eq!(result.blocked[0].0.item, "claim");
        assert_eq!(result.blocked[0].1, Blocked::Load);
    }

    #[test]
    fn one_shared_turn_gates_the_landing_queue_head() {
        let head = Candidate {
            item: "queue head",
            kind: Kind::Landing {
                needs_worker: false,
            },
            priority: Priority::Normal,
            progress: Progress::default(),
        };
        for (resources, expected) in [
            (Resources::default(), None),
            (
                Resources {
                    landing_turn_busy: true,
                    ..Resources::default()
                },
                Some(Blocked::LandingTurn),
            ),
            (
                Resources {
                    integrating: true,
                    ..Resources::default()
                },
                Some(Blocked::LandingInProgress),
            ),
            (
                Resources {
                    container_held: true,
                    ..Resources::default()
                },
                Some(Blocked::LandingHeld),
            ),
        ] {
            let decision = admit(vec![head.clone()], resources);
            assert_eq!(
                decision.blocked.first().map(|(_, reason)| *reason),
                expected
            );
            assert_eq!(decision.starts_work(), expected.is_none());
        }
    }

    #[test]
    fn landing_and_claim_share_disk_or_take_one_turn_by_priority() {
        use crate::domain::{RunId, disk::LandingCandidate};

        const GIB: u64 = 1 << 30;
        const JOINT_NEED: u64 = 45 * GIB;
        let hi = Priority::High;
        let irq = Priority::Interrupt;
        let ready = Progress::default();
        let held = Progress {
            held: true,
            ..ready
        };
        let unanswered = Progress {
            unanswered: true,
            ..ready
        };
        let open = Resources {
            free_slots: 3,
            ..Resources::default()
        };
        let turn = Resources {
            landing_turn_busy: true,
            ..open
        };
        let spacing = Resources {
            spacing_secs: Some(1),
            last_claim_ms: Some(0),
            ..open
        };
        let both = (&["landing", "claim"][..], None);
        let landing_wins = (&["landing"][..], Some("claim"));
        let claim_wins = (&["claim"][..], Some("landing"));
        let just_landing = (&["landing"][..], None);
        let just_claim = (&["claim"][..], None);
        let worker = Some(20 * GIB);
        for (free, worker, priority, landing_progress, resources, (starts, waits_for_disk)) in [
            (JOINT_NEED, worker, hi, ready, open, both),
            (JOINT_NEED, worker, irq, ready, open, both),
            // One byte short of the joint budget, the reserve still lets
            // exactly one start. A landing wins a tie.
            (JOINT_NEED - 1, worker, hi, ready, open, landing_wins),
            (JOINT_NEED - 1, worker, irq, ready, open, claim_wins),
            (10 * GIB, worker, hi, ready, open, landing_wins),
            (10 * GIB, worker, irq, ready, open, claim_wins),
            (10 * GIB, worker, hi, ready, turn, just_claim),
            (10 * GIB, worker, hi, held, open, just_claim),
            (10 * GIB, worker, hi, unanswered, open, just_claim),
            (10 * GIB, worker, irq, ready, spacing, just_landing),
            // Below the reserve, neither standalone start fits.
            (MIN_RESERVE_BYTES - 1, worker, irq, ready, open, (&[], None)),
            (100 * GIB, None, hi, ready, open, just_landing),
            (100 * GIB, None, irq, ready, open, claim_wins),
        ] {
            let run = RunId::new("landing").unwrap();
            let disk = DiskState {
                free: Some(free),
                total: 0,
                expected: worker,
                integrator_expected: worker,
                runs: vec![],
                moving: 0,
                landing_current: None,
                landing_waiting: landing_waiting(false, [landing_progress]),
                relaxed: None,
            }
            .decide(
                DEFAULT_RESERVE_SHARE,
                &[LandingCandidate {
                    run: run.clone(),
                    current: Some(0),
                    integrating: false,
                    waiting: true,
                }],
                Default::default(),
                0,
            );
            let landing = disk.landing(&run).unwrap();
            let mut resources = resources.with_disk(&disk);
            resources.landing_disk = Some(LandingDisk {
                claims: landing.claims_with_landing,
            });
            for needs_worker in [false, true] {
                let mut candidates = vec![
                    Candidate::slot(SlotKind::Claim, priority, "claim"),
                    Candidate::slot(SlotKind::Claim, Priority::Normal, "another claim"),
                ];
                if landing.allowed {
                    candidates.push(Candidate {
                        item: "landing",
                        kind: Kind::Landing { needs_worker },
                        priority: hi,
                        progress: landing_progress,
                    });
                }
                let decision = admit(candidates, resources);
                assert_eq!(
                    decision
                        .starts
                        .into_iter()
                        .map(|candidate| candidate.item)
                        .collect::<Vec<_>>(),
                    starts,
                    "free={free}, priority={priority:?}, needs_worker={needs_worker}, progress={landing_progress:?}, resources={resources:?}"
                );
                if let Some(waiting) = waits_for_disk {
                    assert!(
                        decision.blocked.iter().any(|(candidate, reason)| {
                            candidate.item == waiting && *reason == Blocked::DiskCompetition
                        }),
                        "free={free}, priority={priority:?}, needs_worker={needs_worker}: {waiting} waits for disk"
                    );
                }
            }
        }
    }

    #[test]
    fn only_a_claim_with_a_worker_slot_competes_with_the_landing_for_disk() {
        use Priority::{High, Interrupt, Low};

        let landing = Candidate {
            item: "landing",
            kind: Kind::Landing {
                needs_worker: false,
            },
            priority: High,
            progress: Progress::default(),
        };
        let claim = Candidate::slot(SlotKind::Claim, Interrupt, "claim");
        for (slots, return_priority, starts) in [
            (1, Interrupt, &["landing", "return"][..]),
            (2, Interrupt, &["return", "claim"][..]),
            (1, Low, &["claim"][..]),
        ] {
            let decision = admit(
                vec![
                    landing.clone(),
                    claim.clone(),
                    returning("return", return_priority, 1),
                ],
                Resources {
                    free_slots: slots,
                    disk_claims: Some(1),
                    landing_disk: Some(LandingDisk { claims: Some(0) }),
                    ..Resources::default()
                },
            );
            assert_eq!(
                decision
                    .starts
                    .into_iter()
                    .map(|candidate| candidate.item)
                    .collect::<Vec<_>>(),
                starts,
                "worker slots={slots}, return priority={return_priority:?}"
            );
        }
    }

    #[test]
    fn stopped_runs_keep_their_current_size_without_a_disk_reservation() {
        use crate::domain::{disk::disk_usage, run_phase::Phase};

        let open = Progress::default();
        for (progress, can_reserve) in [
            (Progress { held: true, ..open }, false),
            (open, true),
            (
                Progress {
                    unanswered: true,
                    ..open
                },
                false,
            ),
            (
                Progress {
                    gates: Gates {
                        ci: true,
                        ..open.gates
                    },
                    ..open
                },
                false,
            ),
            (
                Progress {
                    gates: Gates {
                        env: true,
                        ..open.gates
                    },
                    ..open
                },
                false,
            ),
            (
                Progress {
                    gates: Gates {
                        branch: true,
                        ..open.gates
                    },
                    ..open
                },
                false,
            ),
        ] {
            for (phase, moving, workers, integrators) in [
                (Phase::Landing, 1, 0, 1),
                (Phase::Worker, 1, 1, 0),
                (Phase::AwaitingE2e, 1, 1, 0),
                (Phase::LandingQueue, 0, 0, 0),
                (Phase::Held, 0, 0, 0),
            ] {
                let current = Some(2 * (1 << 30));
                let usage = disk_usage(phase, true, true);
                let run = progress.disk_run(usage, current);
                let disk = DiskState {
                    free: Some(MIN_RESERVE_BYTES),
                    total: 0,
                    expected: Some(8 * (1 << 30)),
                    integrator_expected: Some(8 * (1 << 30)),
                    runs: vec![run],
                    moving: usize::from(progress.disk_usage(usage).active),
                    landing_current: None,
                    landing_waiting: false,
                    relaxed: None,
                };
                let needs = disk.needs(DEFAULT_RESERVE_SHARE);
                let expected = if can_reserve && moving > 0 {
                    (moving, workers, integrators, 6 * (1 << 30))
                } else {
                    assert_eq!(run.stage, DiskStage::Waiting, "{phase:?}, {progress:?}");
                    (0, 0, 0, 0)
                };
                assert_eq!(run.current, current, "{phase:?}, {progress:?}");
                assert_eq!(
                    (
                        needs.moving,
                        needs.workers,
                        needs.integrators,
                        needs.remaining
                    ),
                    expected,
                    "{phase:?}, {progress:?}",
                );
            }
        }
    }
}
