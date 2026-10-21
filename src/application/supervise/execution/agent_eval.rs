//! The rounds of the eval of an agent the supervisor runs (ADR-t1728-1
//! decisions 5, 7, 8 and 9): requested through the queue's use case
//! ([`crate::application::agent_eval`]), never by the supervisor itself.
//!
//! A round runs outside the run slots, one per queue at once (the round
//! that started and has not finished, read from the eval's events), the
//! landing's dev before the oldest request. It reads the agent's
//! definition and cases from the landing branch's commit (a production set
//! of the queue's side, `--set`, from that set's view, made again from its
//! events and Git: [`crate::application::agent_cases::view`]; the dev a
//! run waits for before it lands reads the definition from the run's head,
//! [`super::eval_gate`]), and for each run
//! of a case starts one agent job ([`crate::application::agent_job`]) on
//! `[roles.review]`'s provider through the headless jobs' one way
//! ([`super::jobs`]): recorded in `headless_jobs` as `agent_eval`, under
//! the review's agent job timeout, retried once after a non-zero exit, and
//! stopped with what it started. Its provider is never switched: while it
//! cannot be used no new run starts, and the round waits. The estimate
//! before the round and the check before each run hold it to `[eval]`'s
//! limits. A running round has one owner, the supervisor that started or
//! took it up (its token on the round's events): another supervisor on
//! the queue leaves it alone while the owner lives, and takes it up, in
//! one write transaction ([`crate::application::EvalRounds`]), only when
//! the owner is gone. It then stops the jobs the gone one left (their
//! estimate counts as spent), reads the round back from its events and
//! starts the runs left. A round starts in one write transaction too, so
//! two supervisors never both start one.
//!
//! Before a case's first agent job, the case gets the program reviews a
//! run's review gets (ADR-t1728-1 (i)): those the round's commit
//! configures whose paths the case's change touches, read with their
//! scripts at that commit (never from the case's tree, whatever its patch
//! changes), run one after another in the slot of the case's run, on the
//! backend of the review's actor
//! ([`crate::application::review_programs::ProgramBackends`]): each first
//! against the tree of the case's base (without what the case's tree
//! hides), where one that exits non-zero is not run for the case (the
//! rule it checks is broken before the change, ADR-t2300-1; its end on a
//! base is kept for the round's other cases of that base), and then those
//! left against the case's tree. They spend nothing of the provider's.
//! Once each exits 0 the agent job starts; a case one of them rejects
//! never reaches the agent and is out of its scores; one that could not
//! start or ran past its time on either tree closes the round incomplete,
//! and no run starts after it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::jobs::{HeadlessJob, JobFailed, JobSubject, ProgramEnd, start_program_against};
use super::*;
use crate::application::AgentReads;
use crate::application::agent_eval::{Incomplete, finished as finished_payload};
use crate::application::agent_job::{self, AgentJob};
use crate::application::review::CONFIG_FILE;
use crate::application::review_programs::{SnapshotProgram, programs_at, required_programs};
use crate::domain::actor_model::{ActorLaunch, ModelRole};
use crate::domain::agent_eval::programs::{self, Against, CaseCheck, ProgramFailure, Step};
use crate::domain::agent_eval::record::{self, Round};
use crate::domain::agent_eval::round::{
    ProviderCost, ProviderWait, Refusal, RefusalReason, RoundKey, case_set_digest,
    definition_digest, estimate, holdout_refusal, launchable, may_start_next, next_round,
    provider_wait, run_cost, subset_refusal,
};
use crate::domain::agent_eval::{
    Case, EVALS_DIR, PATCH_DIR, PATCH_EXTENSION, Split, patch_hash, read_agent_cases,
};
use crate::domain::headless_job::JobKind;
use crate::domain::review_programs::ReviewProgram;
use crate::domain::review_subagents::{
    AgentRole as ToolRole, AgentTools, DEFINITION_DIR, find_definition,
};

/// A round this process runs.
pub(super) struct EvalRound {
    id: i64,
    agent: String,
    provider: Provider,
    launch: ActorLaunch,
    definition: String,
    tools: AgentTools,
    /// The cases it runs, in the list's order; a case's index names its
    /// directory, so no path carries its id.
    cases: Vec<Case>,
    /// Each case's patch by its hash.
    patches: BTreeMap<String, String>,
    per_run_usd: f64,
    max_cost_usd: f64,
    concurrency: usize,
    prices: ProviderCost,
    /// The runs to start, in order.
    left: VecDeque<(String, u32)>,
    running: Vec<EvalJob>,
    /// The cases' trees made so far, by case id.
    trees: BTreeMap<String, CaseTree>,
    /// The runs started again after a non-zero exit.
    retried: BTreeSet<(String, u32)>,
    spent_usd: f64,
    /// The next run would spend past the limit: none more starts.
    cost_limited: bool,
    /// Why its new runs wait for the provider, as last recorded.
    waiting: Option<ProviderWait>,
    /// `agent-evals/<id>/`.
    dir: PathBuf,
    /// The landing branch's commit its cases and program reviews are read
    /// from (and its definition, but for the dev a run waits for before it
    /// lands).
    commit: String,
    /// The program reviews the commit configures, or why they did not
    /// read, which fails every case that runs.
    programs: std::result::Result<Vec<ReviewProgram>, String>,
    /// What each case's program reviews said, by case id, once its check
    /// ended.
    checks: BTreeMap<String, CaseCheck>,
    /// How each program exited on a base's tree, by base commit and
    /// program: the round's other cases of that base do not run it again.
    base_ends: BTreeMap<(String, String), bool>,
}

/// The tree of one case: `base` with the patch committed as `head`.
struct CaseTree {
    dir: PathBuf,
    tree: PathBuf,
    base: String,
    head: String,
    material: PathBuf,
}

/// One run of a case in progress: its agent job, or, before it, one of
/// the case's program jobs.
struct EvalJob {
    case: String,
    round: u32,
    job: HeadlessJob,
    /// The case's program reviews when `job` is one of them.
    checking: Option<CaseChecking>,
}

/// The program reviews of a case under way.
struct CaseChecking {
    /// The programs the case needs, in the configured order.
    selected: Vec<SnapshotProgram>,
    /// How each that ran on the base's tree ended ([`programs::step`]).
    on_base: Vec<Option<bool>>,
    /// How each that ran on the case's tree ended.
    on_case: Vec<Option<bool>>,
    /// The end of the latest that ran.
    last: Option<ProgramEnd>,
    /// The base's tree is there: made before the first program that runs
    /// against it, removed once none is left to run there.
    base_made: bool,
}

impl CaseChecking {
    fn new(selected: Vec<SnapshotProgram>) -> Self {
        Self {
            selected,
            on_base: Vec::new(),
            on_case: Vec::new(),
            last: None,
            base_made: false,
        }
    }

    fn names(&self) -> Vec<String> {
        self.selected
            .iter()
            .map(|program| program.program.name.clone())
            .collect()
    }

