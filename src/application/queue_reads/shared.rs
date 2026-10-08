//! The read of the shared parts' `asks` table (docs/design/architecture.md,
//! 共有の部品): `asks`, through [`AskStore`] alone.

use anyhow::Result;
use serde_json::{Value, json};

use super::AsksRead;
use crate::application::{AskQuery, AskStore};

/// `asks`.
pub fn asks(queue: &(impl AskStore + ?Sized), read: &AsksRead) -> Result<Value> {
    Ok(json!({"asks": queue.asks(AskQuery {
        all: read.all,
        open: read.open,
        role: super::session_role(read.role.as_ref())?,
    })?}))
}
