use super::*;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::signature::{self, KeyPair};
use std::{
    sync::Mutex,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

struct Clock;
impl crate::application::Clock for Clock {
    fn system_time(&self) -> SystemTime {
        UNIX_EPOCH
    }
    fn monotonic(&self) -> Instant {
        panic!("no waits")
    }
}

#[test]
fn responses_distinguish_absence_permissions_and_retryable_failures() {
    assert_eq!(
        classify(
            Operation::Create,
            422,
            r#"{"message":"Reference already exists"}"#
        ),
        Ok(Reply::Present)
    );
    assert_eq!(classify(Operation::Delete, 404, ""), Ok(Reply::Missing));
    assert_eq!(
        classify(
            Operation::Delete,
            422,
            r#"{"message":"Reference does not exist"}"#
        ),
        Ok(Reply::Missing)
    );
    assert_eq!(
        classify(Operation::Delete, 422, ""),
        Err(RefFailure::Invalid)
    );
    for status in [401, 403] {
        assert_eq!(
            classify(Operation::Read, status, ""),
            Err(RefFailure::Permission)
        );
    }
    for status in [429, 500, 503] {
        assert_eq!(
            classify(Operation::Create, status, ""),
            Err(RefFailure::Temporary)
        );
    }
    assert_eq!(
        classify(
            Operation::Read,
            403,
            r#"{"message":"API rate limit exceeded"}"#
        ),
        Err(RefFailure::Temporary)
    );
}
#[test]
fn names_requests_and_readback_are_strict_and_do_not_overwrite() {
    assert_eq!(
        ref_name("run-1", 2, false).unwrap(),
        "refs/heads/dagq/run-1/2"
    );
    assert_eq!(
        ref_name("run-1", 2, true).unwrap(),
        "refs/heads/dagq-landing/run-1/2"
    );
    for run in ["", "../main", "a?b", "a/b", "a.b"] {
        assert!(ref_name(run, 1, false).is_err());
    }
    assert!(ref_name("r", 0, false).is_err());
    let name = "refs/heads/dagq/r/2";
    assert_eq!(
        request_path("o/r", name, Operation::Read).unwrap(),
        "/repos/o/r/git/ref/heads/dagq/r/2"
    );
    assert_eq!(
        request_path("o/r", name, Operation::Delete).unwrap(),
        "/repos/o/r/git/refs/heads/dagq/r/2"
    );
    assert_eq!(
        request_path("o/r", name, Operation::Create).unwrap(),
        "/repos/o/r/git/refs"
    );
    let sha = "b".repeat(40);
    assert_eq!(
        create_body(name, &sha).unwrap(),
        json!({"ref":name,"sha":sha})
    );
    assert!(create_body(name, "invalid").is_err());
    assert_eq!(
        read_head(
            &json!({"ref":name,"object":{"type":"commit","sha":sha}}).to_string(),
            name
        )
        .unwrap(),
        sha
    );
    for body in [
        "{}".into(),
        json!({"ref":name,"object":{"type":"tag","sha":sha}}).to_string(),
        json!({"ref":"refs/heads/main","object":{"type":"commit","sha":sha}}).to_string(),
    ] {
        assert!(read_head(&body, name).is_err());
    }
}

struct Api {
    public: [Vec<u8>; 2],
    calls: Mutex<Vec<(String, String, String)>>,
    failure: Mutex<Option<u16>>,
}
impl GithubApi for Api {
    fn request(
        &self,
        method: &str,
        url: &str,
        token: &AppToken,
        body: Option<&Value>,
        _: bool,
    ) -> Result<String, ConfigUnavailable> {
        // Only token discovery/issuance may use an App JWT. Verify distinct
        // keys, not merely the selected issuer claim or a shared expected helper.
        let pieces: Vec<_> = token.0.split('.').collect();
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(pieces[1]).unwrap()).unwrap();
        let main = claims["iss"] == "22";
        assert!(main || claims["iss"] == "11");
        signature::UnparsedPublicKey::new(
            &signature::RSA_PKCS1_2048_8192_SHA256,
            &self.public[usize::from(main)],
        )
        .verify(
            format!("{}.{}", pieces[0], pieces[1]).as_bytes(),
            &URL_SAFE_NO_PAD.decode(pieces[2]).unwrap(),
        )
        .unwrap();
        if url.ends_with("/installation") {
            assert_eq!(method, "GET");
            return Ok(r#"{"id":31}"#.into());
        }
        assert_eq!(method, "POST");
        assert!(url.ends_with("/app/installations/31/access_tokens"));
        let expected = if main {
            json!({"repositories":["repo"],"permissions":{"contents":"write","workflows":"write"}})
        } else {
            json!({"repositories":["repo"],"permissions":{"contents":"read"}})
        };
        assert_eq!(body, Some(&expected));
        Ok(json!({"token":if main {"main-secret"} else {"runner-secret"}}).to_string())
    }
    fn request_status(
        &self,
        method: &str,
        url: &str,
        token: &AppToken,
        body: Option<&Value>,
    ) -> Result<(u16, String), ConfigUnavailable> {
        assert_eq!(token.0, "main-secret");
        if let Some(status) = *self.failure.lock().unwrap() {
            return Ok((
                status,
                "main-secret runner-secret upstream-private-body".into(),
            ));
        }
        self.calls
            .lock()
            .unwrap()
            .push((method.into(), url.into(), "main".into()));
        if method == "POST" {
            assert_eq!(
                body,
                Some(&json!({"ref":"refs/heads/dagq/r/2","sha":"b".repeat(40)}))
            );
            Ok((422, r#"{"message":"Reference already exists"}"#.into()))
        } else {
            assert_eq!(method, "DELETE");
            Ok((404, String::new()))
        }
    }
    fn get_status(&self, url: &str, token: &AppToken) -> Result<(u16, String), ConfigUnavailable> {
        assert_eq!(token.0, "runner-secret");
        if let Some(status) = *self.failure.lock().unwrap() {
            return Ok((
                status,
                "main-secret runner-secret upstream-private-body".into(),
            ));
        }
        self.calls
            .lock()
            .unwrap()
            .push(("GET".into(), url.into(), "runner".into()));
        let name = if url.ends_with("heads/main") {
            "refs/heads/main"
        } else {
            "refs/heads/dagq/r/2"
        };
        Ok((
            200,
            json!({"ref":name,"object":{"sha":"a".repeat(40),"type":"commit"}}).to_string(),
        ))
    }
}
#[test]
fn distinct_apps_issue_narrow_tokens_for_ref_writes_and_uncached_reads() {
    let keys: Vec<_> = (0..2)
        .map(|_| {
            rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256)
                .unwrap()
                .serialize_pem()
        })
        .collect();
    let public = keys
        .iter()
        .map(|pem| {
            use rustls::pki_types::{PrivateKeyDer, pem::PemObject};
            let PrivateKeyDer::Pkcs8(der) = PrivateKeyDer::from_pem_slice(pem.as_bytes()).unwrap()
            else {
                panic!("pkcs8")
            };
            signature::RsaKeyPair::from_pkcs8(der.secret_pkcs8_der())
                .unwrap()
                .public_key()
                .as_ref()
                .to_vec()
        })
        .collect::<Vec<_>>();
    let api = Arc::new(Api {
        public: [public[0].clone(), public[1].clone()],
        calls: Mutex::default(),
        failure: Mutex::default(),
    });
    let pairs = [
        ("DAGQ_GITHUB_RUNNER_APP_ID", "11"),
        ("DAGQ_GITHUB_RUNNER_APP_KEY", keys[0].as_str()),
        ("DAGQ_GITHUB_MAIN_APP_ID", "22"),
        ("DAGQ_GITHUB_MAIN_APP_KEY", keys[1].as_str()),
        ("DAGQ_GITHUB_REPOSITORY", "owner/repo"),
        (
            "DAGQ_GITHUB_API_BASE_URL",
            "https://enterprise.example/api/v3",
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
    let runner_only = DeploymentConfig::read(
        |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name && !k.starts_with("DAGQ_GITHUB_MAIN_APP_"))
                .map(|(_, v)| (*v).into())
        },
        |_| unreachable!(),
    )
    .unwrap();
    assert!(
        GithubRefs::on(&runner_only, Arc::new(Clock), api.clone())
            .unwrap()
            .is_none()
    );
    assert!(api.calls.lock().unwrap().is_empty());
    let refs = GithubRefs::on(&config, Arc::new(Clock), api.clone())
        .unwrap()
        .unwrap();
    refs.create("r", 2, &"b".repeat(40)).unwrap();
    // Existing ref is read again; a different head is returned, never claimed
    // to be the requested head by the adapter (the takeover domain gates it).
    assert_eq!(refs.read("r", 2).unwrap(), Some("a".repeat(40)));
    assert_eq!(refs.main_head().unwrap().as_str(), "a".repeat(40));
    refs.delete("r", 1).unwrap();
    refs.delete_landing("r", 2).unwrap();
    let calls = api.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .map(|(m, _, app)| (m.as_str(), app.as_str()))
            .collect::<Vec<_>>(),
        [
            ("POST", "main"),
            ("GET", "runner"),
            ("GET", "runner"),
            ("DELETE", "main"),
            ("DELETE", "main")
        ]
    );
    assert_eq!(
        calls[0].1,
        "https://enterprise.example/api/v3/repos/owner/repo/git/refs"
    );
    assert!(calls[1].1.ends_with("/git/ref/heads/dagq/r/2"));
    assert!(calls[4].1.ends_with("/git/refs/heads/dagq-landing/r/2"));
    assert!(!format!("{:?}", AppToken("main-secret".into())).contains("main-secret"));
    drop(calls);
    for (status, expected) in [(403, RefFailure::Permission), (503, RefFailure::Temporary)] {
        *api.failure.lock().unwrap() = Some(status);
        for error in [
            refs.create("r", 2, &"b".repeat(40)).unwrap_err(),
            refs.read("r", 2).unwrap_err(),
            refs.main_head().unwrap_err(),
            refs.delete("r", 1).unwrap_err(),
        ] {
            assert_eq!(error.downcast_ref::<RefFailure>(), Some(&expected));
            let diagnostic = format!("{error:#} {error:?}");
            for secret in ["main-secret", "runner-secret", "upstream-private-body"] {
                assert!(!diagnostic.contains(secret));
            }
        }
    }
}

#[test]
fn no_runner_credentials_leave_the_local_path_unconfigured() {
    let config = DeploymentConfig::read(|_| None, |_| unreachable!()).unwrap();
    // No keys, no API calls and no change to local defaults.
    assert!(
        GithubRefs::from_deployment(&config, Arc::new(Clock))
            .unwrap()
            .is_none()
    );
}
