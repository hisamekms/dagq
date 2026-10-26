//! The hold of a task and its release (ADR-t1879-1): `hold` and `release`,
//! which only a person at a plain terminal and the inbox run. Each is
//! authorized as the caller on the task (a run named is resolved to its
//! task first) before it reaches the store, and a refusal is recorded as
//! `authorization_denied`. The policy is the [`crate::domain::StaticPolicy`]'s
//! (`docs/design/authorization.md`).

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{DenialLog, Gate};
use crate::application::container_image::KeptImageRecord;
use crate::application::inherit::{ReleasedBranches, ReleasedRead};
use crate::application::queue_commands::{
    QueueCommand, ReleaseImagesParams, ReleaseParams, ReleaseToRead, ReleaseWork,
};
use crate::domain::task_hold::{
    self, HoldRecorded, HoldState, HoldTarget, ProgressVersion, ReleaseChoice, ReleaseFacts,
    ReleaseReadAt, ReleaseRecorded, TaskHold,
};
use crate::domain::{
    ActorContext, Authorizer, Capability, CommitSha, Resource, RunId, Task, TaskId, TaskRun,
};

/// What a release reads before its store's transaction: the task, its
/// open hold, its latest run with its [`ProgressVersion`], whether it has
/// a session a resume goes on in and whether a result was kept for it
/// ([`ReleaseFacts`]).
#[derive(Debug, Clone)]
pub struct ReleaseView {
    pub task: Task,
    pub hold: Option<TaskHold>,
    pub run: Option<TaskRun>,
    pub version: Option<ProgressVersion>,
    pub session: bool,
    pub kept: bool,
}

impl ReleaseView {
    /// What the release decides on ([`task_hold::release_decision`]).
    pub fn facts(&self) -> ReleaseFacts {
        ReleaseFacts {
            task: self.task.status(),
            run: self.run.as_ref().map(TaskRun::status),
            run_provider: self.run.as_ref().map(TaskRun::actual_provider),
            session: self.session,
            kept: self.kept,
        }
    }
}

/// A release as the store applies it: what it names, the provider and
/// model it chose, and, for a release that ends the run for a new one,
/// the work read of that run ([`ReleasedBranches::released_work`]) with
/// the hold and the run's version it was read at, which the store's
/// transaction checks again ([`task_hold::check_release_read`]).
#[derive(Debug, Clone)]
pub struct ReleaseRequest {
    pub target: HoldTarget,
    pub choice: ReleaseChoice,
    pub work: Option<(ReleaseReadAt, ReleasedRead)>,
}

/// What the hold commands read and change of the queue.
pub trait HoldStore: DenialLog {
    /// The task of run `run`; `None` for a run the queue does not know.
    fn run_task(&self, run: &RunId) -> Result<Option<TaskId>>;
    /// Hold the task `target` names, as `by_role` with actor id `by_id`.
    fn hold_task(
        &mut self,
        target: &HoldTarget,
        reason: &str,
        until: Option<i64>,
        by_role: &str,
        by_id: &str,
    ) -> Result<HoldRecorded>;
    /// What a release of the task `target` names reads before its
    /// transaction.
    fn release_view(&self, target: &HoldTarget) -> Result<ReleaseView>;
    /// Release the open hold of the task `request.target` names. A release
    /// that carries the run's work over calls `keep` with the run and the
    /// head to keep inside its transaction, before it commits: an error
    /// refuses the release, which changes nothing.
    fn release_task(
        &mut self,
        request: &ReleaseRequest,
        by_role: &str,
        by_id: &str,
        keep: &dyn Fn(&TaskRun, &CommitSha) -> Result<()>,
    ) -> Result<ReleaseRecorded>;
    /// Record `records`, the image's records a release's client kept
    /// ([`Holds::record_images`]), as the queue's events of the caller, in
    /// one transaction.
    fn record_images(
        &mut self,
        records: &[(crate::domain::EventKind, serde_json::Value)],
    ) -> Result<()>;
}

