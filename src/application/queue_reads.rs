//! The queue's reads as the queue service's read use cases take them
//! ([Queue service], ADR-t1233-5 decisions 1 to 3): one [`QueueRead`] per
//! read command of the command line a read role's job or a worker runs
//! (`list`, `events`, `timeline`, `stats`, `kpi`, `forecast`, `marks`,
//! `search`, `related`, `findings`, `goal show`, ...), with the command
//! line's options as its params and their defaults. The command line and
//! the service both answer one with [`answer`] (through
//! `compose::read_queue`, which opens nothing and passes the queue and the
//! host's [`QueueReadSources`]), so a read gives the same JSON either way.
//!
//! The params name no program to run and no file to write: the service's
//! own cmux lists the workspaces, the service's `graph` draws no SVG
//! (the host's d2 would run), and `watch`, `report`, `graph --out`,
//! `locate` and `doctor` are no read use case.
//!
//! [Queue service]: ../../docs/design/queue-service.md

use anyhow::Result;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use crate::application::forecast::ForecastQuery;
use crate::application::{
    AskQuery, AskStore, Clock, EventReads, ObserverLog, QueueRecords, RunLog, StatusFilter,
    TaskQuery, TaskStore, claim_candidates, dependency_graph, observer, watch,
};
use crate::domain::kpi::{CompareSpec, KpiQuery};
use crate::domain::queue_service::UseCase;
use crate::domain::search::{self, SearchQuery};
use crate::domain::stats::{Cursor, StatsQuery};
use crate::domain::{
    ChangeSet, EventFilter, EventId, FindingId, FindingQuery, FindingTarget, GoalDetail, GoalId,
    NoteQuery, Proposal, ProposalId, Resource, RunId, SessionRole, TaskId, TaskStatus,
};

/// A read of the queue, as one read use case of the service takes it.
#[derive(Debug, Clone, PartialEq)]
pub enum QueueRead {
    List(ListRead),
    Candidates,
    Graph(GraphRead),
    Status(RoleRead),
    Asks(AsksRead),
    Events(EventsRead),
    Timeline(TimelineRead),
    Stats(StatsRead),
    Kpi(KpiRead),
    Forecast(ForecastRead),
    Notes(NotesRead),
    Marks(MarksRead),
    Findings(FindingsRead),
    Search(SearchRead),
    Related(RelatedRead),
    GoalList(GoalListRead),
    GoalShow(GoalShowRead),
    Lint(LintRead),
    ObserveHistory(ObserveHistoryRead),
    ObserveInput(ObserveInputRead),
}

/// `list`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRead {
    #[serde(default)]
    pub status: Vec<String>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub goal: Option<i64>,
    #[serde(default = "twenty")]
    pub limit: u32,
    #[serde(default)]
    pub before: Option<i64>,
    #[serde(default)]
    pub full: bool,
}

/// `graph`'s options, without `--out`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphRead {
    #[serde(default)]
    pub goal: Option<i64>,
    /// `json`, `d2` or `svg`.
    #[serde(default = "json_format")]
    pub format: String,
}

/// `status`'s option: the role whose attention to show.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleRead {
    #[serde(default)]
    pub role: Option<String>,
}

/// `asks`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AsksRead {
    #[serde(default)]
    pub open: bool,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub all: bool,
}

/// `events`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsRead {
    #[serde(default)]
    pub after: i64,
    #[serde(default = "hundred")]
    pub limit: u32,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub full: bool,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub task: Option<i64>,
    #[serde(default)]
    pub goal: Option<i64>,
    #[serde(default)]
    pub kind: Vec<String>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
}

/// `timeline`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimelineRead {
    pub run: String,
    #[serde(default = "default_gap")]
    pub gap: i64,
    #[serde(default)]
    pub full: bool,
}

/// `stats`'s options, without `--cmux`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatsRead {
    #[serde(default, deserialize_with = "cursor")]
    pub since: Option<Cursor>,
    #[serde(default, deserialize_with = "cursor")]
    pub until: Option<Cursor>,
    #[serde(default)]
    pub goal: Option<i64>,
    #[serde(default)]
    pub full: bool,
}

/// `kpi`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KpiRead {
    /// `day` or `week`.
    #[serde(default = "day")]
    pub period: String,
    #[serde(default = "seven")]
    pub last: u16,
    #[serde(default, deserialize_with = "cursor")]
    pub at: Option<Cursor>,
    #[serde(default, deserialize_with = "cursor")]
    pub since: Option<Cursor>,
    #[serde(default, deserialize_with = "cursor")]
    pub until: Option<Cursor>,
    #[serde(default, rename = "change")]
    pub changes: Vec<String>,
    #[serde(default, rename = "area")]
    pub areas: Vec<String>,
    #[serde(default)]
    pub by: Vec<String>,
    #[serde(default)]
    pub cross: bool,
    #[serde(default, deserialize_with = "compare")]
    pub compare: Option<CompareSpec>,
    #[serde(default = "default_window")]
    pub window: i64,
    #[serde(default)]
    pub goal: Option<i64>,
}

/// `forecast`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForecastRead {
    #[serde(default)]
    pub task: Option<i64>,
    #[serde(default)]
    pub goal: Option<i64>,
    #[serde(default)]
    pub parallel: Option<u16>,
    #[serde(default = "default_trials")]
    pub trials: u32,
}

/// `notes`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotesRead {
    #[serde(default)]
    pub goal: Option<i64>,
    #[serde(default)]
    pub task: Option<i64>,
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default = "twenty")]
    pub limit: u32,
}

/// `marks`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarksRead {
    #[serde(default, deserialize_with = "cursor")]
    pub since: Option<Cursor>,
    #[serde(default, deserialize_with = "cursor")]
    pub until: Option<Cursor>,
}

