//! The summaries of the pushes made again from the run and task rows
//! ([`restore_pushes`]) against the build of every event, over histories
//! laid down at random from a seed: tasks with a run from before the
//! switch (legacy, some of which go on recording their phases and pushes
//! after it) and runs of today's form after it, their pushes failing
//! and pending, going back to their landing, pushed, ended without a
//! landing, tasks completed while a push is pending or canceled, notes,
//! telemetry tokens, and pushes of no run. At every position `at` and for
//! rows written at a position P at or after it (built up to `at` alone,
//! written pass by pass as the events came and taken at several P, a
//! legacy row kept as first written, its mark and its version, the version
//! today's or an older one, and
//! built up to the end alone), the summaries (where each run's last failed
//! push went, the runs waiting, the push that closed each, and the runs and
//! tasks not settled that read each push kept, so whether each had settled)
//! are those of the build of every event up to `at`, reading only what the
//! rows allow; a builder going on from them writes the run and task rows
//! that build writes.

use super::*;
use crate::domain::run_phase::Phase;

/// A generator of numbers from a seed (xorshift64*): the same seed lays
/// down the same history.
struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number below `n`.
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }

    /// True `percent` times in a hundred.
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn branch(&mut self) -> &'static str {
        ["main", "late", "other"][self.below(3)]
    }
}

