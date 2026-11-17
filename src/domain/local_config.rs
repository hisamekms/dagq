//! The uncommitted, expiring operational layer of the main checkout
//! (`dagq.local.toml`, ADR-t5108-1): what one read of it found
//! ([`LocalLayerRead`]), which change between two reads a supervisor
//! records ([`LocalLayerTracker`]), the record ([`LocalLayerRecord`]) and
//! what `status` and `doctor` show of it ([`LocalLayerRead::report`]).
//!
//! The record is the queue event `supervisor_local_config_changed`
//! ([`crate::domain::EventKind::SupervisorLocalConfigChanged`]), written by
//! a supervisor's pass only when its read differs from the pass before
//! ([`LocalLayerTracker::observe`]), its payload a [`LocalLayerRecord`]
//! (the fields are on it; [`LocalLayerChange`] names the changes).
//!
//! A supervisor starts from [`LocalLayerRead::absent`]: one that starts
//! over a file in use records it once.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// From how long before its `expires_at` `status` and `doctor` tell that
/// the layer expires soon (`expiring_soon`): 24 hours (86 400 seconds),
/// long enough that the inbox sees it in a day's watch before the values
/// go back.
pub const EXPIRY_WARNING_SECS: i64 = 24 * 60 * 60;

/// The most bytes of a [`LocalLayerRecord`] as JSON (64 KiB), to which a
/// supervisor fits each record before it sends it
/// ([`LocalLayerRecord::fit`]): well within one request to the queue
/// service ([`crate::domain::queue_service::MAX_REQUEST_BYTES`] less its
/// envelope), so that the record of any file the reader takes (up to
/// 4 MiB, whose values and names a record repeats) is one event.
pub const MAX_RECORD_BYTES: usize = 64 * 1024;
const _: () = assert!(
    MAX_RECORD_BYTES
        <= crate::domain::queue_service::MAX_REQUEST_BYTES
            - crate::domain::telemetry::REQUEST_ENVELOPE_BYTES
);

/// The most characters of a text a record that does not fit
/// [`MAX_RECORD_BYTES`] keeps of each value, name and committed change
/// (256); the rest is cut with `…`.
pub const RECORD_TEXT_CHARS: usize = 256;

/// The most characters of a rejection's `reason` a read keeps (1024),
/// cut with `…`: a reason can quote the file (a table's name), and it is
/// shown in `status`, `doctor` and the record.
pub const REASON_CHARS: usize = 1024;

/// `text` cut to `chars` characters with `…` when longer; whether it was.
fn cut(text: &mut String, chars: usize) -> bool {
    match text.char_indices().nth(chars) {
        Some((at, _)) => {
            text.truncate(at);
            text.push('…');
            true
        }
        None => false,
    }
}

/// Whether the layer is used, and if not, why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum LocalLayerState {
    /// No `dagq.local.toml` beside the main checkout's `dagq.toml`.
    Absent,
    /// The layer's keys override the committed values.
    Effective,
    /// Its `expires_at` passed: the committed values are used.
    Expired,
    /// The layer is not used: unreadable, tracked by Git, a key outside
    /// the allowlist, broken syntax or value, an expiry missing or too far
    /// ahead, or a committed `dagq.toml` that cannot be read.
    Rejected { reason: String },
}

impl LocalLayerState {
    /// [`LocalLayerState::Rejected`] for `reason`, cut to [`REASON_CHARS`].
    pub fn rejected(reason: impl Into<String>) -> Self {
        let mut reason = reason.into();
        cut(&mut reason, REASON_CHARS);
        Self::Rejected { reason }
    }
}

/// One key the layer overrides: its table and name, the value the layer
/// gives (`local`) and the committed `dagq.toml`'s (`committed`, `None`
/// when the committed file does not set it), each as written without a
/// comment (a string's quotes normalized), and each as the line's text
/// without its comment (`local_text`, `committed_text`), which the
/// comparisons give to the parser ([`ValueComparer`]). `redundant`: with the layer's other keys
/// in effect, the parser reads the layer's value as the committed one (one
/// value spelled two ways, as `'5%'` and `'5.0%'`, is the same), so
/// removing the line leaves the configuration in effect as it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalKey {
    pub table: String,
    pub key: String,
    pub local: String,
    pub committed: Option<String>,
    #[serde(default)]
    pub local_text: String,
    #[serde(default)]
    pub committed_text: Option<String>,
    #[serde(default)]
    pub redundant: bool,
}

impl LocalKey {
    fn name(&self) -> String {
        format!("{}.{}", self.table, self.key)
    }
    fn id(&self) -> (String, String) {
        (self.table.clone(), self.key.clone())
    }
    /// The layer sets the same value as the committed file: a line not
    /// needed.
    pub fn redundant(&self) -> bool {
        self.redundant
    }
}

