//! The builder of the version's set ([`OMITTED_KINDS`]), which copies no
//! queue event of the set after C to a queue row, against the builder of
//! no kind (the one before it, "of now") on the same EventStore and the
//! same steps: after each step the ledger of the set read by the functions
//! of now gives the values the ledger of now's form (the same ledger, a row
//! of each omitted event in (C, S] given back, no coverage) gives; after a
//! step that cut no pass by its writes the two builders' values are the
//! same; and the builder of the set records every input the other did, its
//! R no earlier (ADR-t4644-1 decision 4).

use super::*;
use crate::application::ports::frozen::values;
use crate::domain::ledger::owner::{self, Owner};
use crate::domain::ledger::queue::{OMITTED_KINDS, Unwritten, queue_rows};
use crate::domain::ledger::session::written::omitted::{Omission, POINT_EVERY};
use crate::domain::ledger::settle;
use crate::domain::ledger::testkit::frozen::{Input, Step, THREE, inputs};

/// No kind omitted: the builder of now.
pub(super) fn none() -> Omission {
    Omission::of([], POINT_EVERY)
}

/// The version's set.
fn version() -> Omission {
    Omission::of_version()
}

/// The queue's events of the store's coverage of the set in its (C, S]:
/// those of no row.
pub(super) fn omitted_of(memory: &Memory) -> Vec<RunEvent> {
    let Some(coverage) = memory.omitted.borrow().clone() else {
        return Vec::new();
    };
    memory
        .events
        .borrow()
        .iter()
        .filter(|event| event.id > coverage.from && event.id <= coverage.through)
        .filter(|event| {
            coverage
                .kinds
                .iter()
                .any(|kind| kind == event.kind.as_str())
        })
        .filter(|event| owner::owner(event) == Owner::Queue)
        .cloned()
        .collect()
}

/// The ledger of now's form of `memory`'s: its events, rows and marks, no
/// coverage, and a queue row of each omitted event in (C, S] it lacks (one
/// it holds, written before a builder read again from its cursors' start
/// on rows of no mark, stays): `old`'s row of it when that store holds one
/// (the row the builder of now wrote), else the row [`queue_rows`] gives
/// it, settled as at `now_ms`.
pub(super) fn old_form(memory: &Memory, old: Option<&Memory>, now_ms: i64) -> Memory {
    let form = Memory::default();
    form.events.replace(memory.events.borrow().clone());
    form.rows.replace(memory.rows.borrow().clone());
    form.mark.replace(*memory.mark.borrow());
    form.rows_mark.replace(*memory.rows_mark.borrow());
    let events = memory.events.borrow();
    for event in omitted_of(memory) {
        let key = (LedgerKind::Queue, event.id.to_string());
        if form.rows.borrow().contains_key(&key) {
            continue;
        }
        let row = old
            .and_then(|old| old.rows.borrow().get(&key).cloned())
            .unwrap_or_else(|| {
                let up_to: Vec<RunEvent> = events
                    .iter()
                    .filter(|other| other.id <= event.id)
                    .cloned()
                    .collect();
                let mut row = queue_rows(
                    std::slice::from_ref(&event),
                    &up_to,
                    now_ms,
                    BUILT,
                    &Unwritten::default(),
                )
                .into_iter()
                .find(|row| row.key == key.1)
                .unwrap();
                row.is_final = settle::settlement(&row, row.inputs_through.last(), now_ms).settles;
                row
            });
        form.rows.borrow_mut().insert(key, row);
    }
    form
}

/// (a) `memory`'s values read by the functions of now are those of its
/// ledger of now's form ([`old_form`]) read by them too, the queue rows'
/// value that of the rows of now's form: the values of the form.
#[track_caller]
pub(super) fn of_old_form(
    memory: &Memory,
    old: Option<&Memory>,
    input: &Input,
    label: &str,
) -> Vec<(String, Value)> {
    let form = old_form(memory, old, ms(input.now));
    let of_form = values(&form, input);
    let rows = of_form
        .iter()
        .find(|(name, _)| name == "queue rows")
        .map(|(_, rows)| rows.clone())
        .unwrap();
    let mut now = values(memory, input);
    for (name, value) in &mut now {
        if name == "queue rows" {
            *value = rows.clone();
        }
    }
    for ((name, a), (other, b)) in now.iter().zip(&of_form) {
        assert_eq!(name, other, "{label}");
        assert_eq!(a, b, "{label}: {name}");
    }
    assert_eq!(now.len(), of_form.len(), "{label}");
    of_form
}

/// The inputs `memory` records: its rows' records and the omitted events of
/// its coverage's (C, S]. A run's or task's event of the set is one only
/// as its row's record.
fn recorded(memory: &Memory) -> BTreeSet<EventId> {
    memory
        .rows
        .borrow()
        .values()
        .flat_map(|row| row.records.iter().map(|event| event.id))
        .chain(omitted_of(memory).iter().map(|event| event.id))
        .collect()
}

fn r(memory: &Memory) -> Option<EventId> {
    rows_written(memory).unwrap().map(|cursors| cursors.events)
}

/// (d) The builder of the set (`new`) is not behind the builder of now
/// (`old`): it records every input `old` records and its R is no earlier;
/// with a coverage it moves (not stopped), S no earlier than `old`'s R too.
#[track_caller]
fn not_behind(new: (&Memory, &Builder), old: &Memory, label: &str) {
    let (memory, builder) = new;
    let lacks: Vec<EventId> = recorded(old)
        .difference(&recorded(memory))
        .copied()
        .collect();
    assert!(lacks.is_empty(), "{label}: {lacks:?} not recorded");
    assert!(
        r(memory) >= r(old),
        "{label}: R {:?} < {:?}",
        r(memory),
        r(old)
    );
    if !builder.omission_stopped()
        && let Some(coverage) = memory.omitted.borrow().as_ref()
    {
        assert!(
            Some(coverage.through) >= r(old),
            "{label}: S {:?} < R {:?}",
            coverage.through,
            r(old)
        );
    }
}

/// A store and its builder, of `omission`.
struct Side {
    memory: Memory,
    builder: Builder,
    omission: Omission,
    /// Whether the last pass left marks (cut by its writes) or failed.
    cut: bool,
}

impl Side {
    fn new(omission: Omission) -> Self {
        let mut builder = Builder::default();
        builder.set_omission(omission.clone());
        Self {
            memory: Memory::default(),
            builder,
            omission,
            cut: false,
        }
    }

    /// The builder a new process starts on the store, of its set.
    fn start(&mut self, secs: i64) {
        self.builder = start_builder(&self.memory, ms(secs)).unwrap();
        self.builder.set_omission(self.omission.clone());
    }

    /// Take `step` of `input` as the builder's steps of the frozen inputs do.
    fn step(&mut self, step: Step, input: &Input) {
        let memory = &self.memory;
        let cut = |built: Built| built.backlog > 0 || built.failure.is_some();
        match step {
            Step::Arrive(n) => {
                memory.events.replace(input.log.events[..n].to_vec());
            }
            Step::Pass(secs) => self.cut = cut(pass(memory, &mut self.builder, secs)),
            Step::Drain(secs) => {
                self.cut = cut(passes_to_drain(memory, &mut self.builder, secs));
            }
            Step::MarksGone => {
                memory.mark.replace(None);
                memory.rows_mark.replace(None);
            }
            Step::SessionWriteFails(secs) => {
                memory.batch_fails.set(1);
                pass(memory, &mut self.builder, secs);
                memory.batch_fails.set(0);
                self.cut = true;
            }
            Step::Start(secs) => self.start(secs),
            Step::FromTheStart => {
                self.builder = Builder::default();
                self.builder.set_omission(self.omission.clone());
            }
        }
    }
}

