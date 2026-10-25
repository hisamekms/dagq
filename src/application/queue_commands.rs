//! The dialogue and the operations of the host's CLI as the queue
//! service's use cases ([Queue service], ADR-t3102-2 decision 6 (4),
//! ADR-t3102-3 decision 3): one [`QueueCommand`] per command a person or
//! the inbox runs on the host (and a planner, as far as the policy grants
//! it) that changes or reads the queue apart from the reads of
//! [`super::queue_reads`]: `answer`, `ask close`, `hold`, `release`,
//! `replan request`, `rebind`, `recover`, `run screen|send|log`, `planner
//! screen|send|log|request`, `report`, `graph --out` and `ready
//! --inherit`.
//!
//! The params are the command line's options as the client resolved them:
//! a path is absolute (the service runs in another directory), the words
//! of `planner request` are read by the client (`--text-file`, `-`), and
//! what clap refuses is refused on the client before anything is sent. The
//! command line on the DB and the service both run one with the
//! composition root's same function (`compose::run_command`), so the
//! answer is the command line's. Each runs in the service as the one
//! operation of the application it is on the DB, its writes in the
//! transaction that operation has there, and none is split across round
//! trips with the client.
//!
//! The answer of `run log` and `planner log` is the background session the
//! log is read from; the client prints the file and follows it, as it
//! reads no queue for that. The answer of `graph --out` is the d2 source
//! and the tasks it shows; the client draws it with its own d2 and writes
//! the file, as no service draws (`graph --format svg`). No Git and no
//! program runs in the service (ADR-t3102-3 decision 5), so three are sent
//! with what the client read with them: `report` after
//! [`QueueCommand::ReportInputs`] (the near-term diagram's d2 source, the
//! bound repository and the commits landed in the report's periods, whose
//! areas the client reads in its main checkout and whose diagram it
//! draws), `release` after
//! [`QueueCommand::ReleaseRead`] (the run whose work the client reads and
//! keeps, which the release's transaction checks again), `ready --inherit`
//! after [`QueueCommand::ReadyInheritRead`] (the latest run whose head the
//! client chooses and writes under `refs/dagq/runs/<run-id>` before the
//! record, putting the ref back when the record is refused, ADR-t3294-1),
//! and `rebind` with
//! the Git common directory the client read; the client repairs the run
//! worktrees `rebind`'s answer lists.
//!
//! [Queue service]: ../../docs/design/queue-service.md

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::commands::operations::Operation;
use super::screen::RunTarget;
use crate::domain::queue_service::UseCase;
use crate::domain::{Capability, PlannerId, Resource, RunId};

/// One of the host's dialogue and operation commands, as its use case of
/// the service takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueCommand {
    Answer(AnswerParams),
    AskClose(AskCloseParams),
    Hold(HoldParams),
    Release(ReleaseParams),
    ReleaseRead(ReleaseParams),
    ReleaseImages(ReleaseImagesParams),
    ReplanRequest(ReplanRequestParams),
    Rebind(RebindParams),
    Recover(RunParams),
    RunScreen(RunParams),
    RunSend(RunParams),
    RunLog(RunParams),
    PlannerScreen(PlannerParams),
    PlannerSend(PlannerParams),
    PlannerLog(PlannerParams),
    PlannerRequest(PlannerRequestParams),
    Report(ReportParams),
    ReportInputs(ReportInputsParams),
    GraphExport(GraphExportParams),
    ReadyInheritRead(ReadyInheritParams),
    ReadyInherit(ReadyInheritParams),
    ReadyInheritImages(ReadyInheritImagesParams),
}

/// `answer`'s: the ask and the answer's text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerParams {
    pub id: i64,
    pub text: String,
}

/// `ask close`'s: the ask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskCloseParams {
    pub id: i64,
}

/// `hold`'s: the task (a number) or run it names, the reason, and
/// `--until` in unix seconds (a time only: the client refuses an event
/// id, as the command line does).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HoldParams {
    pub target: String,
    pub reason: String,
    #[serde(default)]
    pub until: Option<i64>,
}

