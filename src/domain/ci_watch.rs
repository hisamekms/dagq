//! The supervisor's watch of the landing branch's CI (ADR-t1920-1): what
//! `[ci_watch]` sets, how a finished CI run reads (green, red or skipped,
//! the tests its JUnit names), the list of the tests that fail already
//! (folded from the `ci_checked` events, its only record), what one run
//! changes in it and records, the key of a `ci_failure` finding, whether
//! the supervisor's build contains a red range, and whether the means to
//! read GitHub are there. Reading GitHub and the queue lives elsewhere
//! (`infrastructure::ci_watch`, `application::ci_watch`); this is what the
//! readings mean.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::event_kind::EventKind;
use super::{AttentionNext, RunEvent};

/// A finished run of the watched workflow was read (one per run whose
/// outcome is settled; a check that found none writes nothing).
pub const CI_CHECKED: &str = EventKind::CiChecked.as_str();
/// The watched branch turned red (from green, or at the first record).
pub const CI_TURNED_RED: &str = EventKind::CiTurnedRed.as_str();
/// The watched branch turned green again after red.
pub const CI_TURNED_GREEN: &str = EventKind::CiTurnedGreen.as_str();
/// The means to read GitHub went away: an attention for the inbox.
pub const CI_WATCH_UNAVAILABLE: &str = EventKind::CiWatchUnavailable.as_str();
/// The means to read GitHub came back.
pub const CI_WATCH_AVAILABLE: &str = EventKind::CiWatchAvailable.as_str();
/// The same passing failure (network, a non-zero `gh`) repeated
/// [`CI_WATCH_FAILURE_LIMIT`] times, or the jobs of one success run could
/// not be read that many times in a row (ADR-t2034-1 decision 5).
pub const CI_CHECK_FAILED: &str = EventKind::CiCheckFailed.as_str();
/// A success run lacks a job `required_jobs` names: the setting and the
/// workflow disagree (ADR-t2034-1 decision 5), an attention for the inbox
/// until a green run with every named job is recorded.
pub const CI_JOBS_MISSING: &str = EventKind::CiJobsMissing.as_str();
/// The two kinds of the means' state, for reading the latest of them.
pub const CI_WATCH_ACCESS_KINDS: [&str; 2] = [CI_WATCH_UNAVAILABLE, CI_WATCH_AVAILABLE];
/// The supervisor started holding its claims, resumes and landings for
/// the watch (`reason`: `pending` before this process's first answer,
/// `unreadable` while the last one found the means missing; `workflow`,
/// `supervisor`), or holds them for the other reason.
pub const CI_WATCH_HELD: &str = EventKind::CiWatchHeld.as_str();
/// The hold for the watch ended (`reason` that ended, `supervisor`).
pub const CI_WATCH_RESUMED: &str = EventKind::CiWatchResumed.as_str();
/// The supervisor's hold for the watch, which the check's own records do
/// not show: [`CI_WATCH_UNAVAILABLE`] is the queue's answer, not whether
/// a supervisor waits for one.
pub const CI_WATCH_HOLD: super::claim_hold::OwnHold = super::claim_hold::OwnHold {
    held: EventKind::CiWatchHeld,
    resumed: EventKind::CiWatchResumed,
};
/// Every kind the watch records, oldest first in the queue.
pub const CI_WATCH_KINDS: [&str; 7] = [
    CI_CHECKED,
    CI_TURNED_RED,
    CI_TURNED_GREEN,
    CI_WATCH_UNAVAILABLE,
    CI_WATCH_AVAILABLE,
    CI_CHECK_FAILED,
    CI_JOBS_MISSING,
];

/// The kind of the finding a red run records (ADR-t1920-1 decision 4).
pub const FINDING_KIND: &str = "ci_failure";
/// `interval_secs` when `[ci_watch]` does not set it.
pub const DEFAULT_INTERVAL_SECS: u64 = 600;
/// The least `interval_secs` `[ci_watch]` takes.
pub const MIN_INTERVAL_SECS: u64 = 60;
/// How many times the same passing failure repeats before
/// [`CI_CHECK_FAILED`] is recorded.
pub const CI_WATCH_FAILURE_LIMIT: usize = 3;
/// How many finished runs one check lists (`gh run list --limit`).
pub const RUN_LIST_LIMIT: usize = 50;
/// How long one `gh` call may take.
pub const CI_WATCH_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// `[ci_watch]` of `dagq.toml`: the workflow whose pushes to the branch
/// the supervisor watches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CiWatchConfig {
    /// The workflow file name or name (`gh run list --workflow`).
    pub workflow: String,
    /// The watched branch; `None` is the landing branch.
    pub branch: Option<String>,
    pub interval_secs: u64,
    /// The globs of the artifacts holding JUnit XML; empty reads none.
    pub junit_artifacts: Vec<String>,
    /// `required_jobs = ["name", ...]`: the `name`s of the jobs a success
    /// run must have run to `success` to read green (ADR-t2034-1 decision
    /// 4). A success run where one of them ended otherwise (skipped on a
    /// docs-only push), is absent, or whose jobs cannot be read is not
    /// green ([`read_green`]). Empty (the key left out) reads no job.
    pub required_jobs: Vec<String>,
}

impl CiWatchConfig {
    pub const KEYS: [&'static str; 5] = [
        "workflow",
        "branch",
        "interval_secs",
        "junit_artifacts",
        "required_jobs",
    ];

    /// How long after a check the next one is due.
    pub const fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }
}

/// What a finished run settles: green, red, or nothing (skipped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiState {
    Green,
    Red,
}

impl CiState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Red => "red",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "green" => Some(Self::Green),
            "red" => Some(Self::Red),
            _ => None,
        }
    }
}

/// The state a run's `conclusion` settles: `success` green, `failure` and
/// `timed_out` red, any other end (`cancelled`, `skipped`, `neutral`,
/// `action_required`, `startup_failure`, `stale`) none, so the next settled
/// run takes its range.
pub fn classify(conclusion: &str) -> Option<CiState> {
    match conclusion {
        "success" => Some(CiState::Green),
        "failure" | "timed_out" => Some(CiState::Red),
        _ => None,
    }
}

/// A job of a run and how it ended, as `gh run view --json jobs` gives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiJob {
    pub name: String,
    /// Empty while the job has not ended.
    pub conclusion: String,
}

/// Why a success run is not read green but counted with the runs that
/// settle nothing (in the next settled run's `skipped_runs`, and its
/// `undecided` with this reason): it neither turns the watch green nor
/// takes anything off the list (ADR-t2034-1 decisions 4 and 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Undecided {
    /// A named job ended other than `success` (`skipped` when the run
    /// left it out): the named jobs that did, with their conclusions.
    JobsNotPassed { jobs: Vec<CiJob> },
    /// A named job is not among the run's jobs (renamed or moved in the
    /// workflow): the names missing. [`CI_JOBS_MISSING`] tells the inbox.
    JobsMissing { jobs: Vec<String> },
    /// The run's jobs could not be read [`CI_WATCH_FAILURE_LIMIT`] times
    /// in a row: the last error.
    JobsUnreadable { error: String, failures: usize },
}

/// What a success run reads as once `required_jobs` is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GreenReading {
    /// Every named job ended `success`: green.
    Green,
    /// Not green: counted with the runs that settle nothing.
    Undecided(Undecided),
    /// Its jobs could not be read, fewer than [`CI_WATCH_FAILURE_LIMIT`]
    /// times in a row: neither it nor any later run is processed, and the
    /// next check reads it again.
    Retry,
}

/// What the success run reads as by `required_jobs` (`required`) and its
/// jobs (`jobs`, or the error reading them, the `failures`-th in a row
/// with this one). A named job that is missing outweighs one that did
/// not pass; a job named twice in the run must pass in each. Neither an
/// absent job nor an unreadable list is read green, so the list of the
/// failing tests is not emptied by a run whose jobs are not known.
pub fn read_green(
    required: &[String],
    jobs: Result<&[CiJob], &str>,
    failures: usize,
) -> GreenReading {
    if required.is_empty() {
        return GreenReading::Green;
    }
    let jobs = match jobs {
        Ok(jobs) => jobs,
        Err(_) if failures < CI_WATCH_FAILURE_LIMIT => return GreenReading::Retry,
        Err(error) => {
            return GreenReading::Undecided(Undecided::JobsUnreadable {
                error: error.to_owned(),
                failures,
            });
        }
    };
    let missing: Vec<String> = required
        .iter()
        .filter(|name| !jobs.iter().any(|job| &job.name == *name))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return GreenReading::Undecided(Undecided::JobsMissing { jobs: missing });
    }
    let not_passed: Vec<CiJob> = jobs
        .iter()
        .filter(|job| required.contains(&job.name) && job.conclusion != "success")
        .cloned()
        .collect();
    if not_passed.is_empty() {
        GreenReading::Green
    } else {
        GreenReading::Undecided(Undecided::JobsNotPassed { jobs: not_passed })
    }
}

/// How many checks in a row could not read the jobs of each success run
/// (by run ID and attempt): what a check counts on from and hands to the
/// next. A check keeps only the runs whose jobs it failed to read, so a run
/// read or recorded since drops out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobsUnread(BTreeMap<(i64, i64), usize>);

impl JobsUnread {
    /// One more failure to read `run`'s jobs, counted on from `last` (the
    /// previous check's) and kept here: the failures in a row. Past
    /// [`CI_WATCH_FAILURE_LIMIT`] it stays there, so a run given up is
    /// given up again at once and told once, and every run has its own
    /// count, so several unreadable runs in a row are each given up.
    pub fn fail(&mut self, last: &Self, run: &CiRun) -> usize {
        let key = (run.run_id, run.attempt);
        let before = last.failures(run);
        let failures = (before + 1).min(CI_WATCH_FAILURE_LIMIT + 1);
        self.0.insert(key, failures);
        failures
    }

    /// The failures in a row counted for `run` (0 for none).
    pub fn failures(&self, run: &CiRun) -> usize {
        self.0
            .get(&(run.run_id, run.attempt))
            .copied()
            .unwrap_or_default()
    }
}

/// One finished run of the watched workflow, as `gh run list` gives it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiRun {
    pub run_id: i64,
    pub run_number: i64,
    pub sha: String,
    pub conclusion: String,
    pub url: String,
    pub created_at: String,
    /// The run's attempt (`gh run list`'s `attempt`): a re-run keeps the
    /// run's ID and creation time and counts up from 1.
    pub attempt: i64,
}

impl CiRun {
    /// Its place in the order the runs were created in.
    fn key(&self) -> (&str, i64) {
        (self.created_at.as_str(), self.run_id)
    }
}

