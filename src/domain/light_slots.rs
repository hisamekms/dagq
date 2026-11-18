//! Worker capacity excludes human waits and the serial landing lane.
//! Landing-turn waits outside worker capacity are capped at `parallel`.

use super::change::{ChangeSet, TaskChange};
use super::claim_spacing;

/// `[supervisor] light_changes`: the changes a repository calls light.
/// Values come from `[tasks] changes`; the runtime assigns them no meaning.
/// This classifies candidates, never opens claim capacity. Empty classifies
/// none as light; tasks without declared paths are not light either.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LightChanges(Vec<TaskChange>);

impl LightChanges {
    /// `values` checked against `[tasks] changes` (`changes`): each one of
    /// the set, none twice. A value with no set to check it against is
    /// refused, so is an empty list (the key left out is the default).
    pub fn new(values: Vec<TaskChange>, changes: Option<&ChangeSet>) -> Result<Self, String> {
        if values.is_empty() {
            return Err(
                "names no change; leave the key out to classify no changes as light".into(),
            );
        }
        for (index, value) in values.iter().enumerate() {
            if values[..index].contains(value) {
                return Err(format!("names {value} twice"));
            }
            let Some(changes) = changes else {
                return Err(format!(
                    "names {value}, but there is no [tasks] changes to name it in"
                ));
            };
            if !changes.values().contains(value) {
                return Err(format!(
                    "names {value}, which is not one of [tasks] changes"
                ));
            }
        }
        Ok(Self(values))
    }

    pub fn values(&self) -> &[TaskChange] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether a task of `change` with `paths` is classified as light: its
    /// change is light, and it declares the paths it changes (a task with
    /// none may change anything).
    pub fn admits(&self, change: Option<&TaskChange>, paths: &[String]) -> bool {
        change.is_some_and(|change| self.0.contains(change)) && !paths.is_empty()
    }
}

/// The landing queue that counts outside the slots: the runs waiting only
/// for their landing turn, up to `parallel` (the landing is serial, so a
/// longer queue counts in the slots past it).
pub fn outside_the_slots(landing_queue: usize, parallel: usize) -> usize {
    landing_queue.min(parallel)
}

/// What the next claim may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimRoom {
    /// A free slot: any task, in the claim order.
    Any,
    /// Historical light room, retained for recorded classes and deferrals.
    /// The worker-capacity judgment never returns this variant.
    LightOnly,
    /// No claim.
    None,
}

/// Worker capacity in use: active workers and the excess landing queue
/// over `parallel`. Ready landings contribute to backpressure even after
/// their leases were returned. Active workers exclude human waits and
/// integrating runs, but include review, e2e, revise and resume.
pub fn worker_slots(active_workers: usize, queued: usize, parallel: usize) -> usize {
    active_workers + queued.saturating_sub(parallel)
}

/// A queued run takes a worker slot to run e2e. Removing it from the
/// queue and starting its e2e are one admission: if it replaces an excess
/// wait, it uses capacity already charged to the queue. Even when direct
/// approvals grew the queue past the limit, replacements can drain it
/// without increasing the charge or exceeding the actual worker limit.
pub fn queued_e2e_has_room(active_workers: usize, queued: usize, parallel: usize) -> bool {
    active_workers < parallel
        && worker_slots(active_workers + 1, queued.saturating_sub(1), parallel)
            <= worker_slots(active_workers, queued, parallel).max(parallel)
}

/// New claims use worker capacity regardless of change, paths or priority.
/// Runs returning from human waits take precedence over every new claim.
/// Resume/adopt/recovery and returning runs compare the same worker count
/// with `parallel`, without the new-claim returning gate.
pub fn claim_room(used: usize, parallel: usize, returning: usize) -> ClaimRoom {
    if returning == 0 && used < parallel {
        ClaimRoom::Any
    } else {
        ClaimRoom::None
    }
}

