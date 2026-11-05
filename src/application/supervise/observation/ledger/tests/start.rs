//! A new process's builder ([`start_builder`]): from the position R the
//! store's marks give, with every book made again from the rows as they
//! were at R, or from its cursors' start.

use super::pushes_mixed::mixed;
use super::*;
use crate::domain::command_rules::{
    CommandClassifier, CommandRule, TelemetrySettings, effective_rules,
};
use crate::domain::command_shape::{DigestKey, Subcommands, shape_value_with};
use crate::domain::ledger::testkit::{forgotten_log, queue_across, two_late_log, waits_across};
use crate::domain::session_step::ResourceSample;
use crate::domain::toolchain::Toolchains;

mod time_read;

type Rows = BTreeMap<(LedgerKind, String), LedgerRow>;

/// The latest time of an event read up to the event `at` of `log`, as the
/// build of every event holds it there.
fn read_ms_at(log: &Log, at: EventId) -> i64 {
    let mut builder = Builder::default();
    let read: Vec<RunEvent> = log
        .events
        .iter()
        .filter(|event| event.id <= at)
        .cloned()
        .collect();
    builder.mark_events(&read);
    builder.folds_at(i64::MIN)
}

/// A store of the events of `log` up to `upto`, the node samples `samples`,
/// and the rows a builder from its cursors' start drains there at `secs`.
fn drained_store(log: &Log, samples: &[NodeSample], upto: usize, secs: i64) -> Memory {
    let memory = Memory::default();
    memory.events.replace(log.events[..upto].to_vec());
    memory.samples.replace(samples.to_vec());
    drain(&memory, &mut Builder::default(), secs);
    memory
}

/// A started builder's rows on `memory` given every event of `log`,
/// drained at `secs`.
#[track_caller]
fn started_and_drained(memory: &Memory, log: &Log, secs: i64) -> Rows {
    memory.events.replace(log.events.clone());
    let mut builder = start_builder(memory, ms(secs)).unwrap();
    drain(memory, &mut builder, secs);
    memory.rows.borrow().clone()
}

/// The rows of a builder from its cursors' start on a copy of what
/// `memory` holds (rows and inputs), given every event of `log`, drained at
/// `secs`: what going on from R is to write. The marks are none of its.
fn from_the_start_on(memory: &Memory, log: &Log, secs: i64) -> Rows {
    let whole = Memory::of(log);
    whole.rows.replace(memory.rows.borrow().clone());
    whole.samples.replace(memory.samples.borrow().clone());
    whole
        .run_resources
        .replace(memory.run_resources.borrow().clone());
    whole.steps.replace(memory.steps.borrow().clone());
    whole.step_id.set(memory.step_id.get());
    whole.owners.replace(memory.owners.borrow().clone());
    whole.inside.replace(memory.inside.borrow().clone());
    whole.inside_id.set(memory.inside_id.get());
    drain(&whole, &mut Builder::default(), secs);
    whole.rows.into_inner()
}

/// What differs between two sets of rows, by key.
fn differing(a: &Rows, b: &Rows) -> Vec<(LedgerKind, String)> {
    a.iter()
        .filter(|(key, row)| b.get(*key) != Some(row))
        .map(|(key, _)| key.clone())
        .chain(b.keys().filter(|key| !a.contains_key(*key)).cloned())
        .collect()
}

/// The histories a start goes on over, with the node samples of each and
/// a time past all of their events.
fn histories() -> Vec<(&'static str, Log, Vec<NodeSample>, i64)> {
    let (every, samples) = every_kind_of_row();
    let (sessions, _) = sessions_across();
    let (queue, _) = queue_across();
    let (waits, ..) = waits_across();
    let mut going_on = Vec::new();
    for (name, log, samples) in [
        ("every kind of row", every, samples),
        ("in time order", in_time_order(), Vec::new()),
        ("sessions across", sessions, Vec::new()),
        ("queue across", queue, Vec::new()),
        ("waits across", waits, Vec::new()),
        ("two late", two_late_log(), Vec::new()),
        ("mixed", mixed(3).0, Vec::new()),
        ("mixed", mixed(11).0, Vec::new()),
    ] {
        going_on.push((name, log, samples, LATER));
    }
    going_on
}

/// What `rows` lack or hold otherwise of `whole`, the build of every input
/// on an empty store. A legacy row keeps the values it was first written
/// with, those of the events up to there; a run written as it waited on its
/// push and on its way again since keeps its row, which `whole` has none of.
fn differing_from_whole(rows: &Rows, whole: &Rows) -> Vec<(LedgerKind, String)> {
    whole
        .iter()
        .filter(|(key, row)| {
            rows.get(*key)
                .is_none_or(|held| held != *row && !held.is_legacy())
        })
        .map(|(key, _)| key.clone())
        .collect()
}

