//! `dagq-broker-client mcp`: the worker's MCP server on stdio. Each tool is
//! one broker operation of [`BrokerClient`]; a refusal of the broker comes
//! back as a tool error (`isError`) that carries the broker's error body as
//! it answered. JSON-RPC 2.0, one message per line, without batches.

use std::io::{self, BufRead, Write};

use dagq_broker_protocol::{ErrorBody, fs as fs_ops, git, process};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::client::{BrokerClient, ClientError};
use crate::{BUILD, NAME};

/// The MCP versions this server speaks, newest first; an `initialize` that
/// asks for another gets the newest.
pub const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// The longest a text field of an answer (a file's content, a diff, the
/// output of a command) is passed to the model; the rest is cut and the cut
/// is named in `mcp_cut`.
pub const TEXT_LIMIT_BYTES: usize = 40_000;

/// The most items of a list (a directory's entries, the log's commits)
/// passed to the model.
pub const ITEM_LIMIT: usize = 1_000;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// One tool: its name, what the model reads about it, and its input schema.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    schema: fn() -> Value,
}

impl Tool {
    pub fn input_schema(&self) -> Value {
        (self.schema)()
    }
}

const PATH_NOTE: &str = "Paths are relative to the run's worktree (the workspace); an absolute \
path, `..` out of it, a symlink out of it and `.git` are refused.";

