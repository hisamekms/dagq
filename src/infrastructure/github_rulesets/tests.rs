use super::*;
use crate::domain::ruleset_gate::{
    Unverified,
    test_support::{APPS, reference},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::json;
use std::sync::Mutex;

/// GitHub as the fake answers it: the installation, the token, the list's
/// pages and each ruleset, or a fixed status for the list.
struct Api {
    calls: Mutex<Vec<(String, String, Option<Value>)>>,
    rulesets: Vec<Value>,
    list_status: (u16, String),
}
impl Api {
    fn new(rulesets: Vec<Value>) -> Self {
        Self {
            calls: Mutex::new(vec![]),
            rulesets,
            list_status: (200, String::new()),
        }
    }
}
impl GithubApi for Api {
    fn request(
        &self,
        method: &str,
        url: &str,
        bearer: &AppToken,
        body: Option<&Value>,
        _: bool,
    ) -> Result<String, ConfigUnavailable> {
        // The App JWT names the runner App, never the main App.
        let claims: Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(bearer.0.split('.').nth(1).unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(claims["iss"], "11");
        self.calls
            .lock()
            .unwrap()
            .push((method.into(), url.into(), body.cloned()));
        Ok(if method == "POST" {
            json!({"token": "installation-secret"}).to_string()
        } else {
            json!({"id": 31}).to_string()
        })
    }
    fn get_status(&self, url: &str, bearer: &AppToken) -> Result<(u16, String), ConfigUnavailable> {
        assert_eq!(bearer.0, "installation-secret");
        self.calls
            .lock()
            .unwrap()
            .push(("GET".into(), url.into(), None));
        let (_, rest) = url.split_once("/repos/owner/repo/rulesets").unwrap();
        if let Some(query) = rest.strip_prefix('?') {
            if self.list_status.0 != 200 {
                return Ok(self.list_status.clone());
            }
            let page: usize = query.rsplit('=').next().unwrap().parse().unwrap();
            let listed: Vec<Value> = self
                .rulesets
                .iter()
                .skip((page - 1) * PER_PAGE)
                .take(PER_PAGE)
                .map(|r| json!({"id": r["id"], "name": r["name"]}))
                .collect();
            return Ok((200, Value::Array(listed).to_string()));
        }
        let id: u64 = rest.trim_start_matches('/').parse().unwrap();
        Ok(match self.rulesets.iter().find(|r| r["id"] == id) {
            Some(ruleset) => (200, ruleset.to_string()),
            None => (404, json!({"message": "Not Found"}).to_string()),
        })
    }
}

fn key() -> String {
    rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256)
        .unwrap()
        .serialize_pem()
}

/// A clock at 1000 s, or one that moves `step` seconds each time it is read.
#[derive(Default)]
struct Clock {
    step: u64,
    reads: std::sync::atomic::AtomicU64,
}
impl crate::application::Clock for Clock {
    fn system_time(&self) -> std::time::SystemTime {
        let n = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000 + n * self.step)
    }
    fn monotonic(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
}

fn config(runner: &str, main: &str) -> DeploymentConfig {
    let pairs = [
        ("DAGQ_GITHUB_RUNNER_APP_ID", "11"),
        ("DAGQ_GITHUB_RUNNER_APP_KEY", runner),
        ("DAGQ_GITHUB_MAIN_APP_ID", "22"),
        ("DAGQ_GITHUB_MAIN_APP_KEY", main),
        ("DAGQ_GITHUB_REPOSITORY", "owner/repo"),
    ];
    DeploymentConfig::read(
        |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).into())
        },
        |_| unreachable!(),
    )
    .unwrap()
}

