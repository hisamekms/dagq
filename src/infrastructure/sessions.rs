//! The spans of the Claude sessions dagq uses (ADR-0048 decision 2): after
//! an event that starts or ends one is inserted, the `session_opened` /
//! `session_closed` events [`crate::domain::sessions::changes`] decides are
//! written in the same transaction, at the same time as that event. A span
//! that closes takes its transcript's turns with it (its active time,
//! decision 8), except a hook's close: the supervisor finishes its intake
//! after it closes, alongside the finished turns of spans still open
//! ([`record_open_turns`]). No transcript is read under a write
//! lock (task 543): a write that may close spans reads theirs first
//! ([`read_before`]), and the close takes what was read. The close's
//! analysis of it (its turns, tokens, models and work breakdown) is made
//! there too, to the end the close is expected at, and the work's
//! `worktime.jsonl` is written after the commit (task 1334): under the lock
//! the close only checks that analysis against what it reads of the queue,
//! and inserts.

use crate::domain::event_kind::{self, EventKind};
use std::cell::RefCell;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use tracing::{debug, info};

use super::{
    sqlite::{event_row, json_col},
    transcripts::ClaudeTranscripts,
};
use crate::{
    application::{TranscriptSource, Transcripts},
    domain::{
        EventId, GoalId, RunEvent, RunId, TaskId,
        sessions::{
            GOAL_REVIEW, HEADLESS_ROUTE, HOOK_KINDS, INFERRED, JOB_FINISHED, OpenSpan, PLAN_REVIEW,
            REVIEW, RUN_SESSION, RUNTIME_PLANNER, SESSION_CLOSED, SESSION_OPENED, SESSION_TURNS,
            Scope, SessionHook, SpanChange, SpanContext, changes, hook_changes, queue_span_kind,
            scope,
        },
        stats::rfc3339_millis,
        tokens::{self, ModelUse, SpanTokens, TokenUsage},
        transcript::{
            TRANSCRIPT_NOT_CLAUDE, TRANSCRIPT_NOT_READ_BEFORE, Transcript, TranscriptRecord, Turn,
            Turns, Unreadable, millis_text, span_turns, turns,
        },
        turn::{HeadlessSpan, TURN_COMMANDS_MISSING, codex_span_turns},
        worktime::{self, Breakdown, Command},
    },
};

/// The run directory's file of the commands of its sessions (task 514).
pub const WORKTIME_FILE: &str = "worktime.jsonl";

thread_local! {
    /// The transcripts of the spans a write transaction about to begin on
    /// this thread may close, read before it began ([`read_before`]).
    static READ_BEFORE: RefCell<Vec<(OpenSpan, Result<ReadTranscript, Unreadable>)>> =
        const { RefCell::new(Vec::new()) };
    /// Whether the queue's repository is dagq's source, judged before the
    /// same transaction began for the run sessions' spans it may close
    /// ([`dagq_source`]): the judgement runs Git, never under the write
    /// lock.
    static SOURCE_BEFORE: RefCell<Vec<(EventId, bool)>> = const { RefCell::new(Vec::new()) };
    /// The `worktime.jsonl` lines of the spans a write transaction on this
    /// thread closed, written when the [`ReadBefore`] of their spans is
    /// dropped, if their transaction committed (task 1334): the hooks
    /// [`watch_commits`] puts on the connection follow it.
    static WORKTIME_AFTER: RefCell<Vec<WorktimeLines>> = const { RefCell::new(Vec::new()) };
}

/// The SQL function that names a connection [`watch_commits`] watches.
const CONNECTION_FUNCTION: &str = "dagq_sessions_connection";

/// A span's transcript read before a write transaction, with its turns and
/// the close's analysis to the end it is expected at (task 1334).
#[derive(Debug)]
struct ReadTranscript {
    transcript: Transcript,
    turns: Turns,
    last_at: Option<i64>,
    analysis: Option<Analysis>,
}

/// What a close analyses of a transcript: from `start` to `end`, and to
/// `cut` for its tokens and models (unix milliseconds).
#[derive(Debug, Clone, PartialEq)]
struct Want {
    start: i64,
    end: i64,
    cut: i64,
    /// Whether its tokens are the transcript's (not a headless span's).
    usage: bool,
    work: Option<WorkInputs>,
}

/// What a run session's work breakdown is made with, beside its
/// transcript.
#[derive(Debug, Clone, PartialEq)]
struct WorkInputs {
    /// Where its `worktime.jsonl` goes; not part of the breakdown.
    run_dir: Option<String>,
    verification: Vec<String>,
    source: bool,
}

/// The close's analysis of a transcript for a [`Want`].
#[derive(Debug)]
struct Analysis {
    want: Want,
    /// Every turn of the span (those recorded already included).
    turns: Vec<Turn>,
    usage: Option<Result<TokenUsage, &'static str>>,
    models: Vec<ModelUse>,
    work: Option<Breakdown>,
    /// Its tokens and models to every cut, to pick those to an earlier
    /// one.
    tokens: SpanTokens,
}

/// The analysis for `want` of `records`, whose turns are `turns`.
fn analyse(records: &[TranscriptRecord], turns: &Turns, want: Want) -> Analysis {
    #[cfg(test)]
    tests::ANALYSES.with(|analyses| analyses.set(analyses.get() + 1));
    let tokens = SpanTokens::new(records, want.start, want.cut);
    let (usage, models) = tokens.at(want.cut);
    Analysis {
        turns: span_turns(turns.all(), want.start, Some(want.end), None),
        usage: want.usage.then_some(usage),
        models,
        tokens,
        work: want.work.as_ref().map(|work| {
            worktime::breakdown(
                records,
                want.start,
                want.end,
                &work.verification,
                work.source,
            )
        }),
        want,
    }
}

impl ReadTranscript {
    fn new(transcript: Transcript) -> Self {
        Self {
            turns: turns(&transcript.records),
            last_at: transcript.last_at(),
            transcript,
            analysis: None,
        }
    }

    /// The analysis for `want`. Inside a write transaction it is only the
    /// one made before it (task 1334), moved to `want`'s end and cut
    /// ([`Analysis::to`]); nothing of the transcript is analysed under the
    /// write lock, and `None` when it cannot be moved. Outside one,
    /// analysed now when it was not before or cannot be moved.
    fn analysis(&mut self, conn: &Connection, want: Want) -> Option<Analysis> {
        if let Some(made) = self.analysis.take()
            && let Some(analysis) = made.to(self.last_at, &want)
        {
            return Some(analysis);
        }
        conn.is_autocommit()
            .then(|| analyse(&self.transcript.records, &self.turns, want))
    }
}

impl Analysis {
    /// The analysis for `want` made from this one of a transcript whose
    /// last record is at `last`, as [`analyse`] would make it, with
    /// nothing of the transcript read (task 1334). To an earlier end and
    /// cut: the turns are cut there, the tokens and models those it made
    /// to that cut are picked ([`SpanTokens::at`]), and the work breakdown
    /// is cut there ([`Breakdown::retarget`]). To a later one: the same,
    /// the breakdown longer, when every record is before this one's. Its repeats of `integrate`'s checks are counted for the
    /// verification `want` read. `None` when it cannot be: another start,
    /// kind of tokens or source, or a later end or cut with a record at or
    /// after this one's.
    fn to(mut self, last: Option<i64>, want: &Want) -> Option<Analysis> {
        let refused = |made: &Want| {
            info!(
                "session transcript read before the write does not fit its close: made for {made:?}, wanted {want:?}"
            );
            None
        };
        let inputs = |want: &Want| {
            (
                want.start,
                want.usage,
                want.work.as_ref().map(|work| work.source),
            )
        };
        let after = |made: i64, wanted: i64| wanted > made && last.is_some_and(|last| last >= made);
        if inputs(&self.want) != inputs(want)
            || after(self.want.end, want.end)
            || after(self.want.cut, want.cut)
        {
            return refused(&self.want);
        }
        if want.end < self.want.end {
            // The turns that start before the end, cut there: those of the
            // span to that end.
            self.turns.retain(|turn| turn.start < want.end);
            for turn in &mut self.turns {
                turn.end = turn.end.min(want.end);
            }
        }
        if want.cut < self.want.cut {
            let (usage, models) = self.tokens.at(want.cut);
            self.usage = want.usage.then_some(usage);
            self.models = models;
        }
        if let (Some(work), Some(inputs)) = (self.work.as_mut(), &want.work) {
            if !work.retarget(last, want.end) {
                return refused(&self.want);
            }
            work.verify(&inputs.verification);
        }
        self.want = want.clone();
        Some(self)
    }
}

/// The `worktime.jsonl` lines of a close of the span `opened` in a write
/// transaction on `connection`, to append to `path` once the transaction
/// is committed.
struct WorktimeLines {
    opened: EventId,
    connection: i64,
    commit: Commit,
    path: std::path::PathBuf,
    span: Value,
    commands: Vec<Command>,
}

/// Where the transaction of a close's [`WorktimeLines`] is.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Commit {
    /// Neither committed nor rolled back yet.
    Open,
    /// Its commit began: rolled back if it fails.
    Committing,
    /// Committed: the connection prepared a statement, wrote or committed
    /// again since, which it does only once the commit finished.
    Committed,
}

/// Put on `conn` the hooks that follow its transactions for the
/// `worktime.jsonl` lines of the closes in them (task 1334): a commit's
/// lines are written, a rolled back one's never. A close is known by the
/// transaction that wrote it, not by its event's id, which SQLite gives
/// again after a rollback (to another writer's close of the same span).
pub(super) fn watch_commits(conn: &Connection) -> Result<()> {
    static NEXT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    conn.create_scalar_function(
        CONNECTION_FUNCTION,
        0,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        move |_| Ok(id),
    )?;
    // The hooks run on the thread that commits or rolls back, the one that
    // wrote the lines; they touch no database. A commit that fails after
    // its hook is rolled back at once, in the same statement, and the
    // rollback drops its lines; a rollback after anything else on the
    // connection is another transaction's, and leaves them.
    conn.commit_hook(Some(move || {
        follow_commits(id, |commit| match commit {
            // The commit before it finished: this is another transaction.
            Commit::Committing => Some(Commit::Committed),
            Commit::Open => Some(Commit::Committing),
            Commit::Committed => Some(Commit::Committed),
        });
        false
    }))?;
    conn.rollback_hook(Some(move || {
        follow_commits(id, |commit| (commit == Commit::Committed).then_some(commit));
    }))?;
    // A statement prepared (another transaction's `BEGIN`, a `ROLLBACK`)
    // or a row written after a commit began comes after that commit
    // finished.
    conn.update_hook(Some(move |_, _: &str, _: &str, _| commit_finished(id)))?;
    conn.authorizer(Some(move |_: rusqlite::hooks::AuthContext<'_>| {
        commit_finished(id);
        rusqlite::hooks::Authorization::Allow
    }))?;
    Ok(())
}

/// The commit begun on `connection` finished: its lines are committed.
fn commit_finished(connection: i64) {
    follow_commits(connection, |commit| match commit {
        Commit::Committing => Some(Commit::Committed),
        commit => Some(commit),
    });
}

/// Move the lines of the transactions of `connection` on this thread to
/// what `next` makes of where they are; `None` drops them.
fn follow_commits(connection: i64, next: impl Fn(Commit) -> Option<Commit>) {
    let _ = WORKTIME_AFTER.try_with(|after| {
        let Ok(mut after) = after.try_borrow_mut() else {
            return;
        };
        after.retain_mut(|lines| {
            if lines.connection != connection {
                return true;
            }
            match next(lines.commit) {
                Some(commit) => {
                    lines.commit = commit;
                    true
                }
                None => false,
            }
        });
    });
}

impl WorktimeLines {
    /// Append them, logging only when that fails.
    fn append(&self) {
        let lines: String = self
            .commands
            .iter()
            .map(|command| format!("{}\n", command.line(&self.span)))
            .collect();
        if let Err(error) = super::agent_dir::append(&self.path, lines.as_bytes()) {
            info!(
                "session span {} ({}): {} not written: {error}",
                self.opened,
                self.span["kind"].as_str().unwrap_or_default(),
                self.path.display()
            );
        }
    }
}

/// The spans a write about to begin may close.
#[derive(Debug, Clone, Copy)]
pub(super) enum Closing<'a> {
    /// The run's, by events of these kinds on it.
    Run(&'a RunId, &'a [&'a str]),
    /// The run's, by an event of this kind and payload on it: a revise's
    /// closes end when it was sent.
    Event(&'a RunId, &'a str, &'a Value),
    /// The observer's, by an event of the queue of this kind and payload.
    Queue(&'a str, &'a Value),
    /// The plan review's of this id, or of every plan review when `None`;
    /// closed as inferred when `true`, as `close_plan_review` closes them.
    PlanReviews(Option<i64>, bool),
    /// The goal review's of this id, or of every goal review when `None`;
    /// closed as inferred when `true`, as `close_goal_review` closes them.
    GoalReviews(Option<i64>, bool),
    /// These spans, each closed for this reason.
    Spans(&'a [(OpenSpan, &'static str)]),
}

/// The transcripts [`read_before`] read, for the closes of the write that
/// follows it on this thread; dropped with it. Dropped after that write
/// (its owner keeps it to the end of the write), it writes the
/// `worktime.jsonl` lines of the closes of its spans whose transactions
/// committed (task 1334): those rolled back were dropped then.
#[must_use = "the transcripts are there only while this lives"]
pub(super) struct ReadBefore {
    spans: Vec<EventId>,
}

impl Drop for ReadBefore {
    fn drop(&mut self) {
        READ_BEFORE.with_borrow_mut(|read| {
            read.retain(|(span, _)| !self.spans.contains(&span.opened_event_id));
        });
        SOURCE_BEFORE.with_borrow_mut(|read| {
            read.retain(|(span, _)| !self.spans.contains(span));
        });
        let lines: Vec<WorktimeLines> = WORKTIME_AFTER.with_borrow_mut(|after| {
            let (mine, others) = std::mem::take(after)
                .into_iter()
                .partition(|lines| self.spans.contains(&lines.opened));
            *after = others;
            mine
        });
        for lines in lines {
            if lines.commit == Commit::Open {
                // Left out rather than perhaps written twice: a close
                // rolled back is closed again, with its lines, later.
                info!(
                    "session span {}: {} not written, its transaction was neither committed nor rolled back",
                    lines.opened,
                    lines.path.display()
                );
            } else {
                lines.append();
            }
        }
    }
}

/// Read, before a write transaction begins, the transcripts of the open
/// spans `closing` may close, for their closes in it (ADR-0048 decision
/// 10): a worker's transcript can be megabytes, and reading it under the
/// write lock kept other processes' writes waiting past the busy timeout.
/// A span that opens between this and the transaction closes without its
/// active time ([`TRANSCRIPT_NOT_READ_BEFORE`]). Each transcript is also
/// analysed for its close as if it closed now (task 1334). Called inside a
/// transaction it reads nothing.
pub(super) fn read_before(conn: &Connection, closing: Closing<'_>) -> Result<ReadBefore> {
    if !conn.is_autocommit() {
        debug!("session transcripts not read: a write transaction is open");
        return Ok(ReadBefore { spans: Vec::new() });
    }
    let spans = match closing {
        Closing::Run(run_id, kinds) => {
            let kinds: Vec<&str> = kinds
                .iter()
                .copied()
                .filter(|kind| scope(kind) == Some(Scope::Run))
                .collect();
            if kinds.is_empty() {
                Vec::new()
            } else {
                let open = open_spans(conn, "o.run_id=?1", "c.run_id=?1", params![run_id])?;
                let context = SpanContext::default();
                kinds
                    .iter()
                    .flat_map(|kind| changes(kind, &Value::Null, &open, &context))
                    .filter_map(closed_span)
                    .collect()
            }
        }
        Closing::Event(run_id, kind, payload) => {
            if scope(kind) == Some(Scope::Run) {
                let open = open_spans(conn, "o.run_id=?1", "c.run_id=?1", params![run_id])?;
                // A revise's closes end when it was sent, as `write_changes`
                // ends them when that was before the event.
                let sent = crate::domain::request_sent_at_millis(payload)
                    .filter(|_| kind == EventKind::ReviseRequested.as_str());
                changes(kind, payload, &open, &SpanContext::default())
                    .into_iter()
                    .filter_map(closed_span)
                    .map(|(span, reason, _)| (span, reason, sent))
                    .collect()
            } else {
                Vec::new()
            }
        }
        Closing::Queue(kind, payload) => {
            if let Some(open) = queue_open_spans(conn, kind)? {
                let context = SpanContext {
                    at_ms: rfc3339_millis(&now(conn)?),
                    ..SpanContext::default()
                };
                changes(kind, payload, &open, &context)
                    .into_iter()
                    .filter_map(closed_span)
                    .collect()
            } else {
                Vec::new()
            }
        }
        Closing::PlanReviews(plan_review_id, inferred) => open_spans(
            conn,
            "o.run_id IS NULL AND json_extract(o.payload,'$.kind')=?1
             AND (?2 IS NULL OR json_extract(o.payload,'$.plan_review_id')=?2)",
            "c.run_id IS NULL",
            params![PLAN_REVIEW, plan_review_id],
        )?
        .into_iter()
        .map(|span| (span, Some(job_close_reason(inferred)), None))
        .collect(),
        Closing::GoalReviews(goal_review_id, inferred) => open_goal_reviews(conn, goal_review_id)?
            .into_iter()
            .map(|span| (span, Some(job_close_reason(inferred)), None))
            .collect(),
        Closing::Spans(spans) => spans
            .iter()
            .map(|(span, reason)| (span.clone(), Some(*reason), None))
            .collect(),
    };
    // Made first, so that what was read is dropped with it when a later
    // span fails.
    let mut guard = ReadBefore { spans: Vec::new() };
    let mut source = None;
    for (span, reason, sent) in spans {
        if guard.spans.contains(&span.opened_event_id) {
            continue;
        }
        guard.spans.push(span.opened_event_id);
        let transcript = read(conn, &span);
        // A run session's close with its transcript records its work
        // breakdown, whose rules depend on the repository (ADR-t614-1).
        // A Codex worker's breakdown is made from its turns' commands.
        let codex = !span.claude() && span.headless();
        if (transcript.is_ok() || codex) && RUN_SESSION.contains(&span.kind()) {
            let source = match source {
                Some(source) => source,
                None => *source.insert(judge_dagq_source(conn)?),
            };
            SOURCE_BEFORE.with_borrow_mut(|read| read.push((span.opened_event_id, source)));
        }
        let transcript = match transcript {
            Ok(transcript) => {
                let mut read = ReadTranscript::new(transcript);
                let run_id: Option<RunId> = conn.query_row(
                    "SELECT run_id FROM run_events WHERE id=?1",
                    [span.opened_event_id],
                    |r| r.get(0),
                )?;
                // The close comes after now, which is after the transcript
                // was read.
                let now = now(conn)?;
                let at = Closing::at(&now, sent, read.last_at);
                if let Some(mut want) =
                    want(conn, &span, run_id.as_ref(), reason, &at, read.last_at)?
                {
                    // Made past every record, it moves to whatever end the
                    // close has (`Analysis::to`): the clock may say the
                    // close is before a record.
                    if let Some(last) = read.last_at {
                        want.end = want.end.max(last + 1);
                        want.cut = want.cut.max(last + 1);
                    }
                    read.analysis = Some(analyse(&read.transcript.records, &read.turns, want));
                }
                Ok(read)
            }
            Err(unreadable) => Err(unreadable),
        };
        READ_BEFORE.with_borrow_mut(|read| read.push((span, transcript)));
    }
    Ok(guard)
}

impl Closing<'_> {
    /// When a close predicted at `now` ends: when its revise was `sent`,
    /// if that was before now, else now, or past the transcript's `last`
    /// record when the clock says that is later.
    fn at(now: &str, sent: Option<i64>, last: Option<i64>) -> String {
        let now_ms = rfc3339_millis(now);
        match (now_ms, sent) {
            (Some(now_ms), Some(sent)) if sent < now_ms => millis_text(sent),
            (Some(now_ms), _) => millis_text(now_ms.max(last.map_or(now_ms, |last| last + 1))),
            (None, _) => now.to_owned(),
        }
    }
}

/// The span a change closes, with why, and when it ends when that is not
/// at its event.
fn closed_span(change: SpanChange) -> Option<(OpenSpan, Option<&'static str>, Option<i64>)> {
    match change {
        SpanChange::Close { span, reason } => Some((span, Some(reason), None)),
        SpanChange::Open(_) => None,
    }
}

/// What the close of `span` of `run_id` at `now`, for `reason` (not
/// inferred when unknown), analyses of its transcript whose last record is
/// at `last`; `None` when the span's times cannot be read.
fn want(
    conn: &Connection,
    span: &OpenSpan,
    run_id: Option<&RunId>,
    reason: Option<&str>,
    now: &str,
    last: Option<i64>,
) -> Result<Option<Want>> {
    let Some((start, now_ms)) = times(conn, span, now)? else {
        return Ok(None);
    };
    if span.headless() {
        // Its tokens are its run's turns'; its models and work are to now.
        let work = match run_id {
            Some(run_id) => Some(work_inputs(conn, run_id, span)?),
            None => None,
        };
        return Ok(Some(Want {
            start,
            end: now_ms,
            cut: now_ms,
            usage: false,
            work,
        }));
    }
    // An inferred close ends at the transcript's last record, which is the
    // span's: its tokens and models are to just after it.
    let inferred = reason == Some(INFERRED);
    let end = match last.filter(|_| inferred) {
        Some(last) => last.clamp(start, now_ms),
        None => now_ms,
    };
    let work = match run_id.filter(|_| RUN_SESSION.contains(&span.kind())) {
        Some(run_id) => Some(work_inputs(conn, run_id, span)?),
        None => None,
    };
    Ok(Some(Want {
        start,
        end,
        cut: if inferred { end + 1 } else { end },
        usage: true,
        work,
    }))
}

/// The transcript of `span` for its close: the one [`read_before`] read,
/// else, outside a write transaction, read now; else unreadable.
fn transcript_for_close(conn: &Connection, span: &OpenSpan) -> Result<ReadTranscript, Unreadable> {
    let taken = READ_BEFORE.with_borrow_mut(|read| {
        read.iter()
            .position(|(read, _)| read == span)
            .map(|at| read.swap_remove(at).1)
    });
    taken.unwrap_or_else(|| read(conn, span).map(ReadTranscript::new))
}

/// Write the spans the event `event_id` (of `kind`, with `payload`, just
/// inserted on `task_id` and `run_id`) opens and closes.
pub(super) fn follow(
    conn: &Connection,
    event_id: EventId,
    task_id: Option<TaskId>,
    run_id: Option<&RunId>,
    kind: EventKind,
    payload: &Value,
) -> Result<()> {
    let kind = kind.as_str();
    let Some(scope) = scope(kind) else {
        return Ok(());
    };
    let (open, context) = match (scope, task_id, run_id) {
        (Scope::Run, Some(task_id), Some(run_id)) => (
            open_spans(
                conn,
                "o.task_id=?1 AND o.run_id=?2",
                "c.task_id=?1 AND c.run_id=?2",
                params![task_id, run_id],
            )?,
            run_context(conn, run_id)?,
        ),
        (Scope::Proposal, Some(task_id), None) => {
            let proposal = payload["proposal_id"].as_i64();
            (
                open_spans(
                    conn,
                    "o.task_id=?1 AND o.run_id IS NULL AND json_extract(o.payload,'$.proposal_id')=?2",
                    "c.task_id=?1 AND c.run_id IS NULL",
                    params![task_id, proposal],
                )?,
                SpanContext {
                    goal_ids: proposal_goals(conn, proposal)?,
                    ..SpanContext::default()
                },
            )
        }
        (Scope::Queue, None, None) => {
            let Some(open) = queue_open_spans(conn, kind)? else {
                return Ok(());
            };
            let at: String = conn.query_row(
                "SELECT created_at FROM run_events WHERE id=?1",
                [event_id],
                |r| r.get(0),
            )?;
            (
                open,
                SpanContext {
                    at_ms: rfc3339_millis(&at),
                    ..SpanContext::default()
                },
            )
        }
        (Scope::Planner, None, None) => {
            let Some(planner) = payload["planner_id"].as_i64() else {
                return Ok(());
            };
            let open = open_planner_spans(conn, planner)?;
            // On the task its span opened on, else where a new one opens:
            // the first task of the planner's proposal, as the hook's.
            let (anchor, proposal, goal_ids) = planner_anchor(conn, planner)?;
            let task = match open.first() {
                Some(span) => span_task(conn, span)?,
                None => anchor,
            };
            let context = SpanContext {
                goal_ids,
                proposal_id: proposal,
                ..SpanContext::default()
            };
            return write_changes(conn, event_id, (task, None), kind, payload, &open, &context);
        }
        _ => return Ok(()),
    };
    write_changes(
        conn,
        event_id,
        (task_id, run_id),
        kind,
        payload,
        &open,
        &context,
    )
}