/// `findings`'s options: at most one target (`task`, `run`, `goal` or
/// `queue: true`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindingsRead {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub status: Vec<String>,
    #[serde(default, rename = "kind")]
    pub kinds: Vec<String>,
    #[serde(default)]
    pub task: Option<i64>,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub goal: Option<i64>,
    #[serde(default)]
    pub queue: bool,
    #[serde(default)]
    pub full: bool,
}

/// `search`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchRead {
    pub query: String,
    #[serde(default)]
    pub status: Vec<String>,
    #[serde(default, rename = "kind")]
    pub kinds: Vec<String>,
    #[serde(default)]
    pub goal: Option<i64>,
    #[serde(default = "twenty")]
    pub limit: u32,
    #[serde(default)]
    pub full: bool,
}

/// `related`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelatedRead {
    pub task: i64,
    #[serde(default)]
    pub status: Vec<String>,
    #[serde(default = "ten")]
    pub limit: u32,
}

/// `goal list`'s options: the tags a goal must carry one of (ADR-t1639-1
/// decision 7), none for every goal.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalListRead {
    #[serde(default)]
    pub tag: Vec<String>,
}

impl GoalListRead {
    fn tags(&self) -> Result<Vec<crate::domain::GoalTag>> {
        Ok(self
            .tag
            .iter()
            .map(|tag| tag.parse())
            .collect::<Result<_, _>>()?)
    }
}

/// `goal show`'s options.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalShowRead {
    pub id: i64,
    #[serde(default)]
    pub full: bool,
}

/// `lint`'s targets: tasks and the members of proposals.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LintRead {
    #[serde(default)]
    pub tasks: Vec<i64>,
    #[serde(default)]
    pub proposals: Vec<i64>,
}

/// `observe --history`'s option.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserveHistoryRead {
    #[serde(default = "history_limit")]
    pub limit: usize,
}

/// `observe --input`'s options: the observation's directory name, the
/// section's path, and the page of a list.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserveInputRead {
    pub observation: String,
    #[serde(default)]
    pub section: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default = "input_page")]
    pub limit: usize,
}

const fn input_page() -> usize {
    crate::application::observer::INPUT_PAGE
}

const fn ten() -> u32 {
    10
}
const fn twenty() -> u32 {
    20
}
const fn hundred() -> u32 {
    100
}
const fn seven() -> u16 {
    7
}
fn day() -> String {
    "day".to_owned()
}
fn json_format() -> String {
    "json".to_owned()
}
const fn default_gap() -> i64 {
    crate::domain::timeline::DEFAULT_GAP_SECS
}
const fn default_window() -> i64 {
    crate::domain::kpi::DEFAULT_WINDOW_DAYS
}
#[allow(clippy::cast_possible_truncation)]
const fn default_trials() -> u32 {
    crate::domain::forecast::DEFAULT_TRIALS as u32
}
const fn history_limit() -> usize {
    crate::application::observer::HISTORY_LIMIT
}

/// A cursor written as the command line takes it: an event id, `@<unix
/// seconds>` or an RFC 3339 time (a bare number is an event id too).
fn cursor<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Cursor>, D::Error> {
    let text = match Option::<Value>::deserialize(deserializer)? {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(text)) => text,
        Some(Value::Number(number)) => number.to_string(),
        Some(other) => {
            return Err(serde::de::Error::custom(format!(
                "a cursor is a string or an event id, not {other}"
            )));
        }
    };
    text.parse().map(Some).map_err(serde::de::Error::custom)
}

/// `--compare` as the command line takes it.
fn compare<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<CompareSpec>, D::Error> {
    let text = match Option::<Value>::deserialize(deserializer)? {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(text)) => text,
        Some(Value::Number(number)) => number.to_string(),
        Some(other) => {
            return Err(serde::de::Error::custom(format!(
                "a comparison is a string, not {other}"
            )));
        }
    };
    text.parse().map(Some).map_err(serde::de::Error::custom)
}

/// The roles `status --role` and `asks --role` take.
pub const READ_ROLES: [&str; 2] = ["inbox", "planner"];
/// The statuses `findings --status` takes.
pub const FINDING_STATUSES: [&str; 4] = ["open", "proposed", "resolved", "dismissed"];
/// The kinds `search --kind` takes.
pub const SEARCH_KINDS: [&str; 4] = ["task", "goal", "note", "commit"];
/// The formats the service's `graph` takes: not `svg`, which the host's
/// d2 draws, so that no read starts a program on the service's side.
pub const GRAPH_FORMATS: [&str; 2] = ["json", "d2"];

/// A `kpi --change`: a task change, or `unknown` for the tasks without one.
pub fn parse_kpi_change(value: &str) -> Result<String, String> {
    use crate::domain::TaskChange;
    if value == TaskChange::NONE {
        return Ok(value.to_owned());
    }
    value
        .parse::<TaskChange>()
        .map(String::from)
        .map_err(|error| error.to_string())
}

/// A `kpi --area`: an area's name, `unknown` or `other`.
pub fn parse_kpi_area(value: &str) -> Result<String, String> {
    use crate::domain::areas::{OTHER, UNKNOWN, check_name};
    if value == UNKNOWN || value == OTHER {
        return Ok(value.to_owned());
    }
    check_name(value).map(|()| value.to_owned())
}

fn run_id(what: &str, run: Option<&String>) -> Result<()> {
    match run {
        Some(run) => crate::domain::RunId::new(run.clone())
            .map(drop)
            .map_err(|error| bad(format!("{what}: {error}"))),
        None => Ok(()),
    }
}
/// The axes `kpi --by` takes.
pub const KPI_AXES: [&str; 15] = [
    "change",
    "area",
    "build",
    "parallel",
    "slot",
    "load",
    "toolchain",
    "claude",
    "provider",
    "route",
    "codex",
    "group",
    "model",
    "effort",
    "nature",
];

/// What makes a read's params not the command line's: what clap refuses
/// before the command runs.
#[derive(Debug)]
pub struct BadRead(pub String);