/// A store drained at any position and started from its marks writes the
/// rows of the build of every event: every row of the build from an empty
/// store at the same time, whole (its range of inputs, its mark of settled
/// and its version), and those of a builder from its cursors' start on the
/// same store, there and at a later time past some graces. The positions cross
/// an open run, a task not ended, a supervisor's life that goes on and the
/// waits open before a claim, as the histories hold them.
#[test]
fn a_builder_started_at_any_position_writes_the_rows_of_every_event() {
    for (name, log, samples, secs) in histories() {
        let whole = {
            let memory = Memory::of(&log);
            memory.samples.replace(samples.clone());
            drain(&memory, &mut Builder::default(), secs);
            memory.rows.into_inner()
        };
        for upto in 1..=log.events.len() {
            let memory = drained_store(&log, &samples, upto, secs);
            let r = r_of(&memory);
            assert_eq!(r.events, id_at(&log, upto), "{name} {upto}");
            let from_start = from_the_start_on(&memory, &log, secs);
            let rows = started_and_drained(&memory, &log, secs);
            assert_eq!(differing(&rows, &from_start), Vec::new(), "{name} {upto}");
            assert_eq!(
                differing_from_whole(&rows, &whole),
                Vec::new(),
                "{name} {upto}"
            );
            assert_eq!(r_of(&memory).events, log.last(), "{name} {upto}");

            // Stopped in the graces: time alone settles them.
            let stopped_at = last_secs(&log, upto) + 1;
            let memory = drained_store(&log, &samples, upto, stopped_at);
            let expected = from_the_start_on(&memory, &log, secs);
            let rows = started_and_drained(&memory, &log, secs);
            assert_eq!(
                differing(&rows, &expected),
                Vec::new(),
                "{name} {upto} stopped in the graces"
            );
        }
    }
}

/// The seconds after [`T0`] of the latest of the first `upto` events of
/// `log`.
fn last_secs(log: &Log, upto: usize) -> i64 {
    log.events[..upto]
        .iter()
        .filter_map(|event| crate::domain::stats::timestamp_millis(&event.created_at))
        .max()
        .map_or(0, |at| (at - ms(0)) / 1000)
}

/// A builder started again each `every` events writes the rows of one that
/// goes on over the same passes, whatever it folded by each start.
#[test]
fn starts_again_and_again_write_the_rows_of_every_event() {
    for (name, log, samples, secs) in histories() {
        for every in [1, 3] {
            let memory = Memory::default();
            memory.samples.replace(samples.clone());
            let going = Memory::default();
            going.samples.replace(samples.clone());
            let mut goes_on = Builder::default();
            let mut upto = every.min(log.events.len());
            loop {
                memory.events.replace(log.events[..upto].to_vec());
                let mut builder = start_builder(&memory, ms(secs)).unwrap();
                drain(&memory, &mut builder, secs);
                going.events.replace(log.events[..upto].to_vec());
                drain(&going, &mut goes_on, secs);
                if upto == log.events.len() {
                    break;
                }
                upto = (upto + every).min(log.events.len());
            }
            assert_eq!(
                differing(&memory.rows.borrow(), &going.rows.borrow()),
                Vec::new(),
                "{name} every {every}"
            );
        }
    }
}

/// A settled run a late event reaches reads back by id the push that closed
/// its pending push, a push the store's rows name and the builder does not
/// keep; a builder started across it writes its row and its task's as the
/// build of every event.
#[test]
fn a_started_builder_reads_back_the_closing_push_of_a_settled_run_a_late_event_reaches() {
    let (a, b) = (run_id(1), run_id(2));
    let mut log = Log::default();
    log.worked(1, &a, 0)
        .integrated(1, &a, 700)
        .push_failed(1, &a, 910)
        .add(
            Some(1),
            None,
            "task_status_changed",
            json!({"from": "in_progress", "to": "completed"}),
            910,
        )
        .landed(2, &b, 2000);
    let memory = Memory::of(&log);
    drain(&memory, &mut Builder::default(), LATER);
    log.add(Some(1), Some(&a), "note_added", json!({}), LATER + 10);

    memory.read_one.borrow_mut().clear();
    let rows = started_and_drained(&memory, &log, LATER + 20);
    assert!(
        memory
            .read_one
            .borrow()
            .iter()
            .any(|event| event.kind == EventKind::PushFinished),
        "the closing push is read by id"
    );
    let whole = Inputs::new(&log.events, ms(LATER + 20), BUILT, true);
    for key in [
        (LedgerKind::Run, a.clone()),
        (LedgerKind::Task, "1".to_owned()),
    ] {
        assert_eq!(
            build(&target_of(key.0, &key.1), &whole, &[]).as_ref(),
            Some(&rows[&key]),
            "{key:?}"
        );
    }
}

/// The first of each scan by cursor, and how many inputs all of them read.
fn scans_of(memory: &Memory) -> BTreeMap<&'static str, (i64, usize)> {
    let mut scans: BTreeMap<&'static str, (i64, usize)> = BTreeMap::new();
    for (cursor, after, read) in memory.scans.borrow().iter() {
        let scan = scans.entry(cursor).or_insert((*after, 0));
        scan.1 += read;
    }
    scans
}

