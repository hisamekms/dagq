//! `review` (ADR-0016 decision 7): the review material of a run that awaits
//! integration or a session, written to `<run_dir>/review.md` for a
//! subagent or the headless review job to read.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::Path;

use super::EntryKind;
use super::{
    Repository, RunFiles, TaskStore, fenced, integrate::integrate_logs, or_none, path_text,
};
use crate::domain::local_checks::{self, LocalChecks, Reading};
use crate::domain::review_builtins::{self, BuiltinAgent, EffectiveBuiltin};
use crate::domain::review_subagents::{self, AgentRole, AgentTools, ReviewAgents};
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
/// hands the path to a subagent instead of reading it. The queue is only
/// read: no event, no state change; without such a run nothing is
/// written. The diff is Git's bytes as they are, so review.md holds
/// non-UTF-8 bytes when the run changes a file in another encoding. The
/// temporary files are removed on success and on failure. `range`, when
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
        .or(run.worktree_path().filter(|_| !run.in_container()))
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
    let checks = local_checks_section(files, &*repository, run_dir, &head);
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
        checks.as_deref(),
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

/// `git log --oneline` of `run`'s own commits up to its head
/// ([`super::run_git::head`]), from their [`range_base`] with the landing
/// branch's current commit: what a new session of the run is handed as
/// its work so far, without the commits of other tasks it took in.
pub fn run_commits(repository: &dyn Repository, run: &TaskRun) -> Result<String> {
    let head = super::run_git::head(repository, run)?;
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
    /// ([`review_subagents::ReviewSubagent::provider`]).
    pub provider: Option<Provider>,
    /// For a built-in agent, whose definition is the binary's
    /// (`definition_path` is its name then), the project's rules for it
    /// as the landing branch's commit has them, `None` without the file
    /// (ADR-t2259-1 decision 6); `None` for a project's agent.
    pub builtin: Option<BuiltinSnapshot>,
}

/// What a built-in agent's job is given beside the binary's definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinSnapshot {
    pub rules: Option<ProjectRules>,
}

/// A project's rules file for a built-in agent, read from the landing
/// branch's commit: its path, its text and the text's SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRules {
    pub path: String,
    pub text: String,
    pub digest: String,
}