impl std::fmt::Display for BadRead {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadRead {}

fn bad(text: impl Into<String>) -> anyhow::Error {
    BadRead(text.into()).into()
}

fn one_of(what: &str, value: &str, allowed: &[&str]) -> Result<()> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(bad(format!(
            "{what} is one of {}, not {value:?}",
            allowed.join(", ")
        )))
    }
}

fn at_least_one(what: &str, value: u32) -> Result<()> {
    if value >= 1 {
        Ok(())
    } else {
        Err(bad(format!("{what} is 1 or more")))
    }
}

impl QueueRead {
    /// The read `use_case` asks for with `params`: `None` for a use case
    /// that is no read. Params the command line would refuse are a
    /// [`BadRead`].
    pub fn parse(use_case: UseCase, params: &Value) -> Result<Option<Self>> {
        fn read<T: for<'de> Deserialize<'de>>(use_case: UseCase, params: &Value) -> Result<T> {
            let params = if params.is_null() {
                &Value::Object(serde_json::Map::new())
            } else {
                params
            };
            serde_json::from_value(params.clone()).map_err(|error| {
                bad(format!(
                    "the params of {} do not read: {error}",
                    use_case.as_str()
                ))
            })
        }
        fn none(use_case: UseCase, params: &Value) -> Result<()> {
            match params {
                Value::Null => Ok(()),
                Value::Object(object) if object.is_empty() => Ok(()),
                _ => Err(bad(format!("{} takes no params", use_case.as_str()))),
            }
        }
        let read = match use_case {
            UseCase::List => Self::List(read(use_case, params)?),
            UseCase::Candidates => {
                none(use_case, params)?;
                Self::Candidates
            }
            UseCase::Graph => Self::Graph(read(use_case, params)?),
            UseCase::Status => Self::Status(read(use_case, params)?),
            UseCase::Asks => Self::Asks(read(use_case, params)?),
            UseCase::Events => Self::Events(read(use_case, params)?),
            UseCase::Timeline => Self::Timeline(read(use_case, params)?),
            UseCase::Stats => Self::Stats(read(use_case, params)?),
            UseCase::Kpi => Self::Kpi(read(use_case, params)?),
            UseCase::Forecast => Self::Forecast(read(use_case, params)?),
            UseCase::Notes => Self::Notes(read(use_case, params)?),
            UseCase::Marks => Self::Marks(read(use_case, params)?),
            UseCase::Findings => Self::Findings(read(use_case, params)?),
            UseCase::Search => Self::Search(read(use_case, params)?),
            UseCase::Related => Self::Related(read(use_case, params)?),
            UseCase::GoalList => Self::GoalList(read(use_case, params)?),
            UseCase::GoalShow => Self::GoalShow(read(use_case, params)?),
            UseCase::Lint => Self::Lint(read(use_case, params)?),
            UseCase::ObserveHistory => Self::ObserveHistory(read(use_case, params)?),
            UseCase::ObserveInput => Self::ObserveInput(read(use_case, params)?),
            UseCase::Hello
            | UseCase::Ask
            | UseCase::Show
            | UseCase::Note
            | UseCase::ProposalList
            | UseCase::ProposalShow
            | UseCase::FindingRecord
            | UseCase::FindingResolve
            | UseCase::FindingDismiss => return Ok(None),
        };
        read.check()?;
        Ok(Some(read))
    }

    /// What the command line's parser would refuse of these params.
    pub fn check(&self) -> Result<()> {
        match self {
            Self::List(read) => {
                at_least_one("list's limit", read.limit)?;
                if read.all && !read.status.is_empty() {
                    return Err(bad("list takes all or status, not both"));
                }
            }
            Self::Graph(read) => one_of("graph's format", &read.format, &GRAPH_FORMATS)?,
            Self::Status(read) => {
                if let Some(role) = &read.role {
                    one_of("status's role", role, &READ_ROLES)?;
                }
            }
            Self::Asks(read) => {
                if let Some(role) = &read.role {
                    one_of("asks's role", role, &READ_ROLES)?;
                }
            }
            Self::Events(read) => {
                at_least_one("events's limit", read.limit)?;
                run_id("events's run", read.run.as_ref())?;
            }
            Self::Timeline(read) => {
                run_id("timeline's run", Some(&read.run))?;
                if read.gap < 1 {
                    return Err(bad("timeline's gap is 1 or more"));
                }
            }
            Self::Kpi(read) => {
                one_of("kpi's period", &read.period, &["day", "week"])?;
                if !(1..=400).contains(&read.last) {
                    return Err(bad("kpi's last is 1 to 400"));
                }
                if read.at.is_some() && (read.since.is_some() || read.until.is_some()) {
                    return Err(bad("kpi takes at or since and until, not both"));
                }
                if !(1..=365).contains(&read.window) {
                    return Err(bad("kpi's window is 1 to 365"));
                }
                for axis in &read.by {
                    one_of("kpi's by", axis, &KPI_AXES)?;
                }
                for change in &read.changes {
                    parse_kpi_change(change)
                        .map_err(|error| bad(format!("kpi's change: {error}")))?;
                }
                for area in &read.areas {
                    parse_kpi_area(area).map_err(|error| bad(format!("kpi's area: {error}")))?;
                }
            }
            Self::Forecast(read) => {
                if read.parallel.is_some_and(|parallel| parallel > 256) {
                    return Err(bad("forecast's parallel is 0 to 256"));
                }
                if !(1..=100_000).contains(&read.trials) {
                    return Err(bad("forecast's trials is 1 to 100000"));
                }
            }
            Self::Notes(read) => at_least_one("notes's limit", read.limit)?,
            Self::Findings(read) => {
                if read.all && !read.status.is_empty() {
                    return Err(bad("findings takes all or status, not both"));
                }
                for status in &read.status {
                    one_of("findings's status", status, &FINDING_STATUSES)?;
                }
                let targets = usize::from(read.task.is_some())
                    + usize::from(read.run.is_some())
                    + usize::from(read.goal.is_some())
                    + usize::from(read.queue);
                run_id("findings's run", read.run.as_ref())?;
                if targets > 1 {
                    return Err(bad(
                        "findings names at most one target: task, run, goal or queue",
                    ));
                }
            }
            Self::Search(read) => {
                at_least_one("search's limit", read.limit)?;
                for kind in &read.kinds {
                    one_of("search's kind", kind, &SEARCH_KINDS)?;
                }
            }
            Self::Related(read) => {
                at_least_one("related's limit", read.limit)?;
                for status in &read.status {
                    status
                        .trim()
                        .parse::<crate::domain::TaskStatus>()
                        .map_err(|error| bad(format!("related's status: {error}")))?;
                }
            }
            Self::Lint(read) => {
                if read.tasks.is_empty() && read.proposals.is_empty() {
                    return Err(bad("lint names a task or a proposal"));
                }
            }
            Self::ObserveInput(read) => {
                crate::application::observer::check_read(&read.observation, read.limit)
                    .map_err(|error| bad(format!("{error:#}")))?;
            }
            Self::GoalList(read) => {
                read.tags()
                    .map_err(|error| bad(format!("goal list's tag: {error}")))?;
            }
            Self::Candidates
            | Self::Stats(_)
            | Self::Marks(_)
            | Self::GoalShow(_)
            | Self::ObserveHistory(_) => {}
        }
        Ok(())
    }

