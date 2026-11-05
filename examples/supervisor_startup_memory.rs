//! What a supervisor keeps in its allocator after its start's catch-up,
//! measured in the process (docs/plans/supervisor-startup-memory-spike.md).
//!
//! `prepare <dir> <events> [<pad>]` lays down a queue at `<dir>/queue.db` of about
//! `<events>` events settled (finished runs and tasks, closed sessions,
//! supervisors' lives and their queue events) and a few open ones, then
//! builds its ledger as a supervisor before the restart did, so that the
//! store holds the rows settled and the marks of where they are written;
//! `<pad>` adds a text of that many bytes to every payload. `measure <dir>`
//! is the restarted supervisor on the same ports the supervisor uses
//! (`SqliteQueue`), along both ways a start goes: from the marks
//! (`start_builder`, what a store with the position R and no older row
//! starts on, [`go_on`]) and from its cursors' start (what any other store
//! starts on). Each is the first pass of the judgments of now
//! (`carried_live_inputs` on a new `LiveHistory`) and then the ledger's
//! catch-up (`build_pass` until no mark waits). It says after each stage the
//! bytes and allocations in use and the peak, of the way from the cursors'
//! start what each part held, and of the way from the marks how many of
//! each it read ([`Reads`]), counted out of the stages.
//!
//! Run with `cargo run --release --example supervisor_startup_memory --
//! prepare "$TMPDIR/q20k" 20000` and then `... -- measure "$TMPDIR/q20k"`.
//!
//! `check` lays down two queues under the temporary directory that differ
//! only in how many settled events they hold ([`CHECK_SMALL`], and four
//! times as many), restarts a supervisor on each along both ways as
//! `measure` does, and asserts that neither what it keeps after the
//! catch-up nor its peak grows with the settled events: from the cursors'
//! start by [`within`], from the marks by [`within_cap`], where it also
//! asserts that the start went on from R: docs/design/measurement.md
//! "台帳を作る係".
//!
//! An allocator that counts for the whole process is set here only: the
//! library's tests count per thread, and neither they nor the integration
//! tests carry this one (it would count every other
//! test's allocations, and the integration tests' boundaries are kept in
//! their register).

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use dagq::application::LedgerStore;
use dagq::application::live_inputs::{LiveSources, carried_live_inputs};
use dagq::application::supervise::{Retain, build_pass, start_builder};
use dagq::domain::EventFilter;
use dagq::domain::ledger::LedgerKind;
use dagq::domain::ledger::builder::{Builder, EVENTS_PER_READ, Inputs, Part};
use dagq::domain::ledger::session::written::{Cursors, rows_written_through};
use dagq::domain::live_alerts::{LiveHistory, judge};
use dagq::domain::marks::utc_text;
use dagq::infrastructure::adapters::{ClaudeCode, SystemProcesses};
use dagq::infrastructure::run_files::LocalRunFiles;
use dagq::infrastructure::sqlite::SqliteQueue;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

/// Counts the bytes and the allocations in use, and the peak of the bytes
/// since the last [`reset_peak`].
struct Counting;

static BYTES: AtomicUsize = AtomicUsize::new(0);
static COUNT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static PEAK_COUNT: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize, allocations: usize) {
    let bytes = BYTES.fetch_add(by, Ordering::Relaxed) + by;
    let count = COUNT.fetch_add(allocations, Ordering::Relaxed) + allocations;
    PEAK.fetch_max(bytes, Ordering::Relaxed);
    PEAK_COUNT.fetch_max(count, Ordering::Relaxed);
}

