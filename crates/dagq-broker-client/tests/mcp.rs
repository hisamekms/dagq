//! `dagq-broker-client mcp` on stdio against a broker started in the test
//! (in process, no podman): MCP's initialize, tools/list and tools/call
//! flow as JSON lines, each tool does its broker operation in the run's
//! worktree, a refusal of the broker is a tool error carrying its error body,
//! and a long answer is cut with the cut named.

mod common;

use common::*;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc::{self, Receiver};

use dagq_broker_client::mcp::TEXT_LIMIT_BYTES;
use dagq_broker_client::{TOKEN_FILE_ENV, URL_ENV};
use dagq_broker_protocol::{BrokerCapability, ErrorCode};
use serde_json::{Value, json};

/// A running `dagq-broker-client mcp`.
struct Mcp {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

impl Mcp {
    fn start(broker: &Broker, token_file: &Path) -> Self {
        let mut child = client_command(std::env::var_os("LLVM_PROFILE_FILE"))
            .arg("mcp")
            .env(URL_ENV, &broker.url)
            .env(TOKEN_FILE_ENV, token_file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if send.send(line).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
            next_id: 0,
        }
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Send the request `method` and wait at most [`LIMIT`] for its answer.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let line = self
            .lines
            .recv_timeout(LIMIT)
            .unwrap_or_else(|_| panic!("no answer to {method}"));
        let answer: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(answer["jsonrpc"], "2.0");
        assert_eq!(answer["id"], id, "{answer}");
        answer
    }

    /// `tools/call` of `tool`: whether it is a tool error, and the JSON its
    /// text content holds.
    fn call(&mut self, tool: &str, arguments: Value) -> (bool, Value) {
        let answer = self.request("tools/call", json!({"name": tool, "arguments": arguments}));
        let result = &answer["result"];
        let content = result["content"].as_array().expect("content");
        assert_eq!(content.len(), 1, "{answer}");
        assert_eq!(content[0]["type"], "text");
        let value: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        let is_error = result["isError"].as_bool().unwrap();
        if is_error {
            assert_eq!(result["structuredContent"], value);
        }
        (is_error, value)
    }

    fn ok(&mut self, tool: &str, arguments: Value) -> Value {
        let (is_error, value) = self.call(tool, arguments);
        assert!(!is_error, "{tool}: {value}");
        value
    }

    /// The broker's refusal of `tool`: its error body, with `code`.
    fn refused(&mut self, tool: &str, arguments: Value, code: ErrorCode) -> Value {
        let (is_error, value) = self.call(tool, arguments);
        assert!(is_error, "{tool}: {value}");
        assert_eq!(value["error"]["code"], code.as_str(), "{tool}: {value}");
        assert!(!value["error"]["message"].as_str().unwrap().is_empty());
        assert!(!value["error"]["request_id"].as_str().unwrap().is_empty());
        value
    }

    fn initialize(&mut self) {
        let answer = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }),
        );
        assert_eq!(answer["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(answer["result"]["serverInfo"]["name"], "dagq-broker-client");
        assert!(answer["result"]["capabilities"]["tools"].is_object());
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    }

    /// Close stdin and wait at most [`LIMIT`] for the server to exit.
    fn finish(self) -> Option<i32> {
        let Self {
            mut child, stdin, ..
        } = self;
        drop(stdin);
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(child.wait());
        });
        receive
            .recv_timeout(LIMIT)
            .expect("the MCP server did not exit")
            .unwrap()
            .code()
    }
}

