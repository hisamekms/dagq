//! The CI watch's look at the age of a declared workflow's latest success
//! wired to the queue (docs/design/supervisor-lifecycle/ci-watch.md
//! "成果物の古さ", ADR-t3924-1): `[[ci_watch.freshness]]` as the port reads
//! it, the production check on its job thread, its records written through
//! the queue service, and the attention `status` shows, with a fake of
//! what `gh` reads and starts.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{agent::Script, harness::Wiring};
use crate::{
    application::{ci_watch::CiSource, supervise::CiWatchPort},
    domain::{
        EventKind,
        ci_watch::{
            Access, CiJob, CiRun, CiWatchConfig, FailedJob, Junit, freshness::FreshnessWatch,
        },
    },
};

const TABLE: &str = "table.yml";

/// What the fake `gh` holds: the runs of [`TABLE`] newest first (or why
/// they cannot be read), and the starts asked of it.
#[derive(Default)]
struct Runs {
    runs: Vec<CiRun>,
    unreadable: bool,
    /// How often the latest success was asked for.
    reads: usize,
    dispatched: Vec<(String, String)>,
}

/// The CI as `gh` would read it: the means are there, the watched
/// workflow has no push run, and [`TABLE`]'s runs are what the test sets.
#[derive(Default)]
struct FakeCi(Mutex<Runs>);

impl FakeCi {
    fn set(&self, runs: Vec<CiRun>) {
        let mut state = self.0.lock().unwrap();
        state.runs = runs;
        state.unreadable = false;
    }

    fn unreadable(&self) {
        self.0.lock().unwrap().unreadable = true;
    }

    fn reads(&self) -> usize {
        self.0.lock().unwrap().reads
    }

    fn dispatched(&self) -> Vec<(String, String)> {
        self.0.lock().unwrap().dispatched.clone()
    }
}

impl CiSource for FakeCi {
    fn access(&self) -> Result<Access> {
        Ok(Access::Available {
            program: "gh".into(),
            resolved: "/fake/gh".into(),
            repo: "o/n".into(),
        })
    }
    fn completed_runs(&self) -> Result<Vec<CiRun>> {
        Ok(Vec::new())
    }
    fn scheduled_runs(&self, _: &str) -> Result<Vec<CiRun>> {
        Ok(Vec::new())
    }
    fn latest_success(&self, workflow: &str, _: &str) -> Result<Option<CiRun>> {
        let mut state = self.0.lock().unwrap();
        state.reads += 1;
        if state.unreadable {
            bail!("gh run list: HTTP 502");
        }
        assert_eq!(workflow, TABLE);
        Ok(state
            .runs
            .iter()
            .find(|run| run.conclusion == "success")
            .cloned())
    }
    fn dispatch(&self, workflow: &str, branch: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .dispatched
            .push((workflow.to_owned(), branch.to_owned()));
        Ok(())
    }
    fn failed_jobs(&self, _: i64) -> Result<Vec<FailedJob>> {
        Ok(Vec::new())
    }
    fn jobs(&self, _: i64, _: i64) -> Result<Vec<CiJob>> {
        Ok(Vec::new())
    }
    fn junit(&self, _: i64) -> Junit {
        Junit::NotConfigured
    }
    fn commits(&self, _: &str, _: &str) -> Option<u64> {
        None
    }
    fn is_ancestor(&self, _: &str, _: &str) -> Option<bool> {
        None
    }
}

fn run(id: i64, conclusion: &str, created_at: &str) -> CiRun {
    CiRun {
        run_id: id,
        run_number: id,
        sha: format!("{id:040}"),
        conclusion: conclusion.into(),
        url: format!("https://github.com/o/n/actions/runs/{id}"),
        created_at: created_at.into(),
        attempt: 1,
    }
}

/// The port of a `[ci_watch]` that declares [`TABLE`] with a limit of 48
/// hours told 6 hours before, read through `ci`.
fn port(ci: &Arc<FakeCi>) -> CiWatchPort {
    let config = CiWatchConfig {
        workflow: "ci.yml".into(),
        branch: None,
        interval_secs: 600,
        junit_artifacts: Vec::new(),
        required_jobs: Vec::new(),
        scheduled_workflows: Vec::new(),
        freshness: vec![FreshnessWatch {
            workflow: TABLE.into(),
            max_age_secs: 48 * 3600,
            lead_secs: 6 * 3600,
        }],
    };
    let ci = ci.clone();
    CiWatchPort {
        file: Arc::new(move || Ok(Some(config.clone()))),
        source: Arc::new(move |_, _| Ok(ci.clone() as Arc<dyn CiSource>)),
        interval: Some(Duration::from_secs(3600)),
    }
}

fn events(wiring: &Wiring, kind: EventKind) -> Vec<Value> {
    wiring
        .queue()
        .ci_freshness_events()
        .unwrap()
        .into_iter()
        .filter(|event| event.kind == kind)
        .map(|event| event.payload)
        .collect()
}

fn stale_attention(wiring: &Wiring) -> Vec<Value> {
    wiring.status()["attention"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["kind"] == "ci_workflow_stale")
        .cloned()
        .collect()
}

