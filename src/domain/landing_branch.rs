//! The branch runs land on (ADR-t615-1): `branch` of `[repository]` in
//! `dagq.toml` when it is set, otherwise the first local branch of the push
//! remote's HEAD, `main` and `master`. Resolved every time it is used,
//! from the repository and the file as they are then; never stored.
use serde::{Deserialize, Serialize};

use super::{DomainError, PUSH_REMOTE};

string_enum!(BranchSource {
    Config => "config",
    RemoteHead => "remote_head",
    Main => "main",
    Master => "master",
});

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
/// `remote_head` the branch the push remote's HEAD names (`None` without
/// the remote or its HEAD), and `exists(name)` whether `refs/heads/<name>`
/// is a local branch. The error says what could not be resolved and how to
/// set it.
pub fn resolve(
    configured: Option<&str>,
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
        "cannot resolve the landing branch: {PUSH_REMOTE}'s HEAD names no local branch, and there is no local main or master; {HINT}"
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
        resolve(configured, remote_head, &mut |name| {
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
    fn serializes_its_fields() {
        assert_eq!(
            serde_json::to_value(LandingBranch::main()).unwrap(),
            serde_json::json!({"branch": "main", "branch_source": "main"})
        );
    }
}