// SAFETY: every call is passed on to `System` unchanged; only counters move.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract is `System`'s.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            grew(layout.size(), 1);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as `alloc`.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            grew(layout.size(), 1);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: as `alloc`.
        unsafe { System.dealloc(pointer, layout) };
        BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        COUNT.fetch_sub(1, Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as `alloc`.
        let moved = unsafe { System.realloc(pointer, layout, new_size) };
        if !moved.is_null() {
            if new_size >= layout.size() {
                grew(new_size - layout.size(), 0);
            } else {
                BYTES.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The bytes and the allocations in use.
fn in_use() -> (usize, usize) {
    (BYTES.load(Ordering::Relaxed), COUNT.load(Ordering::Relaxed))
}

fn reset_peak() {
    PEAK.store(BYTES.load(Ordering::Relaxed), Ordering::Relaxed);
    PEAK_COUNT.store(COUNT.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// The process's largest resident size (bytes on macOS, KiB on Linux).
fn max_rss() -> i64 {
    // SAFETY: `getrusage` writes the struct it is given.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) };
    usage.ru_maxrss
}

const T0: i64 = 1_900_000_000;
/// Seconds between two events laid down.
const STEP: i64 = 10;

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// The rows the stages print: name, bytes and allocations in use after
/// it, peak bytes and peak allocations during it, seconds.
struct Report {
    base: (usize, usize),
}

impl Report {
    fn stage(&self, name: &str, started: Instant) {
        let (bytes, count) = in_use();
        println!(
            "| {name} | {:.1} | {} | {:.1} | {} | {:.2} |",
            mib(bytes.saturating_sub(self.base.0)),
            count.saturating_sub(self.base.1),
            mib(PEAK.load(Ordering::Relaxed).saturating_sub(self.base.0)),
            PEAK_COUNT
                .load(Ordering::Relaxed)
                .saturating_sub(self.base.1),
            started.elapsed().as_secs_f64()
        );
        reset_peak();
    }
}

/// Lays down the events of a queue in id order: `push` writes each one.
struct Writer<'a> {
    db: &'a Connection,
    insert: rusqlite::Statement<'a>,
    at: i64,
    events: usize,
    payload_bytes: usize,
    kinds: std::collections::BTreeMap<&'static str, usize>,
    class: std::collections::BTreeMap<&'static str, usize>,
    /// A text of this many bytes put in each payload's `pad`, for payloads
    /// larger than the synthetic ones.
    pad: String,
}

impl Writer<'_> {
    fn push(
        &mut self,
        class: &'static str,
        task: Option<i64>,
        run: Option<&str>,
        kind: &'static str,
        payload: &Value,
    ) {
        self.at += STEP;
        let mut payload = payload.clone();
        if !self.pad.is_empty() {
            payload["pad"] = Value::String(self.pad.clone());
        }
        let text = payload.to_string();
        self.payload_bytes += text.len();
        self.insert
            .execute(params![task, run, kind, text, utc_text(self.at * 1000)])
            .unwrap();
        self.events += 1;
        *self.kinds.entry(kind).or_default() += 1;
        *self.class.entry(class).or_default() += 1;
    }

    fn run(&mut self, task: i64, run: &str, kind: &'static str, payload: &Value) {
        self.push("run", Some(task), Some(run), kind, payload);
    }

    fn phase(&mut self, task: i64, run: &str, phase: &str, cause: &str) {
        self.run(
            task,
            run,
            "run_phase_changed",
            &json!({"phase": phase, "blocker": "ai", "holds": "worker_slot",
                    "attempt": {"kind": "first", "n": 1}, "cause": cause, "v": 1}),
        );
    }

    fn queue(&mut self, kind: &'static str, payload: &Value) {
        self.push("queue", None, None, kind, payload);
    }
}

fn run_id(n: i64) -> String {
    format!("{n:08x}-1111-4111-8111-111111111111")
}

/// One finished run of `task` and its task's events: claimed, worked,
/// reviewed, landed and pushed (30 events).
fn finished_run(w: &mut Writer<'_>, task: i64, supervisor: &str) {
    let run = run_id(task);
    let run = run.as_str();
    w.push(
        "task",
        Some(task),
        None,
        "task_created",
        &json!({"title": format!("chore: task {task} of the synthetic queue"), "priority": "normal", "change": "runtime"}),
    );
    w.push(
        "task",
        Some(task),
        None,
        "task_status_changed",
        &json!({"from": "draft", "to": "ready"}),
    );
    w.run(
        task,
        run,
        "run_claimed",
        &json!({"from": "ready", "to": "in_progress", "provider": "claude", "supervisor": supervisor}),
    );
    w.phase(task, run, "provisioning", "claimed");
    w.run(
        task,
        run,
        "agent_started",
        &json!({"pid": 4242, "provider": "claude"}),
    );
    w.phase(task, run, "worker", "agent_started");
    for turn in 0..8 {
        w.run(
            task,
            run,
            "session_turns",
            &json!({"turns": turn + 1, "input_tokens": 123_456, "output_tokens": 7_890}),
        );
    }
    w.run(
        task,
        run,
        "first_commit_observed",
        &json!({"commit": "0e918c82759ed3b44ed2a5464c5afb4eecc83397"}),
    );
    w.run(
        task,
        run,
        "receipt_observed",
        &json!({"result": "succeeded", "summary": "x".repeat(600)}),
    );
    w.phase(task, run, "validating", "receipt");
    w.phase(task, run, "review", "validated");
    w.run(
        task,
        run,
        "review_finished",
        &json!({"verdict": "pass", "body": "y".repeat(1500), "findings": [{"file": "src/lib.rs", "line": 1}]}),
    );
    w.run(
        task,
        run,
        "validation_finished",
        &json!({"status": "awaiting_integration"}),
    );
    w.run(task, run, "landing_queued", &json!({"via": "exit"}));
    w.phase(task, run, "awaiting_slot", "queued");
    w.phase(task, run, "landing", "slot");
    w.run(
        task,
        run,
        "integration_started",
        &json!({"supervisor": supervisor}),
    );
    w.run(task, run, "integration_rebased", &json!({"onto": "main"}));
    w.run(
        task,
        run,
        "run_integrated",
        &json!({"from": "validating", "to": "integrated", "status": "integrated"}),
    );
    w.phase(task, run, "push", "integrated");
    w.run(
        task,
        run,
        "push_finished",
        &json!({"remote": "origin", "branch": "main"}),
    );
    w.phase(task, run, "ended", "pushed");
    w.push(
        "task",
        Some(task),
        None,
        "task_status_changed",
        &json!({"from": "in_progress", "to": "completed"}),
    );
}

/// How many events of the queue as a whole come with each finished run:
/// with the 30 of the run and its task, 71 % of the events are the
/// queue's (the share of `queue_rows` in the production builder's log).
const QUEUE_EVENTS_PER_RUN: usize = 74;
/// How many finished runs one supervisor's life holds (an exec each).
const RUNS_PER_LIFE: i64 = 40;
/// How many finished runs one planner session comes with.
const RUNS_PER_SESSION: i64 = 5;
/// The runs left open at the end, of every size.
const OPEN_RUNS: i64 = 4;
/// The finished runs of the last life, which no stop ends, of every size: so
/// that two queues differ in their settled events only and what that life's
/// rows hold (settled by silence or not, by the time they are read at) comes
/// to as much in each. A multiple of
/// [`RUNS_PER_SESSION`], so that it holds as many sessions wherever it falls.
const OPEN_LIFE_RUNS: i64 = 10;
/// The events of a finished run and of the queue as a whole with it, the
/// sessions aside.
const EVENTS_PER_RUN: usize = 30 + QUEUE_EVENTS_PER_RUN;

/// The events of the queue as a whole that come with one finished run:
/// heartbeats, main's observations and the stream of the judgments of
/// now, a slots full and a deferral.
fn queue_events(w: &mut Writer<'_>, supervisor: &str, task: i64, version: &mut i64) {
    for n in 0..QUEUE_EVENTS_PER_RUN {
        match n {
            0 => w.queue(
                "slots_full_started",
                &json!({"supervisor": supervisor, "class": "heavy"}),
            ),
            1 => w.queue(
                "slots_full_ended",
                &json!({"supervisor": supervisor, "class": "heavy"}),
            ),
            2 => w.push(
                "queue",
                Some(task),
                None,
                "claim_deferred",
                &json!({"supervisor": supervisor, "reason": "paths", "paths": ["src/a.rs", "src/b.rs"]}),
            ),
            3 => w.push(
                "queue",
                Some(task),
                None,
                "claim_deferral_ended",
                &json!({"supervisor": supervisor, "why": "free"}),
            ),
            n if n % 6 == 0 => {
                *version += 1;
                w.queue(
                    "live_alert_input_changed",
                    &json!({"supervisor": supervisor, "version": *version,
                            "inputs": {"free_slots": 1, "candidates": 3, "ready": 2}}),
                );
            }
            n if n % 6 == 1 => w.queue(
                "main_observed",
                &json!({"supervisor": supervisor, "head": "0e918c82759ed3b44ed2a5464c5afb4eecc83397", "commits": 1}),
            ),
            _ => w.queue(
                "supervisor_alive",
                &json!({"supervisor": supervisor, "pid": 2522, "parallel": 4}),
            ),
        }
    }
}

/// The supervisor of the life of `life`.
fn supervisor(life: i64) -> String {
    format!("{life:032x}")
}

/// The events of a new supervisor's life, the last one's stop first if
/// there is one: the life it starts is `life` then.
fn next_life(w: &mut Writer<'_>, life: &mut i64, version: &mut i64) {
    if *life > 0 {
        let before = supervisor(*life);
        w.queue(
            "supervisor_stopped",
            &json!({"supervisor": before, "last_heartbeat_at": w.at}),
        );
    }
    *life += 1;
    let now = supervisor(*life);
    w.queue(
        "supervisor_started",
        &json!({"supervisor": now, "pid": 2522}),
    );
    w.queue(
        "stall_config_loaded",
        &json!({"supervisor": now, "stall": {"worker_secs": 3600}}),
    );
    *version += 1;
    w.queue(
        "live_alert_baseline",
        &json!({"supervisor": now, "version": *version}),
    );
}

/// The next finished run in the life of `life`, with its queue events and,
/// of every [`RUNS_PER_SESSION`]th, a session around it.
fn next_run(w: &mut Writer<'_>, task: &mut i64, life: i64, version: &mut i64) {
    *task += 1;
    let task = *task;
    let now = supervisor(life);
    let opened = (task % RUNS_PER_SESSION == 0).then(|| {
        w.push(
            "session",
            None,
            None,
            "session_opened",
            &json!({"kind": "runtime_planner", "planner_id": task, "supervisor": now}),
        );
        w.db.last_insert_rowid()
    });
    finished_run(w, task, &now);
    queue_events(w, &now, task, version);
    if let Some(opened) = opened {
        w.push(
            "session",
            None,
            None,
            "session_closed",
            &json!({"kind": "runtime_planner", "planner_id": task, "opened_event_id": opened}),
        );
    }
}

fn prepare(dir: &Path, target: usize, pad: usize) {
    lay_down(dir, target, pad, 2 * 86_400, true);
}

/// Lay down the queue of `prepare` at `dir`, its time `after_secs` after
/// its last event, and build its ledger as the supervisor before the
/// restart did; with `says`, print what it holds.
fn lay_down(dir: &Path, target: usize, pad: usize, after_secs: i64, says: bool) -> usize {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("queue.db");
    drop(SqliteQueue::init(&path).unwrap());
    let mut db = Connection::open(&path).unwrap();
    // The synthetic events name tasks, goals and runs the queue has no row
    // of; the folds read the events only.
    db.execute_batch(
        "PRAGMA foreign_keys = OFF; PRAGMA ignore_check_constraints = ON; PRAGMA synchronous = OFF;",
    )
    .unwrap();
    let tx = db.transaction().unwrap();
    let mut w = Writer {
        db: &tx,
        insert: tx
            .prepare(
                "INSERT INTO run_events(task_id, run_id, kind, payload, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .unwrap(),
        at: T0,
        events: 0,
        payload_bytes: 0,
        kinds: Default::default(),
        class: Default::default(),
        pad: "p".repeat(pad),
    };
    let mut task = 0;
    let mut version = 0;
    let mut life = 0;
    let open_life_events = OPEN_LIFE_RUNS as usize * EVENTS_PER_RUN;
    while w.events + open_life_events < target {
        if task % RUNS_PER_LIFE == 0 {
            next_life(&mut w, &mut life, &mut version);
        }
        next_run(&mut w, &mut task, life, &mut version);
    }
    next_life(&mut w, &mut life, &mut version);
    for _ in 0..OPEN_LIFE_RUNS {
        next_run(&mut w, &mut task, life, &mut version);
    }
    // The open rows: the live supervisor's life and runs still working.
    let now = supervisor(life);
    for open in 1..=OPEN_RUNS {
        let open_task = task + open;
        let run = run_id(open_task);
        w.push(
            "task",
            Some(open_task),
            None,
            "task_created",
            &json!({"title": "open"}),
        );
        w.run(
            open_task,
            &run,
            "run_claimed",
            &json!({"from": "ready", "to": "in_progress", "provider": "claude", "supervisor": now}),
        );
        w.phase(open_task, &run, "worker", "agent_started");
    }
    let last_at = w.at;
    let events = w.events;
    if says {
        println!("events: {}", w.events);
        println!("finished runs: {task}, open runs: {OPEN_RUNS}, supervisors' lives: {life}");
        println!("mean payload: {} bytes", w.payload_bytes / w.events.max(1));
        println!("by owner: {:?}", w.class);
        println!("by kind: {:?}", w.kinds);
    }
    drop(w);
    tx.commit().unwrap();
    std::fs::write(dir.join("now"), (last_at + after_secs).to_string()).unwrap();
    // The supervisor before the restart built the ledger up to the end.
    let queue = SqliteQueue::open(&path).unwrap();
    let mut builder = Builder::default();
    let started = Instant::now();
    let passes = catch_up(&queue, &mut builder, now_ms(dir));
    if says {
        println!(
            "ledger built by the earlier supervisor: {passes} passes, {:.1} s",
            started.elapsed().as_secs_f64()
        );
    }
    events
}

fn now_ms(dir: &Path) -> i64 {
    let now: i64 = std::fs::read_to_string(dir.join("now"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    now * 1000
}

/// Passes of the ledger at `now_ms` until no mark waits; how many.
fn catch_up(queue: &SqliteQueue, builder: &mut Builder, now_ms: i64) -> usize {
    let built_at = utc_text(now_ms);
    let mut passes = 0;
    loop {
        let built = build_pass(queue, builder, (now_ms, &built_at), Retain::default(), None);
        assert_eq!(built.failure, None);
        passes += 1;
        if built.backlog == 0 || passes >= 100_000 {
            return passes;
        }
    }
}

/// Clones each part `parts` names and prints what the clone allocated.
fn print_parts(owner: &str, parts: impl FnOnce(&mut dyn FnMut(&'static str, &dyn Fn() -> Part))) {
    parts(&mut |name, clone| {
        let before = in_use();
        let part = clone();
        let after = in_use();
        println!(
            "| {owner} | {name} | {:.2} | {} |",
            mib(after.0 - before.0),
            after.1 - before.1
        );
        drop(part);
    });
}

fn measure(dir: &Path) {
    let now_ms = now_ms(dir);
    let queue = SqliteQueue::open(dir.join("queue.db")).unwrap();
    println!("## the start from the marks (start_builder)");
    println!();
    println!("| stage | in use MiB | allocations | peak MiB | peak allocations | s |");
    println!("| --- | --- | --- | --- | --- | --- |");
    let gone_on = {
        let base = in_use();
        let report = Report { base };
        go_on(&queue, now_ms, &|name, started| report.stage(name, started))
    };
    println!();
    gone_on.print(queue.latest_event_id().unwrap().as_i64());
    println!();
    println!("## the start from the cursors' start");
    println!();
    let base = in_use();
    reset_peak();
    let report = Report { base };
    println!("| stage | in use MiB | allocations | peak MiB | peak allocations | s |");
    println!("| --- | --- | --- | --- | --- | --- |");

    // The first pass of the judgments of now, as `LiveAlertWatch::pass`
    // runs it (the records it writes aside).
    let started = Instant::now();
    let history = first_live_pass(&queue, now_ms);
    report.stage("live: first pass (LiveHistory kept)", started);

    // The ledger's catch-up from its cursors' start.
    // The first part of the ledger's first pass, as `build_pass` reads the
    // events (the node samples, steps, resources and `due` aside): every
    // event past its cursor marked, before any row is built or swept.
    let started = Instant::now();
    let mut builder = Builder::default();
    let latest = queue.latest_event_id().unwrap();
    while latest > builder.events {
        let new = queue
            .events_between(
                builder.events,
                latest,
                &EventFilter::default(),
                EVENTS_PER_READ,
            )
            .unwrap();
        if new.is_empty() {
            break;
        }
        builder.mark_events(&new);
    }
    let marked = builder.kept().size();
    report.stage(
        &format!(
            "ledger: pass 1, events marked (Kept events {}, rows {})",
            marked.events(),
            marked.queue_rows + marked.session_rows
        ),
        started,
    );
    // What the pass's rows hold before it writes or sweeps any: the rows
    // it builds folded from Kept ([`Inputs::of`]), dropped again here.
    let started = Instant::now();
    let built_at = utc_text(now_ms);
    {
        let rows = builder.rows_to_build(builder.kept());
        let inputs = Inputs::of(builder.kept(), now_ms, &built_at, &rows);
        let (bytes, count) = in_use();
        println!(
            "| ledger: pass 1, Inputs::of of {} rows held | {:.1} | {} | | | |",
            inputs.rows.len(),
            mib(bytes.saturating_sub(base.0)),
            count.saturating_sub(base.1)
        );
    }
    report.stage("ledger: pass 1, Inputs::of dropped", started);
    let started = Instant::now();
    let first = build_pass(
        &queue,
        &mut builder,
        (now_ms, &built_at),
        Retain::default(),
        None,
    );
    assert_eq!(first.failure, None);
    report.stage(
        &format!(
            "ledger: pass 1, rows built and swept (written {}, backlog {})",
            first.written, first.backlog
        ),
        started,
    );
    let started = Instant::now();
    let second = build_pass(
        &queue,
        &mut builder,
        (now_ms, &built_at),
        Retain::default(),
        None,
    );
    assert_eq!(second.failure, None);
    report.stage(
        &format!(
            "ledger: pass 2 (written {}, backlog {})",
            second.written, second.backlog
        ),
        started,
    );
    let started = Instant::now();
    let passes = catch_up(&queue, &mut builder, now_ms) + 2;
    report.stage(&format!("ledger: caught up ({passes} passes)"), started);
    let kept = builder.kept().size();
    println!();
    println!("KeptSize after the catch-up: {kept:?}");
    println!("max rss: {}", max_rss());
    println!();

    println!("| holder | part | clone MiB | clone allocations |");
    println!("| --- | --- | --- | --- |");
    print_parts("Kept", |each| builder.kept().clone_parts(each));
    print_parts("Builder", |each| builder.clone_parts(each));
    println!();

    let started = Instant::now();
    let before = in_use();
    drop(history);
    let after = in_use();
    println!(
        "dropped LiveHistory: {:.2} MiB, {} allocations",
        mib(before.0 - after.0),
        before.1 - after.1
    );
    let kept = builder.kept().clone();
    let before = in_use();
    drop(builder);
    let after = in_use();
    println!(
        "dropped Builder but Kept: {:.2} MiB, {} allocations",
        mib(before.0 - after.0),
        before.1 - after.1
    );
    let before = in_use();
    drop(kept);
    let after = in_use();
    println!(
        "dropped Kept: {:.2} MiB, {} allocations",
        mib(before.0 - after.0),
        before.1 - after.1
    );
    println!();
    println!("| stage | in use MiB | allocations | peak MiB | peak allocations | s |");
    println!("| --- | --- | --- | --- | --- | --- |");
    report.stage("left after every holder dropped", started);
}

/// How many events the smaller queue of `check` holds, about: a page
/// ([`EVENTS_PER_READ`]) and more, so that both queues are read in more
/// than one page, and small enough that `check` takes some tens of seconds
/// in the dev profile.
const CHECK_SMALL: usize = 6_000;

/// How long after its last event the queues of `check` are read: past the
/// grace of every row (a week, an interactive session's after it closed),
/// so that every row but those of the runs left open and the live
/// supervisor's life is settled in the store, and the two queues differ
/// in their settled events only.
const CHECK_AFTER_SECS: i64 = 8 * 86_400;

/// By how much of the smaller queue's bytes the larger one's may exceed
/// them, and by how many bytes besides ([`within`]). What the builder keeps
/// that is not of a row open still grows with the runs, by design: the
/// waits before a claim that a task's fold reads (two of each run here),
/// the landing slot's attempts (one of each run) and the summaries of the
/// pushes; and the judgments of now keep a work clock of each run. They
/// came to about 1.5 KB of each run of about a hundred events (some 15
/// bytes of each settled event, 0.3 MiB for the 18,000 events more of the
/// larger queue), under [`CHECK_KEPT_SLACK`]. The queue rows and the marks
/// of the rows dropped that the builder kept besides took 246 bytes of
/// each settled event (docs/plans/supervisor-startup-memory-spike.md
/// "結果"), about 4 MiB here, and the peak that held the events and the
/// folds of every row 3.2 KB of each, about 55 MiB, past
/// [`CHECK_PEAK_SLACK`]; the peak of a page read and its folds was about
/// 12 MiB of either queue, and grew by 1.3 MiB. The 20 % is this
/// judgment's own margin; the start from the marks has [`within_cap`].
const CHECK_RATIO: f64 = 1.2;
const CHECK_KEPT_SLACK: usize = 1024 * 1024;
const CHECK_PEAK_SLACK: usize = 4 * 1024 * 1024;

/// What a restarted supervisor's catch-up held: the bytes in use after it
/// (the builder and the judgments' history kept) and the peak during it,
/// over those in use before.
struct Restart {
    events: usize,
    kept: usize,
    peak: usize,
    passes: usize,
}

/// The first pass of the judgments of now, as `LiveAlertWatch::pass` runs
/// it (the records it writes aside): the history it keeps.
fn first_live_pass(queue: &SqliteQueue, now_ms: i64) -> LiveHistory {
    let mut history = LiveHistory::default();
    let files = LocalRunFiles;
    let signals = ClaudeCode {
        executable: "claude".into(),
    };
    let no_file = || Ok(None);
    let sources = LiveSources {
        files: &files,
        signals: &signals,
        config_file: &no_file,
    };
    let (inputs, events) = carried_live_inputs(
        queue,
        &SystemProcesses,
        now_ms / 1000,
        &mut history,
        &sources,
    )
    .unwrap();
    let alerts = judge(&events, now_ms / 1000, &inputs).keys();
    std::hint::black_box((&inputs, &events, &alerts));
    history
}

/// Restart a supervisor on the queue at `dir`, of `events` events.
fn restart(dir: &Path, events: usize) -> Restart {
    let now_ms = now_ms(dir);
    let queue = SqliteQueue::open(dir.join("queue.db")).unwrap();
    let base = in_use().0;
    reset_peak();
    let history = first_live_pass(&queue, now_ms);
    let mut builder = Builder::default();
    let passes = catch_up(&queue, &mut builder, now_ms);
    let kept = in_use().0.saturating_sub(base);
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    println!(
        "{events} events: kept {:.2} MiB, peak {:.2} MiB, {passes} passes; {:?}",
        mib(kept),
        mib(peak),
        builder.kept().size()
    );
    drop((builder, history));
    Restart {
        events,
        kept,
        peak,
        passes,
    }
}

/// How many of each the start from the marks reads, and what the store
/// holds of them: read out of the stages, on the store as the start finds
/// it, so that their allocations count in no stage.
struct Reads {
    /// The queue rows the store holds, and the heads of those the start reads.
    queue_rows: usize,
    queue_heads: usize,
    session_heads: usize,
    node_heads: usize,
    /// The run and task rows not settled at R.
    open_heads: usize,
    /// The records of the summaries of the pushes, those of no run with them.
    push_records: usize,
}

impl Reads {
    fn of(queue: &SqliteQueue, r: &Cursors) -> Self {
        let since = queue.ledger_tasks_open_since_at(r.events).unwrap();
        Self {
            queue_rows: queue.ledger_queue_heads().unwrap().len(),
            queue_heads: queue.ledger_queue_heads_at(r.events, since).unwrap().len(),
            session_heads: queue.ledger_heads(LedgerKind::Session).unwrap().len(),
            node_heads: queue.ledger_heads(LedgerKind::Node).unwrap().len(),
            open_heads: queue
                .ledger_heads_open_at(r.events, r.resources)
                .unwrap()
                .len(),
            push_records: queue.ledger_push_records().unwrap().len()
                + queue.ledger_runless_pushes().unwrap().len(),
        }
    }
}

/// What a restarted supervisor held that started from the marks, over the
/// bytes in use before its first pass of the judgments of now.
struct GoneOn {
    /// The position R the marks gave.
    r: i64,
    /// (a) The peak during the call of `start_builder`.
    start_peak: usize,
    /// (b) The peak after that call up to the end of the catch-up.
    catch_up_peak: usize,
    /// (c) The bytes in use after the catch-up.
    kept: usize,
    passes: usize,
    reads: Reads,
}

impl GoneOn {
    fn print(&self, latest: i64) {
        println!(
            "R {} of {latest} events; (a) start_builder peak {} bytes ({:.2} MiB); (b) catch-up peak {} bytes ({:.2} MiB); (c) kept {} bytes ({:.2} MiB); {} passes",
            self.r,
            self.start_peak,
            mib(self.start_peak),
            self.catch_up_peak,
            mib(self.catch_up_peak),
            self.kept,
            mib(self.kept),
            self.passes
        );
        let reads = &self.reads;
        println!(
            "read: store queue rows {}, queue heads {}, session heads {}, node heads {}, open run and task heads {}, push records {}",
            reads.queue_rows,
            reads.queue_heads,
            reads.session_heads,
            reads.node_heads,
            reads.open_heads,
            reads.push_records
        );
    }
}

/// Restart a supervisor on `queue` by the way a store with marks starts:
/// the first pass of the judgments of now, then `start_builder` and the
/// passes until no mark waits. `said` is told each stage's end; it may print
/// and reset the peak. Panics unless the start went on from R, that is
/// unless the marks give R, the store holds no older row and the builder's
/// cursors stand at R: a start from the cursors' start is not what this
/// measures.
fn go_on(queue: &SqliteQueue, now_ms: i64, said: &dyn Fn(&str, Instant)) -> GoneOn {
    let r = rows_written_through(
        queue.ledger_marks().unwrap().as_ref(),
        &queue.ledger_heads(LedgerKind::Session).unwrap(),
    )
    .expect("the marks give no R: the earlier supervisor's build wrote none");
    assert!(r.events.as_i64() > 0, "R is at the cursors' start");
    assert!(
        !queue.ledger_older_rows().unwrap(),
        "the store holds a row older than this version"
    );
    let reads = Reads::of(queue, &r);
    let base = in_use().0;
    reset_peak();
    let peak = || PEAK.load(Ordering::Relaxed).saturating_sub(base);

    let started = Instant::now();
    let history = first_live_pass(queue, now_ms);
    said("first pass of the judgments of now", started);
    reset_peak();

    let started = Instant::now();
    let mut builder = start_builder(queue, now_ms).unwrap();
    let start_peak = peak();
    let from = builder.events;
    said("(a) after the call of start_builder", started);
    assert_eq!(from, r.events, "the start did not go on from R");
    reset_peak();

    let started = Instant::now();
    let passes = catch_up(queue, &mut builder, now_ms);
    let catch_up_peak = peak();
    let kept = in_use().0.saturating_sub(base);
    said("(b)(c) after the catch-up", started);

    let started = Instant::now();
    drop((builder, history));
    said("after every holder dropped", started);
    GoneOn {
        r: r.events.as_i64(),
        start_peak,
        catch_up_peak,
        kept,
        passes,
        reads,
    }
}

/// The history the caps of the start from the marks are the extrapolation
/// to, in events: production's in goal 241.
const CAP_EVENTS: usize = 1_100_000;
/// (a) What the call of `start_builder` may peak at over a history of
/// [`CAP_EVENTS`] ([`within_cap`]).
const CAP_START_PEAK: usize = 128 * 1024 * 1024;
/// (b) What the peak after that call up to the end of the catch-up may
/// reach over the same history.
const CAP_CATCH_UP_PEAK: usize = 128 * 1024 * 1024;
/// (c) What may stay in use after the catch-up over the same history.
const CAP_KEPT: usize = 64 * 1024 * 1024;

/// Whether `large` exceeds `small` by no more than `cap` scaled to the
/// `more_events` settled events that are all that differs of the two
/// queues: `large - small <= cap * more_events / CAP_EVENTS`, the judgment
/// of the start from the marks. Three quantities are judged each on its
/// own so that the page reads' peak of the catch-up (about 12 MiB) hides no
/// growth of the start's. No ratio of `small` is allowed: the 20 % of
/// [`within`] would let a slope through unseen, and the difference reads
/// directly as what [`CAP_EVENTS`] would come to. What differs by where the
/// live life falls in each queue is made the same by [`OPEN_LIFE_RUNS`].
///
/// A cap holds the growth by design and that growth is not taken off
/// besides: the waits, the landing slot's attempts, the summaries of the
/// pushes and the judgments' work clock keep about 15 bytes of each settled
/// event ([`CHECK_RATIO`]), about 16 MiB of [`CAP_EVENTS`]. [`CAP_KEPT`] is
/// four times that; [`CAP_START_PEAK`] and [`CAP_CATCH_UP_PEAK`] are eight
/// times, for what the start's reads hold for a while too (the rows' heads
/// and the records of the pushes). A growth past them is what to fix, not a
/// cap to widen. They come to about 1 to 2 % of production's 6.2 GB.
fn within_cap(large: usize, small: usize, cap: usize, more_events: usize) -> bool {
    large.saturating_sub(small) as f64 <= cap as f64 * more_events as f64 / CAP_EVENTS as f64
}

/// Whether `large` is within [`CHECK_RATIO`] of `small` and `slack`.
fn within(large: usize, small: usize, slack: usize) -> bool {
    large as f64 <= small as f64 * CHECK_RATIO + slack as f64
}

fn check() {
    let root =
        std::env::temp_dir().join(format!("supervisor-startup-check-{}", std::process::id()));
    let restarted = |target: usize| {
        let dir = root.join(target.to_string());
        let events = lay_down(&dir, target, 0, CHECK_AFTER_SECS, false);
        // On the store as the earlier supervisor left it.
        let now_ms = now_ms(&dir);
        let queue = SqliteQueue::open(dir.join("queue.db")).unwrap();
        let gone_on = go_on(&queue, now_ms, &|_, _| ());
        print!("{events} events from the marks: ");
        gone_on.print(queue.latest_event_id().unwrap().as_i64());
        drop(queue);
        (restart(&dir, events), gone_on)
    };
    let (small, small_on) = restarted(CHECK_SMALL);
    let (large, large_on) = restarted(4 * small.events);
    std::fs::remove_dir_all(&root).unwrap();
    assert!(
        large.events >= 4 * small.events,
        "{} {}",
        large.events,
        small.events
    );
    assert!(small.passes > 0 && large.passes > 0);
    let more_events = large.events - small.events;
    for (what, large_bytes, small_bytes, cap) in [
        (
            "(a) the peak of the call of start_builder",
            large_on.start_peak,
            small_on.start_peak,
            CAP_START_PEAK,
        ),
        (
            "(b) the peak after start_builder up to the end of the catch-up",
            large_on.catch_up_peak,
            small_on.catch_up_peak,
            CAP_CATCH_UP_PEAK,
        ),
        (
            "(c) kept after the catch-up from the marks",
            large_on.kept,
            small_on.kept,
            CAP_KEPT,
        ),
    ] {
        println!(
            "{what}: {:.2} MiB more of {more_events} events more, within {:.2} MiB",
            mib(large_bytes.saturating_sub(small_bytes)),
            cap as f64 * more_events as f64 / CAP_EVENTS as f64 / (1024.0 * 1024.0),
        );
        assert!(
            within_cap(large_bytes, small_bytes, cap, more_events),
            "{what} grows with the settled events: {:.2} MiB of {} events, {:.2} MiB of {}",
            mib(large_bytes),
            large.events,
            mib(small_bytes),
            small.events
        );
    }
    assert!(
        within(large.kept, small.kept, CHECK_KEPT_SLACK),
        "kept after the catch-up grows with the settled events: {:.2} MiB of {} events, {:.2} MiB of {}",
        mib(large.kept),
        large.events,
        mib(small.kept),
        small.events
    );
    assert!(
        within(large.peak, small.peak, CHECK_PEAK_SLACK),
        "the catch-up's peak grows with the settled events: {:.2} MiB of {} events, {:.2} MiB of {}",
        mib(large.peak),
        large.events,
        mib(small.peak),
        small.events
    );
    println!("ok");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("prepare") if (4..=5).contains(&args.len()) => {
            let pad = args.get(4).map_or(0, |pad| pad.parse().unwrap());
            prepare(Path::new(&args[2]), args[3].parse().unwrap(), pad);
        }
        Some("measure") if args.len() == 3 => measure(Path::new(&args[2])),
        Some("check") if args.len() == 2 => check(),
        _ => {
            eprintln!(
                "usage: supervisor_startup_memory prepare <dir> <events> [<pad bytes>] | measure <dir> | check"
            );
            std::process::exit(2);
        }
    }
}
