//! The cargo-only measures of `stats` (ADR-t614-1): what only dagq's own
//! checks (`cargo test`, llvm-cov, the e2e test) and the host's `rustc`
//! give. They are shown for dagq's source repository and left out of the
//! output for any other ([`super::without_cargo_measures`]), where a 0 or
//! an `unknown` would read as a fact about that project. The decision is
//! made each time `stats` is shown; the callers of
//! [`super::stats`] itself (the KPI windows, plan review's conflict
//! hotspots, the forecast) see every measure.

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
