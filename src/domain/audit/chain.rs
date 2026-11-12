//! The chain of the audit records (ADR-t4840-2 decisions 2 and 3): each
//! record's hash is the SHA-256 of the record but its `hash`, normalized by
//! the JSON Canonicalization Scheme (RFC 8785), and holds the previous
//! record's hash, so a record rewritten, missing or moved shows in
//! [`verify`]. The version [`CHAIN_VERSION`] names the pair of the
//! normalization and the algorithm; a new version's first record holds the
//! old one's last hash as its `prev_hash`.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::record::AuditRecord;
use super::{ACTION_VERSION, SCHEMA_VERSION};

/// The version of the normalization and the hash's algorithm: v1 is JCS
/// and SHA-256.
pub const CHAIN_VERSION: &str = "v1";

/// The `prev_hash` of the first record of [`CHAIN_VERSION`]: the SHA-256
/// of the version's name.
pub fn genesis() -> String {
    sha256_hex(format!("dagq-audit-chain-{CHAIN_VERSION}").as_bytes())
}

/// `value` normalized by JCS (RFC 8785): the members of each object in the
/// order of their names' UTF-16 code units, no white space, the strings
/// escaped as JSON escapes them (`serde_json`'s escapes are JCS's), and the
/// numbers as IEEE 754 doubles written as ECMAScript writes them
/// ([`write_number`]).
pub fn canonical_bytes(value: &Value) -> Vec<u8> {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out.into_bytes()
}

/// `number` as an IEEE 754 double, written as ECMAScript's
/// `Number.prototype.toString` writes it (RFC 8785 3.2.2.3): the shortest
/// digits that read back to the same double, without an exponent when the
/// decimal point falls within 21 digits left or 6 zeros right of them, else
/// as `d.ddde±n`; minus zero is `0`. JSON holds no NaN or infinity. An
/// integer past 2^53 is the double nearest it, as JCS reads it.
pub fn write_number(number: f64, out: &mut String) {
    if number == 0.0 {
        out.push('0');
        return;
    }
    if number < 0.0 {
        out.push('-');
    }
    // Rust's `{:e}` writes the shortest round-trip digits (`d.ddde-7`),
    // but of two as near it may take either; ECMAScript takes the one
    // nearest the double, the even one on a tie, which is the double
    // rounded to as many digits (Rust rounds the exact value half to even)
    // when that reads back to it.
    let shortest = format!("{:e}", number.abs());
    let significant = shortest.split_once('e').map_or(0, |(mantissa, _)| {
        mantissa.chars().filter(char::is_ascii_digit).count()
    });
    let nearest = format!("{:.*e}", significant.saturating_sub(1), number.abs());
    let scientific = if nearest.parse::<f64>() == Ok(number.abs()) {
        nearest
    } else {
        shortest
    };
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("`{:e}` writes an exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    // The decimal point's place after the first `n` digits.
    let n = exponent
        .parse::<i32>()
        .expect("`{:e}` writes an integer exponent")
        + 1;
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if n - 1 < 0 { '-' } else { '+' });
        out.push_str(&(n - 1).abs().to_string());
    }
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => {
            out.push_str(&value.to_string());
        }
        Value::Number(number) => {
            write_number(number.as_f64().expect("a JSON number is finite"), out);
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(members) => {
            let mut sorted: Vec<(&String, &Value)> = members.iter().collect();
            sorted.sort_by(|(a, _), (b, _)| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (index, (name, member)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(name.clone()).to_string());
                out.push(':');
                write_canonical(member, out);
            }
            out.push('}');
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The normalized bytes a record's hash is computed on: the record without
/// its `hash`.
pub fn hashed_bytes(record: &AuditRecord) -> Vec<u8> {
    let mut value = serde_json::to_value(record).expect("a record serializes");
    if let Value::Object(members) = &mut value {
        members.remove("hash");
    }
    canonical_bytes(&value)
}

/// A record's hash: the SHA-256, in lowercase hex, of [`hashed_bytes`].
pub fn record_hash(record: &AuditRecord) -> String {
    sha256_hex(&hashed_bytes(record))
}

/// Where a check of the chain starts: the `seq` it expects first, the hash
/// that record must hold as its `prev_hash`, and each sender's last
/// `sender_seq` before it. From the start of the chain ([`Self::GENESIS`]),
/// a sender's first record is its 1; from a checkpoint, a sender not named
/// starts where its first record says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainAnchor {
    pub next_seq: u64,
    pub prev_hash: String,
    pub senders: BTreeMap<String, u64>,
    pub from_genesis: bool,
}

impl ChainAnchor {
    /// The start of the chain.
    pub fn genesis() -> Self {
        Self {
            next_seq: 1,
            prev_hash: genesis(),
            senders: BTreeMap::new(),
            from_genesis: true,
        }
    }
}

/// How a record differs from what the chain expects there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MismatchKind {
    /// The record's fields do not give its stored hash: a field rewritten.
    Hash,
    /// The record does not hold the previous record's hash: a record
    /// replaced, or its `prev_hash` changed and its hash computed again.
    PrevHash,
    /// The expected `seq` is nowhere in what follows: records removed.
    MissingSeq { expected: u64 },
    /// The expected `seq` comes later: records moved.
    OutOfOrder { expected: u64 },
    /// A sender's counter skipped: that sender's records removed or cut.
    SenderSeqGap { sender_id: String, expected: u64 },
    /// A sender's counter repeated or went back: a record twice.
    SenderSeqDuplicate { sender_id: String, last: u64 },
    /// A record's versions are not this binary's.
    UnknownVersion,
}

