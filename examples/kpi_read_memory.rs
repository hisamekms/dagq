//! What one read of `kpi` and of the observer's KPIs (`ObserverKpi`) holds,
//! layer by layer, measured in the process
//! (docs/plans/kpi-read-memory-spike.md).
//!
//! `prepare <dir> <events>` lays down a queue at `<dir>/queue.db` of about
//! `<events>` events, with its runs' session steps and the host's samples,
//! and builds its ledger as a supervisor does, page by page behind the
//! events. `measure <dir> <kpi|observer> [<candidate>]` makes one read, cut
//! in the stages (a) the rows' text, (b) the rows ([`dagq::domain::ledger::LedgerRow`]),
//! (c) the window, (d) the folds, and prints one line of JSON: each layer's
//! peak and what it carried. `whole <dir> <kpi|observer>` makes the read as
//! the runtime does, uncut, to set the cut read's peak against. `parts <dir>`
//! reads the rows and prints what a copy of each of their parts takes, in a
//! process of its own so that the copies count in no read's peak.
//!
//! The candidates are the prototypes the document names: none of them is
//! wired to `kpi`, `stats` or `report`.
//!
//! One process makes one read. A process whose counted bytes in use pass
//! [`LIMIT`] stops in the allocator, whichever stage it is in.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use dagq::application::kpi::{self, Host};
use dagq::application::supervise::{Retain, build_pass};
use dagq::application::{LedgerStore, NodeSampleStore, SessionStepStore};
use dagq::domain::areas::AreaMap;
use dagq::domain::environment::NodeRate;
use dagq::domain::host_metrics::{COLUMNS, HostSample, SampleSource};
use dagq::domain::kpi::config::KpiConfig;
use dagq::domain::kpi::{KpiQuery, Period, observe};
use dagq::domain::ledger::builder::Builder;
use dagq::domain::ledger::window::{
    self, LedgerWindow, WindowQuery, run_on_its_way, runs_on_their_way,
};
use dagq::domain::ledger::{LedgerKind, LedgerRow, steps};
use dagq::domain::marks::utc_text;
use dagq::domain::session_step::{Step, StepKind, StepTarget};
use dagq::domain::{RunEvent, RunId};
use dagq::infrastructure::sqlite::SqliteQueue;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

/// The bytes one process may hold counted: past them it stops, to keep a
/// measure on a host shared with the workers and the supervisor from
/// exhausting it. Its resident size may pass them: the allocator's rounding
/// and SQLite's own allocations are not counted
/// (docs/plans/kpi-read-memory-spike.md).
const LIMIT: usize = 3 * 1024 * 1024 * 1024;

/// Counts the bytes in use and their peak since the last [`reset_peak`].
struct Counting;

static BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn stop_at_limit() -> ! {
    let said = b"kpi_read_memory: past the limit of 3 GiB counted in use\n";
    // SAFETY: writes a static buffer; it allocates nothing, as an
    // allocator's call must not.
    unsafe {
        libc::write(2, said.as_ptr().cast(), said.len());
        libc::_exit(42);
    }
}

fn grew(by: usize) {
    let bytes = BYTES.fetch_add(by, Ordering::Relaxed) + by;
    PEAK.fetch_max(bytes, Ordering::Relaxed);
    if bytes > LIMIT {
        stop_at_limit();
    }
}

