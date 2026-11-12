//! Boundary: a real CLI talks to a loopback issuer and exchange endpoint and
//! saves one private file. No Keychain calls, real IdP or queue service.
use crate::common::{self, Bounded, actor::WithoutActor};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[test]
fn device_login_uses_loopback_http_and_private_terminal_credentials() {
    // Under no run directory: the file store refuses one, and a worker's
    // `TMPDIR` is in its run directory.
    let root = common::short_tempdir();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let server_base = base.clone();
    let server = thread::spawn(move || {
        let end = Instant::now() + Duration::from_secs(30);
        let mut requests = vec![];
        while !stopping.load(Ordering::SeqCst) && Instant::now() < end {
            let (mut stream, _) = match listener.accept() {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            // macOS inherits O_NONBLOCK from the listener on accept.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut head = vec![];
            while !head.ends_with(b"\r\n\r\n") && head.len() < 16384 {
                assert!(Instant::now() < end, "fixture request timed out");
                let mut byte = [0];
                assert_eq!(stream.read(&mut byte).unwrap(), 1);
                head.push(byte[0]);
            }
            let head = String::from_utf8(head).unwrap();
            let length = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            assert!(length <= 256 * 1024, "fixture request too large");
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            let body = String::from_utf8(body).unwrap();
            let path = head
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap();
            let reply = match path {
                "/.well-known/openid-configuration" => {
                    json!({"issuer":server_base,"device_authorization_endpoint":format!("{server_base}/device"),"token_endpoint":format!("{server_base}/token")})
                }
                "/device" => {
                    assert!(body.contains("scope=openid") && body.contains("client_id=test-cli"));
                    json!({"device_code":"device-secret","user_code":"USER-CODE","verification_uri":format!("{server_base}/verify"),"expires_in":60,"interval":1})
                }
                "/token" => {
                    assert!(body.contains("device_code=device-secret"));
                    json!({"id_token":"id-token-secret","access_token":"access-must-not-be-exchanged","refresh_token":"refresh-secret"})
                }
                "/api/use-case/exchange_token" => {
                    let sent: Value = serde_json::from_str(&body).unwrap();
                    assert_eq!(
                        sent,
                        json!({"api_version":2,"use_case":"exchange_token","params":{"jwt":"id-token-secret"}})
                    );
                    json!({"api_version":2,"min_api_version":2,"ok":true,"result":{"token":"dagq-secret","issuer":server_base,"subject":"subject","role":"member","expires_at":i64::MAX,"display_name":"A Person"}})
                }
                _ => panic!("unexpected endpoint {path}"),
            };
            requests.push(path.to_string());
            let reply = reply.to_string();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len()).unwrap();
        }
        requests
    });
    let run = |args: &[&str]| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dagq"));
        command
            .without_actor_env()
            .env_remove("DAGQ_SERVICE_SOCKET")
            .env_remove("DAGQ_SERVICE_URL")
            .env_remove("DAGQ_CONTROL_URL")
            .env_remove("DAGQ_LOGIN_ISSUER")
            .env_remove("DAGQ_LOGIN_CLIENT_ID")
            .env_remove("DAGQ_LOGIN_SCOPE")
            .env("DAGQ_CREDENTIAL_STORE", "file")
            .env("XDG_CONFIG_HOME", root.path())
            .current_dir(root.path())
            .args(args);
        command.bounded_output().unwrap()
    };
    let login = run(&[
        "login",
        "--control-url",
        &base,
        "--issuer",
        &base,
        "--client-id",
        "test-cli",
    ]);
    // Stop and join even if the subprocess failed, keeping the fixture bounded.
    stop.store(true, Ordering::SeqCst);
    let requests = server.join().unwrap();
    assert!(
        login.status.success(),
        "{}",
        String::from_utf8_lossy(&login.stdout)
    );
    assert_eq!(
        requests,
        vec![
            "/.well-known/openid-configuration",
            "/device",
            "/token",
            "/api/use-case/exchange_token"
        ]
    );
    for secret in [
        "device-secret",
        "id-token-secret",
        "access-must-not-be-exchanged",
        "refresh-secret",
        "dagq-secret",
    ] {
        assert!(!String::from_utf8_lossy(&login.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&login.stderr).contains(secret));
    }
    let directory = root.path().join("dagq/credentials");
    let file = directory.join("login.json");
    assert_eq!(
        std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let stored: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(stored["token"], "dagq-secret");
    assert_eq!(stored["destination"]["control_url"], base);
    let who = run(&["whoami"]);
    assert!(who.status.success());
    let shown: Value = serde_json::from_slice(&who.stdout).unwrap();
    assert_eq!(shown["control_url"], base);
    assert_eq!(shown["subject"], "subject");
    assert_eq!(shown["display_name"], "A Person");
    assert!(!shown.to_string().contains("dagq-secret"));
    assert!(run(&["logout"]).status.success());
    assert!(!file.exists());
    assert!(run(&["logout"]).status.success());
    let absent = run(&["whoami"]);
    assert_eq!(absent.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&absent.stderr).contains("dagq login"));
    assert!(!root.path().join("data.db").exists());
}
