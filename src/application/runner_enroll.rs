//! Runner terminal commands and credential rotation. No secret has Debug or
//! goes into a CLI result. Only register-code deliberately returns its code.
use crate::domain::queue_service::{ServiceResponse, UseCase};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

/// Human commands use the terminal's HTTPS client. Enroll runs on the runner
/// host: no positional secret, secret flag or environment fallback exists.
#[derive(clap::Subcommand, Clone)]
pub enum RunnerCommand {
    /// Print a one-use registration code for manual transfer to the runner.
    RegisterCode,
    /// Exchange a code read from stdin (default) or --file; print no secrets.
    Enroll {
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// List the authenticated person's runners (all for an administrator).
    List,
    /// Revoke a runner as its owner or an administrator.
    Revoke { runner_id: String },
    /// Set the administrator's sharing approval; --remove clears it.
    Share {
        runner_id: String,
        #[arg(long)]
        remove: bool,
    },
}
impl RunnerCommand {
    pub fn request(&self) -> (UseCase, Value) {
        match self {
            Self::RegisterCode => (UseCase::RunnerCode, json!({})),
            Self::Enroll { .. } => (UseCase::RunnerEnroll, json!({})),
            Self::List => (UseCase::RunnerList, json!({})),
            Self::Revoke { runner_id } => (UseCase::RunnerRevoke, json!({"runner_id":runner_id})),
            Self::Share { runner_id, remove } => (
                UseCase::RunnerShare,
                json!({"runner_id":runner_id,"shared": !remove}),
            ),
        }
    }
}
/// No socket/local fallback, even when the local queue has no OIDC issuer.
/// A saved human credential must carry its issuer before human requests send.
pub fn destination<'a>(
    url: Option<&'a str>,
    conflicting_route: bool,
    db_named: bool,
    role: Option<&str>,
) -> Result<&'a str> {
    ensure!(
        role.is_none_or(|r| r == "user"),
        "authorization_denied: runner commands require a person's terminal"
    );
    ensure!(
        !db_named && !conflicting_route,
        "runner commands require only DAGQ_CONTROL_URL; remove --db and socket/broker credentials"
    );
    let url = url.filter(|u| !u.trim().is_empty()).ok_or_else(|| anyhow::anyhow!("runner commands require DAGQ_CONTROL_URL and an OIDC deployment; run dagq login on your terminal"))?;
    crate::application::login::check_url(url)?;
    Ok(url)
}
/// An absent issuer is a local deployment, never a socket fallback.
pub fn human_issuer(issuer: Option<&str>) -> Result<()> {
    let issuer = issuer
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("runner commands require an OIDC issuer; run dagq login"))?;
    crate::application::login::check_url(issuer)
}
/// Stdin is the only default; no environment variable supplies a value.
#[derive(Debug, PartialEq, Eq)]
pub enum CodeSource<'a> {
    Stdin,
    File(&'a std::path::Path),
}
pub fn source(file: Option<&std::path::Path>) -> CodeSource<'_> {
    file.map_or(CodeSource::Stdin, CodeSource::File)
}
pub fn code(bytes: &[u8]) -> Result<&str> {
    let value = std::str::from_utf8(bytes).unwrap_or("").trim();
    ensure!(
        bytes.len() <= 4097
            && !value.is_empty()
            && value.len() <= 4096
            && !value.chars().any(char::is_whitespace),
        "read one registration code from stdin or --file"
    );
    Ok(value)
}
pub const ENROLL_AGAIN: &str =
    "runner credential refused; obtain a new register-code and run dagq runner enroll again";
