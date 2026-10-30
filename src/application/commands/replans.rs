//! The replanning commands (ADR-t1521-1, ADR-t1521-2): `replan request`,
//! which only the user and the inbox run (at a person's word), `replan
//! propose`, which a worker runs on its own run only, `replan resolve`,
//! the user's and the inbox's decision on an operation held `blocked`
//! (continue, withdraw or plan again, ADR-t1521-2 decision 2), and the trusted
//! runtime's detection and moves of an operation (the park, the apply),
//! which no command line reaches. Each is authorized as the caller on the
//! run before it reaches the store, and a refusal is recorded as
//! `authorization_denied`. The policy is the
//! [`crate::domain::StaticPolicy`]'s (`docs/design/authorization.md`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use super::{DenialLog, Gate};
use crate::application::RunFiles;
use crate::application::queue_commands::{
    PinToRead, QueueCommand, ReplanResolveParams, ReplanResolveToRead,
};
use crate::domain::replan_snapshot::{self, MANIFEST, PinRead, SnapshotManifest};
use crate::domain::task_replan::{
    NewReplan, ReplanMove, ReplanOperation, ReplanOrigin, ReplanRecorded, ReplanResolved,
    ReplanStatus, ResolveReplan,
};
use crate::domain::{ActorContext, Authorizer, Capability, ReplanId, Resource, RunId, TaskId};

/// What the replanning commands read and change of the queue.
pub trait ReplanStore: DenialLog {
    /// The task of run `run`; `None` for a run the queue does not know.
    fn run_task(&self, run: &RunId) -> Result<Option<TaskId>>;
    /// Record `new` from `origin` as `by_role` with actor id `by_id`: a new
    /// operation on its run, or the task's open one, so that a task keeps
    /// at most one open operation. One that guards the task stays on the
    /// run it fenced, and a request on any run of the task joins it; one
    /// that does not guard is carried to the task's latest run (by a
    /// request on that run too), and a request on another run is refused
    /// while it is open there.
    fn record_replan(
        &mut self,
        new: &NewReplan,
        origin: ReplanOrigin,
        by_role: &str,
        by_id: &str,
    ) -> Result<ReplanRecorded>;
    /// Operation `id`; a missing one is an error.
    fn replan_operation(&self, id: ReplanId) -> Result<ReplanOperation>;
    /// Move operation `id`, read under `generation`, as `step` says.
    fn move_replan(
        &mut self,
        id: ReplanId,
        generation: i64,
        step: &ReplanMove,
    ) -> Result<ReplanOperation>;
    /// Resolve the operation `resolve` names as `by_role` with actor id
    /// `by_id`, in one transaction that reads it again
    /// ([`crate::domain::task_replan::resolve_to`]); `snapshot_problem` is
    /// why its snapshot is not as saved.
    fn resolve_replan(
        &mut self,
        resolve: &ResolveReplan,
        snapshot_problem: Option<&str>,
        by_role: &str,
        by_id: &str,
    ) -> Result<ReplanResolved>;
}

/// The replanning commands of one actor on one store.
pub struct Replans<'a, S> {
    store: &'a mut S,
    gate: Gate<'a>,
}

impl<'a, S: ReplanStore> Replans<'a, S> {
    pub fn new(store: &'a mut S, actor: &'a ActorContext, authorizer: &'a dyn Authorizer) -> Self {
        Self {
            store,
            gate: Gate { actor, authorizer },
        }
    }

    /// `replan request`: a person's (or the inbox's) request on a run,
    /// which also records the operation's planning request.
    pub fn request(&mut self, new: &NewReplan) -> Result<ReplanRecorded> {
        self.record(new, Capability::ReplanRequest, ReplanOrigin::Manual)
    }

    /// `replan propose`: a worker's proposal on its own run, refused on
    /// any other run.
    pub fn propose(&mut self, new: &NewReplan) -> Result<ReplanRecorded> {
        self.record(new, Capability::ReplanPropose, ReplanOrigin::Worker)
    }

    /// The runtime's detection of a run to diagnose: the trusted runtime's
    /// only.
    pub fn detect(&mut self, new: &NewReplan) -> Result<ReplanRecorded> {
        self.record(new, Capability::ReplanPark, ReplanOrigin::Runtime)
    }

