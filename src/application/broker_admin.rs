//! The broker's reading commands ([Broker] management commands and audit):
//! [`logs`] prints the tail of the queue's container's `podman logs`, and
//! [`audit`] reads the broker's audit day files and keeps the lines a run,
//! a task and a time range name. Neither changes anything: [`logs`] starts
//! no machine and makes no container, and [`audit`] reads the files as the
//! broker wrote them, without taking them into the queue DB.
//!
//! [Broker]: ../../docs/design/broker.md

use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use super::broker::{
    BrokerFailure, BrokerResult, FailureCode, MachineState, Podman, container_status,
    machine_status,
};
use crate::domain::stats::rfc3339_millis;

/// The lines `dagq broker logs` prints without `--tail`.
pub const DEFAULT_LOG_TAIL: u32 = 200;
/// The lines `dagq broker audit` keeps without `--limit`: the latest ones.
pub const DEFAULT_AUDIT_LIMIT: usize = 1000;

/// What `dagq broker logs` prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogsReport {
    pub container: String,
    pub tail: u32,
    /// What the container wrote to its stdout, the last `tail` lines.
    pub stdout: String,
    /// What the container wrote to its stderr, the last `tail` lines.
    pub stderr: String,
}

/// The arguments of `podman logs` for the last `tail` lines of `container`
/// on `machine`.
pub fn logs_args(machine: &str, container: &str, tail: u32) -> Vec<String> {
    [
        "--connection",
        machine,
        "logs",
        "--tail",
        &tail.to_string(),
        container,
    ]
    .iter()
    .map(|word| (*word).to_owned())
    .collect()
}

