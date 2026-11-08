//! Repository rules fetched without a checkout. Only a fully parsed candidate
//! replaces the last good snapshot; a failed first read holds new claims.
//! Ports return fixed errors, never upstream bodies, credentials or URLs.

use std::fmt;

/// An opaque credential: neither Debug nor Display exposes its value.
pub struct AppToken(pub String);
impl fmt::Debug for AppToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AppToken([redacted])")
    }
}

/// Reusable by lease token issuance: the caller supplies the reduced scope.
/// Implementations authenticate as the configured runner App, never the main App.
pub trait InstallationTokens: Send + Sync {
    fn issue(&self, scope: &serde_json::Value, now: i64) -> Result<AppToken, ConfigUnavailable>;
}

/// GitHub reads use an installation token, with the file pinned to a commit.
pub trait MainRules: InstallationTokens {
    fn main_sha(&self, token: &AppToken) -> Result<String, ConfigUnavailable>;
    fn contents(&self, token: &AppToken, sha: &str) -> Result<String, ConfigUnavailable>;
}

/// Safe to publish in status, events and logs, including on a first-read hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigUnavailable;
impl fmt::Display for ConfigUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("repository_config_unavailable: main dagq.toml has not been read")
    }
}
impl std::error::Error for ConfigUnavailable {}

/// The first-read reason, published through the supervisor-owned hold projection.
pub fn config_hold(missing: bool) -> Option<crate::domain::claim_hold::ClaimHold> {
    missing.then_some(crate::domain::claim_hold::ClaimHold {
        reason: crate::domain::claim_hold::HoldReason::RepositoryConfigUnavailable,
        value: 0.0,
        threshold: 1.0,
        ask_id: None,
    })
}

/// Explicitly narrowing both dimensions avoids GitHub's installation-wide defaults.
pub fn read_scope(repository_name: &str) -> serde_json::Value {
    serde_json::json!({"repositories": [repository_name], "permissions": {"contents": "read"}})
}

/// Bound API polling independently of the number of consumers of the rules.
/// Failed first reads retry at the same interval; clock rollback retries now.
pub fn refresh_due(last: Option<i64>, now: i64) -> bool {
    last.is_none_or(|last| now < last || now.saturating_sub(last) >= 30)
}

/// A SHA is committed to the cache only together with a successfully parsed file.
pub fn needs_read(previous: Option<&str>, main: &str) -> bool {
    previous != Some(main)
}

/// Failed refreshes keep the previous value; with no snapshot claims fail closed.
pub fn after_read<T>(previous: Option<T>, candidate: Result<T, ConfigUnavailable>) -> Option<T> {
    candidate.ok().or(previous)
}

/// The complete parsed rules, not merely the supervisor table. Each process owns
/// its cache; no token or failed candidate is kept in it. The caller serializes
/// refreshes and supplies its clock and parser, so these decisions need no I/O.
pub struct RulesCache<T> {
    snapshot: Option<(String, T)>,
}
impl<T: Clone> Default for RulesCache<T> {
    fn default() -> Self {
        Self { snapshot: None }
    }
}
impl<T: Clone> RulesCache<T> {
    pub fn current(&self) -> Result<T, ConfigUnavailable> {
        self.snapshot
            .as_ref()
            .map(|(_, value)| value.clone())
            .ok_or(ConfigUnavailable)
    }

    pub fn refresh(
        &mut self,
        source: &dyn MainRules,
        repository_name: &str,
        now: i64,
        parse: impl FnOnce(&str) -> Result<T, ConfigUnavailable>,
    ) -> Result<T, ConfigUnavailable> {
        let candidate = (|| {
            let token = source.issue(&read_scope(repository_name), now)?;
            let sha = source.main_sha(&token)?;
            if !needs_read(self.snapshot.as_ref().map(|(sha, _)| sha.as_str()), &sha) {
                return self.snapshot.clone().ok_or(ConfigUnavailable);
            }
            let rules = parse(&source.contents(&token, &sha)?)?;
            Ok((sha, rules))
        })();
        self.snapshot = after_read(self.snapshot.take(), candidate);
        self.snapshot
            .as_ref()
            .map(|(_, value)| value.clone())
            .ok_or(ConfigUnavailable)
    }
}

/// Thread-safe polling use case. All consumers share one parsed snapshot and
/// retry interval. The parser is an infrastructure adapter, never diagnostics.
pub struct RemoteRules<T> {
    source: Box<dyn MainRules>,
    repository_name: String,
    clock: std::sync::Arc<dyn crate::application::Clock>,
    parse: fn(&str) -> Result<T, ConfigUnavailable>,
    state: std::sync::Mutex<(RulesCache<T>, Option<i64>)>,
}
impl<T: Clone> RemoteRules<T> {
    pub fn new(
        source: Box<dyn MainRules>,
        repository_name: String,
        clock: std::sync::Arc<dyn crate::application::Clock>,
        parse: fn(&str) -> Result<T, ConfigUnavailable>,
    ) -> Self {
        Self {
            source,
            repository_name,
            clock,
            parse,
            state: Default::default(),
        }
    }
    /// Only a failure before any successful parse is an error. The pure
    /// refresh_due decision receives time from the injected clock here.
    pub fn read(&self) -> anyhow::Result<T> {
        let mut state = self.state.lock().map_err(|_| ConfigUnavailable)?;
        let now = self.clock.now();
        if !refresh_due(state.1, now) {
            return state.0.current().map_err(Into::into);
        }
        state.1 = Some(now);
        state
            .0
            .refresh(&*self.source, &self.repository_name, now, self.parse)
            .map_err(Into::into)
    }
}