// SAFETY: every call is passed on to `System` unchanged; only counters move.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract is `System`'s.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            grew(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as `alloc`.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            grew(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: as `alloc`.
        unsafe { System.dealloc(pointer, layout) };
        BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as `alloc`.
        let moved = unsafe { System.realloc(pointer, layout, new_size) };
        if !moved.is_null() {
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                BYTES.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn in_use() -> usize {
    BYTES.load(Ordering::Relaxed)
}

fn reset_peak() {
    PEAK.store(in_use(), Ordering::Relaxed);
}

fn peak() -> usize {
    PEAK.load(Ordering::Relaxed)
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
/// Seconds between two events: production's 1.2 million in about six weeks.
const STEP: i64 = 3;
/// The seconds of the host's samples: production samples every 30 s and
/// keeps 30 days, about one sample per 14 of its events.
const SAMPLE_EVERY_SECS: i64 = 14 * STEP;
/// The events written between two builds of the ledger.
const EVENTS_PER_BUILD: usize = 10_000;
/// The steps of a finished run's session.
const STEPS_PER_RUN: i64 = 240;
/// The bytes of a receipt's summary and of a review's body.
const SUMMARY_BYTES: usize = 2_400;
const REVIEW_BYTES: usize = 3_000;
/// Events of the queue as a whole per finished run, and runs per
/// supervisor's life and per planner session: the shares
/// docs/plans/supervisor-startup-memory-spike.md set to production's.
const QUEUE_EVENTS_PER_RUN: usize = 74;
const RUNS_PER_LIFE: i64 = 40;
const RUNS_PER_SESSION: i64 = 5;
const SHA: &str = "0e918c82759ed3b44ed2a5464c5afb4eecc83397";

/// Lays down the events in id order: `push` writes each.
struct Writer<'a> {
    db: &'a Connection,
    insert: rusqlite::Statement<'a>,
    at: i64,
    events: usize,
    payload_bytes: usize,
    kinds: std::collections::BTreeMap<&'static str, (usize, usize)>,
}

impl Writer<'_> {
    fn push(&mut self, task: Option<i64>, run: Option<&str>, kind: &'static str, payload: &Value) {
        self.at += STEP;
        let text = payload.to_string();
        self.payload_bytes += text.len();
        self.insert
            .execute(params![task, run, kind, text, utc_text(self.at * 1000)])
            .unwrap();
        self.events += 1;
        let of_kind = self.kinds.entry(kind).or_default();
        of_kind.0 += 1;
        of_kind.1 += text.len();
    }

    fn run(&mut self, task: i64, run: &str, kind: &'static str, payload: &Value) {
        self.push(Some(task), Some(run), kind, payload);
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
        self.push(None, None, kind, payload);
    }
}

fn run_id(n: i64) -> String {
    format!("{n:08x}-1111-4111-8111-111111111111")
}

fn supervisor(life: i64) -> String {
    format!("{life:032x}")
}

/// One finished run of `task` with its task's events (30 events).
fn finished_run(w: &mut Writer<'_>, task: i64, now: &str) {
    let run = run_id(task);
    let run = run.as_str();
    let change = ["fix", "feature", "refactor", "test", "docs"][(task % 5) as usize];
    w.push(
        Some(task),
        None,
        "task_created",
        &json!({"title": format!("chore: task {task} of the synthetic queue"), "priority": "normal",
                "change": change, "goal_id": task / 20, "status": "draft", "origin": "planner",
                "depends_on": [task - 1]}),
    );
    w.push(
        Some(task),
        None,
        "task_status_changed",
        &json!({"from": "draft", "to": "ready"}),
    );
    w.run(
        task,
        run,
        "run_claimed",
        &json!({"from": "ready", "to": "in_progress", "provider": "claude", "supervisor": now}),
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
    w.run(task, run, "first_commit_observed", &json!({"commit": SHA}));
    w.run(
        task,
        run,
        "receipt_observed",
        &json!({"result": "succeeded", "summary": "x".repeat(SUMMARY_BYTES)}),
    );
    w.phase(task, run, "validating", "receipt");
    w.phase(task, run, "review", "validated");
    w.run(
        task,
        run,
        "review_finished",
        &json!({"verdict": "pass", "body": "y".repeat(REVIEW_BYTES), "findings": [{"file": "src/lib.rs", "line": 1}]}),
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
        &json!({"supervisor": now}),
    );
    w.run(task, run, "integration_rebased", &json!({"onto": "main"}));
    w.run(
        task,
        run,
        "run_integrated",
        &json!({"from": "validating", "to": "integrated", "status": "integrated", "commit": format!("{task:040x}")}),
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
        Some(task),
        None,
        "task_status_changed",
        &json!({"from": "in_progress", "to": "completed"}),
    );
}

/// The queue's events that come with one finished run, and main's commit.
fn queue_events(w: &mut Writer<'_>, now: &str, task: i64, version: &mut i64) {
    for n in 0..QUEUE_EVENTS_PER_RUN {
        match n {
            0 => w.queue(
                "slots_full_started",
                &json!({"supervisor": now, "class": "heavy"}),
            ),
            1 => w.queue(
                "slots_full_ended",
                &json!({"supervisor": now, "class": "heavy"}),
            ),
            2 => w.push(
                Some(task),
                None,
                "claim_deferred",
                &json!({"supervisor": now, "reason": "paths", "paths": ["src/a.rs", "src/b.rs"]}),
            ),
            3 => w.push(
                Some(task),
                None,
                "claim_deferral_ended",
                &json!({"supervisor": now, "why": "free"}),
            ),
            4 => w.queue(
                "main_commits_recorded",
                &json!({"after": null, "commits": [{"sha": format!("{task:040x}"), "at": w.at,
                    "changes": [{"path": "src/application/kpi.rs"}, {"path": "src/domain/ledger/window.rs"},
                                {"path": "docs/plans/kpi-read-memory-spike.md"}, {"path": "tests/it/runtime_report.rs"}]}]}),
            ),
            n if n % 6 == 0 => {
                *version += 1;
                let run = run_id(task);
                w.queue(
                    "live_alert_input_changed",
                    &json!({"supervisor": now, "version": *version, "target": "run", "run_id": run,
                            "value": {"run_id": run, "task_id": task, "status": "running",
                                      "workspace_id": format!("workspace:{task}"), "idle": null, "receipt": null,
                                      "input": w.at, "background_since": null, "background_alive": null,
                                      "outlier_threshold": 3600, "position": n}}),
                );
            }
            n if n % 6 == 1 => w.queue(
                "main_observed",
                &json!({"head": SHA, "at": w.at, "since": T0, "reverts": true}),
            ),
            _ => w.queue("supervisor_alive", &json!({"supervisor": now})),
        }
    }
}

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
        &json!({"supervisor": now, "version": *version,
                "slots": {"free_slots": 1, "candidates": 3, "ready": 2},
                "config": {"worker_secs": 3600}, "runs": [], "open": []}),
    );
}

