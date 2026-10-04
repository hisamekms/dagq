//! What a person, or the inbox at a person's word, reads from and sends
//! to a run's or a planner's session without typing `cmux` (ADR-t1228-1
//! decisions 4, 5 and 7): `run screen` / `planner screen` read the
//! session's screen, at most [`MAX_LINES`] lines, and `run send` /
//! `planner send` type into it only a key of [`SessionKey`] or the answer
//! of an answered ask about that session, never free text. The target is
//! named by its run (or task) or planner id and its workspace is looked up
//! in the queue. Each read and each send is recorded with its actor
//! (`screen_read`, `screen_input_sent`), without the screen's text. A
//! headless run or planner has no screen: a read says where its turns are
//! and a send is refused (ADR-t1228-1 decision 4, ADR-t1533-1). The
//! caller authorizes the command first (`screen.read`, `screen.send`;
//! `docs/design/authorization.md`).

use std::path::Path;

use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};

use super::planner::planner_dir;
use super::supervise::{Input, Submission, submit_input};
use super::{AgentSignals, PlannerAnswerRoute, Queue, WorkspaceBackend};
use crate::domain::{
    AskId, AskKind, EventKind, PlannerId, PlannerRoute, PlannerSession, Resource, RunId, TaskId,
    TaskRun, turn::turns_dir, worker::WorkerMode,
};

/// The lines a read returns when the caller names no number.
pub const DEFAULT_LINES: usize = 40;
/// The most lines a read returns: a larger number is cut to it.
pub const MAX_LINES: usize = 200;
/// The most keys one send types.
pub const MAX_KEYS: usize = 10;

/// A key a send may type: choosing and confirming a dialog's option,
/// submitting what is left in the input box, and `/exit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKey {
    Enter,
    Escape,
    Up,
    Down,
    /// A dialog's numbered option, 1 to 9.
    Digit(u8),
    /// `/exit`, typed and submitted as the supervisor types it.
    Exit,
}

impl SessionKey {
    /// The names the command line takes.
    pub const NAMES: &'static str = "enter, escape, up, down, 1-9, exit";

    /// The key named `name`; anything outside the set is refused.
    pub fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "enter" => Self::Enter,
            "escape" => Self::Escape,
            "up" => Self::Up,
            "down" => Self::Down,
            "exit" => Self::Exit,
            digit if digit.len() == 1 && matches!(digit.as_bytes()[0], b'1'..=b'9') => {
                Self::Digit(digit.as_bytes()[0] - b'0')
            }
            other => bail!(
                "{other:?} is not a key a session may be sent ({})",
                Self::NAMES
            ),
        })
    }

    /// The key as cmux's `send-key` names it; `None` for `/exit`, which is
    /// typed.
    fn cmux_key(self) -> Option<String> {
        Some(match self {
            Self::Enter => "enter".to_owned(),
            Self::Escape => "escape".to_owned(),
            Self::Up => "up".to_owned(),
            Self::Down => "down".to_owned(),
            Self::Digit(digit) => digit.to_string(),
            Self::Exit => return None,
        })
    }

    /// The key as the command line and the record name it.
    pub fn name(self) -> String {
        self.cmux_key().unwrap_or_else(|| "exit".to_owned())
    }
}

/// The run a `run screen` / `run send` names: a run id, or a task id for
/// the task's latest run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunTarget {
    Run(RunId),
    Task(TaskId),
}

impl RunTarget {
    /// A number is a task id; anything else a run id.
    pub fn parse(text: &str) -> Result<Self> {
        match text.trim().parse::<i64>() {
            Ok(id) => Ok(Self::Task(TaskId::new(id))),
            Err(_) => Ok(Self::Run(RunId::new(text.trim())?)),
        }
    }

    /// What the command acts on for the authorizer.
    pub fn resource(&self) -> Resource {
        match self {
            Self::Run(id) => Resource::run(id.clone()),
            Self::Task(id) => Resource::task(*id),
        }
    }
}

/// What a send types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sending {
    /// Keys of the set, in order.
    Keys(Vec<SessionKey>),
    /// The answer of this ask, as the supervisor delivers one.
    Answer(AskId),
}

