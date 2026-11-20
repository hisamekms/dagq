//! Runner-App authentication and GitHub reads for the control side. API_BASE_URL
//! is the REST base (including `/api/v3` on Enterprise), HTTPS with no userinfo,
//! query or fragment. Requests never follow redirects. No upstream body, URL,
//! key, JWT, token or transport error is propagated to diagnostics or events.
//! JWTs last nine minutes (iat backdated one minute); each refresh obtains a
//! fresh installation token, used only in memory for that refresh. Dependency
//! wire tracing is compiled out, and Authorization HeaderValues are sensitive.

use super::deployment_config::DeploymentConfig;
use crate::application::supervise::repository_config::{
    AppToken, ConfigUnavailable, InstallationTokens, MainRules,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::signature;
use rustls::pki_types::{PrivateKeyDer, pem::PemObject};
use serde_json::{Value, json};
use std::{io::Read, sync::Arc, time::Duration};

/// A request port, also usable for fake API replies without sockets. Credentials
/// have no printable representation; all failures cross as a fixed safe error.
pub trait GithubApi: Send + Sync {
    fn request(
        &self,
        method: &str,
        url: &str,
        bearer: &AppToken,
        body: Option<&Value>,
        raw: bool,
    ) -> Result<String, ConfigUnavailable>;
    /// Status-preserving mutations for ref adapters. Legacy fakes may keep the
    /// success-only implementation; production retains GitHub's refusal status.
    fn request_status(
        &self,
        method: &str,
        url: &str,
        bearer: &AppToken,
        body: Option<&Value>,
    ) -> Result<(u16, String), ConfigUnavailable> {
        self.request(method, url, bearer, body, false)
            .map(|text| (200, text))
    }
    /// Raw contents with status retained so only a real 404 means absent.
    fn raw_status(&self, url: &str, bearer: &AppToken) -> Result<(u16, String), ConfigUnavailable> {
        self.request("GET", url, bearer, None, true)
            .map(|body| (200, body))
    }
    /// A JSON GET answered with any HTTP status and its body (bounded as
    /// [`Self::request`]'s), for a reader that tells refusals apart. Only a
    /// transport failure is an error. A fake without statuses answers 200.
    fn get_status(&self, url: &str, bearer: &AppToken) -> Result<(u16, String), ConfigUnavailable> {
        self.request("GET", url, bearer, None, false)
            .map(|body| (200, body))
    }
}

pub struct HttpsGithub(ureq::Agent);
impl Default for HttpsGithub {
    fn default() -> Self {
        Self(
            ureq::Agent::config_builder()
                .https_only(true)
                .max_redirects(0)
                .max_redirects_will_error(true)
                .timeout_global(Some(Duration::from_secs(15)))
                .proxy(None)
                .build()
                .into(),
        )
    }
}
impl GithubApi for HttpsGithub {
    fn request(
        &self,
        method: &str,
        url: &str,
        bearer: &AppToken,
        body: Option<&Value>,
        raw: bool,
    ) -> Result<String, ConfigUnavailable> {
        let accept = if raw {
            "application/vnd.github.raw+json"
        } else {
            "application/vnd.github+json"
        };
        let authorization = bearer_header(bearer)?;
        let response = match method {
            "POST" => self
                .0
                .post(url)
                .header("Authorization", authorization)
                .header("User-Agent", "dagq")
                .header("Accept", accept)
                .header("Content-Type", "application/json")
                .send(body.ok_or(ConfigUnavailable)?.to_string()),
            "GET" => self
                .0
                .get(url)
                .header("Authorization", authorization)
                .header("User-Agent", "dagq")
                .header("Accept", accept)
                .call(),
            _ => return Err(ConfigUnavailable),
        }
        .map_err(|_| ConfigUnavailable)?;
        if !response.status().is_success() {
            return Err(ConfigUnavailable);
        }
        bounded_body(response)
    }
    fn request_status(
        &self,
        method: &str,
        url: &str,
        bearer: &AppToken,
        body: Option<&Value>,
    ) -> Result<(u16, String), ConfigUnavailable> {
        let request = match method {
            "POST" => self.0.post(url),
            "DELETE" => self.0.delete(url).force_send_body(),
            _ => return Err(ConfigUnavailable),
        };
        let response = request
            .config()
            .http_status_as_error(false)
            .build()
            .header("Authorization", bearer_header(bearer)?)
            .header("User-Agent", "dagq")
            .header("Accept", "application/vnd.github+json")
            .header("Content-Type", "application/json")
            .send(body.map_or_else(String::new, Value::to_string))
            .map_err(|_| ConfigUnavailable)?;
        let status = response.status().as_u16();
        Ok((status, bounded_body(response)?))
    }
    fn raw_status(&self, url: &str, bearer: &AppToken) -> Result<(u16, String), ConfigUnavailable> {
        self.read_status(url, bearer, "application/vnd.github.raw+json")
    }
    fn get_status(&self, url: &str, bearer: &AppToken) -> Result<(u16, String), ConfigUnavailable> {
        self.read_status(url, bearer, "application/vnd.github+json")
    }
}
impl HttpsGithub {
    fn read_status(
        &self,
        url: &str,
        bearer: &AppToken,
        accept: &str,
    ) -> Result<(u16, String), ConfigUnavailable> {
        let response = self
            .0
            .get(url)
            .config()
            .http_status_as_error(false)
            .build()
            .header("Authorization", bearer_header(bearer)?)
            .header("User-Agent", "dagq")
            .header("Accept", accept)
            .call()
            .map_err(|_| ConfigUnavailable)?;
        let status = response.status().as_u16();
        // Raw contents supports GitHub's full 100 MiB file ceiling. JSON and
        // all pre-existing calls retain the smaller diagnostic/body bound.
        let limit = if accept == "application/vnd.github.raw+json" {
            100 * 1_048_576
        } else {
            1_048_576
        };
        Ok((status, bounded_body_to(response, limit)?))
    }
}

/// Existing JSON/startup reads retain their 1 MiB UTF-8 bound.
fn bounded_body(response: ureq::http::Response<ureq::Body>) -> Result<String, ConfigUnavailable> {
    bounded_body_to(response, 1_048_576)
}
fn bounded_body_to(
    response: ureq::http::Response<ureq::Body>,
    limit: usize,
) -> Result<String, ConfigUnavailable> {
    read_bounded(response.into_body().into_reader(), limit)
}
/// Bound even chunked responses, and reject invalid UTF-8 without echoing bytes.
fn read_bounded(reader: impl Read, limit: usize) -> Result<String, ConfigUnavailable> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ConfigUnavailable)?;
    if bytes.len() > limit {
        return Err(ConfigUnavailable);
    }
    String::from_utf8(bytes).map_err(|_| ConfigUnavailable)
}