/// The runs a check processes, in the order they were created: each one
/// whose run ID and attempt no `ci_checked` recorded (settled or among its
/// `skipped_runs`), created no earlier than the first run the watch
/// recorded, so a run that ended after a later-created one, or a re-run's
/// new attempt, is still read; only the newest one at the first check of
/// a queue. `gap` when the list was full and even its oldest run came
/// after the latest recorded one: the runs in between are not read.
pub fn runs_to_process(mut runs: Vec<CiRun>, watch: &WatchState) -> (Vec<CiRun>, bool) {
    runs.sort_by(|a, b| a.key().cmp(&b.key()));
    let (Some(head), Some((floor_at, floor_id))) = (watch.head(), &watch.floor) else {
        return (runs.pop().into_iter().collect(), false);
    };
    let gap = runs.len() >= RUN_LIST_LIMIT && runs.first().is_some_and(|run| run.key() > head);
    let floor = (floor_at.as_str(), *floor_id);
    let runs = runs
        .into_iter()
        .filter(|run| run.key() >= floor && !watch.processed.contains(&(run.run_id, run.attempt)))
        .collect();
    (runs, gap)
}

/// A JUnit test case's outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TestOutcome {
    /// It has a `<skipped>` child: it did not run.
    Skipped,
    Passed,
    /// It has a `<failure>` or `<error>` child.
    Failed,
}

/// The test cases of one JUnit XML file, each named by its `classname`
/// and `name` joined by one space (as nextest prints them; `name` alone
/// without a `classname`). An error when the text holds no test suite.
pub fn parse_junit(xml: &str) -> Result<Vec<(String, TestOutcome)>, String> {
    if !xml.contains("<testsuite") {
        return Err("no <testsuite> element".to_owned());
    }
    let mut cases = Vec::new();
    let mut rest = xml;
    while let Some(start) = find_tag(rest, "testcase") {
        let after = &rest[start + "<testcase".len()..];
        let end = after
            .find('>')
            .ok_or_else(|| "an unclosed <testcase> tag".to_owned())?;
        let attributes = &after[..end];
        let self_closing = attributes.trim_end().ends_with('/');
        let classname = attribute(attributes, "classname").unwrap_or_default();
        let name = attribute(attributes, "name")
            .ok_or_else(|| "a <testcase> without a name".to_owned())?;
        let full = if classname.is_empty() {
            name
        } else {
            format!("{classname} {name}")
        };
        let body_start = &after[end + 1..];
        let (body, next) = if self_closing {
            ("", body_start)
        } else {
            let close = body_start
                .find("</testcase>")
                .ok_or_else(|| format!("<testcase {full}> is not closed"))?;
            (
                &body_start[..close],
                &body_start[close + "</testcase>".len()..],
            )
        };
        let outcome = if find_tag(body, "failure").is_some() || find_tag(body, "error").is_some() {
            TestOutcome::Failed
        } else if find_tag(body, "skipped").is_some() {
            TestOutcome::Skipped
        } else {
            TestOutcome::Passed
        };
        cases.push((full, outcome));
        rest = next;
    }
    Ok(cases)
}

/// Where the element `<name` starts in `text`: followed by a space, `>` or
/// `/`, so `<failure` does not match `<flakyFailure` nor `<failures`.
fn find_tag(text: &str, name: &str) -> Option<usize> {
    let open = format!("<{name}");
    let mut from = 0;
    while let Some(found) = text[from..].find(&open) {
        let at = from + found;
        match text[at + open.len()..].chars().next() {
            Some(c) if c.is_whitespace() || c == '>' || c == '/' => return Some(at),
            _ => from = at + open.len(),
        }
    }
    None
}

/// The value of `key` among a tag's attributes, entities decoded.
fn attribute(attributes: &str, key: &str) -> Option<String> {
    let mut rest = attributes;
    loop {
        let at = rest.find(key)?;
        let before = rest[..at].chars().next_back();
        let after = rest[at + key.len()..].trim_start();
        if before.is_none_or(char::is_whitespace)
            && let Some(value) = after.strip_prefix('=')
        {
            let value = value.trim_start();
            let quote = value.chars().next()?;
            if quote != '"' && quote != '\'' {
                return None;
            }
            let end = value[1..].find(quote)?;
            return Some(unescape(&value[1..=end]));
        }
        rest = &rest[at + key.len()..];
    }
}

/// XML's five entities and character references.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let Some(end) = tail.find(';') else {
            out.push_str(tail);
            return out;
        };
        let entity = &tail[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .map(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| entity.strip_prefix('#').map(|dec| dec.parse::<u32>().ok()))
                .flatten()
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(&tail[..=end]),
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

/// The outcome of each test over several JUnit files (the jobs of one
/// run): failed in any is failed, else passed in any is passed.
pub fn merge_outcomes(
    files: impl IntoIterator<Item = Vec<(String, TestOutcome)>>,
) -> BTreeMap<String, TestOutcome> {
    let mut merged = BTreeMap::new();
    for (name, outcome) in files.into_iter().flatten() {
        merged
            .entry(name)
            .and_modify(|seen: &mut TestOutcome| *seen = (*seen).max(outcome))
            .or_insert(outcome);
    }
    merged
}

/// What a run's JUnit gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Junit {
    /// `[ci_watch]` names no artifact.
    NotConfigured,
    /// No artifact, one that could not be downloaded, or XML that could
    /// not be read: the run's test outcomes are not known.
    Missing,
    Read(BTreeMap<String, TestOutcome>),
}

impl Junit {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::Missing => "missing",
            Self::Read(_) => "read",
        }
    }

    fn failed(&self) -> Vec<String> {
        match self {
            Self::Read(tests) => tests
                .iter()
                .filter(|(_, outcome)| **outcome == TestOutcome::Failed)
                .map(|(name, _)| name.clone())
                .collect(),
            _ => Vec::new(),
        }
    }

    fn passed(&self, name: &str) -> bool {
        matches!(self, Self::Read(tests) if tests.get(name) == Some(&TestOutcome::Passed))
    }
}

/// A job of a red run that failed, with its failed steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedJob {
    pub job: String,
    pub steps: Vec<String>,
}

impl FailedJob {
    /// The list items of a failure without a test name: one per failed
    /// step, or the job alone when no step is named.
    fn items(&self) -> Vec<String> {
        if self.steps.is_empty() {
            return vec![format!("job:{}", self.job)];
        }
        self.steps
            .iter()
            .map(|step| format!("job:{}/step:{step}", self.job))
            .collect()
    }
}

/// A list item's kind: a test, or a failed job and step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Test,
    JobStep,
}

impl FailureKind {
    fn of(name: &str) -> Self {
        if name.starts_with("job:") {
            Self::JobStep
        } else {
            Self::Test
        }
    }
}

/// A run named by its ID, commit and URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunRef {
    pub run_id: i64,
    pub sha: String,
    pub url: String,
}

/// Where a list item was added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Added {
    pub run_id: i64,
    pub attempt: i64,
    pub sha: String,
    pub url: String,
    /// When its `ci_checked` was recorded.
    pub at: String,
}

/// A test (or a failed job and step) that fails on the watched branch
/// now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KnownFailure {
    pub name: String,
    pub kind: FailureKind,
    pub added: Added,
    /// The `ci_failure` finding recorded for the run that added it.
    pub finding_id: Option<i64>,
}

/// The run a `ci_checked` recorded that was created last (a run recorded
/// late, after it, does not take its place), at its attempt recorded last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LatestRun {
    pub run_id: i64,
    pub attempt: i64,
    pub sha: String,
    pub url: String,
    pub conclusion: String,
    #[serde(skip)]
    pub created_at: String,
}

/// The watch as its events leave it: the only record of the list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchState {
    pub state: Option<CiState>,
    pub latest: Option<LatestRun>,
    /// When the latest `ci_checked` was recorded.
    pub checked_at: Option<String>,
    /// The latest green run before now.
    pub last_green: Option<RunRef>,
    /// The first red run of the red in progress, with its creation time.
    pub red_since: Option<(RunRef, String)>,
    /// The list, by name.
    pub failures: BTreeMap<String, KnownFailure>,
    /// Whether any event of the watch is recorded.
    pub recorded: bool,
    /// The run ID and attempt of every run a `ci_checked` recorded,
    /// settled or skipped: processed, never again.
    pub processed: BTreeSet<(i64, i64)>,
    /// The creation time and run ID of the first run recorded: the watch
    /// does not read back before it.
    pub floor: Option<(String, i64)>,
    /// The ID of the latest `ci_checked` event: a record decided after it
    /// is written only while it is still the latest.
    pub last_event: Option<i64>,
    /// The payload of the latest [`CI_JOBS_MISSING`] while no green run
    /// (which had every named job) was recorded after it, kept until one
    /// is; whether it stands as an attention is
    /// [`Self::standing_jobs_missing`], against the setting read now.
    pub jobs_missing: Option<Value>,
}

impl WatchState {
    /// Fold the watch's events, oldest first; kinds other than
    /// [`CI_CHECKED`] only mark that the watch recorded something.
    pub fn fold(events: &[RunEvent]) -> Self {
        let mut state = Self::default();
        for event in events {
            state.recorded |= CI_WATCH_KINDS.contains(&event.kind.as_str());
            if event.kind == CI_JOBS_MISSING {
                state.jobs_missing = Some(event.payload.clone());
            }
            if event.kind == CI_CHECKED {
                state.apply(&event.payload, &event.created_at);
                state.last_event = Some(event.id.as_i64());
            }
        }
        state
    }

    /// Fold one `ci_checked`. A run recorded `late` (created before the
    /// latest recorded one) is only marked processed: it changes neither
    /// the list nor the state. An event without `attempt` (or
    /// `skipped_attempts`) is of attempt 1.
    fn apply(&mut self, payload: &Value, recorded_at: &str) {
        let text = |key: &str| payload[key].as_str().unwrap_or_default().to_owned();
        let run = RunRef {
            run_id: payload["run_id"].as_i64().unwrap_or_default(),
            sha: text("sha"),
            url: text("url"),
        };
        let attempt = payload["attempt"].as_i64().unwrap_or(1);
        self.processed.insert((run.run_id, attempt));
        let skipped_attempts = payload["skipped_attempts"].as_array();
        for (index, id) in payload["skipped_runs"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            if let Some(id) = id.as_i64() {
                let attempt = skipped_attempts
                    .and_then(|attempts| attempts.get(index))
                    .and_then(Value::as_i64)
                    .unwrap_or(1);
                self.processed.insert((id, attempt));
            }
        }
        let key = (text("created_at"), run.run_id);
        if self.floor.as_ref().is_none_or(|floor| key < *floor) {
            self.floor = Some(key);
        }
        self.checked_at = Some(recorded_at.to_owned());
        if payload["late"].as_bool() == Some(true) {
            return;
        }
        for removed in payload["removed"].as_array().into_iter().flatten() {
            if let Some(name) = removed["name"].as_str() {
                self.failures.remove(name);
            }
        }
        let finding_id = payload["finding_id"].as_i64();
        for name in payload["added"].as_array().into_iter().flatten() {
            if let Some(name) = name.as_str() {
                self.failures.insert(
                    name.to_owned(),
                    KnownFailure {
                        name: name.to_owned(),
                        kind: FailureKind::of(name),
                        added: Added {
                            run_id: run.run_id,
                            attempt,
                            sha: run.sha.clone(),
                            url: run.url.clone(),
                            at: recorded_at.to_owned(),
                        },
                        finding_id,
                    },
                );
            }
        }
        let state = payload["state"].as_str().and_then(CiState::parse);
        match state {
            Some(CiState::Green) => {
                self.last_green = Some(run.clone());
                self.red_since = None;
                self.jobs_missing = None;
            }
            Some(CiState::Red) if self.state != Some(CiState::Red) => {
                self.red_since = Some((run.clone(), text("created_at")));
            }
            _ => {}
        }
        if state.is_some() {
            self.state = state;
        }
        self.latest = Some(LatestRun {
            run_id: run.run_id,
            attempt,
            sha: run.sha,
            url: run.url,
            conclusion: text("conclusion"),
            created_at: text("created_at"),
        });
    }

