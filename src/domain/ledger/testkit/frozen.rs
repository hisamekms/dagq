//! The inputs whose readers' values are fixed as the functions of main at
//! commit `369d7c62657660606feee3911710a7b0b8e03cf1` give them
//! ([`FIXED_AT`]), for the tasks that change how the ledger is built and
//! how a window is read to compare their values with those, not only the
//! new functions with each other. Each [`Input`] is a deterministic history
//! of the EventStore (a [`Log`]) and the passes of the builder over it
//! ([`Step`]); what each event is a record of is [`owner::owner`]'s
//! ([`Input::owners`]). The values fixed (`stats`, `kpi`, `report`, the
//! check of the targets and the queue rows, with the window's positions)
//! and the test that holds them are
//! `application::ports::observation::frozen` (its module doc says how to
//! read them); the passes run on the store in memory of the builder's
//! tests (`application::supervise::observation::ledger::tests::frozen`).
//!
//! The inputs ([`inputs`], by name):
//!
//! - `reached` (i): R at the last event read; on the way a session write
//!   that fails (W stays back), a new process started from R, and a
//!   builder from its cursors' start (reading again from before R).
//! - `cut` (ii): one pass cut by [`WRITES_PER_PASS`], queue rows of the
//!   three kinds ([`THREE`]) and of other kinds held past R.
//! - `on its way` (iii): a run not ended whose events are before and after R.
//! - `store ahead` (iv): the EventStore holds events of the three kinds and
//!   others past the builder's read; `--until`s past it and before it.
//! - `clock back` (v): events of the three kinds timed before the ones
//!   before them, with time cursors across them.
//! - `no r` (vi): no R (the marks gone, as a raised version leaves them),
//!   the last input a queue row of one event of the three kinds.
//! - `queue and run` (vii): each of the three kinds of the queue's and of
//!   runs' or tasks' in turn by id, a run's token event of no row (its task
//!   canceled) between queue rows held, a pass cut by the bound and the
//!   pass after it; a span of few of the queue's and many of the
//!   runs'.
//! - `timeless` (viii): events of the three kinds whose `created_at` does
//!   not parse: of each kind one after a timed one of it (the last six
//!   events), and a long span of them (the first forty, the kinds in turn).
//!
//! A later task reads each input with its own builder and readers and
//! compares the values with those fixed; the input is not to be changed,
//! or the values fixed are not those of main's functions.

use serde_json::json;

use super::{Log, run_id};
use crate::domain::EventId;
use crate::domain::ledger::builder::WRITES_PER_PASS;
use crate::domain::ledger::owner::{self, Owner};

/// The commit of main whose functions gave the values fixed.
pub(crate) const FIXED_AT: &str = "369d7c62657660606feee3911710a7b0b8e03cf1";

/// The kinds of one event each no reader of the ledger names
/// (docs/plans/ledger-queue-rows-readers-spike.md).
pub(crate) const THREE: [&str; 3] = [
    "authorization_denied",
    "telemetry_token_issued",
    "telemetry_token_revoked",
];

/// A time well past every event of the inputs and their graces (seconds
/// after [`super::T0`]).
pub(crate) const LATER: i64 = 1_000_000;

/// One step of the builder over an input, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// The EventStore holds the first `n` events of the input.
    Arrive(usize),
    /// One pass at `secs`.
    Pass(i64),
    /// Passes at `secs` until one leaves no mark (at most ten).
    Drain(i64),
    /// The store's marks are gone: a raised version's (marks of another
    /// version give no R) or a store that never had them.
    MarksGone,
    /// The next write of the session rows fails part way (W stays where it
    /// was), and a pass at `secs`.
    SessionWriteFails(i64),
    /// A new process: the builder the store's marks start at `secs`.
    Start(i64),
    /// A new builder from its cursors' start on the rows held: it reads
    /// every event again, from before R.
    FromTheStart,
}

/// An input: its events, the builder's steps, and where the readers read.
#[derive(Debug, Clone)]
pub(crate) struct Input {
    pub(crate) name: &'static str,
    pub(crate) log: Log,
    pub(crate) steps: Vec<Step>,
    /// The time the readers read at (seconds after [`super::T0`]).
    pub(crate) now: i64,
    /// Time cursors (seconds after [`super::T0`]): each a `stats --since`, a
    /// `kpi --since` and the time of a read from a time.
    pub(crate) times: Vec<i64>,
    /// Event ids, each a `stats --until`.
    pub(crate) untils: Vec<EventId>,
}