/// Keep generic header Debug from exposing credentials as well as the HTTP
/// client's redacted request logging. Raw wire tracing is disabled in Cargo.
fn bearer_header(token: &AppToken) -> Result<ureq::http::HeaderValue, ConfigUnavailable> {
    let mut value: ureq::http::HeaderValue = format!("Bearer {}", token.0)
        .parse()
        .map_err(|_| ConfigUnavailable)?;
    value.set_sensitive(true);
    Ok(value)
}

/// Runner-facing App authentication. `from_deployment` selects only the runner
/// settings; the private MainApp reuses its signing/issuance primitive while
/// exposing no repository-read methods.
/// The installation is discovered for this repository using an App JWT; issuing
/// a token is a separate port so lease issuance can supply its own reduced scope.
pub struct RunnerApp {
    app_id: String,
    key: signature::RsaKeyPair,
    base: String,
    repository: String,
    api: Arc<dyn GithubApi>,
}
impl RunnerApp {
    pub fn from_deployment(
        config: &DeploymentConfig,
        api: Arc<dyn GithubApi>,
    ) -> Result<Option<Self>, ConfigUnavailable> {
        let Some(id) = config.value("DAGQ_GITHUB_RUNNER_APP_ID") else {
            return Ok(None);
        };
        Self::new(
            id,
            config
                .value("DAGQ_GITHUB_RUNNER_APP_KEY")
                .ok_or(ConfigUnavailable)?,
            config
                .value("DAGQ_GITHUB_REPOSITORY")
                .ok_or(ConfigUnavailable)?,
            config
                .value("DAGQ_GITHUB_API_BASE_URL")
                .unwrap_or("https://api.github.com"),
            api,
        )
        .map(Some)
    }
    pub fn new(
        id: &str,
        pem: &str,
        repository: &str,
        base: &str,
        api: Arc<dyn GithubApi>,
    ) -> Result<Self, ConfigUnavailable> {
        if !valid_base(base)
            || !valid_repository(repository)
            || !id.parse::<u64>().is_ok_and(|n| n > 0)
        {
            return Err(ConfigUnavailable);
        }
        let key = match PrivateKeyDer::from_pem_slice(pem.as_bytes())
            .map_err(|_| ConfigUnavailable)?
        {
            PrivateKeyDer::Pkcs1(key) => signature::RsaKeyPair::from_der(key.secret_pkcs1_der()),
            PrivateKeyDer::Pkcs8(key) => signature::RsaKeyPair::from_pkcs8(key.secret_pkcs8_der()),
            _ => return Err(ConfigUnavailable),
        }
        .map_err(|_| ConfigUnavailable)?;
        Ok(Self {
            app_id: id.into(),
            key,
            base: base.trim_end_matches('/').into(),
            repository: repository.into(),
            api,
        })
    }
    fn jwt(&self, now: i64) -> Result<AppToken, ConfigUnavailable> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD.encode(json!({"iss": self.app_id, "iat": now.saturating_sub(60), "exp": now.saturating_add(540)}).to_string());
        let message = format!("{header}.{claims}");
        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &signature::RSA_PKCS1_SHA256,
                &ring::rand::SystemRandom::new(),
                message.as_bytes(),
                &mut signature,
            )
            .map_err(|_| ConfigUnavailable)?;
        Ok(AppToken(format!(
            "{message}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        )))
    }
    fn json(&self, path: &str, token: &AppToken) -> Result<Value, ConfigUnavailable> {
        serde_json::from_str(&self.api.request(
            "GET",
            &format!("{}{path}", self.base),
            token,
            None,
            false,
        )?)
        .map_err(|_| ConfigUnavailable)
    }
}
impl InstallationTokens for RunnerApp {
    fn issue(&self, scope: &Value, now: i64) -> Result<AppToken, ConfigUnavailable> {
        let jwt = self.jwt(now)?;
        let installation = self.json(&format!("/repos/{}/installation", self.repository), &jwt)?;
        let id = installation["id"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or(ConfigUnavailable)?;
        let text = self.api.request(
            "POST",
            &format!("{}/app/installations/{id}/access_tokens", self.base),
            &jwt,
            Some(scope),
            false,
        )?;
        let value: Value = serde_json::from_str(&text).map_err(|_| ConfigUnavailable)?;
        let token = value["token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or(ConfigUnavailable)?;
        Ok(AppToken(token.into()))
    }
}
impl MainRules for RunnerApp {
    fn main_sha(&self, token: &AppToken) -> Result<String, ConfigUnavailable> {
        let value = self.json(
            &format!("/repos/{}/git/ref/heads/main", self.repository),
            token,
        )?;
        let sha = value["object"]["sha"].as_str().ok_or(ConfigUnavailable)?;
        if !valid_sha(sha) {
            return Err(ConfigUnavailable);
        }
        Ok(sha.into())
    }
    fn contents(&self, token: &AppToken, sha: &str) -> Result<String, ConfigUnavailable> {
        if !valid_sha(sha) {
            return Err(ConfigUnavailable);
        }
        self.api.request(
            "GET",
            &format!(
                "{}/repos/{}/contents/dagq.toml?ref={sha}",
                self.base, self.repository
            ),
            token,
            None,
            true,
        )
    }
}
fn valid_sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}
/// Restrict URL segments so repository settings cannot inject a path or query.
pub fn valid_repository(s: &str) -> bool {
    s.split('/').count() == 2
        && s.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        })
}
/// Accept Enterprise's REST prefix but no credentials or URL suffix parameters.
pub fn valid_base(s: &str) -> bool {
    s.parse::<ureq::http::Uri>().is_ok_and(|url| {
        url.scheme_str() == Some("https")
            && url.host().is_some()
            && !url.authority().is_some_and(|a| a.as_str().contains('@'))
            && url.query().is_none()
            && !s.contains('#')
    })
}