/// Whether the builder of now's last pass over `input` was cut by its
/// writes (left marks) or failed.
pub(super) fn ends_cut(input: &Input) -> bool {
    let mut old = Side::new(none());
    for step in &input.steps {
        old.step(*step, input);
    }
    old.cut
}

/// What a step of a pair left: cut (by either builder's writes) or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Round {
    Whole,
    Cut,
}

/// `input`'s steps by the builder of now (`old`) and by `new` on stores of
/// their own, `each` given both and how the step left them after each.
fn in_step(
    input: &Input,
    old: &mut Side,
    new: &mut Side,
    mut each: impl FnMut(usize, &Side, &Side, Round),
) {
    for (n, step) in input.steps.iter().enumerate() {
        old.step(*step, input);
        new.step(*step, input);
        let round = if old.cut || new.cut {
            Round::Cut
        } else {
            Round::Whole
        };
        each(n, old, new, round);
    }
}

/// The version's set is the three kinds of the Spike's holds.
#[test]
fn the_versions_set_is_the_three_kinds() {
    let mut kinds: Vec<&str> = OMITTED_KINDS.iter().map(|kind| kind.as_str()).collect();
    kinds.sort_unstable();
    assert_eq!(kinds, THREE);
    assert_eq!(version().kinds, THREE);
}

/// After every step of each frozen input, by both builders from their
/// cursors' start (C at 0): the values of the set's ledger are those of its
/// ledger of now's form (a), and it is not behind the builder of now (d).
/// The set's ledger holds no row of an omitted event, and some input
/// leaves an omitted event, of each kind.
#[test]
fn after_every_step_the_values_are_those_of_the_ledger_of_now_s_form() {
    let mut kinds = BTreeSet::new();
    for input in inputs() {
        let (mut old, mut new) = (Side::new(none()), Side::new(version()));
        in_step(&input, &mut old, &mut new, |n, old, new, _| {
            let label = format!("{} step {n}", input.name);
            of_old_form(&new.memory, Some(&old.memory), &input, &label);
            not_behind((&new.memory, &new.builder), &old.memory, &label);
            let omitted = omitted_of(&new.memory);
            let records = recorded_rows(&new.memory);
            assert!(
                omitted.iter().all(|event| !records.contains(&event.id)),
                "{label}"
            );
            kinds.extend(omitted.iter().map(|event| event.kind.as_str().to_owned()));
            if let Some(coverage) = new.memory.omitted.borrow().as_ref() {
                assert_eq!(coverage.from, EventId::new(0), "{label}");
            }
        });
    }
    assert_eq!(kinds, THREE.iter().map(|kind| (*kind).to_owned()).collect());
}

/// The ids of the records of `memory`'s rows.
fn recorded_rows(memory: &Memory) -> BTreeSet<EventId> {
    memory
        .rows
        .borrow()
        .values()
        .flat_map(|row| row.records.iter().map(|event| event.id))
        .collect()
}

/// Events after the frozen `reached`'s: the queue's of the set and of other
/// kinds and a run's of the set.
fn reached_and(after: &[(&str, Option<&str>)]) -> Input {
    let mut input = inputs()
        .into_iter()
        .find(|input| input.name == "reached")
        .unwrap();
    let last = input.log.events.len();
    let mut secs = input.now;
    for (kind, run) in after {
        secs += 1;
        input
            .log
            .add(run.map(|_| 1), *run, kind, json!({"target": "job:1"}), secs);
    }
    input.now = secs + 1;
    input.steps = vec![Step::Arrive(last), Step::Drain(input.now)];
    for n in last + 1..=input.log.events.len() {
        input.steps.extend([Step::Arrive(n), Step::Pass(input.now)]);
    }
    input.untils.push(input.log.last());
    input
}

/// (b) After a step that cut neither builder's pass: the values of both are
/// the same, `next_cursor` too (the set's queue rows those of now less the
/// rows of its omitted events): both caught up, and R at an event of the
/// set alone read, whose row only the builder of now writes (R at 100,
/// event 101 read and written). R and S move past the omitted events.
#[test]
fn after_a_step_not_cut_the_values_are_those_of_the_builder_of_now() {
    let mut whole = 0;
    let mut checked = |input: &Input| {
        let (mut old, mut new) = (Side::new(none()), Side::new(version()));
        in_step(input, &mut old, &mut new, |n, old, new, round| {
            if round == Round::Cut {
                return;
            }
            whole += 1;
            let label = format!("{} step {n}", input.name);
            let form = of_old_form(&new.memory, Some(&old.memory), input, &label);
            let of_old = values(&old.memory, input);
            assert_eq!(form.len(), of_old.len(), "{label}");
            for ((name, a), (_, b)) in form.iter().zip(&of_old) {
                assert_eq!(a, b, "{label}: {name}");
            }
            assert_eq!(r(&new.memory), r(&old.memory), "{label}");
        });
        (old, new)
    };
    for input in inputs() {
        checked(&input);
    }
    // R at 100, then only 101 (the queue's refusal) read: the builder of now
    // writes its row, the set's none; neither cut.
    let input = reached_and(&[("authorization_denied", None)]);
    let (old, new) = checked(&input);
    let last = input.log.last();
    let key = (LedgerKind::Queue, last.as_i64().to_string());
    assert!(old.memory.rows.borrow().contains_key(&key));
    assert!(!new.memory.rows.borrow().contains_key(&key));
    assert!(!old.cut && !new.cut);
    assert_eq!((r(&old.memory), r(&new.memory)), (Some(last), Some(last)));
    assert_eq!(new.memory.omitted.borrow().as_ref().unwrap().through, last);
    let position = values(&new.memory, &input);
    assert_eq!(
        position
            .iter()
            .find(|(name, _)| name == "stats position")
            .unwrap()
            .1["next_cursor"],
        json!(last.as_i64())
    );
    // Of each kind, the queue's and a run's: R and S past them all.
    let input = reached_and(&[
        ("telemetry_token_issued", None),
        ("mark_added", None),
        ("telemetry_token_issued", Some(run_id(1).as_str())),
        ("telemetry_token_revoked", None),
        ("authorization_denied", None),
    ]);
    let (_, new) = checked(&input);
    assert_eq!(r(&new.memory), Some(input.log.last()));
    assert_eq!(
        new.memory.omitted.borrow().as_ref().unwrap().through,
        input.log.last()
    );
    assert!(whole > 20, "{whole}");
}

