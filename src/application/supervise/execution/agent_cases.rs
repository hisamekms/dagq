//! The production sets of an agent's cases the supervisor makes
//! (ADR-t1728-1 (g)): asked for through the queue's use case
//! ([`crate::application::agent_cases`]) by a person, the inbox or a
//! planner, never by the supervisor itself.
//!
//! One set at a time, queue-wide, outside the run slots, the oldest
//! request first, by one supervisor: the one that took it in one write
//! transaction (`EvalRounds::take_case_set`), or took it over from an
//! owner that is gone. Another supervisor on the queue leaves it alone,
//! so no two label the same case or share its tree:
//!
//! - prepare: read the agent's definition and case lists from the landing
//!   branch's commit (the rule codes, `k` and the rules' documents the
//!   definition links to), collect the production reviews of the period
//!   that selected the agent from `review_started` and `review_finished`,
//!   and write each one's change (`git diff <base>...<head>`) to the set's
//!   patches by its content's hash; a review whose commits are gone is
//!   left out with why. More reviews than `[eval] max_runs` refuse the set.
//! - label: one agent job per case on the provider other than
//!   `[roles.review]`'s (the improving side's), through the eval's agent
//!   job path (`agent_job::build`, `AgentProvider::agent_job_command`,
//!   recorded in `headless_jobs` as `agent_eval`), on the case's tree with
//!   the agents' definitions and the eval's cases hidden, its "definition"
//!   the rules' documents and codes alone
//!   ([`production::label_definition`]). While that provider cannot be
//!   used no job starts and the set waits; a job stopped at its wall holds
//!   the provider and its case is labeled again.
//! - build: the labeled cases make the set's files on the queue's data dir
//!   ([`crate::application::agent_cases::view`]) and `agent_cases_built`
//!   records its digest; the cases whose label disagrees with the
//!   production verdict are disputed, and one `decide` ask to the inbox
//!   asks a person to decide them (`agent decide`), about the task of
//!   the first disputed case's review.
//!
//! A job a gone owner left is stopped by the headless jobs' takeover, and
//! its case, unlabeled in the events, is labeled again by the new owner.

use std::collections::BTreeSet;

use super::agent_eval::case_reads;
use super::jobs::{HeadlessJob, JobFailed, JobSubject};
use super::*;
use crate::application::agent_cases::{set_dir, sets_of, view, write_patch};
use crate::application::agent_job::{self, AgentJob};
use crate::domain::actor_model::{ActorLaunch, ModelRole};
use crate::domain::agent_eval::production::{
    self, CaseSet, Frame, PreparedCase, ReviewEvent, candidates, label_definition, label_of,
    unreachable,
};
use crate::domain::agent_eval::{EVALS_DIR, Split};
use crate::domain::headless_job::JobKind;
use crate::domain::review_subagents::{
    AgentRole as ToolRole, AgentTools, DEFINITION_DIR, find_definition,
};
use crate::domain::{AskKind, AskReason, NewAsk};

/// What the case's tree leaves out of its working tree: every agent's
/// definition and the eval's case lists and patches, so the labeling job
/// judges by the rules alone.
const HIDDEN: [&str; 2] = [DEFINITION_DIR, ".dagq/agent-cases"];

/// The options of the `decide` ask about a set's disputed cases, which is
/// about the task of the first disputed case's review (an ask needs a task
/// or a run): the decisions are recorded with `agent decide`, or the cases
/// stay disputed (out of the primary scores). The inbox acts on the answer;
/// the supervisor applies none.
pub const DISPUTED_OPTIONS: [&str; 2] = ["decided", "keep_disputed"];

/// The set this process labels.
pub(super) struct CaseLabeling {
    set: i64,
    agent: String,
    provider: Provider,
    launch: ActorLaunch,
    definition: String,
    tools: AgentTools,
    frame: Frame,
    concurrency: usize,
    running: Vec<LabelJob>,
    /// The cases a job stopped at the provider's wall: labeled again.
    again: BTreeSet<String>,
    /// `agent-cases/<set>/`.
    dir: PathBuf,
}

/// One case's labeling job in progress.
struct LabelJob {
    case: String,
    job: HeadlessJob,
    tree: PathBuf,
}

/// The provider that labels for the improving side on `review`: the other
/// one (ADR-t1728-1 (g)), Codex while the review's agents run on Claude.
pub const fn label_provider(review: Provider) -> Provider {
    match review {
        Provider::Claude => Provider::Codex,
        Provider::Codex => Provider::Claude,
    }
}

