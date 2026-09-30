//! The ask requests of a headless worker (ADR-t813-3 decision 3): a Codex
//! worker's `dagq ask` cannot write the queue from its sandbox, so it
//! writes the ask to its run directory's [`ASK_REQUESTS_DIR`], and the
//! supervisor opens it here, held to what the command itself is held to:
//! the policy, as the worker of the run whose directory holds the request
//! (only its `worker_question` on its own run or task), and the checks of
//! the ask's options. An ask it opens takes the path of any
//! `worker_question` (the inbox is notified, the run waits, the answer is
//! the next turn's request). A request that opens none is recorded
//! refused, as `authorization_denied` when the policy refused it. Each is
//! taken once: `ask_request_taken` is written with the ask in one
//! transaction, and the file is renamed to `<id>.taken` after it.
//!
//! The directory is the worker's, and the supervisor runs outside its
//! sandbox: it must not read, rename or remove for the worker what the
//! sandbox keeps the worker from. The directory is opened without following
//! a link at it or at the run directory ([`RunFiles::open_agent_dir`]) and
//! its entries are handled through that handle only: a link or anything
//! else in place of either, and any entry that is not a regular file, is
//! recorded refused and left as it is, neither followed nor renamed.

use super::*;
use crate::application::commands::{DenialLog, Gate};
use crate::application::{AgentDir, AgentDirHandle, EntryKind};
use crate::domain::ask_request::{ASK_REQUESTS_DIR, AskRequest, pending_id, taken_name, valid_id};
use crate::domain::{ActorContext, Capability, Resource, StaticPolicy};

/// The most names of pending requests one pass looks at, in the order of
/// their ids (random, not their age), and the most it takes that the queue
/// had not taken: a worker that fills its directory holds up no more of
/// the pass than this. What is left waits for the next passes.
const NAMES_PER_PASS: usize = 256;
const REQUESTS_PER_PASS: usize = 64;

/// The largest request read: a question and its options.
const REQUEST_BYTES: usize = 64 * 1024;

/// The supervisor's queue, where a refusal of a worker's request is
/// recorded.
struct QueueLog<'q>(&'q dyn Queue);

impl DenialLog for QueueLog<'_> {
    fn record_denial(&self, payload: Value) -> Result<()> {
        self.0
            .record_queue_event(EventKind::AuthorizationDenied, payload)
            .map(drop)
    }
}

/// The request key a refusal of the directory itself is recorded under:
/// no request id has a `/`.
fn directory_key() -> String {
    format!("{ASK_REQUESTS_DIR}/")
}

/// One pending entry of the directory: `<id>.json`, whatever it is.
struct Pending {
    id: String,
    name: String,
    kind: EntryKind,
}

/// The ask requests of a run, as its directory holds them.
enum Requests {
    /// An interactive run, or no directory.
    None,
    /// No directory of the run's own: why, for its refusal.
    Refused(String),
    /// The open directory and its pending entries in the order of their
    /// ids, at most [`NAMES_PER_PASS`].
    Open(Box<dyn AgentDirHandle>, Vec<Pending>),
}

fn requests(sv: &Supervisor<'_>, run: &TaskRun) -> std::io::Result<Requests> {
    // An interactive run's worker opens its asks itself.
    if !headless(run) {
        return Ok(Requests::None);
    }
    let Some(run_dir) = run.run_dir() else {
        return Ok(Requests::None);
    };
    Ok(
        match sv
            .files
            .open_agent_dir(&Path::new(run_dir).join(ASK_REQUESTS_DIR))?
        {
            AgentDir::Missing => Requests::None,
            AgentDir::Not(kind) => Requests::Refused(format!(
                "{ASK_REQUESTS_DIR} is {}, not a directory: nothing is read through it",
                kind.as_str()
            )),
            AgentDir::ParentNot(kind) => Requests::Refused(format!(
                "the run directory is {}, not a directory: nothing is read through it",
                kind.as_str()
            )),
            AgentDir::Open(dir) => {
                // By name first: only what may be a request is looked at.
                let mut names: Vec<(String, String)> = dir
                    .names()?
                    .into_iter()
                    .filter_map(|name| Some((pending_id(&name)?.to_owned(), name)))
                    .collect();
                names.sort();
                names.truncate(NAMES_PER_PASS);
                let mut pending = Vec::new();
                for (id, name) in names {
                    if let Some(kind) = dir.kind(&name)? {
                        pending.push(Pending { id, name, kind });
                    }
                }
                Requests::Open(dir, pending)
            }
        },
    )
}

/// Whether the headless `run` has a request not taken yet: a turn that
/// ended after the pass took the requests may have left one, so its idle
/// marker is not read as a turn without an ask before the next pass takes
/// it. Only a regular file the queue has not taken counts: an entry that
/// is not one is refused and never read, and a taken request its
/// supervisor could not mark stays taken, so neither holds the run.
pub(super) fn ask_requests_pending(sv: &Supervisor<'_>, run: &TaskRun) -> bool {
    let Ok(Requests::Open(_, pending)) = requests(sv, run) else {
        return false;
    };
    pending.iter().any(|entry| {
        entry.kind == EntryKind::File
            && !sv
                .queue
                .ask_request_taken(run.id(), &entry.id)
                .unwrap_or(true)
    })
}

