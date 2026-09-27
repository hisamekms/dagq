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
