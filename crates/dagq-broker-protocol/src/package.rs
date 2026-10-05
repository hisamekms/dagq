//! The package operation `package.install`: one of the few commands the
//! repository configured (`[broker.package]` of `dagq.toml`, `--package` of
//! `serve`), chosen by its name. The request names the command and never
//! its argv, so nothing else runs through it.

use serde::{Deserialize, Serialize};

/// `POST /v1/package/install`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRequest {
    /// The configured command's name, `cargo-fetch` and so on.
    pub name: String,
    /// Capped by the server's maximum, as for `process.exec`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// The answer is `process.exec`'s.
pub type InstallResponse = crate::process::ExecResponse;

/// Whether `name` may name a configured command: ASCII letters, digits,
/// `-`, `_` and `.`, at most 64 bytes.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Why the command `name` = `argv` cannot be configured, or `Ok`: a valid
/// name, a program name (no `/`) other than `git` (only the git backend
/// runs git), and no empty program or NUL byte. `dagq` checks
/// `[broker.package]` and `serve` checks `--package` with it.
pub fn check_command(name: &str, argv: &[String]) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!(
            "package command {name:?}: the name is letters, digits, `-`, `_` and `.` (at most 64)"
        ));
    }
    let Some(program) = argv.first() else {
        return Err(format!("package command {name}: the argv is empty"));
    };
    if program.is_empty() || argv.iter().any(|arg| arg.contains('\0')) {
        return Err(format!(
            "package command {name}: the argv holds an empty program or a NUL byte"
        ));
    }
    if program.contains('/') {
        return Err(format!(
            "package command {name}: argv[0] must be a program name, not a path"
        ));
    }
    if program == "git" {
        return Err(format!(
            "package command {name}: git runs through the git operations only"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode};

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn the_request_names_the_command_only() {
        let request = InstallRequest {
            name: "cargo-fetch".to_owned(),
            timeout_secs: Some(9),
        };
        let json = r#"{"name":"cargo-fetch","timeout_secs":9}"#;
        assert_eq!(String::from_utf8(encode(&request).unwrap()).unwrap(), json);
        assert_eq!(decode::<InstallRequest>(json.as_bytes()).unwrap(), request);
        assert_eq!(
            decode::<InstallRequest>(br#"{"name":"n"}"#).unwrap(),
            InstallRequest {
                name: "n".to_owned(),
                timeout_secs: None
            }
        );
        for body in [
            r#"{"name":"n","argv":["sh"]}"#,
            r#"{"name":"n","env":{}}"#,
            r#"{"timeout_secs":1}"#,
        ] {
            assert!(decode::<InstallRequest>(body.as_bytes()).is_err(), "{body}");
        }
    }

    #[test]
    fn a_command_is_a_named_program_other_than_git() {
        assert!(check_command("cargo-fetch", &argv(&["cargo", "fetch"])).is_ok());
        assert!(check_command("npm_install.1", &argv(&["npm", "install"])).is_ok());
        let cases: [(&str, &[&str], &str); 8] = [
            ("", &["cargo"], "the name"),
            ("a b", &["cargo"], "the name"),
            (&"x".repeat(65), &["cargo"], "the name"),
            ("empty", &[], "argv is empty"),
            ("blank", &[""], "empty program"),
            ("nul", &["cargo", "a\0b"], "NUL"),
            ("path", &["/usr/bin/cargo"], "not a path"),
            ("git", &["git", "fetch"], "git operations"),
        ];
        for (name, words, expected) in cases {
            let error = check_command(name, &argv(words)).unwrap_err();
            assert!(error.contains(expected), "{name}: {error}");
        }
        assert!(valid_name(&"x".repeat(64)));
    }
}
