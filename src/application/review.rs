//! `review` (ADR-0016 decision 7): the review material of a run that awaits
//! integration or a session, written to `<run_dir>/review.md` for a
//! subagent or the headless review job to read.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::Path;

use super::{
    Repository, RunFiles, TaskStore, fenced, integrate::integrate_logs, or_none, path_text,
};
use crate::domain::review_subagents::{self, ReviewSubagent};
use crate::domain::{CommitSha, Goal, Receipt, RunStatus, Task, TaskId, TaskRun};
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
/// `<base>...<head>`, `base` being the run's base commit and `head` the
/// receipt's commit. When a session already rebased `head` onto the current
/// `main`, `base` is that `main` instead, so the review does not repeat
/// what other tasks landed meanwhile. The diff itself is never returned, so the caller
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
/// `head` is the receipt's commit, `base` the run's base commit, or the
/// current `main` when a session already rebased `head` onto it.
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
    let base = if main != run.base_commit() && repository.is_ancestor(main.as_str(), &head)? {
        main.to_string()
    } else {
        run.base_commit().to_string()
    };
    Ok(ReviewRange { base, head })
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
/// the changed paths that selected it, and its definition as committed on
/// the landing branch with the definition's SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotAgent {
    pub name: String,
    pub matched: Vec<String>,
    pub definition_path: String,
    pub definition: String,
    pub digest: String,
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

    /// What the review job reads (`review-subagents-<attempt>.json`): the
    /// event's value with each definition's text.
    pub fn job_input(&self) -> Value {
        json!({
            "commit": self.commit,
            "base": self.range.base,
            "head": self.range.head,
            "agents": self.agents.iter().map(|agent| json!({
                "agent": agent.name,
                "paths": agent.matched,
                "definition": agent.definition_path,
                "digest": agent.digest,
                "text": agent.definition,
            })).collect::<Vec<_>>(),
        })
    }
}

