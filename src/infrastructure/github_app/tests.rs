use super::*;
use crate::application::supervise::repository_config::read_scope;
use ring::signature::KeyPair;
use std::sync::Mutex;

type ApiCall = (String, String, Option<Value>, bool);

struct Api {
    calls: Mutex<Vec<ApiCall>>,
    public: Vec<u8>,
    failure: bool,
}
impl GithubApi for Api {
    fn request(
        &self,
        method: &str,
        url: &str,
        bearer: &AppToken,
        body: Option<&Value>,
        raw: bool,
    ) -> Result<String, ConfigUnavailable> {
        if self.failure {
            return Err(ConfigUnavailable);
        }
        if url.ends_with("/installation") || method == "POST" {
            let pieces: Vec<_> = bearer.0.split('.').collect();
            assert_eq!(pieces.len(), 3);
            let message = format!("{}.{}", pieces[0], pieces[1]);
            signature::UnparsedPublicKey::new(&signature::RSA_PKCS1_2048_8192_SHA256, &self.public)
                .verify(
                    message.as_bytes(),
                    &URL_SAFE_NO_PAD.decode(pieces[2]).unwrap(),
                )
                .unwrap();
            let claims: Value =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(pieces[1]).unwrap()).unwrap();
            assert_eq!(claims, json!({"iss":"11","iat":940,"exp":1540}));
            let header: Value =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(pieces[0]).unwrap()).unwrap();
            assert_eq!(header, json!({"alg":"RS256","typ":"JWT"}));
        } else {
            assert_eq!(bearer.0, "installation-secret");
        }
        self.calls
            .lock()
            .unwrap()
            .push((method.into(), url.into(), body.cloned(), raw));
        Ok(if url.ends_with("/installation") {
            json!({"id":31}).to_string()
        } else if method == "POST" {
            json!({"token":"installation-secret"}).to_string()
        } else if raw {
            "[supervisor]\nparallel = 2\n".into()
        } else {
            json!({"object":{"sha":"a".repeat(40)}}).to_string()
        })
    }
}
fn key() -> String {
    rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256)
        .unwrap()
        .serialize_pem()
}
#[test]
fn runner_key_signs_repository_installation_requests_and_pinned_enterprise_reads() {
    let runner_key = key();
    let main_key = key();
    let PrivateKeyDer::Pkcs8(der) = PrivateKeyDer::from_pem_slice(runner_key.as_bytes()).unwrap()
    else {
        panic!()
    };
    let public = signature::RsaKeyPair::from_pkcs8(der.secret_pkcs8_der())
        .unwrap()
        .public_key()
        .as_ref()
        .to_vec();
    let api = Arc::new(Api {
        calls: Mutex::new(vec![]),
        public,
        failure: false,
    });
    let pairs = [
        ("DAGQ_GITHUB_RUNNER_APP_ID", "11"),
        ("DAGQ_GITHUB_RUNNER_APP_KEY", runner_key.as_str()),
        ("DAGQ_GITHUB_MAIN_APP_ID", "22"),
        ("DAGQ_GITHUB_MAIN_APP_KEY", main_key.as_str()),
        ("DAGQ_GITHUB_REPOSITORY", "owner/repo"),
        (
            "DAGQ_GITHUB_API_BASE_URL",
            "https://enterprise.example/api/v3/",
        ),
    ];
    let config = DeploymentConfig::read(
        |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).into())
        },
        |_| unreachable!(),
    )
    .unwrap();
    let app = RunnerApp::from_deployment(&config, api.clone())
        .unwrap()
        .unwrap();
    let token = app.issue(&read_scope("repo"), 1000).unwrap();
    let sha = app.main_sha(&token).unwrap();
    assert!(app.contents(&token, &sha).unwrap().contains("parallel = 2"));
    assert_eq!(
        *api.calls.lock().unwrap(),
        vec![
            (
                "GET".into(),
                "https://enterprise.example/api/v3/repos/owner/repo/installation".into(),
                None,
                false
            ),
            (
                "POST".into(),
                "https://enterprise.example/api/v3/app/installations/31/access_tokens".into(),
                Some(json!({"repositories":["repo"],"permissions":{"contents":"read"}})),
                false
            ),
            (
                "GET".into(),
                "https://enterprise.example/api/v3/repos/owner/repo/git/ref/heads/main".into(),
                None,
                false
            ),
            (
                "GET".into(),
                format!(
                    "https://enterprise.example/api/v3/repos/owner/repo/contents/dagq.toml?ref={sha}"
                ),
                None,
                true
            ),
        ]
    );
}
#[test]
fn failures_and_invalid_destinations_cannot_expose_credentials() {
    // ureq-proto traces raw protocol bytes, including bearer headers and token
    // response bodies: the compile-time ceiling must stay below Trace.
    assert!(std::hint::black_box(log::STATIC_MAX_LEVEL) <= log::LevelFilter::Debug);
    let header = bearer_header(&AppToken("installation-secret".into())).unwrap();
    assert!(header.is_sensitive());
    assert!(!format!("{header:?}").contains("installation-secret"));
    assert!(bearer_header(&AppToken("secret\nbroken".into())).is_err());
    for url in [
        "http://api.github.com",
        "https://secret@host/api/v3",
        "https://host?token=secret",
        "https://host/#secret",
    ] {
        assert!(!valid_base(url));
    }
    for repository in [
        "owner/repo?token=secret",
        "owner/../repo",
        "owner/..",
        "/repo",
    ] {
        assert!(!valid_repository(repository));
    }
    assert!(valid_base("https://host:8443/api/v3"));
    assert!(valid_repository("owner/repo.git"));
    let api = Arc::new(Api {
        calls: Mutex::new(vec![]),
        public: vec![],
        failure: true,
    });
    let app = RunnerApp::new("11", &key(), "owner/repo", "https://api.github.com", api).unwrap();
    let error = app.issue(&read_scope("repo"), 1000).unwrap_err();
    assert_eq!(error, ConfigUnavailable);
    assert!(!format!("{error:?}: {error}").contains("secret"));
    assert!(
        app.contents(&AppToken("secret".into()), "bad?token=secret")
            .is_err()
    );
}
