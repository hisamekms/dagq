//! `graph --format d2|svg` (ADR-0077): the near-term diagram's d2 source,
//! and the SVG drawn by a stub `d2` found on PATH, or why there is none.

use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};

use crate::common::{self, Bounded};

use common::cli::*;

/// Run with PATH only `bin` and the system's: the host's own d2, if any,
/// is not found.
fn invoke_with_bin(db: &Path, bin: &Path, args: &[&str]) -> Output {
    let path = std::env::join_paths([bin, Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    Command::new(env!("CARGO_BIN_EXE_dagq"))
        .env("PATH", path)
        .env_remove("DAGQ_ROLE")
        .arg("--db")
        .arg(db)
        .args(args)
        .bounded_output()
        .unwrap()
}

fn stub(bin: &Path, name: &str, body: &str) {
    let path = bin.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn graph_prints_the_near_term_diagram_as_d2_or_svg() {
    let (dir, db) = queue();
    let goal = ok(&db, &["goal", "add", "near \"term\" goal"])["id"].to_string();
    let first = ok(&db, &["add", "groundwork", "--goal", &goal])["id"].to_string();
    let second = ok(
        &db,
        &[
            "add",
            "on top",
            "--goal",
            &goal,
            "--depends-on",
            &first,
            "--priority",
            "high",
        ],
    )["id"]
        .to_string();
    let quiet = ok(&db, &["add", "someday"])["id"].to_string();
    for id in [&first, &second, &quiet] {
        ok(&db, &["ready", id, "--bypass-review"]);
    }
    let json = ok(&db, &["graph"]);
    assert_eq!(json, ok(&db, &["graph", "--format", "json"]));
    assert!(json["tasks"].is_array() && json["critical"].is_array());

    let bin = dir.path().join("tools");
    std::fs::create_dir(&bin).unwrap();
    let d2 = invoke_with_bin(&db, &bin, &["graph", "--format", "d2"]);
    assert!(
        d2.status.success(),
        "{}",
        String::from_utf8_lossy(&d2.stderr)
    );
    let source = String::from_utf8(d2.stdout).unwrap();
    assert!(source.starts_with("# dagq graph"), "{source}");
    assert!(source.contains(&format!("goal_{goal}: {{")));
    assert!(source.contains(&format!("label: \"goal {goal}: near \\\"term\\\" goal\"")));
    assert!(
        source.contains(&format!("t{first}: {{")) && source.contains(&format!("t{second}: {{"))
    );
    assert!(!source.contains(&format!("t{quiet}: {{")), "{source}");
    assert!(source.contains(&format!("t{first} -> t{second}:")));
    // The same queue gives the same source.
    assert_eq!(
        invoke_with_bin(&db, &bin, &["graph", "--format", "d2"]).stdout,
        source.as_bytes()
    );

    let out = dir.path().join("graph.d2");
    let written = ok(
        &db,
        &["graph", "--format", "d2", "--out", out.to_str().unwrap()],
    );
    assert_eq!(written["format"], "d2");
    assert_eq!(
        written["tasks"],
        serde_json::json!([
            first.parse::<i64>().unwrap(),
            second.parse::<i64>().unwrap()
        ])
    );
    assert_eq!(std::fs::read_to_string(&out).unwrap(), source);
    assert!(refused(&db, &["graph", "--out", out.to_str().unwrap()]).contains("--out needs"));
    // The observer and the headless review read the graph but write no file.
    for role in ["observer", "reviewer"] {
        ok_as(role, &db, &["graph"]);
        let denied = invoke_as(
            Some(role),
            &db,
            &["graph", "--format", "d2", "--out", out.to_str().unwrap()],
        );
        assert!(!denied.status.success(), "{role}");
        assert!(String::from_utf8_lossy(&denied.stderr).contains("may not change queue state"));
    }

    // Without the tools: no SVG, and why.
    let missing = invoke_with_bin(&db, &bin, &["graph", "--format", "svg"]);
    assert!(!missing.status.success());
    assert!(missing.stdout.is_empty());
    let error = String::from_utf8_lossy(&missing.stderr);
    assert!(
        error.contains("d2 and d2plugin-tala not found on PATH"),
        "{error}"
    );
    let doctor: serde_json::Value =
        serde_json::from_slice(&invoke_with_bin(&db, &bin, &["doctor"]).stdout).unwrap();
    assert_eq!(doctor["d2"]["d2"], serde_json::Value::Null);
    assert!(
        doctor["d2"]["error"]
            .as_str()
            .unwrap()
            .contains("d2plugin-tala")
    );

    // A stub d2 echoes the source it read inside an <svg>.
    stub(&bin, "d2plugin-tala", "exit 0");
    stub(
        &bin,
        "d2",
        "[ \"$1\" = --layout=tala ] || exit 9\nprintf '<svg>'; cat; printf '</svg>'",
    );
    let svg = invoke_with_bin(&db, &bin, &["graph", "--format", "svg"]);
    assert!(
        svg.status.success(),
        "{}",
        String::from_utf8_lossy(&svg.stderr)
    );
    assert_eq!(
        String::from_utf8(svg.stdout).unwrap(),
        format!("<svg>{source}</svg>")
    );
    let doctor: serde_json::Value =
        serde_json::from_slice(&invoke_with_bin(&db, &bin, &["doctor"]).stdout).unwrap();
    assert_eq!(doctor["d2"]["d2"]["path"], bin.join("d2").to_str().unwrap());
    assert_eq!(
        doctor["d2"]["tala"]["path"],
        bin.join("d2plugin-tala").to_str().unwrap()
    );
    assert!(doctor["d2"].get("error").is_none());

    stub(
        &bin,
        "d2",
        "cat >/dev/null; echo 'tala: license check' >&2; exit 2",
    );
    let failed = invoke_with_bin(&db, &bin, &["graph", "--format", "svg"]);
    assert!(!failed.status.success());
    let error = String::from_utf8_lossy(&failed.stderr);
    assert!(
        error.contains("exit 2") && error.contains("license check"),
        "{error}"
    );
}

/// `dagq report` draws the report's dependency diagram as the supervisor
/// does (ADR-0077 decision 7): d2 and TALA from PATH, the SVG inline, and
/// the reason in its place when they cannot draw.
#[test]
fn report_carries_the_near_term_diagram_or_why_not() {
    let (dir, db) = queue();
    let first = ok(&db, &["add", "groundwork"])["id"].as_i64().unwrap();
    let second = ok(
        &db,
        &[
            "add",
            "on top",
            "--depends-on",
            &first.to_string(),
            "--priority",
            "high",
        ],
    )["id"]
        .as_i64()
        .unwrap();
    let bin = dir.path().join("tools");
    let config = dir.path().join("config");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    let path =
        std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")]).unwrap();
    let report = |args: &[&str]| -> serde_json::Value {
        let output = Command::new(env!("CARGO_BIN_EXE_dagq"))
            .env("PATH", &path)
            .env("TZ", "UTC")
            .env("XDG_CONFIG_HOME", &config)
            .env_remove("DAGQ_ROLE")
            .arg("--db")
            .arg(&db)
            .arg("report")
            .args(args)
            .bounded_output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };

    let printed = report(&["--print", "json"]);
    assert_eq!(
        printed["diagram"]["tasks"],
        serde_json::json!([first, second])
    );
    assert_eq!(printed["diagram"]["d2_source"], true);
    assert!(
        printed["diagram"]["reason"]
            .as_str()
            .unwrap()
            .contains("d2 and d2plugin-tala not found on PATH")
    );
    let written = report(&[]);
    let page = std::fs::read_to_string(written["html"].as_str().unwrap()).unwrap();
    assert!(page.contains("Not drawn: cannot draw the dependency diagram"));

    stub(&bin, "d2plugin-tala", "exit 0");
    stub(
        &bin,
        "d2",
        "[ \"$1\" = --layout=tala ] || exit 9\nprintf '<svg xmlns=\"http://www.w3.org/2000/svg\"><text>'; sed 's/</[/g'; printf '</text><a href=\"https://x\">x</a></svg>'",
    );
    let written = report(&[]);
    let page = std::fs::read_to_string(written["html"].as_str().unwrap()).unwrap();
    let start = page
        .find("<div class=\"scroll diagram\"><svg><text>")
        .unwrap();
    assert!(page[start..].contains("on top"));
    for external in ["http://", "https://", "xmlns", "<script", "url(", "src="] {
        assert!(!page.contains(external), "{external}");
    }
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(written["json"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(
        json["diagram"],
        serde_json::json!({"tasks": [first, second], "d2_source": true})
    );
}

#[test]
fn graph_of_a_goal_draws_the_prerequisites_outside_it() {
    let (dir, db) = queue();
    let add = |args: &[&str]| ok(&db, args)["id"].to_string();
    let before = add(&["goal", "add", "groundwork goal"]);
    let gate = add(&["goal", "add", "gate goal"]);
    let focus = add(&["goal", "add", "focus goal"]);
    let base = add(&["add", "base", "--goal", &before]);
    let aside = add(&["add", "aside", "--goal", &before]);
    let gated = add(&["add", "gated", "--goal", &gate]);
    let top = add(&[
        "add",
        "top",
        "--goal",
        &focus,
        "--depends-on",
        &base,
        "--depends-on-goal",
        &gate,
        "--priority",
        "high",
    ]);
    for id in [&base, &aside, &gated, &top] {
        ok(&db, &["ready", id, "--bypass-review"]);
    }
    // The JSON stays narrowed to the goal.
    let json = ok(&db, &["graph", "--goal", &focus]);
    let listed: Vec<String> = json["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|task| task["id"].to_string())
        .collect();
    assert_eq!(listed, std::slice::from_ref(&top));

    let bin = dir.path().join("tools");
    std::fs::create_dir(&bin).unwrap();
    let args = ["graph", "--goal", &focus, "--format", "d2"];
    let d2 = invoke_with_bin(&db, &bin, &args);
    assert!(
        d2.status.success(),
        "{}",
        String::from_utf8_lossy(&d2.stderr)
    );
    let source = String::from_utf8(d2.stdout).unwrap();
    for id in [&base, &gated, &top] {
        assert!(source.contains(&format!("t{id}: {{")), "{id}: {source}");
    }
    assert!(!source.contains(&format!("t{aside}: {{")), "{source}");
    for goal in [&before, &gate, &focus] {
        assert!(source.contains(&format!("goal_{goal}: {{")), "{goal}");
    }
    assert!(source.contains(&format!("label: \"goal {before}: groundwork goal\"")));
    assert!(source.contains(&format!("t{base} -> t{top}:")));
    assert!(source.contains(&format!("goal_{gate} -> t{top}:")));
    assert_eq!(invoke_with_bin(&db, &bin, &args).stdout, source.as_bytes());

    let out = dir.path().join("goal.d2");
    let written = ok(
        &db,
        &[
            "graph",
            "--goal",
            &focus,
            "--format",
            "d2",
            "--out",
            out.to_str().unwrap(),
        ],
    );
    let mut drawn = [&base, &gated, &top].map(|id| id.parse::<i64>().unwrap());
    drawn.sort_unstable();
    assert_eq!(written["tasks"], serde_json::json!(drawn));
}