    /// What the read is authorized on: the queue, as the command line
    /// asks the policy for every read command (`queue.read`).
    pub fn resource(&self) -> Resource {
        Resource::Queue
    }

    /// The use case and the params a client-mode `dagq` sends for this
    /// read (goal 82's stage (3)): what [`Self::parse`] reads back as the
    /// same read, every option written out.
    pub fn request(&self) -> (UseCase, Value) {
        let cursor = |cursor: &Option<Cursor>| cursor.map(Cursor::text);
        match self {
            Self::List(read) => (
                UseCase::List,
                json!({"status": read.status, "all": read.all, "goal": read.goal,
                       "limit": read.limit, "before": read.before, "full": read.full}),
            ),
            Self::Candidates => (UseCase::Candidates, json!({})),
            Self::Graph(read) => (
                UseCase::Graph,
                json!({"goal": read.goal, "format": read.format}),
            ),
            Self::Status(read) => (UseCase::Status, json!({"role": read.role})),
            Self::Asks(read) => (
                UseCase::Asks,
                json!({"open": read.open, "role": read.role, "all": read.all}),
            ),
            Self::Events(read) => (
                UseCase::Events,
                json!({"after": read.after, "limit": read.limit, "all": read.all,
                       "full": read.full, "run": read.run, "task": read.task,
                       "goal": read.goal, "kind": read.kind, "since": read.since,
                       "until": read.until}),
            ),
            Self::Timeline(read) => (
                UseCase::Timeline,
                json!({"run": read.run, "gap": read.gap, "full": read.full}),
            ),
            Self::Stats(read) => (
                UseCase::Stats,
                json!({"since": cursor(&read.since), "until": cursor(&read.until),
                       "goal": read.goal, "full": read.full}),
            ),
            Self::Kpi(read) => (
                UseCase::Kpi,
                json!({"period": read.period, "last": read.last, "at": cursor(&read.at),
                       "since": cursor(&read.since), "until": cursor(&read.until),
                       "change": read.changes, "area": read.areas, "by": read.by,
                       "cross": read.cross, "compare": read.compare.map(CompareSpec::text),
                       "window": read.window, "goal": read.goal}),
            ),
            Self::Forecast(read) => (
                UseCase::Forecast,
                json!({"task": read.task, "goal": read.goal, "parallel": read.parallel,
                       "trials": read.trials}),
            ),
            Self::Notes(read) => (
                UseCase::Notes,
                json!({"goal": read.goal, "task": read.task, "since": read.since,
                       "limit": read.limit}),
            ),
            Self::Marks(read) => (
                UseCase::Marks,
                json!({"since": cursor(&read.since), "until": cursor(&read.until)}),
            ),
            Self::Findings(read) => (
                UseCase::Findings,
                json!({"id": read.id, "all": read.all, "status": read.status,
                       "kind": read.kinds, "task": read.task, "run": read.run,
                       "goal": read.goal, "queue": read.queue, "full": read.full}),
            ),
            Self::Search(read) => (
                UseCase::Search,
                json!({"query": read.query, "status": read.status, "kind": read.kinds,
                       "goal": read.goal, "limit": read.limit, "full": read.full}),
            ),
            Self::Related(read) => (
                UseCase::Related,
                json!({"task": read.task, "status": read.status, "limit": read.limit}),
            ),
            // No `tag` without one, so a service from before `--tag`,
            // which takes no params, still answers a plain `goal list`.
            Self::GoalList(read) if read.tag.is_empty() => (UseCase::GoalList, json!({})),
            Self::GoalList(read) => (UseCase::GoalList, json!({"tag": read.tag})),
            Self::GoalShow(read) => (UseCase::GoalShow, json!({"id": read.id, "full": read.full})),
            Self::Lint(read) => (
                UseCase::Lint,
                json!({"tasks": read.tasks, "proposals": read.proposals}),
            ),
            Self::ObserveHistory(read) => (UseCase::ObserveHistory, json!({"limit": read.limit})),
            Self::ObserveInput(read) => (
                UseCase::ObserveInput,
                json!({"observation": read.observation, "section": read.section,
                       "offset": read.offset, "limit": read.limit}),
            ),
        }
    }
}

