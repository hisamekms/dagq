//! GitHub's reads of the repository's rulesets for the ruleset gate
//! (ADR-t4332-1, [`crate::domain::ruleset_gate`]). The list
//! (`GET /repos/{owner}/{repo}/rulesets`, `includes_parents` left at its
//! default `true`, so Organization rulesets that apply are listed too) is
//! paged, and each ruleset is read by id.
//!
//! The token is the runner App's installation token, issued for this read
//! alone with one repository and `metadata: read` only, which is all both
//! reads need. It is never kept past the read, never handed to a runner,
//! and Administration is never asked for: a token that could write rulesets
//! would let a leaked key undo the prohibitions the gate checks. The main
//! App's key is never read here.
//!
//! Every failure is `Unreadable`, except GitHub's refusal of rulesets for
//! the repository's plan; both fail closed.

use super::deployment_config::DeploymentConfig;
use super::github_app::{GithubApi, HttpsGithub, RunnerApp, valid_base, valid_repository};
use crate::application::queue_service::{RulesetGate, RulesetSource};
use crate::application::supervise::repository_config::{
    AppToken, ConfigUnavailable, InstallationTokens,
};
use crate::domain::ruleset_gate::{Apps, Read, Ruleset};
use serde_json::Value;
use std::sync::Arc;

/// Rulesets per page, GitHub's maximum.
const PER_PAGE: usize = 100;
/// Pages read before giving up as unreadable.
const MAX_PAGES: usize = 10;
/// Seconds one whole read may take, the requests' own timeouts aside: past
/// it the read is unreadable. The read runs off the write line; this only
/// bounds how long a result can lag.
const READ_DEADLINE_SECS: i64 = 120;

/// The issuance scope of the read's token: one repository, metadata read.
pub fn metadata_scope(repository_name: &str) -> Value {
    serde_json::json!({"repositories": [repository_name], "permissions": {"metadata": "read"}})
}

pub struct GithubRulesets {
    tokens: Box<dyn InstallationTokens>,
    api: Arc<dyn GithubApi>,
    clock: Arc<dyn crate::application::Clock>,
    apps: Apps,
    base: String,
    repository: String,
}

impl GithubRulesets {
    pub fn new(
        tokens: Box<dyn InstallationTokens>,
        api: Arc<dyn GithubApi>,
        clock: Arc<dyn crate::application::Clock>,
        apps: Apps,
        base: &str,
        repository: &str,
    ) -> Result<Self, ConfigUnavailable> {
        if !valid_base(base) || !valid_repository(repository) || apps.runner == 0 {
            return Err(ConfigUnavailable);
        }
        Ok(Self {
            tokens,
            api,
            clock,
            apps,
            base: base.trim_end_matches('/').into(),
            repository: repository.into(),
        })
    }

    fn get(&self, path: &str, token: &AppToken, started: i64) -> Result<Value, Read> {
        let now = self.clock.now();
        if now < started || now.saturating_sub(started) > READ_DEADLINE_SECS {
            return Err(Read::Unreadable);
        }
        let (status, body) = self
            .api
            .get_status(
                &format!("{}/repos/{}{path}", self.base, self.repository),
                token,
            )
            .map_err(|_| Read::Unreadable)?;
        if status == 200 {
            return serde_json::from_str(&body).map_err(|_| Read::Unreadable);
        }
        Err(if plan_refused(status, &body) {
            Read::PlanUnavailable
        } else {
            Read::Unreadable
        })
    }

    fn rulesets(&self, now: i64) -> Result<Vec<Ruleset>, Read> {
        let name = self.repository.split('/').nth(1).ok_or(Read::Unreadable)?;
        let token = self
            .tokens
            .issue(&metadata_scope(name), now)
            .map_err(|_| Read::Unreadable)?;
        let mut ids = Vec::new();
        for page in 1..=MAX_PAGES {
            let listed = self.get(
                &format!("/rulesets?per_page={PER_PAGE}&page={page}"),
                &token,
                now,
            )?;
            let listed = listed.as_array().ok_or(Read::Unreadable)?;
            for entry in listed {
                ids.push(entry["id"].as_u64().ok_or(Read::Unreadable)?);
            }
            if listed.len() < PER_PAGE {
                return ids
                    .into_iter()
                    .map(|id| {
                        serde_json::from_value(self.get(&format!("/rulesets/{id}"), &token, now)?)
                            .map_err(|_| Read::Unreadable)
                    })
                    .collect();
            }
        }
        Err(Read::Unreadable)
    }
}

/// GitHub's answer for a repository whose plan has no rulesets: a 403 whose
/// message asks to upgrade. Any other refusal reads as unreadable.
fn plan_refused(status: u16, body: &str) -> bool {
    status == 403
        && serde_json::from_str::<Value>(body).is_ok_and(|v| {
            v["message"]
                .as_str()
                .is_some_and(|m| m.contains("Upgrade to GitHub") || m.contains("plan"))
        })
}

impl RulesetSource for GithubRulesets {
    fn read(&self, now: i64) -> Read {
        self.rulesets(now).map_or_else(|read| read, Read::Rulesets)
    }
    fn apps(&self) -> Apps {
        self.apps
    }
}

/// The gate of a deployment that configures the runner App, made as the
/// queue service starts; its first read is the watch's first step
/// ([`crate::application::queue_service::watch_rulesets`]). `None` without
/// a runner App: local queues never pass through the gate.
pub fn ruleset_gate(
    config: &DeploymentConfig,
    clock: Arc<dyn crate::application::Clock>,
) -> anyhow::Result<Option<Arc<RulesetGate>>> {
    ruleset_gate_on(config, clock, Arc::new(HttpsGithub::default()))
}

fn ruleset_gate_on(
    config: &DeploymentConfig,
    clock: Arc<dyn crate::application::Clock>,
    api: Arc<dyn GithubApi>,
) -> anyhow::Result<Option<Arc<RulesetGate>>> {
    let Some(app) = RunnerApp::from_deployment(config, api.clone())? else {
        return Ok(None);
    };
    // Ids only: the bypass actors name an App by its id. The main App's key
    // is never read here.
    let id = |name: &str| config.value(name).and_then(|id| id.parse::<u64>().ok());
    let apps = Apps {
        runner: id("DAGQ_GITHUB_RUNNER_APP_ID").ok_or(ConfigUnavailable)?,
        main: id("DAGQ_GITHUB_MAIN_APP_ID"),
    };
    let source = GithubRulesets::new(
        Box::new(app),
        api,
        clock.clone(),
        apps,
        config
            .value("DAGQ_GITHUB_API_BASE_URL")
            .unwrap_or("https://api.github.com"),
        config
            .value("DAGQ_GITHUB_REPOSITORY")
            .ok_or(ConfigUnavailable)?,
    )?;
    Ok(Some(Arc::new(RulesetGate::new(Box::new(source), clock))))
}

#[cfg(test)]
mod tests;
