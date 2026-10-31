//! What a supervisor keeps in its allocator after its start's catch-up,
//! measured in the process (docs/plans/supervisor-startup-memory-spike.md).
//!
//! `prepare <dir> <events> [<pad>]` lays down a queue at `<dir>/queue.db` of about
//! `<events>` events settled (finished runs and tasks, closed sessions,
//! supervisors' lives and their queue events) and a few open ones, then
//! builds its ledger as a supervisor before the restart did, so that the
//! store holds the rows settled; `<pad>` adds a text of that many bytes to
//! every payload. `measure <dir>` is the restarted
//! supervisor: the first pass of the judgments of now
//! (`carried_live_inputs` on a new `LiveHistory`) and the ledger's catch-up
//! (`build_pass` from its cursors' start until no mark waits), on the same
//! ports the supervisor uses (`SqliteQueue`), and says after each stage the
//! bytes and allocations in use and the peak, then what each part held.
//!
//! Run with `cargo run --release --example supervisor_startup_memory --
//! prepare "$TMPDIR/q20k" 20000` and then `... -- measure "$TMPDIR/q20k"`.
//! A counting global allocator is set here only: neither the library's
//! tests nor the integration tests carry it.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use dagq::application::stats::{LiveSources, carried_live_inputs};
use dagq::application::supervise::{Retain, build_pass};
use dagq::domain::EventFilter;
use dagq::domain::ledger::builder::{Builder, EVENTS_PER_READ, Inputs, Part};
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

fn prepare(dir: &Path, target: usize, pad: usize) {
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
    while w.events < target {
        task += 1;
        if (task - 1) % RUNS_PER_LIFE == 0 {
            if life > 0 {
                let before = supervisor(life);
                w.queue(
                    "supervisor_stopped",
                    &json!({"supervisor": before, "last_heartbeat_at": w.at}),
                );
            }
            life += 1;
            let now = supervisor(life);
            w.queue(
                "supervisor_started",
                &json!({"supervisor": now, "pid": 2522}),
            );
            w.queue(
                "stall_config_loaded",
                &json!({"supervisor": now, "stall": {"worker_secs": 3600}}),
            );
            version += 1;
            w.queue(
                "live_alert_baseline",
                &json!({"supervisor": now, "version": version}),
            );
        }
        let now = supervisor(life);
        if task % RUNS_PER_SESSION == 0 {
            w.push(
                "session",
                None,
                None,
                "session_opened",
                &json!({"kind": "runtime_planner", "planner_id": task, "supervisor": now}),
            );
        }
        finished_run(&mut w, task, &now);
        queue_events(&mut w, &now, task, &mut version);
        if task % RUNS_PER_SESSION == 0 {
            w.push(
                "session",
                None,
                None,
                "session_closed",
                &json!({"kind": "runtime_planner", "planner_id": task}),
            );
        }
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
    println!("events: {}", w.events);
    println!("finished runs: {task}, open runs: {OPEN_RUNS}, supervisors' lives: {life}");
    println!("mean payload: {} bytes", w.payload_bytes / w.events.max(1));
    println!("by owner: {:?}", w.class);
    println!("by kind: {:?}", w.kinds);
    drop(w);
    tx.commit().unwrap();
    std::fs::write(dir.join("now"), (last_at + 2 * 86_400).to_string()).unwrap();
    // The supervisor before the restart built the ledger up to the end.
    let queue = SqliteQueue::open(&path).unwrap();
    let mut builder = Builder::default();
    let started = Instant::now();
    let passes = catch_up(&queue, &mut builder, now_ms(dir));
    println!(
        "ledger built by the earlier supervisor: {passes} passes, {:.1} s",
        started.elapsed().as_secs_f64()
    );
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
    let base = in_use();
    reset_peak();
    let report = Report { base };
    println!("| stage | in use MiB | allocations | peak MiB | peak allocations | s |");
    println!("| --- | --- | --- | --- | --- | --- |");

    // The first pass of the judgments of now, as `LiveAlertWatch::pass`
    // runs it (the records it writes aside).
    let started = Instant::now();
    let mut history = LiveHistory::default();
    {
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
            &queue,
            &SystemProcesses,
            now_ms / 1000,
            &mut history,
            &sources,
        )
        .unwrap();
        let alerts = judge(&events, now_ms / 1000, &inputs).keys();
        std::hint::black_box((&inputs, &events, &alerts));
    }
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("prepare") if (4..=5).contains(&args.len()) => {
            let pad = args.get(4).map_or(0, |pad| pad.parse().unwrap());
            prepare(Path::new(&args[2]), args[3].parse().unwrap(), pad);
        }
        Some("measure") if args.len() == 3 => measure(Path::new(&args[2])),
        _ => {
            eprintln!(
                "usage: supervisor_startup_memory prepare <dir> <events> [<pad bytes>] | measure <dir>"
            );
            std::process::exit(2);
        }
    }
}
