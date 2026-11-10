//! The builder's steps of each input of
//! [`crate::domain::ledger::testkit::frozen`] on the store in memory, and
//! the readers' values of the ledger it made held to those fixed
//! ([`crate::application::ports::frozen`]).

use super::*;
use crate::application::ports::frozen::{check, values};
use crate::domain::ledger::owner::{self, Owner};
use crate::domain::ledger::session::written::omitted::Omission;
use crate::domain::ledger::testkit::frozen::{Input, Step, THREE, inputs};
use crate::domain::ledger::testkit::omitted::counted;

/// The store after `input`'s steps.
fn built(input: &Input) -> Memory {
    built_with(input, None, |_, _| {})
}

/// The store after `input`'s steps by builders omitting `omission`'s kinds
/// (the version's with none), `after` given the store and the builder after
/// each step.
fn built_with(
    input: &Input,
    omission: Option<&Omission>,
    after: impl FnMut(&Memory, &Builder),
) -> Memory {
    built_on(Memory::default(), input, omission, after)
}

/// [`built_with`] on `memory` (its clock's cost of a write set,
/// [`Costed`]).
pub(super) fn built_on(
    memory: Memory,
    input: &Input,
    omission: Option<&Omission>,
    mut after: impl FnMut(&Memory, &Builder),
) -> Memory {
    let omit = |mut builder: Builder| {
        if let Some(omission) = omission {
            builder.set_omission(omission.clone());
        }
        builder
    };
    let mut builder = omit(Builder::default());
    for step in &input.steps {
        match *step {
            Step::Arrive(n) => {
                memory.events.replace(input.log.events[..n].to_vec());
            }
            Step::Pass(secs) => {
                pass(&memory, &mut builder, secs);
            }
            Step::Drain(secs) => {
                passes_to_drain(&memory, &mut builder, secs);
            }
            Step::MarksGone => {
                memory.mark.replace(None);
                memory.rows_mark.replace(None);
            }
            Step::SessionWriteFails(secs) => {
                memory.batch_fails.set(1);
                pass(&memory, &mut builder, secs);
                memory.batch_fails.set(0);
            }
            Step::Start(secs) => builder = omit(start_builder(&memory, ms(secs)).unwrap()),
            Step::FromTheStart => builder = omit(Builder::default()),
        }
        after(&memory, &builder);
    }
    memory
}

fn named<'a>(values: &'a [(String, Value)], name: &str) -> &'a Value {
    &values.iter().find(|(n, _)| n == name).unwrap().1
}

/// The ids of the records of the queue rows `memory` holds.
fn queue_records(memory: &Memory) -> BTreeSet<EventId> {
    memory
        .rows
        .borrow()
        .values()
        .filter(|row| row.kind == LedgerKind::Queue)
        .flat_map(|row| row.records.iter().map(|event| event.id))
        .collect()
}

/// Each input's readers' values of the ledger the builder of now makes
/// are those fixed.
#[test]
fn the_readers_values_of_each_input_are_those_fixed() {
    let all = inputs();
    let each: Vec<_> = all
        .iter()
        .map(|input| (input, values(&built(input), input)))
        .collect();
    check(&each);
}

