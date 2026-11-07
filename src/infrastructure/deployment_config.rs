//! Deployment settings, outside repository/host configuration and the DB.
//! Every name in `NAMES` also accepts `<name>_FILE`: its value is a mounted
//! UTF-8 file's path. One final LF (or CRLF) is removed, preserving PEMs and
//! other whitespace. Both forms, unreadable files, non-text and empty values
//! refuse startup. Diagnostics contain only names, never values or paths.
//! The CLI returns 78 (EX_CONFIG) for these errors before opening a queue.

use crate::domain::store_setting::{STORE_CONNECTION_ENV, STORE_KIND_ENV, StoreSetting};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fmt,
    net::SocketAddr,
    sync::Arc,
};

/// Accepted base names (each supports `_FILE`). STORE is `sqlite` (default)
/// or `postgres`; DATABASE_URL is a `postgres://` or `postgresql://` URL (including sslmode).
/// OBJECT_STORE_ENDPOINT is an explicit http(s) URL even for AWS S3; BUCKET
/// and REGION are nonempty S3 names. ACCESS_KEY_ID and SECRET_ACCESS_KEY
/// are a pair; absent, leave credentials to the SDK's standard discovery.
/// OIDC_ISSUER is an HTTPS issuer URL, AUDIENCE its nonempty audience (the
/// domain OidcConfig retains the RS256/ES256 defaults).
/// GITHUB_{RUNNER,MAIN}_APP_ID are positive numeric IDs of distinct Apps;
/// their APP_KEY values are PEM private keys, normally supplied via _FILE.
/// GITHUB_REPOSITORY and IMAGE_CI_REPOSITORY are `owner/name`;
/// IMAGE_CI_WORKFLOW is the workflow path, IMAGE_CI_REF its full `refs/...`.
/// ANTHROPIC_API_KEY is optional: absent disables the team's API key.
/// TIMEZONE is an IANA zone name, default UTC; LISTEN is a numeric socket
/// address, default 0.0.0.0:8080. TLS_CERT and TLS_KEY are an optional pair
/// of PEM certificate chain and private key (normally `_FILE`).
/// Postgres requires DB, all object store destinations, OIDC, both Apps,
/// repository and all three CI constraints. Timezone and listen have defaults.
pub const NAMES: &[&str] = &[
    "DAGQ_STORE",
    "DAGQ_DATABASE_URL",
    "DAGQ_OBJECT_STORE_ENDPOINT",
    "DAGQ_OBJECT_STORE_BUCKET",
    "DAGQ_OBJECT_STORE_REGION",
    "DAGQ_OBJECT_STORE_ACCESS_KEY_ID",
    "DAGQ_OBJECT_STORE_SECRET_ACCESS_KEY",
    "DAGQ_OIDC_ISSUER",
    "DAGQ_OIDC_AUDIENCE",
    "DAGQ_GITHUB_RUNNER_APP_ID",
    "DAGQ_GITHUB_RUNNER_APP_KEY",
    "DAGQ_GITHUB_MAIN_APP_ID",
    "DAGQ_GITHUB_MAIN_APP_KEY",
    "DAGQ_GITHUB_REPOSITORY",
    "DAGQ_ANTHROPIC_API_KEY",
    "DAGQ_IMAGE_CI_REPOSITORY",
    "DAGQ_IMAGE_CI_WORKFLOW",
    "DAGQ_IMAGE_CI_REF",
    "DAGQ_TIMEZONE",
    "DAGQ_LISTEN",
    "DAGQ_TLS_CERT",
    "DAGQ_TLS_KEY",
];

/// Names alone, sorted and deduplicated across all startup checks.
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidSettings(pub BTreeSet<String>);
impl fmt::Display for InvalidSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid deployment settings: {}",
            self.0.iter().cloned().collect::<Vec<_>>().join(", ")
        )
    }
}
impl std::error::Error for InvalidSettings {}

