//! The CI watch's reading of GitHub through the host's `gh` and of the
//! main checkout's Git (ADR-t1920-1, the design's "ghの呼び方"). Every call
//! runs in the main checkout with the supervisor's environment (not
//! `[run.env]`), names the repository with `--repo`, and stops after
//! [`CI_WATCH_CALL_TIMEOUT`].

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::adapters::capture;
use super::run_env::resolve_program;
use crate::application::ci_watch::CiSource;
use crate::domain::ci_watch::{
    Access, CI_WATCH_CALL_TIMEOUT, CiJob, CiRun, CiWatchConfig, FailedJob, Junit, RUN_LIST_LIMIT,
    Unavailable, github_repo, job_failed, merge_outcomes, parse_junit, remote_repo,
};

/// The program `[ci_watch]` reads GitHub with when no other is given.
pub const GH: &str = "gh";

/// `gh` and Git on the host for one `[ci_watch]`.
pub struct GhSource {
    /// `gh`, or a path to it.
    pub program: String,
    /// The PATH `program` is looked up in (the supervisor's own).
    pub path: Option<OsString>,
    /// The main checkout every call runs in.
    pub checkout: PathBuf,
    /// The push remote whose URL names the repository.
    pub remote: String,
    pub config: CiWatchConfig,
    /// The watched branch.
    pub branch: String,
    /// Where the artifacts are downloaded (`<queue dir>/ci-watch`).
    pub scratch: PathBuf,
    pub timeout: Duration,
    /// The `gh` and `<owner>/<name>` of the last look that found them.
    found: Mutex<Option<(PathBuf, String)>>,
}

impl GhSource {
    pub fn new(
        program: &str,
        path: Option<OsString>,
        checkout: &Path,
        remote: &str,
        config: CiWatchConfig,
        branch: &str,
        queue_dir: &Path,
    ) -> Self {
        Self {
            program: program.to_owned(),
            path,
            checkout: checkout.to_owned(),
            remote: remote.to_owned(),
            config,
            branch: branch.to_owned(),
            scratch: queue_dir.join("ci-watch"),
            timeout: CI_WATCH_CALL_TIMEOUT,
            found: Mutex::new(None),
        }
    }

    fn path_text(&self) -> String {
        self.path
            .as_deref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn found(&self) -> Result<(PathBuf, String)> {
        self.found
            .lock()
            .map_err(|_| anyhow::anyhow!("the CI watch's state is poisoned"))?
            .clone()
            .context("the means to read GitHub were not looked at")
    }

    /// Run `gh args...` for the repository found; its stdout, or an error
    /// with its stderr.
    fn gh(&self, args: &[&str]) -> Result<String> {
        let (gh, repo) = self.found()?;
        let mut command = Command::new(&gh);
        command
            .args(args)
            .args(["--repo", &repo])
            .current_dir(&self.checkout);
        let (status, stdout, stderr) = capture(&mut command, self.timeout)
            .with_context(|| format!("gh {}", args.join(" ")))?;
        if !status.success() {
            bail!("gh {} failed ({status}): {}", args.join(" "), stderr.trim());
        }
        Ok(stdout)
    }

    fn git(&self, args: &[&str]) -> Option<(bool, String)> {
        let mut command = Command::new("git");
        command.arg("-C").arg(&self.checkout).args(args);
        let (status, stdout, _) = capture(&mut command, self.timeout).ok()?;
        Some((status.success(), stdout))
    }

    /// The JUnit XML files of one run's artifacts, downloaded under
    /// `dir`.
    fn download(
        &self,
        run_id: i64,
        dir: &Path,
    ) -> Result<Vec<Vec<(String, crate::domain::ci_watch::TestOutcome)>>> {
        let id = run_id.to_string();
        let target = dir.to_string_lossy().into_owned();
        // Each glob into a directory of its own, so overlapping globs do
        // not collide; one that matches nothing leaves the others read.
        for (index, glob) in self.config.junit_artifacts.iter().enumerate() {
            let into = format!("{target}/{index}");
            if let Err(error) =
                self.gh(&["run", "download", &id, "--pattern", glob, "--dir", &into])
            {
                tracing::warn!(error = %format_args!("{error:#}"), "the artifacts {glob} of CI run {run_id} could not be downloaded: {error:#}");
            }
        }
        let mut files = Vec::new();
        xml_files(dir, &mut files)?;
        if files.is_empty() {
            bail!("no JUnit XML in the artifacts");
        }
        files
            .iter()
            .map(|file| {
                let text =
                    fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
                parse_junit(&text).map_err(|error| anyhow::anyhow!("{}: {error}", file.display()))
            })
            .collect()
    }
}

/// A job or step that counts as failed ([`job_failed`]).
fn ended_badly(conclusion: &Value) -> bool {
    conclusion.as_str().is_some_and(job_failed)
}

/// Every `*.xml` under `dir`, depth first.
fn xml_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(fs::DirEntry::path);
    for entry in entries {
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            xml_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "xml") {
            out.push(path);
        }
    }
    Ok(())
}

