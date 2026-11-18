//! Remote refs use GitHub REST only; no checkout, git subprocess or ruleset
//! reads. The two App credentials never leave this adapter. All errors carry
//! fixed categories, never upstream bodies, URLs, transport errors or tokens.
use super::{
    deployment_config::DeploymentConfig,
    github_app::{GithubApi, HttpsGithub, MainApp, RunnerApp, valid_base, valid_repository},
};
use crate::application::{
    Clock, RunnerRefs,
    supervise::repository_config::{AppToken, ConfigUnavailable, InstallationTokens, read_scope},
};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefFailure {
    Permission,
    Temporary,
    Invalid,
}
impl std::fmt::Display for RefFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Permission => "github_ref_permission_denied",
            Self::Temporary => "github_ref_temporarily_unavailable",
            Self::Invalid => "github_ref_invalid_request",
        })
    }
}
impl std::error::Error for RefFailure {}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Present,
    Missing,
}
#[derive(Debug, Clone, Copy)]
pub enum Operation {
    Create,
    Read,
    Delete,
}

/// A 422 creation is never proof of success (including an existing ref): only
/// the subsequent uncached GET and durable head comparison verify it. GitHub
/// also answers 422 for an absent DELETE; accept only that exact fixed message.
/// Rate limits (403 with a rate-limit message, or 429) are retryable; other
/// 401/403 refusals are permissions. Malformed/unknown replies fail closed.
pub fn classify(op: Operation, status: u16, body: &str) -> Result<Reply, RefFailure> {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_owned));
    match (op, status) {
        (Operation::Read, 200) | (Operation::Create, 201 | 422) => Ok(Reply::Present),
        (Operation::Delete, 204 | 404) | (Operation::Read, 404) => Ok(Reply::Missing),
        (Operation::Delete, 422) if message.as_deref() == Some("Reference does not exist") => {
            Ok(Reply::Missing)
        }
        (_, 429 | 500..=599) => Err(RefFailure::Temporary),
        (_, 403)
            if message
                .as_ref()
                .is_some_and(|m| m.to_ascii_lowercase().contains("rate limit")) =>
        {
            Err(RefFailure::Temporary)
        }
        (_, 401 | 403) => Err(RefFailure::Permission),
        (_, 400 | 422) => Err(RefFailure::Invalid),
        _ => Err(RefFailure::Temporary),
    }
}

/// Match the assigned branch (`dagq/<run>/<generation>`). Landing candidates
/// use the separate `dagq-landing` namespace with the same generation. Inputs
/// cannot escape these namespaces; neither main nor tags are accepted here.
pub fn ref_name(run: &str, generation: i64, landing: bool) -> Result<String, RefFailure> {
    if generation <= 0
        || run.is_empty()
        || !run
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(RefFailure::Invalid);
    }
    let namespace = if landing { "dagq-landing" } else { "dagq" };
    Ok(format!("refs/heads/{namespace}/{run}/{generation}"))
}
fn valid_sha(sha: &str) -> bool {
    matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}
/// GitHub's singular GET endpoint differs from POST/DELETE's plural refs.
/// Repository and names are already validated at the construction boundary.
fn request_path(repository: &str, name: &str, operation: Operation) -> Result<String, RefFailure> {
    let path = name.strip_prefix("refs/").ok_or(RefFailure::Invalid)?;
    Ok(match operation {
        Operation::Create => format!("/repos/{repository}/git/refs"),
        Operation::Read => format!("/repos/{repository}/git/ref/{path}"),
        Operation::Delete => format!("/repos/{repository}/git/refs/{path}"),
    })
}

/// Creation never sets force or updates an existing ref.
pub fn create_body(name: &str, head: &str) -> Result<Value, RefFailure> {
    if !valid_sha(head) {
        return Err(RefFailure::Invalid);
    }
    Ok(json!({"ref": name, "sha": head}))
}
/// Reject non-commit objects and malformed or mismatched reference responses.
pub fn read_head(body: &str, name: &str) -> Result<String, RefFailure> {
    let value: Value = serde_json::from_str(body).map_err(|_| RefFailure::Temporary)?;
    let sha = value["object"]["sha"]
        .as_str()
        .filter(|s| valid_sha(s))
        .ok_or(RefFailure::Temporary)?;
    if value["ref"] != name || value["object"]["type"] != "commit" {
        return Err(RefFailure::Temporary);
    }
    Ok(sha.into())
}

