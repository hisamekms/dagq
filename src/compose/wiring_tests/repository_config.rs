//! A deployment's unread first rules hold the actual claim and reach status.
use super::{
    agent::Script,
    harness::{LIMIT, Wiring},
};
use crate::application::{
    TaskReads,
    supervise::{PausePoint, repository_config::ConfigUnavailable},
};
use std::sync::{Arc, Mutex, mpsc};

#[test]
fn unread_first_rules_hold_claims_and_publish_a_safe_status_reason() {
    let (arrived, arrival) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let released = Mutex::new(released);
    let wiring = Wiring::new(
        "unread_first_rules_hold_claims_and_publish_a_safe_status_reason",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    )
    .with_supervisor_file(Arc::new(|| Err(ConfigUnavailable.into())))
    .with_pauses(Arc::new(move |point, _| {
        if point == PausePoint::LoopEnded {
            arrived.send(()).unwrap();
            released.lock().unwrap().recv_timeout(LIMIT).unwrap();
        }
    }));
    let task = wiring.ready_task(&["check"]);
    let mut checked = false;
    wiring.supervise_once_while(|queue| {
        if arrival.try_recv().is_err() {
            return false;
        }
        assert!(queue.show(task).unwrap().runs.is_empty());
        let status = wiring.status();
        assert!(
            status.to_string().contains("repository_config_unavailable"),
            "{status}"
        );
        let held = queue
            .latest_queue_event(&["repository_config_held"])
            .unwrap()
            .unwrap();
        assert_eq!(held.payload["reason"], "repository_config_unavailable");
        assert!(!held.payload.to_string().contains("secret"));
        checked = true;
        release.send(()).unwrap();
        true
    });
    assert!(checked);
}

