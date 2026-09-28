//! Runtime tests: the push of the KPIs to the host's command (ADR-0051
//! decisions 18, 22 and 23) after the supervisor's daily reports.
use dagq::domain::EventKind;
use std::os::unix::fs::PermissionsExt;

use crate::runtime_support;

use runtime_support::*;

/// What stands for a webhook URL given to the command: it may reach no
/// event and no file of the repository.
const SECRET: &str = "https://hooks.example/T000/B000/s3cr3t-t0ken";

fn events_of(db: &Path, kind: &str) -> Vec<Value> {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT payload FROM run_events WHERE kind=?1 ORDER BY id")
        .unwrap()
        .query_map([kind], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|p| serde_json::from_str(&p.unwrap()).unwrap())
        .collect()
}

fn all_payloads(db: &Path) -> String {
    Connection::open(db)
        .unwrap()
        .prepare("SELECT payload FROM run_events")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A queue whose task is canceled (the supervisor claims nothing), with
/// the push script `script` in `host.toml` and two targets no day meets
/// (no landing, no automatic repair), so both are in breach at once.
struct Setup {
    _fixture: Fixture,
    repo: PathBuf,
    db: PathBuf,
    queue_dir: PathBuf,
    inbox: PathBuf,
    script: PathBuf,
    options: SuperviseOptions,
}

fn setup(body: &str, push: bool) -> Setup {
    let (fixture, repo, db) = fixture();
    SqliteQueue::open(&db)
        .unwrap()
        .transition(TaskId::new(1), TaskAction::Cancel)
        .unwrap();
    let queue_dir = db.canonicalize().unwrap().parent().unwrap().to_path_buf();
    let inbox = queue_dir.join("pushed");
    fs::create_dir(&inbox).unwrap();
    let script = queue_dir.join("push.sh");
    write_script(&script, &inbox, body);
    let mut host = String::from(
        "[kpi]\nmin_samples = 1\n\n[kpi.targets.landings]\nmin = 1\n\n[kpi.targets.auto_repairs]\nmin = 1\n",
    );
    if push {
        host.push_str(&format!(
            "\n[push]\ncommand = [\"{}\", \"{SECRET}\"]\ntimeout_secs = 10\nmax_breach_per_day = 1\n",
            script.display()
        ));
    }
    fs::write(queue_dir.join("host.toml"), host).unwrap();
    let options = SuperviseOptions {
        report_daily: true,
        host_config: Some(queue_dir.join("no host-wide file.toml")),
        push_retry: [Duration::from_millis(10), Duration::from_millis(10)],
        ..supervise_options(1, true)
    };
    Setup {
        _fixture: fixture,
        repo,
        db,
        queue_dir,
        inbox,
        script,
        options,
    }
}

/// The push script: `body` after what it read is kept in `inbox`, one
/// file per message with its environment next to it.
fn write_script(script: &Path, inbox: &Path, body: &str) {
    fs::write(
        script,
        format!(
            "#!/bin/sh\nn=$(ls '{inbox}' | wc -l | tr -d ' ')\ncat > \"{inbox}/$n.json\"\nprintf '%s\\n%s\\n%s\\n%s\\n' \"$DAGQ_PUSH_KIND\" \"$DAGQ_QUEUE\" \"$DAGQ_REPORT_HTML\" \"$DAGQ_REPORT_JSON\" > \"{inbox}/$n.env\"\n{body}\n",
            inbox = inbox.display()
        ),
    )
    .unwrap();
    fs::set_permissions(script, fs::Permissions::from_mode(0o755)).unwrap();
}

/// What the command read, each message with its environment, in order
/// (the n-th message's files are named after the 2n files before them).
fn pushed(inbox: &Path) -> Vec<(Value, Vec<String>)> {
    let mut messages = Vec::new();
    for n in 0.. {
        let Ok(json) = fs::read(inbox.join(format!("{}.json", 2 * n))) else {
            break;
        };
        let env = fs::read_to_string(inbox.join(format!("{}.env", 2 * n))).unwrap();
        assert_eq!(json.last(), Some(&b'\n'));
        messages.push((
            serde_json::from_slice(&json).unwrap(),
            env.lines().map(str::to_owned).collect(),
        ));
    }
    messages
}

#[test]
fn the_daily_summary_and_a_breach_reach_the_command_once() {
    let s = setup("exit 0", true);
    let backend = TestWorkspace::new(&s.db, false, VALID_AGENT);
    let outcome = supervise_with(&s.db, &s.repo, &backend, &s.options).unwrap();
    assert_eq!(outcome["runs"], json!([]), "{outcome}");

    // Both targets of the days and of the weeks went into breach; the day's
    // limit let one of them be pushed at once.
    let started = events_of(&s.db, "kpi_breach_started");
    assert_eq!(started.len(), 4, "{started:?}");
    let pushed_now: Vec<&Value> = started.iter().filter(|b| b["pushed"] == true).collect();
    assert_eq!(pushed_now.len(), 1, "{started:?}");
    assert!(
        started
            .iter()
            .all(|b| b["streak"].as_u64().unwrap() >= 2 && b["since"].is_string())
    );

    let messages = pushed(&s.inbox);
    let kinds: Vec<&str> = messages
        .iter()
        .map(|(m, _)| m["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["breach", "daily", "weekly"], "{messages:?}");
    let db = s.db.canonicalize().unwrap().display().to_string();
    let (breach, breach_env) = &messages[0];
    assert_eq!(breach["breaches"][0]["kpi"], pushed_now[0]["kpi"]);
    assert_eq!(breach["queue"], db);
    assert!(breach["title"].as_str().unwrap().contains("target breach"));
    assert_eq!(breach_env, &["breach", db.as_str(), "", ""]);

    let (daily, daily_env) = &messages[1];
    let written = events_of(&s.db, "report_written");
    let latest_day = written.iter().rev().find(|w| w["period"] == "day").unwrap();
    assert_eq!(daily["period"], latest_day["label"]);
    assert_eq!(daily["report_html"], latest_day["html"]);
    assert_eq!(daily_env[0], "daily");
    assert_eq!(daily_env[2], latest_day["html"].as_str().unwrap());
    assert!(Path::new(&daily_env[3]).exists());
    let breaches: Vec<&str> = daily["breaches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["kpi"].as_str().unwrap())
        .collect();
    assert!(breaches.contains(&"landings") && breaches.contains(&"auto_repairs"));
    assert_eq!(daily["open_asks"], 0);
    let text = daily["text"].as_str().unwrap();
    assert!(text.contains("Breaches:\n- "), "{text}");
    assert!(text.contains("open asks: 0"), "{text}");
    assert!(
        daily["title"]
            .as_str()
            .unwrap()
            .ends_with("landings 0, breaches 2")
    );
    assert_eq!(messages[2].1[0], "weekly");

    let sent = events_of(&s.db, "kpi_push_sent");
    assert_eq!(sent.len(), 3);
    assert!(events_of(&s.db, "kpi_push_failed").is_empty());
    // The command and its argument stay in host.toml, out of the events
    // and of the repository.
    let payloads = all_payloads(&s.db);
    assert!(!payloads.contains(SECRET));
    assert!(!payloads.contains(&s.script.display().to_string()));
    assert!(!s.repo.join("dagq.toml").exists());
    assert!(!s.queue_dir.starts_with(&s.repo));

    // Nothing new: another pass pushes nothing and records no breach again.
    supervise_with(&s.db, &s.repo, &backend, &s.options).unwrap();
    assert_eq!(pushed(&s.inbox).len(), 3);
    assert_eq!(events_of(&s.db, "kpi_breach_started").len(), 4);
}

#[test]
fn without_push_nothing_is_called_nor_recorded() {
    let s = setup("exit 0", false);
    let backend = TestWorkspace::new(&s.db, false, VALID_AGENT);
    supervise_with(&s.db, &s.repo, &backend, &s.options).unwrap();
    assert!(!events_of(&s.db, "report_written").is_empty());
    assert!(fs::read_dir(&s.inbox).unwrap().next().is_none());
    for kind in ["kpi_push_sent", "kpi_push_failed", "kpi_push_abandoned"] {
        assert!(events_of(&s.db, kind).is_empty(), "{kind}");
    }
    // The breaches are the observer's too: they are recorded, none pushed.
    let started = events_of(&s.db, "kpi_breach_started");
    assert_eq!(started.len(), 4);
    assert!(started.iter().all(|b| b["pushed"] == false));

    // An empty command in the queue's file turns a host-wide one off.
    let wide = s.queue_dir.join("wide.toml");
    fs::write(
        &wide,
        format!("[push]\ncommand = [\"{}\"]\n", s.script.display()),
    )
    .unwrap();
    let mut host = fs::read_to_string(s.queue_dir.join("host.toml")).unwrap();
    host.push_str("\n[push]\ncommand = []\n");
    fs::write(s.queue_dir.join("host.toml"), host).unwrap();
    Connection::open(&s.db)
        .unwrap()
        .execute("DELETE FROM run_events WHERE kind='report_written'", [])
        .unwrap();
    let options = SuperviseOptions {
        host_config: Some(wide),
        ..s.options.clone()
    };
    supervise_with(&s.db, &s.repo, &backend, &options).unwrap();
    assert!(fs::read_dir(&s.inbox).unwrap().next().is_none());
}

#[test]
fn a_failing_command_is_retried_then_told_once_and_cleared_by_a_success() {
    let s = setup("echo \"boom: $1\" >&2; exit 7", true);
    let backend = TestWorkspace::new(&s.db, false, VALID_AGENT);
    let outcome = supervise_with(&s.db, &s.repo, &backend, &s.options).unwrap();
    assert_eq!(outcome["outcome"], "finished", "{outcome}");
    // The reports were written whatever the push did.
    assert_eq!(events_of(&s.db, "report_written").len(), 8);

    // Each of the three messages was tried three times.
    let failed = events_of(&s.db, "kpi_push_failed");
    assert_eq!(failed.len(), 9, "{failed:?}");
    for kind in ["breach", "daily", "weekly"] {
        let attempts: Vec<(u64, bool)> = failed
            .iter()
            .filter(|f| f["push_kind"] == kind)
            .map(|f| (f["attempt"].as_u64().unwrap(), f["gave_up"] == true))
            .collect();
        assert_eq!(attempts, [(1, false), (2, false), (3, true)], "{kind}");
    }
    assert_eq!(failed[0]["exit_code"], 7);
    assert_eq!(failed[0]["timed_out"], false);
    assert_eq!(failed[0]["stderr_tail"], "boom: [argument]");
    assert!(!all_payloads(&s.db).contains(SECRET));
    assert_eq!(pushed(&s.inbox).len(), 9);

    // One attention for the inbox, however many messages were given up.
    let abandoned = events_of(&s.db, "kpi_push_abandoned");
    assert_eq!(abandoned.len(), 1, "{abandoned:?}");
    assert_eq!(abandoned[0]["reason_category"], "recovery_failed");
    let status = runtime::status(&s.db).unwrap();
    let fix = status["attention"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["kind"] == "kpi_push_abandoned")
        .unwrap_or_else(|| panic!("{status}"))
        .clone();
    assert_eq!(fix["next"], "fix the push command");
    assert_eq!(fix["reason_category"], "recovery_failed");
    assert!(
        fix["last_error"].as_str().unwrap().contains("exit code 7"),
        "{fix}"
    );

    // The command fixed, the next summaries go through and the attention
    // ends.
    write_script(&s.script, &s.inbox, "exit 0");
    Connection::open(&s.db)
        .unwrap()
        .execute("DELETE FROM run_events WHERE kind='report_written'", [])
        .unwrap();
    supervise_with(&s.db, &s.repo, &backend, &s.options).unwrap();
    assert_eq!(events_of(&s.db, "kpi_push_sent").len(), 2);
    let status = runtime::status(&s.db).unwrap();
    assert!(
        !status["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["kind"] == "kpi_push_abandoned"),
        "{status}"
    );
}

/// Of two supervisors that judged the same breach, one records it, and
/// an ended breach can start again.
#[test]
fn a_breach_is_recorded_once_per_start() {
    let (_dir, _repo, db) = fixture();
    let queue = SqliteQueue::open(&db).unwrap();
    let breach = json!({"period": "day", "kpi": "landings", "stratum": "all"});
    let started = queue
        .record_kpi_breach(EventKind::KpiBreachStarted, breach.clone(), Some((10, 1)))
        .unwrap()
        .unwrap();
    assert_eq!(
        (started["pushed"].clone(), started["day"].clone()),
        (json!(true), json!(10))
    );
    assert!(
        queue
            .record_kpi_breach(EventKind::KpiBreachStarted, breach.clone(), Some((10, 1)))
            .unwrap()
            .is_none()
    );
    assert_eq!(queue.kpi_breaches_open().unwrap(), [started]);
    // The day's limit: another breach that day is not pushed.
    let other = json!({"period": "day", "kpi": "auto_repairs", "stratum": "all"});
    let second = queue
        .record_kpi_breach(EventKind::KpiBreachStarted, other, Some((10, 1)))
        .unwrap()
        .unwrap();
    assert_eq!(second["pushed"], false);
    assert!(
        queue
            .record_kpi_breach(EventKind::KpiBreachResolved, breach.clone(), None)
            .unwrap()
            .is_some()
    );
    assert!(
        queue
            .record_kpi_breach(EventKind::KpiBreachResolved, breach.clone(), None)
            .unwrap()
            .is_none()
    );
    assert_eq!(queue.kpi_breaches_open().unwrap().len(), 1);
    let again = queue
        .record_kpi_breach(EventKind::KpiBreachStarted, breach, Some((11, 1)))
        .unwrap()
        .unwrap();
    assert_eq!(again["pushed"], true);
    assert!(
        queue
            .record_kpi_breach(EventKind::KpiPushSent, json!({}), None)
            .is_err()
    );
    assert!(queue.record_kpi_push_abandoned(json!({"n": 1})).unwrap());
    assert!(!queue.record_kpi_push_abandoned(json!({"n": 2})).unwrap());
}