    /// `replan resolve`: a person's (or the inbox's) decision on an
    /// operation held `blocked` by anything but its apply, on the run it
    /// is on. `snapshot_problem` reads the snapshot of the operation the
    /// store has (`None`: as the park saved it, or none), so that one that
    /// does not match refuses every choice before anything is written.
    pub fn resolve(
        &mut self,
        resolve: &ResolveReplan,
        snapshot_problem: impl FnOnce(&ReplanOperation) -> Option<String>,
    ) -> Result<ReplanResolved> {
        resolve.validate()?;
        let op = self.to_resolve(resolve.operation)?;
        let problem = snapshot_problem(&op);
        let actor = self.gate.actor;
        self.store.resolve_replan(
            resolve,
            problem.as_deref(),
            actor.role().as_str(),
            actor.actor_id(),
        )
    }

    /// Operation `id` as a person's resolution reads it before it is sent
    /// (the snapshot whose pinned ref its caller reads with Git), refused
    /// as the resolution is to any role but the user and the inbox.
    pub fn to_resolve(&mut self, id: ReplanId) -> Result<ReplanOperation> {
        self.gate
            .refuse_ungranted(&*self.store, Capability::ReplanResolve, &Resource::Queue)?;
        let op = self.store.replan_operation(id)?;
        self.gate.authorize(
            &*self.store,
            Capability::ReplanResolve,
            &Resource::Run {
                id: op.run_id.clone(),
                task: Some(op.task_id),
            },
        )?;
        Ok(op)
    }

    /// Move operation `id` (read under `generation`) as `step` says: into
    /// the replacement ([`Capability::ReplanApply`]) or any other step
    /// ([`Capability::ReplanPark`]), the trusted runtime's only.
    pub fn advance(
        &mut self,
        id: ReplanId,
        generation: i64,
        step: &ReplanMove,
    ) -> Result<ReplanOperation> {
        let capability = if matches!(step.to, ReplanStatus::Applying | ReplanStatus::Applied) {
            Capability::ReplanApply
        } else {
            Capability::ReplanPark
        };
        self.gate
            .refuse_ungranted(&*self.store, capability, &Resource::Queue)?;
        let op = self.store.replan_operation(id)?;
        self.gate.authorize(
            &*self.store,
            capability,
            &Resource::Run {
                id: op.run_id,
                task: Some(op.task_id),
            },
        )?;
        self.store.move_replan(id, generation, step)
    }

    fn record(
        &mut self,
        new: &NewReplan,
        capability: Capability,
        origin: ReplanOrigin,
    ) -> Result<ReplanRecorded> {
        new.validate()?;
        let task = self.store.run_task(&new.run)?;
        self.gate.authorize(
            &*self.store,
            capability,
            &Resource::Run {
                id: new.run.clone(),
                task,
            },
        )?;
        let actor = self.gate.actor;
        self.store
            .record_replan(new, origin, actor.role().as_str(), actor.actor_id())
    }
}

/// Why the snapshot of `op` under `replans_dir` is not as the park saved
/// it, read with `files`: `None` for one that is, or for an operation with
/// no snapshot. Its manifest must name the operation and its run and only
/// relative paths, every file must read back with its size and hash, and
/// `pin`, what the caller read with Git (none runs here), must say that
/// the ref the manifest names still names the head it saved: a pin not
/// read, or read of another ref or head, is a problem too.
pub fn snapshot_problem(
    files: &dyn RunFiles,
    replans_dir: &Path,
    op: &ReplanOperation,
    pin: Option<&PinRead>,
) -> Option<String> {
    let id = op.snapshot_id.as_deref()?;
    let manifest = match check_snapshot_files(files, replans_dir, op, id) {
        Ok(manifest) => manifest,
        Err(error) => return Some(format!("{error:#}")),
    };
    match pin {
        None => Some(format!(
            "{} that pins the saved head {} of snapshot {id} was not read",
            manifest.pinned_ref, manifest.head
        )),
        Some(pin) if pin.pinned_ref != manifest.pinned_ref || pin.head != manifest.head => {
            Some(format!(
                "{} naming {} was read, not {} naming the saved head {} of snapshot {id}",
                pin.pinned_ref, pin.head, manifest.pinned_ref, manifest.head
            ))
        }
        Some(pin) if !pin.names_head => Some(format!(
            "{} does not name the saved head {} of snapshot {id}",
            manifest.pinned_ref, manifest.head
        )),
        Some(_) => None,
    }
}