    fn step(&self) -> Step {
        programs::step(&self.names(), &self.on_base, &self.on_case)
    }

    /// The tree the next program runs against: the base's until each ran
    /// there.
    fn against(&self) -> Against {
        if self.on_base.len() < self.selected.len() {
            Against::Base
        } else {
            Against::Case
        }
    }

    /// The tree of the program that could not tell: the base's while one
    /// ran past its time there or not each ran there yet.
    fn failing_tree(&self) -> Against {
        if self.on_base.contains(&None) {
            Against::Base
        } else {
            self.against()
        }
    }

    /// The programs that ran against the case's tree, and `next` when
    /// given (one that did not start there).
    fn ran(&self, next: Option<&str>) -> Vec<String> {
        let mut ran: Vec<String> = programs::kept(&self.names(), &self.on_base)
            .into_iter()
            .take(self.on_case.len())
            .collect();
        ran.extend(next.map(str::to_owned));
        ran
    }

    fn base_failing(&self) -> Vec<String> {
        programs::base_failing(&self.names(), &self.on_base)
    }

    fn program_named(&self, name: &str) -> bool {
        self.selected
            .iter()
            .any(|program| program.program.name == name)
    }

    fn program(&self, name: &str) -> &SnapshotProgram {
        self.selected
            .iter()
            .find(|program| program.program.name == name)
            .expect("a selected program")
    }

    /// Record how the program that ran last ended, `success` being its
    /// exit's (`None` past its time).
    fn ended(&mut self, success: Option<bool>) {
        match self.against() {
            Against::Base => self.on_base.push(success),
            Against::Case => self.on_case.push(success),
        }
    }
}

/// The tree of `against` of `tree`: what a case's program runs in.
fn tree_of(tree: &CaseTree, against: Against) -> PathBuf {
    match against {
        Against::Base => tree.dir.join("base"),
        Against::Case => tree.tree.clone(),
    }
}

const fn against_str(against: Against) -> &'static str {
    match against {
        Against::Base => "base",
        Against::Case => "case",
    }
}

/// What the case's tree leaves out (its files and its Git's two commits,
/// the base's and the change's; the material's commit in the repository
/// keeps them): the eval's own
/// case lists, with each case's expected verdict, and their patches,
/// which a job in the tree could otherwise read.
const HIDDEN: [&str; 2] = [".dagq/agents/*/evals", ".dagq/agent-cases"];

/// Where the agent job of a case (an eval's run, a set's labeling) may
/// read ([`AgentReads`]): the case's tree and its material's directory,
/// never the repository's checkout or Git directory nor the queue's data
/// directory (the case lists and patches, the other cases' and runs'
/// files, the sets), whose expectations the job must not see.
pub(super) fn case_reads(layout: &Layout, tree: &Path, material: &Path) -> AgentReads {
    let mut unreadable = vec![
        layout.repo_root.clone(),
        layout.main_checkout.clone(),
        layout.common_dir.clone(),
    ];
    unreadable.extend(layout.agent_evals_dir.parent().map(Path::to_owned));
    unreadable.dedup();
    AgentReads {
        readable: [Some(tree), material.parent()]
            .into_iter()
            .flatten()
            .map(Path::to_owned)
            .collect(),
        unreadable,
    }
}

/// What a round reads of the landing branch's commit.
struct Snapshot {
    definition: String,
    tools: AgentTools,
    /// The list's `k`.
    list_k: u32,
    cases: Vec<Case>,
    patches: BTreeMap<String, String>,
    /// For a production set of the queue's side, the id of the queue's
    /// event its set was read up to (`agent_eval_started`'s `set_through`).
    set_through: Option<i64>,
}

fn refusal(reason: RefusalReason, detail: String) -> Refusal {
    Refusal {
        reason,
        detail,
        estimate: None,
    }
}

