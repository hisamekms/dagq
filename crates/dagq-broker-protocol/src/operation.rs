//! [`Operation`]: the broker's endpoints, each with its path and the
//! capability it needs.

use std::fmt;

use crate::BrokerCapability;

/// One endpoint of the broker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    Health,
    FsRead,
    FsList,
    FsWrite,
    FsEdit,
    ProcessExec,
    GitStatus,
    GitDiff,
    GitLog,
    GitShow,
    GitAdd,
    GitCommit,
    GitRestore,
}

impl Operation {
    /// Every operation, in order.
    pub const ALL: [Operation; 13] = [
        Self::Health,
        Self::FsRead,
        Self::FsList,
        Self::FsWrite,
        Self::FsEdit,
        Self::ProcessExec,
        Self::GitStatus,
        Self::GitDiff,
        Self::GitLog,
        Self::GitShow,
        Self::GitAdd,
        Self::GitCommit,
        Self::GitRestore,
    ];

    /// The name in the audit, `fs.read` and so on.
    pub fn name(self) -> &'static str {
        match self {
            Self::Health => "health",
            Self::FsRead => "fs.read",
            Self::FsList => "fs.list",
            Self::FsWrite => "fs.write",
            Self::FsEdit => "fs.edit",
            Self::ProcessExec => "process.exec",
            Self::GitStatus => "git.status",
            Self::GitDiff => "git.diff",
            Self::GitLog => "git.log",
            Self::GitShow => "git.show",
            Self::GitAdd => "git.add",
            Self::GitCommit => "git.commit",
            Self::GitRestore => "git.restore",
        }
    }

    /// The HTTP method: `GET` for health, `POST` for everything else.
    pub fn method(self) -> &'static str {
        match self {
            Self::Health => "GET",
            _ => "POST",
        }
    }

    /// The HTTP path, `/v1/<backend>/<op>`.
    pub fn path(self) -> &'static str {
        match self {
            Self::Health => "/v1/health",
            Self::FsRead => "/v1/fs/read",
            Self::FsList => "/v1/fs/list",
            Self::FsWrite => "/v1/fs/write",
            Self::FsEdit => "/v1/fs/edit",
            Self::ProcessExec => "/v1/process/exec",
            Self::GitStatus => "/v1/git/status",
            Self::GitDiff => "/v1/git/diff",
            Self::GitLog => "/v1/git/log",
            Self::GitShow => "/v1/git/show",
            Self::GitAdd => "/v1/git/add",
            Self::GitCommit => "/v1/git/commit",
            Self::GitRestore => "/v1/git/restore",
        }
    }

    /// The capability a token needs for it; `None` for health, which needs
    /// no token.
    pub fn capability(self) -> Option<BrokerCapability> {
        match self {
            Self::Health => None,
            Self::FsRead | Self::FsList => Some(BrokerCapability::FsRead),
            Self::FsWrite | Self::FsEdit => Some(BrokerCapability::FsWrite),
            Self::ProcessExec => Some(BrokerCapability::ProcessExec),
            Self::GitStatus | Self::GitDiff | Self::GitLog | Self::GitShow => {
                Some(BrokerCapability::GitRead)
            }
            Self::GitAdd | Self::GitCommit | Self::GitRestore => Some(BrokerCapability::GitWrite),
        }
    }

    /// The operation at `method` and `path`, or `None` (`invalid_request`).
    pub fn route(method: &str, path: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|operation| operation.method() == method && operation.path() == path)
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_operation_routes_back_to_itself() {
        for operation in Operation::ALL {
            assert_eq!(
                Operation::route(operation.method(), operation.path()),
                Some(operation)
            );
            assert!(operation.path().starts_with("/v1/"));
            assert_eq!(operation.to_string(), operation.name());
        }
    }

    #[test]
    fn unknown_routes_are_none() {
        assert_eq!(Operation::route("POST", "/v1/health"), None);
        assert_eq!(Operation::route("GET", "/v1/fs/read"), None);
        assert_eq!(Operation::route("POST", "/v1/git/push"), None);
        assert_eq!(Operation::route("POST", "/v1/fs/read/"), None);
    }

    #[test]
    fn capabilities_follow_the_backend() {
        let table: Vec<_> = Operation::ALL
            .iter()
            .map(|operation| (operation.name(), operation.capability().map(|c| c.as_str())))
            .collect();
        assert_eq!(
            table,
            [
                ("health", None),
                ("fs.read", Some("fs.read")),
                ("fs.list", Some("fs.read")),
                ("fs.write", Some("fs.write")),
                ("fs.edit", Some("fs.write")),
                ("process.exec", Some("process.exec")),
                ("git.status", Some("git.read")),
                ("git.diff", Some("git.read")),
                ("git.log", Some("git.read")),
                ("git.show", Some("git.read")),
                ("git.add", Some("git.write")),
                ("git.commit", Some("git.write")),
                ("git.restore", Some("git.write")),
            ]
        );
    }
}