/// (c) After a step that cut a pass by its writes the two builders' values
/// are not held the same, only (a) and (d) hold: from C at 100 the queue's
/// refusals 101 and 102 read in one page, a pass of one write: the builder
/// of now writes 101's row alone, its latest and `next_cursor` 101, and the
/// set's writes none, S 102, its latest and `next_cursor` 102. The frozen
/// inputs' cut steps too.
#[test]
fn after_a_step_cut_the_values_are_those_of_the_ledger_of_now_s_form() {
    let mut cut = 0;
    for input in inputs() {
        let (mut old, mut new) = (Side::new(none()), Side::new(version()));
        in_step(&input, &mut old, &mut new, |n, old, new, round| {
            if round == Round::Whole {
                return;
            }
            cut += 1;
            let label = format!("{} step {n}", input.name);
            of_old_form(&new.memory, Some(&old.memory), &input, &label);
            not_behind((&new.memory, &new.builder), &old.memory, &label);
        });
    }
    assert!(cut > 0);

    let mut input = reached_and(&[
        ("authorization_denied", None),
        ("authorization_denied", None),
    ]);
    let n = input.log.events.len();
    let (c, last) = (input.id(n - 2), input.log.last());
    input.steps = vec![Step::Arrive(n - 2), Step::Drain(input.now)];
    let (mut old, mut new) = (Side::new(none()), Side::new(none()));
    for step in &input.steps {
        old.step(*step, &input);
        new.step(*step, &input);
    }
    // The set's builder takes over from R (100): C there.
    new.omission = version();
    new.start(input.now);
    for side in [&old, &new] {
        side.memory.events.replace(input.log.events.clone());
        side.memory.clock.costs(PASS_TIME);
    }
    old.step(Step::Pass(input.now), &input);
    new.step(Step::Pass(input.now), &input);
    let coverage = new.memory.omitted.borrow().clone().unwrap();
    assert_eq!((coverage.from, coverage.through), (c, last));
    let key = |id: EventId| (LedgerKind::Queue, id.as_i64().to_string());
    for id in [input.id(n - 1), last] {
        assert!(!new.memory.rows.borrow().contains_key(&key(id)));
    }
    assert!(old.memory.rows.borrow().contains_key(&key(input.id(n - 1))));
    assert!(!old.memory.rows.borrow().contains_key(&key(last)));
    assert!(old.cut);
    let next = |memory: &Memory| {
        let values = values(memory, &input);
        let position = &values
            .iter()
            .find(|(name, _)| name == "stats position")
            .unwrap()
            .1;
        (position["upto"].clone(), position["next_cursor"].clone())
    };
    let first = json!(input.id(n - 1).as_i64());
    assert_eq!(next(&old.memory), (first.clone(), first));
    let last_json = json!(last.as_i64());
    assert_eq!(next(&new.memory), (last_json.clone(), last_json));
    of_old_form(&new.memory, Some(&old.memory), &input, "101 and 102");
    not_behind((&new.memory, &new.builder), &old.memory, "101 and 102");
}

/// The builder of now makes the ledger of no coverage, then the set's
/// builder of a new process takes it over at each step of each frozen
/// input (as a landing moves the store): C is where its first pass reads
/// from (R, or 0 with no R), the omitted events up to C stay rows, and
/// after every step (a) and (d) hold against the builder of now alone.
#[test]
fn the_set_s_builder_taking_over_the_ledger_of_now_covers_from_where_it_reads() {
    let mut taken = 0;
    for input in inputs() {
        for over in 1..input.steps.len() {
            let mut old = Side::new(none());
            let mut new = Side::new(none());
            let mut from = None;
            for (n, step) in input.steps.iter().enumerate() {
                if n == over {
                    new.omission = version();
                    new.start(input.now);
                    from = Some(new.builder.events);
                }
                old.step(*step, &input);
                new.step(*step, &input);
                if n < over {
                    // Both the builder of now's, on stores of their own.
                    continue;
                }
                let label = format!("{} over {over} step {n}", input.name);
                if let Some(coverage) = new.memory.omitted.borrow().as_ref() {
                    // Where its first pass read from, unless a step read
                    // from the cursors' start before it.
                    assert!(
                        Some(coverage.from) == from || coverage.from == EventId::new(0),
                        "{label}: C {:?} from {from:?}",
                        coverage.from
                    );
                    if coverage.from > EventId::new(0) {
                        taken += 1;
                    }
                }
                of_old_form(&new.memory, Some(&old.memory), &input, &label);
                not_behind((&new.memory, &new.builder), &old.memory, &label);
            }
        }
    }
    assert!(taken > 0);
}

/// The store a builder of the set stops on (R past S, moved by a builder
/// that knows no S; or a coverage of another set): C, S and the summaries
/// stay, the omitted events in (C, S] stay of no row and those after S are
/// queue rows' records as before. After every step the values are those of
/// the ledger of now's form (a), every input the builder of now records is
/// recorded (by the summaries kept and the rows after S), and R is no
/// earlier; S is not held to R.
#[test]
fn a_store_that_stops_the_set_s_builder_holds_c_to_s_and_writes_the_rows_after_s() {
    let input = reached_and(&[
        ("authorization_denied", None),
        ("telemetry_token_issued", None),
        ("authorization_denied", None),
        ("mark_added", None),
        ("telemetry_token_revoked", None),
        ("authorization_denied", None),
    ]);
    let n = input.log.events.len();
    let half = n - 6;
    for case in ["r past s", "other kinds"] {
        let mut old = Side::new(none());
        let mut new = Side::new(match case {
            "r past s" => version(),
            _ => Omission::of(["authorization_denied"], POINT_EVERY),
        });
        let arrive = |old: &mut Side, new: &mut Side, count: usize| {
            for side in [&mut *old, &mut *new] {
                side.step(Step::Arrive(count), &input);
                side.step(Step::Drain(input.now), &input);
            }
        };
        arrive(&mut old, &mut new, half - 3);
        let held = new.memory.omitted.borrow().clone().unwrap();
        if case == "r past s" {
            // A builder that knows no S moves R past it.
            new.omission = none();
            new.start(input.now);
            arrive(&mut old, &mut new, half);
            assert!(r(&new.memory) > Some(held.through));
        }
        new.omission = version();
        new.start(input.now);
        for count in half + 1..=n {
            arrive(&mut old, &mut new, count);
            let label = format!("{case} {count}");
            assert!(new.builder.omission_stopped(), "{label}");
            assert_eq!(new.memory.omitted.borrow().as_ref(), Some(&held), "{label}");
            of_old_form(&new.memory, Some(&old.memory), &input, &label);
            not_behind((&new.memory, &new.builder), &old.memory, &label);
        }
        let records = recorded_rows(&new.memory);
        for event in &input.log.events[half..] {
            let queue = owner::owner(event) == Owner::Queue;
            if queue && event.id > held.through {
                assert!(records.contains(&event.id), "{case}: {:?}", event.kind);
            }
        }
        for event in omitted_of(&new.memory) {
            assert!(!records.contains(&event.id), "{case}");
        }
        assert_eq!(r(&new.memory), Some(input.log.last()), "{case}");
    }
}

