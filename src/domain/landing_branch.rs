//! The branch runs land on (ADR-t615-1): `branch` of `[repository]` in
//! `dagq.toml` when it is set, otherwise the first local branch of the push
//! remote's HEAD, `main` and `master`. Resolved every time it is used,
//! from the repository and the file as they are then; never stored. The
//! same table names the remote the landing is pushed to (`remote`, default
//! [`DEFAULT_REMOTE`]) and whether it is pushed at all (`push`).
use serde::{Deserialize, Serialize};

use super::DomainError;

/// The remote the landing is pushed to when `[repository] remote` is not
/// set (ADR-0019 decision 3, ADR-t615-1).
pub const DEFAULT_REMOTE: &str = "origin";

string_enum!(BranchSource {
    Config => "config",
    RemoteHead => "remote_head",
    Main => "main",
    Master => "master",
});

string_enum!(RemoteSource {
    Config => "config",
    Default => "default",
});

/// `[repository]` of `dagq.toml` as written; `None` is a key not set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryConfig {
    /// `branch`: the landing branch; none guesses it.
    pub branch: Option<String>,
    /// `remote`: the remote the landing is pushed to.
    pub remote: Option<String>,
    /// `push`: `false` lands without pushing.
    pub push: Option<bool>,
}

impl RepositoryConfig {
    /// The push remote's name: `remote`, else [`DEFAULT_REMOTE`].
    pub fn remote(&self) -> &str {
        self.remote.as_deref().unwrap_or(DEFAULT_REMOTE)
    }

    /// Where the name of [`Self::remote`] came from.
    pub fn remote_source(&self) -> RemoteSource {
        match self.remote {
            Some(_) => RemoteSource::Config,
            None => RemoteSource::Default,
        }
    }

    /// Whether the landing is pushed: `push`, else `true`.
    pub fn push(&self) -> bool {
        self.push.unwrap_or(true)
    }
}

/// Where the landing is pushed and whether it is, as `up` and `doctor`
/// report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PushTarget {
    pub remote: String,
    pub remote_source: RemoteSource,
    /// Whether the repository has the remote now.
    pub remote_exists: bool,
    pub push: bool,
}

impl PushTarget {
    pub fn new(config: &RepositoryConfig, remote_exists: bool) -> Self {
        Self {
            remote: config.remote().to_owned(),
            remote_source: config.remote_source(),
            remote_exists,
            push: config.push(),
        }
    }

    /// An error when the landing is pushed to a remote `[repository]
    /// remote` names that the repository does not have; the default
    /// remote may be missing (the push is skipped).
    pub fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.push || self.remote_exists || self.remote_source == RemoteSource::Default,
            "{}",
            missing_remote(&self.remote)
        );
        Ok(())
    }
}

/// Why the push to the remote `[repository] remote` names but the
/// repository does not have fails.
pub fn missing_remote(remote: &str) -> String {
    format!(
        "the remote {remote} that [repository] remote of dagq.toml names does not exist; add it with git remote add, or change remote or set push = false under [repository] in the dagq.toml of the repository's main checkout"
    )
}

/// The landing branch and the push, as `up`'s preflight and `doctor`
/// resolve them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepositorySettings {
    #[serde(flatten)]
    pub branch: LandingBranch,
    #[serde(flatten)]
    pub push: PushTarget,
}

/// The landing branch and where its name came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LandingBranch {
    /// The branch's short name (`main`, not `refs/heads/main`).
    #[serde(rename = "branch")]
    pub name: String,
    #[serde(rename = "branch_source")]
    pub source: BranchSource,
}

impl LandingBranch {
    /// `main` as the default guess resolves it, for fakes and fallbacks.
    pub fn main() -> Self {
        Self {
            name: "main".to_owned(),
            source: BranchSource::Main,
        }
    }

    /// `refs/heads/<name>`.
    pub fn reference(&self) -> String {
        format!("refs/heads/{}", self.name)
    }
}

