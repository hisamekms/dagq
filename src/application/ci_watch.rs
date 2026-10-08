//! The supervisor's watch of the landing branch's CI (ADR-t1920-1, the
//! design's CI watch): one check reads the means to read GitHub, records
//! when they go or come back, then records each settled run after the
//! latest one recorded, with the list of the tests that fail already, the
//! turns to red and green, and the `ci_failure` finding of new failures.
//! A run is processed once per attempt, by its ID and attempt, so one that
//! ended after a later-created one and a re-run's new attempt are read too.
//! The readers of the list (`ci failures`, `status`) fold the same events.
//! What GitHub says comes through [`CiSource`]; what it means is
//! `domain::ci_watch`.

use std::collections::BTreeSet;

use anyhow::Result;
use serde_json::{Value, json};

use super::{QueueRecords, RunLog};
use crate::domain::EventKind;
use crate::domain::ci_watch::{
    Access, CI_WATCH_ACCESS_KINDS, CI_WATCH_FAILURE_LIMIT, CiCheckRecord, CiJob, CiRun, CiState,
    CiWatchConfig, FINDING_KIND, FailedJob, GreenReading, JobsUnread, Junit, RangeFacts, RunInput,
    Undecided, WatchState, binary_contains, classify, decide, failures_view, read_green,
    runs_to_process, status_view,
};
use crate::domain::{FindingTarget, Impact, LeaseToken, NewFinding, TaskId};

/// What the watch reads GitHub and the repository through (`gh` and Git
/// on the host; fakes in tests).
pub trait CiSource: Send + Sync {
    /// Whether `gh` resolves and is logged in, and the repository it reads;
    /// an error is a passing failure (a call that ran out of time).
    fn access(&self) -> Result<Access>;
    /// The finished push runs of the watched workflow and branch, at most
    /// [`crate::domain::ci_watch::RUN_LIST_LIMIT`].
    fn completed_runs(&self) -> Result<Vec<CiRun>>;
    /// The failed jobs of a red run, with their failed steps.
    fn failed_jobs(&self, run_id: i64) -> Result<Vec<FailedJob>>;
    /// Every job of a run's attempt with how it ended, for a success run
    /// read against `required_jobs` ([`read_green`]); an error when they
    /// cannot be read.
    fn jobs(&self, run_id: i64, attempt: i64) -> Result<Vec<CiJob>>;
    /// The JUnit of a run's artifacts ([`Junit::Missing`] when they cannot
    /// be read).
    fn junit(&self, run_id: i64) -> Junit;
    /// `git rev-list --count from..to`, `None` when Git cannot tell.
    fn commits(&self, from: &str, to: &str) -> Option<u64>;
    /// Whether `ancestor` is `of` or its ancestor, `None` when Git cannot
    /// tell.
    fn is_ancestor(&self, ancestor: &str, of: &str) -> Option<bool>;
}

/// What one check did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    /// Whether the means to read GitHub are there: claims and landings
    /// wait while they are not.
    pub available: bool,
    /// The settled runs it recorded.
    pub recorded: usize,
    /// Another supervisor recorded a run first; the next check goes on
    /// after it.
    pub taken: bool,
    /// The success runs whose jobs this check could not read, with the
    /// failures in a row: the next check is given it back to count on.
    pub jobs_unread: JobsUnread,
}

