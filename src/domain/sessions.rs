//! The Claude sessions dagq uses, recorded as spans (ADR-0048 decisions 1,
//! 2 and 7): a span is a session open for one purpose, with its kind, its
//! session id and the `session_opened` / `session_closed` events that start
//! and end it. The runtime writes them next to the event that starts or ends
//! the span, in the same transaction; this module decides which spans an
//! event opens and closes.

use serde_json::{Value, json};

use super::EventId;

pub const SESSION_OPENED: &str = "session_opened";
pub const SESSION_CLOSED: &str = "session_closed";
/// The transcript's turns of a span, recorded while it is open and when it
/// closes (ADR-0048 decision 8).
pub const SESSION_TURNS: &str = "session_turns";

pub const WORKER: &str = "worker";
pub const RESUME: &str = "resume";
pub const REVISE: &str = "revise";
pub const REVIEW: &str = "review";
pub const TRIAGE: &str = "triage";
pub const OBSERVER: &str = "observer";
pub const PLAN_REVIEW: &str = "plan_review";
pub const RUNTIME_PLANNER: &str = "runtime_planner";
pub const INBOX: &str = "inbox";
pub const PLANNER: &str = "planner";

/// Every kind of span, in the order `stats` lists them.
pub const KINDS: [&str; 10] = [
    WORKER,
    RESUME,
    REVISE,
    REVIEW,
    TRIAGE,
    OBSERVER,
    PLAN_REVIEW,
    RUNTIME_PLANNER,
    INBOX,
    PLANNER,
];

/// The kinds of a run's own session: the worker's, a resume's, and the
/// revise it is sent back to.
pub const RUN_SESSION: [&str; 3] = [WORKER, RESUME, REVISE];

/// Why a span was closed.
pub const EXITED: &str = "exited";
pub const JOB_FINISHED: &str = "job_finished";
pub const NEXT_SPAN: &str = "next_span";
/// No event ended it: it was closed when the runtime found it had ended.
pub const INFERRED: &str = "inferred";

/// Where the events that may open or close spans are recorded, which says
/// which open spans they are about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// An event of a run: its spans.
    Run,
    /// An event of a proposal's plan review (on its first task): the spans
    /// of that proposal.
    Proposal,
    /// An event of the queue itself: the observer's spans.
    Queue,
}

/// The scope of an event of `kind`, when it may open or close a span.
pub fn scope(kind: &str) -> Option<Scope> {
    match kind {
        "agent_started" | "revise_requested" | "revise_unsent" | "session_exited"
        | "workspace_closed" | "run_recovered" | "review_started" | "review_finished"
        | "review_failed" | "review_retried" | "triage_started" | "triage_finished"
        | "triage_failed" => Some(Scope::Run),
        "plan_review_started" | "plan_review_finished" | "plan_review_failed" => {
            Some(Scope::Proposal)
        }
        "observe_started" | "observe_finished" => Some(Scope::Queue),
        _ => None,
    }
}

/// A span that is open: its `session_opened` event and payload.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenSpan {
    pub opened_event_id: EventId,
    pub payload: Value,
}

impl OpenSpan {
    pub fn kind(&self) -> &str {
        self.payload["kind"].as_str().unwrap_or_default()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.payload["session_id"].as_str()
    }
}

/// What the runtime knows of the run or proposal an event is about, beyond
/// the event itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpanContext {
    /// The run's worktree, where its sessions and its review run.
    pub worktree: Option<String>,
    /// The run's directory, where its triage runs.
    pub run_dir: Option<String>,
    /// The run's worker workspace.
    pub workspace_id: Option<String>,
    /// The `resume_started` events the run has, this one's included.
    pub resumes: i64,
    /// The `revise_requested` events the run has, this one's included, but
    /// those a `revise_unsent` withdrew.
    pub revises: i64,
    /// The goals of the proposal's tasks (a plan review's), ascending.
    pub goal_ids: Vec<i64>,
}