#[test]
fn the_read_token_is_the_runner_apps_for_one_repository_and_metadata_read_only() {
    assert_eq!(
        metadata_scope("repo"),
        json!({"repositories": ["repo"], "permissions": {"metadata": "read"}})
    );
    assert!(
        !metadata_scope("repo")
            .to_string()
            .contains("administration")
    );
    let api = Arc::new(Api::new(reference()));
    let gate = ruleset_gate_on(
        &config(&key(), &key()),
        Arc::new(Clock::default()),
        api.clone(),
    )
    .unwrap()
    .unwrap();
    // Made without reading: the watch's first step reads, off the line.
    assert!(api.calls.lock().unwrap().is_empty());
    assert!(gate.refresh());
    let calls = api.calls.lock().unwrap().clone();
    assert_eq!(calls[1].0, "POST");
    assert_eq!(calls[1].2, Some(metadata_scope("repo")));
    assert_eq!(
        calls[2].1,
        "https://api.github.com/repos/owner/repo/rulesets?per_page=100&page=1"
    );
    assert_eq!(calls.len(), 3 + 6);
    // Taking the result reads nothing more.
    assert_eq!(gate.current(), Ok(()));
    assert_eq!(api.calls.lock().unwrap().len(), calls.len());
    // The Apps' ids come from the deployment: the reference's R1 and R6 name
    // the main App (22) and verified above; the runner App (11) on R4 does not.
    let mut bypassed = reference();
    bypassed[3]["bypass_actors"] = json!([{"actor_type": "Integration", "actor_id": 11}]);
    let api = Arc::new(Api::new(bypassed));
    let gate = ruleset_gate_on(
        &config(&key(), &key()),
        Arc::new(Clock::default()),
        api.clone(),
    )
    .unwrap()
    .unwrap();
    gate.refresh();
    assert!(gate.current().is_err());
    // No runner App, no gate: a local queue never passes through it.
    let none = DeploymentConfig::read(|_| None, |_| unreachable!()).unwrap();
    assert!(
        ruleset_gate_on(&none, Arc::new(Clock::default()), api)
            .unwrap()
            .is_none()
    );
}

fn source(api: Arc<Api>) -> GithubRulesets {
    source_at(api, Clock::default())
}

fn source_at(api: Arc<Api>, clock: Clock) -> GithubRulesets {
    struct Tokens;
    impl InstallationTokens for Tokens {
        fn issue(&self, scope: &Value, _: i64) -> Result<AppToken, ConfigUnavailable> {
            assert_eq!(scope, &metadata_scope("repo"));
            Ok(AppToken("installation-secret".into()))
        }
    }
    GithubRulesets::new(
        Box::new(Tokens),
        api,
        Arc::new(clock),
        APPS,
        "https://api.github.com",
        "owner/repo",
    )
    .unwrap()
}

#[test]
fn every_page_is_read_and_a_refusal_or_bad_answer_is_unverified() {
    // More than a page: the second page is read too.
    let mut many = reference();
    for id in 100..250 {
        many.push(json!({"id": id, "target": "push", "enforcement": "active", "rules": []}));
    }
    // A read past its deadline is unreadable.
    let slow = Clock {
        step: 60,
        ..Clock::default()
    };
    assert_eq!(
        source_at(Arc::new(Api::new(reference())), slow).read(1_000),
        Read::Unreadable
    );
    let api = Arc::new(Api::new(many));
    let read = source(api.clone()).read(1_000);
    assert_eq!(crate::domain::ruleset_gate::judge(&read, APPS), Ok(()));
    let pages = api
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.1.contains("per_page"))
        .count();
    assert_eq!(pages, 2);
    // The plan's refusal, another refusal, a body that is no list.
    let plan = json!({"message": "Upgrade to GitHub Pro or make this repository public to enable this feature."});
    for (status, body, expected) in [
        (403, plan.to_string(), Read::PlanUnavailable),
        (
            403,
            json!({"message": "Resource not accessible by integration"}).to_string(),
            Read::Unreadable,
        ),
        (500, "oops".into(), Read::Unreadable),
    ] {
        let mut api = Api::new(reference());
        api.list_status = (status, body);
        assert_eq!(source(Arc::new(api)).read(1_000), expected);
    }
    // A ruleset that cannot be read or parsed.
    let mut broken = reference();
    broken[2]["rules"] = json!("not rules");
    assert_eq!(
        source(Arc::new(Api::new(broken))).read(1_000),
        Read::Unreadable
    );
    // The reasons never carry the token.
    for reason in [Unverified::Unreadable, Unverified::PlanUnavailable] {
        assert!(!reason.message().contains("installation-secret"));
    }
}
