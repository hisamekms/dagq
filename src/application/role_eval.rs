//! The common entry for verdict-role evals. A harness owns reading its
//! production snapshot, its tree policy, prompt assembly, job and verdict.
use super::actor_executor::HeadlessProgram;
use super::role_snapshot::{Input, Snapshot};
use crate::domain::actor_model::{ActorLaunch, AgentModel, ModelRole, RoleModels};
use crate::domain::agent_eval::verdict;
use crate::domain::{ActorContext, Provider, TaskRun};
use anyhow::{Context, Result, bail, ensure};
use std::path::Path;

/// `agent eval role:plan_review`, `role:goal_review` and `role:review` (the
/// whole run review's judgment) select whole verdict roles, independently
/// of review agents named by their definition directories.
/// Its held-out definition digest is the versioned harness identity below;
/// changing replay or scoring semantics must change that identity.
#[derive(Clone, Copy, Debug)]
pub enum RoleHarness {
    PlanReview,
    GoalReview,
    RunReview,
}

/// The files a run review case's material directory may hold, as
/// [`Snapshot::capture`] names them.
const RUN_MATERIAL: [&str; 3] = ["review.md", "builtin-acceptance.md", "known-failures.json"];

fn full_commit(name: &str) -> bool {
    name.len() == 40 && name.bytes().all(|c| c.is_ascii_hexdigit())
}