impl Supervisor<'_> {
    /// Run the eval's rounds for one pass: start a round when none runs
    /// and `starting` (or read back the one a gone supervisor left), reap
    /// its runs, start more within its limits, and finish it. Whether
    /// anything moved. An error is only warned of: the round is read back
    /// from its events on a later pass.
    pub(super) fn agent_eval_pass(&mut self, starting: bool) -> bool {
        match self.tend_agent_eval(starting) {
            Ok(progressed) => progressed,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "agent eval: {error:#}");
                false
            }
        }
    }

    /// A round runs whose new runs do not wait for the provider: the loop
    /// waits for it like a job.
    pub(super) fn agent_eval_busy(&self) -> bool {
        self.agent_eval
            .as_ref()
            .is_some_and(|round| round.waiting.is_none())
    }

    /// Remove the trees of every case of round `id`'s first `cases`
    /// directories (made by this process or a gone one).
    fn remove_case_trees(&self, id: i64, cases: usize) {
        let dir = self.layout.agent_evals_dir.join(id.to_string());
        for index in 0..cases {
            for name in ["tree", "base"] {
                let tree = dir.join(index.to_string()).join(name);
                if let Err(error) = self.repository.remove_case_tree(&tree) {
                    warn!(error = %format_args!("{error:#}"), "agent eval {id}: the case tree {} could not be removed: {error:#}", tree.display());
                }
            }
        }
    }

    /// Abandon the runs in progress (the loop ended): a supervisor that
    /// takes over finds them without an end.
    pub(super) fn abandon_agent_eval(&mut self) {
        if let Some(round) = &mut self.agent_eval {
            for run in &mut round.running {
                run.job.abandon();
            }
        }
    }

    fn tend_agent_eval(&mut self, starting: bool) -> Result<bool> {
        let mut progressed = false;
        if self.agent_eval.is_none() {
            if !starting {
                return Ok(false);
            }
            let rounds = self.eval_rounds()?;
            if let Some(round) = record::running(&rounds) {
                // One round per queue: one another supervisor runs is left
                // to it; one whose owner is gone is taken up.
                let (own, processes) = (self.layout.pid, self.processes.clone());
                let alive = move |pid: u32| pid != own && processes.alive(pid);
                if self.queue.take_up_eval_round(
                    round.id,
                    self.registration.token().as_str(),
                    &alive,
                )? {
                    self.resume_round(round)?;
                    progressed = true;
                }
            } else if let Some(id) = next_round(&record::waiting(&rounds), false) {
                let round = rounds
                    .iter()
                    .find(|round| round.id == id)
                    .expect("a waiting round");
                progressed |= self.start_round(round, &rounds)?;
            }
        }
        if self.agent_eval.is_none() {
            return Ok(progressed);
        }
        progressed |= self.poll_eval_runs()?;
        if starting {
            progressed |= self.start_eval_runs()?;
        }
        progressed |= self.finish_round()?;
        // A supervisor that stops starting (a drain, a stop, the service
        // down) lets go of a round once its runs in progress ended: the
        // loop does not wait for the runs left, and the next supervisor at
        // work takes the round up from its events.
        if !starting
            && self
                .agent_eval
                .as_ref()
                .is_some_and(|round| round.running.is_empty())
        {
            self.agent_eval = None;
        }
        Ok(progressed)
    }

    /// The rounds the eval's events say, oldest first.
    pub(super) fn eval_rounds(&self) -> Result<Vec<Round>> {
        let kinds: Vec<&str> = record::KINDS.iter().map(|kind| kind.as_str()).collect();
        let upto = self.queue.latest_event_id()?;
        let every = usize::try_from(i64::MAX).unwrap_or(usize::MAX);
        let events = self
            .queue
            .events_of_between(&kinds, EventId::new(0), upto, every)?;
        Ok(record::rounds(events.iter().map(|event| {
            (event.id.as_i64(), &event.kind, &event.payload)
        })))
    }

    fn record_eval(&self, kind: EventKind, payload: Value) -> Result<()> {
        self.queue.record_queue_event(kind, payload).map(drop)
    }

    /// Why `provider` takes no new run now
    /// ([`crate::domain::agent_eval::round::provider_wait`]).
    pub(super) fn eval_wait(&self, provider: Provider) -> Option<ProviderWait> {
        provider_wait(
            provider,
            self.provider_held(provider).is_some(),
            self.no_claude,
            self.job_agent(provider).is_some(),
        )
    }

    fn record_wait(&self, id: i64, provider: Provider, wait: ProviderWait) -> Result<()> {
        info!(
            "agent eval {id}: its runs wait, {} cannot be used ({})",
            provider.as_str(),
            wait.as_str()
        );
        self.record_eval(
            EventKind::AgentEvalWaiting,
            json!({"eval_id": id, "provider": provider, "reason": wait}),
        )
    }

    /// Record `agent_eval_refused`; `over` is the definition past its
    /// limit, whose bytes and limit it names.
    fn refuse_round(
        &self,
        id: i64,
        agent: &str,
        refusal: &Refusal,
        over: Option<agent_job::DefinitionOverLimit>,
    ) -> Result<()> {
        info!("agent eval {id} of {agent} not started: {refusal}");
        let mut payload = json!({
            "eval_id": id,
            "agent": agent,
            "reason": refusal.reason,
            "detail": refusal.detail,
            "estimate": refusal.estimate.map(|estimate| estimate.record()),
        });
        if let Some(over) = over {
            payload["definition_bytes"] = json!(over.bytes);
            payload["limit"] = json!(over.limit);
        }
        // Another supervisor may have settled the round meanwhile: then
        // nothing is recorded.
        self.queue
            .settle_eval_round(id, EventKind::AgentEvalRefused, payload)
            .map(drop)
    }

    /// The landing branch's `[roles.review]` launch, on `provider` when a
    /// round started on it already (a round never moves).
    fn eval_launch(&self, provider: Option<Provider>) -> ActorLaunch {
        let launch = self
            .role_models(ModelRole::Review)
            .launch(ModelRole::Review);
        match provider {
            Some(provider) if provider != launch.provider => ActorLaunch {
                provider,
                model: None,
                effort: None,
                ..launch
            },
            _ => launch,
        }
    }

    /// Read the agent's definition from `definition_commit`'s tree and its
    /// `split` cases (those of `selected`, when given) from `commit`'s,
    /// or, for a production set of the queue's side (`set`), its cases and
    /// patches from the set's view
    /// ([`crate::application::agent_cases::view`]); a refusal when they
    /// are not there or do not read.
    fn eval_snapshot(
        &self,
        (agent, split, set, set_through): (&str, Split, Option<i64>, Option<i64>),
        (definition_commit, commit): (&str, &str),
        selected: Option<&[String]>,
    ) -> Result<std::result::Result<Snapshot, Refusal>> {
        let repository = &*self.repository;
        let Some((path, definition)) =
            find_definition(agent, |path| repository.file_in(definition_commit, path))?
        else {
            return Ok(Err(refusal(
                RefusalReason::DefinitionMissing,
                format!("no definition of {agent} in the commit {definition_commit}"),
            )));
        };
        let tools = match AgentTools::declared(ToolRole::Review, &definition) {
            Ok(tools) => tools,
            Err(why) => {
                return Ok(Err(refusal(
                    RefusalReason::DefinitionInvalid,
                    format!("{path} declares its tools by mistake: {why}"),
                )));
            }
        };
        if let Some(set) = set {
            return self.set_snapshot(agent, (set, set_through), (definition, tools), selected);
        }
        let evals = format!("{DEFINITION_DIR}/{agent}/{EVALS_DIR}");
        let mut files = Vec::new();
        for path in repository.paths_in(commit, &evals)? {
            let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
            if let Some(text) = repository.file_in(commit, &path)? {
                files.push((name, text));
            }
        }
        let patches: BTreeSet<String> = repository
            .paths_in(commit, PATCH_DIR)?
            .iter()
            .filter_map(|path| patch_hash(path.rsplit('/').next().unwrap_or(path)))
            .map(str::to_owned)
            .collect();
        let named: Vec<(&str, &str)> = files
            .iter()
            .map(|(name, text)| (name.as_str(), text.as_str()))
            .collect();
        let lists = match read_agent_cases(agent, &named, &patches) {
            Ok(lists) => lists,
            Err(problems) => {
                let said: Vec<String> = problems.iter().take(5).map(ToString::to_string).collect();
                return Ok(Err(refusal(
                    RefusalReason::CasesInvalid,
                    format!(
                        "{} problem(s) in {evals}: {}",
                        problems.len(),
                        said.join("; ")
                    ),
                )));
            }
        };
        let Some(list) = lists.split(split) else {
            return Ok(Err(refusal(
                RefusalReason::CasesInvalid,
                format!("no {evals}/{}", split.file_name()),
            )));
        };
        let where_ = format!("{evals}/{}", split.file_name());
        let cases = match selected_cases(&list.cases, selected, &where_) {
            Ok(cases) => cases,
            Err(refused) => return Ok(Err(refused)),
        };
        let mut texts = BTreeMap::new();
        for case in &cases {
            let path = format!("{PATCH_DIR}/{}.{PATCH_EXTENSION}", case.patch);
            let Some(text) = repository.file_in(commit, &path)? else {
                return Ok(Err(refusal(
                    RefusalReason::CasesInvalid,
                    format!("no {path} for case {}", case.id),
                )));
            };
            texts.insert(case.patch.clone(), text);
        }
        Ok(Ok(Snapshot {
            definition,
            tools,
            list_k: list.k,
            cases,
            patches: texts,
            set_through: None,
        }))
    }

    /// The snapshot of production set `set` of the queue's side for
    /// `agent`, its definition and tools read already: the set's cases and
    /// patches from its view, made again from its events and Git where its
    /// files are gone; a refusal for a set that is not `agent`'s or not
    /// built, or whose list does not read.
    fn set_snapshot(
        &self,
        agent: &str,
        (set, through): (i64, Option<i64>),
        (definition, tools): (String, AgentTools),
        selected: Option<&[String]>,
    ) -> Result<std::result::Result<Snapshot, Refusal>> {
        // Its labels and a person's decisions as of the round's start: a
        // round taken up reads them up to the same event, so a decision
        // made since changes neither its expectations nor its disputes.
        let (sets, set_through) = self.case_sets_through(through)?;
        let Some(found) = sets.iter().find(|found| found.id == set) else {
            return Ok(Err(refusal(
                RefusalReason::CasesInvalid,
                format!("no production set {set}"),
            )));
        };
        if found.request.agent != agent || found.built.is_none() {
            return Ok(Err(refusal(
                RefusalReason::CasesInvalid,
                format!(
                    "production set {set} is {}'s and {}, not a built set of {agent}",
                    found.request.agent,
                    found.status()
                ),
            )));
        }
        let dir = crate::application::agent_cases::set_dir(&self.layout.agent_cases_dir, set);
        let made =
            crate::application::agent_cases::view(found, &*self.repository, &*self.files, &dir)?;
        for (case, why) in &made.left_out {
            warn!("agent eval of set {set}: case {case} is left out: {why}");
        }
        let hashes: BTreeSet<String> = made.patches.keys().cloned().collect();
        let where_ = format!("production set {set}");
        let list = match crate::domain::agent_eval::read_case_file(
            agent,
            Split::Production.file_name(),
            &made.list.to_string(),
            &hashes,
        ) {
            Ok(list) => list,
            Err(problems) => {
                let said: Vec<String> = problems.iter().take(5).map(ToString::to_string).collect();
                return Ok(Err(refusal(
                    RefusalReason::CasesInvalid,
                    format!(
                        "{} problem(s) in {where_}: {}",
                        problems.len(),
                        said.join("; ")
                    ),
                )));
            }
        };
        let cases = match selected_cases(&list.cases, selected, &where_) {
            Ok(cases) => cases,
            Err(refused) => return Ok(Err(refused)),
        };
        Ok(Ok(Snapshot {
            definition,
            tools,
            list_k: list.k,
            cases,
            patches: made.patches,
            set_through: Some(set_through),
        }))
    }

    /// Start the waiting `round` when its provider can be used: a held-out
    /// part of cases refused first, then its definition and cases read,
    /// the once rule of a hold-out, the definition's limit and the
    /// estimate checked, each of which refuses it, and
    /// `agent_eval_started` recorded with its estimate reserved. Whether it
    /// moved (started or refused).
    fn start_round(&mut self, round: &Round, rounds: &[Round]) -> Result<bool> {
        let request = &round.request;
        // The use case refuses a held-out part of cases unrecorded; one in
        // the record all the same is refused here alike (ADR-t2229-1).
        if let Some(refusal) = subset_refusal(request.split, request.cases.is_some(), request.rerun)
        {
            self.refuse_round(round.id, &request.agent, &refusal, None)?;
            return Ok(true);
        }
        let launch = self.eval_launch(None);
        let provider = launch.provider;
        if let Some(wait) = self.eval_wait(provider) {
            if round.waiting.as_deref() != Some(wait.as_str()) {
                self.record_wait(round.id, provider, wait)?;
            }
            return Ok(false);
        }
        let config = self.verifier.eval_config()?;
        let commit = self.repository.main_head()?.into_string();
        // The dev a run waits for before it lands measures the run's
        // definition on the landing branch's cases (ADR-t1728-1 decision
        // 10).
        let definition_commit = request.definition_commit.as_deref().unwrap_or(&commit);
        let snapshot = match self.eval_snapshot(
            (&request.agent, request.split, request.set, None),
            (definition_commit, &commit),
            request.cases.as_deref(),
        )? {
            Ok(snapshot) => snapshot,
            Err(refusal) => {
                self.refuse_round(round.id, &request.agent, &refusal, None)?;
                return Ok(true);
            }
        };
        let planned: Vec<(String, u32)> = snapshot
            .cases
            .iter()
            .map(|case| {
                (
                    case.id.clone(),
                    request.k.unwrap_or_else(|| case.runs(snapshot.list_k)),
                )
            })
            .collect();
        let planned_runs: u32 = planned.iter().map(|(_, k)| *k).sum();
        let key = RoundKey {
            agent: request.agent.clone(),
            definition_digest: definition_digest(&snapshot.definition),
            case_set_digest: case_set_digest(
                snapshot
                    .cases
                    .iter()
                    .map(|case| (case.id.as_str(), snapshot.patches[&case.patch].as_bytes())),
            ),
        };
        let over = agent_job::check_definition(&snapshot.definition).err();
        let refused = holdout_refusal(
            request.split,
            &key,
            &record::held_out_keys(rounds),
            request.rerun,
        )
        .or_else(|| over.map(|over| refusal(RefusalReason::DefinitionOverLimit, over.to_string())));
        let recent = record::recent_costs(rounds, &request.agent, provider, config.recent_runs);
        let estimate = match refused
            .map_or_else(|| estimate(planned_runs, provider, &recent, &config), Err)
        {
            Ok(estimate) => estimate,
            Err(refusal) => {
                let over = over.filter(|_| refusal.reason == RefusalReason::DefinitionOverLimit);
                self.refuse_round(round.id, &request.agent, &refusal, over)?;
                return Ok(true);
            }
        };
        let programs = self.eval_programs(&commit);
        let started = self.queue.settle_eval_round(
            round.id,
            EventKind::AgentEvalStarted,
            json!({
                "eval_id": round.id,
                "supervisor": self.registration.token(),
                "agent": request.agent,
                "split": request.split.as_str(),
                "provider": provider,
                "model": launch.model,
                "definition_commit": definition_commit,
                "cases_commit": (definition_commit != commit).then_some(&commit),
                "definition_digest": key.definition_digest,
                "case_set_digest": key.case_set_digest,
                "cases": snapshot.cases.iter().map(|case| &case.id).collect::<Vec<_>>(),
                "planned": planned,
                "planned_runs": planned_runs,
                "estimate": estimate.record(),
                "reserved_usd": estimate.total_usd,
                "max_runs": config.max_runs,
                "max_cost_usd": config.max_cost_usd,
                "concurrency": config.concurrency,
                "threshold": config.threshold,
                "requested_by": request.requested_by,
                "set": request.set,
                "set_through": snapshot.set_through,
                "rerun": request.rerun,
            }),
        )?;
        if !started {
            // Another supervisor started or refused it, or started another
            // round, first.
            return Ok(false);
        }
        info!(
            "agent eval {} of {} started: {} {} runs on {}, estimated at ${:.2}",
            round.id,
            request.agent,
            planned_runs,
            request.split.as_str(),
            provider.as_str(),
            estimate.total_usd
        );
        self.agent_eval = Some(EvalRound {
            id: round.id,
            agent: request.agent.clone(),
            provider,
            launch,
            definition: snapshot.definition,
            tools: snapshot.tools,
            cases: snapshot.cases,
            patches: snapshot.patches,
            per_run_usd: estimate.per_run_usd,
            max_cost_usd: config.max_cost_usd,
            concurrency: config.concurrency,
            prices: config.provider(provider),
            left: planned
                .iter()
                .flat_map(|(case, k)| (0..*k).map(move |run| (case.clone(), run)))
                .collect(),
            running: Vec::new(),
            trees: BTreeMap::new(),
            retried: BTreeSet::new(),
            spent_usd: 0.0,
            cost_limited: false,
            waiting: None,
            dir: self.layout.agent_evals_dir.join(round.id.to_string()),
            commit,
            programs,
            checks: BTreeMap::new(),
            base_ends: BTreeMap::new(),
        });
        Ok(true)
    }

    /// The project's program reviews `commit` configures, or why they do
    /// not read. The built-in programs are not run over a case: they check
    /// the lines a run's change adds before it lands, and a case's patch is
    /// no such change.
    fn eval_programs(&self, commit: &str) -> std::result::Result<Vec<ReviewProgram>, String> {
        let verifier = &self.verifier;
        programs_at(&*self.repository, commit, &|text| {
            verifier.review_programs_in(text)
        })
        .map(|configured| configured.programs)
        .map_err(|error| format!("{error:#}"))
    }

    /// Take up `round`, which a gone supervisor left running: its runs
    /// without an end (their jobs were stopped by the takeover) end as
    /// abandoned, their estimate spent, and the round goes on with the
    /// runs left, on the definition and cases it started with.
    fn resume_round(&mut self, round: &Round) -> Result<()> {
        let started = round.started.clone().context("a running round started")?;
        for (case, run) in round.unended() {
            self.record_eval(
                EventKind::AgentEvalRunFinished,
                json!({
                    "eval_id": round.id,
                    "case": case,
                    "round": run,
                    "result": null,
                    "error": "stopped with the supervisor that started it",
                    "cost": run_cost(None, &ProviderCost::default(), started.per_run_usd).record(),
                    "abandoned": true,
                }),
            )?;
        }
        let rounds = self.eval_rounds()?;
        let round = rounds
            .iter()
            .find(|read| read.id == round.id)
            .context("the round read back")?;
        let request = &round.request;
        let snapshot = self.eval_snapshot(
            (
                &request.agent,
                request.split,
                request.set,
                started.set_through,
            ),
            (&started.definition_commit, started.cases_commit()),
            Some(
                &started
                    .planned
                    .iter()
                    .map(|(case, _)| case.clone())
                    .collect::<Vec<_>>(),
            ),
        )?;
        let config = self.verifier.eval_config()?;
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(refusal) => {
                // The commit it started on no longer reads: it ends with
                // the runs it has.
                warn!("agent eval {} cannot go on: {refusal}", round.id);
                let payload = finished_payload(round, &[], Some(Incomplete::CasesUnreadable));
                self.record_eval(EventKind::AgentEvalFinished, payload)?;
                self.remove_case_trees(round.id, started.planned.len());
                return Ok(());
            }
        };
        info!(
            "agent eval {} of {} taken up: {} run(s) left",
            round.id,
            request.agent,
            round.left().len()
        );
        self.agent_eval = Some(EvalRound {
            id: round.id,
            agent: request.agent.clone(),
            provider: started.provider,
            launch: self.eval_launch(Some(started.provider)),
            definition: snapshot.definition,
            tools: snapshot.tools,
            cases: snapshot.cases,
            patches: snapshot.patches,
            per_run_usd: started.per_run_usd,
            max_cost_usd: started.max_cost_usd,
            concurrency: started.concurrency,
            prices: config.provider(started.provider),
            left: round.left().into(),
            running: Vec::new(),
            trees: BTreeMap::new(),
            retried: round.retried(),
            spent_usd: round.spent_usd(),
            cost_limited: false,
            waiting: None,
            dir: self.layout.agent_evals_dir.join(round.id.to_string()),
            programs: self.eval_programs(started.cases_commit()),
            commit: started.cases_commit().to_owned(),
            checks: round.checks.clone(),
            base_ends: BTreeMap::new(),
        });
        Ok(())
    }

    /// Start the runs the round may start now: none while its provider
    /// cannot be used (recorded once per reason), up to its concurrency,
    /// and each only while what it spent, the runs in progress and the
    /// next stay within its dollars (else no more starts).
    fn start_eval_runs(&mut self) -> Result<bool> {
        let mut round = self.agent_eval.take().expect("a round runs");
        let started = self.start_eval_runs_of(&mut round);
        self.agent_eval = Some(round);
        started
    }

    fn start_eval_runs_of(&mut self, round: &mut EvalRound) -> Result<bool> {
        if round.cost_limited || round.left.is_empty() || programs::any_failed(&round.checks) {
            return Ok(false);
        }
        match self.eval_wait(round.provider) {
            Some(wait) => {
                if round.waiting != Some(wait) {
                    self.record_wait(round.id, round.provider, wait)?;
                    round.waiting = Some(wait);
                }
                return Ok(false);
            }
            None => round.waiting = None,
        }
        let mut progressed = false;
        for _ in 0..launchable(round.running.len(), round.concurrency, round.left.len()) {
            if !may_start_next(
                round.spent_usd,
                round.running.len(),
                round.per_run_usd,
                round.max_cost_usd,
            ) {
                info!(
                    "agent eval {}: the next run would pass ${:.2}; no more starts",
                    round.id, round.max_cost_usd
                );
                round.cost_limited = true;
                break;
            }
            // A run of a case whose programs run now waits for them.
            let Some((case, run)) = round
                .left
                .iter()
                .position(|(case, _)| {
                    !round
                        .running
                        .iter()
                        .any(|job| job.checking.is_some() && &job.case == case)
                })
                .and_then(|next| round.left.remove(next))
            else {
                break;
            };
            match round.checks.get(&case) {
                Some(CaseCheck::Passed) => self.start_eval_run(round, &case, run)?,
                Some(_) => {}
                None => self.check_case(round, &case, run)?,
            }
            progressed = true;
            if programs::any_failed(&round.checks) {
                break;
            }
        }
        Ok(progressed)
    }

    /// Start the program reviews of `case` before its run `run`: those the
    /// round's commit configures whose paths the case's change touches,
    /// each script read at that commit. A case that needs none goes to its
    /// agent at once.
    fn check_case(&mut self, round: &mut EvalRound, case: &str, run: u32) -> Result<()> {
        let index = round
            .cases
            .iter()
            .position(|known| known.id == case)
            .context("a case of the round")?;
        if self.case_tree(round, index).is_err() {
            // The run cannot start: its own start records why.
            return self.start_eval_run(round, case, run);
        }
        let selected = match &round.programs {
            Ok(configured) if configured.is_empty() => Ok(Vec::new()),
            Ok(configured) => {
                let tree = &round.trees[case];
                self.repository
                    .changed_paths(&tree.base, &tree.head)
                    .and_then(|changed| {
                        required_programs(&*self.repository, &round.commit, configured, &changed)
                    })
                    .map_err(|error| format!("{error:#}"))
            }
            Err(error) => Err(error.clone()),
        };
        match selected {
            Ok(selected) => self.go_on_checking(round, case, run, CaseChecking::new(selected)),
            Err(detail) => {
                let check = CaseCheck::Failed {
                    program: CONFIG_FILE.to_owned(),
                    failure: ProgramFailure::StartFailed,
                };
                let checking = CaseChecking::new(Vec::new());
                self.record_check(round, case, &check, &checking, None, Some(detail))
            }
        }
    }

    /// The next step of `case`'s program reviews ([`programs::step`]):
    /// start its next program (against the base's tree, unless the
    /// round's end of it there is known, then against the case's), or end
    /// its check, starting its run `run`'s agent job when every program it
    /// ran against the case's tree exited 0. The base's tree is made
    /// before the first program that runs there and removed once none is
    /// left to run there.
    fn go_on_checking(
        &mut self,
        round: &mut EvalRound,
        case: &str,
        run: u32,
        mut checking: CaseChecking,
    ) -> Result<()> {
        let base_commit = round.trees[case].base.clone();
        let mut step = checking.step();
        while let Step::Run(name, Against::Base) = &step {
            match round.base_ends.get(&(base_commit.clone(), name.clone())) {
                Some(success) => checking.on_base.push(Some(*success)),
                None => break,
            }
            step = checking.step();
        }
        if checking.base_made
            && (checking.against() == Against::Case || matches!(step, Step::Done(_)))
        {
            checking.base_made = false;
            let base = tree_of(&round.trees[case], Against::Base);
            if let Err(error) = self.repository.remove_case_tree(&base) {
                warn!(error = %format_args!("{error:#}"), "agent eval {}: the base tree {} could not be removed: {error:#}", round.id, base.display());
            }
        }
        // Another case's program failed meanwhile: nothing more starts.
        let halted = programs::any_failed(&round.checks);
        match step {
            Step::Done(CaseCheck::Passed) => {
                if !checking.selected.is_empty() {
                    self.record_check(round, case, &CaseCheck::Passed, &checking, None, None)?;
                }
                round.checks.insert(case.to_owned(), CaseCheck::Passed);
                if halted || self.eval_wait(round.provider).is_some() {
                    // The run waits with the round's others: for the
                    // provider, or for none, as the round closes.
                    round.left.push_front((case.to_owned(), run));
                    return Ok(());
                }
                self.start_eval_run(round, case, run)
            }
            Step::Done(check) => {
                let last = checking.last.clone();
                self.record_check(round, case, &check, &checking, last.as_ref(), None)
            }
            Step::Run(..) if halted => {
                round.left.push_front((case.to_owned(), run));
                Ok(())
            }
            Step::Run(name, against) => {
                let program = checking.program(&name).clone();
                let started = self
                    .case_base_tree(round, case, against, &mut checking)
                    .and_then(|()| self.start_case_program(round, case, run, &program, against));
                match started {
                    Ok(job) => {
                        round.running.push(EvalJob {
                            case: case.to_owned(),
                            round: run,
                            job,
                            checking: Some(checking),
                        });
                        Ok(())
                    }
                    Err(error) => {
                        warn!(error = %format_args!("{error:#}"), "agent eval {}: the program {name} of case {case} could not start against its {} tree: {error:#}", round.id, against_str(against));
                        let check = CaseCheck::Failed {
                            program: name,
                            failure: ProgramFailure::StartFailed,
                        };
                        self.record_check(
                            round,
                            case,
                            &check,
                            &checking,
                            None,
                            Some(format!("{error:#}")),
                        )
                    }
                }
            }
        }
    }

    /// Make the tree of `case`'s base before the first program that runs
    /// against it (`against` the base, the tree not made yet).
    fn case_base_tree(
        &self,
        round: &EvalRound,
        case: &str,
        against: Against,
        checking: &mut CaseChecking,
    ) -> Result<()> {
        if against != Against::Base || checking.base_made {
            return Ok(());
        }
        let tree = &round.trees[case];
        self.repository
            .add_case_base_tree(&tree_of(tree, Against::Base), &tree.base, &HIDDEN)?;
        checking.base_made = true;
        Ok(())
    }

    /// Start `program` against `case`'s tree or its base's (`against`) as
    /// a program job before its run `run`, on the backend of the review's
    /// actor, its output and what it runs in the case's directory.
    fn start_case_program(
        &self,
        round: &EvalRound,
        case: &str,
        run: u32,
        program: &SnapshotProgram,
        against: Against,
    ) -> Result<HeadlessJob> {
        let tree = &round.trees[case];
        let name = &program.program.name;
        let (label, stem) = match against {
            Against::Base => (
                format!("{}:{}:{case}:{name}:base", round.agent, round.id),
                format!("base-{run}-{name}"),
            ),
            Against::Case => (
                format!("{}:{}:{case}:{name}", round.agent, round.id),
                format!("program-{run}-{name}"),
            ),
        };
        let subject = JobSubject::eval_program(label, usize::try_from(run).unwrap_or(usize::MAX));
        let timeout = self.job_timeout(&subject);
        start_program_against(
            &self.job_ports(),
            self.spawner,
            self.programs,
            program,
            (&tree_of(tree, against), &[]),
            (&tree.dir, &tree.dir.join("programs")),
            (&stem, subject),
            timeout,
        )
    }

    /// Record `check`, the end of `case`'s program reviews, with the
    /// programs that ran against its tree, those not run for it as they
    /// failed on its base, the tree of the one that failed it, the end of
    /// the one that stopped or failed it and why it failed; a case it
    /// stops has none of its runs left.
    fn record_check(
        &self,
        round: &mut EvalRound,
        case: &str,
        check: &CaseCheck,
        checking: &CaseChecking,
        end: Option<&ProgramEnd>,
        detail: Option<String>,
    ) -> Result<()> {
        info!(
            "agent eval {}: the programs of case {case}: {}",
            round.id,
            check.record()
        );
        let mut payload = check.record();
        payload["eval_id"] = json!(round.id);
        payload["case"] = json!(case);
        let failed_on = match check {
            CaseCheck::Failed { program, .. } if checking.program_named(program) => {
                Some(checking.failing_tree())
            }
            _ => None,
        };
        // One that did not start has no end among those that ran.
        let unstarted = match check {
            CaseCheck::Failed {
                program,
                failure: ProgramFailure::StartFailed,
            } if failed_on == Some(Against::Case) => Some(program.as_str()),
            _ => None,
        };
        payload["programs"] = json!(checking.ran(unstarted));
        payload["base_failing"] = json!(checking.base_failing());
        payload["against"] = json!(failed_on.map(against_str));
        if let Some(end) = end {
            payload["exit"] = json!(end.exit.as_ref().map(ToString::to_string));
            payload["stdout_tail"] = json!(end.stdout_tail);
            payload["stderr_tail"] = json!(end.stderr_tail);
        }
        if let Some(detail) = detail {
            payload["detail"] = json!(detail);
        }
        self.record_eval(EventKind::AgentEvalCaseChecked, payload)?;
        if matches!(check, CaseCheck::Stopped { .. }) {
            round.left.retain(|(left, _)| left != case);
        }
        round.checks.insert(case.to_owned(), check.clone());
        Ok(())
    }

    /// Make the case's tree, material and directory once.
    fn case_tree(&self, round: &mut EvalRound, index: usize) -> Result<()> {
        let case = &round.cases[index];
        if round.trees.contains_key(&case.id) {
            return Ok(());
        }
        let dir = round.dir.join(index.to_string());
        self.files
            .create_dir_all(&dir)
            .with_context(|| format!("create {}", dir.display()))?;
        let patch = dir.join("change.patch");
        self.files
            .write(&patch, round.patches[&case.patch].as_bytes())
            .with_context(|| format!("write {}", patch.display()))?;
        let tree = dir.join("tree");
        let head = self
            .repository
            .add_case_tree(&tree, &case.base_commit, &patch, &HIDDEN)?
            .into_string();
        let base = case.base_commit.clone();
        let commits = self.repository.log_oneline(&base, &head)?;
        // In a directory of its own, the only one beside the tree the job
        // is given: not the runs' directories, so a run reads no other's
        // output.
        let material_dir = dir.join("material");
        self.files
            .create_dir_all(&material_dir)
            .with_context(|| format!("create {}", material_dir.display()))?;
        let diff = material_dir.join("change.diff");
        self.repository.diff_to_file(&base, &head, &diff)?;
        let material = material_dir.join("material.md");
        self.files.write_fenced(
            &material,
            &format!(
                "# The change\n\nbase {base}\nhead {head}\n\n## Commits\n\n{}\n\n## The full diff `{base}...{head}`\n",
                or_none(commits.trim())
            ),
            "diff",
            &diff,
        )?;
        round.trees.insert(
            case.id.clone(),
            CaseTree {
                dir,
                tree,
                base,
                head,
                material,
            },
        );
        Ok(())
    }

    /// Start one run of `case`: its agent job built from the definition
    /// and the case's tree, started on the round's provider, and
    /// `agent_eval_run_started` recorded with its prompt's bytes. A run
    /// that could not start ends at once without a judgment.
    fn start_eval_run(&mut self, round: &mut EvalRound, case: &str, run: u32) -> Result<()> {
        let index = round
            .cases
            .iter()
            .position(|known| known.id == case)
            .context("a case of the round")?;
        let label = format!("{}:{}:{case}:{run}", round.agent, round.id);
        let started = self.case_tree(round, index).and_then(|()| {
            let tree = &round.trees[case];
            let dir = tree.dir.join(format!("run-{run}"));
            self.files
                .create_dir_all(&dir)
                .with_context(|| format!("create {}", dir.display()))?;
            let language = self.verifier.language();
            let reads = case_reads(self.layout, &tree.tree, &tree.material);
            let built = agent_job::build(&AgentJob {
                agent: &round.agent,
                definition: &round.definition,
                tools: &round.tools,
                // The eval gives a built-in no project's rules
                // (ADR-t2259-1 decision 10).
                rules: None,
                base: &tree.base,
                head: &tree.head,
                material: &tree.material,
                cwd: &tree.tree,
                dir: &dir,
                language: language.as_ref(),
                reads: Some(&reads),
            })?;
            self.files
                .write(&dir.join("prompt.txt"), built.prompt.text.as_bytes())?;
            let agent = self.job_agent(round.provider).with_context(|| {
                format!("no {} runs on this supervisor", round.provider.as_str())
            })?;
            let (stdout, stderr) = (dir.join("agent.out"), dir.join("agent.err"));
            let child = self
                .actors_on(agent)
                .spawn(ActorExecutionSpec::new(
                    ActorContext::agent_eval_job(round.id, index, run),
                    WorkspaceAccess::Read(tree.tree.clone()),
                    ActorProgram::Headless {
                        program: HeadlessProgram::AgentJob(&built.launch),
                        session_id: None,
                        launch: Some(&round.launch),
                        without_mcp: true,
                        env: Vec::new(),
                        without_env: &[],
                        streams: Streams::Files {
                            stdout: &stdout,
                            stderr: &stderr,
                        },
                    },
                ))
                .context("start the agent job")?
                .process()?;
            Ok((built.prompt.bytes, child, stdout, stderr))
        });
        match started {
            Ok((prompt_bytes, child, stdout, stderr)) => {
                let job = self.headless_job(
                    "agent eval",
                    child,
                    stdout,
                    stderr,
                    JobSubject {
                        kind: headless_job::AGENT_EVAL,
                        job: JobKind::Agent,
                        review_stage: true,
                        label: Some(label.clone()),
                        run_id: None,
                        proposal_id: None,
                        goal_id: None,
                        attempt: usize::try_from(run).unwrap_or(usize::MAX),
                        provider: round.provider,
                    },
                );
                self.record_eval(
                    EventKind::AgentEvalRunStarted,
                    json!({
                        "eval_id": round.id,
                        "case": case,
                        "round": run,
                        "provider": round.provider,
                        "job": label,
                        "prompt_bytes": prompt_bytes,
                    }),
                )?;
                round.running.push(EvalJob {
                    case: case.to_owned(),
                    round: run,
                    job,
                    checking: None,
                });
            }
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "agent eval {} run {label} could not start: {error:#}", round.id);
                self.record_eval(
                    EventKind::AgentEvalRunStarted,
                    json!({"eval_id": round.id, "case": case, "round": run, "provider": round.provider, "job": label}),
                )?;
                let cost = run_cost(None, &round.prices, round.per_run_usd);
                self.record_eval(
                    EventKind::AgentEvalRunFinished,
                    json!({
                        "eval_id": round.id, "case": case, "round": run, "result": null,
                        "error": format!("{error:#}"), "cost": cost.record(), "abandoned": false,
                    }),
                )?;
                round.spent_usd += cost.usd;
            }
        }
        Ok(())
    }

    /// Reap the runs that ended: each one's result (its verdict JSON, or
    /// none), what it cost and from where, recorded with its Execution. A
    /// run whose job exited non-zero is started once more
    /// ([`JobKind::retries`]): the first end is abandoned, its dollars
    /// spent.
    fn poll_eval_runs(&mut self) -> Result<bool> {
        let mut round = self.agent_eval.take().expect("a round runs");
        let polled = self.poll_eval_runs_of(&mut round);
        self.agent_eval = Some(round);
        polled
    }

    fn poll_eval_runs_of(&mut self, round: &mut EvalRound) -> Result<bool> {
        let agent = self.job_agent(round.provider).unwrap_or(self.reviewer);
        let mut progressed = false;
        let mut index = 0;
        let mut checked = Vec::new();
        while index < round.running.len() {
            if round.running[index].checking.is_some() {
                let ended = match round.running[index].job.poll_program(&*self.files) {
                    Ok(None) => {
                        index += 1;
                        continue;
                    }
                    Ok(Some(end)) => Ok(end),
                    Err(error) => Err(error),
                };
                checked.push((round.running.remove(index), ended));
                progressed = true;
                continue;
            }
            let end = match round.running[index].job.poll_end(&*self.files, agent) {
                Ok(Some(end)) => end,
                Ok(None) => {
                    index += 1;
                    continue;
                }
                Err(error) => Err(JobFailed::Exited(format!("{error:#}"))),
            };
            let run = round.running.remove(index);
            let duration_secs = run.job.elapsed().as_secs();
            let stdout = self
                .files
                .read_to_string(&run.job.stdout)
                .unwrap_or_default();
            let session = agent.job_session(&stdout, run.job.started_at);
            let tokens = session
                .as_ref()
                .and_then(|session| session.tokens.as_ref())
                .and_then(|tokens| tokens.tokens.as_ref());
            let cost = run_cost(tokens, &round.prices, round.per_run_usd);
            let key = (run.case.clone(), run.round);
            let (result, error, again) = match end {
                Ok(reply) => match agent_job::read_reply(&reply) {
                    Some(result) => (Some(result), None, false),
                    None => (
                        None,
                        Some("the agent job printed no readable verdict".to_owned()),
                        false,
                    ),
                },
                Err(failed) => {
                    let wall = self.job_failure(&run.job).switch_reason();
                    let error = failed.error().to_owned();
                    if let Some(reason) = wall {
                        // A login or usage limit: the provider is held and
                        // the run waits with the round for it, not
                        // counted as a retry.
                        let said = format!("{error}\n{stdout}");
                        if let Err(held) = self.hold_provider(round.provider, reason, None, &said) {
                            warn!(error = %format_args!("{held:#}"), "agent eval {}: {} could not be held: {held:#}", round.id, round.provider.as_str());
                        }
                        (None, Some(error), true)
                    } else {
                        let again =
                            JobKind::Agent.retries(failed.stop()) && !round.retried.contains(&key);
                        if again {
                            round.retried.insert(key.clone());
                        }
                        (None, Some(error), again)
                    }
                }
            };
            if again {
                round.left.push_front(key.clone());
            }
            let retry = again && round.retried.contains(&key);
            let mut payload = json!({
                "eval_id": round.id,
                "case": run.case,
                "round": run.round,
                "result": result,
                "error": error,
                "cost": cost.record(),
                "abandoned": again,
                "retry": retry,
                "duration_secs": duration_secs,
            });
            crate::domain::headless_job::JobSession::record_execution(
                session.as_ref(),
                &mut payload,
            );
            self.record_eval(EventKind::AgentEvalRunFinished, payload)?;
            round.spent_usd += cost.usd;
            progressed = true;
        }
        for (run, ended) in checked {
            let mut checking = run.checking.expect("a program job");
            match ended {
                Ok(end) => {
                    let success = end.exit.as_ref().map(|exit| exit.success);
                    if let (Against::Base, Step::Run(name, _), Some(success)) =
                        (checking.against(), checking.step(), success)
                    {
                        let base = round.trees[&run.case].base.clone();
                        round.base_ends.insert((base, name), success);
                    }
                    checking.ended(success);
                    checking.last = Some(end);
                    self.go_on_checking(round, &run.case, run.round, checking)?;
                }
                Err(error) => {
                    let Step::Run(name, _) = checking.step() else {
                        unreachable!("a program job runs the case's next program");
                    };
                    let check = CaseCheck::Failed {
                        program: name,
                        failure: ProgramFailure::StartFailed,
                    };
                    self.record_check(
                        round,
                        &run.case,
                        &check,
                        &checking,
                        None,
                        Some(format!("{error:#}")),
                    )?;
                }
            }
        }
        Ok(progressed)
    }

    /// Finish the round once no run is in progress and none is left to
    /// start (or none more may start for its dollars): its scores recorded
    /// as `agent_eval_finished` from its events, and its cases' trees
    /// removed.
    fn finish_round(&mut self) -> Result<bool> {
        let done = self.agent_eval.as_ref().is_some_and(|round| {
            round.running.is_empty()
                && (round.left.is_empty()
                    || round.cost_limited
                    || programs::any_failed(&round.checks))
        });
        if !done {
            return Ok(false);
        }
        let round = self.agent_eval.take().expect("a round runs");
        let rounds = self.eval_rounds()?;
        let recorded = rounds
            .iter()
            .find(|read| read.id == round.id)
            .context("the round read back")?;
        let incomplete =
            Incomplete::of(programs::any_failed(&round.checks), !round.left.is_empty());
        let payload = finished_payload(recorded, &round.cases, incomplete);
        info!(
            "agent eval {} of {} finished: {} (passed {})",
            round.id, round.agent, payload["outcome"], payload["passed"]
        );
        self.record_eval(EventKind::AgentEvalFinished, payload)?;
        self.remove_case_trees(round.id, round.cases.len());
        Ok(true)
    }
}

