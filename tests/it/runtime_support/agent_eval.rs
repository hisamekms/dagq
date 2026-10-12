//! The eval of an agent's fixtures (ADR-t1728-1): the agent `demo` and its
//! cases committed on the landing branch, the stub provider that plays its
//! agent jobs (the eval's runs, and the jobs that label a production set's
//! cases), the supervisor that runs them, and the queue's events read
//! back. Shared by the eval's and the production sets' tests.

use crate::common::shell_path;
use crate::plan_review::{Fixture, PlanWorkspace, git, options};

use anyhow::{Result, bail};
use dagq::application::{AgentJobLaunch, AgentProvider, CommandSpec};
use dagq::domain::headless_job::{JobAccess, JobSession};
use dagq::domain::tokens::{ExecutionTokens, TokenUsage};
use dagq::domain::{EventId, EventKind, TaskRun};
use dagq::infrastructure::sqlite::SqliteQueue;
use dagq::runtime;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use super::git_out;

/// The definition the round measures.
pub(crate) const DEFINITION: &str =
    "---\ndescription: checks D-1\n---\nFlag D-1 when a change adds notes.txt.\n";

/// A violation of D-1 and a clean answer, as the agent prints them.
pub(crate) fn violation() -> String {
    json!({"agent": "demo", "status": "completed", "verdict": "revise",
           "reasons": [{"text": "adds notes.txt", "codes": ["D-1"]}], "summary": "breaks D-1"})
    .to_string()
}

pub(crate) fn clean() -> String {
    json!({"agent": "demo", "status": "completed", "verdict": "pass", "reasons": [], "summary": "fine"})
        .to_string()
}

/// The agent's jobs: each prints the next reply (the last one again when
/// they run out), copies its standard input to `stdin-<n>.txt` in `out`,
/// and reports $0.25 for the run.
pub(crate) struct EvalAgent {
    replies: Mutex<Vec<String>>,
    launches: Mutex<Vec<AgentJobLaunch>>,
    /// Whether each job's tree held the case lists or the patches.
    pub(crate) saw_cases: Mutex<Vec<bool>>,
    out: PathBuf,
}

impl EvalAgent {
    pub(crate) fn new(out: &Path, replies: &[String]) -> Self {
        Self {
            replies: Mutex::new(replies.to_vec()),
            launches: Mutex::new(Vec::new()),
            saw_cases: Mutex::new(Vec::new()),
            out: out.to_owned(),
        }
    }

    pub(crate) fn launches(&self) -> Vec<AgentJobLaunch> {
        self.launches.lock().unwrap().clone()
    }

    /// What the job of launch `n` read on its standard input.
    pub(crate) fn stdin(&self, n: usize) -> String {
        fs::read_to_string(self.out.join(format!("stdin-{n}.txt"))).unwrap()
    }
}

impl AgentProvider for EvalAgent {
    fn preflight(&self) -> Result<()> {
        Ok(())
    }
    fn headless_command(&self, _: &Path, _: &str, _: JobAccess) -> Result<CommandSpec> {
        bail!("no other job runs in these tests")
    }
    fn review_command(&self, _: &TaskRun, _: &Path, _: &str, _: JobAccess) -> Result<CommandSpec> {
        unreachable!("no run is reviewed in these tests")
    }
    fn agent_job_command(&self, job: &AgentJobLaunch) -> Result<CommandSpec> {
        let mut launches = self.launches.lock().unwrap();
        let n = launches.len();
        launches.push(job.clone());
        self.saw_cases.lock().unwrap().push(
            job.cwd.join(".dagq/agents/demo/evals").exists()
                || job.cwd.join(".dagq/agent-cases").exists(),
        );
        let mut replies = self.replies.lock().unwrap();
        let reply = if replies.len() > 1 {
            replies.remove(0)
        } else {
            replies[0].clone()
        };
        let copy = self.out.join(format!("stdin-{n}.txt"));
        let mut command = CommandSpec::new("/bin/sh");
        command
            .current_dir(&job.cwd)
            .arg("-c")
            .arg(format!(
                "cat > {}; printf '%s\\n' {}",
                shell_path(&copy),
                shell_path(&reply)
            ))
            .stdin(job.prompt.as_str());
        Ok(command)
    }
    fn job_session(&self, _: &str, _: Option<i64>) -> Option<JobSession> {
        Some(JobSession {
            tokens: Some(ExecutionTokens {
                tokens: Some(TokenUsage {
                    input: 1_000,
                    output: 100,
                    cost_usd: Some(0.25),
                    ..TokenUsage::default()
                }),
                ..ExecutionTokens::default()
            }),
            ..JobSession::default()
        })
    }
    fn review_timeout(&self) -> Duration {
        Duration::from_secs(30)
    }
}

/// Commit the agent `demo` (its definition, its dev cases and their
/// patch) and an agent `huge` whose definition is past the agent job's
/// limit on the landing branch; the cases' base commit.
pub(crate) fn commit_agents(repo: &Path) -> String {
    let patch = "diff --git a/notes.txt b/notes.txt\nnew file mode 100644\n--- /dev/null\n+++ b/notes.txt\n@@ -0,0 +1 @@\n+hello\n";
    let hash = format!("{:x}", Sha256::digest(patch.as_bytes()));
    let patches = repo.join(".dagq/agent-cases/patches");
    fs::create_dir_all(&patches).unwrap();
    fs::write(patches.join(format!("{hash}.patch")), patch).unwrap();
    let write = |base: &str| {
        let case = |id: &str, verdict: &str, codes: &[&str]| {
            json!({"id": id, "source": "handmade", "made_by": "test", "base_commit": base,
                   "patch": hash, "review": {"input": {}, "expected": {"verdict": verdict, "codes": codes}}})
        };
        for (agent, definition) in [
            ("demo", DEFINITION.to_owned()),
            (
                "huge",
                "x".repeat(dagq::application::prompt::AGENT_JOB_DEFINITION_BYTES + 1),
            ),
        ] {
            let dir = repo.join(format!(".dagq/agents/{agent}/evals"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.parent().unwrap().join("AGENT.md"), definition).unwrap();
            let list = json!({"agent": agent, "role": "review", "codes": ["D-1"], "k": 1, "cases": [
                case("needs-d1", "violation", &["D-1"]),
                case("clean-one", "clean", &[]),
            ]});
            fs::write(dir.join("dev.json"), list.to_string()).unwrap();
        }
        git(repo, &["add", "."]);
        git(repo, &["commit", "-q", "-m", "agents"]);
    };
    // Twice: the cases' base commit is the first, which already holds case
    // lists and patches, as a production case's base does.
    write(&git_out(repo, &["rev-parse", "HEAD"]));
    let base = git_out(repo, &["rev-parse", "HEAD"]);
    write(&base);
    base
}

pub(crate) fn supervise(fx: &Fixture, agent: &EvalAgent) -> Value {
    runtime::supervise_with_reviewer(
        &fx.db,
        &fx.repo,
        &PlanWorkspace::default(),
        &fx.claude,
        agent,
        Path::new(env!("CARGO_BIN_EXE_dagq")),
        &options(0, Duration::from_secs(3600)),
    )
    .unwrap()
}

/// The payloads of the queue's events of `kind`, oldest first.
pub(crate) fn events(db: &Path, kind: EventKind) -> Vec<Value> {
    let queue = SqliteQueue::open(db).unwrap();
    let upto = queue.latest_event_id().unwrap();
    queue
        .events_of_between(&[kind.as_str()], EventId::new(0), upto, 10_000)
        .unwrap()
        .into_iter()
        .map(|event| event.payload)
        .collect()
}