/// What a read takes beyond the queue's records that the composition root
/// assembles: the reads composed with the host (`status`, `stats`, `kpi`,
/// `forecast`, the improvements, the repository's set of changes), the
/// SVG the host's d2 draws, the observation's input files, the command
/// line's views and the clock.
pub trait QueueReadSources<Q: ?Sized> {
    fn status(&self, queue: &Q, role: Option<SessionRole>) -> Result<Value>;
    fn stats(&self, queue: &Q, query: &StatsQuery) -> Result<Value>;
    fn kpi(&self, queue: &Q, query: &KpiQuery) -> Result<Value>;
    fn forecast(&self, queue: &Q, query: &ForecastQuery) -> Result<Value>;
    /// The limit of the improvement proposals and the ones waiting
    /// (`findings`' `improvements`).
    fn improvements(&self, queue: &Q) -> Result<Value>;
    /// The repository's set of changes (ADR-t980-1), none without a
    /// checkout.
    fn changes(&self, queue: &Q) -> Result<Option<ChangeSet>>;
    /// The repository's set of goal tags (ADR-t1639-1 decision 6), none
    /// without a checkout.
    fn goal_tags(&self, queue: &Q) -> Result<Option<crate::domain::TagSet>>;
    /// The SVG the host's d2 draws from `source`.
    fn render_svg(&self, source: &str) -> Result<String>;
    /// A page of an observation's input (`observe --input`).
    fn observe_input(&self, read: &ObserveInputRead) -> Result<Value>;
    /// What the command line prints as is (`graph --format d2|svg`).
    fn raw_stdout(&self, text: String) -> Value;
    /// `goal show`'s compact form, without `--full`.
    fn goal_view(&self, detail: &GoalDetail) -> Value;
    /// The clock of `queue` (`timeline`'s time since the last event).
    fn clock<'q>(&self, queue: &'q Q) -> &'q dyn Clock;
}

/// The near-term dependency diagram of `graph --format d2|svg` (ADR-0077)
/// as d2 source, and the tasks it shows.
pub fn graph_diagram(
    queue: &(impl TaskStore + ?Sized),
    goal_id: Option<GoalId>,
) -> Result<(String, Vec<TaskId>)> {
    let input = queue.graph_input()?;
    let graph = dependency_graph(input.clone(), goal_id);
    let titles = queue
        .list_goals()?
        .into_iter()
        .map(|goal| (goal.id, goal.title))
        .collect();
    let diagram = match goal_id {
        // The goal's prerequisites and critical steps outside it are
        // drawn too (ADR-0077 decision 1).
        Some(goal) => crate::application::diagram::near_term_in_goal(
            &dependency_graph(input, None),
            &graph,
            goal,
            &titles,
        ),
        None => crate::application::diagram::near_term(&graph, &titles),
    };
    Ok((diagram.to_d2(), diagram.task_ids()))
}

/// The JSON a read of the queue gives, the same for the command line and
/// the queue service (ADR-t1233-5 decision 1): the read's options become
/// the queries of the queue's ports, and what the host has to compose
/// comes from `sources`.
pub fn answer<Q>(
    queue: &mut Q,
    sources: &dyn QueueReadSources<Q>,
    read: &QueueRead,
) -> Result<Value>
where
    Q: TaskStore + AskStore + RunLog + EventReads + ObserverLog + QueueRecords,
{
    let goal = |id: Option<i64>| id.map(GoalId::new);
    let role = |role: &Option<String>| -> Result<Option<SessionRole>> {
        Ok(role.as_deref().map(str::parse).transpose()?)
    };
    Ok(match read {
        QueueRead::List(read) => serde_json::to_value(queue.list(&TaskQuery {
            status: list_status(read)?,
            goal_id: goal(read.goal),
            limit: usize::try_from(read.limit)?,
            before: read.before.map(TaskId::new),
            full: read.full,
        })?)?,
        QueueRead::Candidates => {
            let graph = dependency_graph(queue.graph_input()?, None);
            serde_json::to_value(claim_candidates(queue.candidates()?, &graph))?
        }
        QueueRead::Graph(read) => match read.format.as_str() {
            "json" => {
                serde_json::to_value(dependency_graph(queue.graph_input()?, goal(read.goal)))?
            }
            format => {
                let (source, _) = graph_diagram(queue, goal(read.goal))?;
                let text = if format == "svg" {
                    sources.render_svg(&source)?
                } else {
                    source
                };
                sources.raw_stdout(text)
            }
        },
        QueueRead::Status(read) => sources.status(queue, role(&read.role)?)?,
        QueueRead::Asks(read) => json!({"asks": queue.asks(AskQuery {
            all: read.all,
            open: read.open,
            role: role(&read.role)?,
        })?}),
        QueueRead::Events(read) => watch::events_in(queue, &events_query(read)?)?,
        QueueRead::Timeline(read) => watch::timeline_in(
            queue,
            &RunId::new(read.run.clone())?,
            read.gap,
            read.full,
            sources.clock(queue),
        )?,
        QueueRead::Stats(read) => sources.stats(
            queue,
            &StatsQuery {
                since: read.since,
                until: read.until,
                goal_id: goal(read.goal),
                full: read.full,
            },
        )?,
        QueueRead::Kpi(read) => sources.kpi(queue, &kpi_query(read)?)?,
        QueueRead::Forecast(read) => sources.forecast(
            queue,
            &ForecastQuery {
                task_id: read.task.map(TaskId::new),
                goal_id: goal(read.goal),
                parallel: read.parallel.map(usize::from),
                trials: read.trials as usize,
            },
        )?,
        QueueRead::Notes(read) => serde_json::to_value(queue.notes(&NoteQuery {
            goal_id: goal(read.goal),
            task_id: read.task.map(TaskId::new),
            since: read.since.map(EventId::new),
            limit: usize::try_from(read.limit)?,
        })?)?,
        QueueRead::Marks(read) => {
            let marks = crate::domain::marks::marks(&queue.all_events()?, read.since, read.until);
            json!({ "marks": marks })
        }
        QueueRead::Findings(read) => {
            let findings = QueueRecords::findings(queue, &finding_query(read)?)?;
            // The limit's settings not reading does not hide the findings.
            let improvements = sources
                .improvements(queue)
                .unwrap_or_else(|error| json!({"error": format!("{error:#}")}));
            json!({"findings": findings, "improvements": improvements})
        }
        QueueRead::Search(read) => serde_json::to_value(
            queue.search_documents(&SearchQuery {
                terms: read.query.clone(),
                kinds: read
                    .kinds
                    .iter()
                    .map(|kind| kind.parse())
                    .collect::<Result<_, _>>()?,
                statuses: read
                    .status
                    .iter()
                    .map(|value| search::parse_status(value))
                    .collect::<Result<_, _>>()?,
                goal_id: goal(read.goal),
                limit: usize::try_from(read.limit)?,
                full: read.full,
            })?,
        )?,
        QueueRead::Related(read) => {
            let statuses = read
                .status
                .iter()
                .map(|status| Ok(status.trim().parse::<TaskStatus>()?.as_str().to_owned()))
                .collect::<Result<Vec<_>>>()?;
            serde_json::to_value(queue.related_tasks(
                TaskId::new(read.task),
                &statuses,
                usize::try_from(read.limit)?,
            )?)?
        }
        QueueRead::GoalList(read) => serde_json::to_value(crate::domain::goal::list(
            queue.list_goals()?,
            &read.tags()?,
        ))?,
        QueueRead::GoalShow(read) => {
            let detail = queue.show_goal(GoalId::new(read.id))?;
            if read.full {
                serde_json::to_value(detail)?
            } else {
                sources.goal_view(&detail)
            }
        }
        QueueRead::Lint(read) => {
            let mut proposals = Vec::with_capacity(read.proposals.len());
            for id in &read.proposals {
                proposals.push(queue.show_proposal(ProposalId::new(*id))?);
            }
            let targets = lint_targets(&read.tasks, &proposals);
            // The repository's set of changes holds the tasks lint checks
            // (ADR-t980-1).
            let mut input = queue.lint_input(&targets)?;
            input.changes = sources.changes(queue)?;
            input.goal_tags = sources.goal_tags(queue)?;
            json!({"tasks": targets, "violations": crate::domain::lint::lint(&input)})
        }
        QueueRead::ObserveHistory(read) => observer::history(queue, read.limit)?,
        QueueRead::ObserveInput(read) => sources.observe_input(read)?,
    })
}

