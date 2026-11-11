//! Deployment settings, outside repository/host configuration and the DB.
//! Every name in `NAMES` also accepts `<name>_FILE`: its value is a mounted
//! UTF-8 file's path. One final LF (or CRLF) is removed, preserving PEMs and
//! other whitespace. Both forms, unreadable files, non-text and empty values
//! refuse startup. DATABASE_CA_FILE is a PEM file path, not a secret-file
//! suffix for DATABASE_CA; it has no indirect form. Diagnostics contain only
//! names and fixed reasons, never values or paths.
//! The CLI returns 78 (EX_CONFIG) for these errors before opening a queue.

use crate::domain::credential_preference::{self as credential, Deployment};
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
/// OIDC_ISSUER is an HTTPS issuer URL, AUDIENCE its nonempty audience.
/// For human login DAGQ_OIDC_AUDIENCE must equal the CLI public client_id
/// (the ID token audience). OidcConfig retains the RS256/ES256 defaults.
/// GITHUB_{RUNNER,MAIN}_APP_ID are positive numeric IDs of distinct Apps;
/// their APP_KEY values are PEM private keys, normally supplied via _FILE.
/// GITHUB_API_BASE_URL is the HTTPS REST base, default https://api.github.com;
/// Enterprise includes its /api/v3 prefix. No credentials, query or fragment.
/// GITHUB_REPOSITORY and IMAGE_CI_REPOSITORY are `owner/name`;
/// IMAGE_CI_WORKFLOW is the workflow path, IMAGE_CI_REF its full `refs/...`.
/// The five IMAGE_CI_ settings are one group, all or none
/// ([`DeploymentConfig::image_publish`]).
/// ANTHROPIC_API_KEY is optional: absent disables the team's API key.
/// CLAUDE_TEAM_API_KEY and CLAUDE_PREFER are the team's Claude credential
/// choice ([`DeploymentConfig::claude`], `domain::credential_preference`).
/// TIMEZONE is an IANA zone name, default UTC; LISTEN is a numeric socket
/// address, default 0.0.0.0:8080. TLS_CERT and TLS_KEY are an optional pair
/// of PEM certificate chain and private key (normally `_FILE`).
/// Postgres requires DB, all object store destinations, OIDC, both Apps,
/// repository and all five CI settings. Timezone and listen have defaults.
pub const NAMES: &[&str] = &[
    "DAGQ_ACTIONS_ISSUER",
    "DAGQ_ACTIONS_AUDIENCE",
    "DAGQ_ACTIONS_REPOSITORY",
    "DAGQ_ACTIONS_WORKFLOW_REF",
    "DAGQ_ACTIONS_REF",
    "DAGQ_ACTIONS_OWNER_ISSUER",
    "DAGQ_ACTIONS_OWNER_SUBJECT",
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
    "DAGQ_GITHUB_API_BASE_URL",
    "DAGQ_ANTHROPIC_API_KEY",
    "DAGQ_CLAUDE_TEAM_API_KEY",
    "DAGQ_CLAUDE_PREFER",
    "DAGQ_IMAGE_CI_ISSUER",
    "DAGQ_IMAGE_CI_AUDIENCE",
    "DAGQ_IMAGE_CI_REPOSITORY",
    "DAGQ_IMAGE_CI_WORKFLOW",
    "DAGQ_IMAGE_CI_REF",
    "DAGQ_TIMEZONE",
    "DAGQ_LISTEN",
    "DAGQ_TLS_CERT",
    "DAGQ_TLS_KEY",
];

/// Names sorted and deduplicated across all startup checks, with an optional
/// fixed reason for an invalid database CA bundle. Never retains input values.
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidSettings(pub BTreeSet<String>, Option<&'static str>);
impl fmt::Display for InvalidSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid deployment settings: {}",
            self.0.iter().cloned().collect::<Vec<_>>().join(", ")
        )?;
        if let Some(reason) = self.1 {
            write!(f, "; {DATABASE_CA_FILE}: {reason}")?;
        }
        Ok(())
    }
}
impl std::error::Error for InvalidSettings {}