/// One check: the means, then every run whose ID and attempt no record
/// holds, in the order they were created ([`runs_to_process`]; the newest
/// only at a queue's first check), the runs ended
/// with no outcome counted into the next settled one's `skipped_runs`.
/// With `required_jobs`, a success run's jobs are read too and one that
/// does not read green ([`read_green`]) counts with them; one whose jobs
/// cannot be read stops the check there, to be read again by the next
/// (`jobs_unread`, the last check's count for each run), and at the
/// limit is counted with them and `ci_check_failed` recorded once.
/// `build` is the supervisor's build identifier, whose commit tells
/// whether it contains a red range. An error (a call that failed or ran
/// out of time) is a passing failure the caller counts.
pub fn check<Q: RunLog + QueueRecords + ?Sized>(
    queue: &Q,
    source: &dyn CiSource,
    config: &CiWatchConfig,
    branch: &str,
    supervisor: &LeaseToken,
    build: &str,
    jobs_unread: &JobsUnread,
) -> Result<CheckOutcome> {
    let access = source.access()?;
    let last = queue.latest_queue_event(&CI_WATCH_ACCESS_KINDS)?;
    if let Some((kind, mut payload)) =
        access.transition(last.as_ref().map(|e| (e.kind.as_str(), &e.payload)))
    {
        payload["supervisor"] = json!(supervisor);
        queue.record_queue_event(kind, payload)?;
    }
    let mut outcome = CheckOutcome {
        available: access.is_available(),
        recorded: 0,
        taken: false,
        jobs_unread: JobsUnread::default(),
    };
    if !outcome.available {
        return Ok(outcome);
    }
    let runs = source.completed_runs()?;
    let mut watch = WatchState::fold(&queue.ci_watch_events()?);
    let (runs, mut gap) = runs_to_process(runs, &watch);
    let mut skipped = Vec::new();
    let mut undecided: Vec<(i64, i64, Undecided)> = Vec::new();
    let named = crate::build_id::named_commit(build);
    for run in &runs {
        let Some(state) = classify(&run.conclusion) else {
            skipped.push((run.run_id, run.attempt));
            continue;
        };
        if state == CiState::Green && !config.required_jobs.is_empty() {
            let jobs = source.jobs(run.run_id, run.attempt);
            let reading = match &jobs {
                Ok(list) => read_green(&config.required_jobs, Ok(list), 0),
                Err(error) => {
                    let failures = outcome.jobs_unread.fail(jobs_unread, run);
                    read_green(&config.required_jobs, Err(&format!("{error:#}")), failures)
                }
            };
            match reading {
                GreenReading::Green => {}
                GreenReading::Retry => {
                    if let Err(error) = &jobs {
                        tracing::warn!(error = %format_args!("{error:#}"), "the jobs of CI run {} could not be read ({} in a row); it is read again at the next check: {error:#}", run.run_id, outcome.jobs_unread.failures(run));
                    }
                    return Ok(outcome);
                }
                GreenReading::Undecided(why) => {
                    if let Undecided::JobsMissing { jobs } = &why
                        && let Some(mut payload) =
                            watch.jobs_missing_event(run, jobs, config, branch)
                    {
                        payload["supervisor"] = json!(supervisor);
                        queue.record_queue_event(EventKind::CiJobsMissing, payload)?;
                        watch = WatchState::fold(&queue.ci_watch_events()?);
                    }
                    if let Undecided::JobsUnreadable { error, failures } = &why {
                        // The count stays past the limit while the run is
                        // read again (until a settled run records it), so
                        // it is told once.
                        if *failures == CI_WATCH_FAILURE_LIMIT {
                            tracing::warn!(error = %error, "the jobs of CI run {} could not be read {failures} times in a row; it is counted as settling nothing", run.run_id);
                            queue.record_queue_event(
                                EventKind::CiCheckFailed,
                                json!({
                                    "error": error,
                                    "failures": failures,
                                    "run_id": run.run_id,
                                    "attempt": run.attempt,
                                    "supervisor": supervisor,
                                }),
                            )?;
                        }
                    }
                    skipped.push((run.run_id, run.attempt));
                    undecided.push((run.run_id, run.attempt, why));
                    continue;
                }
            }
        }
        let failed_jobs = match state {
            // A run whose jobs cannot be read is taken without them, so
            // one run does not stop the watch.
            CiState::Red => source.failed_jobs(run.run_id).unwrap_or_else(|error| {
                tracing::warn!(error = %format_args!("{error:#}"), "the jobs of CI run {} could not be read: {error:#}", run.run_id);
                Vec::new()
            }),
            CiState::Green => Vec::new(),
        };
        let junit = if config.junit_artifacts.is_empty() {
            Junit::NotConfigured
        } else {
            source.junit(run.run_id)
        };
        let input = RunInput {
            run,
            state,
            skipped: &skipped,
            undecided: &undecided,
            junit: &junit,
            failed_jobs: &failed_jobs,
            gap,
        };
        let mut decision = decide(
            &watch,
            &input,
            &RangeFacts::default(),
            &config.workflow,
            branch,
        );
        // The range is read from Git only for a finding that carries it.
        if decision.finding.is_some() {
            let from = watch.last_green.as_ref().map(|green| green.sha.as_str());
            let facts = RangeFacts {
                commits: from.and_then(|from| source.commits(from, &run.sha)),
                binary: Some(binary_contains(named, from, &run.sha, |a, b| {
                    source.is_ancestor(a, b)
                })),
                binary_commit: named.map(str::to_owned),
            };
            decision = decide(&watch, &input, &facts, &config.workflow, branch);
        }
        let stamp = |payload: Option<Value>| {
            payload.map(|mut payload| {
                payload["supervisor"] = json!(supervisor);
                payload
            })
        };
        let record = CiCheckRecord {
            previous: watch.last_event,
            checked: stamp(Some(decision.checked)).unwrap_or_default(),
            turned_red: stamp(decision.turned_red),
            turned_green: stamp(decision.turned_green),
            finding: decision.finding.map(|finding| NewFinding {
                kind: FINDING_KIND.to_owned(),
                target: FindingTarget::Queue,
                subject: finding.subject,
                summary: finding.summary,
                detail: Some(finding.detail.to_string()),
                impact: Some(Impact::High),
                evidence: Vec::new(),
                propose: Some(finding.propose),
                by: crate::domain::ActorRole::Supervisor.as_str().to_owned(),
            }),
            resolve: decision.resolved,
        };
        if queue.record_ci_check(record)?.is_none() {
            outcome.taken = true;
            return Ok(outcome);
        }
        outcome.recorded += 1;
        skipped.clear();
        undecided.clear();
        gap = false;
        watch = WatchState::fold(&queue.ci_watch_events()?);
    }
    Ok(outcome)
}