/// The hold commands of one actor on one store.
pub struct Holds<'a, S> {
    store: &'a mut S,
    gate: Gate<'a>,
}

impl<'a, S: HoldStore> Holds<'a, S> {
    pub fn new(store: &'a mut S, actor: &'a ActorContext, authorizer: &'a dyn Authorizer) -> Self {
        Self {
            store,
            gate: Gate { actor, authorizer },
        }
    }

    /// `hold`: hold the task `target` names for `reason`, with `until` the
    /// time to remind a person (unix seconds).
    pub fn hold(
        &mut self,
        target: &HoldTarget,
        reason: &str,
        until: Option<i64>,
    ) -> Result<HoldRecorded> {
        self.authorize(target)?;
        let actor = self.gate.actor;
        self.store.hold_task(
            target,
            reason,
            until,
            actor.role().as_str(),
            actor.actor_id(),
        )
    }

    /// `release`: release the open hold of the task `target` names, with
    /// the provider and model of `choice` for the task's later turns and
    /// runs (ADR-t1879-1). Once the run waits, a release that ends it for
    /// a new one ([`task_hold::ends_run`]) reads its work first
    /// (`branches`: its own commits, and its uncommitted work saved as a
    /// commit; a release whose work cannot be saved is refused) and keeps
    /// the head the next run carries over under `refs/dagq/runs/<run-id>`
    /// in the store's transaction, before it commits: a head that cannot be
    /// kept refuses the release, which then changes nothing, as does a
    /// task whose hold or run moved since the work was read
    /// (`release_read_moved`). While the run settles nothing is read: the
    /// store refuses it with `hold_not_settled`.
    pub fn release(
        &mut self,
        target: &HoldTarget,
        choice: ReleaseChoice,
        branches: &dyn ReleasedBranches,
    ) -> Result<ReleaseRecorded> {
        self.authorize(target)?;
        let view = self.store.release_view(target)?;
        let work = match to_read(&view, &choice) {
            Some((at, run)) => Some((at, branches.released_work(&run)?)),
            None => None,
        };
        let actor = self.gate.actor;
        self.store.release_task(
            &ReleaseRequest {
                target: target.clone(),
                choice,
                work,
            },
            actor.role().as_str(),
            actor.actor_id(),
            &|run, head| branches.keep(run, head),
        )
    }

    /// What `release` of the task `target` with `choice` reads before the
    /// store's transaction, for a caller that reads it with Git itself (the
    /// queue service's client, ADR-t3102-3 decision 5): the run whose work
    /// it carries over, with the hold and the version it is read at;
    /// `None` when it reads none.
    pub fn release_read(
        &mut self,
        target: &HoldTarget,
        choice: &ReleaseChoice,
    ) -> Result<Option<(ReleaseReadAt, TaskRun)>> {
        self.authorize(target)?;
        let view = self.store.release_view(target)?;
        Ok(to_read(&view, choice))
    }

    /// `release` with the work its caller read and kept already
    /// ([`Self::release_read`], [`ReleasedBranches`]): the store's
    /// transaction checks the read again as for [`Self::release`], and a
    /// release that ends the run while `work` is none or read under another
    /// hold or version is refused and changes nothing. No Git runs here.
    pub fn release_read_by_caller(
        &mut self,
        target: &HoldTarget,
        choice: ReleaseChoice,
        work: Option<(ReleaseReadAt, ReleasedRead)>,
    ) -> Result<ReleaseRecorded> {
        self.authorize(target)?;
        let actor = self.gate.actor;
        self.store.release_task(
            &ReleaseRequest {
                target: target.clone(),
                choice,
                work,
            },
            actor.role().as_str(),
            actor.actor_id(),
            &|_, _| Ok(()),
        )
    }