/// Every kind of input after a store's R: an event, a node sample, a step,
/// a window from outside and a sample from inside.
fn with_every_input(memory: &Memory, log: &mut Log, run: &str, seq: i64) {
    log.add(Some(1), Some(run), "note_added", json!({}), LATER + 1);
    memory.events.replace(log.events.clone());
    let last = memory.samples.borrow().last().map_or(0, |sample| sample.id);
    let new = sample(last + 1, 7_200);
    memory.samples.borrow_mut().push(new);
    let target = StepTarget::run(&RunId::new(run).unwrap());
    memory.register_session(&target, "s", ms(0)).unwrap();
    let mut step = Step::new(seq, StepKind::ToolCall, 0, 10);
    step.tool = Some("Bash".into());
    memory.record_steps(&target, "s", &[step], 1_000).unwrap();
    let attachment = attach(log, run, 100);
    memory.events.replace(log.events.clone());
    measured(memory, run, attachment, (110, 300), Some(30.0), true);
    let mut inside = ResourceSample::new(seq, 0, 100_000);
    inside.cpu_seconds = Some(40.0);
    memory
        .record_resources(&target, "s", &[inside], 1_000)
        .unwrap();
}

/// On a store with R and no older row, a new builder's first pass reads of
/// each cursor only what came after R's: from R, not from 0, as many
/// inputs as came after it, and the next builder likewise from the R that
/// pass wrote.
#[test]
fn a_builder_started_from_r_reads_each_cursor_after_it_only() {
    let (mut log, samples) = every_kind_of_row();
    let run = run_id(1);
    let memory = drained_store(&log, &samples, log.events.len(), LATER);
    // Inputs of every cursor before R, so that none of its positions is 0.
    with_every_input(&memory, &mut log, &run, 1);
    drain(&memory, &mut Builder::default(), LATER + 1);
    let r = r_of(&memory);
    assert!(r.events.as_i64() > 0 && r.samples > 0 && r.steps > 0);
    assert!(r.resources > 0 && r.inside_resources > 0);
    with_every_input(&memory, &mut log, &run, 2);
    memory.scans.borrow_mut().clear();

    let mut builder = start_builder(&memory, ms(LATER + 2)).unwrap();
    assert_eq!(builder.cursors(), r);
    assert!(memory.scans.borrow().is_empty(), "no scan before its pass");
    let built = pass(&memory, &mut builder, LATER + 2);
    assert_eq!(built.failure, None);
    let scans = scans_of(&memory);
    assert_eq!(scans["events"], (r.events.as_i64(), 2));
    assert_eq!(scans["samples"], (r.samples, 1));
    assert_eq!(scans["steps"], (r.steps, 1));
    assert_eq!(scans["resources"], (r.resources, 1));
    assert_eq!(scans["inside_resources"], (r.inside_resources, 1));

    drain(&memory, &mut builder, LATER + 2);
    let next = r_of(&memory);
    assert_eq!(next, builder.cursors());
    memory.scans.borrow_mut().clear();
    let mut again = start_builder(&memory, ms(LATER + 2)).unwrap();
    pass(&memory, &mut again, LATER + 2);
    let scans = scans_of(&memory);
    assert!(!scans.contains_key("events"));
    assert_eq!(scans["samples"], (next.samples, 0));
}

/// A history of `settled` runs and tasks settled, each with a push and
/// steps classified, then rows not settled: a task and a run on their way,
/// a run pending on its push, and a supervisor's life that goes on.
fn settled_and_open(settled: usize, memory: &Memory) -> Log {
    let mut log = Log::default();
    log.add(
        None,
        None,
        "supervisor_started",
        json!({"supervisor": "s"}),
        0,
    );
    for n in 0..settled {
        let run = many_run_id(n + 1);
        let secs = i64::try_from(n).unwrap() * 10 + 10;
        log.landed(i64::try_from(n).unwrap() + 1, &run, secs);
        memory.events.replace(log.events.clone());
        let target = StepTarget::run(&RunId::new(&run).unwrap());
        memory.register_session(&target, &run, ms(secs)).unwrap();
        let mut step = Step::new(1, StepKind::ToolCall, 0, 10);
        step.tool = Some("Bash".into());
        step.command_shape = shape_value_with("make test", &known());
        memory
            .record_steps(&target, &run, &[step], ms(secs))
            .unwrap();
    }
    let at = i64::try_from(settled).unwrap() * 10 + 100;
    let (open, pending) = (many_run_id(9_001), many_run_id(9_002));
    log.worked(9_001, &open, at);
    log.worked(9_002, &pending, at)
        .integrated(9_002, &pending, at + 700)
        .push_failed(9_002, &pending, at + 800);
    log.add(Some(9_003), None, "task_created", json!({}), at + 900);
    memory.events.replace(log.events.clone());
    log
}

/// The subcommands the rules know.
fn known() -> Subcommands {
    Subcommands::default().keyed(Some(DigestKey::test()))
}

/// The rules that classify `make` commands as `class`.
fn rules(class: &str) -> CommandClassifier {
    let toolchains = Toolchains::none().with_telemetry(TelemetrySettings {
        commands: vec![CommandRule::new(class, "make")],
        ..TelemetrySettings::default()
    });
    CommandClassifier::new(effective_rules(&toolchains), known())
}

/// How much a start and its first pass read by target and from the
/// cursors, on a store of `settled` runs settled before R and later events
/// reaching open rows only.
#[derive(Debug, PartialEq, Eq)]
struct ReadsOfAStart {
    scans: BTreeMap<&'static str, usize>,
    events: usize,
    runs_read: Vec<RunId>,
    read_one: Vec<EventId>,
    cut: CutReads,
}

