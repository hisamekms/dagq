//! The peaks of a run's WORKER and INTEGRATOR disk stages wired from the
//! off-loop measures to their `run_stage_peak` records, whatever the
//! `[disk] estimate` method. When a stage is left and how its measures
//! count are the unit tests of `domain::disk::StagePeaks`.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;

use super::{
    agent::{Script, succeeded, verdict},
    harness::Wiring,
};
use crate::{application::TaskReads, domain::RunStatus};

const GIB: u64 = 1 << 30;
/// How long the test waits for the landing to be let go on before the
/// verification goes on by itself.
const LIMIT: Duration = Duration::from_secs(60);
/// One walk interval and a second: the next pass walks the moving runs.
const STEP: u64 = crate::domain::disk::MOVING_SIZE_INTERVAL_SECS + 1;

/// The test's stopped clock, its monotonic time moved by `seconds`.
struct Moved {
    base: Arc<dyn crate::application::Clock>,
    seconds: Arc<AtomicU64>,
}

impl crate::application::Clock for Moved {
    fn system_time(&self) -> SystemTime {
        self.base.system_time()
    }
    fn monotonic(&self) -> Instant {
        self.base.monotonic() + Duration::from_secs(self.seconds.load(Ordering::SeqCst))
    }
}

/// A run measured 8, 12 and 11 GiB in its WORKER stage (its turn held),
/// then 14 GiB in its INTEGRATOR stage (its verification held), each
/// measure started by moving the clock a walk interval on; the
/// `run_stage_peak` payloads it left, with its run's id.
fn peaks_of_one_run(name: &str, disk: Option<&'static str>) -> (Vec<Value>, Value) {
    let seconds = Arc::new(AtomicU64::new(0));
    let mut wiring = Wiring::new(
        name,
        Script {
            receipts: vec![succeeded],
            verdicts: vec![verdict("pass", "meets the acceptance")],
        },
    )
    .with_tick(Duration::from_millis(1));
    let base = wiring.generators().clock;
    wiring = wiring.with_clock(Arc::new(Moved {
        base,
        seconds: seconds.clone(),
    }));
    if let Some(disk) = disk {
        wiring = wiring.with_repository_startup_file(Arc::new(move || {
            let config = crate::infrastructure::run_env::parse_config(disk)?;
            Ok(
                crate::application::supervise::repository_config::StartupRules {
                    stall: config.stall,
                    disk: config.disk,
                    resume: config.resume,
                    review_jobs: config.review_jobs,
                },
            )
        }));
    }
    wiring.free.store(100 * GIB, Ordering::SeqCst);
    let size = Arc::new(AtomicU64::new(8 * GIB));
    let walks = Arc::new(AtomicUsize::new(0));
    wiring.moving_sizes = {
        let (size, walks) = (size.clone(), walks.clone());
        Arc::new(move |path: &std::path::Path| {
            if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("worktree"))
            {
                walks.fetch_add(1, Ordering::SeqCst);
                Ok(Some(size.load(Ordering::SeqCst)))
            } else {
                Ok(None)
            }
        })
    };
    let (release_turn, turn) = std::sync::mpsc::channel();
    wiring.agent.hold_next_turn(turn);
    let landing = Arc::new(AtomicBool::new(false));
    let (release_landing, verification) = std::sync::mpsc::channel::<()>();
    {
        let landing = landing.clone();
        wiring.verifier.on_command(
            "check",
            Box::new(move || {
                landing.store(true, Ordering::SeqCst);
                let _ = verification.recv_timeout(LIMIT);
            }),
        );
    }
    let task = wiring.ready_task(&["check"]);
    let peaks = |queue: &crate::infrastructure::sqlite::SqliteQueue| -> Vec<Value> {
        queue
            .all_events()
            .unwrap()
            .into_iter()
            .filter(|event| event.kind.as_str() == "run_stage_peak")
            .map(|event| event.payload)
            .collect()
    };
    // Each step: the walks it waits for, the size of the next walk, and
    // what it lets go on after a few more passes read that walk.
    let mut step = 0;
    let mut read_from = None;
    wiring.supervise_once_while_stoppable(|queue, stop| {
        let wanted = match step {
            0..=2 => step + 1,
            3 => {
                if !landing.load(Ordering::SeqCst) || peaks(queue).is_empty() {
                    return false;
                }
                size.store(14 * GIB, Ordering::SeqCst);
                seconds.fetch_add(STEP, Ordering::SeqCst);
                step = 4;
                return false;
            }
            4 => 4,
            _ => {
                if queue.show(task).unwrap().runs[0].status() != RunStatus::Integrated
                    || peaks(queue).len() < 2
                {
                    return false;
                }
                stop();
                return true;
            }
        };
        if walks.load(Ordering::SeqCst) < wanted {
            return false;
        }
        let passes = *read_from.get_or_insert(wiring.passes());
        if wiring.passes() < passes + 3 {
            return false;
        }
        read_from = None;
        match step {
            0 => size.store(12 * GIB, Ordering::SeqCst),
            1 => size.store(11 * GIB, Ordering::SeqCst),
            2 => {
                assert!(peaks(queue).is_empty(), "no stage left yet");
                release_turn.send(()).unwrap();
                step = 3;
                return false;
            }
            _ => {
                release_landing.send(()).unwrap();
                step = 5;
                return false;
            }
        }
        seconds.fetch_add(STEP, Ordering::SeqCst);
        step += 1;
        false
    });
    let mut queue = wiring.queue();
    let run = queue.show(task).unwrap().runs[0].id().clone();
    (peaks(&queue), serde_json::json!(run))
}

/// Acceptance 2 of task 5065, recent (the default) method: one
/// `run_stage_peak` of the WORKER stage at its receipt with the largest of
/// its 8, 12 and 11 GiB, and one of the INTEGRATOR stage at the end of
/// the landing's verification.
#[test]
fn a_runs_worker_and_integrator_peaks_are_recorded_as_each_stage_ends() {
    let (peaks, run) = peaks_of_one_run(
        "a_runs_worker_and_integrator_peaks_are_recorded_as_each_stage_ends",
        None,
    );
    assert_peaks(&peaks, &run, "recent");
}

/// The fixed method records the same peaks.
#[test]
fn the_fixed_method_records_the_stage_peaks_too() {
    let (peaks, run) = peaks_of_one_run(
        "the_fixed_method_records_the_stage_peaks_too",
        Some("[disk]\nestimate = \"fixed\"\nrun_size = \"20GiB\"\n"),
    );
    assert_peaks(&peaks, &run, "fixed");
}

fn assert_peaks(peaks: &[Value], run: &Value, estimate: &str) {
    assert_eq!(peaks.len(), 2, "{peaks:?}");
    let field = |at: usize, key: &str| peaks[at][key].clone();
    assert_eq!(field(0, "run_id"), *run);
    assert_eq!(field(0, "phase"), "worker");
    assert_eq!(field(0, "peak_bytes"), 12 * GIB);
    assert_eq!(field(0, "measures"), 3);
    assert_eq!(field(0, "estimate"), estimate);
    assert!(field(0, "task_id").is_number());
    assert_eq!(field(1, "run_id"), *run);
    assert_eq!(field(1, "phase"), "integrator");
    assert_eq!(field(1, "peak_bytes"), 14 * GIB);
    // The 11 GiB held as the stage began and the 14 GiB walked in it.
    assert_eq!(field(1, "measures"), 2);
    assert_eq!(field(1, "estimate"), estimate);
}
