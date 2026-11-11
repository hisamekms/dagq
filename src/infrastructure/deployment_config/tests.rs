use super::*;

// Fresh test keys live only in this test's memory, never in the repository.
fn app_key() -> String {
    rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256)
        .unwrap()
        .serialize_pem()
}

fn tls_pair() -> (String, String) {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    (generated.cert.pem(), generated.signing_key.serialize_pem())
}

fn read(pairs: &[(&str, &str)]) -> Result<DeploymentConfig, InvalidSettings> {
    DeploymentConfig::read(
        |n| {
            pairs
                .iter()
                .find(|(name, _)| *name == n)
                .map(|(_, v)| OsString::from(v))
        },
        |_| Err(()),
    )
}
fn postgres(app_key: &str) -> Vec<(&'static str, &str)> {
    vec![
        ("DAGQ_STORE", "postgres"),
        ("DAGQ_DATABASE_URL", "postgres://user:secret@db/queue"),
        ("DAGQ_OBJECT_STORE_ENDPOINT", "https://objects.example.com"),
        ("DAGQ_OBJECT_STORE_BUCKET", "queue"),
        ("DAGQ_OBJECT_STORE_REGION", "region"),
        ("DAGQ_OIDC_ISSUER", "https://identity.example.com"),
        ("DAGQ_OIDC_AUDIENCE", "queue"),
        ("DAGQ_GITHUB_RUNNER_APP_ID", "1"),
        ("DAGQ_GITHUB_RUNNER_APP_KEY", app_key),
        ("DAGQ_GITHUB_MAIN_APP_ID", "2"),
        ("DAGQ_GITHUB_MAIN_APP_KEY", app_key),
        ("DAGQ_GITHUB_REPOSITORY", "owner/repo"),
        (
            "DAGQ_IMAGE_CI_ISSUER",
            "https://token.actions.githubusercontent.com",
        ),
        ("DAGQ_IMAGE_CI_AUDIENCE", "dagq-images"),
        ("DAGQ_IMAGE_CI_REPOSITORY", "owner/images"),
        ("DAGQ_IMAGE_CI_WORKFLOW", ".github/workflows/release.yml"),
        ("DAGQ_IMAGE_CI_REF", "refs/heads/main"),
    ]
}
fn refused(pairs: &[(&str, &str)]) -> InvalidSettings {
    match read(pairs) {
        Err(e) => e,
        Ok(_) => panic!("configuration unexpectedly accepted"),
    }
}
#[test]
fn no_deployment_is_sqlite_and_socket_only() {
    let config = read(&[]).unwrap();
    assert_eq!(config.store, StoreSetting::Sqlite);
    assert!(config.http.is_none());
    assert_eq!(config.value("DAGQ_TIMEZONE"), Some("UTC"));
    assert_eq!(config.value("DAGQ_LISTEN"), Some("0.0.0.0:8080"));
    assert_eq!(config.value("DAGQ_ANTHROPIC_API_KEY"), None);
    assert_eq!(
        config.claude(),
        Deployment {
            team_api_key: false,
            prefer: credential::Order::PersonalFirst
        }
    );
}
#[test]
fn the_team_key_and_order_are_read_and_allowing_a_missing_key_names_no_value() {
    let config = read(&[
        ("DAGQ_ANTHROPIC_API_KEY", "sk-team-secret"),
        ("DAGQ_CLAUDE_PREFER", "team_first"),
    ])
    .unwrap();
    assert_eq!(
        config.claude(),
        Deployment {
            team_api_key: true,
            prefer: credential::Order::TeamFirst
        }
    );
    let kept = read(&[
        ("DAGQ_ANTHROPIC_API_KEY", "sk-team-secret"),
        ("DAGQ_CLAUDE_TEAM_API_KEY", "false"),
    ])
    .unwrap();
    assert!(!kept.claude().team_api_key);
    let error = refused(&[
        ("DAGQ_CLAUDE_TEAM_API_KEY", "true"),
        ("DAGQ_CLAUDE_PREFER", "secret-order"),
    ]);
    assert_eq!(
        error.0,
        BTreeSet::from(
            [
                "DAGQ_ANTHROPIC_API_KEY",
                "DAGQ_CLAUDE_PREFER",
                "DAGQ_CLAUDE_TEAM_API_KEY"
            ]
            .map(String::from)
        )
    );
    assert!(!error.to_string().contains("secret-order"));
    // A mounted key counts as present; its value is never in a refusal.
    let mounted = DeploymentConfig::read(
        |n| match n {
            "DAGQ_CLAUDE_TEAM_API_KEY" => Some("true".into()),
            "DAGQ_ANTHROPIC_API_KEY_FILE" => Some("key-path".into()),
            _ => None,
        },
        |_| Ok(b"sk-mounted\n".to_vec()),
    )
    .unwrap();
    assert!(mounted.claude().team_api_key);
}
#[test]
fn every_setting_accepts_a_mounted_file_and_rejects_both_forms() {
    let app_key = app_key();
    let (cert, key) = tls_pair();
    let mut pairs = postgres(&app_key);
    pairs.extend([
        ("DAGQ_OBJECT_STORE_ACCESS_KEY_ID", "id"),
        ("DAGQ_OBJECT_STORE_SECRET_ACCESS_KEY", "secret"),
        ("DAGQ_ANTHROPIC_API_KEY", "secret"),
        ("DAGQ_CLAUDE_TEAM_API_KEY", "true"),
        ("DAGQ_CLAUDE_PREFER", "team_first"),
        ("DAGQ_TIMEZONE", "Asia/Tokyo"),
        ("DAGQ_GITHUB_API_BASE_URL", "https://github.example/api/v3"),
        ("DAGQ_LISTEN", "127.0.0.1:0"),
        ("DAGQ_TLS_CERT", cert.as_str()),
        ("DAGQ_TLS_KEY", key.as_str()),
    ]);
    pairs.extend([
        (
            "DAGQ_ACTIONS_ISSUER",
            "https://token.actions.githubusercontent.com",
        ),
        ("DAGQ_ACTIONS_AUDIENCE", "dagq"),
        ("DAGQ_ACTIONS_REPOSITORY", "org/repo"),
        (
            "DAGQ_ACTIONS_WORKFLOW_REF",
            "org/repo/.github/workflows/runner.yml@refs/heads/main",
        ),
        ("DAGQ_ACTIONS_REF", "refs/heads/main"),
        ("DAGQ_ACTIONS_OWNER_ISSUER", "https://idp.example"),
        ("DAGQ_ACTIONS_OWNER_SUBJECT", "person"),
    ]);
    for &name in NAMES {
        let file_name = format!("{name}_FILE");
        let original = pairs.iter().find(|(n, _)| *n == name).unwrap().1;
        let config = DeploymentConfig::read(
            |n| {
                if n == name {
                    None
                } else if n == file_name {
                    Some("mounted-secret".into())
                } else {
                    pairs
                        .iter()
                        .find(|(k, _)| *k == n)
                        .map(|(_, v)| (*v).into())
                }
            },
            |path| {
                assert_eq!(path, std::path::Path::new("mounted-secret"));
                Ok(format!("{original}\r\n").into_bytes())
            },
        )
        .unwrap();
        assert_eq!(config.value(name), Some(original));
        let mut both = pairs.clone();
        both.push((&file_name, "a-secret-path"));
        let error = refused(&both);
        assert!(error.0.contains(name));
        assert!(error.0.contains(&file_name));
        assert!(!error.to_string().contains("a-secret-path"));
        assert!(!format!("{error:?}").contains("BEGIN"));
    }
}
#[test]
fn postgres_reports_all_missing_names_in_one_secret_free_error() {
    let error = refused(&[
        ("DAGQ_STORE", "postgres"),
        ("DAGQ_DATABASE_URL", "password-secret"),
        ("DAGQ_GITHUB_RUNNER_APP_ID", "bad-secret"),
        ("DAGQ_GITHUB_MAIN_APP_ID", "bad-secret"),
        ("DAGQ_TLS_CERT", "certificate-secret"),
    ]);
    for name in [
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
        "DAGQ_TLS_KEY",
    ] {
        assert!(error.0.contains(name), "{name}: {error}");
    }
    assert!(!error.to_string().contains("secret"));
    assert!(!format!("{error:?}").contains("secret"));
}
#[test]
fn complete_postgres_leaves_credentials_to_standard_discovery() {
    let app_key = app_key();
    let config = read(&postgres(&app_key)).unwrap();
    assert!(matches!(
        config.store,
        StoreSetting::Postgres {
            connection: Some(_)
        }
    ));
    assert!(config.http.is_some());
    assert_eq!(config.value("DAGQ_OBJECT_STORE_ACCESS_KEY_ID"), None);
    assert_eq!(config.value("DAGQ_OBJECT_STORE_SECRET_ACCESS_KEY"), None);
    for (a, b) in [
        (
            "DAGQ_OBJECT_STORE_ACCESS_KEY_ID",
            "DAGQ_OBJECT_STORE_SECRET_ACCESS_KEY",
        ),
        ("DAGQ_OIDC_ISSUER", "DAGQ_OIDC_AUDIENCE"),
        ("DAGQ_TLS_CERT", "DAGQ_TLS_KEY"),
    ] {
        let error = refused(&[(a, "secret")]);
        assert!(error.0.contains(a));
        assert!(error.0.contains(b));
        let error = refused(&[(b, "secret")]);
        assert!(error.0.contains(a));
        assert!(error.0.contains(b));
    }
}
#[test]
fn issuer_opens_http_and_a_valid_certificate_pair_selects_tls() {
    let (cert, key) = tls_pair();
    let (_, different_key) = tls_pair();
    let mut pairs = vec![
        ("DAGQ_OIDC_ISSUER", "https://identity.example.com"),
        ("DAGQ_OIDC_AUDIENCE", "queue"),
    ];
    let http = read(&pairs).unwrap().http.unwrap();
    assert_eq!(http.address, "0.0.0.0:8080".parse::<SocketAddr>().unwrap());
    assert!(http.tls.is_none());
    pairs.extend([
        ("DAGQ_TLS_CERT", cert.as_str()),
        ("DAGQ_TLS_KEY", key.as_str()),
        ("DAGQ_LISTEN", "[::1]:8443"),
    ]);
    let http = read(&pairs).unwrap().http.unwrap();
    assert!(http.tls.is_some());
    assert_eq!(http.address, "[::1]:8443".parse::<SocketAddr>().unwrap());
    assert!(
        read(&[
            ("DAGQ_TLS_CERT", cert.as_str()),
            ("DAGQ_TLS_KEY", key.as_str())
        ])
        .unwrap()
        .http
        .is_none()
    );
    assert!(
        refused(&[
            ("DAGQ_TLS_CERT", cert.as_str()),
            ("DAGQ_TLS_KEY", different_key.as_str())
        ])
        .0
        .contains("DAGQ_TLS_KEY")
    );
}
#[test]
fn invalid_values_and_contradictions_are_collected_without_values() {
    let mut pairs = postgres("invalid, replaced below before validation");
    for (name, value) in &mut pairs {
        if *name != "DAGQ_STORE" && *name != "DAGQ_DATABASE_URL" {
            *value = "";
        }
    }
    pairs.extend([
        ("DAGQ_TIMEZONE", "secret-not-a-zone"),
        ("DAGQ_LISTEN", "secret-not-an-address"),
    ]);
    let error = refused(&pairs);
    assert_eq!(error.0.len(), 17);
    assert!(!error.to_string().contains("secret"));
    for (name, value) in [
        ("DAGQ_STORE", "secret"),
        ("DAGQ_DATABASE_URL", "secret"),
        ("DAGQ_OIDC_ISSUER", "http://secret"),
        ("DAGQ_OBJECT_STORE_ENDPOINT", "secret"),
        ("DAGQ_GITHUB_RUNNER_APP_ID", "0"),
        ("DAGQ_GITHUB_REPOSITORY", "secret"),
        ("DAGQ_IMAGE_CI_REPOSITORY", "owner/repo/secret"),
        ("DAGQ_IMAGE_CI_REF", "secret"),
        ("DAGQ_TLS_CERT", "secret"),
        ("DAGQ_GITHUB_MAIN_APP_KEY", "secret"),
    ] {
        assert!(refused(&[(name, value)]).0.contains(name), "{name}");
    }
    let error = refused(&[
        ("DAGQ_GITHUB_MAIN_APP_ID", "1"),
        ("DAGQ_GITHUB_RUNNER_APP_ID", "1"),
    ]);
    assert!(error.0.contains("DAGQ_GITHUB_MAIN_APP_ID"));
    assert!(error.0.contains("DAGQ_GITHUB_RUNNER_APP_ID"));
}
#[test]
fn unreadable_non_text_and_empty_inputs_fail_closed() {
    use std::os::unix::ffi::OsStringExt;
    for &name in NAMES {
        let file_name = format!("{name}_FILE");
        for bytes in [Err(()), Ok(vec![0xff]), Ok(b"\n".to_vec())] {
            let result = DeploymentConfig::read(
                |n| (n == file_name).then(|| "secret-path".into()),
                |_| bytes.clone(),
            );
            let error = result.err().unwrap();
            assert!(error.0.contains(name));
            assert!(!error.to_string().contains("secret-path"));
        }
        assert!(
            DeploymentConfig::read(
                |n| (n == name).then(|| OsString::from_vec(vec![0xff])),
                |_| Err(())
            )
            .is_err()
        );
    }
}

