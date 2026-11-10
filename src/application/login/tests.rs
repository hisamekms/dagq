use super::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

fn dest() -> Destination {
    destination(
        [
            Some("http://127.0.0.1:1234".into()),
            Some("https://idp.example".into()),
            Some("cli".into()),
        ],
        |_| None,
    )
    .unwrap()
}
fn saved() -> Credentials {
    Credentials {
        destination: dest(),
        token: "old-dagq-secret".into(),
        refresh_token: Some("old-refresh-secret".into()),
        identity: Identity {
            issuer: "issuer".into(),
            subject: "subject".into(),
            role: "member".into(),
            expires_at: 100,
            display_name: None,
        },
    }
}
#[derive(Default)]
struct Store {
    value: RefCell<Option<Credentials>>,
    calls: RefCell<Vec<&'static str>>,
    fails: Cell<bool>,
    save_fails: Cell<bool>,
}
impl CredentialStore for Store {
    fn load(&self) -> Result<Option<Credentials>> {
        self.calls.borrow_mut().push("load");
        ensure!(!self.fails.get(), "unavailable");
        Ok(self.value.borrow().clone())
    }
    fn save(&self, c: &Credentials) -> Result<()> {
        self.calls.borrow_mut().push("save");
        ensure!(!self.fails.get() && !self.save_fails.get(), "unavailable");
        *self.value.borrow_mut() = Some(c.clone());
        Ok(())
    }
    fn delete(&self) -> Result<()> {
        self.calls.borrow_mut().push("delete");
        ensure!(!self.fails.get(), "unavailable");
        *self.value.borrow_mut() = None;
        Ok(())
    }
}
struct Http {
    replies: RefCell<VecDeque<Value>>,
    sent: RefCell<Vec<(String, Value)>>,
}
impl Http {
    fn new(replies: Vec<Value>) -> Self {
        Self {
            replies: RefCell::new(replies.into()),
            sent: RefCell::new(vec![]),
        }
    }
    fn next(&self, url: &str, value: Value) -> Result<Value> {
        self.sent.borrow_mut().push((url.into(), value));
        self.replies
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("unexpected request"))
    }
}
impl LoginHttp for Http {
    fn get(&self, url: &str) -> Result<Value> {
        self.next(url, Value::Null)
    }
    fn form(&self, url: &str, p: &Params) -> Result<Value> {
        self.next(url, serde_json::to_value(p).unwrap())
    }
    fn exchange(&self, control: &str, jwt: &str) -> Result<Value> {
        self.next(control, json!({"jwt":jwt}))
    }
}
struct Interaction {
    now: Cell<i64>,
    waits: RefCell<Vec<u64>>,
    browser: Cell<bool>,
}
impl Interaction {
    fn new() -> Self {
        Self {
            now: Cell::new(0),
            waits: RefCell::new(vec![]),
            browser: Cell::new(false),
        }
    }
}
impl LoginInteraction for Interaction {
    fn now(&self) -> i64 {
        self.now.get()
    }
    fn sleep(&self, s: u64) {
        self.waits.borrow_mut().push(s);
        self.now.set(self.now.get() + s as i64);
    }
    fn entropy(&self) -> Result<[u8; 32]> {
        Ok([1; 32])
    }
    fn device(&self, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
    fn authorize(&self, _: &str, p: &Params, state: &str) -> Result<(String, String)> {
        assert!(p.contains(&(
            "max_age".into(),
            crate::domain::queue_service::APPROVAL_LOGIN_SECONDS.to_string()
        )));
        assert!(p.contains(&("code_challenge_method".into(), "S256".into())));
        assert!(p.contains(&("state".into(), state.into())));
        self.browser.set(true);
        Ok((
            "code-secret".into(),
            "http://127.0.0.1:4444/callback".into(),
        ))
    }
}
fn doc(device: bool) -> Value {
    let mut d = json!({"issuer":"https://idp.example","token_endpoint":"https://idp.example/token","authorization_endpoint":"https://idp.example/authorize","scopes_supported":["openid","offline_access"],"jwks_uri":"this is intentionally ignored"});
    if device {
        d["device_authorization_endpoint"] = json!("https://idp.example/device");
    }
    d
}
fn exchange() -> Value {
    json!({"ok":true,"result":{"token":"new-dagq-secret","issuer":"returned-issuer","subject":"returned-subject","role":"admin","expires_at":200,"on_call":true},"api_version":2,"min_api_version":2})
}
fn options() -> LoginOptions {
    LoginOptions {
        destination: dest(),
        browser: false,
        scope: None,
        auth_params: vec![],
    }
}
fn start() -> Value {
    json!({"device_code":"device-secret","user_code":"USER","verification_uri":"https://idp.example/verify","expires_in":600})
}

#[test]
fn destinations_require_each_input_and_flags_win_without_a_store() {
    let missing = destination([None, None, None], |_| None)
        .unwrap_err()
        .to_string();
    for name in [
        "DAGQ_CONTROL_URL",
        "DAGQ_LOGIN_ISSUER",
        "DAGQ_LOGIN_CLIENT_ID",
    ] {
        assert!(missing.contains(name));
    }
    let d = destination([None, Some("https://flag".into()), None], |name| {
        Some(
            match name {
                "DAGQ_CONTROL_URL" => "https://control",
                "DAGQ_LOGIN_ISSUER" => "https://environment",
                _ => "environment-client",
            }
            .into(),
        )
    })
    .unwrap();
    assert_eq!(d.issuer, "https://flag");
    assert_eq!(d.client_id, "environment-client");
    for url in [
        "http://external.example",
        "http://localhost",
        "http://localhost:8080",
        "http://localhost.localdomain",
        "http://127.0.0.1.evil",
        "https://user:secret@host",
        "https://",
        "https://host/#fragment",
    ] {
        assert!(check_url(url).is_err(), "{url}");
    }
    for url in [
        "https://host",
        "http://127.0.0.1:42",
        "http://[::1]:42",
        "http://127.0.0.2:42",
        "https://localhost",
    ] {
        assert!(check_url(url).is_ok(), "{url}");
    }
}
#[test]
fn discovery_matches_exact_issuer_ignores_jwks_and_selects_the_flow() {
    for (device, browser, want) in [
        (true, false, Flow::Device),
        (false, false, Flow::Browser),
        (true, true, Flow::Browser),
    ] {
        assert_eq!(
            discovery("https://idp.example", doc(device), browser)
                .unwrap()
                .1,
            want
        );
    }
    assert!(discovery("https://idp.example/", doc(true), false).is_err());
    for endpoint in [
        "token_endpoint",
        "authorization_endpoint",
        "device_authorization_endpoint",
    ] {
        let mut d = doc(true);
        d[endpoint] = json!("http://external");
        assert!(discovery("https://idp.example", d, false).is_err());
    }
    let mut d = doc(false);
    d["authorization_endpoint"] = Value::Null;
    assert!(discovery("https://idp.example", d, false).is_err());
}
#[test]
fn scope_precedence_and_additional_parameters_preserve_public_client_fields() {
    assert_eq!(scopes(None, None, &[]), "openid");
    assert_eq!(
        scopes(None, None, &["offline_access".into()]),
        "openid offline_access"
    );
    assert_eq!(
        scopes(None, Some("email profile"), &[]),
        "openid email profile"
    );
    assert_eq!(
        scopes(Some("openid profile"), Some("email"), &[]),
        "openid profile"
    );
    assert_eq!(
        auth_params(&["audience=a=b".into(), "access_type=offline".into()]).unwrap(),
        vec![
            ("audience".into(), "a=b".into()),
            ("access_type".into(), "offline".into())
        ]
    );
    for key in [
        "client_id",
        "scope",
        "response_type",
        "redirect_uri",
        "state",
        "code_challenge",
        "code_challenge_method",
        "max_age",
        "client_secret",
        "",
    ] {
        assert!(auth_params(&[format!("{key}=bad")]).is_err());
    }
    assert!(auth_params(&["malformed".into()]).is_err());
}
#[test]
fn device_poll_waits_and_slows_and_stops_on_every_other_error() {
    assert_eq!(
        poll(&json!({"error":"authorization_pending"}), 5),
        Poll::Wait(5)
    );
    assert_eq!(poll(&json!({"error":"slow_down"}), 5), Poll::Wait(10));
    for error in [
        "expired_token",
        "access_denied",
        "authorization_declined",
        "unknown",
    ] {
        assert_eq!(poll(&json!({"error":error}), 5), Poll::Stop);
    }
    assert_eq!(poll(&json!({"id_token":"opaque"}), 5), Poll::Success);
    let malformed_error = json!({"error":42,"id_token":"must-not-exchange"});
    assert_eq!(poll(&malformed_error, 5), Poll::Stop);
    assert!(id_token(&malformed_error).is_err());
    let http = Http::new(vec![
        doc(true),
        start(),
        json!({"error":"authorization_pending"}),
        json!({"error":"slow_down"}),
        json!({"id_token":"id-secret","access_token":"never-exchange","refresh_token":"refresh-secret"}),
        exchange(),
    ]);
    let store = Store::default();
    let clock = Interaction::new();
    let mut options = options();
    options.auth_params = auth_params(&["access_type=offline".into()]).unwrap();
    login(&options, &http, &store, &clock).unwrap();
    assert_eq!(*clock.waits.borrow(), vec![5, 5, 10]);
    assert_eq!(
        http.sent.borrow().last().unwrap().1,
        json!({"jwt":"id-secret"})
    );
    assert!(http.sent.borrow()[1].1.to_string().contains("access_type"));
    let c = store.load().unwrap().unwrap();
    assert_eq!(c.token, "new-dagq-secret");
    assert_eq!(c.refresh_token.as_deref(), Some("refresh-secret"));
    assert_eq!(c.identity.subject, "returned-subject");
}
#[test]
fn pkce_is_s256_and_redirect_state_is_checked_before_code_is_used() {
    assert!(callback_address().ip().is_loopback());
    assert_eq!(callback_address().port(), 0);
    // RFC 7636 appendix B verifier and challenge.
    let bytes: [u8; 32] = URL_SAFE_NO_PAD
        .decode("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk")
        .unwrap()
        .try_into()
        .unwrap();
    let (v, c) = pkce(&bytes);
    assert_eq!(v, "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
    assert_eq!(c, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    let p = vec![
        ("state".into(), "expected".into()),
        ("code".into(), "secret".into()),
    ];
    assert_eq!(redirect(&p, "expected").unwrap(), "secret");
    assert!(redirect(&p, "wrong").is_err());
    let mut duplicate = p.clone();
    duplicate.push(("state".into(), "expected".into()));
    assert!(redirect(&duplicate, "expected").is_err());
    for browser in [false, true] {
        let mut opts = options();
        opts.browser = browser;
        let http = Http::new(vec![doc(browser), json!({"id_token":"id"}), exchange()]);
        let clock = Interaction::new();
        login(&opts, &http, &Store::default(), &clock).unwrap();
        assert!(clock.browser.get());
    }
}
#[test]
fn failed_login_never_replaces_the_previous_destination_or_exchanges_access_tokens() {
    let store = Store::default();
    store.save(&saved()).unwrap();
    for error in ["expired_token", "access_denied", "other"] {
        let http = Http::new(vec![doc(true), start(), json!({"error":error})]);
        assert!(login(&options(), &http, &store, &Interaction::new()).is_err());
        assert_eq!(http.sent.borrow().len(), 3);
    }
    let http = Http::new(vec![
        doc(true),
        start(),
        json!({"access_token":"must-not-send"}),
    ]);
    assert!(
        login(&options(), &http, &store, &Interaction::new())
            .unwrap_err()
            .to_string()
            .contains("scope")
    );
    assert_eq!(http.sent.borrow().len(), 3);
    for reply in [
        json!({"ok":false,"error":{"code":"unauthenticated","message":"secret"}}),
        json!({"ok":false,"error":{"code":"api_version_mismatch"},"min_api_version":3,"api_version":4}),
    ] {
        let http = Http::new(vec![doc(true), start(), json!({"id_token":"id"}), reply]);
        let error = login(&options(), &http, &store, &Interaction::new())
            .unwrap_err()
            .to_string();
        assert!(!error.contains("secret"));
        if error.contains("api_version_mismatch") {
            assert!(error.contains("3..4") && error.contains("install"));
        }
    }
    assert_eq!(store.load().unwrap().unwrap().token, "old-dagq-secret");
    let mut next = options();
    next.destination.control_url = "https://another-control.example".into();
    let http = Http::new(vec![
        doc(true),
        start(),
        json!({"id_token":"new-id"}),
        exchange(),
    ]);
    login(&next, &http, &store, &Interaction::new()).unwrap();
    assert_eq!(store.load().unwrap().unwrap().destination, next.destination);
}
#[test]
fn renewal_is_destination_bound_and_retries_only_once_even_when_live() {
    let c = saved();
    assert_eq!(
        renewal(&c, "http://127.0.0.1:1234/", 99, Rejected::No),
        Renewal::Keep
    );
    assert_eq!(
        renewal(&c, "http://127.0.0.1:1234", 100, Rejected::No),
        Renewal::Refresh
    );
    assert_eq!(
        renewal(&c, "http://127.0.0.1:1234", 99, Rejected::First),
        Renewal::Refresh
    );
    assert_eq!(
        renewal(&c, "http://127.0.0.1:1234", 99, Rejected::AfterRenewal),
        Renewal::Login
    );
    let store = Store::default();
    store.save(&c).unwrap();
    let http = Http::new(vec![]);
    assert!(read_token("https://other", Rejected::No, 0, &http, &store).is_err());
    assert!(http.sent.borrow().is_empty());
    assert!(
        read_token(
            &c.destination.control_url,
            Rejected::AfterRenewal,
            0,
            &http,
            &store
        )
        .is_err()
    );
    assert!(http.sent.borrow().is_empty());
    assert_eq!(
        read_token(&c.destination.control_url, Rejected::No, 99, &http, &store)
            .unwrap()
            .token,
        c.token
    );
    for (replacement, rejected, now) in [
        (None, Rejected::No, 100),
        (Some("rotated-refresh"), Rejected::First, 99),
    ] {
        let store = Store::default();
        store.save(&c).unwrap();
        let mut token = json!({"id_token":"fresh-id", "access_token":"never-send"});
        if let Some(r) = replacement {
            token["refresh_token"] = json!(r);
        }
        let http = Http::new(vec![doc(true), token, exchange()]);
        let fresh = read_token(&c.destination.control_url, rejected, now, &http, &store).unwrap();
        assert_eq!(
            fresh.refresh_token.as_deref(),
            Some(replacement.unwrap_or("old-refresh-secret"))
        );
        assert_eq!(http.sent.borrow()[2].1, json!({"jwt":"fresh-id"}));
        assert_eq!(store.load().unwrap().unwrap().token, "new-dagq-secret");
    }
}
#[test]
fn failed_refresh_sends_neither_old_token_nor_old_jwt_and_guides_login() {
    for reply in [
        json!({"access_token":"not-an-id"}),
        json!({"error":"invalid_grant"}),
    ] {
        let store = Store::default();
        store.save(&saved()).unwrap();
        let http = Http::new(vec![doc(true), reply]);
        assert_eq!(
            read_token(&dest().control_url, Rejected::No, 100, &http, &store)
                .err()
                .unwrap()
                .to_string(),
            LOGIN_AGAIN
        );
        assert_eq!(http.sent.borrow().len(), 2);
        assert_eq!(store.load().unwrap().unwrap().token, "old-dagq-secret");
    }
    let mut c = saved();
    c.refresh_token = None;
    let store = Store::default();
    store.save(&c).unwrap();
    let http = Http::new(vec![]);
    assert!(read_token(&dest().control_url, Rejected::No, 100, &http, &store).is_err());
    assert!(http.sent.borrow().is_empty());
}
#[test]
fn whoami_uses_only_stored_identity_and_destination_and_logout_is_idempotent() {
    let store = Store::default();
    let http = Http::new(vec![]);
    assert_eq!(
        whoami(&http, &store, 0).unwrap_err().to_string(),
        LOGIN_AGAIN
    );
    store.save(&saved()).unwrap();
    let shown = whoami(&http, &store, 99).unwrap();
    assert_eq!(
        shown,
        json!({"issuer":"issuer","subject":"subject","role":"member","expires_at":100,"control_url":"http://127.0.0.1:1234"})
    );
    let mut c = saved();
    c.identity.display_name = Some("Display Name".into());
    store.save(&c).unwrap();
    assert_eq!(
        whoami(&http, &store, 99).unwrap()["display_name"],
        "Display Name"
    );
    let value = whoami(&http, &store, 99).unwrap().to_string();
    for secret in ["old-dagq-secret", "old-refresh-secret"] {
        assert!(!value.contains(secret));
    }
    store.delete().unwrap();
    store.delete().unwrap();
    assert!(http.sent.borrow().is_empty());
}
#[test]
fn store_choice_routes_without_opening_the_other_store_and_cleans_old_file() {
    assert!(StoreChoice::parse(Some("unknown")).is_err());
    assert_eq!(StoreChoice::parse(None).unwrap(), StoreChoice::Auto);
    assert_eq!(StoreChoice::parse(Some("os")).unwrap(), StoreChoice::Os);
    assert_eq!(StoreChoice::parse(Some("file")).unwrap(), StoreChoice::File);
    for (choice, os_calls, file_calls) in [
        (StoreChoice::File, 0, 3),
        (StoreChoice::Os, 3, 0),
        (StoreChoice::Auto, 3, 2),
    ] {
        let os = Store::default();
        let file = Store::default();
        let stores = Stores {
            choice,
            os: &os,
            file: &file,
        };
        stores.save(&saved()).unwrap();
        assert!(stores.load().unwrap().is_some());
        stores.delete().unwrap();
        assert_eq!(os.calls.borrow().len(), os_calls);
        assert_eq!(file.calls.borrow().len(), file_calls);
    }
    let os = Store::default();
    os.fails.set(true);
    let file = Store::default();
    let stores = Stores {
        choice: StoreChoice::Auto,
        os: &os,
        file: &file,
    };
    stores.save(&saved()).unwrap();
    assert!(stores.load().unwrap().is_some());
    let explicit = Stores {
        choice: StoreChoice::Os,
        os: &os,
        file: &file,
    };
    let before = file.calls.borrow().len();
    assert!(explicit.save(&saved()).is_err());
    assert!(explicit.load().is_err());
    assert_eq!(file.calls.borrow().len(), before);
    struct AbsentOs;
    impl CredentialStore for AbsentOs {
        fn load(&self) -> Result<Option<Credentials>> {
            Err(OsStoreUnavailable.into())
        }
        fn save(&self, _: &Credentials) -> Result<()> {
            Err(OsStoreUnavailable.into())
        }
        fn delete(&self) -> Result<()> {
            Err(OsStoreUnavailable.into())
        }
    }
    let file = Store::default();
    let explicit = Stores {
        choice: StoreChoice::Os,
        os: &AbsentOs,
        file: &file,
    };
    assert!(explicit.load().is_err());
    assert!(explicit.save(&saved()).is_err());
    assert!(explicit.delete().is_err());
    assert!(file.calls.borrow().is_empty());
    let auto = Stores {
        choice: StoreChoice::Auto,
        os: &AbsentOs,
        file: &file,
    };
    auto.save(&saved()).unwrap();
    assert!(auto.load().unwrap().is_some());
    auto.delete().unwrap();
    assert!(file.value.borrow().is_none());
}

#[test]
fn gate_attempts_exactly_one_secret_free_denial_even_when_recording_fails() {
    use crate::application::commands::{DenialLog, Gate};
    use crate::domain::{ActorContext, AuthorizationError, Capability, Resource, StaticPolicy};
    struct Log {
        attempts: RefCell<Vec<Value>>,
        fail: bool,
    }
    impl DenialLog for Log {
        fn record_denial(&self, p: Value) -> Result<()> {
            self.attempts.borrow_mut().push(p);
            ensure!(!self.fail, "record failed");
            Ok(())
        }
    }
    for fail in [false, true] {
        let actor =
            ActorContext::from_env(|name| (name == "DAGQ_ROLE").then(|| "inbox".into())).unwrap();
        let log = Log {
            attempts: RefCell::new(vec![]),
            fail,
        };
        let error = Gate {
            actor: &actor,
            authorizer: &StaticPolicy,
        }
        .authorize(&log, Capability::IdentityLogin, &Resource::Queue)
        .unwrap_err();
        assert!(error.downcast_ref::<AuthorizationError>().is_some());
        assert_eq!(
            *log.attempts.borrow(),
            vec![
                json!({"event":"authorization_denied","role":"inbox","capability":"identity.login","reason":"not granted","resource":{"kind":"queue"}})
            ]
        );
    }
}

#[test]
fn invalid_discovery_is_refused_before_any_authorization_or_exchange_post() {
    for reply in [
        json!({"issuer":"https://wrong","token_endpoint":"https://idp.example/token"}),
        json!({"issuer":"https://idp.example","token_endpoint":"http://external"}),
    ] {
        let http = Http::new(vec![reply]);
        let store = Store::default();
        assert!(login(&options(), &http, &store, &Interaction::new()).is_err());
        assert_eq!(http.sent.borrow().len(), 1);
        assert!(store.calls.borrow().is_empty());
    }
}

#[test]
fn refresh_reply_keeps_or_rotates_only_the_refresh_credential_and_requires_id_token() {
    for (reply, expected) in [
        (json!({"id_token":"new-id"}), "old-refresh"),
        (
            json!({"id_token":"new-id","refresh_token":"rotated"}),
            "rotated",
        ),
    ] {
        let (id, refresh) = refresh_response(&reply, "old-refresh").unwrap();
        assert_eq!(id, "new-id");
        assert_eq!(refresh, expected);
    }
    assert!(refresh_response(&json!({"access_token":"never"}), "old-refresh").is_err());
}

#[test]
fn a_file_fallback_replaces_a_readable_old_os_destination() {
    let os = Store::default();
    os.save(&saved()).unwrap();
    os.save_fails.set(true);
    let file = Store::default();
    let stores = Stores {
        choice: StoreChoice::Auto,
        os: &os,
        file: &file,
    };
    let mut replacement = saved();
    replacement.destination.control_url = "https://control-b.example".into();
    replacement.token = "new-dagq-secret".into();
    stores.save(&replacement).unwrap();
    assert!(os.value.borrow().is_none());
    assert_eq!(*os.calls.borrow(), ["save", "save", "delete"]);
    let loaded = stores.load().unwrap().unwrap();
    assert_eq!(loaded.destination, replacement.destination);
    assert_eq!(loaded.token, replacement.token);
    let http = Http::new(vec![]);
    assert_eq!(
        whoami(&http, &stores, 99).unwrap()["control_url"],
        "https://control-b.example"
    );
    assert!(http.sent.borrow().is_empty());

    // A failed file write must leave the old OS set available.
    let os = Store::default();
    os.save(&saved()).unwrap();
    os.save_fails.set(true);
    let file = Store::default();
    file.fails.set(true);
    let stores = Stores {
        choice: StoreChoice::Auto,
        os: &os,
        file: &file,
    };
    assert!(stores.save(&replacement).is_err());
    assert_eq!(*os.calls.borrow(), ["save", "save"]);
    assert_eq!(os.load().unwrap().unwrap().destination, saved().destination);
}

#[test]
fn approval_guidance_and_success_use_exchange_fields_for_both_flows() {
    use crate::domain::oidc::CLOCK_SKEW_SECS;
    use crate::domain::queue_service::{APPROVAL_LOGIN_SECONDS, approval_login};
    for browser in [false, true] {
        let flow = if browser { Flow::Browser } else { Flow::Device };
        for at in [
            None,
            Some(1000 - APPROVAL_LOGIN_SECONDS),
            Some(999),
            Some(1000 + CLOCK_SKEW_SECS),
            Some(1001 + CLOCK_SKEW_SECS),
        ] {
            let decision = approval_login(at, 1000);
            let mut reply = exchange();
            let fields = serde_json::to_value(&decision).unwrap();
            reply["result"]
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let guidance = approval_guidance(&reply["result"], &flow);
            if let Some(until) = decision.approval_until {
                assert!(guidance.contains(&until.to_string()));
                assert!(guidance.contains("承認に使える期限"));
            } else if at == Some(1001 + CLOCK_SKEW_SECS) {
                assert!(guidance.contains("この login は承認に使えない"));
                assert!(guidance.contains("時計のずれ"));
                assert!(!guidance.contains("--browser"));
                assert!(!guidance.contains("承認に使える期限"));
            } else {
                assert!(guidance.contains("この IdP では承認に使えない"));
                assert_eq!(guidance.contains("--browser"), !browser);
                if browser {
                    assert!(
                        guidance.contains("auth_time")
                            && guidance.contains("max_age")
                            && guidance.contains("管理者")
                    );
                }
            }
            let mut replies = vec![doc(!browser)];
            if !browser {
                replies.push(start());
            }
            replies.extend([json!({"id_token":"opaque-jwt"}), reply]);
            let http = Http::new(replies);
            let store = Store::default();
            let mut opts = options();
            opts.browser = browser;
            login(&opts, &http, &store, &Interaction::new()).unwrap();
            assert_eq!(store.load().unwrap().unwrap().token, "new-dagq-secret");
            if !browser {
                let params: Params =
                    serde_json::from_value(http.sent.borrow()[1].1.clone()).unwrap();
                assert!(params.contains(&("max_age".into(), APPROVAL_LOGIN_SECONDS.to_string())));
            }
        }
        assert_eq!(
            approval_guidance(&json!({}), &flow),
            approval_guidance(
                &serde_json::to_value(approval_login(None, 0)).unwrap(),
                &flow
            )
        );
    }
}