/// (vii) of the frozen inputs, its run's and task's events of the set and
/// the queue's in turn, a pass cut by its writes with the run's row and some
/// of the queue's rows written: the set's builder leaves the queue's of the
/// set after C of no row and writes the run's, task's and session's rows as
/// the builder of now does.
#[test]
fn the_rows_of_runs_tasks_and_sessions_are_those_of_now() {
    let input = inputs()
        .into_iter()
        .find(|input| input.name == "queue and run")
        .unwrap();
    let (mut old, mut new) = (Side::new(none()), Side::new(version()));
    let mut cut = false;
    in_step(&input, &mut old, &mut new, |n, old, new, round| {
        cut |= round == Round::Cut;
        let not_queue = |memory: &Memory| -> BTreeMap<(LedgerKind, String), LedgerRow> {
            memory
                .rows
                .borrow()
                .iter()
                .filter(|((kind, _), _)| *kind != LedgerKind::Queue)
                .map(|(key, row)| (key.clone(), row.clone()))
                .collect()
        };
        if round == Round::Whole {
            assert_eq!(not_queue(&old.memory), not_queue(&new.memory), "step {n}");
        }
        // Each run's or task's row the builder of now wrote is the set's
        // too, written as it is or later.
        for (key, row) in not_queue(&old.memory) {
            let theirs = new.memory.rows.borrow().get(&key).cloned();
            let theirs = theirs.unwrap_or_else(|| panic!("step {n}: {key:?} not written"));
            assert!(
                theirs.inputs_through.last() >= row.inputs_through.last(),
                "step {n}"
            );
        }
    });
    assert!(cut);
    // The runs' and tasks' events of the set: the rows of both record the
    // same of them.
    let theirs: BTreeSet<EventId> = input
        .log
        .events
        .iter()
        .filter(|event| THREE.contains(&event.kind.as_str()))
        .filter(|event| owner::owner(event) != Owner::Queue)
        .map(|event| event.id)
        .collect();
    assert!(theirs.len() > 60);
    let of = |memory: &Memory| -> BTreeSet<EventId> {
        recorded_rows(memory)
            .intersection(&theirs)
            .copied()
            .collect()
    };
    assert_eq!(of(&new.memory), of(&old.memory));
    assert!(!of(&new.memory).is_empty());
    // The queue's of the set after C (0) are no row's.
    let omitted = omitted_of(&new.memory);
    assert!(omitted.len() > 3);
    let records = recorded_rows(&new.memory);
    assert!(omitted.iter().all(|event| !records.contains(&event.id)));
}

/// A new process on a store of a coverage whose R of the queue rows and of
/// the other rows differ (a pass cut by its writes, frozen (ii)) replays
/// each stream from its own R: the omitted events after C it reads again
/// stay of no row, those up to C rows, and (a) and (d) hold once drained.
#[test]
fn a_split_restart_over_a_coverage_leaves_the_omitted_events_of_no_row() {
    let input = inputs()
        .into_iter()
        .find(|input| input.name == "cut")
        .unwrap();
    let (mut old, mut new) = (Side::new(none()), Side::new(version()));
    in_step(&input, &mut old, &mut new, |_, _, _, _| {});
    let mark = new.memory.rows_mark.borrow().unwrap();
    let (core, queue) = (mark.core.unwrap(), mark.queue.unwrap());
    assert_ne!(core.through.events, queue.through.events);
    let coverage = new.memory.omitted.borrow().clone().unwrap();
    let replayed = |event: &RunEvent| {
        event.id > core.through.events.min(queue.through.events)
            && event.id <= core.through.events.max(queue.through.events)
    };
    assert!(omitted_of(&new.memory).iter().any(replayed));
    for side in [&mut old, &mut new] {
        side.start(input.now);
        side.step(Step::Drain(input.now), &input);
        side.step(Step::Drain(input.now), &input);
    }
    assert_eq!(
        new.memory.omitted.borrow().as_ref().unwrap().from,
        coverage.from
    );
    let records = recorded_rows(&new.memory);
    assert!(
        omitted_of(&new.memory)
            .iter()
            .all(|event| !records.contains(&event.id))
    );
    of_old_form(&new.memory, Some(&old.memory), &input, "split restart");
    not_behind((&new.memory, &new.builder), &old.memory, "split restart");
}

/// The builder of now drained on `input`'s events: what the set's builder
/// is held not to be behind.
fn of_now_drained(input: &Input) -> Side {
    let mut old = Side::new(none());
    old.step(Step::Arrive(input.log.events.len()), input);
    old.step(Step::Drain(input.now), input);
    old
}

/// The queue's events of the set up to `c` of `input` have a row in
/// `memory`.
#[track_caller]
fn rows_up_to(memory: &Memory, input: &Input, c: EventId) {
    let rows = memory.rows.borrow();
    let of_set: Vec<&RunEvent> = input
        .log
        .events
        .iter()
        .filter(|event| event.id <= c && THREE.contains(&event.kind.as_str()))
        .filter(|event| owner::owner(event) == Owner::Queue)
        .collect();
    assert!(!of_set.is_empty());
    for event in of_set {
        let key = (LedgerKind::Queue, event.id.to_string());
        assert!(rows.contains_key(&key), "{key:?} has no row");
    }
}

/// The set's builder read the queue's refusals 101 and 102 past S (100) and
/// its write of them failed; a builder that knows no S then wrote 101's row
/// and R at 101, past S, so the set's builder's next pass stops. It reads
/// again what it left without rows and the stop copies: 101 and 102 get
/// their rows (102's of no other writer), R moves to 102, and no input the
/// builder of now records is left of neither a row nor the coverage.
#[test]
fn a_stop_after_reading_past_s_gives_the_events_after_s_their_rows() {
    let input = reached_and(&[
        ("authorization_denied", None),
        ("authorization_denied", None),
    ]);
    let n = input.log.events.len();
    let (s, first, last) = (input.id(n - 2), input.id(n - 1), input.log.last());
    let memory = Memory::default();
    let arrive = |count: usize| memory.events.replace(input.log.events[..count].to_vec());
    let mut set = Builder::default();
    set.set_omission(version());
    arrive(n - 2);
    drain(&memory, &mut set, input.now);
    assert_eq!(memory.omitted.borrow().as_ref().unwrap().through, s);
    arrive(n);
    memory.batch_fails.set(1);
    pass(&memory, &mut set, input.now);
    memory.batch_fails.set(0);
    assert_eq!(set.events, last);
    assert_eq!(memory.omitted.borrow().as_ref().unwrap().through, s);
    arrive(n - 1);
    let mut knows_no_s = start_builder(&memory, ms(input.now)).unwrap();
    knows_no_s.set_omission(none());
    drain(&memory, &mut knows_no_s, input.now);
    assert_eq!(r(&memory), Some(first));
    let key = |id: EventId| (LedgerKind::Queue, id.to_string());
    assert!(!memory.rows.borrow().contains_key(&key(last)));
    arrive(n);
    drain(&memory, &mut set, input.now);
    assert!(set.omission_stopped());
    assert_eq!(set.regain_after(), None);
    for id in [first, last] {
        assert!(memory.rows.borrow().contains_key(&key(id)), "{id:?}");
    }
    assert_eq!(r(&memory), Some(last));
    assert_eq!(memory.omitted.borrow().as_ref().unwrap().through, s);
    let old = of_now_drained(&input);
    not_behind((&memory, &set), &old.memory, "stop");
    of_old_form(&memory, Some(&old.memory), &input, "stop");
}