/// The ref and the head the caller of a resolution of `op` reads with Git
/// ([`PinRead`]): those the manifest of its snapshot names, once it and
/// its files read back as saved; `None` for an operation with no
/// snapshot, or one whose snapshot does not read back (the resolution
/// refuses it then whatever is read).
pub fn pin_to_read(
    files: &dyn RunFiles,
    replans_dir: &Path,
    op: &ReplanOperation,
) -> Option<PinToRead> {
    let id = op.snapshot_id.as_deref()?;
    let manifest = check_snapshot_files(files, replans_dir, op, id).ok()?;
    Some(PinToRead {
        pinned_ref: manifest.pinned_ref,
        head: manifest.head,
    })
}

/// What a resolution's caller checks with Git, outside the queue service
/// (ADR-t3102-3 decision 5): the ports [`prepare_resolve`] reads through.
pub struct ResolveClient<'a> {
    /// The main checkout of the repository whose Git common directory it
    /// is given.
    pub main_checkout: &'a dyn Fn(&Path) -> Result<PathBuf>,
    /// Whether, in the repository of the main checkout given, the first
    /// commit (or ref) is the second or an ancestor of it.
    pub is_ancestor: &'a dyn Fn(&Path, &str, &str) -> Result<bool>,
}

/// `params` as the queue service's client sends `replan resolve`: `call`
/// is asked what the resolution reads ([`QueueCommand::ReplanResolveRead`]),
/// and the ref that pins the operation's snapshot is read with the
/// caller's Git in the main checkout of the repository the queue is bound
/// to. It names the saved head when each is the other's ancestor; a ref
/// that cannot be read, or a queue with no repository or main checkout
/// found, does not. Nothing is read for an operation with no snapshot, or
/// one whose snapshot does not read back: the resolution refuses that
/// itself.
pub fn prepare_resolve(
    params: &mut ReplanResolveParams,
    client: &ResolveClient<'_>,
    mut call: impl FnMut(&QueueCommand) -> Result<serde_json::Value>,
) -> Result<()> {
    let to_read: ReplanResolveToRead = serde_json::from_value(call(
        &QueueCommand::ReplanResolveRead(ReplanResolveParams {
            pin: None,
            ..params.clone()
        }),
    )?)
    .context("the resolution's read does not read")?;
    let Some(pin) = to_read.pin else {
        return Ok(());
    };
    let names_head = to_read.repository.as_deref().is_some_and(|common_dir| {
        (client.main_checkout)(Path::new(common_dir))
            .and_then(|main| {
                Ok((client.is_ancestor)(&main, &pin.head, &pin.pinned_ref)?
                    && (client.is_ancestor)(&main, &pin.pinned_ref, &pin.head)?)
            })
            .unwrap_or(false)
    });
    params.pin = Some(PinRead {
        pinned_ref: pin.pinned_ref,
        head: pin.head,
        names_head,
    });
    Ok(())
}

