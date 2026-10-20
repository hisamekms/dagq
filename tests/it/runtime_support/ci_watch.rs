//! The fake `gh` the supervisor's CI watch (ADR-t1920-1) reads in the
//! runtime tests, and the files beside it that give its runs, jobs and
//! JUnit artifacts.
use super::*;

/// A fake `gh`: `auth status` passes once `authed` exists beside it, `run
/// list` prints `runs.json`, `run view ID` prints `jobs-ID.json` (fails
/// while `jobs-ID.fail` exists), and `run download ID --pattern P --dir D`
/// copies the artifacts `artifacts-ID/P` into `D` (fails when none
/// matches, as gh does). Each call is appended to `calls`.
pub const FAKE_GH: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo "$*" >> "$dir/calls"
case "$1 $2" in
"auth status")
    [ -f "$dir/authed" ] && exit 0
    echo "You are not logged into any GitHub hosts. To log in, run: gh auth login" >&2
    exit 1 ;;
"run list") cat "$dir/runs.json" ;;
"run view")
    [ -f "$dir/jobs-$3.fail" ] && { echo "HTTP 502" >&2; exit 1; }
    cat "$dir/jobs-$3.json" 2>/dev/null || echo '{"jobs":[]}' ;;
"run download")
    id=$3
    out=
    pattern=
    while [ $# -gt 0 ]; do
        [ "$1" = --dir ] && out=$2
        [ "$1" = --pattern ] && pattern=$2
        shift
    done
    found=
    for artifact in "$dir/artifacts-$id"/$pattern; do
        [ -d "$artifact" ] || continue
        found=1
        mkdir -p "$out" && cp -R "$artifact" "$out/" || exit 1
    done
    [ -n "$found" ] || { echo "no valid artifacts found to download" >&2; exit 1; } ;;
*) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#;

/// `watched_with` whose fake `gh` goes in `bin`.
pub fn watched_in(bin: &Path, repo: &Path, more: &str) -> PathBuf {
    fs::write(
        repo.join("dagq.toml"),
        // Overlapping globs and one that matches nothing: each into its
        // own directory, the others read all the same.
        format!("{RUST_DECLARED}[ci_watch]\nworkflow = \"ci.yml\"\njunit_artifacts = [\"junit-*\", \"junit-mac*\", \"absent-*\"]\n{more}"),
    )
    .unwrap();
    git(repo, &["add", "dagq.toml"]);
    git(repo, &["commit", "-m", "ci watch"]);
    git(
        repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/name.git",
        ],
    );
    fs::create_dir_all(bin).unwrap();
    let gh = bin.join("gh");
    crate::common::template::script(&gh, FAKE_GH);
    gh
}

/// The runs `gh run list` returns: (id, conclusion, commit), in order,
/// each at its first attempt.
pub fn runs(gh: &Path, runs: &[(i64, &str, &str)]) {
    let first: Vec<(i64, i64, &str, &str)> = runs
        .iter()
        .map(|(id, conclusion, sha)| (*id, 1, *conclusion, *sha))
        .collect();
    attempts(gh, &first);
}

/// The runs `gh run list` returns: (id, attempt, conclusion, commit); a
/// run's creation time follows its ID whatever its attempt.
pub fn attempts(gh: &Path, runs: &[(i64, i64, &str, &str)]) {
    let list: Vec<Value> = runs
        .iter()
        .map(|(id, attempt, conclusion, sha)| {
            json!({
                "databaseId": id, "number": id, "attempt": attempt, "headSha": sha,
                "conclusion": conclusion,
                "url": format!("https://github.com/owner/name/actions/runs/{id}"),
                "createdAt": format!("2026-10-06T00:{id:02}:00Z"), "displayTitle": "x",
            })
        })
        .collect();
    fs::write(gh.with_file_name("runs.json"), json!(list).to_string()).unwrap();
}

/// A red run's failed job and its JUnit, with `failed` failing and
/// `passed` passing.
pub fn red(gh: &Path, id: i64, failed: &[&str], passed: &[&str]) {
    fs::write(
        gh.with_file_name(format!("jobs-{id}.json")),
        json!({"jobs": [
            {"name": "test", "conclusion": "failure",
             "steps": [{"name": "nextest", "conclusion": "failure"}]},
            {"name": "lint", "conclusion": "success", "steps": []},
        ]})
        .to_string(),
    )
    .unwrap();
    let cases: String = failed
        .iter()
        .map(|name| {
            format!("<testcase classname=\"dagq::it\" name=\"{name}\"><failure/></testcase>")
        })
        .chain(
            passed
                .iter()
                .map(|name| format!("<testcase classname=\"dagq::it\" name=\"{name}\"/>")),
        )
        .collect();
    let dir = gh
        .with_file_name(format!("artifacts-{id}"))
        .join("junit-macos");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("junit.xml"),
        format!("<testsuites><testsuite name=\"dagq::it\">{cases}</testsuite></testsuites>"),
    )
    .unwrap();
}

pub fn head(repo: &Path) -> String {
    git_out(repo, &["rev-parse", "HEAD"]).trim().to_owned()
}

pub fn commit(repo: &Path, name: &str) -> String {
    fs::write(repo.join(name), name).unwrap();
    git(repo, &["add", name]);
    git(repo, &["commit", "-m", name]);
    head(repo)
}