impl Input {
    /// What each event is a record of.
    pub(crate) fn owners(&self) -> Vec<(EventId, Owner)> {
        self.log
            .events
            .iter()
            .map(|event| (event.id, owner::owner(event)))
            .collect()
    }

    /// The id of the `n`th event (from 1).
    pub(crate) fn id(&self, n: usize) -> EventId {
        self.log.events[n - 1].id
    }
}

/// A refusal of a telemetry report, as executor-telemetry gets them for a
/// session not on its run (`commands::denial`), of `task` if any.
fn denied(log: &mut Log, task: Option<i64>, secs: i64) {
    log.add(
        task,
        None,
        "authorization_denied",
        json!({"event": "authorization_denied", "role": "executor",
               "capability": "telemetry.report", "reason": "not_on_resource",
               "resource": {"kind": "run", "id": run_id(9)}}),
        secs,
    );
}

/// A telemetry token of `target` issued (or revoked) at `secs`; a run's
/// when `run` is (as `telemetry_tokens::issue_run` and `revoke_run` record
/// them), else the queue's (a job's).
fn token(log: &mut Log, issued: bool, run: Option<(i64, &str)>, target: &str, secs: i64) {
    let (kind, payload) = if issued {
        ("telemetry_token_issued", json!({"target": target}))
    } else {
        (
            "telemetry_token_revoked",
            json!({"target": target, "reason": "ended"}),
        )
    };
    log.add(
        run.map(|(task, _)| task),
        run.map(|(_, run)| run),
        kind,
        payload,
        secs,
    );
}

/// A run id of `n` past the 256 of [`run_id`].
fn many(n: usize) -> String {
    format!("{n:08x}-5555-4555-8555-555555555555")
}

/// `count` landed runs of tasks from `task`, from `secs` ten seconds apart.
fn landed_many(log: &mut Log, task: i64, count: usize, secs: i64) {
    for n in 0..count {
        let k = i64::try_from(n).unwrap();
        log.landed(
            task + k,
            &many(usize::try_from(task).unwrap() + n),
            secs + 10 * k,
        );
    }
}

/// The queue's events of every kind the inputs share: a supervisor alive,
/// a mark, a slots full started and ended, a finding, a job's tokens and
/// refusals, and a landed run of task 1.
fn common(log: &mut Log, secs: i64) {
    let full = json!({"supervisor": "s1", "class": "heavy"});
    log.add(
        None,
        None,
        "supervisor_alive",
        json!({"supervisor": "s1"}),
        secs,
    )
    .add(None, None, "mark_added", json!({"mark": "m"}), secs + 1)
    .add(None, None, "slots_full_started", full.clone(), secs + 2)
    .add(
        None,
        None,
        "finding_recorded",
        super::finding_record("open"),
        secs + 3,
    );
    token(log, true, None, "job:1", secs + 4);
    denied(log, None, secs + 5);
    log.landed(1, &run_id(1), secs + 10);
    denied(log, None, secs + 950);
    token(log, false, None, "job:1", secs + 960);
    log.add(None, None, "slots_full_ended", full, secs + 970);
}

/// A planner's session opened and closed at `secs`: a session row.
fn session(log: &mut Log, planner: i64, secs: i64) {
    let opened = i64::try_from(log.events.len()).unwrap() + 1;
    log.add(
        None,
        None,
        "session_opened",
        json!({"kind": "runtime_planner", "planner_id": planner}),
        secs,
    )
    .add(
        None,
        None,
        "session_closed",
        json!({"kind": "runtime_planner", "planner_id": planner, "opened_event_id": opened,
               "session_id": format!("p-{planner}"), "model": "m"}),
        secs + 10,
    );
}

/// (i) R at the last event read.
fn reached() -> Input {
    let mut log = Log::default();
    common(&mut log, 0);
    session(&mut log, 1, 1_000);
    let first = log.events.len();
    log.landed(2, &run_id(2), 2_000);
    session(&mut log, 2, 2_100);
    denied(&mut log, None, 3_000);
    log.add(None, None, "mark_added", json!({"mark": "n"}), 3_001);
    let all = log.events.len();
    let until = log.events[first + 3].id;
    Input {
        name: "reached",
        steps: vec![
            Step::Arrive(first),
            Step::Drain(LATER),
            Step::Arrive(all),
            Step::SessionWriteFails(LATER),
            Step::Start(LATER),
            Step::Pass(LATER),
            Step::FromTheStart,
            Step::Drain(LATER + 1),
        ],
        now: LATER + 2,
        times: vec![5, 960, 3_000],
        untils: vec![until, log.last()],
        log,
    }
}