/// The open spans of the headless session of planner `planner`
/// (ADR-t1394-2 decision 4), oldest first.
fn open_planner_spans(conn: &Connection, planner: i64) -> Result<Vec<OpenSpan>> {
    open_spans(
        conn,
        &format!(
            "o.run_id IS NULL AND json_extract(o.payload,'$.kind')='{RUNTIME_PLANNER}'
             AND json_extract(o.payload,'$.route')='{HEADLESS_ROUTE}'
             AND json_extract(o.payload,'$.planner_id')=?1"
        ),
        "c.run_id IS NULL",
        params![planner],
    )
}

/// Close the spans of planner `planner`'s headless session still open, as
/// `inferred`: its row closed without a `planner_closed` (its session could
/// not be opened or recorded). Returns how many it closed.
pub(super) fn close_planner_spans(conn: &Connection, planner: i64) -> Result<usize> {
    let open = open_planner_spans(conn, planner)?;
    let now = now(conn)?;
    for span in &open {
        let task = span_task(conn, span)?;
        close(conn, &now, task, None, span, INFERRED)?;
    }
    Ok(open.len())
}

/// Write the spans the event `event_id` (of `kind`, with `payload`, just
/// inserted on `goal_id`) opens and closes: a goal review's, recorded on
/// the goal's first task (where its `approve_goal` ask belongs) with the
/// goal in their payload.
pub(super) fn follow_goal(
    conn: &Connection,
    event_id: EventId,
    goal_id: GoalId,
    kind: EventKind,
    payload: &Value,
) -> Result<()> {
    let kind = kind.as_str();
    if scope(kind) != Some(Scope::Goal) {
        return Ok(());
    }
    let anchor: Option<TaskId> = conn.query_row(
        "SELECT min(id) FROM tasks WHERE goal_id=?1",
        [goal_id],
        |r| r.get(0),
    )?;
    let Some(anchor) = anchor else {
        return Ok(());
    };
    let open = open_spans(
        conn,
        "o.run_id IS NULL AND json_extract(o.payload,'$.kind')=?1
         AND json_extract(o.payload,'$.goal_id')=?2",
        "c.run_id IS NULL",
        params![GOAL_REVIEW, goal_id],
    )?;
    let context = SpanContext {
        goal_ids: vec![goal_id.as_i64()],
        ..SpanContext::default()
    };
    write_changes(
        conn,
        event_id,
        (Some(anchor), None),
        kind,
        payload,
        &open,
        &context,
    )
}

/// Write the `session_opened` / `session_closed` the event `event_id` of
/// `kind` makes of the spans `open`, on `task_id` and `run_id`, at the
/// event's time.
fn write_changes(
    conn: &Connection,
    event_id: EventId,
    (task_id, run_id): (Option<TaskId>, Option<&RunId>),
    kind: &str,
    payload: &Value,
    open: &[OpenSpan],
    context: &SpanContext,
) -> Result<()> {
    let changes = changes(kind, payload, open, context);
    if changes.is_empty() {
        return Ok(());
    }
    let at: String = conn.query_row(
        "SELECT created_at FROM run_events WHERE id=?1",
        [event_id],
        |r| r.get(0),
    )?;
    // The spans switch when the revise was sent (`sent_at`), so that its
    // turn is the revise's: a `revise_requested` written after the revise
    // was typed (as before task 241) comes later than that.
    let at = crate::domain::request_sent_at_millis(payload)
        .filter(|_| kind == EventKind::ReviseRequested)
        .map(millis_text)
        .filter(|sent| rfc3339_millis(sent) < rfc3339_millis(&at))
        .unwrap_or(at);
    let mut exited = RunSessionClosed::default();
    for change in changes {
        match change {
            SpanChange::Close { span, reason } => {
                if let Some(closed) =
                    close_with(conn, &at, task_id, run_id, &span, reason, Some(payload))?
                {
                    exited = closed;
                }
            }
            SpanChange::Open(payload) => {
                insert_at(
                    conn,
                    task_id,
                    run_id,
                    EventKind::SessionOpened,
                    &payload,
                    &at,
                )?;
            }
        }
    }
    // The session's exit carries the work (task 514) and the tokens (task
    // 199) of the span it ended.
    if kind == EventKind::SessionExited {
        for (key, value) in [("work_breakdown", exited.work), ("tokens", exited.tokens)] {
            if let Some(value) = value {
                conn.execute(
                    "UPDATE run_events SET payload=json_set(payload,?3,json(?2)) WHERE id=?1",
                    params![event_id, serde_json::to_string(&value)?, format!("$.{key}")],
                )?;
            }
        }
    }
    Ok(())
}

/// What the close of a run's own session (worker, resume, revise) hands its
/// exit: its work breakdown with its kind and attempt, and its tokens.
#[derive(Debug, Default)]
struct RunSessionClosed {
    work: Option<Value>,
    tokens: Option<Value>,
}

/// The `work` of the latest closed span of `kind` of `run` opened after
/// the event `after`, with the span's kind and attempt: what `resume_finished`
/// carries of the resumed session (task 514).
pub(super) fn closed_work(
    conn: &Connection,
    run_id: &RunId,
    kind: &str,
    after: EventId,
) -> Result<Option<Value>> {
    Ok(closed_value(conn, run_id, kind, after, "work")?
        .map(|(work, opened)| exited_work(&work, kind, &opened["attempt"])))
}

/// The `tokens` of the latest closed span of `kind` of `run` opened after
/// the event `after`: what `resume_finished` carries of the resumed session
/// (task 199).
pub(super) fn closed_tokens(
    conn: &Connection,
    run_id: &RunId,
    kind: &str,
    after: EventId,
) -> Result<Option<Value>> {
    Ok(closed_value(conn, run_id, kind, after, "tokens")?.map(|(tokens, _)| tokens))
}

/// `key` of the latest `session_closed` of `kind` of `run` that has it,
/// whose span opened after the event `after`, and the span's
/// `session_opened` payload.
fn closed_value(
    conn: &Connection,
    run_id: &RunId,
    kind: &str,
    after: EventId,
    key: &str,
) -> Result<Option<(Value, Value)>> {
    let found: Option<(Value, Value)> = conn
        .query_row(
            &closed_value_sql(),
            params![run_id, kind, after, format!("$.{key}")],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
        .map(|(closed, opened)| {
            (
                serde_json::from_str(&closed).unwrap_or_default(),
                serde_json::from_str(&opened).unwrap_or_default(),
            )
        });
    Ok(found.map(|(closed, opened)| (closed[key].clone(), opened)))
}

/// The query of [`closed_value`]: the run's `session_closed` by
/// `events_by_run`, each joined to its `session_opened` by the key.
fn closed_value_sql() -> String {
    format!(
        "SELECT c.payload, o.payload FROM run_events c
           JOIN run_events o ON o.id=json_extract(c.payload,'$.opened_event_id')
         WHERE c.run_id=?1 AND c.kind='{SESSION_CLOSED}' AND o.id>?3
           AND json_extract(c.payload,'$.kind')=?2
           AND json_extract(c.payload,?4) IS NOT NULL
         ORDER BY c.id DESC LIMIT 1"
    )
}

/// `work` with the kind and attempt of its span, as the session's exit and
/// `resume_finished` carry it.
fn exited_work(work: &Value, kind: &str, attempt: &Value) -> Value {
    let mut exited = work.clone();
    exited["kind"] = json!(kind);
    exited["attempt"] = attempt.clone();
    exited
}

/// The time now, in the form of `created_at`.
fn now(conn: &Connection) -> Result<String> {
    Ok(
        conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')", [], |r| {
            r.get(0)
        })?,
    )
}

/// Write the `session_closed` of `span` at `now`, with the turns of its
/// transcript not recorded yet and its active time. A span closed as `inferred` ends at its transcript's last
/// record when that is earlier (ADR-0048 decision 7). A transcript that
/// cannot be read leaves the active time unrecorded, and says why. A run's
/// own session also gets its work breakdown (task 514), returned with the
/// span's kind and attempt, and every span its tokens (task 199), which a
/// run's own session also returns. Every span also gets the model and
/// effort its messages were written with (task 579).
fn close(
    conn: &Connection,
    now: &str,
    task_id: Option<TaskId>,
    run_id: Option<&RunId>,
    span: &OpenSpan,
    reason: &str,
) -> Result<Option<RunSessionClosed>> {
    close_with(conn, now, task_id, run_id, span, reason, None)
}

/// [`close`], by the event whose payload is `ending` when an event closes
/// it: the span of a job whose provider names its session itself (Codex,
/// ADR-t1063-1 decision 6) takes the session id, model and why no model
/// was read that the job's end records, since it has no transcript.
fn close_with(
    conn: &Connection,
    now: &str,
    task_id: Option<TaskId>,
    run_id: Option<&RunId>,
    span: &OpenSpan,
    reason: &str,
    ending: Option<&Value>,
) -> Result<Option<RunSessionClosed>> {
    let mut payload = SpanChange::closed_payload(span, reason);
    if !span.claude()
        && !span.headless()
        && let Some(ending) = ending
    {
        // A span closed as `inferred` by another job's start (a review
        // started again on Claude after a Codex one) does not take that
        // job's session id.
        if span.session_id().is_none()
            && reason != INFERRED
            && let Some(session) = ending.get("session_id").filter(|id| id.is_string())
        {
            payload["session_id"] = session.clone();
        }
        for key in ["model", "model_unknown"] {
            if let Some(value) = ending.get(key).filter(|value| value.is_string()) {
                payload[key] = value.clone();
            }
        }
    }
    if span.headless() {
        let closed_at = now.to_owned();
        let mut closed = RunSessionClosed::default();
        let worktime = close_headless(
            conn,
            now,
            (task_id, run_id),
            span,
            reason,
            &mut payload,
            &mut closed,
        )?;
        insert_at(
            conn,
            task_id,
            run_id,
            EventKind::SessionClosed,
            &payload,
            &closed_at,
        )?;
        write_worktime(conn, worktime);
        return Ok(Some(closed).filter(|closed| closed.work.is_some() || closed.tokens.is_some()));
    }
    let (closed_at, closed, worktime) =
        fill_transcript(conn, now, (task_id, run_id), span, reason, &mut payload)?;
    insert_at(
        conn,
        task_id,
        run_id,
        EventKind::SessionClosed,
        &payload,
        &closed_at,
    )?;
    write_worktime(conn, worktime);
    Ok(run_id
        .filter(|_| RUN_SESSION.contains(&span.kind()))
        .filter(|_| closed.work.is_some() || closed.tokens.is_some())
        .map(|_| closed))
}

/// Fill a close's `payload` (or its deferred hook intake's, task 655) with
/// what the transcript of `span` measures to `now`, recording its turns
/// not recorded yet. Returns when the close ends, what a run session's
/// close carries, and its work's `worktime.jsonl` lines. Under a write
/// lock it reads nothing: it takes what [`read_before`] read.
fn fill_transcript(
    conn: &Connection,
    now: &str,
    (task_id, run_id): (Option<TaskId>, Option<&RunId>),
    span: &OpenSpan,
    reason: &str,
    payload: &mut Value,
) -> Result<(String, RunSessionClosed, Option<WorktimeLines>)> {
    let mut closed_at = now.to_owned();
    let mut closed = RunSessionClosed::default();
    let mut worktime = None;
    match (transcript_for_close(conn, span), times(conn, span, now)?) {
        (Ok(mut read), Some((start, now_ms))) => {
            let mut end = now_ms;
            if reason == INFERRED
                && let Some(last) = read.last_at
            {
                end = last.clamp(start, now_ms);
            }
            // What the queue has now: the run's directory and verification,
            // and the turns recorded so far (another process may have
            // recorded more since the transcript was read).
            let work = match run_id.filter(|_| RUN_SESSION.contains(&span.kind())) {
                Some(run_id) => Some(work_inputs(conn, run_id, span)?),
                None => None,
            };
            // An inferred close ends at the transcript's last record, which
            // is the span's.
            let tokens_end = if reason == INFERRED { end + 1 } else { end };
            let want = Want {
                start,
                end,
                cut: tokens_end,
                usage: true,
                work,
            };
            let Some(analysis) = read.analysis(conn, want) else {
                payload["active"] = json!("unavailable");
                payload["active_unavailable"] = json!(TRANSCRIPT_NOT_READ_BEFORE);
                return Ok((closed_at, closed, None));
            };
            if Some(end) != rfc3339_millis(now) {
                closed_at = millis_text(end);
            }
            let recorded = recorded_turns(conn, span.opened_event_id)?;
            let through = recorded.iter().map(|turn| turn.end).max();
            let new: Vec<Turn> = analysis
                .turns
                .iter()
                .copied()
                .filter(|turn| through.is_none_or(|through| turn.start > through))
                .collect();
            if !new.is_empty() {
                let turns_payload = turns_payload(span, &new);
                insert_at(
                    conn,
                    task_id,
                    run_id,
                    EventKind::SessionTurns,
                    &turns_payload,
                    now,
                )?;
            }
            let millis: i64 = recorded.iter().chain(&new).map(|turn| turn.millis()).sum();
            payload["active"] = json!("recorded");
            payload["active_secs"] = json!(millis / 1000);
            match analysis.usage {
                Some(Ok(usage)) => {
                    payload["tokens"] = usage.payload();
                    closed.tokens = Some(usage.payload());
                }
                Some(Err(code)) => info!(
                    code,
                    version = read.transcript.version().unwrap_or("unknown"),
                    "session span {} ({}): tokens not recorded, {code} (Claude Code {})",
                    span.opened_event_id,
                    span.kind(),
                    read.transcript.version().unwrap_or("version unknown"),
                ),
                None => {}
            }
            // The model and effort its messages used (task 579), none when
            // no message names a model.
            tokens::models_payload(&analysis.models, payload);
            if let (Some(inputs), Some(breakdown)) = (analysis.want.work, analysis.work) {
                let (work, lines) = work_breakdown(span, inputs, breakdown);
                closed.work = Some(exited_work(&work, span.kind(), &span.payload["attempt"]));
                payload["work"] = work;
                worktime = lines;
            }
        }
        (Err(unreadable), _) => {
            unavailable(span, &unreadable);
            payload["active"] = json!("unavailable");
            payload["active_unavailable"] = json!(unreadable.code);
        }
        (Ok(_), None) => {
            payload["active"] = json!("unavailable");
            payload["active_unavailable"] = json!("span_time_unparsable");
        }
    }
    Ok((closed_at, closed, worktime))
}

/// Fill the `session_closed` of a headless worker's `span` at `now`
/// (ADR-t813-2 decision 7): its active time is its run's turns, each from
/// its `turn_started` to its `turn_finished` (one still running counts to
/// the close), recorded as `session_turns` like a transcript's; its tokens
/// are the `tokens` its turns recorded from the provider's output. A
/// Claude session's transcript, when it can be read, still gives the model
/// and effort and the work breakdown. A Codex worker's session has no
/// transcript: its work breakdown is made from the commands files its
/// turns' wrapper wrote ([`codex_breakdown`], task 1354), else it is
/// `null` with why. Returns the work's `worktime.jsonl` lines.
fn close_headless(
    conn: &Connection,
    now: &str,
    (task_id, run_id): (Option<TaskId>, Option<&RunId>),
    span: &OpenSpan,
    reason: &str,
    payload: &mut Value,
    closed: &mut RunSessionClosed,
) -> Result<Option<WorktimeLines>> {
    let transcript = transcript_for_close(conn, span);
    let owner = TurnEvents::of(run_id, span);
    let (Some(owner), Some((start, now_ms))) = (owner, times(conn, span, now)?) else {
        payload["active"] = json!("unavailable");
        payload["active_unavailable"] = json!("span_time_unparsable");
        return Ok(None);
    };
    let events = owner.events(conn, span.opened_event_id)?;
    let headless = HeadlessSpan::of(&events, start);
    let recorded = recorded_turns(conn, span.opened_event_id)?;
    // A span closed as inferred (its session gone unseen) does not know
    // when a turn still running ended.
    let new = headless.new_turns(&recorded, (reason != INFERRED).then_some(now_ms));
    if !new.is_empty() {
        insert_at(
            conn,
            task_id,
            run_id,
            EventKind::SessionTurns,
            &turns_payload(span, &new),
            now,
        )?;
    }
    let millis: i64 = recorded.iter().chain(&new).map(|turn| turn.millis()).sum();
    payload["active"] = json!("recorded");
    payload["active_secs"] = json!(millis / 1000);
    if let Some(tokens) = &headless.tokens {
        payload["tokens"] = tokens.payload();
        closed.tokens = Some(tokens.payload());
    }
    // A planner's session has no worktree to find its transcript in: its
    // model is the one its turns said they ran on, its effort its opener's.
    let TurnEvents::Run(run_id) = owner else {
        if let Some(model) = crate::domain::turn::turns_model(&events) {
            payload["model"] = json!(model);
            payload["effort"] = json!(
                span.payload["launch"]["effort"]
                    .as_str()
                    .unwrap_or("unknown")
            );
        }
        return Ok(None);
    };
    // Codex has no transcript: its turns' commands are in the files its
    // wrapper wrote.
    if !span.claude() {
        if !RUN_SESSION.contains(&span.kind()) {
            return Ok(None);
        }
        let inputs = work_inputs(conn, run_id, span)?;
        return Ok(
            match codex_breakdown(span, &inputs, &events, (start, now_ms), reason == INFERRED) {
                Ok(breakdown) => {
                    let (work, lines) = work_breakdown(span, inputs, breakdown);
                    closed.work = Some(exited_work(&work, span.kind(), &span.payload["attempt"]));
                    payload["work"] = work;
                    lines
                }
                Err(code) => {
                    payload["work"] = Value::Null;
                    payload["work_unavailable"] = json!(code);
                    None
                }
            },
        );
    }
    match transcript {
        Ok(mut read) => {
            let want = Want {
                start,
                end: now_ms,
                cut: now_ms,
                usage: false,
                work: Some(work_inputs(conn, run_id, span)?),
            };
            let Some(analysis) = read.analysis(conn, want) else {
                return Ok(None);
            };
            tokens::models_payload(&analysis.models, payload);
            let (Some(inputs), Some(breakdown)) = (analysis.want.work, analysis.work) else {
                return Ok(None);
            };
            let (work, lines) = work_breakdown(span, inputs, breakdown);
            closed.work = Some(exited_work(&work, span.kind(), &span.payload["attempt"]));
            payload["work"] = work;
            Ok(lines)
        }
        Err(unreadable) => {
            debug!(
                "session span {} ({}): no model or work breakdown, {}: {}",
                span.opened_event_id,
                span.kind(),
                unreadable.code,
                unreadable.detail
            );
            Ok(None)
        }
    }
}

/// The run directory of the run is not known: no turn's commands can be
/// read.
pub const RUN_DIR_UNKNOWN: &str = "run_dir_unknown";
/// A turn's file of its commands is not what its wrapper writes (not
/// JSON lines of commands, not UTF-8, a link, too large).
pub const TURN_COMMANDS_UNPARSABLE: &str = "turn_commands_unparsable";
/// How large a turn's file of its commands is read: one line per command,
/// read under the write lock, in a directory the worker can write.
const TURN_COMMANDS_BYTES: u64 = 4 * 1024 * 1024;

/// The work breakdown of the Codex `span` closed at `end` that opened at
/// `start` (unix milliseconds), from its turns (`events`, oldest first,
/// taken as [`codex_span_turns`] decides) and the commands their wrapper
/// wrote in the run directory, as a Claude span's is from its transcript;
/// else the code of why it cannot be made. The files are read here, under
/// the write lock when there is one: they are small (one line per
/// command, read to [`TURN_COMMANDS_BYTES`]), unlike a transcript.
fn codex_breakdown(
    span: &OpenSpan,
    inputs: &WorkInputs,
    events: &[RunEvent],
    (start, end): (i64, i64),
    inferred: bool,
) -> Result<Breakdown, &'static str> {
    let run_dir = inputs.run_dir.as_deref().ok_or(RUN_DIR_UNKNOWN)?;
    let run_dir = std::path::Path::new(run_dir);
    let turns = codex_span_turns(events, start, end, inferred)?;
    let mut read = Vec::new();
    for turn in turns {
        let Some(number) = turn.commands_of else {
            read.push((turn.start, turn.end, Vec::new()));
            continue;
        };
        let path = crate::domain::turn::commands_path(run_dir, number);
        // The run directory is the worker's to write: read without
        // following a link, and bounded.
        let bytes = super::agent_dir::read_file(&path)
            .and_then(|file| {
                if file.metadata()?.len() > TURN_COMMANDS_BYTES {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "larger than a turn's commands are read",
                    ));
                }
                super::agent_dir::read_bounded(file)
            })
            .map_err(|error| {
                debug!(
                    "session span {}: {} not read: {error}",
                    span.opened_event_id,
                    path.display()
                );
                if error.kind() == std::io::ErrorKind::NotFound {
                    TURN_COMMANDS_MISSING
                } else {
                    TURN_COMMANDS_UNPARSABLE
                }
            })?;
        let text = String::from_utf8(bytes).map_err(|_| TURN_COMMANDS_UNPARSABLE)?;
        let commands = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str::<crate::domain::turn::TurnCommand>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| TURN_COMMANDS_UNPARSABLE)?;
        read.push((turn.start, turn.end, commands));
    }
    let turns: Vec<worktime::HeadlessTurn<'_>> = read
        .iter()
        .map(|(from, to, commands)| worktime::HeadlessTurn {
            start: *from,
            end: *to,
            commands,
        })
        .collect();
    Ok(worktime::headless_breakdown(
        &turns,
        start,
        end,
        &inputs.verification,
        inputs.source,
    ))
}

/// Whose turns a headless span's are: its run's (a worker's, ADR-t813-2
/// decision 7), or the queue's of its planner (ADR-t1394-2 decision 4).
#[derive(Clone, Copy)]
enum TurnEvents<'a> {
    Run(&'a RunId),
    Planner(i64),
}

impl<'a> TurnEvents<'a> {
    /// The owner of `span`'s turns, on `run_id` when it has one.
    fn of(run_id: Option<&'a RunId>, span: &OpenSpan) -> Option<Self> {
        match run_id {
            Some(run_id) => Some(Self::Run(run_id)),
            None => span.payload["planner_id"].as_i64().map(Self::Planner),
        }
    }

    /// Its `turn_started` and `turn_finished` after the event `opened`,
    /// oldest first.
    fn events(self, conn: &Connection, opened: EventId) -> Result<Vec<RunEvent>> {
        Ok(match self {
            Self::Run(run_id) => conn
                .prepare(&run_turns_sql())?
                .query_map(params![run_id, opened], event_row)?
                .collect::<rusqlite::Result<_>>()?,
            Self::Planner(planner) => conn
                .prepare(&planner_turns_sql())?
                .query_map(params![planner, opened], event_row)?
                .collect::<rusqlite::Result<_>>()?,
        })
    }
}

/// The kinds of a turn, as the condition of a query on `run_events`.
fn turn_kinds() -> String {
    format!(
        "kind IN ('{}','{}')",
        event_kind::TURN_STARTED,
        event_kind::TURN_FINISHED
    )
}

/// The turns of run `?1` after the event `?2`, by `events_by_run`
/// (goal 103).
fn run_turns_sql() -> String {
    format!(
        "SELECT * FROM run_events WHERE run_id=?1 AND id>?2 AND {} ORDER BY id",
        turn_kinds()
    )
}

/// The turns of planner `?1` after the event `?2`, by their kind: `+run_id`
/// keeps `events_by_run` from walking every event that has no run.
fn planner_turns_sql() -> String {
    format!(
        "SELECT * FROM run_events WHERE +run_id IS NULL AND id>?2 AND {}
           AND json_extract(payload,'$.planner_id')=?1 ORDER BY id",
        turn_kinds()
    )
}

/// The number of events of kind `?2` of run `?1`, by `events_by_run`.
const RUN_EVENT_COUNT_SQL: &str = "SELECT count(*) FROM run_events WHERE run_id=?1 AND kind=?2";