/// Open the pending ask requests in the run directory of the headless
/// `run`, in the order of their ids. Nothing here ends the run: what the
/// worker wrote to its directory, or a queue that failed to take it, is
/// reported, and a request not taken is tried again at the next pass.
pub(super) fn take_ask_requests(sv: &mut Supervisor<'_>, run: &TaskRun) {
    let requests = match requests(sv, run) {
        Ok(requests) => requests,
        Err(error) => {
            warn!(run_id = %run.id(), "the ask requests of {} are not readable: {error}", run.id());
            return;
        }
    };
    match requests {
        Requests::None => {}
        Requests::Refused(reason) => {
            if let Err(error) = refuse(sv, run, &directory_key(), &reason) {
                warn!(run_id = %run.id(), "the ask requests of {} are not taken yet: {error:#}", run.id());
            }
        }
        Requests::Open(dir, pending) => {
            let mut taken = 0;
            for entry in &pending {
                if taken == REQUESTS_PER_PASS {
                    break;
                }
                match take(sv, run, &*dir, entry) {
                    Ok(true) => taken += 1,
                    Ok(false) => {}
                    Err(error) => {
                        warn!(run_id = %run.id(), "ask request {} of {} is not taken yet: {error:#}", entry.id, run.id());
                    }
                }
            }
        }
    }
}

/// Record `id` refused with `reason` unless it was taken already (looked at
/// first, so a refusal taken before costs no write).
fn refuse(sv: &mut Supervisor<'_>, run: &TaskRun, id: &str, reason: &str) -> Result<()> {
    if !sv.queue.ask_request_taken(run.id(), id)?
        && sv.queue.refuse_ask_request(run.id(), id, reason)?
    {
        warn!(run_id = %run.id(), "ask request {id} of {} opens no ask: {reason}", run.id());
    }
    Ok(())
}

/// Take the request of `entry`: open its ask, or record why it opens none,
/// then mark the file taken; whether the queue had not taken it before. An
/// entry that is not a regular file is refused and left as it is. A
/// failure of the queue records nothing and leaves the file for the next
/// pass.
fn take(
    sv: &mut Supervisor<'_>,
    run: &TaskRun,
    dir: &dyn AgentDirHandle,
    entry: &Pending,
) -> Result<bool> {
    let id = &entry.id;
    let new = !sv.queue.ask_request_taken(run.id(), id)?;
    if entry.kind != EntryKind::File {
        if new {
            refuse(
                sv,
                run,
                id,
                &format!(
                    "{} is {}, not a regular file: it is neither read nor moved",
                    entry.name,
                    entry.kind.as_str()
                ),
            )?;
        }
        return Ok(new);
    }
    if new {
        match check(sv, run, dir, entry) {
            Ok(ask) => {
                if let Some(outcome) = sv.queue.ask_on_request(run.id(), id, ask)? {
                    info!(run_id = %run.id(), ask_id = %outcome.ask.id, "ask request {id} of {} opened ask {}", run.id(), outcome.ask.id);
                    // The ask stands: a notification that fails is only
                    // reported, as the command reports it.
                    match crate::application::ask::notify(
                        &mut *sv.queue,
                        &sv.layout.main_checkout,
                        &outcome,
                        sv.cmux,
                    ) {
                        Ok(value) => {
                            if let Some(error) = value.get("notify_error") {
                                warn!(run_id = %run.id(), "the inbox was not notified of ask {}: {error}", outcome.ask.id);
                            }
                        }
                        Err(error) => {
                            warn!(run_id = %run.id(), "the inbox was not notified of ask {}: {error:#}", outcome.ask.id);
                        }
                    }
                }
            }
            Err(reason) => refuse(sv, run, id, &reason)?,
        }
    }
    // Renamed or removed through the directory: an entry swapped for a link
    // meanwhile is moved itself, never what it points at.
    if let Err(error) = dir.rename(&entry.name, &taken_name(id)) {
        // Taken all the same (the queue says so): a file that cannot be
        // marked (the worker put something at its taken name) goes, so it
        // is not looked at again. One that cannot go either (the worker
        // made the directory read-only) stays, taken, and counts no more as
        // pending: only noted, since every pass meets it again.
        if let Err(removal) = dir.remove(&entry.name) {
            tracing::debug!(run_id = %run.id(), "taken ask request {} of {} stays: {error}; {removal}", entry.name, run.id());
        }
    }
    Ok(new)
}

/// The ask the request of `entry` opens, or why it opens none: a request
/// that cannot be read (no longer a regular file, too large), is no
/// request, names another id than its file, or fails the checks of
/// [`AskRequest::worker_ask`], or that the policy refuses to the worker of
/// `run` (recorded as `authorization_denied` too).
fn check(
    sv: &Supervisor<'_>,
    run: &TaskRun,
    dir: &dyn AgentDirHandle,
    entry: &Pending,
) -> Result<NewAsk, String> {
    let id = &entry.id;
    if !valid_id(id) {
        return Err(format!("{id:?} is no request id"));
    }
    let text = dir
        .read(&entry.name, REQUEST_BYTES)
        .map_err(|error| format!("unreadable: {error}"))?;
    let request: AskRequest =
        serde_json::from_slice(&text).map_err(|error| format!("malformed: {error}"))?;
    if request.id != *id {
        return Err(format!(
            "the request names id {:?} in the file of {id:?}",
            request.id
        ));
    }
    let worker = ActorContext::worker(run.id(), run.task_id());
    let ask = request.worker_ask(&worker, Some(run.task_id()))?;
    Gate {
        actor: &worker,
        authorizer: &StaticPolicy,
    }
    .authorize(
        &QueueLog(&*sv.queue),
        Capability::AskOpen,
        &Resource::NewAsk {
            kind: ask.kind.clone(),
            run: ask.run_id.clone(),
            task: ask.task_id,
        },
    )
    .map_err(|error| format!("{error:#}"))?;
    Ok(ask)
}