    /// The records of the image the client of a release of `target` made
    /// ready for the run's facts-only turn as it read the run's work
    /// ([`crate::application::container_image::KeptImageRecords`]), recorded
    /// as the caller's once authorized as the release is, whether the work
    /// was read or not, as the store records them where the turn runs on
    /// the DB. Each is stamped at `now`, the time it is recorded here (Unix
    /// seconds), so a failure holds the container from then and from no
    /// time its sender names.
    pub fn record_images(
        &mut self,
        target: &HoldTarget,
        records: &[KeptImageRecord],
        now: i64,
    ) -> Result<ImagesRecorded> {
        self.authorize(target)?;
        if !records.is_empty() {
            let records: Vec<_> = records.iter().map(|record| record.recorded(now)).collect();
            self.store.record_images(&records)?;
        }
        Ok(ImagesRecorded {
            recorded: records.len(),
        })
    }

    fn authorize(&self, target: &HoldTarget) -> Result<()> {
        let resource = match target {
            HoldTarget::Task(task) => Resource::task(*task),
            HoldTarget::Run(run) => Resource::Run {
                id: run.clone(),
                task: self.store.run_task(run)?,
            },
        };
        self.gate
            .authorize(&*self.store, Capability::TaskHold, &resource)
    }
}

/// What [`Holds::record_images`] recorded: how many records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ImagesRecorded {
    pub recorded: usize,
}

/// What a release's caller reads and keeps the run's work with, outside
/// the queue service (ADR-t3102-3 decision 5): the ports
/// [`prepare_release`] reads through.
pub struct ReleaseClient<'a> {
    /// The main checkout of the repository whose Git common directory it
    /// is given.
    pub main_checkout: &'a dyn Fn(&Path) -> Result<PathBuf>,
    /// Runs its callback with the branches of the runs' repositories, their
    /// containers bound to the main checkout given (none when none was
    /// found), and returns the records of the images a facts-only turn made
    /// ready meanwhile, in the order made: none for a caller that runs no
    /// such turn.
    pub branches: &'a WithBranches<'a>,
}

/// [`ReleaseClient::branches`]: given the main checkout, it runs the
/// callback with the branches and returns the image's records it kept.
pub type WithBranches<'a> =
    dyn Fn(Option<PathBuf>, &mut dyn FnMut(&dyn ReleasedBranches)) -> Vec<KeptImageRecord> + 'a;

/// `params` as the queue service's client sends `release` (ADR-t3102-3
/// decision 5): `call` is asked what the release reads
/// ([`QueueCommand::ReleaseRead`]), and the work of a run it names is read
/// and kept with the caller's Git ([`read_and_keep`]) in the main checkout
/// of the repository the queue is bound to (none when it is bound to none
/// or no main checkout is found). The records of the images a facts-only
/// turn of the run made ready are sent ([`QueueCommand::ReleaseImages`]),
/// only when there are some, whether the work was read or not: when both
/// fail, the read's error is the release's and the records' is logged;
/// when only the records fail, theirs is.
pub fn prepare_release(
    params: &mut ReleaseParams,
    client: &ReleaseClient<'_>,
    mut call: impl FnMut(&QueueCommand) -> Result<serde_json::Value>,
) -> Result<()> {
    use anyhow::Context;
    let to_read: ReleaseToRead =
        serde_json::from_value(call(&QueueCommand::ReleaseRead(params.clone()))?)
            .context("the release's read does not read")?;
    let Some(read) = to_read.read else {
        return Ok(());
    };
    let main = to_read
        .repository
        .as_deref()
        .and_then(|common_dir| (client.main_checkout)(Path::new(common_dir)).ok());
    let mut read = Some(read);
    let mut done = None;
    let records = (client.branches)(main, &mut |branches| {
        if let Some(read) = read.take() {
            done = Some(read_and_keep(read, branches));
        }
    });
    let done = done.context("the release's work was not read")?;
    if !records.is_empty() {
        let sent = call(&QueueCommand::ReleaseImages(ReleaseImagesParams {
            target: params.target.clone(),
            records,
        }));
        match (&done, sent) {
            (_, Ok(_)) => {}
            (Ok(_), Err(error)) => return Err(error),
            (Err(_), Err(error)) => tracing::warn!(
                error = %format_args!("{error:#}"),
                "the image's records of a release's facts-only turn were not recorded: {error:#}"
            ),
        }
    }
    let (at, read) = done?;
    params.work = Some(ReleaseWork { at, read });
    Ok(())
}