/// One-repository contents:write and workflows:write (the main App scope)
/// are issued only for creation/deletion of refs to accepted commits; the
/// main App cannot be selected for a GET. The runner App always requests only
/// contents:read here, even when its installation has wider permissions.
pub struct GithubRefs {
    runner: Box<dyn InstallationTokens>,
    main: MainApp,
    api: Arc<dyn GithubApi>,
    clock: Arc<dyn Clock>,
    base: String,
    repository: String,
}
impl GithubRefs {
    pub fn from_deployment(
        config: &DeploymentConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Option<Self>, ConfigUnavailable> {
        Self::on(config, clock, Arc::new(HttpsGithub::default()))
    }
    fn on(
        config: &DeploymentConfig,
        clock: Arc<dyn Clock>,
        api: Arc<dyn GithubApi>,
    ) -> Result<Option<Self>, ConfigUnavailable> {
        // Optional App pairs are validated by DeploymentConfig. A runner-only
        // deployment retains its existing ruleset/pull service; it simply has
        // no main-App ref writer/watch or trusted main-head port configured.
        if config.value("DAGQ_GITHUB_MAIN_APP_ID").is_none() {
            return Ok(None);
        }
        let Some(runner) = RunnerApp::from_deployment(config, api.clone())? else {
            return Ok(None);
        };
        let base = config
            .value("DAGQ_GITHUB_API_BASE_URL")
            .unwrap_or("https://api.github.com");
        let repository = config
            .value("DAGQ_GITHUB_REPOSITORY")
            .ok_or(ConfigUnavailable)?;
        if !valid_base(base) || !valid_repository(repository) {
            return Err(ConfigUnavailable);
        }
        Ok(Some(Self {
            runner: Box::new(runner),
            main: MainApp::from_deployment(config, api.clone())?,
            api,
            clock,
            base: base.trim_end_matches('/').into(),
            repository: repository.into(),
        }))
    }
    fn token(&self, write: bool) -> Result<AppToken, RefFailure> {
        let name = self
            .repository
            .split('/')
            .nth(1)
            .ok_or(RefFailure::Invalid)?;
        if write {
            self.main.issue(
                &json!({"repositories": [name], "permissions": {"contents": "write", "workflows": "write"}}),
                self.clock.now(),
            )
        } else {
            self.runner.issue(&read_scope(name), self.clock.now())
        }
        .map_err(|_| RefFailure::Temporary)
    }
    fn read_named(&self, name: &str) -> Result<Option<String>, RefFailure> {
        let token = self.token(false)?;
        let path = request_path(&self.repository, name, Operation::Read)?;
        let (status, body) = self
            .api
            .get_status(&format!("{}{path}", self.base), &token)
            .map_err(|_| RefFailure::Temporary)?;
        match classify(Operation::Read, status, &body)? {
            Reply::Missing => Ok(None),
            Reply::Present => read_head(&body, name).map(Some),
        }
    }
    pub fn main_head(&self) -> anyhow::Result<crate::domain::CommitSha> {
        let head = self
            .read_named("refs/heads/main")?
            .ok_or(RefFailure::Temporary)?;
        Ok(crate::domain::CommitSha::parse(head, "main head")?)
    }
    fn remove(&self, name: String) -> anyhow::Result<()> {
        let token = self.token(true)?;
        let path = request_path(&self.repository, &name, Operation::Delete)?;
        let (status, body) = self
            .api
            .request_status("DELETE", &format!("{}{path}", self.base), &token, None)
            .map_err(|_| RefFailure::Temporary)?;
        classify(Operation::Delete, status, &body)?;
        Ok(())
    }
}
impl RunnerRefs for GithubRefs {
    fn create(&self, run: &str, generation: i64, head: &str) -> anyhow::Result<()> {
        let name = ref_name(run, generation, false)?;
        let path = request_path(&self.repository, &name, Operation::Create)?;
        let body = create_body(&name, head)?;
        let token = self.token(true)?;
        let (status, text) = self
            .api
            .request_status("POST", &format!("{}{path}", self.base), &token, Some(&body))
            .map_err(|_| RefFailure::Temporary)?;
        classify(Operation::Create, status, &text)?;
        Ok(())
    }
    fn read(&self, run: &str, generation: i64) -> anyhow::Result<Option<String>> {
        Ok(self.read_named(&ref_name(run, generation, false)?)?)
    }
    fn exists(&self, run: &str, generation: i64) -> anyhow::Result<bool> {
        Ok(self.read(run, generation)?.is_some())
    }
    fn delete(&self, run: &str, generation: i64) -> anyhow::Result<()> {
        self.remove(ref_name(run, generation, false)?)
    }
    fn delete_landing(&self, run: &str, generation: i64) -> anyhow::Result<()> {
        self.remove(ref_name(run, generation, true)?)
    }
}

#[cfg(test)]
mod tests;
