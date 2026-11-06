//! The HTTPS fetching of an OIDC issuer's discovery and key documents
//! (ADR-t3985-1 decision 1), a blocking client with no async runtime. The
//! decisions on an answer are [`crate::domain::oidc`]'s; this holds the
//! transport to them: `https` only, no redirect, no compressed answer, the
//! time limit over the whole fetch and at most one byte read past the limit.
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
        Self::with(RootCerts::WebPki, Duration::from_secs(FETCH_TIMEOUT_SECS))
    }

    /// Trusts only the given DER certificates (an issuer under a private CA),
    /// with this limit on the time of one fetch instead of
    /// [`FETCH_TIMEOUT_SECS`].
    pub fn with_roots(roots: &[&'static [u8]], timeout: Duration) -> Self {
        let roots: Vec<Certificate<'static>> =
            roots.iter().map(|der| Certificate::from_der(der)).collect();
        Self::with(RootCerts::new_with_certs(&roots), timeout)
    }

    fn with(roots: RootCerts, timeout: Duration) -> Self {
        let config = ureq::Agent::config_builder()
            .https_only(true)
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