impl RoleHarness {
    pub fn target(name: &str) -> Option<Self> {
        match name {
            "role:plan_review" => Some(Self::PlanReview),
            "role:goal_review" => Some(Self::GoalReview),
            "role:review" => Some(Self::RunReview),
            _ => None,
        }
    }
    pub fn role(self) -> ModelRole {
        match self {
            Self::PlanReview => ModelRole::PlanReview,
            Self::GoalReview => ModelRole::GoalReview,
            Self::RunReview => ModelRole::Review,
        }
    }
    pub fn identity(self) -> &'static str {
        match self {
            Self::PlanReview => "role:plan_review:snapshot-v1:score-v1",
            Self::GoalReview => "role:goal_review:snapshot-v1:score-v1",
            Self::RunReview => "role:review:snapshot-v1:score-v1",
        }
    }
    /// Validate the production record without consulting the source paths.
    /// Main and base commits are immutable full Git object names. Material
    /// paths must stay below the case's material directory and be unique.
    /// Whether a run review's change rebuilds is the case's run to find
    /// ([`Self::tree`]), not the shape's.
    pub fn snapshot(self, value: &serde_json::Value) -> Result<Snapshot> {
        let s: Snapshot = serde_json::from_value(value.clone())?;
        ensure!(full_commit(&s.main_commit), "invalid main commit");
        match (self, &s.input) {
            (Self::PlanReview, Input::Plan(_)) => (),
            (Self::GoalReview, Input::Goal(_)) => {
                ensure!(
                    s.materials.is_empty(),
                    "goal review has no external material"
                );
            }
            (Self::RunReview, Input::Run(r)) => {
                // The case's job directory is named after it
                // ([`Self::job_run`]): one plain component, never a path.
                let id = Path::new(r.run.id().as_str());
                ensure!(
                    id.components().count() == 1
                        && matches!(
                            id.components().next(),
                            Some(std::path::Component::Normal(_))
                        ),
                    "invalid run id"
                );
                let change = s.change.as_ref().context("run review has no change")?;
                ensure!(
                    full_commit(&change.base) && full_commit(&change.head),
                    "invalid change base or head"
                );
                let has = |name: &str| s.materials.iter().any(|m| m.at == Path::new(name));
                ensure!(
                    has(RUN_MATERIAL[0])
                        && has(RUN_MATERIAL[1]) == r.rules.is_some()
                        && has(RUN_MATERIAL[2]) == r.known_failures.is_some(),
                    "run review material does not match its inputs"
                );
            }
            _ => bail!("snapshot belongs to another role"),
        }
        ensure!(
            s.change.is_some() == matches!(self, Self::RunReview),
            "only the run review reads a change copy"
        );
        let mut paths = std::collections::BTreeSet::new();
        for m in &s.materials {
            ensure!(
                !m.at.as_os_str().is_empty()
                    && m.at
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_)))
                    && paths.insert(&m.at),
                "invalid or duplicate material path"
            );
            ensure!(
                match self {
                    Self::RunReview => RUN_MATERIAL.iter().any(|name| m.at == Path::new(name)),
                    _ => m.at.starts_with("replacement"),
                },
                "unexpected material"
            );
        }
        // Production assembly itself validates the proposal's inputs.
        if let Input::Plan(p) = &s.input {
            ensure!(
                p.replacement.as_ref().is_some_and(|(_, p)| p.is_some()) || s.materials.is_empty(),
                "material without a replacement"
            );
            p.build()?;
        }
        Ok(s)
    }
    /// A case made from a record keeps the tree identity production read,
    /// which only a handmade run review case may leave out.
    pub fn check_source(self, source: &str, snapshot: &Snapshot) -> Result<()> {
        ensure!(
            source == "handmade" || snapshot.change.as_ref().is_none_or(|c| c.tree.is_some()),
            "a {source} run review case has no recorded tree"
        );
        Ok(())
    }
    /// Read a verdict with exactly the production parser, retaining the
    /// printed object for event replay and scoring.
    pub fn reply(self, text: &str) -> Option<serde_json::Value> {
        match self {
            Self::PlanReview => {
                crate::domain::plan_review::PlanReviewVerdict::parse(text).ok()?;
            }
            Self::GoalReview => {
                crate::domain::goal_review::GoalReviewVerdict::parse(text).ok()?;
            }
            Self::RunReview => {
                crate::domain::ReviewVerdict::parse(text).ok()?;
            }
        }
        super::agent_job::read_reply(text)
    }
    pub fn score(
        self,
        cases: &[crate::domain::agent_eval::Case],
        runs: &[verdict::RoleRun<'_>],
        threshold: f64,
        disputed: bool,
    ) -> serde_json::Value {
        match self {
            Self::PlanReview => verdict::score(cases, runs, threshold, disputed),
            Self::GoalReview => verdict::score_goal(cases, runs, threshold, disputed),
            Self::RunReview => verdict::score_review(cases, runs, threshold, disputed),
        }
    }
    /// The production fitter's limit, retained in eval job measurements.
    pub fn prompt_limit(self) -> usize {
        match self {
            Self::PlanReview => super::prompt::PLAN_REVIEW_PROMPT_LIMIT,
            Self::GoalReview => super::prompt::GOAL_REVIEW_PROMPT_LIMIT,
            Self::RunReview => super::prompt::RUN_REVIEW_PROMPT_LIMIT,
        }
    }
    pub fn actor(self, eval: i64, index: usize, run: u32) -> ActorContext {
        match self {
            Self::PlanReview => {
                ActorContext::plan_review_job(format_args!("agent-eval:{eval}:{index}"), run)
            }
            Self::GoalReview => {
                ActorContext::goal_review_job(format_args!("agent-eval:{eval}:{index}"), run)
            }
            // The review job's role with no run, like an agent case's.
            Self::RunReview => ActorContext::agent_eval_job(eval, index, run),
        }
    }
    /// Use the production builder, fit before relocating references, and
    /// expose only saved material. No expectation enters this operation.
    pub fn prompt(
        self,
        snapshot: &Snapshot,
        files: &dyn super::RunFiles,
        material: &Path,
        tree: &Path,
    ) -> Result<String> {
        snapshot.for_case(files, material, tree)?.build()
    }
    /// The recorded run as the whole review's command takes it, its paths
    /// moved below `dir` (the case run's own directory, outside what the
    /// job may read): its provider writes the job's settings and logs
    /// there, never in the production run's directory. `None` for the roles
    /// that start no review program.
    pub fn job_run(self, snapshot: &Snapshot, dir: &Path) -> Option<TaskRun> {
        match &snapshot.input {
            Input::Run(r) if matches!(self, Self::RunReview) => Some(r.run.clone().relocated(dir)),
            _ => None,
        }
    }
    /// Select the same headless job intent and access as production. The
    /// shared executor applies the case read boundary and omits credentials.
    /// `run` is [`Self::job_run`]'s, which the run review needs.
    pub fn program<'a>(
        self,
        tree: &'a Path,
        prompt: &'a str,
        reads: &'a super::AgentReads,
        run: Option<&'a TaskRun>,
    ) -> Result<HeadlessProgram<'a>> {
        let access = match self {
            Self::PlanReview => super::prompt::PLAN_REVIEW_ACCESS,
            Self::GoalReview => super::prompt::GOAL_REVIEW_ACCESS,
            Self::RunReview => {
                return Ok(HeadlessProgram::Review {
                    run: run.context("a run review case's run")?,
                    tree,
                    prompt,
                    access: super::prompt::REVIEW_ACCESS,
                    reads: Some(reads),
                });
            }
        };
        Ok(HeadlessProgram::Job {
            cwd: tree,
            prompt,
            access,
            reads: Some(reads),
        })
    }
    /// The commit a case's tree starts from: recorded main, or the run
    /// review's change base.
    pub fn base(self, snapshot: &Snapshot) -> &str {
        match (&snapshot.change, self) {
            (Some(change), Self::RunReview) => &change.base,
            _ => &snapshot.main_commit,
        }
    }
    pub fn checked(
        self,
        requested: &AgentModel,
        models: &RoleModels,
        declared: &[Provider],
    ) -> std::result::Result<ActorLaunch, String> {
        let written = crate::domain::agent_eval::round::check_requested_role_launch(
            requested,
            models,
            self.role(),
            &AgentModel::default(),
            declared,
        )?;
        models.checked_agent_launch(self.role(), &written)
    }
    /// Whether the round runs the project's program reviews over a case
    /// before its job, as production runs them before the job (only the
    /// run review has that stage; ADR-t3948-1).
    pub fn runs_programs(self) -> bool {
        matches!(self, Self::RunReview)
    }
    /// Whether its job is one of a run's review stage, whose time limit
    /// `[review.jobs]` and the launched provider set as production's.
    pub fn review_stage(self) -> bool {
        matches!(self, Self::RunReview)
    }
    /// Verdict roles share the lifecycle but choose their own tree policy.
    /// Plan and goal review read recorded main, with every case store hidden.
    /// The run review reads the tree production read: the change copy at
    /// `patch` (outside the tree and material) applied to the recorded base
    /// in a Git of the case's own, never the run's branch, head or today's
    /// main, its full tree compared with the record's identity when it has
    /// one. A refusal is the error the case's run ends with, before any job.
    /// The commit returned is the case's head, the change's end the
    /// program reviews select by and run against.
    pub fn tree(
        self,
        (repository, files): (&dyn super::Repository, &dyn super::RunFiles),
        snapshot: &Snapshot,
        tree: &Path,
        patch: &Path,
    ) -> Result<String> {
        match self {
            Self::PlanReview | Self::GoalReview => {
                repository.add_case_base_tree(tree, &snapshot.main_commit, &verdict::HIDDEN)?;
                Ok(snapshot.main_commit.clone())
            }
            Self::RunReview => {
                let change = snapshot
                    .change
                    .as_ref()
                    .context("run review has no change")?;
                super::role_snapshot::rebuild(
                    repository,
                    files,
                    change,
                    patch,
                    tree,
                    &verdict::HIDDEN,
                )?
                .map(crate::domain::CommitSha::into_string)
                .map_err(anyhow::Error::msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::role_fixtures::{plan_snapshot as snapshot, run_snapshot};
    use serde_json::json;
    #[test]
    fn snapshots_build_the_production_prompt_and_reject_wrong_records() {
        let harness = RoleHarness::PlanReview;
        let s = snapshot(&"a".repeat(40));
        let value = serde_json::to_value(&s).unwrap();
        let read = harness.snapshot(&value).unwrap();
        let Input::Plan(input) = &read.input else {
            panic!("plan")
        };
        let files = crate::application::memory_files::MemoryFiles::default();
        let prompt = harness
            .prompt(
                &read,
                &files,
                Path::new("/case/material"),
                Path::new("/case/tree"),
            )
            .unwrap();
        assert_eq!(
            prompt,
            input
                .build()
                .unwrap()
                .text
                .replace("/production/repository", "/case/tree")
        );
        for bad in [
            json!(null),
            json!({"main_commit":"a"}),
            {
                let mut v = value.clone();
                v["main_commit"] = json!("main");
                v
            },
            {
                let mut v = value.clone();
                v["materials"] = json!([{"source":"/secret","at":"../outside","bytes":""}]);
                v
            },
            {
                let mut v = value;
                v["input"]["role"] = json!("goal");
                v
            },
        ] {
            assert!(harness.snapshot(&bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn goal_snapshot_replays_saved_values_and_rejects_other_roles_and_material() {
        use crate::application::role_snapshot::GoalInput;
        let harness = RoleHarness::target("role:goal_review").unwrap();
        let input = GoalInput {
            goal: json!({"id":42,"title":"goal","acceptance":"works"}),
            tasks: vec![json!({"id":1,"status":"completed","result":"saved result"})],
            follow_ups: vec![json!({"task_id":3,"judgement":"out_of_scope"})],
            replacements: vec![],
            events: vec![],
            previous: vec![],
            gaps_in_a_row: 3,
            repo_root: "/production/repository".into(),
            language: Some(crate::domain::language::Language {
                tag: "ja".into(),
                source: crate::domain::language::LanguageSource::User,
            }),
        };
        let production = input.build().text;
        let value = serde_json::to_value(Snapshot {
            main_commit: "a".repeat(40),
            change: None,
            materials: vec![],
            input: Input::Goal(Box::new(input)),
        })
        .unwrap();
        let read = harness.snapshot(&value).unwrap();
        let files = crate::application::memory_files::MemoryFiles::default();
        assert_eq!(
            harness
                .prompt(
                    &read,
                    &files,
                    Path::new("/case/material"),
                    Path::new("/case/tree")
                )
                .unwrap(),
            production.replace("/production/repository", "/case/tree")
        );
        assert!(RoleHarness::PlanReview.snapshot(&value).is_err());
        assert!(
            harness
                .snapshot(&serde_json::to_value(snapshot(&"a".repeat(40))).unwrap())
                .is_err()
        );
        for bad in [
            {
                let mut v = value.clone();
                v["main_commit"] = json!("main");
                v
            },
            {
                let mut v = value.clone();
                v["materials"] = json!([{"source":"/secret","at":"replacement/file","bytes":""}]);
                v
            },
            {
                let mut v = value;
                v["change"] = json!({});
                v
            },
        ] {
            assert!(harness.snapshot(&bad).is_err(), "{bad}");
        }
        let gaps = r#"{"verdict":"gaps","criteria":[{"criterion":"works","met":false}],"gaps":[{"title":"fix"}]}"#;
        assert_eq!(harness.reply(gaps).unwrap()["verdict"], "gaps");
        assert!(harness.reply(r#"{"verdict":"gaps"}"#).is_none());
        assert!(
            harness
                .reply(r#"{"verdict":"pass","reasons":[],"summary":"ok"}"#)
                .is_none()
        );
    }

    #[test]
    fn run_snapshots_replay_the_production_prompt_and_reject_other_shapes() {
        let harness = RoleHarness::target("role:review").unwrap();
        assert_eq!(harness.role(), ModelRole::Review);
        let s = run_snapshot(&"a".repeat(40), Some("tree"));
        let value = serde_json::to_value(&s).unwrap();
        let read = harness.snapshot(&value).unwrap();
        let Input::Run(input) = &read.input else {
            panic!("run")
        };
        // The production builder, with only the material paths moved.
        let files = crate::application::memory_files::MemoryFiles::default();
        let prompt = harness
            .prompt(
                &read,
                &files,
                Path::new("/case/material"),
                Path::new("/case/tree"),
            )
            .unwrap();
        assert_eq!(
            prompt,
            input
                .build()
                .text
                .replace(
                    "/production/run/review-1-builtin-acceptance.md",
                    "/case/material/builtin-acceptance.md"
                )
                .replace("/production/run/review.md", "/case/material/review.md")
        );
        assert!(!prompt.contains("/production/"));
        assert_eq!(
            crate::application::RunFiles::read(&files, Path::new("/case/material/review.md"))
                .unwrap(),
            b"the material\0\xff"
        );
        assert_eq!(harness.base(&read), "a".repeat(40));
        // Its tree starts from the change base; the other roles' from main.
        assert_eq!(
            RoleHarness::PlanReview.base(&snapshot(&"d".repeat(40))),
            "d".repeat(40)
        );
        // The job's run writes nothing in the production run's directory.
        let run = harness.job_run(&read, Path::new("/case/run-0")).unwrap();
        assert!(run.run_dir().unwrap().starts_with("/case/run-0/"));
        assert!(
            RoleHarness::PlanReview
                .job_run(&read, Path::new("/case"))
                .is_none()
        );
        let reads = crate::application::AgentReads {
            readable: vec![],
            unreadable: vec![],
        };
        assert!(matches!(
            harness
                .program(Path::new("/case/tree"), "p", &reads, Some(&run))
                .unwrap(),
            HeadlessProgram::Review {
                access: crate::application::prompt::REVIEW_ACCESS,
                reads: Some(_),
                ..
            }
        ));
        assert!(
            harness
                .program(Path::new("/case/tree"), "p", &reads, None)
                .is_err()
        );
        assert_eq!(
            harness.prompt_limit(),
            crate::application::prompt::RUN_REVIEW_PROMPT_LIMIT
        );
        // Only the run review has production's program stage and the time
        // limit of a run's review stage.
        assert!(harness.runs_programs() && harness.review_stage());
        for other in [RoleHarness::PlanReview, RoleHarness::GoalReview] {
            assert!(!other.runs_programs() && !other.review_stage());
        }
        // Only a handmade case may leave out the recorded tree.
        let handmade = run_snapshot(&"a".repeat(40), None);
        assert!(harness.check_source("handmade", &handmade).is_ok());
        assert!(harness.check_source("production", &handmade).is_err());
        assert!(harness.check_source("production", &read).is_ok());
        assert!(RoleHarness::PlanReview.snapshot(&value).is_err());
        assert!(
            harness
                .snapshot(&serde_json::to_value(snapshot(&"a".repeat(40))).unwrap())
                .is_err()
        );
        let edit = |f: &dyn Fn(&mut serde_json::Value)| {
            let mut v = value.clone();
            f(&mut v);
            v
        };
        for bad in [
            edit(&|v| v["change"] = json!(null)),
            edit(&|v| v["change"]["base"] = json!("main")),
            edit(&|v| v["change"]["head"] = json!("run-branch")),
            edit(&|v| v["materials"][0]["at"] = json!("other.md")),
            edit(&|v| v["materials"][1]["at"] = json!("../outside")),
            edit(&|v| v["materials"].as_array_mut().unwrap().truncate(1)),
            edit(&|v| v["materials"][1]["at"] = json!("known-failures.json")),
            // A run id that names a path would move the job's directory
            // out of the case's.
            edit(&|v| v["input"]["input"]["run"]["id"] = json!("../../outside")),
            edit(&|v| v["input"]["input"]["run"]["id"] = json!("/abs/dir")),
            edit(&|v| v["input"]["input"]["run"]["id"] = json!("a/b")),
            edit(&|v| v["input"]["input"]["run"]["id"] = json!("..")),
        ] {
            assert!(harness.snapshot(&bad).is_err(), "{bad}");
        }
        let revise = r#"{"verdict":"revise","reasons":[{"text":"fix","codes":["acceptance_unmet"]}],"summary":"fix"}"#;
        assert_eq!(harness.reply(revise).unwrap()["verdict"], "revise");
        // A plan verdict or a recorded one with its agents is not what the
        // whole review prints.
        assert!(harness.reply(r#"{"verdict":"pass"}"#).is_none());
        assert!(
            harness
                .reply(r#"{"verdict":"pass","reasons":[],"summary":"ok","agents":[]}"#)
                .is_none()
        );
    }

    #[test]
    fn role_launch_checks_only_its_role_and_declared_destinations() {
        use crate::domain::actor_model::RoleModel;
        for h in [
            RoleHarness::PlanReview,
            RoleHarness::GoalReview,
            RoleHarness::RunReview,
        ] {
            let mut roles = RoleModels::default();
            *roles.entry(h.role()) = RoleModel {
                provider: Some(Provider::Codex),
                ..Default::default()
            };
            let asked = AgentModel {
                provider: Some(Provider::Claude),
                model: Some("claude-haiku-5-5".into()),
                effort: Some("low".into()),
            };
            assert!(h.checked(&asked, &roles, &[]).is_err());
            let launch = h.checked(&asked, &roles, &[Provider::Claude]).unwrap();
            assert_eq!(
                (launch.provider, launch.model, launch.effort),
                (Provider::Claude, asked.model, asked.effort)
            );
            assert_eq!(
                h.checked(&AgentModel::default(), &roles, &[])
                    .unwrap()
                    .provider,
                Provider::Codex
            );
        }
    }
}