/// The workflow and branch the list is of: `[ci_watch]`'s when it is set,
/// else the latest record's.
fn watched<Q: RunLog + QueueRecords + ?Sized>(
    config: Option<&CiWatchConfig>,
    branch: Option<&str>,
    queue: &Q,
) -> Result<(Option<String>, Option<String>)> {
    let latest = queue
        .latest_queue_event(&[crate::domain::ci_watch::CI_CHECKED])?
        .map(|event| event.payload);
    let recorded = |key: &str| {
        latest
            .as_ref()
            .and_then(|payload| payload[key].as_str().map(str::to_owned))
    };
    Ok((
        config
            .map(|config| config.workflow.clone())
            .or_else(|| recorded("workflow")),
        branch.map(str::to_owned).or_else(|| recorded("branch")),
    ))
}

/// `dagq ci failures [--task ID]`: the tests that fail already on the
/// watched branch. With `task`, the items of the `ci_failure` findings the
/// task fixes move to `kept_for_task` (ADR-t1920-1 decision 5). `config`
/// is `[ci_watch]` as read now, `branch` the branch it watches. Reads only.
pub fn known_failures<Q: RunLog + QueueRecords + ?Sized>(
    queue: &Q,
    config: Option<&CiWatchConfig>,
    branch: Option<&str>,
    task: Option<TaskId>,
) -> Result<Value> {
    let watch = WatchState::fold(&queue.ci_watch_events()?);
    let kept: BTreeSet<i64> = match task {
        Some(task) => queue
            .ci_failure_findings_of(task)?
            .into_iter()
            .map(|id| id.as_i64())
            .collect(),
        None => BTreeSet::new(),
    };
    let last = queue.latest_queue_event(&CI_WATCH_ACCESS_KINDS)?;
    let (workflow, branch) = watched(config, branch, queue)?;
    Ok(failures_view(
        config.is_some(),
        workflow.as_deref(),
        branch.as_deref(),
        &watch,
        last.as_ref().map(|event| event.kind.as_str()),
        &kept,
    ))
}

/// What `status` takes from `[ci_watch]` as read now (`read`): the jobs it
/// names, so a `ci_jobs_missing` it no longer names is not an attention
/// (ADR-t2034-1 decision 5), `None` when it could not be read (unknown,
/// which keeps the attention); and whether the watch is on, for
/// [`status`]'s `enabled`.
pub fn status_reading(read: &Result<Option<CiWatchConfig>>) -> (Option<&[String]>, bool) {
    match read {
        Ok(config) => (
            Some(config.as_ref().map_or(&[][..], |c| &c.required_jobs[..])),
            config.is_some(),
        ),
        Err(_) => (None, false),
    }
}

/// `status`'s `ci`: null until the watch recorded anything.
pub fn status<Q: RunLog + QueueRecords + ?Sized>(queue: &Q, enabled: bool) -> Result<Value> {
    let watch = WatchState::fold(&queue.ci_watch_events()?);
    let last = queue.latest_queue_event(&CI_WATCH_ACCESS_KINDS)?;
    Ok(status_view(
        enabled,
        &watch,
        last.as_ref().map(|event| event.kind.as_str()),
    ))
}

/// `doctor`'s `ci_watch`: none without `[ci_watch]`, `{error}` when the
/// file cannot be read, else what `probe` found of the means to read the
/// CI (the table, `gh`, `authenticated`, `repo`) with the supervisor's
/// last `ci_watch_unavailable` / `ci_watch_available` as
/// `supervisor_last`.
pub fn doctor<Q: RunLog + ?Sized>(
    queue: &Q,
    config: Result<Option<CiWatchConfig>>,
    probe: impl FnOnce(&CiWatchConfig) -> Value,
) -> Result<Option<Value>> {
    let config = match config {
        Ok(Some(config)) => config,
        Ok(None) => return Ok(None),
        Err(error) => return Ok(Some(json!({"error": format!("{error:#}")}))),
    };
    let mut view = probe(&config);
    view["supervisor_last"] = json!(
        queue
            .latest_queue_event(&CI_WATCH_ACCESS_KINDS)?
            .map(|event| json!({"kind": event.kind, "created_at": event.created_at, "payload": event.payload}))
    );
    Ok(Some(view))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table names its jobs and turns the watch on, no table names none
    /// and leaves it off, and a file that cannot be read names nothing
    /// known (the attention stays) and leaves it off.
    #[test]
    fn status_reads_the_named_jobs_and_whether_the_watch_is_on() {
        let config = CiWatchConfig {
            workflow: "ci.yml".into(),
            branch: None,
            interval_secs: 60,
            junit_artifacts: Vec::new(),
            required_jobs: vec!["test".into()],
        };
        let named = Ok(Some(config));
        assert_eq!(
            status_reading(&named),
            (Some(&["test".to_owned()][..]), true)
        );
        assert_eq!(status_reading(&Ok(None)), (Some(&[][..]), false));
        assert_eq!(status_reading(&Err(anyhow::anyhow!("bad"))), (None, false));
    }
}