/// Two builders of the set on a store of no coverage, of different C, each
/// way: the one whose write the store refuses takes the other's coverage.
/// Taking a later C (its own 0, the other's a new process's on a store of
/// split R whose first pass its writes cut, so that the store's R of the
/// queue rows is before that C), it reads again only the events after that
/// R (those up to it are recorded) and gives those it left without rows up
/// to that C their rows: every queue event of the set up to C has one. Taking an earlier C (its own at
/// R, the other's 0), the events it holds as rows are covered too, which
/// changes no value. Either way, once drained, none of the builder of now's
/// inputs is left of neither a row nor the coverage, and the values are
/// those of the ledger of now's form.
#[test]
fn a_builder_taking_a_coverage_of_another_c_records_every_input_either_way() {
    let input = inputs()
        .into_iter()
        .find(|input| input.name == "reached")
        .unwrap();
    let n = input.log.events.len();
    let now = input.now;
    let old = of_now_drained(&input);

    // A later C: a new process's on a store of split R (frozen (ii), the
    // queue rows' R behind the others'), its first pass cut by its writes:
    // its C the joined cursor, some of the queue rows of the set after the
    // queue rows' R up to C not written, and that R before C.
    let cut = inputs()
        .into_iter()
        .find(|input| input.name == "cut")
        .unwrap();
    let cut_old = of_now_drained(&cut);
    let mut split = Side::new(none());
    for step in &cut.steps {
        split.step(*step, &cut);
    }
    let memory = split.memory;
    let mark = memory.rows_mark.borrow().unwrap();
    let (core_r, queue_r) = (
        mark.core.unwrap().through.events,
        mark.queue.unwrap().through.events,
    );
    assert!(queue_r < core_r);
    let cut_now = cut.now;
    let mut from_start = Builder::default();
    from_start.set_omission(version());
    memory
        .before_steps
        .replace(Some(Box::new(move |memory: &Memory| {
            let mut later = start_builder(memory, ms(cut_now)).unwrap();
            later.set_omission(version());
            let rows =
                u32::try_from(crate::domain::ledger::testkit::frozen::WRITES_PER_PASS).unwrap();
            memory.clock.costs(PASS_TIME);
            let built = pass(memory, &mut later, cut_now);
            memory.clock.costs(PASS_TIME / rows);
            assert_eq!(built.failure, None);
        })));
    pass(&memory, &mut from_start, cut_now);
    assert!(memory.before_steps.borrow().is_none(), "the other pass ran");
    let c = memory.omitted.borrow().as_ref().unwrap().from;
    let stored = memory
        .rows_mark
        .borrow()
        .unwrap()
        .queue
        .unwrap()
        .through
        .events;
    assert!(
        queue_r <= stored && stored < c,
        "{queue_r:?} {stored:?} {c:?}"
    );
    // It reads again after the store's R of the queue rows alone.
    let scans = memory.scans.borrow().len();
    drain(&memory, &mut from_start, cut_now);
    assert!(
        memory.scans.borrow()[scans..]
            .iter()
            .all(|(name, after, _)| *name != "events" || *after >= stored.as_i64()),
        "{:?}",
        &memory.scans.borrow()[scans..]
    );
    assert_eq!(memory.omitted.borrow().as_ref().unwrap().from, c);
    rows_up_to(&memory, &cut, c);
    not_behind((&memory, &from_start), &cut_old.memory, "a later C");
    of_old_form(&memory, Some(&cut_old.memory), &cut, "a later C");

    // An earlier C: the store's from the start, written first.
    let memory = Memory::default();
    memory.events.replace(input.log.events[..n / 2].to_vec());
    let mut of_now = Builder::default();
    of_now.set_omission(none());
    drain(&memory, &mut of_now, now);
    let mut from_r = start_builder(&memory, ms(now)).unwrap();
    from_r.set_omission(version());
    memory.events.replace(input.log.events.clone());
    memory
        .before_steps
        .replace(Some(Box::new(move |memory: &Memory| {
            let mut from_start = Builder::default();
            from_start.set_omission(version());
            let built = build_pass_limited(
                memory,
                &mut from_start,
                (ms(now), BUILT),
                Retain::default(),
                None,
                (n / 2, n / 2 + 5, SESSION_BYTES_PER_WRITE),
                Timing::default(),
            );
            assert_eq!(built.failure, None);
        })));
    pass(&memory, &mut from_r, now);
    assert!(memory.before_steps.borrow().is_none(), "the other pass ran");
    assert_eq!(
        memory.omitted.borrow().as_ref().unwrap().from,
        EventId::new(0)
    );
    drain(&memory, &mut from_r, now);
    assert_eq!(
        memory.omitted.borrow().as_ref().unwrap().from,
        EventId::new(0)
    );
    assert_eq!(r(&memory), Some(input.log.last()));
    not_behind((&memory, &from_r), &old.memory, "an earlier C");
    of_old_form(&memory, Some(&old.memory), &input, "an earlier C");
}

/// The stop race over a landing attempt: the attempt starts before S, the
/// queue's refusal 101 is read inside it, `run_integrated` 102 ends it and
/// the refusal 103 follows, the last timed after 102 (the clock going on)
/// or long before it (turned back). The set's builder reads them, its write
/// fails, a builder that knows no S writes up to 101, and the set's builder
/// stops and gives 101 and 103 their rows. The attempt's row keeps its
/// through at 102, as the fold of every event gives it. A write of 103's
/// row that fails leaves the queue's R before 103 at the latest time read
/// up to 102, as the read of the events marks it. Then a new process
/// drains: none of the builder of now's inputs is left of neither a row nor
/// the coverage, and the values are those of the ledger of now's form.
#[test]
fn a_regained_row_moves_no_attempt_back_and_holds_r_at_the_time_read_before_it() {
    for back in [false, true] {
        let mut input = inputs()
            .into_iter()
            .find(|input| input.name == "reached")
            .unwrap();
        let t = input.now + 1;
        let run = run_id(9);
        input.log.worked(9, &run, t);
        input
            .log
            .phase(
                9,
                &run,
                t + 10,
                crate::domain::run_phase::Phase::Landing,
                "x",
            )
            .add(
                Some(9),
                Some(&run),
                "integration_started",
                json!({}),
                t + 10,
            );
        let start = input.log.last();
        let s_at = input.log.events.len();
        input
            .log
            .add(None, None, "authorization_denied", json!({}), t + 11);
        input.log.add(
            Some(9),
            Some(&run),
            "run_integrated",
            json!({"from": "validating", "to": "integrated", "status": "integrated"}),
            t + 12,
        );
        let ended = input.log.last();
        let at = if back { t - 100 } else { t + 13 };
        input
            .log
            .add(None, None, "authorization_denied", json!({}), at);
        input.now = t + 200;
        let n = input.log.events.len();
        let (first, last) = (input.id(n - 2), input.log.last());
        let label = if back { "turned back" } else { "going on" };

        let memory = Memory::default();
        let arrive = |count: usize| memory.events.replace(input.log.events[..count].to_vec());
        let mut set = Builder::default();
        set.set_omission(version());
        arrive(s_at);
        drain(&memory, &mut set, input.now);
        let held = (memory.omitted.borrow().clone(), r(&memory));
        arrive(n);
        memory.refuses.set(true);
        memory.refuses_batch_marks.set(true);
        pass(&memory, &mut set, input.now);
        memory.refuses.set(false);
        memory.refuses_batch_marks.set(false);
        assert_eq!(set.events, last, "{label}");
        assert_eq!(
            (memory.omitted.borrow().clone(), r(&memory)),
            held,
            "{label}"
        );
        arrive(n - 2);
        let mut knows_no_s = start_builder(&memory, ms(input.now)).unwrap();
        knows_no_s.set_omission(none());
        drain(&memory, &mut knows_no_s, input.now);
        assert_eq!(r(&memory), Some(first), "{label}");
        arrive(n);
        let key = |id: EventId| (LedgerKind::Queue, id.to_string());
        memory.unwritable.replace(Some(key(last)));
        let built = pass(&memory, &mut set, input.now);
        assert!(built.failure.is_some(), "{label}");
        assert!(set.omission_stopped(), "{label}");
        assert!(memory.rows.borrow().contains_key(&key(first)), "{label}");
        let queue = memory.ledger_marks().unwrap().unwrap().rows.unwrap();
        let queue = queue.position(true).unwrap();
        let read_ms = input
            .log
            .events
            .iter()
            .filter(|event| event.id <= ended)
            .filter_map(|event| crate::domain::stats::timestamp_millis(&event.created_at))
            .max()
            .unwrap();
        assert_eq!(
            (queue.through.events, queue.read_ms),
            (ended, read_ms),
            "{label}"
        );
        memory.unwritable.replace(None);
        let mut again = start_builder(&memory, ms(input.now)).unwrap();
        again.set_omission(version());
        drain(&memory, &mut again, input.now);
        let old = of_now_drained(&input);
        let slot = |memory: &Memory| memory.rows.borrow().get(&key(start)).cloned();
        let (theirs, now) = (slot(&old.memory).unwrap(), slot(&memory).unwrap());
        assert_eq!(theirs.subject, now.subject, "{label}");
        assert_eq!(
            (theirs.inputs_through.events, ended),
            (now.inputs_through.events, ended),
            "{label}"
        );
        assert!(memory.rows.borrow().contains_key(&key(last)), "{label}");
        assert_eq!(r(&memory), Some(last), "{label}");
        not_behind((&memory, &again), &old.memory, label);
        of_old_form(&memory, Some(&old.memory), &input, label);
    }
}

