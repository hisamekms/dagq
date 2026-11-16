//! Main-checkout files drive the real loop across pass boundaries. Git and
//! workers stay harness fakes; file reads and the compose ports are real.
use super::{agent::Script, harness::Wiring};
use crate::{
    application::{Clock, TaskReads},
    domain::{EventKind, transcript::millis_text},
};
use std::{
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
};

/// Advance only the config readers' clock; the loop's leases and sessions
/// keep the harness clock, so expiry needs neither a sleep nor a restart.
struct ConfigClock {
    base: Arc<dyn Clock>,
    seconds: AtomicU64,
}
impl Clock for ConfigClock {
    fn system_time(&self) -> std::time::SystemTime {
        self.base.system_time()
            + std::time::Duration::from_secs(self.seconds.load(Ordering::SeqCst))
    }
    fn monotonic(&self) -> std::time::Instant {
        self.base.monotonic() + std::time::Duration::from_secs(self.seconds.load(Ordering::SeqCst))
    }
}

#[test]
fn local_operational_files_change_claims_and_reset_each_pass() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".git/config"), "[core]\nbare = false").unwrap();
    fs::write(dir.path().join("dagq.toml"), "[disk]\nreserve = '5%'\n[stall]\nidle_without_receipt_secs = 100\n[resume]\nconflict_only_limit = 3\n[supervisor]\nmax_waiting = 2\n").unwrap();
    let wiring = Wiring::new(
        "local_operational_files_change_claims_and_reset_each_pass",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
    .with_volume(100 << 30)
    .with_tick(std::time::Duration::from_millis(1));
    wiring.free.store(10 << 30, Ordering::SeqCst);
    let clock = Arc::new(ConfigClock {
        base: wiring.generators().clock,
        seconds: AtomicU64::new(0),
    });
    let operational =
        crate::compose::operational_file(dir.path().to_owned(), clock.clone(), Default::default());
    let supervisor = crate::compose::supervisor_file(dir.path().to_owned(), clock.clone());
    let (arrived, arrival) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let released = Mutex::new(released);
    let counter = Mutex::new(0);
    let active = Arc::new(AtomicBool::new(false));
    let expected = active.clone();
    let ending = Arc::new(AtomicBool::new(false));
    let ended = ending.clone();
    let wiring = wiring
        .with_supervisor_file(supervisor)
        .with_operational_file(Arc::new(move || {
            if ended.load(Ordering::SeqCst) {
                return operational();
            }
            let mut pass = counter.lock().unwrap();
            arrived.send(*pass).unwrap();
            released
                .lock()
                .unwrap()
                .recv_timeout(super::harness::LIMIT)
                .unwrap();
            let rules = operational()?;
            assert_eq!(
                rules.stall.idle_without_receipt_secs,
                if expected.load(Ordering::SeqCst) {
                    200
                } else {
                    100
                }
            );
            assert_eq!(
                rules.resume.conflict_only_limit,
                if expected.load(Ordering::SeqCst) {
                    7
                } else {
                    3
                }
            );
            *pass += 1;
            Ok(rules)
        }));
    let local = dir.path().join("dagq.local.toml");
    let valid = || {
        format!(
            "[local]\nexpires_at = '{}'\n[disk]\nreserve = '20%'\n[stall]\nidle_without_receipt_secs = 200\n[resume]\nconflict_only_limit = 7\n[supervisor]\nmax_waiting = 3\n",
            millis_text(clock.now() * 1000 + 86_400_000)
        )
    };
    let mut stage = 0;
    let mut previous_active = false;
    wiring.supervise_once_while_stoppable(|queue, stop| {
        let Ok(pass) = arrival.try_recv() else {
            return false;
        };
        assert!(pass < 100, "the disk hold did not settle at stage {stage}");
        if pass > 0 {
            let slots = queue.supervisors().unwrap();
            assert_eq!(
                slots[0].max_waiting,
                Some(if previous_active { 3 } else { 2 })
            );
        }
        let last = queue
            .latest_queue_event(&[
                EventKind::ClaimHeld.as_str(),
                EventKind::ClaimResumed.as_str(),
            ])
            .unwrap();
        // Disk cleanup can take a pass before the hold is recorded. Keep the
        // file stable until the real claim decision, rather than racing that
        // job or assuming how many passes it takes to finish.
        let transition = match stage {
            0 => pass > 0,
            1 | 3 | 5 => last
                .as_ref()
                .is_some_and(|event| event.kind == EventKind::ClaimHeld),
            _ => last
                .as_ref()
                .is_some_and(|event| event.kind == EventKind::ClaimResumed),
        };
        if transition {
            if stage % 2 == 1 {
                let event = last.as_ref().unwrap();
                assert_eq!(event.payload["disk"]["reserve"], 20_u64 << 30);
                assert_eq!(event.payload["disk"]["reserve_share"], 0.2);
            }
            match stage {
                0 | 2 | 4 => {
                    fs::write(&local, valid()).unwrap();
                    active.store(true, Ordering::SeqCst);
                }
                1 => {
                    // The file is untouched: only wall time crosses its
                    // expiry, and the next pass must fall back to the base.
                    clock.seconds.store(86_400, Ordering::SeqCst);
                    active.store(false, Ordering::SeqCst);
                }
                3 => {
                    fs::write(&local, format!("{}[repository]\npush = false\n", valid())).unwrap();
                    active.store(false, Ordering::SeqCst);
                }
                5 => {
                    fs::remove_file(&local).unwrap();
                    active.store(false, Ordering::SeqCst);
                }
                _ => {
                    ending.store(true, Ordering::SeqCst);
                    stop();
                }
            }
            stage += 1;
        }
        previous_active = active.load(Ordering::SeqCst);
        release.send(()).unwrap();
        stage == 7
    });
    assert_eq!(stage, 7);
}

