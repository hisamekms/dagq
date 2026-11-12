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

/// Physical rows only: no coverage supplementation or row restoration.
fn stored_omitted_queue_keys(memory: &Memory, through: EventId) -> Vec<String> {
    memory
        .rows
        .borrow()
        .values()
        .filter(|row| row.kind == LedgerKind::Queue)
        .filter(|row| {
            row.subject
                .as_ref()
                .is_some_and(|s| THREE.contains(&s.kind.as_str()))
        })
        .filter(|row| row.records.iter().any(|event| event.id <= through))
        .map(|row| row.key.clone())
        .collect()
}

/// Every settled row of these fixtures is current and within the public
/// reach. Untimed events have no ended_at and never settle, so acceptance
/// 1 protects their rows even after publication.
fn assert_published_omitted_rows_removed(memory: &Memory, through: EventId) {
    let remaining: Vec<_> = stored_omitted_queue_keys(memory, through)
        .into_iter()
        .filter(|key| memory.rows.borrow()[&(LedgerKind::Queue, key.clone())].is_final)
        .collect();
    assert!(
        remaining.is_empty(),
        "published settled queue rows remain: {remaining:?}"
    );
}

/// Compare all reader values, giving back only physical queue-row JSON
/// removed by publication's wider coverage. Windows still read the actual
/// sparse store. Compare recorded input as rows plus public coverage too.
fn compare_after_removal(memory: &Memory, reference: &Memory, input: &Input) {
    let missing: Vec<_> = reference
        .rows
        .borrow()
        .iter()
        .filter(|(key, row)| {
            row.kind == LedgerKind::Queue && !memory.rows.borrow().contains_key(key)
        })
        .map(|(_, row)| row.clone())
        .collect();
    assert_eq!(
        with_rows_given_back(values(memory, input), memory, &missing),
        values(reference, input),
        "{}",
        input.name
    );
    let recorded = |store: &Memory| -> BTreeSet<EventId> {
        queue_records(store)
            .into_iter()
            .chain(
                super::omitting::omitted_of(store)
                    .iter()
                    .map(|event| event.id),
            )
            .collect()
    };
    assert_eq!(recorded(memory), recorded(reference));
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
        // Preserve the omitting builder's historical coverage and missing
        // queue rows until a test explicitly starts rebuilding it.
        builder.set_rebuild_limit(0);
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
                builder.set_rebuild_limit(0);
            }
            Step::FromTheStart => {
                builder = Builder::default();
                builder.set_omission(set(n));
                builder.set_rebuild_limit(0);
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

/// Rebuild each input through the real pass, interleaving its arrivals
/// with a small private fold. Both sides start from the omitting builder's
/// sparse ledger with C after the first event. Only the queue-rows output
/// is normalized with missing rows given back; windows read the real holes.
#[test]
fn rebuilding_each_frozen_input_keeps_every_value_and_publishes_the_full_fold() {
    let all = inputs();
    let mut each = Vec::new();
    for input in &all {
        let (Step::Arrive(first), Step::Drain(secs)) = (input.steps[0], input.steps[1]) else {
            panic!("{} starts with an arrival and drain", input.name);
        };
        // Read a prefix with rows, then establish the historical coverage
        // with the omitting builder. Leave the first arrival's last three
        // events for real rebuild passes, keeping the canonical log intact.
        let mut setup = input.clone();
        setup.steps = vec![
            Step::Arrive(5),
            Step::Drain(secs),
            Step::Start(secs),
            Step::Arrive(first - 3),
            Step::Drain(secs),
        ];
        let mut sides = [0, 1].map(|_| {
            let (memory, _) = without_omitted_rows(&setup, 2);
            let mut builder = start_builder(&memory, ms(secs)).unwrap();
            builder.set_omission(three());
            (memory, builder)
        });
        let initial = sides[0].0.omitted.borrow().clone().unwrap();
        assert!(initial.from > input.log.events[0].id, "{}", input.name);
        assert!(initial.rebuilding.is_none());
        let protected_prefix: Vec<_> = stored_omitted_queue_keys(&sides[0].0, initial.from)
            .into_iter()
            .filter(|key| !sides[0].0.rows.borrow()[&(LedgerKind::Queue, key.clone())].is_final)
            .collect();
        if input.name == "timeless" {
            assert!(!protected_prefix.is_empty());
        }
        let records = queue_records(&sides[0].0);
        let omitted = super::omitting::omitted_of(&sides[0].0);
        assert!(!omitted.is_empty(), "{}", input.name);
        assert!(omitted.iter().all(|event| !records.contains(&event.id)));
        let largest_kind = three()
            .kinds
            .iter()
            .map(|kind| {
                setup.log.events[..first - 3]
                    .iter()
                    .filter(|e| e.kind.as_str() == kind)
                    .count()
            })
            .max()
            .unwrap();
        let limit = (largest_kind / 3).clamp(1, 64);
        sides[0].1.set_rebuild_limit(limit);
        sides[1].1.set_rebuild_limit(0);
        let old = built(input);
        let mut rounds = 0;
        let mut moving_before_publication = 0;
        let mut arrival_passes = 0;
        let mut published = false;
        let mut run_pass = |sides: &mut [(Memory, Builder); 2], secs| {
            let previous = sides[0].0.omitted.borrow().clone().unwrap();
            let new_events = sides[0].0.events.borrow().last().unwrap().id > previous.through;
            let results = sides
                .each_mut()
                .map(|(memory, builder)| pass(memory, builder, secs));
            assert_eq!(results[0].failure, results[1].failure, "{}", input.name);
            assert_eq!(results[0].backlog, results[1].backlog, "{}", input.name);
            rounds += 1;
            if new_events {
                arrival_passes += 1;
            }
            let memory = &sides[0].0;
            let reference = &sides[1].0;
            let held = memory.omitted.borrow().clone().unwrap();
            let normal = reference.omitted.borrow().clone().unwrap();
            assert_eq!(held.through, normal.through, "{}", input.name);
            assert_eq!(*memory.rows_mark.borrow(), *reference.rows_mark.borrow());
            assert_eq!(*memory.mark.borrow(), *reference.mark.borrow());
            compare_after_removal(memory, reference, input);
            // The row-given-back comparison reads every non-row value
            // from the sparse ledger, before and after publication.
            let form = super::omitting::old_form(memory, Some(&old), ms(input.now));
            let taken: Vec<_> = form
                .rows
                .borrow()
                .iter()
                .filter(|(key, _)| !memory.rows.borrow().contains_key(key))
                .map(|(_, row)| row.clone())
                .collect();
            assert_eq!(
                with_rows_given_back(values(memory, input), memory, &taken),
                of_old_form(memory, Some(&old), input, input.name)
            );
            if held.from == EventId::new(0) {
                assert_published_omitted_rows_removed(memory, held.through);
                for key in &protected_prefix {
                    assert!(
                        memory
                            .rows
                            .borrow()
                            .contains_key(&(LedgerKind::Queue, key.clone())),
                        "{}: an unsettled prefix row was removed",
                        input.name
                    );
                }
                let (summaries, points) = counted(
                    &memory.events.borrow(),
                    &three().kinds,
                    (EventId::new(0), held.through),
                    EVERY,
                );
                assert_eq!(held.summaries, summaries, "{}", input.name);
                assert_eq!(
                    memory.points.borrow().values().cloned().collect::<Vec<_>>(),
                    points
                );
                assert!(held.rebuilding.is_none());
                published = true;
            } else {
                assert!(!published, "{} cannot undo publication", input.name);
                let mut public = held.clone();
                let private = public.rebuilding.take();
                if let Some(private) = private {
                    assert!(private.positions.values().all(|p| *p <= held.through));
                }
                assert_eq!(public, normal, "{}", input.name);
                if held.through > previous.through {
                    moving_before_publication += 1;
                }
            }
            results[0].backlog == 0 || results[0].failure.is_some()
        };
        let mut steps = vec![
            Step::Arrive(first - 2),
            Step::Pass(secs),
            Step::Arrive(first - 1),
            Step::Pass(secs),
            Step::Arrive(first),
        ];
        steps.extend_from_slice(&input.steps[1..]);
        for step in steps {
            match step {
                Step::Arrive(n) => {
                    for (memory, _) in &sides {
                        memory.events.replace(input.log.events[..n].to_vec());
                    }
                }
                Step::Pass(secs) => {
                    run_pass(&mut sides, secs);
                }
                Step::Drain(secs) => {
                    for _ in 0..DRAIN_PASSES {
                        if run_pass(&mut sides, secs) {
                            break;
                        }
                    }
                }
                Step::SessionWriteFails(secs) => {
                    for (memory, _) in &sides {
                        memory.batch_fails.set(1);
                    }
                    run_pass(&mut sides, secs);
                    for (memory, _) in &sides {
                        memory.batch_fails.set(0);
                    }
                }
                Step::MarksGone => {
                    for (memory, _) in &sides {
                        memory.mark.replace(None);
                        memory.rows_mark.replace(None);
                    }
                }
                Step::Start(_) | Step::FromTheStart => {
                    for (n, (memory, builder)) in sides.iter_mut().enumerate() {
                        *builder = if matches!(step, Step::FromTheStart) {
                            Builder::default()
                        } else {
                            let Step::Start(at) = step else {
                                unreachable!()
                            };
                            start_builder(memory, ms(at)).unwrap()
                        };
                        builder.set_omission(three());
                        builder.set_rebuild_limit(if n == 0 { limit } else { 0 });
                    }
                }
            }
        }
        // At the canonical input's end, retain the frozen-value anchor
        // (and its existing same-input allowance for a cut pass).
        each.push((
            input,
            of_old_form(&sides[0].0, Some(&old), input, input.name),
        ));
        // Drain private work too: ordinary backlog does not count it.
        for _ in 0..largest_kind + 3 {
            run_pass(&mut sides, input.now);
            if sides[0].0.omitted.borrow().as_ref().unwrap().from == EventId::new(0) {
                break;
            }
        }
        assert!(published, "{}", input.name);
        assert!(rounds > 1 && arrival_passes >= 2, "{}", input.name);
        assert!(
            moving_before_publication > 0,
            "{} advances S before publication",
            input.name
        );
    }
    check_ended_whole(&each);
}

/// A private page or tail-probe failure cannot suppress the normal
/// mark-only or row-writing pass. Its entire checkpoint stays put even
/// after earlier kinds were read; a normal row error still takes priority.
#[test]
fn a_failed_rebuild_read_still_writes_the_normal_marks_and_keeps_its_checkpoint() {
    for stage in ["events_of_between", "ledger_kind_has_after"] {
        for rows in ["none", "healthy", "unwritable"] {
            let mut log = Log::default();
            for n in 0..24 {
                log.add(None, None, THREE[n % 3], json!({}), 1);
            }
            let input = Input {
                name: "failing private read",
                log: log.clone(),
                steps: vec![
                    Step::Arrive(6),
                    Step::Drain(LATER),
                    Step::Start(LATER),
                    Step::Arrive(24),
                    Step::Drain(LATER),
                ],
                now: LATER,
                times: vec![1],
                untils: vec![],
            };
            let mut sides = [0, 1].map(|_| {
                let (memory, _) = without_omitted_rows(&input, 2);
                let mut builder = start_builder(&memory, ms(LATER)).unwrap();
                builder.set_omission(three());
                builder.set_rebuild_limit(1);
                assert!(pass(&memory, &mut builder, LATER).failure.is_none());
                (memory, builder)
            });
            sides[1].1.set_rebuild_limit(0);
            let checkpoint = sides[0]
                .0
                .omitted
                .borrow()
                .as_ref()
                .unwrap()
                .rebuilding
                .clone();
            assert!(checkpoint.is_some());
            for kind in THREE {
                log.add(None, None, kind, json!({}), 2);
            }
            if rows != "none" {
                log.landed(9, &run_id(9), 3);
                log.add(
                    None,
                    None,
                    "session_opened",
                    json!({"kind": "runtime_planner", "planner_id": 9}),
                    4,
                );
            }
            for (memory, _) in &sides {
                memory.events.replace(log.events.clone());
                if rows == "unwritable" {
                    memory
                        .unwritable
                        .replace(Some((LedgerKind::Run, run_id(9))));
                }
            }
            sides[0].0.rebuild_read_failure.set(Some((stage, THREE[2])));
            let before_batches = sides[0].0.batches.get();
            let results = sides
                .each_mut()
                .map(|(memory, builder)| pass(memory, builder, LATER));
            let [(memory, builder), (reference, _)] = &mut sides;
            assert_eq!(memory.failed_rebuild_reads.get(), 1, "{stage} {rows}");
            let failure = results[0].failure.as_ref().unwrap();
            if rows == "unwritable" {
                assert_eq!(results[0].failure, results[1].failure);
                assert!(failure.contains("cannot be written"), "{failure}");
            } else {
                assert!(results[1].failure.is_none());
                assert!(failure.contains(stage), "{failure}");
            }
            assert!(
                memory.batches.get() > before_batches,
                "normal write still attempted"
            );
            assert_eq!(*memory.mark.borrow(), *reference.mark.borrow());
            assert_eq!(*memory.rows_mark.borrow(), *reference.rows_mark.borrow());
            assert_eq!(memory.mark.borrow().as_ref().unwrap().through, log.last());
            let held = memory.omitted.borrow().clone().unwrap();
            assert_eq!(held.from, EventId::new(6));
            assert_eq!(held.through, log.last());
            assert_eq!(held.rebuilding, checkpoint);
            assert_eq!(held, reference.omitted.borrow().clone().unwrap());
            assert_eq!(*memory.rows.borrow(), *reference.rows.borrow());
            // The following healthy pass resumes the saved checkpoint,
            // rather than a partially folded prefix of the failing pass.
            memory.rebuild_read_failure.set(None);
            memory.unwritable.replace(None);
            assert!(pass(memory, builder, LATER).failure.is_none());
            let next = memory.omitted.borrow().clone().unwrap();
            assert!(
                next.rebuilding
                    .as_ref()
                    .unwrap()
                    .positions
                    .iter()
                    .all(|(kind, at)| *at > checkpoint.as_ref().unwrap().positions[kind])
            );
        }
    }
}

/// Two actual builders, on the same arrivals: rebuilding never delays the
/// public S, R, or queue writes. Heavy arrivals postpone only publication.
#[test]
fn rebuilding_follows_the_updated_s_while_arrivals_outpace_it_then_catches_up() {
    use crate::domain::ledger::session::written::omitted::{Coverage, Omitted};
    let mut log = Log::default();
    for secs in 1..=100 {
        if secs <= 96 {
            log.add(None, None, "mark_added", json!({"mark": "prefix"}), secs);
        } else {
            log.add(None, None, THREE[0], json!({}), secs);
        }
    }
    let setup = |limit| {
        let memory = Memory::default();
        memory.events.replace(log.events.clone());
        let mut old = Builder::default();
        old.set_omission(none());
        drain(&memory, &mut old, LATER);
        memory.write_omitted(&Omitted {
            coverage: Coverage::start(&three().kinds, EventId::new(100)),
            points: Vec::new(),
        });
        let mut builder = start_builder(&memory, ms(LATER)).unwrap();
        builder.set_omission(three());
        builder.set_rebuild_limit(limit);
        (memory, builder)
    };
    let (memory, mut builder) = setup(2);
    let (reference, mut original) = setup(0);
    let (quiet, mut without_arrivals) = setup(2);
    assert_eq!(
        stored_omitted_queue_keys(&memory, EventId::new(100)).len(),
        4
    );
    assert_eq!(
        stored_omitted_queue_keys(&quiet, EventId::new(100)).len(),
        4
    );
    let input = Input {
        name: "arriving",
        log: log.clone(),
        steps: vec![],
        now: LATER,
        times: vec![50, 100],
        untils: vec![EventId::new(100)],
    };
    let mut published = false;
    for round in 0..32 {
        let arrivals = if round == 0 {
            1
        } else if round < 5 {
            // More than the two-event fold plus its one-event tail probe.
            4
        } else {
            1
        };
        for _ in 0..arrivals {
            log.add(None, None, THREE[0], json!({}), 101 + round);
        }
        memory.events.replace(log.events.clone());
        reference.events.replace(log.events.clone());
        assert!(pass(&memory, &mut builder, LATER).failure.is_none());
        assert!(pass(&reference, &mut original, LATER).failure.is_none());
        assert!(pass(&quiet, &mut without_arrivals, LATER).failure.is_none());
        let quiet_coverage = quiet.omitted.borrow().clone().unwrap();
        // Four initial events at two per pass publish in two passes when
        // none arrive. The arriving side must remain unpublished longer.
        assert_eq!(quiet_coverage.from == EventId::new(0), round >= 1);
        assert_eq!(quiet_coverage.through, EventId::new(100));
        if quiet_coverage.from == EventId::new(0) {
            assert_published_omitted_rows_removed(&quiet, quiet_coverage.through);
        }
        assert!(!builder.omission_stopped());
        let public = reference.omitted.borrow().clone().unwrap();
        let held = memory.omitted.borrow().clone().unwrap();
        assert_eq!(held.through, log.last());
        assert_eq!(r_of(&memory).events, log.last());
        compare_after_removal(&memory, &reference, &input);
        if round == 0 {
            assert_eq!(held.through, EventId::new(101));
            assert_eq!(
                held.rebuilding.as_ref().unwrap().positions[THREE[0]],
                EventId::new(98)
            );
        }
        if round < 5 {
            assert_eq!(
                held.from,
                EventId::new(100),
                "heavy arrivals alone delay publication"
            );
        }
        if held.from == EventId::new(0) {
            assert_published_omitted_rows_removed(&memory, held.through);
            let (summaries, points) = counted(
                &log.events,
                &three().kinds,
                (EventId::new(0), log.last()),
                three().every,
            );
            assert_eq!(held.summaries, summaries);
            assert_eq!(
                memory.points.borrow().values().cloned().collect::<Vec<_>>(),
                points
            );
            published = true;
            break;
        }
        let mut held = held;
        let private = held.rebuilding.take().unwrap();
        assert!(private.positions.values().all(|p| *p <= held.through));
        assert_eq!(held, public);
    }
    assert!(published);
}

/// Prefix and suffix folds cannot be merged: their apparent identical
/// suffix summaries hide distinct backwards jumps across C.
#[test]
fn rebuilding_replays_the_suffix_instead_of_merging_it() {
    use crate::domain::ledger::session::written::omitted::{Coverage, Covering};
    for (middle, expected) in [(1, 199), (30, 170)] {
        let mut log = Log::default();
        for secs in [200, 50, middle, 100, 40] {
            log.add(None, None, THREE[0], json!({}), secs);
        }
        let omission = three();
        let mut public = Coverage::start(&omission.kinds, EventId::new(1));
        public.read(&log.events[1..], omission.every, log.last(), &mut vec![]);
        assert_eq!(public.summaries[THREE[0]].back_ms, 60_000);
        let mut covering = Covering::default();
        covering.begin(&omission, (Some(&public), None), log.last());
        for round in 0..3 {
            covering
                .rebuild(omission.every, 2, |kind, after, upto, limit| {
                    let events: Vec<_> = log
                        .events
                        .iter()
                        .filter(|e| e.kind.as_str() == kind && e.id > after && e.id <= upto)
                        .take(limit)
                        .cloned()
                        .collect();
                    let complete = log
                        .events
                        .iter()
                        .filter(|e| e.kind.as_str() == kind && e.id <= upto)
                        .map(|e| e.id)
                        .max()
                        <= events.last().map(|e| e.id);
                    Ok::<_, std::convert::Infallible>((events, complete))
                })
                .unwrap();
            assert_eq!(covering.held().unwrap().from == EventId::new(0), round == 2);
        }
        let next = covering.to_write().unwrap();
        assert_eq!(next.coverage.from, EventId::new(0));
        assert_eq!(next.coverage.summaries[THREE[0]].back_ms, expected * 1000);
        assert_eq!(
            next.coverage.summaries,
            counted(
                &log.events,
                &omission.kinds,
                (EventId::new(0), log.last()),
                omission.every
            )
            .0
        );
    }
}

/// In-progress folds survive restarts, no R and W rollback. A stop freezes
/// both the public and private folds, including a builder already stopped.
#[test]
fn rebuilding_survives_marks_changes_and_freezes_on_a_stop() {
    use crate::domain::ledger::session::written::omitted::{Coverage, Omitted};
    let mut log = Log::default();
    for secs in 1..=100 {
        log.add(None, None, THREE[0], json!({}), secs);
    }
    let memory = Memory::default();
    memory.events.replace(log.events.clone());
    let mut old = Builder::default();
    old.set_omission(none());
    drain(&memory, &mut old, LATER);
    memory.write_omitted(&Omitted {
        coverage: Coverage::start(&three().kinds, EventId::new(100)),
        points: vec![],
    });
    let mut builder = start_builder(&memory, ms(LATER)).unwrap();
    builder.set_omission(three());
    builder.set_rebuild_limit(2);
    pass(&memory, &mut builder, LATER);
    let first = memory.omitted.borrow().clone().unwrap();
    assert!(first.rebuilding.is_some());
    for mode in 0..3 {
        match mode {
            0 => {
                memory.mark.replace(Some(
                    crate::domain::ledger::session::written::SessionsWritten::at(EventId::new(0)),
                ));
            }
            1 => {
                memory.mark.borrow_mut().as_mut().unwrap().ledger_v -= 1;
                memory.rows_mark.borrow_mut().as_mut().unwrap().ledger_v -= 1;
            }
            _ => {
                memory.mark.replace(None);
                memory.rows_mark.replace(None);
            }
        }
        let before = memory.omitted.borrow().clone().unwrap();
        builder = start_builder(&memory, ms(LATER)).unwrap();
        builder.set_omission(three());
        builder.set_rebuild_limit(2);
        assert!(pass(&memory, &mut builder, LATER).failure.is_none());
        let next = memory.omitted.borrow().clone().unwrap();
        assert_eq!(
            (next.from, next.through, &next.summaries),
            (before.from, before.through, &before.summaries)
        );
        assert!(
            next.rebuilding.as_ref().unwrap().positions[THREE[0]]
                >= before.rebuilding.as_ref().unwrap().positions[THREE[0]]
        );
        let (previous, rebuilt) = (
            before.rebuilding.as_ref().unwrap(),
            next.rebuilding.as_ref().unwrap(),
        );
        assert!(rebuilt.points.starts_with(&previous.points));
        for (kind, position) in &rebuilt.positions {
            let (expected, _) = counted(
                &log.events,
                std::slice::from_ref(kind),
                (EventId::new(0), *position),
                three().every,
            );
            assert_eq!(rebuilt.summaries[kind], expected[kind]);
            assert!(rebuilt.summaries[kind].seen >= previous.summaries[kind].seen);
        }
    }
    // Stop on a queue R beyond S, even with a rebuild held.
    let mut marks = memory.ledger_marks().unwrap().unwrap();
    marks.omitted = None;
    marks.rows = Some(crate::domain::ledger::session::written::RowsWritten::at(
        crate::domain::ledger::session::written::Cursors {
            events: EventId::new(101),
            ..Default::default()
        },
        0,
    ));
    memory.write_ledger_marks(&marks).unwrap();
    let stopped = memory.omitted.borrow().clone().unwrap();
    builder = start_builder(&memory, ms(LATER)).unwrap();
    builder.set_omission(three());
    builder.set_rebuild_limit(2);
    let reference = Memory::default();
    reference.events.replace(memory.events.borrow().clone());
    reference.rows.replace(memory.rows.borrow().clone());
    reference.mark.replace(*memory.mark.borrow());
    reference.rows_mark.replace(*memory.rows_mark.borrow());
    let mut original_public = stopped.clone();
    original_public.rebuilding = None;
    reference.omitted.replace(Some(original_public.clone()));
    reference.points.replace(memory.points.borrow().clone());
    let mut original = start_builder(&reference, ms(LATER)).unwrap();
    original.set_omission(three());
    original.set_rebuild_limit(0);
    for _ in 0..2 {
        pass(&memory, &mut builder, LATER);
        pass(&reference, &mut original, LATER);
        assert_eq!(reference.omitted.borrow().as_ref(), Some(&original_public));
        assert!(builder.omission_stopped());
        assert_eq!(memory.omitted.borrow().as_ref(), Some(&stopped));
    }
    // A different stored set also stops and retains the private fold.
    let mut different = stopped.clone();
    different.kinds.push("other".into());
    memory.omitted.replace(Some(different.clone()));
    builder = Builder::default();
    builder.set_omission(three());
    builder.set_rebuild_limit(2);
    pass(&memory, &mut builder, LATER);
    assert!(builder.omission_stopped());
    assert_eq!(memory.omitted.borrow().as_ref(), Some(&different));
}

/// The builder's read cursor can be beyond public S; neither a window
/// nor rebuilding substitutes that cursor or an incomplete prefix for S.
#[test]
fn a_private_prefix_at_s_100_with_the_read_at_200_is_no_window_input() {
    use crate::domain::ledger::session::written::omitted::{Coverage, Covering, Omitted};
    let input = inputs().into_iter().find(|i| i.name == "reached").unwrap();
    let memory = built(&input);
    let before = values(&memory, &input);
    let mut coverage = Coverage::start(&three().kinds, EventId::new(50));
    coverage.read(
        &input
            .log
            .events
            .iter()
            .filter(|e| e.id > EventId::new(50) && e.id <= EventId::new(100))
            .cloned()
            .collect::<Vec<_>>(),
        three().every,
        EventId::new(100),
        &mut vec![],
    );
    let mut covering = Covering::default();
    covering.begin(&three(), (Some(&coverage), None), EventId::new(200));
    covering
        .rebuild(three().every, 1, |kind, after, upto, limit| {
            assert_eq!(upto, EventId::new(100));
            let events = memory.ledger_events_of_kind(kind, after, upto, limit)?;
            let complete = events.len() < limit
                || !memory.ledger_kind_has_after(
                    kind,
                    events.last().expect("full page").id,
                    upto,
                )?;
            Ok::<_, anyhow::Error>((events, complete))
        })
        .unwrap();
    let next = covering.to_write().unwrap();
    assert!(next.coverage.rebuilding.is_some());
    memory.write_omitted(&Omitted {
        coverage: next.coverage,
        points: vec![],
    });
    assert_eq!(values(&memory, &input), before);
}

/// Start with the actual old builder's ledger and its frozen reader
/// values, then delete covered rows. Restore only physical-row output
/// for the frozen comparison; the actual books and windows use no rows
/// given back. Exercise no R, version loss, a removed kind and C > 0.
#[test]
fn deleting_existing_rows_preserves_all_frozen_values_and_books() {
    use crate::domain::ledger::session::written::omitted::{Coverage, Omitted};
    let all = inputs();
    let mut each = Vec::new();
    for input in &all {
        let original = built(input);
        let expected = values(&original, input);
        assert_eq!(differs(input, &expected), None);
        let modes: &[&str] = match input.name {
            "reached" => &["published", "prefix"],
            "no r" => &["published", "no coverage", "before tail"],
            _ => &["published"],
        };
        for &mode in modes {
            let memory = Memory::default();
            memory.events.replace(original.events.borrow().clone());
            memory.rows.replace(original.rows.borrow().clone());
            memory.mark.replace(*original.mark.borrow());
            memory.rows_mark.replace(*original.rows_mark.borrow());
            let last_saved = memory
                .rows
                .borrow()
                .values()
                .flat_map(|r| r.records.iter().map(|e| e.id))
                .max()
                .unwrap();
            let r = memory
                .rows_mark
                .borrow()
                .map(|r| r.through.events)
                .unwrap_or(last_saved);
            let s = if mode == "before tail" {
                EventId::new(last_saved.as_i64() - 1)
            } else {
                r
            };
            let c = if mode == "prefix" {
                EventId::new(5)
            } else {
                EventId::new(0)
            };
            if mode != "no coverage" {
                let kinds = three().kinds;
                let (summaries, points) = counted(&memory.events.borrow(), &kinds, (c, s), EVERY);
                let mut coverage = Coverage::start(&kinds, c);
                coverage.through = s;
                coverage.summaries = summaries;
                memory.write_omitted(&Omitted { coverage, points });
            }
            let coverage = memory.omitted.borrow().clone();
            let points = memory.points.borrow().clone();
            let before: Vec<_> = memory.rows.borrow().values().cloned().collect();
            let book = |memory: &Memory| {
                let heads = memory.ledger_queue_heads().unwrap();
                QueueBook::from_heads(&heads, &PreClaimBook::default(), last_saved)
            };
            let expected_book = book(&memory);
            let n = memory.remove_omitted_queue_rows().unwrap();
            let taken: Vec<_> = before
                .into_iter()
                .filter(|row| {
                    !memory
                        .rows
                        .borrow()
                        .contains_key(&(row.kind, row.key.clone()))
                })
                .collect();
            assert_eq!(n, taken.len());
            if mode == "published" {
                assert!(n > 0, "{}", input.name);
            }
            if mode == "no coverage" {
                assert_eq!(n, 0);
            }
            if input.name == "no r" {
                let tail = input.log.last().to_string();
                assert_eq!(
                    memory
                        .ledger_row(LedgerKind::Queue, &tail)
                        .unwrap()
                        .is_some(),
                    mode == "before tail" || mode == "no coverage"
                );
            }
            assert_eq!(book(&memory), expected_book, "{} {mode}", input.name);
            assert_eq!(memory.omitted.borrow().clone(), coverage);
            assert_eq!(*memory.points.borrow(), points);
            assert_eq!(
                with_rows_given_back(values(&memory, input), &memory, &taken),
                expected,
                "{} {mode}",
                input.name
            );
            // A version bump invalidates R; coverage must still supply edges.
            memory.mark.replace(None);
            memory.rows_mark.replace(None);
            assert_eq!(
                with_rows_given_back(values(&memory, input), &memory, &taken),
                expected
            );
            if mode == "published" {
                // Restart with one kind removed, initializing the pass's
                // omission state before any new input is folded.
                let mut builder = start_builder(&memory, ms(input.now)).unwrap();
                builder.set_omission(Omission::of(THREE[..2].iter().copied(), EVERY));
                // Initialize omission against saved coverage without changing
                // any input/row: begin_coverage is the pass's start operation.
                builder.begin_coverage_from(None, coverage.as_ref());
                assert!(builder.omission_stopped());
                assert_eq!(
                    with_rows_given_back(values(&memory, input), &memory, &taken),
                    expected
                );
                each.push((input, expected.clone()));
            }
        }
    }
    check(&each);
}