/// `list`'s statuses: every one with `--all`, the open ones without
/// `--status`, else those named.
fn list_status(read: &ListRead) -> Result<StatusFilter> {
    Ok(if read.all {
        StatusFilter::Any
    } else if read.status.is_empty() {
        StatusFilter::Open
    } else {
        StatusFilter::Only(
            read.status
                .iter()
                .map(|value| value.trim().parse::<TaskStatus>())
                .collect::<Result<_, _>>()?,
        )
    })
}

/// `events`' filters as the query of [`watch::events_in`].
fn events_query(read: &EventsRead) -> Result<watch::EventsQuery> {
    Ok(watch::EventsQuery {
        after: EventId::new(read.after),
        limit: read.limit as usize,
        all: read.all,
        full: read.full,
        filter: EventFilter {
            kinds: (!read.kind.is_empty()).then(|| read.kind.clone()),
            run: read.run.clone().map(RunId::new).transpose()?,
            task: read.task.map(TaskId::new),
            goal: read.goal.map(GoalId::new),
            since: read.since.as_deref().map(watch::event_time).transpose()?,
            until: read.until.as_deref().map(watch::event_time).transpose()?,
        },
    })
}

fn kpi_query(read: &KpiRead) -> Result<KpiQuery> {
    Ok(KpiQuery {
        period: read.period.parse().map_err(anyhow::Error::msg)?,
        last: usize::from(read.last),
        at: read.at,
        since: read.since,
        until: read.until,
        changes: read.changes.clone(),
        areas: read.areas.clone(),
        by: read
            .by
            .iter()
            .map(|axis| axis.parse())
            .collect::<Result<_, String>>()
            .map_err(anyhow::Error::msg)?,
        cross: read.cross,
        compare: read.compare,
        window_days: read.window,
        goal_id: read.goal.map(GoalId::new),
    })
}

/// `findings`' query: the first of `--task`, `--run`, `--goal` and
/// `--queue` given is its target.
fn finding_query(read: &FindingsRead) -> Result<FindingQuery> {
    let target = match (read.task, &read.run, read.goal) {
        (Some(task), _, _) => Some(FindingTarget::Task(TaskId::new(task))),
        (_, Some(run), _) => Some(FindingTarget::Run(RunId::new(run.clone())?)),
        (_, _, Some(id)) => Some(FindingTarget::Goal(GoalId::new(id))),
        _ => read.queue.then_some(FindingTarget::Queue),
    };
    Ok(FindingQuery {
        id: read.id.map(FindingId::new),
        all: read.all,
        statuses: read
            .status
            .iter()
            .map(|value| value.parse())
            .collect::<Result<_, _>>()?,
        kinds: read.kinds.clone(),
        target,
        full: read.full,
    })
}

