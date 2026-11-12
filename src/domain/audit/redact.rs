//! Secrets out of the audit record (ADR-t4840-2): a record keeps no
//! secret's value. A name is a credential's by [`is_credential_name`], the
//! one judgment the narrowed environment of a run's processes uses too
//! (`crate::infrastructure::passed_env`), so the two never disagree.

use serde_json::Value;

/// The words that make a variable's name a credential's.
const CREDENTIAL_WORDS: &[&str] = &["TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL", "KEY"];

/// The credentials named without one of [`CREDENTIAL_WORDS`].
const CREDENTIAL_NAMES: &[&str] = &[
    "SSH_AUTH_SOCK",
    "SSH_ASKPASS",
    "GIT_ASKPASS",
    "SUDO_ASKPASS",
    crate::domain::store_setting::STORE_CONNECTION_ENV,
    "DAGQ_DATABASE_URL_FILE",
];

/// What a credential's value is replaced with in a record: its name stays.
pub const REDACTED: &str = "[redacted]";

/// Whether `name` names a credential: no narrowed environment is given it
/// from the starting process unless its caller names it as an exception,
/// and no audit record keeps its value.
pub fn is_credential_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    CREDENTIAL_NAMES.contains(&upper.as_str())
        || CREDENTIAL_WORDS.iter().any(|word| upper.contains(word))
}

/// `value` with the value of every member a credential names, at any
/// depth, replaced by [`REDACTED`], and so the `value` of an object whose
/// `name` is a credential's (`{"name": "GITHUB_TOKEN", "value": ...}`); the
/// names stay. A secret inside free text (an error's words, a URL) is not
/// seen: the sender keeps it out.
pub fn redact_value(value: Value) -> Value {
    match value {
        Value::Object(mut members)
            if members
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(is_credential_name)
                && members.contains_key("value") =>
        {
            members.insert("value".to_owned(), Value::String(REDACTED.to_owned()));
            redact_value_members(members)
        }
        Value::Object(members) => redact_value_members(members),
        Value::Array(items) => Value::Array(items.into_iter().map(redact_value).collect()),
        other => other,
    }
}

fn redact_value_members(members: serde_json::Map<String, Value>) -> Value {
    Value::Object(
        members
            .into_iter()
            .map(|(name, member)| {
                let member = if is_credential_name(&name) {
                    Value::String(REDACTED.to_owned())
                } else {
                    redact_value(member)
                };
                (name, member)
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::super::chain::hashed_bytes;
    use super::super::record::fixtures::{admission, input};
    use super::super::record::{Change, admit};
    use super::super::{AuditAction, Sender};
    use super::*;
    use serde_json::json;

    /// `passed_env::credential` is this function, re-exported, so the
    /// narrowed environment and the record judge every name alike.
    #[test]
    fn a_name_is_a_credentials_by_its_words_or_its_exact_name() {
        for name in [
            "GITHUB_TOKEN",
            "aws_secret_access_key",
            "api_key",
            "SSH_AUTH_SOCK",
            "ssh_auth_sock",
            "DAGQ_DATABASE_URL_FILE",
            crate::domain::store_setting::STORE_CONNECTION_ENV,
        ] {
            assert!(is_credential_name(name), "{name}");
        }
        for name in ["PATH", "HOME", "CARGO_TARGET_DIR", "SSH_AUTH_SOCK_DIR"] {
            assert!(!is_credential_name(name), "{name}");
        }
    }

    #[test]
    fn a_credentials_value_leaves_the_record_and_its_name_stays() {
        let mut with = input(1, AuditAction::ConfigChanged);
        with.change = Some(Change {
            before: Some(json!({"env": {"GITHUB_TOKEN": "ghp_before", "PATH": "/bin"}})),
            after: Some(
                json!({"env": [{"DAGQ_DATABASE_URL_FILE": "/run/s"}, {"api_key": "k-1"},
                {"name": "NPM_TOKEN", "value": "npm-1"}, {"name": "PATH", "value": "/usr/bin"}]}),
            ),
            digest: None,
        });
        let record = admit(
            with,
            admission(1, &super::super::chain::genesis(), Sender::Integrate),
        )
        .unwrap();
        let bytes = String::from_utf8(hashed_bytes(&record)).unwrap();
        for secret in ["ghp_before", "/run/s", "k-1", "npm-1"] {
            assert!(!bytes.contains(secret), "{secret} in {bytes}");
        }
        for name in ["GITHUB_TOKEN", "DAGQ_DATABASE_URL_FILE", "api_key"] {
            assert!(
                bytes.contains(&format!("\"{name}\":\"{REDACTED}\"")),
                "{name}"
            );
        }
        assert!(bytes.contains("\"PATH\":\"/bin\""));
        assert!(bytes.contains(&format!("\"name\":\"NPM_TOKEN\",\"value\":\"{REDACTED}\"")));
        assert!(bytes.contains("\"name\":\"PATH\",\"value\":\"/usr/bin\""));
    }
}
