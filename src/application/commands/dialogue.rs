//! The dialogue and record commands (task 733): `ask`, `ask close`,
//! `answer`, `note`, `mark` and `finding record / resolve / dismiss`. Each
//! is authorized as the caller before it reaches the store: a worker asks
//! its `worker_question` on its own run only, a planner its
//! `planner_question` on no run, the observer its `blocked` ask on a
//! finding; only the user and the inbox answer (ADR-t728-3 decision 4),
//! and the answer records whose authority it carries, the user's own or the
//! inbox's delegated one, from the actor's type. The writer each record
//! keeps (`asked_by`, `by`) is the actor's, whatever the caller passed.
//! The policy is the [`crate::domain::StaticPolicy`]'s
//! (`docs/design/authorization.md`).

use anyhow::Result;
use serde_json::Value;

use super::{DenialLog, Gate};
use crate::domain::{
    ActorContext, Answerer, Ask, AskId, AskKind, AuthorizationError, Authorizer, Capability,
    EventId, Finding, FindingId, FindingOutcome, FindingStatus, FindingTarget, NewAsk, NewFinding,
    NewNote, NoteTarget, Resource, RunEvent, authorization::DenyReason, stats::Cursor,
};

/// A change mark to record, or one to retract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkChange {
    Record {
        label: String,
        note: Option<String>,
        at: Option<Cursor>,
    },
    Retract(EventId),
}

/// What the dialogue and record commands read and change of the queue.
pub trait DialogueStore: DenialLog {
    /// The ask; a missing ask is an error.
    fn read_ask(&self, id: AskId) -> Result<Ask>;
    /// Register the ask and notify the inbox of a new one.
    fn open_ask(&mut self, ask: NewAsk) -> Result<Value>;
    fn answer(&mut self, id: AskId, text: &str, answerer: Answerer) -> Result<Ask>;
    fn close_ask(&mut self, id: AskId) -> Result<Ask>;
    fn add_note(&mut self, note: NewNote) -> Result<RunEvent>;
    fn mark(&mut self, change: MarkChange, by: &str) -> Result<Value>;
    fn record_finding(&mut self, finding: NewFinding) -> Result<FindingOutcome>;
    fn set_finding_status(
        &mut self,
        id: FindingId,
        to: FindingStatus,
        reason: &str,
        by: &str,
    ) -> Result<Finding>;
}

/// The dialogue and record commands of one actor on one store.
pub struct Dialogue<'a, S> {
    store: &'a mut S,
    gate: Gate<'a>,
}

impl<'a, S: DialogueStore> Dialogue<'a, S> {
    pub fn new(store: &'a mut S, actor: &'a ActorContext, authorizer: &'a dyn Authorizer) -> Self {
        Self {
            store,
            gate: Gate { actor, authorizer },
        }
    }

    /// `ask`: a `blocked` ask on a finding raises that finding
    /// ([`Capability::FindingAsk`], the observer's); any other opens an ask
    /// of its kind on the run or task it names. `asked_by` is the actor's.
    /// A `blocked` ask without a recommendation is refused
    /// ([`NewAsk::check_asker`], ADR-t451-1 decision 2).
    pub fn ask(&mut self, mut ask: NewAsk) -> Result<Value> {
        let (capability, resource) = match ask.finding_id {
            Some(finding) if ask.kind == AskKind::Blocked => {
                (Capability::FindingAsk, Resource::Finding(finding))
            }
            _ => (
                Capability::AskOpen,
                Resource::NewAsk {
                    kind: ask.kind.clone(),
                    run: ask.run_id.clone(),
                    task: ask.task_id,
                },
            ),
        };
        self.gate.authorize(&*self.store, capability, &resource)?;
        ask.check_asker()?;
        ask.asked_by = self.gate.actor.written_by().to_owned();
        self.store.open_ask(ask)
    }

    /// `answer`: the user's own, or the inbox's at a person's word
    /// (delegated), which the ask's row and `ask_answered` record.
    pub fn answer(&mut self, id: AskId, text: &str) -> Result<Ask> {
        self.authorize_ask(Capability::AskAnswer, id)?;
        let Some(answerer) = self.gate.actor.answerer() else {
            // A policy that let another actor answer names no authority
            // for it: refused rather than recorded as someone else's.
            let error = AuthorizationError {
                role: self.gate.actor.role(),
                capability: Capability::AskAnswer,
                reason: DenyReason::NotGranted,
            };
            super::record(&*self.store, &error, &Resource::Ask { id, run: None });
            return Err(error.into());
        };
        self.store.answer(id, text, answerer)
    }

