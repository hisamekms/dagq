//! The queue's reads as the queue service's read use cases take them
//! ([Queue service], ADR-t1233-5 decisions 1 to 3): one [`QueueRead`] per
//! read command of the command line a read role's job or a worker runs
//! (`list`, `events`, `timeline`, `stats`, `kpi`, `forecast`, `marks`,
//! `search`, `related`, `findings`, `goal show`, ...), with the command
//! line's options as its params and their defaults. The command line and
//! the service both answer one with `compose::read_queue`, so a read gives
//! the same JSON either way.
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

use crate::domain::Resource;
use crate::domain::kpi::CompareSpec;
use crate::domain::queue_service::UseCase;
use crate::domain::stats::Cursor;

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
    GoalList,
    GoalShow(GoalShowRead),
    Lint(LintRead),
    ObserveHistory(ObserveHistoryRead),
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
    crate::observer::HISTORY_LIMIT
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
            UseCase::GoalList => {
                none(use_case, params)?;
                Self::GoalList
            }
            UseCase::GoalShow => Self::GoalShow(read(use_case, params)?),
            UseCase::Lint => Self::Lint(read(use_case, params)?),
            UseCase::ObserveHistory => Self::ObserveHistory(read(use_case, params)?),
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
            Self::Candidates
            | Self::Stats(_)
            | Self::Marks(_)
            | Self::GoalList
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
            Self::GoalList => (UseCase::GoalList, json!({})),
            Self::GoalShow(read) => (UseCase::GoalShow, json!({"id": read.id, "full": read.full})),
            Self::Lint(read) => (
                UseCase::Lint,
                json!({"tasks": read.tasks, "proposals": read.proposals}),
            ),
            Self::ObserveHistory(read) => (UseCase::ObserveHistory, json!({"limit": read.limit})),
        }
    }
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
            Some(QueueRead::GoalList)
        );
        assert_eq!(
            QueueRead::parse(UseCase::ObserveHistory, &Value::Null).unwrap(),
            Some(QueueRead::ObserveHistory(ObserveHistoryRead {
                limit: crate::observer::HISTORY_LIMIT
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
            (UseCase::GoalShow, json!({"id": 2, "full": true})),
            (UseCase::Lint, json!({"tasks": [1], "proposals": [2]})),
            (UseCase::ObserveHistory, json!({"limit": 4})),
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
}