/// Startup-only tables must be installed together before the first claim,
/// even when GitHub was unavailable during composition. Explicit overrides
/// retain the same precedence as local startup. Later snapshots do not change
/// these tables until a restart.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StartupRules {
    pub stall: crate::domain::stall::StallConfig,
    pub disk: crate::domain::disk::DiskConfig,
    pub resume: crate::domain::resume::ResumeConfig,
    pub review_jobs: crate::domain::headless_job::ReviewJobs,
}
#[derive(Clone, Copy, Default)]
pub struct StartupOverrides {
    pub stall: Option<crate::domain::stall::StallConfig>,
    pub disk: Option<crate::domain::disk::DiskConfig>,
    pub resume: Option<crate::domain::resume::ResumeConfig>,
}
impl StartupOverrides {
    pub fn resolve(self, rules: StartupRules) -> StartupRules {
        StartupRules {
            stall: self.stall.unwrap_or(rules.stall),
            disk: self.disk.unwrap_or(rules.disk),
            resume: self.resume.unwrap_or(rules.resume),
            review_jobs: rules.review_jobs,
        }
    }
}
/// Deployment only: retry until a complete snapshot can supply startup tables.
pub type RepositoryStartupFile =
    std::sync::Arc<dyn Fn() -> anyhow::Result<StartupRules> + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        sha: Mutex<Result<String, ConfigUnavailable>>,
        reads: Mutex<Vec<String>>,
        text: Mutex<String>,
    }
    impl InstallationTokens for Fake {
        fn issue(&self, scope: &serde_json::Value, _: i64) -> Result<AppToken, ConfigUnavailable> {
            assert_eq!(
                scope,
                &serde_json::json!({"repositories":["repo"],"permissions":{"contents":"read"}})
            );
            Ok(AppToken("secret".into()))
        }
    }
    impl MainRules for Fake {
        fn main_sha(&self, _: &AppToken) -> Result<String, ConfigUnavailable> {
            self.sha.lock().unwrap().clone()
        }
        fn contents(&self, _: &AppToken, sha: &str) -> Result<String, ConfigUnavailable> {
            self.reads.lock().unwrap().push(sha.into());
            Ok(self.text.lock().unwrap().clone())
        }
    }
    #[test]
    fn startup_tables_preserve_explicit_overrides() {
        let original = StartupRules {
            stall: Default::default(),
            disk: Default::default(),
            resume: Default::default(),
            review_jobs: Default::default(),
        };
        assert_eq!(StartupOverrides::default().resolve(original), original);
        let mut stall = original.stall;
        stall.idle_without_receipt_secs = 123;
        let mut disk = original.disk;
        disk.reserve = 0.2;
        let resume = crate::domain::resume::ResumeConfig {
            conflict_only_limit: 11,
        };
        let applied = StartupOverrides {
            stall: Some(stall),
            disk: Some(disk),
            resume: Some(resume),
        }
        .resolve(original);
        assert_eq!(
            applied,
            StartupRules {
                stall,
                disk,
                resume,
                ..original
            }
        );
    }
    #[test]
    fn decisions_narrow_scope_reread_only_changes_and_fail_closed_without_a_snapshot() {
        assert_eq!(
            read_scope("repo"),
            serde_json::json!({"repositories":["repo"],"permissions":{"contents":"read"}})
        );
        assert!(refresh_due(None, 100));
        assert!(!refresh_due(Some(100), 129));
        assert!(refresh_due(Some(100), 130));
        assert!(refresh_due(Some(100), 99));
        assert!(!needs_read(Some("a"), "a"));
        assert!(needs_read(Some("a"), "b"));
        assert!(needs_read(None, "a"));
        assert_eq!(after_read(None::<u8>, Err(ConfigUnavailable)), None);
        assert_eq!(after_read(Some(1), Err(ConfigUnavailable)), Some(1));
        assert_eq!(after_read(Some(1), Ok(2)), Some(2));
        assert!(!format!("{:?}", AppToken("secret".into())).contains("secret"));
        let hold = config_hold(true).unwrap();
        assert_eq!(
            serde_json::to_value(&hold).unwrap()["reason"],
            "repository_config_unavailable"
        );
        assert!(hold.message().contains("no new run is claimed"));
        assert!(config_hold(false).is_none());
        assert!(
            ConfigUnavailable
                .to_string()
                .contains("repository_config_unavailable")
        );
    }
    #[test]
    fn failed_reads_retry_and_never_replace_a_good_parsed_snapshot() {
        let fake = Fake {
            sha: Mutex::new(Err(ConfigUnavailable)),
            reads: Mutex::new(vec![]),
            text: Mutex::new("1".into()),
        };
        let mut cache = RulesCache::default();
        let parse = |s: &str| s.parse::<u8>().map_err(|_| ConfigUnavailable);
        assert_eq!(
            cache.refresh(&fake, "repo", 1, parse),
            Err(ConfigUnavailable)
        );
        *fake.sha.lock().unwrap() = Ok("a".into());
        assert_eq!(cache.refresh(&fake, "repo", 1, parse), Ok(1));
        assert_eq!(cache.refresh(&fake, "repo", 1, parse), Ok(1));
        assert_eq!(*fake.reads.lock().unwrap(), vec!["a"]);
        *fake.sha.lock().unwrap() = Ok("b".into());
        *fake.text.lock().unwrap() = "invalid".into();
        assert_eq!(cache.refresh(&fake, "repo", 1, parse), Ok(1));
        *fake.text.lock().unwrap() = "2".into();
        assert_eq!(cache.refresh(&fake, "repo", 1, parse), Ok(2));
        *fake.sha.lock().unwrap() = Err(ConfigUnavailable);
        assert_eq!(cache.refresh(&fake, "repo", 1, parse), Ok(2));
        assert_eq!(*fake.reads.lock().unwrap(), vec!["a", "b", "b"]);
    }
}
