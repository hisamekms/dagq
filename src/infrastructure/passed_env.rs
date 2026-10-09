//! The narrowed environment of a process that runs code or programs on the
//! host for a run (ADR-t1895-2 decision 6): the e2e gate and the program
//! job of a run's review are not given the environment of the process that
//! starts them. Each is given back only the names it allows and the
//! prefixes it adds, never a credential ([`credential`]), which no caller
//! can turn off; a caller can only name the exceptions it needs (the e2e's
//! cmux socket password).

use std::{collections::BTreeMap, ffi::OsString};

/// The words that make a variable's name a credential's.
const CREDENTIAL_WORDS: &[&str] = &["TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL", "KEY"];

/// The credentials named without one of [`CREDENTIAL_WORDS`].
const CREDENTIAL_NAMES: &[&str] = &[
    "SSH_AUTH_SOCK",
    "SSH_ASKPASS",
    "GIT_ASKPASS",
    "SUDO_ASKPASS",
];

/// Whether `name` names a credential, which no narrowed environment is
/// given from the starting process, though it be allowed by name or
/// prefix, unless its caller names it as an exception.
pub fn credential(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    CREDENTIAL_NAMES.contains(&upper.as_str())
        || CREDENTIAL_WORDS.iter().any(|word| upper.contains(word))
}

/// What one caller lets through of the starting process's environment.
#[derive(Debug, Clone, Copy)]
pub struct PassedEnv<'a> {
    /// The names given, compared exactly.
    pub names: &'a [&'a str],
    /// The prefixes whose names are given too.
    pub prefixes: &'a [&'a str],
    /// The credentials given all the same, by exact name: what the
    /// caller's own work cannot do without.
    pub exceptions: &'a [&'a str],
}

impl PassedEnv<'_> {
    /// Whether the starting process's `name` is given.
    pub fn passes(&self, name: &str) -> bool {
        self.exceptions.contains(&name)
            || ((self.names.contains(&name)
                || self.prefixes.iter().any(|prefix| name.starts_with(prefix)))
                && !credential(name))
    }

    /// What of `inherited` is given, as [`Self::passes`] says; a name that
    /// is not UTF-8 is not.
    pub fn filter(
        &self,
        inherited: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> BTreeMap<OsString, OsString> {
        inherited
            .into_iter()
            .filter(|(name, _)| name.to_str().is_some_and(|name| self.passes(name)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names and prefixes a caller allows are given, a credential
    /// among them never, but for the exceptions the caller names.
    #[test]
    fn a_credential_is_given_only_as_a_named_exception() {
        let narrow = PassedEnv {
            names: &["PATH", "API_TOKEN"],
            prefixes: &["LC_"],
            exceptions: &[],
        };
        assert!(narrow.passes("PATH") && narrow.passes("LC_ALL"));
        for name in ["API_TOKEN", "LC_SECRET", "HOME", "path"] {
            assert!(!narrow.passes(name), "{name}");
        }
        let excepted = PassedEnv {
            exceptions: &["SOCKET_PASSWORD"],
            ..narrow
        };
        assert!(excepted.passes("SOCKET_PASSWORD"));
        assert!(!excepted.passes("API_TOKEN"));
        let given = excepted.filter([
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("SOCKET_PASSWORD"), OsString::from("p")),
            (OsString::from("GH_TOKEN"), OsString::from("t")),
        ]);
        assert_eq!(
            given.keys().collect::<Vec<_>>(),
            [&OsString::from("PATH"), &OsString::from("SOCKET_PASSWORD")]
        );
    }
}