#[test]
fn each_tool_does_its_broker_operation_in_the_run_worktree() {
    let broker = Broker::start();
    let token_file = broker.full_token_file("jti-mcp");
    let mut mcp = Mcp::start(&broker, &token_file);
    mcp.initialize();

    let tools = mcp.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "read_file",
            "list_dir",
            "write_file",
            "edit_file",
            "exec",
            "git_status",
            "git_diff",
            "git_log",
            "git_show",
            "git_add",
            "git_commit",
            "git_restore",
            "package_install"
        ]
    );

    let written = mcp.ok(
        "write_file",
        json!({"path": "dir/a.txt", "content": "one\ntwo\n", "create_dirs": true}),
    );
    assert_eq!(written, json!({"bytes": 8}));
    let edited = mcp.ok(
        "edit_file",
        json!({"path": "dir/a.txt", "old_string": "two", "new_string": "three"}),
    );
    assert_eq!(edited, json!({"replacements": 1}));
    let read = mcp.ok("read_file", json!({"path": "dir/a.txt", "offset": 1}));
    assert_eq!(
        read,
        json!({"content": "three\n", "lines": 1, "truncated": false})
    );
    let listed = mcp.ok("list_dir", json!({"path": "dir"}));
    assert_eq!(listed["entries"][0]["name"], "a.txt");
    assert_eq!(
        fs::read_to_string(broker.workspace().join("dir/a.txt")).unwrap(),
        "one\nthree\n"
    );

    let executed = mcp.ok(
        "exec",
        json!({"argv": ["sh", "-c", "cat dir/a.txt; cat; exit 3"], "stdin": "in\n", "timeout_secs": 10}),
    );
    assert_eq!(executed["exit_code"], 3);
    assert_eq!(executed["stdout"], "one\nthree\nin\n");

    let status = mcp.ok("git_status", json!({}));
    assert_eq!(status["branch"], "dagq/run-1");
    mcp.ok("git_add", json!({"paths": ["dir/a.txt"]}));
    let diff = mcp.ok("git_diff", json!({"staged": true}));
    assert!(diff["diff"].as_str().unwrap().contains("+three"), "{diff}");
    let commit = mcp.ok("git_commit", json!({"message": "add a.txt"}))["commit"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        host_git(&broker.root().join("repo"), &["rev-parse", "dagq/run-1"]),
        commit
    );
    let log = mcp.ok("git_log", json!({"limit": 1}));
    assert_eq!(log["commits"][0]["commit"], commit.as_str());
    let shown = mcp.ok("git_show", json!({"paths": ["dir/a.txt"]}));
    assert_eq!(shown["commit"], commit.as_str());
    assert!(shown["show"].as_str().unwrap().contains("add a.txt"));
    fs::write(broker.workspace().join("dir/a.txt"), "scratch\n").unwrap();
    mcp.ok("git_restore", json!({"paths": ["dir/a.txt"]}));
    assert_eq!(
        fs::read_to_string(broker.workspace().join("dir/a.txt")).unwrap(),
        "one\nthree\n"
    );
    assert_eq!(mcp.finish(), Some(0));

    // Each call reached the broker as the run, and the token is not in the
    // audit.
    let audit = broker.audit_text();
    let token = fs::read_to_string(&token_file).unwrap();
    assert!(!audit.contains(token.trim()));
    assert_eq!(audit.matches("\"run_id\":\"run-1\"").count(), 12, "{audit}");
}

#[test]
fn the_brokers_refusals_are_tool_errors_with_its_error_body() {
    let broker = Broker::start();
    fs::write(broker.root().join("outside"), "secret\n").unwrap();
    let token_file = broker.full_token_file("jti-full");
    let mut mcp = Mcp::start(&broker, &token_file);
    mcp.initialize();
    for path in ["../outside", "/etc/hosts", ".git/HEAD"] {
        mcp.refused(
            "read_file",
            json!({"path": path}),
            ErrorCode::WorkspaceViolation,
        );
    }
    std::os::unix::fs::symlink(
        broker.root().join("outside"),
        broker.workspace().join("out"),
    )
    .unwrap();
    mcp.refused(
        "read_file",
        json!({"path": "out"}),
        ErrorCode::WorkspaceViolation,
    );
    mcp.refused(
        "exec",
        json!({"argv": ["cat", "README"]}),
        ErrorCode::CapabilityDenied,
    );
    mcp.refused(
        "edit_file",
        json!({"path": "README", "old_string": "absent", "new_string": "x"}),
        ErrorCode::InvalidRequest,
    );
    // Arguments of the wrong shape never reach the broker.
    let (is_error, value) = mcp.call("git_add", json!({"paths": "README"}));
    assert!(is_error);
    assert_eq!(value["client_error"]["kind"], "invalid_arguments");
    // An unknown tool is a JSON-RPC error.
    let answer = mcp.request("tools/call", json!({"name": "git_push", "arguments": {}}));
    assert_eq!(answer["error"]["code"], -32602);
    assert_eq!(mcp.finish(), Some(0));

    // A token without git.write, and an expired one.
    let read_only = broker.token_file(
        &broker.claims("jti-ro", &[BrokerCapability::FsRead], now() + 3600),
        true,
    );
    let mut mcp = Mcp::start(&broker, &read_only);
    mcp.initialize();
    mcp.ok("read_file", json!({"path": "README"}));
    mcp.refused(
        "git_commit",
        json!({"message": "m"}),
        ErrorCode::CapabilityDenied,
    );
    mcp.refused(
        "write_file",
        json!({"path": "b", "content": "b"}),
        ErrorCode::CapabilityDenied,
    );
    assert_eq!(mcp.finish(), Some(0));
    let expired = broker.token_file(
        &broker.claims("jti-old", &BrokerCapability::ALL, now() - 1),
        true,
    );
    let mut mcp = Mcp::start(&broker, &expired);
    mcp.initialize();
    mcp.refused("git_status", json!({}), ErrorCode::Unauthorized);
    assert_eq!(mcp.finish(), Some(0));
    assert!(!fs::exists(broker.workspace().join("b")).unwrap());
}