/// Read the review's subagents from the landing branch's committed tree,
/// never from a worktree or the main checkout's files (ADR-t1453-1
/// decision 4), and select those the paths `<base>...<head>` changes
/// require (decision 3). `Ok(None)` when the landing branch has no
/// `dagq.toml` or one without `[review.subagents.*]`: the review goes as
/// before. `Err` when the file cannot be read or parsed or a selected
/// agent's definition is not in the tree: the required checks are not
/// known, so the review must not pass.
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
        let definition_path = review_subagents::definition_path(&selected.name);
        let definition = repository
            .file_in(&commit, &definition_path)
            .with_context(|| format!("read {definition_path} in the landing branch's commit {commit}"))?
            .with_context(|| {
                format!(
                    "the review subagent {} that {CONFIG_FILE} names has no definition {definition_path} in the landing branch's commit {commit}",
                    selected.name
                )
            })?;
        agents.push(SnapshotAgent {
            digest: format!("{:x}", Sha256::digest(definition.as_bytes())),
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

/// What the review's prompt adds when its range requires agents: the
/// agents, the paths that selected each, and where their definitions as
/// committed on the landing branch are (`input`).
pub fn review_subagents_prompt(snapshot: &SubagentSnapshot, input: &Path) -> String {
    let mut out = format!(
        "\nRequired review subagents: the changes of this review require the agents below. Their definitions, as committed on the landing branch at {commit}, are in {input} (one entry per agent, its text under \"text\").\n",
        commit = snapshot.commit,
        input = input.display(),
    );
    for agent in &snapshot.agents {
        out.push_str(&format!(
            "- {name} (changed: {paths})\n",
            name = agent.name,
            paths = agent.matched.join(", "),
        ));
    }
    out.push_str(SUBAGENTS_INSTRUCTION);
    out
}

/// How the review runs its required subagents and reports their results
/// (ADR-t1453-1 decisions 5 and 6), after the list of agents.
pub const SUBAGENTS_INSTRUCTION: &str = "Each of these agents is given to you as a subagent of the same name with that definition. Besides your own review, run every one of them as a subagent on this review's changes, wait until all of them have finished, and fold their findings into your one verdict. Your verdict must not be lighter than any agent's (pass < revise < concern), and it keeps each agent's reasons, recommendation, confidence and reason_category. Do not skip an agent: one that could not run or finish is reported as failed, and the review does not pass then.
Add to the verdict JSON an \"agents\" array with exactly one entry per agent above and no other: {\"agent\": \"<name>\", \"status\": \"completed\" or \"failed\", \"verdict\": \"pass\" | \"revise\" | \"concern\", \"reasons\": [...], \"summary\": \"...\"}, and for a concern also \"recommendation\", \"confidence\" and \"reason_category\" as in the verdict.
";

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
    use crate::domain::{Provider, RunId, RunRecord, TaskRecord, TaskStatus};
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
        })
        .unwrap()
    }

    fn run() -> TaskRun {
        TaskRun::restore(RunRecord {
            id: RunId::new(RUN).unwrap(),
            task_id: TaskId::new(7),
            status: RunStatus::AwaitingIntegration,
            requested_provider: Provider::Claude,
            actual_provider: Provider::Claude,
            worker_mode: crate::domain::worker::WorkerMode::Headless,
            base_commit: CommitSha::try_from(SHA).unwrap(),
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
    /// [`snapshot_subagents`] reads, with nothing else.
    struct Committed {
        files: Vec<(&'static str, &'static str)>,
        changed: Vec<String>,
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
        fn merge_base(&self, _: &str, _: &str) -> Result<Option<CommitSha>> {
            Ok(None)
        }
        fn changed_paths(&self, from: &str, to: &str) -> Result<Vec<String>> {
            assert_eq!((from, to), ("base", "head"));
            Ok(self.changed.clone())
        }
        fn current_branch(&self, _: &Path) -> Result<Option<String>> {
            unreachable!()
        }
        fn head(&self, _: &Path) -> Result<CommitSha> {
            unreachable!()
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
        fn log_oneline(&self, _: &str, _: &str) -> Result<String> {
            unreachable!()
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

    /// The review's subagents as the landing branch's commit has them
    /// (ADR-t1453-1 decisions 3 and 4): those the range's paths select,
    /// with the committed definition and its digest; the snapshot the
    /// event records and the job reads, and what the prompt adds.
    #[test]
    fn the_committed_config_selects_the_agents_the_range_requires() {
        let found = snapshot(
            vec![
                (CONFIG_FILE, CONFIG),
                (".dagq/review-agents/design.md", DEFINITION),
            ],
            &["change.txt", "other.txt"],
        )
        .unwrap()
        .unwrap();
        let digest = format!("{:x}", Sha256::digest(DEFINITION.as_bytes()));
        let expected = json!({"commit": SHA, "base": "base", "head": "head", "agents": [{
            "agent": "design", "paths": ["change.txt"],
            "definition": ".dagq/review-agents/design.md", "digest": digest}]});
        assert_eq!(found.event_value(), expected);
        let mut input = expected;
        input["agents"][0]["text"] = json!(DEFINITION);
        assert_eq!(found.job_input(), input);
        let prompt = review_subagents_prompt(&found, Path::new("/runs/r/review-subagents-1.json"));
        for part in [
            "Required review subagents: ".to_owned(),
            format!(
                "committed on the landing branch at {SHA}, are in /runs/r/review-subagents-1.json"
            ),
            "- design (changed: change.txt)\n".to_owned(),
            SUBAGENTS_INSTRUCTION.to_owned(),
        ] {
            assert!(prompt.contains(&part), "{part} in {prompt}");
        }
    }

    /// Without `[review.subagents]` on the landing branch (no `dagq.toml`,
    /// or one without the table) the review is as before: no snapshot, so
    /// no `subagents` in `review_started`, no job input and no word of
    /// subagents in the prompt or the material. With the table but a range
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
            crate::application::prompt::review_prompt(&task(""), &run(), "/runs/r/review.md", None)
                .text;
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
                "the review subagent design that dagq.toml names has no definition .dagq/review-agents/design.md in the landing branch's commit {SHA}"
            )),
            "{error}"
        );
    }
}
