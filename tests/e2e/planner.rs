//! The planner sessions `dagq plan` opens (ADR-0041 decision 6) in a real
//! cmux, and the `/exit` a person types into one.
use super::*;

/// The planners of the queue as `planners --all` lists them, by ID.
fn planners(env: &Env) -> Vec<Value> {
    dagq(env, &["planners", "--all"])["planners"]
        .as_array()
        .unwrap()
        .clone()
}

/// Type `/exit` at a session's prompt the way the supervisor does, and
/// read the screen until the session took it: Enter while the input box
/// shows `/exit` (again when one got lost), `/exit` typed again when the
/// box stayed empty for a while (a cmux call under load may lose it). An
/// Enter sent right after the typing could reach the session before the
/// text did and leave `/exit` in the box (task 1008). Done once the stub
/// says `bye` or its workspace is gone; gives up after [`WAIT_LIMIT`].
fn send_exit(cmux: &Path, workspace: &str) {
    use dagq::infrastructure::claude::{input_pending, input_ready};
    let deadline = Instant::now() + WAIT_LIMIT;
    let call = |args: &[&str]| {
        let output = Command::new(cmux).args(args).bounded_output().unwrap();
        if !output.status.success() {
            eprintln!("cmux {args:?} failed: {output:?}");
        }
    };
    let mut sent: Option<Instant> = None;
    let mut screen = String::new();
    while Instant::now() < deadline {
        let read = Command::new(cmux)
            .args(["read-screen", "--workspace", workspace, "--lines", "40"])
            .bounded_output()
            .unwrap();
        // A failed listing is not "gone": the loop reads the screen again.
        if !read.status.success() && matches!(try_listed_workspace(cmux, workspace), Ok(None)) {
            return;
        }
        screen = String::from_utf8_lossy(&read.stdout).into_owned();
        let lines: Vec<&str> = screen.lines().filter(|l| !l.trim().is_empty()).collect();
        // `bye` anywhere under the stub's last box: the wrapper's report
        // and cmux's notices may follow it.
        let below_box = lines
            .iter()
            .rposition(|line| line.trim_start().starts_with('─'))
            .map_or(0, |rule| rule + 1);
        if lines[below_box..].iter().any(|line| line.trim() == "bye") {
            return;
        }
        if input_pending(&screen, "/exit") {
            call(&["send-key", "--workspace", workspace, "--", "enter"]);
            sent = Some(Instant::now());
            thread::sleep(Duration::from_secs(1));
            continue;
        }
        if input_ready(&screen) && sent.is_none_or(|at| at.elapsed() >= Duration::from_secs(5)) {
            call(&["send", "--workspace", workspace, "--", "/exit"]);
            sent = Some(Instant::now());
        }
        thread::sleep(Duration::from_millis(300));
    }
    panic!("workspace {workspace} did not take /exit within {WAIT_LIMIT:?}:\n{screen}");
}