/// Resolve the landing branch. `configured` is `[repository] branch`,
/// `remote` the push remote's name and `remote_head` the branch its HEAD
/// names (`None` without
/// the remote or its HEAD), and `exists(name)` whether `refs/heads/<name>`
/// is a local branch. The error says what could not be resolved and how to
/// set it.
pub fn resolve(
    configured: Option<&str>,
    remote: &str,
    remote_head: Option<&str>,
    exists: &mut dyn FnMut(&str) -> anyhow::Result<bool>,
) -> anyhow::Result<LandingBranch> {
    if let Some(name) = configured {
        anyhow::ensure!(
            exists(name)?,
            "the landing branch {name} that [repository] branch of dagq.toml names is not a local branch (refs/heads/{name}); {HINT}"
        );
        return Ok(LandingBranch {
            name: name.to_owned(),
            source: BranchSource::Config,
        });
    }
    let candidates = remote_head
        .map(|name| (name, BranchSource::RemoteHead))
        .into_iter()
        .chain([
            ("main", BranchSource::Main),
            ("master", BranchSource::Master),
        ]);
    for (name, source) in candidates {
        if exists(name)? {
            return Ok(LandingBranch {
                name: name.to_owned(),
                source,
            });
        }
    }
    anyhow::bail!(
        "cannot resolve the landing branch: {remote}'s HEAD names no local branch, and there is no local main or master; {HINT}"
    )
}

/// What an unresolved landing branch tells the operator to do.
pub const HINT: &str = "name the branch runs land on as branch = \"<name>\" under [repository] in the dagq.toml of the repository's main checkout";

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(
        configured: Option<&str>,
        remote_head: Option<&str>,
        local: &[&str],
    ) -> anyhow::Result<LandingBranch> {
        resolve(configured, DEFAULT_REMOTE, remote_head, &mut |name| {
            Ok(local.contains(&name))
        })
    }

    #[test]
    fn a_configured_branch_wins_and_must_exist() {
        let branch = resolved(Some("trunk"), Some("main"), &["trunk", "main"]).unwrap();
        assert_eq!(branch.name, "trunk");
        assert_eq!(branch.source, BranchSource::Config);
        assert_eq!(branch.reference(), "refs/heads/trunk");
        let error = resolved(Some("trunk"), None, &["main"]).unwrap_err();
        assert!(error.to_string().contains("[repository]"), "{error}");
    }

    #[test]
    fn guesses_remote_head_then_main_then_master() {
        let head = resolved(None, Some("dev"), &["dev", "main"]).unwrap();
        assert_eq!(
            (head.name.as_str(), head.source),
            ("dev", BranchSource::RemoteHead)
        );
        let main = resolved(None, Some("gone"), &["main", "master"]).unwrap();
        assert_eq!(main, LandingBranch::main());
        let master = resolved(None, None, &["master"]).unwrap();
        assert_eq!(
            (master.name.as_str(), master.source),
            ("master", BranchSource::Master)
        );
        let error = resolved(None, None, &["feature"]).unwrap_err();
        assert!(error.to_string().contains("cannot resolve"), "{error}");
        assert!(error.to_string().contains("dagq.toml"), "{error}");
    }

    #[test]
    fn the_push_defaults_to_origin_and_checks_a_configured_remote() {
        let default = RepositoryConfig::default();
        let target = PushTarget::new(&default, false);
        assert_eq!(
            (target.remote.as_str(), target.remote_source, target.push),
            ("origin", RemoteSource::Default, true)
        );
        // A missing default remote only skips the push.
        target.check().unwrap();
        let configured = RepositoryConfig {
            remote: Some("upstream".into()),
            ..RepositoryConfig::default()
        };
        let error = PushTarget::new(&configured, false).check().unwrap_err();
        assert!(error.to_string().contains("upstream"), "{error}");
        assert!(error.to_string().contains("[repository]"), "{error}");
        PushTarget::new(&configured, true).check().unwrap();
        // push = false does not look at the remote.
        let off = RepositoryConfig {
            push: Some(false),
            ..configured
        };
        let target = PushTarget::new(&off, false);
        assert!(!target.push);
        target.check().unwrap();
    }

    #[test]
    fn serializes_its_fields() {
        let settings = RepositorySettings {
            branch: LandingBranch::main(),
            push: PushTarget::new(&RepositoryConfig::default(), true),
        };
        assert_eq!(
            serde_json::to_value(settings).unwrap(),
            serde_json::json!({
                "branch": "main", "branch_source": "main", "remote": "origin",
                "remote_source": "default", "remote_exists": true, "push": true,
            })
        );
        assert_eq!(
            serde_json::to_value(LandingBranch::main()).unwrap(),
            serde_json::json!({"branch": "main", "branch_source": "main"})
        );
    }
}