/// The first record where the chain does not hold: its stored `seq`, its
/// index in what was checked and how it differs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainMismatch {
    pub seq: u64,
    pub index: usize,
    pub kind: MismatchKind,
}

/// Checks `records`, in the order they are stored, from `anchor`, and
/// returns the first mismatch, or where a next check would start. The
/// checks of a record, in order: its versions, its `seq`, its `prev_hash`,
/// its hash, and its sender's `sender_seq`.
pub fn verify(anchor: ChainAnchor, records: &[AuditRecord]) -> Result<ChainAnchor, ChainMismatch> {
    let ChainAnchor {
        mut next_seq,
        mut prev_hash,
        mut senders,
        from_genesis,
    } = anchor;
    for (index, record) in records.iter().enumerate() {
        let mismatch = |kind| ChainMismatch {
            seq: record.seq,
            index,
            kind,
        };
        if record.schema_version != SCHEMA_VERSION
            || record.action_version != ACTION_VERSION
            || record.chain_version != CHAIN_VERSION
        {
            return Err(mismatch(MismatchKind::UnknownVersion));
        }
        if record.seq != next_seq {
            let later = records[index..].iter().any(|r| r.seq == next_seq);
            return Err(mismatch(if later {
                MismatchKind::OutOfOrder { expected: next_seq }
            } else {
                MismatchKind::MissingSeq { expected: next_seq }
            }));
        }
        if record.prev_hash != prev_hash {
            return Err(mismatch(MismatchKind::PrevHash));
        }
        if record_hash(record) != record.hash {
            return Err(mismatch(MismatchKind::Hash));
        }
        let last = senders.get(&record.sender_id).copied();
        let expected = match last {
            Some(last) => Some(last.saturating_add(1)),
            None if from_genesis => Some(1),
            None => None,
        };
        if let Some(expected) = expected {
            if record.sender_seq < expected {
                return Err(mismatch(MismatchKind::SenderSeqDuplicate {
                    sender_id: record.sender_id.clone(),
                    last: expected - 1,
                }));
            }
            if record.sender_seq > expected {
                return Err(mismatch(MismatchKind::SenderSeqGap {
                    sender_id: record.sender_id.clone(),
                    expected,
                }));
            }
        }
        senders.insert(record.sender_id.clone(), record.sender_seq);
        next_seq = next_seq.saturating_add(1);
        prev_hash.clone_from(&record.hash);
    }
    Ok(ChainAnchor {
        next_seq,
        prev_hash,
        senders,
        from_genesis,
    })
}

