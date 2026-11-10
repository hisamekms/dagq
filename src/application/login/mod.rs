//! Human CLI login and credential renewal. Tokens are opaque here: only the
//! exchange response supplies identity and expiry. No JWT claims are decoded.
use anyhow::{Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// One terminal has one credential set, including its destination. Successful
/// login replaces it; failures leave it intact. Never derive Debug for secrets.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub destination: Destination,
    pub token: String,
    pub refresh_token: Option<String>,
    pub identity: Identity,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Destination {
    pub control_url: String,
    pub issuer: String,
    /// Public OIDC client ID. Deployment DAGQ_OIDC_AUDIENCE must equal this
    /// value: the exchanged ID token's audience contains this client ID.
    pub client_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub issuer: String,
    pub subject: String,
    pub role: String,
    pub expires_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}
/// Each flag takes precedence over its named environment variable. Login never
/// fills missing destination values from previously stored credentials.
pub fn destination(
    flags: [Option<String>; 3],
    env: impl Fn(&str) -> Option<String>,
) -> Result<Destination> {
    let names = [
        crate::domain::queue_service::CONTROL_URL_ENV,
        "DAGQ_LOGIN_ISSUER",
        "DAGQ_LOGIN_CLIENT_ID",
    ];
    let values: Vec<_> = flags
        .into_iter()
        .zip(names)
        .map(|(flag, name)| flag.or_else(|| env(name)).filter(|v| !v.trim().is_empty()))
        .collect();
    let missing: Vec<_> = values
        .iter()
        .zip(names)
        .filter_map(|(v, n)| v.is_none().then_some(n))
        .collect();
    ensure!(
        missing.is_empty(),
        "login requires {} (or their flags)",
        missing.join(", ")
    );
    let mut values = values.into_iter().map(Option::unwrap);
    let dest = Destination {
        control_url: values.next().unwrap(),
        issuer: values.next().unwrap(),
        client_id: values.next().unwrap(),
    };
    check_url(&dest.control_url)?;
    check_url(&dest.issuer)?;
    Ok(dest)
}
/// HTTPS or numeric loopback HTTP only; names such as localhost cannot
/// establish where the resolver will connect. No userinfo or fragments; a
/// loopback exception belongs to the human CLI, never the server verifier.
pub fn check_url(url: &str) -> Result<()> {
    let uri: ureq::http::Uri = url
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid login endpoint URL"))?;
    let authority = uri.authority().map(|a| a.as_str()).unwrap_or("");
    let host = uri.host().unwrap_or("").trim_matches(['[', ']']);
    let loopback = host
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback());
    ensure!(
        !host.is_empty()
            && !authority.contains('@')
            && !url.contains('#')
            && (uri.scheme_str() == Some("https")
                || (uri.scheme_str() == Some("http") && loopback)),
        "login endpoints require HTTPS (HTTP is allowed only on numeric loopback addresses)"
    );
    Ok(())
}
#[derive(Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub token_endpoint: String,
    pub device_authorization_endpoint: Option<String>,
    pub authorization_endpoint: Option<String>,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Flow {
    Device,
    Browser,
}
fn discovery_endpoints(expected: &str, body: Value) -> Result<Discovery> {
    let d: Discovery =
        serde_json::from_value(body).map_err(|_| anyhow::anyhow!("invalid login discovery"))?;
    crate::domain::oidc::check_discovery_issuer(expected, &d.issuer)?;
    check_url(&d.token_endpoint)?;
    for endpoint in [&d.device_authorization_endpoint, &d.authorization_endpoint]
        .into_iter()
        .flatten()
    {
        check_url(endpoint)?;
    }
    Ok(d)
}
pub fn discovery(expected: &str, body: Value, browser: bool) -> Result<(Discovery, Flow)> {
    let d = discovery_endpoints(expected, body)?;
    let flow = if browser || d.device_authorization_endpoint.is_none() {
        ensure!(
            d.authorization_endpoint.is_some(),
            "discovery has no authorization endpoint"
        );
        Flow::Browser
    } else {
        Flow::Device
    };
    Ok((d, flow))
}
/// OIDC openid is mandatory. Default offline_access is sent only when the
/// issuer advertises it; caller-supplied scopes are whitespace separated.
pub fn scopes(flag: Option<&str>, environment: Option<&str>, supported: &[String]) -> String {
    let mut scopes: Vec<&str> = flag
        .or(environment)
        .map(|s| s.split_whitespace().collect())
        .unwrap_or_else(|| {
            if supported.iter().any(|s| s == "offline_access") {
                vec!["offline_access"]
            } else {
                vec![]
            }
        });
    if !scopes.contains(&"openid") {
        scopes.insert(0, "openid");
    }
    scopes.join(" ")
}
pub type Params = Vec<(String, String)>;
pub fn auth_params(given: &[String]) -> Result<Params> {
    let reserved = [
        "client_id",
        "scope",
        "response_type",
        "redirect_uri",
        "state",
        "code_challenge",
        "code_challenge_method",
        "max_age",
        "client_secret",
    ];
    given
        .iter()
        .map(|p| {
            let (name, value) = p
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("auth-param requires NAME=VALUE"))?;
            ensure!(
                !name.is_empty() && !reserved.contains(&name),
                "auth-param name is reserved or empty"
            );
            Ok((name.into(), value.into()))
        })
        .collect()
}
/// RFC 8628: initial interval defaults to five seconds; slow_down adds five
/// seconds for all subsequent polls. Every other OAuth error stops polling.
#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    Wait(u64),
    Success,
    Stop,
}
pub fn poll(answer: &Value, interval: u64) -> Poll {
    match answer.get("error") {
        Some(Value::String(error)) if error == "authorization_pending" => Poll::Wait(interval),
        Some(Value::String(error)) if error == "slow_down" => {
            Poll::Wait(interval.saturating_add(5))
        }
        Some(_) => Poll::Stop,
        None => Poll::Success,
    }
}
/// The browser callback binds a numeric loopback address on an ephemeral port.
pub fn callback_address() -> std::net::SocketAddr {
    (std::net::Ipv4Addr::LOCALHOST, 0).into()
}
pub fn pkce(entropy: &[u8; 32]) -> (String, String) {
    let verifier = URL_SAFE_NO_PAD.encode(entropy);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}
