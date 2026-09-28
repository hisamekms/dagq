//! [`BrokerCapability`]: what a token lets its run do through the broker. A
//! namespace of its own, apart from the queue's `Capability` (ADR-t827-4
//! decision 5).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// One thing a token allows. The names are explicit and closed: a name this
/// build does not know is refused when read (fail closed), so a token that
/// names one is refused as a whole.
///
/// The order is the declared one, and a set of capabilities
/// (`BTreeSet<BrokerCapability>`) serializes in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum BrokerCapability {
    #[serde(rename = "fs.read")]
    FsRead,
    #[serde(rename = "fs.write")]
    FsWrite,
    #[serde(rename = "process.exec")]
    ProcessExec,
    #[serde(rename = "git.read")]
    GitRead,
    #[serde(rename = "git.write")]
    GitWrite,
}

impl BrokerCapability {
    /// Every capability, in order.
    pub const ALL: [BrokerCapability; 5] = [
        Self::FsRead,
        Self::FsWrite,
        Self::ProcessExec,
        Self::GitRead,
        Self::GitWrite,
    ];

    /// The name on the wire, `fs.read` and so on.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FsRead => "fs.read",
            Self::FsWrite => "fs.write",
            Self::ProcessExec => "process.exec",
            Self::GitRead => "git.read",
            Self::GitWrite => "git.write",
        }
    }
}

impl fmt::Display for BrokerCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A capability name this build does not know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownCapability(pub String);

impl fmt::Display for UnknownCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown broker capability `{}`", self.0)
    }
}

impl std::error::Error for UnknownCapability {}

impl FromStr for BrokerCapability {
    type Err = UnknownCapability;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|capability| capability.as_str() == name)
            .ok_or_else(|| UnknownCapability(name.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn names_round_trip_through_serde_and_from_str() {
        for capability in BrokerCapability::ALL {
            let json = serde_json::to_string(&capability).unwrap();
            assert_eq!(json, format!("\"{capability}\""));
            assert_eq!(
                serde_json::from_str::<BrokerCapability>(&json).unwrap(),
                capability
            );
            assert_eq!(capability.as_str().parse(), Ok(capability));
        }
    }

    #[test]
    fn an_unknown_name_is_refused() {
        for name in ["git.push", "fs.*", "FS.READ", "", "reserved.network"] {
            assert!(
                serde_json::from_str::<BrokerCapability>(&format!("\"{name}\"")).is_err(),
                "{name}"
            );
            let error = name.parse::<BrokerCapability>().unwrap_err();
            assert_eq!(error, UnknownCapability(name.to_owned()));
            assert!(error.to_string().contains(name));
        }
    }

    #[test]
    fn a_set_serializes_in_the_declared_order() {
        let set: BTreeSet<_> = BrokerCapability::ALL.into_iter().rev().collect();
        assert_eq!(
            serde_json::to_string(&set).unwrap(),
            r#"["fs.read","fs.write","process.exec","git.read","git.write"]"#
        );
    }
}
