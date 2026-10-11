//! `review` (ADR-0016 decision 7): the review material of a run that awaits
//! integration or a session, written to `<run_dir>/review.md` for a
//! subagent or the headless review job to read.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::Path;

use super::{
    Repository, RunFiles, TaskStore, fenced, integrate::integrate_logs, or_none, path_text,
};
use crate::domain::review_subagents::{self, AgentRole, AgentTools, ReviewSubagent};
use crate::domain::{CommitSha, Goal, Provider, Receipt, RunStatus, Task, TaskId, TaskRun};
use sha2::{Digest, Sha256};

/// What `review` reads and writes through.
pub struct Review<'a> {
    pub queue: &'a mut dyn TaskStore,
    pub files: &'a dyn RunFiles,
    /// The repository of a run's checkout.
    pub open_repository: &'a dyn Fn(&Path) -> Result<Box<dyn Repository>>,
    /// Names the temporary files, so two reviews of one run do not collide.
    pub pid: u32,
}

/// Write the review material of the task's run that awaits integration or a
/// session to `<run_dir>/review.md` (temporary file, then rename) and report
/// where it is with the size of the diff (ADR-0016, decision 7). The file holds the
/// task, its goal, the receipt, the commits, the diffstat and the full diff
/// `<base>...<head>`, `head` being the receipt's commit and `base` where
/// the run's own commits start ([`range_base`]), so the review does not
/// repeat what other tasks landed while the run took in a main. The diff itself is never returned, so the caller
/// hands the path to a subagent instead of reading it. `range`, when
/// given, is the attempt's range already fixed ([`SubagentSnapshot::range`])
/// and is used as it is, so that the material shows the diff its required
/// agents were selected from even when `main` moved since.
pub fn review(ports: Review<'_>, task_id: TaskId, range: Option<&ReviewRange>) -> Result<Value> {
    let Review {
        queue,
        files,
        open_repository,
        pid,
    } = ports;
    let detail = queue.show(task_id)?;
    let run = detail
        .runs
        .iter()
        .find(|r| {
            matches!(
                r.status(),
                RunStatus::AwaitingIntegration | RunStatus::NeedsSession
            )
        })
        .cloned()
        .with_context(|| {
            format!(
                "task {task_id} ({}) has no run awaiting integration or a session",
                detail.task.status().as_str()
            )
        })?;
    let task = detail.task;
    let goal = match task.goal_id() {
        Some(goal_id) => Some(queue.show_goal(goal_id)?.goal),
        None => None,
    };
    let run_dir = Path::new(run.run_dir().context("missing run directory")?);
    let receipt_path = Path::new(run.receipt_path().context("missing receipt path")?);
    let receipt = Receipt::parse(
        &files
            .read_to_string(receipt_path)
            .with_context(|| format!("read receipt {}", receipt_path.display()))?,
    )?;
    let checkout = run
        .repo_path()
        .or(run.worktree_path())
        .context("run has no repository path")?;
    let repository = open_repository(Path::new(checkout))?;
    let ReviewRange { base, head } = match range {
        Some(range) => {
            anyhow::ensure!(
                range.head.eq_ignore_ascii_case(receipt.commit()),
                "the review's range ends at {}, not at the receipt's commit {}",
                range.head,
                receipt.commit()
            );
            range.clone()
        }
        None => review_range(&*repository, &run, &receipt)?,
    };
    let log = repository.log_oneline(&base, &head)?;
    let stat = repository.diff_stat(&base, &head)?;
    let numbers = repository.diff_numbers(&base, &head)?;
    let text = review_markdown(
        files,
        &task,
        &run,
        goal.as_ref(),
        &receipt,
        &base,
        &head,
        &log,
        &stat,
    );
    let path = run_dir.join("review.md");
    let temporary = run_dir.join(format!(".review.md.{pid}.tmp"));
    let diff = run_dir.join(format!(".review.md.{pid}.diff.tmp"));
    let written = write_review(&*repository, files, &base, &head, &text, &diff, &temporary)
        .and_then(|()| {
            files
                .rename(&temporary, &path)
                .with_context(|| format!("write {}", path.display()))
        });
    let _ = files.remove_file(&diff);
    if let Err(error) = written {
        let _ = files.remove_file(&temporary);
        return Err(error);
    }
    Ok(json!({
        "run_id": run.id(),
        "task_id": task.id(),
        "path": path_text(&path)?,
        "base": base,
        "head": head,
        "files_changed": numbers.files_changed,
        "insertions": numbers.insertions,
        "deletions": numbers.deletions,
    }))
}

/// The range `<base>...<head>` a review of `run` reads (see [`review`]):
/// `head` is the receipt's commit, `base` its [`range_base`] with the
/// current `main`.
pub fn review_range(
    repository: &dyn Repository,
    run: &TaskRun,
    receipt: &Receipt,
) -> Result<ReviewRange> {
    let main = repository.main_head()?;
    review_range_at(repository, run, receipt, &main)
}