/// The ledgers are those the inputs say: (ii) and (vii) (as its cut pass
/// leaves it) cut by the bound,
/// queue rows held past R, in (ii) of the three kinds and of other kinds,
/// in (vii) with an event of the three kinds of the queue's past R of no
/// row;
/// (iii) the run on its way with events before and after R; (iv) the store
/// ahead of the builder's read; (vi) no R; and (i) R at the last event,
/// (v) the clock turned back, (viii) a long span of no time.
#[test]
fn each_input_makes_the_ledger_it_says() {
    let all = inputs();
    let of = |name: &str| all.iter().find(|input| input.name == name).unwrap();
    for name in ["cut", "queue and run"] {
        // (vii) as its cut pass leaves it, before the pass after it.
        let mut input = of(name).clone();
        if name == "queue and run" {
            assert_eq!(input.steps.pop(), Some(Step::Pass(LATER)));
        }
        let memory = built(&input);
        let r = r_of(&memory).events;
        assert!(r < of(name).log.last(), "{name}");
        let past: Vec<LedgerRow> = memory
            .rows
            .borrow()
            .values()
            .filter(|row| row.kind == LedgerKind::Queue && row.inputs_through.events > r)
            .cloned()
            .collect();
        let of_three = |row: &LedgerRow| {
            row.subject
                .as_ref()
                .is_some_and(|subject| THREE.contains(&subject.kind.as_str()))
        };
        if name == "cut" {
            assert!(past.iter().any(of_three), "{name}");
            assert!(past.iter().any(|row| !of_three(row)), "{name}");
        } else {
            assert!(!past.is_empty(), "{name}");
            let records = queue_records(&memory);
            assert!(of(name).log.events.iter().any(|event| {
                event.id > r
                    && THREE.contains(&event.kind.as_str())
                    && owner::owner(event) == Owner::Queue
                    && !records.contains(&event.id)
            }));
        }
    }
    let reached = of("reached");
    assert_eq!(r_of(&built(reached)).events, reached.log.last());
    // (v) Events of the three kinds timed before an event before them.
    let back = &of("clock back").log.events;
    let at = |event: &RunEvent| crate::domain::stats::timestamp_millis(&event.created_at);
    assert!(back.iter().enumerate().any(|(n, event)| {
        THREE.contains(&event.kind.as_str())
            && back[..n].iter().any(|before| at(before) > at(event))
    }));
    // (viii) A long span of events of no time.
    let timeless = &of("timeless").log.events;
    assert!(timeless.iter().take(40).all(|event| at(event).is_none()));
    let way = of("on its way");
    let memory = built(way);
    let r = r_of(&memory).events;
    let run = run_id(3);
    let ids: Vec<EventId> = way
        .log
        .events
        .iter()
        .filter(|event| event.run_id.as_ref().is_some_and(|id| id.as_str() == run))
        .map(|event| event.id)
        .collect();
    assert!(ids.iter().any(|id| *id <= r) && ids.iter().any(|id| *id > r));
    assert!(!memory.rows.borrow().contains_key(&(LedgerKind::Run, run)));

    let ahead = of("store ahead");
    let memory = built(ahead);
    assert!(r_of(&memory).events < ahead.log.last());
    assert_eq!(memory.events.borrow().len(), ahead.log.events.len());

    let memory = built(of("no r"));
    assert_eq!(rows_written(&memory).unwrap(), None);
    let last = of("no r").log.events.last().unwrap().clone();
    assert_eq!(last.kind.as_str(), "authorization_denied");
    assert!(queue_records(&memory).contains(&last.id));
}

