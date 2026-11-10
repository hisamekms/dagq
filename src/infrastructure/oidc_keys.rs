//! The HTTPS fetching of an OIDC issuer's discovery and key documents
//! (ADR-t3985-1 decision 1), a blocking client with no async runtime. The
//! decisions on an answer are [`crate::domain::oidc`]'s; this holds the
//! transport to them: `https` only, no redirect, no compressed answer, the
//! time limit over the whole fetch and at most one byte read past the limit.
//!
//! Human login also uses this agent for bounded form and JSON POSTs, with
//! HTTP permitted only for the CLI's checked loopback URLs, and so does a
//! person's client-mode `dagq` for its use cases at a control side
//! ([`HttpsDocumentSource::for_control`]).
//!
//! The environment's proxy settings are not read: what is checked is the
//! issuer's own answer, on the control side's direct way out.

use std::io::Read;
use std::time::Duration;

use ureq::http::header::{CACHE_CONTROL, CONTENT_LENGTH};
use ureq::tls::{Certificate, RootCerts, TlsConfig};

use crate::application::oidc::{OidcDocument, OidcDocumentSource};
use crate::domain::oidc::{
    FETCH_TIMEOUT_SECS, KeyFetchFailure, MAX_DOCUMENT_BYTES, check_body_length, check_response,
};

/// The fetching over HTTPS, trusting the Web PKI's roots or given ones.
pub struct HttpsDocumentSource {
    agent: ureq::Agent,
}

impl HttpsDocumentSource {
    /// Trusts the bundled Web PKI roots.
    pub fn new() -> Self {
        Self::with(
            RootCerts::WebPki,
            Duration::from_secs(FETCH_TIMEOUT_SECS),
            true,
        )
    }

    /// Trusts only the given DER certificates (an issuer under a private CA),
    /// with this limit on the time of one fetch instead of
    /// [`FETCH_TIMEOUT_SECS`].
    pub fn with_roots(roots: &[&'static [u8]], timeout: Duration) -> Self {
        let roots: Vec<Certificate<'static>> =
            roots.iter().map(|der| Certificate::from_der(der)).collect();
        Self::with(RootCerts::new_with_certs(&roots), timeout, true)
    }

    fn with(roots: RootCerts, timeout: Duration, https_only: bool) -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(https_only)
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .timeout_global(Some(timeout))
            .proxy(None)
            .tls_config(TlsConfig::builder().root_certs(roots).build())
            .build();
        Self {
            agent: config.into(),
        }
    }
}

impl Default for HttpsDocumentSource {
    fn default() -> Self {
        Self::new()
    }
}

impl OidcDocumentSource for HttpsDocumentSource {
    fn get(&self, url: &str) -> Result<OidcDocument, KeyFetchFailure> {
        let unreachable = |e: &dyn std::fmt::Display| KeyFetchFailure::Unreachable(e.to_string());
        let response = self.agent.get(url).call().map_err(|e| unreachable(&e))?;
        let headers = response.headers();
        // A declared length that cannot be read is left to the limit on the
        // bytes read below.
        let declared = headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok());
        check_response(response.status().as_u16(), declared)?;
        // A list may be split over several header lines (RFC 9110 section 5.2).
        let cache_control: Vec<&str> = headers
            .get_all(CACHE_CONTROL)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        let cache_control = (!cache_control.is_empty()).then(|| cache_control.join(", "));

        let mut body = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(MAX_DOCUMENT_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|e| unreachable(&e))?;
        check_body_length(body.len() as u64)?;
        Ok(OidcDocument {
            body,
            cache_control,
        })
    }
}