/// Whether two texts of one key's value are one value as the parser reads
/// them, given by the read that knows the committed file: the reader of
/// [`LocalLayerRead`] substitutes each text for the key in the
/// configuration the layer makes (the layer's other keys over that file)
/// and compares what the parser makes of the two (`same(table, key, a,
/// b)`).
/// Without one (a read made by hand), texts compare as written.
#[derive(Clone, Default)]
pub struct ValueComparer(Option<std::sync::Arc<SameValue>>);

/// `same(table, key, a, b)` of a [`ValueComparer`].
type SameValue = dyn Fn(&str, &str, &str, &str) -> bool + Send + Sync;

impl ValueComparer {
    /// A comparer that asks `same`.
    pub fn new(same: impl Fn(&str, &str, &str, &str) -> bool + Send + Sync + 'static) -> Self {
        Self(Some(std::sync::Arc::new(same)))
    }
    /// Whether the committed texts `a` and `b` of `table`.`key` (`None`:
    /// not set) are the same value.
    pub fn same(&self, table: &str, key: &str, a: Option<&str>, b: Option<&str>) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => match &self.0 {
                Some(same) => same(table, key, a, b),
                None => a.trim() == b.trim(),
            },
            _ => false,
        }
    }
}

impl std::fmt::Debug for ValueComparer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ValueComparer")
    }
}

/// Two reads are compared by what they found, not by how they compare.
impl PartialEq for ValueComparer {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for ValueComparer {}

/// What one read of the layer found. `hash` is the SHA-256 of the file's
/// bytes whether or not the layer is used (`None` when absent or when the
/// file could not be read), `expires_at` its expiry in Unix milliseconds
/// once read, `keys` the overridden keys (empty unless the layer is used).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalLayerRead {
    #[serde(flatten)]
    pub state: LocalLayerState,
    pub hash: Option<String>,
    #[serde(skip)]
    pub expires_at: Option<i64>,
    pub keys: Vec<LocalKey>,
    /// How this read compares two texts of a key's value.
    #[serde(skip)]
    pub values: ValueComparer,
}

/// A change a [`LocalLayerRecord`] records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalLayerChange {
    /// The layer began to be used.
    Effective,
    /// Its bytes changed and it is still used.
    Changed,
    /// The file is the same, but the committed value of a key it
    /// overrides changed, as a landing changes it.
    CommittedChanged,
    /// Its `expires_at` passed: recorded once until the file changes.
    Expired,
    /// It is not used (`reason`); again only for another reason or file.
    Rejected,
    /// The file went.
    Removed,
}

/// A key of a [`LocalLayerRecord`]: the [`LocalKey`] read, whose values
/// and texts in a record only show, with what `status` compares the
/// committed value now with. That is `committed_at_apply`, the committed
/// value's text when the layer last began or changed, compared as the
/// parser reads both ([`ValueComparer`]); or, when that text was too long
/// for the record ([`LocalLayerRecord::fit`] then cuts it to show),
/// `committed_at_apply_sha256`, the SHA-256 of the whole text, which the
/// committed text now is compared with as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedKey {
    #[serde(flatten)]
    pub key: LocalKey,
    #[serde(default)]
    pub committed_at_apply: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_at_apply_sha256: Option<String>,
}

impl RecordedKey {
    /// The SHA-256 of the whole committed text at apply, `None` when the
    /// committed file did not set the key.
    fn basis_sha256(&self) -> Option<String> {
        self.committed_at_apply_sha256.clone().or_else(|| {
            self.committed_at_apply
                .as_deref()
                .map(|text| sha256(text.as_bytes()))
        })
    }
}

fn sha256(bytes: &[u8]) -> String {
    crate::domain::replan_snapshot::sha256(bytes)
}

/// The SHA-256 a record keeps of the keys it leaves out
/// (`omitted_sha256`): of the JSON array of each key's table, name and
/// SHA-256 of its committed text (null when not set), in the keys' order.
fn omitted_digest<'a>(keys: impl Iterator<Item = (&'a str, &'a str, Option<String>)>) -> String {
    let keys: Vec<_> = keys
        .map(|(table, key, committed)| json!([table, key, committed]))
        .collect();
    sha256(&serde_json::to_vec(&keys).unwrap_or_default())
}

/// A committed value that moved under the layer, as written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedChange {
    pub table: String,
    pub key: String,
    pub from: Option<String>,
    pub to: Option<String>,
}