/// A span to write: `Close` one that is open, or `Open` a new one with the
/// payload of its `session_opened`.
#[derive(Debug, Clone, PartialEq)]
pub enum SpanChange {
    Close {
        span: OpenSpan,
        reason: &'static str,
    },
    Open(Value),
}

impl SpanChange {
    /// The payload of the `session_closed` of a `Close`.
    pub fn closed_payload(span: &OpenSpan, reason: &str) -> Value {
        json!({
            "opened_event_id": span.opened_event_id,
            "kind": span.kind(),
            "session_id": span.session_id(),
            "reason": reason,
        })
    }
}

/// The spans an event of `kind` with `payload` closes and opens, given the
/// spans open in its scope (oldest first) and what `context` knows. Closes
/// come first. An event that is not about spans changes none.
pub fn changes(
    kind: &str,
    payload: &Value,
    open: &[OpenSpan],
    context: &SpanContext,
) -> Vec<SpanChange> {
    let close = |kinds: &[&str], reason: &'static str| {
        open.iter()
            .filter(|span| kinds.contains(&span.kind()))
            .map(|span| SpanChange::Close {
                span: span.clone(),
                reason,
            })
            .collect::<Vec<_>>()
    };
    let text = |key: &str| payload.get(key).and_then(Value::as_str);
    match kind {
        // A session starting while another is still recorded open: that
        // one ended without a `session_exited` (ADR-0048 decision 7).
        "agent_started" => {
            let mut changes = close(&RUN_SESSION, INFERRED);
            let (span, attempt) = if context.resumes > 0 {
                (RESUME, context.resumes)
            } else {
                (WORKER, 1)
            };
            changes.push(SpanChange::Open(json!({
                "kind": span,
                "session_id": text("session_id"),
                "cwd": context.worktree,
                "transcript_path": null,
                "attempt": attempt,
                "workspace_id": (span == WORKER).then_some(&context.workspace_id),
            })));
            changes
        }
        // The same session goes on as a revise.
        "revise_requested" => {
            let mut changes = close(&RUN_SESSION, NEXT_SPAN);
            let session_id = open
                .iter()
                .rev()
                .find(|span| RUN_SESSION.contains(&span.kind()))
                .and_then(OpenSpan::session_id)
                .map(str::to_owned);
            changes.push(SpanChange::Open(json!({
                "kind": REVISE,
                "session_id": session_id,
                "cwd": context.worktree,
                "transcript_path": null,
                "attempt": context.revises,
                "workspace_id": text("workspace_id"),
            })));
            changes
        }
        // A revise that could not be sent never reached the session: it goes
        // on as what it was before (the revise sent before it, else the
        // worker or the resume).
        "revise_unsent" => {
            let mut changes = close(&[REVISE], NEXT_SPAN);
            let reopened = open.iter().rev().find(|span| span.kind() == REVISE);
            let (span, attempt) = if context.revises > 0 {
                (REVISE, context.revises)
            } else if context.resumes > 0 {
                (RESUME, context.resumes)
            } else {
                (WORKER, 1)
            };
            changes.push(SpanChange::Open(json!({
                "kind": span,
                "session_id": reopened.and_then(OpenSpan::session_id),
                "cwd": context.worktree,
                "transcript_path": null,
                "attempt": attempt,
                "workspace_id": (span == WORKER).then_some(&context.workspace_id),
            })));
            changes
        }
        "session_exited" => close(&RUN_SESSION, EXITED),
        // The session went with its workspace, or the run was recovered.
        "workspace_closed" => close(&RUN_SESSION, INFERRED),
        // A recovered run's review died with its supervisor too.
        "run_recovered" => close(&[WORKER, RESUME, REVISE, REVIEW], INFERRED),
        "review_started" => {
            let mut changes = close(&[REVIEW], INFERRED);
            changes.push(job(REVIEW, payload, context.worktree.as_deref()));
            changes
        }
        "review_finished" | "review_failed" | "review_retried" => close(&[REVIEW], JOB_FINISHED),
        // A run is triaged once its session is gone.
        "triage_started" => {
            let mut changes = close(&[WORKER, RESUME, REVISE, REVIEW, TRIAGE], INFERRED);
            changes.push(job(TRIAGE, payload, context.run_dir.as_deref()));
            changes
        }
        "triage_finished" | "triage_failed" => close(&[TRIAGE], JOB_FINISHED),
        "plan_review_started" => {
            let mut changes = close(&[PLAN_REVIEW], INFERRED);
            let mut opened = job(PLAN_REVIEW, payload, text("cwd"));
            if let SpanChange::Open(opened) = &mut opened {
                opened["proposal_id"] = payload["proposal_id"].clone();
                opened["plan_review_id"] = payload["plan_review_id"].clone();
                opened["goal_ids"] = json!(context.goal_ids);
            }
            changes.push(opened);
            changes
        }
        "plan_review_finished" | "plan_review_failed" => open
            .iter()
            .filter(|span| {
                span.kind() == PLAN_REVIEW
                    && span.payload["plan_review_id"] == payload["plan_review_id"]
            })
            .map(|span| SpanChange::Close {
                span: span.clone(),
                reason: JOB_FINISHED,
            })
            .collect(),
        // An observer that died with its supervisor recorded no finish.
        "observe_started" => {
            let mut changes = close(&[OBSERVER], INFERRED);
            changes.push(job(OBSERVER, payload, text("dir")));
            changes
        }
        "observe_finished" => open
            .iter()
            .filter(|span| span.kind() == OBSERVER && span.payload["cwd"] == payload["dir"])
            .map(|span| SpanChange::Close {
                span: span.clone(),
                reason: JOB_FINISHED,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The span of a headless job, whose session id the runtime gave it
/// (ADR-0048 decision 4) and recorded in the event that starts it.
fn job(kind: &str, payload: &Value, cwd: Option<&str>) -> SpanChange {
    SpanChange::Open(json!({
        "kind": kind,
        "session_id": payload.get("session_id").and_then(Value::as_str),
        "cwd": cwd,
        "transcript_path": null,
        "attempt": payload.get("attempt").and_then(Value::as_i64),
    }))
}

/// The kinds of span the plugin's hook records, on no run (ADR-0048
/// decision 6): the sessions the runtime does not start headless.
pub const HOOK_KINDS: [&str; 3] = [RUNTIME_PLANNER, INBOX, PLANNER];

/// A `SessionStart` or `SessionEnd` of an inbox or planner session, as the
/// plugin's hook reports it (ADR-0048 decision 6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHook {
    pub event: HookEvent,
    /// The span's kind, one of [`HOOK_KINDS`] ([`hook_kind`]).
    pub kind: &'static str,
    pub session_id: String,
    pub transcript_path: Option<String>,
    pub cwd: Option<String>,
    /// The cmux workspace the session runs in (`CMUX_WORKSPACE_ID`).
    pub workspace_id: Option<String>,
    /// The planner session (`DAGQ_PLANNER_ID`) of a planner's.
    pub planner_id: Option<i64>,
}

impl SessionHook {
    /// The hook's report of `event` (`open` for a `SessionStart`, `close`
    /// for a `SessionEnd`) from its stdin `input` and the session's
    /// environment `env`; `None` when the session is not one the hook
    /// records ([`hook_kind`]). An input without a session id is an error.
    pub fn from_hook(
        event: &str,
        input: &Value,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, String> {
        let Some(kind) = hook_kind(
            env("DAGQ_SESSION_KIND").as_deref(),
            env("DAGQ_ROLE").as_deref(),
            env("DAGQ_PLANNER_ORIGIN").as_deref(),
        ) else {
            return Ok(None);
        };
        let text = |key: &str| {
            input
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        let session_id = text("session_id").ok_or("the hook input has no session_id")?;
        let event = match event {
            "open" => HookEvent::Start {
                source: text("source").unwrap_or_else(|| "startup".into()),
            },
            "close" => HookEvent::End {
                reason: text("reason").unwrap_or_else(|| "other".into()),
            },
            other => return Err(format!("unknown session event {other:?}: open or close")),
        };
        Ok(Some(Self {
            event,
            kind,
            session_id,
            transcript_path: text("transcript_path"),
            cwd: text("cwd"),
            workspace_id: env("CMUX_WORKSPACE_ID").filter(|id| !id.trim().is_empty()),
            planner_id: env("DAGQ_PLANNER_ID").and_then(|id| id.trim().parse().ok()),
        }))
    }
}

/// What the hook saw: a session starting (its `source`: `startup`,
/// `resume`, `clear`, `compact`) or ending (its `reason`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    Start { source: String },
    End { reason: String },
}

/// The kind of the span of a session with `DAGQ_SESSION_KIND` `session_kind`,
/// `DAGQ_ROLE` `role` and `DAGQ_PLANNER_ORIGIN` `origin`, when the hook
/// records one: the workspace's `DAGQ_SESSION_KIND`, else (a workspace
/// opened before it) the role, a planner the runtime opened being a
/// `runtime_planner`. Every other session (workers included) has none.
pub fn hook_kind(
    session_kind: Option<&str>,
    role: Option<&str>,
    origin: Option<&str>,
) -> Option<&'static str> {
    if let Some(kind) = session_kind.filter(|kind| !kind.is_empty()) {
        return HOOK_KINDS.into_iter().find(|known| *known == kind);
    }
    match (role?, origin) {
        ("inbox", _) => Some(INBOX),
        ("planner", Some("runtime")) => Some(RUNTIME_PLANNER),
        ("planner", _) => Some(PLANNER),
        _ => None,
    }
}

/// Why a span closed at a `SessionEnd` of `reason`: `clear` and `logout`
/// as they are, any other end (`prompt_input_exit`, `other`, ...) the
/// session's exit.
pub fn end_reason(reason: &str) -> &'static str {
    match reason {
        "clear" => "clear",
        "logout" => "logout",
        _ => EXITED,
    }
}

/// The spans a hook's report closes and opens, given the spans of
/// [`HOOK_KINDS`] open (oldest first). A start of a session whose span is
/// open goes on with it (`resume`, `compact`, a `clear` that kept the
/// session id); one of a new session id closes the spans open in the same
/// workspace as `next_span` (a `/clear` gave the session a new id) and
/// opens its own. An end closes the session's span; one already closed
/// changes nothing. `context` is added to the opened payload (a runtime
/// planner's proposal and goals).
pub fn hook_changes(hook: &SessionHook, open: &[OpenSpan], context: &Value) -> Vec<SpanChange> {
    let own = |span: &&OpenSpan| {
        HOOK_KINDS.contains(&span.kind()) && span.session_id() == Some(hook.session_id.as_str())
    };
    match &hook.event {
        HookEvent::End { reason } => open
            .iter()
            .filter(own)
            .map(|span| SpanChange::Close {
                span: span.clone(),
                reason: end_reason(reason),
            })
            .collect(),
        HookEvent::Start { .. } if open.iter().any(|span| own(&span)) => Vec::new(),
        HookEvent::Start { source } => {
            let mut changes: Vec<SpanChange> = open
                .iter()
                .filter(|span| {
                    HOOK_KINDS.contains(&span.kind())
                        && hook.workspace_id.is_some()
                        && span.payload["workspace_id"].as_str() == hook.workspace_id.as_deref()
                })
                .map(|span| SpanChange::Close {
                    span: span.clone(),
                    reason: NEXT_SPAN,
                })
                .collect();
            let mut payload = json!({
                "kind": hook.kind,
                "session_id": hook.session_id,
                "cwd": hook.cwd,
                "transcript_path": hook.transcript_path,
                "workspace_id": hook.workspace_id,
                "provider": "claude",
                "source": source,
            });
            if let Some(planner) = hook.planner_id {
                payload["planner_id"] = json!(planner);
            }
            if let Some(context) = context.as_object() {
                for (key, value) in context {
                    payload[key] = value.clone();
                }
            }
            changes.push(SpanChange::Open(payload));
            changes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(id: i64, payload: Value) -> OpenSpan {
        OpenSpan {
            opened_event_id: EventId::new(id),
            payload,
        }
    }

    fn opened(change: &SpanChange) -> &Value {
        match change {
            SpanChange::Open(payload) => payload,
            SpanChange::Close { .. } => panic!("expected an open, got {change:?}"),
        }
    }

    fn closed(change: &SpanChange) -> (i64, &'static str) {
        match change {
            SpanChange::Close { span, reason } => (span.opened_event_id.as_i64(), *reason),
            SpanChange::Open(_) => panic!("expected a close, got {change:?}"),
        }
    }

    #[test]
    fn scopes_cover_the_starts_and_ends_of_every_recorded_kind() {
        assert_eq!(scope("agent_started"), Some(Scope::Run));
        assert_eq!(scope("triage_failed"), Some(Scope::Run));
        assert_eq!(scope("plan_review_failed"), Some(Scope::Proposal));
        assert_eq!(scope("observe_finished"), Some(Scope::Queue));
        assert_eq!(scope("run_claimed"), None);
        assert_eq!(scope(SESSION_OPENED), None);
        assert_eq!(KINDS.len(), 10);
    }

    /// The worker's session opens with the run's id, goes on as a revise,
    /// and ends at its exit.
    #[test]
    fn a_worker_session_opens_switches_to_revise_and_exits() {
        let context = SpanContext {
            worktree: Some("/wt".into()),
            workspace_id: Some("W".into()),
            ..SpanContext::default()
        };
        let started = changes(
            "agent_started",
            &json!({"pid": 1, "session_id": "run-1"}),
            &[],
            &context,
        );
        assert_eq!(started.len(), 1);
        let payload = opened(&started[0]);
        assert_eq!(payload["kind"], WORKER);
        assert_eq!(payload["session_id"], "run-1");
        assert_eq!(payload["cwd"], "/wt");
        assert_eq!(payload["attempt"], 1);
        assert_eq!(payload["workspace_id"], "W");

        let worker = span(10, payload.clone());
        let revise_context = SpanContext {
            revises: 1,
            ..context.clone()
        };
        let revised = changes(
            "revise_requested",
            &json!({"workspace_id": "W"}),
            std::slice::from_ref(&worker),
            &revise_context,
        );
        assert_eq!(closed(&revised[0]), (10, NEXT_SPAN));
        let payload = opened(&revised[1]);
        assert_eq!(payload["kind"], REVISE);
        assert_eq!(payload["session_id"], "run-1");
        assert_eq!(payload["attempt"], 1);

        let revise = span(12, payload.clone());
        let exited = changes("session_exited", &json!({}), &[revise], &context);
        assert_eq!(exited.len(), 1);
        assert_eq!(closed(&exited[0]), (12, EXITED));
        let closed_payload = SpanChange::closed_payload(&worker, EXITED);
        assert_eq!(closed_payload["opened_event_id"], 10);
        assert_eq!(closed_payload["kind"], WORKER);
        assert_eq!(closed_payload["session_id"], "run-1");
    }

    /// A revise withdrawn by `revise_unsent` never reached the session: the
    /// revise span closes and the session goes on as the worker's.
    #[test]
    fn a_withdrawn_revise_goes_back_to_the_session_it_replaced() {
        let context = SpanContext {
            worktree: Some("/wt".into()),
            workspace_id: Some("W".into()),
            ..Default::default()
        };
        let revise = span(
            12,
            json!({"kind": REVISE, "session_id": "run-1", "attempt": 1}),
        );
        let withdrawn = changes(
            "revise_unsent",
            &json!({"attempt": 1}),
            std::slice::from_ref(&revise),
            &context,
        );
        assert_eq!(closed(&withdrawn[0]), (12, NEXT_SPAN));
        let payload = opened(&withdrawn[1]);
        assert_eq!(payload["kind"], WORKER);
        assert_eq!(payload["session_id"], "run-1");
        assert_eq!(payload["attempt"], 1);
        assert_eq!(payload["workspace_id"], "W");
        // After a revise that was sent, it goes on as that revise.
        let after_one = SpanContext {
            revises: 1,
            ..context.clone()
        };
        let withdrawn = changes("revise_unsent", &json!({}), &[revise], &after_one);
        let payload = opened(&withdrawn[1]);
        assert_eq!(payload["kind"], REVISE);
        assert_eq!(payload["attempt"], 1);
        assert_eq!(payload["workspace_id"], json!(null));
    }

    /// A resume's session is `resume`; one still open when the next starts
    /// is closed as inferred.
    #[test]
    fn a_resume_opens_its_own_span_and_infers_the_end_of_a_lost_one() {
        let context = SpanContext {
            resumes: 2,
            ..SpanContext::default()
        };
        let lost = span(5, json!({"kind": WORKER, "session_id": "r"}));
        let started = changes(
            "agent_started",
            &json!({"session_id": "r"}),
            &[lost],
            &context,
        );
        assert_eq!(closed(&started[0]), (5, INFERRED));
        let payload = opened(&started[1]);
        assert_eq!(payload["kind"], RESUME);
        assert_eq!(payload["attempt"], 2);
        assert_eq!(payload["workspace_id"], Value::Null);
        for (kind, closes) in [("workspace_closed", 1), ("run_recovered", 2)] {
            let open = span(7, json!({"kind": RESUME}));
            let review = span(8, json!({"kind": REVIEW}));
            let changed = changes(kind, &json!({}), &[open, review], &context);
            assert_eq!(changed.len(), closes, "{kind}");
            assert_eq!(closed(&changed[0]), (7, INFERRED));
        }
    }

    /// Headless jobs open with the session id they were given and close at
    /// their finish; one restarted without a finish is inferred.
    #[test]
    fn jobs_open_with_their_session_id_and_close_at_their_finish() {
        let context = SpanContext {
            worktree: Some("/wt".into()),
            run_dir: Some("/run".into()),
            ..SpanContext::default()
        };
        let review = changes(
            "review_started",
            &json!({"attempt": 2, "session_id": "s-review"}),
            &[span(1, json!({"kind": REVIEW}))],
            &context,
        );
        assert_eq!(closed(&review[0]), (1, INFERRED));
        let payload = opened(&review[1]);
        assert_eq!(payload["kind"], REVIEW);
        assert_eq!(payload["session_id"], "s-review");
        assert_eq!(payload["attempt"], 2);
        assert_eq!(payload["cwd"], "/wt");
        for kind in ["review_finished", "review_failed", "review_retried"] {
            let changed = changes(kind, &json!({}), &[span(3, payload.clone())], &context);
            assert_eq!(closed(&changed[0]), (3, JOB_FINISHED), "{kind}");
        }

        let triage = changes(
            "triage_started",
            &json!({"attempt": 1, "session_id": "s-triage"}),
            &[span(4, json!({"kind": WORKER}))],
            &context,
        );
        assert_eq!(closed(&triage[0]), (4, INFERRED));
        let payload = opened(&triage[1]);
        assert_eq!(payload["kind"], TRIAGE);
        assert_eq!(payload["cwd"], "/run");
        for kind in ["triage_finished", "triage_failed"] {
            let changed = changes(kind, &json!({}), &[span(6, payload.clone())], &context);
            assert_eq!(closed(&changed[0]), (6, JOB_FINISHED), "{kind}");
        }

        let plan_context = SpanContext {
            goal_ids: vec![3, 4],
            ..SpanContext::default()
        };
        let plan = changes(
            "plan_review_started",
            &json!({"proposal_id": 9, "plan_review_id": 2, "attempt": 1, "session_id": "s-plan"}),
            &[],
            &plan_context,
        );
        let payload = opened(&plan[0]);
        assert_eq!(payload["kind"], PLAN_REVIEW);
        assert_eq!(payload["proposal_id"], 9);
        assert_eq!(payload["plan_review_id"], 2);
        assert_eq!(payload["goal_ids"], json!([3, 4]));
        let other = span(8, json!({"kind": PLAN_REVIEW, "plan_review_id": 1}));
        let finished = changes(
            "plan_review_failed",
            &json!({"plan_review_id": 2}),
            &[other, span(9, payload.clone())],
            &plan_context,
        );
        assert_eq!(finished.len(), 1);
        assert_eq!(closed(&finished[0]), (9, JOB_FINISHED));

        let observe = changes(
            "observe_started",
            &json!({"mode": "hourly", "dir": "/obs/1", "session_id": "s-obs"}),
            &[span(10, json!({"kind": OBSERVER, "cwd": "/obs/0"}))],
            &SpanContext::default(),
        );
        assert_eq!(closed(&observe[0]), (10, INFERRED));
        let payload = opened(&observe[1]);
        assert_eq!(payload["cwd"], "/obs/1");
        let finished = changes(
            "observe_finished",
            &json!({"dir": "/obs/1"}),
            &[span(11, payload.clone())],
            &SpanContext::default(),
        );
        assert_eq!(closed(&finished[0]), (11, JOB_FINISHED));
        assert!(changes("run_claimed", &json!({}), &[], &context).is_empty());
    }

    #[test]
    fn the_hook_kind_comes_from_the_session_kind_else_the_role() {
        assert_eq!(hook_kind(Some("inbox"), Some("planner"), None), Some(INBOX));
        assert_eq!(
            hook_kind(Some("runtime_planner"), Some("planner"), None),
            Some(RUNTIME_PLANNER)
        );
        assert_eq!(hook_kind(Some("worker"), Some("inbox"), None), None);
        assert_eq!(hook_kind(Some(""), Some("inbox"), None), Some(INBOX));
        assert_eq!(hook_kind(None, Some("planner"), None), Some(PLANNER));
        assert_eq!(
            hook_kind(None, Some("planner"), Some("person")),
            Some(PLANNER)
        );
        assert_eq!(
            hook_kind(None, Some("planner"), Some("runtime")),
            Some(RUNTIME_PLANNER)
        );
        assert_eq!(hook_kind(None, Some("worker"), None), None);
        assert_eq!(hook_kind(None, None, None), None);
        assert_eq!(end_reason("clear"), "clear");
        assert_eq!(end_reason("logout"), "logout");
        assert_eq!(end_reason("prompt_input_exit"), EXITED);
        assert_eq!(end_reason("other"), EXITED);
    }

    #[test]
    fn a_hook_input_names_its_session_and_the_environment_its_span() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        let input = json!({"session_id": "s", "transcript_path": "/t.jsonl", "cwd": "/repo",
                           "source": "clear", "reason": "logout"});
        let planner = env(&[
            ("DAGQ_ROLE", "planner"),
            ("DAGQ_SESSION_KIND", "runtime_planner"),
            ("CMUX_WORKSPACE_ID", "W"),
            ("DAGQ_PLANNER_ID", "7"),
        ]);
        let hook = SessionHook::from_hook("open", &input, planner)
            .unwrap()
            .unwrap();
        assert_eq!(
            hook,
            SessionHook {
                event: HookEvent::Start {
                    source: "clear".into()
                },
                kind: RUNTIME_PLANNER,
                session_id: "s".into(),
                transcript_path: Some("/t.jsonl".into()),
                cwd: Some("/repo".into()),
                workspace_id: Some("W".into()),
                planner_id: Some(7),
            }
        );
        let inbox = env(&[("DAGQ_ROLE", "inbox"), ("CMUX_WORKSPACE_ID", " ")]);
        let hook = SessionHook::from_hook("close", &input, inbox)
            .unwrap()
            .unwrap();
        assert_eq!(
            hook.event,
            HookEvent::End {
                reason: "logout".into()
            }
        );
        assert_eq!(hook.workspace_id, None);
        assert_eq!(hook.planner_id, None);
        let bare = json!({"session_id": "s"});
        let hook = SessionHook::from_hook("open", &bare, inbox)
            .unwrap()
            .unwrap();
        assert_eq!(
            hook.event,
            HookEvent::Start {
                source: "startup".into()
            }
        );
        let hook = SessionHook::from_hook("close", &bare, inbox)
            .unwrap()
            .unwrap();
        assert_eq!(
            hook.event,
            HookEvent::End {
                reason: "other".into()
            }
        );
        assert_eq!(
            SessionHook::from_hook("open", &input, env(&[("DAGQ_ROLE", "worker")])).unwrap(),
            None
        );
        assert!(SessionHook::from_hook("open", &json!({}), inbox).is_err());
        assert!(SessionHook::from_hook("stop", &input, inbox).is_err());
    }

    #[test]
    fn a_hook_opens_goes_on_replaces_and_closes_spans() {
        let hook = |event: HookEvent, session: &str, workspace: Option<&str>| SessionHook {
            event,
            kind: INBOX,
            session_id: session.into(),
            transcript_path: None,
            cwd: None,
            workspace_id: workspace.map(str::to_owned),
            planner_id: Some(2),
        };
        let start = |session, workspace| {
            hook(
                HookEvent::Start {
                    source: "startup".into(),
                },
                session,
                workspace,
            )
        };
        let opened_changes =
            hook_changes(&start("s-1", Some("W")), &[], &json!({"proposal_id": 4}));
        let payload = opened(&opened_changes[0]);
        assert_eq!(payload["kind"], INBOX);
        assert_eq!(payload["session_id"], "s-1");
        assert_eq!(payload["workspace_id"], "W");
        assert_eq!(payload["source"], "startup");
        assert_eq!(payload["planner_id"], 2);
        assert_eq!(payload["proposal_id"], 4);
        let open = [
            span(1, payload.clone()),
            span(
                2,
                json!({"kind": INBOX, "session_id": "s-9", "workspace_id": "V"}),
            ),
            // A run's span is never the hook's.
            span(
                3,
                json!({"kind": WORKER, "session_id": "s-1", "workspace_id": "W"}),
            ),
        ];
        // The same session: goes on.
        assert!(hook_changes(&start("s-1", Some("W")), &open, &Value::Null).is_empty());
        // A new session id in the workspace: replaces its span.
        let replaced = hook_changes(&start("s-2", Some("W")), &open, &Value::Null);
        assert_eq!(replaced.len(), 2);
        assert_eq!(closed(&replaced[0]), (1, NEXT_SPAN));
        assert_eq!(opened(&replaced[1])["session_id"], "s-2");
        // No workspace: closes nothing.
        let alone = hook_changes(&start("s-3", None), &open, &Value::Null);
        assert_eq!(alone.len(), 1);
        let end = |session| {
            hook(
                HookEvent::End {
                    reason: "clear".into(),
                },
                session,
                Some("W"),
            )
        };
        let ended = hook_changes(&end("s-1"), &open, &Value::Null);
        assert_eq!(ended.len(), 1);
        assert_eq!(closed(&ended[0]), (1, "clear"));
        assert!(hook_changes(&end("s-4"), &open, &Value::Null).is_empty());
    }
}
