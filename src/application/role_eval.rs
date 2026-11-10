//! The common entry for verdict-role evals. A harness owns reading its
//! production snapshot, its tree policy, prompt assembly, job and verdict.
use super::actor_executor::HeadlessProgram;
use super::role_snapshot::{Input, Snapshot};
use crate::domain::actor_model::{ActorLaunch, AgentModel, ModelRole, RoleModels};
use crate::domain::agent_eval::verdict;
use crate::domain::{ActorContext, Provider};
use anyhow::{Result, bail, ensure};
use std::path::Path;

/// `agent eval role:plan_review` selects the whole plan review role,
/// independently of review agents named by their definition directories.
/// Its held-out definition digest is the versioned harness identity below;
/// changing replay or scoring semantics must change that identity.
#[derive(Clone, Copy, Debug)]
pub enum RoleHarness {
    PlanReview,
}
impl RoleHarness {
    pub fn target(name: &str) -> Option<Self> {
        match name {
            "role:plan_review" => Some(Self::PlanReview),
            _ => None,
        }
    }
    pub fn role(self) -> ModelRole {
        match self {
            Self::PlanReview => ModelRole::PlanReview,
        }
    }
    pub fn identity(self) -> &'static str {
        match self {
            Self::PlanReview => "role:plan_review:snapshot-v1:score-v1",
        }
    }
    /// Validate the production record without consulting the source paths.
    /// Main commits are immutable full Git object names. Material paths
    /// must stay below the case's material directory and be unique.
    pub fn snapshot(self, value: &serde_json::Value) -> Result<Snapshot> {
        let s: Snapshot = serde_json::from_value(value.clone())?;
        ensure!(
            s.main_commit.len() == 40 && s.main_commit.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid main commit"
        );
        match (self, &s.input) {
            (Self::PlanReview, Input::Plan(_)) => (),
            _ => bail!("snapshot belongs to another role"),
        }
        ensure!(s.change.is_none(), "plan review has no change copy");
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
            ensure!(m.at.starts_with("replacement"), "unexpected plan material");
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
    /// Read a verdict with exactly the production parser, retaining the
    /// printed object for event replay and scoring.
    pub fn reply(self, text: &str) -> Option<serde_json::Value> {
        match self {
            Self::PlanReview => {
                crate::domain::plan_review::PlanReviewVerdict::parse(text).ok()?;
            }
        }
        super::agent_job::read_reply(text)
    }
    pub fn score(
        self,
        cases: &[crate::domain::agent_eval::Case],
        runs: &[(&str, Option<&serde_json::Value>)],
        threshold: f64,
        disputed: bool,
    ) -> serde_json::Value {
        match self {
            Self::PlanReview => verdict::score(cases, runs, threshold, disputed),
        }
    }
    pub fn actor(self, eval: i64, index: usize, run: u32) -> ActorContext {
        match self {
            Self::PlanReview => {
                ActorContext::plan_review_job(format_args!("agent-eval:{eval}:{index}"), run)
            }
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
    /// Select the same headless job intent and access as production. The
    /// shared executor applies the case read boundary and omits credentials.
    pub fn program<'a>(
        self,
        tree: &'a Path,
        prompt: &'a str,
        reads: &'a super::AgentReads,
    ) -> HeadlessProgram<'a> {
        match self {
            Self::PlanReview => HeadlessProgram::Job {
                cwd: tree,
                prompt,
                access: super::prompt::PLAN_REVIEW_ACCESS,
                reads: Some(reads),
            },
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
    /// Verdict roles share the lifecycle but choose their own tree policy.
    /// Plan review reads the recorded main tree, with every case store hidden.
    pub fn tree(
        self,
        repository: &dyn super::Repository,
        snapshot: &Snapshot,
        tree: &Path,
    ) -> Result<()> {
        match self {
            Self::PlanReview => {
                repository.add_case_base_tree(tree, &snapshot.main_commit, &verdict::HIDDEN)
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::application::role_snapshot::{PlanInput, Whole};
    use serde_json::json;
    pub(crate) fn snapshot(main: &str) -> Snapshot {
        Snapshot {
            main_commit: main.into(), change: None, materials: vec![],
            input: Input::Plan(Box::new(PlanInput {
                proposal: serde_json::from_value(json!({
                    "id":1,"status":"submitted","owner":{"origin":"person","workspace_id":null},
                    "submitted_at":"now","revise_count":0,"task_ids":[],"goal_ids":[],"created_at":"","updated_at":""
                })).unwrap(),
                tasks: Whole(vec![]), goals: Whole(vec![]), lint: vec![], others: vec![], queued: vec![],
                queued_left_out: 0, expected: Default::default(), precedents: vec![], hotspots: vec![], candidates: vec![],
                repo_root: "/production/repository".into(), language: None, replacement: None,
            })),
        }
    }
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
    fn role_launch_checks_only_its_role_and_declared_destinations() {
        use crate::domain::actor_model::RoleModel;
        let h = RoleHarness::PlanReview;
        let mut roles = RoleModels::default();
        *roles.entry(ModelRole::PlanReview) = RoleModel {
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