/// The store holds `input`'s first `count` events.
fn arrive(memory: &Memory, input: &Input, count: i64) {
    memory
        .events
        .replace(input.log.events[..usize::try_from(count).unwrap()].to_vec());
}

/// The event reads of `memory` since its `from`th scan are all after `r`.
#[track_caller]
fn reads_after(memory: &Memory, from: usize, r: i64) {
    let scans = memory.scans.borrow();
    assert!(
        scans[from..]
            .iter()
            .all(|(name, after, _)| *name != "events" || *after >= r),
        "{:?}",
        &scans[from..]
    );
}

/// Its event scans so far.
fn scans(memory: &Memory) -> usize {
    memory.scans.borrow().len()
}

/// The store, the set's builder and the store's coverage as the builder
/// is about to take another writer's coverage behind its cursor, with no
/// stop. It read 300 events (refusals and others in turn) by C 0, none
/// written; another writer then wrote the coverage of C 100 and S 150 and
/// R at 150.
fn taking_a_coverage_behind() -> (Input, Memory, Builder, Coverage) {
    let mut input = inputs()
        .into_iter()
        .find(|input| input.name == "reached")
        .unwrap();
    let t = input.now + 1;
    let start = input.log.events.len();
    for k in 0..i64::try_from(300 - start).unwrap() {
        let kind = if k % 2 == 0 {
            "authorization_denied"
        } else {
            "mark_added"
        };
        input.log.add(None, None, kind, json!({"mark": "m"}), t + k);
    }
    input.now = t + 400;
    assert_eq!(input.log.last(), EventId::new(300));
    let id = EventId::new;
    let memory = Memory::default();
    // Read all by C 0, none of it written.
    let mut set = Builder::default();
    set.set_omission(version());
    arrive(&memory, &input, 300);
    memory.refuses.set(true);
    memory.refuses_batch_marks.set(true);
    pass(&memory, &mut set, input.now);
    memory.refuses.set(false);
    memory.refuses_batch_marks.set(false);
    assert_eq!((set.events, r(&memory)), (id(300), None));
    // Another writer's coverage of C 100 and S 150.
    arrive(&memory, &input, 100);
    let mut of_now = Builder::default();
    of_now.set_omission(none());
    drain(&memory, &mut of_now, input.now);
    arrive(&memory, &input, 150);
    let mut other = start_builder(&memory, ms(input.now)).unwrap();
    other.set_omission(version());
    drain(&memory, &mut other, input.now);
    let held = memory.omitted.borrow().clone().unwrap();
    assert_eq!((held.from, held.through), (id(100), id(150)));
    assert_eq!(r(&memory), Some(id(150)));
    arrive(&memory, &input, 300);
    (input, memory, set, held)
}

/// The store, the set's builder and the store's coverage after a change of
/// the rule while a read again is pending, the set's builder about to stop.
/// It took the coverage of [`taking_a_coverage_behind`] and read again up
/// to 200 before the next page failed, leaving the refusals in (100, 200]
/// without rows by that coverage; a builder that knows no S then wrote up
/// to 151, R past S. Every read again was after the store's R.
fn stopping_to_read_again() -> (Input, Memory, Builder, Coverage) {
    let (input, memory, mut set, held) = taking_a_coverage_behind();
    let id = EventId::new;
    // The set's builder takes it and reads again up to 200, a page at a
    // time, until a read fails.
    let from = scans(&memory);
    memory.reads_fail_from.set(Some(id(200)));
    let built = build_pass_limited(
        &memory,
        &mut set,
        (ms(input.now), BUILT),
        Retain::default(),
        None,
        (20, 10_000, SESSION_BYTES_PER_WRITE),
        Timing::default(),
    );
    assert!(built.failure.is_some());
    assert!(
        matches!(set.regain_after(), Some((next, through)) if next >= id(200) && through == id(300)),
        "{:?}",
        set.regain_after()
    );
    reads_after(&memory, from, 150);
    memory.reads_fail_from.set(None);
    // A builder that knows no S writes up to 151.
    writes_up_to(&memory, &input, 151);
    arrive(&memory, &input, 300);
    (input, memory, set, held)
}

/// A builder that knows no S, a new process on `memory`, writes every
/// input up to `count`: R there.
fn writes_up_to(memory: &Memory, input: &Input, count: i64) {
    arrive(memory, input, count);
    let mut knows_no_s = start_builder(memory, ms(input.now)).unwrap();
    knows_no_s.set_omission(none());
    drain(memory, &mut knows_no_s, input.now);
    assert_eq!(r(memory), Some(EventId::new(count)));
}

/// The set's builder drained on `memory` after [`stopping_to_read_again`]:
/// it stopped, kept the coverage it took, and every refusal after S has its
/// row; R at the last event, and no input of the builder of now is left of
/// neither a row nor the coverage.
#[track_caller]
fn drained_after_reading_again(
    (input, memory, set): (&Input, &Memory, &mut Builder),
    held: &Coverage,
    label: &str,
) {
    drain(memory, set, input.now);
    assert!(set.omission_stopped(), "{label}");
    assert_eq!(set.regain_after(), None, "{label}");
    assert_eq!(memory.omitted.borrow().as_ref(), Some(held), "{label}");
    let rows = memory.rows.borrow().clone();
    for event in &input.log.events {
        if event.kind.as_str() == "authorization_denied" && event.id > held.through {
            let key = (LedgerKind::Queue, event.id.to_string());
            assert!(rows.contains_key(&key), "{label}: {key:?} has no row");
        }
    }
    assert_eq!(r(memory), Some(EventId::new(300)), "{label}");
    let old = of_now_drained(input);
    not_behind((memory, set), &old.memory, label);
    of_old_form(memory, Some(&old.memory), input, label);
}