/// (ii) One pass cut by the bound: queue rows of the three kinds and of
/// other kinds past R.
fn cut() -> Input {
    let mut log = Log::default();
    common(&mut log, 0);
    let read = log.events.len();
    landed_many(&mut log, 100, WRITES_PER_PASS / 3 + 5, 2_000);
    for n in 0..3 {
        denied(&mut log, None, 9_000 + n);
        log.add(None, None, "mark_added", json!({"mark": "late"}), 9_000 + n);
    }
    token(&mut log, true, None, "job:2", 9_010);
    token(&mut log, false, None, "job:2", 9_020);
    let all = log.events.len();
    Input {
        name: "cut",
        steps: vec![
            Step::Arrive(read),
            Step::Drain(LATER),
            Step::Arrive(all),
            Step::Pass(LATER),
        ],
        now: LATER + 1,
        times: vec![2_000, 9_001],
        untils: vec![log.events[all - 3].id],
        log,
    }
}

/// (iii) A run not ended whose events are before and after R.
fn on_its_way() -> Input {
    let mut log = Log::default();
    common(&mut log, 0);
    let a = run_id(3);
    log.worked(3, &a, 1_000);
    let read = log.events.len();
    landed_many(&mut log, 300, WRITES_PER_PASS / 3 + 5, 2_000);
    log.phase(3, &a, 9_000, crate::domain::run_phase::Phase::Landing, "x")
        .add(Some(3), Some(&a), "integration_started", json!({}), 9_000);
    denied(&mut log, None, 9_010);
    let all = log.events.len();
    Input {
        name: "on its way",
        steps: vec![
            Step::Arrive(read),
            Step::Drain(LATER),
            Step::Arrive(all),
            Step::Pass(LATER),
        ],
        now: LATER + 1,
        times: vec![1_500, 9_005],
        untils: vec![log.events[all - 2].id],
        log,
    }
}

/// (iv) The EventStore ahead of the builder's read.
fn store_ahead() -> Input {
    let mut log = Log::default();
    common(&mut log, 0);
    denied(&mut log, None, 1_000);
    let before = log.last();
    log.add(None, None, "mark_added", json!({"mark": "read"}), 1_001);
    let read = log.events.len();
    denied(&mut log, None, 2_000);
    token(&mut log, true, None, "job:3", 2_001);
    log.add(None, None, "mark_added", json!({"mark": "ahead"}), 2_002);
    log.landed(4, &run_id(4), 2_010);
    token(&mut log, false, None, "job:3", 3_000);
    let all = log.events.len();
    Input {
        name: "store ahead",
        steps: vec![Step::Arrive(read), Step::Drain(LATER), Step::Arrive(all)],
        now: LATER + 1,
        times: vec![1_000, 2_001],
        untils: vec![before, log.events[read + 1].id, log.last()],
        log,
    }
}

/// (v) Events of the three kinds timed before those before them.
fn clock_back() -> Input {
    let mut log = Log::default();
    common(&mut log, 0);
    log.add(None, None, "mark_added", json!({"mark": "a"}), 2_000);
    denied(&mut log, None, 1_500);
    token(&mut log, true, None, "job:4", 1_400);
    log.add(None, None, "mark_added", json!({"mark": "b"}), 2_100);
    token(&mut log, false, None, "job:4", 1_450);
    let all = log.events.len();
    Input {
        name: "clock back",
        steps: vec![Step::Arrive(all), Step::Drain(LATER)],
        now: LATER + 1,
        times: vec![1_420, 1_480, 1_999, 2_050],
        untils: vec![log.events[all - 4].id],
        log,
    }
}

/// (vi) No R; the last input a queue row of one refusal.
fn no_r() -> Input {
    let mut log = Log::default();
    common(&mut log, 0);
    log.landed(5, &run_id(5), 1_000);
    denied(&mut log, None, 3_000);
    let all = log.events.len();
    Input {
        name: "no r",
        steps: vec![Step::Arrive(all), Step::Drain(LATER), Step::MarksGone],
        now: LATER + 1,
        times: vec![2_999, 3_000],
        untils: vec![log.events[all - 2].id],
        log,
    }
}