/// The cases of `all` a round runs: those of `selected` when given (each
/// must be there), else all; a refusal naming `place` when one is missing
/// or none is left.
fn selected_cases(
    all: &[Case],
    selected: Option<&[String]>,
    place: &str,
) -> std::result::Result<Vec<Case>, Refusal> {
    let cases: Vec<Case> = match selected {
        Some(ids) => {
            if let Some(missing) = ids
                .iter()
                .find(|id| !all.iter().any(|case| &case.id == *id))
            {
                return Err(refusal(
                    RefusalReason::CasesInvalid,
                    format!("no case {missing} in {place}"),
                ));
            }
            all.iter()
                .filter(|case| ids.contains(&case.id))
                .cloned()
                .collect()
        }
        None => all.to_vec(),
    };
    if cases.is_empty() {
        return Err(refusal(
            RefusalReason::CasesInvalid,
            format!("{place} has no case to run"),
        ));
    }
    Ok(cases)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A case's job reads its tree and its material's directory, and
    /// never the repository's checkout, its Git directory or the queue's
    /// data directory, where the case lists, the other cases' and runs'
    /// files and the production sets are.
    #[test]
    fn a_cases_job_reads_its_tree_and_material_and_never_the_repository_or_the_queue() {
        let layout = Layout::for_test(Path::new("/q"));
        let case = layout.agent_evals_dir.join("7/3");
        let reads = case_reads(
            &layout,
            &case.join("tree"),
            &case.join("material/material.md"),
        );
        assert_eq!(
            reads.readable,
            [case.join("tree"), case.join("material")],
            "{reads:?}"
        );
        assert_eq!(
            reads.unreadable,
            [
                PathBuf::from("/repo"),
                PathBuf::from("/repo/.git"),
                PathBuf::from("/q")
            ],
            "{reads:?}"
        );
        assert!(layout.agent_cases_dir.starts_with("/q"));
    }
}