/// The manifest of snapshot `id` of `op`, once it and every file it names
/// read back as the park saved them ([`snapshot_problem`]).
pub fn check_snapshot_files(
    files: &dyn RunFiles,
    replans_dir: &Path,
    op: &ReplanOperation,
    id: &str,
) -> Result<SnapshotManifest> {
    let dir = replan_snapshot::snapshot_dir(replans_dir, id);
    let manifest: SnapshotManifest = serde_json::from_slice(
        &files
            .read(&dir.join(MANIFEST))
            .with_context(|| format!("read the manifest of snapshot {id}"))?,
    )
    .with_context(|| format!("parse the manifest of snapshot {id}"))?;
    ensure!(
        manifest.operation == op.id && manifest.run_id == op.run_id,
        "snapshot {id} is of replan operation {} and run {}, not of operation {} and run {}",
        manifest.operation,
        manifest.run_id,
        op.id,
        op.run_id
    );
    manifest.check_paths()?;
    let mut saved = Vec::new();
    for file in &manifest.files {
        if let Ok(contents) = files.read(&dir.join(&file.name)) {
            saved.push((
                file.name.clone(),
                contents.len() as u64,
                replan_snapshot::sha256(&contents),
            ));
        }
    }
    replan_snapshot::check_saved(&manifest, &saved)?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::Value;

    use super::*;
    use crate::domain::task_replan::{
        Diagnosis, OriginalConditions, ReplanResolution, acceptance_items,
    };
    use crate::domain::{ActorRole, StaticPolicy};

    #[derive(Default)]
    struct Store {
        denials: RefCell<Vec<Value>>,
        recorded: Vec<(ReplanOrigin, String)>,
        moved: Vec<ReplanStatus>,
        resolved: Vec<(ReplanResolution, Option<String>, String)>,
        operation: Option<ReplanOperation>,
    }

    impl DenialLog for Store {
        fn record_denial(&self, payload: Value) -> Result<()> {
            self.denials.borrow_mut().push(payload);
            Ok(())
        }
    }

    impl ReplanStore for Store {
        fn run_task(&self, _: &RunId) -> Result<Option<TaskId>> {
            Ok(Some(TaskId::new(7)))
        }

        fn record_replan(
            &mut self,
            _: &NewReplan,
            origin: ReplanOrigin,
            by_role: &str,
            _: &str,
        ) -> Result<ReplanRecorded> {
            self.recorded.push((origin, by_role.to_owned()));
            anyhow::bail!("recorded")
        }

        fn replan_operation(&self, _: ReplanId) -> Result<ReplanOperation> {
            self.operation.clone().context("no operation in this store")
        }

        fn move_replan(
            &mut self,
            _: ReplanId,
            _: i64,
            step: &ReplanMove,
        ) -> Result<ReplanOperation> {
            self.moved.push(step.to);
            anyhow::bail!("moved")
        }

        fn resolve_replan(
            &mut self,
            resolve: &ResolveReplan,
            snapshot_problem: Option<&str>,
            by_role: &str,
            _: &str,
        ) -> Result<ReplanResolved> {
            self.resolved.push((
                resolve.resolution,
                snapshot_problem.map(str::to_owned),
                by_role.to_owned(),
            ));
            anyhow::bail!("resolved")
        }
    }

    fn new(run: &str) -> NewReplan {
        NewReplan {
            run: RunId::new(run).unwrap(),
            reason: "the review keeps sending it back".into(),
            evidence: Vec::new(),
        }
    }

    #[test]
    fn each_role_records_only_the_origin_it_may_and_a_worker_only_on_its_run() {
        let worker = ActorContext::worker(&RunId::new("own").unwrap(), TaskId::new(7));
        let cases: [(ActorContext, bool, bool); 6] = [
            (ActorContext::user(), true, false),
            (
                ActorContext::instance(ActorRole::Inbox, "inbox"),
                true,
                false,
            ),
            (worker, false, true),
            (ActorContext::instance(ActorRole::Planner, 3), false, false),
            (ActorContext::instance(ActorRole::Observer, 1), false, false),
            (
                ActorContext::instance(ActorRole::ReviewJob, 1),
                false,
                false,
            ),
        ];
        for (actor, requests, proposes) in cases {
            let mut store = Store::default();
            let mut replans = Replans::new(&mut store, &actor, &StaticPolicy);
            let requested = replans.request(&new("own"));
            let proposed = replans.propose(&new("own"));
            let elsewhere = replans.propose(&new("other"));
            assert!(elsewhere.is_err());
            let error = |result: Result<ReplanRecorded>| result.unwrap_err().to_string();
            assert_eq!(error(requested) == "recorded", requests, "{actor:?}");
            assert_eq!(error(proposed) == "recorded", proposes, "{actor:?}");
            let expected: Vec<ReplanOrigin> = [
                requests.then_some(ReplanOrigin::Manual),
                proposes.then_some(ReplanOrigin::Worker),
            ]
            .into_iter()
            .flatten()
            .collect();
            assert_eq!(
                store
                    .recorded
                    .iter()
                    .map(|(origin, _)| *origin)
                    .collect::<Vec<_>>(),
                expected
            );
            // Every refusal is recorded, the worker's on another run too.
            let refused = usize::from(!requests) + usize::from(!proposes) + 1;
            assert_eq!(store.denials.borrow().len(), refused, "{actor:?}");
        }
    }

    #[test]
    fn only_the_runtime_detects_or_moves_an_operation() {
        let step = ReplanMove {
            to: ReplanStatus::PauseRequested,
            reason: "diagnosed".into(),
            snapshot_id: None,
            diagnosis: None,
        };
        for actor in [
            ActorContext::user(),
            ActorContext::instance(ActorRole::Inbox, "inbox"),
            ActorContext::instance(ActorRole::Planner, 3),
            ActorContext::worker(&RunId::new("own").unwrap(), TaskId::new(7)),
        ] {
            let mut store = Store::default();
            let mut replans = Replans::new(&mut store, &actor, &StaticPolicy);
            let detected = replans.detect(&new("own")).unwrap_err();
            assert!(detected.to_string().contains("replan.park"), "{detected}");
            let moved = replans.advance(ReplanId::new(1), 1, &step).unwrap_err();
            assert!(moved.to_string().contains("replan.park"), "{moved}");
            assert!(store.recorded.is_empty());
            assert!(store.moved.is_empty());
        }
        let supervisor = ActorContext::instance(ActorRole::Supervisor, 1);
        let mut store = Store::default();
        let mut replans = Replans::new(&mut store, &supervisor, &StaticPolicy);
        assert_eq!(
            replans.detect(&new("own")).unwrap_err().to_string(),
            "recorded"
        );
        // Read before it moves: this store has none.
        assert!(replans.advance(ReplanId::new(1), 1, &step).is_err());
        assert_eq!(
            store.recorded,
            vec![(ReplanOrigin::Runtime, "supervisor".into())]
        );
    }

    fn blocked() -> ReplanOperation {
        ReplanOperation {
            id: ReplanId::new(1),
            task_id: TaskId::new(7),
            run_id: RunId::new("own").unwrap(),
            goal_id: None,
            origin: ReplanOrigin::Manual,
            status: ReplanStatus::Blocked,
            status_reason: None,
            generation: 2,
            return_phase: Some(ReplanStatus::Planning),
            reason: "too long".into(),
            conditions: OriginalConditions {
                title: "t".into(),
                description: "d".into(),
                acceptance: acceptance_items("a"),
                paths: Vec::new(),
                verify: Vec::new(),
                evidence: Value::Null,
                predecessors: Vec::new(),
                successors: Vec::new(),
                goal: None,
            },
            task_version: String::new(),
            goal_acceptance_version: None,
            diagnosis: Diagnosis::default(),
            snapshot_id: Some("s".into()),
            request_id: None,
            request_status: None,
            proposal_id: None,
            requested_by: "user".into(),
            requested_by_id: "user".into(),
            mappings: Vec::new(),
            replaced_by: Vec::new(),
            created_at: 0,
            updated_at: 0,
            closed_at: None,
        }
    }

    /// Only a person and the inbox resolve an operation held `blocked`:
    /// a worker (on its own run too), a planner, a job, the observer and
    /// the supervisor are refused before the store is asked, and each
    /// refusal is recorded. The snapshot read goes to the store with it.
    #[test]
    fn only_a_person_and_the_inbox_resolve_a_blocked_operation() {
        let resolve = ResolveReplan {
            operation: ReplanId::new(1),
            generation: 2,
            resolution: ReplanResolution::Continue,
            reason: "go on".into(),
        };
        let mut actors = vec![
            (ActorContext::user(), true),
            (ActorContext::instance(ActorRole::Inbox, "inbox"), true),
            (
                ActorContext::worker(&RunId::new("own").unwrap(), TaskId::new(7)),
                false,
            ),
            (ActorContext::instance(ActorRole::Planner, 3), false),
            (ActorContext::instance(ActorRole::Observer, 1), false),
            (ActorContext::instance(ActorRole::Supervisor, 1), false),
        ];
        for job in [
            ActorRole::ReviewJob,
            ActorRole::RecoveryJob,
            ActorRole::PlanReviewJob,
            ActorRole::GoalReviewJob,
        ] {
            actors.push((ActorContext::instance(job, 1), false));
        }
        for (actor, resolves) in actors {
            let mut store = Store {
                operation: Some(blocked()),
                ..Store::default()
            };
            let error = Replans::new(&mut store, &actor, &StaticPolicy)
                .resolve(&resolve, |_| Some("a.diff differs".into()))
                .unwrap_err()
                .to_string();
            if resolves {
                assert_eq!(error, "resolved", "{actor:?}");
                assert_eq!(
                    store.resolved,
                    vec![(
                        ReplanResolution::Continue,
                        Some("a.diff differs".to_owned()),
                        actor.role().as_str().to_owned()
                    )]
                );
                assert!(store.denials.borrow().is_empty());
            } else {
                assert!(error.contains("replan.resolve"), "{actor:?}: {error}");
                assert!(store.resolved.is_empty());
                assert_eq!(store.denials.borrow().len(), 1, "{actor:?}");
            }
        }
        // A blank reason is refused before anything is read.
        let mut store = Store::default();
        let blank = ResolveReplan {
            reason: " ".into(),
            ..resolve
        };
        assert!(
            Replans::new(&mut store, &ActorContext::user(), &StaticPolicy)
                .resolve(&blank, |_| None)
                .is_err()
        );
        assert!(store.denials.borrow().is_empty());
    }

    /// A resolution's caller reads the ref the operation's snapshot pins
    /// with its Git in the main checkout of the queue's repository, and
    /// sends whether it names the saved head (each the other's ancestor);
    /// a ref that names another commit, a Git that fails or a queue with
    /// no repository or main checkout does not name it, and nothing is read
    /// for an operation the read names no pin of.
    #[test]
    fn a_resolution_s_caller_reads_the_pinned_ref_with_its_git_before_it_sends_it() {
        use crate::application::queue_commands::{PinToRead, ReplanResolveToRead};
        fn prepared(
            to_read: &ReplanResolveToRead,
            is_ancestor: &dyn Fn(&Path, &str, &str) -> Result<bool>,
        ) -> (Option<PinRead>, Vec<QueueCommand>) {
            let mut params = ReplanResolveParams {
                id: 7,
                generation: 3,
                resolution: ReplanResolution::Continue,
                reason: "go on".into(),
                pin: None,
            };
            let mut sent = Vec::new();
            prepare_resolve(
                &mut params,
                &ResolveClient {
                    main_checkout: &|common_dir| {
                        ensure!(common_dir == Path::new("/repo/.git"), "no main checkout");
                        Ok(PathBuf::from("/repo"))
                    },
                    is_ancestor,
                },
                |command| {
                    sent.push(command.clone());
                    Ok(serde_json::to_value(to_read)?)
                },
            )
            .unwrap();
            (params.pin, sent)
        }
        let pinned = ReplanResolveToRead {
            pin: Some(PinToRead {
                pinned_ref: "refs/dagq/replans/7".into(),
                head: "abc".into(),
            }),
            repository: Some("/repo/.git".into()),
        };
        let pin = |names_head| PinRead {
            pinned_ref: "refs/dagq/replans/7".into(),
            head: "abc".into(),
            names_head,
        };
        let (read, sent) = prepared(&pinned, &|main, _, _| Ok(main == Path::new("/repo")));
        assert_eq!(read, Some(pin(true)));
        assert!(
            matches!(&sent[..], [QueueCommand::ReplanResolveRead(read)] if read.id == 7 && read.pin.is_none()),
            "{sent:?}"
        );
        // A ref moved past the head names it one way only.
        let (read, _) = prepared(&pinned, &|_, ancestor, _| Ok(ancestor == "abc"));
        assert_eq!(read, Some(pin(false)));
        let (read, _) = prepared(&pinned, &|_, _, _| anyhow::bail!("no such ref"));
        assert_eq!(read, Some(pin(false)));
        for repository in [None, Some("/elsewhere/.git".to_owned())] {
            let unbound = ReplanResolveToRead {
                repository,
                ..pinned.clone()
            };
            let (read, _) = prepared(&unbound, &|_, _, _| Ok(true));
            assert_eq!(read, Some(pin(false)));
        }
        let none = ReplanResolveToRead {
            pin: None,
            ..pinned
        };
        let (read, _) = prepared(&none, &|_, _, _| panic!("no Git runs with no pin"));
        assert_eq!(read, None);
    }
}
