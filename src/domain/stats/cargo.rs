//! The cargo-only measures of `stats`: the counts of what `integrate`'s
//! checks (the whole test suite, llvm-cov, the e2e test) are repeated.
//! They are shown where the rules that work count those checks and left
//! out of the output for any other repository
//! ([`super::without_cargo_measures`]), where a 0 or an `unknown` would
//! read as a fact about that project. The host's `rustc` is left out the
//! same way, by whether the repository's profiles record the Rust
//! toolchain ([`super::without_rustc`]). The decision is
//! made each time `stats` is shown; the callers of
//! [`super::stats`] itself (the KPI windows, plan review's conflict
//! hotspots, the forecast) see every measure. The work breakdown's
//! categories (`e2e`, `llvm_cov`, the tests) are not hidden: the records
//! do not mark them as cargo's.

use serde::{Serialize, Serializer};

/// A cargo-only measure: its value, or hidden (not serialized at all when
/// the field skips [`CargoOnly::is_hidden`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CargoOnly<T> {
    Shown(T),
    Hidden,
}

impl<T: Default> Default for CargoOnly<T> {
    fn default() -> Self {
        Self::Shown(T::default())
    }
}

impl<T> CargoOnly<T> {
    pub fn is_hidden(&self) -> bool {
        matches!(self, Self::Hidden)
    }

    /// The value, `None` when hidden.
    pub fn shown(&self) -> Option<&T> {
        match self {
            Self::Shown(value) => Some(value),
            Self::Hidden => None,
        }
    }

    /// The value to change, `None` when hidden.
    pub fn shown_mut(&mut self) -> Option<&mut T> {
        match self {
            Self::Shown(value) => Some(value),
            Self::Hidden => None,
        }
    }

    pub fn hide(&mut self) {
        *self = Self::Hidden;
    }
}

impl<T: Serialize> Serialize for CargoOnly<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Shown(value) => value.serialize(serializer),
            Self::Hidden => serializer.serialize_none(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[derive(Serialize)]
    struct Row {
        #[serde(skip_serializing_if = "CargoOnly::is_hidden")]
        count: CargoOnly<i64>,
        #[serde(skip_serializing_if = "CargoOnly::is_hidden")]
        name: CargoOnly<Option<String>>,
    }

    /// A shown measure serializes as its value (a null for `None`); a
    /// hidden one is left out.
    #[test]
    fn a_shown_measure_is_its_value_and_a_hidden_one_is_left_out() {
        let mut row = Row {
            count: CargoOnly::default(),
            name: CargoOnly::Shown(None),
        };
        assert_eq!(row.count.shown(), Some(&0));
        assert_eq!(
            serde_json::to_value(&row).unwrap(),
            json!({"count": 0, "name": null})
        );
        row.count.hide();
        row.name.hide();
        assert!(row.count.is_hidden());
        assert_eq!(row.count.shown(), None);
        assert_eq!(serde_json::to_value(&row).unwrap(), json!({}));
        assert_eq!(
            serde_json::to_value(CargoOnly::<i64>::Hidden).unwrap(),
            json!(null)
        );
    }
}