/// [`review_range`] with `main`, the landing branch's commit, read
/// already.
pub fn review_range_at(
    repository: &dyn Repository,
    run: &TaskRun,
    receipt: &Receipt,
    main: &CommitSha,
) -> Result<ReviewRange> {
    let head = receipt.commit().to_ascii_lowercase();
    let base = range_base(repository, run, main, &head)?;
    Ok(ReviewRange { base, head })
}

/// Where `run`'s own commits up to `head` start: the merge base of `main`,
/// the landing branch's commit, and `head`, found as the declared paths'
/// check and `integrate` find it. A run that took in other tasks'
/// landings by a rebase or a merge onto any main since its claim starts
/// after them; one that took in none starts at its base commit. The run's
/// base commit when the two have no merge base.
pub fn range_base(
    repository: &dyn Repository,
    run: &TaskRun,
    main: &CommitSha,
    head: &str,
) -> Result<String> {
    Ok(repository
        .merge_base(main.as_str(), head)?
        .map_or_else(|| run.base_commit().to_string(), CommitSha::into_string))
}

/// `git log --oneline` of `run`'s own commits in its worktree
/// `worktree`, from their [`range_base`] with the landing branch's
/// current commit: what a new session of the run is handed as its work so
/// far, without the commits of other tasks it took in.
pub fn run_commits(repository: &dyn Repository, run: &TaskRun, worktree: &Path) -> Result<String> {
    let head = repository.head(worktree)?;
    let base = range_base(repository, run, &repository.main_head()?, head.as_str())?;
    repository.log_oneline(&base, head.as_str())
}

/// The range `<base>...<head>` of one review attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRange {
    pub base: String,
    pub head: String,
}

/// The configuration file whose `[review.subagents.<agent>]` a review
/// reads from the landing branch's committed tree.
pub const CONFIG_FILE: &str = "dagq.toml";

/// One required agent of a review as the snapshot holds it: its name,
/// the changed paths that selected it, its definition as committed on the
/// landing branch with the definition's SHA-256, and the tools the
/// definition declares (ADR-t1728-2; no launch is given them yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotAgent {
    pub name: String,
    pub matched: Vec<String>,
    pub definition_path: String,
    pub definition: String,
    pub digest: String,
    pub tools: AgentTools,
    /// The provider its `[review.subagents.<agent>]` names
    /// ([`ReviewSubagent::provider`]).
    pub provider: Option<Provider>,
}

/// The review's subagents read from the landing branch's commit `commit`
/// when a review starts (ADR-t1453-1 decisions 3 and 4): the agents its
/// range requires, none when the range touches no configured glob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentSnapshot {
    pub commit: String,
    /// The range the agents were selected from, fixed with `commit`; the
    /// attempt's review.md shows the same range.
    pub range: ReviewRange,
    pub agents: Vec<SnapshotAgent>,
}