/// (vii) The three kinds of runs and tasks are no queue row's records;
/// the token of the run of no row is not the latest, the `next_cursor` or
/// the `queue_before` of any window.
#[test]
fn the_three_kinds_of_runs_and_tasks_are_no_queue_rows_records() {
    let input = inputs()
        .into_iter()
        .find(|input| input.name == "queue and run")
        .unwrap();
    let memory = built(&input);
    let records = queue_records(&memory);
    let owners = input.owners();
    let of_three = |id: EventId| {
        THREE.contains(
            &input.log.events[usize::try_from(id.as_i64() - 1).unwrap()]
                .kind
                .as_str(),
        )
    };
    let others: Vec<EventId> = owners
        .iter()
        .filter(|(id, owner)| *owner != Owner::Queue && of_three(*id))
        .map(|(id, _)| *id)
        .collect();
    assert!(others.len() > 60);
    assert!(others.iter().all(|id| !records.contains(id)));
    // Each kind of the three in turn by id: one of the queue's, one of a
    // run's or task's, one of the queue's, those of the queue's held.
    for kind in THREE {
        let of_kind: Vec<(EventId, Owner)> = input
            .log
            .events
            .iter()
            .filter(|event| event.kind.as_str() == kind)
            .map(|event| (event.id, owner::owner(event)))
            .collect();
        let in_turn = of_kind.windows(3).any(|three| {
            three[0].1 == Owner::Queue
                && three[1].1 != Owner::Queue
                && three[2].1 == Owner::Queue
                && records.contains(&three[0].0)
                && !records.contains(&three[1].0)
                && records.contains(&three[2].0)
        });
        assert!(in_turn, "{kind}");
    }
    // The run of no row: its token, between two refusals of the queue's
    // whose rows are held.
    let n = input.log.events.len();
    let token = input.id(n - 3);
    assert_eq!(
        input.log.events[n - 4].kind.as_str(),
        "telemetry_token_issued"
    );
    assert_eq!(owners[n - 4], (token, Owner::Run));
    for after in [n - 2, n] {
        assert_eq!(owners[after - 1].1, Owner::Queue);
        assert!(records.contains(&input.id(after)));
    }
    assert!(records.iter().any(|id| *id < token));
    assert!(!records.contains(&token));
    assert!(
        !memory
            .rows
            .borrow()
            .contains_key(&(LedgerKind::Run, run_id(7)))
    );
    let values = values(&memory, &input);
    assert_eq!(
        named(&values, "stats position")["upto"],
        json!(input.id(n).as_i64())
    );
    // A time cursor at the token's time answers the last event before run
    // 7's claim (run 6's revocation), of a row held.
    assert_eq!(
        named(&values, "stats --since t9221 position")["after"],
        json!(input.id(n - 5).as_i64())
    );
    for (name, value) in &values {
        // A window `--until` an event ends at that event, whatever it is.
        let fields: &[&str] = if name.contains("--until") {
            &["next_cursor", "after"]
        } else {
            &["next_cursor", "upto", "after"]
        };
        for field in fields {
            if let Some(id) = value.get(*field) {
                assert_ne!(id, &json!(token.as_i64()), "{name} {field}");
            }
        }
        if name.starts_with("queue_before") {
            assert_ne!(value, &json!(token.as_i64()), "{name}");
        }
    }
}

/// (viii) Events of each of the three kinds of the queue's whose time does
/// not parse, each after a timed one of its kind: the last is the latest,
/// the time cursor at each timed one's time answers the timed one, so does
/// `queue_before`, and the window without `--since` starts at the first
/// time that parses, past forty of no time.
#[test]
fn an_event_of_no_time_is_the_latest_and_no_time_cursors_answer() {
    let input = inputs()
        .into_iter()
        .find(|input| input.name == "timeless")
        .unwrap();
    let memory = built(&input);
    let n = input.log.events.len();
    let records = queue_records(&memory);
    let owners = input.owners();
    let values = values(&memory, &input);
    assert_eq!(
        named(&values, "stats position")["upto"],
        json!(input.id(n).as_i64())
    );
    // Of each kind, one timed and the next of no time, the queue's.
    for (k, time) in [(n - 5, 4_000), (n - 3, 4_500), (n - 1, 5_000)] {
        let (timed, timeless) = (&input.log.events[k - 1], &input.log.events[k]);
        assert_eq!(timed.kind, timeless.kind);
        assert!(THREE.contains(&timeless.kind.as_str()));
        assert_eq!(owners[k].1, Owner::Queue);
        assert!(records.contains(&timeless.id));
        let at = named(&values, &format!("stats --since t{time} position"));
        assert_eq!(at["after"], json!(timed.id.as_i64()), "t{time}");
        assert_eq!(
            named(&values, &format!("queue_before t{time}")),
            &json!(timed.id.as_i64()),
            "t{time}"
        );
    }
    let kinds: BTreeSet<&str> = input.log.events[..40]
        .iter()
        .map(|event| event.kind.as_str())
        .collect();
    assert_eq!(kinds, THREE.into_iter().collect());
    assert!(
        input.log.events[..40]
            .iter()
            .all(|event| records.contains(&event.id))
    );
    // The first forty events have no time: the window starts at the first
    // that has one.
    let first = &input.log.events[40];
    assert_eq!(
        named(&values, "stats position")["window_ms"][0],
        json!(crate::domain::stats::timestamp_millis(&first.created_at).unwrap())
    );
}

/// A point every so many events of a kind, few enough that the inputs
/// place several.
const EVERY: u64 = 3;

/// The three kinds omitted, as a builder of the next version omits them.
fn three() -> Omission {
    Omission::of(THREE, EVERY)
}

