//! Fixed inputs of verdict roles, independent of the queue and the lifetime
//! of the files production prompts name. Missing records are not cases and
//! must never be reconstructed from later events.

use super::prompt::*;
use super::review::PersonAnswers;
use super::supervisor_service::Wire;
use super::{EntryKind, Repository, RunFiles, TaskListItem};
use crate::domain::language::Language;
use crate::domain::replan_plan::ReplanBrief;
use crate::domain::stats::conflicts::ConflictHotspot;
use crate::domain::{Ask, Goal, LintViolation, Proposal, Task, TaskDetail, TaskId, TaskRun};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A whole value, using the existing lossless service representation rather
/// than the CLI view (which omits task priority provenance, for example).
#[derive(Clone)]
pub struct Whole<T>(pub T);
impl<T: Wire + Clone> Serialize for Whole<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.clone().into_form().serialize(s)
    }
}
impl<'de, T: Wire> Deserialize<'de> for Whole<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        T::from_form(T::Form::deserialize(d)?)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// Owned plan assembly inputs. The lossless wrappers avoid changing domain
/// deserialization or the public CLI's serialization to support snapshots.
#[derive(Clone, Serialize, Deserialize)]
pub struct PlanInput {
    pub proposal: Proposal,
    pub tasks: Whole<Vec<TaskDetail>>,
    pub goals: Whole<Vec<Goal>>,
    pub lint: Vec<LintViolation>,
    pub others: Vec<(Proposal, Whole<Vec<Task>>)>,
    pub queued: Vec<TaskListItem>,
    pub queued_left_out: usize,
    pub expected: BTreeMap<TaskId, Vec<String>>,
    pub precedents: Vec<Ask>,
    pub hotspots: Vec<ConflictHotspot>,
    pub candidates: Vec<DuplicateCandidates>,
    pub repo_root: PathBuf,
    pub language: Option<Language>,
    pub replacement: Option<(ReplanBrief, Option<PathBuf>)>,
}
impl PlanInput {
    pub fn capture(m: &PlanReviewMaterial<'_>) -> Self {
        Self {
            proposal: m.proposal.clone(),
            tasks: Whole(m.tasks.to_vec()),
            goals: Whole(m.goals.to_vec()),
            lint: m.lint.to_vec(),
            others: m
                .others
                .iter()
                .map(|(p, t)| (p.clone(), Whole(t.clone())))
                .collect(),
            queued: m.queued.to_vec(),
            queued_left_out: m.queued_left_out,
            expected: m.expected.clone(),
            precedents: m.precedents.to_vec(),
            hotspots: m.hotspots.to_vec(),
            candidates: m.candidates.to_vec(),
            repo_root: m.repo_root.to_owned(),
            language: m.language.cloned(),
            replacement: m
                .replacement
                .map(|(b, p)| (b.clone(), p.map(Path::to_owned))),
        }
    }
    pub fn build(&self) -> Result<PlanReviewPrompt> {
        let others = self
            .others
            .iter()
            .map(|(p, t)| (p.clone(), t.0.clone()))
            .collect::<Vec<_>>();
        plan_review_prompt(&PlanReviewMaterial {
            proposal: &self.proposal,
            tasks: &self.tasks.0,
            goals: &self.goals.0,
            lint: &self.lint,
            others: &others,
            queued: &self.queued,
            queued_left_out: self.queued_left_out,
            expected: &self.expected,
            precedents: &self.precedents,
            hotspots: &self.hotspots,
            candidates: &self.candidates,
            repo_root: &self.repo_root,
            language: self.language.as_ref(),
            replacement: self.replacement.as_ref().map(|(b, p)| (b, p.as_deref())),
        })
    }
}

