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

/// The Markdown before the diff: the task, its goal, the receipt, the
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