/// With the three kinds covered, after every step of each input the store
/// holds C at 0 (each input's builder reads from the first event), S at
/// the position read, and each kind's summary and points those counted of
/// the input's events in (C, S]: (vii)'s runs' and tasks' events of the
/// three kinds in the points' count alone, (viii)'s events of no time in L
/// alone, (v)'s clock turned back in D. The readers' values are those
/// fixed: no reader reads the coverage, and the rows are those of now.
#[test]
fn the_coverage_of_each_input_is_counted_of_c_to_s_after_every_step() {
    let all = inputs();
    let mut each = Vec::new();
    for input in &all {
        let mut covered = 0;
        let memory = built_with(input, Some(&three()), |memory, _| {
            let Some(coverage) = memory.omitted.borrow().clone() else {
                return;
            };
            covered += 1;
            assert_eq!(coverage.from, EventId::new(0), "{}", input.name);
            let events = memory.events.borrow();
            let (summaries, points) = counted(
                &events,
                &coverage.kinds,
                (coverage.from, coverage.through),
                EVERY,
            );
            assert_eq!(coverage.summaries, summaries, "{}", input.name);
            let held: Vec<_> = memory.points.borrow().values().cloned().collect();
            assert_eq!(held, points, "{}", input.name);
        });
        assert!(covered > 0, "{}", input.name);
        each.push((input, values(&memory, input)));
    }
    check(&each);

    let of = |name: &str| all.iter().find(|input| input.name == name).unwrap();
    // (i) S at the last event read, where R is.
    let input = of("reached");
    let memory = built_with(input, Some(&three()), |_, _| {});
    let s = memory.omitted.borrow().as_ref().unwrap().through;
    assert_eq!(
        (s, r_of(&memory).events),
        (input.log.last(), input.log.last())
    );
    // (vii) The runs' and tasks' events of the three kinds are in no L, T,
    // M or D (those counted of the queue's alone), and in the points' count.
    let input = of("queue and run");
    let memory = built_with(input, Some(&three()), |_, _| {});
    let coverage = memory.omitted.borrow().clone().unwrap();
    let theirs: Vec<&RunEvent> = input
        .log
        .events
        .iter()
        .filter(|event| THREE.contains(&event.kind.as_str()))
        .filter(|event| owner::owner(event) != Owner::Queue)
        .collect();
    assert!(theirs.len() > 60);
    for kind in THREE {
        let summary = &coverage.summaries[kind];
        let of_kind = input
            .log
            .events
            .iter()
            .filter(|event| event.kind.as_str() == kind);
        assert_eq!(
            summary.seen,
            u64::try_from(of_kind.clone().count()).unwrap(),
            "{kind}"
        );
        let queue: Vec<&RunEvent> = of_kind
            .filter(|event| owner::owner(event) == Owner::Queue)
            .collect();
        assert_eq!(summary.last, queue.last().map(|event| event.id), "{kind}");
        for event in theirs.iter().filter(|event| event.kind.as_str() == kind) {
            assert_ne!(summary.last, Some(event.id), "{kind}");
            assert_ne!(summary.timed, Some(event.id), "{kind}");
        }
    }
    // The last token, a run's of no row, is no kind's L; a point may fall on
    // it, its values the queue's before it.
    let token = input.id(input.log.events.len() - 3);
    assert!(
        coverage
            .summaries
            .values()
            .all(|summary| summary.last != Some(token))
    );

    // (viii) Of each kind, the last event has no time: it is L, and T is the
    // timed one before it; M is that one's time.
    let input = of("timeless");
    let memory = built_with(input, Some(&three()), |_, _| {});
    let coverage = memory.omitted.borrow().clone().unwrap();
    let n = input.log.events.len();
    for k in [n - 5, n - 3, n - 1] {
        let (timed, timeless) = (&input.log.events[k - 1], &input.log.events[k]);
        let summary = &coverage.summaries[timeless.kind.as_str()];
        assert_eq!(summary.last, Some(timeless.id));
        assert_eq!(summary.timed, Some(timed.id));
        assert_eq!(
            summary.max_ms,
            crate::domain::stats::timestamp_millis(&timed.created_at)
        );
    }

    // (v) The clock turned back past other kinds' events: each kind's last
    // event, timed before a mark before it, is its T and its time M, and D
    // is 0, as each kind's own times rise (D is of a kind's events alone).
    let input = of("clock back");
    let memory = built_with(input, Some(&three()), |_, _| {});
    let coverage = memory.omitted.borrow().clone().unwrap();
    for kind in THREE {
        let last = input
            .log
            .events
            .iter()
            .rfind(|event| event.kind.as_str() == kind)
            .unwrap();
        let summary = &coverage.summaries[kind];
        assert_eq!(summary.timed, Some(last.id), "{kind}");
        assert_eq!(
            summary.max_ms,
            crate::domain::stats::timestamp_millis(&last.created_at),
            "{kind}"
        );
        assert_eq!(summary.back_ms, 0, "{kind}");
    }
}

