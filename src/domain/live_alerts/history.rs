//! What a supervisor keeps of the events between its observations of the
//! judgments of now (docs/design/measurement.md "今の判定の記録"), so a
//! pass reads only the events that came since the last one and the events
//! of the runs not finished, not the whole history.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::Value;

use super::super::{
    EventId, EventKind, GoalId, RunEvent, RunId, TaskId, claim_defer, claim_hold,
    stats::{LiveRun, WorkClock, timestamp_millis, work_outliers},
};

/// How many events one read of the events past the cursor asks for.
pub const EVENTS_PER_READ: usize = 5_000;

/// How many events one read of the first pass's work kinds
/// ([`FirstPassReads::work`]) asks for. Each page is folded and let go
/// before the next is read ([`SeededFold`]), so what those reads hold is a
/// page at any size of the history. A read holds more than a page for a
/// while: `events_of_between` reads this many of each kind before it sorts
/// and cuts them, so a read holds up to one limit per kind. To hold as much
/// on a small queue as on a large one, the limit stays under what the
/// smaller queue of `examples/supervisor_startup_memory.rs` holds of each
/// kind, about 58: at this limit both read 150 events at once of the three
/// kinds they hold, and the pages do not differ between them, only what
/// grows with the runs and the tasks (the example asserts that the limit stays
/// under). The work events
/// of production are three of each run, some tens of thousands, so a
/// process's first pass makes some hundreds of these reads, each down the
/// index of its kind; a pass of a process comes once per exec.
pub const WORK_EVENTS_PER_READ: usize = 50;

/// What the first pass of a process reads to fold the events up to its
/// cursor into a [`LiveHistory`] without reading every event
/// ([`LiveHistory::seeded`]): each part of the history is decided by the
/// newest of a few kinds, or by every event of a few kinds, by the rules
/// [`LiveHistory`]'s fold reads (`observe`), so these reads fold to the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstPassReads {
    /// The newest event of each (the newer of the two decides): a
    /// `claim_held` opens the hold whatever came before it, a
    /// `claim_resumed` ends it.
    pub hold: [EventKind; 2],
    /// Read past an open hold's event: what ends it when the supervisor
    /// that opened it stopped.
    pub hold_ends: EventKind,
    /// The newest event of it: the thresholds.
    pub stall: EventKind,
    /// Per task, its newest event of one of them: the task's deferral is
    /// open when it is a `claim_deferred`.
    pub deferrals: [EventKind; 3],
    /// Every event of them, [`WORK_EVENTS_PER_READ`] at a time and folded
    /// page by page: each run's work clock, all but whether a run that
    /// neither landed nor closed as satisfied finished, which any event's
    /// `status` says ([`LiveHistory::unsettled`]).
    pub work: [EventKind; 4],
}

impl FirstPassReads {
    /// Every kind the reads name.
    pub fn kinds(&self) -> BTreeSet<EventKind> {
        self.hold
            .into_iter()
            .chain([self.hold_ends, self.stall])
            .chain(self.deferrals)
            .chain(self.work)
            .collect()
    }

    /// Whether `held`, the newest event of [`Self::hold`], opens a hold
    /// that the stops past it ([`Self::hold_ends`]) may end.
    pub fn opens_hold(&self, held: &RunEvent) -> bool {
        held.kind == claim_hold::CLAIMS.held
    }
}

/// The reads of the first pass of a process ([`FirstPassReads`]).
pub const fn first_pass_reads() -> FirstPassReads {
    FirstPassReads {
        hold: [claim_hold::CLAIMS.held, claim_hold::CLAIMS.resumed],
        hold_ends: EventKind::SupervisorStopped,
        stall: EventKind::StallConfigLoaded,
        deferrals: [
            EventKind::ClaimDeferred,
            EventKind::ClaimDeferralEnded,
            EventKind::RunClaimed,
        ],
        work: [
            EventKind::RunClaimed,
            EventKind::ReceiptObserved,
            EventKind::RunSatisfiedClosed,
            EventKind::RunIntegrated,
        ],
    }
}

