//! A ledger row past one request or one answer of the queue service
//! (`MAX_REQUEST_BYTES`, `MAX_ANSWER_BYTES`), as what builds the ledger
//! writes and reads it: its JSON crosses in parts, each within the limit,
//! named by the digest of the whole text.
//!
//! A write sends its parts first, which the service holds in its process
//! (none is written to the queue), and then the write itself, which takes
//! them, checks the text against the digest and writes the row over the
//! one read in one transaction, as a row sent whole is. Parts held longer
//! than [`STAGED_SECS`] are dropped, and no more than [`STAGED_BYTES`] are
//! held at once: a write whose parts are gone (the service started again
//! between them) fails, and the next pass sends it again. A read asks for
//! the row's text a part at a time, each with the row's fingerprint, and
//! starts again while a part is of another row than the one before (another
//! writer wrote it between them).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::queue_service::{MAX_ANSWER_BYTES, MAX_REQUEST_BYTES};

/// How many bytes of a row's text one part of a write holds at most: its
/// JSON string, every character escaped at worst, stays within the
/// request's limit.
pub(super) const WRITE_PART_BYTES: usize = MAX_REQUEST_BYTES * 3 / 8;

/// How many bytes of a row's text one part of a read holds at most, within
/// the answer's limit as [`WRITE_PART_BYTES`] is within the request's.
pub(super) const READ_PART_BYTES: usize = MAX_ANSWER_BYTES * 3 / 8;

/// How long (seconds) the service holds the parts of a write not taken.
pub const STAGED_SECS: i64 = 600;

/// How many bytes of parts the service holds at most, all writes together.
pub const STAGED_BYTES: usize = 256 << 20;

/// The parts of the writes not taken yet, by the digest of their text.
static STAGED: LazyLock<Mutex<HashMap<String, Staged>>> = LazyLock::new(Default::default);

struct Staged {
    parts: Vec<Option<String>>,
    bytes: usize,
    at: i64,
}

/// One part of a row's text as a read answers it: the row's fingerprint
/// ([`crate::domain::ledger::LedgerRow::fingerprint`], the digest of the
/// whole text), the text's length, the part and where the next begins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowPart {
    pub fingerprint: String,
    pub total: usize,
    pub part: String,
    pub next: usize,
}

/// The digest of `text`, as the fingerprint of a row is of its JSON.
pub(super) fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// The end of a part of `text` from `offset` of at most `bytes`: on a
/// character's boundary, and past `offset` while there is text.
fn part_end(text: &str, offset: usize, bytes: usize) -> usize {
    let mut end = offset.saturating_add(bytes.max(1)).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == offset && offset < text.len() {
        end = offset + 1;
        while !text.is_char_boundary(end) {
            end += 1;
        }
    }
    end
}

/// `text` in consecutive parts of at most `bytes` each, on characters'
/// boundaries.
pub(super) fn split(text: &str, bytes: usize) -> Vec<&str> {
    let (mut parts, mut offset) = (Vec::new(), 0);
    while offset < text.len() {
        let end = part_end(text, offset, bytes);
        parts.push(&text[offset..end]);
        offset = end;
    }
    parts
}

/// The part of the row's `text` (whose fingerprint is `fingerprint`) from
/// `offset`, at most `bytes` of it.
pub(super) fn part_at(
    text: &str,
    fingerprint: String,
    offset: usize,
    bytes: usize,
) -> Result<RowPart> {
    ensure!(
        offset <= text.len() && text.is_char_boundary(offset),
        "the row's text has no part at {offset}"
    );
    let next = part_end(text, offset, bytes);
    Ok(RowPart {
        fingerprint,
        total: text.len(),
        part: text[offset..next].to_owned(),
        next,
    })
}

fn staged() -> std::sync::MutexGuard<'static, HashMap<String, Staged>> {
    STAGED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Hold `part`, the `index`th of the `count` parts of the text whose digest