/// The tools, in the order `tools/list` gives them.
pub const TOOLS: [Tool; 12] = [
    Tool {
        name: "read_file",
        description: "Read a text file of the run's worktree through the dagq broker. Paths are \
relative to the run's worktree (the workspace); an absolute path, `..` out of it, a symlink out \
of it and `.git` are refused. `offset` is the first line (0-based) and `limit` the number of \
lines. The broker reads at most its fs limit and says `truncated`; the content passed back is \
cut at 40000 bytes (named in `mcp_cut`), so read a long file in parts with offset and limit.",
        schema: || {
            object(
                &[
                    ("path", path_schema()),
                    ("offset", integer("The first line to read, 0-based.")),
                    ("limit", integer("How many lines to read.")),
                ],
                &["path"],
            )
        },
    },
    Tool {
        name: "list_dir",
        description: "List a directory of the run's worktree through the dagq broker: each \
entry's name, kind (file, dir, symlink, other) and size. Use `.` for the worktree itself. Paths \
are relative to the run's worktree; an absolute path, `..` out of it, a symlink out of it and \
`.git` are refused. At most 1000 entries are passed back (the rest is named in `mcp_cut`).",
        schema: || object(&[("path", path_schema())], &["path"]),
    },
    Tool {
        name: "write_file",
        description: "Write (create or replace) a text file of the run's worktree through the \
dagq broker, atomically. Paths are relative to the run's worktree; an absolute path, `..` out of \
it, a symlink out of it and `.git` are refused. `create_dirs` creates missing parent \
directories. Content over the broker's fs limit is refused (output_limit).",
        schema: || {
            object(
                &[
                    ("path", path_schema()),
                    ("content", string("The whole new content of the file.")),
                    (
                        "create_dirs",
                        boolean("Create the missing parent directories (default false)."),
                    ),
                ],
                &["path", "content"],
            )
        },
    },
    Tool {
        name: "edit_file",
        description: "Replace text in a file of the run's worktree through the dagq broker, like \
the built-in Edit: `old_string` must occur exactly once, or with `replace_all` every occurrence \
is replaced. Paths are relative to the run's worktree; an absolute path, `..` out of it, a \
symlink out of it and `.git` are refused. An edit that does not match is refused \
(invalid_request).",
        schema: || {
            object(
                &[
                    ("path", path_schema()),
                    ("old_string", string("The text to replace.")),
                    ("new_string", string("The text to replace it with.")),
                    (
                        "replace_all",
                        boolean("Replace every occurrence (default false)."),
                    ),
                ],
                &["path", "old_string", "new_string"],
            )
        },
    },
    Tool {
        name: "exec",
        description: "Run one program in the run's worktree through the dagq broker, without a \
shell: `argv[0]` must be on the broker's allowlist (capability_denied otherwise). `env` adds \
variables, of which the broker keeps only the names it allows. `timeout_secs` is capped by the \
broker's maximum, and a process past it is stopped (timeout). The broker refuses output over \
its limit (output_limit); stdout and stderr passed back are each cut at 40000 bytes (named in \
`mcp_cut`). A non-zero exit is not an error: read `exit_code`.",
        schema: || {
            object(
                &[
                    (
                        "argv",
                        json!({
                            "type": "array",
                            "items": {"type": "string"},
                            "minItems": 1,
                            "description": "The program and its arguments."
                        }),
                    ),
                    (
                        "env",
                        json!({
                            "type": "object",
                            "additionalProperties": {"type": "string"},
                            "description": "Env to add; only the names the broker allows are kept."
                        }),
                    ),
                    ("stdin", string("The standard input.")),
                    (
                        "timeout_secs",
                        integer("Seconds before the process is stopped, capped by the broker."),
                    ),
                ],
                &["argv"],
            )
        },
    },
    Tool {
        name: "git_status",
        description: "`git status` of the run's worktree through the dagq broker: the branch and \
each changed path with its two status letters.",
        schema: || object(&[], &[]),
    },
    Tool {
        name: "git_diff",
        description: "`git diff` of the run's worktree through the dagq broker: the worktree \
against the index, or with `staged` the index against HEAD, optionally only for `paths` \
(relative to the worktree). The diff passed back is cut at 40000 bytes (named in `mcp_cut`); \
narrow it with paths.",
        schema: || {
            object(
                &[
                    (
                        "staged",
                        boolean(
                            "The index against HEAD instead of the worktree against the index.",
                        ),
                    ),
                    ("paths", paths_schema("Only these paths.")),
                ],
                &[],
            )
        },
    },
    Tool {
        name: "git_log",
        description: "`git log` of the run branch through the dagq broker, newest first: each \
commit's id, author, time and subject. `limit` is 20 by default and at most 200.",
        schema: || {
            object(
                &[("limit", integer("How many commits (at most 200)."))],
                &[],
            )
        },
    },
    Tool {
        name: "git_show",
        description: "`git show` of one commit the run branch reaches (HEAD by default) through \
the dagq broker: its message and patch, optionally only for `paths` (relative to the worktree). \
The text passed back is cut at 40000 bytes (named in `mcp_cut`).",
        schema: || {
            object(
                &[
                    (
                        "commit",
                        string("A revision HEAD reaches; HEAD when absent."),
                    ),
                    ("paths", paths_schema("Only the patch of these paths.")),
                ],
                &[],
            )
        },
    },
    Tool {
        name: "git_add",
        description: "`git add` paths of the run's worktree through the dagq broker. Paths are \
relative to the run's worktree; paths out of it are refused.",
        schema: || {
            object(
                &[("paths", paths_schema("The paths to stage, at least one."))],
                &["paths"],
            )
        },
    },
    Tool {
        name: "git_commit",
        description: "`git commit` the staged changes on the run branch through the dagq broker, \
as the run's committer. Only the run's own branch; there is no push, fetch or remote.",
        schema: || object(&[("message", string("The commit message."))], &["message"]),
    },
    Tool {
        name: "git_restore",
        description: "`git restore` paths of the run's worktree through the dagq broker: the \
worktree back from the index, or with `staged` the index back from HEAD (unstage). Paths are \
relative to the run's worktree.",
        schema: || {
            object(
                &[
                    (
                        "staged",
                        boolean("Unstage instead of discarding worktree changes."),
                    ),
                    ("paths", paths_schema("The paths to restore, at least one.")),
                ],
                &["paths"],
            )
        },
    },
];

