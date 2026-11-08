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
        let mut bytes = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(1_048_577)
            .read_to_end(&mut bytes)
            .map_err(|_| ConfigUnavailable)?;
        if bytes.len() > 1_048_576 {
            return Err(ConfigUnavailable);
        }
        String::from_utf8(bytes).map_err(|_| ConfigUnavailable)
    }
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

/// Owns only the runner App's signing key. The main App settings are never read.
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