/// `release`'s and `release_read`'s: the task or run it names, and the
/// provider and model it chooses; `release`'s also the work its client
/// read and kept of the run `release_read` named (none when it named
/// none).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseParams {
    pub target: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub work: Option<ReleaseWork>,
}

/// The work of a run a release ends for a new one, as its client read it
/// with Git and kept its head under `refs/dagq/runs/<run-id>`, and the
/// hold and version `release_read` named, which the store's transaction
/// checks again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseWork {
    pub at: crate::domain::task_hold::ReleaseReadAt,
    pub read: super::inherit::ReleasedRead,
}

/// `release_images`'s: the task or run the release names, by whose hold
/// the records are authorized, and the image records its client kept as it
/// made the image of the run's facts-only turn ready, each of
/// [`crate::domain::container_image::CONTAINER_IMAGE_KINDS`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseImagesParams {
    pub target: String,
    pub records: Vec<super::container_image::KeptImageRecord>,
}

/// `release_read`'s answer: the run whose work the release reads, with
/// the hold and version it is read at (none when it reads none), and the
/// Git common directory of the repository the queue is bound to, whose
/// main checkout the client's containers are found from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseToRead {
    pub read: Option<(
        crate::domain::task_hold::ReleaseReadAt,
        crate::domain::TaskRun,
    )>,
    pub repository: Option<String>,
}

/// `ready --inherit`'s and `ready_inherit_read`'s: the task and the reason
/// it is carried over for; `ready --inherit`'s also the head its client
/// chose and wrote, from what `ready_inherit_read` read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadyInheritParams {
    pub id: i64,
    pub reason: String,
    #[serde(default)]
    pub work: Option<InheritWork>,
}

/// The head a `ready --inherit`'s client chose and wrote under
/// `refs/dagq/runs/<run-id>` (none when the run's branch holds nothing to
/// carry over), and what it was chosen from, which the store's transaction
/// checks again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InheritWork {
    pub at: super::inherit::InheritReadAt,
    #[serde(default)]
    pub head: Option<crate::domain::CommitSha>,
}

/// `ready_inherit_read`'s answer: what the head is chosen from, the Git
/// common directory of the repository the queue is bound to, whose main
/// checkout the client's containers are found from, and the path of the
/// queue's DB the answer is of, whose directory names the queue's machine,
/// image and volumes a facts-only turn runs in; the client opens no DB at
/// it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadyInheritToRead {
    #[serde(flatten)]
    pub read: super::commands::planning::InheritToRead,
    pub repository: Option<String>,
    #[serde(default)]
    pub queue: Option<String>,
}

/// `ready_inherit_images`'s: the task `ready --inherit` names, by whose
/// authorization the records are, and the image records its client kept as
/// it made the image of the run's facts-only turn ready.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadyInheritImagesParams {
    pub id: i64,
    pub records: Vec<super::container_image::KeptImageRecord>,
}

/// `replan request`'s: the run, the reason and the events it cites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplanRequestParams {
    pub run: String,
    pub reason: String,
    #[serde(default)]
    pub evidence: Vec<i64>,
}

/// `rebind`'s: the Git common directory of the repository to bind the
/// queue to, an absolute path the client read from its checkout (`--repo`,
/// else its working directory), as no Git runs in the service; the client
/// repairs the run worktrees the answer lists. A caller the policy does
/// not grant `rebind` reads no Git and sends none, and is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebindParams {
    #[serde(default)]
    pub git_common_dir: Option<PathBuf>,
    /// The queue directory the client's data home gives a queue of that
    /// repository, absolute: where the answer's `move_to` points while the
    /// queue is elsewhere, as the client finds a queue from its checkout
    /// (the service's data home may be another). None when the client has
    /// no data home.
    #[serde(default)]
    pub repository_queue_dir: Option<PathBuf>,
}

/// `recover`'s and `run screen|send|log`'s: the run, or a task's number
/// for its latest run (the screens and the log).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunParams {
    pub run: String,
}