impl Sending {
    /// Keys parsed from their names, or the ask; a send needs exactly one
    /// of them.
    pub fn parse(keys: &[String], answer: Option<i64>) -> Result<Self> {
        match (keys.is_empty(), answer) {
            (false, None) => {
                ensure!(
                    keys.len() <= MAX_KEYS,
                    "at most {MAX_KEYS} keys are sent at once"
                );
                let keys = keys
                    .iter()
                    .map(|key| SessionKey::parse(key))
                    .collect::<Result<Vec<_>>>()?;
                ensure!(
                    !keys.contains(&SessionKey::Exit) || keys.len() == 1,
                    "exit is sent alone"
                );
                Ok(Self::Keys(keys))
            }
            (true, Some(ask)) => Ok(Self::Answer(AskId::new(ask))),
            _ => bail!("send either --key (repeatable) or --answer ASK"),
        }
    }
}

/// The ports a read or a send uses.
pub struct ScreenPorts<'a> {
    pub cmux: &'a dyn WorkspaceBackend,
    pub signals: &'a dyn AgentSignals,
}

/// The run `target` names: the run, or its task's latest run.
pub(crate) fn resolve_run(queue: &mut dyn Queue, target: &RunTarget) -> Result<TaskRun> {
    match target {
        RunTarget::Run(id) => queue.run(id),
        RunTarget::Task(id) => match queue.show(*id)?.runs.pop() {
            Some(run) => Ok(run),
            None => bail!("task {id} has no run"),
        },
    }
}

/// The workspace of `run`'s session that a person may read or type into:
/// one with a terminal (not a headless run's) that is still open.
fn run_workspace(run: &TaskRun) -> Result<String> {
    ensure!(
        run.worker_mode() != WorkerMode::Headless,
        "run {} is headless: its session has no screen and takes no keys (read its turns in {}/turns; answer its asks with `answer`)",
        run.id(),
        run.run_dir().unwrap_or("its run directory")
    );
    ensure!(
        run.workspace_closed_at().is_none(),
        "the workspace of run {} is closed",
        run.id()
    );
    match run.workspace_id() {
        Some(workspace) => Ok(workspace.to_owned()),
        None => bail!("run {} has no workspace", run.id()),
    }
}

/// The workspace of `planner`'s session, while the queue has not given it
/// up.
fn planner_workspace(planner: &PlannerSession) -> Result<String> {
    ensure!(
        planner.closed_at.is_none(),
        "planner {} is closed",
        planner.id
    );
    match &planner.workspace_id {
        Some(workspace) => Ok(workspace.clone()),
        None => bail!("planner {} has no workspace", planner.id),
    }
}

/// The last `lines` lines of `screen`, its trailing blank lines left out,
/// and whether lines were cut.
fn last_lines(screen: &str, lines: usize) -> (String, usize, bool) {
    let all: Vec<&str> = screen.lines().collect();
    let end = all
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(0, |last| last + 1);
    let start = end.saturating_sub(lines);
    (all[start..end].join("\n"), end - start, start > 0)
}

/// Read the screen of `workspace`, at most `lines` lines (cut to
/// [`MAX_LINES`]), and the record of the read without the text.
fn read(cmux: &dyn WorkspaceBackend, workspace: &str, lines: usize) -> Result<(Value, Value)> {
    let limit = lines.clamp(1, MAX_LINES);
    let screen = cmux.capture(workspace)?;
    let (text, returned, cut) = last_lines(&screen, limit);
    let record = json!({
        "workspace_id": workspace,
        "lines_requested": lines,
        "lines_limit": limit,
        "lines": returned,
    });
    let mut output = record.clone();
    output["truncated"] = json!(cut);
    output["screen"] = json!(text);
    Ok((output, record))
}

/// `run screen`: the screen of the session of the run `target` names. A
/// headless run has none; the reply says where its turns are instead.
pub fn run_screen(
    queue: &mut dyn Queue,
    cmux: &dyn WorkspaceBackend,
    target: &RunTarget,
    lines: usize,
) -> Result<Value> {
    let run = resolve_run(queue, target)?;
    if run.worker_mode() == WorkerMode::Headless {
        return Ok(json!({
            "run_id": run.id(),
            "task_id": run.task_id(),
            "worker_mode": run.worker_mode(),
            "screen": null,
            "reason": "a headless run has no screen",
            "turns": run.run_dir().map(|dir| format!("{dir}/turns")),
        }));
    }
    let workspace = run_workspace(&run)?;
    let (mut output, mut record) = read(cmux, &workspace, lines)?;
    record["target"] = json!("run");
    queue.record_runtime_event(run.id(), EventKind::ScreenRead, record)?;
    output["run_id"] = json!(run.id());
    output["task_id"] = json!(run.task_id());
    Ok(output)
}

