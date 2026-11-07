//! Decisions shared by deployment validation and the optional driver.
use anyhow::{Result, bail};

/// The driver accepts only disable/prefer/require. Prefer (also the driver's
/// default) is upgraded to require: never retry in plaintext. Unlike libpq's
/// require, TLS always verifies the certificate chain and hostname.
/// No setting can disable verification.
#[derive(Debug, PartialEq, Eq)]
pub enum Transport<'a> {
    Plaintext,
    Tls(TrustRoots<'a>),
}

/// Standard Mozilla roots by default; an explicit CA file replaces them,
/// rather than extending them. Certificates have already been validated.
#[derive(PartialEq, Eq)]
pub enum TrustRoots<'a> {
    Standard,
    Custom(&'a [Vec<u8>]),
}

impl std::fmt::Debug for TrustRoots<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Standard => "Standard",
            Self::Custom(_) => "Custom(<redacted>)",
        })
    }
}

/// Pure policy, compiled and tested even without the Postgres driver.
pub fn policy<'a>(mode: Option<&str>, ca: Option<&'a [Vec<u8>]>) -> Result<Transport<'a>> {
    match mode {
        Some("disable") => Ok(Transport::Plaintext),
        None | Some("prefer" | "require") => Ok(Transport::Tls(match ca {
            Some(certificates) => TrustRoots::Custom(certificates),
            None => TrustRoots::Standard,
        })),
        _ => bail!("invalid Postgres sslmode; expected disable, prefer or require"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_explicit_disable_allows_plaintext() {
        for mode in [None, Some("prefer"), Some("require")] {
            assert_eq!(
                policy(mode, None).unwrap(),
                Transport::Tls(TrustRoots::Standard)
            );
        }
        assert_eq!(policy(Some("disable"), None).unwrap(), Transport::Plaintext);
        for mode in ["verify-full", "allow", "secret"] {
            let error = policy(Some(mode), None).unwrap_err();
            assert!(!format!("{error:?}").contains(mode));
        }
    }

    #[test]
    fn custom_roots_replace_standard_roots() {
        let ca = vec![vec![1, 2, 3]];
        for mode in [None, Some("prefer"), Some("require")] {
            assert_eq!(
                policy(mode, Some(&ca)).unwrap(),
                Transport::Tls(TrustRoots::Custom(&ca))
            );
        }
        assert_eq!(
            policy(Some("disable"), Some(&ca)).unwrap(),
            Transport::Plaintext
        );
        assert_eq!(
            format!("{:?}", TrustRoots::Custom(&ca)),
            "Custom(<redacted>)"
        );
    }
}