/// `planner screen|send|log`'s: the planner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannerParams {
    pub planner: i64,
}

/// `planner request`'s: the planner and the words the client read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannerRequestParams {
    pub planner: i64,
    pub text: String,
}

/// `report`'s: the period (`day` or `week`), the cursor whose period it
/// is (`--at`, as its text), the directory to write under (`--out`, an
/// absolute path; the queue's `reports/` without) and whether it is only
/// returned (`--print json`), with what the client read and drew from the
/// [`QueueCommand::ReportInputs`] answered to it: the near-term diagram
/// (none: a command not sent yet, which the report refuses), the main
/// checkout of the repository the queue is bound to, and the areas of each
/// landed commit it read with Git there (none: no `[areas]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportParams {
    pub period: String,
    #[serde(default)]
    pub at: Option<String>,
    #[serde(default)]
    pub out: Option<PathBuf>,
    #[serde(default)]
    pub print: bool,
    #[serde(default)]
    pub diagram: Option<DrawnDiagram>,
    #[serde(default)]
    pub checkout: Option<PathBuf>,
    #[serde(default)]
    pub areas: Option<std::collections::BTreeMap<String, Vec<String>>>,
}

pub use super::report::DrawnDiagram;

/// `report`'s first requests, of the report's period and `--at` (as
/// [`ReportParams`] names them): their answer is the
/// [`super::report::ReportInputs`], which the client reads Git and draws
/// with and sends with the report. The first names no checkout, and is
/// answered no landed commit; the client asks again with the main checkout
/// it found from the binding answered, when its `dagq.toml` maps areas, and
/// is answered the commits landed in the periods the report reads, by that
/// checkout's `[kpi]` settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportInputsParams {
    pub period: String,
    #[serde(default)]
    pub at: Option<String>,
    #[serde(default)]
    pub checkout: Option<PathBuf>,
}

/// `graph --out`'s: the goal the diagram is narrowed to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphExportParams {
    #[serde(default)]
    pub goal: Option<i64>,
}

/// Params that are not the command line's: what clap, or the client
/// before it sends, refuses.
#[derive(Debug)]
pub struct BadCommand(pub String);

impl std::fmt::Display for BadCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadCommand {}

/// A path the service is given absolute, or why not.
fn absolute(what: &str, path: &Path) -> Result<()> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(BadCommand(format!(
            "{what} is an absolute path, not {}: the queue service runs in another directory",
            path.display()
        ))
        .into())
    }
}

impl QueueCommand {
    /// The command `use_case` asks for with `params`: `None` for a use case
    /// that is none of these. Params the command line would refuse are a
    /// [`BadCommand`].
    pub fn parse(use_case: UseCase, params: &Value) -> Result<Option<Self>> {
        fn read<T: for<'de> Deserialize<'de>>(use_case: UseCase, params: &Value) -> Result<T> {
            let params = if params.is_null() {
                &Value::Object(serde_json::Map::new())
            } else {
                params
            };
            serde_json::from_value(params.clone()).map_err(|error| {
                BadCommand(format!(
                    "the params of {} do not read: {error}",
                    use_case.as_str()
                ))
                .into()
            })
        }
        let command = match use_case {
            UseCase::Answer => Self::Answer(read(use_case, params)?),
            UseCase::AskClose => Self::AskClose(read(use_case, params)?),
            UseCase::Hold => Self::Hold(read(use_case, params)?),
            UseCase::Release => Self::Release(read(use_case, params)?),
            UseCase::ReleaseRead => Self::ReleaseRead(read(use_case, params)?),
            UseCase::ReleaseImages => Self::ReleaseImages(read(use_case, params)?),
            UseCase::ReplanRequest => Self::ReplanRequest(read(use_case, params)?),
            UseCase::Rebind => Self::Rebind(read(use_case, params)?),
            UseCase::Recover => Self::Recover(read(use_case, params)?),
            UseCase::RunScreen => Self::RunScreen(read(use_case, params)?),
            UseCase::RunSend => Self::RunSend(read(use_case, params)?),
            UseCase::RunLog => Self::RunLog(read(use_case, params)?),
            UseCase::PlannerScreen => Self::PlannerScreen(read(use_case, params)?),
            UseCase::PlannerSend => Self::PlannerSend(read(use_case, params)?),
            UseCase::PlannerLog => Self::PlannerLog(read(use_case, params)?),
            UseCase::PlannerRequest => Self::PlannerRequest(read(use_case, params)?),
            UseCase::Report => Self::Report(read(use_case, params)?),
            UseCase::ReportInputs => Self::ReportInputs(read(use_case, params)?),
            UseCase::GraphExport => Self::GraphExport(read(use_case, params)?),
            UseCase::ReadyInheritRead => Self::ReadyInheritRead(read(use_case, params)?),
            UseCase::ReadyInherit => Self::ReadyInherit(read(use_case, params)?),
            UseCase::ReadyInheritImages => Self::ReadyInheritImages(read(use_case, params)?),
            _ => return Ok(None),
        };
        command.check()?;
        Ok(Some(command))
    }