/// [`LiveHistory::seeded`] with pages of events besides the events it is
/// given whole, so that no one holds the pages' events together: each page
/// folds into the history with the events given whole that come before its
/// end, in the order of their ids, and each event once, as one fold of them
/// all. An event in both (a `run_claimed` is the newest of its task's
/// deferral kinds and a work event too) folds into its run's work clock
/// once, and an event given whole with a run (of whatever kind) has its
/// run's work clock as it has in the fold of every event.
#[derive(Debug)]
pub struct SeededFold {
    upto: EventId,
    /// The events given whole not folded yet, the next one last.
    whole: Vec<RunEvent>,
    history: LiveHistory,
}

impl SeededFold {
    /// A fold up to `upto` of `whole`, in no order and not once each;
    /// those past `upto` are not folded.
    #[must_use]
    pub fn new(upto: EventId, mut whole: Vec<RunEvent>) -> Self {
        whole.retain(|event| event.id <= upto);
        whole.sort_by_key(|event| std::cmp::Reverse(event.id));
        Self {
            upto,
            whole,
            history: LiveHistory::default(),
        }
    }

    /// Fold `page` (ascending id, past the pages before it) and the events
    /// given whole that come before each of its events; those past
    /// `upto` are not folded.
    pub fn page(&mut self, page: &[RunEvent]) {
        let upto = self.upto;
        for event in page.iter().filter(|event| event.id <= upto) {
            self.whole_through(event.id);
            self.history.mark(std::slice::from_ref(event));
        }
    }

    /// The history: the events given whole that no page passed folded too.
    #[must_use]
    pub fn finish(mut self) -> LiveHistory {
        self.whole_through(self.upto);
        self.history.reach(self.upto);
        self.history
    }

    /// Fold the events given whole up to `id`. An event already folded is
    /// passed over by [`LiveHistory::mark`], which is what folds one in both
    /// once.
    fn whole_through(&mut self, id: EventId) {
        while self.whole.last().is_some_and(|event| event.id <= id) {
            if let Some(event) = self.whole.pop() {
                self.history.mark(std::slice::from_ref(&event));
            }
        }
    }
}

/// The events up to [`Self::upto`] folded to what the judgments of now
/// ([`super::judge`] and the inputs `stats` reads off the events) take
/// from beyond the runs not finished: the claim hold open, the open
/// deferral of each task, the latest `stall_config_loaded`, and each run's
/// work clock and its first task. A run's own events are not kept: a run
/// comes back unfinished (a triage's resume), so a pass reads those of the
/// runs not finished from the queue. The asks the judgments read are those
/// of these runs, which their own events open and close.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LiveHistory {
    upto: EventId,
    hold: Option<RunEvent>,
    deferrals: BTreeMap<TaskId, RunEvent>,
    stall: Option<RunEvent>,
    works: HashMap<RunId, (TaskId, WorkClock)>,
}

impl LiveHistory {
    /// The last event folded (0 before any).
    pub fn upto(&self) -> EventId {
        self.upto
    }

    /// Fold `events` (ascending id): those past [`Self::upto`], which
    /// moves on to the last of them.
    pub fn mark(&mut self, events: &[RunEvent]) {
        for event in events {
            if event.id > self.upto {
                self.observe(event);
                self.upto = event.id;
            }
        }
    }

    /// The history up to `upto` folded from `events`, what the first pass
    /// of a process read ([`first_pass_reads`]) of the events up to there:
    /// the hold, the deferrals and the thresholds are those of every event
    /// up to `upto`, and so is each run's work clock but for the runs
    /// [`Self::unsettled`] names, whose own events [`Self::refold`] folds.
    /// The events need not be in order nor once; those past `upto` are not
    /// folded.
    pub fn seeded(upto: EventId, events: Vec<RunEvent>) -> Self {
        SeededFold::new(upto, events).finish()
    }