impl Supervisor<'_> {
    /// Make the production sets asked for, one pass: prepare the oldest
    /// waiting one, start and reap its labeling jobs, and build it. An
    /// error is only warned of: the set is read back from its events on a
    /// later pass.
    pub(super) fn agent_cases_pass(&mut self, starting: bool) -> bool {
        match self.tend_agent_cases(starting) {
            Ok(progressed) => progressed,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "agent cases: {error:#}");
                false
            }
        }
    }

    /// A labeling job runs: the loop waits for it like a job.
    pub(super) fn agent_cases_busy(&self) -> bool {
        self.agent_cases
            .as_ref()
            .is_some_and(|labeling| !labeling.running.is_empty())
    }

    /// Abandon the labeling jobs in progress (the loop ended).
    pub(super) fn abandon_agent_cases(&mut self) {
        if let Some(labeling) = &mut self.agent_cases {
            for run in &mut labeling.running {
                run.job.abandon();
            }
        }
    }

    /// The sets the production cases' events say, oldest first.
    pub(super) fn case_sets(&self) -> Result<Vec<CaseSet>> {
        self.case_sets_through(None).map(|(sets, _)| sets)
    }

    /// The sets as their events up to `through` (an event id) say, and
    /// the id they were read up to: every event when `None`, read up to
    /// the queue's latest. A round of `agent eval --set` reads its set at
    /// the id it started on again, so a later decision does not change it.
    pub(super) fn case_sets_through(&self, through: Option<i64>) -> Result<(Vec<CaseSet>, i64)> {
        let kinds: Vec<&str> = production::KINDS.iter().map(|kind| kind.as_str()).collect();
        let upto = match through {
            Some(through) => EventId::new(through),
            None => self.queue.latest_event_id()?,
        };
        let every = usize::try_from(i64::MAX).unwrap_or(usize::MAX);
        let events = self
            .queue
            .events_of_between(&kinds, EventId::new(0), upto, every)?;
        Ok((sets_of(&events), upto.as_i64()))
    }

    fn tend_agent_cases(&mut self, starting: bool) -> Result<bool> {
        let mut progressed = self.poll_label_jobs()?;
        if !starting {
            if self
                .agent_cases
                .as_ref()
                .is_some_and(|labeling| labeling.running.is_empty())
            {
                self.agent_cases = None;
            }
            return Ok(progressed);
        }
        let sets = self.case_sets()?;
        let Some(set) = sets
            .iter()
            .find(|set| set.refused.is_none() && set.built.is_none())
        else {
            return Ok(progressed);
        };
        // One owner per set: a set another live supervisor makes is left to
        // it, and one whose owner is gone is taken (its jobs were stopped
        // by the headless jobs' takeover, and its unlabeled cases are
        // labeled again).
        let (own, processes) = (self.layout.pid, self.processes.clone());
        let alive = move |pid: u32| pid != own && processes.alive(pid);
        if !self
            .queue
            .take_case_set(set.id, self.registration.token().as_str(), &alive)?
        {
            return Ok(progressed);
        }
        if set.prepared.is_none() {
            self.prepare_set(set)?;
            return Ok(true);
        }
        let idle = self
            .agent_cases
            .as_ref()
            .is_none_or(|labeling| labeling.running.is_empty());
        if idle && set.unlabeled().is_empty() {
            self.build_set(set)?;
            self.agent_cases = None;
            return Ok(true);
        }
        if self.agent_cases.as_ref().is_none_or(|l| l.set != set.id) {
            let Some(labeling) = self.labeling(set)? else {
                return Ok(progressed);
            };
            self.agent_cases = Some(labeling);
        }
        progressed |= self.start_label_jobs(set)?;
        Ok(progressed)
    }

    fn refuse_set(&self, id: i64, reason: &str, detail: String) -> Result<()> {
        info!("agent cases {id} not made: {detail}");
        self.queue
            .record_queue_event(
                EventKind::AgentCasesRefused,
                json!({"set_id": id, "reason": reason, "detail": detail}),
            )
            .map(drop)
    }

    /// What the cases of `agent` are judged in, read from `commit`
    /// ([`production::frame`] of its definition and case lists); `Err` with
    /// why when there is no definition or no code.
    fn frame(&self, agent: &str, commit: &str) -> Result<std::result::Result<Frame, String>> {
        let repository = &*self.repository;
        let Some((_, definition)) =
            find_definition(agent, |path| repository.file_in(commit, path))?
        else {
            return Ok(Err(format!(
                "no definition of {agent} in the landing branch's commit {commit}"
            )));
        };
        let mut lists = Vec::new();
        for split in Split::ALL {
            let path = format!("{DEFINITION_DIR}/{agent}/{EVALS_DIR}/{}", split.file_name());
            if let Some(text) = repository.file_in(commit, &path)? {
                lists.push((split.file_name(), text));
            }
        }
        let lists: Vec<(&str, &str)> = lists
            .iter()
            .map(|(name, text)| (*name, text.as_str()))
            .collect();
        Ok(production::frame(agent, &definition, &lists, commit))
    }

    /// Prepare `set`: its frame, its production reviews and each one's
    /// patch, recorded as `agent_cases_prepared`; or refuse it.
    fn prepare_set(&self, set: &CaseSet) -> Result<()> {
        let request = &set.request;
        let commit = self.repository.main_head()?.into_string();
        let frame = match self.frame(&request.agent, &commit)? {
            Ok(frame) => frame,
            Err(why) => return self.refuse_set(set.id, "definition_missing", why),
        };
        let kinds = [
            EventKind::ReviewStarted.as_str(),
            EventKind::ReviewFinished.as_str(),
        ];
        let upto = self.queue.latest_event_id()?;
        let every = usize::try_from(i64::MAX).unwrap_or(usize::MAX);
        let events = self
            .queue
            .events_of_between(&kinds, EventId::new(0), upto, every)?;
        let period = match production::period(request.since.as_deref(), request.until.as_deref()) {
            Ok(period) => period,
            Err(why) => return self.refuse_set(set.id, "period_invalid", why),
        };
        let found = candidates(
            &request.agent,
            period,
            events.iter().filter_map(|event| {
                Some(ReviewEvent {
                    run_id: event.run_id.as_ref()?.as_str(),
                    kind: &event.kind,
                    payload: &event.payload,
                    created_at: &event.created_at,
                })
            }),
        );
        let config = self.verifier.eval_config()?;
        if found.is_empty() {
            return self.refuse_set(
                set.id,
                "no_reviews",
                format!(
                    "no production review of the period selected {} and has its judgment",
                    request.agent
                ),
            );
        }
        if found.len() > config.max_runs as usize {
            return self.refuse_set(
                set.id,
                "too_many_reviews",
                format!(
                    "{} reviews of {} in the period, past [eval] max_runs {}: ask again for a shorter period",
                    found.len(),
                    request.agent,
                    config.max_runs
                ),
            );
        }
        let dir = set_dir(&self.layout.agent_cases_dir, set.id);
        let mut cases = Vec::new();
        let mut skipped = Vec::new();
        for review in found {
            let there = |commit: &str| self.repository.reachable_commit(commit).unwrap_or(false);
            if let Some(why) = unreachable(
                (&review.base, there(&review.base)),
                (&review.head, there(&review.head)),
            ) {
                skipped.push(json!({"id": review.id(), "run_id": review.run_id,
                    "attempt": review.attempt, "base": review.base, "head": review.head, "reason": why}));
                continue;
            }
            // A change that cannot be read is left out with why: one review
            // never stops the set, nor the sets asked for after it.
            match write_patch(
                &*self.repository,
                &*self.files,
                &dir,
                (&review.base, &review.head),
            ) {
                Ok(patch) => cases.push(PreparedCase { review, patch }),
                Err(error) => skipped.push(json!({"id": review.id(), "run_id": review.run_id,
                    "attempt": review.attempt, "base": review.base, "head": review.head,
                    "reason": format!("its change could not be read: {error:#}")})),
            }
        }
        info!(
            "agent cases {} of {}: {} case(s), {} review(s) left out",
            set.id,
            request.agent,
            cases.len(),
            skipped.len()
        );
        self.queue.record_queue_event(
            EventKind::AgentCasesPrepared,
            json!({
                "set_id": set.id,
                "agent": request.agent,
                "frame": frame.record(),
                "cases": cases.iter().map(PreparedCase::record).collect::<Vec<_>>(),
                "skipped": skipped,
            }),
        )?;
        Ok(())
    }

    /// What labels `set`'s cases, or `None` while its provider cannot be
    /// used.
    fn labeling(&self, set: &CaseSet) -> Result<Option<CaseLabeling>> {
        let review = self
            .role_models(ModelRole::Review)
            .launch(ModelRole::Review);
        let provider = label_provider(review.provider);
        if let Some(wait) = self.eval_wait(provider) {
            info!(
                "agent cases {}: its labels wait, {} cannot be used ({})",
                set.id,
                provider.as_str(),
                wait.as_str()
            );
            return Ok(None);
        }
        let frame = set.frame.clone().context("a prepared set has its frame")?;
        let definition = label_definition(&set.request.agent, &frame);
        let tools = AgentTools::declared(ToolRole::Review, &definition)
            .map_err(|why| anyhow::anyhow!("the label's tools: {why}"))?;
        Ok(Some(CaseLabeling {
            set: set.id,
            agent: set.request.agent.clone(),
            provider,
            launch: ActorLaunch {
                provider,
                model: None,
                effort: None,
                ..review
            },
            definition,
            tools,
            frame,
            concurrency: self.verifier.eval_config()?.concurrency,
            running: Vec::new(),
            again: BTreeSet::new(),
            dir: set_dir(&self.layout.agent_cases_dir, set.id),
        }))
    }

    /// Start a labeling job for each case of `set` with no label and none
    /// running, up to the eval's concurrency, while the provider can be
    /// used.
    fn start_label_jobs(&mut self, set: &CaseSet) -> Result<bool> {
        let mut labeling = self.agent_cases.take().expect("a set is labeled");
        let started = self.start_label_jobs_of(&mut labeling, set);
        self.agent_cases = Some(labeling);
        started
    }

    fn start_label_jobs_of(&mut self, labeling: &mut CaseLabeling, set: &CaseSet) -> Result<bool> {
        if self.eval_wait(labeling.provider).is_some() {
            return Ok(false);
        }
        let mut progressed = false;
        let prepared = set.prepared.as_deref().unwrap_or_default();
        for case in set.unlabeled() {
            if labeling.running.len() >= labeling.concurrency {
                break;
            }
            let id = case.id();
            if labeling.running.iter().any(|run| run.case == id) {
                continue;
            }
            let index = prepared
                .iter()
                .position(|known| known.id() == id)
                .unwrap_or_default();
            labeling.again.remove(&id);
            match self.start_label_job(labeling, case, index) {
                Ok(run) => labeling.running.push(run),
                Err(error) => {
                    warn!(error = %format_args!("{error:#}"), "agent cases {}: the label of {id} could not start: {error:#}", labeling.set);
                    self.record_label(labeling, &id, None, Some(format!("{error:#}")))?;
                }
            }
            progressed = true;
        }
        Ok(progressed)
    }

    /// Start the labeling job of `case` (the set's `index`th): its tree
    /// (`base` with the patch committed, the definitions hidden), its
    /// material, and the agent job of the rules alone on the label's
    /// provider.
    fn start_label_job(
        &mut self,
        labeling: &CaseLabeling,
        case: &PreparedCase,
        index: usize,
    ) -> Result<LabelJob> {
        let dir = labeling.dir.join("labels").join(index.to_string());
        self.files
            .create_dir_all(&dir)
            .with_context(|| format!("create {}", dir.display()))?;
        let patch = labeling
            .dir
            .join("patches")
            .join(format!("{}.patch", case.patch));
        let tree = dir.join("tree");
        let base = case.review.base.clone();
        let head = self
            .repository
            .add_case_tree(&tree, &base, &patch, &HIDDEN)?
            .into_string();
        let material_dir = dir.join("material");
        self.files
            .create_dir_all(&material_dir)
            .with_context(|| format!("create {}", material_dir.display()))?;
        let diff = material_dir.join("change.diff");
        if self.files.exists(&diff) {
            self.files.remove_file(&diff)?;
        }
        self.repository.diff_to_file(&base, &head, &diff)?;
        let commits = self.repository.log_oneline(&base, &head)?;
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
        let run_dir = dir.join("run");
        self.files
            .create_dir_all(&run_dir)
            .with_context(|| format!("create {}", run_dir.display()))?;
        let language = self.verifier.language();
        let reads = case_reads(self.layout, &tree, &material);
        let built = agent_job::build(&AgentJob {
            agent: &labeling.agent,
            definition: &labeling.definition,
            tools: &labeling.tools,
            rules: None,
            base: &base,
            head: &head,
            material: &material,
            cwd: &tree,
            dir: &run_dir,
            language: language.as_ref(),
            reads: Some(&reads),
        })?;
        self.files
            .write(&run_dir.join("prompt.txt"), built.prompt.text.as_bytes())?;
        let agent = self.job_agent(labeling.provider).with_context(|| {
            format!("no {} runs on this supervisor", labeling.provider.as_str())
        })?;
        let (stdout, stderr) = (run_dir.join("agent.out"), run_dir.join("agent.err"));
        let child = self
            .actors_on(agent)
            .spawn(ActorExecutionSpec::new(
                ActorContext::agent_eval_job(format_args!("cases-{}", labeling.set), index, 0),
                WorkspaceAccess::Read(tree.clone()),
                ActorProgram::Headless {
                    program: HeadlessProgram::AgentJob(&built.launch),
                    session_id: None,
                    launch: Some(&labeling.launch),
                    without_mcp: true,
                    env: Vec::new(),
                    without_env: &[],
                    streams: Streams::Files {
                        stdout: &stdout,
                        stderr: &stderr,
                    },
                },
            ))
            .context("start the labeling job")?
            .process()?;
        let id = case.id();
        let job = self.headless_job(
            "agent cases",
            child,
            stdout,
            stderr,
            JobSubject {
                kind: headless_job::AGENT_EVAL,
                job: JobKind::Agent,
                review_stage: true,
                label: Some(format!("{}:cases-{}:{id}:0", labeling.agent, labeling.set)),
                run_id: None,
                proposal_id: None,
                goal_id: None,
                attempt: 0,
                provider: labeling.provider,
            },
        );
        info!(
            "agent cases {}: labeling {id} on {}",
            labeling.set,
            labeling.provider.as_str()
        );
        Ok(LabelJob {
            case: id,
            job,
            tree,
        })
    }

    /// Record `case`'s label, or that it has none with why.
    fn record_label(
        &self,
        labeling: &CaseLabeling,
        case: &str,
        label: Option<production::Label>,
        error: Option<String>,
    ) -> Result<()> {
        self.queue
            .record_queue_event(
                EventKind::AgentCasesLabeled,
                json!({
                    "set_id": labeling.set,
                    "case": case,
                    "label": label.as_ref().map(production::Label::record),
                    "error": error,
                    "provider": labeling.provider,
                    "made_by": format!("{} (rules only, definitions hidden)", labeling.provider.as_str()),
                }),
            )
            .map(drop)
    }

    /// Reap the labeling jobs that ended: each one's label, or none with
    /// why; one stopped at its provider's wall holds the provider and its
    /// case is labeled again.
    fn poll_label_jobs(&mut self) -> Result<bool> {
        let Some(mut labeling) = self.agent_cases.take() else {
            return Ok(false);
        };
        let polled = self.poll_label_jobs_of(&mut labeling);
        self.agent_cases = Some(labeling);
        polled
    }

    fn poll_label_jobs_of(&mut self, labeling: &mut CaseLabeling) -> Result<bool> {
        let Some(agent) = self.job_agent(labeling.provider) else {
            return Ok(false);
        };
        let mut progressed = false;
        let mut index = 0;
        while index < labeling.running.len() {
            let end = match labeling.running[index].job.poll_end(&*self.files, agent) {
                Ok(Some(end)) => end,
                Ok(None) => {
                    index += 1;
                    continue;
                }
                Err(error) => Err(JobFailed::Exited(format!("{error:#}"))),
            };
            let run = labeling.running.remove(index);
            progressed = true;
            let (label, error) = match end {
                Ok(reply) => match agent_job::read_reply(&reply)
                    .and_then(|result| label_of(&result, &labeling.frame.codes))
                {
                    Some(label) => (Some(label), None),
                    None => (
                        None,
                        Some("the labeling job printed no readable verdict".to_owned()),
                    ),
                },
                Err(failed) => {
                    let error = failed.error().to_owned();
                    if let Some(reason) = self.job_failure(&run.job).switch_reason() {
                        let stdout = self
                            .files
                            .read_to_string(&run.job.stdout)
                            .unwrap_or_default();
                        if let Err(held) = self.hold_provider(
                            labeling.provider,
                            reason,
                            None,
                            &format!("{error}\n{stdout}"),
                        ) {
                            warn!(error = %format_args!("{held:#}"), "agent cases {}: {} could not be held: {held:#}", labeling.set, labeling.provider.as_str());
                        }
                        labeling.again.insert(run.case.clone());
                        self.remove_label_tree(&run.tree);
                        continue;
                    }
                    (None, Some(error))
                }
            };
            self.record_label(labeling, &run.case, label, error)?;
            self.remove_label_tree(&run.tree);
        }
        Ok(progressed)
    }

    fn remove_label_tree(&self, tree: &Path) {
        if let Err(error) = self.repository.remove_case_tree(tree) {
            warn!(error = %format_args!("{error:#}"), "agent cases: the case tree {} could not be removed: {error:#}", tree.display());
        }
    }

    /// Build `set` once every case has its label: its files on the queue's
    /// data dir, `agent_cases_built` with its digest, and one `decide` ask
    /// about its disputed cases.
    fn build_set(&mut self, set: &CaseSet) -> Result<()> {
        let dir = set_dir(&self.layout.agent_cases_dir, set.id);
        let made = view(set, &*self.repository, &*self.files, &dir)?;
        let cases = set.cases();
        let disputed: Vec<Value> = cases
            .iter()
            .filter_map(|case| {
                let why = case.disputed.as_ref()?;
                Some(json!({
                    "case": case.case.id(),
                    "reason": why,
                    "production": case.case.review.verdict,
                    "label": case.label.record(),
                }))
            })
            .collect();
        let ask_id = if disputed.is_empty() {
            None
        } else {
            Some(self.ask_disputed(set, &disputed)?)
        };
        info!(
            "agent cases {} of {} built: {} case(s), {} disputed, digest {}",
            set.id,
            set.request.agent,
            cases.len(),
            disputed.len(),
            made.digest
        );
        self.queue.record_queue_event(
            EventKind::AgentCasesBuilt,
            json!({
                "set_id": set.id,
                "agent": set.request.agent,
                "digest": made.digest,
                "cases": made.list["cases"].as_array().into_iter().flatten().map(|case| &case["id"]).collect::<Vec<_>>(),
                "left_out": made.left_out.iter().map(|(case, why)| json!({"case": case, "reason": why})).collect::<Vec<_>>(),
                "unlabeled": set.labels.iter().filter(|(_, record)| record.label.is_none()).map(|(case, record)| json!({"case": case, "error": record.error})).collect::<Vec<_>>(),
                "disputed": disputed,
                "ask_id": ask_id,
                "dir": dir.display().to_string(),
            }),
        )?;
        Ok(())
    }

    /// Ask the inbox for a person's decision on `set`'s disputed cases.
    fn ask_disputed(&mut self, set: &CaseSet, disputed: &[Value]) -> Result<i64> {
        let listed: Vec<String> = disputed
            .iter()
            .map(|case| {
                format!(
                    "- {}: production {}, label {} {} ({})",
                    case["case"].as_str().unwrap_or_default(),
                    case["production"].as_str().unwrap_or_default(),
                    case["label"]["verdict"].as_str().unwrap_or_default(),
                    case["label"]["codes"],
                    case["label"]["reason"].as_str().unwrap_or_default()
                )
            })
            .collect();
        let question = format!(
            "The production set {set} of {agent} has {n} case(s) whose label (by a provider other than the review's, from the rules alone) disagrees with the production verdict:\n{list}\n\
Each stays disputed, out of the primary scores, until a person decides its label with `dagq agent decide {set} <case> --verdict violation|clean [--codes <codes>] --reason <why>`; `dagq agent set {set}` shows the cases. \
Answer `decided` once the decisions are recorded, or `keep_disputed` to leave them out.",
            set = set.id,
            agent = set.request.agent,
            n = disputed.len(),
            list = listed.join("\n"),
        );
        // About the task of the first disputed case's review: an ask needs
        // a task or a run, and the set has neither.
        let anchor = set
            .cases()
            .iter()
            .find(|case| case.disputed.is_some())
            .map(|case| RunId::new(&case.case.review.run_id))
            .transpose()?
            .map(|run| self.queue.run(&run).map(|run| run.task_id()))
            .transpose()?;
        let outcome = crate::application::ask::ask(
            &mut *self.queue,
            NewAsk {
                recommendation: None,
                confidence: None,
                kind: AskKind::Decide,
                task_id: anchor,
                run_id: None,
                question,
                options: DISPUTED_OPTIONS.iter().map(|o| (*o).to_owned()).collect(),
                asked_by: "supervisor".to_owned(),
                reason_category: AskReason::Scope,
                topics: Vec::new(),
                finding_id: None,
                request_id: None,
            },
        )?;
        outcome["id"].as_i64().context("ask returned no id")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The label's provider is never the improving side's.
    #[test]
    fn the_label_is_another_providers() {
        assert_eq!(label_provider(Provider::Claude), Provider::Codex);
        assert_eq!(label_provider(Provider::Codex), Provider::Claude);
    }
}