#[test]
fn database_ca_is_optional_and_replaces_roots_with_every_file_certificate() {
    assert_eq!(database_ca(None, None, |_| panic!("no file")), Ok(None));
    let (cert, _) = tls_pair();
    let bundle = format!("{cert}{cert}");
    let ca = database_ca(Some("postgres"), Some("private-path".into()), |_| {
        Ok(bundle.as_bytes().to_vec())
    })
    .unwrap()
    .unwrap();
    assert_eq!(ca.len(), 2);
    let key = app_key();
    let mut pairs = postgres(&key);
    pairs.push((DATABASE_CA_FILE, "private-path"));
    let config = DeploymentConfig::read(
        |n| {
            pairs
                .iter()
                .find(|(name, _)| *name == n)
                .map(|(_, v)| OsString::from(v))
        },
        |_| Ok(bundle.as_bytes().to_vec()),
    )
    .unwrap();
    let StoreSetting::Postgres {
        connection: Some(connection),
    } = config.store
    else {
        panic!("expected Postgres connection");
    };
    assert_eq!(connection.ca_certificates(), Some(ca.as_slice()));
    let debug = format!("{connection:?}");
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("BEGIN"));
}

#[test]
fn database_ca_refuses_misplacement_unreadable_empty_and_invalid_files_without_secrets() {
    for kind in [None, Some("sqlite")] {
        assert_eq!(
            database_ca(kind, Some("private-path".into()), |_| panic!(
                "must not read"
            )),
            Err("requires the postgres store")
        );
    }
    for (contents, reason) in [
        (None, "cannot read PEM file"),
        (
            Some("private-contents"),
            "PEM file contains no certificates",
        ),
        (
            Some("-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----"),
            "invalid PEM certificate",
        ),
        (
            Some("-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----"),
            "invalid CA certificate",
        ),
    ] {
        let result = database_ca(Some("postgres"), Some("private-path".into()), |_| {
            contents.map(|s| s.as_bytes().to_vec()).ok_or(())
        });
        assert_eq!(result, Err(reason));
        let key = app_key();
        let mut pairs = postgres(&key);
        pairs.push((DATABASE_CA_FILE, "private-path"));
        let error = DeploymentConfig::read(
            |n| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == n)
                    .map(|(_, v)| OsString::from(v))
            },
            |_| contents.map(|s| s.as_bytes().to_vec()).ok_or(()),
        )
        .err()
        .expect("startup must fail");
        assert!(error.0.contains(DATABASE_CA_FILE));
        assert!(error.to_string().contains(reason));
        for secret in ["private-path", "private-contents", "secret", "BEGIN"] {
            assert!(!format!("{error:?} {error}").contains(secret));
        }
    }
    let error = refused(&[(DATABASE_CA_FILE, "private-path")]);
    assert!(error.to_string().contains("requires the postgres store"));
}

