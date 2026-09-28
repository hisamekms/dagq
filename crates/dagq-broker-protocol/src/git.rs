//! The git operations: `git.status`, `git.diff`, `git.log`, `git.show`
//! (capability `git.read`), `git.add`, `git.commit` and `git.restore`
//! (capability `git.write`). There is no push, fetch, remote, config or
//! branch operation.

use serde::{Deserialize, Serialize};

/// `POST /v1/git/status`: takes nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusRequest {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusResponse {
    /// The branch `HEAD` is on, or `None` when it is detached.
    pub branch: Option<String>,
    pub entries: Vec<StatusEntry>,
}

/// One line of `git status --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusEntry {
    pub path: String,
    /// The two status letters, `XY` (` M`, `??`, ...).
    pub status: String,
}

/// `POST /v1/git/diff`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffRequest {
    /// The index against `HEAD` rather than the worktree against the index.
    #[serde(default)]
    pub staged: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffResponse {
    pub diff: String,
    pub truncated: bool,
}

/// `POST /v1/git/log`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogRequest {
    /// Default 20, at most 200.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogResponse {
    /// Newest first.
    pub commits: Vec<Commit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Commit {
    pub commit: String,
    pub author: String,
    /// Author time, UNIX seconds.
    pub time: i64,
    pub subject: String,
}

/// `POST /v1/git/show`: one commit of the run branch's history (its
/// message and patch).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShowRequest {
    /// A revision that names a commit `HEAD` reaches; `HEAD` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Only the patch of these paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShowResponse {
    /// The full commit id shown.
    pub commit: String,
    pub show: String,
    pub truncated: bool,
}

/// `POST /v1/git/add`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddRequest {
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddResponse {}

/// `POST /v1/git/commit`: only on the token's branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitRequest {
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitResponse {
    pub commit: String,
}

/// `POST /v1/git/restore`: the worktree's paths back from the index, or
/// with `staged`, the index's paths back from `HEAD` (unstage).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreRequest {
    #[serde(default)]
    pub staged: bool,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoreResponse {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode};

    fn text<T: Serialize>(value: &T) -> String {
        String::from_utf8(encode(value).unwrap()).unwrap()
    }

    #[test]
    fn requests_serialize_in_field_order() {
        assert_eq!(text(&StatusRequest {}), "{}");
        assert_eq!(text(&DiffRequest::default()), r#"{"staged":false}"#);
        let diff = DiffRequest {
            staged: true,
            paths: vec!["b".to_owned(), "a".to_owned()],
        };
        assert_eq!(text(&diff), r#"{"staged":true,"paths":["b","a"]}"#);
        assert_eq!(decode::<DiffRequest>(text(&diff).as_bytes()).unwrap(), diff);
        assert_eq!(
            decode::<DiffRequest>(b"{}").unwrap(),
            DiffRequest::default()
        );
        assert_eq!(text(&LogRequest { limit: Some(3) }), r#"{"limit":3}"#);
        assert_eq!(decode::<LogRequest>(b"{}").unwrap(), LogRequest::default());
        assert_eq!(text(&ShowRequest::default()), "{}");
        let show = ShowRequest {
            commit: Some("HEAD~1".to_owned()),
            paths: vec!["a".to_owned()],
        };
        assert_eq!(text(&show), r#"{"commit":"HEAD~1","paths":["a"]}"#);
        assert_eq!(decode::<ShowRequest>(text(&show).as_bytes()).unwrap(), show);
        let restore = RestoreRequest {
            staged: true,
            paths: vec!["a".to_owned()],
        };
        assert_eq!(text(&restore), r#"{"staged":true,"paths":["a"]}"#);
        assert_eq!(
            decode::<RestoreRequest>(br#"{"paths":["a"]}"#).unwrap(),
            RestoreRequest {
                staged: false,
                paths: vec!["a".to_owned()]
            }
        );
        assert_eq!(
            text(&AddRequest {
                paths: vec!["a".to_owned()]
            }),
            r#"{"paths":["a"]}"#
        );
        assert_eq!(
            text(&CommitRequest {
                message: "m".to_owned()
            }),
            r#"{"message":"m"}"#
        );
    }

    #[test]
    fn responses_serialize_in_field_order() {
        let status = StatusResponse {
            branch: Some("dagq/r".to_owned()),
            entries: vec![StatusEntry {
                path: "a".to_owned(),
                status: " M".to_owned(),
            }],
        };
        let json = r#"{"branch":"dagq/r","entries":[{"path":"a","status":" M"}]}"#;
        assert_eq!(text(&status), json);
        assert_eq!(decode::<StatusResponse>(json.as_bytes()).unwrap(), status);
        assert_eq!(
            text(&DiffResponse {
                diff: "d".to_owned(),
                truncated: false
            }),
            r#"{"diff":"d","truncated":false}"#
        );
        let log = LogResponse {
            commits: vec![Commit {
                commit: "c".to_owned(),
                author: "A".to_owned(),
                time: 1,
                subject: "s".to_owned(),
            }],
        };
        let json = r#"{"commits":[{"commit":"c","author":"A","time":1,"subject":"s"}]}"#;
        assert_eq!(text(&log), json);
        assert_eq!(decode::<LogResponse>(json.as_bytes()).unwrap(), log);
        assert_eq!(text(&AddResponse {}), "{}");
        assert_eq!(text(&RestoreResponse {}), "{}");
        let show = ShowResponse {
            commit: "c".to_owned(),
            show: "s".to_owned(),
            truncated: true,
        };
        let json = r#"{"commit":"c","show":"s","truncated":true}"#;
        assert_eq!(text(&show), json);
        assert_eq!(decode::<ShowResponse>(json.as_bytes()).unwrap(), show);
        assert_eq!(
            text(&CommitResponse {
                commit: "c".to_owned()
            }),
            r#"{"commit":"c"}"#
        );
    }

    #[test]
    fn unknown_fields_are_refused() {
        assert!(decode::<StatusRequest>(br#"{"remote":"origin"}"#).is_err());
        assert!(decode::<DiffRequest>(br#"{"ext_diff":true}"#).is_err());
        assert!(decode::<LogRequest>(br#"{"all":true}"#).is_err());
        assert!(decode::<ShowRequest>(br#"{"commit":"main","ext_diff":true}"#).is_err());
        assert!(decode::<RestoreRequest>(br#"{"paths":["a"],"source":"main"}"#).is_err());
        assert!(decode::<RestoreRequest>(br#"{"staged":true}"#).is_err());
        assert!(decode::<AddRequest>(br#"{"paths":[],"force":true}"#).is_err());
        assert!(decode::<CommitRequest>(br#"{"message":"m","no_verify":true}"#).is_err());
        assert!(decode::<CommitRequest>(br#"{"message":"m","push":true}"#).is_err());
        assert!(decode::<AddResponse>(br#"{"x":1}"#).is_err());
    }
}