    /// `ask close`: the user's, the inbox's and the supervisor's.
    pub fn close(&mut self, id: AskId) -> Result<Ask> {
        self.authorize_ask(Capability::AskClose, id)?;
        self.store.close_ask(id)
    }

    /// `note` on a task, a run or a goal; `by` is the actor's.
    pub fn note(&mut self, mut note: NewNote) -> Result<RunEvent> {
        let resource = match &note.target {
            NoteTarget::Task(task) => Resource::task(*task),
            NoteTarget::Run(run) => Resource::run(run.clone()),
            NoteTarget::Goal(goal) => Resource::Goal(*goal),
        };
        self.gate
            .authorize(&*self.store, Capability::NoteWrite, &resource)?;
        note.by = self.gate.actor.written_by().to_owned();
        self.store.add_note(note)
    }

    /// `mark`, or `mark --retract`: a change mark on the queue.
    pub fn mark(&mut self, change: MarkChange) -> Result<Value> {
        self.gate
            .authorize(&*self.store, Capability::MarkWrite, &Resource::Queue)?;
        let by = self.gate.actor.written_by();
        self.store.mark(change, by)
    }

    /// `finding record` on its target; `by` is the actor's.
    pub fn record_finding(&mut self, mut finding: NewFinding) -> Result<FindingOutcome> {
        let resource = match &finding.target {
            FindingTarget::Queue => Resource::Queue,
            FindingTarget::Goal(goal) => Resource::Goal(*goal),
            FindingTarget::Task(task) => Resource::task(*task),
            FindingTarget::Run(run) => Resource::run(run.clone()),
        };
        self.gate
            .authorize(&*self.store, Capability::FindingRecord, &resource)?;
        finding.by = self.gate.actor.written_by().to_owned();
        self.store.record_finding(finding)
    }

    /// `finding resolve`: the problem no longer occurs.
    pub fn resolve_finding(&mut self, id: FindingId, reason: &str) -> Result<Finding> {
        self.set_finding(
            Capability::FindingResolve,
            id,
            FindingStatus::Resolved,
            reason,
        )
    }

    /// `finding dismiss`: nobody will remedy it.
    pub fn dismiss_finding(&mut self, id: FindingId, reason: &str) -> Result<Finding> {
        self.set_finding(
            Capability::FindingDismiss,
            id,
            FindingStatus::Dismissed,
            reason,
        )
    }

    fn set_finding(
        &mut self,
        capability: Capability,
        id: FindingId,
        to: FindingStatus,
        reason: &str,
    ) -> Result<Finding> {
        self.gate
            .authorize(&*self.store, capability, &Resource::Finding(id))?;
        let by = self.gate.actor.written_by();
        self.store.set_finding_status(id, to, reason, by)
    }