/// `planner screen`: the screen of `planner`'s session. A headless planner
/// has none; the reply says where its turns are instead (`turns/` of its
/// directory under `planners_dir`), and nothing is read or recorded.
pub fn planner_screen(
    queue: &mut dyn Queue,
    cmux: &dyn WorkspaceBackend,
    planners_dir: &Path,
    planner: PlannerId,
    lines: usize,
) -> Result<Value> {
    let session = queue.planner(planner)?;
    if session.route == PlannerRoute::Headless {
        return Ok(json!({
            "planner_id": planner,
            "route": session.route,
            "screen": null,
            "reason": "a headless planner has no screen",
            "turns": turns_dir(&planner_dir(planners_dir, planner)).display().to_string(),
        }));
    }
    let workspace = planner_workspace(&session)?;
    let (mut output, mut record) = read(cmux, &workspace, lines)?;
    record["target"] = json!("planner");
    record["planner_id"] = json!(planner);
    queue.record_queue_event(EventKind::ScreenRead, record)?;
    output["planner_id"] = json!(planner);
    Ok(output)
}

/// Type `keys` into `workspace`, `/exit` as the supervisor types it.
fn send_keys(ports: &ScreenPorts<'_>, workspace: &str, keys: &[SessionKey]) -> Result<Value> {
    if keys == [SessionKey::Exit] {
        let (submission, retries) =
            submit_input(ports.cmux, ports.signals, workspace, Input::Exit)?;
        return Ok(outcome(&submission, retries));
    }
    for key in keys {
        if let Some(name) = key.cmux_key() {
            ports.cmux.send_key(workspace, &name)?;
        }
    }
    Ok(json!({"outcome": "sent"}))
}

/// What a typed text or `/exit` came to, as the record and the reply name
/// it.
fn outcome(submission: &Submission, retries: usize) -> Value {
    let outcome = match submission {
        Submission::Submitted(_) | Submission::Queued => "submitted",
        Submission::Dialog(_) => "dialog",
        Submission::Stuck(_) => "stuck",
        Submission::Unsent => "unsent",
    };
    json!({"outcome": outcome, "retries": retries})
}

/// The answered ask `id`, refused when it is still open.
fn answered(queue: &dyn Queue, id: AskId) -> Result<(crate::domain::Ask, String)> {
    let ask = queue.read_ask(id)?;
    match ask.answer.clone() {
        Some(answer) => Ok((ask, answer)),
        None => bail!("ask {id} has no answer yet: answer it first"),
    }
}

/// The base of a send's record and reply.
fn sent_record(workspace: &str, sending: &Sending) -> Value {
    match sending {
        Sending::Keys(keys) => json!({
            "workspace_id": workspace,
            "input": "keys",
            "keys": keys.iter().map(|key| key.name()).collect::<Vec<_>>(),
        }),
        Sending::Answer(ask) => json!({
            "workspace_id": workspace,
            "input": "answer",
            "ask_id": ask,
        }),
    }
}

fn merge(mut record: Value, sent: Value) -> Value {
    if let (Some(record), Value::Object(sent)) = (record.as_object_mut(), sent) {
        record.extend(sent);
    }
    record
}

/// `run send`: type `sending` into the session of the run `target` names.
/// An answer must be of an answered ask on that run, and is typed as the
/// supervisor delivers a worker's answer. A headless run takes no keys.
pub fn run_send(
    queue: &mut dyn Queue,
    ports: &ScreenPorts<'_>,
    target: &RunTarget,
    sending: &Sending,
) -> Result<Value> {
    let run = resolve_run(queue, target)?;
    let workspace = run_workspace(&run)?;
    let sent = match sending {
        Sending::Keys(keys) => send_keys(ports, &workspace, keys)?,
        Sending::Answer(id) => {
            let (ask, answer) = answered(queue, *id)?;
            ensure!(
                ask.run_id.as_ref() == Some(run.id()),
                "ask {id} is not about run {}",
                run.id()
            );
            let text = super::prompt::answer_text(&run, ask.id, &answer);
            let (submission, retries) =
                submit_input(ports.cmux, ports.signals, &workspace, Input::Text(&text))?;
            // The supervisor does not type a worker's answer again.
            if ask.kind == AskKind::WorkerQuestion && ask.closed_at.is_none() {
                queue.ask_delivered(ask.id, &workspace)?;
            }
            outcome(&submission, retries)
        }
    };
    let mut record = merge(sent_record(&workspace, sending), sent);
    record["target"] = json!("run");
    queue.record_runtime_event(run.id(), EventKind::ScreenInputSent, record.clone())?;
    record["run_id"] = json!(run.id());
    record["task_id"] = json!(run.task_id());
    Ok(record)
}

