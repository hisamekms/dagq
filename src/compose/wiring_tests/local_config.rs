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
            let read = operational();
            let rules = read.rules.as_ref().unwrap();
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
            read
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
        let read = file();
        if read
            .rules
            .as_ref()
            .is_ok_and(|rules| rules.stall.idle_without_receipt_secs == 200)
        {
            observed.store(true, Ordering::SeqCst);
        }
        read
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

/// The real loop records `supervisor_local_config_changed` once for each
/// change of the main checkout's layer (it begins, changes, a committed
/// value under it changes, it expires, is rejected, is removed) and never
/// for a pass that read the same layer, with the values in use.
#[test]
fn each_change_of_the_local_layer_is_recorded_once_by_the_pass() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".git/config"), "[core]\nbare = false").unwrap();
    let base = dir.path().join("dagq.toml");
    // Swapped in by a rename, so a pass never reads half a file.
    let put = |path: &std::path::Path, text: &str| {
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, text).unwrap();
        fs::rename(&tmp, path).unwrap();
    };
    put(&base, "[stall]\nidle_without_receipt_secs = 100\n");
    let wiring = Wiring::new(
        "each_change_of_the_local_layer_is_recorded_once_by_the_pass",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
    .with_tick(std::time::Duration::from_millis(1));
    let clock = Arc::new(ConfigClock {
        base: wiring.generators().clock,
        seconds: AtomicU64::new(0),
    });
    let operational =
        crate::compose::operational_file(dir.path().to_owned(), clock.clone(), Default::default());
    let passes = Arc::new(AtomicU64::new(0));
    let counted = passes.clone();
    let wiring = wiring.with_operational_file(Arc::new(move || {
        let read = operational();
        counted.fetch_add(1, Ordering::SeqCst);
        read
    }));
    let local = dir.path().join("dagq.local.toml");
    let layer_text = |secs: u64| {
        format!(
            "[local]\nexpires_at = '{}'\n[stall]\nidle_without_receipt_secs = {secs}\n",
            millis_text(clock.now() * 1000 + 86_400_000)
        )
    };
    // Each stage: the change it expects recorded, then what it changes.
    let expected = [
        None,
        Some("effective"),
        Some("changed"),
        Some("committed_changed"),
        Some("expired"),
        Some("rejected"),
        Some("removed"),
    ];
    let mut stage = 0;
    let mut since = 0;
    let mut recorded = Vec::new();
    wiring.supervise_once_while_stoppable(|queue, stop| {
        let events = queue
            .events_of_between(
                &[EventKind::SupervisorLocalConfigChanged.as_str()],
                crate::domain::EventId::new(0),
                crate::domain::EventId::new(i64::MAX),
                100,
            )
            .unwrap();
        let pass = passes.load(Ordering::SeqCst);
        // Two whole passes after the change read the same layer again.
        if events.len() != stage || pass < since + 3 {
            assert!(events.len() <= stage, "a pass recorded again: {events:?}");
            return false;
        }
        if let Some(change) = expected[stage] {
            assert_eq!(events[stage - 1].payload["change"], change);
        }
        match stage {
            0 => put(&local, &layer_text(200)),
            1 => put(&local, &layer_text(300)),
            2 => put(&base, "[stall]\nidle_without_receipt_secs = 150\n"),
            3 => clock.seconds.store(86_400, Ordering::SeqCst),
            4 => put(
                &local,
                &format!("{}[repository]\npush = false\n", layer_text(300)),
            ),
            5 => fs::remove_file(&local).unwrap(),
            _ => {
                recorded = events.iter().map(|event| event.payload.clone()).collect();
                stop();
                return true;
            }
        }
        since = passes.load(Ordering::SeqCst);
        stage += 1;
        false
    });
    assert_eq!(recorded.len(), 6);
    let effective = &recorded[0];
    assert_eq!(effective["keys"][0]["key"], "idle_without_receipt_secs");
    assert_eq!(effective["keys"][0]["local"], "200");
    assert_eq!(effective["keys"][0]["committed"], "100");
    assert!(effective["hash"].is_string());
    assert!(effective["expires_at"].is_string());
    assert!(effective["supervisor"].is_string());
    // The value in use changed with the layer, in the same record.
    assert_eq!(
        effective["in_use"]["stall"]["idle_without_receipt_secs"],
        200
    );
    assert_eq!(
        recorded[1]["in_use"]["stall"]["idle_without_receipt_secs"],
        300
    );
    assert_eq!(
        recorded[2]["committed_changes"][0],
        serde_json::json!({"table": "stall", "key": "idle_without_receipt_secs", "from": "100", "to": "150"})
    );
    assert_eq!(
        recorded[3]["in_use"]["stall"]["idle_without_receipt_secs"],
        150
    );
    assert_eq!(recorded[4]["reason"], "local table not allowed: repository");
    assert_eq!(recorded[5]["state"], "absent");
}

