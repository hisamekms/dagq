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

#[test]
fn compare_paths_require_a_complete_list_and_keep_both_rename_paths() {
    assert_eq!(
        complete_changed_paths(&json!({"files":[
            {"filename":"new/AGENT.md", "status":"renamed", "previous_filename":"old/AGENT.md"},
            {"filename":"dagq.toml", "status":"modified"}
        ]}))
        .unwrap(),
        ["dagq.toml", "new/AGENT.md", "old/AGENT.md"]
    );
    assert_eq!(
        complete_changed_paths(&json!({"files":[]})).unwrap(),
        Vec::<String>::new()
    );
    for value in [
        json!({}),
        json!({"files":[{}]}),
        json!({"files":[{"filename":"new", "status":"renamed"}]}),
        json!({"files":vec![json!({"filename":"a"}); 300]}),
        json!({"files":vec![json!({"filename":"a"}); 301]}),
    ] {
        assert!(complete_changed_paths(&value).is_err());
    }
    assert!(complete_changed_paths(&json!({"files":vec![json!({"filename":"a"});299]})).is_ok());
}

struct ReviewClock;
impl crate::application::Clock for ReviewClock {
    fn system_time(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + Duration::from_secs(1000)
    }
    fn monotonic(&self) -> std::time::Instant {
        panic!("no waits")
    }
}
struct ReviewApi {
    auth: Api,
    compare: Mutex<Value>,
    file_status: Mutex<u16>,
}
impl GithubApi for ReviewApi {
    fn request(
        &self,
        method: &str,
        url: &str,
        bearer: &AppToken,
        body: Option<&Value>,
        raw: bool,
    ) -> Result<String, ConfigUnavailable> {
        // Authenticate and record through the independently verifying fake.
        let text = self.auth.request(method, url, bearer, body, raw)?;
        if url.contains("/compare/") {
            Ok(self.compare.lock().unwrap().to_string())
        } else if url.ends_with("/git/ref/heads/main") {
            Ok(
                json!({"ref":"refs/heads/main","object":{"type":"commit", "sha":"a".repeat(40)}})
                    .to_string(),
            )
        } else {
            Ok(text)
        }
    }
    fn raw_status(&self, url: &str, bearer: &AppToken) -> Result<(u16, String), ConfigUnavailable> {
        Ok((
            *self.file_status.lock().unwrap(),
            self.request("GET", url, bearer, None, true)?,
        ))
    }
}
#[test]
fn review_reads_pin_raw_files_compare_ranges_and_request_only_runner_read_scope() {
    use crate::application::review_input::ReviewReads;
    let pem = key();
    let PrivateKeyDer::Pkcs8(der) = PrivateKeyDer::from_pem_slice(pem.as_bytes()).unwrap() else {
        panic!()
    };
    let public = signature::RsaKeyPair::from_pkcs8(der.secret_pkcs8_der())
        .unwrap()
        .public_key()
        .as_ref()
        .to_vec();
    let api = Arc::new(ReviewApi {
        auth: Api {
            calls: Mutex::new(vec![]),
            public,
            failure: false,
        },
        compare: Mutex::new(
            json!({"merge_base_commit":{"sha":"b".repeat(40)},"files":[{"filename":"src/a.rs"}]}),
        ),
        file_status: Mutex::new(200),
    });
    // Distinct App keys: the fake verifies every issuance with the runner's
    // public key, so selecting the main signing key cannot pass this test.
    let main_pem = key();
    let pairs = [
        ("DAGQ_GITHUB_RUNNER_APP_ID", "11"),
        ("DAGQ_GITHUB_RUNNER_APP_KEY", pem.as_str()),
        ("DAGQ_GITHUB_MAIN_APP_ID", "22"),
        ("DAGQ_GITHUB_MAIN_APP_KEY", main_pem.as_str()),
        ("DAGQ_GITHUB_REPOSITORY", "owner/repo"),
        (
            "DAGQ_GITHUB_API_BASE_URL",
            "https://enterprise.example/api/v3",
        ),
    ];
    let config = DeploymentConfig::read(
        |n| {
            pairs
                .iter()
                .find(|(k, _)| *k == n)
                .map(|(_, v)| (*v).into())
        },
        |_| unreachable!(),
    )
    .unwrap();
    assert!(
        GithubReviewReads::from_deployment(&config, Arc::new(ReviewClock))
            .unwrap()
            .is_some()
    );
    let reads = GithubReviewReads {
        app: RunnerApp::from_deployment(&config, api.clone())
            .unwrap()
            .unwrap(),
        clock: Arc::new(ReviewClock),
    };
    let main = reads.main_head().unwrap();
    assert_eq!(main.as_str(), "a".repeat(40));
    let head = "c".repeat(40);
    assert!(
        reads
            .file_in(&head, "dagq.toml")
            .unwrap()
            .unwrap()
            .contains("parallel = 2")
    );
    assert!(reads.file_in(&head, "dir/a?#.md").is_ok());
    assert_eq!(
        reads
            .merge_base(main.as_str(), &head)
            .unwrap()
            .unwrap()
            .as_str(),
        "b".repeat(40)
    );
    assert_eq!(
        reads.changed_paths(main.as_str(), &head).unwrap(),
        ["src/a.rs"]
    );
    *api.file_status.lock().unwrap() = 404;
    assert_eq!(reads.file_in(&head, "absent").unwrap(), None);
    *api.file_status.lock().unwrap() = 403;
    let error = reads.file_in(&head, "denied").unwrap_err();
    assert!(!format!("{error:?}").contains("installation-secret"));
    *api.compare.lock().unwrap() = json!({"files":vec![json!({"filename":"a"});300]});
    assert!(reads.changed_paths(main.as_str(), &head).is_err());
    *api.compare.lock().unwrap() = json!({"message":"installation-secret"});
    assert!(reads.changed_paths(main.as_str(), &head).is_err());
    let calls = api.auth.calls.lock().unwrap();
    for (_, _, body, _) in calls.iter().filter(|(method, _, _, _)| method == "POST") {
        assert_eq!(
            body.as_ref().unwrap(),
            &json!({"repositories":["repo"],"permissions":{"contents":"read"}})
        );
    }
    assert!(calls.iter().any(|(_, url, _, raw)| *raw
        && url
            == &format!(
                "https://enterprise.example/api/v3/repos/owner/repo/contents/dagq.toml?ref={head}"
            )));
    assert!(
        calls
            .iter()
            .any(|(_, url, _, raw)| *raw && url.contains("dir/a%3F%23.md?ref="))
    );
    assert!(calls.iter().any(|(_, url, _, _)| url
        == &format!(
            "https://enterprise.example/api/v3/repos/owner/repo/compare/{}...{head}?per_page=1",
            main.as_str()
        )));
}

#[test]
fn raw_contents_can_exceed_the_json_bound_without_truncation() {
    let large = vec![b'x'; 1_048_577];
    assert_eq!(
        read_bounded(large.as_slice(), 100 * 1_048_576)
            .unwrap()
            .len(),
        1_048_577
    );
    assert!(read_bounded(large.as_slice(), 1_048_576).is_err());
    assert_eq!(read_bounded(b"abc".as_slice(), 3).unwrap(), "abc");
    assert!(read_bounded(b"abcd".as_slice(), 3).is_err());
    assert!(read_bounded(&[0xff][..], 3).is_err());
}