/// The work of the run `read` names, read by `branches` and its head kept
/// there under `refs/dagq/runs/<run-id>`: what the queue service's client
/// does with Git before it sends the release
/// ([`Holds::release_read_by_caller`]). Work that cannot be read or kept is
/// an error, and the release is not sent; one the store refuses after the
/// head was kept leaves only the ref, which holds the run's own work, as a
/// commit that fails after the keep does on the DB.
pub fn read_and_keep(
    read: (ReleaseReadAt, TaskRun),
    branches: &dyn ReleasedBranches,
) -> Result<(ReleaseReadAt, ReleasedRead)> {
    use anyhow::Context;
    let (at, run) = read;
    let work = branches.released_work(&run)?;
    if let Some(head) = work.head() {
        branches.keep(&run, head).with_context(|| {
            format!(
                "the work run {} carries over could not be kept as {}; nothing is released",
                run.id(),
                crate::application::inherit::run_ref(run.id())
            )
        })?;
    }
    Ok((at, work))
}

/// The run a release with `choice` reads the work of, and the hold and
/// version it is read at: once the run waits under the open hold and the
/// release ends it for a new one ([`task_hold::ends_run`]).
fn to_read(view: &ReleaseView, choice: &ReleaseChoice) -> Option<(ReleaseReadAt, TaskRun)> {
    match (&view.hold, &view.run, &view.version) {
        (Some(hold), Some(run), Some(version))
            if hold.state() == HoldState::Held && task_hold::ends_run(view.facts(), choice) =>
        {
            Some((
                ReleaseReadAt {
                    hold: hold.id,
                    run: run.id().clone(),
                    version: version.clone(),
                },
                run.clone(),
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::Value;

    use super::*;
    use crate::domain::{ActorRole, HoldId, StaticPolicy};

    #[derive(Default)]
    struct Store {
        denials: RefCell<Vec<Value>>,
        held: Vec<(HoldTarget, String)>,
        released: Vec<HoldTarget>,
        images: Vec<(crate::domain::EventKind, Value)>,
    }

    impl DenialLog for Store {
        fn record_denial(&self, payload: Value) -> Result<()> {
            self.denials.borrow_mut().push(payload);
            Ok(())
        }
    }

    fn hold(task: TaskId) -> TaskHold {
        TaskHold {
            id: HoldId::new(1),
            task_id: task,
            run_id: None,
            reason: "why".into(),
            until: None,
            by_role: "user".into(),
            by_id: "user".into(),
            created_at: 1,
            settled_at: Some(1),
        }
    }

    impl HoldStore for Store {
        fn record_images(
            &mut self,
            records: &[(crate::domain::EventKind, serde_json::Value)],
        ) -> Result<()> {
            self.images.extend(records.iter().cloned());
            Ok(())
        }

        fn run_task(&self, _: &RunId) -> Result<Option<TaskId>> {
            Ok(Some(TaskId::new(7)))
        }

        fn hold_task(
            &mut self,
            target: &HoldTarget,
            _: &str,
            _: Option<i64>,
            by_role: &str,
            _: &str,
        ) -> Result<HoldRecorded> {
            self.held.push((target.clone(), by_role.to_owned()));
            Ok(HoldRecorded {
                hold: hold(TaskId::new(7)),
                state: HoldState::Held,
            })
        }

        fn release_view(&self, _: &HoldTarget) -> Result<ReleaseView> {
            let task = Task::new(
                TaskId::new(7),
                crate::domain::NewTask {
                    title: "t".into(),
                    description: String::new(),
                    acceptance: String::new(),
                    verification_commands: Vec::new(),
                    dependencies: Vec::new(),
                    goal_dependencies: Vec::new(),
                    goal_id: None,
                    context: String::new(),
                    required_evidence: Vec::new(),
                    paths: Vec::new(),
                    priority: Default::default(),
                    change: None,
                    provider: None,
                    worker_mode: None,
                    wait_for_build: false,
                    execution_class: Default::default(),
                },
                "now".into(),
            )?;
            Ok(ReleaseView {
                task,
                hold: Some(hold(TaskId::new(7))),
                run: None,
                version: None,
                session: false,
                kept: false,
            })
        }

        fn release_task(
            &mut self,
            request: &ReleaseRequest,
            _: &str,
            _: &str,
            _: &dyn Fn(&TaskRun, &CommitSha) -> Result<()>,
        ) -> Result<ReleaseRecorded> {
            self.released.push(request.target.clone());
            Ok(ReleaseRecorded {
                hold: hold(TaskId::new(7)),
                continuation: "claim",
                decision: task_hold::ReleaseDecision::Claim,
                provider: None,
                model: None,
                inherit: None,
                replan_carry_ended: None,
            })
        }
    }

    /// Branches a release with no run never reads.
    struct NoBranches;

    impl ReleasedBranches for NoBranches {
        fn released_work(&self, _: &TaskRun) -> Result<ReleasedRead> {
            anyhow::bail!("no run's work is read")
        }
        fn keep(&self, _: &TaskRun, _: &CommitSha) -> Result<()> {
            anyhow::bail!("no head is kept")
        }
    }

    /// The release a client reads with Git itself is authorized as the
    /// release is, both when it asks what to read and when it sends what
    /// it read; with no run to read it reads none and reaches the store
    /// with no work.
    #[test]
    fn a_release_read_by_its_caller_is_authorized_as_the_release_is() {
        let run = RunId::new("r1").unwrap();
        let cases: [(ActorContext, bool); 3] = [
            (ActorContext::user(), true),
            (ActorContext::instance(ActorRole::Inbox, "inbox"), true),
            (ActorContext::worker(&run, TaskId::new(7)), false),
        ];
        for (actor, may) in cases {
            let mut store = Store::default();
            let mut holds = Holds::new(&mut store, &actor, &StaticPolicy);
            let target = HoldTarget::Run(run.clone());
            let read = holds.release_read(&target, &ReleaseChoice::default());
            let released = holds.release_read_by_caller(&target, ReleaseChoice::default(), None);
            assert_eq!(read.is_ok(), may, "{actor:?}");
            assert_eq!(released.is_ok(), may, "{actor:?}");
            if may {
                assert!(read.unwrap().is_none());
            }
            assert_eq!(store.released.len(), usize::from(may));
            assert_eq!(store.denials.borrow().len(), if may { 0 } else { 2 });
        }
    }

    /// The image's records a release's client kept are recorded for the
    /// user and the inbox as their release is authorized, each stamped at
    /// the time it is recorded, and refused and the refusal recorded for a
    /// worker.
    #[test]
    fn a_release_s_image_records_are_authorized_as_the_release_is() {
        let run = RunId::new("r1").unwrap();
        let records = [KeptImageRecord::ContainerImageFailed {
            image: Some("i".into()),
            reason: "no image".into(),
        }];
        for (actor, may) in [
            (ActorContext::user(), true),
            (ActorContext::instance(ActorRole::Inbox, "inbox"), true),
            (ActorContext::worker(&run, TaskId::new(7)), false),
        ] {
            let mut store = Store::default();
            let recorded = Holds::new(&mut store, &actor, &StaticPolicy).record_images(
                &HoldTarget::Run(run.clone()),
                &records,
                42,
            );
            assert_eq!(recorded.is_ok(), may, "{actor:?}");
            if may {
                assert_eq!(recorded.unwrap(), ImagesRecorded { recorded: 1 });
                assert_eq!(store.images, [records[0].recorded(42)]);
                assert_eq!(store.images[0].1["at"], 42);
            }
            assert_eq!(store.images.len(), usize::from(may));
            assert_eq!(store.denials.borrow().len(), usize::from(!may));
            let none = Holds::new(&mut store, &ActorContext::user(), &StaticPolicy)
                .record_images(&HoldTarget::Run(run.clone()), &[], 42)
                .unwrap();
            assert_eq!(none, ImagesRecorded { recorded: 0 });
            assert_eq!(store.images.len(), usize::from(may));
        }
    }

    /// A run whose work a release reads.
    fn released_run() -> TaskRun {
        use crate::domain::{Provider, RunRecord, RunStatus};
        TaskRun::restore(RunRecord {
            id: RunId::new("r1").unwrap(),
            task_id: TaskId::new(7),
            status: RunStatus::Interrupted,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: crate::domain::worker::WorkerMode::Headless,
            base_commit: CommitSha::try_from("a".repeat(40)).unwrap(),
            branch: Some("dagq/r1".into()),
            worktree_path: Some("/w".into()),
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: Some("/q/runs/r1".into()),
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap()
    }

    /// Branches that read the work `own` (none: fail to read it) and keep
    /// what they are asked to.
    struct Work {
        own: Option<CommitSha>,
        kept: RefCell<Vec<CommitSha>>,
    }

    impl ReleasedBranches for Work {
        fn released_work(&self, _: &TaskRun) -> Result<ReleasedRead> {
            let own = anyhow::Context::context(self.own.clone(), "the worktree is gone")?;
            Ok(ReleasedRead {
                own: Some(own),
                saved: None,
            })
        }
        fn keep(&self, _: &TaskRun, head: &CommitSha) -> Result<()> {
            self.kept.borrow_mut().push(head.clone());
            Ok(())
        }
    }

    /// What a [`prepare_release`] did: the commands it sent, the main
    /// checkout its branches were given, the heads they kept, and its
    /// outcome with the params it left.
    struct Prepared {
        sent: Vec<&'static str>,
        main: Option<Option<PathBuf>>,
        kept: Vec<CommitSha>,
        outcome: Result<ReleaseParams>,
    }

    /// [`prepare_release`] of a release whose read names `read` (a run or
    /// none) on a queue bound to `/r/.git`, whose main checkout is found
    /// when `found`, with branches that read the work when `reads` and
    /// make `records`, and a service that refuses the records when
    /// `records_refused`.
    fn prepare(
        read: bool,
        found: bool,
        reads: bool,
        records: Vec<KeptImageRecord>,
        records_refused: bool,
    ) -> Prepared {
        use crate::application::queue_commands::QueueCommand as C;
        use anyhow::Context;
        let at = ReleaseReadAt {
            hold: HoldId::new(1),
            run: RunId::new("r1").unwrap(),
            version: ProgressVersion {
                task_status: crate::domain::TaskStatus::InProgress,
                task_updated_at: "now".into(),
                run_status: crate::domain::RunStatus::Interrupted,
                last_event: None,
            },
        };
        let answer = ReleaseToRead {
            read: read.then(|| (at, released_run())),
            repository: Some("/r/.git".into()),
        };
        let head = CommitSha::try_from("b".repeat(40)).unwrap();
        let work = Work {
            own: reads.then(|| head.clone()),
            kept: RefCell::new(Vec::new()),
        };
        let main = RefCell::new(None);
        let main_checkout = |common_dir: &Path| {
            assert_eq!(common_dir, Path::new("/r/.git"));
            anyhow::ensure!(found, "no main checkout");
            Ok(PathBuf::from("/r"))
        };
        let branches = |checkout: Option<PathBuf>, read: &mut dyn FnMut(&dyn ReleasedBranches)| {
            *main.borrow_mut() = Some(checkout);
            read(&work);
            records.clone()
        };
        let mut sent = Vec::new();
        let mut params = ReleaseParams {
            target: "r1".into(),
            provider: None,
            model: None,
            work: None,
        };
        let outcome = prepare_release(
            &mut params,
            &ReleaseClient {
                main_checkout: &main_checkout,
                branches: &branches,
            },
            |command: &C| match command {
                C::ReleaseRead(_) => {
                    sent.push("release_read");
                    Ok(serde_json::to_value(&answer).unwrap())
                }
                C::ReleaseImages(images) => {
                    sent.push("release_images");
                    assert_eq!(images.target, "r1");
                    assert_eq!(images.records, records);
                    anyhow::ensure!(!records_refused, "the records are refused");
                    Ok(serde_json::json!({"recorded": images.records.len()}))
                }
                other => panic!("{other:?}"),
            },
        )
        .map(|()| params)
        .context("prepared");
        Prepared {
            sent,
            main: main.into_inner(),
            kept: work.kept.into_inner(),
            outcome,
        }
    }

    /// A release reads and keeps the run's work with its caller's Git in
    /// the bound repository's main checkout, none when it is not found;
    /// the image's records go to the service only when there are some,
    /// read or not; when both fail the read's error wins, when only the
    /// records fail theirs does; a release that reads no run reads nothing.
    #[test]
    fn a_release_reads_its_work_and_sends_the_image_records_it_made() {
        let failed = KeptImageRecord::ContainerImageFailed {
            image: None,
            reason: "no image".into(),
        };
        let head = CommitSha::try_from("b".repeat(40)).unwrap();

        let done = prepare(true, true, true, vec![failed.clone()], false);
        assert_eq!(done.sent, ["release_read", "release_images"]);
        assert_eq!(done.main, Some(Some(PathBuf::from("/r"))));
        assert_eq!(done.kept, std::slice::from_ref(&head));
        let work = done.outcome.unwrap().work.unwrap();
        assert_eq!(work.read.own, Some(head.clone()));
        assert_eq!(work.at.run, RunId::new("r1").unwrap());

        let done = prepare(true, false, true, Vec::new(), false);
        assert_eq!(done.sent, ["release_read"]);
        assert_eq!(done.main, Some(None));
        assert!(done.outcome.unwrap().work.is_some());

        let done = prepare(false, true, true, vec![failed.clone()], false);
        assert_eq!(done.sent, ["release_read"]);
        assert_eq!(done.main, None);
        assert!(done.outcome.unwrap().work.is_none());

        // The read fails: the records still go, and the read's error is
        // the release's whether or not they are taken.
        for refused in [false, true] {
            let done = prepare(true, true, false, vec![failed.clone()], refused);
            assert_eq!(done.sent, ["release_read", "release_images"]);
            let error = format!("{:#}", done.outcome.unwrap_err());
            assert!(error.contains("the worktree is gone"), "{error}");
        }
        // Only the records fail: theirs is the release's.
        let done = prepare(true, true, true, vec![failed], true);
        let error = format!("{:#}", done.outcome.unwrap_err());
        assert!(error.contains("the records are refused"), "{error}");
        assert_eq!(done.kept, [head]);
    }

    #[test]
    fn only_a_person_and_the_inbox_hold_or_release_a_task_or_a_run() {
        let run = RunId::new("r1").unwrap();
        let targets = [
            HoldTarget::Task(TaskId::new(7)),
            HoldTarget::Run(run.clone()),
        ];
        let cases: [(ActorContext, bool); 6] = [
            (ActorContext::user(), true),
            (ActorContext::instance(ActorRole::Inbox, "inbox"), true),
            (ActorContext::worker(&run, TaskId::new(7)), false),
            (ActorContext::instance(ActorRole::Planner, 3), false),
            (ActorContext::instance(ActorRole::Supervisor, 1), false),
            (ActorContext::instance(ActorRole::RecoveryJob, 1), false),
        ];
        for (actor, may) in cases {
            let mut store = Store::default();
            let mut holds = Holds::new(&mut store, &actor, &StaticPolicy);
            for target in &targets {
                let held = holds.hold(target, "the login expired", None);
                let released = holds.release(target, ReleaseChoice::default(), &NoBranches);
                assert_eq!(held.is_ok(), may, "{actor:?}");
                assert_eq!(released.is_ok(), may, "{actor:?}");
                if let Err(error) = held {
                    assert!(error.to_string().contains("task.hold"), "{error}");
                }
            }
            assert_eq!(store.held.len(), if may { 2 } else { 0 });
            assert_eq!(store.released.len(), if may { 2 } else { 0 });
            assert_eq!(store.denials.borrow().len(), if may { 0 } else { 4 });
        }
    }
}