    /// Authorize `capability` on the ask `id`: a role without it is
    /// refused before the ask is read, whether or not it exists; the rest
    /// are judged with the ask's run.
    fn authorize_ask(&mut self, capability: Capability, id: AskId) -> Result<()> {
        self.gate
            .refuse_ungranted(&*self.store, capability, &Resource::Ask { id, run: None })?;
        let run = self.store.read_ask(id)?.run_id;
        self.gate
            .authorize(&*self.store, capability, &Resource::Ask { id, run })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use anyhow::anyhow;

    use super::*;
    use crate::domain::{
        ActorRole, AnswerAuthority, AskReason, GoalId, RunId, StaticPolicy, TaskId,
    };

    /// A store that holds one ask on run `r1`, records the refusals and
    /// what reached it, and answers every change with the error
    /// `store: <what>`, so a test sees whether a command reached it.
    #[derive(Default)]
    struct Store {
        denials: RefCell<Vec<Value>>,
        answerer: Option<Answerer>,
        writer: Option<String>,
    }

    fn reached(what: &str) -> anyhow::Error {
        anyhow!("store: {what}")
    }

    impl DenialLog for Store {
        fn record_denial(&self, payload: Value) -> Result<()> {
            self.denials.borrow_mut().push(payload);
            Ok(())
        }
    }

    impl DialogueStore for Store {
        fn read_ask(&self, id: AskId) -> Result<Ask> {
            if id != AskId::new(1) {
                return Err(anyhow!("ask {id} does not exist"));
            }
            Ok(Ask {
                recommendation: None,
                confidence: None,
                topics: Vec::new(),
                id,
                kind: AskKind::Decide,
                task_id: Some(TaskId::new(1)),
                run_id: Some(RunId::new("r1").unwrap()),
                question: "q".into(),
                options: Vec::new(),
                answer: None,
                asked_by: "supervisor".into(),
                reason_category: AskReason::RecoveryFailed,
                subject: None,
                affected: Vec::new(),
                created_at: 0,
                answered_at: None,
                closed_at: None,
                finding_id: None,
                request_id: None,
                answered_by: None,
                option_index: None,
                answer_authority: None,
                answer_approval: None,
            })
        }
        fn open_ask(&mut self, ask: NewAsk) -> Result<Value> {
            self.writer = Some(ask.asked_by);
            Err(reached("ask"))
        }
        fn answer(&mut self, _: AskId, _: &str, answerer: Answerer) -> Result<Ask> {
            self.answerer = Some(answerer);
            Err(reached("answer"))
        }
        fn close_ask(&mut self, _: AskId) -> Result<Ask> {
            Err(reached("ask close"))
        }
        fn add_note(&mut self, note: NewNote) -> Result<RunEvent> {
            self.writer = Some(note.by);
            Err(reached("note"))
        }
        fn mark(&mut self, _: MarkChange, by: &str) -> Result<Value> {
            self.writer = Some(by.to_owned());
            Err(reached("mark"))
        }
        fn record_finding(&mut self, finding: NewFinding) -> Result<FindingOutcome> {
            self.writer = Some(finding.by);
            Err(reached("finding record"))
        }
        fn set_finding_status(
            &mut self,
            _: FindingId,
            to: FindingStatus,
            _: &str,
            by: &str,
        ) -> Result<Finding> {
            self.writer = Some(by.to_owned());
            Err(reached(&format!("finding {to:?}")))
        }
    }

    type Command = fn(&mut Dialogue<'_, Store>) -> Result<()>;

    const ASK: AskId = AskId::new(1);

    fn new_ask(kind: AskKind, run: Option<&str>, task: Option<i64>) -> NewAsk {
        NewAsk {
            recommendation: None,
            confidence: None,
            topics: if kind == AskKind::WorkerQuestion {
                vec!["task_overlap".into()]
            } else {
                Vec::new()
            },
            kind,
            task_id: task.map(TaskId::new),
            run_id: run.map(|id| RunId::new(id).unwrap()),
            question: "q".into(),
            options: Vec::new(),
            asked_by: "forged".into(),
            reason_category: AskReason::Scope,
            finding_id: None,
            request_id: None,
        }
    }

    fn new_finding() -> NewFinding {
        NewFinding {
            kind: "stall".into(),
            target: FindingTarget::Task(TaskId::new(1)),
            subject: String::new(),
            summary: "s".into(),
            detail: None,
            impact: None,
            evidence: Vec::new(),
            propose: None,
            by: "forged".into(),
        }
    }

    fn note_on(target: NoteTarget) -> NewNote {
        NewNote {
            target,
            text: "t".into(),
            kind: None,
            by: "forged".into(),
        }
    }

    /// Every command of the module, by name.
    fn commands() -> Vec<(&'static str, Command)> {
        vec![
            ("ask worker_question --run r1", |d| {
                d.ask(new_ask(AskKind::WorkerQuestion, Some("r1"), None))
                    .map(drop)
            }),
            ("ask decide --task 1", |d| {
                d.ask(new_ask(AskKind::Decide, None, Some(1))).map(drop)
            }),
            ("ask blocked --finding", |d| {
                let mut ask = new_ask(AskKind::Blocked, None, None);
                ask.finding_id = Some(FindingId::new(1));
                ask.recommendation = Some("propose".into());
                d.ask(ask).map(drop)
            }),
            ("answer", |d| d.answer(ASK, "yes").map(drop)),
            ("ask close", |d| d.close(ASK).map(drop)),
            ("note --task 1", |d| {
                d.note(note_on(NoteTarget::Task(TaskId::new(1)))).map(drop)
            }),
            ("note --run r2", |d| {
                d.note(note_on(NoteTarget::Run(RunId::new("r2").unwrap())))
                    .map(drop)
            }),
            ("note --goal 1", |d| {
                d.note(note_on(NoteTarget::Goal(GoalId::new(1)))).map(drop)
            }),
            ("mark", |d| {
                d.mark(MarkChange::Record {
                    label: "l".into(),
                    note: None,
                    at: None,
                })
                .map(drop)
            }),
            ("mark --retract", |d| {
                d.mark(MarkChange::Retract(EventId::new(1))).map(drop)
            }),
            ("finding record", |d| {
                d.record_finding(new_finding()).map(drop)
            }),
            ("finding resolve", |d| {
                d.resolve_finding(FindingId::new(1), "gone").map(drop)
            }),
            ("finding dismiss", |d| {
                d.dismiss_finding(FindingId::new(1), "no").map(drop)
            }),
        ]
    }

    enum Outcome {
        Allowed,
        Denied(AuthorizationError),
    }

    fn run(actor: &ActorContext, command: Command) -> (Outcome, Store) {
        let mut store = Store::default();
        let error = command(&mut Dialogue::new(&mut store, actor, &StaticPolicy)).unwrap_err();
        let outcome = match error.downcast::<AuthorizationError>() {
            Ok(denied) => Outcome::Denied(denied),
            Err(error) => {
                assert!(error.to_string().starts_with("store: "), "{error:#}");
                Outcome::Allowed
            }
        };
        (outcome, store)
    }

    /// The commands `actor` may run, by name.
    fn allowed(actor: &ActorContext) -> Vec<&'static str> {
        commands()
            .into_iter()
            .filter(|(name, command)| {
                let (outcome, store) = run(actor, *command);
                let denied = store.denials.into_inner();
                match outcome {
                    Outcome::Allowed => {
                        assert!(denied.is_empty(), "{name}");
                        true
                    }
                    Outcome::Denied(error) => {
                        assert_eq!(denied.len(), 1, "{name}");
                        assert_eq!(denied[0]["role"], actor.role().as_str());
                        assert_eq!(denied[0]["capability"], error.capability.as_str());
                        false
                    }
                }
            })
            .map(|(name, _)| name)
            .collect()
    }

    fn worker() -> ActorContext {
        ActorContext::worker(&RunId::new("r1").unwrap(), TaskId::new(1))
    }

    #[test]
    fn each_role_runs_its_own_dialogue_and_record_commands() {
        let all: Vec<_> = commands().into_iter().map(|(name, _)| name).collect();
        for actor in [
            ActorContext::user(),
            ActorContext::instance(ActorRole::Inbox, 1),
        ] {
            assert_eq!(allowed(&actor), all, "{actor:?}");
        }
        assert_eq!(
            allowed(&worker()),
            ["ask worker_question --run r1", "note --task 1"]
        );
        assert_eq!(
            allowed(&ActorContext::instance(ActorRole::Planner, 1)),
            [
                "note --task 1",
                "note --run r2",
                "note --goal 1",
                "mark",
                "mark --retract",
                "finding resolve",
                "finding dismiss"
            ]
        );
        assert_eq!(
            allowed(&ActorContext::instance(ActorRole::Observer, 1)),
            ["ask blocked --finding", "finding record", "finding resolve"]
        );
        assert_eq!(
            allowed(&ActorContext::instance(ActorRole::Supervisor, 1)),
            [
                "ask worker_question --run r1",
                "ask decide --task 1",
                "ask close",
                "note --task 1",
                "note --run r2",
                "note --goal 1",
                "finding record",
                "finding resolve",
                "finding dismiss"
            ]
        );
        for role in [
            ActorRole::ReviewJob,
            ActorRole::RecoveryJob,
            ActorRole::PlanReviewJob,
            ActorRole::GoalReviewJob,
            ActorRole::ThroughputReviewJob,
            ActorRole::Wrapper,
            ActorRole::Integrator,
        ] {
            assert!(
                allowed(&ActorContext::instance(role, 1)).is_empty(),
                "{role:?}"
            );
        }
    }

    #[test]
    fn workers_jobs_the_observer_and_the_planner_neither_answer_nor_close() {
        let mut actors = vec![
            worker(),
            ActorContext::instance(ActorRole::Observer, 1),
            ActorContext::instance(ActorRole::Planner, 1),
        ];
        actors.extend(
            [
                ActorRole::ReviewJob,
                ActorRole::RecoveryJob,
                ActorRole::PlanReviewJob,
                ActorRole::GoalReviewJob,
                ActorRole::ThroughputReviewJob,
            ]
            .map(|role| ActorContext::instance(role, 1)),
        );
        for actor in actors {
            for command in [(|d| d.answer(ASK, "yes").map(drop)) as Command, |d| {
                d.close(ASK).map(drop)
            }] {
                let (outcome, store) = run(&actor, command);
                let Outcome::Denied(error) = outcome else {
                    panic!("{actor:?} answered or closed");
                };
                assert_eq!(error.reason, DenyReason::NotGranted);
                assert!(store.answerer.is_none());
                // Refused before the ask is read: its run is not recorded.
                assert_eq!(
                    store.denials.into_inner()[0]["resource"],
                    serde_json::json!({"kind": "ask", "id": 1, "run": null})
                );
            }
        }
    }

    #[test]
    fn the_answer_records_the_users_own_authority_or_the_inboxs_delegated_one() {
        for (actor, by, authority) in [
            (ActorContext::user(), "person", AnswerAuthority::User),
            (
                ActorContext::instance(ActorRole::Inbox, 1),
                "inbox",
                AnswerAuthority::Delegated,
            ),
        ] {
            let (outcome, store) = run(&actor, |d| d.answer(ASK, "yes").map(drop));
            assert!(matches!(outcome, Outcome::Allowed));
            let answerer = store.answerer.unwrap();
            assert_eq!((answerer.by, answerer.authority), (by, authority));
        }
    }

    #[test]
    fn a_worker_asks_on_its_own_run_only_and_of_its_own_kind() {
        let me = worker();
        for (ask, reason) in [
            (
                new_ask(AskKind::WorkerQuestion, Some("r2"), None),
                "not on this resource",
            ),
            (
                new_ask(AskKind::WorkerQuestion, None, Some(2)),
                "not on this resource",
            ),
            (
                new_ask(AskKind::Decide, Some("r1"), None),
                "not of this kind",
            ),
        ] {
            let mut store = Store::default();
            let error = Dialogue::new(&mut store, &me, &StaticPolicy)
                .ask(ask)
                .unwrap_err()
                .downcast::<AuthorizationError>()
                .unwrap();
            assert_eq!(error.reason.as_str(), reason);
            assert!(store.writer.is_none(), "reached the store");
            let denied = store.denials.into_inner();
            assert_eq!(denied[0]["resource"]["kind"], "new_ask");
        }
    }

    /// ADR-t451-1 decision 2: the observer's blocked ask carries its
    /// reading as the recommendation, or it reaches no store.
    #[test]
    fn a_blocked_ask_without_a_recommendation_is_refused_before_the_store() {
        let observer = ActorContext::instance(ActorRole::Observer, 1);
        for recommendation in [None, Some("  ")] {
            let mut ask = new_ask(AskKind::Blocked, None, None);
            ask.finding_id = Some(FindingId::new(1));
            ask.recommendation = recommendation.map(str::to_owned);
            let mut store = Store::default();
            let error = Dialogue::new(&mut store, &observer, &StaticPolicy)
                .ask(ask)
                .unwrap_err()
                .to_string();
            assert!(
                error.starts_with("a blocked ask needs --recommend (ADR-t451-1 decision 2)"),
                "{error}"
            );
            assert!(error.contains("finding's --detail"), "{error}");
            assert!(store.writer.is_none(), "reached the store");
        }
        // Another kind still opens without one.
        let mut store = Store::default();
        let error = Dialogue::new(&mut store, &ActorContext::user(), &StaticPolicy)
            .ask(new_ask(AskKind::Decide, None, Some(1)))
            .unwrap_err();
        assert!(error.to_string().starts_with("store: "), "{error:#}");
    }

    #[test]
    fn the_writer_is_the_actors_whatever_the_caller_passed() {
        for (actor, writer) in [
            (ActorContext::user(), "human"),
            (worker(), "worker"),
            (ActorContext::instance(ActorRole::Observer, 1), "observer"),
            (ActorContext::instance(ActorRole::Planner, 1), "planner"),
        ] {
            for (name, command) in commands() {
                let (outcome, store) = run(&actor, command);
                if matches!(outcome, Outcome::Allowed)
                    && store.answerer.is_none()
                    && name != "ask close"
                {
                    assert_eq!(store.writer.as_deref(), Some(writer), "{actor:?} {name}");
                }
            }
        }
    }

    #[test]
    fn an_ask_the_store_cannot_find_is_its_error_not_a_refusal() {
        let mut store = Store::default();
        let user = ActorContext::user();
        let error = Dialogue::new(&mut store, &user, &StaticPolicy)
            .answer(AskId::new(9), "x")
            .unwrap_err();
        assert_eq!(error.to_string(), "ask 9 does not exist");
        assert!(store.denials.into_inner().is_empty());
    }
}