/// The room after the gates every claim passes: no
/// claim while the claims are held (the load or the disk), nor while the
/// claim spacing in effect (`spacing_secs`) after the queue's latest claim
/// (`last_claim_ms`) waits at `now_ms` (ADR-t1479-1).
pub fn gated(
    room: ClaimRoom,
    held: bool,
    spacing_secs: Option<usize>,
    last_claim_ms: Option<i64>,
    now_ms: i64,
) -> ClaimRoom {
    let spaced = claim_spacing::waits(
        claim_spacing::next_claim_ms(spacing_secs, last_claim_ms),
        now_ms,
    );
    if held || spaced {
        ClaimRoom::None
    } else {
        room
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(name: &str) -> TaskChange {
        name.parse().unwrap()
    }

    fn set() -> ChangeSet {
        ChangeSet::new(vec![change("feature"), change("docs"), change("config")]).unwrap()
    }

    #[test]
    fn light_changes_are_of_the_set_once_each() {
        let light =
            LightChanges::new(vec![change("docs"), change("config")], Some(&set())).unwrap();
        assert_eq!(light.values(), [change("docs"), change("config")]);
        assert!(LightChanges::default().is_empty());
        let error = |values: Vec<TaskChange>, set: Option<&ChangeSet>| {
            LightChanges::new(values, set).unwrap_err()
        };
        assert!(
            error(vec![change("measure")], Some(&set())).contains("not one of [tasks] changes")
        );
        assert!(error(vec![change("docs"), change("docs")], Some(&set())).contains("twice"));
        assert!(error(vec![change("docs")], None).contains("no [tasks] changes"));
        assert!(error(Vec::new(), Some(&set())).contains("names no change"));
    }

    #[test]
    fn only_a_light_change_with_its_paths_is_admitted() {
        let light = LightChanges::new(vec![change("docs")], Some(&set())).unwrap();
        let paths = vec!["docs/**".to_owned()];
        assert!(light.admits(Some(&change("docs")), &paths));
        // No --paths: it may change anything.
        assert!(!light.admits(Some(&change("docs")), &[]));
        assert!(!light.admits(Some(&change("feature")), &paths));
        assert!(!light.admits(None, &paths));
        assert!(!LightChanges::default().admits(Some(&change("docs")), &paths));
    }

    #[test]
    fn landing_stages_leave_two_claims_for_any_change() {
        // One worker, one integrator, two queued landings: two more workers.
        let used = worker_slots(1, 2, 3);
        assert_eq!(used, 1);
        for light in [
            LightChanges::default(),
            LightChanges::new(vec![change("docs")], Some(&set())).unwrap(),
        ] {
            // A heavy interrupt is not classified light but gets the same room.
            assert!(!light.admits(Some(&change("feature")), &["src/**".into()]));
            assert_eq!(claim_room(used, 3, 0), ClaimRoom::Any);
            assert_eq!(claim_room(used + 1, 3, 0), ClaimRoom::Any);
            assert_eq!(claim_room(used + 2, 3, 0), ClaimRoom::None);
        }
    }

    #[test]
    fn landing_queue_excess_is_worker_backpressure() {
        assert_eq!(outside_the_slots(5, 3), 3);
        assert_eq!(worker_slots(1, 5, 3), 3);
        assert_eq!(claim_room(3, 3, 0), ClaimRoom::None);
        assert_eq!(worker_slots(0, 6, 3), 3);
        assert_eq!(worker_slots(0, 5, 3), 2);
        assert_eq!(worker_slots(0, 0, 3), 0);
    }

    #[test]
    fn queued_e2e_can_replace_excess_but_cannot_overbook_workers() {
        assert!(queued_e2e_has_room(2, 3, 3));
        assert!(!queued_e2e_has_room(3, 3, 3));
        assert!(queued_e2e_has_room(0, 6, 3));
        assert!(queued_e2e_has_room(2, 7, 3));
        assert!(!queued_e2e_has_room(3, 7, 3));
    }

    #[test]
    fn returning_runs_precede_claims_and_failed_landings_are_workers() {
        assert_eq!(claim_room(worker_slots(1, 2, 3), 3, 1), ClaimRoom::None);
        // Landing failure moves out of the lane into needs_session.
        assert_eq!(worker_slots(2, 2, 3), 2);
        assert_eq!(claim_room(2, 3, 0), ClaimRoom::Any);
        assert_eq!(worker_slots(3, 2, 3), 3);
        assert_eq!(claim_room(3, 3, 0), ClaimRoom::None);
        // Review and e2e are ordinary workers, not queued landings.
        assert_eq!(worker_slots(3, 0, 3), 3);
    }

    #[test]
    fn the_load_hold_and_the_claim_spacing_gate_every_claim() {
        let t0 = 1_791_072_000_000;
        // A worker slot does not bypass the load gate.
        assert_eq!(gated(ClaimRoom::Any, true, None, None, t0), ClaimRoom::None);
        // Within the spacing after the latest claim, no new claim.
        let latest = Some(t0);
        assert_eq!(
            gated(ClaimRoom::Any, false, Some(180), latest, t0 + 10_000),
            ClaimRoom::None
        );
        // Every claim starts the next spacing.
        assert_eq!(
            gated(ClaimRoom::Any, false, Some(180), latest, t0 + 179_000),
            ClaimRoom::None
        );
        assert_eq!(
            gated(ClaimRoom::Any, false, Some(180), latest, t0 + 180_000),
            ClaimRoom::Any
        );
        // No spacing in effect (the load hold off): only the hold gates.
        assert_eq!(
            gated(ClaimRoom::Any, false, None, latest, t0 + 1_000),
            ClaimRoom::Any
        );
        assert_eq!(gated(ClaimRoom::Any, false, None, None, t0), ClaimRoom::Any);
    }
}
