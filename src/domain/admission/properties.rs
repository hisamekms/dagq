use super::*;
use proptest::prelude::*;

fn gates(bits: u8) -> Gates {
    Gates {
        ci: bits & 1 != 0,
        env: bits & 2 != 0,
        branch: bits & 4 != 0,
    }
}

fn candidates() -> impl Strategy<Value = Vec<Candidate<usize>>> {
    let entry = (
        0_u8..6,
        any::<i64>(),
        prop::sample::select(Priority::ALL.to_vec()),
        any::<u8>(),
    );
    proptest::collection::vec(entry, 0..16).prop_map(|line| {
        line.into_iter()
            .enumerate()
            .map(|(item, (kind, ended_at, priority, progress))| {
                let kind = match kind {
                    0 => Kind::Slot(SlotKind::Claim),
                    1 => Kind::Slot(SlotKind::Resume),
                    2 => Kind::Slot(SlotKind::Recovery),
                    3 => Kind::Slot(SlotKind::Return { ended_at }),
                    _ => Kind::Landing {
                        needs_worker: kind == 5,
                    },
                };
                // Favor progressing candidates while varying every gate.
                let progress = if progress < 64 {
                    Progress {
                        held: progress & 8 != 0,
                        unanswered: progress & 16 != 0,
                        gates: gates(progress),
                    }
                } else {
                    Progress::default()
                };
                Candidate {
                    item,
                    kind,
                    priority,
                    progress,
                }
            })
            .collect()
    })
}

fn resources() -> impl Strategy<Value = Resources> {
    let hold = prop::option::of(prop::sample::select(vec![
        Blocked::Authentication,
        Blocked::Load,
        Blocked::Disk(None),
    ]));
    let budget = || prop::option::of(prop_oneof![0_usize..5, any::<usize>()]);
    let waits = prop::option::of(Just(Blocked::Disk(Some(ClaimsWaitReason::NoSamples))));
    let stops = prop_oneof![3 => Just(0), 1 => 0_u8..32];
    let base = (0_usize..5, 0_u8..8, hold, budget(), waits, stops);
    let timing = (
        prop::option::of(0_usize..6),
        prop::option::of(-100_i64..10_000),
        -100_i64..10_000,
    );
    (base, prop::option::of(budget()), timing).prop_map(
        |(
            (free_slots, gates_bits, claim_hold, disk_claims, disk_wait, stops),
            landing_disk,
            (spacing_secs, last_claim_ms, now_ms),
        )| Resources {
            free_slots,
            gates: gates(gates_bits),
            repository_config_missing: stops & 1 != 0,
            service_down: stops & 2 != 0,
            claim_hold,
            disk_claims,
            disk_wait,
            landing_disk: landing_disk.map(|claims| LandingDisk { claims }),
            spacing_secs,
            last_claim_ms,
            now_ms,
            container_held: stops & 4 != 0,
            integrating: stops & 8 != 0,
            landing_turn_busy: stops & 16 != 0,
        },
    )
}

/// Single-candidate rules for the unspent snapshot. LandingQueue supplies
/// preselected landings, including the disk and e2e replacement capacity
/// checks. The joint disk budget and priority only affect the order.
fn expected_startable(candidate: &Candidate<usize>, resources: Resources) -> bool {
    let Progress {
        held,
        unanswered,
        gates: run,
    } = candidate.progress;
    if held || unanswered || run.ci || run.env || run.branch {
        return false;
    }

    let Gates { ci, env, branch } = resources.gates;
    let shared_gates_clear = !(ci || env || branch);
    let worker_available = resources.free_slots > 0;
    match candidate.kind {
        Kind::Slot(SlotKind::Claim) => {
            let spacing_elapsed = match (resources.last_claim_ms, resources.spacing_secs) {
                (Some(last), Some(seconds)) => {
                    let elapsed_ms = i128::from(resources.now_ms) - i128::from(last);
                    elapsed_ms >= (seconds as i128) * 1000
                }
                _ => true,
            };
            shared_gates_clear
                && worker_available
                && !resources.repository_config_missing
                && !resources.service_down
                && resources.claim_hold.is_none()
                && resources.disk_claims.is_none_or(|available| available > 0)
                && spacing_elapsed
        }
        Kind::Slot(SlotKind::Resume) => shared_gates_clear && worker_available,
        Kind::Slot(SlotKind::Recovery) => !branch && worker_available,
        Kind::Slot(SlotKind::Return { .. }) => worker_available,
        Kind::Landing { .. } => {
            shared_gates_clear
                && !resources.container_held
                && !resources.integrating
                && !resources.landing_turn_busy
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn a_startable_candidate_implies_a_start(line in candidates(), resources in resources()) {
        let mut has_startable_candidate = false;
        for candidate in &line {
            let expected = expected_startable(candidate, resources);
            prop_assert_eq!(candidate.can_start(resources), expected, "{:?}, {:?}", candidate, resources);
            has_startable_candidate |= expected;
        }
        let decision = admit(line, resources);
        prop_assert_eq!(decision.has_startable_candidate, has_startable_candidate);
        prop_assert!(!has_startable_candidate || !decision.starts.is_empty(), "{:?}", decision.blocked);
    }
}

#[test]
fn an_ineligible_pass_breaks_the_streak_and_a_start_rearms_the_alert() {
    use crate::domain::EventKind::{AdmissionResumed, AdmissionStalled};
    let mut stall = Stall::default();
    let mut stalled = Pass::default();
    // Inject a decision that lost a candidate to exercise the detector's
    // invariant check independently of the scheduler.
    stalled.observe(&Decision {
        starts: Vec::new(),
        has_startable_candidate: true,
        blocked: vec![(
            Candidate::slot(SlotKind::Claim, Priority::High, ()),
            Blocked::Returning,
        )],
    });
    assert_eq!(stalled.reasons(), &["Returning"]);
    let empty = Pass::default();
    let mut started = Pass::default();
    started.started();

    assert_eq!(stall.observe(&stalled, 1), None);
    assert_eq!(stall.observe(&empty, 1), None);
    assert_eq!(stall.observe(&stalled, 1), None);
    assert_eq!(stall.observe(&stalled, 1), Some(AdmissionStalled));
    assert_eq!(stall.observe(&empty, 1), None);
    assert_eq!(stall.observe(&stalled, 1), None);
    assert_eq!(stall.observe(&started, 1), Some(AdmissionResumed));
    assert_eq!(stall.observe(&stalled, 0), Some(AdmissionStalled));
}