/// A change of the rule while a read again is pending
/// ([`stopping_to_read_again`]): the set's builder's next pass stops. It
/// reads again after the store's R alone (the inputs up to it are
/// recorded), a pass's events at most: a pass short of the end writes
/// nothing and leaves R. Every refusal after S (152 to 200 too) gets its
/// row, and once drained no input of the builder of now is left of neither
/// a row nor the coverage.
#[test]
fn a_rule_changed_while_reading_again_weighs_what_was_read_again() {
    let (input, memory, mut set, held) = stopping_to_read_again();
    let id = EventId::new;
    // A pass reads again 40 events at most: short of the end, it writes
    // nothing.
    let from = scans(&memory);
    let rows_before = memory.rows.borrow().clone();
    let built = build_pass_limited(
        &memory,
        &mut set,
        (ms(input.now), BUILT),
        Retain::default(),
        None,
        (20, 40, SESSION_BYTES_PER_WRITE),
        Timing::default(),
    );
    assert_eq!(built.failure, None);
    assert!(set.omission_stopped());
    assert!(
        matches!(set.regain_after(), Some((next, _)) if next > id(151) && next < id(300)),
        "{:?}",
        set.regain_after()
    );
    assert_eq!(r(&memory), Some(id(151)));
    assert_eq!(*memory.rows.borrow(), rows_before);
    drained_after_reading_again(
        (&input, &memory, &mut set),
        &held,
        "changed while reading again",
    );
    reads_after(&memory, from, 151);
}

/// A pass whose read again its events or its time cut
/// ([`stopping_to_read_again`]) read a page at least, wrote nothing, left
/// R and the rows as they were and says it was cut behind the latest
/// event, so that the next pass starts at once rather than after the
/// interval ([`goes_on`], [`pass_due`]); the passes that follow at once
/// read the rest again and drain.
#[test]
fn a_read_again_cut_by_its_events_or_its_time_goes_on_at_once() {
    let id = EventId::new;
    let interval = Duration::from_secs(crate::domain::ledger::builder::INTERVAL_SECS);
    for by_time in [false, true] {
        let label = if by_time {
            "by its time"
        } else {
            "by its events"
        };
        let (input, memory, mut set, held) = stopping_to_read_again();
        let rows_before = memory.rows.borrow().clone();
        let leaping = super::pass_time::Leaping(std::sync::Mutex::new(Instant::now()));
        let (per_pass, timing) = if by_time {
            (10_000, Timing(Some(&leaping)))
        } else {
            (40, Timing::default())
        };
        let (mut passes, mut last) = (0, None);
        loop {
            let built = build_pass_limited(
                &memory,
                &mut set,
                (ms(input.now), BUILT),
                Retain::default(),
                None,
                (20, per_pass, SESSION_BYTES_PER_WRITE),
                timing,
            );
            assert_eq!(built.failure, None, "{label}");
            passes += 1;
            let Some((next, _)) = set.regain_after() else {
                break;
            };
            // Cut short of the end: a page at least read again, nothing
            // written, R where it was, the next pass at once.
            assert!(
                last.is_none_or(|last| next > last),
                "{label}: {last:?} to {next}"
            );
            last = Some(next);
            assert!(built.work.events > 0, "{label}");
            assert!(built.cut, "{label}");
            assert_eq!(built.work.latest, 300, "{label}");
            assert!(built.backlog > 0, "{label}");
            assert!(goes_on(&built), "{label}: {:?}", built.marks.rows);
            let at = Instant::now();
            assert!(pass_due(Some(at), at, interval, goes_on(&built)), "{label}");
            assert_eq!(r(&memory), Some(id(151)), "{label}");
            assert_eq!(*memory.rows.borrow(), rows_before, "{label}");
            assert!(passes < 20, "{label}: passes {passes}");
        }
        assert!(passes > 2, "{label}: passes {passes}");
        drained_after_reading_again((&input, &memory, &mut set), &held, label);
    }
}

/// A read again pending ([`stopping_to_read_again`], then a pass of 40
/// events past R 151) goes on after the store's R as each pass's start
/// reads it, the rule not changed: another writer that knows no S wrote up
/// to 250, or up to 300 where the read again ends, and the set's builder
/// reads no event again up to there (none at all at 300) and drains, every
/// input recorded.
#[test]
fn a_read_again_pending_goes_on_after_the_r_another_writes() {
    let id = EventId::new;
    for other_r in [250, 300] {
        let label = format!("R {other_r}");
        let (input, memory, mut set, held) = stopping_to_read_again();
        let built = build_pass_limited(
            &memory,
            &mut set,
            (ms(input.now), BUILT),
            Retain::default(),
            None,
            (20, 40, SESSION_BYTES_PER_WRITE),
            Timing::default(),
        );
        assert_eq!(built.failure, None, "{label}");
        let Some((next, _)) = set.regain_after() else {
            panic!("{label}: nothing pending");
        };
        assert!(next < id(250), "{label}: {next}");
        writes_up_to(&memory, &input, other_r);
        arrive(&memory, &input, 300);
        let from = scans(&memory);
        set.begin_coverage(memory.ledger_marks().unwrap().as_ref());
        if other_r < 300 {
            assert_eq!(set.regain_after(), Some((id(other_r), id(300))), "{label}");
        } else {
            assert_eq!(set.regain_after(), None, "{label}");
        }
        drained_after_reading_again((&input, &memory, &mut set), &held, &label);
        reads_after(&memory, from, other_r);
    }
}

/// The coverage of another C taken behind the cursor, with no stop
/// ([`taking_a_coverage_behind`]): the read again of the rows and then the
/// read of the events after its S up to the cursor, both after the store's
/// R, are within each pass's events (40) or its time (a page, the clock
/// leaping [`PASS_TIME`] at each read). A pass short of the end of either
/// is cut, writes nothing (the store's coverage, R and rows stay) and the
/// next starts at once; the read up to the cursor too goes over passes,
/// from the S it folded up to. Once read, the builder writes S at its
/// cursor, the refusals after C of no row, and records every input.
#[test]
fn a_coverage_taken_behind_is_read_up_to_within_each_pass() {
    let id = EventId::new;
    for by_time in [false, true] {
        let label = if by_time {
            "by its time"
        } else {
            "by its events"
        };
        let (input, memory, mut set, held) = taking_a_coverage_behind();
        let rows_before = memory.rows.borrow().clone();
        let leaping = super::pass_time::Leaping(std::sync::Mutex::new(Instant::now()));
        let (per_pass, most, timing) = if by_time {
            (10_000, 20, Timing(Some(&leaping)))
        } else {
            (40, 40, Timing::default())
        };
        let from = scans(&memory);
        let (mut passes, mut catching_up) = (0, 0);
        loop {
            let scanned = scans(&memory);
            let built = build_pass_limited(
                &memory,
                &mut set,
                (ms(input.now), BUILT),
                Retain::default(),
                None,
                (20, per_pass, SESSION_BYTES_PER_WRITE),
                timing,
            );
            passes += 1;
            assert_eq!(built.failure, None, "{label}");
            assert!(!set.omission_stopped(), "{label}");
            // The events read after a position, the pass's events.
            let read: usize = memory.scans.borrow()[scanned..]
                .iter()
                .filter(|(name, _, _)| *name == "events")
                .map(|(_, _, read)| read)
                .sum();
            assert!(
                read > 0 && read <= most,
                "{label}: pass {passes} read {read}"
            );
            assert_eq!(built.work.events, read, "{label}: pass {passes}");
            if set.regain_after().is_none() && set.coverage_behind().is_none() {
                break;
            }
            if set.regain_after().is_none() {
                catching_up += 1;
            }
            assert!(built.cut, "{label}");
            assert_eq!(built.work.latest, 300, "{label}");
            assert!(goes_on(&built), "{label}: {:?}", built.marks.rows);
            assert_eq!(r(&memory), Some(id(150)), "{label}");
            assert_eq!(memory.omitted.borrow().as_ref(), Some(&held), "{label}");
            assert_eq!(*memory.rows.borrow(), rows_before, "{label}");
            assert!(passes < 40, "{label}: passes {passes}");
        }
        assert!(catching_up > 1, "{label}: {catching_up} passes caught up");
        drain(&memory, &mut set, input.now);
        reads_after(&memory, from, 150);
        let coverage = memory.omitted.borrow().clone().unwrap();
        assert_eq!(
            (coverage.from, coverage.through),
            (id(100), id(300)),
            "{label}"
        );
        assert_eq!(r(&memory), Some(id(300)), "{label}");
        let rows = memory.rows.borrow().clone();
        for event in &input.log.events {
            let key = (LedgerKind::Queue, event.id.to_string());
            if event.kind.as_str() == "authorization_denied" && event.id > held.from {
                assert!(!rows.contains_key(&key), "{label}: {key:?} has a row");
            }
        }
        let old = of_now_drained(&input);
        not_behind((&memory, &set), &old.memory, label);
        of_old_form(&memory, Some(&old.memory), &input, label);
    }
}