/// `planner send`: type `sending` into `planner`'s session. An answer must
/// be of an answered `planner_question` whose answer goes to that planner,
/// typed as the supervisor types it. A headless planner takes nothing.
pub fn planner_send(
    queue: &mut dyn Queue,
    ports: &ScreenPorts<'_>,
    planner: PlannerId,
    sending: &Sending,
) -> Result<Value> {
    let session = queue.planner(planner)?;
    ensure!(
        session.route != PlannerRoute::Headless,
        "planner {planner} is headless: its session has no screen and takes no keys (the supervisor delivers the answer of its question as its next turn; hand it a follow-up with `planner request`)"
    );
    let workspace = planner_workspace(&session)?;
    let sent = match sending {
        Sending::Keys(keys) => send_keys(ports, &workspace, keys)?,
        Sending::Answer(id) => {
            let (ask, answer) = answered(queue, *id)?;
            ensure!(
                ask.kind == AskKind::PlannerQuestion
                    && matches!(
                        queue.planner_answer_route(&ask)?,
                        PlannerAnswerRoute::Planner(to) if to.id == planner
                    ),
                "ask {id} is not a question whose answer goes to planner {planner}"
            );
            // Claimed first, as the supervisor claims its typing, so the
            // two do not both type it.
            ensure!(
                queue.claim_planner_answer(ask.id, planner, &workspace)?,
                "the answer of ask {id} is being typed into planner {planner} already, or no longer goes there"
            );
            let text = format!("answer to ask {}: {answer}", ask.id);
            let (submission, retries) =
                submit_input(ports.cmux, ports.signals, &workspace, Input::Text(&text))?;
            // The supervisor does not type it again.
            queue.ask_delivered(ask.id, &workspace)?;
            outcome(&submission, retries)
        }
    };
    let mut record = merge(sent_record(&workspace, sending), sent);
    record["target"] = json!("planner");
    record["planner_id"] = json!(planner);
    queue.record_queue_event(EventKind::ScreenInputSent, record.clone())?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_keys_of_the_set_parse() {
        for (name, key) in [
            ("enter", SessionKey::Enter),
            ("escape", SessionKey::Escape),
            ("up", SessionKey::Up),
            ("down", SessionKey::Down),
            ("1", SessionKey::Digit(1)),
            ("9", SessionKey::Digit(9)),
            ("exit", SessionKey::Exit),
        ] {
            assert_eq!(SessionKey::parse(name).unwrap(), key);
            assert_eq!(key.name(), name);
        }
        for refused in ["0", "10", "a", "ctrl-c", "/exit", "Enter", "", "enter "] {
            assert!(SessionKey::parse(refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn a_send_is_keys_or_an_answer_never_both_or_text() {
        let keys = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            Sending::parse(&keys(&["down", "enter"]), None).unwrap(),
            Sending::Keys(vec![SessionKey::Down, SessionKey::Enter])
        );
        assert_eq!(
            Sending::parse(&[], Some(3)).unwrap(),
            Sending::Answer(AskId::new(3))
        );
        assert!(Sending::parse(&[], None).is_err());
        assert!(Sending::parse(&keys(&["enter"]), Some(3)).is_err());
        assert!(Sending::parse(&keys(&["hello world"]), None).is_err());
        assert!(Sending::parse(&keys(&["exit", "enter"]), None).is_err());
        assert!(Sending::parse(&keys(&["down"; MAX_KEYS + 1]), None).is_err());
    }

    #[test]
    fn a_read_keeps_the_last_lines_without_the_trailing_blank_ones() {
        let screen = "a\nb\nc\n\n  \n";
        assert_eq!(last_lines(screen, 2), ("b\nc".to_owned(), 2, true));
        assert_eq!(last_lines(screen, 40), ("a\nb\nc".to_owned(), 3, false));
        assert_eq!(last_lines("", 5), (String::new(), 0, false));
    }

    #[test]
    fn a_target_is_a_task_by_number_and_a_run_otherwise() {
        assert_eq!(
            RunTarget::parse("12").unwrap(),
            RunTarget::Task(TaskId::new(12))
        );
        let run = RunTarget::parse("5b7e2393-1023").unwrap();
        assert!(matches!(run, RunTarget::Run(_)));
        assert!(matches!(run.resource(), Resource::Run { .. }));
        assert!(matches!(
            RunTarget::parse("7").unwrap().resource(),
            Resource::Task { .. }
        ));
    }
}