/// Values deliberately have no Debug: even a misplaced value may be secret.
pub struct DeploymentConfig {
    pub store: StoreSetting,
    values: BTreeMap<&'static str, String>,
    pub http: Option<HttpConfig>,
}
/// Decided without binding a port. None on DeploymentConfig means socket only.
pub struct HttpConfig {
    pub address: SocketAddr,
    pub tls: Option<Arc<rustls::ServerConfig>>,
}
impl DeploymentConfig {
    /// Read process I/O only here; `read` below owns all decisions and is
    /// testable with an environment and mounted files supplied as values.
    pub fn from_process() -> Result<Self, InvalidSettings> {
        Self::from_env(|name| std::env::var_os(name))
    }
    /// The filesystem adapter with an injected environment, also used by
    /// the composition root before running a CLI command. Decisions stay
    /// in `read`, which does no I/O itself.
    pub fn from_env(env: impl Fn(&str) -> Option<OsString>) -> Result<Self, InvalidSettings> {
        Self::read(env, |path| std::fs::read(path).map_err(|_| ()))
    }
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str).or(match name {
            "DAGQ_TIMEZONE" => Some("UTC"),
            "DAGQ_LISTEN" => Some("0.0.0.0:8080"),
            _ => None,
        })
    }
    pub fn read(
        env: impl Fn(&str) -> Option<OsString>,
        file: impl Fn(&std::path::Path) -> Result<Vec<u8>, ()>,
    ) -> Result<Self, InvalidSettings> {
        let mut errors = BTreeSet::new();
        let mut values = BTreeMap::new();
        for &name in NAMES {
            let file_name = format!("{name}_FILE");
            let direct = env(name);
            let mounted = env(&file_name);
            let text = match (direct, mounted) {
                (Some(_), Some(_)) => {
                    errors.extend([name.to_owned(), file_name]);
                    continue;
                }
                (Some(value), None) => value.into_string().map_err(|_| ()),
                (None, Some(path)) => file(std::path::Path::new(&path))
                    .and_then(|bytes| String::from_utf8(bytes).map_err(|_| ()))
                    .map(|s| {
                        s.strip_suffix("\r\n")
                            .or_else(|| s.strip_suffix('\n'))
                            .unwrap_or(&s)
                            .to_owned()
                    }),
                (None, None) => continue,
            };
            match text {
                Ok(text) if !text.trim().is_empty() => {
                    values.insert(name, text);
                }
                _ => {
                    errors.insert(name.to_owned());
                }
            }
        }
        let get = |name: &str| values.get(name).map(String::as_str);
        let store = StoreSetting::from_env(|name| get(name).map(OsString::from));
        if store.is_err() {
            errors.insert(STORE_KIND_ENV.to_owned());
            if get(STORE_CONNECTION_ENV).is_some() {
                errors.insert(STORE_CONNECTION_ENV.to_owned());
            }
        }
        if get(STORE_KIND_ENV) == Some("postgres") {
            for name in [
                "DAGQ_DATABASE_URL",
                "DAGQ_OBJECT_STORE_ENDPOINT",
                "DAGQ_OBJECT_STORE_BUCKET",
                "DAGQ_OBJECT_STORE_REGION",
                "DAGQ_OIDC_ISSUER",
                "DAGQ_OIDC_AUDIENCE",
                "DAGQ_GITHUB_RUNNER_APP_ID",
                "DAGQ_GITHUB_RUNNER_APP_KEY",
                "DAGQ_GITHUB_MAIN_APP_ID",
                "DAGQ_GITHUB_MAIN_APP_KEY",
                "DAGQ_GITHUB_REPOSITORY",
                "DAGQ_IMAGE_CI_REPOSITORY",
                "DAGQ_IMAGE_CI_WORKFLOW",
                "DAGQ_IMAGE_CI_REF",
            ] {
                if get(name).is_none() {
                    errors.insert(name.to_owned());
                }
            }
        }
        for (a, b) in [
            (
                "DAGQ_OBJECT_STORE_ACCESS_KEY_ID",
                "DAGQ_OBJECT_STORE_SECRET_ACCESS_KEY",
            ),
            ("DAGQ_OIDC_ISSUER", "DAGQ_OIDC_AUDIENCE"),
            ("DAGQ_TLS_CERT", "DAGQ_TLS_KEY"),
            ("DAGQ_GITHUB_RUNNER_APP_ID", "DAGQ_GITHUB_RUNNER_APP_KEY"),
            ("DAGQ_GITHUB_MAIN_APP_ID", "DAGQ_GITHUB_MAIN_APP_KEY"),
        ] {
            if get(a).is_some() != get(b).is_some() {
                errors.extend([a.to_owned(), b.to_owned()]);
            }
        }
        for name in ["DAGQ_GITHUB_RUNNER_APP_ID", "DAGQ_GITHUB_MAIN_APP_ID"] {
            if get(name).is_some_and(|s| !s.parse::<u64>().is_ok_and(|n| n > 0)) {
                errors.insert(name.to_owned());
            }
        }
        let runner_app = get("DAGQ_GITHUB_RUNNER_APP_ID").and_then(|id| id.parse::<u64>().ok());
        let main_app = get("DAGQ_GITHUB_MAIN_APP_ID").and_then(|id| id.parse::<u64>().ok());
        if runner_app.is_some() && runner_app == main_app {
            errors.extend([
                "DAGQ_GITHUB_RUNNER_APP_ID".to_owned(),
                "DAGQ_GITHUB_MAIN_APP_ID".to_owned(),
            ]);
        }
        for name in ["DAGQ_GITHUB_REPOSITORY", "DAGQ_IMAGE_CI_REPOSITORY"] {
            if get(name)
                .is_some_and(|s| s.split('/').count() != 2 || s.split('/').any(str::is_empty))
            {
                errors.insert(name.to_owned());
            }
        }
        for (name, schemes) in [
            ("DAGQ_OBJECT_STORE_ENDPOINT", &["https", "http"][..]),
            ("DAGQ_OIDC_ISSUER", &["https"][..]),
            ("DAGQ_DATABASE_URL", &["postgres", "postgresql"][..]),
        ] {
            if get(name).is_some_and(|s| !valid_url(s, schemes)) {
                errors.insert(name.to_owned());
            }
        }
        if get("DAGQ_IMAGE_CI_REF").is_some_and(|s| !s.starts_with("refs/") || s.len() == 5) {
            errors.insert("DAGQ_IMAGE_CI_REF".to_owned());
        }
        for name in ["DAGQ_GITHUB_RUNNER_APP_KEY", "DAGQ_GITHUB_MAIN_APP_KEY"] {
            if get(name).is_some_and(|s| !valid_app_key(s)) {
                errors.insert(name.to_owned());
            }
        }
        if get("DAGQ_TIMEZONE").is_some_and(|s| s.parse::<chrono_tz::Tz>().is_err()) {
            errors.insert("DAGQ_TIMEZONE".to_owned());
        }
        let address = get("DAGQ_LISTEN")
            .unwrap_or("0.0.0.0:8080")
            .parse::<SocketAddr>();
        if address.is_err() {
            errors.insert("DAGQ_LISTEN".to_owned());
        }
        let tls = match (get("DAGQ_TLS_CERT"), get("DAGQ_TLS_KEY")) {
            (Some(cert), Some(key)) => match tls_config(cert, key) {
                Ok(tls) => Some(Arc::new(tls)),
                Err(()) => {
                    errors.extend(["DAGQ_TLS_CERT".to_owned(), "DAGQ_TLS_KEY".to_owned()]);
                    None
                }
            },
            _ => None,
        };
        if !errors.is_empty() {
            return Err(InvalidSettings(errors));
        }
        let http = get("DAGQ_OIDC_ISSUER").map(|_| HttpConfig {
            address: address.unwrap(),
            tls,
        });
        Ok(Self {
            store: store.expect("checked store"),
            values,
            http,
        })
    }
}
fn tls_config(cert: &str, key: &str) -> Result<rustls::ServerConfig, ()> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    let certificates = CertificateDer::pem_slice_iter(cert.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ())?;
    let key = PrivateKeyDer::from_pem_slice(key.as_bytes()).map_err(|_| ())?;
    rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|_| ())?
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|_| ())
}

fn valid_app_key(pem: &str) -> bool {
    use rustls::pki_types::{PrivateKeyDer, pem::PemObject};
    match PrivateKeyDer::from_pem_slice(pem.as_bytes()) {
        Ok(PrivateKeyDer::Pkcs1(key)) => {
            ring::signature::RsaKeyPair::from_der(key.secret_pkcs1_der()).is_ok()
        }
        Ok(PrivateKeyDer::Pkcs8(key)) => {
            ring::signature::RsaKeyPair::from_pkcs8(key.secret_pkcs8_der()).is_ok()
        }
        _ => false,
    }
}

fn valid_url(text: &str, schemes: &[&str]) -> bool {
    text.parse::<ureq::http::Uri>().is_ok_and(|uri| {
        uri.scheme_str()
            .is_some_and(|scheme| schemes.contains(&scheme))
            && uri.host().is_some_and(|host| !host.is_empty())
    })
}

#[cfg(test)]
mod tests;