/// Validate before using any returned code. Repeated parameters are refused.
pub fn redirect(params: &Params, expected_state: &str) -> Result<String> {
    let one = |name| {
        let values: Vec<_> = params
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .collect();
        if values.len() == 1 {
            Some(values[0])
        } else {
            None
        }
    };
    ensure!(
        one("state") == Some(expected_state),
        "login redirect state mismatch"
    );
    ensure!(
        !params.iter().any(|(k, _)| k == "error"),
        "login authorization refused"
    );
    one("code")
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("login redirect has no code"))
}
/// No signature check is done in the CLI: the value came directly from the
/// token endpoint and the control side verifies it. Access tokens never pass.
pub fn id_token(answer: &Value) -> Result<&str> {
    ensure!(
        answer.get("error").is_none(),
        "issuer token request refused; run dagq login"
    );
    answer.get("id_token").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or_else(|| anyhow::anyhow!("no id_token returned; run dagq login with an OIDC scope (openid is required); check the issuer's scope configuration"))
}
/// Refresh may omit a new refresh token, retaining the previous credential,
/// but must supply a new ID token. An access token never substitutes for it.
pub fn refresh_response<'a>(answer: &'a Value, previous: &str) -> Result<(&'a str, String)> {
    ensure!(
        answer.get("error").is_none(),
        "issuer refresh refused; run dagq login"
    );
    Ok((
        id_token(answer)?,
        answer["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or(previous)
            .to_owned(),
    ))
}
pub const LOGIN_AGAIN: &str =
    "credentials unavailable or refused; run dagq login on your own terminal";

/// An OS backend absent on this platform, rather than a locked or failing
/// backend that might still hold credentials. Auto logout can skip only this
/// absence; explicitly selecting os returns the error for every operation.
#[derive(Debug)]
pub struct OsStoreUnavailable;
impl std::fmt::Display for OsStoreUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OS credential store unsupported; use DAGQ_CREDENTIAL_STORE=file")
    }
}
impl std::error::Error for OsStoreUnavailable {}
pub trait CredentialStore {
    fn load(&self) -> Result<Option<Credentials>>;
    fn save(&self, credentials: &Credentials) -> Result<()>;
    fn delete(&self) -> Result<()>;
}
/// Bounded synchronous JSON transport; form replies may contain OAuth errors.
pub trait LoginHttp {
    fn get(&self, url: &str) -> Result<Value>;
    fn form(&self, url: &str, params: &Params) -> Result<Value>;
    fn exchange(&self, control: &str, id_token: &str) -> Result<Value>;
}
pub trait LoginInteraction {
    fn now(&self) -> i64;
    fn sleep(&self, seconds: u64);
    fn entropy(&self) -> Result<[u8; 32]>;
    fn device(&self, verification_uri: &str, user_code: &str) -> Result<()>;
    /// Bind a loopback listener before composing the URL; add redirect_uri,
    /// open the browser, then validate the response using [`redirect`].
    fn authorize(&self, endpoint: &str, params: &Params, state: &str) -> Result<(String, String)>;
}
pub struct LoginOptions {
    pub destination: Destination,
    pub browser: bool,
    pub scope: Option<String>,
    pub auth_params: Params,
}
/// Compatibility refusal from the control side; only numeric version bounds
/// are retained, never arbitrary response data or credential values.
#[derive(Debug)]
pub struct ApiVersionMismatch {
    pub minimum: u64,
    pub maximum: u64,
}
impl std::fmt::Display for ApiVersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "api_version_mismatch: control side requires API versions {}..{}; install a compatible dagq binary",
            self.minimum, self.maximum
        )
    }
}
impl std::error::Error for ApiVersionMismatch {}
fn received(dest: &Destination, answer: Value, refresh: Option<String>) -> Result<Credentials> {
    // Response from the versioned exchange use case; never print its token or
    // arbitrary server error text. Version mismatch alone has a fixed guide.
    if answer["error"]["code"] == "api_version_mismatch" {
        let number = |name| {
            answer[name].as_u64().ok_or_else(|| {
                anyhow::anyhow!(
                    "invalid exchange API version range; install a compatible dagq binary"
                )
            })
        };
        return Err(ApiVersionMismatch {
            minimum: number("min_api_version")?,
            maximum: number("api_version")?,
        }
        .into());
    }
    ensure!(
        answer["ok"] == true,
        "token exchange refused; {LOGIN_AGAIN}"
    );
    let value = &answer["result"];
    let token = value["token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("invalid token exchange response"))?;
    let identity: Identity = serde_json::from_value(value.clone())
        .map_err(|_| anyhow::anyhow!("invalid token exchange identity"))?;
    Ok(Credentials {
        destination: dest.clone(),
        token: token.into(),
        refresh_token: refresh,
        identity,
    })
}
pub fn login(
    options: &LoginOptions,
    http: &dyn LoginHttp,
    store: &dyn CredentialStore,
    interaction: &dyn LoginInteraction,
) -> Result<Identity> {
    let dest = &options.destination;
    check_url(&dest.control_url)?;
    check_url(&dest.issuer)?;
    let (d, flow) = discovery(
        &dest.issuer,
        http.get(&crate::domain::oidc::discovery_url(&dest.issuer))?,
        options.browser,
    )?;
    let mut params = options.auth_params.clone();
    params.extend([
        ("client_id".into(), dest.client_id.clone()),
        (
            "scope".into(),
            scopes(options.scope.as_deref(), None, &d.scopes_supported),
        ),
    ]);
    let answer = match flow {
        Flow::Device => {
            let start = http.form(d.device_authorization_endpoint.as_deref().unwrap(), &params)?;
            let text = |key| {
                start[key]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid device authorization response"))
            };
            let verification = text("verification_uri")?;
            check_url(verification)?;
            interaction.device(verification, text("user_code")?)?;
            let code = text("device_code")?;
            let mut interval = start["interval"].as_u64().unwrap_or(5).max(1);
            let expires = start["expires_in"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("device authorization has no expiry"))?;
            let end = interaction
                .now()
                .saturating_add(i64::try_from(expires).unwrap_or(i64::MAX));
            loop {
                ensure!(
                    interaction
                        .now()
                        .saturating_add(i64::try_from(interval).unwrap_or(i64::MAX))
                        < end,
                    "device authorization expired"
                );
                interaction.sleep(interval);
                let answer = http.form(
                    &d.token_endpoint,
                    &vec![
                        (
                            "grant_type".into(),
                            "urn:ietf:params:oauth:grant-type:device_code".into(),
                        ),
                        ("device_code".into(), code.into()),
                        ("client_id".into(), dest.client_id.clone()),
                    ],
                )?;
                match poll(&answer, interval) {
                    Poll::Wait(next) => interval = next,
                    Poll::Success => break answer,
                    Poll::Stop => bail!("device authorization refused or expired"),
                }
            }
        }
        Flow::Browser => {
            let (verifier, challenge) = pkce(&interaction.entropy()?);
            let state = URL_SAFE_NO_PAD.encode(interaction.entropy()?);
            params.extend([
                ("response_type".into(), "code".into()),
                ("state".into(), state.clone()),
                ("code_challenge".into(), challenge),
                ("code_challenge_method".into(), "S256".into()),
            ]);
            let (code, redirect_uri) = interaction.authorize(
                d.authorization_endpoint.as_deref().unwrap(),
                &params,
                &state,
            )?;
            http.form(
                &d.token_endpoint,
                &vec![
                    ("grant_type".into(), "authorization_code".into()),
                    ("client_id".into(), dest.client_id.clone()),
                    ("code".into(), code),
                    ("redirect_uri".into(), redirect_uri),
                    ("code_verifier".into(), verifier),
                ],
            )?
        }
    };
    let jwt = id_token(&answer)?;
    let credentials = received(
        dest,
        http.exchange(&dest.control_url, jwt)?,
        answer["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
    )?;
    store.save(&credentials)?;
    Ok(credentials.identity)
}
/// The caller reports unauthenticated once, and reports rejection of the retry
/// separately. This function never resends a request or loops over renewal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejected {
    No,
    First,
    AfterRenewal,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Renewal {
    Keep,
    Refresh,
    Login,
}
pub fn renewal(credentials: &Credentials, control: &str, now: i64, rejected: Rejected) -> Renewal {
    if control.trim_end_matches('/') != credentials.destination.control_url.trim_end_matches('/')
        || rejected == Rejected::AfterRenewal
    {
        return Renewal::Login;
    }
    if rejected == Rejected::No && now < credentials.identity.expires_at {
        Renewal::Keep
    } else if credentials.refresh_token.is_some() {
        Renewal::Refresh
    } else {
        Renewal::Login
    }
}
pub fn read_token(
    control: &str,
    rejected: Rejected,
    now: i64,
    http: &dyn LoginHttp,
    store: &dyn CredentialStore,
) -> Result<Credentials> {
    let old = store.load()?.ok_or_else(|| anyhow::anyhow!(LOGIN_AGAIN))?;
    match renewal(&old, control, now, rejected) {
        Renewal::Keep => Ok(old),
        Renewal::Login => bail!(LOGIN_AGAIN),
        Renewal::Refresh => {
            let attempt = || -> Result<Credentials> {
                let dest = &old.destination;
                check_url(&dest.control_url)?;
                check_url(&dest.issuer)?;
                let d = discovery_endpoints(
                    &dest.issuer,
                    http.get(&crate::domain::oidc::discovery_url(&dest.issuer))?,
                )?;
                let answer = http.form(
                    &d.token_endpoint,
                    &vec![
                        ("grant_type".into(), "refresh_token".into()),
                        ("refresh_token".into(), old.refresh_token.clone().unwrap()),
                        ("client_id".into(), dest.client_id.clone()),
                    ],
                )?;
                let (jwt, refresh) =
                    refresh_response(&answer, old.refresh_token.as_deref().unwrap())?;
                let fresh = received(dest, http.exchange(&dest.control_url, jwt)?, Some(refresh))?;
                store.save(&fresh)?;
                Ok(fresh)
            };
            attempt().map_err(|_| anyhow::anyhow!(LOGIN_AGAIN))
        }
    }
}
pub fn whoami(http: &dyn LoginHttp, store: &dyn CredentialStore, now: i64) -> Result<Value> {
    let stored = store.load()?.ok_or_else(|| anyhow::anyhow!(LOGIN_AGAIN))?;
    let credentials = read_token(
        &stored.destination.control_url,
        Rejected::No,
        now,
        http,
        store,
    )?;
    let mut shown = serde_json::to_value(credentials.identity)?;
    shown["control_url"] = json!(credentials.destination.control_url);
    Ok(shown)
}

/// DAGQ_CREDENTIAL_STORE chooses one shared backend for login, whoami, logout
/// and request clients. Auto reads OS then file, writes OS with fallback and
/// deletes the older file after an OS write. After a file fallback succeeds,
/// it tries to delete an older OS entry so the next read finds the new set.
/// Logout removes both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreChoice {
    Auto,
    Os,
    File,
}
impl StoreChoice {
    pub fn parse(value: Option<&str>) -> Result<Self> {
        match value {
            None => Ok(Self::Auto),
            Some("os") => Ok(Self::Os),
            Some("file") => Ok(Self::File),
            _ => bail!("DAGQ_CREDENTIAL_STORE must be os or file"),
        }
    }
}
pub struct Stores<'a> {
    pub choice: StoreChoice,
    pub os: &'a dyn CredentialStore,
    pub file: &'a dyn CredentialStore,
}
impl CredentialStore for Stores<'_> {
    fn load(&self) -> Result<Option<Credentials>> {
        match self.choice {
            StoreChoice::File => self.file.load(),
            StoreChoice::Os => self.os.load(),
            StoreChoice::Auto => match self.os.load() {
                Ok(Some(c)) => Ok(Some(c)),
                _ => self.file.load(),
            },
        }
    }
    fn save(&self, credentials: &Credentials) -> Result<()> {
        match self.choice {
            StoreChoice::File => self.file.save(credentials),
            StoreChoice::Os => self.os.save(credentials),
            StoreChoice::Auto => match self.os.save(credentials) {
                Ok(()) => self.file.delete(),
                Err(_) => {
                    self.file.save(credentials)?;
                    // A failed OS write may still leave a readable older set.
                    // Cleanup is best-effort when that store is unavailable.
                    let _ = self.os.delete();
                    Ok(())
                }
            },
        }
    }
    fn delete(&self) -> Result<()> {
        match self.choice {
            StoreChoice::File => self.file.delete(),
            StoreChoice::Os => self.os.delete(),
            StoreChoice::Auto => {
                let os = self.os.delete();
                let file = self.file.delete();
                file?;
                match os {
                    Err(error) if error.is::<OsStoreUnavailable>() => Ok(()),
                    result => result,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