fn reads_of_a_start(settled: usize) -> ReadsOfAStart {
    let classifier = rules("test");
    let memory = Memory::default();
    let mut log = settled_and_open(settled, &memory);
    let by = |builder: &mut Builder, secs: i64, memory: &Memory| {
        build_pass(
            memory,
            builder,
            (ms(secs), BUILT),
            Retain::default(),
            Some(&classifier),
        )
    };
    let mut first = Builder::default();
    for _ in 0..DRAIN_PASSES {
        if by(&mut first, LATER, &memory).backlog == 0 {
            break;
        }
    }
    let (open, pending) = (many_run_id(9_001), many_run_id(9_002));
    log.add(Some(9_001), Some(&open), "note_added", json!({}), LATER + 1);
    log.add(
        Some(9_002),
        Some(&pending),
        "note_added",
        json!({}),
        LATER + 1,
    );
    memory.events.replace(log.events.clone());

    memory.forget_reads();
    memory.read.set(0);
    memory.scans.borrow_mut().clear();
    memory.cut_reads.set(CutReads::default());
    let mut builder = start_builder(&memory, ms(LATER + 2)).unwrap();
    for _ in 0..DRAIN_PASSES {
        if by(&mut builder, LATER + 2, &memory).backlog == 0 {
            break;
        }
    }
    let mut scans = BTreeMap::new();
    for (cursor, _, read) in memory.scans.borrow().iter() {
        *scans.entry(*cursor).or_insert(0) += read;
    }
    let mut runs_read = memory.runs_read.borrow().clone();
    runs_read.sort();
    ReadsOfAStart {
        scans,
        events: memory.read.get(),
        runs_read,
        read_one: memory
            .read_one
            .borrow()
            .iter()
            .map(|event| event.id)
            .collect(),
        cut: memory.cut_reads.get(),
    }
}

/// A start and its first pass read as many inputs from the cursors, and
/// by target as many times and as many events, however many rows settled
/// before R with no input after it, by the same rules: their records and
/// the events behind them are not read.
#[test]
fn the_reads_of_a_start_do_not_grow_with_the_rows_settled() {
    let (few, many) = (reads_of_a_start(2), reads_of_a_start(40));
    // Ids of what is read shift with the rows before it; only their
    // numbers are to agree.
    assert_eq!(few.scans, many.scans);
    assert_eq!(few.events, many.events);
    assert_eq!(few.runs_read.len(), many.runs_read.len());
    assert_eq!(few.read_one.len(), many.read_one.len());
    assert_eq!(few.cut, many.cut);
    assert_eq!(few.scans["events"], 2);
    // Of the runs read whole only those the scans reached, not settled.
    let open = [many_run_id(9_001), many_run_id(9_002)];
    assert!(
        many.runs_read
            .iter()
            .all(|run| open.iter().any(|id| id == run.as_str())),
        "{:?}",
        many.runs_read
    );
}

/// Rules of another version classify again the runs settled long ago on a
/// store with R, with no input of them after it: the row is that of a build
/// of every input by those rules, and the pass reads the cursors after R
/// alone.
#[test]
fn a_builder_started_from_r_classifies_a_settled_run_again_when_only_the_rules_change() {
    let (first, second) = (rules("build"), rules("test"));
    let memory = Memory::default();
    let log = settled_and_open(1, &memory);
    let by = |builder: &mut Builder, memory: &Memory, classifier: &CommandClassifier| {
        for _ in 0..DRAIN_PASSES {
            let built = build_pass(
                memory,
                builder,
                (ms(LATER), BUILT),
                Retain::default(),
                Some(classifier),
            );
            assert_eq!(built.failure, None);
            if built.backlog == 0 {
                return;
            }
        }
        panic!("marks left");
    };
    by(&mut Builder::default(), &memory, &first);
    let run = many_run_id(1);
    let before = memory.row(LedgerKind::Run, &run);
    assert!(before.is_final);
    assert_eq!(
        crate::domain::ledger::steps::rules_of(&before.detail),
        Some(first.rules().version().as_str())
    );
    let r = r_of(&memory);

    memory.scans.borrow_mut().clear();
    memory.runs_read.borrow_mut().clear();
    let mut builder = start_builder(&memory, ms(LATER)).unwrap();
    by(&mut builder, &memory, &second);
    let scans = scans_of(&memory);
    assert!(!scans.contains_key("events"));
    assert_eq!(scans["steps"], (r.steps, 0));

    let whole = Memory::of(&log);
    whole.steps.replace(memory.steps.borrow().clone());
    whole.step_id.set(memory.step_id.get());
    whole.owners.replace(memory.owners.borrow().clone());
    by(&mut Builder::default(), &whole, &second);
    assert_eq!(
        memory.row(LedgerKind::Run, &run),
        whole.row(LedgerKind::Run, &run)
    );
    assert_ne!(memory.row(LedgerKind::Run, &run), before);
}