/// The version's builder omits no kind: it writes no coverage and no
/// point, and reads no mark for one.
#[test]
fn a_builder_of_no_kind_writes_no_coverage() {
    for input in inputs() {
        let memory = built(&input);
        assert_eq!(*memory.omitted.borrow(), None, "{}", input.name);
        assert!(memory.points.borrow().is_empty(), "{}", input.name);
    }
}

/// Same deterministic EventStore, with the old single-R restart
/// certificate and with split certificates: fully caught-up rows and all
/// stats, kpi and report values from the common readers agree.
#[test]
fn split_and_single_r_builders_catch_up_to_the_same_rows_and_reader_values() {
    for input in inputs()
        .into_iter()
        .filter(|input| matches!(input.name, "cut" | "queue and run"))
    {
        let split = built(&input);
        let single = built_with(&input, None, |memory, _| {
            if let Some(mark) = memory.rows_mark.borrow_mut().as_mut() {
                mark.core = None;
                mark.queue = None;
            }
        });
        drain(
            &split,
            &mut start_builder(&split, ms(input.now)).unwrap(),
            input.now,
        );
        drain(
            &single,
            &mut start_builder(&single, ms(input.now)).unwrap(),
            input.now,
        );
        assert_eq!(
            *split.rows.borrow(),
            *single.rows.borrow(),
            "{}",
            input.name
        );
        assert_eq!(
            values(&split, &input),
            values(&single, &input),
            "{}",
            input.name
        );
    }
}

/// The store after `input`'s steps by builders omitting the three kinds
/// from step `from` on (the builder of then sets them: C is where its next
/// pass reads from), with the queue rows of the omitted events in (C, S]
/// taken out, as a builder that omits them writes none; and the rows taken
/// out. The coverage, its points and every other row stay.
fn without_omitted_rows(input: &Input, from: usize) -> (Memory, Vec<LedgerRow>) {
    let memory = Memory::default();
    let mut builder = Builder::default();
    let omit = |builder: &mut Builder, n: usize| {
        if n >= from {
            builder.set_omission(three());
        }
    };
    for (n, step) in input.steps.iter().enumerate() {
        omit(&mut builder, n);
        match *step {
            Step::Arrive(count) => {
                memory.events.replace(input.log.events[..count].to_vec());
            }
            Step::Pass(secs) => {
                pass(&memory, &mut builder, secs);
            }
            Step::Drain(secs) => {
                passes_to_drain(&memory, &mut builder, secs);
            }
            Step::MarksGone => {
                memory.mark.replace(None);
                memory.rows_mark.replace(None);
            }
            Step::SessionWriteFails(secs) => {
                memory.batch_fails.set(1);
                pass(&memory, &mut builder, secs);
                memory.batch_fails.set(0);
            }
            Step::Start(secs) => {
                builder = start_builder(&memory, ms(secs)).unwrap();
                omit(&mut builder, n);
            }
            Step::FromTheStart => {
                builder = Builder::default();
                omit(&mut builder, n);
            }
        }
    }
    let Some(coverage) = memory.omitted.borrow().clone() else {
        return (memory, Vec::new());
    };
    let covered = |row: &LedgerRow| {
        row.kind == LedgerKind::Queue
            && row
                .subject
                .as_ref()
                .is_some_and(|subject| THREE.contains(&subject.kind.as_str()))
            && !row.records.is_empty()
            && row.records.iter().all(|event| {
                event.id > coverage.from
                    && event.id <= coverage.through
                    && owner::owner(event) == Owner::Queue
            })
    };
    let mut taken = Vec::new();
    memory.rows.borrow_mut().retain(|_, row| {
        let out = covered(row);
        if out {
            taken.push(row.clone());
        }
        !out
    });
    (memory, taken)
}

