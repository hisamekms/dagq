//! Worker capacity excludes human waits and the serial landing lane.
//! Landing-turn waits outside worker capacity are capped at `parallel`.

use super::change::{ChangeSet, TaskChange};

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

    fn claim_room(used: usize, parallel: usize) -> ClaimRoom {
        super::super::admission::claim_room(super::super::admission::Resources {
            free_slots: parallel.saturating_sub(used),
            ..Default::default()
        })
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
            assert_eq!(claim_room(used, 3), ClaimRoom::Any);
            assert_eq!(claim_room(used + 1, 3), ClaimRoom::Any);
            assert_eq!(claim_room(used + 2, 3), ClaimRoom::None);
        }
    }

    #[test]
    fn landing_queue_excess_is_worker_backpressure() {
        assert_eq!(worker_slots(1, 5, 3), 3);
        assert_eq!(claim_room(3, 3), ClaimRoom::None);
        assert_eq!(worker_slots(0, 6, 3), 3);
        assert_eq!(worker_slots(0, 5, 3), 2);
        assert_eq!(worker_slots(0, 0, 3), 0);
    }

    #[test]
    fn failed_landings_are_workers() {
        // Landing failure moves out of the lane into needs_session.
        assert_eq!(worker_slots(2, 2, 3), 2);
        assert_eq!(claim_room(2, 3), ClaimRoom::Any);
        assert_eq!(worker_slots(3, 2, 3), 3);
        assert_eq!(claim_room(3, 3), ClaimRoom::None);
        // Review still takes a worker slot.
        assert_eq!(worker_slots(3, 0, 3), 3);
    }
}
