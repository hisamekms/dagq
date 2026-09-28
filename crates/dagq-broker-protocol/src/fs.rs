//! The fs operations: `fs.read`, `fs.list` (capability `fs.read`),
//! `fs.write` and `fs.edit` (capability `fs.write`). Every `path` is relative
//! to the token's workspace, or absolute inside it.

use serde::{Deserialize, Serialize};

/// `POST /v1/fs/read`: lines of a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    pub path: String,
    /// The first line to read, from 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// How many lines to read at most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadResponse {
    pub content: String,
    /// The number of lines in `content`.
    pub lines: u64,
    /// Whether the file goes on after `content`.
    pub truncated: bool,
}

/// `POST /v1/fs/list`: the entries of a directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRequest {
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListResponse {
    /// In the order of their names.
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    /// Bytes, for a file; 0 for anything else.
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// `POST /v1/fs/write`: a file's whole content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteRequest {
    pub path: String,
    pub content: String,
    /// Create the missing parent directories.
    #[serde(default)]
    pub create_dirs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteResponse {
    pub bytes: u64,
}

/// `POST /v1/fs/edit`: replace `old_string` with `new_string`, like the
/// built-in Edit: exactly one match, or at least one with `replace_all`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditRequest {
    pub path: String,
    pub old_string: String,
    pub new_string: String,
    #[serde(default)]
    pub replace_all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditResponse {
    pub replacements: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode};

    fn text<T: Serialize>(value: &T) -> String {
        String::from_utf8(encode(value).unwrap()).unwrap()
    }

    #[test]
    fn requests_serialize_in_field_order_and_leave_out_what_is_absent() {
        let read = ReadRequest {
            path: "src/a.rs".to_owned(),
            offset: None,
            limit: Some(10),
        };
        assert_eq!(text(&read), r#"{"path":"src/a.rs","limit":10}"#);
        assert_eq!(decode::<ReadRequest>(text(&read).as_bytes()).unwrap(), read);
        let write = WriteRequest {
            path: "a".to_owned(),
            content: "x\n".to_owned(),
            create_dirs: false,
        };
        assert_eq!(
            text(&write),
            r#"{"path":"a","content":"x\n","create_dirs":false}"#
        );
        assert_eq!(
            decode::<WriteRequest>(br#"{"path":"a","content":"x\n"}"#).unwrap(),
            write
        );
        let edit = EditRequest {
            path: "a".to_owned(),
            old_string: "o".to_owned(),
            new_string: "n".to_owned(),
            replace_all: true,
        };
        assert_eq!(
            text(&edit),
            r#"{"path":"a","old_string":"o","new_string":"n","replace_all":true}"#
        );
        assert_eq!(decode::<EditRequest>(text(&edit).as_bytes()).unwrap(), edit);
        let list = ListRequest {
            path: ".".to_owned(),
        };
        assert_eq!(text(&list), r#"{"path":"."}"#);
    }

    #[test]
    fn responses_serialize_in_field_order() {
        assert_eq!(
            text(&ReadResponse {
                content: "a\n".to_owned(),
                lines: 1,
                truncated: true
            }),
            r#"{"content":"a\n","lines":1,"truncated":true}"#
        );
        let list = ListResponse {
            entries: vec![
                Entry {
                    name: "a".to_owned(),
                    kind: EntryKind::File,
                    size: 3,
                },
                Entry {
                    name: "b".to_owned(),
                    kind: EntryKind::Dir,
                    size: 0,
                },
                Entry {
                    name: "c".to_owned(),
                    kind: EntryKind::Symlink,
                    size: 0,
                },
                Entry {
                    name: "d".to_owned(),
                    kind: EntryKind::Other,
                    size: 0,
                },
            ],
        };
        let json = concat!(
            r#"{"entries":[{"name":"a","kind":"file","size":3},{"name":"b","kind":"dir","size":0},"#,
            r#"{"name":"c","kind":"symlink","size":0},{"name":"d","kind":"other","size":0}]}"#
        );
        assert_eq!(text(&list), json);
        assert_eq!(decode::<ListResponse>(json.as_bytes()).unwrap(), list);
        assert_eq!(text(&WriteResponse { bytes: 2 }), r#"{"bytes":2}"#);
        assert_eq!(
            text(&EditResponse { replacements: 1 }),
            r#"{"replacements":1}"#
        );
    }

    #[test]
    fn unknown_fields_and_kinds_are_refused() {
        assert!(decode::<ReadRequest>(br#"{"path":"a","follow_symlinks":true}"#).is_err());
        assert!(decode::<ListRequest>(br#"{"path":"a","recursive":true}"#).is_err());
        assert!(decode::<WriteRequest>(br#"{"path":"a","content":"","mode":493}"#).is_err());
        assert!(
            decode::<EditRequest>(br#"{"path":"a","old_string":"","new_string":"","regex":true}"#)
                .is_err()
        );
        assert!(
            decode::<ListResponse>(br#"{"entries":[{"name":"a","kind":"socket","size":0}]}"#)
                .is_err()
        );
        assert!(decode::<ReadRequest>(br#"{"offset":1}"#).is_err());
    }
}