impl CiSource for GhSource {
    fn access(&self) -> Result<Access> {
        let unavailable = |reason: Unavailable, detail: &str| Access::Unavailable {
            reason,
            program: self.program.clone(),
            path: self.path_text(),
            message: Access::message(reason, &self.program, &self.path_text(), detail),
        };
        let Some(gh) = resolve_program(&self.program, self.path.as_deref()) else {
            return Ok(unavailable(Unavailable::GhMissing, ""));
        };
        // Git that did not run is a passing failure; a remote it does not
        // know, or one that is not GitHub's, is the setting's.
        let read = self.git(&["remote", "get-url", &self.remote]);
        let repo = match remote_repo(
            &self.remote,
            read.as_ref().map(|(ok, url)| (*ok, url.as_str())),
        )
        .map_err(anyhow::Error::msg)?
        {
            Ok(repo) => repo,
            Err(detail) => return Ok(unavailable(Unavailable::NotGithub, &detail)),
        };
        let mut command = Command::new(&gh);
        command
            .args(["auth", "status", "--hostname", "github.com"])
            .current_dir(&self.checkout);
        let (status, _, stderr) = capture(&mut command, self.timeout)
            .with_context(|| format!("{} auth status", gh.display()))?;
        if !status.success() {
            let detail = stderr
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("gh auth status failed")
                .to_owned();
            return Ok(unavailable(Unavailable::GhUnauthenticated, &detail));
        }
        *self
            .found
            .lock()
            .map_err(|_| anyhow::anyhow!("the CI watch's state is poisoned"))? =
            Some((gh.clone(), repo.clone()));
        Ok(Access::Available {
            program: self.program.clone(),
            resolved: gh.to_string_lossy().into_owned(),
            repo,
        })
    }

    fn completed_runs(&self) -> Result<Vec<CiRun>> {
        let limit = RUN_LIST_LIMIT.to_string();
        let out = self.gh(&[
            "run",
            "list",
            "--workflow",
            &self.config.workflow,
            "--branch",
            &self.branch,
            "--event",
            "push",
            "--status",
            "completed",
            "--limit",
            &limit,
            "--json",
            "databaseId,number,attempt,headSha,conclusion,url,createdAt,displayTitle",
        ])?;
        let runs: Vec<Value> =
            serde_json::from_str(&out).context("read the output of gh run list")?;
        runs.iter()
            .map(|run| {
                let text = |key: &str| run[key].as_str().unwrap_or_default().to_owned();
                Ok(CiRun {
                    run_id: run["databaseId"]
                        .as_i64()
                        .context("a run of gh run list has no databaseId")?,
                    run_number: run["number"].as_i64().unwrap_or_default(),
                    sha: text("headSha"),
                    conclusion: text("conclusion"),
                    url: text("url"),
                    created_at: text("createdAt"),
                    attempt: run["attempt"].as_i64().unwrap_or(1),
                })
            })
            .collect()
    }