    /// Refuse what the command line refuses before it runs: a relative
    /// path, and a period or a cursor clap does not take.
    fn check(&self) -> Result<()> {
        match self {
            Self::ReleaseRead(params) if params.work.is_some() => Err(BadCommand(
                "release_read takes no work: it names the work to read".to_owned(),
            )
            .into()),
            Self::ReadyInheritRead(params) if params.work.is_some() => Err(BadCommand(
                "ready_inherit_read takes no work: it reads what the head is chosen from"
                    .to_owned(),
            )
            .into()),
            Self::ReadyInherit(params) if params.work.is_none() => Err(BadCommand(
                "ready_inherit is sent with the head its client chose of what ready_inherit_read \
                 read"
                    .to_owned(),
            )
            .into()),
            Self::Rebind(params) => [
                ("rebind's git_common_dir", &params.git_common_dir),
                (
                    "rebind's repository_queue_dir",
                    &params.repository_queue_dir,
                ),
            ]
            .into_iter()
            .try_for_each(|(name, dir)| dir.as_deref().map_or(Ok(()), |dir| absolute(name, dir))),
            Self::Report(params) => {
                if !matches!(params.period.as_str(), "day" | "week") {
                    return Err(BadCommand(format!(
                        "report's period is day or week, not {:?}",
                        params.period
                    ))
                    .into());
                }
                if let Some(at) = &params.at {
                    at.parse::<crate::domain::stats::Cursor>()
                        .map_err(|error| BadCommand(format!("report's at: {error}")))?;
                }
                if params.print && params.out.is_some() {
                    return Err(BadCommand("report's print and out conflict".to_owned()).into());
                }
                if let Some(checkout) = &params.checkout {
                    absolute("report's checkout", checkout)?;
                }
                params
                    .out
                    .as_deref()
                    .map_or(Ok(()), |out| absolute("report's out", out))
            }
            _ => Ok(()),
        }
    }

    /// The use case the command goes to.
    pub fn use_case(&self) -> UseCase {
        match self {
            Self::Answer(_) => UseCase::Answer,
            Self::AskClose(_) => UseCase::AskClose,
            Self::Hold(_) => UseCase::Hold,
            Self::Release(_) => UseCase::Release,
            Self::ReleaseRead(_) => UseCase::ReleaseRead,
            Self::ReleaseImages(_) => UseCase::ReleaseImages,
            Self::ReplanRequest(_) => UseCase::ReplanRequest,
            Self::Rebind(_) => UseCase::Rebind,
            Self::Recover(_) => UseCase::Recover,
            Self::RunScreen(_) => UseCase::RunScreen,
            Self::RunSend(_) => UseCase::RunSend,
            Self::RunLog(_) => UseCase::RunLog,
            Self::PlannerScreen(_) => UseCase::PlannerScreen,
            Self::PlannerSend(_) => UseCase::PlannerSend,
            Self::PlannerLog(_) => UseCase::PlannerLog,
            Self::PlannerRequest(_) => UseCase::PlannerRequest,
            Self::Report(_) => UseCase::Report,
            Self::ReportInputs(_) => UseCase::ReportInputs,
            Self::GraphExport(_) => UseCase::GraphExport,
            Self::ReadyInheritRead(_) => UseCase::ReadyInheritRead,
            Self::ReadyInherit(_) => UseCase::ReadyInherit,
            Self::ReadyInheritImages(_) => UseCase::ReadyInheritImages,
        }
    }