/// The test's clock stands at 2026-11-01T00:00:00Z. A latest success of
/// 2026-10-30T00:13:49Z (about 48 hours, past the 42 the lead leaves),
/// with only cancelled runs after it, tells the inbox once and starts the
/// workflow once on the landing branch; the next supervisor's look at the
/// same success holds both; a newer success closes the attention.
#[test]
fn a_stale_workflow_is_told_and_started_once_until_a_newer_success() {
    let ci = Arc::new(FakeCi::default());
    ci.set(vec![
        run(13, "cancelled", "2026-10-31T00:13:00Z"),
        run(12, "cancelled", "2026-10-30T12:13:00Z"),
        run(11, "success", "2026-10-30T00:13:49Z"),
    ]);
    let wiring = Wiring::new(
        "a_stale_workflow_is_told_and_started_once_until_a_newer_success",
        Script {
            receipts: Vec::new(),
            verdicts: Vec::new(),
        },
    )
    .with_ci_watch(port(&ci));

    wiring.supervise_once();
    let stale = events(&wiring, EventKind::CiWorkflowStale);
    assert_eq!(stale.len(), 1, "{stale:?}");
    assert_eq!(stale[0]["workflow"], TABLE);
    assert_eq!(stale[0]["branch"], "main");
    assert_eq!(stale[0]["run_id"], 11);
    assert_eq!(stale[0]["created_at"], "2026-10-30T00:13:49Z");
    assert_eq!(stale[0]["max_age_secs"], 48 * 3600);
    assert_eq!(ci.dispatched(), vec![(TABLE.to_owned(), "main".to_owned())]);
    let attention = stale_attention(&wiring);
    assert_eq!(attention.len(), 1, "{attention:?}");
    assert_eq!(attention[0]["next"], "look at the stale workflow's runs");
    assert!(
        attention[0]["last_error"]
            .as_str()
            .unwrap()
            .starts_with("table.yml on main: latest success run 11"),
        "{attention:?}"
    );

    // The inbox's watch tells which workflow is how old against its limit.
    let told: Vec<Value> = wiring
        .inbox_watch()
        .into_iter()
        .filter(|event| event["kind"] == "ci_workflow_stale")
        .collect();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["next"], "look at the stale workflow's runs");
    assert_eq!(
        told[0]["reason"],
        "table.yml on main: latest success run 11 created 2026-10-30T00:13:49Z, 47h old, limit 48h; workflow_dispatch tried once"
    );

    // Another supervisor reads the same success: nothing is started again.
    wiring.supervise_once();
    assert_eq!(ci.reads(), 2);
    assert_eq!(events(&wiring, EventKind::CiWorkflowStale).len(), 1);
    assert_eq!(ci.dispatched().len(), 1);
    assert_eq!(stale_attention(&wiring).len(), 1);

    // The run it started succeeded: the attention closes.
    ci.set(vec![
        run(14, "success", "2026-10-31T23:00:00Z"),
        run(11, "success", "2026-10-30T00:13:49Z"),
    ]);
    wiring.supervise_once();
    let fresh = events(&wiring, EventKind::CiWorkflowFresh);
    assert_eq!(fresh.len(), 1, "{fresh:?}");
    assert_eq!(fresh[0]["run_id"], 14);
    assert_eq!(fresh[0]["stale_run_id"], 11);
    assert_eq!(stale_attention(&wiring), Vec::<Value>::new());
    assert_eq!(ci.dispatched().len(), 1);
}

/// A workflow whose runs `gh` cannot read writes no record, starts
/// nothing and is no failure of the check: no `ci_check_failed`, the
/// claims and landings not held, no turn to red or green, and no test
/// counted as failing already.
#[test]
fn an_unreadable_workflow_changes_nothing_of_the_watch() {
    let ci = Arc::new(FakeCi::default());
    ci.unreadable();
    let wiring = Wiring::new(
        "an_unreadable_workflow_changes_nothing_of_the_watch",
        Script {
            receipts: Vec::new(),
            verdicts: Vec::new(),
        },
    )
    .with_ci_watch(port(&ci));

    for _ in 0..crate::domain::ci_watch::CI_WATCH_FAILURE_LIMIT {
        wiring.supervise_once();
    }
    // Each supervisor tried to read it.
    assert_eq!(ci.reads(), crate::domain::ci_watch::CI_WATCH_FAILURE_LIMIT);
    let queue = wiring.queue();
    assert_eq!(queue.ci_freshness_events().unwrap(), Vec::new());
    assert!(ci.dispatched().is_empty());
    let latest = |kinds: &[&str]| {
        queue
            .latest_queue_event(kinds)
            .unwrap()
            .map(|event| event.kind.to_string())
    };
    assert_eq!(latest(&["ci_check_failed"]), None);
    assert_eq!(latest(&["ci_turned_red", "ci_turned_green"]), None);
    assert_eq!(
        latest(&["ci_watch_held", "ci_watch_resumed"]).as_deref(),
        Some("ci_watch_resumed")
    );
    let failures = crate::application::ci_watch::known_failures(&queue, None, None, None).unwrap();
    assert_eq!(failures["failures"], json!([]), "{failures}");
    assert_eq!(stale_attention(&wiring), Vec::<Value>::new());
}