/// is `digest`, at `now` (unix seconds); the parts held past
/// [`STAGED_SECS`] go first. Refused past [`STAGED_BYTES`] held.
pub(super) fn stage(
    digest: &str,
    index: usize,
    count: usize,
    part: String,
    now: i64,
) -> Result<()> {
    ensure!(
        index < count && count <= STAGED_BYTES / WRITE_PART_BYTES + 1,
        "part {index} of {count} is no part of a row's write"
    );
    let mut staged = staged();
    staged.retain(|_, held| now.saturating_sub(held.at) < STAGED_SECS);
    let held: usize = staged.values().map(|held| held.bytes).sum();
    let entry = staged.entry(digest.to_owned()).or_insert_with(|| Staged {
        parts: vec![None; count],
        bytes: 0,
        at: now,
    });
    ensure!(
        entry.parts.len() == count,
        "the write of {digest} was sent in {} parts, not {count}",
        entry.parts.len()
    );
    let before = entry.parts[index].as_ref().map_or(0, String::len);
    if held - before + part.len() > STAGED_BYTES {
        bail!("the queue service holds {held} bytes of rows' parts already, its limit");
    }
    entry.bytes = entry.bytes - before + part.len();
    entry.parts[index] = Some(part);
    entry.at = now;
    Ok(())
}

/// Take the text whose digest is `digest` from its parts held: it fails
/// while a part is missing or the text is not of the digest, and the parts
/// are dropped either way.
pub(super) fn take(digest: &str) -> Result<String> {
    let held = staged()
        .remove(digest)
        .with_context(|| format!("the queue service holds no parts of the row {digest}"))?;
    let mut text = String::with_capacity(held.bytes);
    for (index, part) in held.parts.into_iter().enumerate() {
        text.push_str(
            &part.with_context(|| format!("part {index} of the row {digest} did not come"))?,
        );
    }
    ensure!(
        self::digest(&text) == digest,
        "the parts of the row {digest} are of another text"
    );
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_splits_on_characters_and_reads_back_from_its_parts() {
        let text = "aé漢😀b".repeat(7);
        for bytes in [1, 2, 3, 5, 8, 1000] {
            let parts = split(&text, bytes);
            assert_eq!(parts.concat(), text, "{bytes}");
            assert!(parts.iter().all(|part| !part.is_empty()));
            let (mut read, mut offset) = (String::new(), 0);
            while offset < text.len() {
                let part = part_at(&text, "f".into(), offset, bytes).unwrap();
                assert_eq!(part.total, text.len());
                read.push_str(&part.part);
                offset = part.next;
            }
            assert_eq!(read, text, "{bytes}");
        }
        assert!(split("", 4).is_empty());
        assert!(part_at(&text, "f".into(), 2, 4).is_err());
        assert!(part_at(&text, "f".into(), text.len() + 1, 4).is_err());
    }

    #[test]
    fn a_write_takes_its_parts_once_all_came_only_of_its_digest_and_not_after_their_time() {
        let text = "{\"row\":\"one of the tests\"}";
        let id = digest(text);
        let parts = split(text, 5);
        for (index, part) in parts.iter().enumerate().rev() {
            stage(&id, index, parts.len(), (*part).to_owned(), 100).unwrap();
        }
        assert_eq!(take(&id).unwrap(), text);
        // Taken once.
        assert!(take(&id).is_err());
        // A part missing, or of another count.
        stage(&id, 0, parts.len(), parts[0].to_owned(), 100).unwrap();
        assert!(stage(&id, 1, parts.len() + 1, parts[1].to_owned(), 100).is_err());
        assert!(take(&id).is_err());
        // Of another text than its digest.
        let other = digest("another");
        stage(&other, 0, 1, text.to_owned(), 100).unwrap();
        assert!(take(&other).is_err());
        assert!(stage(&id, 3, 2, String::new(), 100).is_err());

        // Parts not taken go after their time (the held ones are one test's:
        // another's stage would drop them).
        stage(&id, 0, 1, text.to_owned(), 1_000).unwrap();
        stage(&digest("x"), 0, 1, "x".into(), 1_000 + STAGED_SECS).unwrap();
        assert!(take(&id).is_err());
        assert_eq!(take(&digest("x")).unwrap(), "x");
    }
}