/// The payload of `supervisor_local_config_changed`. `supervisor` is the
/// token of the supervisor that read the file and `read_at` when
/// (RFC 3339); `state` (with `reason` when rejected), `hash`, `expires_at`
/// (RFC 3339) and `keys` are its read; `committed_changes` the keys whose
/// committed value moved, with `committed_changed`; `in_use` the
/// `[stall]`, `[disk]` and `[resume]` the supervisor uses after the pass,
/// from the same read, so a change of the values used is in this one
/// record rather than in an event per value. `keys_omitted` and
/// `committed_changes_omitted` count those left out to fit the record
/// ([`LocalLayerRecord::fit`]), and `omitted_sha256` is what `status`
/// compares the left-out keys' committed values with
/// ([`omitted_digest`]). Fields a record lacks read as empty; a
/// payload that does not read as this type (another binary's, or
/// damaged) is no record ([`LocalLayerRecord::of`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalLayerRecord {
    pub change: LocalLayerChange,
    #[serde(flatten)]
    pub state: LocalLayerState,
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub read_at: Option<String>,
    #[serde(default)]
    pub keys: Vec<RecordedKey>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub committed_changes: Vec<CommittedChange>,
    #[serde(default)]
    pub supervisor: Option<String>,
    #[serde(default)]
    pub in_use: Option<Value>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub keys_omitted: usize,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub committed_changes_omitted: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted_sha256: Option<String>,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// The bytes of `value` as JSON.
fn json_size(value: &impl Serialize) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

impl LocalLayerRecord {
    /// The record an event's `payload` holds; `None` when it does not
    /// read as one, which `status` and `doctor` take as no record.
    pub fn of(payload: &Value) -> Option<Self> {
        serde_json::from_value(payload.clone()).ok()
    }

    /// This record made to fit [`MAX_RECORD_BYTES`] as JSON, so that one
    /// change is one event whatever file was read, keeping what `status`
    /// compares with before what only shows. A record that fits is left as
    /// it is. Otherwise, in turn, while it does not fit: each text that
    /// only shows (the reason, a key's values and texts, a committed
    /// change's values) longer than [`RECORD_TEXT_CHARS`] is cut there with
    /// `…`; then a longer `committed_at_apply` is cut the same way, its
    /// whole text kept as `committed_at_apply_sha256`; then the keys that
    /// no longer fit are left out from the first that does not, counted in
    /// `keys_omitted` and kept together as `omitted_sha256`, and so are the
    /// committed changes, counted in `committed_changes_omitted`. A key's
    /// table and name are never cut: `status` finds the key by them. The
    /// `hash` is still of the whole file.
    pub fn fit(&mut self) {
        if json_size(self) <= MAX_RECORD_BYTES {
            return;
        }
        let short = |text: &mut String| cut(text, RECORD_TEXT_CHARS);
        if let LocalLayerState::Rejected { reason } = &mut self.state {
            short(reason);
        }
        for recorded in &mut self.keys {
            let key = &mut recorded.key;
            short(&mut key.local);
            short(&mut key.local_text);
            for text in [&mut key.committed, &mut key.committed_text]
                .into_iter()
                .flatten()
            {
                short(text);
            }
        }
        for change in &mut self.committed_changes {
            for text in [&mut change.from, &mut change.to].into_iter().flatten() {
                short(text);
            }
        }
        if json_size(self) > MAX_RECORD_BYTES {
            for recorded in &mut self.keys {
                if let Some(text) = &mut recorded.committed_at_apply
                    && text.chars().count() > RECORD_TEXT_CHARS
                {
                    recorded.committed_at_apply_sha256 = Some(sha256(text.as_bytes()));
                    short(text);
                }
            }
        }
        if json_size(self) <= MAX_RECORD_BYTES {
            return;
        }
        let keys = std::mem::take(&mut self.keys);
        let changes = std::mem::take(&mut self.committed_changes);
        // Room for the counts and the digest, which are added after.
        let mut used = json_size(self) + 192;
        let mut room = |size: usize| {
            let fits = used + size < MAX_RECORD_BYTES;
            if fits {
                used += size + 1;
            }
            fits
        };
        let mut omitted = Vec::new();
        for key in keys {
            if omitted.is_empty() && room(json_size(&key)) {
                self.keys.push(key);
            } else {
                omitted.push(key);
            }
        }
        for change in changes {
            if self.committed_changes_omitted == 0 && room(json_size(&change)) {
                self.committed_changes.push(change);
            } else {
                self.committed_changes_omitted += 1;
            }
        }
        if !omitted.is_empty() {
            self.keys_omitted = omitted.len();
            self.omitted_sha256 = Some(omitted_digest(omitted.iter().map(|key| {
                (
                    key.key.table.as_str(),
                    key.key.key.as_str(),
                    key.basis_sha256(),
                )
            })));
        }
    }
}