    /// The use case and the params a client sends for the command: what
    /// [`Self::parse`] reads back as the same command.
    pub fn request(&self) -> (UseCase, Value) {
        let params = match self {
            Self::Answer(params) => serde_json::to_value(params),
            Self::AskClose(params) => serde_json::to_value(params),
            Self::Hold(params) => serde_json::to_value(params),
            Self::Release(params) | Self::ReleaseRead(params) => serde_json::to_value(params),
            Self::ReleaseImages(params) => serde_json::to_value(params),
            Self::ReplanRequest(params) => serde_json::to_value(params),
            Self::Rebind(params) => serde_json::to_value(params),
            Self::Recover(params)
            | Self::RunScreen(params)
            | Self::RunSend(params)
            | Self::RunLog(params) => serde_json::to_value(params),
            Self::PlannerScreen(params) | Self::PlannerSend(params) | Self::PlannerLog(params) => {
                serde_json::to_value(params)
            }
            Self::PlannerRequest(params) => serde_json::to_value(params),
            Self::Report(params) => serde_json::to_value(params),
            Self::ReportInputs(params) => serde_json::to_value(params),
            Self::GraphExport(params) => serde_json::to_value(params),
            Self::ReadyInheritRead(params) | Self::ReadyInherit(params) => {
                serde_json::to_value(params)
            }
            Self::ReadyInheritImages(params) => serde_json::to_value(params),
        };
        // Every params type is a struct of plain fields.
        (self.use_case(), params.unwrap_or(Value::Null))
    }

    /// The runtime operation the command is (task 734), authorized before
    /// it runs, as far as the params name what it acts on: `None` for the
    /// ones that are not.
    pub fn operation(&self) -> Option<Operation> {
        let screen = |run: &str| {
            RunTarget::parse(run).map_or(Resource::Unresolved, |target| target.resource())
        };
        let planner = |params: &PlannerParams| Resource::Planner(PlannerId::new(params.planner));
        Some(match self {
            Self::Rebind(_) => Operation::Rebind,
            Self::Recover(params) => Operation::Recover(RunId::new(&params.run).ok()),
            // A background session's log is read as its screen is.
            Self::RunScreen(params) | Self::RunLog(params) => {
                Operation::ReadScreen(screen(&params.run))
            }
            Self::RunSend(params) => Operation::SendToScreen(screen(&params.run)),
            Self::PlannerScreen(params) | Self::PlannerLog(params) => {
                Operation::ReadScreen(planner(params))
            }
            Self::PlannerSend(params) => Operation::SendToScreen(planner(params)),
            Self::PlannerRequest(params) => {
                Operation::RequestPlanner(PlannerId::new(params.planner))
            }
            _ => return None,
        })
    }

    /// What the policy is asked before the command runs, as the command
    /// line asks it: the operation's capability and resource, and
    /// `queue.export` on the queue for `report` and `graph --out`. `None`
    /// for the ones the application authorizes itself as it runs them,
    /// with what it reads of the queue (`answer`, `ask close`, `hold`,
    /// `release`, `replan request`, `ready --inherit`).
    pub fn authorization(&self) -> Option<(Capability, Resource)> {
        match self {
            Self::Report(_) | Self::ReportInputs(_) | Self::GraphExport(_) => {
                Some((Capability::ExportFile, Resource::Queue))
            }
            _ => self.operation().map(|operation| operation.request()),
        }
    }