    /// The runs whose work the history measured and whose finish it did
    /// not see: a run finishes at any event whose `status` is `failed` or
    /// `interrupted`, of whatever kind, so the first pass reads their own
    /// events whole ([`Self::refold`]), those of the runs not finished too,
    /// as one may finish before a pass refolds it. A run with no work
    /// measured counts in no median until it is read again.
    pub fn unsettled(&self) -> Vec<RunId> {
        let mut runs: Vec<RunId> = self
            .works
            .iter()
            .filter(|(_, (_, clock))| clock.unsettled())
            .map(|(run, _)| run.clone())
            .collect();
        runs.sort();
        runs
    }

    /// Fold `run`'s work clock again from `events`, its own events as the
    /// queue reads them, up to [`Self::upto`]: what every event up to there
    /// folds to for it. A pass refolds each run not finished, so a run the
    /// first pass did not read whole that comes back has its clock of
    /// every event.
    pub fn refold(&mut self, run: &RunId, events: &[RunEvent]) {
        self.works.remove(run);
        for event in events {
            if event.id <= self.upto && event.run_id.as_ref() == Some(run) {
                self.observe_work(event);
            }
        }
    }

    /// Move [`Self::upto`] on to `upto`, with no event between.
    pub fn reach(&mut self, upto: EventId) {
        self.upto = self.upto.max(upto);
    }

    /// Fold one event by the rules the judgments read the whole history
    /// by: [`claim_hold::hold_spans`]' for the claim hold,
    /// [`claim_defer::deferral_spans`]' for the deferrals,
    /// [`StallConfig::loaded`](super::super::stall::StallConfig::loaded)'s
    /// for the thresholds, and [`WorkClock`]'s.
    fn observe(&mut self, event: &RunEvent) {
        let opener = self
            .hold
            .as_ref()
            .map(|held| held.payload.get("supervisor").and_then(Value::as_str));
        if claim_hold::hold_end(claim_hold::CLAIMS, opener, event).is_some() {
            self.hold = (event.kind == claim_hold::CLAIMS.held).then(|| event.clone());
        }
        if let Some((task, _)) = claim_defer::deferral_end(event) {
            if event.kind == EventKind::ClaimDeferred {
                self.deferrals.insert(task, event.clone());
            } else {
                self.deferrals.remove(&task);
            }
        }
        if event.kind == EventKind::StallConfigLoaded {
            self.stall = Some(event.clone());
        }
        self.observe_work(event);
    }

    /// Fold `event` into its run's [`WorkClock`], the run's task being
    /// that of its first event.
    fn observe_work(&mut self, event: &RunEvent) {
        if let (Some(run), Some(task)) = (&event.run_id, event.task_id) {
            self.works
                .entry(run.clone())
                .or_insert((task, WorkClock::default()))
                .1
                .observe(event, timestamp_millis(&event.created_at));
        }
    }

    /// The events the judgments of now read at [`Self::upto`]: those kept
    /// with `runs` (the events of the runs not finished, as the queue reads
    /// them), each up to [`Self::upto`] and once, ascending. Judged as the
    /// whole history up to there, they give the same.
    pub fn events(&self, runs: impl IntoIterator<Item = RunEvent>) -> Vec<RunEvent> {
        let mut events: Vec<RunEvent> = self
            .hold
            .iter()
            .chain(self.deferrals.values())
            .chain(&self.stall)
            .cloned()
            .chain(runs.into_iter().filter(|event| event.id <= self.upto))
            .collect();
        events.sort_by_key(|event| event.id);
        events.dedup_by_key(|event| event.id);
        events
    }