/// A store with no mark gives a builder from its cursors' start: it reads
/// every event, and its writes write the marks, from which the next start
/// goes on and reads from R.
#[test]
fn a_store_with_no_mark_starts_from_the_cursors_start_and_the_next_start_goes_on_from_r() {
    let (log, w) = sessions_across();
    let (memory, _) = lagging_store(&log, w);
    assert_eq!(rows_written(&memory).unwrap(), None);
    memory.events.replace(log.events.clone());
    let mut builder = start_builder(&memory, ms(LATER)).unwrap();
    assert_eq!(builder.cursors(), Cursors::default());
    let built = drain(&memory, &mut builder, LATER);
    assert_eq!(built.failure, None);
    let first_scan = memory.scans.borrow()[0];
    assert_eq!(first_scan, ("events", 0, log.events.len()));
    assert!(are_of_every_event(
        &rows_of_kind(&memory, LedgerKind::Session),
        &log
    ));
    let r = r_of(&memory);
    assert_eq!(r, builder.cursors());
    assert_eq!(r.events, log.last());

    memory.scans.borrow_mut().clear();
    let mut next = start_builder(&memory, ms(LATER)).unwrap();
    assert_eq!(next.cursors(), r);
    pass(&memory, &mut next, LATER);
    assert!(!scans_of(&memory).contains_key("events"));
}

/// (a) On a store whose session rows and marks are written and whose run,
/// task, queue and node rows lag past the bound of writes, with no input
/// after, a start writes what the store lacks as the build of every input.
/// Without it the rows lack them.
#[test]
fn a_start_on_rows_left_past_the_bound_writes_what_the_store_lacks() {
    let (mut log, samples) = every_kind_of_row();
    for task in 1_000..1_000 + i64::try_from(WRITES_PER_PASS).unwrap() {
        log.add(Some(task), None, "task_created", json!({}), 3_300);
    }
    let runs = WRITES_PER_PASS + 5;
    for n in 1..=runs {
        log.landed(
            2_000 + i64::try_from(n).unwrap(),
            &many_run_id(n),
            i64::try_from(n).unwrap() * 10,
        );
    }
    let memory = Memory::of(&log);
    memory.samples.replace(samples);
    let whole = rows_of_every_input(&memory, LATER);
    let built = pass(&memory, &mut Builder::default(), LATER);
    assert_eq!(built.failure, None);
    assert!(built.backlog > 0);
    assert_eq!(
        sessions_written_through(&memory).unwrap(),
        Some(log.last()),
        "the session rows are written"
    );
    let r = r_of(&memory);
    assert!(r.events < log.last());
    let held = memory.rows.borrow().clone();
    let lacking = differing(&held, &whole);
    for kind in [
        LedgerKind::Run,
        LedgerKind::Task,
        LedgerKind::Queue,
        LedgerKind::Node,
    ] {
        assert!(lacking.iter().any(|(of, _)| *of == kind), "{kind:?}");
    }

    let rows = started_and_drained(&memory, &log, LATER);
    assert_eq!(differing(&rows, &whole), Vec::new());
    assert_eq!(r_of(&memory).events, log.last());
}

/// (a) So on a store whose run rows lag by the windows of their resources
/// and by their steps, past the bound of writes, with no other input after:
/// R stops at each cursor's first such input, and a start writes them.
#[test]
fn a_start_on_run_rows_stale_past_the_bound_writes_their_late_inputs() {
    let runs = WRITES_PER_PASS + 5;
    let mut log = Log::default();
    let mut attachments = Vec::new();
    for n in 1..=runs {
        let run = many_run_id(n);
        let secs = i64::try_from(n).unwrap() * 1_000;
        log.landed(i64::try_from(n).unwrap(), &run, secs);
        attachments.push((run.clone(), attach(&mut log, &run, secs), secs));
    }
    let memory = Memory::of(&log);
    drain(&memory, &mut Builder::default(), LATER);
    for (run, attachment, secs) in &attachments {
        measured(
            &memory,
            run,
            *attachment,
            (secs + 10, secs + 300),
            Some(30.0),
            true,
        );
    }
    let mut builder = Builder::default();
    drain(&memory, &mut builder, LATER);
    for (run, _, secs) in &attachments {
        let target = StepTarget::run(&RunId::new(run).unwrap());
        memory.register_session(&target, run, ms(*secs)).unwrap();
        let mut step = Step::new(1, StepKind::ToolCall, 0, 10);
        step.tool = Some("Bash".into());
        memory
            .record_steps(&target, run, &[step], ms(*secs))
            .unwrap();
    }
    let whole = rows_of_every_input(&memory, LATER);
    let built = pass(&memory, &mut builder, LATER);
    assert_eq!((built.failure, built.written), (None, WRITES_PER_PASS));
    let r = r_of(&memory);
    assert_eq!(r.events, log.last());
    assert!(r.steps < builder.steps);
    assert!(!differing(&memory.rows.borrow(), &whole).is_empty());

    let rows = started_and_drained(&memory, &log, LATER);
    assert_eq!(differing(&rows, &whole), Vec::new());
}