#[test]
fn actions_configuration_is_optional_complete_and_validated_before_startup() {
    let values = [
        (
            "DAGQ_ACTIONS_ISSUER",
            "https://token.actions.githubusercontent.com",
        ),
        ("DAGQ_ACTIONS_AUDIENCE", "dagq"),
        ("DAGQ_ACTIONS_REPOSITORY", "org/repo"),
        (
            "DAGQ_ACTIONS_WORKFLOW_REF",
            "org/repo/.github/workflows/runner.yml@refs/heads/main",
        ),
        ("DAGQ_ACTIONS_REF", "refs/heads/main"),
        ("DAGQ_ACTIONS_OWNER_ISSUER", "https://idp.example"),
        ("DAGQ_ACTIONS_OWNER_SUBJECT", "person"),
    ];
    assert!(read(&[]).unwrap().github_actions().is_none());
    let config = read(&values).unwrap();
    assert!(config.http.is_some());
    assert_eq!(config.github_actions().unwrap().owner.subject, "person");
    for missing in 0..values.len() {
        let partial: Vec<_> = values
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != missing)
            .map(|(_, pair)| *pair)
            .collect();
        assert!(read(&partial).is_err());
        assert!(read(&[values[missing]]).is_err());
        let mut bad = values;
        bad[missing].1 = "";
        assert!(read(&bad).is_err());
    }
    for (index, bad) in [
        (0, "https://other.example"),
        (2, "repo"),
        (3, "other/workflow"),
        (4, "main"),
        (5, "http://idp.example"),
    ] {
        let mut changed = values;
        changed[index].1 = bad;
        assert!(read(&changed).is_err());
    }
}