/// The session steps of the run of `task`: a model's turn and a tool's call
/// in turn, as a worker's session reports them.
fn run_steps(task: i64) -> Vec<Step> {
    (1..=STEPS_PER_RUN)
        .map(|seq| {
            let mut step = Step::new(
                seq,
                if seq % 2 == 0 {
                    StepKind::ToolCall
                } else {
                    StepKind::ModelTurn
                },
                seq * 20_000,
                15_000 + task % 7,
            );
            if seq % 2 == 0 {
                step.tool = Some("Bash".to_owned());
                step.command_shape =
                    Some(json!({"program": "cargo", "words": ["test", "--locked"]}));
                step.exit_code = Some(0);
                step.output_bytes = Some(4_096);
            } else {
                step.model = Some("claude-opus-5-5".to_owned());
            }
            step
        })
        .collect()
}

/// One host's sample with every column.
fn host_sample(unix: i64) -> HostSample {
    let mut sample = HostSample::new(unix);
    for column in &COLUMNS[2..] {
        sample.set(column, Some(((unix % 1000) as f64).mul_add(0.25, 1.5)));
    }
    sample
}

/// How many bytes `<dir>/queue.db` and its write-ahead log hold.
fn queue_bytes(dir: &Path) -> u64 {
    ["queue.db", "queue.db-wal"]
        .iter()
        .filter_map(|name| std::fs::metadata(dir.join(name)).ok())
        .map(|file| file.len())
        .sum()
}

/// Passes of the ledger at `now_ms` until no mark waits.
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