/// (b) On a store whose R is before its W, a start takes the session rows at
/// R: no round twice and no late record lost. Taking them at W and going on
/// from R gives a planner's next session a round again.
#[test]
fn a_start_with_r_before_w_takes_the_session_rows_at_r() {
    let (log, position) = sessions_behind_tasks();
    for upto in [position, position + 2, log.events.len()] {
        let memory = Memory::default();
        memory.events.replace(log.events[..upto].to_vec());
        let built = pass(&memory, &mut Builder::default(), LATER);
        assert_eq!(built.failure, None);
        let w = sessions_written_through(&memory).unwrap().unwrap();
        let r = r_of(&memory);
        assert!(r.events < w, "{upto}");

        let whole = Memory::of(&log);
        drain(&whole, &mut Builder::default(), LATER);
        let rows = started_and_drained(&memory, &log, LATER);
        assert_eq!(
            differing(&rows, &whole.rows.into_inner()),
            Vec::new(),
            "{upto}"
        );
    }

    // Taken at W, the events between R and W are assigned twice.
    let upto = log.events.len();
    let memory = Memory::default();
    memory.events.replace(log.events[..upto].to_vec());
    pass(&memory, &mut Builder::default(), LATER);
    let w = sessions_written_through(&memory).unwrap().unwrap();
    let r = r_of(&memory);
    let mut builder = Builder::default();
    builder.go_on_from_rows(r);
    let held = SessionsHeld::at(
        &memory.ledger_heads(LedgerKind::Session).unwrap(),
        &memory.ledger_session_anchors().unwrap(),
        w,
    );
    let records = held
        .open
        .iter()
        .flat_map(|key| memory.row(LedgerKind::Session, key).records)
        .collect();
    builder.restore_sessions(held, records);
    drain(&memory, &mut builder, LATER + 1);
    assert!(!are_of_every_event(
        &rows_of_kind(&memory, LedgerKind::Session),
        &log
    ));
}

/// (c) A store with no mark that lacks a new session's row and a closed
/// session's late record, and whose run, task and queue rows lag past the
/// bound of writes, starts from its cursors' start and writes the rows of
/// every event.
#[test]
fn a_start_on_a_store_with_no_mark_and_lagging_rows_writes_the_rows_of_every_event() {
    let (sessions, w) = sessions_across();
    let (held, _) = lagging_store(&sessions, w);
    let mut log = sessions;
    let start = log.last().as_i64();
    for n in 0..WRITES_PER_PASS + 10 {
        log.landed(
            5_000 + i64::try_from(n).unwrap(),
            &many_run_id(n + 1),
            i64::try_from(n).unwrap() * 10,
        );
    }
    assert!(log.last().as_i64() > start);
    let memory = Memory::of(&log);
    memory.rows.replace(held.rows.into_inner());
    assert_eq!(rows_written(&memory).unwrap(), None);
    let whole = Memory::of(&log);
    drain(&whole, &mut Builder::default(), LATER);
    let rows = started_and_drained(&memory, &log, LATER);
    assert_eq!(differing(&rows, &whole.rows.into_inner()), Vec::new());
}

/// A store with R that holds a row older than this version starts from its
/// cursors' start; so does one with no mark whatever its rows' versions. A
/// legacy row's version, which the store keeps as first written, is no such
/// row: a start goes on from R over it.
#[test]
fn a_store_with_an_older_row_starts_from_the_cursors_start() {
    let (log, samples) = every_kind_of_row();
    let whole = drained_store(&log, &samples, log.events.len(), LATER)
        .rows
        .into_inner();

    let older = |memory: &Memory, key: &(LedgerKind, String)| {
        memory.rows.borrow_mut().get_mut(key).unwrap().ledger_v = LEDGER_V - 1;
    };
    let some_task = whole
        .keys()
        .find(|(kind, _)| *kind == LedgerKind::Task)
        .unwrap()
        .clone();

    let memory = drained_store(&log, &samples, log.events.len(), LATER);
    older(&memory, &some_task);
    assert!(rows_written(&memory).unwrap().is_some());
    let mut builder = start_builder(&memory, ms(LATER)).unwrap();
    assert_eq!(builder.cursors(), Cursors::default());
    drain(&memory, &mut builder, LATER);
    assert_eq!(differing(&memory.rows.borrow(), &whole), Vec::new());
    assert_eq!(memory.scans.borrow().first().unwrap().1, 0);

    let memory = drained_store(&log, &samples, log.events.len(), LATER);
    older(&memory, &some_task);
    memory.mark.replace(None);
    memory.rows_mark.replace(None);
    assert_eq!(rows_written(&memory).unwrap(), None);
    let mut builder = start_builder(&memory, ms(LATER)).unwrap();
    assert_eq!(builder.cursors(), Cursors::default());
    drain(&memory, &mut builder, LATER);
    assert_eq!(differing(&memory.rows.borrow(), &whole), Vec::new());

    let legacy = Memory::default();
    let mut log = Log::default();
    log.legacy(1, &run_id(1), 0);
    legacy.events.replace(log.events.clone());
    drain(&legacy, &mut Builder::default(), LATER);
    assert!(
        legacy
            .rows
            .borrow_mut()
            .values_mut()
            .filter(|row| row.is_legacy())
            .map(|row| row.ledger_v = LEDGER_V - 1)
            .count()
            > 0
    );
    assert!(!legacy.ledger_older_rows().unwrap());
    let builder = start_builder(&legacy, ms(LATER)).unwrap();
    assert_eq!(builder.cursors(), r_of(&legacy));
    assert_eq!(builder.events, log.last());
}