#[cfg(test)]
mod tests;

/// Complete parsed rules; polling and cache updates belong to the application.
pub type RemoteRules =
    crate::application::supervise::repository_config::RemoteRules<super::run_env::Config>;
/// Select the runner credentials and parser without inspecting a checkout.
/// Deployment configuration is trusted to select the recipient of credentials,
/// including an Enterprise host. Redirects cannot change that recipient.
pub fn remote_repository_rules(
    config: &DeploymentConfig,
    clock: Arc<dyn crate::application::Clock>,
) -> anyhow::Result<Option<Arc<RemoteRules>>> {
    let Some(app) = RunnerApp::from_deployment(config, Arc::new(HttpsGithub::default()))? else {
        return Ok(None);
    };
    let name = config
        .value("DAGQ_GITHUB_REPOSITORY")
        .ok_or(ConfigUnavailable)?
        .split('/')
        .nth(1)
        .ok_or(ConfigUnavailable)?
        .to_owned();
    Ok(Some(Arc::new(RemoteRules::new(
        Box::new(app),
        name,
        clock,
        |text| super::run_env::parse_config(text).map_err(|_| ConfigUnavailable),
    ))))
}
/// Adapt a parsed snapshot into startup tables, preserving explicit options.
pub fn repository_startup_file(
    source: Arc<RemoteRules>,
    overrides: crate::application::supervise::repository_config::StartupOverrides,
) -> crate::application::supervise::repository_config::RepositoryStartupFile {
    Arc::new(move || {
        let rules = source.read()?;
        Ok(overrides.resolve(
            crate::application::supervise::repository_config::StartupRules {
                stall: rules.stall,
                disk: rules.disk,
                resume: rules.resume,
                review_jobs: rules.review_jobs,
            },
        ))
    })
}