/// The work `breakdown` of `span`, made with `inputs`: the aggregate for
/// the events, and the lines of its commands for the run directory's
/// `worktime.jsonl`, which [`write_worktime`] writes once the close is
/// committed.
fn work_breakdown(
    span: &OpenSpan,
    inputs: WorkInputs,
    breakdown: Breakdown,
) -> (Value, Option<WorktimeLines>) {
    let payload = breakdown.payload();
    let lines = inputs.run_dir.map(|run_dir| {
        let mut span_payload = span.payload.clone();
        span_payload["opened_event_id"] = json!(span.opened_event_id);
        WorktimeLines {
            opened: span.opened_event_id,
            connection: 0,
            commit: Commit::Open,
            path: std::path::Path::new(&run_dir).join(WORKTIME_FILE),
            span: span_payload,
            commands: breakdown.commands,
        }
    });
    (payload, lines)
}

/// Write `lines` of the `session_closed` just inserted: now outside a write
/// transaction (the insert committed), else when the [`ReadBefore`] of
/// their span is dropped, if the transaction committed (task 1334).
/// Failing to write the file is only logged.
fn write_worktime(conn: &Connection, lines: Option<WorktimeLines>) {
    let Some(mut lines) = lines else {
        return;
    };
    if conn.is_autocommit() {
        lines.append();
        return;
    }
    match conn.query_row(&format!("SELECT {CONNECTION_FUNCTION}()"), [], |r| r.get(0)) {
        Ok(connection) => {
            lines.connection = connection;
            WORKTIME_AFTER.with_borrow_mut(|after| after.push(lines));
        }
        // Left out rather than perhaps written for a close rolled back.
        Err(error) => info!(
            "session span {}: {} not written, its commit is not watched: {error}",
            lines.opened,
            lines.path.display()
        ),
    }
}

/// What the work breakdown of `span` of `run_id` is made with: the run's
/// directory and its task's verification, and whether the queue's
/// repository is dagq's source.
fn work_inputs(conn: &Connection, run_id: &RunId, span: &OpenSpan) -> Result<WorkInputs> {
    let (run_dir, verification): (Option<String>, Option<String>) = conn
        .query_row(
            "SELECT r.run_dir, t.verification_commands FROM task_runs r
               JOIN tasks t ON t.id=r.task_id WHERE r.id=?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or_default();
    Ok(WorkInputs {
        run_dir,
        verification: verification
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default(),
        source: dagq_source(conn, span)?,
    })
}

/// Whether the repository the queue is bound to is dagq's source
/// (ADR-t614-1), for the close of `span`: the work breakdown's cargo-only
/// kinds and counts are kept only for it. Judged by [`read_before`] before
/// the write transaction began, else, outside one, now; inside one without
/// that judgement (never with a transcript read before), not.
fn dagq_source(conn: &Connection, span: &OpenSpan) -> Result<bool> {
    let judged = SOURCE_BEFORE.with_borrow(|read| {
        read.iter()
            .find(|(opened, _)| *opened == span.opened_event_id)
            .map(|(_, source)| *source)
    });
    match judged {
        Some(source) => Ok(source),
        None if conn.is_autocommit() => judge_dagq_source(conn),
        None => Ok(false),
    }
}

/// Whether the repository the queue is bound to is dagq's source
/// (ADR-t614-1), judged now from its main checkout's `Cargo.toml`. A queue
/// bound to no repository, or whose main checkout cannot be found, is not.
fn judge_dagq_source(conn: &Connection) -> Result<bool> {
    let bound: Option<String> = conn
        .query_row(
            "SELECT git_common_dir FROM queue_repository WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let Some(dir) = bound else {
        return Ok(false);
    };
    match super::adapters::main_checkout_of(std::path::Path::new(&dir)) {
        Ok(checkout) => Ok(super::adapters::is_dagq_source(&checkout)),
        Err(error) => {
            info!(
                "work breakdown without the cargo-only measures: no main checkout of {dir}: {error:#}"
            );
            Ok(false)
        }
    }
}

/// The transcript of `span`, never read while `conn` holds a write
/// transaction (task 543).
fn read(conn: &Connection, span: &OpenSpan) -> Result<Transcript, Unreadable> {
    if !conn.is_autocommit() {
        return Err(Unreadable {
            code: TRANSCRIPT_NOT_READ_BEFORE,
            version: None,
            detail: "the transcript was not read before the write transaction began".into(),
        });
    }
    if !span.claude() {
        return Err(Unreadable {
            code: TRANSCRIPT_NOT_CLAUDE,
            version: None,
            detail: format!(
                "the session is {}'s, which has no Claude Code transcript",
                span.payload["provider"]
                    .as_str()
                    .unwrap_or("another provider")
            ),
        });
    }
    #[cfg(test)]
    tests::READS.with(|reads| reads.set(reads.get() + 1));
    let text = |key: &str| span.payload[key].as_str().map(str::to_owned);
    ClaudeTranscripts::from_env().read(&TranscriptSource {
        session_id: text("session_id"),
        cwd: text("cwd"),
        transcript_path: text("transcript_path"),
    })
}

/// Say in the log why the active time of `span` is not recorded.
fn unavailable(span: &OpenSpan, unreadable: &Unreadable) {
    info!(
        code = unreadable.code,
        version = unreadable.version.as_deref().unwrap_or("unknown"),
        "session span {} ({}): active time and tokens not recorded, {}: {} (Claude Code {})",
        span.opened_event_id,
        span.kind(),
        unreadable.code,
        unreadable.detail,
        unreadable.version.as_deref().unwrap_or("version unknown"),
    );
}

/// Unix milliseconds of the start of `span` and of `now`.
fn times(conn: &Connection, span: &OpenSpan, now: &str) -> Result<Option<(i64, i64)>> {
    let start: String = conn.query_row(
        "SELECT created_at FROM run_events WHERE id=?1",
        [span.opened_event_id],
        |r| r.get(0),
    )?;
    Ok(rfc3339_millis(&start)
        .zip(rfc3339_millis(now))
        .map(|(start, now)| (start, now.max(start))))
}

/// The query of the `session_turns` of the span opened by `?1`, by
/// `events_by_opened_event` (task 1333).
fn recorded_turns_sql() -> String {
    format!(
        "SELECT payload FROM run_events WHERE kind='{SESSION_TURNS}'
           AND json_extract(payload,'$.opened_event_id')=?1 ORDER BY id"
    )
}

/// The query of whether the span opened by `?1` is closed, by
/// `events_by_opened_event`.
fn span_closed_sql() -> String {
    format!(
        "SELECT EXISTS (SELECT 1 FROM run_events WHERE kind='{SESSION_CLOSED}'
           AND json_extract(payload,'$.opened_event_id')=?1)"
    )
}

/// Whether the span opened by `opened` is closed.
fn span_closed(conn: &Connection, opened: EventId) -> Result<bool> {
    Ok(conn.query_row(&span_closed_sql(), [opened], |r| r.get(0))?)
}

/// The turns of the span opened by `opened` recorded so far.
fn recorded_turns(conn: &Connection, opened: EventId) -> Result<Vec<Turn>> {
    let payloads: Vec<Value> = conn
        .prepare(&recorded_turns_sql())?
        .query_map([opened], |r| json_col(r, "payload"))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(payloads
        .iter()
        .flat_map(|payload| payload["turns"].as_array().cloned().unwrap_or_default())
        .filter_map(|turn| {
            let time = |at: usize| {
                turn.get(at)
                    .and_then(Value::as_str)
                    .and_then(rfc3339_millis)
            };
            Some(Turn {
                start: time(0)?,
                end: time(1)?,
            })
        })
        .collect())
}

/// The payload of a `session_turns` of `span`: its turns as
/// `[start, end]`, and `through`, the end of the last one.
fn turns_payload(span: &OpenSpan, turns: &[Turn]) -> Value {
    json!({
        "opened_event_id": span.opened_event_id,
        "kind": span.kind(),
        "session_id": span.session_id(),
        "turns": turns
            .iter()
            .map(|turn| [millis_text(turn.start), millis_text(turn.end)])
            .collect::<Vec<_>>(),
        "through": turns.iter().map(|turn| turn.end).max().map(millis_text),
    })
}

/// Why a closed hook span's transcript is read after its close (task
/// 655): its end is the close's, never moved to the transcript's last
/// record.
const DEFERRED: &str = "deferred";

/// The `active_unavailable` of a hook's `session_closed` whose transcript
/// the supervisor has not taken in yet (task 655).
const HOOK_INTAKE_PENDING: &str = "hook_intake_pending";

/// The query of the closed hook spans whose transcript the supervisor has
/// not taken in yet, with when each closed (task 655). Its `NOT EXISTS`
/// finds their final `session_turns` by `events_by_opened_event`.
fn pending_hook_intakes_sql() -> String {
    let kinds = HOOK_KINDS.map(|kind| format!("'{kind}'")).join(",");
    format!(
        "SELECT o.id, o.payload, o.created_at, c.created_at AS closed_at
             FROM run_events c JOIN run_events o
               ON o.id=json_extract(c.payload,'$.opened_event_id')
             WHERE c.kind='{SESSION_CLOSED}' AND c.run_id IS NULL
               AND json_extract(c.payload,'$.active_unavailable')='{HOOK_INTAKE_PENDING}'
               AND o.kind='{SESSION_OPENED}'
               AND json_extract(o.payload,'$.kind') IN ({kinds})
               AND NOT EXISTS (SELECT 1 FROM run_events t WHERE t.kind='{SESSION_TURNS}'
                 AND json_extract(t.payload,'$.opened_event_id')=+o.id
                 AND json_extract(t.payload,'$.final')=1)
             ORDER BY o.id"
    )
}

/// The query of whether the closed hook span opened by `?1` has its final
/// `session_turns`, by `events_by_opened_event`.
fn hook_intake_finished_sql() -> String {
    format!(
        "SELECT EXISTS (SELECT 1 FROM run_events WHERE kind='{SESSION_TURNS}'
           AND json_extract(payload,'$.opened_event_id')=?1
           AND json_extract(payload,'$.final')=1)"
    )
}

/// Finish closed hook spans once, including their last (not necessarily
/// followed by another input) turn. An append-only final session_turns event
/// carries the measurements; even an unreadable transcript gets a final marker.
fn record_closed_hook_turns(conn: &Connection) -> Result<usize> {
    let pending = |conn: &Connection| -> Result<Vec<(OpenSpan, String)>> {
        Ok(conn
            .prepare(&pending_hook_intakes_sql())?
            .query_map([], |r| {
                Ok((
                    OpenSpan {
                        opened_event_id: r.get("id")?,
                        payload: json_col(r, "payload")?,
                        opened_ms: rfc3339_millis(&r.get::<_, String>("created_at")?),
                    },
                    r.get("closed_at")?,
                ))
            })?
            .collect::<rusqlite::Result<_>>()?)
    };
    let mut recorded = 0;
    for (span, end) in pending(conn)? {
        // One span that cannot be finalized does not hold back the others
        // nor the open spans' turns: it is tried again next time.
        match finalize_hook_intake(conn, &span, &end) {
            Ok(true) => recorded += 1,
            Ok(false) => {}
            Err(error) => info!(
                "session span {} ({}): hook intake not recorded now: {error:#}",
                span.opened_event_id,
                span.kind(),
            ),
        }
    }
    Ok(recorded)
}

/// Take in the transcript of the closed hook `span` that ended at `end`:
/// its remaining turns and its final `session_turns` with its
/// measurements. `false` when another intake finished it first.
fn finalize_hook_intake(conn: &Connection, span: &OpenSpan, end: &str) -> Result<bool> {
    let _read = read_before(conn, Closing::Spans(&[(span.clone(), DEFERRED)]))?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    // Another intake may have finished while this one read the transcript.
    let finished: bool =
        tx.query_row(&hook_intake_finished_sql(), [span.opened_event_id], |r| {
            r.get(0)
        })?;
    if finished {
        return Ok(false);
    }
    let task = span_task(&tx, span)?;
    let mut payload = turns_payload(span, &[]);
    // The hook fixed the end already, including an inferred next-start
    // fallback. Do not move that boundary while enriching measurements.
    let (_, _, worktime) = fill_transcript(&tx, end, (task, None), span, DEFERRED, &mut payload)?;
    payload["final"] = json!(true);
    insert_at(
        &tx,
        task,
        None,
        EventKind::SessionTurns,
        &payload,
        &now(&tx)?,
    )?;
    write_worktime(&tx, worktime);
    tx.commit()?;
    Ok(true)
}