/// The kind of a landing slot's rows and the keys of those in `rows`.
fn landing_slots(rows: &Rows) -> BTreeMap<String, LedgerRow> {
    rows.iter()
        .filter(|((kind, _), row)| {
            *kind == LedgerKind::Queue
                && row
                    .subject
                    .as_ref()
                    .is_some_and(|subject| subject.kind == "landing_slot")
        })
        .map(|((_, key), row)| (key.clone(), row.clone()))
        .collect()
}

/// Two attempts of the landing slot: one before the other, and the other's
/// two events after its end timed before it ([`two_late_log`]); the id of
/// the other's `run_integrated`, and the first attempt's run.
fn two_attempts() -> (Log, i64, String) {
    let run = run_id(9);
    let mut log = Log::default();
    log.add(Some(50), Some(&run), "integration_started", json!({}), 1)
        .add(Some(50), Some(&run), "run_integrated", json!({}), 2);
    let shift = log.last().as_i64();
    for event in two_late_log().events {
        log.events.push(RunEvent {
            id: EventId::new(event.id.as_i64() + shift),
            ..event
        });
    }
    (log, shift + 2, run)
}

/// The landing slot's rows of the fold of the first `upto` events of `log`
/// at `secs`, on an empty store.
fn slots_of_fold(log: &Log, upto: usize, secs: i64) -> BTreeMap<String, LedgerRow> {
    let step = Memory::default();
    step.events.replace(log.events[..upto].to_vec());
    drain(&step, &mut Builder::default(), secs);
    landing_slots(&step.rows.borrow())
}

/// R before two events after a landing slot's attempt ended timed before
/// its end, the store's rows written over the events up to `written` at
/// `stored_at`: a builder started there whose passes at `secs` read those
/// events one at a time from `from` leaves the slot's rows as the store
/// holds them or writes the fold of every event up to the pass; in the end
/// they are those of the build of every event. The keys it wrote of them,
/// and whether a fold up to a pass was behind the store's row.
fn replayed_slots(
    (written, stored_at): (usize, i64),
    from: usize,
    secs: i64,
) -> (BTreeSet<String>, bool) {
    let (log, r_at, _) = two_attempts();
    let memory = drained_store(&log, &[], written, stored_at);
    let stored = landing_slots(&memory.rows.borrow());
    assert_eq!(stored.len(), 2);
    let mut r = r_of(&memory);
    r.events = EventId::new(r_at);
    memory
        .rows_mark
        .replace(Some(RowsWritten::at(r, read_ms_at(&log, r.events))));
    assert_eq!(
        rows_written(&memory).unwrap().unwrap().events,
        EventId::new(r_at)
    );

    memory.written_keys.borrow_mut().clear();
    let mut builder = start_builder(&memory, ms(secs)).unwrap();
    let mut wrote = BTreeSet::new();
    let mut went_back = false;
    for upto in from..=log.events.len() {
        memory.events.replace(log.events[..upto].to_vec());
        let built = passes_to_drain(&memory, &mut builder, secs);
        assert_eq!(built.failure, None);
        let fold = slots_of_fold(&log, upto, secs);
        went_back |= fold
            .iter()
            .any(|(key, row)| row.inputs_through.events < stored[key].inputs_through.events);
        let written: BTreeSet<String> = memory
            .written_keys
            .borrow_mut()
            .drain(..)
            .filter(|(kind, _)| *kind == LedgerKind::Queue)
            .map(|(_, key)| key)
            .collect();
        for (key, row) in &landing_slots(&memory.rows.borrow()) {
            if written.contains(key) {
                assert_eq!(fold.get(key), Some(row), "{upto}: {key} as written");
                wrote.insert(key.clone());
            }
        }
    }
    assert_eq!(
        landing_slots(&memory.rows.borrow()),
        slots_of_fold(&log, log.events.len(), secs)
    );
    (wrote, went_back)
}

/// [`replayed_slots`]. Over rows settled and written past the two events,
/// even read one at a time from R, a fold behind the store's row writes
/// nothing, so that no slot's row is written: the other attempt's, which no
/// event reaches, among them, and each is the build of every event as the
/// store held it. Over rows in their grace written at the first of the two,
/// each row settles as the fold up to the pass, and the attempt's is
/// written again with the second.
#[test]
fn a_landing_slots_row_replayed_from_r_is_the_fold_up_to_each_pass_or_is_not_written() {
    let (log, r_at, _) = two_attempts();
    let n = log.events.len();
    let r_at = usize::try_from(r_at).unwrap();
    let (wrote, went_back) = replayed_slots((n, LATER), r_at + 1, LATER);
    assert_eq!(wrote, BTreeSet::new());
    assert!(went_back);

    let first_late = r_at + 2;
    let in_grace = last_secs(&log, first_late) + 1;
    let (wrote, _) = replayed_slots((first_late, in_grace), first_late, LATER);
    assert_eq!(wrote.len(), 2);
}

