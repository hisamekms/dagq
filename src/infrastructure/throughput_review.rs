//! The throughput review's files under `<queue dir>/reports/reviews` (one
//! directory per review), the configuration its agent starts with, the
//! host's time zone and the headless agent's process, for
//! [`crate::application::throughput_review`].
use crate::{
    application::{
        AgentProvider,
        observer::HeadlessAgent,
        throughput_review::{ThroughputReviewHost, reviews_dir},
    },
    domain::{
        actor_model::{ActorLaunch, ModelRole},
        language::Language,
        throughput_review::Window,
    },
};
use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

/// `<queue dir>/reports/reviews/<mode>-<period>/`, suffixed when one
/// already exists (a review run again by hand).
fn review_dir(db: &Path, period: &Window) -> Result<PathBuf> {
    let root = reviews_dir(db);
    fs::create_dir_all(&root).with_context(|| format!("create {}", root.display()))?;
    for n in 0.. {
        let name = if n == 0 {
            format!("{}-{}", period.mode.as_str(), period.label)
        } else {
            format!("{}-{}-{n}", period.mode.as_str(), period.label)
        };
        let dir = root.join(name);
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("create {}", dir.display())),
        }
    }
    unreachable!("the suffixes do not run out")
}

/// What the job's agent starts with (ADR-0079 decision 7):
/// `[roles.throughput_review]` of the bound checkout's `dagq.toml`; none,
/// no checkout, or a file that cannot be read starts it with the
/// provider's default.
pub fn review_launch(checkout: Option<&Path>) -> ActorLaunch {
    let Some(checkout) = checkout else {
        return ActorLaunch::default_of(ModelRole::ThroughputReview);
    };
    match crate::infrastructure::run_env::load_role_models(checkout) {
        Ok(models) => models.launch(ModelRole::ThroughputReview),
        Err(error) => {
            tracing::warn!(error = %format_args!("{error:#}"), "[roles.throughput_review] could not be read; starting it with the default: {error:#}");
            ActorLaunch::default_of(ModelRole::ThroughputReview)
        }
    }
}

/// The throughput review's host: the files under `<queue
/// dir>/reports/reviews`, the bound checkout's configuration, the host's
/// time zone and the agent's local process.
pub struct LocalThroughputReview;

impl ThroughputReviewHost for LocalThroughputReview {
    fn utc_offset(&self, now: i64) -> i64 {
        crate::infrastructure::clock::local_utc_offset(now)
    }
    fn pids(&self) -> (u32, u32) {
        (std::process::id(), std::os::unix::process::parent_id())
    }
    fn review_dir(&self, db: &Path, period: &Window) -> Result<PathBuf> {
        review_dir(db, period)
    }
    fn write(&self, path: &Path, contents: &str) -> Result<()> {
        Ok(fs::write(path, contents)?)
    }
    fn read(&self, path: &Path) -> Result<String> {
        Ok(fs::read_to_string(path)?)
    }
    fn launch(&self, checkout: Option<&Path>) -> ActorLaunch {
        review_launch(checkout)
    }
    fn language(&self, checkout: Option<&Path>, user_config: Option<&Path>) -> Option<Language> {
        crate::infrastructure::language::language_for_prompt(checkout, user_config)
    }
    fn run(
        &self,
        provider: &dyn AgentProvider,
        db: &Path,
        dir: &Path,
        prompt: &str,
        agent: &HeadlessAgent<'_>,
    ) -> Result<Option<i32>> {
        crate::infrastructure::observer::run_agent(provider, db, dir, prompt, agent)
    }
}