/// The tasks `lint` checks: those named, then the proposals' tasks, each
/// once in the order first named.
fn lint_targets(tasks: &[i64], proposals: &[Proposal]) -> Vec<TaskId> {
    let mut targets: Vec<TaskId> = tasks.iter().copied().map(TaskId::new).collect();
    for proposal in proposals {
        targets.extend_from_slice(proposal.task_ids());
    }
    let mut seen = std::collections::HashSet::new();
    targets.retain(|id| seen.insert(*id));
    targets
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn the_params_default_to_the_command_line_s_defaults() {
        let Some(QueueRead::Events(events)) =
            QueueRead::parse(UseCase::Events, &Value::Null).unwrap()
        else {
            panic!("no events read");
        };
        assert_eq!((events.after, events.limit, events.all), (0, 100, false));
        let Some(QueueRead::Kpi(kpi)) = QueueRead::parse(UseCase::Kpi, &json!({})).unwrap() else {
            panic!("no kpi read");
        };
        assert_eq!(kpi.period, "day");
        assert_eq!(kpi.last, 7);
        assert_eq!(kpi.window, crate::domain::kpi::DEFAULT_WINDOW_DAYS);
        let Some(QueueRead::Stats(stats)) = QueueRead::parse(
            UseCase::Stats,
            &json!({"since": 12, "until": "@1790000000", "full": true}),
        )
        .unwrap() else {
            panic!("no stats read");
        };
        assert_eq!(stats.since, Some("12".parse().unwrap()));
        assert_eq!(stats.until, Some("@1790000000".parse().unwrap()));
        let Some(QueueRead::Kpi(kpi)) =
            QueueRead::parse(UseCase::Kpi, &json!({"compare": "5", "change": ["fix"]})).unwrap()
        else {
            panic!("no kpi read");
        };
        assert_eq!(kpi.compare, Some("5".parse().unwrap()));
        assert_eq!(kpi.changes, ["fix"]);
        assert_eq!(
            QueueRead::parse(UseCase::GoalList, &json!({})).unwrap(),
            Some(QueueRead::GoalList(GoalListRead::default()))
        );
        assert_eq!(
            QueueRead::GoalList(GoalListRead::default()).request(),
            (UseCase::GoalList, json!({}))
        );
        assert_eq!(
            QueueRead::parse(UseCase::ObserveHistory, &Value::Null).unwrap(),
            Some(QueueRead::ObserveHistory(ObserveHistoryRead {
                limit: crate::application::observer::HISTORY_LIMIT
            }))
        );
        let related = QueueRead::parse(UseCase::Related, &json!({"task": 4})).unwrap();
        assert_eq!(related.unwrap().resource(), Resource::Queue);
    }

    #[test]
    fn a_read_s_request_reads_back_as_the_same_read() {
        for (use_case, params) in [
            (
                UseCase::List,
                json!({"status": ["ready"], "goal": 3, "limit": 5, "before": 9, "full": true}),
            ),
            (UseCase::Candidates, json!({})),
            (UseCase::Graph, json!({"goal": 2, "format": "d2"})),
            (UseCase::Status, json!({"role": "inbox"})),
            (UseCase::Asks, json!({"open": true, "role": "planner"})),
            (
                UseCase::Events,
                json!({"after": 4, "limit": 7, "all": true, "full": true, "run": "r1",
                                      "task": 2, "goal": 1, "kind": ["note"], "since": "1", "until": "@2"}),
            ),
            (
                UseCase::Timeline,
                json!({"run": "r1", "gap": 30, "full": true}),
            ),
            (
                UseCase::Stats,
                json!({"since": "12", "until": "2026-09-26T08:52:00.123Z", "goal": 1, "full": true}),
            ),
            (
                UseCase::Kpi,
                json!({"period": "week", "last": 4, "at": "@1790000000", "change": ["fix"],
                                   "area": ["runtime"], "by": ["provider"], "cross": true,
                                   "compare": "1..2,3..4", "window": 9, "goal": 2}),
            ),
            (
                UseCase::Forecast,
                json!({"task": 3, "parallel": 2, "trials": 50}),
            ),
            (UseCase::Notes, json!({"goal": 1, "since": 4, "limit": 3})),
            (UseCase::Marks, json!({"since": "1", "until": "9"})),
            (
                UseCase::Findings,
                json!({"id": 2, "status": ["open"], "kind": ["stall"], "queue": true, "full": true}),
            ),
            (
                UseCase::Search,
                json!({"query": "x y", "status": ["ready"], "kind": ["task"], "goal": 1, "limit": 2}),
            ),
            (
                UseCase::Related,
                json!({"task": 4, "status": ["ready"], "limit": 3}),
            ),
            (UseCase::GoalList, json!({})),
            (UseCase::GoalList, json!({"tag": ["codex", "cmux"]})),
            (UseCase::GoalShow, json!({"id": 2, "full": true})),
            (UseCase::Lint, json!({"tasks": [1], "proposals": [2]})),
            (UseCase::ObserveHistory, json!({"limit": 4})),
            (
                UseCase::ObserveInput,
                json!({"observation": "1791005872-1", "section": "stats.runs", "offset": 3, "limit": 2}),
            ),
        ] {
            let read = QueueRead::parse(use_case, &params).unwrap().unwrap();
            let (sent, again) = read.request();
            assert_eq!(sent, use_case);
            assert_eq!(
                QueueRead::parse(sent, &again).unwrap(),
                Some(read),
                "{params}"
            );
        }
    }

    #[test]
    fn what_the_command_line_refuses_is_a_bad_read_and_a_write_is_no_read() {
        for (use_case, params) in [
            (UseCase::List, json!({"limit": 0})),
            (UseCase::List, json!({"all": true, "status": ["ready"]})),
            (UseCase::List, json!({"sql": "x"})),
            (UseCase::Candidates, json!({"x": 1})),
            (UseCase::GoalList, json!([1])),
            (UseCase::GoalList, json!({"tag": ["Codex"]})),
            (UseCase::GoalList, json!({"tags": ["codex"]})),
            (UseCase::Graph, json!({"format": "png"})),
            (UseCase::Graph, json!({"format": "svg"})),
            (UseCase::Kpi, json!({"change": ["Bad Change!"]})),
            (UseCase::Kpi, json!({"area": ["Bad Area!"]})),
            (UseCase::Related, json!({"task": 1, "status": ["gone"]})),
            (UseCase::Events, json!({"run": ""})),
            (UseCase::Findings, json!({"run": ""})),
            (UseCase::Timeline, json!({"run": ""})),
            (UseCase::Status, json!({"role": "worker"})),
            (UseCase::Asks, json!({"role": "observer"})),
            (UseCase::Events, json!({"limit": 0})),
            (UseCase::Timeline, json!({})),
            (UseCase::Timeline, json!({"run": "r", "gap": 0})),
            (UseCase::Stats, json!({"since": "yesterday"})),
            (UseCase::Stats, json!({"since": [1]})),
            (UseCase::Stats, json!({"cmux": "/bin/sh"})),
            (UseCase::Kpi, json!({"period": "month"})),
            (UseCase::Kpi, json!({"last": 0})),
            (UseCase::Kpi, json!({"at": "1", "since": "2"})),
            (UseCase::Kpi, json!({"window": 0})),
            (UseCase::Kpi, json!({"by": ["colour"]})),
            (UseCase::Kpi, json!({"compare": "a..b"})),
            (UseCase::Kpi, json!({"compare": true})),
            (UseCase::Forecast, json!({"parallel": 300})),
            (UseCase::Forecast, json!({"trials": 0})),
            (UseCase::Notes, json!({"limit": 0})),
            (UseCase::Findings, json!({"all": true, "status": ["open"]})),
            (UseCase::Findings, json!({"status": ["gone"]})),
            (UseCase::Findings, json!({"task": 1, "queue": true})),
            (UseCase::Search, json!({})),
            (UseCase::Search, json!({"query": "x", "limit": 0})),
            (UseCase::Search, json!({"query": "x", "kind": ["file"]})),
            (UseCase::Related, json!({"task": 1, "limit": 0})),
            (UseCase::Lint, json!({})),
        ] {
            let error = QueueRead::parse(use_case, &params).unwrap_err();
            assert!(
                error.downcast_ref::<BadRead>().is_some(),
                "{params}: {error}"
            );
        }
        for use_case in [UseCase::Ask, UseCase::Show, UseCase::FindingRecord] {
            assert_eq!(QueueRead::parse(use_case, &json!({})).unwrap(), None);
        }
    }

    fn list(value: Value) -> ListRead {
        let Some(QueueRead::List(read)) = QueueRead::parse(UseCase::List, &value).unwrap() else {
            panic!("no list read");
        };
        read
    }

    #[test]
    fn list_shows_the_open_tasks_unless_all_or_statuses_are_named() {
        assert_eq!(list_status(&list(json!({}))).unwrap(), StatusFilter::Open);
        assert_eq!(
            list_status(&list(json!({"all": true}))).unwrap(),
            StatusFilter::Any
        );
        assert_eq!(
            list_status(&list(json!({"status": [" completed ", "ready"]}))).unwrap(),
            StatusFilter::Only(vec![TaskStatus::Completed, TaskStatus::Ready])
        );
        assert!(list_status(&list(json!({"status": ["nope"]}))).is_err());
    }

    #[test]
    fn events_filters_name_kinds_only_when_given_and_read_days_as_utc_midnight() {
        let Some(QueueRead::Events(read)) = QueueRead::parse(
            UseCase::Events,
            &json!({"after": 4, "task": 7, "since": "2026-10-01", "kind": ["run_claimed"]}),
        )
        .unwrap() else {
            panic!("no events read");
        };
        let query = events_query(&read).unwrap();
        assert_eq!((query.after, query.limit), (EventId::new(4), 100));
        assert_eq!(query.filter.kinds, Some(vec!["run_claimed".to_owned()]));
        assert_eq!(query.filter.task, Some(TaskId::new(7)));
        assert_eq!(query.filter.since.as_deref(), Some("2026-10-01T00:00:00Z"));
        assert_eq!(query.filter.until, None);
        let Some(QueueRead::Events(bare)) = QueueRead::parse(UseCase::Events, &json!({})).unwrap()
        else {
            panic!("no events read");
        };
        assert_eq!(events_query(&bare).unwrap().filter.kinds, None);
    }

    #[test]
    fn findings_target_the_first_of_task_run_goal_and_queue() {
        let Some(QueueRead::Findings(none)) =
            QueueRead::parse(UseCase::Findings, &json!({})).unwrap()
        else {
            panic!("no findings read");
        };
        let target = |read: &FindingsRead| finding_query(read).unwrap().target;
        assert_eq!(target(&none), None);
        let all = FindingsRead {
            task: Some(3),
            run: Some("r1".into()),
            goal: Some(1),
            queue: true,
            ..none.clone()
        };
        assert_eq!(target(&all), Some(FindingTarget::Task(TaskId::new(3))));
        let run = FindingsRead {
            task: None,
            ..all.clone()
        };
        assert_eq!(
            target(&run),
            Some(FindingTarget::Run(RunId::new("r1").unwrap()))
        );
        let goal = FindingsRead { run: None, ..run };
        assert_eq!(target(&goal), Some(FindingTarget::Goal(GoalId::new(1))));
        let queue = FindingsRead { goal: None, ..goal };
        assert_eq!(target(&queue), Some(FindingTarget::Queue));
    }

    #[test]
    fn lint_checks_the_named_tasks_then_the_proposals_tasks_each_once() {
        let owner = crate::domain::PlannerOwner {
            origin: crate::domain::PlannerOrigin::Person,
            workspace_id: None,
        };
        let proposal = Proposal::submit(
            ProposalId::new(1),
            owner,
            vec![TaskId::new(5), TaskId::new(2), TaskId::new(9)],
            Vec::new(),
            "now".into(),
        )
        .unwrap();
        assert_eq!(
            lint_targets(&[9, 4, 9], &[proposal]),
            [9, 4, 2, 5].map(TaskId::new).to_vec()
        );
    }

    #[test]
    fn kpi_rejects_an_unknown_axis_with_its_message() {
        let Some(QueueRead::Kpi(mut read)) = QueueRead::parse(UseCase::Kpi, &json!({})).unwrap()
        else {
            panic!("no kpi read");
        };
        let query = kpi_query(&read).unwrap();
        assert_eq!(query.last, 7);
        assert!(query.by.is_empty());
        read.by = vec!["nope".into()];
        assert!(kpi_query(&read).is_err());
    }
}