    /// The [`CI_JOBS_MISSING`] that stands as an attention against
    /// `named`, the jobs `required_jobs` of `[ci_watch]` names now (empty
    /// without the table; `None` when `dagq.toml` cannot be read, which
    /// keeps it): [`Self::jobs_missing`] while `named` still names one of
    /// its jobs. Once the setting names none of them (taken out of
    /// `required_jobs`, the key emptied, the table removed) the
    /// disagreement is fixed and no green run is waited for; the list and
    /// the state are not touched, so nothing reads green by it.
    pub fn standing_jobs_missing(&self, named: Option<&[String]>) -> Option<&Value> {
        let payload = self.jobs_missing.as_ref()?;
        let Some(named) = named else {
            return Some(payload);
        };
        payload["jobs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|job| named.iter().any(|name| name == job))
            .then_some(payload)
    }

    /// The [`CI_JOBS_MISSING`] payload for `run`, which lacks the named
    /// jobs `missing`; none while the same ones stand missing (no green
    /// since), so one disagreement is told once.
    pub fn jobs_missing_event(
        &self,
        run: &CiRun,
        missing: &[String],
        config: &CiWatchConfig,
        branch: &str,
    ) -> Option<Value> {
        let same = self
            .jobs_missing
            .as_ref()
            .is_some_and(|last| last["jobs"] == json!(missing));
        (!same).then(|| {
            json!({
                "workflow": config.workflow,
                "branch": branch,
                "run_id": run.run_id,
                "attempt": run.attempt,
                "sha": run.sha,
                "url": run.url,
                "jobs": missing,
                "required_jobs": config.required_jobs,
                "message": format!(
                    "the CI run {} of {} has no job {} that required_jobs of [ci_watch] names: no success run is read green until dagq.toml or the workflow is fixed",
                    run.url,
                    config.workflow,
                    missing.join(", ")
                ),
            })
        })
    }

    /// The creation time and run ID of the latest recorded run.
    fn head(&self) -> Option<(&str, i64)> {
        self.latest
            .as_ref()
            .map(|run| (run.created_at.as_str(), run.run_id))
    }

    /// Whether `run` was created before the latest recorded run (and is
    /// not a re-run of it): it ended late, and a later-created run already
    /// decided the list and the state.
    pub fn is_late(&self, run: &CiRun) -> bool {
        self.latest.as_ref().is_some_and(|head| {
            head.run_id != run.run_id && run.key() < (head.created_at.as_str(), head.run_id)
        })
    }
}

/// Whether the supervisor's build contains a red range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BinaryContains {
    /// It contains the first red commit, and so the whole range.
    All,
    /// It contains part of the range.
    Some,
    /// It is at the last green or before it.
    None,
    /// A release, a build that does not name its commit, or ancestry that
    /// could not be read.
    Unknown,
}

/// Whether the commit `named` (the build's, [`crate::build_id::named_commit`])
/// contains the range `from..to` (`from` the last green, `None` when there
/// is none), `is_ancestor(a, b)` telling whether `a` is `b` or its
/// ancestor (`None` when Git could not tell).
pub fn binary_contains(
    named: Option<&str>,
    from: Option<&str>,
    to: &str,
    is_ancestor: impl Fn(&str, &str) -> Option<bool>,
) -> BinaryContains {
    let Some(named) = named else {
        return BinaryContains::Unknown;
    };
    if is_ancestor(to, named) == Some(true) {
        return BinaryContains::All;
    }
    let Some(from) = from else {
        return BinaryContains::Unknown;
    };
    if is_ancestor(named, from) == Some(true) {
        return BinaryContains::None;
    }
    if is_ancestor(from, named) == Some(true) && is_ancestor(named, to) == Some(true) {
        return BinaryContains::Some;
    }
    BinaryContains::Unknown
}

/// The range a red run's fix task carries.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RangeFacts {
    /// `git rev-list --count from..to`; `None` without a last green or
    /// when Git could not count.
    pub commits: Option<u64>,
    pub binary: Option<BinaryContains>,
    /// The commit the supervisor's build names.
    pub binary_commit: Option<String>,
}

/// The `ci_failure` finding a red run with new failures records.
#[derive(Debug, Clone, PartialEq)]
pub struct FailureFinding {
    /// `ci_failure:<key>` ([`failure_key`]).
    pub subject: String,
    pub summary: String,
    /// What the fix task carries, one JSON object.
    pub detail: Value,
    pub propose: String,
}

/// What processing one settled run records.
#[derive(Debug, Clone, PartialEq)]
pub struct RunDecision {
    /// The `ci_checked` payload; the store adds `finding_id`.
    pub checked: Value,
    /// The `ci_turned_red` payload; the store adds `finding_ids`.
    pub turned_red: Option<Value>,
    pub turned_green: Option<Value>,
    pub finding: Option<FailureFinding>,
    /// The findings whose every item left the list with this run, with
    /// the reason they are resolved for.
    pub resolved: Vec<(i64, String)>,
}

/// What the store writes for one settled run, in one transaction.
#[derive(Debug, Clone)]
pub struct CiCheckRecord {
    /// The event ID of the latest `ci_checked` the caller read (`None` for
    /// none): another supervisor's record since then takes the run.
    pub previous: Option<i64>,
    pub checked: Value,
    pub turned_red: Option<Value>,
    pub turned_green: Option<Value>,
    /// The finding of the new failures; its evidence is the events above.
    pub finding: Option<super::NewFinding>,
    /// The findings to resolve, each with its reason: only an `open` one
    /// with no planner of the runtime's open for it.
    pub resolve: Vec<(i64, String)>,
}

/// What the store wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CiCheckRecorded {
    pub event: super::EventId,
    pub finding: Option<super::FindingId>,
    pub resolved: Vec<super::FindingId>,
}

/// One settled run to process.
#[derive(Debug, Clone)]
pub struct RunInput<'a> {
    pub run: &'a CiRun,
    pub state: CiState,
    /// The runs skipped since the last settled one: run ID and attempt.
    pub skipped: &'a [(i64, i64)],
    /// Those of them that ended `success` but did not read green, with
    /// why ([`read_green`]).
    pub undecided: &'a [(i64, i64, Undecided)],
    pub junit: &'a Junit,
    pub failed_jobs: &'a [FailedJob],
    /// The list returned was full and its oldest run came after the last
    /// recorded one.
    pub gap: bool,
}

/// What `input` changes in the list `watch` holds and records: the tests
/// it adds (a red run's failures not on the list; the failed jobs' steps
/// when the run's JUnit is missing or names no failed test), the ones it
/// removes (a test its JUnit shows passing; all of them on a green run),
/// the turn to red or green, the finding of the added ones, and the
/// findings left with no item. A run created before the latest recorded
/// one (and not a re-run of it, [`WatchState::is_late`]) is recorded
/// `late` and changes nothing: no item added or removed, no turn, no
/// finding, so it neither rolls the list back nor overrides the later
/// run's decision. A re-run of the latest one is decided like a new run.
pub fn decide(
    watch: &WatchState,
    input: &RunInput<'_>,
    facts: &RangeFacts,
    workflow: &str,
    branch: &str,
) -> RunDecision {
    let run = input.run;
    let late = watch.is_late(run);
    let failed_tests = input.junit.failed();
    let mut removed: Vec<(String, &'static str)> = Vec::new();
    let mut added: Vec<String> = Vec::new();
    match input.state {
        _ if late => {}
        CiState::Green => {
            removed.extend(watch.failures.keys().map(|name| (name.clone(), "green")));
        }
        CiState::Red => {
            removed.extend(
                watch
                    .failures
                    .values()
                    .filter(|item| item.kind == FailureKind::Test && input.junit.passed(&item.name))
                    .map(|item| (item.name.clone(), "passed")),
            );
            let mut failing: BTreeSet<String> = failed_tests.iter().cloned().collect();
            if failed_tests.is_empty() {
                failing.extend(input.failed_jobs.iter().flat_map(FailedJob::items));
            }
            added.extend(
                failing
                    .into_iter()
                    .filter(|name| !watch.failures.contains_key(name)),
            );
        }
    }
    let gone: BTreeSet<&str> = removed.iter().map(|(name, _)| name.as_str()).collect();
    let left = watch.failures.len() - removed.len() + added.len();
    let mut checked = json!({
        "workflow": workflow,
        "branch": branch,
        "run_id": run.run_id,
        "run_number": run.run_number,
        "attempt": run.attempt,
        "sha": run.sha,
        "url": run.url,
        "created_at": run.created_at,
        "conclusion": run.conclusion,
        "state": input.state.as_str(),
        "skipped_runs": input.skipped.iter().map(|(id, _)| id).collect::<Vec<_>>(),
        "skipped_attempts": input.skipped.iter().map(|(_, attempt)| attempt).collect::<Vec<_>>(),
        "junit": input.junit.as_str(),
        "failed_tests": failed_tests,
        "failed_jobs": input.failed_jobs,
        "added": added,
        "removed": removed
            .iter()
            .map(|(name, reason)| json!({"name": name, "reason": reason}))
            .collect::<Vec<_>>(),
        "known_failures": left,
    });
    if input.gap {
        checked["gap"] = json!(true);
    }
    if !input.undecided.is_empty() {
        checked["undecided"] = input
            .undecided
            .iter()
            .map(|(run_id, attempt, why)| {
                let mut entry = json!(why);
                entry["run_id"] = json!(run_id);
                entry["attempt"] = json!(attempt);
                entry
            })
            .collect();
    }
    if late {
        checked["late"] = json!(true);
        return RunDecision {
            checked,
            turned_red: None,
            turned_green: None,
            finding: None,
            resolved: Vec::new(),
        };
    }
    let this =
        json!({"run_id": run.run_id, "attempt": run.attempt, "sha": run.sha, "url": run.url});
    let turned_red =
        (input.state == CiState::Red && watch.state != Some(CiState::Red)).then(|| {
            json!({
                "workflow": workflow,
                "branch": branch,
                "run_id": run.run_id,
                "sha": run.sha,
                "url": run.url,
                "last_green": watch.last_green,
            })
        });
    let turned_green =
        (input.state == CiState::Green && watch.state == Some(CiState::Red)).then(|| {
            let (since, since_at) = watch.red_since.clone().unwrap_or_else(|| {
                (
                    RunRef {
                        run_id: run.run_id,
                        sha: run.sha.clone(),
                        url: run.url.clone(),
                    },
                    run.created_at.clone(),
                )
            });
            let secs = match (
                super::stats::rfc3339_millis(&since_at),
                super::stats::rfc3339_millis(&run.created_at),
            ) {
                (Some(from), Some(to)) => Some((to - from).max(0) / 1000),
                _ => None,
            };
            json!({
                "workflow": workflow,
                "branch": branch,
                "run_id": run.run_id,
                "sha": run.sha,
                "url": run.url,
                "red_since": since,
                "red_secs": secs,
            })
        });
    let finding = (!added.is_empty()).then(|| {
        let from = watch.last_green.as_ref().map(|green| green.sha.clone());
        let since = from.as_deref().unwrap_or(&run.sha);
        FailureFinding {
            subject: failure_key(&added),
            summary: format!(
                "CI {workflow} on {branch} fails: {} new failure(s) since {}",
                added.len(),
                short_sha(since)
            ),
            detail: json!({
                "tests": added,
                "failed_jobs": input.failed_jobs,
                "range": {"from": from, "to": run.sha, "commits": facts.commits},
                "url": run.url,
                "binary_contains": facts.binary.unwrap_or(BinaryContains::Unknown),
                "binary_commit": facts.binary_commit,
                "run": this,
            }),
            propose: format!("CI on {branch} is red; a fix task is needed"),
        }
    });
    let mut emptied: BTreeSet<i64> = watch
        .failures
        .values()
        .filter(|item| gone.contains(item.name.as_str()))
        .filter_map(|item| item.finding_id)
        .collect();
    for item in watch.failures.values() {
        if !gone.contains(item.name.as_str())
            && let Some(id) = item.finding_id
        {
            emptied.remove(&id);
        }
    }
    RunDecision {
        checked,
        turned_red,
        turned_green,
        finding,
        resolved: emptied
            .into_iter()
            .map(|id| (id, format!("its tests passed on {}", short_sha(&run.sha))))
            .collect(),
    }
}

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// The key of a set of new failures: the SHA-256 of their names, sorted
/// and joined by `\n`, its first 16 hex digits after `ci_failure:`.
pub fn failure_key(names: &[String]) -> String {
    let mut sorted: Vec<&str> = names.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let digest = Sha256::digest(sorted.join("\n").as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{FINDING_KIND}:{}", &hex[..16])
}

/// `<owner>/<name>` of a GitHub remote URL (`https://github.com/o/n(.git)`,
/// `git@github.com:o/n(.git)`, `ssh://git@github.com/o/n(.git)`); `None`
/// for any other host.
pub fn github_repo(url: &str) -> Option<String> {
    let url = url.trim();
    let path = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, name) = path.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .then(|| format!("{owner}/{name}"))
}

/// Why the means to read GitHub are not there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unavailable {
    /// `gh` does not resolve on the PATH.
    GhMissing,
    /// `gh auth status` fails.
    GhUnauthenticated,
    /// The push remote is not a GitHub URL.
    NotGithub,
}

