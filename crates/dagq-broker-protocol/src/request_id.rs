//! [`BrokerRequestId`]: the id the server gives each request, in its error
//! bodies and its audit lines.

use std::fmt;

use serde::{Deserialize, Serialize};

/// The id of one request (a UUID the server makes). On the wire it is the
/// bare string.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BrokerRequestId(String);

impl BrokerRequestId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BrokerRequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_the_bare_string_on_the_wire() {
        let id = BrokerRequestId::new("7a1d");
        assert_eq!(serde_json::to_string(&id).unwrap(), r#""7a1d""#);
        assert_eq!(
            serde_json::from_str::<BrokerRequestId>(r#""7a1d""#).unwrap(),
            id
        );
        assert_eq!(id.as_str(), "7a1d");
        assert_eq!(id.to_string(), "7a1d");
        assert!(serde_json::from_str::<BrokerRequestId>("7").is_err());
    }
}