use crate::application::Clock;
use crate::application::supervise::repository_config::{
    AppToken, InstallationTokens, MainRules, RemoteRules, StartupOverrides,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

struct RulesApi(Arc<AtomicBool>);
impl InstallationTokens for RulesApi {
    fn issue(&self, _: &serde_json::Value, _: i64) -> Result<AppToken, ConfigUnavailable> {
        self.0
            .load(Ordering::SeqCst)
            .then(|| AppToken("placeholder".into()))
            .ok_or(ConfigUnavailable)
    }
}
impl MainRules for RulesApi {
    fn main_sha(&self, _: &AppToken) -> Result<String, ConfigUnavailable> {
        Ok("a".repeat(40))
    }
    fn contents(&self, _: &AppToken, _: &str) -> Result<String, ConfigUnavailable> {
        Ok("[stall]\nidle_without_receipt_secs = 713\n[disk]\nreserve = '12%'\n[resume]\nconflict_only_limit = 7\n[review.jobs]\nconcurrency = 3\nagent_timeout_secs = 123\nprogram_timeout_secs = 234\n".into())
    }
}
struct RulesClock {
    seconds: AtomicU64,
    start: Instant,
}
impl Clock for RulesClock {
    fn system_time(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(self.seconds.load(Ordering::SeqCst))
    }
    fn monotonic(&self) -> Instant {
        self.start
    }
}
fn rules(
    available: Arc<AtomicBool>,
    clock: Arc<RulesClock>,
) -> Arc<RemoteRules<crate::infrastructure::run_env::Config>> {
    Arc::new(RemoteRules::new(
        Box::new(RulesApi(available)),
        "repo".into(),
        clock,
        |text| crate::infrastructure::run_env::parse_config(text).map_err(|_| ConfigUnavailable),
    ))
}

#[test]
fn first_success_installs_all_startup_tables_before_claiming() {
    let available = Arc::new(AtomicBool::new(false));
    let clock = Arc::new(RulesClock {
        seconds: AtomicU64::new(100),
        start: Instant::now(),
    });
    let source = rules(available.clone(), clock.clone());
    // Composition's first request failed. The running process must recover.
    assert!(source.read().is_err());
    let supervisor = source.clone();
    let w = Wiring::new(
        "startup-rules-retry",
        Script {
            receipts: vec![super::agent::succeeded],
            verdicts: vec![super::agent::verdict("pass", "done")],
        },
    )
    .with_supervisor_file(Arc::new(move || {
        supervisor.read().map(|r| Some(r.supervisor))
    }))
    .with_repository_startup_file(crate::infrastructure::github_app::repository_startup_file(
        source,
        StartupOverrides::default(),
    ));
    let task = w.ready_task(&["check"]);
    let mut recovered = false;
    w.supervise_once_while_stoppable(|q, stop| {
        if q.latest_queue_event(&["repository_config_held"])
            .unwrap()
            .is_none()
        {
            return false;
        }
        if !available.load(Ordering::SeqCst) {
            assert!(q.show(task).unwrap().runs.is_empty());
            assert!(
                w.status()
                    .to_string()
                    .contains("repository_config_unavailable")
            );
            available.store(true, Ordering::SeqCst);
            clock.seconds.store(130, Ordering::SeqCst);
            return false;
        }
        let Some(applied) = q
            .latest_queue_event(&["supervisor_config_changed"])
            .unwrap()
        else {
            return false;
        };
        let Some(config) = applied.payload.get("repository_startup") else {
            return false;
        };
        assert_eq!(config["stall"]["idle_without_receipt_secs"], 713);
        assert_eq!(config["disk"]["reserve"], 0.12);
        assert_eq!(config["resume"]["conflict_only_limit"], 7);
        assert_eq!(config["review_jobs"]["concurrency"], 3);
        assert_eq!(config["review_jobs"]["agent_timeout_secs"], 123);
        assert_eq!(config["review_jobs"]["program_timeout_secs"], 234);
        let Some(claim) = q.latest_event_of("run_claimed").unwrap() else {
            return false;
        };
        assert!(claim.id > applied.id);
        recovered = true;
        stop();
        true
    });
    assert!(recovered);
}

#[test]
fn independent_snapshots_keep_a_missing_supervisors_hold_visible() {
    use crate::domain::{
        EventKind, LeaseToken,
        claim_hold::{OwnHold, REPOSITORY_CONFIG_HOLD as HOLD},
    };
    let w = Wiring::new(
        "independent-rules",
        Script {
            receipts: vec![],
            verdicts: vec![],
        },
    );
    let clock = Arc::new(RulesClock {
        seconds: AtomicU64::new(100),
        start: Instant::now(),
    });
    let available = Arc::new(AtomicBool::new(true));
    let good = rules(available.clone(), clock.clone());
    assert!(good.read().is_ok());
    available.store(false, Ordering::SeqCst);
    clock.seconds.store(130, Ordering::SeqCst);
    let missing = rules(available, clock);
    assert!(missing.read().is_err());
    assert!(good.read().is_ok());
    let mut q = w.queue();
    let a = LeaseToken::new("rules-a");
    let b = LeaseToken::new("rules-b");
    for token in [&a, &b] {
        q.register_supervisor(token, std::process::id(), 1, "test")
            .unwrap();
    }
    let (kind, payload) = HOLD
        .transition(
            Some((
                "repository_config_unavailable",
                serde_json::json!({"message":"main dagq.toml unread"}),
            )),
            None,
            &a,
        )
        .unwrap();
    q.record_queue_event(kind, payload).unwrap();
    // B's successful cache and ordinary queue-wide hold pass cannot close A.
    q.record_queue_event(EventKind::ClaimResumed, serde_json::json!({"supervisor":b}))
        .unwrap();
    let records = q
        .latest_events_by_supervisor(&HOLD.kinds(), &[a.as_str(), b.as_str()])
        .unwrap();
    assert!(
        HOLD.transition(None, OwnHold::latest_of(&records, b.as_str()), &b)
            .is_none()
    );
    assert!(HOLD.holds(OwnHold::latest_of(&records, a.as_str())));
    let status = w.status();
    let rows = status.to_string();
    assert!(rows.contains("repository_config_unavailable"), "{status}");
    assert!(rows.contains("repository_config_hold"), "{status}");
    let supervisors = status["supervisors"].as_array().unwrap();
    let a_status = supervisors
        .iter()
        .find(|row| row["repository_config_hold"]["supervisor"] == a.as_str())
        .unwrap();
    assert_eq!(
        a_status["repository_config_hold"]["reason"],
        "repository_config_unavailable"
    );
    assert_eq!(
        supervisors
            .iter()
            .filter(|row| row.get("repository_config_hold").is_some())
            .count(),
        1
    );
    let held = w.claim_holds();
    assert_eq!(
        held.iter()
            .filter(|h| h["reason"] == "repository_config_held"
                && h["record"]["reason"] == "repository_config_unavailable")
            .count(),
        1
    );
}