/// Main App credentials never implement the runner token port. They are kept
/// behind the ref writer, scoped to one repository and the main App's
/// contents/workflows write permissions. Reads use RunnerApp.
pub(super) struct MainApp(RunnerApp);
impl MainApp {
    pub(super) fn from_deployment(
        config: &DeploymentConfig,
        api: Arc<dyn GithubApi>,
    ) -> Result<Self, ConfigUnavailable> {
        Ok(Self(RunnerApp::new(
            config
                .value("DAGQ_GITHUB_MAIN_APP_ID")
                .ok_or(ConfigUnavailable)?,
            config
                .value("DAGQ_GITHUB_MAIN_APP_KEY")
                .ok_or(ConfigUnavailable)?,
            config
                .value("DAGQ_GITHUB_REPOSITORY")
                .ok_or(ConfigUnavailable)?,
            config
                .value("DAGQ_GITHUB_API_BASE_URL")
                .unwrap_or("https://api.github.com"),
            api,
        )?))
    }
    pub(super) fn issue(&self, scope: &Value, now: i64) -> Result<AppToken, ConfigUnavailable> {
        self.0.issue(scope, now)
    }
}

/// Control-side committed reads. Only RunnerApp implements the token port used
/// here: contents:read for one repository, also for dagq.toml. No main key is
/// loaded. Tokens live for one call; failures expose only a fixed error. API
/// requests are outside the writer transaction, as are the ref adapter's reads.
pub struct GithubReviewReads {
    app: RunnerApp,
    clock: Arc<dyn crate::application::Clock>,
}
impl GithubReviewReads {
    pub fn from_deployment(
        config: &DeploymentConfig,
        clock: Arc<dyn crate::application::Clock>,
    ) -> Result<Option<Self>, ConfigUnavailable> {
        Ok(
            RunnerApp::from_deployment(config, Arc::new(HttpsGithub::default()))?
                .map(|app| Self { app, clock }),
        )
    }
    fn token(&self) -> Result<AppToken, ConfigUnavailable> {
        let name = self
            .app
            .repository
            .split('/')
            .nth(1)
            .ok_or(ConfigUnavailable)?;
        self.app.issue(
            &crate::application::supervise::repository_config::read_scope(name),
            self.clock.now(),
        )
    }
    fn compare(&self, a: &str, b: &str) -> Result<Value, ConfigUnavailable> {
        if !valid_sha(a) || !valid_sha(b) {
            return Err(ConfigUnavailable);
        }
        let token = self.token()?;
        self.app.json(
            &format!(
                "/repos/{}/compare/{a}...{b}?per_page=1",
                self.app.repository
            ),
            &token,
        )
    }
}