/// `values` with the queue rows of `left` given back the rows taken out,
/// in the order the store holds them: what the store held before.
fn with_rows_given_back(
    mut values: Vec<(String, Value)>,
    memory: &Memory,
    taken: &[LedgerRow],
) -> Vec<(String, Value)> {
    let mut queue: Vec<LedgerRow> = memory
        .rows
        .borrow()
        .values()
        .filter(|row| row.kind == LedgerKind::Queue)
        .cloned()
        .chain(taken.iter().cloned())
        .collect();
    queue.sort_by(|a, b| a.key.cmp(&b.key));
    let rows: Vec<Value> = queue
        .iter()
        .map(|row| serde_json::to_value(row).unwrap())
        .collect();
    for (name, value) in &mut values {
        if name == "queue rows" {
            *value = Value::Array(rows.clone());
        }
    }
    values
}

/// The ledger of each input with the queue rows of the three kinds' omitted
/// events in (C, S] taken out and the coverage of them held, read by the
/// functions of now, gives every reader the values fixed of the ledger that
/// holds those rows (the queue rows' value of the rows left with those
/// taken out given back, so that the other rows are those fixed too): with
/// C at 0, and with C past the first events where the inputs' builder sets
/// the kinds after its first pass, the omitted events up to C left as queue
/// rows. The reader omits no kind of its own (this version's set is empty):
/// it reads the kinds of the coverage the store holds.
#[test]
fn the_ledger_without_the_omitted_rows_gives_the_values_fixed() {
    assert!(crate::domain::ledger::queue::OMITTED_KINDS.is_empty());
    let all = inputs();
    let mut past_c = 0;
    for from in [0, 2] {
        let mut each = Vec::new();
        for input in &all {
            let (memory, taken) = without_omitted_rows(input, from);
            let held = memory.omitted.borrow().clone();
            let Some(coverage) = held else {
                // The builder set the kinds after its last pass.
                assert!(from > 0 && taken.is_empty(), "{}", input.name);
                each.push((input, values(&memory, input)));
                continue;
            };
            if from == 0 {
                assert_eq!(coverage.from, EventId::new(0), "{}", input.name);
                assert!(!taken.is_empty(), "{}", input.name);
            } else if coverage.from > EventId::new(0)
                && !taken.is_empty()
                && queue_records(&memory).iter().any(|id| {
                    *id <= coverage.from
                        && THREE.contains(
                            &input.log.events[usize::try_from(id.as_i64() - 1).unwrap()]
                                .kind
                                .as_str(),
                        )
                })
            {
                past_c += 1;
            }
            let read = values(&memory, input);
            each.push((input, with_rows_given_back(read, &memory, &taken)));
        }
        check(&each);
    }
    assert!(past_c > 0);
}

