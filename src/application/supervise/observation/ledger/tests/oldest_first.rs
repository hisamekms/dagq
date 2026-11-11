//! A mark holding R back is looked at every pass, whatever rank of the
//! round a new process restores: queue marks of an older version that
//! grow past a pass's checks faster than the round comes back to the
//! tasks stop no task's position, and R moves past it.

use super::*;

/// More queue marks of an older version than two passes look at, so that
/// a round that goes on from the rank restored never comes back to the
/// tasks while they grow by this every pass.
const GROWTH: usize = 2 * CHECKS_PER_PASS + 500;

/// `GROWTH` events, each a queue row the store holds of an older version
/// with its input recorded: a sweep keeps each marked for its rewrite,
/// holding no position.
fn grow(log: &mut Log, memory: &Memory) {
    let from = log.events.len();
    for _ in 0..GROWTH {
        log.add(None, None, ONE_EVENT, json!({}), 1);
    }
    memory.events.replace(log.events.clone());
    for mut row in crate::domain::ledger::build(&log.events[from..], ms(2), BUILT) {
        row.ledger_v = LEDGER_V - 1;
        memory
            .rows
            .borrow_mut()
            .insert((row.kind, row.key.clone()), row);
    }
}

/// One pass of a new process's builder, and the mark R it left.
fn restart(memory: &Memory) -> (RowsWritten, Built) {
    let mut builder = start_builder(memory, ms(2)).unwrap();
    let built = pass(memory, &mut builder, 2);
    assert_eq!(built.failure, None);
    (memory.rows_mark.borrow().unwrap(), built)
}

#[test]
fn the_oldest_mark_holding_r_is_taken_whatever_the_rank_restored() {
    let mut log = Log::default();
    log.add(Some(1), None, "task_created", json!({"status": "draft"}), 1);
    let memory = Memory::of(&log);
    assert_eq!(restart(&memory).0.through.events, log.last());
    grow(&mut log, &memory);
    let (moved, _) = restart(&memory);
    assert_eq!(moved.through.events, log.last());
    assert!(moved.take_offset > 0, "the round's rank is past the tasks");

    // A later event of the task marks it, holding R before it.
    let before = log.last();
    log.add(
        Some(1),
        None,
        "task_status_changed",
        json!({"from": "draft", "to": "ready"}),
        1,
    );
    // While its row cannot be read, nothing records the task's input: it
    // holds R where it was, and the pass's round writes the others.
    memory
        .broken
        .replace(Some((LedgerKind::Task, "1".to_owned())));
    grow(&mut log, &memory);
    let mut builder = start_builder(&memory, ms(2)).unwrap();
    let failed = pass(&memory, &mut builder, 2);
    assert!(failed.failure.is_some());
    assert_eq!(failed.written, WRITES_PER_PASS);
    drop(builder);
    assert!(memory.rows_mark.borrow().unwrap().through.events <= before);
    memory.broken.replace(None);
    let mut last = before;
    for _ in 0..2 {
        grow(&mut log, &memory);
        let (mark, built) = restart(&memory);
        assert!(mark.through.events >= last, "R does not go back");
        last = mark.through.events;
        assert!(
            built
                .marks
                .oldest
                .as_ref()
                .is_none_or(|(target, _)| *target != Target::Task(TaskId::new(1))),
            "task 1 still holds R: {:?}",
            built.marks.oldest
        );
    }
    assert!(last > before, "R moved past the task's position");
    // With no new input, R comes to the latest event within a pass.
    let (mark, _) = restart(&memory);
    assert_eq!(mark.through.events, log.last());

    // Every row written again at this version, task 1's too, is the fold
    // of every event: what a pass that started anew wrote holds no input
    // it skipped.
    let inputs = Inputs::new(&log.events, ms(2), BUILT, true);
    let rows = memory.rows.borrow();
    let task = &rows[&(LedgerKind::Task, "1".to_owned())];
    assert_eq!(task.inputs_through.events, log.events[GROWTH + 1].id);
    let mut written = 0;
    for ((kind, key), row) in rows.iter().filter(|(_, row)| row.ledger_v == LEDGER_V) {
        let target = match kind {
            LedgerKind::Task => Target::Task(TaskId::new(key.parse().unwrap())),
            _ => Target::Row(*kind, key.clone()),
        };
        assert_eq!(
            row,
            &build(&target, &inputs, &[]).unwrap(),
            "{kind:?} {key}"
        );
        written += 1;
    }
    assert!(written > WRITES_PER_PASS, "{written}");
}