impl SnapshotAgent {
    /// What a built-in agent's `review_job_started` records of which
    /// built-in judged (ADR-t2259-1 decision 9): its name, the build
    /// identifier of this dagq, the SHA-256 of the definition, and the
    /// rules file's path and SHA-256 (`null` without one); `None` for a
    /// project's agent, whose definition's digest `review_started` has.
    pub fn builtin_record(&self) -> Option<Value> {
        let builtin = self.builtin.as_ref()?;
        Some(json!({
            "builtin": self.name,
            "build": crate::VERSION,
            "digest": self.digest,
            "rules": builtin.rules.as_ref().map(|rules| json!({
                "path": rules.path,
                "digest": rules.digest,
            })),
        }))
    }
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
/// require (decision 3). A landing branch without `dagq.toml` writes no
/// agent, and the built-ins go by their defaults; a built-in's definition
/// is the binary's, and its project's rules are read from the same commit
/// (ADR-t2259-1 decisions 4 to 6). `Ok(None)` when no agent can be
/// selected, or when `dagq.toml` writes no agent's table and none is
/// selected: the review goes as before. `Err` when the file cannot be
/// read or parsed, a selected agent's definition is not in the tree, or
/// its tools' declaration is a mistake (ADR-t1728-2): the required checks
/// are not known, so the review must not pass.
/// `range` gives `<base>` and `<head>` for the landing branch's commit it
/// is given ([`review_range_at`]), the one the configuration is read
/// from, and is called only when some agent can be selected (an enabled
/// built-in counts): the landing branch is read once per attempt.
pub fn snapshot_subagents(
    repository: &dyn Repository,
    parse: &dyn Fn(&str) -> Result<ReviewAgents>,
    range: &dyn Fn(&CommitSha) -> Result<ReviewRange>,
) -> Result<Option<SubagentSnapshot>> {
    let main = repository.main_head()?;
    let commit = main.to_string();
    let written = landing_agents(repository, &commit, parse)?;
    let configured = written.selectable();
    if configured.is_empty() {
        return Ok(None);
    }
    let range = range(&main)?;
    // `<base>...<head>`: from their merge base, as review.md's diff.
    let from = repository
        .merge_base(&range.base, &range.head)?
        .map_or_else(|| range.base.clone(), CommitSha::into_string);
    let changed = repository.changed_paths(&from, &range.head)?;
    let selected = review_subagents::select(&configured, &changed);
    if selected.is_empty() && !written.written() {
        return Ok(None);
    }
    let mut agents = Vec::new();
    for selected in selected {
        let provider = configured
            .iter()
            .find(|agent| agent.name == selected.name)
            .and_then(|agent| agent.provider);
        if let Some(builtin) = BuiltinAgent::named(&selected.name).ok().flatten() {
            agents.push(builtin_snapshot(
                repository, &commit, builtin, selected, provider,
            )?);
            continue;
        }
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
        agents.push(SnapshotAgent {
            provider,
            digest: format!("{:x}", Sha256::digest(definition.as_bytes())),
            tools,
            name: selected.name,
            matched: selected.matched,
            definition_path,
            definition,
            builtin: None,
        });
    }
    Ok(Some(SubagentSnapshot {
        commit,
        range,
        agents,
    }))
}

/// The review's agents the landing branch's commit `commit` writes in its
/// `dagq.toml`, none without the file.
fn landing_agents(
    repository: &dyn Repository,
    commit: &str,
    parse: &dyn Fn(&str) -> Result<ReviewAgents>,
) -> Result<ReviewAgents> {
    let Some(text) = repository
        .file_in(commit, CONFIG_FILE)
        .with_context(|| format!("read {CONFIG_FILE} in the landing branch's commit {commit}"))?
    else {
        return Ok(ReviewAgents::default());
    };
    parse(&text)
        .with_context(|| format!("parse {CONFIG_FILE} in the landing branch's commit {commit}"))
}

/// The project's rules for `builtin` on the landing branch's commit
/// `commit`, `None` without the file.
fn project_rules(
    repository: &dyn Repository,
    commit: &str,
    builtin: BuiltinAgent,
) -> Result<Option<ProjectRules>> {
    rules_at(repository, commit, review_builtins::rules_path(builtin))
}

/// The project's rules for the whole review, `builtin:acceptance`, on the
/// landing branch's commit `commit` (ADR-t2259-1 decision 6), `None`
/// without the file: never a worktree's or the run branch's file, and no
/// instructions file in their place.
pub fn acceptance_rules(repository: &dyn Repository, commit: &str) -> Result<Option<ProjectRules>> {
    rules_at(repository, commit, review_builtins::acceptance_rules_path())
}

/// What the whole review's `review_job_started` records of which
/// `builtin:acceptance` judged (ADR-t2259-1 decision 9), in a built-in
/// agent's shape ([`SnapshotAgent::builtin_record`]): its name, the build
/// identifier of this dagq, the SHA-256 of its prompt's fixed sentences
/// ([`crate::application::prompt::acceptance_digest`]), and the rules
/// file's path and SHA-256 (`null` without one).
pub fn acceptance_record(rules: Option<&ProjectRules>) -> Value {
    json!({
        "builtin": review_builtins::ACCEPTANCE,
        "build": crate::VERSION,
        "digest": crate::application::prompt::acceptance_digest(),
        "rules": rules.map(|rules| json!({
            "path": rules.path,
            "digest": rules.digest,
        })),
    })
}

/// The file `path` on the landing branch's commit `commit` as a project's
/// rules, `None` without it.
fn rules_at(
    repository: &dyn Repository,
    commit: &str,
    path: String,
) -> Result<Option<ProjectRules>> {
    Ok(repository
        .file_in(commit, &path)
        .with_context(|| format!("read {path} in the landing branch's commit {commit}"))?
        .map(|text| ProjectRules {
            digest: format!("{:x}", Sha256::digest(text.as_bytes())),
            path,
            text,
        }))
}

/// The selected built-in agent `builtin`: the binary's definition, and the
/// project's rules for it from the landing branch's commit `commit`.
fn builtin_snapshot(
    repository: &dyn Repository,
    commit: &str,
    builtin: BuiltinAgent,
    selected: review_subagents::SelectedAgent,
    provider: Option<Provider>,
) -> Result<SnapshotAgent> {
    let definition = builtin.definition().to_owned();
    let tools = AgentTools::declared(AgentRole::Review, &definition).map_err(|why| {
        anyhow::anyhow!(
            "the built-in {}'s definition is a mistake: {why}",
            selected.name
        )
    })?;
    Ok(SnapshotAgent {
        provider,
        digest: format!("{:x}", Sha256::digest(definition.as_bytes())),
        tools,
        definition_path: selected.name.clone(),
        name: selected.name,
        matched: selected.matched,
        definition,
        builtin: Some(BuiltinSnapshot {
            rules: project_rules(repository, commit, builtin)?,
        }),
    })
}

/// `doctor`'s check of the agents the role sections of the landing
/// branch's `dagq.toml` name (ADR-t1728-1): `agents`, each project's agent
/// with its section and the definition read
/// ([`review_subagents::find_definition`], `null` without one);
/// `builtins`, the whole review `builtin:acceptance` first (always on, on
/// every change: `paths` and `provider` `null`, its provider is
/// `[roles.review]`'s) and then each built-in agent as `dagq.toml` makes
/// it over its defaults, each with its rules file
/// ([`EffectiveBuiltin::doctor_value`]); `without_rules`, the names of
/// the enabled built-ins whose rules file the landing branch's commit does
/// not have, which judge without the project's rules; and `errors`, an agent without a definition, named in more than one role's
/// section, or whose definition declares tools that are a mistake
/// (ADR-t1728-2). A landing branch without `dagq.toml` has the built-ins
/// by their defaults. `Err` when the file cannot be read or parsed (an
/// unknown built-in's name and a key of the wrong type among them,
/// ADR-t2259-1 decision 4). A review does not pass with either meanwhile
/// (ADR-t1453-1 decision 6).
pub fn check_agents(
    repository: &dyn Repository,
    parse: &dyn Fn(&str) -> Result<ReviewAgents>,
) -> Result<Value> {
    let commit = repository.main_head()?.into_string();
    let written = landing_agents(repository, &commit, parse)?;
    let named = review_subagents::named_for_review(&written.agents);
    let acceptance = acceptance_rules(repository, &commit)?;
    let mut without_rules = Vec::new();
    if acceptance.is_none() {
        without_rules.push(review_builtins::ACCEPTANCE.to_owned());
    }
    let mut builtins = vec![json!({
        "agent": review_builtins::ACCEPTANCE,
        "enabled": true,
        "enabled_by_default": true,
        "paths": null,
        "paths_written": false,
        "provider": null,
        "rules": acceptance.map(|rules| rules.path),
    })];
    for builtin in EffectiveBuiltin::all(&written.builtins) {
        let rules = project_rules(repository, &commit, builtin.agent)?;
        if builtin.enabled && rules.is_none() {
            without_rules.push(builtin.agent.name());
        }
        builtins.push(builtin.doctor_value(rules.as_ref().map(|rules| rules.path.as_str())));
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
    Ok(json!({
        "commit": commit,
        "agents": agents,
        "builtins": builtins,
        "without_rules": without_rules,
        "errors": errors,
    }))
}

/// The records of the worker's local checks in the records' directory of
/// `run_dir` ([`local_checks::dir_of`]) read against `tree`, the
/// receipt's commit's tree (ADR-t2589-1 decisions 3 and 4): each file
/// named `*.json` is a record, read in the order of the names, without
/// following a link and up to [`local_checks::RECORD_BYTES`], and those
/// past [`local_checks::MAX_RECORDS`] are broken unread. A directory that
/// is not one (a link, a file) or cannot be listed is one broken record.
/// Nothing is run or interpreted, and no log is read. Never fails: a
/// record changes no review's way, only what its material shows.
pub fn read_local_checks(files: &dyn RunFiles, run_dir: &Path, tree: &str) -> LocalChecks {
    let dir = local_checks::dir_of(run_dir);
    let mut checks = LocalChecks::default();
    match files.entry_kind(&dir) {
        Ok(EntryKind::Dir) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return checks,
        _ => {
            checks.add(Reading::Broken);
            return checks;
        }
    }
    let Ok(entries) = files.read_dir(&dir) else {
        checks.add(Reading::Broken);
        return checks;
    };
    let (records, over) = local_checks::records_to_read(entries);
    for _ in 0..over {
        checks.add(Reading::Broken);
    }
    for path in &records {
        let bytes = match files.entry_kind(path) {
            Ok(EntryKind::File) => files
                .read_range(path, 0, local_checks::RECORD_BYTES + 1)
                .ok()
                .filter(|bytes| bytes.len() as u64 <= local_checks::RECORD_BYTES),
            _ => None,
        };
        checks.add(local_checks::judge(bytes.as_deref(), tree));
    }
    checks
}

/// The section of review.md on the worker's local checks
/// ([`read_local_checks`] against the tree of `head`, the receipt's
/// commit; a run in a container's is the run branch taken in on the
/// control side): the matched records' items and how many were left out,
/// never a log's text (ADR-t2589-1 decision 5). `None` without any
/// record, so the material of a project whose scripts write none is as it
/// was, and `None` too when the commit's tree cannot be read: the records
/// never fail the review.
fn local_checks_section(
    files: &dyn RunFiles,
    repository: &dyn Repository,
    run_dir: &Path,
    head: &str,
) -> Option<String> {
    let dir = local_checks::dir_of(run_dir);
    if matches!(files.entry_kind(&dir), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        return None;
    }
    let tree = match repository.tree_of(head) {
        Ok(tree) => tree,
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "the tree of {head} could not be read for the local check records: {error:#}");
            return None;
        }
    };
    let checks = read_local_checks(files, run_dir, &tree);
    if checks.is_empty() {
        return None;
    }
    let matched = serde_json::to_string_pretty(&checks.matched_value()).unwrap_or_default();
    Some(format!(
        "\n## Local check records\n\n\
         What the project's scripts recorded of the worker's local checks in {dir}: \
         the records whose tree at the start and at the end is the tree {tree} of the receipt's commit {head}, \
         each with its command, exit status, start, duration in milliseconds, kind and log file in that directory. \
         The worker can write these records (advisory); they are not checked for tampering, and the landing's verification runs again regardless.\n\n\
         - matched: {matched_count}\n\
         - of another tree, or whose tree changed while it ran: {other}\n\
         - unreadable, malformed or over the limits: {broken}\n\n{body}",
        dir = dir.display(),
        matched_count = checks.matched.len(),
        other = checks.other_tree,
        broken = checks.broken,
        body = fenced("json", &matched),
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
    local_checks: Option<&str>,
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
    if let Some(section) = local_checks {
        out.push_str(section);
    }
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
        TaskRun::restore(record_on(base)).unwrap()
    }

    fn record_on(base: &str) -> RunRecord {
        RunRecord {
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
        }
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
            None,
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

    /// A committed `dagq.toml` that turns `builtin:security` off and
    /// `builtin:tests` on, on `lib/**` only and on Codex.
    const BUILTINS: &str = "the committed dagq.toml of the built-ins";

    /// `design` reviews `change.txt` and `docs/**`; `unused` matches
    /// nothing and has no definition. A config of `bad` does not parse;
    /// one without the table names no agent.
    fn parse(text: &str) -> Result<ReviewAgents> {
        let agent = |name: &str, paths: &[&str]| review_subagents::ReviewSubagent {
            name: name.to_owned(),
            provider: None,
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        };
        let setting =
            |agent, enabled, paths: Option<&[&str]>, provider| review_builtins::BuiltinSetting {
                agent,
                enabled: Some(enabled),
                paths: paths.map(|paths| paths.iter().map(|p| (*p).to_owned()).collect()),
                provider,
            };
        match text {
            CONFIG => Ok(ReviewAgents {
                agents: vec![
                    agent("design", &["change.txt", "docs/**"]),
                    agent("unused", &["nothing/**"]),
                ],
                builtins: Vec::new(),
            }),
            BUILTINS => Ok(ReviewAgents {
                agents: Vec::new(),
                builtins: vec![
                    setting(BuiltinAgent::Security, false, None, None),
                    setting(
                        BuiltinAgent::Tests,
                        true,
                        Some(&["lib/**"]),
                        Some(Provider::Codex),
                    ),
                ],
            }),
            "bad" => anyhow::bail!("dagq.toml:1: [review.subagents.design] has no paths"),
            _ => Ok(ReviewAgents::default()),
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

    /// `doctor`'s view of the whole review without its rules file.
    fn acceptance_builtin(rules: Option<&str>) -> Value {
        json!({"agent": "builtin:acceptance", "enabled": true, "enabled_by_default": true,
               "paths": null, "paths_written": false, "provider": null, "rules": rules})
    }

    /// `doctor`'s view of `builtins` by their defaults: the whole review,
    /// then security on and tests off, on their default globs, without
    /// rules files.
    fn default_builtins() -> Value {
        json!(
            std::iter::once(acceptance_builtin(None))
                .chain(
                    EffectiveBuiltin::all(&[])
                        .iter()
                        .map(|builtin| builtin.doctor_value(None))
                )
                .collect::<Vec<_>>()
        )
    }

    /// `doctor`'s check of the landing branch's agents (ADR-t1728-1): each
    /// named agent with the definition read, and an agent without one as
    /// an error; the built-ins by their defaults without `dagq.toml` or
    /// agents.
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
        let nothing = json!({"commit": SHA, "agents": [], "builtins": default_builtins(),
            "without_rules": ["builtin:acceptance", "builtin:security"], "errors": []});
        assert_eq!(check(Vec::new()).unwrap(), nothing);
        assert_eq!(check(vec![(CONFIG_FILE, "[run.env]")]).unwrap(), nothing);
        assert_eq!(nothing["builtins"][1]["enabled"], json!(true));
        assert_eq!(nothing["builtins"][2]["enabled"], json!(false));
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
            json!({"commit": SHA, "agents": [
                {"agent": "design", "section": "review.subagents", "definition": ".dagq/agents/design/AGENT.md"},
                {"agent": "unused", "section": "review.subagents", "definition": null},
            ], "builtins": default_builtins(),
            "without_rules": ["builtin:acceptance", "builtin:security"], "errors": [
                "[review.subagents.unused] names an agent without a definition: .dagq/agents/unused/AGENT.md is not committed",
            ]})
        );
        // The built-ins as dagq.toml writes them, with the rules files the
        // landing branch has: only an enabled built-in without one is
        // named as judging without the project's rules.
        let builtins = check(vec![
            (CONFIG_FILE, BUILTINS),
            (".dagq/review/builtin-tests.md", "Our tests live in t/."),
            (".dagq/review/builtin-acceptance.md", "Check the changelog."),
        ])
        .unwrap();
        assert_eq!(builtins["without_rules"], json!([]));
        let tests_without = check(vec![(CONFIG_FILE, BUILTINS)]).unwrap();
        assert_eq!(
            tests_without["without_rules"],
            json!(["builtin:acceptance", "builtin:tests"])
        );
        assert_eq!(
            builtins["builtins"],
            json!([
                acceptance_builtin(Some(".dagq/review/builtin-acceptance.md")),
                {"agent": "builtin:security", "enabled": false, "enabled_by_default": true,
                 "paths": BuiltinAgent::Security.default_paths(), "paths_written": false,
                 "provider": null, "rules": null},
                {"agent": "builtin:tests", "enabled": true, "enabled_by_default": false,
                 "paths": ["lib/**"], "paths_written": true, "provider": "codex",
                 "rules": ".dagq/review/builtin-tests.md"},
            ])
        );
        // A file elsewhere, as in the directory before ADR-t1728-1, is no
        // definition.
        let elsewhere = check(vec![
            (CONFIG_FILE, CONFIG),
            (concat!(".dagq/review", "-agents/design.md"), DEFINITION),
            (".dagq/agents/unused.md", DEFINITION),
        ])
        .unwrap();
        assert_eq!(elsewhere["agents"][0]["definition"], json!(null));
        assert_eq!(elsewhere["errors"].as_array().unwrap().len(), 2);
    }

    /// The whole review's rules are the landing branch's file only
    /// (ADR-t2259-1 decision 6), and its record names builtin:acceptance,
    /// this build, its fixed sentences' digest and the rules' digest, or
    /// none without the file (decision 9).
    #[test]
    fn the_whole_reviews_rules_and_record_come_from_the_landing_branch() {
        const RULES: &str = "Check the changelog.\n";
        let repository = |files| Committed {
            files,
            ..Committed::default()
        };
        let without = acceptance_rules(
            &repository(vec![("AGENTS.md", "Read me."), ("CLAUDE.md", "Me too.")]),
            SHA,
        )
        .unwrap();
        assert_eq!(without, None);
        let digest = crate::application::prompt::acceptance_digest();
        assert_eq!(
            acceptance_record(None),
            json!({"builtin": "builtin:acceptance", "build": crate::VERSION,
                   "digest": digest, "rules": null})
        );
        let with = acceptance_rules(
            &repository(vec![(".dagq/review/builtin-acceptance.md", RULES)]),
            SHA,
        )
        .unwrap()
        .unwrap();
        assert_eq!(with.text, RULES);
        let rules_digest = format!("{:x}", Sha256::digest(RULES.as_bytes()));
        assert_eq!(
            acceptance_record(Some(&with)),
            json!({"builtin": "builtin:acceptance", "build": crate::VERSION, "digest": digest,
                   "rules": {"path": ".dagq/review/builtin-acceptance.md", "digest": rules_digest}})
        );
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
        let prompt = crate::application::prompt::review_prompt(
            &task(""),
            &run(),
            "/runs/r/review.md",
            None,
            None,
        )
        .text;
        assert!(!prompt.contains("subagent"), "{prompt}");
        let material = material(&task(""));
        assert!(!material.contains("subagent"), "{material}");
    }

    /// Without `dagq.toml`, a change of code selects `builtin:security` on
    /// its default globs, with the binary's definition and no rules file;
    /// the landing branch's rules file is handed on with its digest, and
    /// the job's record names the built-in, this build and both digests
    /// (ADR-t2259-1 decisions 5, 6 and 9).
    #[test]
    fn a_change_of_code_selects_the_builtin_security_by_default() {
        let found = snapshot(Vec::new(), &["app/main.py", "README.txt"])
            .unwrap()
            .unwrap();
        assert_eq!(found.agents.len(), 1, "{found:?}");
        let security = &found.agents[0];
        assert_eq!(security.name, "builtin:security");
        assert_eq!(security.matched, ["app/main.py"]);
        assert_eq!(security.definition, BuiltinAgent::Security.definition());
        assert_eq!(security.definition_path, "builtin:security");
        assert_eq!(security.builtin, Some(BuiltinSnapshot { rules: None }));
        let digest = format!(
            "{:x}",
            Sha256::digest(BuiltinAgent::Security.definition().as_bytes())
        );
        assert_eq!(
            security.builtin_record(),
            Some(
                json!({"builtin": "builtin:security", "build": crate::VERSION,
                        "digest": digest, "rules": null})
            )
        );
        const RULES: &str = "Payments go through pay/ only.";
        let ruled = snapshot(
            vec![(".dagq/review/builtin-security.md", RULES)],
            &["app/main.py"],
        )
        .unwrap()
        .unwrap();
        let rules_digest = format!("{:x}", Sha256::digest(RULES.as_bytes()));
        assert_eq!(
            ruled.agents[0].builtin_record().unwrap()["rules"],
            json!({"path": ".dagq/review/builtin-security.md", "digest": rules_digest})
        );
        assert_eq!(
            ruled.agents[0]
                .builtin
                .as_ref()
                .unwrap()
                .rules
                .as_ref()
                .unwrap()
                .text,
            RULES
        );
        // A project's agent carries no such record.
        let project = snapshot(
            vec![
                (CONFIG_FILE, CONFIG),
                (".dagq/agents/design/AGENT.md", DEFINITION),
            ],
            &["change.txt"],
        )
        .unwrap()
        .unwrap();
        assert_eq!(project.agents[0].builtin_record(), None);
        // A change of no code selects nothing, as before.
        assert!(snapshot(Vec::new(), &["README.txt"]).unwrap().is_none());
    }

    /// `paths` written replaces a built-in's default globs, not adds to
    /// them, `enabled` turns each on or off, and its `provider` goes with
    /// the agent its job starts as (ADR-t2259-1 decisions 4 and 5).
    #[test]
    fn a_builtins_written_paths_replace_its_defaults_and_its_provider_is_kept() {
        let off = snapshot(vec![(CONFIG_FILE, BUILTINS)], &["app/main.py"])
            .unwrap()
            .unwrap();
        assert!(off.agents.is_empty(), "{off:?}");
        let tests = snapshot(vec![(CONFIG_FILE, BUILTINS)], &["lib/a.txt", "app/main.py"])
            .unwrap()
            .unwrap();
        assert_eq!(
            tests
                .agents
                .iter()
                .map(|agent| (agent.name.as_str(), agent.matched.clone(), agent.provider))
                .collect::<Vec<_>>(),
            [(
                "builtin:tests",
                vec!["lib/a.txt".to_owned()],
                Some(Provider::Codex)
            )]
        );
        assert_eq!(tests.agents[0].definition, BuiltinAgent::Tests.definition());
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
            run_commits(
                &repository,
                &TaskRun::restore(RunRecord {
                    worktree_path: Some("/worktree".into()),
                    ..record_on(CLAIMED)
                })
                .unwrap()
            )
            .unwrap(),
            format!("{MIDWAY}..{HEAD}")
        );
    }
}