    /// Whether the command only reads the queue, so the service runs it on
    /// a read-only connection as the command line opens one for it: the
    /// screens, the logs, `report` and `graph --out` (which write files,
    /// but no queue state).
    pub fn reads_only(&self) -> bool {
        matches!(
            self,
            Self::RunScreen(_)
                | Self::RunSend(_)
                | Self::RunLog(_)
                | Self::PlannerLog(_)
                | Self::Report(_)
                | Self::ReportInputs(_)
                | Self::GraphExport(_)
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn every() -> Vec<QueueCommand> {
        let run = || RunParams {
            run: "r1".to_owned(),
        };
        let planner = || PlannerParams { planner: 2 };
        vec![
            QueueCommand::Answer(AnswerParams {
                id: 3,
                text: "yes".to_owned(),
            }),
            QueueCommand::AskClose(AskCloseParams { id: 3 }),
            QueueCommand::Hold(HoldParams {
                target: "4".to_owned(),
                reason: "wait".to_owned(),
                until: Some(1_800_000_000),
            }),
            QueueCommand::Release(ReleaseParams {
                target: "r1".to_owned(),
                provider: Some("codex".to_owned()),
                model: None,
                work: None,
            }),
            QueueCommand::ReleaseRead(ReleaseParams {
                target: "7".to_owned(),
                provider: None,
                model: None,
                work: None,
            }),
            QueueCommand::ReleaseImages(ReleaseImagesParams {
                target: "7".to_owned(),
                records: vec![
                    super::super::container_image::KeptImageRecord::ContainerImageFailed {
                        image: None,
                        reason: "no image".to_owned(),
                    },
                ],
            }),
            QueueCommand::ReplanRequest(ReplanRequestParams {
                run: "r1".to_owned(),
                reason: "split".to_owned(),
                evidence: vec![7],
            }),
            QueueCommand::ReadyInheritRead(ReadyInheritParams {
                id: 7,
                reason: "keep it".to_owned(),
                work: None,
            }),
            QueueCommand::ReadyInherit(ReadyInheritParams {
                id: 7,
                reason: "keep it".to_owned(),
                work: Some(InheritWork {
                    at: super::super::inherit::InheritReadAt {
                        task: crate::domain::TaskStatus::InProgress,
                        run: Some(super::super::inherit::InheritRunAt {
                            id: RunId::new("r1").unwrap(),
                            status: crate::domain::RunStatus::Failed,
                            result_commit: None,
                        }),
                    },
                    head: Some(crate::domain::CommitSha::try_from("b".repeat(40)).unwrap()),
                }),
            }),
            QueueCommand::ReadyInheritImages(ReadyInheritImagesParams {
                id: 7,
                records: Vec::new(),
            }),
            QueueCommand::Rebind(RebindParams {
                git_common_dir: Some(PathBuf::from("/repo/.git")),
                repository_queue_dir: Some(PathBuf::from("/data/dagq/0123456789abcdef")),
            }),
            QueueCommand::Recover(run()),
            QueueCommand::RunScreen(run()),
            QueueCommand::RunSend(run()),
            QueueCommand::RunLog(run()),
            QueueCommand::PlannerScreen(planner()),
            QueueCommand::PlannerSend(planner()),
            QueueCommand::PlannerLog(planner()),
            QueueCommand::PlannerRequest(PlannerRequestParams {
                planner: 2,
                text: "and this".to_owned(),
            }),
            QueueCommand::Report(ReportParams {
                period: "week".to_owned(),
                at: Some("@1800000000".to_owned()),
                out: Some(PathBuf::from("/reports")),
                print: false,
                diagram: Some(DrawnDiagram {
                    tasks: vec![3],
                    d2_source: true,
                    reason: None,
                    svg: Some("<svg/>".to_owned()),
                }),
                checkout: Some(PathBuf::from("/repo")),
                areas: Some([("a".repeat(40), vec!["docs".to_owned()])].into()),
            }),
            QueueCommand::ReportInputs(ReportInputsParams {
                period: "week".to_owned(),
                at: Some("40".to_owned()),
                checkout: Some(PathBuf::from("/repo")),
            }),
            QueueCommand::GraphExport(GraphExportParams { goal: Some(5) }),
        ]
    }

    #[test]
    fn each_command_s_request_reads_back_as_the_same_command() {
        for command in every() {
            let (use_case, params) = command.request();
            assert_eq!(
                QueueCommand::parse(use_case, &params).unwrap(),
                Some(command)
            );
        }
        assert_eq!(
            QueueCommand::parse(UseCase::Show, &json!({})).unwrap(),
            None
        );
    }

    #[test]
    fn params_the_command_line_refuses_are_a_bad_command() {
        for (use_case, params) in [
            (UseCase::Answer, json!({"id": 3})),
            (UseCase::AskClose, json!({"id": 3, "extra": true})),
            // The read names no head, and the record is sent with one.
            (
                UseCase::ReadyInheritRead,
                json!({"id": 3, "reason": "r", "work": {"at": {"task": "in_progress", "run": null}}}),
            ),
            (UseCase::ReadyInherit, json!({"id": 3, "reason": "r"})),
            (UseCase::Rebind, json!({"git_common_dir": "relative/.git"})),
            (UseCase::Rebind, json!({"repo": "/checkout"})),
            (UseCase::Report, json!({"period": "month"})),
            (UseCase::Report, json!({"period": "day", "at": "soon"})),
            (UseCase::Report, json!({"period": "day", "out": "reports"})),
            (
                UseCase::Report,
                json!({"period": "day", "out": "/reports", "print": true}),
            ),
        ] {
            let error = QueueCommand::parse(use_case, &params).unwrap_err();
            assert!(error.downcast_ref::<BadCommand>().is_some(), "{params}");
        }
    }

    /// The policy is asked what the command line asks it before each runs:
    /// the runtime operation's, `queue.export` for the exports, and
    /// nothing for those the application authorizes as it runs them.
    #[test]
    fn each_command_is_authorized_as_the_command_line_authorizes_it() {
        let asked: Vec<(UseCase, Option<Capability>)> = every()
            .iter()
            .map(|command| {
                (
                    command.use_case(),
                    command.authorization().map(|(capability, _)| capability),
                )
            })
            .collect();
        assert_eq!(
            asked,
            [
                (UseCase::Answer, None),
                (UseCase::AskClose, None),
                (UseCase::Hold, None),
                (UseCase::Release, None),
                (UseCase::ReleaseRead, None),
                (UseCase::ReleaseImages, None),
                (UseCase::ReplanRequest, None),
                (UseCase::ReadyInheritRead, None),
                (UseCase::ReadyInherit, None),
                (UseCase::ReadyInheritImages, None),
                (UseCase::Rebind, Some(Capability::QueueAdmin)),
                (UseCase::Recover, Some(Capability::RunRecover)),
                (UseCase::RunScreen, Some(Capability::ScreenRead)),
                (UseCase::RunSend, Some(Capability::ScreenSend)),
                (UseCase::RunLog, Some(Capability::ScreenRead)),
                (UseCase::PlannerScreen, Some(Capability::ScreenRead)),
                (UseCase::PlannerSend, Some(Capability::ScreenSend)),
                (UseCase::PlannerLog, Some(Capability::ScreenRead)),
                (UseCase::PlannerRequest, Some(Capability::PlannerRequest)),
                (UseCase::Report, Some(Capability::ExportFile)),
                (UseCase::ReportInputs, Some(Capability::ExportFile)),
                (UseCase::GraphExport, Some(Capability::ExportFile)),
            ]
        );
        // A task's number names its latest run's task, anything else a run.
        let params = every()
            .into_iter()
            .find_map(|command| match command {
                QueueCommand::RunLog(params) => Some(params),
                _ => None,
            })
            .unwrap();
        assert!(matches!(
            QueueCommand::RunLog(params).authorization(),
            Some((_, Resource::Run { .. }))
        ));
        assert!(matches!(
            QueueCommand::RunScreen(RunParams {
                run: "12".to_owned()
            })
            .authorization(),
            Some((_, Resource::Task { .. }))
        ));
    }
}