    /// [`outlier_thresholds`](super::super::stats::outlier_thresholds) of
    /// `runs` on the history folded, `goals` mapping a task to its goal.
    pub fn outlier_thresholds(
        &self,
        goals: &HashMap<TaskId, Option<GoalId>>,
        runs: &[LiveRun],
    ) -> HashMap<RunId, i64> {
        let works = self
            .works
            .iter()
            .map(|(run, (task, clock))| (run, (goals.get(task).copied().flatten(), clock)))
            .collect();
        work_outliers(&works, runs)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: Some(TaskId::new(1)),
            goal_id: None,
            run_id: Some(RunId::new("0aa21145-c873-4cec-aee3-ee7f07f52e4a".to_owned()).unwrap()),
            kind: kind.into(),
            payload,
            created_at: format!("2026-10-30T00:00:{:02}.000Z", id % 60),
            actor: None,
        }
    }

    /// Histories with every part open: the hold of supervisor `a`, task 1
    /// deferred and the thresholds, the run of the probes not claimed,
    /// claimed with no receipt, and with its receipt, so an event of a kind
    /// its fold reads changes one of them.
    fn open() -> Vec<LiveHistory> {
        let base = [
            event(1, "claim_held", json!({"supervisor": "a"})),
            event(2, "claim_deferred", json!({})),
            event(3, "stall_config_loaded", json!({})),
        ];
        let steps = [
            vec![],
            vec![
                event(4, "run_claimed", json!({})),
                event(5, "claim_deferred", json!({})),
            ],
            vec![
                event(4, "run_claimed", json!({})),
                event(5, "claim_deferred", json!({})),
                event(6, "receipt_observed", json!({})),
            ],
        ];
        steps
            .into_iter()
            .map(|steps| {
                let mut history = LiveHistory::default();
                history.mark(&base);
                history.mark(&steps);
                history
            })
            .collect()
    }

    /// Whether the fold of an event of `kind`, on the run and task of the
    /// open histories and naming their hold's supervisor, changes one.
    fn changes(kind: EventKind, payload: Value) -> bool {
        open().into_iter().any(|before| {
            let mut after = before.clone();
            after.mark(&[event(10, kind.as_str(), payload.clone())]);
            let mut moved = before;
            moved.reach(EventId::new(10));
            after != moved
        })
    }

    /// The kinds the first pass reads are those the fold of every event
    /// reads: an event of any other kind changes nothing, and one of each
    /// of them changes the history. A kind the fold starts to read and the
    /// first pass does not fails here.
    #[test]
    fn the_first_pass_reads_the_kinds_the_fold_reads() {
        let read = first_pass_reads().kinds();
        let folded: BTreeSet<EventKind> = EventKind::ALL
            .iter()
            .copied()
            .filter(|kind| changes(*kind, json!({"supervisor": "a"})))
            .collect();
        assert_eq!(folded, read);
    }

    /// Of a kind the first pass does not read, an event whose `status` is
    /// `failed` or `interrupted` finishes its run: the first pass reads
    /// whole the runs whose finish only such an event can tell.
    #[test]
    fn any_kind_finishes_a_run_by_its_status() {
        let mut history = LiveHistory::default();
        history.mark(&[
            event(1, "run_claimed", json!({})),
            event(2, "receipt_observed", json!({})),
        ]);
        let run = event(1, "run_claimed", json!({})).run_id.unwrap();
        assert_eq!(history.unsettled(), std::slice::from_ref(&run));
        let kind = EventKind::ReviewFinished;
        assert!(!first_pass_reads().kinds().contains(&kind));
        assert!(!changes(kind, json!({"status": "passed"})));
        for status in ["failed", "interrupted"] {
            let mut finished = history.clone();
            finished.mark(&[event(3, kind.as_str(), json!({"status": status}))]);
            assert!(finished.unsettled().is_empty());
        }
    }

    /// Seeded from the reads of the first pass and refolded on the run's
    /// own events, the history is the fold of every event; the reads
    /// past the cursor are not folded.
    #[test]
    fn a_seeded_history_refolded_is_the_fold_of_every_event() {
        let all = [
            event(1, "claim_held", json!({"supervisor": "a"})),
            event(2, "run_claimed", json!({})),
            event(3, "agent_started", json!({})),
            event(4, "receipt_observed", json!({})),
            event(5, "review_finished", json!({"status": "failed"})),
            event(6, "stall_config_loaded", json!({})),
            event(7, "claim_deferred", json!({})),
        ];
        let mut whole = LiveHistory::default();
        whole.mark(&all);
        let read: Vec<RunEvent> = all
            .iter()
            .filter(|event| [1, 2, 4, 6, 7].contains(&event.id.as_i64()))
            .chain([&event(8, "claim_resumed", json!({}))])
            .rev()
            .cloned()
            .collect();
        let mut seeded = LiveHistory::seeded(EventId::new(7), read);
        assert_ne!(seeded, whole);
        let run = all[1].run_id.clone().unwrap();
        assert_eq!(seeded.unsettled(), std::slice::from_ref(&run));
        seeded.refold(&run, &all);
        assert_eq!(seeded, whole);
    }

    /// A run's claim in the events given whole and in a page, and its
    /// receipt in the next page: its clock is measured from the first fold
    /// of its claim, the claim counts once, and the fold is that of every
    /// event, whichever page each event falls in.
    #[test]
    fn a_fold_by_pages_is_the_fold_of_every_event() {
        let run = |id: i64, kind: &str, run: &str, at: i64| RunEvent {
            run_id: Some(RunId::new(run).unwrap()),
            created_at: crate::domain::marks::utc_text(at * 1000),
            ..event(id, kind, json!({}))
        };
        let (first, second) = (
            "3aa21145-c873-4cec-aee3-ee7f07f52e4a",
            "4bb21145-c873-4cec-aee3-ee7f07f52e4a",
        );
        let claim = run(2, "run_claimed", first, 100);
        // A later claim of the run: only the first starts the clock.
        let again = run(4, "run_claimed", first, 300);
        let all = [
            event(1, "claim_deferred", json!({})),
            claim.clone(),
            event(3, "claim_held", json!({"supervisor": "a"})),
            again.clone(),
            run(5, "receipt_observed", first, 500),
            // A run only an event given whole names: it has a work clock
            // in the fold of every event.
            run(6, "stall_nudged", second, 600),
            run(7, "run_integrated", first, 700),
            event(8, "stall_config_loaded", json!({})),
        ];
        let mut whole = LiveHistory::default();
        whole.mark(&all);
        whole.reach(EventId::new(9));
        let given = || {
            vec![
                all[7].clone(),
                all[5].clone(),
                all[0].clone(),
                all[2].clone(),
                claim.clone(),
                claim.clone(),
                event(10, "claim_resumed", json!({})),
            ]
        };
        let pages = [
            vec![claim.clone(), again.clone()],
            vec![all[4].clone(), all[6].clone()],
        ];
        // The pages cut at every place, and none; the pages' last event is
        // past the event given whole or short of it.
        for cut in 0..=4 {
            let work = [
                pages.concat()[..cut].to_vec(),
                pages.concat()[cut..].to_vec(),
            ];
            let mut fold = SeededFold::new(EventId::new(9), given());
            for page in &work {
                fold.page(page);
            }
            let folded = fold.finish();
            assert_eq!(folded, whole, "cut at {cut}");
            assert_eq!(folded.upto(), EventId::new(9));
        }
        // The same events all given whole.
        let mut every = given();
        every.extend(pages.concat());
        assert_eq!(LiveHistory::seeded(EventId::new(9), every), whole);
        // A page's event past where the fold ends is not folded.
        let mut fold = SeededFold::new(EventId::new(6), Vec::new());
        fold.page(&pages.concat());
        let cut = fold.finish();
        let mut short = LiveHistory::default();
        short.mark(&[claim, again, all[4].clone()]);
        short.reach(EventId::new(6));
        assert_eq!(cut, short);
    }
}