/// [`taking_a_coverage_behind`] with 40 events past the builder's cursor
/// (300) not read yet: the pass that ends the read again and the read up
/// to the cursor reads on within the events (40) and the time (a page, the
/// clock leaping [`PASS_TIME`] at each read) they left it. By its time it
/// reads none past 300; by its events, no more than the pass's. It is cut
/// short of the latest event and the next pass starts at once
/// ([`goes_on`]) and reads on. Once drained, S and R are at the last event
/// and every input is recorded.
#[test]
fn the_pass_that_ends_a_read_again_reads_on_within_what_it_left() {
    let id = EventId::new;
    for by_time in [false, true] {
        let label = if by_time {
            "by its time"
        } else {
            "by its events"
        };
        let (mut input, memory, mut set, _) = taking_a_coverage_behind();
        let t = input.now - 100;
        for k in 0..40 {
            let kind = if k % 2 == 0 {
                "authorization_denied"
            } else {
                "mark_added"
            };
            input.log.add(None, None, kind, json!({"mark": "m"}), t + k);
        }
        assert_eq!(input.log.last(), id(340));
        arrive(&memory, &input, 340);
        let leaping = super::pass_time::Leaping(std::sync::Mutex::new(Instant::now()));
        let (per_pass, most, timing) = if by_time {
            (10_000, 20, Timing(Some(&leaping)))
        } else {
            (40, 40, Timing::default())
        };
        let mut passes = 0;
        let ended = loop {
            let scanned = scans(&memory);
            let built = build_pass_limited(
                &memory,
                &mut set,
                (ms(input.now), BUILT),
                Retain::default(),
                None,
                (20, per_pass, SESSION_BYTES_PER_WRITE),
                timing,
            );
            passes += 1;
            assert_eq!(built.failure, None, "{label}");
            let events = memory.scans.borrow()[scanned..]
                .iter()
                .filter(|(name, _, _)| *name == "events")
                .copied()
                .collect::<Vec<_>>();
            let read: usize = events.iter().map(|(_, _, read)| read).sum();
            let on: usize = events
                .iter()
                .filter(|(_, after, _)| *after >= 300)
                .map(|(_, _, read)| read)
                .sum();
            assert!(read <= most, "{label}: pass {passes} read {read}");
            assert!(built.cut, "{label}: pass {passes}");
            assert!(goes_on(&built), "{label}: pass {passes}");
            assert!(passes < 40, "{label}: passes {passes}");
            if set.regain_after().is_none() && set.coverage_behind().is_none() {
                break (on, set.events);
            }
            assert_eq!(on, 0, "{label}: pass {passes} read on while reading again");
        };
        // The pass that ended them read on within what they left.
        let (on, at) = ended;
        if by_time {
            assert_eq!((on, at), (0, id(300)), "{label}");
        } else {
            assert!(at < id(340), "{label}: {at}");
        }
        let scanned = scans(&memory);
        let built = build_pass_limited(
            &memory,
            &mut set,
            (ms(input.now), BUILT),
            Retain::default(),
            None,
            (20, per_pass, SESSION_BYTES_PER_WRITE),
            timing,
        );
        assert_eq!(built.failure, None, "{label}");
        assert!(
            memory.scans.borrow()[scanned..]
                .iter()
                .any(|(name, after, read)| *name == "events" && *after >= at.as_i64() && *read > 0),
            "{label}: the next pass read on"
        );
        assert!(set.events > at, "{label}");
        drain(&memory, &mut set, input.now);
        let coverage = memory.omitted.borrow().clone().unwrap();
        assert_eq!(
            (coverage.from, coverage.through),
            (id(100), id(340)),
            "{label}"
        );
        assert_eq!(r(&memory), Some(id(340)), "{label}");
        let old = of_now_drained(&input);
        not_behind((&memory, &set), &old.memory, label);
        of_old_form(&memory, Some(&old.memory), &input, label);
    }
}

/// A landing attempt open (its `integration_started` written as the slot's
/// row and every mark taken), then the queue's refusal of no time, of no
/// run, task or supervisor, alone: the slot's open attempt reads it, so by
/// the empty set and by the version's both builders write the slot's row
/// again through it, in a round not cut. The values of the two are the
/// same (the queue rows' given back the refusal's row), and so is the
/// slot's row.
#[test]
fn an_omitted_event_of_no_time_moves_the_open_slot_on_as_any_other() {
    let mut input = inputs()
        .into_iter()
        .find(|input| input.name == "reached")
        .unwrap();
    let t = input.now + 1;
    let run = run_id(9);
    input.log.worked(9, &run, t);
    input
        .log
        .phase(
            9,
            &run,
            t + 10,
            crate::domain::run_phase::Phase::Landing,
            "x",
        )
        .add(
            Some(9),
            Some(&run),
            "integration_started",
            json!({}),
            t + 10,
        );
    let start = input.log.last();
    let open = input.log.events.len();
    input
        .log
        .add(None, None, "authorization_denied", json!({}), t + 11);
    input.log.events.last_mut().unwrap().created_at = "not a time".into();
    let refusal = input.log.last();
    input.now = t + 20;
    input.untils.push(refusal);
    input.steps = vec![
        Step::Arrive(open),
        Step::Drain(input.now),
        Step::Arrive(open + 1),
        Step::Pass(input.now),
    ];
    let key = |id: EventId| (LedgerKind::Queue, id.to_string());
    let (mut old, mut new) = (Side::new(none()), Side::new(version()));
    let mut whole = 0;
    in_step(&input, &mut old, &mut new, |n, old, new, round| {
        let label = format!("step {n}");
        assert_eq!(round, Round::Whole, "{label}");
        let slot = |memory: &Memory| memory.rows.borrow().get(&key(start)).cloned();
        let theirs = slot(&old.memory);
        assert_eq!(slot(&new.memory), theirs, "{label}");
        if n == 3 {
            assert_eq!(theirs.unwrap().inputs_through.events, refusal, "{label}");
            assert!(!new.memory.rows.borrow().contains_key(&key(refusal)));
            assert!(old.memory.rows.borrow().contains_key(&key(refusal)));
        }
        let form = of_old_form(&new.memory, Some(&old.memory), &input, &label);
        assert_eq!(form, values(&old.memory, &input), "{label}");
        assert_eq!(r(&new.memory), r(&old.memory), "{label}");
        whole += 1;
    });
    assert_eq!(whole, 4);
}