/// Goal review's material is already values; its language is an assembly
/// input too. It has no external material file today.
#[derive(Clone, Serialize, Deserialize)]
pub struct GoalInput {
    pub goal: serde_json::Value,
    pub tasks: Vec<serde_json::Value>,
    pub replacements: Vec<serde_json::Value>,
    pub follow_ups: Vec<serde_json::Value>,
    pub events: Vec<serde_json::Value>,
    pub previous: Vec<serde_json::Value>,
    pub gaps_in_a_row: usize,
    pub repo_root: PathBuf,
    pub language: Option<Language>,
}
impl GoalInput {
    pub fn capture(m: GoalReviewMaterial<'_>, language: Option<Language>) -> Self {
        Self {
            goal: m.goal,
            tasks: m.tasks,
            replacements: m.replacements,
            follow_ups: m.follow_ups,
            events: m.events,
            previous: m.previous,
            gaps_in_a_row: m.gaps_in_a_row,
            repo_root: m.repo_root.to_owned(),
            language,
        }
    }
    pub fn build(&self) -> FittedPrompt {
        goal_review_prompt(&GoalReviewMaterial {
            goal: self.goal.clone(),
            tasks: self.tasks.clone(),
            replacements: self.replacements.clone(),
            follow_ups: self.follow_ups.clone(),
            events: self.events.clone(),
            previous: self.previous.clone(),
            gaps_in_a_row: self.gaps_in_a_row,
            repo_root: &self.repo_root,
        })
        .with_language(self.language.as_ref())
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RulesInput {
    pub path: String,
    pub commit: String,
    pub text: String,
    pub whole_at: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct RunInput {
    pub task: Whole<Task>,
    pub run: TaskRun,
    pub review_path: String,
    pub known_failures: Option<KnownFailures>,
    pub rules: Option<RulesInput>,
    pub answers: PersonAnswers,
    pub language: Option<Language>,
}
impl RunInput {
    pub fn build(&self) -> FittedPrompt {
        review_prompt(
            &self.task.0,
            &self.run,
            &self.review_path,
            self.known_failures.as_ref(),
            self.rules.as_ref().map(|r| AcceptanceRules {
                path: &r.path,
                commit: &r.commit,
                text: &r.text,
                whole_at: &r.whole_at,
            }),
            &self.answers,
        )
        .with_language(self.language.as_ref())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "role", content = "input", rename_all = "snake_case")]
pub enum Input {
    Plan(Box<PlanInput>),
    Goal(Box<GoalInput>),
    Run(Box<RunInput>),
}

/// One file's exact bytes, not UTF-8 text: review.md may contain non-UTF-8
/// diffs. JSON encodes these bytes as base64. `at` is relative to a case's
/// material directory; `source` is only provenance and is never read on replay.
#[derive(Clone, Serialize, Deserialize)]
pub struct Material {
    pub source: PathBuf,
    pub at: PathBuf,
    #[serde(with = "base64_bytes")]
    pub bytes: Vec<u8>,
}

mod base64_bytes {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        STANDARD
            .decode(String::deserialize(d)?)
            .map_err(serde::de::Error::custom)
    }
}

/// A case assembles with the original paths before relocating the rendered
/// references. Path length affects the production fitter's cut points:
/// fitting with relocated paths would silently change the material's text.
/// Only this builder is used for the case's prompt; the source values stay
/// intact and remain round-trippable without the original files.
#[derive(Clone, Serialize, Deserialize)]
pub struct CaseInput {
    pub source: Input,
    pub paths: Vec<(String, String)>,
}
impl CaseInput {
    pub fn build(&self) -> Result<String> {
        let text = match &self.source {
            Input::Plan(p) => p.build()?.text,
            Input::Goal(g) => g.build().text,
            Input::Run(r) => r.build().text,
        };
        // Replace in one pass, longest source path first. A replacement
        // may itself contain another original path, and must not be edited.
        let mut paths = self
            .paths
            .iter()
            .filter(|(from, _)| !from.is_empty())
            .collect::<Vec<_>>();
        paths.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        let mut out = String::new();
        let mut rest = text.as_str();
        while !rest.is_empty() {
            let found = paths
                .iter()
                .filter_map(|(from, to)| rest.find(from.as_str()).map(|at| (at, from, to)))
                .min_by_key(|(at, _, _)| *at);
            let Some((at, from, to)) = found else {
                out.push_str(rest);
                break;
            };
            out.push_str(&rest[..at]);
            out.push_str(to);
            rest = &rest[at + from.len()..];
        }
        Ok(out)
    }
}

/// The whole role's launch snapshot: `input-snapshot.json` in a plan/goal
/// job directory, `review-{attempt}-snapshot.json` in a run directory.
/// JSON holds `main_commit`, tagged assembly `input`, exact material bytes,
/// and (for run review) `change`. The binary patch is a sibling
/// `review-{attempt}-change.patch`, outside the job's material.
/// The whole review.md is fixed here; its section decomposition for review
/// agents belongs to task 4116. These records never read or overwrite that
/// task's files. A failed write leaves no usable snapshot; production still
/// launches. Retention follows the owning job/run directory.
#[derive(Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub main_commit: String,
    pub input: Input,
    pub materials: Vec<Material>,
    pub change: Option<ReviewChange>,
}

/// The material's range and the commit/tree actually read by the job.
/// A different `tree_commit` from `head` is retained as evidence but cannot
/// become a case. `patch` is relative to the snapshot's directory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewChange {
    pub base: String,
    pub head: String,
    pub tree_commit: String,
    pub tree: String,
    pub patch: PathBuf,
}
impl Snapshot {
    pub fn capture(
        files: &dyn RunFiles,
        main_commit: String,
        input: Input,
        change: Option<ReviewChange>,
    ) -> Result<Self> {
        let mut snapshot = Self {
            main_commit,
            input,
            materials: Vec::new(),
            change,
        };
        let sources = match &snapshot.input {
            Input::Plan(p) => p
                .replacement
                .as_ref()
                .and_then(|(_, p)| p.as_ref())
                .map(|p| vec![(p.clone(), PathBuf::from("replacement"))])
                .unwrap_or_default(),
            Input::Goal(_) => Vec::new(),
            Input::Run(r) => {
                let mut paths = vec![(PathBuf::from(&r.review_path), PathBuf::from("review.md"))];
                if let Some(r) = &r.rules {
                    paths.push((
                        PathBuf::from(&r.whole_at),
                        PathBuf::from("builtin-acceptance.md"),
                    ));
                }
                if let Some(k) = &r.known_failures {
                    paths.push((PathBuf::from(&k.file), PathBuf::from("known-failures.json")));
                }
                paths
            }
        };
        for (source, at) in sources {
            snapshot.copy(files, &source, &at)?;
        }
        Ok(snapshot)
    }
    fn copy(&mut self, files: &dyn RunFiles, source: &Path, at: &Path) -> Result<()> {
        match files.entry_kind(source)? {
            EntryKind::Dir => {
                for child in files.read_dir(source)? {
                    self.copy(
                        files,
                        &child,
                        &at.join(child.file_name().context("material has no filename")?),
                    )?;
                }
            }
            EntryKind::File => self.materials.push(Material {
                source: source.to_owned(),
                at: at.to_owned(),
                bytes: files.read(source)?,
            }),
            _ => bail!(
                "material {} is not a regular file or directory",
                source.display()
            ),
        }
        Ok(())
    }
    /// Write only saved bytes for a case and return its prompt assembly.
    /// No source file is consulted. Repository and material references are
    /// relocated after fitting so that every other byte stays production's.
    pub fn for_case(
        &self,
        files: &dyn RunFiles,
        material: &Path,
        tree: &Path,
    ) -> Result<CaseInput> {
        files.create_dir_all(material)?;
        if matches!(&self.input, Input::Plan(p) if p.replacement.as_ref().is_some_and(|(_,p)| p.is_some()))
        {
            files.create_dir_all(&material.join("replacement"))?;
        }
        for saved in &self.materials {
            anyhow::ensure!(
                !saved.at.is_absolute()
                    && saved
                        .at
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                "invalid saved material path"
            );
            let path = material.join(&saved.at);
            files.create_dir_all(path.parent().context("material has no parent")?)?;
            files.write(&path, &saved.bytes)?;
        }
        let mut paths = Vec::new();
        match &self.input {
            Input::Plan(p) => {
                paths.push((
                    p.repo_root.display().to_string(),
                    tree.display().to_string(),
                ));
                if let Some((_, Some(path))) = &p.replacement {
                    paths.push((
                        path.display().to_string(),
                        material.join("replacement").display().to_string(),
                    ));
                }
            }
            Input::Goal(g) => paths.push((
                g.repo_root.display().to_string(),
                tree.display().to_string(),
            )),
            Input::Run(r) => {
                paths.push((
                    r.review_path.clone(),
                    material.join("review.md").display().to_string(),
                ));
                if let Some(r) = &r.rules {
                    paths.push((
                        r.whole_at.clone(),
                        material.join("builtin-acceptance.md").display().to_string(),
                    ));
                }
                if let Some(k) = &r.known_failures {
                    paths.push((
                        k.file.clone(),
                        material.join("known-failures.json").display().to_string(),
                    ));
                }
            }
        }
        Ok(CaseInput {
            source: self.input.clone(),
            paths,
        })
    }
}