/// `dagq plan` opens planners on demand, side by side (ADR-0041 decision
/// 6): each in its own workspace `[<repo>]planner#<id>` with the
/// planner's role, queue, origin and ID in its environment, in the queue's
/// group, Blue and not pinned. Each runs the session wrapper, whose agent
/// (the stub) submits a proposal owned by that workspace and goes idle;
/// `planners` reports each alive and idle, then the one sent `/exit` as no
/// longer alive with its exit code, and closed once its workspace is gone.
#[test]
#[ignore = "needs a running cmux; run with --ignored"]
fn plan_opens_planners_side_by_side_that_submit_go_idle_and_exit() {
    let fixture = fixture();
    let Fixture {
        cmux,
        repo,
        stub,
        env,
        db,
        ..
    } = &fixture;
    let mut workspaces = WorkspaceGuard {
        cmux: cmux.clone(),
        ids: Vec::new(),
    };
    let plugin_dir = fixture._dir.path().join("plugin");
    fs::create_dir(&plugin_dir).unwrap();
    let plan_args = [
        "plan",
        "--cmux",
        cmux.to_str().unwrap(),
        "--claude",
        stub.to_str().unwrap(),
        "--plugin-dir",
        plugin_dir.to_str().unwrap(),
    ];
    let first = dagq_opening(env, &[], &plan_args, &mut workspaces);
    let second = dagq_opening(env, &[], &plan_args, &mut workspaces);
    eprintln!("plan: {first}\nplan: {second}");
    let repo_name = repo.file_name().unwrap().to_str().unwrap();
    let db = db.canonicalize().unwrap();
    let group = fixture.group().expect("the queue's workspace group exists");
    let members: Vec<String> = group["member_workspace_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().to_ascii_lowercase())
        .collect();
    let mut ids = Vec::new();
    for (report, planner) in [(&first, 1), (&second, 2)] {
        assert_eq!(report["planner"]["id"], planner, "{report}");
        assert_eq!(report["name"], format!("[{repo_name}]planner#{planner}"));
        assert_eq!(report["warnings"], json!([]), "{report}");
        let id = report["planner"]["workspace_id"]
            .as_str()
            .unwrap()
            .to_owned();
        uuid::Uuid::parse_str(&id).expect("workspace id is a UUID");
        let listed = listed_workspace(cmux, &id).expect("the planner workspace is listed");
        assert_eq!(listed["title"], format!("[{repo_name}]planner#{planner}"));
        assert_eq!(listed["pinned"], false, "{listed}");
        assert_eq!(listed["custom_color"], "#1565C0", "{listed}");
        let workspace_env = workspace_env(cmux, &id);
        assert_eq!(workspace_env["DAGQ_ROLE"], "planner", "{workspace_env}");
        assert_eq!(workspace_env["DAGQ_QUEUE"], db.to_str().unwrap());
        assert_eq!(workspace_env["DAGQ_PLANNER_ORIGIN"], "person");
        assert_eq!(workspace_env["DAGQ_PLANNER_ID"], planner.to_string());
        assert!(members.contains(&id.to_ascii_lowercase()), "{group}");
        ids.push(id);
    }
    assert_ne!(ids[0], ids[1]);

    // Both planners run at once: each submits its own proposal and goes
    // idle, which `planners` reports from its wrapper and idle marker.
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        let listed = planners(env);
        if listed.iter().all(|planner| planner["state"] == "idle") && listed.len() == 2 {
            for planner in &listed {
                assert_eq!(planner["alive"], true, "{planner}");
                assert!(planner["idle_since"].is_i64(), "{planner}");
                assert!(planner["wrapper_pid"].is_u64(), "{planner}");
                assert!(planner["agent_pid"].is_u64(), "{planner}");
            }
            break;
        }
        assert!(
            Instant::now() < deadline,
            "planners never went idle: {listed:#?}"
        );
        thread::sleep(Duration::from_millis(300));
    }
    let proposals = dagq(env, &["proposal", "list"]);
    let mut owners: Vec<String> = proposals["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|proposal| {
            assert_eq!(proposal["owner"]["origin"], "person", "{proposal}");
            proposal["owner"]["workspace_id"]
                .as_str()
                .unwrap()
                .to_ascii_lowercase()
        })
        .collect();
    owners.sort();
    let mut expected: Vec<String> = ids.iter().map(|id| id.to_ascii_lowercase()).collect();
    expected.sort();
    assert_eq!(owners, expected, "{proposals}");
    for (report, planner) in [(&first, 1), (&second, 2)] {
        let dir = PathBuf::from(report["dir"].as_str().unwrap());
        let debug = fs::read_to_string(dir.join("claude.log")).unwrap();
        eprintln!("planner {planner}: {debug}");
        assert!(
            debug.contains(&format!(
                "--plugin-dir {}",
                plugin_dir.canonicalize().unwrap().display()
            )),
            "{debug}"
        );
        assert!(
            debug.contains(&format!(
                "env: DAGQ_ROLE=planner DAGQ_PLANNER_ORIGIN=person DAGQ_PLANNER_ID={planner}"
            )),
            "{debug}"
        );
    }

    // The first planner is sent /exit: its wrapper records the exit and it
    // is no longer alive; the second is still idle.
    send_exit(cmux, &ids[0]);
    let deadline = Instant::now() + WAIT_LIMIT;
    let exited = loop {
        let listed = planners(env);
        if listed[0]["exit_code"] == 0 {
            break listed;
        }
        assert!(
            Instant::now() < deadline,
            "planner 1 never exited: {listed:#?}"
        );
        thread::sleep(Duration::from_millis(300));
    };
    assert!(
        exited[0]["state"] == "exited" || exited[0]["state"] == "closed",
        "{exited:#?}"
    );
    assert_eq!(exited[0]["alive"], false);
    assert_eq!(exited[1]["state"], "idle", "{exited:#?}");

    // A person closes the first planner's workspace (cmux may already have
    // closed it with its command): `planners` reports it closed, and the
    // next `plan` opens a third.
    if workspace_listed(cmux, &ids[0]) {
        let close = Command::new(cmux)
            .args(["workspace", "close", &ids[0]])
            .bounded_output()
            .unwrap();
        assert!(close.status.success(), "{close:?}");
    }
    wait_until_not_listed(cmux, &ids[0]);
    let third = dagq_opening(env, &[], &plan_args, &mut workspaces);
    assert_eq!(third["planner"]["id"], 3, "{third}");
    let states: Vec<Value> = planners(env)
        .iter()
        .map(|planner| planner["state"].clone())
        .collect();
    assert_eq!(states[0], "closed", "{states:?}");
    assert_eq!(states[1], "idle", "{states:?}");
    assert_eq!(states.len(), 3, "{states:?}");
    send_exit(cmux, &ids[1]);
}