impl HttpsDocumentSource {
    /// The CLI alone admits HTTP loopback; each request validates its URL
    /// before the shared agent sees it. All size, redirect and time limits
    /// remain the same as the control side's HTTPS document fetches.
    pub fn for_login() -> Self {
        Self::with(
            RootCerts::WebPki,
            Duration::from_secs(FETCH_TIMEOUT_SECS),
            false,
        )
    }
    fn login_body(response: ureq::http::Response<ureq::Body>) -> anyhow::Result<serde_json::Value> {
        anyhow::ensure!(
            !response.status().is_redirection(),
            "login HTTP redirect refused"
        );
        anyhow::ensure!(
            response.status().as_u16() < 500,
            "login HTTP endpoint failed"
        );
        if let Some(length) = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
        {
            check_body_length(length)?;
        }
        let mut body = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(MAX_DOCUMENT_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|_| anyhow::anyhow!("login HTTP response unavailable"))?;
        check_body_length(body.len() as u64)?;
        serde_json::from_slice(&body).map_err(|_| anyhow::anyhow!("invalid login HTTP response"))
    }
}
impl HttpsDocumentSource {
    /// The client of a person's `dagq` to a control side's HTTP entrance
    /// ([`crate::infrastructure::queue_service::ControlHttp`]): the login's
    /// agent (no redirect followed, no proxy from the environment, HTTP
    /// only where `application::login::check_url` lets it), each call's own
    /// time limit, and the queue service's answer limit instead of a
    /// document's.
    pub fn for_control() -> Self {
        Self::for_login()
    }
}

impl crate::infrastructure::queue_service::ControlHttp for HttpsDocumentSource {
    fn post(
        &self,
        url: &str,
        bearer: Option<&str>,
        body: &[u8],
        timeout: Duration,
    ) -> anyhow::Result<(u16, Vec<u8>)> {
        crate::application::login::check_url(url)?;
        let mut request = self
            .agent
            .post(url)
            .config()
            .timeout_global(Some(timeout))
            .build()
            .content_type("application/json");
        if let Some(token) = bearer {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        // The error says where, never the header.
        let response = request
            .send(body)
            .map_err(|error| anyhow::anyhow!("{url}: {error}"))?;
        let status = response.status().as_u16();
        let limit = crate::domain::queue_service::MAX_ANSWER_BYTES as u64;
        let mut answer = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(limit + 1)
            .read_to_end(&mut answer)
            .map_err(|error| anyhow::anyhow!("{url}: the answer did not read: {error}"))?;
        Ok((status, answer))
    }
}

impl crate::application::login::LoginHttp for HttpsDocumentSource {
    fn get(&self, url: &str) -> anyhow::Result<serde_json::Value> {
        crate::application::login::check_url(url)?;
        let reply = self
            .agent
            .get(url)
            .call()
            .map_err(|_| anyhow::anyhow!("login discovery unavailable"))?;
        anyhow::ensure!(reply.status().is_success(), "login discovery refused");
        Self::login_body(reply)
    }
    fn form(
        &self,
        url: &str,
        params: &crate::application::login::Params,
    ) -> anyhow::Result<serde_json::Value> {
        crate::application::login::check_url(url)?;
        let body = super::login::form_encode(params);
        let response = self
            .agent
            .post(url)
            .content_type("application/x-www-form-urlencoded")
            .send(body.as_bytes())
            .map_err(|_| anyhow::anyhow!("login token endpoint unavailable"))?;
        Self::login_body(response)
    }
    fn exchange(&self, control: &str, jwt: &str) -> anyhow::Result<serde_json::Value> {
        crate::application::login::check_url(control)?;
        let url = format!(
            "{}/api/use-case/exchange_token",
            control.trim_end_matches('/')
        );
        let body = serde_json::json!({"api_version": crate::domain::queue_service::API_VERSION,
            "use_case": "exchange_token", "params": {"jwt": jwt}})
        .to_string();
        let response = self
            .agent
            .post(&url)
            .content_type("application/json")
            .send(body.as_bytes())
            .map_err(|_| anyhow::anyhow!("token exchange unavailable"))?;
        Self::login_body(response)
    }
}