impl SubagentSnapshot {
    /// What `review_started` records: the commit, and each agent with its
    /// matched paths and the digest of its definition.
    pub fn event_value(&self) -> Value {
        json!({
            "commit": self.commit,
            "base": self.range.base,
            "head": self.range.head,
            "agents": self.agents.iter().map(|agent| json!({
                "agent": agent.name,
                "paths": agent.matched,
                "definition": agent.definition_path,
                "digest": agent.digest,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Read the review's subagents from the landing branch's committed tree,
/// never from a worktree or the main checkout's files (ADR-t1453-1
/// decision 4), and select those the paths `<base>...<head>` changes
/// require (decision 3). `Ok(None)` when the landing branch has no
/// `dagq.toml` or one without `[review.subagents.*]`: the review goes as
/// before. `Err` when the file cannot be read or parsed, a selected
/// agent's definition is not in the tree, or its tools' declaration is a
/// mistake (ADR-t1728-2): the required checks are not known, so the
/// review must not pass.
/// `range` gives `<base>` and `<head>` for the landing branch's commit it
/// is given ([`review_range_at`]), the one the configuration is read
/// from, and is called only when agents are configured: the landing
/// branch is read once per attempt.
pub fn snapshot_subagents(
    repository: &dyn Repository,
    parse: &dyn Fn(&str) -> Result<Vec<ReviewSubagent>>,
    range: &dyn Fn(&CommitSha) -> Result<ReviewRange>,
) -> Result<Option<SubagentSnapshot>> {
    let main = repository.main_head()?;
    let commit = main.to_string();
    let Some(text) = repository
        .file_in(&commit, CONFIG_FILE)
        .with_context(|| format!("read {CONFIG_FILE} in the landing branch's commit {commit}"))?
    else {
        return Ok(None);
    };
    let configured = parse(&text)
        .with_context(|| format!("parse {CONFIG_FILE} in the landing branch's commit {commit}"))?;
    if configured.is_empty() {
        return Ok(None);
    }
    let range = range(&main)?;
    // `<base>...<head>`: from their merge base, as review.md's diff.
    let from = repository
        .merge_base(&range.base, &range.head)?
        .map_or_else(|| range.base.clone(), CommitSha::into_string);
    let changed = repository.changed_paths(&from, &range.head)?;
    let mut agents = Vec::new();
    for selected in review_subagents::select(&configured, &changed) {
        let (definition_path, definition) = review_subagents::find_definition(&selected.name, |path| {
            repository
                .file_in(&commit, path)
                .with_context(|| format!("read {path} in the landing branch's commit {commit}"))
        })?
        .with_context(|| {
            format!(
                "the review subagent {} that {CONFIG_FILE} names has no definition {} in the landing branch's commit {commit}",
                selected.name,
                review_subagents::definition_path(&selected.name)
            )
        })?;
        let tools = AgentTools::declared(AgentRole::Review, &definition).map_err(|why| {
            anyhow::anyhow!(
                "the review subagent {}'s definition {definition_path} in the landing branch's commit {commit} is a mistake: {why}",
                selected.name
            )
        })?;
        let provider = configured
            .iter()
            .find(|agent| agent.name == selected.name)
            .and_then(|agent| agent.provider);
        agents.push(SnapshotAgent {
            provider,
            digest: format!("{:x}", Sha256::digest(definition.as_bytes())),
            tools,
            name: selected.name,
            matched: selected.matched,
            definition_path,
            definition,
        });
    }
    Ok(Some(SubagentSnapshot {
        commit,
        range,
        agents,
    }))
}

/// `doctor`'s check of the agents the role sections of the landing
/// branch's `dagq.toml` name (ADR-t1728-1): each agent with its section
/// and the definition read ([`review_subagents::find_definition`], `null`
/// without one), and `errors`, an agent without a definition, named in
/// more than one role's section, or whose definition declares tools that
/// are a mistake (ADR-t1728-2). `Ok(None)` without `dagq.toml` or without
/// an agent named; `Err` when the file cannot be read or parsed. A review
/// does not pass in either case meanwhile (ADR-t1453-1 decision 6).
pub fn check_agents(
    repository: &dyn Repository,
    parse: &dyn Fn(&str) -> Result<Vec<ReviewSubagent>>,
) -> Result<Option<Value>> {
    let commit = repository.main_head()?.into_string();
    let Some(text) = repository
        .file_in(&commit, CONFIG_FILE)
        .with_context(|| format!("read {CONFIG_FILE} in the landing branch's commit {commit}"))?
    else {
        return Ok(None);
    };
    let configured = parse(&text)
        .with_context(|| format!("parse {CONFIG_FILE} in the landing branch's commit {commit}"))?;
    let named = review_subagents::named_for_review(&configured);
    if named.is_empty() {
        return Ok(None);
    }
    let mut agents = Vec::new();
    let mut defined = Vec::new();
    let mut declared = Vec::new();
    for entry in &named {
        let found = review_subagents::find_definition(&entry.agent, |path| {
            repository
                .file_in(&commit, path)
                .with_context(|| format!("read {path} in the landing branch's commit {commit}"))
        })?;
        if let Some((path, text)) = &found {
            defined.push(entry.agent.clone());
            // Today's one role section is the review's.
            if let Err(why) = AgentTools::declared(AgentRole::Review, text) {
                declared.push(format!("[{}.{}] {path}: {why}", entry.section, entry.agent));
            }
        }
        agents.push(json!({
            "agent": entry.agent,
            "section": entry.section,
            "definition": found.map(|(path, _)| path),
        }));
    }
    let mut errors =
        review_subagents::config_problems(&named, &|agent| defined.iter().any(|d| d == agent));
    errors.extend(declared);
    Ok(Some(
        json!({"commit": commit, "agents": agents, "errors": errors}),
    ))
}

/// Write `text` and then the full diff `<base>...<head>` as a fenced block to
/// `temporary`. Git streams the diff to the file `diff` first, as raw bytes
/// and never through memory, because the fence must be longer than any
/// backtick run in it; the file is then copied under the fence.
fn write_review(
    repository: &dyn Repository,
    files: &dyn RunFiles,
    base: &str,
    head: &str,
    text: &str,
    diff: &Path,
    temporary: &Path,
) -> Result<()> {
    repository.diff_to_file(base, head, diff)?;
    files.write_fenced(temporary, text, "diff", diff)
}

/// Where review.md says the verification logs are: integrate writes
/// `integrate-<attempt>-verify-N.log` per attempt, and the latest attempt's
/// logs are named when one ran.
pub fn review_logs_hint(files: &dyn RunFiles, run_dir: Option<&str>) -> String {
    let Some(run_dir) = run_dir else {
        return "(no run directory)".to_owned();
    };
    let pattern = format!(
        "{run_dir}/integrate-<attempt>-verify-N.log (one set per integrate attempt, written when integrate runs the verification commands after its rebase)"
    );
    let (latest, _) = integrate_logs(files, Path::new(run_dir));
    if latest.is_empty() {
        return format!("{pattern}; none yet");
    }
    let latest: Vec<String> = latest.iter().map(|p| p.display().to_string()).collect();
    format!("{pattern}; latest attempt: {}", latest.join(", "))
}

/// The Markdown before the diff: the task (its context too, where a
/// planner names the related documents: ADR-t1428-1), its goal, the receipt, the
/// commits and the diffstat. Fences are longer than any backtick run in
/// what they hold, so a diff of Markdown cannot close them early.
#[allow(clippy::too_many_arguments)]
fn review_markdown(
    files: &dyn RunFiles,
    task: &Task,
    run: &TaskRun,
    goal: Option<&Goal>,
    receipt: &Receipt,
    base: &str,
    head: &str,
    log: &str,
    stat: &str,
) -> String {
    let mut out = format!(
        "# Review of task {id}: {title}\n\n\
         - run: {run_id} ({status})\n\
         - base: {base} (run base {run_base})\n\
         - head: {head}\n\
         - branch: {branch}\n\
         - worktree: {worktree}\n\
         - verification logs: {logs}\n\n\
         ## Task\n\n\
         ### Description\n\n{description}\n\n\
         ### Context\n\n{context}\n\n\
         ### Acceptance\n\n{acceptance}\n\n\
         ### Verification commands\n\n{verify}\n",
        id = task.id(),
        title = task.title(),
        run_id = run.id(),
        status = run.status().as_str(),
        run_base = run.base_commit(),
        branch = run.branch().unwrap_or("(none)"),
        worktree = run.worktree_path().unwrap_or("(none)"),
        logs = review_logs_hint(files, run.run_dir()),
        description = or_none(task.description()),
        context = or_none(task.context()),
        acceptance = or_none(task.acceptance()),
        verify = fenced("sh", &task.verification_commands().join("\n")),
    );
    if let Some(goal) = goal {
        out.push_str(&format!(
            "\n## Goal {id}: {title}\n\n\
             ### Goal acceptance\n\n{acceptance}\n\n\
             ### Goal constraints\n\n{constraints}\n",
            id = goal.id(),
            title = goal.title(),
            acceptance = or_none(goal.acceptance()),
            constraints = or_none(goal.constraints()),
        ));
    }
    out.push_str(&format!(
        "\n## Receipt\n\n### Summary\n\n{summary}\n",
        summary = or_none(receipt.summary())
    ));
    for (name, check) in [
        ("Tests", receipt.tests()),
        ("E2E", receipt.e2e()),
        ("Subagent review", receipt.subagent_review()),
    ] {
        out.push_str(&format!(
            "\n### {name}: {status}\n\n{evidence}\n",
            status = check.status().as_str(),
            evidence = or_none(check.evidence_or_reason()),
        ));
    }
    let follow_ups = match receipt.follow_ups() {
        Some(Value::Array(items)) if !items.is_empty() => items
            .iter()
            .map(|item| {
                format!(
                    "- {}: {}",
                    item["title"].as_str().unwrap_or("(untitled)"),
                    item["description"].as_str().unwrap_or("")
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => "(none)".to_owned(),
    };
    out.push_str(&format!("\n### Follow-ups\n\n{follow_ups}\n"));
    out.push_str(&format!(
        "\n## Commits\n\n`git log --oneline {base}..{head}`\n\n{log}\n\
         ## Diffstat\n\n`git diff --stat {base}...{head}`\n\n{stat}\n\
         ## Diff\n\n`git diff {base}...{head}`\n\n",
        log = fenced("", log),
        stat = fenced("", stat),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::memory_files::MemoryFiles;
    use crate::domain::{RunId, RunRecord, TaskRecord, TaskStatus};
    use serde_json::json;

    const SHA: &str = "1111111111111111111111111111111111111111";
    const RUN: &str = "00000000-0000-4000-8000-000000000001";

    fn task(context: &str) -> Task {
        Task::restore(TaskRecord {
            goal_priority: None,
            id: TaskId::new(7),
            title: "work".into(),
            description: "change the behavior".into(),
            acceptance: "the behavior changes".into(),
            verification_commands: vec!["make gate".into()],
            required_evidence: Vec::new(),
            paths: Vec::new(),
            priority: Default::default(),
            change: None,
            status: TaskStatus::InProgress,
            goal_id: None,
            context: context.into(),
            created_at: String::new(),
            updated_at: String::new(),
            worker: crate::domain::worker::Worker::CLAUDE_INTERACTIVE,
            named_mode: None,
            wait_for_build: false,
            execution_class: Default::default(),
        })
        .unwrap()
    }

    fn run() -> TaskRun {
        run_on(SHA)
    }

    /// A run claimed on the base commit `base`.
    fn run_on(base: &str) -> TaskRun {
        TaskRun::restore(RunRecord {
            id: RunId::new(RUN).unwrap(),
            task_id: TaskId::new(7),
            status: RunStatus::AwaitingIntegration,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: crate::domain::worker::WorkerMode::Headless,
            base_commit: CommitSha::try_from(base).unwrap(),
            branch: None,
            worktree_path: None,
            workspace_id: None,
            receipt_path: None,
            log_path: None,
            result_commit: None,
            repo_path: None,
            run_dir: None,
            last_error: None,
            workspace_closed_at: None,
            created_at: String::new(),
        })
        .unwrap()
    }

    fn material(task: &Task) -> String {
        let receipt = Receipt::parse(
            &json!({
                "run_id": RUN,
                "result": "succeeded",
                "commit": SHA,
                "tests": {"status": "passed", "evidence_or_reason": "unit"},
                "e2e": {"status": "not_applicable", "evidence_or_reason": "none"},
                "subagent_review": {"status": "not_applicable", "evidence_or_reason": "small"},
                "summary": "done",
            })
            .to_string(),
        )
        .unwrap();
        review_markdown(
            &MemoryFiles::default(),
            task,
            &run(),
            None,
            &receipt,
            SHA,
            SHA,
            "",
            "",
        )
    }

    /// Task 1429 (ADR-t1428-1): the material carries the task's context,
    /// where a planner names the related documents, between its
    /// description and its acceptance; `(none)` when it is empty.
    #[test]
    fn the_review_material_carries_the_task_context_or_none() {
        let named = material(&task("Read docs/design/review.md, section Material."));
        assert!(
            named.contains(
                "### Description\n\nchange the behavior\n\n\
                 ### Context\n\nRead docs/design/review.md, section Material.\n\n\
                 ### Acceptance\n\nthe behavior changes\n"
            ),
            "{named}"
        );
        let empty = material(&task("  "));
        assert!(
            empty.contains("### Context\n\n(none)\n\n### Acceptance"),
            "{empty}"
        );
        assert_eq!(empty.matches("### Context").count(), 1);
    }

    /// A repository whose landing branch is at [`SHA`] and holds `files`
    /// there only, and whose reviewed range changes `changed`: what
    /// [`snapshot_subagents`] reads, with nothing else. `merge_bases`
    /// gives the merge base of each pair of commits it has one for, and
    /// `head` a worktree's commit, for the range's base.
    #[derive(Default)]
    struct Committed {
        files: Vec<(&'static str, &'static str)>,
        changed: Vec<String>,
        merge_bases: Vec<(&'static str, &'static str, &'static str)>,
        head: Option<&'static str>,
    }

    impl Repository for Committed {
        fn is_dagq_source(&self) -> bool {
            false
        }
        fn main_head(&self) -> Result<CommitSha> {
            Ok(CommitSha::try_from(SHA).unwrap())
        }
        fn file_in(&self, commit: &str, path: &str) -> Result<Option<String>> {
            // Only the landing branch's commit is ever read.
            assert_eq!(commit, SHA);
            Ok(self
                .files
                .iter()
                .find(|(name, _)| *name == path)
                .map(|(_, text)| (*text).to_owned()))
        }
        fn merge_base(&self, a: &str, b: &str) -> Result<Option<CommitSha>> {
            Ok(self
                .merge_bases
                .iter()
                .find(|(x, y, _)| (*x, *y) == (a, b))
                .map(|(_, _, base)| CommitSha::try_from(*base).unwrap()))
        }
        fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
            assert_eq!((from, to), ("base", "head"));
            Ok(self.changed.clone())
        }
        fn current_branch(&self, _: &Path) -> Result<Option<String>> {
            unreachable!()
        }
        fn head(&self, _: &Path) -> Result<CommitSha> {
            Ok(CommitSha::try_from(self.head.unwrap()).unwrap())
        }
        fn is_ancestor(&self, _: &str, _: &str) -> Result<bool> {
            unreachable!()
        }
        fn status(&self, _: &Path) -> Result<String> {
            unreachable!()
        }
        fn rebase_in_progress(&self, _: &Path) -> Result<bool> {
            unreachable!()
        }
        fn rebase_abort(&self, _: &Path) -> Result<()> {
            unreachable!()
        }
        fn rebase(&self, _: &Path, _: &str) -> Result<std::result::Result<(), String>> {
            unreachable!()
        }
        fn conflicted_files(&self, _: &Path) -> Result<Vec<String>> {
            unreachable!()
        }
        fn added_paths(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unreachable!()
        }
        fn paths_in(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unreachable!()
        }
        fn paths_containing(&self, _: &str, _: &str, _: &[String]) -> Result<Vec<String>> {
            unreachable!()
        }
        fn rename_and_commit(
            &self,
            _: &Path,
            _: &str,
            _: &str,
            _: &[String],
        ) -> Result<std::result::Result<CommitSha, String>> {
            unreachable!()
        }
        fn tree_of(&self, _: &str) -> Result<String> {
            unreachable!()
        }
        fn commit_tree(&self, _: &str, _: &str, _: &[String]) -> Result<CommitSha> {
            unreachable!()
        }
        fn update_ref(&self, _: &str, _: &str) -> Result<()> {
            unreachable!()
        }
        fn advance_main(
            &self,
            _: &crate::domain::landing_branch::LandingBranch,
            _: &str,
            _: &str,
        ) -> Result<()> {
            unreachable!()
        }
        fn repair_worktree(&self, _: &Path) -> Result<()> {
            unreachable!()
        }
        fn remove_worktree_and_branch(&self, _: &Path, _: &str) -> Result<()> {
            unreachable!()
        }
        fn branches(&self) -> Result<Vec<String>> {
            unreachable!()
        }
        fn delete_branch(&self, _: &str) -> Result<()> {
            unreachable!()
        }
        fn tracks(&self, _: &Path, _: &str) -> Result<bool> {
            unreachable!()
        }
        fn main_checkout(&self) -> Result<Option<std::path::PathBuf>> {
            unreachable!()
        }
        fn create_worktree(&self, _: &TaskRun) -> Result<String> {
            unreachable!()
        }
        fn merge_conflicts(&self, _: &str, _: &str) -> Result<Vec<String>> {
            unreachable!()
        }
        fn landed_task_ids(&self, _: &str, _: &str) -> Result<Vec<TaskId>> {
            unreachable!()
        }
        fn log_oneline(&self, base: &str, head: &str) -> Result<String> {
            Ok(format!("{base}..{head}"))
        }
        fn diff_stat(&self, _: &str, _: &str) -> Result<String> {
            unreachable!()
        }
        fn diff_numbers(&self, _: &str, _: &str) -> Result<super::super::DiffNumbers> {
            unreachable!()
        }
        fn diff_to_file(&self, _: &str, _: &str, _: &Path) -> Result<()> {
            unreachable!()
        }
    }

    const CONFIG: &str = "the committed dagq.toml";
    const DEFINITION: &str = "---\ndescription: main's design checks\n---\nCheck the design.\n";

    /// `design` reviews `change.txt` and `docs/**`; `unused` matches
    /// nothing and has no definition. A config of `bad` does not parse;
    /// one without the table names no agent.
    fn parse(text: &str) -> Result<Vec<ReviewSubagent>> {
        let agent = |name: &str, paths: &[&str]| ReviewSubagent {
            name: name.to_owned(),
            provider: None,
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        };
        match text {
            CONFIG => Ok(vec![
                agent("design", &["change.txt", "docs/**"]),
                agent("unused", &["nothing/**"]),
            ]),
            "bad" => anyhow::bail!("dagq.toml:1: [review.subagents.design] has no paths"),
            _ => Ok(Vec::new()),
        }
    }

    fn snapshot(
        files: Vec<(&'static str, &'static str)>,
        changed: &[&str],
    ) -> Result<Option<SubagentSnapshot>> {
        let repository = Committed {
            files,
            changed: changed.iter().map(|p| (*p).to_owned()).collect(),
            ..Committed::default()
        };
        let range = |main: &CommitSha| {
            assert_eq!(main.as_str(), SHA);
            Ok(ReviewRange {
                base: "base".to_owned(),
                head: "head".to_owned(),
            })
        };
        snapshot_subagents(&repository, &parse, &range)
    }

    /// The definition is read from `.dagq/agents/<agent>/AGENT.md` only
    /// (ADR-t1728-1), and `review_started` records that path; a file
    /// elsewhere, such as the one file per agent of the directory before
    /// ADR-t1728-1, is no definition, so the review does not pass.
    #[test]
    fn the_definition_is_read_from_the_agents_directory_only() {
        let found = snapshot(
            vec![
                (CONFIG_FILE, CONFIG),
                (".dagq/agents/design/AGENT.md", DEFINITION),
            ],
            &["change.txt"],
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            found.event_value()["agents"][0]["definition"],
            json!(".dagq/agents/design/AGENT.md")
        );
        assert_eq!(found.agents[0].definition, DEFINITION);
        let error = format!(
            "{:#}",
            snapshot(
                vec![
                    (CONFIG_FILE, CONFIG),
                    // The directory before ADR-t1728-1, its name split so
                    // that no path to it is left in the code.
                    (concat!(".dagq/review", "-agents/design.md"), DEFINITION),
                ],
                &["change.txt"],
            )
            .unwrap_err()
        );
        assert!(
            error.contains("has no definition .dagq/agents/design/AGENT.md"),
            "{error}"
        );
    }

    /// `doctor`'s check of the landing branch's agents (ADR-t1728-1): each
    /// named agent with the definition read, and an agent without one as
    /// an error; nothing to report without `dagq.toml` or agents.
    #[test]
    fn doctor_checks_that_every_named_agent_has_a_definition() {
        let check = |files: Vec<(&'static str, &'static str)>| {
            check_agents(
                &Committed {
                    files,
                    ..Committed::default()
                },
                &parse,
            )
        };
        assert_eq!(check(Vec::new()).unwrap(), None);
        assert_eq!(check(vec![(CONFIG_FILE, "[run.env]")]).unwrap(), None);
        assert!(
            format!("{:#}", check(vec![(CONFIG_FILE, "bad")]).unwrap_err())
                .contains("[review.subagents.design] has no paths")
        );
        assert_eq!(
            check(vec![
                (CONFIG_FILE, CONFIG),
                (".dagq/agents/design/AGENT.md", DEFINITION),
            ])
            .unwrap(),
            Some(json!({"commit": SHA, "agents": [
                {"agent": "design", "section": "review.subagents", "definition": ".dagq/agents/design/AGENT.md"},
                {"agent": "unused", "section": "review.subagents", "definition": null},
            ], "errors": [
                "[review.subagents.unused] names an agent without a definition: .dagq/agents/unused/AGENT.md is not committed",
            ]}))
        );
        // A file elsewhere, as in the directory before ADR-t1728-1, is no
        // definition.
        let elsewhere = check(vec![
            (CONFIG_FILE, CONFIG),
            (concat!(".dagq/review", "-agents/design.md"), DEFINITION),
            (".dagq/agents/unused.md", DEFINITION),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(elsewhere["agents"][0]["definition"], json!(null));
        assert_eq!(elsewhere["errors"].as_array().unwrap().len(), 2);
    }

    /// The definition's tools (ADR-t1728-2): a declaration from the
    /// runtime's list is kept with the agent, none is the review's reads,
    /// and a name not in the list or a tool beyond a review's reads is a
    /// mistake the review does not pass and `doctor` reports.
    #[test]
    fn a_definitions_tools_are_checked_by_the_snapshot_and_doctor() {
        const READS: &str = "---\ndescription: d\ntools: [read]\n---\nCheck.\n";
        const SHELL: &str = "---\ndescription: d\ntools: [read, shell]\n---\nCheck.\n";
        let tools = |definition| {
            snapshot(
                vec![
                    (CONFIG_FILE, CONFIG),
                    (".dagq/agents/design/AGENT.md", definition),
                ],
                &["change.txt"],
            )
            .map(|found| found.unwrap().agents[0].tools.clone())
        };
        assert_eq!(
            tools(READS).unwrap(),
            AgentTools::new([review_subagents::AgentTool::Read])
        );
        assert_eq!(
            tools(DEFINITION).unwrap(),
            AgentRole::Review.default_tools()
        );
        let why = "shell is beyond what an agent of [review.subagents] may use (read, grep, glob)";
        let refused = format!("{:#}", tools(SHELL).unwrap_err());
        assert!(
            refused.contains(&format!(
                "the review subagent design's definition .dagq/agents/design/AGENT.md in the landing branch's commit {SHA} is a mistake: the frontmatter's tools declares {why}"
            )),
            "{refused}"
        );
        let checked = check_agents(
            &Committed {
                files: vec![
                    (CONFIG_FILE, CONFIG),
                    (".dagq/agents/design/AGENT.md", SHELL),
                    (".dagq/agents/unused/AGENT.md", READS),
                ],
                ..Committed::default()
            },
            &parse,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            checked["errors"],
            json!([format!(
                "[review.subagents.design] .dagq/agents/design/AGENT.md: the frontmatter's tools declares {why}"
            )])
        );
    }

    /// The review's subagents as the landing branch's commit has them
    /// (ADR-t1453-1 decisions 3 and 4): those the range's paths select,
    /// with the committed definition and its digest; the snapshot the
    /// event records.
    #[test]
    fn the_committed_config_selects_the_agents_the_range_requires() {
        let found = snapshot(
            vec![
                (CONFIG_FILE, CONFIG),
                (".dagq/agents/design/AGENT.md", DEFINITION),
            ],
            &["change.txt", "other.txt"],
        )
        .unwrap()
        .unwrap();
        let digest = format!("{:x}", Sha256::digest(DEFINITION.as_bytes()));
        let expected = json!({"commit": SHA, "base": "base", "head": "head", "agents": [{
            "agent": "design", "paths": ["change.txt"],
            "definition": ".dagq/agents/design/AGENT.md", "digest": digest}]});
        assert_eq!(found.event_value(), expected);
    }

    /// Without `[review.subagents]` on the landing branch (no `dagq.toml`,
    /// or one without the table) the review is as before: no snapshot, so
    /// no `subagents` in `review_started` and no word of subagents in the
    /// prompt or the material. With the table but a range
    /// it does not select, the snapshot records the empty selection.
    #[test]
    fn a_review_without_required_agents_reads_as_before() {
        assert!(snapshot(Vec::new(), &["change.txt"]).unwrap().is_none());
        assert!(
            snapshot(vec![(CONFIG_FILE, "[run.env]")], &["change.txt"])
                .unwrap()
                .is_none()
        );
        let none = snapshot(vec![(CONFIG_FILE, CONFIG)], &["other.txt"])
            .unwrap()
            .unwrap();
        assert_eq!(
            none.event_value(),
            json!({"commit": SHA, "base": "base", "head": "head", "agents": []})
        );
        let prompt =
            crate::application::prompt::review_prompt(&task(""), &run(), "/runs/r/review.md").text;
        assert!(!prompt.contains("subagent"), "{prompt}");
        let material = material(&task(""));
        assert!(!material.contains("subagent"), "{material}");
    }

    /// The required checks cannot be known, so the review must not pass
    /// (ADR-t1453-1 decision 4): a committed `dagq.toml` that does not
    /// parse, or a selected agent whose definition is not in the landing
    /// branch's commit, is an error saying which. An agent that is not
    /// selected needs no definition.
    #[test]
    fn an_unreadable_config_or_a_missing_definition_is_an_error() {
        let error = format!(
            "{:#}",
            snapshot(vec![(CONFIG_FILE, "bad")], &["change.txt"]).unwrap_err()
        );
        assert!(
            error.contains(&format!(
                "parse dagq.toml in the landing branch's commit {SHA}"
            )) && error.contains("[review.subagents.design] has no paths"),
            "{error}"
        );
        let error = format!(
            "{:#}",
            snapshot(vec![(CONFIG_FILE, CONFIG)], &["change.txt"]).unwrap_err()
        );
        assert!(
            error.contains(&format!(
                "the review subagent design that dagq.toml names has no definition .dagq/agents/design/AGENT.md in the landing branch's commit {SHA}"
            )),
            "{error}"
        );
    }
    const CLAIMED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const MIDWAY: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const HEAD: &str = "cccccccccccccccccccccccccccccccccccccccc";

    /// The base of the range from `main` ([`SHA`]) to [`HEAD`] of a run
    /// claimed on [`CLAIMED`], in a repository where their merge base is
    /// `merge_base`.
    fn base_with(merge_base: Option<&'static str>) -> String {
        let repository = Committed {
            merge_bases: merge_base
                .map(|base| (SHA, HEAD, base))
                .into_iter()
                .collect(),
            ..Committed::default()
        };
        range_base(
            &repository,
            &run_on(CLAIMED),
            &CommitSha::try_from(SHA).unwrap(),
            HEAD,
        )
        .unwrap()
    }

    /// The range starts at the merge base of the current main and the
    /// head: a run rebased onto a main that moved on since starts at that
    /// midway main, so other tasks' commits before it are not the run's;
    /// one not rebased starts at its base commit, one rebased onto the
    /// current main at that main, as before; without a merge base, at the
    /// run's base commit.
    #[test]
    fn the_range_starts_at_the_merge_base_of_the_current_main_and_the_head() {
        assert_eq!(base_with(Some(MIDWAY)), MIDWAY);
        assert_eq!(base_with(Some(CLAIMED)), CLAIMED);
        assert_eq!(base_with(Some(SHA)), SHA);
        assert_eq!(base_with(None), CLAIMED);
        let receipt = Receipt::parse(
            &json!({
                "run_id": RUN,
                "result": "succeeded",
                "commit": HEAD.to_ascii_uppercase(),
                "tests": {"status": "passed", "evidence_or_reason": "unit"},
                "e2e": {"status": "not_applicable", "evidence_or_reason": "none"},
                "subagent_review": {"status": "not_applicable", "evidence_or_reason": "small"},
                "summary": "done",
            })
            .to_string(),
        )
        .unwrap();
        let repository = Committed {
            merge_bases: vec![(SHA, HEAD, MIDWAY)],
            ..Committed::default()
        };
        assert_eq!(
            review_range_at(
                &repository,
                &run_on(CLAIMED),
                &receipt,
                &CommitSha::try_from(SHA).unwrap()
            )
            .unwrap(),
            ReviewRange {
                base: MIDWAY.to_owned(),
                head: HEAD.to_owned(),
            }
        );
    }

    /// A new session of a run rebased onto a midway main is handed the
    /// commits from that main to its worktree's head, not those of other
    /// tasks since its claim.
    #[test]
    fn a_handoff_shows_the_runs_own_commits_only() {
        let repository = Committed {
            merge_bases: vec![(SHA, HEAD, MIDWAY)],
            head: Some(HEAD),
            ..Committed::default()
        };
        assert_eq!(
            run_commits(&repository, &run_on(CLAIMED), Path::new("/worktree")).unwrap(),
            format!("{MIDWAY}..{HEAD}")
        );
    }
}