/// Best effort publication: read/copy failures and partial writes never
/// change production's launch result. Publish JSON last, by rename.
pub fn record(files: &dyn RunFiles, path: &Path, capture: impl FnOnce() -> Result<Snapshot>) {
    let temporary = path.with_extension("json.tmp");
    let result = (|| {
        let snapshot = capture()?;
        files.write(&temporary, &serde_json::to_vec_pretty(&snapshot)?)?;
        files.rename(&temporary, path)?;
        Ok::<_, anyhow::Error>(())
    })();
    if let Err(error) = result {
        let _ = files.remove_file(&temporary);
        let _ = files.remove_file(path);
        tracing::warn!(path = %path.display(), "could not record verdict role inputs: {error:#}");
    }
}

/// Refusals are values, separate from Git and file reads. Compare the full
/// tree before hiding the case lists/expectations (ADR-t3948-1 decision 2b).
pub fn refusal(
    change: &ReviewChange,
    reachable: bool,
    patch: Option<&[u8]>,
    rebuilt: Option<&str>,
) -> Option<String> {
    if change.tree_commit != change.head {
        return Some("job tree commit differs from range head".into());
    }
    if !reachable {
        return Some("base is not reachable from a ref".into());
    }
    if patch.is_none_or(|p| p.is_empty()) {
        return Some("change copy is missing or empty".into());
    }
    if rebuilt.is_some_and(|tree| tree != change.tree) {
        return Some("rebuilt tree differs from recorded tree".into());
    }
    None
}

/// Rebuild using only the reachable base and fixed binary patch: never read
/// the run's branch or head. The returned commit identifies the full tree in
/// the source repository; `path` has its own Git with `hidden` removed from
/// every commit, as `add_case_tree` promises. A refusal removes the case.
pub fn rebuild(
    repository: &dyn Repository,
    files: &dyn RunFiles,
    change: &ReviewChange,
    patch: &Path,
    path: &Path,
    hidden: &[&str],
) -> Result<std::result::Result<crate::domain::CommitSha, String>> {
    let bytes = files.read(patch).ok();
    let reachable = repository.reachable_commit(&change.base)?;
    if let Some(why) = refusal(change, reachable, bytes.as_deref(), None) {
        return Ok(Err(why));
    }
    let commit = repository.add_case_tree(path, &change.base, patch, hidden)?;
    let tree = repository.tree_id(commit.as_str())?;
    if let Some(why) = refusal(change, true, bytes.as_deref(), Some(&tree)) {
        repository.remove_case_tree(path)?;
        return Ok(Err(why));
    }
    Ok(Ok(commit))
}