/// Only the rotating refresh is persisted. Short bearer lives in Session only.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub control_url: String,
    pub runner_id: String,
    pub refresh: String,
}
pub struct Session {
    pub token: String,
    pub expires_at: i64,
}
/// Refresh thirty seconds before expiry, or immediately after a refusal.
pub fn needs_refresh(session: Option<&Session>, now: i64) -> bool {
    session.is_none_or(|s| s.expires_at <= now.saturating_add(30))
}
pub trait RunnerCredentialStore {
    fn load(&self) -> Result<Credential>;
    fn save(&self, value: &Credential) -> Result<()>;
}
pub trait RunnerExchange {
    fn exchange(&self, url: &str, use_case: UseCase, params: Value) -> Result<ServiceResponse>;
}
/// Ignore server-supplied error text, which might echo a submitted secret.
/// Version refusal precedes parsing and saving any returned credentials.
/// Only unauthenticated (including revocation) requires enrollment again.
/// Other codes are reported without server text or an automatic retry: a
/// transport failure may have committed a rotation already.
pub fn answer(response: ServiceResponse) -> Result<Value> {
    if !crate::domain::queue_service::understands(&response)
        || response.error.as_ref().is_some_and(|e| {
            e.code == crate::domain::queue_service::ServiceErrorCode::ApiVersionMismatch
        })
    {
        return Err(crate::application::login::ApiVersionMismatch {
            minimum: response.min_api_version.into(),
            maximum: response.api_version.into(),
        }
        .into());
    }
    if let Some(error) = response.error {
        if error.code == crate::domain::queue_service::ServiceErrorCode::Unauthenticated {
            anyhow::bail!(ENROLL_AGAIN);
        }
        anyhow::bail!(
            "runner exchange failed ({}); retry later",
            error.code.as_str()
        );
    }
    ensure!(
        response.ok,
        "runner exchange failed (invalid_response); retry later"
    );
    response
        .result
        .ok_or_else(|| anyhow::anyhow!("runner exchange failed (invalid_response); retry later"))
}
fn install(
    store: &dyn RunnerCredentialStore,
    url: &str,
    response: ServiceResponse,
) -> Result<Session> {
    let value = answer(response)?;
    let text = |name| {
        value
            .get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| anyhow::anyhow!("invalid runner exchange response"))
    };
    let credential = Credential {
        control_url: url.into(),
        runner_id: text("runner_id")?,
        refresh: text("refresh")?,
    };
    let session = Session {
        token: text("token")?,
        expires_at: value
            .get("expires_at")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow::anyhow!("invalid runner exchange response"))?,
    };
    store.save(&credential)?;
    Ok(session)
}
pub fn enroll(
    http: &dyn RunnerExchange,
    store: &dyn RunnerCredentialStore,
    url: &str,
    bytes: &[u8],
) -> Result<Session> {
    let response = http.exchange(url, UseCase::RunnerEnroll, json!({"code":code(bytes)?}))?;
    install(store, url, response)
}
/// Called by the runner process before expiry or after an authentication refusal
/// (pass None then). Never retries a rotation: reuse revokes the credential.
/// A failed write leaves an intact old credential; if already rotated remotely,
/// it needs manual enrollment. No short token is returned until saving succeeds.
pub fn refresh(
    http: &dyn RunnerExchange,
    store: &dyn RunnerCredentialStore,
    session: Option<Session>,
    now: i64,
) -> Result<Session> {
    if !needs_refresh(session.as_ref(), now) {
        return Ok(session.unwrap());
    }
    let old = store.load()?;
    let response = http.exchange(
        &old.control_url,
        UseCase::RunnerRefresh,
        json!({"refresh": old.refresh}),
    )?;
    install(store, &old.control_url, response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    #[derive(clap::Parser)]
    struct Arguments {
        #[command(subcommand)]
        command: RunnerCommand,
    }
    #[test]
    fn arguments_reject_secret_values_and_select_only_stdin_or_file() {
        use clap::Parser;
        for args in [
            vec!["runner", "enroll", "secret"],
            vec!["runner", "enroll", "--code", "secret"],
            vec!["runner", "enroll", "--token", "secret"],
            vec!["runner", "enroll", "--env", "CODE"],
            vec!["runner", "share"],
            vec!["runner", "list", "extra"],
        ] {
            assert!(Arguments::try_parse_from(args).is_err());
        }
        assert!(matches!(
            Arguments::try_parse_from(["runner", "enroll"])
                .unwrap()
                .command,
            RunnerCommand::Enroll { file: None }
        ));
        assert!(matches!(
            Arguments::try_parse_from(["runner", "enroll", "--file", "input"])
                .unwrap()
                .command,
            RunnerCommand::Enroll { file: Some(_) }
        ));
        for (args, case) in [
            (vec!["runner", "register-code"], UseCase::RunnerCode),
            (vec!["runner", "list"], UseCase::RunnerList),
            (vec!["runner", "revoke", "r"], UseCase::RunnerRevoke),
            (vec!["runner", "share", "r"], UseCase::RunnerShare),
        ] {
            assert_eq!(
                Arguments::try_parse_from(args).unwrap().command.request().0,
                case
            );
        }
        assert_eq!(
            Arguments::try_parse_from(["runner", "share", "r", "--remove"])
                .unwrap()
                .command
                .request()
                .1["shared"],
            false
        );
    }
    struct Store(RefCell<Option<Credential>>, bool);
    impl RunnerCredentialStore for Store {
        fn load(&self) -> Result<Credential> {
            let value = self.0.borrow();
            let value = value.as_ref().unwrap();
            Ok(Credential {
                control_url: value.control_url.clone(),
                runner_id: value.runner_id.clone(),
                refresh: value.refresh.clone(),
            })
        }
        fn save(&self, value: &Credential) -> Result<()> {
            ensure!(!self.1, "write failure");
            *self.0.borrow_mut() = Some(Credential {
                control_url: value.control_url.clone(),
                runner_id: value.runner_id.clone(),
                refresh: value.refresh.clone(),
            });
            Ok(())
        }
    }
    struct Http(bool, bool);
    impl RunnerExchange for Http {
        fn exchange(&self, url: &str, case: UseCase, params: Value) -> Result<ServiceResponse> {
            assert_eq!(url, "https://control");
            assert!(matches!(
                case,
                UseCase::RunnerEnroll | UseCase::RunnerRefresh
            ));
            assert!(
                params
                    .get(if case == UseCase::RunnerEnroll {
                        "code"
                    } else {
                        "refresh"
                    })
                    .is_some()
            );
            let mut response = if self.0 {
                ServiceResponse::success(
                    json!({"runner_id":"r", "refresh":"new-refresh", "token":"short-secret", "expires_at":200}),
                )
            } else {
                ServiceResponse::failure(
                    crate::domain::queue_service::ServiceErrorCode::Unauthenticated,
                    "echoed-secret",
                )
            };
            if self.1 {
                response.min_api_version = API_VERSION + 1;
                response.api_version = API_VERSION + 1;
            }
            Ok(response)
        }
    }
    use crate::domain::queue_service::API_VERSION;
    #[test]
    fn routes_refuse_before_any_io_and_guides_name_https_and_oidc() {
        assert!(destination(Some("https://control"), false, false, None).is_ok());
        for (url, route, db, role) in [
            (None, false, false, None),
            (Some("https://control"), true, false, None),
            (Some("https://control"), false, true, None),
            (Some("https://control"), false, false, Some("worker")),
            (Some("http://evil"), false, false, None),
        ] {
            assert!(destination(url, route, db, role).is_err());
        }
        let guide = destination(None, false, false, None)
            .unwrap_err()
            .to_string();
        assert!(guide.contains("DAGQ_CONTROL_URL") && guide.contains("OIDC"));
        for issuer in [None, Some(""), Some("http://evil")] {
            assert!(human_issuer(issuer).is_err());
        }
        assert!(human_issuer(Some("https://issuer")).is_ok());
        for role in crate::domain::ActorRole::ALL {
            assert_eq!(
                destination(Some("https://control"), false, false, Some(role.as_str())).is_ok(),
                role == crate::domain::ActorRole::User
            );
        }
    }
    #[test]
    fn input_is_one_bounded_code_from_stdin_or_the_named_file() {
        assert_eq!(source(None), CodeSource::Stdin);
        let path = std::path::Path::new("input");
        assert_eq!(source(Some(path)), CodeSource::File(path));
        assert_eq!(code(b" value\n").unwrap(), "value");
        for bytes in [
            b"".as_slice(),
            b"one two",
            b"\xff",
            &vec![b'x'; 4097],
            &vec![b' '; 4098],
        ] {
            assert!(code(bytes).is_err());
        }
    }
    #[test]
    fn enroll_saves_only_refresh_and_never_saves_a_refusal_or_version_mismatch() {
        for http in [Http(false, false), Http(true, true)] {
            let store = Store(RefCell::new(None), false);
            let error = enroll(&http, &store, "https://control", b"code")
                .err()
                .unwrap()
                .to_string();
            assert!(store.0.borrow().is_none());
            assert!(!error.contains("echoed-secret"));
        }
        let store = Store(RefCell::new(None), false);
        let session = enroll(&Http(true, false), &store, "https://control", b"code").unwrap();
        assert_eq!(session.token, "short-secret");
        let saved = serde_json::to_string(store.0.borrow().as_ref().unwrap()).unwrap();
        assert!(saved.contains("new-refresh"));
        assert!(!saved.contains("short-secret") && !saved.contains("code"));
    }
    fn old() -> Credential {
        Credential {
            control_url: "https://control".into(),
            runner_id: "r".into(),
            refresh: "old-refresh".into(),
        }
    }
    #[test]
    fn refresh_replaces_only_on_success_and_revocation_requests_enrollment() {
        let store = Store(RefCell::new(Some(old())), false);
        refresh(&Http(true, false), &store, None, 0).unwrap();
        assert_eq!(store.0.borrow().as_ref().unwrap().refresh, "new-refresh");
        let store = Store(RefCell::new(Some(old())), true);
        assert!(refresh(&Http(true, false), &store, None, 0).is_err());
        assert_eq!(store.0.borrow().as_ref().unwrap().refresh, "old-refresh");
        let error = refresh(&Http(false, false), &store, None, 0)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(error, ENROLL_AGAIN);
        assert!(needs_refresh(None, 0));
        let session = Session {
            token: "short-secret".into(),
            expires_at: 100,
        };
        assert!(!needs_refresh(Some(&session), 69));
        assert!(needs_refresh(Some(&session), 70));
        assert_eq!(
            refresh(&Http(false, false), &store, Some(session), 0)
                .unwrap()
                .token,
            "short-secret"
        );
    }
    #[test]
    fn only_authentication_refusals_require_enrollment_again() {
        use crate::domain::queue_service::ServiceErrorCode as Code;
        struct Refused(Code);
        impl RunnerExchange for Refused {
            fn exchange(&self, _: &str, _: UseCase, _: Value) -> Result<ServiceResponse> {
                Ok(ServiceResponse::failure(self.0, "echoed-refresh-secret"))
            }
        }
        for code in [
            Code::Unauthenticated,
            Code::NotLeader,
            Code::Failed,
            Code::BadRequest,
            Code::Busy,
            Code::AuthorizationDenied,
        ] {
            let http = Refused(code);
            let fresh = Store(RefCell::new(None), false);
            let enrolled = enroll(&http, &fresh, "https://control", b"code")
                .err()
                .unwrap()
                .to_string();
            assert!(fresh.0.borrow().is_none());
            let existing = Store(RefCell::new(Some(old())), false);
            let renewed = refresh(&http, &existing, None, 0)
                .err()
                .unwrap()
                .to_string();
            assert_eq!(existing.0.borrow().as_ref().unwrap().refresh, "old-refresh");
            for error in [enrolled, renewed] {
                assert!(!error.contains("echoed-refresh-secret"));
                if code == Code::Unauthenticated {
                    assert_eq!(error, ENROLL_AGAIN);
                } else {
                    assert!(error.contains(code.as_str()) && error.contains("retry later"));
                    assert!(!error.contains("enroll") && !error.contains("register-code"));
                }
            }
        }
        let mut invalid = ServiceResponse::success(json!({}));
        invalid.ok = false;
        assert!(
            answer(invalid)
                .unwrap_err()
                .to_string()
                .contains("invalid_response")
        );
    }
}
