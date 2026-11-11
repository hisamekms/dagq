//! The builder's steps of each input of
//! [`crate::domain::ledger::testkit::frozen`] on the store in memory, and
//! the readers' values of the ledger it made held to those fixed
//! ([`crate::application::ports::frozen`]).

use super::omitting::{ends_cut, none, of_old_form};
use super::*;
use crate::application::ports::frozen::{check, differs, values};
use crate::domain::ledger::owner::{self, Owner};
use crate::domain::ledger::session::written::omitted::Omission;
use crate::domain::ledger::testkit::frozen::{Input, Step, THREE, inputs};
use crate::domain::ledger::testkit::omitted::counted;

/// The store after `input`'s steps by the builder of now (no kind
/// omitted, [`none`]): the ledger the values were fixed of.
fn built(input: &Input) -> Memory {
    built_with(input, Some(&none()), |_, _| {})
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

/// The three kinds omitted, as the version's builder omits them, points
/// closer than its.
fn three() -> Omission {
    Omission::of(THREE, EVERY)
}

/// With the three kinds covered, after every step of each input the store
/// holds C at 0 (each input's builder reads from the first event), S at
/// the position read, and each kind's summary and points those counted of
/// the input's events in (C, S]: (vii)'s runs' and tasks' events of the
/// three kinds in the points' count alone, (viii)'s events of no time in L
/// alone, (v)'s clock turned back in D. The readers' values of the ledger
/// it makes are those of its ledger of now's form (a row of each omitted
/// event in (C, S] given back, no coverage), and those the values fixed.
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
        let old = built(input);
        each.push((input, of_old_form(&memory, Some(&old), input, input.name)));
    }
    check_ended_whole(&each);

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

/// The builder of no kind (of now) writes no coverage and no point.
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
        let single = built_with(&input, Some(&none()), |memory, _| {
            if let Some(mark) = memory.rows_mark.borrow_mut().as_mut() {
                mark.core = None;
                mark.queue = None;
            }
        });
        let of_now = |memory: &Memory| {
            let mut builder = start_builder(memory, ms(input.now)).unwrap();
            builder.set_omission(none());
            builder
        };
        drain(&split, &mut of_now(&split), input.now);
        drain(&single, &mut of_now(&single), input.now);
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

/// The store after `input`'s steps by the builder of now until step
/// `from` and omitting the three kinds from it on (the builder of then sets
/// them: C is where its next pass reads from), which writes no queue row of
/// the omitted events in (C, S]; and the rows of those events of the ledger
/// of now's form, those the builder of now wrote ([`built`]). The coverage,
/// its points and every other row are the builder's.
fn without_omitted_rows(input: &Input, from: usize) -> (Memory, Vec<LedgerRow>) {
    let memory = Memory::default();
    let set = |n: usize| if n >= from { three() } else { none() };
    let mut builder = Builder::default();
    for (n, step) in input.steps.iter().enumerate() {
        builder.set_omission(set(n));
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
                builder.set_omission(set(n));
            }
            Step::FromTheStart => {
                builder = Builder::default();
                builder.set_omission(set(n));
            }
        }
    }
    let omitted = super::omitting::omitted_of(&memory);
    let form = super::omitting::old_form(&memory, Some(&built(input)), ms(input.now));
    let taken = omitted
        .iter()
        .map(|event| (LedgerKind::Queue, event.id.to_string()))
        .filter(|key| !memory.rows.borrow().contains_key(key))
        .map(|key| form.rows.borrow()[&key].clone())
        .collect();
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

/// The ledger of each input the builder omitting the three kinds makes (no
/// queue row of their omitted events in (C, S], the coverage of them held),
/// read by the functions of now, gives every reader the values fixed of the
/// ledger of the builder of now, which holds those rows (the queue rows'
/// value of the rows it makes with those given back, so that the other
/// rows are those fixed too): with C at 0, and with C past the first events
/// where the inputs' builder sets the kinds after its first pass, the
/// omitted events up to C left as queue rows. The reader reads the kinds of
/// the coverage the store holds.
#[test]
fn the_ledger_without_the_omitted_rows_gives_the_values_fixed() {
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
            let given = with_rows_given_back(read, &memory, &taken);
            let old = built(input);
            assert_eq!(of_old_form(&memory, Some(&old), input, input.name), given);
            each.push((input, given));
        }
        check_ended_whole(&each);
    }
    assert!(past_c > 0);
}

/// The values of each input of `each` are those fixed, but those of an
/// input whose last pass the builder of now left marks after (`cut`'s
/// step (ii) and `on its way`'s last pass, cut by the writes) may not be: a builder that writes no row of
/// the omitted events writes others in their place and records inputs past
/// those the values were fixed of (ADR-t4644-1 decision 4), its values those
/// of its ledger of now's form ([`of_old_form`], which each input's `each`
/// is). `cut` differs.
#[track_caller]
fn check_ended_whole(each: &[(&Input, Vec<(String, Value)>)]) {
    let differing: Vec<(&str, String)> = each
        .iter()
        .filter_map(|(input, values)| Some((input.name, differs(input, values)?)))
        .collect();
    for (name, differs) in &differing {
        let input = each
            .iter()
            .find(|(input, _)| input.name == *name)
            .unwrap()
            .0;
        assert!(ends_cut(input), "{differs}");
    }
    assert!(differing.iter().any(|(name, _)| *name == "cut"));
    let names: Vec<&str> = each.iter().map(|(input, _)| input.name).collect();
    let all: Vec<&str> = inputs().iter().map(|input| input.name).collect();
    assert_eq!(names, all);
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