/// The last `tail` lines the container `container` on `machine` logged.
/// It only reads: a machine that is missing or stopped
/// (`machine_missing`, `machine_stopped`) and a container that is not there
/// (`container_missing`) are failures, not started or made.
pub fn logs(
    podman: &dyn Podman,
    machine: &str,
    container: &str,
    tail: u32,
) -> BrokerResult<LogsReport> {
    match machine_status(podman, machine)?.state {
        MachineState::Missing => {
            return Err(BrokerFailure::new(
                FailureCode::MachineMissing,
                format!(
                    "dagq's Podman machine {machine} does not exist: dagq broker start makes it"
                ),
            ));
        }
        MachineState::Stopped => {
            return Err(BrokerFailure::new(
                FailureCode::MachineStopped,
                format!("dagq's Podman machine {machine} is stopped: dagq broker start starts it"),
            ));
        }
        MachineState::Running => {}
    }
    if container_status(podman, machine, container)?.is_none() {
        return Err(BrokerFailure::new(
            FailureCode::ContainerMissing,
            format!(
                "the broker's container {container} does not exist: dagq broker start makes it"
            ),
        ));
    }
    let output = podman.run(&logs_args(machine, container, tail))?;
    if !output.success {
        let words = match output.stderr.trim() {
            "" => output.stdout.trim(),
            stderr => stderr,
        };
        return Err(BrokerFailure::new(
            FailureCode::PodmanFailed,
            format!("podman logs {container} failed: {words}"),
        ));
    }
    Ok(LogsReport {
        container: container.to_owned(),
        tail,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// Which audit lines `dagq broker audit` keeps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditQuery {
    pub run: Option<String>,
    pub task: Option<u64>,
    /// Unix milliseconds: only lines at or after it.
    pub since: Option<i64>,
    /// Unix milliseconds: only lines before it.
    pub until: Option<i64>,
    /// The latest lines kept; [`DEFAULT_AUDIT_LIMIT`] when `None`.
    pub limit: Option<usize>,
}

/// What `dagq broker audit` prints.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditReport {
    /// The kept lines oldest first, each the broker's line as it wrote it.
    pub entries: Vec<Value>,
    /// Lines that are not a JSON object with a readable `ts` (a broken
    /// line, or the day file's last line cut short while written).
    pub skipped: usize,
    /// Lines that matched but fell outside `--limit` (the oldest ones).
    pub dropped: usize,
}

/// The UTC date (`YYYY-MM-DD`) of Unix milliseconds.
fn utc_date(millis: i64) -> String {
    let days = millis.div_euclid(86_400_000);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn is_date(text: &str) -> bool {
    text.len() == 10
        && text.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

/// The audit lines in `dir` (`<queue dir>/broker/audit`) that `query`
/// keeps. Only the day files (`<YYYY-MM-DD>.jsonl`) whose date lies within
/// `--since` and `--until` are read; a missing `dir` is no line.
pub fn audit(dir: &Path, query: &AuditQuery) -> std::io::Result<AuditReport> {
    let first = query.since.map(utc_date);
    // `--until` is exclusive: at midnight, that day has no line to keep.
    let last = query.until.map(|until| utc_date(until - 1));
    let mut days = Vec::new();
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries {
                let name = entry?.file_name();
                let Some(date) = name.to_str().and_then(|name| name.strip_suffix(".jsonl")) else {
                    continue;
                };
                if is_date(date)
                    && first.as_deref().is_none_or(|first| date >= first)
                    && last.as_deref().is_none_or(|last| date <= last)
                {
                    days.push(date.to_owned());
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    days.sort();
    let mut entries = Vec::new();
    let mut skipped = 0;
    for day in days {
        // The broker removes old day files when it starts.
        let bytes = match std::fs::read(dir.join(format!("{day}.jsonl"))) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let Some((entry, at)) = serde_json::from_slice::<Value>(line)
                .ok()
                .filter(Value::is_object)
                .and_then(|entry| {
                    let at = entry["ts"].as_str().and_then(rfc3339_millis)?;
                    Some((entry, at))
                })
            else {
                skipped += 1;
                continue;
            };
            let keep = query
                .run
                .as_deref()
                .is_none_or(|run| entry["run_id"].as_str() == Some(run))
                && query
                    .task
                    .is_none_or(|task| entry["task_id"].as_u64() == Some(task))
                && query.since.is_none_or(|since| at >= since)
                && query.until.is_none_or(|until| at < until);
            if keep {
                entries.push(entry);
            }
        }
    }
    let limit = query.limit.unwrap_or(DEFAULT_AUDIT_LIMIT);
    let dropped = entries.len().saturating_sub(limit);
    entries.drain(..dropped);
    Ok(AuditReport {
        entries,
        skipped,
        dropped,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::super::broker::PodmanOutput;
    use super::*;

    /// A podman that answers by the start of the arguments and records
    /// every command.
    #[derive(Default)]
    struct Stub {
        missing: bool,
        machines: &'static str,
        container_exists: bool,
        logs: Option<PodmanOutput>,
        calls: RefCell<Vec<String>>,
    }

    fn out(success: bool, code: i32, stdout: &str, stderr: &str) -> PodmanOutput {
        PodmanOutput {
            success,
            code: Some(code),
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        }
    }

    impl Podman for Stub {
        fn run(&self, args: &[String]) -> BrokerResult<PodmanOutput> {
            if self.missing {
                return Err(BrokerFailure::new(
                    FailureCode::PodmanMissing,
                    "podman was not found",
                ));
            }
            let call = args.join(" ");
            self.calls.borrow_mut().push(call.clone());
            Ok(if call.starts_with("machine list") {
                out(true, 0, self.machines, "")
            } else if call.contains(" container exists ") {
                if self.container_exists {
                    out(true, 0, "", "")
                } else {
                    out(false, 1, "", "")
                }
            } else if call.contains(" container inspect ") {
                out(
                    true,
                    0,
                    r#"[{"State":{"Running":false},"ImageName":"i"}]"#,
                    "",
                )
            } else if call.contains(" logs ") {
                self.logs.clone().unwrap_or_else(|| out(true, 0, "", ""))
            } else {
                panic!("unexpected podman {call}")
            })
        }
    }

    const RUNNING: &str = r#"[{"Name":"dagq","Running":true}]"#;
    const STOPPED: &str = r#"[{"Name":"dagq","Running":false}]"#;

    #[test]
    fn logs_name_the_container_and_the_tail_on_dagqs_connection() {
        let podman = Stub {
            machines: RUNNING,
            container_exists: true,
            logs: Some(out(true, 0, "out\n", "listening on 0.0.0.0:8750\n")),
            ..Stub::default()
        };
        let report = logs(&podman, "dagq", "dagq-broker-abc", 7).unwrap();
        assert_eq!(
            report,
            LogsReport {
                container: "dagq-broker-abc".to_owned(),
                tail: 7,
                stdout: "out\n".to_owned(),
                stderr: "listening on 0.0.0.0:8750\n".to_owned(),
            }
        );
        let calls = podman.calls.borrow();
        assert_eq!(
            calls.last().unwrap(),
            "--connection dagq logs --tail 7 dagq-broker-abc"
        );
        // It reads only: no start, run, init or rm.
        assert!(calls.iter().all(|call| {
            !["machine start", "machine init", " run ", " rm "]
                .iter()
                .any(|word| call.contains(word))
        }));
        assert_eq!(
            logs_args("dagq", "c", DEFAULT_LOG_TAIL),
            ["--connection", "dagq", "logs", "--tail", "200", "c"]
        );
    }

    #[test]
    fn logs_without_podman_a_machine_or_the_container_are_structured_errors() {
        let failure = |podman: Stub| logs(&podman, "dagq", "dagq-broker-abc", 5).unwrap_err();
        let missing = failure(Stub {
            missing: true,
            ..Stub::default()
        });
        assert_eq!(missing.to_json()["code"], "podman_missing");
        assert_eq!(
            failure(Stub {
                machines: "[]",
                ..Stub::default()
            })
            .to_json()["code"],
            "machine_missing"
        );
        assert_eq!(
            failure(Stub {
                machines: STOPPED,
                ..Stub::default()
            })
            .to_json()["code"],
            "machine_stopped"
        );
        let podman = Stub {
            machines: RUNNING,
            ..Stub::default()
        };
        let error = logs(&podman, "dagq", "dagq-broker-abc", 5).unwrap_err();
        assert_eq!(error.code, FailureCode::ContainerMissing);
        assert_eq!(error.to_json()["code"], "container_missing");
        assert!(
            !podman
                .calls
                .borrow()
                .iter()
                .any(|call| call.contains(" logs "))
        );
        let error = failure(Stub {
            machines: RUNNING,
            container_exists: true,
            logs: Some(out(false, 125, "", "no such container\n")),
            ..Stub::default()
        });
        assert_eq!(error.code, FailureCode::PodmanFailed);
        assert!(error.message.contains("no such container"), "{error}");
    }

    fn line(ts: &str, run: &str, task: u64, op: &str) -> String {
        json!({
            "ts": ts, "request_id": format!("r-{ts}"), "jti": "j", "run_id": run,
            "task_id": task, "actor_id": "worker", "backend": "fs", "op": op,
            "capability": "fs.read", "path": "a.txt", "program": null, "argc": null,
            "argv_sha256": null, "result": "ok", "exit_code": null, "duration_ms": 1,
            "bytes_in": 2, "bytes_out": 3,
        })
        .to_string()
    }

    fn fixture(dir: &Path) {
        std::fs::write(
            dir.join("2026-09-27.jsonl"),
            format!(
                "{}\n{}\n",
                line("2026-09-27T23:59:59.000Z", "r1", 1, "fs.read"),
                line("2026-09-27T10:00:00.000Z", "r2", 2, "fs.list"),
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("2026-09-28.jsonl"),
            format!(
                "{}\nnot json\n[1,2]\n{{\"ts\":\"yesterday\"}}\n\n{}\n",
                line("2026-09-28T00:00:00.000Z", "r1", 1, "fs.write"),
                line("2026-09-28T12:30:00.500Z", "r2", 2, "git.status"),
            ),
        )
        .unwrap();
        // The last line cut short while the broker wrote it.
        let whole = line("2026-09-29T08:00:00.000Z", "r1", 1, "process.exec");
        std::fs::write(
            dir.join("2026-09-29.jsonl"),
            format!("{whole}\n{}", &whole[..whole.len() / 2]),
        )
        .unwrap();
        std::fs::write(dir.join("notes.txt"), "not a day file\n").unwrap();
    }

    fn ops(report: &AuditReport) -> Vec<&str> {
        report
            .entries
            .iter()
            .map(|entry| entry["op"].as_str().unwrap())
            .collect()
    }

    fn at(text: &str) -> Option<i64> {
        Some(rfc3339_millis(text).unwrap())
    }

    #[test]
    fn audit_keeps_the_lines_of_a_run_a_task_and_a_time_range_across_days() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let read = |query: AuditQuery| audit(dir.path(), &query).unwrap();

        let all = read(AuditQuery::default());
        assert_eq!(
            ops(&all),
            [
                "fs.read",
                "fs.list",
                "fs.write",
                "git.status",
                "process.exec"
            ]
        );
        // "not json", "[1,2]", an unreadable ts and the cut last line.
        assert_eq!(all.skipped, 4);
        assert_eq!(all.dropped, 0);
        // Each line as the broker wrote it: no field added or lost.
        assert_eq!(
            all.entries[0],
            serde_json::from_str::<Value>(&line("2026-09-27T23:59:59.000Z", "r1", 1, "fs.read"))
                .unwrap()
        );

        let run = read(AuditQuery {
            run: Some("r1".to_owned()),
            ..AuditQuery::default()
        });
        assert_eq!(ops(&run), ["fs.read", "fs.write", "process.exec"]);
        let task = read(AuditQuery {
            task: Some(2),
            ..AuditQuery::default()
        });
        assert_eq!(ops(&task), ["fs.list", "git.status"]);

        // --since is inclusive, --until exclusive; only the days within are
        // read (the 29th's cut line is not counted).
        let range = read(AuditQuery {
            since: at("2026-09-27T23:59:59Z"),
            until: at("2026-09-28T12:30:00.500Z"),
            ..AuditQuery::default()
        });
        assert_eq!(ops(&range), ["fs.read", "fs.write"]);
        assert_eq!(range.skipped, 3);
        let later = read(AuditQuery {
            since: at("2026-09-29T00:00:00Z"),
            ..AuditQuery::default()
        });
        assert_eq!(ops(&later), ["process.exec"]);
        assert_eq!(later.skipped, 1);
        // An --until at midnight does not read that day's file.
        let before = read(AuditQuery {
            until: at("2026-09-29T00:00:00Z"),
            ..AuditQuery::default()
        });
        assert_eq!(
            ops(&before),
            ["fs.read", "fs.list", "fs.write", "git.status"]
        );
        assert_eq!(before.skipped, 3);

        // --limit keeps the latest lines, oldest first.
        let limited = read(AuditQuery {
            run: Some("r1".to_owned()),
            limit: Some(2),
            ..AuditQuery::default()
        });
        assert_eq!(ops(&limited), ["fs.write", "process.exec"]);
        assert_eq!(limited.dropped, 1);
        let none = read(AuditQuery {
            run: Some("r9".to_owned()),
            ..AuditQuery::default()
        });
        assert!(none.entries.is_empty());
    }

    #[test]
    fn audit_without_its_dir_is_no_line() {
        let dir = tempfile::tempdir().unwrap();
        let report = audit(&dir.path().join("broker/audit"), &AuditQuery::default()).unwrap();
        assert_eq!(
            serde_json::to_value(report).unwrap(),
            json!({"entries": [], "skipped": 0, "dropped": 0})
        );
    }

    #[test]
    fn utc_dates_of_millis() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(
            utc_date(at("2026-09-28T23:59:59.999Z").unwrap()),
            "2026-09-28"
        );
        assert_eq!(utc_date(at("2024-02-29T00:00:00Z").unwrap()), "2024-02-29");
        assert_eq!(utc_date(-1), "1969-12-31");
    }
}