impl LocalLayerRead {
    /// No layer: what a supervisor starts from.
    pub fn absent() -> Self {
        Self {
            state: LocalLayerState::Absent,
            hash: None,
            expires_at: None,
            keys: Vec::new(),
            values: ValueComparer::default(),
        }
    }

    /// The hash of the file while the layer is used, for the audit's
    /// records of the settings.
    pub fn effective_hash(&self) -> Option<&str> {
        (self.state == LocalLayerState::Effective)
            .then_some(self.hash.as_deref())
            .flatten()
    }

    fn committed(&self) -> BTreeMap<(String, String), Option<String>> {
        self.keys
            .iter()
            .map(|key| (key.id(), key.committed_text.clone()))
            .collect()
    }

    /// Whether `key`'s committed value is `text` as the parser reads both.
    fn committed_is(&self, key: &LocalKey, text: Option<&str>) -> bool {
        self.values
            .same(&key.table, &key.key, text, key.committed_text.as_deref())
    }

    /// `status`' and `doctor`'s `local_config`, at `now` (Unix
    /// milliseconds), `None` while there is no file. `last` is the newest
    /// record; only one of this same file (its `hash`) is compared with: a
    /// key's committed value that differs from its `committed_at_apply`
    /// there (as the parser reads both, or as written against
    /// `committed_at_apply_sha256` when the record cut it) changed after
    /// the layer began; the keys the record left out are compared together
    /// against its `omitted_sha256`; and a record of another file (an older
    /// layer, or this one not yet seen by a supervisor) says nothing of this
    /// one. The fields: `state`, `reason` (rejected),
    /// `hash`, `expires_at` (RFC 3339), `expires_in_secs` and
    /// `expiring_soon` (within [`EXPIRY_WARNING_SECS`]), `keys` (each with
    /// `committed`, `local`, `redundant` and `committed_changed`, the
    /// texts `{from, to}` the parser reads apart, or null), `warnings` (one
    /// line per thing a person acts on: the layer rejected, expired or
    /// expiring soon, a redundant key, a key whose committed value
    /// changed, keys left out of the record whose committed values
    /// changed), `omitted_committed_changed` (only when the record left
    /// keys out: whether any of their committed values changed) and
    /// `last_change` (`change` and `read_at` of the newest record).
    pub fn report(&self, last: Option<&LocalLayerRecord>, now: i64) -> Option<Value> {
        if self.state == LocalLayerState::Absent {
            return None;
        }
        let mut warnings = Vec::new();
        let mut report = serde_json::to_value(self).ok()?;
        match &self.state {
            LocalLayerState::Rejected { reason } => {
                warnings.push(format!("dagq.local.toml is not used: {reason}"));
            }
            LocalLayerState::Expired => {
                warnings.push("dagq.local.toml expired: the committed values are used".into());
            }
            _ => {}
        }
        if let Some(expires_at) = self.expires_at {
            let left = (expires_at - now).div_euclid(1000);
            report["expires_at"] = json!(crate::domain::transcript::millis_text(expires_at));
            report["expires_in_secs"] = json!(left);
            let soon = self.state == LocalLayerState::Effective && left <= EXPIRY_WARNING_SECS;
            report["expiring_soon"] = json!(soon);
            if soon {
                warnings.push(format!("dagq.local.toml expires in {left}s"));
            }
        }
        let same_file = last.filter(|last| self.hash.is_some() && last.hash == self.hash);
        let applied = same_file
            .map(|last| {
                last.keys
                    .iter()
                    .map(|key| (key.key.id(), key))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let committed_sha256 = |key: &LocalKey| {
            key.committed_text
                .as_deref()
                .map(|text| sha256(text.as_bytes()))
        };
        report["keys"] = self
            .keys
            .iter()
            .map(|key| {
                let changed = applied
                    .get(&key.id())
                    .filter(|recorded| match &recorded.committed_at_apply_sha256 {
                        Some(whole) => committed_sha256(key).as_ref() != Some(whole),
                        None => !self.committed_is(key, recorded.committed_at_apply.as_deref()),
                    })
                    .map(|recorded| {
                        json!({"from": recorded.committed_at_apply, "to": key.committed_text})
                    });
                if key.redundant() {
                    warnings.push(format!(
                        "{} is the same as the committed value: the line is not needed",
                        key.name()
                    ));
                }
                if changed.is_some() {
                    warnings.push(format!(
                        "{}: the committed value changed after the layer began",
                        key.name()
                    ));
                }
                json!({
                    "table": key.table, "key": key.key, "committed": key.committed,
                    "local": key.local, "redundant": key.redundant(),
                    "committed_changed": changed,
                })
            })
            .collect();
        if let Some(digest) = same_file.and_then(|last| last.omitted_sha256.as_ref()) {
            let left_out: Vec<_> = self
                .keys
                .iter()
                .filter(|key| !applied.contains_key(&key.id()))
                .collect();
            let now = omitted_digest(
                left_out
                    .iter()
                    .map(|key| (key.table.as_str(), key.key.as_str(), committed_sha256(key))),
            );
            let changed = &now != digest;
            report["omitted_committed_changed"] = json!(changed);
            if changed {
                warnings.push(format!(
                    "a committed value of the {} keys left out of the record changed after the layer began",
                    left_out.len()
                ));
            }
        }
        report["warnings"] = json!(warnings);
        report["last_change"] = last
            .map(|last| json!({"change": last.change, "read_at": last.read_at}))
            .unwrap_or(Value::Null);
        Some(report)
    }
}

/// What a supervisor remembers between its reads of the layer: the last
/// read and the committed values when the layer last began or changed.
#[derive(Debug, Clone)]
pub struct LocalLayerTracker {
    last: LocalLayerRead,
    applied: BTreeMap<(String, String), Option<String>>,
}

impl Default for LocalLayerTracker {
    fn default() -> Self {
        Self {
            last: LocalLayerRead::absent(),
            applied: BTreeMap::new(),
        }
    }
}

impl LocalLayerTracker {
    /// The change from the last read to `read`, `None` when nothing a
    /// person acts on changed: the same file whose committed values the
    /// parser reads as before records nothing.
    fn change(&self, read: &LocalLayerRead) -> Option<LocalLayerChange> {
        use LocalLayerChange as C;
        use LocalLayerState::*;
        let before = &self.last;
        let same_file = before.hash == read.hash;
        match (&before.state, &read.state) {
            (_, Absent) => (before.state != Absent).then_some(C::Removed),
            (Effective, Effective) if same_file => {
                let moved = read.keys.iter().any(|key| {
                    let was = before.keys.iter().find(|was| was.id() == key.id());
                    !read.committed_is(key, was.and_then(|was| was.committed_text.as_deref()))
                });
                moved.then_some(C::CommittedChanged)
            }
            (Effective, Effective) => Some(C::Changed),
            (_, Effective) => Some(C::Effective),
            (Expired, Expired) if same_file => None,
            (_, Expired) => Some(C::Expired),
            (Rejected { reason: was }, Rejected { reason }) if same_file && was == reason => None,
            (_, Rejected { .. }) => Some(C::Rejected),
        }
    }

    /// The record of `read` (without `supervisor` and `in_use`, which the
    /// supervisor adds) when it changed from the last read, which it then
    /// becomes. `read_at` is when it was read (Unix milliseconds).
    pub fn observe(&mut self, read: LocalLayerRead, read_at: i64) -> Option<LocalLayerRecord> {
        let Some(change) = self.change(&read) else {
            self.last = read;
            return None;
        };
        let mut committed_changes = Vec::new();
        if change == LocalLayerChange::CommittedChanged {
            for key in &read.keys {
                let before = self.last.keys.iter().find(|before| before.id() == key.id());
                if let Some(before) = before
                    .filter(|before| !read.committed_is(key, before.committed_text.as_deref()))
                {
                    committed_changes.push(CommittedChange {
                        table: key.table.clone(),
                        key: key.key.clone(),
                        from: before.committed.clone(),
                        to: key.committed.clone(),
                    });
                }
            }
        } else {
            self.applied = read.committed();
        }
        let record = LocalLayerRecord {
            change,
            state: read.state.clone(),
            hash: read.hash.clone(),
            expires_at: read.expires_at.map(crate::domain::transcript::millis_text),
            read_at: Some(crate::domain::transcript::millis_text(read_at)),
            keys: read
                .keys
                .iter()
                .map(|key| RecordedKey {
                    committed_at_apply: self.applied.get(&key.id()).cloned().flatten(),
                    key: key.clone(),
                    committed_at_apply_sha256: None,
                })
                .collect(),
            committed_changes,
            supervisor: None,
            in_use: None,
            keys_omitted: 0,
            committed_changes_omitted: 0,
            omitted_sha256: None,
        };
        self.last = read;
        Some(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(local: &str, committed: Option<&str>) -> LocalKey {
        LocalKey {
            table: "disk".into(),
            key: "reserve".into(),
            local: local.into(),
            committed: committed.map(str::to_owned),
            local_text: local.into(),
            committed_text: committed.map(str::to_owned),
            redundant: committed == Some(local),
        }
    }

    fn effective(hash: &str, keys: Vec<LocalKey>) -> LocalLayerRead {
        LocalLayerRead {
            state: LocalLayerState::Effective,
            hash: Some(hash.into()),
            expires_at: Some(200_000_000),
            keys,
            values: ValueComparer::default(),
        }
    }

    fn other(state: LocalLayerState, hash: &str) -> LocalLayerRead {
        LocalLayerRead {
            state,
            hash: Some(hash.into()),
            expires_at: None,
            keys: Vec::new(),
            values: ValueComparer::default(),
        }
    }

    /// Each case records one event and an unchanged read none: the layer
    /// begins, changes, its committed value changes, it expires, is
    /// rejected and is removed; the record reads back as itself.
    #[test]
    fn each_change_of_the_layer_is_one_record_and_an_unchanged_read_none() {
        let mut tracker = LocalLayerTracker::default();
        let mut changes = Vec::new();
        let rejected = || {
            other(
                LocalLayerState::Rejected {
                    reason: "local table not allowed: repository".into(),
                },
                "c",
            )
        };
        let reads = [
            LocalLayerRead::absent(),
            effective("a", vec![key("'12%'", Some("'5%'"))]),
            effective("a", vec![key("'12%'", Some("'5%'"))]),
            effective("b", vec![key("'20%'", Some("'5%'"))]),
            effective("b", vec![key("'20%'", Some("'8%'"))]),
            effective("b", vec![key("'20%'", Some("'8%'"))]),
            other(LocalLayerState::Expired, "b"),
            other(LocalLayerState::Expired, "b"),
            rejected(),
            rejected(),
            LocalLayerRead::absent(),
            LocalLayerRead::absent(),
        ];
        for (at, read) in reads.into_iter().enumerate() {
            if let Some(record) = tracker.observe(read, at as i64) {
                changes.push(record);
            }
        }
        use LocalLayerChange as C;
        assert_eq!(
            changes
                .iter()
                .map(|record| record.change)
                .collect::<Vec<_>>(),
            [
                C::Effective,
                C::Changed,
                C::CommittedChanged,
                C::Expired,
                C::Rejected,
                C::Removed
            ]
        );
        let payload = serde_json::to_value(&changes[0]).unwrap();
        assert_eq!(payload["change"], "effective");
        assert_eq!(payload["state"], "effective");
        assert_eq!(payload["hash"], "a");
        assert_eq!(payload["keys"][0]["local"], "'12%'");
        assert_eq!(payload["keys"][0]["committed"], "'5%'");
        assert_eq!(payload["expires_at"], "1970-01-03T07:33:20.000Z");
        // The committed value when the layer began is kept across the
        // landing that changed it, which `status` compares with.
        assert_eq!(
            changes[2].keys[0].committed_at_apply.as_deref(),
            Some("'5%'")
        );
        assert_eq!(
            serde_json::to_value(&changes[2]).unwrap()["committed_changes"],
            json!([{"table": "disk", "key": "reserve", "from": "'5%'", "to": "'8%'"}])
        );
        let rejected = serde_json::to_value(&changes[4]).unwrap();
        assert_eq!(rejected["state"], "rejected");
        assert_eq!(rejected["reason"], "local table not allowed: repository");
        assert_eq!(
            serde_json::to_value(&changes[5]).unwrap()["state"],
            "absent"
        );
        for record in &changes {
            let payload = serde_json::to_value(record).unwrap();
            assert_eq!(LocalLayerRecord::of(&payload).as_ref(), Some(record));
        }
    }

    /// A different reason or file for a rejected layer, and an expired
    /// layer whose file changed, are new records; a committed value spelled
    /// another way but taken as the same is none.
    #[test]
    fn a_new_reason_or_file_records_again_and_a_respelling_does_not() {
        let mut tracker = LocalLayerTracker::default();
        let reject = |reason: &str, hash| {
            other(
                LocalLayerState::Rejected {
                    reason: reason.into(),
                },
                hash,
            )
        };
        assert!(tracker.observe(reject("a", "1"), 0).is_some());
        assert!(tracker.observe(reject("b", "1"), 0).is_some());
        assert!(tracker.observe(reject("b", "2"), 0).is_some());
        assert!(tracker.observe(reject("b", "2"), 0).is_none());
        assert!(
            tracker
                .observe(other(LocalLayerState::Expired, "2"), 0)
                .is_some()
        );
        assert!(
            tracker
                .observe(other(LocalLayerState::Expired, "3"), 0)
                .is_some()
        );
        assert!(
            tracker
                .observe(other(LocalLayerState::Expired, "3"), 0)
                .is_none()
        );
        // The read's comparer stands for the parser, which reads `1_000`
        // as `1000`: a respelling is no change, another value is one.
        let spelled = |committed: &str| LocalLayerRead {
            values: ValueComparer::new(|_, _, a, b| a.replace('_', "") == b.replace('_', "")),
            ..effective(
                "e",
                vec![LocalKey {
                    table: "stall".into(),
                    key: "idle_without_receipt_secs".into(),
                    local: "200".into(),
                    committed: Some(committed.into()),
                    local_text: "200".into(),
                    committed_text: Some(committed.into()),
                    redundant: false,
                }],
            )
        };
        assert!(tracker.observe(spelled("1000"), 0).is_some());
        assert!(tracker.observe(spelled("1_000"), 0).is_none());
        assert_eq!(
            tracker
                .observe(spelled("1001"), 0)
                .map(|record| record.change),
            Some(LocalLayerChange::CommittedChanged)
        );
    }

    /// `status`/`doctor` show each key's committed and local values and
    /// the expiry, tell of an expiry within the threshold, a rejection's
    /// reason, a redundant key by its parsed value and a committed value
    /// changed since this file's layer began, and show nothing for no
    /// file.
    #[test]
    fn the_report_shows_keys_expiry_rejection_and_drift() {
        assert_eq!(LocalLayerRead::absent().report(None, 0), None);
        let read = LocalLayerRead {
            keys: vec![
                key("'12%'", Some("'8%'")),
                LocalKey {
                    table: "stall".into(),
                    key: "idle_without_receipt_secs".into(),
                    local: "1_000".into(),
                    committed: Some("1000".into()),
                    local_text: "1_000".into(),
                    committed_text: Some("1000".into()),
                    redundant: true,
                },
            ],
            ..effective("h", Vec::new())
        };
        let mut tracker = LocalLayerTracker::default();
        let began = tracker
            .observe(
                LocalLayerRead {
                    keys: vec![key("'12%'", Some("'5%'")), read.keys[1].clone()],
                    ..read.clone()
                },
                0,
            )
            .unwrap();
        let far = read.report(Some(&began), 0).unwrap();
        assert_eq!(far["state"], "effective");
        assert_eq!(far["hash"], "h");
        assert_eq!(far["expires_at"], "1970-01-03T07:33:20.000Z");
        assert_eq!(far["expires_in_secs"], 200_000);
        assert_eq!(far["expiring_soon"], false);
        assert_eq!(far["keys"][0]["committed"], "'8%'");
        assert_eq!(far["keys"][0]["local"], "'12%'");
        assert_eq!(far["keys"][0]["redundant"], false);
        assert_eq!(
            far["keys"][0]["committed_changed"],
            json!({"from": "'5%'", "to": "'8%'"})
        );
        assert_eq!(far["keys"][1]["redundant"], true);
        assert_eq!(far["keys"][1]["committed_changed"], Value::Null);
        assert_eq!(
            far["last_change"],
            json!({"change": "effective", "read_at": "1970-01-01T00:00:00.000Z"})
        );
        let warnings = far["warnings"].to_string();
        assert!(
            warnings.contains("stall.idle_without_receipt_secs is the same"),
            "{warnings}"
        );
        assert!(
            warnings.contains("disk.reserve: the committed value changed"),
            "{warnings}"
        );
        // A record of another file (an older layer A) says nothing of B.
        let older = LocalLayerRecord {
            hash: Some("a".into()),
            ..began.clone()
        };
        let other_file = read.report(Some(&older), 0).unwrap();
        assert_eq!(other_file["keys"][0]["committed_changed"], Value::Null);
        assert!(
            !other_file["warnings"]
                .to_string()
                .contains("committed value changed")
        );
        let soon = read
            .report(None, 200_000_000 - EXPIRY_WARNING_SECS * 1000)
            .unwrap();
        assert_eq!(soon["expiring_soon"], true);
        assert!(soon["warnings"].to_string().contains("expires in 86400s"));
        let rejected = other(
            LocalLayerState::Rejected {
                reason: "tracked by Git".into(),
            },
            "h",
        )
        .report(None, 0)
        .unwrap();
        assert_eq!(rejected["state"], "rejected");
        assert_eq!(rejected["reason"], "tracked by Git");
        assert_eq!(rejected["keys"], json!([]));
        assert!(
            rejected["warnings"][0]
                .as_str()
                .unwrap()
                .contains("tracked by Git")
        );
    }

    /// An old or damaged payload is no record; one missing the optional
    /// fields reads with them empty.
    /// A record of any size fits [`MAX_RECORD_BYTES`] as JSON, and `status`
    /// still shows a committed value that changed under it: one that fits
    /// is left as it is; a long layer value is cut to show while its key's
    /// `committed_at_apply` stays whole and is compared as the parser reads
    /// it; a long committed value is compared by its SHA-256; keys past the
    /// budget are left out and compared together by `omitted_sha256`. A
    /// reason is cut to [`REASON_CHARS`] when read.
    #[test]
    fn a_record_of_any_size_fits_one_event_and_still_shows_drift() {
        let mut tracker = LocalLayerTracker::default();
        let small = tracker
            .observe(effective("h", vec![key("200", Some("100"))]), 0)
            .unwrap();
        let mut fitted = small.clone();
        fitted.fit();
        assert_eq!(fitted, small);
        // Fitted as the supervisor fits it, and read back as `status` reads it.
        let fitted_of = |keys: Vec<LocalKey>| {
            let mut tracker = LocalLayerTracker::default();
            let mut record = tracker.observe(effective("h", keys), 0).unwrap();
            record.in_use = Some(json!({"stall": {"idle_without_receipt_secs": 200}}));
            assert!(json_size(&record) > MAX_RECORD_BYTES);
            record.fit();
            assert!(
                json_size(&record) <= MAX_RECORD_BYTES,
                "{}",
                json_size(&record)
            );
            assert_eq!(
                record.in_use,
                Some(json!({"stall": {"idle_without_receipt_secs": 200}}))
            );
            LocalLayerRecord::of(&serde_json::to_value(&record).unwrap()).unwrap()
        };
        let drift = |keys: Vec<LocalKey>, record: &LocalLayerRecord| {
            effective("h", keys).report(Some(record), 0).unwrap()
        };
        // Each control character is six bytes of JSON.
        let long = "\u{1}".repeat(20_000);
        // A long layer value only shows.
        let record = fitted_of(vec![key(&long, Some("100"))]);
        assert_eq!(
            record.keys[0].key.local.chars().count(),
            RECORD_TEXT_CHARS + 1
        );
        assert_eq!(record.keys[0].committed_at_apply.as_deref(), Some("100"));
        assert_eq!(record.keys[0].committed_at_apply_sha256, None);
        assert_eq!(
            drift(vec![key(&long, Some("150"))], &record)["keys"][0]["committed_changed"],
            json!({"from": "100", "to": "150"})
        );
        assert_eq!(
            drift(vec![key(&long, Some("100"))], &record)["keys"][0]["committed_changed"],
            Value::Null
        );
        // A long committed value is compared by its SHA-256.
        let record = fitted_of(vec![key("200", Some(&long))]);
        assert!(record.keys[0].committed_at_apply_sha256.is_some());
        let changed = drift(vec![key("200", Some("150"))], &record);
        assert_eq!(changed["keys"][0]["committed_changed"]["to"], "150");
        assert!(
            changed["warnings"]
                .to_string()
                .contains("committed value changed")
        );
        assert_eq!(
            drift(vec![key("200", Some(&long))], &record)["keys"][0]["committed_changed"],
            Value::Null
        );
        // Keys past the budget are left out, and compared together.
        let shown = "\u{1}".repeat(5_000);
        let many = |last: &str| -> Vec<LocalKey> {
            (0..100)
                .map(|at| {
                    let committed = if at == 99 {
                        last.to_owned()
                    } else {
                        at.to_string()
                    };
                    LocalKey {
                        key: format!("k{at:03}"),
                        ..key(&shown, Some(&committed))
                    }
                })
                .collect()
        };
        let record = fitted_of(many("99"));
        assert!(record.keys_omitted > 0 && !record.keys.is_empty());
        assert_eq!(record.keys.len() + record.keys_omitted, 100);
        assert!(record.omitted_sha256.is_some());
        let same = drift(many("99"), &record);
        assert_eq!(same["omitted_committed_changed"], false);
        let moved = drift(many("150"), &record);
        assert_eq!(moved["omitted_committed_changed"], true);
        assert!(
            moved["warnings"]
                .to_string()
                .contains("left out of the record changed")
        );
        let LocalLayerState::Rejected { reason } = LocalLayerState::rejected("x".repeat(5_000))
        else {
            unreachable!()
        };
        assert_eq!(reason.chars().count(), REASON_CHARS + 1);
    }

    #[test]
    fn a_payload_that_does_not_read_is_no_record() {
        assert_eq!(LocalLayerRecord::of(&json!({"change": "unknown"})), None);
        assert_eq!(LocalLayerRecord::of(&json!({"keys": []})), None);
        let bare = LocalLayerRecord::of(&json!({"change": "removed", "state": "absent"})).unwrap();
        assert_eq!(bare.change, LocalLayerChange::Removed);
        assert!(bare.keys.is_empty() && bare.hash.is_none());
    }
}