#[cfg(test)]
mod tests {
    use super::super::record::fixtures::{admission, input};
    use super::super::record::{Change, admit};
    use super::super::{AuditAction, Sender};
    use super::*;
    use serde_json::json;

    /// A chain of `n` records, the queue service's and the integrator's in
    /// turn, each sender counting from 1.
    fn chain_of(n: u64) -> Vec<AuditRecord> {
        let mut records: Vec<AuditRecord> = Vec::new();
        let mut counts = [0u64; 2];
        for seq in 1..=n {
            let prev = records.last().map_or_else(genesis, |r| r.hash.clone());
            let (slot, sender, action) = if seq % 2 == 1 {
                (0, Sender::QueueService, AuditAction::AskAnswered)
            } else {
                (1, Sender::Integrate, AuditAction::LandingMerged)
            };
            counts[slot] += 1;
            records
                .push(admit(input(counts[slot], action), admission(seq, &prev, sender)).unwrap());
        }
        records
    }

    /// The record's hash and its successors' computed again, as one who
    /// rewrites the chain from there would.
    fn rehash_from(records: &mut [AuditRecord], from: usize) {
        for index in from..records.len() {
            if index > 0 {
                records[index].prev_hash = records[index - 1].hash.clone();
            }
            records[index].hash = record_hash(&records[index]);
        }
    }

