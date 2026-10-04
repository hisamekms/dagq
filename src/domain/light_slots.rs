//! The slots a run waiting only for its landing turn leaves, and the light
//! changes that may use them (ADR-t1591-1): a run whose review passed and
//! whose e2e is done waits outside the slots for the single landing, up to
//! `parallel` of them, and the room it leaves is claimed only by a task of
//! one of `[supervisor] light_changes` that declares its `--paths`. A heavy
//! task is claimed as before, only while the slots with the landing queue
//! in them are under `parallel`.

use super::change::{ChangeSet, TaskChange};
use super::claim_spacing;

/// `[supervisor] light_changes`: the changes a repository calls light.
/// Empty (the default) claims nothing outside the slots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LightChanges(Vec<TaskChange>);

impl LightChanges {
    /// `values` checked against `[tasks] changes` (`changes`): each one of
    /// the set, none twice. A value with no set to check it against is
    /// refused, so is an empty list (the key left out is the default).
    pub fn new(values: Vec<TaskChange>, changes: Option<&ChangeSet>) -> Result<Self, String> {
        if values.is_empty() {
            return Err(
                "names no change; leave the key out to claim none in the light room".into(),
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

    /// Whether a task of `change` with `paths` may take the light room: its
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
    /// Only the room the landing queue left: a light task.
    LightOnly,
    /// No claim.
    None,
}

/// The room for the next claim. `used` is the slots as they were always
/// counted (every leased run that does not wait for a person, the landing
/// queue in them), `landing_queue` the runs among them that wait only for
/// their landing turn, `returning` the runs whose wait ended and that wait
/// to go back. A heavy task needs `used` under `parallel`, as before; a
/// light task needs only `used` less the landing queue outside the slots
/// under it, with `light` set and no run waiting to go back, which comes
/// before any new work (ADR-0071 decision 8). A resume and a run going back
/// from a wait use the normal room only.
pub fn claim_room(
    used: usize,
    landing_queue: usize,
    parallel: usize,
    returning: usize,
    light: bool,
) -> ClaimRoom {
    if used < parallel {
        ClaimRoom::Any
    } else if light
        && returning == 0
        && used - outside_the_slots(landing_queue, parallel).min(used) < parallel
    {
        ClaimRoom::LightOnly
    } else {
        ClaimRoom::None
    }
}

/// The room after the gates every claim passes, the light ones too: no
/// claim while the claims are held (the load or the disk), nor while the
/// claim spacing in effect (`spacing_secs`) after the queue's latest claim
/// (`last_claim_ms`, a light one too) waits at `now_ms` (ADR-t1479-1).
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
    fn without_light_changes_the_room_is_the_slots_as_before() {
        for used in 0..6 {
            for queue in 0..=used {
                let room = claim_room(used, queue, 3, 0, false);
                let expected = if used < 3 {
                    ClaimRoom::Any
                } else {
                    ClaimRoom::None
                };
                assert_eq!(room, expected, "used {used}, landing queue {queue}");
            }
        }
    }

    #[test]
    fn the_landing_queue_leaves_room_for_light_tasks_only() {
        // Three slots full of runs waiting for the landing turn.
        assert_eq!(claim_room(3, 3, 3, 0, true), ClaimRoom::LightOnly);
        // A light run claimed into the room counts in the slots.
        assert_eq!(claim_room(4, 3, 3, 0, true), ClaimRoom::LightOnly);
        assert_eq!(claim_room(6, 3, 3, 0, true), ClaimRoom::None);
        // A heavy task still needs a slot with the landing queue in it.
        assert_eq!(claim_room(2, 2, 3, 0, true), ClaimRoom::Any);
        // No landing queue: no light room.
        assert_eq!(claim_room(3, 0, 3, 0, true), ClaimRoom::None);
        // Past `parallel`, the landing queue counts in the slots.
        assert_eq!(outside_the_slots(5, 3), 3);
        assert_eq!(claim_room(6, 5, 3, 0, true), ClaimRoom::None);
        assert_eq!(claim_room(5, 5, 3, 0, true), ClaimRoom::LightOnly);
    }

    #[test]
    fn a_run_going_back_or_resumed_waits_for_a_normal_slot() {
        // A run whose wait ended goes back before any new work: the light
        // room claims nothing while it waits.
        assert_eq!(claim_room(3, 2, 3, 1, true), ClaimRoom::None);
        // A run back in `needs_session` from its landing is resumed only in
        // a normal slot (`used` under `parallel`): with the landing queue
        // filling the slots, only a light claim has room.
        assert_eq!(claim_room(3, 2, 3, 0, true), ClaimRoom::LightOnly);
        assert_ne!(claim_room(3, 2, 3, 0, true), ClaimRoom::Any);
    }

    #[test]
    fn the_load_hold_and_the_claim_spacing_gate_the_light_room_too() {
        let t0 = 1_791_072_000_000;
        // The load hold holds a light claim as any other.
        assert_eq!(
            gated(ClaimRoom::LightOnly, true, None, None, t0),
            ClaimRoom::None
        );
        // Within the spacing after the latest claim, no light claim.
        let latest = Some(t0);
        assert_eq!(
            gated(ClaimRoom::LightOnly, false, Some(180), latest, t0 + 10_000),
            ClaimRoom::None
        );
        // A light claim starts the next spacing: the claim after it, light
        // or not, waits for it.
        assert_eq!(
            gated(ClaimRoom::Any, false, Some(180), latest, t0 + 179_000),
            ClaimRoom::None
        );
        assert_eq!(
            gated(ClaimRoom::LightOnly, false, Some(180), latest, t0 + 180_000),
            ClaimRoom::LightOnly
        );
        // No spacing in effect (the load hold off): only the hold gates.
        assert_eq!(
            gated(ClaimRoom::LightOnly, false, None, latest, t0 + 1_000),
            ClaimRoom::LightOnly
        );
        assert_eq!(gated(ClaimRoom::Any, false, None, None, t0), ClaimRoom::Any);
    }
}