impl Unavailable {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GhMissing => "gh_missing",
            Self::GhUnauthenticated => "gh_unauthenticated",
            Self::NotGithub => "not_github",
        }
    }

    /// What a person does about it.
    pub const fn next(self) -> AttentionNext {
        match self {
            Self::GhMissing => AttentionNext::InstallTool,
            Self::GhUnauthenticated => AttentionNext::LogInToGh,
            Self::NotGithub => AttentionNext::FixDagqToml,
        }
    }

    /// The [`AttentionNext`] of a recorded `reason`.
    pub fn next_of(reason: Option<&str>) -> AttentionNext {
        match reason {
            Some("gh_unauthenticated") => AttentionNext::LogInToGh,
            Some("not_github") => AttentionNext::FixDagqToml,
            _ => AttentionNext::InstallTool,
        }
    }
}

/// What one look at the means to read GitHub found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Available {
        program: String,
        resolved: String,
        repo: String,
    },
    Unavailable {
        reason: Unavailable,
        program: String,
        /// The PATH `gh` was looked up in.
        path: String,
        message: String,
    },
}

impl Access {
    /// Why it is not there, with what a person does: for `up`'s error and
    /// the attention.
    pub fn message(reason: Unavailable, program: &str, path: &str, detail: &str) -> String {
        let what = match reason {
            Unavailable::GhMissing => format!(
                "the GitHub CLI {program:?} that [ci_watch] of dagq.toml needs is not found (PATH: {path}); install it where this PATH finds it"
            ),
            Unavailable::GhUnauthenticated => format!(
                "the GitHub CLI {program:?} that [ci_watch] of dagq.toml needs is not logged in to github.com ({detail}); a person runs `gh auth login`"
            ),
            Unavailable::NotGithub => format!(
                "[ci_watch] of dagq.toml needs a GitHub remote, but {detail}; fix the remote or [repository] remote"
            ),
        };
        format!(
            "{what}, or register a task that takes [ci_watch] out of dagq.toml; no task is claimed and no run lands meanwhile"
        )
    }

    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }

    /// The event the supervisor records when this answer differs from the
    /// latest one on the queue (`last`, its kind and payload, one of
    /// [`CI_WATCH_ACCESS_KINDS`]): unavailable after anything but the same
    /// reason, available after unavailable, else none.
    pub fn transition(&self, last: Option<(&str, &Value)>) -> Option<(EventKind, Value)> {
        match self {
            Self::Unavailable {
                reason,
                program,
                path,
                message,
            } => {
                let same = last.is_some_and(|(kind, payload)| {
                    kind == CI_WATCH_UNAVAILABLE && payload["reason"] == reason.as_str()
                });
                (!same).then(|| {
                    (
                        EventKind::CiWatchUnavailable,
                        json!({
                            "reason": reason.as_str(),
                            "program": program,
                            "path": path,
                            "message": message,
                        }),
                    )
                })
            }
            Self::Available {
                program, resolved, ..
            } => last
                .is_some_and(|(kind, _)| kind == CI_WATCH_UNAVAILABLE)
                .then(|| {
                    (
                        EventKind::CiWatchAvailable,
                        json!({"program": program, "resolved": resolved}),
                    )
                }),
        }
    }
}

/// The run of the same passing failures (a network error, a non-zero
/// `gh` other than for authentication, a timeout).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FailureStreak {
    error: Option<String>,
    failures: usize,
    since: i64,
    recorded: bool,
}

impl FailureStreak {
    /// Count a failure at `now` (unix seconds); the `ci_check_failed`
    /// payload when the same error reaches [`CI_WATCH_FAILURE_LIMIT`], once
    /// per streak.
    pub fn fail(&mut self, error: &str, now: i64) -> Option<Value> {
        if self.error.as_deref() != Some(error) {
            *self = Self {
                error: Some(error.to_owned()),
                failures: 0,
                since: now,
                recorded: false,
            };
        }
        self.failures += 1;
        (self.failures >= CI_WATCH_FAILURE_LIMIT && !self.recorded).then(|| {
            self.recorded = true;
            json!({"error": error, "failures": self.failures, "since": self.since})
        })
    }

    /// A check that went through ends the streak.
    pub fn succeed(&mut self) {
        *self = Self::default();
    }
}

/// `watch` of `ci failures` and `status`: whether the watch reads.
pub fn watch_label(enabled: bool, last_access: Option<&str>) -> &'static str {
    if last_access == Some(CI_WATCH_UNAVAILABLE) {
        "unavailable"
    } else if enabled {
        "available"
    } else {
        "disabled"
    }
}

/// `dagq ci failures`: the list as `watch` holds it, the items of the
/// findings in `kept` (a fix task's own) moved to `kept_for_task`.
pub fn failures_view(
    enabled: bool,
    workflow: Option<&str>,
    branch: Option<&str>,
    watch: &WatchState,
    last_access: Option<&str>,
    kept: &BTreeSet<i64>,
) -> Value {
    if !enabled && !watch.recorded {
        return json!({"enabled": false, "state": "unknown", "watch": "disabled", "failures": []});
    }
    let (kept_items, failures): (Vec<&KnownFailure>, Vec<&KnownFailure>) = watch
        .failures
        .values()
        .partition(|item| item.finding_id.is_some_and(|id| kept.contains(&id)));
    json!({
        "enabled": enabled,
        "workflow": workflow,
        "branch": branch,
        "state": watch.state.map_or("unknown", CiState::as_str),
        "watch": watch_label(enabled, last_access),
        "checked_at": watch.checked_at,
        "latest_run": watch.latest,
        "failures": failures,
        "kept_for_task": kept_items,
    })
}

/// `status`'s `ci`: null until the watch recorded anything.
pub fn status_view(enabled: bool, watch: &WatchState, last_access: Option<&str>) -> Value {
    if !watch.recorded {
        return Value::Null;
    }
    json!({
        "state": watch.state.map_or("unknown", CiState::as_str),
        "watch": watch_label(enabled, last_access),
        "failures": watch.failures.len(),
        "checked_at": watch.checked_at,
        "latest_run_url": watch.latest.as_ref().map(|run| run.url.clone()),
    })
}

/// Whether a job or step of a red run counts as failed: it failed, timed
/// out, did not start, or was cancelled (a timeout cancels the job it
/// stopped).
pub fn job_failed(conclusion: &str) -> bool {
    matches!(
        conclusion,
        "failure" | "timed_out" | "cancelled" | "startup_failure"
    )
}

/// What reading the push remote's URL gives (`git remote get-url`):
/// `None` when Git did not run (a passing failure, `Err`), `Some((false,
/// _))` when the remote is unknown, else the URL. `Ok(Ok(repo))` for a
/// GitHub repository, `Ok(Err(detail))` for a setting that is not GitHub's
/// ([`Unavailable::NotGithub`]).
pub fn remote_repo(
    remote: &str,
    read: Option<(bool, &str)>,
) -> Result<Result<String, String>, String> {
    match read {
        None => Err(format!("git remote get-url {remote} did not run")),
        Some((false, _)) => Ok(Err(format!("the remote {remote} does not resolve"))),
        Some((true, url)) => {
            let url = url.trim();
            Ok(github_repo(url).ok_or_else(|| format!("the remote {remote} is {url}")))
        }
    }
}

/// Whether a check is due: at once when none started in this process
/// (`since_start` none), else once `interval` passed since the last start
/// (`since_start`, measured on the supervisor's monotonic clock).
pub fn check_due(since_start: Option<Duration>, interval: Duration) -> bool {
    since_start.is_none_or(|elapsed| elapsed >= interval)
}