#[test]
fn a_long_answer_is_cut_and_the_cut_is_named() {
    let broker = Broker::start();
    let token_file = broker.full_token_file("jti-long");
    let mut mcp = Mcp::start(&broker, &token_file);
    mcp.initialize();
    let total = TEXT_LIMIT_BYTES * 2 + 5;
    let executed = mcp.ok(
        "exec",
        json!({"argv": ["sh", "-c", format!("head -c {total} /dev/zero | tr '\\0' a")]}),
    );
    assert_eq!(executed["stdout"].as_str().unwrap().len(), TEXT_LIMIT_BYTES);
    assert_eq!(
        executed["mcp_cut"],
        json!({"stdout": {"kept": TEXT_LIMIT_BYTES, "total": total}})
    );
    assert_eq!(executed["exit_code"], 0);
    fs::write(broker.workspace().join("big"), "b".repeat(total)).unwrap();
    let read = mcp.ok("read_file", json!({"path": "big"}));
    assert_eq!(read["mcp_cut"]["content"]["total"], total);
    let small = mcp.ok("read_file", json!({"path": "README"}));
    assert!(small.get("mcp_cut").is_none(), "{small}");
    assert_eq!(mcp.finish(), Some(0));
}

#[test]
fn the_mcp_server_does_not_start_without_a_broker_url() {
    let output = cli(&["mcp"], &[], "");
    assert_eq!(output.code, Some(3), "{}", output.stderr);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.contains(URL_ENV), "{}", output.stderr);
}

#[test]
fn package_install_runs_only_a_configured_command() {
    let broker = Broker::start();
    let token_file = broker.full_token_file("jti-package");
    let mut mcp = Mcp::start(&broker, &token_file);
    mcp.initialize();
    let installed = mcp.ok(
        "package_install",
        json!({"name": "deps", "timeout_secs": 10}),
    );
    assert_eq!(installed["exit_code"], 0);
    assert_eq!(installed["stdout"], "deps installed\n");
    assert_eq!(
        fs::read_to_string(broker.workspace().join("installed.txt")).unwrap(),
        "installed\n"
    );
    for name in ["sh", "cargo-fetch"] {
        mcp.refused(
            "package_install",
            json!({ "name": name }),
            ErrorCode::CapabilityDenied,
        );
    }
    // An argv is not an argument of the tool.
    let (is_error, value) = mcp.call(
        "package_install",
        json!({"name": "deps", "argv": ["sh", "-c", "touch ran"]}),
    );
    assert!(is_error);
    assert_eq!(value["client_error"]["kind"], "invalid_arguments");
    assert_eq!(mcp.finish(), Some(0));

    // A token without package.install runs nothing.
    fs::remove_file(broker.workspace().join("installed.txt")).unwrap();
    let no_package = broker.token_file(
        &broker.claims(
            "jti-no-package",
            &[BrokerCapability::ProcessExec],
            now() + 3600,
        ),
        true,
    );
    let mut mcp = Mcp::start(&broker, &no_package);
    mcp.initialize();
    mcp.refused(
        "package_install",
        json!({"name": "deps"}),
        ErrorCode::CapabilityDenied,
    );
    assert_eq!(mcp.finish(), Some(0));
    assert!(!fs::exists(broker.workspace().join("installed.txt")).unwrap());
    assert!(!fs::exists(broker.workspace().join("ran")).unwrap());
    let audit = broker.audit_text();
    assert_eq!(
        audit.matches("\"op\":\"package.install\"").count(),
        4,
        "{audit}"
    );
}