/// A record of the layer whose answer is lost is kept and sent again
/// under its identity by the resending caller, and the pass does not
/// record the same change again: the queue has it once.
#[test]
fn a_change_whose_answer_is_lost_is_recorded_once() {
    use crate::application::supervisor_service::{ServiceCaller, ServiceUnreachable};
    use crate::domain::queue_service::UseCase;
    use serde_json::Value;
    use std::path::{Path, PathBuf};
    struct LoseOnce {
        inner: Arc<dyn ServiceCaller>,
        armed: Arc<AtomicBool>,
    }
    impl ServiceCaller for LoseOnce {
        fn call(&self, case: UseCase, params: Value) -> anyhow::Result<Value> {
            let ours = params["op"] == "record_queue_event"
                && params["args"][0] == EventKind::SupervisorLocalConfigChanged.as_str();
            if ours && self.armed.swap(false, Ordering::SeqCst) {
                // Written, but its answer never comes back.
                self.inner.call(case, params)?;
                return Err(anyhow::Error::new(ServiceUnreachable::maybe_done(
                    "the answer was lost",
                )));
            }
            self.inner.call(case, params)
        }
        fn with_token(&self, p: &Path) -> anyhow::Result<Arc<dyn ServiceCaller>> {
            self.inner.with_token(p)
        }
        fn sign_in(&self, t: &crate::domain::LeaseToken, p: u32) -> anyhow::Result<()> {
            self.inner.sign_in(t, p)
        }
        fn take_back(&self, t: &crate::domain::LeaseToken, p: u32) -> anyhow::Result<()> {
            self.inner.take_back(t, p)
        }
        fn hand_over(&self) {
            self.inner.hand_over();
        }
        fn sign_out(&self) {
            self.inner.sign_out();
        }
        fn credential(&self) -> Option<PathBuf> {
            self.inner.credential()
        }
    }
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".git/config"), "[core]\nbare = false").unwrap();
    fs::write(
        dir.path().join("dagq.toml"),
        "[stall]\nidle_without_receipt_secs = 100\n",
    )
    .unwrap();
    let wiring = Wiring::new(
        "a_change_whose_answer_is_lost_is_recorded_once",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
    .with_tick(std::time::Duration::from_millis(1));
    let clock = wiring.generators().clock;
    fs::write(
        dir.path().join("dagq.local.toml"),
        format!(
            "[local]\nexpires_at = '{}'\n[stall]\nidle_without_receipt_secs = 200\n",
            millis_text(clock.now() * 1000 + 86_400_000)
        ),
    )
    .unwrap();
    let operational =
        crate::compose::operational_file(dir.path().to_owned(), clock, Default::default());
    let passes = Arc::new(AtomicU64::new(0));
    let counted = passes.clone();
    let armed = Arc::new(AtomicBool::new(true));
    let fault = armed.clone();
    let wiring = wiring
        .with_operational_file(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            operational()
        }))
        .with_service_wrap(Arc::new(move |inner| {
            Arc::new(LoseOnce {
                inner,
                armed: fault.clone(),
            })
        }));
    let mut seen = 0;
    wiring.supervise_once_while_stoppable(|queue, stop| {
        if armed.load(Ordering::SeqCst) || passes.load(Ordering::SeqCst) < 6 {
            return false;
        }
        seen = queue
            .events_of_between(
                &[EventKind::SupervisorLocalConfigChanged.as_str()],
                crate::domain::EventId::new(0),
                crate::domain::EventId::new(i64::MAX),
                100,
            )
            .unwrap()
            .len();
        stop();
        true
    });
    assert!(!armed.load(Ordering::SeqCst), "the answer was never lost");
    assert_eq!(seen, 1);
}