/// A started builder takes the latest time of the events up to R, as one
/// that read them all, though the event at R is timed before an earlier one.
#[test]
fn a_started_builder_holds_the_latest_time_read_up_to_r() {
    let mut log = Log::default();
    log.add(Some(1), None, "task_created", json!({}), 100).add(
        Some(2),
        None,
        "task_created",
        json!({}),
        50,
    );
    let memory = Memory::of(&log);
    let mut whole = Builder::default();
    drain(&memory, &mut whole, 200);
    let started = start_builder(&memory, ms(200)).unwrap();
    assert_eq!(started.events, log.last());
    assert_eq!(started.folds_at(ms(0)), ms(100));
    assert_eq!(started.folds_at(ms(0)), whole.folds_at(ms(0)));
}

/// [`pass_of_watch`]: a store that does not open keeps the builder as it
/// was, none or not; a start that fails gives none, and the next pass
/// starts from the marks, not from its cursors' start.
#[test]
fn a_watch_that_cannot_start_starts_again_and_never_falls_to_the_cursors_start() {
    let (log, position) = sessions_across();
    let memory = drained_to(&log, position);
    let r = r_of(&memory);
    assert!(r.events.as_i64() > 0);
    let at = (ms(LATER), BUILT);
    let closed = anyhow::anyhow!("no queue");

    let (held, built) = pass_of_watch(Err(&closed), None, at, Retain::default(), None);
    assert!(held.is_none());
    assert!(built.failure.unwrap().contains("no queue"));
    let mut going = Builder::default();
    going.go_on_from_rows(r);
    let (held, built) = pass_of_watch(
        Err(&closed),
        Some(going.clone()),
        at,
        Retain::default(),
        None,
    );
    assert_eq!(held, Some(going));
    assert!(built.failure.is_some());

    let open = memory
        .rows
        .borrow()
        .values()
        .find(|row| row.kind == LedgerKind::Session && !row.is_final)
        .map(|row| (row.kind, row.key.clone()))
        .unwrap();
    memory.broken.replace(Some(open));
    let (held, built) = pass_of_watch(Ok(&memory), None, at, Retain::default(), None);
    assert!(held.is_none());
    assert!(built.failure.unwrap().contains("start the builder"));
    memory.broken.replace(None);
    memory.scans.borrow_mut().clear();
    let (held, built) = pass_of_watch(Ok(&memory), None, at, Retain::default(), None);
    assert_eq!(built.failure, None);
    assert_eq!(held.unwrap().cursors(), r);
    assert_eq!(scans_of(&memory)["steps"].0, r.steps);
    assert!(!scans_of(&memory).contains_key("events"));
}

/// A read that fails on the way gives no builder: a pass does not go on
/// from books made in part.
#[test]
fn a_start_whose_read_fails_gives_no_builder() {
    let (log, position) = sessions_across();
    let memory = drained_to(&log, position);
    let open = memory
        .rows
        .borrow()
        .values()
        .find(|row| row.kind == LedgerKind::Session && !row.is_final)
        .map(|row| (row.kind, row.key.clone()))
        .unwrap();
    memory.broken.replace(Some(open));
    assert!(start_builder(&memory, ms(LATER)).is_err());
}

/// A start takes as many queue heads at once however many settled rows of
/// each kind it forgets the store holds, and takes them.
#[test]
fn a_start_takes_as_many_queue_heads_however_many_rows_it_forgets() {
    let taken = |fill: usize| {
        let log = forgotten_log(fill);
        let memory = Memory::of(&log);
        drain(&memory, &mut Builder::default(), LATER);
        assert_eq!(r_of(&memory).events, log.last());
        let builder = start_builder(&memory, ms(LATER)).unwrap();
        assert_eq!(builder.events, log.last());
        let whole = memory.ledger_queue_heads().unwrap().len();
        (
            memory.heads_given.get(),
            whole,
            builder.kept().size().queue_rows,
        )
    };
    let (few, many) = (taken(2), taken(40));
    assert!(few.0 > 0);
    assert_eq!(few.0, many.0);
    assert_eq!(few.2, many.2);
    assert_eq!(many.1 - few.1, 38 * 3);
}

/// The books made at any position of the heads left out at it are those of
/// every head ([`books_at`]), the rows written at every position or later;
/// and a builder started on them writes the rows of every event: for the
/// rows of each kind left out.
#[test]
fn the_books_and_rows_of_a_start_are_those_of_every_head() {
    let log = forgotten_log(3);
    let whole = Memory::of(&log);
    drain(&whole, &mut Builder::default(), LATER);
    let every = whole.rows.borrow().clone();
    let mut left_out = 0;
    for at in log.events.iter().map(|event| event.id) {
        let heads = whole.ledger_queue_heads_at(at).unwrap();
        left_out = left_out.max(whole.ledger_queue_heads().unwrap().len() - heads.len());
        assert_eq!(
            books_of_heads(&whole, &heads, at, ms(LATER)).unwrap(),
            books_at(&whole, at, ms(LATER)).unwrap(),
            "{at}"
        );
    }
    assert!(left_out >= 3 * 3);
    for upto in 1..=log.events.len() {
        let memory = drained_store(&log, &[], upto, LATER);
        let from_start = from_the_start_on(&memory, &log, LATER);
        let rows = started_and_drained(&memory, &log, LATER);
        assert_eq!(differing(&rows, &from_start), Vec::new(), "{upto}");
        assert_eq!(differing_from_whole(&rows, &every), Vec::new(), "{upto}");
    }
}