#[test]
fn image_ci_configuration_is_optional_complete_and_validated_before_startup() {
    let values = [
        (
            "DAGQ_IMAGE_CI_ISSUER",
            "https://token.actions.githubusercontent.com",
        ),
        ("DAGQ_IMAGE_CI_AUDIENCE", "dagq-images"),
        ("DAGQ_IMAGE_CI_REPOSITORY", "org/repo"),
        ("DAGQ_IMAGE_CI_WORKFLOW", ".github/workflows/image.yml"),
        ("DAGQ_IMAGE_CI_REF", "refs/heads/main"),
    ];
    // No setting: today's start, socket only and no binding.
    let none = read(&[]).unwrap();
    assert!(none.image_publish().is_none() && none.http.is_none());
    let config = read(&values).unwrap();
    assert!(config.http.is_some());
    let binding = config.image_publish().unwrap();
    assert_eq!(
        binding.workflow_ref,
        "org/repo/.github/workflows/image.yml@refs/heads/main"
    );
    assert_eq!(
        (binding.audience.as_str(), binding.git_ref.as_str()),
        ("dagq-images", "refs/heads/main")
    );
    for missing in 0..values.len() {
        let partial: Vec<_> = values
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != missing)
            .map(|(_, pair)| *pair)
            .collect();
        assert!(refused(&partial).0.contains(values[missing].0));
        assert!(read(&[values[missing]]).is_err());
        let mut empty = values;
        empty[missing].1 = "";
        assert!(read(&empty).is_err());
    }
    for (index, bad) in [
        (0, "https://other.example"),
        (2, "repo"),
        (3, "release.yml"),
        (3, ".github/workflows/"),
        (3, ".github/workflows/a@refs/heads/x"),
        (4, "main"),
        (4, "refs/tags/v1"),
        (4, "refs/heads/"),
    ] {
        let mut changed = values;
        changed[index].1 = bad;
        assert!(refused(&changed).0.contains(values[index].0), "{bad}");
    }
    // Distinct from the standby runners' binding: neither the audience
    // nor the workflow is shared.
    let runners = [
        (
            "DAGQ_ACTIONS_ISSUER",
            "https://token.actions.githubusercontent.com",
        ),
        ("DAGQ_ACTIONS_AUDIENCE", "dagq"),
        ("DAGQ_ACTIONS_REPOSITORY", "org/repo"),
        (
            "DAGQ_ACTIONS_WORKFLOW_REF",
            "org/repo/.github/workflows/runner.yml@refs/heads/main",
        ),
        ("DAGQ_ACTIONS_REF", "refs/heads/main"),
        ("DAGQ_ACTIONS_OWNER_ISSUER", "https://idp.example"),
        ("DAGQ_ACTIONS_OWNER_SUBJECT", "person"),
    ];
    let both: Vec<_> = values.iter().chain(&runners).copied().collect();
    assert!(read(&both).is_ok());
    let mut same_audience = both.clone();
    same_audience[1].1 = "dagq";
    assert!(refused(&same_audience).0.contains("DAGQ_IMAGE_CI_AUDIENCE"));
    let mut same_workflow = both.clone();
    same_workflow[3].1 = ".github/workflows/runner.yml";
    assert!(refused(&same_workflow).0.contains("DAGQ_IMAGE_CI_WORKFLOW"));
}