#[test]
fn a_running_session_uses_the_reloaded_stall_setting() {
    use super::agent::{no_receipt, succeeded, verdict};
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".git/config"), "[core]\nbare = false").unwrap();
    fs::write(
        dir.path().join("dagq.toml"),
        "[stall]\nidle_without_receipt_secs = 100\n",
    )
    .unwrap();
    let wiring = Wiring::new(
        "a_running_session_uses_the_reloaded_stall_setting",
        Script {
            receipts: vec![no_receipt, succeeded],
            verdicts: vec![verdict("pass", "complete")],
        },
    )
    .with_tick(std::time::Duration::from_millis(1));
    let clock = wiring.generators().clock;
    let file =
        crate::compose::operational_file(dir.path().to_owned(), clock.clone(), Default::default());
    let loaded = Arc::new(AtomicBool::new(false));
    let observed = loaded.clone();
    let wiring = wiring.with_operational_file(Arc::new(move || {
        let rules = file()?;
        if rules.stall.idle_without_receipt_secs == 200 {
            observed.store(true, Ordering::SeqCst);
        }
        Ok(rules)
    }));
    let (release_turn, released_turn) = mpsc::sync_channel(1);
    wiring.agent.hold_next_turn(released_turn);
    let mut release_turn = Some(release_turn);
    let local = dir.path().join("dagq.local.toml");
    let expiry = millis_text(clock.now() * 1000 + 86_400_000);
    // Written only once the worker's first turn starts: the supervisor has
    // already read its startup/pass settings. Its next pass must replace
    // the actual stall field used to report the session's nudge.
    wiring.agent.on_next_turn(Box::new(move || {
        fs::write(
            &local,
            format!("[local]\nexpires_at = '{expiry}'\n[stall]\nidle_without_receipt_secs = 200\n"),
        )
        .unwrap();
    }));
    let task = wiring.ready_task(&[]);
    let mut nudged = false;
    wiring.supervise_once_while_stoppable(|queue, stop| {
        if loaded.load(Ordering::SeqCst)
            && let Some(release) = release_turn.take()
        {
            release.send(()).unwrap();
        }
        let detail = queue.show(task).unwrap();
        let Some(run) = detail.runs.first() else {
            return false;
        };
        let events = queue.run_events(run.id()).unwrap();
        let Some(event) = events
            .iter()
            .find(|event| event.kind == EventKind::StallNudged)
        else {
            return false;
        };
        assert_eq!(event.payload["threshold_secs"], 200);
        nudged = true;
        stop();
        true
    });
    assert!(nudged);
}