/// (vii) and (viii) on the ledger without the omitted rows: the token of
/// the run of no row stays out of every window's edges (its event is no
/// omitted one, the queue's records of no row), and the events of no time
/// are the latest and no time cursor's answer.
#[test]
fn without_the_omitted_rows_the_runs_tokens_and_the_events_of_no_time_stay_out() {
    let all = inputs();
    let of = |name: &str| all.iter().find(|input| input.name == name).unwrap();
    let input = of("queue and run");
    let (memory, taken) = without_omitted_rows(input, 0);
    let n = input.log.events.len();
    let token = input.id(n - 3);
    assert!(taken.iter().any(|row| row.records[0].id == input.id(n)));
    let read = values(&memory, input);
    assert_eq!(
        named(&read, "stats position")["upto"],
        json!(input.id(n).as_i64())
    );
    assert_eq!(
        named(&read, "stats --since t9221 position")["after"],
        json!(input.id(n - 5).as_i64())
    );
    for (name, value) in &read {
        let fields: &[&str] = if name.contains("--until") {
            &["next_cursor", "after"]
        } else {
            &["next_cursor", "upto", "after"]
        };
        for field in fields {
            if let Some(id) = value.get(*field) {
                assert_ne!(id, &json!(token.as_i64()), "{name} {field}");
            }
        }
        if name.starts_with("queue_before") {
            assert_ne!(value, &json!(token.as_i64()), "{name}");
        }
    }

    let input = of("timeless");
    let (memory, taken) = without_omitted_rows(input, 0);
    let n = input.log.events.len();
    assert!(taken.len() >= 40);
    let read = values(&memory, input);
    assert_eq!(
        named(&read, "stats position")["upto"],
        json!(input.id(n).as_i64())
    );
    for (k, time) in [(n - 5, 4_000), (n - 3, 4_500), (n - 1, 5_000)] {
        let timed = &input.log.events[k - 1];
        let at = named(&read, &format!("stats --since t{time} position"));
        assert_eq!(at["after"], json!(timed.id.as_i64()), "t{time}");
        assert_eq!(
            named(&read, &format!("queue_before t{time}")),
            &json!(timed.id.as_i64()),
            "t{time}"
        );
    }
    let first = &input.log.events[40];
    assert_eq!(
        named(&read, "stats position")["window_ms"][0],
        json!(crate::domain::stats::timestamp_millis(&first.created_at).unwrap())
    );
}

/// (vii) with the token of the run of no row (its task canceled before it)
/// the last event and S: the stored rows' records stop at the event before
/// it. Without
/// the omitted rows the readers' values are those of the ledger that holds
/// them read by the functions of now (no coverage, so the values of
/// before), and the token is no window's latest, `next_cursor` or
/// `queue_before`.
#[test]
fn the_token_of_a_run_of_no_row_as_s_is_no_edge() {
    let mut input = inputs()
        .into_iter()
        .find(|input| input.name == "queue and run")
        .unwrap();
    // Its events from run 7's claim: the claim, the token, a refusal, the
    // task's cancel, a refusal. Keep the claim, then the cancel, then the
    // token last, each at the time of the one it takes the place of.
    let n = input.log.events.len();
    let events = &mut input.log.events;
    assert_eq!(events[n - 5].kind.as_str(), "run_claimed");
    let mut token = events[n - 4].clone();
    let mut cancel = events[n - 2].clone();
    assert_eq!(cancel.kind.as_str(), "task_status_changed");
    events.truncate(n - 4);
    (cancel.id, cancel.created_at) = (token.id, token.created_at.clone());
    token.id = EventId::new(token.id.as_i64() + 1);
    token.created_at = crate::domain::marks::utc_text(ms(9_222));
    let token_id = token.id;
    events.extend([cancel, token]);
    let token = token_id;
    let all = input.log.events.len();
    assert_eq!(input.log.last(), token);
    for step in &mut input.steps {
        if let Step::Arrive(count) = step {
            *count = (*count).min(all);
        }
    }
    input.untils = vec![token, EventId::new(token.as_i64() - 1)];
    let whole = built(&input);
    assert!(
        !whole
            .rows
            .borrow()
            .contains_key(&(LedgerKind::Run, run_id(7)))
    );
    assert!(!queue_records(&whole).contains(&token));
    let before = values(&whole, &input);
    let (memory, taken) = without_omitted_rows(&input, 0);
    let coverage = memory.omitted.borrow().clone().unwrap();
    assert_eq!(coverage.through, token);
    assert!(!taken.is_empty());
    let now = with_rows_given_back(values(&memory, &input), &memory, &taken);
    assert_eq!(now, before);
    for (name, value) in &now {
        let fields: &[&str] = if name.contains("--until") {
            &["next_cursor", "after"]
        } else {
            &["next_cursor", "upto", "after"]
        };
        for field in fields {
            if let Some(id) = value.get(*field) {
                assert_ne!(id, &json!(token.as_i64()), "{name} {field}");
            }
        }
        if name.starts_with("queue_before") {
            assert_ne!(value, &json!(token.as_i64()), "{name}");
        }
    }
}