/// The events an actor of a history records at once.
#[derive(Debug, Clone)]
enum Step {
    Claimed(i64, String),
    Worked(i64, String),
    /// Ended without a landing.
    Failed(i64, String),
    Landing(i64, String),
    Integrated(i64, String),
    Push(i64, String),
    Pushed(i64, String, &'static str),
    PushFailed(i64, String, &'static str),
    PushPending(i64, String),
    Completed(i64),
    Canceled(i64),
    Note(i64),
    RunNote(i64, String),
    TokenIssued(i64, String),
    TokenRevoked(i64, String),
    /// A push of no run, of a task or of none.
    Runless(Option<i64>, &'static str),
}

fn lay(log: &mut Log, step: &Step, secs: i64) {
    let pushed = |branch: &str| json!({"remote": "origin", "branch": branch, "commit": "c"});
    let status = |to: &str| json!({"from": "in_progress", "to": to});
    match step {
        Step::Claimed(task, run) => {
            log.add(
                Some(*task),
                Some(run),
                "run_claimed",
                json!({"from": "ready", "to": "in_progress", "provider": "claude"}),
                secs,
            )
            .phase(*task, run, secs, Phase::Provisioning, "x");
        }
        Step::Worked(task, run) => {
            log.add(Some(*task), Some(run), "agent_started", json!({}), secs)
                .phase(*task, run, secs, Phase::Worker, "x")
                .add(Some(*task), Some(run), "receipt_observed", json!({}), secs)
                .phase(*task, run, secs, Phase::Validating, "x")
                .add(
                    Some(*task),
                    Some(run),
                    "validation_finished",
                    json!({"status": "awaiting_integration"}),
                    secs,
                )
                .phase(*task, run, secs, Phase::AwaitingSlot, "x");
        }
        Step::Failed(task, run) => {
            log.phase(*task, run, secs, Phase::Ended, "failed");
        }
        Step::Landing(task, run) => {
            log.phase(*task, run, secs, Phase::Landing, "x");
        }
        Step::Integrated(task, run) => {
            log.add(
                Some(*task),
                Some(run),
                "run_integrated",
                json!({"from": "validating", "to": "integrated", "status": "integrated"}),
                secs,
            );
        }
        Step::Push(task, run) => {
            log.phase(*task, run, secs, Phase::Push { in_slot: true }, "x");
        }
        Step::Pushed(task, run, branch) => {
            log.add(
                Some(*task),
                Some(run),
                "push_finished",
                pushed(branch),
                secs,
            )
            .phase(*task, run, secs, Phase::Ended, "pushed");
        }
        Step::PushFailed(task, run, branch) => {
            log.add(
                Some(*task),
                Some(run),
                "push_failed",
                json!({"remote": "origin", "branch": branch, "error": "x"}),
                secs,
            );
        }
        Step::PushPending(task, run) => {
            log.phase(*task, run, secs, Phase::PushPending, "push_failed");
        }
        Step::Completed(task) => {
            log.add(
                Some(*task),
                None,
                "task_status_changed",
                status("completed"),
                secs,
            );
        }
        Step::Canceled(task) => {
            log.add(
                Some(*task),
                None,
                "task_status_changed",
                status("canceled"),
                secs,
            );
        }
        Step::Note(task) => {
            log.add(Some(*task), None, "note_added", json!({}), secs);
        }
        Step::RunNote(task, run) => {
            log.add(Some(*task), Some(run), "note_added", json!({}), secs);
        }
        Step::TokenIssued(task, run) => {
            log.add(
                Some(*task),
                Some(run),
                "telemetry_token_issued",
                json!({}),
                secs,
            );
        }
        Step::TokenRevoked(task, run) => {
            log.add(
                Some(*task),
                Some(run),
                "telemetry_token_revoked",
                json!({}),
                secs,
            );
        }
        Step::Runless(task, branch) => {
            log.add(*task, None, "push_finished", pushed(branch), secs);
        }
    }
}

/// The steps of a run of today's form of `task`: claimed and worked, then
/// ended without a landing, or landed and pushed, or its push failed and
/// pending (its task completed meanwhile or not), left there or back to
/// its landing (left there, or pushed again), up to three pushes. Now and
/// then its telemetry token is issued, revoked at the end or not, and a
/// note of it comes after them.
fn run_steps(seeded: &mut Seeded, task: i64, run: &str) -> Vec<Step> {
    let mut steps = pushes_of_run(seeded, task, run);
    let run = run.to_owned();
    if seeded.chance(20) {
        steps.insert(1, Step::TokenIssued(task, run.clone()));
        if seeded.chance(70) {
            steps.push(Step::TokenRevoked(task, run.clone()));
        }
    }
    if seeded.chance(20) {
        steps.push(Step::RunNote(task, run));
    }
    steps
}

/// The steps of the run's work and pushes ([`run_steps`]).
fn pushes_of_run(seeded: &mut Seeded, task: i64, run: &str) -> Vec<Step> {
    let run = run.to_owned();
    let mut steps = vec![
        Step::Claimed(task, run.clone()),
        Step::Worked(task, run.clone()),
    ];
    if seeded.chance(15) {
        steps.push(Step::Failed(task, run));
        return steps;
    }
    steps.extend([
        Step::Landing(task, run.clone()),
        Step::Integrated(task, run.clone()),
        Step::Push(task, run.clone()),
    ]);
    for _ in 0..3 {
        let branch = seeded.branch();
        if seeded.chance(40) {
            steps.push(Step::Pushed(task, run.clone(), branch));
            if seeded.chance(80) {
                steps.push(Step::Completed(task));
            }
            return steps;
        }
        steps.extend([
            Step::PushFailed(task, run.clone(), branch),
            Step::PushPending(task, run.clone()),
        ]);
        if seeded.chance(40) {
            steps.push(Step::Completed(task));
        }
        if seeded.chance(30) {
            return steps;
        }
        steps.push(Step::Landing(task, run.clone()));
        if seeded.chance(25) {
            return steps;
        }
        steps.push(Step::Push(task, run.clone()));
    }
    steps
}

/// A history laid down from `seed` and the time (seconds) of the passes:
/// before the switch, each task created and, for some, a legacy run of
/// it ([`Log::legacy`]: no `run_phase_changed`, landed); after it, the
/// steps of every actor (each task's runs of today's form in turn, after
/// its legacy run's own for one that goes on recording its phases and
/// pushes, its notes and cancel, and pushes of no run) taken one at a time
/// from an actor picked at random, the time moving a little or, now and
/// then, past the grace of settling.
pub(super) fn mixed(seed: u64) -> (Log, i64) {
    let mut seeded = Seeded(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut going_on = Seeded(seed.wrapping_mul(0xA24B_AED4_963E_E407) | 1);
    let mut log = Log::default();
    let id = |task: i64, n: usize| format!("{task:08x}-{n}{n}{n}{n}-4333-8333-333333333333");
    let tasks = 2 + i64::try_from(seeded.below(3)).unwrap();
    let mut secs = 0;
    let mut actors: Vec<std::collections::VecDeque<Step>> = Vec::new();
    for task in 1..=tasks {
        log.add(Some(task), None, "task_created", json!({}), secs);
        let mut steps = Vec::new();
        if seeded.chance(50) {
            log.legacy(task, &id(task, 1), secs);
            secs += 1300;
            if going_on.chance(40) {
                let of_it = pushes_of_run(&mut going_on, task, &id(task, 1));
                steps.extend(of_it.into_iter().skip(1));
            }
        }
        for n in 0..=seeded.below(3) {
            steps.extend(run_steps(&mut seeded, task, &id(task, n + 2)));
        }
        actors.push(steps.into());
        let mut others = Vec::new();
        for _ in 0..seeded.below(3) {
            others.push(Step::Note(task));
        }
        if seeded.chance(20) {
            others.insert(seeded.below(others.len() + 1), Step::Canceled(task));
        }
        actors.push(others.into());
    }
    for _ in 0..seeded.below(3) {
        let task = seeded
            .chance(50)
            .then(|| 1 + i64::try_from(seeded.below(4)).unwrap());
        actors.push(vec![Step::Runless(task, seeded.branch())].into());
    }
    secs += 2000;
    loop {
        actors.retain(|steps| !steps.is_empty());
        if actors.is_empty() {
            break;
        }
        let at = seeded.below(actors.len());
        let step = actors[at].pop_front().unwrap();
        secs += if seeded.chance(8) {
            900 + i64::try_from(seeded.below(600)).unwrap()
        } else {
            1 + i64::try_from(seeded.below(60)).unwrap()
        };
        lay(&mut log, &step, secs);
    }
    let now = secs + i64::try_from(seeded.below(1200)).unwrap();
    (log, now)
}

/// A store holding `rows` (those a build wrote at a position) and the
/// events of `log`.
fn holding(log: &Log, rows: &BTreeMap<(LedgerKind, String), LedgerRow>) -> Memory {
    let memory = Memory::of(log);
    memory.rows.replace(rows.clone());
    memory
}

/// The rows of `memory`, its legacy rows at an older version: the store
/// keeps the version a legacy row was first written at.
fn legacy_at_an_older_version(memory: &Memory) -> BTreeMap<(LedgerKind, String), LedgerRow> {
    let mut rows = memory.rows.borrow().clone();
    for row in rows.values_mut().filter(|row| row.is_legacy()) {
        row.ledger_v = LEDGER_V - 1;
    }
    rows
}

/// The summaries of the pushes of a build, its pushes kept and the runs it
/// left pending on their push ([`pushes_of`]).
type Summaries = (
    PushesHeld,
    Vec<crate::domain::EventId>,
    BTreeMap<String, Option<TaskId>>,
);

/// Where the summaries of the pushes made again on `store` for the
/// cursors of `whole` are not `every`, those of `whole` (`what`: where).
#[track_caller]
fn differs(
    store: &Memory,
    whole: &Builder,
    every: &Summaries,
    now: i64,
    what: &str,
) -> Option<String> {
    let mut made = whole.clone();
    made.restore_pushes(PushesHeld::default(), Vec::new());
    store.forget_reads();
    restore_pushes(store, &mut made, ms(now)).unwrap();
    assert_reads_allowed(store, &made, what);
    let made = pushes_of(&made);
    let pending = made
        .2
        .iter()
        .all(|(run, task)| every.2.get(run) == Some(task));
    ((&made.0, &made.1) != (&every.0, &every.1) || !pending).then(|| {
        format!(
            "{what}: {} (made / every), kept {:?} / {:?}",
            made.0.differences(&every.0).join("; "),
            made.1,
            every.1
        )
    })
}

/// The run and task rows of `memory`.
fn runs_and_tasks(memory: Memory) -> BTreeMap<(LedgerKind, String), LedgerRow> {
    memory
        .rows
        .into_inner()
        .into_iter()
        .filter(|((kind, _), _)| matches!(kind, LedgerKind::Run | LedgerKind::Task))
        .collect()
}

/// What of `whole` the rows written by a builder going on from `from` lack
/// or hold otherwise, on a store holding `rows` and every event of `log`,
/// with its summaries of the pushes made again from those rows and not.
fn going_on_differs(
    log: &Log,
    rows: &BTreeMap<(LedgerKind, String), LedgerRow>,
    from: &Builder,
    now: i64,
) -> Vec<(LedgerKind, String)> {
    let on = |made: bool| {
        let store = holding(log, rows);
        let mut builder = from.clone();
        if made {
            builder.restore_pushes(PushesHeld::default(), Vec::new());
            restore_pushes(&store, &mut builder, ms(now)).unwrap();
        }
        drain(&store, &mut builder, now + 20);
        runs_and_tasks(store)
    };
    let (whole, made) = (on(false), on(true));
    whole
        .iter()
        .filter(|(key, row)| made.get(key) != Some(row))
        .map(|(key, _)| key.clone())
        .chain(made.keys().filter(|key| !whole.contains_key(key)).cloned())
        .collect()
}

/// How many of the harder places the histories reached.
#[derive(Debug, Default)]
struct Reached {
    /// Legacy tasks with a run of today's form.
    legacy_with_a_run_of_today: usize,
    /// A run with a failed push up to the position and no row at P, that no
    /// row names (claimed after its legacy task's row was first written).
    named_by_no_row: usize,
    /// A run after a failed push and before its pending push, with no row.
    failed_with_no_row: usize,
    /// A run whose row ended, with a later event of it up to the position
    /// that the row does not record.
    went_on_from_a_closed_row: usize,
    /// A run closed by a push whose readers all settled.
    closed_and_settled: usize,
    /// A run or task not settled at the position that reads a push, its
    /// row at P legacy and first written settled, at today's version: only
    /// its fold says.
    legacy_mark_says_settled: usize,
}

impl Reached {
    /// Note what the store `rows` written at P holds of the events up to
    /// the position `at`, where `held` are the summaries of every event.
    fn note(
        &mut self,
        rows: &BTreeMap<(LedgerKind, String), LedgerRow>,
        log: &Log,
        at: usize,
        held: &PushesHeld,
    ) {
        self.legacy_mark_says_settled += usize::from(held.readers().iter().any(|reader| {
            let key = match reader {
                Target::Run(run) => (LedgerKind::Run, run.clone()),
                Target::Task(task) => (LedgerKind::Task, task.to_string()),
                _ => return false,
            };
            rows.get(&key)
                .is_some_and(|row| row.is_legacy() && row.is_final && row.ledger_v == LEDGER_V)
        }));
        let named: BTreeSet<String> = rows
            .values()
            .flat_map(crate::domain::ledger::builder::pushes::task_runs)
            .collect();
        let upto = &log.events[..at];
        let has_row = |run: &str| rows.contains_key(&(LedgerKind::Run, run.to_owned()));
        self.named_by_no_row += usize::from(upto.iter().any(|event| {
            event.kind == EventKind::PushFailed
                && event
                    .run_id
                    .as_ref()
                    .is_some_and(|run| !has_row(run.as_str()) && !named.contains(run.as_str()))
        }));
        self.went_on_from_a_closed_row += usize::from(upto.iter().any(|event| {
            event.run_id.as_ref().is_some_and(|run| {
                rows.get(&(LedgerKind::Run, run.to_string()))
                    .is_some_and(|row| {
                        row.ended_at.is_some() && event.id > row.inputs_through.events
                    })
            })
        }));
    }
}

/// Every place where the summaries made again over `log` are not those of
/// the build of every event or a builder going on from them writes other
/// rows, for rows written pass by pass (at most `most` events a pass) and
/// taken at each of `ps`, and those built up to each position and to the
/// end; what it reached goes to `reached`.
fn places(
    log: &Log,
    now: i64,
    ps: &BTreeSet<usize>,
    most: usize,
    reached: &mut Reached,
) -> Vec<String> {
    let mut found = Vec::new();
    let n = log.events.len();
    // The build of every event up to each position, and its rows.
    let wholes: Vec<(Builder, Summaries, _)> = (0..=n)
        .map(|at| {
            let memory = Memory::default();
            memory.events.replace(log.events[..at].to_vec());
            let mut whole = Builder::default();
            drain(&memory, &mut whole, now);
            let every = pushes_of(&whole);
            let rows = memory.rows.into_inner();
            (whole, every, rows)
        })
        .collect();
    let end = &wholes[n].2;
    // The rows written pass by pass as the events came, taken at each P,
    // where a pass stopped.
    let going = Memory::default();
    let mut builder = Builder::default();
    let mut taken = BTreeMap::new();
    let mut p = 0;
    let mut moved = Seeded(u64::try_from(n).unwrap() | 1);
    loop {
        going.events.replace(log.events[..p].to_vec());
        drain(&going, &mut builder, now);
        if ps.contains(&p) {
            let as_written = going.rows.borrow().clone();
            taken.insert(p, (as_written, legacy_at_an_older_version(&going)));
        }
        if p == n {
            break;
        }
        let next = p + 1 + moved.below(most);
        p = ps
            .range(p + 1..)
            .next()
            .map_or(next, |at| next.min(*at))
            .min(n);
    }
    reached.legacy_with_a_run_of_today += end
        .values()
        .filter(|task| {
            task.kind == LedgerKind::Task
                && task.is_legacy()
                && end.values().any(|run| {
                    run.kind == LedgerKind::Run && run.task_id == task.task_id && !run.is_legacy()
                })
        })
        .count();
    for (at, (whole, every, rows)) in wholes.iter().enumerate() {
        let mut stores = vec![
            (format!("P = at {at}"), rows),
            (format!("P = end, at {at}"), end),
        ];
        for (p, (as_written, older)) in taken.range(at..) {
            reached.note(as_written, log, at, &every.0);
            stores.push((
                format!("written as they came, P = {p}, at {at}"),
                as_written,
            ));
            stores.push((
                format!("written as they came, legacy older, P = {p}, at {at}"),
                older,
            ));
        }
        if at > 0
            && log.events[at - 1].kind == EventKind::PushFailed
            && let Some(run) = &log.events[at - 1].run_id
        {
            reached.failed_with_no_row +=
                usize::from(!rows.contains_key(&(LedgerKind::Run, run.to_string())));
        }
        reached.closed_and_settled += usize::from(every.0.closed_any() && every.1.is_empty());
        for (what, rows) in stores {
            found.extend(differs(&holding(log, rows), whole, every, now, &what));
        }
    }
    for at in [n / 4, n / 2, 3 * n / 4] {
        let (whole, _, rows) = &wholes[at];
        let differ = going_on_differs(log, rows, whole, now);
        if !differ.is_empty() {
            found.push(format!("going on from P = at {at}: {differ:?}"));
        }
    }
    for (p, (as_written, older)) in &taken {
        for at in [p / 2, *p] {
            for rows in [as_written, older] {
                let differ = going_on_differs(log, rows, &wholes[at].0, now);
                if !differ.is_empty() {
                    found.push(format!("going on from P = {p}, at {at}: {differ:?}"));
                }
            }
        }
    }
    found
}

/// Where passes stop in `log`: at its end, at some positions picked, and at
/// every claim before its first phase, where a fold of the run and of its
/// task is legacy and the store keeps it so ([`crate::domain::ledger::run_row`]).
fn positions(log: &Log, picked: &mut Seeded) -> BTreeSet<usize> {
    let n = log.events.len();
    let claims = log
        .events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.kind == EventKind::RunClaimed)
        .map(|(at, _)| at + 1);
    std::iter::once(n)
        .chain((0..6).map(|_| 1 + picked.below(n)))
        .chain(claims)
        .collect()
}

/// That the summaries made again over `log` are those of the build of every
/// event, and a builder going on from them writes its rows, for rows
/// written a pass an event at every P; what it reached.
#[track_caller]
fn assert_as_every_event(log: &Log, now: i64) -> Reached {
    let ps: BTreeSet<usize> = (1..=log.events.len()).collect();
    let mut reached = Reached::default();
    let found = places(log, now, &ps, 1, &mut reached);
    assert!(found.is_empty(), "{}", found.join("\n"));
    reached
}

/// Over histories mixing legacy tasks and runs of today's form, at every
/// position and for rows written at every kind of P at or after it, the
/// summaries of the pushes made again are those of the build of every
/// event up to the position, reading only what the rows allow; a builder
/// going on from them writes the rows of every event. The seeds are some
/// that reach the harder places.
#[test]
fn the_pushes_made_again_over_mixed_histories_are_those_of_every_event() {
    let mut reached = Reached::default();
    let mut found = Vec::new();
    for seed in [590, 544, 35] {
        let (log, now) = mixed(seed);
        let mut picked = Seeded(seed.wrapping_mul(0xD6E8_FEB8_6659_FD93) | 1);
        let ps = positions(&log, &mut picked);
        let most = 1 + picked.below(4);
        found.extend(
            places(&log, now, &ps, most, &mut reached)
                .into_iter()
                .map(|place| format!("seed {seed}: {place}")),
        );
    }
    assert!(
        found.is_empty(),
        "{} mismatches:\n{}",
        found.len(),
        found
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(reached.legacy_with_a_run_of_today > 0, "{reached:?}");
    assert!(reached.named_by_no_row > 0, "{reached:?}");
    assert!(reached.failed_with_no_row > 0, "{reached:?}");
    assert!(reached.went_on_from_a_closed_row > 0, "{reached:?}");
    assert!(reached.closed_and_settled > 0, "{reached:?}");
    assert!(reached.legacy_mark_says_settled > 0, "{reached:?}");
}

/// Long enough after `secs` for what ended there to have settled.
const fn past_the_grace(secs: i64) -> i64 {
    secs + SETTLE_GRACE_MS / 1000 + 100
}

/// A run from before the switch, its row and its task's first written
/// settled at today's version, goes on recording its phases, fails a push
/// and waits on it, and another run's push closes it: the store keeps both
/// marks as first written though their folds, no longer legacy, ended only
/// then. Within the grace the run and its task read that push on and past
/// it neither does, as the build of every event has it, a builder going on
/// writes its rows, and so at every position.
#[test]
fn a_legacy_row_marked_settled_as_first_written_settles_by_its_fold_as_every_event() {
    let (old, other) = (run_id(1), run_id(2));
    let mut log = Log::default();
    log.legacy(1, &old, 0);
    let first = log.events.len();
    log.phase(1, &old, 2000, Phase::Landing, "x")
        .push_failed(1, &old, 2010)
        .completed(1, 2020)
        .landed(2, &other, 2030);
    let pushed = 2030 + 930;
    for (now, reads) in [(pushed + 10, true), (past_the_grace(pushed), false)] {
        let store = Memory::default();
        let mut builder = Builder::default();
        store.events.replace(log.events[..first].to_vec());
        drain(&store, &mut builder, now);
        store.events.replace(log.events.clone());
        drain(&store, &mut builder, now);
        for row in [
            store.row(LedgerKind::Run, &old),
            store.row(LedgerKind::Task, "1"),
        ] {
            assert!(row.is_legacy() && row.is_final && row.ledger_v == LEDGER_V);
        }
        let memory = Memory::of(&log);
        let mut whole = Builder::default();
        drain(&memory, &mut whole, now);
        pass(&memory, &mut whole, now);
        let every = pushes_of(&whole);
        assert!(every.0.closed(&old));
        let readers = every.0.readers();
        assert_eq!(readers.contains(&Target::Run(old.clone())), reads);
        assert_eq!(readers.contains(&Target::Task(TaskId::new(1))), reads);
        assert_eq!(every.1.is_empty(), !reads);
        assert_made_again_as(&store, &whole, &every, now, "the marks as first written");
        let rows = store.rows.borrow().clone();
        assert_eq!(going_on_differs(&log, &rows, &whole, now), []);
        assert_as_every_event(&log, now);
    }
}

/// A run from before the switch goes on recording its phases, so that its
/// task's fold is of today's form up to a position, though another of that
/// time, claimed there and with no phase, finishes after it: at P the task's
/// row is legacy, kept naming the runs of its first write, and names neither
/// that run nor the next, pending, whose push closed it before the position.
/// The task's end there is the pending one's, within the grace though the
/// first's is past it: its row at P is folded on every run it claimed, as
/// every event has it.
#[test]
fn a_legacy_task_row_folds_every_run_it_claimed_as_every_event() {
    let (first, late, second, other) = (run_id(1), run_id(2), run_id(3), run_id(4));
    let mut log = Log::default();
    log.legacy(1, &first, 0);
    let written = log.events.len();
    log.add(
        Some(1),
        Some(&late),
        "run_claimed",
        json!({"provider": "claude"}),
        1300,
    )
    .phase(1, &first, 1310, Phase::Ended, "pushed");
    let pushed = past_the_grace(1310);
    log.worked(1, &second, pushed)
        .integrated(1, &second, pushed + 700)
        .push_failed(1, &second, pushed + 910)
        .completed(1, pushed + 920)
        .landed(2, &other, pushed + 1000);
    let position = log.events.len();
    log.add(
        Some(1),
        Some(&late),
        "run_integrated",
        json!({"from": "validating", "to": "integrated", "status": "integrated"}),
        pushed + 1940,
    );
    let now = pushed + 1950;
    let store = Memory::default();
    let mut builder = Builder::default();
    for p in [written, log.events.len()] {
        store.events.replace(log.events[..p].to_vec());
        drain(&store, &mut builder, now);
    }
    let task = store.row(LedgerKind::Task, "1");
    assert!(task.is_legacy() && task.is_final);
    assert_eq!(
        crate::domain::ledger::builder::pushes::task_runs(&task),
        [first.as_str()]
    );
    let memory = Memory::default();
    memory.events.replace(log.events[..position].to_vec());
    let mut whole = Builder::default();
    drain(&memory, &mut whole, now);
    pass(&memory, &mut whole, now);
    let every = pushes_of(&whole);
    assert!(every.0.readers().contains(&Target::Task(TaskId::new(1))));
    assert_made_again_as(&store, &whole, &every, now, "a run its row does not name");
    assert_as_every_event(&log, now);
}

/// A run of today's form of a legacy task with no row, after its failed
/// push and before its pending push, has a failed push at the position
/// whether its task's row names it (the row written then or later) or not
/// (the row first written before its claim, as the store keeps it): the
/// summaries hold where it went as every event does, and its next pending
/// push waits on that branch, so that a push to another closes it not.
#[test]
fn a_run_of_a_legacy_task_with_no_row_keeps_its_failed_push_as_every_event() {
    let (old, a, b) = (run_id(1), run_id(2), run_id(3));
    let mut log = Log::default();
    log.legacy(1, &old, 0)
        .worked(1, &a, 2000)
        .integrated(1, &a, 2700)
        .add(
            Some(1),
            Some(&a),
            "push_failed",
            json!({"remote": "origin", "branch": "late"}),
            2910,
        );
    let failed = log.events.len();
    log.phase(1, &a, 2910, Phase::PushPending, "push_failed")
        .landed(2, &b, 3000);
    let now = past_the_grace(3930);
    let at_failed = {
        let store = Memory::default();
        store.events.replace(log.events[..failed].to_vec());
        drain(&store, &mut Builder::default(), now);
        store
    };
    let task = at_failed.row(LedgerKind::Task, "1");
    assert!(task.is_legacy());
    assert!(crate::domain::ledger::builder::pushes::task_runs(&task).contains(&a));
    assert!(at_failed.ledger_row(LedgerKind::Run, &a).unwrap().is_none());
    let reached = assert_as_every_event(&log, now);
    assert!(reached.legacy_with_a_run_of_today > 0);
    assert!(reached.failed_with_no_row > 0);
    // The row first written before the claim names only the legacy run.
    let (first, _) = rows_named_before_the_claim(&log, &old, &a, failed, now);
    assert!(!crate::domain::ledger::builder::pushes::task_runs(&first).contains(&a));
    assert!(reached.named_by_no_row > 0);
}

/// The task row a store written pass by pass holds at the position `at`, its
/// first write before the claim of `run`, and the run named by no row.
fn rows_named_before_the_claim(
    log: &Log,
    old: &str,
    run: &str,
    at: usize,
    now: i64,
) -> (LedgerRow, Vec<RunId>) {
    let store = Memory::default();
    let mut builder = Builder::default();
    for p in 0..=at {
        store.events.replace(log.events[..p].to_vec());
        drain(&store, &mut builder, now);
    }
    assert!(store.ledger_row(LedgerKind::Run, old).unwrap().is_some());
    assert!(store.ledger_row(LedgerKind::Run, run).unwrap().is_none());
    (
        store.row(LedgerKind::Task, "1"),
        store.ledger_legacy_runs_without_row().unwrap(),
    )
}

/// A task with a legacy run settled as the build wrote it, so that at a
/// position past the grace of its run of today's form, closed by another
/// run's push, no fold reads that push: a note of the task after the position
/// makes its row at P reach past it, and the fold up to it says the same,
/// though the row's version is the one it was first written at.
#[test]
fn a_legacy_task_a_later_note_reaches_was_settled_as_every_event() {
    let (old, a, b) = (run_id(1), run_id(2), run_id(3));
    let mut log = Log::default();
    log.legacy(1, &old, 0)
        .worked(1, &a, 2000)
        .integrated(1, &a, 2700)
        .push_failed(1, &a, 2910)
        .landed(2, &b, 3000);
    let position = log.events.len();
    let settled_by = past_the_grace(3930);
    log.add(Some(1), None, "note_added", json!({}), settled_by);
    let now = settled_by + 10;
    let at_position = {
        let store = Memory::default();
        store.events.replace(log.events[..position].to_vec());
        let mut whole = Builder::default();
        drain(&store, &mut whole, now);
        pass(&store, &mut whole, now);
        whole
    };
    let every = pushes_of(&at_position);
    assert!(every.0.closed(&a) && every.1.is_empty());
    let reached = assert_as_every_event(&log, now);
    assert!(reached.closed_and_settled > 0);
    let end = Memory::of(&log);
    drain(&end, &mut Builder::default(), now);
    let task = end.row(LedgerKind::Task, "1");
    assert!(task.is_legacy() && task.inputs_through.events > log.events[position - 1].id);
    assert_made_again_as(&end, &at_position, &every, now, "the row at the end");
}

/// The push of a run of a legacy task, claimed after the task's row was
/// first written, with no row at P (between its push and its end) closes
/// another run's pending push: no row names that run, and its claim does,
/// so that the push is read; going on, the other run's task folds it.
#[test]
fn a_push_of_a_run_no_row_names_closes_a_pending_push_as_every_event() {
    let (old, a, b) = (run_id(1), run_id(2), run_id(3));
    let mut log = Log::default();
    log.legacy(1, &old, 0)
        .worked(2, &a, 2000)
        .integrated(2, &a, 2700)
        .push_failed(2, &a, 2910)
        .completed(2, 2920)
        .worked(1, &b, 3000)
        .integrated(1, &b, 3700);
    log.add(
        Some(1),
        Some(&b),
        "push_finished",
        json!({"remote": "origin", "branch": "main", "commit": "c"}),
        3930,
    );
    let pushed = log.events.len();
    log.phase(1, &b, 3930, Phase::Ended, "pushed").add(
        Some(2),
        None,
        "note_added",
        json!({}),
        3940,
    );
    let now = 3950;
    let (task, unnamed) = rows_named_before_the_claim(&log, &old, &b, pushed, now);
    assert!(task.is_legacy());
    assert_eq!(
        crate::domain::ledger::builder::pushes::task_runs(&task),
        [old.as_str()]
    );
    assert_eq!(unnamed, [RunId::new(&b).unwrap()]);
    assert_as_every_event(&log, now);
}

/// A run whose pending push another run's push closed, its row settled,
/// goes back to its landing (the builder writes no row of a run on its way,
/// so the row stays that) and fails another push: at a position after that
/// and before its next pending push the row is settled and records neither,
/// and the task's row does not reach past the position either. Its events
/// are read: where its last failed push went is that of every event, so
/// that its next pending push waits on that branch.
#[test]
fn a_run_past_its_settled_closed_row_keeps_its_later_failed_push_as_every_event() {
    let (a, b) = (run_id(1), run_id(2));
    let back = past_the_grace(1930);
    let mut log = Log::default();
    log.worked(1, &a, 0)
        .integrated(1, &a, 700)
        .push_failed(1, &a, 910)
        .landed(2, &b, 1000)
        .phase(1, &a, back, Phase::Landing, "x");
    log.add(
        Some(1),
        Some(&a),
        "push_failed",
        json!({"remote": "origin", "branch": "other"}),
        back + 10,
    );
    let failed = log.events.len();
    log.phase(1, &a, back + 20, Phase::PushPending, "push_failed")
        .pushed_to(3, &run_id(3), "main", back + 30);
    let now = past_the_grace(back + 30);
    let store = Memory::default();
    let mut builder = Builder::default();
    for p in 0..=failed {
        store.events.replace(log.events[..p].to_vec());
        drain(&store, &mut builder, past_the_grace(1930));
    }
    let row = store.row(LedgerKind::Run, &a);
    assert!(row.is_final && row.ended_at.is_some());
    let through = log.events[failed - 1].id;
    assert_eq!(
        store.ledger_runs_past_their_rows(through).unwrap(),
        [RunId::new(&a).unwrap()]
    );
    let memory = Memory::default();
    memory.events.replace(log.events[..failed].to_vec());
    let mut whole = Builder::default();
    drain(&memory, &mut whole, now);
    let every = pushes_of(&whole);
    store.forget_reads();
    assert_made_again_as(&store, &whole, &every, now, "the settled closed row");
    assert_eq!(*store.runs_read.borrow(), [RunId::new(&a).unwrap()]);
    assert_as_every_event(&log, now);
}

/// A run whose pending push another run's push closed goes back to its
/// landing within the grace (its row never settles), and a task, completed,
/// gets a note after that: at a position before the note the task's row
/// reaches past it and is folded on the run's events up to it, not on its
/// closed row, so that the task has not ended; the run is on its way.
#[test]
fn a_task_folded_across_the_position_reads_a_run_past_its_closed_row_as_every_event() {
    let (a, b) = (run_id(1), run_id(2));
    let mut log = Log::default();
    log.worked(1, &a, 0)
        .integrated(1, &a, 700)
        .push_failed(1, &a, 910)
        .completed(1, 920)
        .landed(2, &b, 1000)
        .phase(1, &a, 1950, Phase::Landing, "x");
    let back = log.events.len();
    log.add(Some(1), None, "note_added", json!({}), 1960);
    for now in [2000, past_the_grace(1960)] {
        assert_as_every_event(&log, now);
    }
    let now = past_the_grace(1960);
    let memory = Memory::default();
    memory.events.replace(log.events[..back].to_vec());
    let mut whole = Builder::default();
    drain(&memory, &mut whole, now);
    let held = pushes_of(&whole).0;
    assert!(held.readers().contains(&Target::Task(TaskId::new(1))));
    assert!(held.readers().contains(&Target::Run(a.clone())));
}

/// A pass folds no earlier than the latest event it read
/// ([`Builder::folds_at`]), and so do the summaries made again: with a clock
/// behind that event, a task a later note reaches was settled at the position
/// as every event has it, the push that closed its run's pending push not
/// kept.
#[test]
fn the_pushes_are_made_again_at_the_time_a_pass_folds_at() {
    let (a, b) = (run_id(1), run_id(2));
    let mut log = Log::default();
    log.worked(1, &a, 0)
        .integrated(1, &a, 700)
        .push_failed(1, &a, 910)
        .completed(1, 910)
        .landed(2, &b, 1000);
    let later = past_the_grace(1930);
    log.add(Some(3), None, "task_created", json!({}), later);
    let position = log.events.len();
    log.add(Some(1), None, "note_added", json!({}), later + 10);
    // Past the grace of the push at 1930 only by the event at `later`.
    let behind = 1930 + SETTLE_GRACE_MS / 1000 - 10;
    let memory = Memory::default();
    memory.events.replace(log.events[..position].to_vec());
    let mut whole = Builder::default();
    drain(&memory, &mut whole, behind);
    pass(&memory, &mut whole, behind);
    let every = pushes_of(&whole);
    assert!(every.0.closed(&a) && every.1.is_empty());
    let end = Memory::of(&log);
    drain(&end, &mut Builder::default(), behind);
    let task = end.row(LedgerKind::Task, "1");
    assert!(task.inputs_through.events > log.events[position - 1].id);
    assert_made_again_as(&end, &whole, &every, behind, "a clock behind");
}