/// What a pass's R holds of the cursors other than the events.
fn others(mark: &RowsWritten) -> [i64; 4] {
    let at = mark.through;
    [at.samples, at.steps, at.resources, at.inside_resources]
}

/// A mark holding R's other cursors back only (a run's steps, its windows
/// measured from outside, its samples from inside, a node's samples) is
/// looked at every pass too, whatever rank of the round a new process
/// restores: no cursor of R stays where it held it, none goes back, and
/// with no new input each comes to the latest within a pass.
#[test]
fn the_oldest_mark_holding_each_cursor_of_r_is_taken_whatever_the_rank_restored() {
    let (steps, outside, inside) = (run_id(1), run_id(2), run_id(3));
    let mut log = Log::default();
    log.landed(1, &steps, -1_000);
    log.landed(2, &outside, -1_000);
    let attachment = attach(&mut log, &outside, -1_000);
    log.landed(3, &inside, -1_000);
    let memory = Memory::of(&log);
    let hour = node::bucket_of(T0);
    memory.samples.replace(vec![sample(1, hour + 1)]);
    let target = session_of(&memory, &steps, "s-1", -990);
    steps_in(
        &memory,
        &target,
        "s-1",
        &[Step::new(1, StepKind::ModelTurn, 0, 1_000)],
        ms(-900),
    );
    measured(
        &memory,
        &outside,
        attachment,
        (-990, -400),
        Some(1.0),
        false,
    );
    let within = session_of(&memory, &inside, "s-2", -990);
    memory
        .record_resources(
            &within,
            "s-2",
            &[ResourceSample::new(1, 0, 1_000)],
            ms(-900),
        )
        .unwrap();
    let (first, _) = restart(&memory);
    assert_eq!(first.through.events, log.last());
    assert_eq!(others(&first), [1, 1, 1, 1]);
    grow(&mut log, &memory);
    let (moved, _) = restart(&memory);
    assert!(moved.take_offset > 0, "the round's rank is past the runs");

    // A later input of each cursor marks a row with no event, holding
    // that cursor of R before it.
    memory.samples.borrow_mut().push(sample(2, hour + 2));
    steps_in(
        &memory,
        &target,
        "s-1",
        &[Step::new(2, StepKind::ToolCall, 1_000, 1_000)],
        ms(-800),
    );
    measured(&memory, &outside, attachment, (-400, -70), Some(1.0), true);
    memory
        .record_resources(
            &within,
            "s-2",
            &[ResourceSample::new(2, 1_000, 1_000)],
            ms(-800),
        )
        .unwrap();
    let mut last = others(&moved);
    for _ in 0..3 {
        grow(&mut log, &memory);
        let (mark, _) = restart(&memory);
        let now = others(&mark);
        assert!(
            now.iter().zip(last).all(|(now, last)| *now >= last),
            "R does not go back: {last:?} to {now:?}"
        );
        last = now;
    }
    assert_eq!(last, [2, 2, 2, 2], "every cursor of R moved past its mark");
    // With no new input, R comes to the latest of every cursor.
    let (mark, _) = restart(&memory);
    assert_eq!(mark.through.events, log.last());
    assert_eq!(others(&mark), [2, 2, 2, 2]);

    // The run, task and node rows are the fold of every input: those of
    // the runs and tasks, as the queue's older rows are no input of theirs.
    let theirs = Memory::of(&Log {
        events: log
            .events
            .iter()
            .filter(|event| event.task_id.is_some() || event.run_id.is_some())
            .cloned()
            .collect(),
    });
    theirs.samples.replace(memory.samples.borrow().clone());
    theirs
        .run_resources
        .replace(memory.run_resources.borrow().clone());
    theirs.steps.replace(memory.steps.borrow().clone());
    theirs.step_id.set(memory.step_id.get());
    theirs.owners.replace(memory.owners.borrow().clone());
    theirs.inside.replace(memory.inside.borrow().clone());
    theirs.inside_id.set(memory.inside_id.get());
    let whole = rows_of_every_input(&theirs, 2);
    let rows = memory.rows.borrow();
    let mut compared = 0;
    for (key, row) in whole.iter().filter(|((kind, _), _)| {
        matches!(kind, LedgerKind::Run | LedgerKind::Task | LedgerKind::Node)
    }) {
        assert_eq!(rows.get(key), Some(row), "{key:?}");
        compared += 1;
    }
    assert_eq!(compared, 3 + 3 + 1);
}