/// Values deliberately have no Debug: even a misplaced value may be secret.
pub struct DeploymentConfig {
    pub store: StoreSetting,
    values: BTreeMap<&'static str, String>,
    pub http: Option<HttpConfig>,
    claude: Deployment,
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
    /// Whether the team's API key may be chosen and the default order,
    /// checked at startup: allowing it without its secret refused to start.
    pub fn claude(&self) -> Deployment {
        self.claude
    }
    /// Actions settings are optional as a group. Each accepts `_FILE`, like
    /// other deployment values. Owner issuer/subject name a registry human;
    /// workflow_ref includes the repository path and pinned refs/... suffix.
    /// The Actions issuer is fixed to GitHub's public OIDC issuer.
    pub fn github_actions(&self) -> Option<crate::domain::runner_registration::GithubActions> {
        use crate::domain::{oidc::OidcIdentity, runner_registration::GithubActions};
        Some(GithubActions {
            issuer: self.value("DAGQ_ACTIONS_ISSUER")?.into(),
            audience: self.value("DAGQ_ACTIONS_AUDIENCE")?.into(),
            repository: self.value("DAGQ_ACTIONS_REPOSITORY")?.into(),
            workflow_ref: self.value("DAGQ_ACTIONS_WORKFLOW_REF")?.into(),
            git_ref: self.value("DAGQ_ACTIONS_REF")?.into(),
            owner: OidcIdentity {
                issuer: self.value("DAGQ_ACTIONS_OWNER_ISSUER")?.into(),
                subject: self.value("DAGQ_ACTIONS_OWNER_SUBJECT")?.into(),
            },
        })
    }
    /// The binding of the CI that hands over image digests, when its five
    /// settings are set (startup refuses some without the others). The
    /// issuer is GitHub's Actions issuer; the audience is the one the CI asks
    /// its JWT for, another than the standby runners'; the workflow is the
    /// repository's `.github/workflows/` file, run at IMAGE_CI_REF, a
    /// `refs/heads/` ref naming main. The JWT's `workflow_ref` is compared
    /// with `<repository>/<workflow>@<ref>`, which must differ from the
    /// runners' DAGQ_ACTIONS_WORKFLOW_REF.
    pub fn image_publish(&self) -> Option<crate::domain::image_publish::PublishBinding> {
        let repository = self.value("DAGQ_IMAGE_CI_REPOSITORY")?;
        let workflow = self.value("DAGQ_IMAGE_CI_WORKFLOW")?;
        let git_ref = self.value("DAGQ_IMAGE_CI_REF")?;
        Some(crate::domain::image_publish::PublishBinding {
            issuer: self.value("DAGQ_IMAGE_CI_ISSUER")?.into(),
            audience: self.value("DAGQ_IMAGE_CI_AUDIENCE")?.into(),
            repository: repository.into(),
            workflow_ref: format!("{repository}/{workflow}@{git_ref}"),
            git_ref: git_ref.into(),
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
        let mut store = StoreSetting::from_env(|name| get(name).map(OsString::from));
        let ca = database_ca(get(STORE_KIND_ENV), env(DATABASE_CA_FILE), &file);
        let ca_error = ca.as_ref().err().copied();
        if ca_error.is_some() {
            errors.insert(DATABASE_CA_FILE.to_owned());
        }
        if let (
            Ok(StoreSetting::Postgres {
                connection: Some(connection),
            }),
            Ok(Some(ca)),
        ) = (&mut store, ca)
        {
            *connection = connection.clone().with_ca_certificates(ca);
        }
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
                "DAGQ_IMAGE_CI_ISSUER",
                "DAGQ_IMAGE_CI_AUDIENCE",
                "DAGQ_IMAGE_CI_REPOSITORY",
                "DAGQ_IMAGE_CI_WORKFLOW",
                "DAGQ_IMAGE_CI_REF",
            ] {
                if get(name).is_none() {
                    errors.insert(name.to_owned());
                }
            }
        }
        let images: Vec<_> = NAMES
            .iter()
            .copied()
            .filter(|name| name.starts_with("DAGQ_IMAGE_CI_"))
            .collect();
        if images.iter().any(|name| get(name).is_some()) {
            for name in images {
                if get(name).is_none() {
                    errors.insert(name.to_owned());
                }
            }
            if get("DAGQ_IMAGE_CI_ISSUER")
                != Some(crate::domain::runner_registration::ACTIONS_ISSUER)
            {
                errors.insert("DAGQ_IMAGE_CI_ISSUER".into());
            }
            if get("DAGQ_IMAGE_CI_WORKFLOW").is_some_and(|w| {
                w.strip_prefix(".github/workflows/")
                    .is_none_or(|file| file.is_empty() || file.contains(['/', '@']))
            }) {
                errors.insert("DAGQ_IMAGE_CI_WORKFLOW".into());
            }
            // The runners' identity never hands over a digest.
            if get("DAGQ_ACTIONS_AUDIENCE").is_some()
                && get("DAGQ_ACTIONS_AUDIENCE") == get("DAGQ_IMAGE_CI_AUDIENCE")
            {
                errors.insert("DAGQ_IMAGE_CI_AUDIENCE".into());
            }
            if let (Some(repo), Some(workflow), Some(git_ref), Some(runners)) = (
                get("DAGQ_IMAGE_CI_REPOSITORY"),
                get("DAGQ_IMAGE_CI_WORKFLOW"),
                get("DAGQ_IMAGE_CI_REF"),
                get("DAGQ_ACTIONS_WORKFLOW_REF"),
            ) && format!("{repo}/{workflow}@{git_ref}") == runners
            {
                errors.insert("DAGQ_IMAGE_CI_WORKFLOW".into());
            }
        }
        let actions: Vec<_> = NAMES
            .iter()
            .copied()
            .filter(|name| name.starts_with("DAGQ_ACTIONS_"))
            .collect();
        if actions.iter().any(|name| get(name).is_some()) {
            for name in actions {
                if get(name).is_none() {
                    errors.insert(name.to_owned());
                }
            }
            if get("DAGQ_ACTIONS_ISSUER") != Some("https://token.actions.githubusercontent.com") {
                errors.insert("DAGQ_ACTIONS_ISSUER".into());
            }
            if get("DAGQ_ACTIONS_OWNER_ISSUER").is_some_and(|s| !valid_url(s, &["https"])) {
                errors.insert("DAGQ_ACTIONS_OWNER_ISSUER".into());
            }
            if get("DAGQ_ACTIONS_REPOSITORY")
                .is_some_and(|s| !super::github_app::valid_repository(s))
            {
                errors.insert("DAGQ_ACTIONS_REPOSITORY".into());
            }
            if get("DAGQ_ACTIONS_REF").is_some_and(|s| !s.starts_with("refs/") || s.len() == 5) {
                errors.insert("DAGQ_ACTIONS_REF".into());
            }
            if let (Some(repo), Some(workflow), Some(git_ref)) = (
                get("DAGQ_ACTIONS_REPOSITORY"),
                get("DAGQ_ACTIONS_WORKFLOW_REF"),
                get("DAGQ_ACTIONS_REF"),
            ) && (!workflow.starts_with(&format!("{repo}/.github/workflows/"))
                || !workflow.ends_with(&format!("@{git_ref}")))
            {
                errors.insert("DAGQ_ACTIONS_WORKFLOW_REF".into());
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
            if get(name).is_some_and(|s| !super::github_app::valid_repository(s)) {
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
        if get("DAGQ_GITHUB_API_BASE_URL").is_some_and(|s| !super::github_app::valid_base(s)) {
            errors.insert("DAGQ_GITHUB_API_BASE_URL".to_owned());
        }
        if get("DAGQ_GITHUB_RUNNER_APP_ID").is_some() && get("DAGQ_GITHUB_REPOSITORY").is_none() {
            errors.insert("DAGQ_GITHUB_REPOSITORY".to_owned());
        }
        if get("DAGQ_IMAGE_CI_REF").is_some_and(|s| {
            s.strip_prefix("refs/heads/")
                .is_none_or(|branch| branch.is_empty() || branch.contains('@'))
        }) {
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
        let claude = Deployment::read(
            get(credential::TEAM_API_KEY_ENV),
            get(credential::PREFER_ENV),
            get(credential::TEAM_API_KEY_SECRET).is_some(),
        )
        .unwrap_or_else(|names| {
            errors.extend(names.into_iter().map(str::to_owned));
            Deployment {
                team_api_key: false,
                prefer: credential::Order::PersonalFirst,
            }
        });
        if !errors.is_empty() {
            return Err(InvalidSettings(errors, ca_error));
        }
        let http = get("DAGQ_OIDC_ISSUER")
            .or_else(|| get("DAGQ_ACTIONS_ISSUER"))
            .or_else(|| get("DAGQ_IMAGE_CI_ISSUER"))
            .map(|_| HttpConfig {
                address: address.unwrap(),
                tls,
            });
        Ok(Self {
            store: store.expect("checked store"),
            values,
            http,
            claude,
        })
    }
}
/// Optional Postgres trust bundle. Its value is a PEM file path. If set,
/// only its certificates are trusted (no standard roots are added). Absent,
/// use the standard Mozilla roots. It is invalid for any other store, even
/// when empty, and is validated before opening a queue in every build.
pub const DATABASE_CA_FILE: &str = "DAGQ_DATABASE_CA_FILE";

/// Resolve and validate the bundle with injected file I/O. Errors keep
/// neither file paths nor contents. Also used by the CI connection probe.
pub fn database_ca(
    kind: Option<&str>,
    path: Option<OsString>,
    file: impl Fn(&std::path::Path) -> Result<Vec<u8>, ()>,
) -> Result<Option<Vec<Vec<u8>>>, &'static str> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    let Some(path) = path else {
        return Ok(None);
    };
    if kind != Some("postgres") {
        return Err("requires the postgres store");
    }
    let bytes = file(std::path::Path::new(&path)).map_err(|_| "cannot read PEM file")?;
    let certificates = CertificateDer::pem_slice_iter(&bytes)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "invalid PEM certificate")?;
    if certificates.is_empty() {
        return Err("PEM file contains no certificates");
    }
    let mut roots = rustls::RootCertStore::empty();
    for certificate in &certificates {
        roots
            .add(certificate.clone())
            .map_err(|_| "invalid CA certificate")?;
    }
    Ok(Some(certificates.into_iter().map(|c| c.to_vec()).collect()))
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