/// Whether the means to read the CI are there after a check that failed
/// on the way (a passing failure): as the queue's latest
/// `ci_watch_unavailable` / `ci_watch_available` says (`last`, its kind),
/// there when none is recorded. The check may have recorded the means'
/// return before it failed.
pub fn available_after_failure(last: Option<&str>) -> bool {
    last != Some(CI_WATCH_UNAVAILABLE)
}

/// Why a supervisor with `[ci_watch]` (`watched`) holds its claims, resumes
/// and landings for the watch, by the means' state at its last answer
/// (`available`): `pending` before its first answer, `unreadable` while the
/// means are missing; `None` while it does not hold.
pub fn hold_reason(watched: bool, available: Option<bool>) -> Option<&'static str> {
    match (watched, available) {
        (false, _) | (true, Some(true)) => None,
        (true, None) => Some("pending"),
        (true, Some(false)) => Some("unreadable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::EventId;

    fn run(id: i64, conclusion: &str, at: &str) -> CiRun {
        CiRun {
            run_id: id,
            run_number: id,
            sha: format!("sha{id}"),
            conclusion: conclusion.into(),
            url: format!("https://github.com/o/n/actions/runs/{id}"),
            created_at: at.into(),
            attempt: 1,
        }
    }

    /// The watch after `ci_checked` of `runs` (settled, attempt 1) in order.
    fn recorded(runs: &[&CiRun]) -> WatchState {
        let events: Vec<RunEvent> = runs
            .iter()
            .enumerate()
            .map(|(index, run)| {
                event(
                    index as i64 + 1,
                    CI_CHECKED,
                    json!({"run_id": run.run_id, "attempt": run.attempt, "sha": run.sha,
                           "url": run.url, "created_at": run.created_at,
                           "conclusion": run.conclusion, "state": "green",
                           "added": [], "removed": []}),
                )
            })
            .collect();
        WatchState::fold(&events)
    }

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: format!("2026-10-06T00:00:{id:02}Z"),
            actor: None,
        }
    }

    fn tests(pairs: &[(&str, TestOutcome)]) -> Junit {
        Junit::Read(
            pairs
                .iter()
                .map(|(name, outcome)| ((*name).to_owned(), *outcome))
                .collect(),
        )
    }

    /// Process `runs` in order from `watch`, folding each decision back as
    /// the store would record it (`finding_id` the run's ID).
    fn process(watch: &mut WatchState, decisions: &mut Vec<RunDecision>, input: RunInput<'_>) {
        let decision = decide(watch, &input, &RangeFacts::default(), "ci.yml", "main");
        let mut checked = decision.checked.clone();
        if decision.finding.is_some() {
            checked["finding_id"] = json!(input.run.run_id);
        }
        watch.apply(&checked, "2026-10-06T00:00:00Z");
        watch.recorded = true;
        decisions.push(decision);
    }

    #[test]
    fn conclusions_settle_green_red_or_nothing() {
        assert_eq!(classify("success"), Some(CiState::Green));
        assert_eq!(classify("failure"), Some(CiState::Red));
        assert_eq!(classify("timed_out"), Some(CiState::Red));
        for skipped in [
            "cancelled",
            "skipped",
            "neutral",
            "action_required",
            "startup_failure",
            "stale",
            "",
        ] {
            assert_eq!(classify(skipped), None, "{skipped}");
        }
    }

    #[test]
    fn the_first_check_takes_the_newest_run_and_later_ones_what_follows() {
        let runs = vec![
            run(3, "success", "2026-10-06T03:00:00Z"),
            run(1, "success", "2026-10-06T01:00:00Z"),
            run(2, "failure", "2026-10-06T02:00:00Z"),
        ];
        let (first, gap) = runs_to_process(runs.clone(), &WatchState::default());
        assert_eq!(first.iter().map(|r| r.run_id).collect::<Vec<_>>(), [3]);
        assert!(!gap);
        let (next, gap) = runs_to_process(runs.clone(), &recorded(&[&runs[1]]));
        assert_eq!(next.iter().map(|r| r.run_id).collect::<Vec<_>>(), [2, 3]);
        assert!(!gap);
        // The runs created before the first one recorded are not read back.
        let (none, _) = runs_to_process(runs.clone(), &recorded(&[&runs[0]]));
        assert!(none.is_empty());
        // The same run at the same attempt is processed once, skipped
        // ones too; a skipped one's other attempt is new.
        let mut watch = recorded(&[&runs[1]]);
        watch.processed.insert((2, 1));
        watch.processed.insert((3, 1));
        assert!(runs_to_process(runs.clone(), &watch).0.is_empty());
        let mut again = runs.clone();
        again[0].attempt = 2;
        let (rerun, _) = runs_to_process(again, &watch);
        assert_eq!(
            rerun
                .iter()
                .map(|r| (r.run_id, r.attempt))
                .collect::<Vec<_>>(),
            [(3, 2)]
        );
    }

    #[test]
    fn a_run_that_ends_after_a_later_one_is_read_late_and_changes_nothing() {
        // Run 2 was created before run 3 but ended after it: run 3 was
        // recorded first.
        let one = run(1, "success", "2026-10-06T01:00:00Z");
        let two = run(2, "failure", "2026-10-06T02:00:00Z");
        let three = run(3, "failure", "2026-10-06T03:00:00Z");
        let mut watch = WatchState::default();
        let mut decisions = Vec::new();
        for (run, state, junit) in [
            (&one, CiState::Green, Junit::NotConfigured),
            (&three, CiState::Red, tests(&[("t a", TestOutcome::Failed)])),
        ] {
            process(
                &mut watch,
                &mut decisions,
                RunInput {
                    run,
                    state,
                    skipped: &[],
                    undecided: &[],
                    junit: &junit,
                    failed_jobs: &[],
                    gap: false,
                },
            );
        }
        let (pending, _) = runs_to_process(vec![one.clone(), three.clone(), two.clone()], &watch);
        assert_eq!(pending.iter().map(|r| r.run_id).collect::<Vec<_>>(), [2]);
        assert!(watch.is_late(&two));
        let before = watch.clone();
        process(
            &mut watch,
            &mut decisions,
            RunInput {
                run: &two,
                state: CiState::Red,
                skipped: &[],
                undecided: &[],
                junit: &tests(&[("t a", TestOutcome::Passed), ("t b", TestOutcome::Failed)]),
                failed_jobs: &[],
                gap: false,
            },
        );
        let late = &decisions[2];
        assert_eq!(late.checked["late"], true);
        assert_eq!(late.checked["run_id"], 2);
        assert_eq!(late.checked["attempt"], 1);
        assert_eq!(late.checked["state"], "red");
        assert_eq!(late.checked["failed_tests"], json!(["t b"]));
        assert_eq!(late.checked["added"], json!([]));
        assert_eq!(late.checked["removed"], json!([]));
        assert_eq!(late.checked["known_failures"], 1);
        assert!(late.turned_red.is_none() && late.turned_green.is_none());
        assert!(late.finding.is_none() && late.resolved.is_empty());
        // Recorded: processed, never again; the list and the state stay.
        assert!(runs_to_process(vec![one, two, three], &watch).0.is_empty());
        assert_eq!(watch.failures, before.failures);
        assert_eq!(watch.latest, before.latest);
        assert_eq!(watch.state, before.state);
        assert_eq!(watch.last_green, before.last_green);
        // A late green does not empty the list either.
        let mut early = run(0, "success", "2026-10-06T02:30:00Z");
        early.run_id = 9;
        let green = decide(
            &watch,
            &RunInput {
                run: &early,
                state: CiState::Green,
                skipped: &[],
                undecided: &[],
                junit: &Junit::NotConfigured,
                failed_jobs: &[],
                gap: false,
            },
            &RangeFacts::default(),
            "ci.yml",
            "main",
        );
        assert_eq!(green.checked["late"], true);
        assert_eq!(green.checked["removed"], json!([]));
        assert!(green.turned_green.is_none() && green.resolved.is_empty());
    }

    #[test]
    fn a_green_re_run_of_the_latest_red_empties_the_list() {
        let one = run(1, "success", "2026-10-06T01:00:00Z");
        let mut two = run(2, "failure", "2026-10-06T02:00:00Z");
        let mut watch = WatchState::default();
        let mut decisions = Vec::new();
        for (run, state, junit) in [
            (&one, CiState::Green, Junit::NotConfigured),
            (&two, CiState::Red, tests(&[("t a", TestOutcome::Failed)])),
        ] {
            process(
                &mut watch,
                &mut decisions,
                RunInput {
                    run,
                    state,
                    skipped: &[],
                    undecided: &[],
                    junit: &junit,
                    failed_jobs: &[],
                    gap: false,
                },
            );
        }
        assert_eq!(watch.failures["t a"].added.attempt, 1);
        // The re-run keeps the ID and the creation time; its attempt is new.
        two.attempt = 2;
        two.conclusion = "success".into();
        let (pending, _) = runs_to_process(vec![one.clone(), two.clone()], &watch);
        assert_eq!(
            pending
                .iter()
                .map(|r| (r.run_id, r.attempt))
                .collect::<Vec<_>>(),
            [(2, 2)]
        );
        assert!(!watch.is_late(&two));
        process(
            &mut watch,
            &mut decisions,
            RunInput {
                run: &two,
                state: CiState::Green,
                skipped: &[],
                undecided: &[],
                junit: &Junit::NotConfigured,
                failed_jobs: &[],
                gap: false,
            },
        );
        let rerun = &decisions[2];
        assert!(rerun.checked.get("late").is_none());
        assert_eq!(rerun.checked["attempt"], 2);
        assert_eq!(
            rerun.checked["removed"],
            json!([{"name": "t a", "reason": "green"}])
        );
        assert_eq!(
            rerun.turned_green.as_ref().unwrap()["red_since"]["run_id"],
            2
        );
        assert_eq!(
            rerun.resolved.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [2]
        );
        assert!(watch.failures.is_empty());
        assert_eq!(watch.state, Some(CiState::Green));
        assert_eq!(
            watch.latest.as_ref().map(|l| (l.run_id, l.attempt)),
            Some((2, 2))
        );
        assert!(runs_to_process(vec![one, two], &watch).0.is_empty());
    }

    #[test]
    fn skipped_runs_are_processed_with_their_attempts() {
        let events = vec![event(
            1,
            CI_CHECKED,
            json!({"run_id": 5, "attempt": 1, "created_at": "2026-10-06T05:00:00Z",
                   "state": "green", "skipped_runs": [3, 4], "skipped_attempts": [2],
                   "added": [], "removed": []}),
        )];
        let watch = WatchState::fold(&events);
        // An older record without `skipped_attempts` reads attempt 1.
        assert_eq!(watch.processed, BTreeSet::from([(3, 2), (4, 1), (5, 1)]));
        assert_eq!(watch.last_event, Some(1));
    }

    #[test]
    fn a_full_list_newer_than_the_last_record_is_a_gap() {
        let runs: Vec<CiRun> = (0..RUN_LIST_LIMIT as i64)
            .map(|i| run(100 + i, "success", &format!("2026-10-07T00:{:02}:00Z", i)))
            .collect();
        let last = recorded(&[&run(1, "success", "2026-10-06T00:00:00Z")]);
        let (all, gap) = runs_to_process(runs.clone(), &last);
        assert_eq!(all.len(), RUN_LIST_LIMIT);
        assert!(gap);
        // Not full: the runs in between are simply none.
        let (_, gap) = runs_to_process(runs[1..].to_vec(), &last);
        assert!(!gap);
        // Full but reaching back to the last record: no gap.
        let mut reaching = runs[1..].to_vec();
        reaching.push(run(1, "success", "2026-10-06T00:00:00Z"));
        let (rest, gap) = runs_to_process(reaching, &last);
        assert_eq!(rest.len(), RUN_LIST_LIMIT - 1);
        assert!(!gap);
    }

    #[test]
    fn junit_names_tests_by_classname_and_name_and_reads_their_outcome() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites name="nextest-run" tests="5" failures="1" errors="1">
  <testsuite name="dagq::it" tests="4">
    <testcase name="runtime_claim::claims_in_order" classname="dagq::it" time="0.5"/>
    <testcase name="runtime_claim::fails" classname="dagq::it" time="0.1">
      <failure type="test failure">assertion &lt;left&gt; failed</failure>
      <system-out>out</system-out>
    </testcase>
    <testcase name="cli::errs" classname="dagq::it"><error message="x"/></testcase>
    <testcase name="cli::later" classname="dagq::it"><skipped/></testcase>
    <testcase name="cli::flaky" classname="dagq::it"><flakyFailure message="once"/></testcase>
  </testsuite>
  <testsuite name="dagq"><testcase name="a &amp; b" classname=""></testcase></testsuite>