fn prepare(dir: &Path, target: usize) {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("queue.db");
    drop(SqliteQueue::init(&path).unwrap());
    let queue = SqliteQueue::open(&path).unwrap();
    let mut db = Connection::open(&path).unwrap();
    // The synthetic events name tasks, goals and runs the queue has no row
    // of; the folds read the events only.
    db.execute_batch(
        "PRAGMA foreign_keys = OFF; PRAGMA ignore_check_constraints = ON; PRAGMA synchronous = OFF;",
    )
    .unwrap();
    let mut builder = Builder::default();
    let (mut task, mut version, mut life) = (0, 0, 0);
    let mut at = T0;
    let mut events = 0;
    let mut payload_bytes = 0;
    let mut passes = 0;
    let mut next_sample = T0;
    let mut kinds = std::collections::BTreeMap::new();
    let mut first = true;
    let rate = NodeRate {
        per_second: Default::default(),
        interval: std::time::Duration::from_secs(SAMPLE_EVERY_SECS.unsigned_abs()),
    };
    while events < target {
        let tx = db.transaction().unwrap();
        let mut w = Writer {
            db: &tx,
            insert: tx
                .prepare(
                    "INSERT INTO run_events(task_id, run_id, kind, payload, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .unwrap(),
            at,
            events: 0,
            payload_bytes: 0,
            kinds: std::collections::BTreeMap::new(),
        };
        if first {
            // The queue's mark that closes the attributes' baseline: the
            // attributes are known from here on.
            w.queue("attributes_baseline", &json!({}));
            first = false;
        }
        let from_task = task;
        while w.events < EVENTS_PER_BUILD && events + w.events < target {
            if task % RUNS_PER_LIFE == 0 {
                next_life(&mut w, &mut life, &mut version);
            }
            task += 1;
            let now = supervisor(life);
            let opened = (task % RUNS_PER_SESSION == 0).then(|| {
                w.push(
                    None,
                    None,
                    "session_opened",
                    &json!({"kind": "runtime_planner", "planner_id": task, "supervisor": now}),
                );
                w.db.last_insert_rowid()
            });
            finished_run(&mut w, task, &now);
            queue_events(&mut w, &now, task, &mut version);
            if let Some(opened) = opened {
                w.push(
                    None,
                    None,
                    "session_closed",
                    &json!({"kind": "runtime_planner", "planner_id": task, "opened_event_id": opened}),
                );
            }
        }
        at = w.at;
        events += w.events;
        payload_bytes += w.payload_bytes;
        for (kind, (count, bytes)) in &w.kinds {
            let of_kind: &mut (usize, usize) = kinds.entry(*kind).or_default();
            of_kind.0 += count;
            of_kind.1 += bytes;
        }
        drop(w);
        tx.commit().unwrap();
        for done in from_task + 1..=task {
            let run = RunId::new(run_id(done)).unwrap();
            queue
                .record_steps(
                    &StepTarget::run(&run),
                    &format!("session-{done}"),
                    &run_steps(done),
                    at * 1000,
                )
                .unwrap();
        }
        while next_sample < at {
            queue
                .record_node_sample(
                    "host",
                    next_sample / 86_400,
                    &host_sample(next_sample),
                    &rate,
                    SampleSource::Sampled,
                    1,
                )
                .unwrap();
            next_sample += SAMPLE_EVERY_SECS;
        }
        passes += catch_up(&queue, &mut builder, at * 1000);
    }
    std::fs::write(dir.join("now"), at.to_string()).unwrap();
    drop(queue);
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    let bytes = queue_bytes(dir);
    let rows: i64 = db
        .query_row("SELECT count(*) FROM ledger_rows", [], |row| row.get(0))
        .unwrap();
    let mean = |bytes: usize, count: usize| bytes / count.max(1);
    println!(
        "{}",
        json!({
            "events": events,
            "runs": task,
            "lives": life,
            "rows": rows,
            "passes": passes,
            "mean_payload": mean(payload_bytes, events),
            "queue_db_bytes": bytes,
            "queue_db_bytes_per_event": bytes / events.max(1) as u64,
            "peak": peak(),
            "by_kind": kinds.iter().map(|(kind, (count, bytes))| {
                (kind.to_string(), json!({"count": count, "mean": mean(*bytes, *count)}))
            }).collect::<serde_json::Map<_, _>>(),
        })
    );
}

fn now_of(dir: &Path) -> i64 {
    std::fs::read_to_string(dir.join("now"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn areas() -> AreaMap {
    AreaMap::new(vec![
        ("runtime".to_owned(), vec!["src/**".to_owned()]),
        ("tests".to_owned(), vec!["tests/**".to_owned()]),
        ("docs".to_owned(), vec!["docs/**".to_owned()]),
    ])
    .unwrap()
}

fn host() -> Host {
    Host {
        utc_offset_secs: 32_400,
        cores: Some(8),
        rust_toolchain: true,
    }
}

/// A read's answer as one number: its JSON with the objects' keys sorted.
fn answer_digest(answer: &impl serde::Serialize) -> (u64, usize) {
    let text = serde_json::to_value(answer).unwrap().to_string();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    (hasher.finish(), text.len())
}

/// The folds of `read` over `window`, as `kpi` and `observer_kpi_read` make
/// them, with the setup they read beside it: the answer's value.
fn fold(window: &LedgerWindow, now: i64, read: &str, queue: &SqliteQueue) -> Value {
    let (config, areas) = (KpiConfig::default(), areas());
    let of = |query: &KpiQuery| {
        kpi::of_window(window, now, host(), &config, Some(&areas), query).unwrap()
    };
    match read {
        "kpi" => serde_json::to_value(of(&KpiQuery::default())).unwrap(),
        "observer" => {
            let at = |period, last| KpiQuery {
                period,
                last,
                ..KpiQuery::default()
            };
            let (day, week) = (of(&at(Period::Day, 7)), of(&at(Period::Week, 4)));
            observe::observer_input(&day, &week, &queue.kpi_breach_events_open().unwrap())
        }
        other => panic!("no read {other}"),
    }
}

/// The rows' text, every column of every row held at once: what a read
/// would hold if it read the rows before it made any.
fn read_text(db: &Connection) -> (usize, usize, Vec<Vec<String>>) {
    let mut held = Vec::new();
    let (mut total, mut largest) = (0, 0);
    let mut statement = db
        .prepare("SELECT * FROM ledger_rows WHERE kind=?1 ORDER BY key")
        .unwrap();
    for kind in LedgerKind::ALL {
        let columns = statement.column_count();
        let mut rows = statement.query([kind.as_str()]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let mut texts = Vec::new();
            let mut bytes = 0;
            for column in 0..columns {
                if let ValueRef::Text(text) = row.get_ref(column).unwrap() {
                    bytes += text.len();
                    texts.push(String::from_utf8_lossy(text).into_owned());
                }
            }
            total += bytes;
            largest = largest.max(bytes);
            held.push(texts);
        }
    }
    (total, largest, held)
}

/// The rows of a window, as `LedgerStore::ledger_window` reads them.
fn read_rows(queue: &SqliteQueue, query: &WindowQuery) -> Vec<LedgerRow> {
    let mut rows = Vec::new();
    for kind in LedgerKind::ALL {
        rows.extend(queue.ledger_rows(kind).unwrap());
    }
    for run in runs_on_their_way(&rows) {
        let events = queue.ledger_run_events(&run).unwrap();
        rows.extend(run_on_its_way(&run, &events, query.now_ms));
    }
    rows
}

/// How many rows the second prototype reads at a time.
const ROWS_PER_PAGE: usize = 50;

/// The second prototype, what a window needs of the rows as they are read:
/// each page's records are moved out and kept once, a queue row is dropped
/// (`kpi` and `report` read no queue row but its records) and a run row
/// loses `detail.steps` (read only by what rebuilds a row, never by a read
/// of a window). Rows and records in lists of the size they need.
fn read_lean(queue: &SqliteQueue, query: &WindowQuery) -> (Vec<RunEvent>, Vec<LedgerRow>) {
    let events = usize::try_from(queue.latest_event_id().unwrap().as_i64()).unwrap_or(0);
    let mut records: Vec<RunEvent> = Vec::with_capacity(events);
    let mut kept: Vec<LedgerRow> = Vec::new();
    let mut keep = |mut row: LedgerRow, kept: &mut Vec<LedgerRow>| {
        records.append(&mut row.records);
        row.records = Vec::new();
        if row.kind == LedgerKind::Queue {
            return;
        }
        if row.kind == LedgerKind::Run
            && let Some(detail) = row.detail.as_object_mut()
        {
            detail.remove(steps::DETAIL_KEY);
        }
        kept.push(row);
    };
    for kind in LedgerKind::ALL {
        let mut after: Option<String> = None;
        loop {
            let page = queue
                .ledger_rows_after(kind, after.as_deref(), ROWS_PER_PAGE)
                .unwrap();
            let Some(last) = page.last() else { break };
            after = Some(last.key.clone());
            for row in page {
                keep(row, &mut kept);
            }
        }
    }
    let mut on_their_way = Vec::new();
    for run in runs_on_their_way(&kept) {
        let events = queue.ledger_run_events(&run).unwrap();
        on_their_way.extend(run_on_its_way(&run, &events, query.now_ms));
    }
    for row in on_their_way {
        keep(row, &mut kept);
    }
    kept.shrink_to_fit();
    records.shrink_to_fit();
    (records, kept)
}

/// The window of `rows` of the candidate named: `none` is the runtime's.
fn window_of(candidate: &str, rows: Vec<LedgerRow>, query: &WindowQuery) -> LedgerWindow {
    match candidate {
        "none" => window::select(rows, query),
        "moved" => window::select_moving_records(rows, query),
        other => panic!("no candidate {other}"),
    }
}

/// What the stages (b) and (c) of the candidate named make.
enum Read {
    Rows(Vec<LedgerRow>),
    Lean(Vec<RunEvent>, Vec<LedgerRow>),
}

struct Layer {
    name: &'static str,
    peak: usize,
    carried: i64,
}

/// A stage: its peak over the bytes in use at its start, and what it left.
fn stage<T>(name: &'static str, layers: &mut Vec<Layer>, run: impl FnOnce() -> T) -> T {
    let start = in_use();
    reset_peak();
    let value = run();
    let peak = peak().saturating_sub(start);
    layers.push(Layer {
        name,
        peak,
        carried: in_use() as i64 - start as i64,
    });
    value
}

fn measure(dir: &Path, read: &str, candidate: &str) {
    let now = now_of(dir);
    let queue = SqliteQueue::open(dir.join("queue.db")).unwrap();
    let db = Connection::open(dir.join("queue.db")).unwrap();
    let query = WindowQuery {
        now_ms: dagq::application::stats::now_ms(now),
        ..WindowQuery::default()
    };
    let base = in_use();
    let mut layers = Vec::new();
    // PEAK stands from each stage's start, so the process's is the largest
    // of the stages'.
    let mut whole = 0;
    let mut seen = || whole = whole.max(peak().saturating_sub(base));
    let (text_bytes, largest_row) = stage("a_text", &mut layers, || {
        let (total, largest, held) = read_text(&db);
        drop(held);
        (total, largest)
    });
    seen();
    let read_of = stage("b_rows", &mut layers, || {
        if candidate == "lean" {
            let (records, rows) = read_lean(&queue, &query);
            Read::Lean(records, rows)
        } else {
            Read::Rows(read_rows(&queue, &query))
        }
    });
    seen();
    let row_count = match &read_of {
        Read::Rows(rows) | Read::Lean(_, rows) => rows.len(),
    };
    let window = stage("c_window", &mut layers, || match read_of {
        Read::Rows(rows) => window_of(candidate, rows, &query),
        Read::Lean(records, rows) => window::select_of_records(records, rows, &query),
    });
    seen();
    let answer = stage("d_folds", &mut layers, || fold(&window, now, read, &queue));
    seen();
    let events = window.events().len();
    let (digest, answer_bytes) = answer_digest(&answer);
    println!(
        "{}",
        json!({
            "mode": "measure",
            "read": read,
            "candidate": candidate,
            "events": events,
            "rows": row_count,
            "whole_peak": whole,
            "layers": layers.iter().map(|layer| json!({
                "layer": layer.name, "peak": layer.peak, "carried": layer.carried,
            })).collect::<Vec<_>>(),
            "text_bytes": text_bytes,
            "largest_row_text": largest_row,
            "answer_digest": digest.to_string(),
            "answer_bytes": answer_bytes,
            "max_rss": max_rss(),
        })
    );
}

/// The read as the runtime makes it, in one piece.
fn whole(dir: &Path, read: &str) {
    let now = now_of(dir);
    let queue = SqliteQueue::open(dir.join("queue.db")).unwrap();
    let base = in_use();
    reset_peak();
    let (config, areas) = (KpiConfig::default(), areas());
    let answer = match read {
        "kpi" => serde_json::to_value(
            kpi::kpi(
                &queue,
                now,
                host(),
                &config,
                Some(&areas),
                &KpiQuery::default(),
            )
            .unwrap(),
        )
        .unwrap(),
        _ => {
            let window = dagq::application::report::window(&queue, now).unwrap();
            fold(&window, now, read, &queue)
        }
    };
    let whole_peak = peak().saturating_sub(base);
    let (digest, answer_bytes) = answer_digest(&answer);
    println!(
        "{}",
        json!({
            "mode": "whole",
            "read": read,
            "whole_peak": whole_peak,
            "answer_digest": digest.to_string(),
            "answer_bytes": answer_bytes,
            "max_rss": max_rss(),
        })
    );
}

/// What a copy of `part` of every row takes, of the rows of `kind` (every
/// kind without), in a list of the length it needs.
fn copy_bytes<T>(
    rows: &[LedgerRow],
    kind: Option<LedgerKind>,
    part: impl Fn(&LedgerRow) -> T,
) -> usize {
    let chosen = rows
        .iter()
        .filter(|row| kind.is_none_or(|kind| row.kind == kind));
    let before = in_use();
    let mut copies: Vec<T> = Vec::with_capacity(chosen.clone().count());
    copies.extend(chosen.map(part));
    let bytes = in_use() - before;
    drop(copies);
    bytes
}

fn parts(dir: &Path) {
    let now = now_of(dir);
    let queue = SqliteQueue::open(dir.join("queue.db")).unwrap();
    let query = WindowQuery {
        now_ms: dagq::application::stats::now_ms(now),
        ..WindowQuery::default()
    };
    let base = in_use();
    let rows = read_rows(&queue, &query);
    let held = in_use() - base;
    let inline = std::mem::size_of::<LedgerRow>();
    let mut of_kinds = serde_json::Map::new();
    for kind in LedgerKind::ALL.into_iter().map(Some).chain([None]) {
        let name = kind.map_or("all", LedgerKind::as_str);
        let count = rows
            .iter()
            .filter(|row| kind.is_none_or(|kind| row.kind == kind))
            .count();
        // Each copy is one element of a list, of its inline size too.
        let of = |part: &dyn Fn(&LedgerRow) -> Box<dyn std::any::Any>| {
            copy_bytes(&rows, kind, part)
                .saturating_sub(count * std::mem::size_of::<Box<dyn std::any::Any>>())
        };
        of_kinds.insert(
            name.to_owned(),
            json!({
                "rows": count,
                "records_count": rows.iter()
                    .filter(|row| kind.is_none_or(|kind| row.kind == kind))
                    .map(|row| row.records.len()).sum::<usize>(),
                "records": of(&|row| Box::new(row.records.clone())),
                "totals": of(&|row| Box::new(row.totals.clone())),
                "intervals": of(&|row| Box::new(row.intervals.clone())),
                "attributes": of(&|row| Box::new(row.attributes.clone())),
                "detail": of(&|row| Box::new(row.detail.clone())),
                "legacy": of(&|row| Box::new(row.legacy.clone())),
                "strings": of(&|row| Box::new((row.key.clone(), row.built_at.clone(), row.outcome.clone(), row.subject.clone()))),
            }),
        );
    }
    println!(
        "{}",
        json!({
            "mode": "parts",
            "rows_held": held,
            "row_inline_size": inline,
            "event_inline_size": std::mem::size_of::<dagq::domain::RunEvent>(),
            "by_kind": of_kinds,
        })
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |at: usize| args[at].as_str();
    match args.get(1).map(String::as_str) {
        Some("prepare") if args.len() == 4 => {
            prepare(Path::new(arg(2)), arg(3).parse().unwrap());
        }
        Some("measure") if args.len() == 4 => measure(Path::new(arg(2)), arg(3), "none"),
        Some("measure") if args.len() == 5 => measure(Path::new(arg(2)), arg(3), arg(4)),
        Some("whole") if args.len() == 4 => whole(Path::new(arg(2)), arg(3)),
        Some("parts") if args.len() == 3 => parts(Path::new(arg(2))),
        _ => {
            eprintln!(
                "usage: kpi_read_memory prepare <dir> <events> | measure <dir> <kpi|observer> [<candidate>] | whole <dir> <kpi|observer> | parts <dir>"
            );
            std::process::exit(2);
        }
    }
}