fn object(properties: &[(&str, Value)], required: &[&str]) -> Value {
    let properties: Map<String, Value> = properties
        .iter()
        .map(|(name, schema)| ((*name).to_owned(), schema.clone()))
        .collect();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn string(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

fn integer(description: &str) -> Value {
    json!({"type": "integer", "minimum": 0, "description": description})
}

fn boolean(description: &str) -> Value {
    json!({"type": "boolean", "description": description})
}

fn path_schema() -> Value {
    string(&format!(
        "A path relative to the run's worktree. {PATH_NOTE}"
    ))
}

fn paths_schema(description: &str) -> Value {
    json!({
        "type": "array",
        "items": {"type": "string"},
        "description": format!("{description} Relative to the run's worktree.")
    })
}

/// Serve MCP on `input` and `output` until `input` ends.
pub fn serve(
    client: &BrokerClient,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> io::Result<()> {
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        if input.read_until(b'\n', &mut bytes)? == 0 {
            return Ok(());
        }
        // A line that is not UTF-8 is answered as a parse error, not the end.
        let line = String::from_utf8_lossy(&bytes);
        if line.trim().is_empty() {
            continue;
        }
        if let Some(answer) = handle(client, &line) {
            let text = serde_json::to_string(&answer).expect("a JSON value serializes");
            writeln!(output, "{text}")?;
            output.flush()?;
        }
    }
}

/// The answer to one line, or `None` for a notification.
pub fn handle(client: &BrokerClient, line: &str) -> Option<Value> {
    let message: Value = match serde_json::from_str(line) {
        Ok(message) => message,
        Err(error) => {
            return Some(rpc_error(
                Value::Null,
                PARSE_ERROR,
                &format!("not JSON: {error}"),
            ));
        }
    };
    let Some(object) = message.as_object() else {
        return Some(rpc_error(
            Value::Null,
            INVALID_REQUEST,
            "a message is one JSON object (batches are not supported)",
        ));
    };
    let id = object.get("id").cloned();
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        // A response to a request of ours: this server sends none.
        return id.map(|id| rpc_error(id, INVALID_REQUEST, "no method"));
    };
    // A notification (`notifications/initialized`, `notifications/cancelled`)
    // needs no answer.
    let id = id?;
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    Some(match method {
        "initialize" => rpc_result(id, initialize(&params)),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, tools_list()),
        "tools/call" => match tools_call(client, &params) {
            Ok(result) => rpc_result(id, result),
            Err(message) => rpc_error(id, INVALID_PARAMS, &message),
        },
        other => rpc_error(id, METHOD_NOT_FOUND, &format!("unknown method `{other}`")),
    })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = PROTOCOL_VERSIONS
        .iter()
        .find(|version| Some(**version) == asked)
        .unwrap_or(&PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": NAME, "version": BUILD},
        "instructions": "The dagq resource broker's tools: files, allowed commands and git of \
    this run's worktree. Paths are relative to the worktree. A refusal of the broker is a tool \
    error whose `error.code` says why (unauthorized, capability_denied, workspace_violation, \
    timeout, output_limit, backend_error, invalid_request)."
    })
}

fn tools_list() -> Value {
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.input_schema()
            })
        })
        .collect();
    json!({"tools": tools})
}

/// A tool's answer: `Ok` for the operation's answer, `Err` for a tool error
/// (the broker's or the client's). An unknown tool or params of the wrong
/// shape are a protocol error (`Err` of this function).
fn tools_call(client: &BrokerClient, params: &Value) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or("tools/call needs the tool's `name`")?;
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => Value::Object(Map::new()),
        Some(arguments @ Value::Object(_)) => arguments.clone(),
        Some(_) => return Err("`arguments` is not an object".to_owned()),
    };
    let answer = match name {
        "read_file" => run(arguments, |q: fs_ops::ReadRequest| client.fs_read(&q)),
        "list_dir" => run(arguments, |q: fs_ops::ListRequest| client.fs_list(&q)),
        "write_file" => run(arguments, |q: fs_ops::WriteRequest| client.fs_write(&q)),
        "edit_file" => run(arguments, |q: fs_ops::EditRequest| client.fs_edit(&q)),
        "exec" => run(arguments, |q: process::ExecRequest| client.exec(&q)),
        "git_status" => run(arguments, |_: git::StatusRequest| client.git_status()),
        "git_diff" => run(arguments, |q: git::DiffRequest| client.git_diff(&q)),
        "git_log" => run(arguments, |q: git::LogRequest| client.git_log(&q)),
        "git_show" => run(arguments, |q: git::ShowRequest| client.git_show(&q)),
        "git_add" => run(arguments, |q: git::AddRequest| client.git_add(&q)),
        "git_commit" => run(arguments, |q: git::CommitRequest| client.git_commit(&q)),
        "git_restore" => run(arguments, |q: git::RestoreRequest| client.git_restore(&q)),
        other => return Err(format!("unknown tool `{other}`")),
    };
    Ok(match answer {
        Ok(value) => {
            let value = cut(value);
            json!({"content": [text(&value)], "isError": false})
        }
        Err(error) => {
            json!({"content": [text(&error)], "structuredContent": error, "isError": true})
        }
    })
}