/// Compare's first page includes at most 300 files, even with commit paging.
/// At the boundary completeness cannot be established, so refuse it too.
/// Include both names of a rename, matching local git diff --no-renames.
/// Malformed or partial responses must never silently disable required checks.
pub fn complete_changed_paths(value: &Value) -> Result<Vec<String>, ConfigUnavailable> {
    let files = value["files"]
        .as_array()
        .filter(|f| f.len() < 300)
        .ok_or(ConfigUnavailable)?;
    let mut paths = std::collections::BTreeSet::new();
    for file in files {
        let name = file["filename"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or(ConfigUnavailable)?;
        paths.insert(name.to_owned());
        if file["status"] == "renamed" {
            let old = file["previous_filename"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or(ConfigUnavailable)?;
            paths.insert(old.to_owned());
        }
    }
    Ok(paths.into_iter().collect())
}
impl crate::application::review_input::ReviewReads for GithubReviewReads {
    fn main_head(&self) -> anyhow::Result<crate::domain::CommitSha> {
        let token = self.token()?;
        let head = super::github_refs::read_reference(
            &*self.app.api,
            &self.app.base,
            &self.app.repository,
            "refs/heads/main",
            &token,
        )?
        .ok_or(ConfigUnavailable)?;
        Ok(crate::domain::CommitSha::parse(head, "main head")?)
    }
    fn file_in(&self, commit: &str, path: &str) -> anyhow::Result<Option<String>> {
        if !valid_sha(commit) || path.is_empty() {
            return Err(ConfigUnavailable.into());
        }
        // Encode the path as data, retaining slash separators. Neither a query
        // nor a fragment in a repository filename can change the pinned ref.
        let path: String = path
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        let token = self.token()?;
        let (status, body) = self.app.api.raw_status(
            &format!(
                "{}/repos/{}/contents/{path}?ref={commit}",
                self.app.base, self.app.repository
            ),
            &token,
        )?;
        match status {
            200 => Ok(Some(body)),
            404 => Ok(None),
            _ => Err(ConfigUnavailable.into()),
        }
    }
    fn merge_base(&self, a: &str, b: &str) -> anyhow::Result<Option<crate::domain::CommitSha>> {
        let value = self.compare(a, b)?;
        let sha = value["merge_base_commit"]["sha"]
            .as_str()
            .filter(|s| valid_sha(s))
            .ok_or(ConfigUnavailable)?;
        Ok(Some(crate::domain::CommitSha::parse(
            sha.to_owned(),
            "merge base",
        )?))
    }
    fn changed_paths(&self, from: &str, to: &str) -> anyhow::Result<Vec<String>> {
        Ok(complete_changed_paths(&self.compare(from, to)?)?)
    }
}
