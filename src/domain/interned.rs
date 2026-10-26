//! Text fields that hold one of a fixed set of `&'static str` (a span's
//! kind, an answerer, why tokens were not counted), read back when such a
//! value crosses between processes (the queue service and a supervisor):
//! each distinct text is kept once for the process's life, so the memory
//! is bounded by the set, not by the reads.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex, PoisonError};

use serde::{Deserialize, Deserializer};

/// A field of such a text. Spelled as a name, not `&'static str`, so that
/// serde's derive does not take it as text borrowed from the input.
pub type Text = &'static str;

static TEXTS: LazyLock<Mutex<HashSet<&'static str>>> = LazyLock::new(Default::default);

/// `text` as the one `&'static str` this process keeps for it.
pub fn intern(text: &str) -> &'static str {
    let mut texts = TEXTS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(kept) = texts.get(text) {
        return kept;
    }
    let kept: &'static str = Box::leak(text.to_owned().into_boxed_str());
    texts.insert(kept);
    kept
}

/// `#[serde(deserialize_with = "crate::domain::interned::text")]`.
pub fn text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<&'static str, D::Error> {
    String::deserialize(deserializer).map(|text| intern(&text))
}

/// [`text`] of an optional field.
pub fn optional<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<&'static str>, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.map(|text| intern(&text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_is_kept_once() {
        let first = intern("interned-test-kind");
        let second = intern(&String::from("interned-test-kind"));
        assert!(std::ptr::eq(first, second));
    }
}