/// Read `arguments` as the operation's request and call it; the answer as
/// JSON, or the tool error.
fn run<Q: DeserializeOwned, A: Serialize>(
    arguments: Value,
    call: impl FnOnce(Q) -> Result<A, ClientError>,
) -> Result<Value, Value> {
    let request: Q = serde_json::from_value(arguments)
        .map_err(|error| client_error("invalid_arguments", &format!("the arguments: {error}")))?;
    match call(request) {
        Ok(answer) => Ok(serde_json::to_value(answer).expect("a protocol value serializes")),
        Err(ClientError::Broker { error, .. }) => {
            Err(serde_json::to_value(ErrorBody { error }).expect("an error body serializes"))
        }
        Err(ClientError::Config(message)) => Err(client_error("config", &message)),
        Err(ClientError::Transport(message)) => Err(client_error("transport", &message)),
        Err(ClientError::Protocol(message)) => Err(client_error("protocol", &message)),
    }
}

/// A failure on the client's side, apart from the broker's error codes.
fn client_error(kind: &str, message: &str) -> Value {
    json!({"client_error": {"kind": kind, "message": message}})
}

fn text(value: &Value) -> Value {
    json!({
        "type": "text",
        "text": serde_json::to_string(value).expect("a JSON value serializes")
    })
}