    fn failed_jobs(&self, run_id: i64) -> Result<Vec<FailedJob>> {
        let out = self.gh(&["run", "view", &run_id.to_string(), "--json", "jobs"])?;
        let view: Value = serde_json::from_str(&out).context("read the output of gh run view")?;
        Ok(view["jobs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|job| ended_badly(&job["conclusion"]))
            .map(|job| FailedJob {
                job: job["name"].as_str().unwrap_or_default().to_owned(),
                steps: job["steps"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|step| ended_badly(&step["conclusion"]))
                    .filter_map(|step| step["name"].as_str().map(str::to_owned))
                    .collect(),
            })
            .collect())
    }

    fn jobs(&self, run_id: i64, attempt: i64) -> Result<Vec<CiJob>> {
        let out = self.gh(&[
            "run",
            "view",
            &run_id.to_string(),
            "--attempt",
            &attempt.to_string(),
            "--json",
            "jobs",
        ])?;
        let view: Value = serde_json::from_str(&out).context("read the output of gh run view")?;
        view["jobs"]
            .as_array()
            .context("the output of gh run view has no jobs")?
            .iter()
            .map(|job| {
                Ok(CiJob {
                    name: job["name"]
                        .as_str()
                        .context("a job of gh run view has no name")?
                        .to_owned(),
                    conclusion: job["conclusion"].as_str().unwrap_or_default().to_owned(),
                })
            })
            .collect()
    }

    fn junit(&self, run_id: i64) -> Junit {
        if self.config.junit_artifacts.is_empty() {
            return Junit::NotConfigured;
        }
        let dir = self.scratch.join(run_id.to_string());
        let _ = fs::remove_dir_all(&dir);
        let read = fs::create_dir_all(&dir)
            .map_err(anyhow::Error::from)
            .and_then(|()| self.download(run_id, &dir));
        let _ = fs::remove_dir_all(&dir);
        match read {
            Ok(files) => Junit::Read(merge_outcomes(files)),
            Err(error) => {
                tracing::warn!(error = %format_args!("{error:#}"), "the JUnit of CI run {run_id} could not be read; only its failed jobs are used: {error:#}");
                Junit::Missing
            }
        }
    }

    fn commits(&self, from: &str, to: &str) -> Option<u64> {
        match self.git(&["rev-list", "--count", &format!("{from}..{to}")])? {
            (true, out) => out.trim().parse().ok(),
            (false, _) => None,
        }
    }

    fn is_ancestor(&self, ancestor: &str, of: &str) -> Option<bool> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.checkout)
            .args(["merge-base", "--is-ancestor", ancestor, of]);
        let (status, _, _) = capture(&mut command, self.timeout).ok()?;
        match status.code() {
            Some(0) => Some(true),
            Some(1) => Some(false),
            _ => None,
        }
    }
}

/// The push remote `[repository]` names in `checkout`
/// ([`crate::domain::landing_branch::RepositoryConfig::remote`]: the default
/// when it names none or cannot be read).
fn remote_of(checkout: &Path) -> String {
    super::run_env::load_repository_config(checkout)
        .unwrap_or_default()
        .remote()
        .to_owned()
}

/// `up`'s preflight (ADR-t1920-1 decision 2): with `[ci_watch]` in the
/// `dagq.toml` of `checkout`, why `gh` on `path` cannot read the CI, or
/// `None` when it can (or there is no table).
pub fn preflight(checkout: &Path, path: Option<OsString>) -> Result<Option<String>> {
    let Some(config) = super::run_env::load_ci_watch(checkout)? else {
        return Ok(None);
    };
    let branch = config.branch.clone().unwrap_or_default();
    let source = GhSource::new(
        GH,
        path,
        checkout,
        &remote_of(checkout),
        config,
        &branch,
        checkout,
    );
    Ok(match source.access()? {
        Access::Available { .. } => None,
        Access::Unavailable { message, .. } => Some(message),
    })
}

/// `doctor`'s `ci_watch` for `[ci_watch]` of `checkout`: the table, the
/// `gh` the caller's `path` resolves (null when none), whether it is
/// logged in to github.com, and the repository the push remote names
/// (null for one that is not GitHub's).
pub fn doctor_view(
    checkout: &Path,
    config: &CiWatchConfig,
    path: Option<&std::ffi::OsStr>,
) -> Value {
    let gh = resolve_program(GH, path);
    let authenticated = gh.as_ref().is_some_and(|gh| {
        let mut command = Command::new(gh);
        command
            .args(["auth", "status", "--hostname", "github.com"])
            .current_dir(checkout);
        capture(&mut command, CI_WATCH_CALL_TIMEOUT).is_ok_and(|(status, _, _)| status.success())
    });
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(checkout)
        .args(["remote", "get-url", &remote_of(checkout)]);
    let repo = capture(&mut command, CI_WATCH_CALL_TIMEOUT)
        .ok()
        .filter(|(status, _, _)| status.success())
        .and_then(|(_, url, _)| github_repo(&url));
    serde_json::json!({
        "config": config,
        "gh": gh,
        "authenticated": authenticated,
        "repo": repo,
    })
}