/// (vii) The three kinds of the queue's and of runs' and tasks' in turn.
fn queue_and_run() -> Input {
    let mut log = Log::default();
    common(&mut log, 0);
    // A run whose life spans the history: marked first, ended late.
    let long = run_id(6);
    log.worked(6, &long, 1_000);
    token(&mut log, true, Some((6, &long)), "run:6", 1_001);
    denied(&mut log, None, 1_002);
    // Of runs and tasks in turn with the queue's, each of the three kinds:
    // a run's token issued and a job's, a refusal of the queue's and of a
    // task, the run's token revoked and the job's.
    for n in 0..4_u8 {
        let run = run_id(20 + n);
        let task = 20 + i64::from(n);
        let job = format!("job:2{n}");
        let at = 1_100 + 100 * i64::from(n);
        token(&mut log, true, Some((task, &run)), "run", at);
        token(&mut log, true, None, &job, at);
        denied(&mut log, None, at + 1);
        denied(&mut log, Some(task), at + 2);
        log.landed(task, &run, at + 3);
        token(&mut log, false, Some((task, &run)), "run", at + 940);
        token(&mut log, false, None, &job, at + 940);
        denied(&mut log, None, at + 941);
    }
    // Few of the queue's, many of runs'.
    for n in 0..30_u8 {
        let run = run_id(40 + n);
        let task = 40 + i64::from(n);
        let at = 3_000 + 10 * i64::from(n);
        token(&mut log, true, Some((task, &run)), "run", at);
        denied(&mut log, Some(task), at + 1);
        token(&mut log, false, Some((task, &run)), "run", at + 2);
        if n % 15 == 7 {
            denied(&mut log, None, at + 3);
        }
    }
    let read = log.events.len();
    landed_many(&mut log, 700, WRITES_PER_PASS / 2 + 30, 4_000);
    denied(&mut log, None, 9_000);
    log.integrated(6, &long, 9_001).pushed(6, &long, 9_210);
    token(&mut log, false, Some((6, &long)), "run:6", 9_211);
    // A run of no row, whose task was canceled (so no reader folds it as
    // on its way), its token between the queue's.
    let later = run_id(7);
    log.add(Some(9_999), Some(&later), "run_claimed", json!({}), 9_220);
    token(&mut log, true, Some((9_999, &later)), "run:7", 9_221);
    denied(&mut log, None, 9_222);
    log.add(
        Some(9_999),
        None,
        "task_status_changed",
        json!({"from": "in_progress", "to": "canceled"}),
        9_223,
    );
    denied(&mut log, None, 9_224);
    let all = log.events.len();
    Input {
        name: "queue and run",
        steps: vec![
            Step::Arrive(read),
            Step::Drain(LATER),
            Step::Arrive(all),
            Step::Pass(LATER),
            Step::Pass(LATER),
        ],
        now: LATER + 1,
        times: vec![1_201, 3_150, 9_211, 9_221],
        untils: vec![log.events[all - 4].id, log.events[all - 5].id, log.last()],
        log,
    }
}

/// (viii) Events of the three kinds whose time does not parse: forty of
/// them first, each kind in turn, and at the end each kind timed and then
/// of no time.
fn timeless() -> Input {
    let no_time = |log: &mut Log| {
        log.events.last_mut().unwrap().created_at = "not a time".into();
    };
    let each = |log: &mut Log, kind: usize, secs: i64| match kind {
        0 => token(log, true, None, "job:8", secs),
        1 => token(log, false, None, "job:8", secs),
        _ => denied(log, None, secs),
    };
    let mut log = Log::default();
    for n in 0..40 {
        each(&mut log, n % 3, i64::try_from(n).unwrap());
        no_time(&mut log);
    }
    common(&mut log, 100);
    log.landed(8, &run_id(8), 2_000);
    for (kind, secs) in [(0, 4_000), (1, 4_500), (2, 5_000)] {
        each(&mut log, kind, secs);
        each(&mut log, kind, secs + 1);
        no_time(&mut log);
    }
    let timed = log.events[log.events.len() - 2].id;
    let all = log.events.len();
    Input {
        name: "timeless",
        steps: vec![Step::Arrive(all), Step::Drain(LATER)],
        now: LATER + 1,
        times: vec![50, 4_000, 4_500, 5_000, 6_000],
        untils: vec![EventId::new(20), timed, log.last()],
        log,
    }
}

/// Every input, (i) to (viii).
pub(crate) fn inputs() -> Vec<Input> {
    vec![
        reached(),
        cut(),
        on_its_way(),
        store_ahead(),
        clock_back(),
        no_r(),
        queue_and_run(),
        timeless(),
    ]
}