/// A layer whose record would pass the queue service's request limit
/// (a value with a comment of over a MiB, then a rejected table whose name
/// is two MiB of control characters) is recorded as one event each by the
/// real loop, through the in-process service that refuses a request past
/// [`crate::domain::queue_service::MAX_REQUEST_BYTES`] as the socket does,
/// and the passes go on.
#[test]
fn a_layer_of_any_size_is_recorded_as_one_event() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".git/config"), "[core]\nbare = false").unwrap();
    fs::write(
        dir.path().join("dagq.toml"),
        "[stall]\nidle_without_receipt_secs = 100\n",
    )
    .unwrap();
    let put = |path: &std::path::Path, text: &str| {
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, text).unwrap();
        fs::rename(&tmp, path).unwrap();
    };
    let wiring = Wiring::new(
        "a_layer_of_any_size_is_recorded_as_one_event",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
    .with_tick(std::time::Duration::from_millis(1));
    let clock = wiring.generators().clock;
    let expires = millis_text(clock.now() * 1000 + 86_400_000);
    let local = dir.path().join("dagq.local.toml");
    let comment = "#".repeat(1_100 * 1024);
    let effective = format!(
        "[local]\nexpires_at = '{expires}'\n[stall]\nidle_without_receipt_secs = 200 {comment}\n"
    );
    let rejected = format!(
        "[local]\nexpires_at = '{expires}'\n[{}]\nx = 1\n",
        "\u{1}".repeat(2 * 1024 * 1024)
    );
    put(&local, &effective);
    let operational =
        crate::compose::operational_file(dir.path().to_owned(), clock, Default::default());
    let passes = Arc::new(AtomicU64::new(0));
    let counted = passes.clone();
    let wiring = wiring.with_operational_file(Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        operational()
    }));
    let mut stage = 0;
    let mut since = 0;
    let mut recorded = Vec::new();
    wiring.supervise_once_while_stoppable(|queue, stop| {
        let events = queue
            .events_of_between(
                &[EventKind::SupervisorLocalConfigChanged.as_str()],
                crate::domain::EventId::new(0),
                crate::domain::EventId::new(i64::MAX),
                100,
            )
            .unwrap();
        // Two whole passes after each record read the same layer again.
        if events.len() != stage + 1 || passes.load(Ordering::SeqCst) < since + 3 {
            assert!(events.len() <= stage + 1, "a pass recorded again");
            return false;
        }
        if stage == 0 {
            put(&local, &rejected);
            since = passes.load(Ordering::SeqCst);
            stage = 1;
            return false;
        }
        recorded = events.iter().map(|event| event.payload.clone()).collect();
        stop();
        true
    });
    assert_eq!(recorded.len(), 2);
    for record in &recorded {
        let size = serde_json::to_vec(record).unwrap().len();
        assert!(
            size <= crate::domain::local_config::MAX_RECORD_BYTES,
            "{size}"
        );
    }
    assert_eq!(recorded[0]["change"], "effective");
    assert_eq!(recorded[0]["keys"][0]["local_text"], "200");
    assert_eq!(
        recorded[0]["in_use"]["stall"]["idle_without_receipt_secs"],
        200
    );
    assert_eq!(recorded[1]["change"], "rejected");
    assert!(
        recorded[1]["reason"]
            .as_str()
            .unwrap()
            .starts_with("local table not allowed: ")
    );
}