</testsuites>"#;
        assert_eq!(
            parse_junit(xml).unwrap(),
            vec![
                (
                    "dagq::it runtime_claim::claims_in_order".to_owned(),
                    TestOutcome::Passed
                ),
                (
                    "dagq::it runtime_claim::fails".to_owned(),
                    TestOutcome::Failed
                ),
                ("dagq::it cli::errs".to_owned(), TestOutcome::Failed),
                ("dagq::it cli::later".to_owned(), TestOutcome::Skipped),
                ("dagq::it cli::flaky".to_owned(), TestOutcome::Passed),
                ("a & b".to_owned(), TestOutcome::Passed),
            ]
        );
        assert!(parse_junit("not xml at all").is_err());
        assert!(parse_junit("<testsuite><testcase classname='x'/></testsuite>").is_err());
        assert!(parse_junit("<testsuite><testcase name='x'>").is_err());
    }

    #[test]
    fn a_test_fails_if_any_job_failed_it_and_passes_if_any_ran_it() {
        let merged = merge_outcomes([
            vec![
                ("a".to_owned(), TestOutcome::Passed),
                ("b".to_owned(), TestOutcome::Skipped),
                ("c".to_owned(), TestOutcome::Skipped),
            ],
            vec![
                ("a".to_owned(), TestOutcome::Failed),
                ("b".to_owned(), TestOutcome::Passed),
            ],
        ]);
        assert_eq!(merged["a"], TestOutcome::Failed);
        assert_eq!(merged["b"], TestOutcome::Passed);
        assert_eq!(merged["c"], TestOutcome::Skipped);
    }

    #[test]
    fn green_to_red_adds_the_new_failures_records_the_turn_and_one_finding() {
        let mut watch = WatchState::default();
        let mut decisions = Vec::new();
        let green = run(1, "success", "2026-10-06T01:00:00Z");
        process(
            &mut watch,
            &mut decisions,
            RunInput {
                run: &green,
                state: CiState::Green,
                skipped: &[],
                undecided: &[],
                junit: &Junit::NotConfigured,
                failed_jobs: &[],
                gap: false,
            },
        );
        assert_eq!(decisions[0].turned_red, None);
        assert_eq!(decisions[0].finding, None);
        let red = run(2, "failure", "2026-10-06T02:00:00Z");
        let junit = tests(&[("t a", TestOutcome::Failed), ("t b", TestOutcome::Passed)]);
        let jobs = [FailedJob {
            job: "test".into(),
            steps: vec!["nextest".into()],
        }];
        let decision = decide(
            &watch,
            &RunInput {
                run: &red,
                state: CiState::Red,
                skipped: &[],
                undecided: &[],
                junit: &junit,
                failed_jobs: &jobs,
                gap: false,
            },
            &RangeFacts {
                commits: Some(3),
                binary: Some(BinaryContains::Some),
                binary_commit: Some("abc".into()),
            },
            "ci.yml",
            "main",
        );
        assert_eq!(decision.checked["added"], json!(["t a"]));
        assert_eq!(decision.checked["failed_tests"], json!(["t a"]));
        assert_eq!(decision.checked["junit"], "read");
        assert_eq!(decision.checked["known_failures"], 1);
        let turned = decision.turned_red.clone().unwrap();
        assert_eq!(turned["last_green"]["sha"], "sha1");
        let finding = decision.finding.clone().unwrap();
        assert_eq!(finding.subject, failure_key(&["t a".to_owned()]));
        assert_eq!(
            finding.summary,
            "CI ci.yml on main fails: 1 new failure(s) since sha1"
        );
        assert_eq!(
            finding.detail["range"],
            json!({"from": "sha1", "to": "sha2", "commits": 3})
        );
        assert_eq!(finding.detail["binary_contains"], "some");
        assert_eq!(finding.detail["binary_commit"], "abc");
        assert_eq!(finding.detail["tests"], json!(["t a"]));
        assert_eq!(finding.detail["url"], red.url);
        assert_eq!(finding.propose, "CI on main is red; a fix task is needed");
    }

    #[test]
    fn a_red_that_goes_on_with_the_same_failures_records_no_new_finding() {
        let mut watch = WatchState::default();
        let mut decisions = Vec::new();
        let junit = tests(&[("t a", TestOutcome::Failed)]);
        for (id, at) in [(1, "2026-10-06T01:00:00Z"), (2, "2026-10-06T02:00:00Z")] {
            let red = run(id, "failure", at);
            process(
                &mut watch,
                &mut decisions,
                RunInput {
                    run: &red,
                    state: CiState::Red,
                    skipped: &[],
                    undecided: &[],
                    junit: &junit,
                    failed_jobs: &[],
                    gap: false,
                },
            );
        }
        assert!(decisions[0].finding.is_some());
        assert!(decisions[0].turned_red.is_some());
        assert_eq!(decisions[1].finding, None);
        assert_eq!(decisions[1].turned_red, None);
        assert_eq!(decisions[1].checked["added"], json!([]));
        assert_eq!(watch.failures["t a"].added.run_id, 1);
        assert_eq!(watch.failures["t a"].finding_id, Some(1));
        // One more failing test is a new set with a key of its own.
        let more = tests(&[("t a", TestOutcome::Failed), ("t c", TestOutcome::Failed)]);
        let red = run(3, "failure", "2026-10-06T03:00:00Z");
        process(
            &mut watch,
            &mut decisions,
            RunInput {
                run: &red,
                state: CiState::Red,
                skipped: &[],
                undecided: &[],
                junit: &more,
                failed_jobs: &[],
                gap: false,
            },
        );
        let finding = decisions[2].finding.clone().unwrap();
        assert_eq!(finding.subject, failure_key(&["t c".to_owned()]));
        assert_ne!(
            finding.subject,
            decisions[0].finding.clone().unwrap().subject
        );
    }

    #[test]
    fn a_test_leaves_the_list_when_it_passes_and_everything_on_green() {
        let mut watch = WatchState::default();
        let mut decisions = Vec::new();
        let jobs = [FailedJob {
            job: "clippy".into(),
            steps: vec!["cargo clippy".into()],
        }];
        let red = run(1, "failure", "2026-10-06T01:00:00Z");
        process(
            &mut watch,
            &mut decisions,
            RunInput {
                run: &red,
                state: CiState::Red,
                skipped: &[],
                undecided: &[],
                junit: &tests(&[("t a", TestOutcome::Failed), ("t b", TestOutcome::Failed)]),
                failed_jobs: &jobs,
                gap: false,
            },
        );
        assert_eq!(watch.failures.len(), 2);
        // JUnit missing: nothing leaves; the failed job and step is an item.
        let red = run(2, "failure", "2026-10-06T02:00:00Z");
        process(
            &mut watch,
            &mut decisions,
            RunInput {
                run: &red,
                state: CiState::Red,
                skipped: &[],
                undecided: &[],
                junit: &Junit::Missing,
                failed_jobs: &jobs,
                gap: false,
            },
        );
        assert_eq!(decisions[1].checked["removed"], json!([]));
        assert_eq!(
            decisions[1].checked["added"],
            json!(["job:clippy/step:cargo clippy"])
        );
        assert_eq!(watch.failures.len(), 3);
        // `t a` passes, `t b` did not run: only `t a` leaves; its finding
        // keeps `t b`, so nothing is resolved.
        let red = run(3, "failure", "2026-10-06T03:00:00Z");
        process(
            &mut watch,
            &mut decisions,
            RunInput {
                run: &red,
                state: CiState::Red,
                skipped: &[],
                undecided: &[],
                junit: &tests(&[("t a", TestOutcome::Passed), ("t z", TestOutcome::Failed)]),
                failed_jobs: &jobs,
                gap: false,
            },
        );
        assert_eq!(
            decisions[2].checked["removed"],
            json!([{"name": "t a", "reason": "passed"}])
        );
        assert!(decisions[2].resolved.is_empty());
        assert!(!watch.failures.contains_key("t a"));
        // Green: everything leaves, the turn is recorded, every finding
        // left with no item is resolved.
        let green = run(5, "success", "2026-10-06T05:00:00Z");
        process(
            &mut watch,
            &mut decisions,
            RunInput {
                run: &green,
                state: CiState::Green,
                skipped: &[(4, 1)],
                undecided: &[],
                junit: &Junit::Missing,
                failed_jobs: &[],
                gap: false,
            },
        );
        let last = &decisions[3];
        assert_eq!(last.checked["known_failures"], 0);
        assert_eq!(last.checked["skipped_runs"], json!([4]));
        assert!(
            last.checked["removed"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["reason"] == "green")
        );
        let turned = last.turned_green.clone().unwrap();
        assert_eq!(turned["red_since"]["run_id"], 1);
        assert_eq!(turned["red_secs"], 4 * 3600);
        assert_eq!(
            last.resolved.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(last.resolved[0].1, "its tests passed on sha5");
        assert!(watch.failures.is_empty());
        assert_eq!(watch.state, Some(CiState::Green));
        assert_eq!(watch.last_green.as_ref().unwrap().run_id, 5);
    }

    #[test]
    fn a_cancelled_run_widens_the_range_of_the_next_settled_one() {
        // The skipped run takes no part: the next red's range starts at the
        // last green, before the cancelled run.
        let mut watch = WatchState::default();
        let mut decisions = Vec::new();
        let green = run(1, "success", "2026-10-06T01:00:00Z");
        process(
            &mut watch,
            &mut decisions,
            RunInput {
                run: &green,
                state: CiState::Green,
                skipped: &[],
                undecided: &[],
                junit: &Junit::NotConfigured,
                failed_jobs: &[],
                gap: false,
            },
        );
        let red = run(3, "failure", "2026-10-06T03:00:00Z");
        let decision = decide(
            &watch,
            &RunInput {
                run: &red,
                state: CiState::Red,
                skipped: &[(2, 1)],
                undecided: &[],
                junit: &tests(&[("t a", TestOutcome::Failed)]),
                failed_jobs: &[],
                gap: false,
            },
            &RangeFacts::default(),
            "ci.yml",
            "main",
        );
        assert_eq!(decision.checked["skipped_runs"], json!([2]));
        assert_eq!(
            decision.finding.unwrap().detail["range"],
            json!({"from": "sha1", "to": "sha3", "commits": null})
        );
    }

    #[test]
    fn a_red_without_failed_tests_lists_its_failed_steps() {
        let jobs = [
            FailedJob {
                job: "lint".into(),
                steps: vec!["fmt".into(), "clippy".into()],
            },
            FailedJob {
                job: "build".into(),
                steps: Vec::new(),
            },
        ];
        let red = run(1, "failure", "2026-10-06T01:00:00Z");
        let decision = decide(
            &WatchState::default(),
            &RunInput {
                run: &red,
                state: CiState::Red,
                skipped: &[],
                undecided: &[],
                junit: &tests(&[("t a", TestOutcome::Passed)]),
                failed_jobs: &jobs,
                gap: true,
            },
            &RangeFacts::default(),
            "ci.yml",
            "main",
        );
        assert_eq!(
            decision.checked["added"],
            json!(["job:build", "job:lint/step:clippy", "job:lint/step:fmt"])
        );
        assert_eq!(decision.checked["gap"], true);
        // No last green: the range starts nowhere and the turn has none.
        assert_eq!(decision.turned_red.unwrap()["last_green"], Value::Null);
        assert_eq!(
            decision.finding.unwrap().detail["range"]["from"],
            Value::Null
        );
    }

    #[test]
    fn the_failure_key_ignores_the_order_of_the_names() {
        let key = failure_key(&["b".to_owned(), "a".to_owned()]);
        assert_eq!(key, failure_key(&["a".to_owned(), "b".to_owned()]));
        assert!(key.starts_with("ci_failure:"));
        assert_eq!(key.len(), "ci_failure:".len() + 16);
        // SHA-256("a\nb").
        assert_eq!(key, "ci_failure:7e18f737311b2dc3");
    }

    #[test]
    fn the_binary_contains_all_some_none_or_unknown_of_the_range() {
        // History: g (green) -> m -> r (red) -> later.
        let order = ["g", "m", "r", "later"];
        let ancestor = |a: &str, b: &str| {
            let (a, b) = (
                order.iter().position(|c| *c == a)?,
                order.iter().position(|c| *c == b)?,
            );
            Some(a <= b)
        };
        assert_eq!(
            binary_contains(Some("later"), Some("g"), "r", ancestor),
            BinaryContains::All
        );
        assert_eq!(
            binary_contains(Some("r"), Some("g"), "r", ancestor),
            BinaryContains::All
        );
        assert_eq!(
            binary_contains(Some("m"), Some("g"), "r", ancestor),
            BinaryContains::Some
        );
        assert_eq!(
            binary_contains(Some("g"), Some("g"), "r", ancestor),
            BinaryContains::None
        );
        assert_eq!(
            binary_contains(None, Some("g"), "r", ancestor),
            BinaryContains::Unknown
        );
        assert_eq!(
            binary_contains(Some("m"), None, "r", ancestor),
            BinaryContains::Unknown
        );
        assert_eq!(
            binary_contains(Some("elsewhere"), Some("g"), "r", ancestor),
            BinaryContains::Unknown
        );
    }

    #[test]
    fn github_remotes_name_their_repository() {
        for url in [
            "https://github.com/hisamekms/dagq",
            "https://github.com/hisamekms/dagq.git",
            "git@github.com:hisamekms/dagq.git",
            "ssh://git@github.com/hisamekms/dagq.git",
        ] {
            assert_eq!(github_repo(url).as_deref(), Some("hisamekms/dagq"), "{url}");
        }
        for url in [
            "https://gitlab.com/o/n.git",
            "/tmp/repo.git",
            "https://github.com/o",
            "https://github.com/o/n/extra",
        ] {
            assert_eq!(github_repo(url), None, "{url}");
        }
    }

    #[test]
    fn the_access_is_recorded_when_its_answer_changes() {
        let missing = Access::Unavailable {
            reason: Unavailable::GhMissing,
            program: "gh".into(),
            path: "/bin".into(),
            message: "m".into(),
        };
        let available = Access::Available {
            program: "gh".into(),
            resolved: "/bin/gh".into(),
            repo: "o/n".into(),
        };
        let (kind, payload) = missing.transition(None).unwrap();
        assert_eq!(kind, EventKind::CiWatchUnavailable);
        assert_eq!(payload["reason"], "gh_missing");
        assert_eq!(
            missing.transition(Some((CI_WATCH_UNAVAILABLE, &payload))),
            None
        );
        let unauthenticated = Access::Unavailable {
            reason: Unavailable::GhUnauthenticated,
            program: "gh".into(),
            path: "/bin".into(),
            message: "m".into(),
        };
        assert!(
            unauthenticated
                .transition(Some((CI_WATCH_UNAVAILABLE, &payload)))
                .is_some()
        );
        let (kind, back) = available
            .transition(Some((CI_WATCH_UNAVAILABLE, &payload)))
            .unwrap();
        assert_eq!(kind, EventKind::CiWatchAvailable);
        assert_eq!(back["resolved"], "/bin/gh");
        assert_eq!(available.transition(None), None);
        assert_eq!(
            available.transition(Some((CI_WATCH_AVAILABLE, &back))),
            None
        );
        assert_eq!(Unavailable::GhMissing.next(), AttentionNext::InstallTool);
        assert_eq!(
            Unavailable::GhUnauthenticated.next(),
            AttentionNext::LogInToGh
        );
        assert_eq!(Unavailable::NotGithub.next(), AttentionNext::FixDagqToml);
        assert_eq!(
            Unavailable::next_of(Some("not_github")),
            AttentionNext::FixDagqToml
        );
        let message = Access::message(Unavailable::GhUnauthenticated, "gh", "/bin", "x");
        assert!(message.contains("gh auth login"), "{message}");
        assert!(message.contains("[ci_watch]"), "{message}");
    }

    #[test]
    fn the_same_passing_failure_is_recorded_once_at_the_limit() {
        let mut streak = FailureStreak::default();
        assert_eq!(streak.fail("net", 10), None);
        assert_eq!(streak.fail("net", 20), None);
        let payload = streak.fail("net", 30).unwrap();
        assert_eq!(payload, json!({"error": "net", "failures": 3, "since": 10}));
        assert_eq!(streak.fail("net", 40), None);
        // Another error starts again.
        assert_eq!(streak.fail("other", 50), None);
        streak.succeed();
        assert_eq!(streak.fail("other", 60), None);
    }

    #[test]
    fn the_views_read_the_folded_events() {
        let events = vec![
            event(
                1,
                CI_CHECKED,
                json!({"run_id": 7, "sha": "s7", "url": "u7", "conclusion": "failure",
                       "created_at": "2026-10-06T01:00:00Z", "state": "red",
                       "added": ["t a", "job:x/step:y"], "removed": [], "finding_id": 4}),
            ),
            event(2, CI_TURNED_RED, json!({})),
            event(3, CI_WATCH_UNAVAILABLE, json!({"reason": "gh_missing"})),
        ];
        let watch = WatchState::fold(&events);
        assert_eq!(watch.state, Some(CiState::Red));
        assert_eq!(watch.failures["job:x/step:y"].kind, FailureKind::JobStep);
        let view = failures_view(
            true,
            Some("ci.yml"),
            Some("main"),
            &watch,
            Some(CI_WATCH_UNAVAILABLE),
            &BTreeSet::new(),
        );
        assert_eq!(view["state"], "red");
        assert_eq!(view["watch"], "unavailable");
        assert_eq!(view["latest_run"]["run_id"], 7);
        assert_eq!(view["failures"].as_array().unwrap().len(), 2);
        assert_eq!(view["failures"][1]["name"], "t a");
        assert_eq!(view["failures"][1]["kind"], "test");
        assert_eq!(view["failures"][1]["finding_id"], 4);
        assert_eq!(
            view["failures"][1]["added"],
            json!({"run_id": 7, "attempt": 1, "sha": "s7", "url": "u7", "at": "2026-10-06T00:00:01Z"})
        );
        let kept = failures_view(
            true,
            Some("ci.yml"),
            Some("main"),
            &watch,
            None,
            &BTreeSet::from([4]),
        );
        assert_eq!(kept["failures"], json!([]));
        assert_eq!(kept["kept_for_task"].as_array().unwrap().len(), 2);
        assert_eq!(kept["watch"], "available");
        let status = status_view(true, &watch, None);
        assert_eq!(
            status,
            json!({"state": "red", "watch": "available", "failures": 2,
                   "checked_at": "2026-10-06T00:00:01Z", "latest_run_url": "u7"})
        );
        // Nothing recorded: no `ci` in status, and a disabled view.
        assert_eq!(
            status_view(false, &WatchState::default(), None),
            Value::Null
        );
        assert_eq!(
            failures_view(
                false,
                None,
                None,
                &WatchState::default(),
                None,
                &BTreeSet::new()
            ),
            json!({"enabled": false, "state": "unknown", "watch": "disabled", "failures": []})
        );
    }

    #[test]
    fn failed_timed_out_cancelled_and_unstarted_jobs_count_as_failed() {
        for failed in ["failure", "timed_out", "cancelled", "startup_failure"] {
            assert!(job_failed(failed), "{failed}");
        }
        for other in ["success", "skipped", "neutral", ""] {
            assert!(!job_failed(other), "{other}");
        }
    }

    #[test]
    fn a_git_that_did_not_run_is_passing_and_an_unknown_remote_is_the_setting() {
        assert_eq!(
            remote_repo("origin", None),
            Err("git remote get-url origin did not run".to_owned())
        );
        assert_eq!(
            remote_repo("origin", Some((false, ""))),
            Ok(Err("the remote origin does not resolve".to_owned()))
        );
        assert_eq!(
            remote_repo("origin", Some((true, "/tmp/repo.git\n"))),
            Ok(Err("the remote origin is /tmp/repo.git".to_owned()))
        );
        assert_eq!(
            remote_repo("up", Some((true, "git@github.com:o/n.git\n"))),
            Ok(Ok("o/n".to_owned()))
        );
    }

    #[test]
    fn a_check_is_due_at_once_and_then_after_the_interval() {
        let interval = Duration::from_secs(600);
        assert!(check_due(None, interval));
        assert!(!check_due(Some(Duration::ZERO), interval));
        // Exactly at the interval, and a millisecond before.
        assert!(!check_due(
            Some(interval - Duration::from_millis(1)),
            interval
        ));
        assert!(check_due(Some(interval), interval));
        assert!(check_due(Some(interval * 2), interval));
    }

    #[test]
    fn after_a_passing_failure_the_queue_s_last_answer_stands() {
        assert!(available_after_failure(None));
        assert!(available_after_failure(Some(CI_WATCH_AVAILABLE)));
        assert!(!available_after_failure(Some(CI_WATCH_UNAVAILABLE)));
    }

    #[test]
    fn the_watch_holds_until_an_answer_finds_the_means() {
        assert_eq!(hold_reason(false, None), None);
        assert_eq!(hold_reason(false, Some(false)), None);
        assert_eq!(hold_reason(true, None), Some("pending"));
        assert_eq!(hold_reason(true, Some(false)), Some("unreadable"));
        assert_eq!(hold_reason(true, Some(true)), None);
    }

    fn job(name: &str, conclusion: &str) -> CiJob {
        CiJob {
            name: name.into(),
            conclusion: conclusion.into(),
        }
    }

    /// ADR-t2034-1 decisions 4 and 5: a success run is green only when
    /// every named job ran to `success`; a skipped one, a missing one and
    /// jobs that stay unreadable up to the limit are not green, and fewer
    /// failures to read retry.
    #[test]
    fn a_success_run_is_green_only_when_every_named_job_succeeded() {
        let required = vec!["rust".to_owned(), "linux".to_owned()];
        let ran = [
            job("docs", "success"),
            job("rust", "success"),
            job("linux", "success"),
        ];
        assert_eq!(read_green(&[], Err("x"), 1), GreenReading::Green);
        assert_eq!(read_green(&required, Ok(&ran), 0), GreenReading::Green);
        let docs_only = [
            job("docs", "success"),
            job("rust", "skipped"),
            job("linux", "skipped"),
        ];
        assert_eq!(
            read_green(&required, Ok(&docs_only), 0),
            GreenReading::Undecided(Undecided::JobsNotPassed {
                jobs: vec![job("rust", "skipped"), job("linux", "skipped")]
            })
        );
        // A job named twice (a matrix) passes only when each did.
        let matrix = [
            job("rust", "success"),
            job("linux", "success"),
            job("linux", "neutral"),
        ];
        assert_eq!(
            read_green(&required, Ok(&matrix), 0),
            GreenReading::Undecided(Undecided::JobsNotPassed {
                jobs: vec![job("linux", "neutral")]
            })
        );
        // A missing job outweighs a skipped one.
        let renamed = [job("docs", "success"), job("rust", "skipped")];
        assert_eq!(
            read_green(&required, Ok(&renamed), 0),
            GreenReading::Undecided(Undecided::JobsMissing {
                jobs: vec!["linux".into()]
            })
        );
        assert_eq!(
            read_green(&required, Ok(&[]), 0),
            GreenReading::Undecided(Undecided::JobsMissing {
                jobs: required.clone()
            })
        );
        for failures in 1..CI_WATCH_FAILURE_LIMIT {
            assert_eq!(
                read_green(&required, Err("HTTP 502"), failures),
                GreenReading::Retry
            );
        }
        assert_eq!(
            read_green(&required, Err("HTTP 502"), CI_WATCH_FAILURE_LIMIT),
            GreenReading::Undecided(Undecided::JobsUnreadable {
                error: "HTTP 502".into(),
                failures: CI_WATCH_FAILURE_LIMIT
            })
        );
    }

    #[test]
    fn unread_jobs_count_on_for_each_run_and_attempt() {
        let first = run(7, "success", "2026-10-06T01:00:00Z");
        let second = run(8, "success", "2026-10-06T02:00:00Z");
        let rerun = CiRun {
            attempt: 2,
            ..first.clone()
        };
        // Each check counts on from the last one's, run by run.
        let mut last = JobsUnread::default();
        for check in 1..=CI_WATCH_FAILURE_LIMIT + 3 {
            let mut next = JobsUnread::default();
            let capped = check.min(CI_WATCH_FAILURE_LIMIT + 1);
            assert_eq!(next.fail(&last, &first), capped);
            assert_eq!(next.fail(&last, &second), capped);
            last = next;
        }
        assert!(matches!(
            read_green(&["rust".into()], Err("e"), last.failures(&first)),
            GreenReading::Undecided(Undecided::JobsUnreadable { .. })
        ));
        // Another attempt counts from one; a run not failed in a check
        // drops out of it.
        let mut next = JobsUnread::default();
        assert_eq!(next.fail(&last, &rerun), 1);
        assert_eq!(next.failures(&first), 0);
        assert_eq!(next.failures(&second), 0);
    }

    /// A success run that did not read green is skipped like a cancelled
    /// one: no turn to green, nothing off the list, and the next settled
    /// run counts it in `skipped_runs` with why in `undecided`.
    #[test]
    fn an_undecided_success_keeps_the_list_and_the_next_settled_run_takes_it() {
        let mut watch = WatchState::default();
        let mut decisions = Vec::new();
        let red = run(1, "failure", "2026-10-06T01:00:00Z");
        process(
            &mut watch,
            &mut decisions,
            RunInput {
                run: &red,
                state: CiState::Red,
                skipped: &[],
                undecided: &[],
                junit: &tests(&[("t a", TestOutcome::Failed)]),
                failed_jobs: &[],
                gap: false,
            },
        );
        let skipped = Undecided::JobsNotPassed {
            jobs: vec![job("rust", "skipped")],
        };
        let missing = Undecided::JobsMissing {
            jobs: vec!["linux".into()],
        };
        let red_again = run(4, "failure", "2026-10-06T04:00:00Z");
        process(
            &mut watch,
            &mut decisions,
            RunInput {
                run: &red_again,
                state: CiState::Red,
                skipped: &[(2, 1), (3, 1)],
                undecided: &[(2, 1, skipped), (3, 1, missing)],
                junit: &tests(&[("t a", TestOutcome::Failed)]),
                failed_jobs: &[],
                gap: false,
            },
        );
        let checked = &decisions[1].checked;
        assert_eq!(checked["skipped_runs"], json!([2, 3]));
        assert_eq!(
            checked["undecided"],
            json!([
                {"run_id": 2, "attempt": 1, "reason": "jobs_not_passed",
                 "jobs": [{"name": "rust", "conclusion": "skipped"}]},
                {"run_id": 3, "attempt": 1, "reason": "jobs_missing", "jobs": ["linux"]},
            ])
        );
        assert!(decisions[1].turned_green.is_none());
        assert_eq!(checked["removed"], json!([]));
        assert_eq!(watch.failures.len(), 1);
        assert_eq!(watch.state, Some(CiState::Red));
        assert!(watch.processed.contains(&(2, 1)) && watch.processed.contains(&(3, 1)));
        // Without undecided runs the field is not written.
        assert!(decisions[0].checked.get("undecided").is_none());
    }

    /// One disagreement of `required_jobs` with the workflow is told once
    /// until a green run is recorded; other missing jobs are told anew.
    #[test]
    fn a_missing_job_is_told_once_until_a_green_run() {
        let config = CiWatchConfig {
            workflow: "ci.yml".into(),
            branch: None,
            interval_secs: DEFAULT_INTERVAL_SECS,
            junit_artifacts: Vec::new(),
            required_jobs: vec!["rust".into(), "linux".into()],
        };
        let success = run(2, "success", "2026-10-06T02:00:00Z");
        let linux = vec!["linux".to_owned()];
        let mut watch = WatchState::default();
        let told = watch
            .jobs_missing_event(&success, &linux, &config, "main")
            .unwrap();
        assert_eq!(told["jobs"], json!(["linux"]));
        assert_eq!(told["required_jobs"], json!(["rust", "linux"]));
        assert_eq!(told["run_id"], 2);
        assert!(told["message"].as_str().unwrap().contains("required_jobs"));
        let mut events = vec![event(1, CI_JOBS_MISSING, told)];
        watch = WatchState::fold(&events);
        assert!(watch.recorded && watch.jobs_missing.is_some());
        let later = run(3, "success", "2026-10-06T03:00:00Z");
        assert!(
            watch
                .jobs_missing_event(&later, &linux, &config, "main")
                .is_none()
        );
        assert!(
            watch
                .jobs_missing_event(&later, &config.required_jobs, &config, "main")
                .is_some()
        );
        // A red run does not end it; a green one does.
        events.push(event(
            2,
            CI_CHECKED,
            json!({"run_id": 4, "created_at": "2026-10-06T04:00:00Z", "state": "red"}),
        ));
        assert!(WatchState::fold(&events).jobs_missing.is_some());
        events.push(event(
            3,
            CI_CHECKED,
            json!({"run_id": 5, "created_at": "2026-10-06T05:00:00Z", "state": "green"}),
        ));
        watch = WatchState::fold(&events);
        assert!(watch.jobs_missing.is_none());
        assert!(
            watch
                .jobs_missing_event(&later, &linux, &config, "main")
                .is_some()
        );
    }

    /// A missing job stands while the setting names it (or cannot be
    /// read), and no longer once it names none of the missing jobs, with
    /// the list and the red state left as they were.
    #[test]
    fn a_missing_job_stands_only_while_the_setting_names_it() {
        let told = json!({"jobs": ["linux", "macos"], "message": "no job linux, macos"});
        let red = json!({
            "run_id": 4,
            "created_at": "2026-10-06T04:00:00Z",
            "state": "red",
            "added": ["dagq::it a::fails"],
        });
        let watch = WatchState::fold(&[
            event(1, CI_CHECKED, red),
            event(2, CI_JOBS_MISSING, told.clone()),
        ]);
        let names = |names: &[&str]| names.iter().map(|&n| n.to_owned()).collect::<Vec<_>>();
        let rust_linux = names(&["rust", "linux"]);
        let macos = names(&["macos"]);
        let rust = names(&["rust"]);
        assert_eq!(watch.standing_jobs_missing(None), Some(&told));
        assert_eq!(watch.standing_jobs_missing(Some(&rust_linux)), Some(&told));
        assert_eq!(watch.standing_jobs_missing(Some(&macos)), Some(&told));
        // Taken out of required_jobs, the key emptied, the table removed.
        assert_eq!(watch.standing_jobs_missing(Some(&rust)), None);
        assert_eq!(watch.standing_jobs_missing(Some(&[])), None);
        // The fold is the same: still red, the list kept.
        assert_eq!(watch.state, Some(CiState::Red));
        assert!(watch.failures.contains_key("dagq::it a::fails"));
        assert!(watch.jobs_missing.is_some());
        assert_eq!(
            WatchState::default().standing_jobs_missing(Some(&rust)),
            None
        );
    }
}
