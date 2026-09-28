//! The process operation `process.exec`: one program of the allowlist, in
//! the workspace, without a shell.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// `POST /v1/process/exec`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecRequest {
    /// The program and its arguments; `argv[0]`'s basename must be allowed.
    pub argv: Vec<String>,
    /// Env to add; the server keeps only the names it allows. A map, so the
    /// names serialize in order.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    /// Capped by the server's maximum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecResponse {
    /// `None` when a signal ended the process.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode};

    fn text<T: Serialize>(value: &T) -> String {
        String::from_utf8(encode(value).unwrap()).unwrap()
    }

    #[test]
    fn env_serializes_in_the_order_of_its_names() {
        let mut env = BTreeMap::new();
        env.insert("ZED".to_owned(), "1".to_owned());
        env.insert("ALPHA".to_owned(), "2".to_owned());
        env.insert("MID".to_owned(), "3".to_owned());
        let request = ExecRequest {
            argv: vec!["ls".to_owned(), "-la".to_owned()],
            env,
            stdin: Some("in".to_owned()),
            timeout_secs: Some(5),
        };
        let json = concat!(
            r#"{"argv":["ls","-la"],"env":{"ALPHA":"2","MID":"3","ZED":"1"},"#,
            r#""stdin":"in","timeout_secs":5}"#
        );
        assert_eq!(text(&request), json);
        let reordered = r#"{"timeout_secs":5,"stdin":"in","env":{"ZED":"1","MID":"3","ALPHA":"2"},"argv":["ls","-la"]}"#;
        let read = decode::<ExecRequest>(reordered.as_bytes()).unwrap();
        assert_eq!(read, request);
        assert_eq!(text(&read), json);
    }

    #[test]
    fn absent_options_are_left_out() {
        let request = ExecRequest {
            argv: vec!["true".to_owned()],
            env: BTreeMap::new(),
            stdin: None,
            timeout_secs: None,
        };
        assert_eq!(text(&request), r#"{"argv":["true"]}"#);
        assert_eq!(
            decode::<ExecRequest>(br#"{"argv":["true"]}"#).unwrap(),
            request
        );
        let response = ExecResponse {
            exit_code: None,
            stdout: "o".to_owned(),
            stderr: String::new(),
            duration_ms: 7,
        };
        let json = r#"{"exit_code":null,"stdout":"o","stderr":"","duration_ms":7}"#;
        assert_eq!(text(&response), json);
        assert_eq!(decode::<ExecResponse>(json.as_bytes()).unwrap(), response);
    }

    #[test]
    fn unknown_fields_are_refused() {
        assert!(decode::<ExecRequest>(br#"{"argv":["ls"],"shell":true}"#).is_err());
        assert!(decode::<ExecRequest>(br#"{"argv":["ls"],"cwd":"/"}"#).is_err());
        assert!(
            decode::<ExecResponse>(
                br#"{"exit_code":0,"stdout":"","stderr":"","duration_ms":1,"pid":3}"#
            )
            .is_err()
        );
    }
}