/// `answer` with each top-level text field cut at [`TEXT_LIMIT_BYTES`] and
/// each top-level list at [`ITEM_LIMIT`]; the cuts are named in `mcp_cut`
/// (`{"<field>": {"kept": N, "total": M}}`, bytes or items).
pub fn cut(answer: Value) -> Value {
    let Value::Object(mut fields) = answer else {
        return answer;
    };
    let mut cuts = Map::new();
    for (name, value) in fields.iter_mut() {
        match value {
            Value::String(text) if text.len() > TEXT_LIMIT_BYTES => {
                let total = text.len();
                let mut kept = TEXT_LIMIT_BYTES;
                while !text.is_char_boundary(kept) {
                    kept -= 1;
                }
                text.truncate(kept);
                cuts.insert(name.clone(), json!({"kept": kept, "total": total}));
            }
            Value::Array(items) if items.len() > ITEM_LIMIT => {
                let total = items.len();
                items.truncate(ITEM_LIMIT);
                cuts.insert(name.clone(), json!({"kept": ITEM_LIMIT, "total": total}));
            }
            _ => {}
        }
    }
    if !cuts.is_empty() {
        fields.insert("mcp_cut".to_owned(), Value::Object(cuts));
    }
    Value::Object(fields)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Endpoint;
    use std::time::Duration;

    /// A client of a port nobody listens on, with a token file that is not
    /// there: every call fails on the client's side.
    fn offline() -> BrokerClient {
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        BrokerClient::new(
            Endpoint::parse(&format!("http://{addr}")).unwrap(),
            Some("/nonexistent/dagq-broker-token".into()),
        )
        .with_read_timeout(Duration::from_secs(5))
    }

    fn ask(line: &str) -> Value {
        handle(&offline(), line).expect("an answer")
    }

    #[test]
    fn initialize_answers_the_asked_version_it_speaks_or_its_newest() {
        let answer = ask(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
        );
        assert_eq!(answer["id"], 1);
        assert_eq!(answer["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(answer["result"]["serverInfo"]["name"], NAME);
        assert_eq!(
            answer["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
        let answer = ask(
            r#"{"jsonrpc":"2.0","id":"a","method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
        );
        assert_eq!(answer["id"], "a");
        assert_eq!(answer["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert_eq!(
            ask(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#)["result"],
            json!({})
        );
    }

    #[test]
    fn notifications_get_no_answer_and_bad_messages_a_json_rpc_error() {
        let client = offline();
        assert!(
            handle(
                &client,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
            )
            .is_none()
        );
        assert!(handle(&client, r#"{"jsonrpc":"2.0","result":{}}"#).is_none());
        for (line, code) in [
            ("{", PARSE_ERROR),
            ("[1]", INVALID_REQUEST),
            (r#"{"jsonrpc":"2.0","id":3,"result":{}}"#, INVALID_REQUEST),
            (
                r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#,
                METHOD_NOT_FOUND,
            ),
            (
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{}}"#,
                INVALID_PARAMS,
            ),
            (
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"push"}}"#,
                INVALID_PARAMS,
            ),
            (
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"git_status","arguments":[1]}}"#,
                INVALID_PARAMS,
            ),
        ] {
            let answer = handle(&client, line).unwrap();
            assert_eq!(answer["error"]["code"], code, "{line}: {answer}");
            assert!(answer.get("result").is_none());
        }
    }

    #[test]
    fn every_tool_is_listed_with_a_closed_object_schema() {
        let answer = ask(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        let tools = answer["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools
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
                "git_restore"
            ]
        );
        for tool in tools {
            let schema = &tool["inputSchema"];
            assert_eq!(schema["type"], "object", "{tool}");
            assert_eq!(schema["additionalProperties"], false, "{tool}");
            for required in schema["required"].as_array().unwrap() {
                assert!(
                    schema["properties"]
                        .get(required.as_str().unwrap())
                        .is_some()
                );
            }
            assert!(tool["description"].as_str().unwrap().contains("broker"));
        }
    }

    fn call(tool: &str, arguments: Value) -> Value {
        let line = json!({"jsonrpc":"2.0","id":7,"method":"tools/call",
            "params":{"name": tool, "arguments": arguments}});
        ask(&line.to_string())["result"].clone()
    }

    #[test]
    fn bad_arguments_and_client_failures_are_tool_errors() {
        let result = call("read_file", json!({"path": "a", "mode": "x"}));
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["structuredContent"]["client_error"]["kind"],
            "invalid_arguments"
        );
        let result = call("write_file", json!({"path": "a"}));
        assert_eq!(
            result["structuredContent"]["client_error"]["kind"],
            "invalid_arguments"
        );
        // No token file: fails before connecting.
        let result = call("git_status", Value::Null);
        assert_eq!(result["isError"], true);
        assert_eq!(
            result["structuredContent"]["client_error"]["kind"],
            "config"
        );
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, result["structuredContent"]);
    }

    #[test]
    fn long_texts_and_lists_are_cut_and_the_cut_is_named() {
        let long = "é".repeat(TEXT_LIMIT_BYTES); // two bytes each
        let answer = cut(json!({
            "stdout": long,
            "stderr": "short",
            "entries": vec![1; ITEM_LIMIT + 5],
            "exit_code": 0
        }));
        assert_eq!(answer["stdout"].as_str().unwrap().len(), TEXT_LIMIT_BYTES);
        assert_eq!(answer["stderr"], "short");
        assert_eq!(answer["entries"].as_array().unwrap().len(), ITEM_LIMIT);
        assert_eq!(
            answer["mcp_cut"],
            json!({
                "stdout": {"kept": TEXT_LIMIT_BYTES, "total": TEXT_LIMIT_BYTES * 2},
                "entries": {"kept": ITEM_LIMIT, "total": ITEM_LIMIT + 5}
            })
        );
        let odd = format!("a{}", "é".repeat(TEXT_LIMIT_BYTES));
        let answer = cut(json!({ "diff": odd }));
        assert_eq!(answer["mcp_cut"]["diff"]["kept"], TEXT_LIMIT_BYTES - 1);
        assert_eq!(cut(json!({"a": "b"})), json!({"a": "b"}));
        assert_eq!(cut(json!(3)), json!(3));
    }

    #[test]
    fn serve_answers_line_by_line_until_the_input_ends() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
            "\n\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#,
            "\n"
        );
        // A line that is not UTF-8 is a parse error, and serving goes on.
        let input = [&b"\xff\n"[..], input.as_bytes()].concat();
        let mut output = Vec::new();
        serve(&offline(), &mut &input[..], &mut output).unwrap();
        let lines: Vec<Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["error"]["code"], PARSE_ERROR);
        assert_eq!(lines[1]["id"], 1);
        assert_eq!(lines[2]["id"], 2);
    }
}