    #[test]
    fn jcs_orders_members_by_utf16_and_writes_no_white_space() {
        // RFC 8785 3.2.3's names, in its order.
        let value: Value = serde_json::from_str(
            r#"{"\u20ac": "Euro Sign", "\r": "Carriage Return", "\ufb33": "Hebrew Letter Dalet With Dagesh",
                "1": "One", "\ud83d\ude00": "Emoji: Grinning Face", "\u0080": "Control", "\u00f6": "Latin Small Letter O With Diaeresis"}"#,
        )
        .unwrap();
        let bytes = String::from_utf8(canonical_bytes(&value)).unwrap();
        let names: Vec<String> = bytes
            .trim_matches(|c| c == '{' || c == '}')
            .split(',')
            .map(|member| member.split(':').next().unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            [
                "\"\\r\"",
                "\"1\"",
                "\"\u{80}\"",
                "\"\u{f6}\"",
                "\"\u{20ac}\"",
                "\"\u{1f600}\"",
                "\"\u{fb33}\""
            ]
        );
        // A known input and its output.
        let value: Value = serde_json::from_str(
            "{ \"b\" : [ 1 , true , null ] ,\n \"a\" : { \"y\" : \"\\u000F\\\"\" , \"x\" : -3 } }",
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(canonical_bytes(&value)).unwrap(),
            r#"{"a":{"x":-3,"y":"\u000f\""},"b":[1,true,null]}"#
        );
    }

    /// RFC 8785 Appendix B's doubles, by their bits, and their text.
    #[test]
    fn jcs_writes_numbers_as_ecmascript_does() {
        for (bits, text) in [
            (0x0000000000000000u64, "0"),
            (0x8000000000000000, "0"),
            (0x0000000000000001, "5e-324"),
            (0x8000000000000001, "-5e-324"),
            (0x7fefffffffffffff, "1.7976931348623157e+308"),
            (0xffefffffffffffff, "-1.7976931348623157e+308"),
            (0x4340000000000000, "9007199254740992"),
            (0xc340000000000000, "-9007199254740992"),
            (0x4430000000000000, "295147905179352830000"),
            (0x44b52d02c7e14af5, "9.999999999999997e+22"),
            (0x44b52d02c7e14af6, "1e+23"),
            (0x44b52d02c7e14af7, "1.0000000000000001e+23"),
            (0x444b1ae4d6e2ef4e, "999999999999999700000"),
            (0x444b1ae4d6e2ef4f, "999999999999999900000"),
            (0x444b1ae4d6e2ef50, "1e+21"),
            (0x3eb0c6f7a0b5ed8c, "9.999999999999997e-7"),
            (0x3eb0c6f7a0b5ed8d, "0.000001"),
            (0x41b3de4355555553, "333333333.3333332"),
            (0x41b3de4355555554, "333333333.33333325"),
            (0x41b3de4355555555, "333333333.3333333"),
            (0x41b3de4355555556, "333333333.3333334"),
            (0x41b3de4355555557, "333333333.33333343"),
            (0xbecbf647612f3696, "-0.0000033333333333333333"),
            (0x43143ff3c1cb0959, "1424953923781206.2"),
        ] {
            let mut out = String::new();
            write_number(f64::from_bits(bits), &mut out);
            assert_eq!(out, text, "{bits:#018x}");
        }
        // Read from JSON text: a fraction, an exponent, minus zero, an
        // integer past 2^53.
        let value: Value =
            serde_json::from_str("[0.5,-1.25,1E3,1.5e-7,-0.0,-0,100,12e20,9007199254740993]")
                .unwrap();
        assert_eq!(
            String::from_utf8(canonical_bytes(&value)).unwrap(),
            "[0.5,-1.25,1000,1.5e-7,0,0,100,1.2e+21,9007199254740992]"
        );
    }

    #[test]
    fn a_record_with_fractions_has_known_bytes_and_hash() {
        let mut record = chain_of(1).remove(0);
        record.change = Some(Change {
            after: Some(json!({"ratio": 0.5, "tiny": 1.5e-7, "big": 1e21, "zero": -0.0})),
            ..Change::default()
        });
        record.hash = record_hash(&record);
        let bytes = String::from_utf8(hashed_bytes(&record)).unwrap();
        let change = r#""change":{"after":{"big":1e+21,"ratio":0.5,"tiny":1.5e-7,"zero":0}},"#;
        assert!(bytes.contains(change), "{bytes}");
        assert_eq!(
            bytes,
            KNOWN_BYTES
                .replace("{genesis}", GENESIS_V1)
                .replace(r#""clock_source""#, &format!("{change}\"clock_source\""),)
        );
        assert_eq!(record.hash, KNOWN_FRACTION_HASH);
        // It reads back from its JSON with the same hash.
        let read: AuditRecord =
            serde_json::from_str(&serde_json::to_string_pretty(&record).unwrap()).unwrap();
        assert_eq!(record_hash(&read), record.hash);
    }

    #[test]
    fn the_same_record_gives_the_same_bytes_and_hash_whatever_its_order_and_spaces() {
        let record = chain_of(1).remove(0);
        let compact = serde_json::to_string(&record).unwrap();
        let pretty = serde_json::to_string_pretty(&record).unwrap();
        // The members of the object, reversed, as another writer might.
        let reversed = {
            let Value::Object(members) = serde_json::to_value(&record).unwrap() else {
                unreachable!()
            };
            let parts: Vec<String> = members
                .iter()
                .rev()
                .map(|(name, value)| format!("{} : {}", json!(name), value))
                .collect();
            format!("{{ {} }}", parts.join(" ,\n "))
        };
        let hashes: Vec<String> = [compact, pretty, reversed]
            .iter()
            .map(|text| record_hash(&serde_json::from_str(text).unwrap()))
            .collect();
        assert_eq!(hashes[0], record.hash);
        assert!(hashes.iter().all(|hash| *hash == record.hash));
    }

    #[test]
    fn a_known_record_has_known_bytes_and_hash_and_any_field_changes_the_hash() {
        let record = chain_of(1).remove(0);
        let bytes = String::from_utf8(hashed_bytes(&record)).unwrap();
        assert_eq!(bytes, KNOWN_BYTES.replace("{genesis}", GENESIS_V1));
        assert_eq!(genesis(), GENESIS_V1);
        assert_eq!(record.hash, KNOWN_HASH);

        let changes: Vec<fn(&mut AuditRecord)> = vec![
            |r| r.seq += 1,
            |r| r.sender_seq += 1,
            |r| r.at.push('x'),
            |r| r.principal.id.push('x'),
            |r| r.resource.id.push('x'),
            |r| r.action = AuditAction::AnswerApplied,
            |r| r.prev_hash = "0".repeat(64),
            |r| r.correlation.run_id = Some("r".to_owned()),
            |r| {
                r.change = Some(Change {
                    digest: Some("d".to_owned()),
                    ..Change::default()
                })
            },
        ];
        for change in changes {
            let mut changed = record.clone();
            change(&mut changed);
            assert_ne!(record_hash(&changed), record.hash);
        }
    }

    /// The SHA-256 of `dagq-audit-chain-v1`, by `shasum -a 256`.
    const GENESIS_V1: &str = "157127b90b22d3b96f57ae88f4d7503066682b5ad7ae4763cc81bff8e2032551";
    /// The normalized bytes of the first record of [`chain_of`] but its
    /// hash, written by hand.
    const KNOWN_BYTES: &str = concat!(
        r#"{"action":"ask.answered","action_version":1,"#,
        r#""at":"2026-11-13T00:00:00Z","attestation":{"level":"enforced"},"#,
        r#""auth":"socket_peer","authority":{"capability":"landing.land"},"#,
        r#""chain_version":"v1","clock_source":"host","correlation":{"task_id":4841},"#,
        r#""outcome":{"status":"succeeded"},"prev_hash":"{genesis}","#,
        r#""principal":{"id":"user","role":"user"},"#,
        r#""received_at":"2026-11-13T00:00:01Z","resource":{"id":"4841","kind":"task"},"#,
        r#""schema_version":1,"sender_id":"queue_service:100","sender_seq":1,"seq":1}"#
    );
    /// The SHA-256 of [`KNOWN_BYTES`] with the change of
    /// `a_record_with_fractions_has_known_bytes_and_hash`, by `shasum -a 256`.
    const KNOWN_FRACTION_HASH: &str =
        "15a04662a6786a4a0fb3576f5718f28b4d2be4488350757776e63112686d7c04";
    /// The SHA-256 of [`KNOWN_BYTES`], by `shasum -a 256`.
    const KNOWN_HASH: &str = "958d8e8f0267b5433652192361e84a6faf9d3ddf7ba7379b01f7681fc719987d";

    #[test]
    fn a_whole_chain_verifies_and_gives_where_the_next_check_starts() {
        let records = chain_of(6);
        let next = verify(ChainAnchor::genesis(), &records).unwrap();
        assert_eq!(next.next_seq, 7);
        assert_eq!(next.prev_hash, records[5].hash);
        assert_eq!(next.senders["queue_service:100"], 3);
        // From that checkpoint, the rest verifies alone.
        let (head, tail) = records.split_at(4);
        let middle = verify(ChainAnchor::genesis(), head).unwrap();
        assert_eq!(verify(middle, tail).unwrap(), next);
        assert_eq!(
            verify(ChainAnchor::genesis(), &[]).unwrap(),
            ChainAnchor::genesis()
        );
    }

    #[test]
    fn a_rewritten_field_is_the_first_mismatch_at_its_row() {
        let mut records = chain_of(5);
        records[2].outcome.reason = Some("rewritten".to_owned());
        records[3].outcome.reason = Some("rewritten too".to_owned());
        let mismatch = verify(ChainAnchor::genesis(), &records).unwrap_err();
        assert_eq!((mismatch.seq, mismatch.kind), (3, MismatchKind::Hash));
        // Its hash computed again, the next row no longer follows it.
        rehash_from(&mut records[..3], 2);
        let mismatch = verify(ChainAnchor::genesis(), &records).unwrap_err();
        assert_eq!((mismatch.seq, mismatch.kind), (4, MismatchKind::PrevHash));
    }

    #[test]
    fn a_missing_row_and_two_rows_swapped_are_told_apart() {
        let mut missing = chain_of(5);
        missing.remove(2);
        let mismatch = verify(ChainAnchor::genesis(), &missing).unwrap_err();
        assert_eq!(
            (mismatch.seq, mismatch.index, mismatch.kind),
            (4, 2, MismatchKind::MissingSeq { expected: 3 })
        );
        let mut swapped = chain_of(5);
        swapped.swap(1, 2);
        let mismatch = verify(ChainAnchor::genesis(), &swapped).unwrap_err();
        assert_eq!(
            (mismatch.seq, mismatch.kind),
            (3, MismatchKind::OutOfOrder { expected: 2 })
        );
    }

    #[test]
    fn a_prev_hash_moved_shows_even_when_the_row_is_hashed_again() {
        let mut records = chain_of(4);
        records[2].prev_hash = records[0].hash.clone();
        let mismatch = verify(ChainAnchor::genesis(), &records).unwrap_err();
        assert_eq!((mismatch.seq, mismatch.kind), (3, MismatchKind::PrevHash));
        records[2].hash = record_hash(&records[2]);
        let mismatch = verify(ChainAnchor::genesis(), &records).unwrap_err();
        assert_eq!((mismatch.seq, mismatch.kind), (3, MismatchKind::PrevHash));
    }

    #[test]
    fn a_senders_skipped_or_repeated_counter_is_the_first_mismatch() {
        let sender = "integrate:100".to_owned();
        // Seq 4 is the integrator's second record; it skips to 3.
        let mut gap = chain_of(5);
        gap[3].sender_seq = 3;
        rehash_from(&mut gap, 3);
        let mismatch = verify(ChainAnchor::genesis(), &gap).unwrap_err();
        assert_eq!(
            (mismatch.seq, mismatch.kind),
            (
                4,
                MismatchKind::SenderSeqGap {
                    sender_id: sender.clone(),
                    expected: 2
                }
            )
        );
        let mut duplicate = chain_of(5);
        duplicate[3].sender_seq = 1;
        rehash_from(&mut duplicate, 3);
        let mismatch = verify(ChainAnchor::genesis(), &duplicate).unwrap_err();
        assert_eq!(
            (mismatch.seq, mismatch.kind),
            (
                4,
                MismatchKind::SenderSeqDuplicate {
                    sender_id: sender,
                    last: 1
                }
            )
        );
        // A sender first met after a checkpoint starts where it says.
        let records = chain_of(4);
        let mut anchor = verify(ChainAnchor::genesis(), &records[..2]).unwrap();
        anchor.senders.clear();
        anchor.from_genesis = false;
        assert!(verify(anchor, &records[2..]).is_ok());
        // A sender's first record from the start of the chain is its 1.
        let mut late = chain_of(1);
        late[0].sender_seq = 2;
        rehash_from(&mut late, 0);
        assert!(matches!(
            verify(ChainAnchor::genesis(), &late).unwrap_err().kind,
            MismatchKind::SenderSeqGap { expected: 1, .. }
        ));
    }

    #[test]
    fn a_record_of_another_version_is_not_verified_as_this_one() {
        let mut records = chain_of(2);
        records[1].chain_version = "v2".to_owned();
        let mismatch = verify(ChainAnchor::genesis(), &records).unwrap_err();
        assert_eq!(
            (mismatch.seq, mismatch.kind),
            (2, MismatchKind::UnknownVersion)
        );
    }
}