/// Record the finished turns (those the next input followed) of every span
/// still open that are not recorded yet, one `session_turns` per span, on
/// the task and run of its `session_opened` (ADR-0048 decision 8). A
/// transcript that cannot be read now is left for the next time. Returns
/// how many spans got turns or a final hook intake. Closed hook spans are
/// finalized too, once. The transcript is read outside the write lock;
/// the span is checked again under it, since the process that closes it
/// (a session's wrapper) records its remaining turns itself.
pub(super) fn record_open_turns(conn: &Connection) -> Result<usize> {
    let open = open_spans(conn, "1=1", "1=1", params![])?;
    // The closed hook spans' intake failing does not hold back the open
    // spans' turns.
    let mut recorded = record_closed_hook_turns(conn).unwrap_or_else(|error| {
        info!("closed hook spans not taken in now: {error:#}");
        0
    });
    for span in open {
        // A headless span's turns are its run's (ADR-t813-2 decision 7).
        let read = (!span.headless()).then(|| read(conn, &span));
        let transcript = match read {
            None => None,
            Some(Ok(transcript)) => Some(transcript),
            Some(Err(unreadable)) => {
                debug!(
                    "session span {} ({}): turns not read now, {}: {}",
                    span.opened_event_id,
                    span.kind(),
                    unreadable.code,
                    unreadable.detail
                );
                continue;
            }
        };
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        if span_closed(&tx, span.opened_event_id)? {
            continue;
        }
        let now = now(&tx)?;
        let Some((start, _)) = times(&tx, &span, &now)? else {
            continue;
        };
        let earlier = recorded_turns(&tx, span.opened_event_id)?;
        let through = earlier.iter().map(|turn| turn.end).max();
        let (task_id, run_id): (Option<TaskId>, Option<RunId>) = tx.query_row(
            "SELECT task_id, run_id FROM run_events WHERE id=?1",
            [span.opened_event_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let new = match (&transcript, &run_id) {
            (Some(transcript), _) => {
                span_turns(turns(&transcript.records).complete, start, None, through)
            }
            (None, run_id) => {
                let Some(owner) = TurnEvents::of(run_id.as_ref(), &span) else {
                    continue;
                };
                HeadlessSpan::of(&owner.events(&tx, span.opened_event_id)?, start)
                    .new_turns(&earlier, None)
            }
        };
        if new.is_empty() {
            continue;
        }
        insert_at(
            &tx,
            task_id,
            run_id.as_ref(),
            EventKind::SessionTurns,
            &turns_payload(&span, &new),
            &now,
        )?;
        tx.commit()?;
        recorded += 1;
    }
    Ok(recorded)
}

/// Close the span of the plan review `plan_review_id` when it is still open:
/// its row was finished as `interrupted` without a `plan_review_finished` /
/// `plan_review_failed` (ADR-0048 decision 7). `inferred` says its
/// supervisor was gone; otherwise the job ended when the proposal moved on.
pub(super) fn close_plan_review(
    conn: &Connection,
    plan_review_id: i64,
    inferred: bool,
) -> Result<()> {
    let open = open_spans(
        conn,
        "o.run_id IS NULL AND json_extract(o.payload,'$.kind')=?1 AND json_extract(o.payload,'$.plan_review_id')=?2",
        "c.run_id IS NULL",
        params![PLAN_REVIEW, plan_review_id],
    )?;
    let reason = job_close_reason(inferred);
    for span in open {
        let task_id: Option<TaskId> = conn.query_row(
            "SELECT task_id FROM run_events WHERE id=?1",
            [span.opened_event_id],
            |r| r.get(0),
        )?;
        close(conn, &now(conn)?, task_id, None, &span, reason)?;
    }
    Ok(())
}

/// Why a plan or goal review's span closes: `inferred` when its job's
/// supervisor went away, else its job finished.
fn job_close_reason(inferred: bool) -> &'static str {
    if inferred { INFERRED } else { JOB_FINISHED }
}

/// The open spans of the goal review `goal_review_id`, or of every goal
/// review when `None`.
fn open_goal_reviews(conn: &Connection, goal_review_id: Option<i64>) -> Result<Vec<OpenSpan>> {
    open_spans(
        conn,
        "o.run_id IS NULL AND json_extract(o.payload,'$.kind')=?1
         AND (?2 IS NULL OR json_extract(o.payload,'$.goal_review_id')=?2)",
        "c.run_id IS NULL",
        params![GOAL_REVIEW, goal_review_id],
    )
}

/// Close the span of the goal review `goal_review_id` when it is still
/// open: its row was finished as `interrupted` without a
/// `goal_review_finished` / `goal_review_failed` (ADR-0048 decision 7).
/// `inferred` says its supervisor was gone; otherwise the job ended when
/// the goal or its tasks changed.
pub(super) fn close_goal_review(
    conn: &Connection,
    goal_review_id: i64,
    inferred: bool,
) -> Result<()> {
    let reason = job_close_reason(inferred);
    for span in open_goal_reviews(conn, Some(goal_review_id))? {
        let task_id = span_task(conn, &span)?;
        close(conn, &now(conn)?, task_id, None, &span, reason)?;
    }
    Ok(())
}

/// Close the review span of `run_id` still open, as `job_finished` now:
/// its headless job ended without a verdict, or could not start, and the
/// `review_failed` that would close it is recorded only after the worker's
/// session exits (task 541). `ending` is what that `review_failed` will
/// record of a Codex job's session (its thread and model), which the span
/// takes as [`close_with`] does. Returns how many spans it closed.
pub(super) fn close_review(
    conn: &Connection,
    run_id: &RunId,
    ending: Option<&Value>,
) -> Result<usize> {
    let open = open_spans(
        conn,
        "o.run_id=?1 AND json_extract(o.payload,'$.kind')=?2",
        "c.run_id=?1",
        params![run_id, REVIEW],
    )?;
    if open.is_empty() {
        return Ok(0);
    }
    let task_id: Option<TaskId> = conn
        .query_row("SELECT task_id FROM task_runs WHERE id=?1", [run_id], |r| {
            r.get(0)
        })
        .optional()?;
    let now = now(conn)?;
    for span in &open {
        close_with(
            conn,
            &now,
            task_id,
            Some(run_id),
            span,
            JOB_FINISHED,
            ending,
        )?;
    }
    Ok(open.len())
}

/// The open spans of [`HOOK_KINDS`], which the plugin's hook records on no
/// run (ADR-0048 decision 6), oldest first.
fn open_hook_spans(conn: &Connection) -> Result<Vec<OpenSpan>> {
    let kinds = HOOK_KINDS.map(|kind| format!("'{kind}'")).join(",");
    open_spans(
        conn,
        &format!(
            "o.run_id IS NULL AND json_extract(o.payload,'$.kind') IN ({kinds})
             AND json_extract(o.payload,'$.route') IS NOT '{HEADLESS_ROUTE}'"
        ),
        "c.run_id IS NULL",
        params![],
    )
}

/// Whether `hook` is of a headless planner's session, whose span its turns
/// record (ADR-t1394-2 decision 4): the hook its `claude -p` turns may
/// still run is left out, so that it neither opens a second span nor
/// closes the planner's at each turn.
fn headless_planner_hook(conn: &Connection, hook: &SessionHook) -> Result<bool> {
    let Some(planner) = hook.planner_id.filter(|_| hook.kind == RUNTIME_PLANNER) else {
        return Ok(false);
    };
    Ok(conn
        .query_row("SELECT route FROM planners WHERE id=?1", [planner], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten()
        .is_some_and(|route| route == HEADLESS_ROUTE))
}

/// The task the `session_opened` of `span` is on.
fn span_task(conn: &Connection, span: &OpenSpan) -> Result<Option<TaskId>> {
    Ok(conn.query_row(
        "SELECT task_id FROM run_events WHERE id=?1",
        [span.opened_event_id],
        |r| r.get(0),
    )?)
}

/// Record what the plugin's hook reported of an inbox or planner session
/// (ADR-0048 decision 6): open its span, go on with the one open for its
/// session id, close the one of the session a `/clear` replaced in its
/// workspace (`next_span`) or, without a workspace, the ones of its kind
/// open without one (`inferred`, ADR-t655-1 decision 3), or close it at
/// its end. A span closed already is not closed again. No transcript is
/// read: a close is marked [`HOOK_INTAKE_PENDING`], and the supervisor's
/// intake records its turns and measurements later
/// ([`record_closed_hook_turns`], ADR-t655-1). A runtime planner's span is on the first task of
/// its proposal, with the proposal and its goals; the others are on no
/// task. Only the spans are written: no run, proposal or planner changes.
pub(super) fn record_hook(conn: &Connection, hook: &SessionHook) -> Result<Value> {
    if headless_planner_hook(conn, hook)? {
        return Ok(json!({
            "kind": hook.kind,
            "session_id": hook.session_id,
            "opened": null,
            "closed": [],
            "skipped": "headless_planner",
        }));
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let (task_id, context) = planner_context(&tx, hook)?;
    let changes = hook_changes(hook, &open_hook_spans(&tx)?, &context);
    let now = now(&tx)?;
    let mut opened = None;
    let mut closed = Vec::new();
    for change in changes {
        match change {
            SpanChange::Close { span, reason } => {
                let task = span_task(&tx, &span)?;
                let mut payload = SpanChange::closed_payload(&span, reason);
                payload["active"] = json!("unavailable");
                payload["active_unavailable"] = json!(HOOK_INTAKE_PENDING);
                insert_at(&tx, task, None, EventKind::SessionClosed, &payload, &now)?;
                closed.push(span.opened_event_id);
            }
            SpanChange::Open(payload) => {
                insert_at(&tx, task_id, None, EventKind::SessionOpened, &payload, &now)?;
                opened = Some(EventId::new(tx.last_insert_rowid()));
            }
        }
    }
    tx.commit()?;
    Ok(json!({
        "kind": hook.kind,
        "session_id": hook.session_id,
        "opened": opened,
        "closed": closed,
    }))
}

/// Where a span of `hook` is recorded, and what its payload adds: a
/// runtime planner's goes on the first task of its planner's proposal,
/// naming the proposal and its goals (as a plan review's does).
fn planner_context(conn: &Connection, hook: &SessionHook) -> Result<(Option<TaskId>, Value)> {
    let Some(planner) = hook.planner_id.filter(|_| hook.kind == RUNTIME_PLANNER) else {
        return Ok((None, Value::Null));
    };
    let (task, proposal, goal_ids) = planner_anchor(conn, planner)?;
    let Some(proposal) = proposal else {
        return Ok((None, Value::Null));
    };
    Ok((task, json!({"proposal_id": proposal, "goal_ids": goal_ids})))
}

/// Where a span of planner `planner` is recorded: the first task of the
/// proposal it was opened for, with the proposal and its goals; nothing
/// without a proposal.
fn planner_anchor(
    conn: &Connection,
    planner: i64,
) -> Result<(Option<TaskId>, Option<i64>, Vec<i64>)> {
    let proposal: Option<i64> = conn
        .query_row(
            "SELECT proposal_id FROM planners WHERE id=?1",
            [planner],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let Some(proposal) = proposal else {
        return Ok((None, None, Vec::new()));
    };
    let task: Option<TaskId> = conn.query_row(
        "SELECT min(id) FROM tasks WHERE proposal_id=?1",
        [proposal],
        |r| r.get(0),
    )?;
    Ok((task, Some(proposal), proposal_goals(conn, Some(proposal))?))
}

/// The open spans the hook recorded that name their workspace, with it.
pub(super) fn hook_workspaces(conn: &Connection) -> Result<Vec<(EventId, String)>> {
    Ok(open_hook_spans(conn)?
        .into_iter()
        .filter_map(|span| {
            let workspace = span.payload["workspace_id"].as_str()?.to_owned();
            Some((span.opened_event_id, workspace))
        })
        .collect())
}

/// Close the spans the hook recorded that are among `gone` and still open,
/// as `inferred`: their workspace is gone, and their `SessionEnd` never
/// came (ADR-0048 decision 7). Each ends at its transcript's last record
/// (now when it cannot be read). Returns how many it closed.
pub(super) fn close_gone_hook_spans(conn: &Connection, gone: &[EventId]) -> Result<usize> {
    let spans: Vec<OpenSpan> = open_hook_spans(conn)?
        .into_iter()
        .filter(|span| gone.contains(&span.opened_event_id))
        .collect();
    if spans.is_empty() {
        return Ok(0);
    }
    let closing: Vec<(OpenSpan, &'static str)> =
        spans.iter().map(|span| (span.clone(), INFERRED)).collect();
    let _read = read_before(conn, Closing::Spans(&closing))?;
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let now = now(&tx)?;
    let mut closed = 0;
    // Found outside the write lock; checked again under it one by one, so a
    // span another process closed meanwhile is not closed twice.
    for span in &spans {
        if span_closed(&tx, span.opened_event_id)? {
            continue;
        }
        let task = span_task(&tx, span)?;
        close(&tx, &now, task, None, span, INFERRED)?;
        closed += 1;
    }
    tx.commit()?;
    Ok(closed)
}

/// The open spans of the job of the queue an event of `kind` is about
/// ([`queue_span_kind`]: the observer's or the throughput review's), or
/// `None` when it is not an event of the queue's spans.
fn queue_open_spans(conn: &Connection, kind: &str) -> Result<Option<Vec<OpenSpan>>> {
    let Some(span_kind) = queue_span_kind(kind).filter(|_| scope(kind) == Some(Scope::Queue))
    else {
        return Ok(None);
    };
    open_spans(
        conn,
        "o.task_id IS NULL AND o.goal_id IS NULL AND json_extract(o.payload,'$.kind')=?1",
        "c.task_id IS NULL AND c.goal_id IS NULL",
        params![span_kind],
    )
    .map(Some)
}

/// The `session_opened` events matching `opened` that no `session_closed`
/// matching `closed` names, oldest first. Both conditions share `params`.
fn open_spans(
    conn: &Connection,
    opened: &str,
    closed: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<OpenSpan>> {
    Ok(conn
        .prepare(&open_spans_sql(opened, closed))?
        .query_map(params, |r| {
            Ok(OpenSpan {
                opened_event_id: r.get("id")?,
                payload: json_col(r, "payload")?,
                opened_ms: rfc3339_millis(&r.get::<_, String>("created_at")?),
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}

/// The query of [`open_spans`]. Its `NOT EXISTS` finds the `session_closed`
/// by `events_by_opened_event` (task 1333): `+o.id` drops the column's
/// INTEGER affinity, which, applied to the expression, kept the index from
/// being used and walked every `session_closed` once per span.
fn open_spans_sql(opened: &str, closed: &str) -> String {
    format!(
        "SELECT o.id, o.payload, o.created_at FROM run_events o
         WHERE o.kind='{SESSION_OPENED}' AND {opened}
           AND NOT EXISTS (SELECT 1 FROM run_events c
                           WHERE c.kind='{SESSION_CLOSED}' AND {closed}
                             AND json_extract(c.payload,'$.opened_event_id')=+o.id)
         ORDER BY o.id"
    )
}

fn run_context(conn: &Connection, run_id: &RunId) -> Result<SpanContext> {
    let run = conn
        .query_row(
            "SELECT worktree_path, run_dir, workspace_id, worker_mode, actual_provider
               FROM task_runs WHERE id=?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let (worktree, run_dir, workspace_id, route, provider) =
        run.unwrap_or((None, None, None, None, None));
    let count = |kind: &str| -> Result<i64> {
        Ok(conn.query_row(RUN_EVENT_COUNT_SQL, params![run_id, kind], |r| r.get(0))?)
    };
    Ok(SpanContext {
        worktree,
        run_dir,
        workspace_id,
        resumes: count(event_kind::RESUME_STARTED)?,
        revises: count(event_kind::REVISE_REQUESTED)? - count(event_kind::REVISE_UNSENT)?,
        goal_ids: Vec::new(),
        route,
        provider,
        at_ms: None,
        proposal_id: None,
    })
}

fn proposal_goals(conn: &Connection, proposal: Option<i64>) -> Result<Vec<i64>> {
    Ok(conn
        .prepare(
            "SELECT DISTINCT goal_id FROM tasks
             WHERE proposal_id=?1 AND goal_id IS NOT NULL ORDER BY goal_id",
        )?
        .query_map([proposal], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Insert a span event at `created_at`.
fn insert_at(
    conn: &Connection,
    task_id: Option<TaskId>,
    run_id: Option<&RunId>,
    kind: EventKind,
    payload: &Value,
    created_at: &str,
) -> Result<()> {
    crate::domain::check_event_target(kind, task_id, None)?;
    crate::domain::write_rules::check_run_has_task(task_id, run_id)?;
    conn.execute(
        "INSERT INTO run_events(task_id,run_id,kind,payload,created_at,actor_role,actor_id,requested_by)
         VALUES (?1,?2,?3,?4,?5,dagq_actor_role(),dagq_actor_id(),dagq_requested_by())",
        params![
            task_id,
            run_id,
            kind.as_str(),
            serde_json::to_string(payload)?,
            created_at
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::LeaseToken;
    use crate::infrastructure::git_binary::git_executable;

    thread_local! {
        /// The transcripts read on this thread.
        pub(super) static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// The transcripts analysed for a close on this thread.
        pub(super) static ANALYSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// The analyses of transcripts on this thread: those made for a close,
    /// and every reading of a transcript's records to analyse them (its
    /// turns, tokens, models or work), wherever it is made.
    fn analysed() -> usize {
        ANALYSES.get() + crate::domain::transcript::ANALYSED.with(std::cell::Cell::get)
    }
    use crate::{
        application::TaskStore,
        domain::{NewTask, RunEvent},
        infrastructure::sqlite::{SqliteQueue, event, event_row},
    };
    use serde_json::json;

    fn task(queue: &mut SqliteQueue) -> TaskId {
        queue
            .add(NewTask {
                title: "t".into(),
                description: String::new(),
                acceptance: String::new(),
                verification_commands: Vec::new(),
                required_evidence: Vec::new(),
                paths: Vec::new(),
                dependencies: Vec::new(),
                goal_dependencies: Vec::new(),
                priority: Default::default(),
                goal_id: None,
                context: String::new(),
                change: None,
                provider: None,
                worker_mode: None,
            })
            .unwrap()
            .id()
    }

    fn spans(queue: &SqliteQueue) -> Vec<RunEvent> {
        queue
            .conn
            .prepare("SELECT * FROM run_events WHERE kind IN ('session_opened','session_closed') ORDER BY id")
            .unwrap()
            .query_map([], event_row)
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// The plan of `sql`, one line per step, its parameters NULL.
    fn plan(conn: &Connection, sql: &str) -> Vec<String> {
        let mut statement = conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let nulls = vec![rusqlite::types::Null; statement.parameter_count()];
        statement
            .query_map(rusqlite::params_from_iter(nulls), |r| {
                r.get::<_, String>("detail")
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// The searches of a span's `session_closed` and `session_turns` by
    /// the `session_opened` they name use `events_by_opened_event`, not a
    /// walk of every row of their kind (task 1333).
    #[test]
    fn the_searches_by_the_opened_event_use_its_index() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let conn = &queue.conn;
        let by_index = "INDEX events_by_opened_event (kind=? AND <expr>=?)";
        let uses = |sql: &str| {
            let plan = plan(conn, sql);
            assert!(
                plan.iter().any(|step| step.contains(by_index)),
                "{sql}\n{plan:#?}"
            );
            plan
        };
        uses(&recorded_turns_sql());
        uses(&span_closed_sql());
        uses(&hook_intake_finished_sql());
        uses(&pending_hook_intakes_sql());
        // Every `closed` condition its callers give open_spans.
        for (opened, closed) in [
            ("1=1", "1=1"),
            ("o.run_id=?1", "c.run_id=?1"),
            (
                "o.task_id=?1 AND o.run_id=?2",
                "c.task_id=?1 AND c.run_id=?2",
            ),
            (
                "o.task_id=?1 AND o.run_id IS NULL",
                "c.task_id=?1 AND c.run_id IS NULL",
            ),
            ("o.run_id IS NULL", "c.run_id IS NULL"),
            (
                "o.task_id IS NULL AND o.goal_id IS NULL",
                "c.task_id IS NULL AND c.goal_id IS NULL",
            ),
        ] {
            let plan = uses(&open_spans_sql(opened, closed));
            let subquery = plan
                .iter()
                .skip_while(|step| !step.contains("CORRELATED"))
                .collect::<Vec<_>>();
            assert!(
                subquery.iter().any(|step| step.contains(by_index))
                    && !subquery.iter().any(|step| step.contains("events_by_kind")),
                "{plan:#?}"
            );
        }
        // closed_value joins the `session_opened` by its key and walks the
        // run's `session_closed` by the kind, never the whole table.
        let plan = plan(conn, &closed_value_sql());
        assert!(
            !plan.iter().any(|step| step.starts_with("SCAN")),
            "{plan:#?}"
        );
    }

    /// Every search of `run_events` by its `run_id` (the SQL in `src` that
    /// says `FROM run_events WHERE run_id`) is a search of an index, never a
    /// walk of the whole table; those by a run use `events_by_run` (goal 103).
    #[test]
    fn the_searches_by_the_run_use_an_index() {
        use crate::infrastructure::{planners, runtime_store};
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let conn = &queue.conn;
        let searches = |sql: &str| {
            let plan = plan(conn, sql);
            assert!(
                plan.iter()
                    .any(|step| step.starts_with("SEARCH run_events"))
                    && !plan.iter().any(|step| step.starts_with("SCAN run_events")),
                "{sql}\n{plan:#?}"
            );
            plan
        };
        for sql in [
            runtime_store::RUN_EVENTS_SQL.to_owned(),
            runtime_store::HAS_RUN_EVENT_SQL.to_owned(),
            runtime_store::last_resume_started_sql(),
            run_turns_sql(),
            RUN_EVENT_COUNT_SQL.to_owned(),
        ] {
            let plan = searches(&sql);
            assert!(
                plan.iter()
                    .any(|step| step.contains("INDEX events_by_run (run_id=?")),
                "{sql}\n{plan:#?}"
            );
        }
        // The planners' events have no run: they are found by their kind,
        // not by `events_by_run` among every event that has no run.
        for sql in [planner_turns_sql(), planners::planner_turn_events_sql()] {
            let plan = searches(&sql);
            assert!(
                !plan.iter().any(|step| step.contains("events_by_run")),
                "{sql}\n{plan:#?}"
            );
        }
    }

    /// The events of a run, a plan review and the observer write their
    /// spans next to them, at their time; every span opened is closed once.
    #[test]
    fn events_open_and_close_their_spans_at_their_time() {
        let dir = tempfile::tempdir().unwrap();
        // No transcripts: every span closes without its active time.
        ClaudeTranscripts::use_config_dir_in_test(dir.path());
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let task_id = task(&mut queue);
        let run = RunId::new("11111111-1111-4111-8111-111111111111").unwrap();
        queue
            .conn
            .execute(
                "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,worktree_path,workspace_id,run_dir)
                 VALUES (?1,?2,'running','claude','claude','b','/wt','W','/run')",
                params![run, task_id],
            )
            .unwrap();
        let conn = &queue.conn;
        let record = |kind: EventKind, payload: Value| {
            event(conn, task_id, Some(&run), kind, payload).unwrap();
        };
        record(EventKind::RunClaimed, json!({}));
        record(EventKind::AgentStarted, json!({"session_id": run}));
        record(EventKind::ReviseRequested, json!({"workspace_id": "W"}));
        record(
            EventKind::ReviewStarted,
            json!({"attempt": 1, "session_id": "s-review"}),
        );
        record(EventKind::ReviewFinished, json!({"verdict": "pass"}));
        record(EventKind::SessionExited, json!({"exit_code": 0}));
        record(EventKind::ResumeStarted, json!({}));
        record(EventKind::AgentStarted, json!({"session_id": run}));
        // The resume's session was lost: the triage closes it as inferred.
        record(
            EventKind::TriageStarted,
            json!({"attempt": 1, "session_id": "s-triage"}),
        );
        record(EventKind::TriageFailed, json!({}));
        event(
            conn,
            task_id,
            None,
            EventKind::PlanReviewStarted,
            json!({"proposal_id": 1, "plan_review_id": 7, "attempt": 1, "session_id": "s-plan"}),
        )
        .unwrap();
        close_plan_review(conn, 7, true).unwrap();
        // Closed once: its finish finds nothing open.
        event(
            conn,
            task_id,
            None,
            EventKind::PlanReviewFailed,
            json!({"proposal_id": 1, "plan_review_id": 7}),
        )
        .unwrap();
        let observed = queue
            .record_queue_event(
                EventKind::ObserveStarted,
                json!({"mode": "hourly", "dir": "/obs", "session_id": "s-obs"}),
            )
            .unwrap();
        queue
            .record_queue_event(EventKind::ObserveFinished, json!({"dir": "/obs"}))
            .unwrap();

        let events = spans(&queue);
        let described: Vec<(String, String, Value)> = events
            .iter()
            .map(|e| {
                (
                    e.kind.clone(),
                    e.payload["kind"].as_str().unwrap().to_owned(),
                    e.payload
                        .get("reason")
                        .cloned()
                        .unwrap_or_else(|| e.payload["session_id"].clone()),
                )
            })
            .collect();
        let row =
            |kind: &str, span: &str, detail: Value| (kind.to_owned(), span.to_owned(), detail);
        assert_eq!(
            described,
            vec![
                row(SESSION_OPENED, "worker", json!(run)),
                row(SESSION_CLOSED, "worker", json!("next_span")),
                row(SESSION_OPENED, "revise", json!(run)),
                row(SESSION_OPENED, "review", json!("s-review")),
                row(SESSION_CLOSED, "review", json!("job_finished")),
                row(SESSION_CLOSED, "revise", json!("exited")),
                row(SESSION_OPENED, "resume", json!(run)),
                row(SESSION_CLOSED, "resume", json!("inferred")),
                row(SESSION_OPENED, "triage", json!("s-triage")),
                row(SESSION_CLOSED, "triage", json!("job_finished")),
                row(SESSION_OPENED, "plan_review", json!("s-plan")),
                row(SESSION_CLOSED, "plan_review", json!("inferred")),
                row(SESSION_OPENED, "observer", json!("s-obs")),
                row(SESSION_CLOSED, "observer", json!("job_finished")),
            ]
        );
        assert_eq!(events[0].payload["cwd"], "/wt");
        assert_eq!(events[0].payload["workspace_id"], "W");
        assert_eq!(events[6].payload["attempt"], 1);
        assert_eq!(events[8].payload["cwd"], "/run");
        // A plan review started without a cwd has a span without one.
        assert_eq!(events[10].payload["cwd"], Value::Null);
        assert_eq!(events[1].payload["opened_event_id"], json!(events[0].id));
        assert!(events[12].id > observed);
        // Each span event has the time of the event that wrote it.
        let time = |id: EventId| -> String {
            queue
                .conn
                .query_row("SELECT created_at FROM run_events WHERE id=?1", [id], |r| {
                    r.get(0)
                })
                .unwrap()
        };
        let started: EventId = queue
            .conn
            .query_row(
                "SELECT min(id) FROM run_events WHERE kind='agent_started'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(time(events[0].id), time(started));
        assert_eq!(time(events[12].id), time(observed));
        for closed in events.iter().filter(|e| e.kind == SESSION_CLOSED) {
            assert_eq!(closed.payload["active"], "unavailable");
        }
        assert_eq!(
            events[1].payload["active_unavailable"],
            "transcript_missing"
        );
    }

    const RUN: &str = "22222222-2222-4222-8222-222222222222";

    /// A queue with one running run in `/wt`, its transcripts under
    /// `config`.
    fn run_queue(dir: &std::path::Path) -> (SqliteQueue, TaskId, RunId) {
        ClaudeTranscripts::use_config_dir_in_test(&dir.join("config"));
        let mut queue = SqliteQueue::init(dir.join("q.db")).unwrap();
        let task_id = task(&mut queue);
        let run = RunId::new(RUN).unwrap();
        queue
            .conn
            .execute(
                "INSERT INTO task_runs(id,task_id,status,requested_provider,actual_provider,base_commit,worktree_path,workspace_id,run_dir)
                 VALUES (?1,?2,'running','claude','claude','b','/wt','W','/run')",
                params![run, task_id],
            )
            .unwrap();
        (queue, task_id, run)
    }

    /// Move every event after `after` to `secs` seconds ago; returns that
    /// time in unix milliseconds.
    fn retime(conn: &Connection, after: i64, secs: i64) -> i64 {
        conn.execute(
            "UPDATE run_events SET created_at=strftime('%Y-%m-%dT%H:%M:%fZ','now',?1) WHERE id>?2",
            params![format!("-{secs} seconds"), after],
        )
        .unwrap();
        let at: String = conn
            .query_row("SELECT max(created_at) FROM run_events", [], |r| r.get(0))
            .unwrap();
        rfc3339_millis(&at).unwrap()
    }

    fn latest(conn: &Connection) -> i64 {
        conn.query_row("SELECT max(id) FROM run_events", [], |r| r.get(0))
            .unwrap()
    }

    /// Write the transcript of the run's session: `turns` as (input, last
    /// output) offsets in seconds from `base`, and an unanswered input at
    /// `pending` when given.
    fn transcript(dir: &std::path::Path, base: i64, turns: &[(i64, i64)], pending: Option<i64>) {
        let project = dir.join("config/projects/-wt");
        std::fs::create_dir_all(&project).unwrap();
        let line = |kind: &str, secs: i64, content: Value| {
            json!({"type": kind, "timestamp": millis_text(base + secs * 1000),
                   "sessionId": RUN, "version": "2.1.283", "message": {"content": content}})
            .to_string()
        };
        let mut lines = Vec::new();
        for &(input, output) in turns {
            lines.push(line("user", input, json!("go")));
            lines.push(line("assistant", output, json!([{"type": "text"}])));
        }
        if let Some(pending) = pending {
            lines.push(line("user", pending, json!("more")));
        }
        std::fs::write(project.join(format!("{RUN}.jsonl")), lines.join("\n")).unwrap();
    }

    fn of_kind(queue: &SqliteQueue, kind: &str) -> Vec<RunEvent> {
        queue
            .conn
            .prepare("SELECT * FROM run_events WHERE kind=?1 ORDER BY id")
            .unwrap()
            .query_map([kind], event_row)
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    /// A Codex review's span names no session at its start (Codex names
    /// its thread, task 1339): its end gives it the thread and the model,
    /// and a later review's start that closes it as `inferred` gives it
    /// none of its own session.
    #[test]
    fn a_codex_review_span_takes_its_thread_from_its_end_only() {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let record = |kind: EventKind, payload: Value| {
            event(&queue.conn, task_id, Some(&run), kind, payload).unwrap();
        };
        let codex = json!({"role": "review", "provider": "codex"});
        record(
            EventKind::ReviewStarted,
            json!({"attempt": 1, "session_id": null, "launch": codex}),
        );
        record(
            EventKind::ReviewFinished,
            json!({"verdict": "revise", "session_id": "t-1", "model": "gpt-6-astra"}),
        );
        record(
            EventKind::ReviewStarted,
            json!({"attempt": 2, "session_id": null, "launch": codex}),
        );
        record(
            EventKind::ReviewStarted,
            json!({"attempt": 3, "session_id": "s-claude",
                   "launch": {"role": "review", "provider": "claude"}}),
        );
        let closed = of_kind(&queue, SESSION_CLOSED);
        assert_eq!(closed.len(), 2, "{closed:?}");
        assert_eq!(closed[0].payload["session_id"], "t-1");
        assert_eq!(closed[0].payload["model"], "gpt-6-astra");
        assert_eq!(closed[1].payload["reason"], INFERRED);
        assert!(closed[1].payload["session_id"].is_null(), "{closed:?}");
    }

    /// A headless Codex worker's span (ADR-t813-2 decision 7) records its
    /// turns from its run's turn events while open and at its close (one
    /// still running counts to the close), and its tokens from the turns'
    /// `tokens`; no Claude Code transcript is read for it.
    #[test]
    fn a_headless_span_takes_its_turns_and_tokens_from_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let conn = &queue.conn;
        conn.execute(
            "UPDATE task_runs SET worker_mode='headless', requested_provider='codex', actual_provider='codex'",
            [],
        )
        .unwrap();
        let reads = READS.with(std::cell::Cell::get);
        let record = |kind: EventKind, payload: Value| {
            event(conn, task_id, Some(&run), kind, payload).unwrap();
            latest(conn)
        };
        let at = |id: i64, millis: i64| {
            conn.execute(
                "UPDATE run_events SET created_at=?1 WHERE id=?2",
                params![millis_text(millis), id],
            )
            .unwrap();
        };
        let now = rfc3339_millis(&now(conn).unwrap()).unwrap();
        let base = now - 100_000;
        let tokens =
            json!({"input": 10, "output": 5, "cache_read": 20, "cache_creation": 0, "messages": 1});
        at(
            record(EventKind::AgentStarted, json!({"session_id": null})),
            base,
        );
        let opened = of_kind(&queue, SESSION_OPENED);
        assert_eq!(opened[0].payload["route"], "headless");
        assert_eq!(opened[0].payload["provider"], "codex");
        at(
            record(EventKind::TurnStarted, json!({"turn": 1})),
            base + 1_000,
        );
        at(
            record(
                EventKind::TurnFinished,
                json!({"turn": 1, "outcome": "succeeded", "tokens": tokens}),
            ),
            base + 31_000,
        );
        at(
            record(EventKind::TurnStarted, json!({"turn": 2})),
            base + 40_000,
        );
        assert_eq!(record_open_turns(conn).unwrap(), 1);
        assert_eq!(record_open_turns(conn).unwrap(), 0);
        at(
            record(
                EventKind::TurnFinished,
                json!({"turn": 2, "outcome": "succeeded", "tokens": tokens}),
            ),
            base + 60_000,
        );
        at(
            record(EventKind::TurnStarted, json!({"turn": 3})),
            base + 70_000,
        );
        record(EventKind::SessionExited, json!({"exit_code": 0}));

        let turns = of_kind(&queue, SESSION_TURNS);
        assert_eq!(turns.len(), 2, "{turns:?}");
        assert_eq!(turns[0].payload["turns"].as_array().unwrap().len(), 1);
        // The second turn and the third, still running at the close.
        assert_eq!(turns[1].payload["turns"].as_array().unwrap().len(), 2);
        let closed = &of_kind(&queue, SESSION_CLOSED)[0];
        assert_eq!(closed.payload["active"], "recorded");
        let active = closed.payload["active_secs"].as_i64().unwrap();
        assert!((80..=90).contains(&active), "{active}");
        let expected = json!({"input": 20, "output": 10, "cache_read": 40, "cache_creation": 0, "messages": 2});
        assert_eq!(closed.payload["tokens"], expected);
        assert!(closed.payload.get("model").is_none());
        // Its run directory has no file of its turns' commands: no work,
        // and why (task 1354).
        assert_eq!(closed.payload["work"], Value::Null);
        assert_eq!(closed.payload["work_unavailable"], TURN_COMMANDS_MISSING);
        assert_eq!(
            of_kind(&queue, "session_exited")[0].payload["tokens"],
            expected
        );
        assert_eq!(READS.with(std::cell::Cell::get), reads);
    }

    /// A turn of [`codex_close`]: its number, when its start and end are
    /// recorded (seconds after the span opened; none: not in the span, or
    /// still running) and its provider.
    type SpanTurnAt = (u64, Option<i64>, Option<i64>, &'static str);

    /// Two Codex turns, 1 at 1..31 and 2 at 40..60 seconds.
    const TWO_TURNS: [SpanTurnAt; 2] = [
        (1, Some(1), Some(31), "codex"),
        (2, Some(40), Some(60), "codex"),
    ];

    /// The close of a headless Codex worker's span in `dir`, opened at
    /// `base`, with `turns`, whose run directory has the files `files(base)`
    /// gives (turn, text) of their commands: its `session_closed` and the
    /// run directory.
    fn codex_close(
        dir: &std::path::Path,
        files: impl Fn(i64) -> Vec<(u64, String)>,
        turns: &[SpanTurnAt],
    ) -> (Value, std::path::PathBuf) {
        let (queue, task_id, run) = run_queue(dir);
        let conn = &queue.conn;
        let run_dir = dir.join("run");
        std::fs::create_dir_all(run_dir.join("turns")).unwrap();
        bind(conn, dir, "dagq");
        conn.execute(
            "UPDATE task_runs SET worker_mode='headless', requested_provider='codex',
               actual_provider='codex', run_dir=?1",
            [run_dir.to_str().unwrap()],
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET verification_commands=?1",
            [r#"["cargo llvm-cov --locked --fail-under-lines 80"]"#],
        )
        .unwrap();
        let record = |kind: EventKind, payload: Value| {
            event(conn, task_id, Some(&run), kind, payload).unwrap();
            latest(conn)
        };
        let at = |id: i64, millis: i64| {
            conn.execute(
                "UPDATE run_events SET created_at=?1 WHERE id=?2",
                params![millis_text(millis), id],
            )
            .unwrap();
        };
        let base = rfc3339_millis(&now(conn).unwrap()).unwrap() - 100_000;
        at(
            record(EventKind::AgentStarted, json!({"session_id": null})),
            base,
        );
        for &(turn, from, to, provider) in turns {
            if let Some(from) = from {
                at(
                    record(
                        EventKind::TurnStarted,
                        json!({"turn": turn, "provider": provider}),
                    ),
                    base + from * 1000,
                );
            }
            if let Some(to) = to {
                at(
                    record(
                        EventKind::TurnFinished,
                        json!({"turn": turn, "outcome": "succeeded", "provider": provider}),
                    ),
                    base + to * 1000,
                );
            }
        }
        for (turn, text) in files(base) {
            std::fs::write(crate::domain::turn::commands_path(&run_dir, turn), text).unwrap();
        }
        record(EventKind::SessionExited, json!({"exit_code": 0}));
        let closed = of_kind(&queue, SESSION_CLOSED)[0].payload.clone();
        (closed, run_dir)
    }

    /// A headless Codex worker's span (task 1354) takes its work from the
    /// commands its turns' wrapper wrote, as a Claude span's from its
    /// transcript: their times (one whose start was not read at its end),
    /// the model's time in its turns, idle between them, the cargo-only
    /// counts of dagq's source and the tests a test command's output named
    /// as failed; and each command is a `worktime.jsonl` line that says how
    /// its times were found.
    #[test]
    fn a_codex_span_takes_its_work_from_its_turns_commands() {
        let dir = tempfile::tempdir().unwrap();
        let (closed, run_dir) = codex_close(
            dir.path(),
            |base| {
                let llvm = json!({"id": "c1", "tool": "command_execution",
                    "command": "cargo llvm-cov --locked", "started": base + 5_000,
                    "ended": base + 25_000, "exit_code": 1, "status": "failed",
                    "failed_tests": ["a::b"]});
                let git = json!({"id": "c2", "tool": "command_execution",
                    "command": "git status", "ended": base + 50_000, "exit_code": 0,
                    "status": "completed"});
                vec![(1, format!("{llvm}\n")), (2, format!("{git}\n"))]
            },
            &TWO_TURNS,
        );
        let work = &closed["work"];
        assert!(work.is_object(), "{closed}");
        assert_eq!(closed.get("work_unavailable"), None);
        assert_eq!(work["secs"]["llvm_cov"], 20, "{work}");
        // 10 seconds of turn 1 and all 20 of turn 2: git took none.
        assert_eq!(work["secs"]["model"], 30, "{work}");
        assert_eq!(work["secs"].get("git"), None, "{work}");
        assert_eq!(
            work["commands"]["llvm_cov"],
            json!({"runs": 1, "failed": 1})
        );
        assert_eq!(work["verification_repeats"], 1);
        assert_eq!(work["llvm_cov_runs"], 1);
        assert_eq!(work["failed_tests"], json!(["a::b"]));
        assert!(!closed.to_string().contains("cargo"));
        let lines: Vec<Value> = std::fs::read_to_string(run_dir.join(WORKTIME_FILE))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0]["category"], "llvm_cov");
        assert_eq!(lines[0]["secs"], 20);
        assert_eq!(lines[0]["time_source"], worktime::TIMES_READ);
        assert_eq!(lines[0]["failed_tests"], json!(["a::b"]));
        assert_eq!(lines[1]["category"], "git");
        assert_eq!(lines[1]["secs"], 0);
        assert_eq!(lines[1]["time_source"], worktime::TIMES_COMPLETED_ONLY);
    }

    /// A headless Claude worker's span keeps taking its work from its
    /// transcript (task 1354 changed only Codex's): its turns' commands
    /// files are not read.
    #[test]
    fn a_headless_claude_span_takes_its_work_from_its_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let conn = &queue.conn;
        let run_dir = dir.path().join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        conn.execute(
            "UPDATE task_runs SET worker_mode='headless', run_dir=?1",
            [run_dir.to_str().unwrap()],
        )
        .unwrap();
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        assert_eq!(
            of_kind(&queue, SESSION_OPENED)[0].payload["route"],
            "headless"
        );
        let project = dir.path().join("config/projects/-wt");
        std::fs::create_dir_all(&project).unwrap();
        let line = |kind: &str, secs: i64, content: Value| {
            json!({"type": kind, "timestamp": millis_text(start + secs * 1000),
                   "sessionId": RUN, "version": "2.1.283", "message": {"content": content}})
            .to_string()
        };
        let lines = [
            line("user", 1, json!("go")),
            line(
                "assistant",
                5,
                json!([{"type": "tool_use", "id": "a", "name": "Bash",
                        "input": {"command": "cargo build"}}]),
            ),
            line(
                "user",
                25,
                json!([{"type": "tool_result", "tool_use_id": "a", "content": "ok"}]),
            ),
            line("assistant", 30, json!([{"type": "text"}])),
        ];
        std::fs::write(project.join(format!("{RUN}.jsonl")), lines.join("\n")).unwrap();
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::SessionExited,
            json!({"exit_code": 0}),
        )
        .unwrap();
        let closed = &of_kind(&queue, SESSION_CLOSED)[0].payload;
        assert_eq!(closed["work"]["secs"]["build"], 20, "{closed}");
        assert_eq!(closed.get("work_unavailable"), None);
        let written = std::fs::read_to_string(run_dir.join(WORKTIME_FILE)).unwrap();
        assert!(!written.contains("time_source"), "{written}");
    }

    /// Why a Codex span records no work: a finished turn without its
    /// file, a file that is not the wrapper's (or a link). Turns that ran
    /// no command are the model's time.
    #[test]
    fn a_codex_span_without_its_turns_commands_says_why() {
        let files = |texts: &'static [&'static str]| {
            move |_| {
                (1..)
                    .zip(texts)
                    .map(|(turn, text)| (turn, (*text).to_owned()))
                    .collect()
            }
        };
        for (texts, code) in [
            (&[""][..], TURN_COMMANDS_MISSING),
            (&["", "not json\n"][..], TURN_COMMANDS_UNPARSABLE),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (closed, run_dir) = codex_close(dir.path(), files(texts), &TWO_TURNS);
            assert_eq!(closed["work"], Value::Null, "{code}: {closed}");
            assert_eq!(closed["work_unavailable"], code, "{closed}");
            assert!(!run_dir.join(WORKTIME_FILE).exists(), "{code}");
        }
        // A link the worker put in the run directory is not followed.
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere.jsonl");
        std::fs::write(&elsewhere, "").unwrap();
        let (closed, _) = codex_close(
            dir.path(),
            |_| {
                let run_dir = dir.path().join("run");
                std::os::unix::fs::symlink(
                    &elsewhere,
                    crate::domain::turn::commands_path(&run_dir, 2),
                )
                .unwrap();
                vec![(1, String::new())]
            },
            &TWO_TURNS,
        );
        assert_eq!(
            closed["work_unavailable"], TURN_COMMANDS_UNPARSABLE,
            "{closed}"
        );
        let dir = tempfile::tempdir().unwrap();
        let (closed, _) = codex_close(dir.path(), files(&["", ""]), &TWO_TURNS);
        assert_eq!(closed["work"]["secs"]["model"], 50, "{closed}");
        assert_eq!(closed["work"]["heavy"], json!([]));
    }

    /// A headless planner's span (ADR-t1394-2 decision 4) opens on its
    /// first turn, records its finished turns while open from the queue's
    /// turn events that name it (not another planner's), and closes with
    /// its row, as inferred when no `planner_closed` ended it: its turns'
    /// active time and tokens, the model they ran on and its opener's
    /// effort. No transcript is read for it.
    #[test]
    fn a_headless_planners_span_takes_its_turns_and_closes_with_its_row() {
        let dir = tempfile::tempdir().unwrap();
        ClaudeTranscripts::use_config_dir_in_test(&dir.path().join("config"));
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let conn = &queue.conn;
        conn.execute_batch(
            "INSERT INTO planners(id,origin,route,created_at) VALUES (4,'runtime','headless',0);
             INSERT INTO planners(id,origin,route,created_at) VALUES (5,'runtime','headless',0);",
        )
        .unwrap();
        let reads = READS.with(std::cell::Cell::get);
        let record = |kind: EventKind, payload: Value, millis: i64| {
            crate::infrastructure::sqlite::record_queue_event_in(conn, kind, &payload).unwrap();
            let id = latest(conn);
            conn.execute(
                "UPDATE run_events SET created_at=?1 WHERE id>=?2",
                params![millis_text(millis), id],
            )
            .unwrap();
        };
        let now = rfc3339_millis(&now(conn).unwrap()).unwrap();
        let base = now - 100_000;
        let tokens =
            json!({"input": 10, "output": 5, "cache_read": 0, "cache_creation": 0, "messages": 1});
        record(
            EventKind::TurnStarted,
            json!({"planner_id": 4, "turn": 1, "session_id": "s-4", "provider": "claude",
                   "launch": {"model": "opus", "effort": "high"}}),
            base,
        );
        let opened = of_kind(&queue, SESSION_OPENED);
        assert_eq!(opened.len(), 1, "{opened:?}");
        assert_eq!(opened[0].task_id, None);
        assert_eq!(opened[0].payload["route"], "headless");
        record(
            EventKind::TurnFinished,
            json!({"planner_id": 4, "turn": 1, "outcome": "succeeded", "tokens": tokens,
                   "model": "claude-opus"}),
            base + 30_000,
        );
        // Another planner's turns are its own span's.
        record(
            EventKind::TurnStarted,
            json!({"planner_id": 5, "turn": 1}),
            base + 31_000,
        );
        record(
            EventKind::TurnFinished,
            json!({"planner_id": 5, "turn": 1, "outcome": "succeeded", "tokens": tokens}),
            base + 35_000,
        );
        record(
            EventKind::TurnStarted,
            json!({"planner_id": 4, "turn": 2}),
            base + 40_000,
        );
        assert_eq!(of_kind(&queue, SESSION_OPENED).len(), 2);
        assert_eq!(record_open_turns(conn).unwrap(), 2);
        assert_eq!(record_open_turns(conn).unwrap(), 0);
        queue
            .close_planner(crate::domain::PlannerId::new(4), Some("gone"))
            .unwrap();
        let closed = of_kind(&queue, SESSION_CLOSED);
        assert_eq!(closed.len(), 1, "{closed:?}");
        let closed = &closed[0].payload;
        assert_eq!(closed["opened_event_id"], opened[0].id.as_i64());
        assert_eq!(closed["reason"], INFERRED);
        assert_eq!(closed["active"], "recorded");
        // The running turn's end is not known.
        assert_eq!(closed["active_secs"], 30);
        assert_eq!(closed["tokens"], tokens);
        assert_eq!(closed["model"], "claude-opus");
        assert_eq!(closed["effort"], "high");
        assert_eq!(READS.with(std::cell::Cell::get), reads);
        // The plugin's hook, if its turns run it, records nothing for a
        // headless planner and does not see its span.
        let planner5 = |event| SessionHook {
            planner_id: Some(5),
            ..hook(dir.path(), event, RUNTIME_PLANNER, "s-5", "s-5")
        };
        let skipped = record_hook(conn, &planner5(Some("startup"))).unwrap();
        assert_eq!(skipped["skipped"], "headless_planner");
        record_hook(conn, &planner5(None)).unwrap();
        assert_eq!(of_kind(&queue, SESSION_OPENED).len(), 2);
        assert_eq!(of_kind(&queue, SESSION_CLOSED).len(), 1);
        assert!(open_hook_spans(conn).unwrap().is_empty());
    }

    /// The finished turns of an open span are recorded as they come; the
    /// span's close records the rest once and its active time.
    #[test]
    fn a_span_records_its_turns_while_open_and_its_active_time_at_its_close() {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let conn = &queue.conn;
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        // The first turn is finished (the next input came); the second not.
        transcript(dir.path(), start, &[(5, 25), (40, 50)], None);
        assert_eq!(record_open_turns(conn).unwrap(), 1);
        // Nothing new: nothing written.
        assert_eq!(record_open_turns(conn).unwrap(), 0);
        let recorded = of_kind(&queue, SESSION_TURNS);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].payload["kind"], "worker");
        assert_eq!(recorded[0].payload["turns"].as_array().unwrap().len(), 1);
        assert_eq!(
            recorded[0].payload["through"],
            json!(millis_text(start + 25_000))
        );
        assert_eq!(recorded[0].run_id.as_ref(), Some(&run));

        event(
            conn,
            task_id,
            Some(&run),
            EventKind::SessionExited,
            json!({"exit_code": 0}),
        )
        .unwrap();
        let recorded = of_kind(&queue, SESSION_TURNS);
        assert_eq!(recorded.len(), 2);
        assert_eq!(
            recorded[1].payload["turns"],
            json!([[millis_text(start + 40_000), millis_text(start + 50_000)]])
        );
        let closed = &of_kind(&queue, SESSION_CLOSED)[0];
        assert_eq!(closed.payload["reason"], "exited");
        assert_eq!(closed.payload["active"], "recorded");
        assert_eq!(closed.payload["active_secs"], 30);
    }

    /// A revise typed into the session before its `revise_requested` was
    /// written switches the spans when it was sent: its turn is the
    /// revise's, not the worker's.
    #[test]
    fn a_revise_switches_the_spans_when_it_was_sent() {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let conn = &queue.conn;
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        // The worker's turn, then the revise typed at 60 s, answered by 80 s.
        transcript(dir.path(), start, &[(5, 25), (61, 80)], None);
        let sent = (start + 60_000) / 1000;
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::ReviseRequested,
            json!({"attempt": 1, "sent_at": sent}),
        )
        .unwrap();
        let spans = spans(&queue);
        assert_eq!(spans[1].payload["active_secs"], 20);
        assert_eq!(spans[1].created_at, millis_text(sent * 1000));
        assert_eq!(spans[2].kind, SESSION_OPENED);
        assert_eq!(spans[2].created_at, millis_text(sent * 1000));
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::SessionExited,
            json!({"exit_code": 0}),
        )
        .unwrap();
        let closed = &of_kind(&queue, SESSION_CLOSED)[1];
        assert_eq!(closed.payload["kind"], "revise");
        assert_eq!(closed.payload["active_secs"], 19);
    }

    /// A revise sent at a time recorded to the millisecond (task 1197)
    /// switches the spans at that millisecond.
    #[test]
    fn a_revise_sent_at_a_millisecond_switches_the_spans_there() {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let conn = &queue.conn;
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        transcript(dir.path(), start, &[(5, 25), (61, 80)], None);
        let sent = start + 60_250;
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::ReviseRequested,
            json!({"attempt": 1, "sent_at": sent as f64 / 1000.0}),
        )
        .unwrap();
        let spans = spans(&queue);
        assert_eq!(spans[1].created_at, millis_text(sent));
        assert_eq!(spans[2].kind, SESSION_OPENED);
        assert_eq!(spans[2].created_at, millis_text(sent));
    }

    /// A span closed as inferred ends at its transcript's last record; its
    /// turns are cut there. One whose transcript cannot be read says why,
    /// and the event that closed it is written all the same.
    #[test]
    fn an_inferred_close_ends_at_the_transcript_and_an_unreadable_one_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let conn = &queue.conn;
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        transcript(dir.path(), start, &[(5, 20)], Some(30));
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::WorkspaceClosed,
            json!({}),
        )
        .unwrap();
        let closed = &of_kind(&queue, SESSION_CLOSED)[0];
        assert_eq!(closed.payload["reason"], "inferred");
        assert_eq!(closed.payload["active_secs"], 15);
        assert_eq!(closed.created_at, millis_text(start + 30_000));

        // The resume's session: its transcript is not JSON.
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::ResumeStarted,
            json!({}),
        )
        .unwrap();
        let before = latest(conn);
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        retime(conn, before, 10);
        std::fs::write(
            dir.path().join(format!("config/projects/-wt/{RUN}.jsonl")),
            "not json",
        )
        .unwrap();
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::SessionExited,
            json!({"exit_code": 0}),
        )
        .unwrap();
        let closed = &of_kind(&queue, SESSION_CLOSED)[1];
        assert_eq!(closed.payload["kind"], "resume");
        assert_eq!(closed.payload["active"], "unavailable");
        assert_eq!(
            closed.payload["active_unavailable"],
            "transcript_unparsable"
        );
        assert_eq!(of_kind(&queue, "session_exited").len(), 1);
        // An open span whose transcript cannot be read is left for later.
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        assert_eq!(record_open_turns(conn).unwrap(), 0);
    }

    /// Bind the queue on `conn` to a new Git repository under `dir` whose
    /// `Cargo.toml` names the package `package`.
    fn bind(conn: &Connection, dir: &std::path::Path, package: &str) {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let init = std::process::Command::new(git_executable().expect("git executable"))
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(init.success());
        std::fs::write(
            repo.join("Cargo.toml"),
            format!("[package]\nname = \"{package}\"\n"),
        )
        .unwrap();
        let common_dir = crate::infrastructure::adapters::git_common_dir(&repo).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO queue_repository(singleton,git_common_dir) VALUES (1,?1)",
            [common_dir.to_str().unwrap()],
        )
        .unwrap();
    }

    /// Outside dagq's source (ADR-t614-1), and for a queue bound to no
    /// repository, a run's session closes with a work breakdown that labels
    /// no command e2e, llvm-cov or test and counts none of what `integrate`
    /// repeats.
    #[test]
    fn outside_dagqs_source_the_work_breakdown_keeps_no_cargo_measure() {
        for package in [Some("other"), None] {
            let dir = tempfile::tempdir().unwrap();
            let (queue, task_id, run) = run_queue(dir.path());
            let conn = &queue.conn;
            if let Some(package) = package {
                bind(conn, dir.path(), package);
            }
            conn.execute(
                "UPDATE tasks SET verification_commands=?1",
                [r#"["cargo llvm-cov --locked --fail-under-lines 80"]"#],
            )
            .unwrap();
            event(
                conn,
                task_id,
                Some(&run),
                EventKind::AgentStarted,
                json!({"session_id": RUN}),
            )
            .unwrap();
            let start = retime(conn, 0, 100);
            let project = dir.path().join("config/projects/-wt");
            std::fs::create_dir_all(&project).unwrap();
            let line = |kind: &str, secs: i64, content: Value| {
                json!({"type": kind, "timestamp": millis_text(start + secs * 1000),
                       "sessionId": RUN, "version": "2.1.283", "message": {"content": content}})
                .to_string()
            };
            let lines = [
                line("user", 1, json!("go")),
                line(
                    "assistant",
                    5,
                    json!([{"type": "tool_use", "id": "a", "name": "Bash",
                            "input": {"command": "cargo llvm-cov --locked"}}]),
                ),
                line(
                    "user",
                    45,
                    json!([{"type": "tool_result", "tool_use_id": "a", "is_error": true,
                            "content": "Exit code 1"}]),
                ),
                line("assistant", 50, json!([{"type": "text"}])),
            ];
            std::fs::write(project.join(format!("{RUN}.jsonl")), lines.join("\n")).unwrap();
            event(
                conn,
                task_id,
                Some(&run),
                EventKind::SessionExited,
                json!({"exit_code": 0}),
            )
            .unwrap();
            let work = &of_kind(&queue, SESSION_CLOSED)[0].payload["work"];
            assert_eq!(work["secs"]["other_command"], 40, "{package:?}: {work}");
            assert!(work["secs"].get("llvm_cov").is_none(), "{work}");
            assert_eq!(work["commands"], json!({}), "{work}");
            for key in ["verification_repeats", "full_tests", "llvm_cov_runs"] {
                assert!(work.get(key).is_none(), "{key}: {work}");
            }
        }
    }

    /// A run's session closes with its work breakdown: the aggregate on its
    /// `session_closed` and its `session_exited`, each command in the run
    /// directory's `worktime.jsonl`; a resume's is found for its
    /// `resume_finished`. An unreadable transcript records none.
    #[test]
    fn a_run_session_records_its_work_breakdown_at_its_close() {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let run_dir = dir.path().join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        let conn = &queue.conn;
        // dagq's source keeps the cargo-only kinds and counts (ADR-t614-1).
        bind(conn, dir.path(), "dagq");
        conn.execute(
            "UPDATE task_runs SET run_dir=?1",
            [run_dir.to_str().unwrap()],
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET verification_commands=?1",
            [r#"["cargo llvm-cov --locked --fail-under-lines 80"]"#],
        )
        .unwrap();
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        let project = dir.path().join("config/projects/-wt");
        std::fs::create_dir_all(&project).unwrap();
        let line = |kind: &str, secs: i64, content: Value| {
            json!({"type": kind, "timestamp": millis_text(start + secs * 1000),
                   "sessionId": RUN, "version": "2.1.283", "message": {"content": content}})
            .to_string()
        };
        let bash = |id: &str, command: &str| {
            json!([{"type": "tool_use", "id": id, "name": "Bash",
                    "input": {"command": command}}])
        };
        let result = |id: &str, error: bool| {
            json!([{"type": "tool_result", "tool_use_id": id, "is_error": error,
                    "content": if error { "Exit code 1" } else { "ok" }}])
        };
        let lines = [
            line("user", 1, json!("go")),
            line("assistant", 5, bash("a", "cargo llvm-cov --locked")),
            line("user", 45, result("a", true)),
            line("assistant", 50, bash("b", "cargo test --locked")),
            line("user", 70, result("b", false)),
            line("assistant", 75, json!([{"type": "text"}])),
        ];
        std::fs::write(project.join(format!("{RUN}.jsonl")), lines.join("\n")).unwrap();
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::SessionExited,
            json!({"exit_code": 0}),
        )
        .unwrap();
        let closed = &of_kind(&queue, SESSION_CLOSED)[0];
        let work = &closed.payload["work"];
        assert_eq!(work["secs"]["llvm_cov"], 40);
        assert_eq!(work["secs"]["test"], 20);
        assert_eq!(
            work["commands"]["llvm_cov"],
            json!({"runs": 1, "failed": 1})
        );
        assert_eq!(work["verification_repeats"], 1);
        assert_eq!(work["full_tests"], 1);
        assert!(!closed.payload.to_string().contains("cargo"));
        let exited = &of_kind(&queue, "session_exited")[0];
        assert_eq!(exited.payload["exit_code"], 0);
        assert_eq!(exited.payload["work_breakdown"]["kind"], "worker");
        assert_eq!(exited.payload["work_breakdown"]["attempt"], 1);
        assert_eq!(exited.payload["work_breakdown"]["secs"], work["secs"]);
        let written = std::fs::read_to_string(run_dir.join(WORKTIME_FILE)).unwrap();
        let written: Vec<Value> = written
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(written.len(), 2);
        assert_eq!(written[0]["category"], "llvm_cov");
        assert_eq!(written[0]["exit_code"], 1);
        assert_eq!(written[0]["kind"], "worker");
        assert_eq!(written[1]["command"], "cargo test --locked");
        assert_eq!(
            closed_work(conn, &run, "worker", EventId::new(0)).unwrap(),
            Some(exited.payload["work_breakdown"].clone())
        );

        // A resume in the same session, whose transcript is unreadable.
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::ResumeStarted,
            json!({}),
        )
        .unwrap();
        let resumed = latest(conn);
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        std::fs::write(project.join(format!("{RUN}.jsonl")), "not json").unwrap();
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::SessionExited,
            json!({"exit_code": 0}),
        )
        .unwrap();
        let exited = &of_kind(&queue, "session_exited")[1];
        assert!(exited.payload.get("work_breakdown").is_none());
        assert!(
            of_kind(&queue, SESSION_CLOSED)[1]
                .payload
                .get("work")
                .is_none()
        );
        assert_eq!(
            closed_work(conn, &run, "resume", EventId::new(resumed)).unwrap(),
            None
        );
    }

    /// A resumed session that exited before its resume finished gives
    /// `resume_finished` its work breakdown, of that attempt only: a span
    /// of an earlier attempt, closed after this attempt started, is not
    /// taken, and an attempt whose span is still open carries none.
    #[test]
    fn resume_finished_carries_the_work_of_its_own_attempt() {
        use crate::domain::CommitSha;
        let dir = tempfile::tempdir().unwrap();
        let (mut queue, task_id, run) = run_queue(dir.path());
        // Every span's transcript is readable, so each closes with its work.
        transcript(dir.path(), 0, &[(1, 2)], None);
        let main = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
        let record = |queue: &SqliteQueue, kind: EventKind, payload: Value| {
            event(&queue.conn, task_id, Some(&run), kind, payload).unwrap();
        };
        let finished = |queue: &SqliteQueue| {
            of_kind(queue, "resume_finished")
                .last()
                .unwrap()
                .payload
                .clone()
        };
        let park = |queue: &SqliteQueue| {
            queue
                .conn
                .execute(
                    "UPDATE task_runs SET status='needs_session', base_commit=?1",
                    [main.as_str()],
                )
                .unwrap();
        };
        let resume = |queue: &mut SqliteQueue, attempt: usize| {
            let (_, started) = queue
                .begin_resume(
                    &run,
                    &LeaseToken::new("tok"),
                    &main,
                    None,
                    Default::default(),
                )
                .unwrap()
                .unwrap();
            assert_eq!(started, attempt);
            record(queue, EventKind::AgentStarted, json!({"session_id": RUN}));
        };
        let finish = |queue: &mut SqliteQueue| {
            queue
                .finish_resume(
                    &run,
                    &LeaseToken::new("tok"),
                    None,
                    None,
                    false,
                    json!({"outcome": "unresolved"}),
                )
                .unwrap();
        };
        queue
            .conn
            .execute("UPDATE tasks SET status='in_progress'", [])
            .unwrap();
        park(&queue);

        // Attempt 1: its session is still open when the resume finishes.
        resume(&mut queue, 1);
        finish(&mut queue);
        assert!(finished(&queue).get("work_breakdown").is_none());
        // Attempt 2 starts: attempt 1's span closes as inferred, with its
        // work, but it is not attempt 2's.
        resume(&mut queue, 2);
        let inferred = &of_kind(&queue, SESSION_CLOSED)[0];
        assert_eq!(inferred.payload["reason"], "inferred");
        assert!(inferred.payload["work"].is_object());
        finish(&mut queue);
        assert!(finished(&queue).get("work_breakdown").is_none());
        // Attempt 3 exits before its resume finishes: its own work.
        resume(&mut queue, 3);
        record(&queue, EventKind::SessionExited, json!({"exit_code": 0}));
        finish(&mut queue);
        let work = finished(&queue)["work_breakdown"].clone();
        assert_eq!(work["kind"], "resume");
        assert_eq!(work["attempt"], 3);
        assert!(work["total_secs"].is_i64());
        assert_eq!(
            of_kind(&queue, "session_exited")[0].payload["work_breakdown"],
            work
        );
    }

    /// Every span closes with the tokens of its transcript's messages in
    /// it: a run's session also gives them to its `session_exited` and a
    /// resume's to its `resume_finished`, a headless job's span has its
    /// own; a usage this reader does not know records none, and leaves the
    /// active time and the run as they were.
    #[test]
    fn spans_record_the_tokens_of_their_transcripts() {
        use crate::domain::CommitSha;
        let dir = tempfile::tempdir().unwrap();
        let (mut queue, task_id, run) = run_queue(dir.path());
        let project = dir.path().join("config/projects/-wt");
        std::fs::create_dir_all(&project).unwrap();
        let write = |session: &str, base: i64, lines: &[(&str, i64, Value)]| {
            let lines: Vec<String> = lines
                .iter()
                .map(|(kind, secs, message)| {
                    json!({"type": kind, "timestamp": millis_text(base + secs * 1000),
                           "sessionId": session, "version": "2.1.283", "message": message})
                    .to_string()
                })
                .collect();
            std::fs::write(project.join(format!("{session}.jsonl")), lines.join("\n")).unwrap();
        };
        let reply = |id: &str, input: i64, output: i64| {
            json!({"id": id, "content": [{"type": "text"}], "usage": {
                "input_tokens": input, "output_tokens": output,
                "cache_read_input_tokens": 100, "cache_creation_input_tokens": 10}})
        };
        let go = json!({"content": "go"});
        let record = |queue: &SqliteQueue, kind: EventKind, payload: Value| {
            event(&queue.conn, task_id, Some(&run), kind, payload).unwrap();
        };

        record(&queue, EventKind::AgentStarted, json!({"session_id": RUN}));
        let start = retime(&queue.conn, 0, 100);
        write(
            RUN,
            start,
            &[
                ("user", 1, go.clone()),
                ("assistant", 2, reply("m1", 3, 5)),
                ("assistant", 3, reply("m1", 3, 9)),
                ("assistant", 4, reply("m2", 2, 1)),
            ],
        );
        // The review job runs in a session of its own.
        record(
            &queue,
            EventKind::ReviewStarted,
            json!({"attempt": 1, "session_id": "s-review"}),
        );
        // Its span opened 50 s ago; the review_started is now.
        let now = retime(&queue.conn, latest(&queue.conn) - 1, 50);
        write(
            "s-review",
            now - 50_000,
            &[("user", 1, go.clone()), ("assistant", 2, reply("r1", 7, 4))],
        );
        record(
            &queue,
            EventKind::ReviewFinished,
            json!({"verdict": "pass"}),
        );
        record(&queue, EventKind::SessionExited, json!({"exit_code": 0}));

        let expected = json!({"input": 5, "output": 10, "cache_read": 200,
                              "cache_creation": 20, "messages": 2});
        let closed = of_kind(&queue, SESSION_CLOSED);
        let review = closed
            .iter()
            .find(|e| e.payload["kind"] == "review")
            .unwrap();
        assert_eq!(
            review.payload["tokens"],
            json!({"input": 7, "output": 4, "cache_read": 100, "cache_creation": 10, "messages": 1})
        );
        let worker = closed
            .iter()
            .find(|e| e.payload["kind"] == "worker")
            .unwrap();
        assert_eq!(worker.payload["tokens"], expected);
        let exited = &of_kind(&queue, "session_exited")[0];
        assert_eq!(exited.payload["tokens"], expected);
        assert_eq!(exited.payload["exit_code"], 0);

        // A resume whose session exits before it finishes: its own tokens.
        let main = CommitSha::try_from("0123456789abcdef0123456789abcdef01234567").unwrap();
        queue
            .conn
            .execute("UPDATE tasks SET status='in_progress'", [])
            .unwrap();
        queue
            .conn
            .execute(
                "UPDATE task_runs SET status='needs_session', base_commit=?1",
                [main.as_str()],
            )
            .unwrap();
        queue
            .begin_resume(
                &run,
                &LeaseToken::new("tok"),
                &main,
                None,
                Default::default(),
            )
            .unwrap()
            .unwrap();
        let before = latest(&queue.conn);
        record(&queue, EventKind::AgentStarted, json!({"session_id": RUN}));
        // Its span opened 20 s ago; begin_resume's events are now.
        let resumed = retime(&queue.conn, before, 20) - 20_000;
        write(
            RUN,
            start,
            &[
                ("user", 1, go.clone()),
                ("assistant", 2, reply("m1", 3, 9)),
                ("user", (resumed - start) / 1000 + 1, go.clone()),
                (
                    "assistant",
                    (resumed - start) / 1000 + 2,
                    reply("m3", 11, 13),
                ),
            ],
        );
        record(&queue, EventKind::SessionExited, json!({"exit_code": 0}));
        queue
            .finish_resume(
                &run,
                &LeaseToken::new("tok"),
                None,
                None,
                false,
                json!({"outcome": "unresolved"}),
            )
            .unwrap();
        let finished = &of_kind(&queue, "resume_finished")[0];
        assert_eq!(
            finished.payload["tokens"],
            json!({"input": 11, "output": 13, "cache_read": 100, "cache_creation": 10, "messages": 1})
        );

        // A usage of an unknown form: no tokens, the rest as before.
        queue
            .conn
            .execute("UPDATE task_runs SET status='needs_session'", [])
            .unwrap();
        queue
            .begin_resume(
                &run,
                &LeaseToken::new("tok"),
                &main,
                None,
                Default::default(),
            )
            .unwrap()
            .unwrap();
        let before = latest(&queue.conn);
        record(&queue, EventKind::AgentStarted, json!({"session_id": RUN}));
        let again = retime(&queue.conn, before, 10) - 10_000;
        write(
            RUN,
            again,
            &[
                ("user", 1, go),
                (
                    "assistant",
                    2,
                    json!({"id": "x", "usage": {"input_tokens": "many"}}),
                ),
            ],
        );
        record(&queue, EventKind::SessionExited, json!({"exit_code": 0}));
        let closed = of_kind(&queue, SESSION_CLOSED);
        let last = closed.last().unwrap();
        assert_eq!(last.payload["active"], "recorded");
        assert!(last.payload.get("tokens").is_none());
        let exited = of_kind(&queue, "session_exited");
        assert!(exited.last().unwrap().payload.get("tokens").is_none());
        assert_eq!(
            closed_tokens(&queue.conn, &run, "resume", EventId::new(before)).unwrap(),
            None
        );
    }

    /// An inferred close ends at the transcript's last record, whose
    /// message is still the span's.
    #[test]
    fn an_inferred_close_keeps_the_tokens_of_the_last_record() {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let conn = &queue.conn;
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        let project = dir.path().join("config/projects/-wt");
        std::fs::create_dir_all(&project).unwrap();
        let line = |kind: &str, secs: i64, message: Value| {
            json!({"type": kind, "timestamp": millis_text(start + secs * 1000),
                   "sessionId": RUN, "version": "2.1.283", "message": message})
            .to_string()
        };
        let lines = [
            line("user", 5, json!({"content": "go"})),
            line(
                "assistant",
                20,
                json!({"id": "m", "content": [],
                 "usage": {"input_tokens": 4, "output_tokens": 6}}),
            ),
        ];
        std::fs::write(project.join(format!("{RUN}.jsonl")), lines.join("\n")).unwrap();
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::WorkspaceClosed,
            json!({}),
        )
        .unwrap();
        let closed = &of_kind(&queue, SESSION_CLOSED)[0];
        assert_eq!(closed.payload["reason"], "inferred");
        assert_eq!(closed.created_at, millis_text(start + 20_000));
        assert_eq!(closed.payload["tokens"]["output"], 6);
    }

    /// Every span closes with the model and effort of its transcript's
    /// messages (task 579): a headless job's and a run's own, with the
    /// breakdown when they changed in it; a span whose transcript cannot be
    /// read records none and still closes.
    #[test]
    fn spans_record_the_models_and_efforts_of_their_transcripts() {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let project = dir.path().join("config/projects/-wt");
        std::fs::create_dir_all(&project).unwrap();
        let write = |session: &str, base: i64, lines: &[(i64, &str, &str, &str)]| {
            let mut text = vec![
                json!({"type": "user", "timestamp": millis_text(base + 1000),
                       "sessionId": session, "message": {"content": "go"}})
                .to_string(),
            ];
            text.extend(lines.iter().map(|(secs, id, model, effort)| {
                json!({"type": "assistant", "timestamp": millis_text(base + secs * 1000),
                       "sessionId": session, "version": "2.1.283", "effort": effort,
                       "message": {"id": id, "model": model, "content": [],
                                   "usage": {"input_tokens": 1, "output_tokens": 1}}})
                .to_string()
            }));
            std::fs::write(project.join(format!("{session}.jsonl")), text.join("\n")).unwrap();
        };
        let record = |kind: EventKind, payload: Value| {
            event(&queue.conn, task_id, Some(&run), kind, payload).unwrap();
        };
        let opus = "claude-opus-5-5";

        record(EventKind::AgentStarted, json!({"session_id": RUN}));
        let start = retime(&queue.conn, 0, 100);
        write(
            RUN,
            start,
            &[
                (2, "m1", opus, "medium"),
                (3, "m2", opus, "high"),
                (4, "m3", opus, "high"),
            ],
        );
        record(
            EventKind::ReviewStarted,
            json!({"attempt": 1, "session_id": "s-review"}),
        );
        let now = retime(&queue.conn, latest(&queue.conn) - 1, 50);
        write("s-review", now - 50_000, &[(2, "r1", opus, "medium")]);
        record(EventKind::ReviewFinished, json!({"verdict": "pass"}));
        // The triage's transcript is missing.
        record(
            EventKind::TriageStarted,
            json!({"attempt": 1, "session_id": "s-gone"}),
        );
        record(EventKind::TriageFinished, json!({"decision": "retry"}));
        record(EventKind::SessionExited, json!({"exit_code": 0}));

        let closed = of_kind(&queue, SESSION_CLOSED);
        let span = |kind: &str| {
            closed
                .iter()
                .find(|e| e.payload["kind"] == kind)
                .unwrap()
                .payload
                .clone()
        };
        let review = span("review");
        assert_eq!(review["model"], opus);
        assert_eq!(review["effort"], "medium");
        assert!(review.get("models").is_none());
        let worker = span("worker");
        assert_eq!(worker["model"], opus);
        assert_eq!(worker["effort"], "high");
        assert_eq!(
            worker["models"],
            json!([
                {"model": opus, "effort": "high", "messages": 2},
                {"model": opus, "effort": "medium", "messages": 1},
            ])
        );
        let triage = span("triage");
        assert_eq!(triage["active"], "unavailable");
        assert!(triage.get("model").is_none());
        assert!(triage.get("effort").is_none());
    }

    /// A goal review's span (task 1062) opens with its
    /// `goal_review_started` on the goal's first task, with its session id,
    /// cwd, goal, review and launch; closes with its finish taking the model
    /// and effort its transcript names, as every job's span does; and one
    /// whose row was finished as `interrupted` is closed as `inferred`,
    /// its transcript read before the write transaction.
    #[test]
    fn a_goal_review_span_records_its_launch_and_the_model_of_its_transcript() {
        use crate::application::TaskStore;
        use crate::infrastructure::sqlite::goal_event;
        let dir = tempfile::tempdir().unwrap();
        let (mut queue, task_id, _) = run_queue(dir.path());
        let goal = queue
            .add_goal(crate::domain::NewGoal {
                title: "g".into(),
                description: String::new(),
                acceptance: String::new(),
                constraints: String::new(),
                doc: None,
                draft: false,
            })
            .unwrap()
            .id();
        queue.set_goal(task_id, Some(goal)).unwrap();
        let later = task(&mut queue);
        queue.set_goal(later, Some(goal)).unwrap();
        let launch = crate::domain::actor_model::ActorLaunch::default_of(
            crate::domain::actor_model::ModelRole::GoalReview,
        )
        .to_value();
        let conn = &queue.conn;
        goal_event(
            conn,
            goal,
            EventKind::GoalReviewStarted,
            json!({"goal_review_id": 7, "attempt": 1, "session_id": "s-goal",
                   "cwd": "/repo", "launch": launch}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        let project = dir.path().join("config/projects/-repo");
        std::fs::create_dir_all(&project).unwrap();
        let lines = [
            json!({"type": "user", "timestamp": millis_text(start + 1000),
                   "sessionId": "s-goal", "message": {"content": "go"}}),
            json!({"type": "assistant", "timestamp": millis_text(start + 2000),
                   "sessionId": "s-goal", "version": "2.1.283", "effort": "high",
                   "message": {"id": "g1", "model": "claude-opus-5-5", "content": [],
                               "usage": {"input_tokens": 1, "output_tokens": 1}}}),
        ]
        .map(|line| line.to_string());
        std::fs::write(project.join("s-goal.jsonl"), lines.join("\n")).unwrap();
        goal_event(
            conn,
            goal,
            EventKind::GoalReviewFinished,
            json!({"goal_review_id": 7, "attempt": 1, "decision": "achieved"}),
        )
        .unwrap();
        let opened = of_kind(&queue, SESSION_OPENED);
        assert_eq!(opened.len(), 1);
        // On the goal's first task, as its `approve_goal` ask is.
        assert_eq!(opened[0].task_id, Some(task_id));
        assert_eq!(opened[0].run_id, None);
        assert_eq!(opened[0].payload["kind"], GOAL_REVIEW);
        assert_eq!(opened[0].payload["session_id"], "s-goal");
        assert_eq!(opened[0].payload["cwd"], "/repo");
        assert_eq!(opened[0].payload["goal_id"], goal.as_i64());
        assert_eq!(opened[0].payload["goal_review_id"], 7);
        assert_eq!(opened[0].payload["launch"], launch);
        assert_eq!(opened[0].payload["launch"]["provider"], "claude");
        let closed = of_kind(&queue, SESSION_CLOSED);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].task_id, Some(task_id));
        assert_eq!(closed[0].payload["reason"], JOB_FINISHED);
        assert_eq!(closed[0].payload["model"], "claude-opus-5-5");
        assert_eq!(closed[0].payload["effort"], "high");
        assert_eq!(closed[0].payload["active"], "recorded");

        // A review whose supervisor went away closes when its row does.
        goal_event(
            conn,
            goal,
            EventKind::GoalReviewStarted,
            json!({"goal_review_id": 8, "attempt": 2, "session_id": "s-goal-2", "cwd": "/repo"}),
        )
        .unwrap();
        {
            let _read = read_before(conn, Closing::GoalReviews(None, true)).unwrap();
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
            close_goal_review(&tx, 8, true).unwrap();
            // Closed already: nothing more.
            close_goal_review(&tx, 8, true).unwrap();
            tx.commit().unwrap();
        }
        let closed = of_kind(&queue, SESSION_CLOSED);
        assert_eq!(closed.len(), 2);
        assert_eq!(closed[1].payload["session_id"], "s-goal-2");
        assert_eq!(closed[1].payload["reason"], INFERRED);
        // Another goal event opens or closes nothing.
        goal_event(conn, goal, EventKind::GoalReviewRearmed, json!({})).unwrap();
        assert_eq!(of_kind(&queue, SESSION_OPENED).len(), 2);
    }

    /// A throughput review's span (task 1086) opens with its
    /// `throughput_review_started` on the queue, with its session id, dir,
    /// mode, period and launch, and closes with its own finish taking the
    /// model and effort of its transcript, read before the write
    /// transaction; a review running beside it stays open, one open past
    /// its time closes as `inferred` at the next event of the throughput
    /// review, and the observer's spans are not touched by either.
    #[test]
    fn a_throughput_review_span_records_its_launch_and_the_model_of_its_transcript() {
        use crate::domain::sessions::{OBSERVER, THROUGHPUT_REVIEW, THROUGHPUT_REVIEW_OPEN_MS};
        let dir = tempfile::tempdir().unwrap();
        let (queue, _, _) = run_queue(dir.path());
        let conn = &queue.conn;
        let write = |cwd: &str, session: &str, base: i64, model: &str, effort: &str| {
            let project = dir
                .path()
                .join("config/projects")
                .join(cwd.replace('/', "-"));
            std::fs::create_dir_all(&project).unwrap();
            let lines = [
                json!({"type": "user", "timestamp": millis_text(base + 1000),
                       "sessionId": session, "message": {"content": "go"}}),
                json!({"type": "assistant", "timestamp": millis_text(base + 2000),
                       "sessionId": session, "version": "2.1.283", "effort": effort,
                       "message": {"id": session, "model": model, "content": [],
                                   "usage": {"input_tokens": 1, "output_tokens": 1}}}),
            ]
            .map(|line| line.to_string());
            std::fs::write(project.join(format!("{session}.jsonl")), lines.join("\n")).unwrap();
        };
        let launch = crate::domain::actor_model::ActorLaunch::default_of(
            crate::domain::actor_model::ModelRole::ThroughputReview,
        )
        .to_value();
        let record = |kind: EventKind, payload: Value| {
            queue.record_queue_event(kind, payload).unwrap();
        };
        record(
            EventKind::ObserveStarted,
            json!({"mode": "hourly", "dir": "/obs", "session_id": "s-obs"}),
        );
        record(
            EventKind::ThroughputReviewStarted,
            json!({"mode": "hourly", "period": "2026-09-29T13", "dir": "/tr/h",
                   "session_id": "s-h", "launch": launch}),
        );
        let start = retime(conn, 0, 100);
        write("/tr/h", "s-h", start, "claude-opus-5-5", "high");
        // A daily review starts while the hourly one runs (as after an
        // exec's handoff): both stay open.
        record(
            EventKind::ThroughputReviewStarted,
            json!({"mode": "daily", "period": "2026-09-28", "dir": "/tr/d",
                   "session_id": "s-d", "launch": launch}),
        );
        let daily_from = latest(conn) - 2;
        let opened = of_kind(&queue, SESSION_OPENED);
        assert_eq!(opened.len(), 3);
        let hourly = &opened[1];
        assert_eq!(hourly.task_id, None);
        assert_eq!(hourly.run_id, None);
        assert_eq!(hourly.payload["kind"], THROUGHPUT_REVIEW);
        assert_eq!(hourly.payload["session_id"], "s-h");
        assert_eq!(hourly.payload["cwd"], "/tr/h");
        assert_eq!(hourly.payload["mode"], "hourly");
        assert_eq!(hourly.payload["period"], "2026-09-29T13");
        assert_eq!(hourly.payload["launch"], launch);
        assert!(of_kind(&queue, SESSION_CLOSED).is_empty());

        READS.set(0);
        record(
            EventKind::ThroughputReviewFinished,
            json!({"mode": "hourly", "period": "2026-09-29T13", "outcome": "succeeded",
                   "dir": "/tr/h", "session_id": "s-h"}),
        );
        assert_eq!(READS.get(), 1);
        let closed = of_kind(&queue, SESSION_CLOSED);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].task_id, None);
        assert_eq!(closed[0].payload["kind"], THROUGHPUT_REVIEW);
        assert_eq!(closed[0].payload["session_id"], "s-h");
        assert_eq!(closed[0].payload["reason"], JOB_FINISHED);
        assert_eq!(closed[0].payload["model"], "claude-opus-5-5");
        assert_eq!(closed[0].payload["effort"], "high");
        assert_eq!(closed[0].payload["active"], "recorded");

        // The daily review died without its finish: a skipped hour closes
        // it once it has been open past its time, at its transcript's end.
        let old = THROUGHPUT_REVIEW_OPEN_MS / 1000 + 60;
        let daily_start = retime(conn, daily_from, old);
        write("/tr/d", "s-d", daily_start, "claude-sonnet-5", "medium");
        record(
            EventKind::ThroughputReviewFinished,
            json!({"mode": "hourly", "period": "2026-09-29T14", "outcome": "skipped"}),
        );
        let closed = of_kind(&queue, SESSION_CLOSED);
        assert_eq!(closed.len(), 2);
        assert_eq!(closed[1].payload["session_id"], "s-d");
        assert_eq!(closed[1].payload["reason"], INFERRED);
        assert_eq!(closed[1].payload["model"], "claude-sonnet-5");
        assert_eq!(closed[1].payload["effort"], "medium");
        assert_eq!(
            rfc3339_millis(&closed[1].created_at),
            Some(daily_start + 2000)
        );
        // The observer's span, older than both, is still open: only its own
        // finish closes it.
        record(EventKind::ObserveFinished, json!({"dir": "/obs"}));
        let closed = of_kind(&queue, SESSION_CLOSED);
        assert_eq!(closed.len(), 3);
        assert_eq!(closed[2].payload["kind"], OBSERVER);
        assert_eq!(closed[2].payload["reason"], JOB_FINISHED);
    }

    #[test]
    fn handoff_closes_reviews_with_transcript_models_and_no_open_stats() {
        use crate::application::{GoalReviewStore, PlanReviewStore};
        use crate::domain::{
            EventId,
            stats::sessions::{SessionWindow, by_kind, spans},
        };
        let dir = tempfile::tempdir().unwrap();
        let (mut queue, task_id, _) = run_queue(dir.path());
        let goal = queue
            .add_goal(crate::domain::NewGoal {
                title: "g".into(),
                description: String::new(),
                acceptance: String::new(),
                constraints: String::new(),
                doc: None,
                draft: false,
            })
            .unwrap()
            .id();
        queue.set_goal(task_id, Some(goal)).unwrap();
        assert_eq!(goal.as_i64(), 1);
        queue
            .conn
            .execute_batch(
                "INSERT INTO proposals(id,status,owner_origin,submitted_at,created_at,updated_at)
             VALUES (5,'submitted','person','t','t','t');
             INSERT INTO plan_reviews(id,proposal_id,attempt,supervisor_token,started_at)
             VALUES (7,5,1,'handoff',0);
             INSERT INTO goal_reviews(id,goal_id,attempt,supervisor_token,fingerprint,started_at)
             VALUES (8,1,1,'handoff','tasks',0);",
            )
            .unwrap();
        for (kind, session, payload) in [
            (
                EventKind::PlanReviewStarted,
                "plan",
                json!({"plan_review_id": 7, "proposal_id": 5}),
            ),
            (
                EventKind::GoalReviewStarted,
                "goal",
                json!({"goal_review_id": 8, "goal_id": 1}),
            ),
        ] {
            let mut payload = payload;
            payload["session_id"] = json!(session);
            payload["cwd"] = json!("/repo");
            payload["attempt"] = json!(1);
            if kind == EventKind::GoalReviewStarted {
                crate::infrastructure::sqlite::goal_event(&queue.conn, goal, kind, payload)
                    .unwrap();
            } else {
                event(&queue.conn, task_id, None, kind, payload).unwrap();
            }
        }
        let start = retime(&queue.conn, 0, 100);
        let project = dir.path().join("config/projects/-repo");
        std::fs::create_dir_all(&project).unwrap();
        for session in ["plan", "goal"] {
            let lines = [
                json!({"type": "user", "timestamp": millis_text(start + 1000),
                       "sessionId": session, "message": {"content": "go"}}),
                json!({"type": "assistant", "timestamp": millis_text(start + 2000),
                       "sessionId": session, "version": "2.1.283", "effort": "high",
                       "message": {"id": session, "model": "claude-opus-5-5", "content": [],
                                   "usage": {"input_tokens": 1, "output_tokens": 1}}}),
            ]
            .map(|line| line.to_string());
            std::fs::write(project.join(format!("{session}.jsonl")), lines.join("\n")).unwrap();
        }
        let token = LeaseToken::new("handoff");
        queue.interrupt_plan_reviews_for_handoff(&token).unwrap();
        queue.interrupt_goal_reviews_for_handoff(&token).unwrap();
        let closed = of_kind(&queue, SESSION_CLOSED);
        assert_eq!(closed.len(), 2);
        for close in closed {
            assert_eq!(close.payload["reason"], JOB_FINISHED);
            assert_eq!(close.payload["active"], "recorded");
            assert_eq!(close.payload["model"], "claude-opus-5-5");
            assert_eq!(close.payload["effort"], "high");
        }
        let events: Vec<RunEvent> = queue
            .conn
            .prepare("SELECT * FROM run_events ORDER BY id")
            .unwrap()
            .query_map([], event_row)
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let last = events.last().unwrap();
        let sessions = by_kind(
            &spans(&events),
            &events,
            SessionWindow {
                after: EventId::new(0),
                upto: last.id,
            },
            rfc3339_millis(&last.created_at).unwrap(),
            |_| true,
        );
        for kind in [GOAL_REVIEW, PLAN_REVIEW] {
            assert_eq!(sessions.by_kind[kind].count, 1);
            assert_eq!(sessions.by_kind[kind].open_now, 0);
        }
    }

    /// A plan or goal review whose supervisor went away closes as inferred
    /// in a write transaction with the analysis made before it: its active
    /// time and turns are its transcript's, and nothing is analysed under
    /// the lock (task 1334).
    #[test]
    fn an_inferred_review_close_takes_the_analysis_made_before_its_write_transaction() {
        use crate::application::TaskStore;
        use crate::infrastructure::sqlite::goal_event;
        let dir = tempfile::tempdir().unwrap();
        let (mut queue, task_id, _) = run_queue(dir.path());
        let goal = queue
            .add_goal(crate::domain::NewGoal {
                title: "g".into(),
                description: String::new(),
                acceptance: String::new(),
                constraints: String::new(),
                doc: None,
                draft: false,
            })
            .unwrap()
            .id();
        queue.set_goal(task_id, Some(goal)).unwrap();
        let conn = &queue.conn;
        event(
            conn,
            task_id,
            None,
            EventKind::PlanReviewStarted,
            json!({"proposal_id": 1, "plan_review_id": 7, "attempt": 1,
                   "session_id": "s-plan", "cwd": "/plan"}),
        )
        .unwrap();
        goal_event(
            conn,
            goal,
            EventKind::GoalReviewStarted,
            json!({"goal_review_id": 8, "attempt": 1, "session_id": "s-goal", "cwd": "/goal"}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        transcript_in(dir.path(), "/plan", "s-plan", start);
        transcript_in(dir.path(), "/goal", "s-goal", start);
        {
            let _read = read_before(conn, Closing::PlanReviews(None, true)).unwrap();
            let before = analysed();
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
            close_plan_review(&tx, 7, true).unwrap();
            tx.commit().unwrap();
            assert_eq!(analysed(), before);
        }
        {
            let _read = read_before(conn, Closing::GoalReviews(None, true)).unwrap();
            let before = analysed();
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
            close_goal_review(&tx, 8, true).unwrap();
            tx.commit().unwrap();
            assert_eq!(analysed(), before);
        }
        let closed = of_kind(&queue, SESSION_CLOSED);
        assert_eq!(closed.len(), 2);
        for (close, kind, session) in [
            (&closed[0], PLAN_REVIEW, "s-plan"),
            (&closed[1], GOAL_REVIEW, "s-goal"),
        ] {
            assert_eq!(close.payload["kind"], kind);
            assert_eq!(close.payload["session_id"], session);
            assert_eq!(close.payload["reason"], INFERRED);
            assert_eq!(close.payload["active"], "recorded");
            assert_eq!(close.payload["active_secs"], 30);
        }
        assert_eq!(of_kind(&queue, SESSION_TURNS).len(), 2);
        assert!(READ_BEFORE.with_borrow(Vec::is_empty));
    }

    /// Write the transcript of `session` of a span in `cwd`: two turns,
    /// 5–25 s and 40–50 s after `base`.
    fn transcript_in(dir: &std::path::Path, cwd: &str, session: &str, base: i64) {
        let project = dir.join("config/projects").join(cwd.replace('/', "-"));
        std::fs::create_dir_all(&project).unwrap();
        let line = |kind: &str, secs: i64, content: Value| {
            json!({"type": kind, "timestamp": millis_text(base + secs * 1000),
                   "sessionId": session, "version": "2.1.283", "message": {"content": content}})
            .to_string()
        };
        let lines = [
            line("user", 5, json!("go")),
            line("assistant", 25, json!([{"type": "text"}])),
            line("user", 40, json!("go")),
            line("assistant", 50, json!([{"type": "text"}])),
        ];
        std::fs::write(project.join(format!("{session}.jsonl")), lines.join("\n")).unwrap();
    }

    /// The writes that close spans in a write transaction — a run's event,
    /// a failed review's close, an event of the queue and a plan review's
    /// close — read the transcripts before it begins and none under its
    /// lock (task 543), and the spans close with the same turns and active
    /// time as before. A span whose transcript was not read before closes
    /// without its active time, and its event is written all the same.
    #[test]
    fn spans_closed_in_a_write_transaction_take_the_transcripts_read_before_it() {
        use crate::application::SessionRegistry;
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let conn = &queue.conn;
        // Whether it is dagq's source is judged before the transaction too
        // (ADR-t614-1): the worker's work keeps the cargo-only counts.
        bind(conn, dir.path(), "dagq");
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        SqliteQueue::record_runtime_event(
            &queue,
            &run,
            EventKind::ReviewStarted,
            json!({"attempt": 1, "session_id": "s-review"}),
        )
        .unwrap();
        queue
            .record_queue_event(
                EventKind::ObserveStarted,
                json!({"mode": "hourly", "dir": "/obs", "session_id": "s-obs"}),
            )
            .unwrap();
        event(
            conn,
            task_id,
            None,
            EventKind::PlanReviewStarted,
            json!({"proposal_id": 1, "plan_review_id": 7, "attempt": 1,
                   "session_id": "s-plan", "cwd": "/plan"}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        transcript_in(dir.path(), "/wt", RUN, start);
        transcript_in(dir.path(), "/wt", "s-review", start);
        transcript_in(dir.path(), "/obs", "s-obs", start);
        transcript_in(dir.path(), "/plan", "s-plan", start);
        READS.set(0);

        SqliteQueue::record_runtime_event(
            &queue,
            &run,
            EventKind::SessionExited,
            json!({"exit_code": 0}),
        )
        .unwrap();
        assert_eq!(
            SessionRegistry::close_review_session(&queue, &run, None).unwrap(),
            1
        );
        queue
            .record_queue_event(EventKind::ObserveFinished, json!({"dir": "/obs"}))
            .unwrap();
        {
            let _read = read_before(conn, Closing::PlanReviews(Some(7), false)).unwrap();
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
            close_plan_review(&tx, 7, false).unwrap();
            tx.commit().unwrap();
        }
        // Each transcript was read once, before its transaction: `read`
        // reads nothing inside one.
        assert_eq!(READS.get(), 4);
        let closed = of_kind(&queue, SESSION_CLOSED);
        let described: Vec<(&str, &str, &Value, &Value)> = closed
            .iter()
            .map(|e| {
                (
                    e.payload["kind"].as_str().unwrap(),
                    e.payload["reason"].as_str().unwrap(),
                    &e.payload["active"],
                    &e.payload["active_secs"],
                )
            })
            .collect();
        let recorded = json!("recorded");
        let secs = json!(30);
        assert_eq!(
            described,
            vec![
                ("worker", "exited", &recorded, &secs),
                ("review", "job_finished", &recorded, &secs),
                ("observer", "job_finished", &recorded, &secs),
                ("plan_review", "job_finished", &recorded, &secs),
            ]
        );
        assert_eq!(of_kind(&queue, SESSION_TURNS).len(), 4);
        assert_eq!(closed[0].payload["work"]["verification_repeats"], 0);
        // Nothing is left for a later write to take.
        assert!(READ_BEFORE.with_borrow(Vec::is_empty));
        assert!(SOURCE_BEFORE.with_borrow(Vec::is_empty));

        // A span closed in a transaction nothing read before: its
        // transcript is not read, and the event is written.
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::ResumeStarted,
            json!({}),
        )
        .unwrap();
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        event(
            &tx,
            task_id,
            Some(&run),
            EventKind::SessionExited,
            json!({"exit_code": 0}),
        )
        .unwrap();
        tx.commit().unwrap();
        assert_eq!(READS.get(), 4);
        let closed = of_kind(&queue, SESSION_CLOSED);
        let last = closed.last().unwrap();
        assert_eq!(last.payload["kind"], "resume");
        assert_eq!(last.payload["active"], "unavailable");
        assert_eq!(
            last.payload["active_unavailable"],
            TRANSCRIPT_NOT_READ_BEFORE
        );
        assert_eq!(of_kind(&queue, "session_exited").len(), 2);
        // Called inside a transaction, `read_before` reads nothing, not
        // even the transcript of a span open.
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate).unwrap();
        let read = read_before(&tx, Closing::Run(&run, &["run_recovered"])).unwrap();
        assert!(read.spans.is_empty());
        drop(read);
        tx.rollback().unwrap();
        assert_eq!(READS.get(), 4);
    }

    /// A `SessionStart` / `SessionEnd` the hook reported for `session` of
    /// `kind` in `workspace`, with its transcript under `dir`.
    fn hook(
        dir: &std::path::Path,
        start: Option<&str>,
        kind: &'static str,
        session: &str,
        workspace: &str,
    ) -> SessionHook {
        use crate::domain::sessions::HookEvent;
        SessionHook {
            event: match start {
                Some(source) => HookEvent::Start {
                    source: source.into(),
                },
                None => HookEvent::End {
                    reason: "prompt_input_exit".into(),
                },
            },
            kind,
            session_id: session.into(),
            transcript_path: Some(dir.join(format!("{session}.jsonl")).display().to_string()),
            cwd: Some("/repo".into()),
            workspace_id: Some(workspace.into()),
            planner_id: None,
            launch: None,
        }
    }

    /// Write the transcript of the hook's `session` under `dir`: `turns` as
    /// (input, last output) offsets in seconds from `base`, then an input
    /// not answered yet at `pending`.
    fn hook_transcript(
        dir: &std::path::Path,
        session: &str,
        base: i64,
        turns: &[(i64, i64)],
        pending: i64,
    ) {
        let line = |kind: &str, secs: i64| {
            json!({"type": kind, "timestamp": millis_text(base + secs * 1000),
                   "sessionId": session, "version": "2.1.283",
                   "message": {"content": if kind == "user" { json!("go") } else { json!([{"type": "text"}]) }}})
            .to_string()
        };
        let mut lines = Vec::new();
        for &(input, output) in turns {
            lines.push(line("user", input));
            lines.push(line("assistant", output));
        }
        lines.push(line("user", pending));
        std::fs::write(dir.join(format!("{session}.jsonl")), lines.join("\n")).unwrap();
    }

    /// The inbox's span opens at its start, records its finished turns
    /// while open, goes on through a compaction, and is replaced at a
    /// /clear (a new session id in its workspace) with its active time; its
    /// late `SessionEnd` and a second one change nothing (ADR-0048
    /// decision 6). `stats` counts the spans with their open and active
    /// time in its window.
    #[test]
    fn the_hook_records_the_inbox_span_once_across_compaction_and_clear() {
        use crate::domain::{
            sessions::INBOX,
            stats::sessions::{SessionWindow, by_kind, spans as stat_spans},
        };
        let dir = tempfile::tempdir().unwrap();
        ClaudeTranscripts::use_config_dir_in_test(&dir.path().join("config"));
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let conn = &queue.conn;
        let started =
            record_hook(conn, &hook(dir.path(), Some("startup"), INBOX, "s-1", "W")).unwrap();
        assert_eq!(started["closed"], json!([]));
        let opened = started["opened"].as_i64().unwrap();
        let start = retime(conn, 0, 100);
        hook_transcript(dir.path(), "s-1", start, &[(5, 25), (40, 50)], 60);
        // Finished turns of the open span, recorded as they come.
        assert_eq!(record_open_turns(conn).unwrap(), 1);
        assert_eq!(of_kind(&queue, SESSION_TURNS)[0].payload["kind"], "inbox");
        // A compaction keeps the session id: the span goes on.
        let compacted =
            record_hook(conn, &hook(dir.path(), Some("compact"), INBOX, "s-1", "W")).unwrap();
        assert_eq!(compacted["opened"], Value::Null);
        assert_eq!(compacted["closed"], json!([]));
        // A /clear whose SessionStart comes first: the new session id
        // closes the span of its workspace as the next span.
        READS.set(0);
        let cleared =
            record_hook(conn, &hook(dir.path(), Some("clear"), INBOX, "s-2", "W")).unwrap();
        assert_eq!(READS.get(), 0);
        assert_eq!(cleared["closed"], json!([opened]));
        let closed = &of_kind(&queue, SESSION_CLOSED)[0];
        assert_eq!(closed.payload["reason"], crate::domain::sessions::NEXT_SPAN);
        assert_eq!(closed.payload["active_unavailable"], "hook_intake_pending");
        assert_eq!(closed.task_id, None);
        assert_eq!(record_open_turns(conn).unwrap(), 1);
        assert_eq!(record_open_turns(conn).unwrap(), 0);
        // Its SessionEnd, late, and a second one: closed once.
        for _ in 0..2 {
            let ended = record_hook(conn, &hook(dir.path(), None, INBOX, "s-1", "W")).unwrap();
            assert_eq!(ended["closed"], json!([]));
        }
        assert_eq!(of_kind(&queue, SESSION_CLOSED).len(), 1);
        assert_eq!(of_kind(&queue, SESSION_OPENED).len(), 2);
        // The new session's span is still open.
        let events: Vec<RunEvent> = queue
            .conn
            .prepare("SELECT * FROM run_events ORDER BY id")
            .unwrap()
            .query_map([], event_row)
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let upto = events.last().unwrap().id;
        let spans = stat_spans(&events);
        let end = rfc3339_millis(&events.last().unwrap().created_at).unwrap();
        let sessions = by_kind(
            &spans,
            &events,
            SessionWindow {
                after: EventId::new(0),
                upto,
            },
            end,
            |_| true,
        );
        let inbox = &sessions.by_kind[INBOX];
        assert_eq!(inbox.count, 2);
        assert_eq!(inbox.open_now, 1);
        assert_eq!(inbox.active.summary.total, 30);
        assert!(inbox.open.summary.total >= 100, "{inbox:?}");
        assert_eq!(sessions.by_kind["planner"].count, 0);
    }

    #[test]
    fn hook_close_defers_the_last_turn_and_measurements_once() {
        use crate::domain::{sessions::INBOX, stats::sessions::spans};
        let dir = tempfile::tempdir().unwrap();
        ClaudeTranscripts::use_config_dir_in_test(&dir.path().join("config"));
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let conn = &queue.conn;
        let start_hook = hook(dir.path(), Some("startup"), INBOX, "s", "W");
        record_hook(conn, &start_hook).unwrap();
        let start = retime(conn, 0, 100);
        hook_transcript(dir.path(), "s", start, &[(5, 25), (40, 50)], 60);
        assert_eq!(record_open_turns(conn).unwrap(), 1);
        // Append the final assistant without another input: open intake
        // must leave this turn for the close, clipping it at that boundary.
        let path = dir.path().join("s.jsonl");
        let mut text = std::fs::read_to_string(&path).unwrap();
        let assistant = |offset| {
            json!({"type":"assistant", "sessionId":"s",
            "version":"2.1.283", "timestamp":millis_text(start + offset),
            "message":{"id":"m-final", "model":"claude-sonnet-4-6",
                "content":[{"type":"text","text":"done"}],
                "usage":{"input_tokens":10,"output_tokens":5}}})
        };
        text.push_str(&format!("\n{}\n{}", assistant(80_000), assistant(150_000)));
        std::fs::write(&path, text).unwrap();
        READS.set(0);
        record_hook(conn, &hook(dir.path(), None, INBOX, "s", "W")).unwrap();
        assert_eq!(READS.get(), 0);
        let closed = of_kind(&queue, SESSION_CLOSED)[0].clone();
        let end = start + 100_000;
        conn.execute(
            "UPDATE run_events SET created_at=?1 WHERE id=?2",
            params![millis_text(end), closed.id],
        )
        .unwrap();
        assert_eq!(record_open_turns(conn).unwrap(), 1);
        assert_eq!(READS.get(), 1);
        assert_eq!(record_open_turns(conn).unwrap(), 0);
        assert_eq!(READS.get(), 1);
        let opened = of_kind(&queue, SESSION_OPENED)[0].id;
        let recorded = recorded_turns(conn, opened).unwrap();
        assert_eq!(
            recorded.iter().map(|turn| turn.millis()).sum::<i64>(),
            70_000
        );
        assert_eq!(recorded.last().unwrap().end, end);
        let events: Vec<RunEvent> = conn
            .prepare("SELECT * FROM run_events ORDER BY id")
            .unwrap()
            .query_map([], event_row)
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let spans = spans(&events);
        assert_eq!(spans[0].active, Some(70));
        assert!(spans[0].tokens.is_some());
        assert_eq!(spans[0].model.as_deref(), Some("claude-sonnet-4-6 unknown"));
        assert!(!spans[0].active_unavailable);
        assert_eq!(spans[0].end, Some(end));
        // Finalized even if intake is called again or SessionEnd is duplicated.
        record_hook(conn, &hook(dir.path(), None, INBOX, "s", "W")).unwrap();
        assert_eq!(of_kind(&queue, SESSION_CLOSED).len(), 1);
        assert_eq!(
            of_kind(&queue, SESSION_TURNS)
                .iter()
                .filter(|e| e.payload["final"] == true)
                .count(),
            1
        );
    }

    #[test]
    fn missing_hook_transcript_stays_closed_and_is_finalized_once() {
        use crate::domain::sessions::INBOX;
        let dir = tempfile::tempdir().unwrap();
        ClaudeTranscripts::use_config_dir_in_test(&dir.path().join("config"));
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let conn = &queue.conn;
        record_hook(
            conn,
            &hook(dir.path(), Some("startup"), INBOX, "missing", "W"),
        )
        .unwrap();
        READS.set(0);
        record_hook(conn, &hook(dir.path(), None, INBOX, "missing", "W")).unwrap();
        assert_eq!(READS.get(), 0);
        assert!(open_hook_spans(conn).unwrap().is_empty());
        assert_eq!(record_open_turns(conn).unwrap(), 1);
        let final_turns = of_kind(&queue, SESSION_TURNS);
        assert_eq!(final_turns.len(), 1);
        assert_eq!(final_turns[0].payload["active"], "unavailable");
        assert_eq!(
            final_turns[0].payload["active_unavailable"],
            "transcript_missing"
        );
        assert_eq!(record_open_turns(conn).unwrap(), 0);
        assert_eq!(READS.get(), 1);
        assert!(open_hook_spans(conn).unwrap().is_empty());
    }

    #[test]
    fn next_start_without_workspace_infers_only_the_same_kind() {
        use crate::domain::sessions::{INBOX, PLANNER, RUNTIME_PLANNER};
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let conn = &queue.conn;
        let no_workspace = |kind, session| SessionHook {
            workspace_id: None,
            ..hook(dir.path(), Some("startup"), kind, session, "unused")
        };
        for (kind, session) in [(INBOX, "i"), (PLANNER, "p"), (RUNTIME_PLANNER, "r")] {
            record_hook(conn, &no_workspace(kind, session)).unwrap();
        }
        record_hook(
            conn,
            &hook(dir.path(), Some("startup"), INBOX, "with-w", "W"),
        )
        .unwrap();
        READS.set(0);
        record_hook(conn, &no_workspace(INBOX, "i")).unwrap();
        assert!(of_kind(&queue, SESSION_CLOSED).is_empty());
        record_hook(conn, &no_workspace(INBOX, "i-next")).unwrap();
        assert_eq!(READS.get(), 0);
        let closed = of_kind(&queue, SESSION_CLOSED);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].payload["session_id"], "i");
        assert_eq!(closed[0].payload["reason"], INFERRED);
        assert_eq!(open_hook_spans(conn).unwrap().len(), 4);
    }

    /// A runtime planner's span is on the first task of its proposal, with
    /// the proposal and its goals, as a plan review's; a person's planner
    /// and one without a proposal are on no task.
    #[test]
    fn a_runtime_planner_span_is_on_its_proposal() {
        use crate::domain::{
            NewGoal,
            sessions::{PLANNER, RUNTIME_PLANNER},
        };
        let dir = tempfile::tempdir().unwrap();
        ClaudeTranscripts::use_config_dir_in_test(&dir.path().join("config"));
        let mut queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let goal = queue
            .add_goal(NewGoal {
                title: "g".into(),
                description: String::new(),
                acceptance: String::new(),
                constraints: String::new(),
                doc: None,
                draft: false,
            })
            .unwrap();
        let first = task(&mut queue);
        let second = task(&mut queue);
        let conn = &queue.conn;
        conn.execute_batch(
            "INSERT INTO proposals(id,status,owner_origin,submitted_at,created_at,updated_at)
               VALUES (5,'revising','runtime','t','t','t');
             INSERT INTO planners(id,origin,proposal_id,created_at) VALUES (3,'runtime',5,0);
             INSERT INTO planners(id,origin,created_at) VALUES (4,'runtime',0);",
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET proposal_id=5, goal_id=?1 WHERE id IN (?2,?3)",
            params![goal.id(), first, second],
        )
        .unwrap();
        let planner = |kind, session: &str, planner| SessionHook {
            planner_id: planner,
            ..hook(dir.path(), Some("startup"), kind, session, session)
        };
        record_hook(conn, &planner(RUNTIME_PLANNER, "p-3", Some(3))).unwrap();
        record_hook(conn, &planner(RUNTIME_PLANNER, "p-4", Some(4))).unwrap();
        record_hook(conn, &planner(PLANNER, "p-5", Some(3))).unwrap();
        let opened = of_kind(&queue, SESSION_OPENED);
        assert_eq!(opened[0].task_id, Some(first));
        assert_eq!(opened[0].payload["proposal_id"], 5);
        assert_eq!(opened[0].payload["goal_ids"], json!([goal.id()]));
        assert_eq!(opened[0].payload["planner_id"], 3);
        for other in &opened[1..] {
            assert_eq!(other.task_id, None);
            assert_eq!(other.payload.get("proposal_id"), None);
        }
        // Its end closes it on the same task.
        record_hook(conn, &hook(dir.path(), None, RUNTIME_PLANNER, "p-3", "p-3")).unwrap();
        let closed = &of_kind(&queue, SESSION_CLOSED)[0];
        assert_eq!(closed.task_id, Some(first));
        assert_eq!(closed.payload["reason"], crate::domain::sessions::EXITED);
    }

    /// The spans of the workspaces the supervisor found gone close as
    /// inferred, at their transcript's last record; the others stay open.
    #[test]
    fn spans_of_gone_workspaces_close_as_inferred() {
        use crate::domain::sessions::{INBOX, PLANNER};
        let dir = tempfile::tempdir().unwrap();
        ClaudeTranscripts::use_config_dir_in_test(&dir.path().join("config"));
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let conn = &queue.conn;
        record_hook(
            conn,
            &hook(dir.path(), Some("startup"), INBOX, "s-1", "W-1"),
        )
        .unwrap();
        record_hook(
            conn,
            &hook(dir.path(), Some("startup"), PLANNER, "s-2", "W-2"),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        hook_transcript(dir.path(), "s-2", start, &[(5, 25)], 30);
        let workspaces = hook_workspaces(conn).unwrap();
        assert_eq!(
            workspaces
                .iter()
                .map(|(_, w)| w.as_str())
                .collect::<Vec<_>>(),
            ["W-1", "W-2"]
        );
        let gone = [workspaces[1].0];
        assert_eq!(close_gone_hook_spans(conn, &gone).unwrap(), 1);
        assert_eq!(close_gone_hook_spans(conn, &gone).unwrap(), 0);
        assert_eq!(close_gone_hook_spans(conn, &[]).unwrap(), 0);
        let closed = &of_kind(&queue, SESSION_CLOSED)[0];
        assert_eq!(closed.payload["kind"], "planner");
        assert_eq!(closed.payload["reason"], INFERRED);
        assert_eq!(closed.payload["active_secs"], 20);
        assert_eq!(closed.created_at, millis_text(start + 30_000));
        assert_eq!(hook_workspaces(conn).unwrap().len(), 1);
    }

    /// An event about a run that names no task is refused before the write
    /// (ADR-t876-1: the rule the `run_events` CHECK held).
    #[test]
    fn an_event_about_a_run_without_its_task_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let queue = SqliteQueue::init(dir.path().join("q.db")).unwrap();
        let run = RunId::new("run-1").unwrap();
        let error = insert_at(
            &queue.conn,
            None,
            Some(&run),
            EventKind::SupervisorStopped,
            &json!({}),
            "2026-09-28T00:00:00.000Z",
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "run run-1 is written without its task");
        let rows: i64 = queue
            .conn
            .query_row("SELECT count(*) FROM run_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    /// What another writer does between `read_before` and the write
    /// transaction that closes the worker's span.
    #[derive(Clone, Copy, PartialEq)]
    enum Between {
        Nothing,
        /// Records the span's finished turns.
        RecordsTurns,
        /// Closes the span (its session's exit).
        Closes,
        /// Nothing, and the write is rolled back.
        RolledBack,
        /// Nothing, the write is rolled back, and then (before the
        /// `read_before` of the write is dropped) another writer closes
        /// the span, its close taking the id the rolled back one had.
        RolledBackThenCloses,
        /// Nothing, and after the commit another transaction on the same
        /// connection, writing nothing, is rolled back before the
        /// `read_before` of the write is dropped.
        CommittedThenAnotherRolledBack,
        /// Changes the task's verification.
        EditsVerification,
    }

    /// A queue whose run's worker span is open, with a transcript of two
    /// turns and a command in each, and the run's directory for its
    /// `worktime.jsonl`; and the time the span opened.
    fn closing_worker() -> (
        tempfile::TempDir,
        SqliteQueue,
        TaskId,
        RunId,
        std::path::PathBuf,
        i64,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let run_dir = dir.path().join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        let conn = &queue.conn;
        bind(conn, dir.path(), "dagq");
        conn.execute(
            "UPDATE task_runs SET run_dir=?1",
            [run_dir.to_str().unwrap()],
        )
        .unwrap();
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        let project = dir.path().join("config/projects/-wt");
        std::fs::create_dir_all(&project).unwrap();
        let line = |kind: &str, secs: i64, content: Value| {
            json!({"type": kind, "timestamp": millis_text(start + secs * 1000),
                   "sessionId": RUN, "version": "2.1.283",
                   "message": {"content": content, "id": format!("m{secs}"),
                               "model": "claude-opus-4-1",
                               "usage": {"input_tokens": 10, "output_tokens": secs}}})
            .to_string()
        };
        let bash = |id: &str, command: &str| json!([{"type": "tool_use", "id": id, "name": "Bash", "input": {"command": command}}]);
        let result = |id: &str| json!([{"type": "tool_result", "tool_use_id": id, "is_error": false, "content": "ok"}]);
        // Two turns, the first finished (the next input came); a command
        // in each.
        let lines = [
            line("user", 1, json!("go")),
            line("assistant", 5, bash("a", "cargo test --locked")),
            line("user", 15, result("a")),
            line("assistant", 20, json!([{"type": "text"}])),
            line("user", 30, json!("more")),
            line("assistant", 32, bash("b", "cargo build")),
            line("user", 40, result("b")),
            line("assistant", 45, json!([{"type": "text"}])),
        ];
        std::fs::write(project.join(format!("{RUN}.jsonl")), lines.join("\n")).unwrap();
        (dir, queue, task_id, run, run_dir, start)
    }

    /// The worker's span closed by a `session_exited` in a write
    /// transaction after `read_before`, with `between` done by another
    /// process (a thread on its own connection) in between (or after the
    /// transaction, for [`Between::RolledBackThenCloses`]): the closes,
    /// the turns recorded, the lines of `worktime.jsonl`, and the
    /// transcripts analysed under the write lock.
    fn close_after(between: Between) -> (Vec<Value>, Vec<Value>, Vec<Value>, usize) {
        let (dir, queue, task_id, run, run_dir, start) = closing_worker();
        let conn = &queue.conn;
        let read = read_before(conn, Closing::Run(&run, &["session_exited"])).unwrap();
        let before = analysed();
        if between == Between::EditsVerification {
            conn.execute(
                "UPDATE tasks SET verification_commands=?1",
                [r#"["cargo test --locked"]"#],
            )
            .unwrap();
        }
        let other = |between: Between| {
            let db = dir.path().join("q.db");
            let config = dir.path().join("config");
            let run = run.clone();
            std::thread::spawn(move || {
                ClaudeTranscripts::use_config_dir_in_test(&config);
                let other = SqliteQueue::open(&db).unwrap();
                if between == Between::RecordsTurns {
                    assert_eq!(record_open_turns(&other.conn).unwrap(), 1);
                } else {
                    event(
                        &other.conn,
                        task_id,
                        Some(&run),
                        EventKind::SessionExited,
                        json!({"exit_code": 0}),
                    )
                    .unwrap();
                }
            })
            .join()
            .unwrap();
        };
        if matches!(between, Between::RecordsTurns | Between::Closes) {
            other(between);
        }
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::SessionExited,
            json!({"exit_code": 0}),
        )
        .unwrap();
        // The lines are written after the commit, not under the lock.
        assert!(!run_dir.join(WORKTIME_FILE).exists() || between == Between::Closes);
        let under_lock = analysed() - before;
        let closed_id = || -> Option<i64> {
            conn.query_row(
                &format!("SELECT max(id) FROM run_events WHERE kind='{SESSION_CLOSED}'"),
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        let rolled_back = closed_id();
        assert!(rolled_back.is_some() || between == Between::Closes);
        if matches!(between, Between::RolledBack | Between::RolledBackThenCloses) {
            conn.execute_batch("ROLLBACK").unwrap();
        } else {
            conn.execute_batch("COMMIT").unwrap();
        }
        if between == Between::RolledBackThenCloses {
            other(Between::Closes);
            // The other close is known apart from the one rolled back by
            // its transaction, not by its id: SQLite gave it the same one.
            assert_eq!(closed_id(), rolled_back);
        }
        if between == Between::CommittedThenAnotherRolledBack {
            conn.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        }
        drop(read);
        assert!(WORKTIME_AFTER.with_borrow(Vec::is_empty));
        let payloads = |kind: &str| -> Vec<Value> {
            of_kind(&queue, kind)
                .into_iter()
                .map(|event| event.payload)
                .collect()
        };
        let closed: Vec<Value> = payloads(SESSION_CLOSED)
            .into_iter()
            .map(|mut closed| {
                // The close's time is the run's, not the transcript's.
                let work = closed["work"].as_object_mut().unwrap();
                work.remove("total_secs");
                work["secs"].as_object_mut().unwrap().remove("idle");
                closed
            })
            .collect();
        let turns: Vec<Value> = payloads(SESSION_TURNS)
            .iter()
            .flat_map(|turns| turns["turns"].as_array().unwrap().clone())
            .collect();
        let worktime: Vec<Value> = std::fs::read_to_string(run_dir.join(WORKTIME_FILE))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let mut found = (closed, turns, worktime, under_lock);
        for value in found.0.iter_mut().chain(&mut found.1).chain(&mut found.2) {
            offsets(value, start);
        }
        found
    }

    /// Make the times in `value` milliseconds after `start`, to compare
    /// closes of queues made at different times.
    fn offsets(value: &mut Value, start: i64) {
        match value {
            Value::String(text) => {
                if let Some(at) = rfc3339_millis(text) {
                    *value = json!(at - start);
                }
            }
            Value::Array(values) => values.iter_mut().for_each(|value| offsets(value, start)),
            Value::Object(values) => values.values_mut().for_each(|value| offsets(value, start)),
            _ => {}
        }
    }

    /// The analysis made before the write transaction is the close's
    /// (task 1334): nothing is analysed under the lock, the
    /// `worktime.jsonl` lines are written once after the commit, and none
    /// of a close rolled back. Another writer that recorded the span's
    /// turns, or closed it, in between leaves the same close, turns and
    /// lines: the turns it recorded are not recorded again, and the span
    /// is not closed twice.
    #[test]
    fn a_close_takes_the_analysis_made_before_its_write_transaction() {
        let (closed, turns, worktime, under_lock) = close_after(Between::Nothing);
        assert_eq!(under_lock, 0);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0]["active"], "recorded");
        assert_eq!(closed[0]["active_secs"], 19 + 15);
        assert_eq!(closed[0]["tokens"]["output"], 5 + 20 + 32 + 45);
        assert_eq!(closed[0]["model"], "claude-opus-4-1");
        assert_eq!(closed[0]["work"]["secs"]["test"], 10);
        assert_eq!(closed[0]["work"]["secs"]["build"], 8);
        assert_eq!(turns.len(), 2);
        let commands: Vec<&Value> = worktime.iter().map(|line| &line["command"]).collect();
        assert_eq!(
            commands,
            [&json!("cargo test --locked"), &json!("cargo build")]
        );

        let recorded = close_after(Between::RecordsTurns);
        assert_eq!(recorded.3, 0);
        assert_eq!(
            (&recorded.0, &recorded.1, &recorded.2),
            (&closed, &turns, &worktime)
        );
        let other = close_after(Between::Closes);
        assert_eq!((&other.0, &other.1, &other.2), (&closed, &turns, &worktime));
        // The verification read under the lock is the one counted.
        let (edited, edited_turns, edited_worktime, under_lock) =
            close_after(Between::EditsVerification);
        assert_eq!(under_lock, 0);
        assert_eq!(closed[0]["work"]["verification_repeats"], 0);
        assert_eq!(edited[0]["work"]["verification_repeats"], 1);
        let mut expected = closed.clone();
        expected[0]["work"]["verification_repeats"] = json!(1);
        assert_eq!(
            (&edited, &edited_turns, &edited_worktime),
            (&expected, &turns, &worktime)
        );

        let (rolled_back, rolled_back_turns, rolled_back_worktime, _) =
            close_after(Between::RolledBack);
        assert!(rolled_back.is_empty());
        assert!(rolled_back_turns.is_empty());
        assert!(rolled_back_worktime.is_empty());
        // Another writer's close after the rollback writes its lines once:
        // not again for the close rolled back.
        let after = close_after(Between::RolledBackThenCloses);
        assert_eq!((&after.0, &after.1, &after.2), (&closed, &turns, &worktime));
        // A rollback of a later transaction leaves a committed close's
        // lines.
        let later = close_after(Between::CommittedThenAnotherRolledBack);
        assert_eq!((&later.0, &later.1, &later.2), (&closed, &turns, &worktime));
    }

    /// A runtime event whose COMMIT fails (task 1500) is rolled back: the
    /// connection is out of the transaction, the close it made is gone and
    /// its `worktime.jsonl` lines are not written, and the next close of
    /// the span writes them once.
    #[test]
    fn a_runtime_event_whose_commit_fails_is_rolled_back_without_its_worktime() {
        let (_dir, queue, _, run, run_dir, _) = closing_worker();
        let conn = &queue.conn;
        // A foreign key checked at COMMIT fails it and, unlike a failed
        // statement, leaves the transaction open.
        conn.execute_batch(
            "CREATE TABLE commit_parent(id INTEGER PRIMARY KEY);
             CREATE TABLE commit_child(parent INTEGER
                 REFERENCES commit_parent(id) DEFERRABLE INITIALLY DEFERRED);
             CREATE TRIGGER fail_commit AFTER INSERT ON run_events
             BEGIN INSERT INTO commit_child VALUES (1); END;",
        )
        .unwrap();
        let exited =
            || queue.record_runtime_event(&run, EventKind::SessionExited, json!({"exit_code": 0}));
        let error = exited().unwrap_err();
        assert!(error.to_string().contains("FOREIGN KEY"), "{error:#}");
        assert!(conn.is_autocommit());
        assert!(of_kind(&queue, SESSION_CLOSED).is_empty());
        assert!(!run_dir.join(WORKTIME_FILE).exists());
        assert!(WORKTIME_AFTER.with_borrow(Vec::is_empty));

        conn.execute_batch("DROP TRIGGER fail_commit").unwrap();
        exited().unwrap();
        assert_eq!(of_kind(&queue, SESSION_CLOSED).len(), 1);
        let worktime = std::fs::read_to_string(run_dir.join(WORKTIME_FILE)).unwrap();
        assert_eq!(worktime.lines().count(), 2);
    }

    /// The worker's span closed by a `session_exited` at a fixed time, 100 s
    /// after it opened, with records of its transcript after that (the
    /// clock went back): in a write transaction after `read_before`
    /// (`prepared`, task 1334), or analysed at once, outside a
    /// transaction, as every close was before. The closes, the turns
    /// recorded and the lines of `worktime.jsonl` (their times from the
    /// span's start), and the transcripts analysed under the write lock.
    fn close_before_records(prepared: bool) -> (Vec<Value>, Vec<Value>, Vec<Value>, usize) {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let run_dir = dir.path().join("run");
        std::fs::create_dir_all(&run_dir).unwrap();
        let conn = &queue.conn;
        bind(conn, dir.path(), "dagq");
        conn.execute(
            "UPDATE task_runs SET run_dir=?1",
            [run_dir.to_str().unwrap()],
        )
        .unwrap();
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        // The queue's now is 100 s after the span opened; any other time
        // is SQLite's own.
        let fixed = millis_text(start + 100_000);
        let other = Connection::open_in_memory().unwrap();
        conn.create_scalar_function(
            "strftime",
            2,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8,
            move |ctx| {
                let (format, at): (String, String) = (ctx.get(0)?, ctx.get(1)?);
                if format == "%Y-%m-%dT%H:%M:%fZ" && at == "now" {
                    return Ok(Some(fixed.clone()));
                }
                other.query_row("SELECT strftime(?1, ?2)", [format, at], |r| {
                    r.get::<_, Option<String>>(0)
                })
            },
        )
        .unwrap();
        let project = dir.path().join("config/projects/-wt");
        std::fs::create_dir_all(&project).unwrap();
        let line = |kind: &str, secs: i64, content: Value| {
            json!({"type": kind, "timestamp": millis_text(start + secs * 1000),
                   "sessionId": RUN, "version": "2.1.283",
                   "message": {"content": content, "id": format!("m{secs}"),
                               "model": if secs < 100 { "claude-opus-4-1" } else { "claude-sonnet-4-5" },
                               "usage": {"input_tokens": 10, "output_tokens": secs}}})
            .to_string()
        };
        let bash = |id: &str, command: &str| json!([{"type": "tool_use", "id": id, "name": "Bash", "input": {"command": command}}]);
        let result = |id: &str| json!([{"type": "tool_result", "tool_use_id": id, "is_error": false, "content": "ok"}]);
        // A command finished before the close, one that runs past it (its
        // result after it), and a turn wholly after it.
        let lines = [
            line("user", 1, json!("go")),
            line("assistant", 5, bash("a", "cargo test --locked")),
            line("user", 15, result("a")),
            line("assistant", 20, json!([{"type": "text"}])),
            line("user", 80, json!("more")),
            line("assistant", 90, bash("b", "cargo build")),
            line("user", 150, result("b")),
            line("assistant", 155, json!([{"type": "text"}])),
            line("user", 170, json!("later")),
            line("assistant", 175, bash("c", "cargo llvm-cov --locked")),
            line("user", 190, result("c")),
            line("assistant", 195, json!([{"type": "text"}])),
        ];
        std::fs::write(project.join(format!("{RUN}.jsonl")), lines.join("\n")).unwrap();
        let close = || {
            event(
                conn,
                task_id,
                Some(&run),
                EventKind::SessionExited,
                json!({"exit_code": 0}),
            )
            .unwrap();
        };
        let mut under_lock = 0;
        if prepared {
            let read = read_before(conn, Closing::Run(&run, &["session_exited"])).unwrap();
            let before = analysed();
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            close();
            under_lock = analysed() - before;
            conn.execute_batch("COMMIT").unwrap();
            drop(read);
        } else {
            close();
        }
        let payloads = |kind: &str| -> Vec<Value> {
            of_kind(&queue, kind)
                .into_iter()
                .map(|event| event.payload)
                .collect()
        };
        let mut closed = payloads(SESSION_CLOSED);
        let mut turns: Vec<Value> = payloads(SESSION_TURNS)
            .iter()
            .flat_map(|turns| turns["turns"].as_array().unwrap().clone())
            .collect();
        let mut worktime: Vec<Value> = std::fs::read_to_string(run_dir.join(WORKTIME_FILE))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for value in closed.iter_mut().chain(&mut turns).chain(&mut worktime) {
            offsets(value, start);
        }
        (closed, turns, worktime, under_lock)
    }

    /// A close before records of its transcript (the clock went back) in a
    /// write transaction takes the analysis made before it, cut at its end
    /// (task 1334): the same close, turns and `worktime.jsonl` lines as
    /// when the transcript was analysed to that end at the close, with
    /// nothing analysed under the lock.
    #[test]
    fn a_close_before_records_of_its_transcript_is_the_same_as_before() {
        let (closed, turns, worktime, _) = close_before_records(false);
        // What was analysed to the close at 100 s: the turns cut there,
        // the tokens and models of the messages before it, the command
        // running then cut there.
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0]["active"], "recorded");
        assert_eq!(closed[0]["active_secs"], 19 + 20);
        assert_eq!(closed[0]["tokens"]["output"], 5 + 20 + 90);
        assert_eq!(closed[0]["model"], "claude-opus-4-1");
        assert_eq!(closed[0]["work"]["total_secs"], 100);
        assert_eq!(closed[0]["work"]["secs"]["build"], 10);
        assert_eq!(turns, [json!([1000, 20000]), json!([80000, 100000])]);
        let commands: Vec<(&Value, &Value)> = worktime
            .iter()
            .map(|line| (&line["command"], &line["end"]))
            .collect();
        assert_eq!(
            commands,
            [
                (&json!("cargo test --locked"), &json!(15000)),
                (&json!("cargo build"), &json!(100000))
            ]
        );
        let (prepared, prepared_turns, prepared_worktime, under_lock) = close_before_records(true);
        assert_eq!(under_lock, 0);
        assert_eq!(
            (prepared, prepared_turns, prepared_worktime),
            (closed, turns, worktime)
        );
    }

    /// The analysis made before a write transaction (task 1334), past
    /// every record, moves to any end and cut of its close as [`analyse`]
    /// makes it, with nothing analysed under the lock: later, earlier,
    /// before a record (the clock went back), in the middle of a command,
    /// before the span's start; with the repeats of `integrate`'s checks
    /// counted for the verification read then. It does not move to
    /// another start, nor to a later end past a record at or after its
    /// own (`None` in a write transaction, analysed outside one).
    #[test]
    fn the_analysis_made_before_moves_to_any_end_of_its_close() {
        let assistant = |at: i64, id: &str, model: &str, output: i64, content: Value| {
            json!({"type": "assistant", "timestamp": millis_text(at), "sessionId": RUN,
                   "message": {"id": id, "model": model, "content": content,
                               "usage": {"input_tokens": 10, "output_tokens": output}}})
        };
        let user = |at: i64, content: Value| {
            json!({"type": "user", "timestamp": millis_text(at), "sessionId": RUN,
                   "message": {"content": content}})
        };
        let bash = |id: &str, command: &str| json!([{"type": "tool_use", "id": id, "name": "Bash", "input": {"command": command}}]);
        let result = |id: &str| json!([{"type": "tool_result", "tool_use_id": id, "is_error": false, "content": "ok"}]);
        let records = Transcript::parse(
            &[
                user(1_000, json!("go")),
                assistant(
                    2_000,
                    "m1",
                    "claude-opus-4-1",
                    5,
                    bash("a", "cargo test --locked"),
                ),
                user(4_000, result("a")),
                assistant(
                    5_000,
                    "m2",
                    "claude-sonnet-4-5",
                    7,
                    json!([{"type": "text"}]),
                ),
                user(6_000, json!("more")),
                assistant(
                    6_500,
                    "m3",
                    "claude-sonnet-4-5",
                    3,
                    bash("b", "cargo build"),
                ),
                user(7_500, result("b")),
                assistant(
                    8_000,
                    "m4",
                    "claude-sonnet-4-5",
                    9,
                    json!([{"type": "text"}]),
                ),
            ]
            .map(|line| line.to_string())
            .join("\n"),
            RUN,
        )
        .unwrap();
        let want = |start: i64, end: i64, cut: i64, verification: &[&str]| Want {
            start,
            end,
            cut,
            usage: true,
            work: Some(WorkInputs {
                run_dir: None,
                verification: verification.iter().map(|c| (*c).to_owned()).collect(),
                source: true,
            }),
        };
        let conn = Connection::open_in_memory().unwrap();
        let made = |end: i64| {
            let mut read = ReadTranscript::new(records.clone());
            read.analysis = Some(analyse(
                &read.transcript.records,
                &read.turns,
                want(0, end, end, &[]),
            ));
            read
        };
        let fresh = |want: Want| {
            let read = ReadTranscript::new(records.clone());
            analyse(&records.records, &read.turns, want)
        };
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        for (end, cut) in [
            (12_000, 12_000),
            (9_000, 9_000),
            (8_000, 8_001),
            (7_000, 7_000),
            (6_800, 6_800),
            (5_000, 5_001),
            (3_000, 3_000),
            (1_500, 1_500),
            (500, 500),
            (-100, -100),
        ] {
            for verification in [&[][..], &["cargo test --locked"][..]] {
                let mut read = made(9_000);
                let before = analysed();
                let moved = read
                    .analysis(&conn, want(0, end, cut, verification))
                    .unwrap();
                assert_eq!(analysed(), before, "{end}");
                let expected = fresh(want(0, end, cut, verification));
                assert_eq!(moved.want, expected.want);
                assert_eq!(moved.turns, expected.turns, "{end}");
                assert_eq!(moved.usage, expected.usage, "{end}");
                assert_eq!(moved.models, expected.models, "{end}");
                assert_eq!(moved.work, expected.work, "{end}");
            }
        }
        // Another start, or later past a record at or after its end: none
        // under the lock.
        for (mut read, want) in [
            (made(9_000), want(1, 9_000, 9_000, &[])),
            (made(3_000), want(0, 12_000, 12_000, &[])),
        ] {
            let before = analysed();
            assert!(read.analysis(&conn, want).is_none());
            assert_eq!(analysed(), before);
        }
        conn.execute_batch("ROLLBACK").unwrap();
        // Outside a write transaction: analysed to that end.
        let mut read = made(3_000);
        let later = read.analysis(&conn, want(0, 12_000, 12_000, &[])).unwrap();
        assert_eq!(later.work, fresh(want(0, 12_000, 12_000, &[])).work);
        assert_eq!(later.work.unwrap().total_millis, 12_000);
    }

    /// A revise recorded in a write transaction, typed before its event was
    /// written, closes the worker's span when it was sent with the analysis
    /// made before (task 1334): the turn after it is the revise's.
    #[test]
    fn a_revise_recorded_in_a_write_transaction_closes_where_it_was_sent() {
        let dir = tempfile::tempdir().unwrap();
        let (queue, task_id, run) = run_queue(dir.path());
        let conn = &queue.conn;
        event(
            conn,
            task_id,
            Some(&run),
            EventKind::AgentStarted,
            json!({"session_id": RUN}),
        )
        .unwrap();
        let start = retime(conn, 0, 100);
        transcript(dir.path(), start, &[(5, 25), (61, 80)], None);
        let sent = (start + 60_000) / 1000;
        ANALYSES.set(0);
        let before = analysed();
        SqliteQueue::record_runtime_event(
            &queue,
            &run,
            EventKind::ReviseRequested,
            json!({"attempt": 1, "sent_at": sent}),
        )
        .unwrap();
        // Once, before the transaction: the analysis, and its readings of
        // the records (the turns, the tokens and models to every cut, the
        // work), none of them again under the lock for the earlier end.
        assert_eq!(ANALYSES.get(), 1);
        assert_eq!(analysed() - before, 1 + 3);
        let spans = spans(&queue);
        assert_eq!(spans[1].payload["active"], "recorded");
        assert_eq!(spans[1].payload["active_secs"], 20);
        assert_eq!(spans[1].created_at, millis_text(sent * 1000));
    }
}
