//! `builtin:secrets` (ADR-t2259-1 decision 1): the lines a change adds that
//! look like a credential. What is read and judged is decided here without
//! side effects: the `git diff` to ask for ([`diff_args`]), the files and
//! line numbers of its added lines ([`scan`]), what each line looks like
//! ([`line_kind`]) and what the program prints and exits with
//! ([`outcome`]). `dagq review-program builtin:secrets` runs `git` and
//! prints (`infrastructure::review_secrets`).
//!
//! Only added lines are read: a removed line, a line already there and a
//! file moved unchanged are not. A rename git does not detect (one changed
//! past half of its lines) reads as a removed file and an added one, whose
//! lines all count as added. A file whose name does not decode exactly is
//! always read. No value is ever printed, only the file, the line and the
//! [`Kind`].

/// What an added line looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The header of a PEM private key (RSA, EC, DSA, OPENSSH, ENCRYPTED,
    /// a PGP private key block): `-----BEGIN`, upper-case words, `PRIVATE
    /// KEY` and an optional ` BLOCK`, then `-----`.
    PrivateKey,
    /// The kinds of [`TOKENS`], each a prefix and a length.
    AwsAccessKeyId,
    GithubToken,
    GitlabToken,
    SlackToken,
    StripeKey,
    GoogleApiKey,
    AnthropicApiKey,
    OpenaiApiKey,
    NpmToken,
    /// A name of [`NAMES`], alone or followed by `_` or `-` and more (so
    /// `author` and `tokenizer` are none), assigned (`:=`, `=>`, `=` or
    /// `:`, the name and the value maybe quoted) a literal of at least
    /// [`MIN_VALUE`] characters of [`value_char`] with a digit whose
    /// Shannon entropy is at least [`MIN_ENTROPY`] bits a character.
    HighEntropyAssignment,
}

impl Kind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PrivateKey => "private_key",
            Self::AwsAccessKeyId => "aws_access_key_id",
            Self::GithubToken => "github_token",
            Self::GitlabToken => "gitlab_token",
            Self::SlackToken => "slack_token",
            Self::StripeKey => "stripe_key",
            Self::GoogleApiKey => "google_api_key",
            Self::AnthropicApiKey => "anthropic_api_key",
            Self::OpenaiApiKey => "openai_api_key",
            Self::NpmToken => "npm_token",
            Self::HighEntropyAssignment => "high_entropy_assignment",
        }
    }
}

/// The characters a token's body is made of.
#[derive(Debug, Clone, Copy)]
enum Body {
    /// `[0-9A-Z]`
    UpperAlnum,
    /// `[A-Za-z0-9]`
    Alnum,
    /// `[A-Za-z0-9_]`
    AlnumUnderscore,
    /// `[A-Za-z0-9_-]`
    AlnumUnderscoreDash,
    /// `[A-Za-z0-9-]`
    AlnumDash,
}

impl Body {
    fn takes(self, byte: u8) -> bool {
        match self {
            Self::UpperAlnum => byte.is_ascii_uppercase() || byte.is_ascii_digit(),
            Self::Alnum => byte.is_ascii_alphanumeric(),
            Self::AlnumUnderscore => byte.is_ascii_alphanumeric() || byte == b'_',
            Self::AlnumUnderscoreDash => {
                byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
            }
            Self::AlnumDash => byte.is_ascii_alphanumeric() || byte == b'-',
        }
    }
}

/// The well-known tokens: a prefix followed by at least so many characters
/// of a body, anywhere on the line, tried in this order.
const TOKENS: &[(Kind, &[&str], Body, usize)] = &[
    (
        Kind::AwsAccessKeyId,
        &["AKIA", "ASIA"],
        Body::UpperAlnum,
        16,
    ),
    (
        Kind::GithubToken,
        &["ghp_", "gho_", "ghu_", "ghs_", "ghr_"],
        Body::Alnum,
        36,
    ),
    (
        Kind::GithubToken,
        &["github_pat_"],
        Body::AlnumUnderscore,
        22,
    ),
    (
        Kind::GitlabToken,
        &["glpat-"],
        Body::AlnumUnderscoreDash,
        20,
    ),
    (
        Kind::SlackToken,
        &["xoxa-", "xoxb-", "xoxp-", "xoxo-", "xoxs-", "xoxr-"],
        Body::AlnumDash,
        10,
    ),
    (Kind::StripeKey, &["sk_live_", "rk_live_"], Body::Alnum, 20),
    (Kind::GoogleApiKey, &["AIza"], Body::AlnumUnderscoreDash, 35),
    (
        Kind::AnthropicApiKey,
        &["sk-ant-"],
        Body::AlnumUnderscoreDash,
        20,
    ),
    (
        Kind::OpenaiApiKey,
        &["sk-proj-", "sk-svcacct-"],
        Body::AlnumUnderscoreDash,
        20,
    ),
    (Kind::NpmToken, &["npm_"], Body::Alnum, 36),
];

/// The names whose assignment [`Kind::HighEntropyAssignment`] reads,
/// lower-case (the line is compared lower-cased).
const NAMES: &[&str] = &[
    "secret",
    "token",
    "password",
    "passwd",
    "pwd",
    "api_key",
    "api-key",
    "apikey",
    "access_key",
    "access-key",
    "accesskey",
    "private_key",
    "private-key",
    "privatekey",
    "credential",
    "auth",
];

/// The shortest literal [`Kind::HighEntropyAssignment`] reads.
const MIN_VALUE: usize = 20;
/// The least entropy, in bits a character, of such a literal.
const MIN_ENTROPY: f64 = 3.5;

/// A character of an assigned literal: `[A-Za-z0-9+/=_.~-]`.
fn value_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"+/=_.~-".contains(&byte)
}

/// What `line`, an added line without its `+`, looks like, the first of
/// [`Kind`]'s checks that holds; `None` for a line like no credential.
pub fn line_kind(line: &str) -> Option<Kind> {
    let bytes = line.as_bytes();
    if private_key_header(bytes) {
        return Some(Kind::PrivateKey);
    }
    for (kind, prefixes, body, min) in TOKENS {
        if prefixes
            .iter()
            .any(|prefix| token(bytes, prefix.as_bytes(), *body, *min))
        {
            return Some(*kind);
        }
    }
    assignment(bytes).then_some(Kind::HighEntropyAssignment)
}

fn private_key_header(line: &[u8]) -> bool {
    const BEGIN: &[u8] = b"-----BEGIN";
    (0..line.len())
        .filter(|at| line[*at..].starts_with(BEGIN))
        .any(|at| {
            let rest = &line[at + BEGIN.len()..];
            let words = rest
                .iter()
                .take_while(|b| **b == b' ' || b.is_ascii_uppercase() || b.is_ascii_digit())
                .count();
            let (words, after) = rest.split_at(words);
            (words.ends_with(b"PRIVATE KEY") || words.ends_with(b"PRIVATE KEY BLOCK"))
                && after.starts_with(b"-----")
        })
}

/// Whether `prefix` followed by at least `min` bytes `body` takes is
/// somewhere on `line`.
fn token(line: &[u8], prefix: &[u8], body: Body, min: usize) -> bool {
    (0..line.len())
        .filter(|at| line[*at..].starts_with(prefix))
        .any(|at| {
            line[at + prefix.len()..]
                .iter()
                .take_while(|b| body.takes(**b))
                .count()
                >= min
        })
}

/// Whether any name of [`NAMES`] on `line` is assigned a literal that looks
/// like a credential: each occurrence is tried.
fn assignment(line: &[u8]) -> bool {
    let lower = line.to_ascii_lowercase();
    (0..lower.len()).any(|at| {
        NAMES.iter().any(|name| {
            lower[at..].starts_with(name.as_bytes())
                && assigned_value(line, at + name.len()).is_some_and(credential_like)
        })
    })
}

/// The literal assigned right after a name ending before `at`: the name's
/// optional `_`/`-` suffix, an optional quote, blanks, the operator,
/// blanks and an optional quote, then the run of [`value_char`]. `None`
/// when no operator follows.
fn assigned_value(line: &[u8], mut at: usize) -> Option<&[u8]> {
    let is_quote = |b: u8| b == b'"' || b == b'\'' || b == b'`';
    let is_blank = |b: u8| b == b' ' || b == b'\t';
    let at_is = |at: usize, test: &dyn Fn(u8) -> bool| line.get(at).copied().is_some_and(test);
    if at_is(at, &|b| b == b'_' || b == b'-') {
        while at_is(at, &|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
            at += 1;
        }
    }
    if at_is(at, &is_quote) {
        at += 1;
    }
    while at_is(at, &is_blank) {
        at += 1;
    }
    let rest = line.get(at..)?;
    let operator = [&b":="[..], b"=>", b"=", b":"]
        .into_iter()
        .find(|op| rest.starts_with(op))?;
    at += operator.len();
    while at_is(at, &is_blank) {
        at += 1;
    }
    if at_is(at, &is_quote) {
        at += 1;
    }
    let rest = line.get(at..)?;
    let length = rest.iter().take_while(|b| value_char(**b)).count();
    Some(&rest[..length])
}

fn credential_like(value: &[u8]) -> bool {
    value.len() >= MIN_VALUE
        && value.iter().any(u8::is_ascii_digit)
        && entropy(value) >= MIN_ENTROPY
}

/// The Shannon entropy of `value` in bits a byte.
#[allow(clippy::cast_precision_loss)]
fn entropy(value: &[u8]) -> f64 {
    let mut counts = [0usize; 256];
    for byte in value {
        counts[usize::from(*byte)] += 1;
    }
    let total = value.len() as f64;
    counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let p = *count as f64 / total;
            -p * p.log2()
        })
        .sum()
}

/// An added line that looks like a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub file: String,
    pub line: u64,
    pub kind: Kind,
}

/// The arguments of the `git` that prints what [`scan`] reads, `base` to
/// `head`: every option a user's or the repository's configuration or
/// attributes could change is given explicitly (no color, no external diff
/// or textconv, every file as text, renames found, `a/` and `b/` prefixes,
/// from the root, every byte of a name outside printable ASCII quoted in
/// octal, so a name reaches [`scan`] as ASCII whatever its bytes), and
/// the commits come after `--end-of-options`. `Err` for a `base` or `head`
/// that could be read as an option.
pub fn diff_args(base: &str, head: &str) -> Result<Vec<String>, String> {
    for commit in [base, head] {
        if commit.is_empty() || commit.starts_with('-') {
            return Err(format!("{commit:?} is no commit"));
        }
    }
    Ok([
        "-c",
        "core.quotePath=true",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--text",
        "--find-renames",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--no-relative",
        "--unified=0",
        "--end-of-options",
        base,
        head,
        "--",
    ]
    .map(str::to_owned)
    .to_vec())
}

/// The added lines of `diff` (what [`diff_args`] prints) that look like a
/// credential, in order; with `only` not empty, those of its files only,
/// and of every file whose name does not decode exactly (no UTF-8, or a
/// quoted form that does not decode), which cannot be told from a path of
/// `only`: such a file is read, never skipped.
pub fn scan(diff: &str, only: &[String]) -> Vec<Finding> {
    let mut found = Vec::new();
    let mut file: Option<String> = None;
    let mut in_hunk = false;
    let mut line = 0u64;
    for text in diff.split('\n') {
        if text.starts_with("diff --git ") {
            in_hunk = false;
            file = None;
            continue;
        }
        if !in_hunk && let Some(name) = text.strip_prefix("+++ ") {
            file = new_name(name)
                .filter(|(name, exact)| only.is_empty() || !exact || only.contains(name))
                .map(|(name, _)| name);
            continue;
        }
        if let Some(header) = text.strip_prefix("@@ ") {
            in_hunk = true;
            line = first_new_line(header);
            continue;
        }
        let Some(name) = file.as_ref().filter(|_| in_hunk) else {
            continue;
        };
        if let Some(added) = text.strip_prefix('+') {
            if let Some(kind) = line_kind(added) {
                found.push(Finding {
                    file: name.clone(),
                    line,
                    kind,
                });
            }
            line += 1;
        } else if text.starts_with(' ') {
            line += 1;
        }
    }
    found
}

/// The file a `+++ ` header names, `None` for `/dev/null`: git's quoted
/// form decoded, without the `b/` prefix and the tab git ends a name that
/// has a space with, and whether it decoded exactly. A name whose bytes are
/// no UTF-8 is its lossy reading, and one whose quoted form does not decode
/// is the header as written: either is not exact.
fn new_name(header: &str) -> Option<(String, bool)> {
    let header = header.strip_suffix('\t').unwrap_or(header);
    if header == "/dev/null" {
        return None;
    }
    let (name, exact) = if header.starts_with('"') {
        match unquote(header).map(String::from_utf8) {
            Some(Ok(name)) => (name, true),
            Some(Err(error)) => (
                String::from_utf8_lossy(error.as_bytes()).into_owned(),
                false,
            ),
            None => (header.to_owned(), false),
        }
    } else {
        (
            header.to_owned(),
            !header.contains(char::REPLACEMENT_CHARACTER),
        )
    };
    Some((name.strip_prefix("b/").unwrap_or(&name).to_owned(), exact))
}

/// The bytes of a name git quoted as a C string (`"b/a\tb\\c"`, `"b/\377"`)
/// with its escapes decoded, `None` when it is not one.
fn unquote(quoted: &str) -> Option<Vec<u8>> {
    let inner = quoted.strip_prefix('"')?.strip_suffix('"')?.as_bytes();
    let mut bytes = Vec::with_capacity(inner.len());
    let mut at = 0;
    while at < inner.len() {
        let byte = inner[at];
        at += 1;
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        let escaped = *inner.get(at)?;
        bytes.push(match escaped {
            b'a' => 0x07,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            b'0'..=b'7' => {
                let digits = std::str::from_utf8(inner.get(at..at + 3)?).ok()?;
                at += 2;
                u8::from_str_radix(digits, 8).ok()?
            }
            other => other,
        });
        at += 1;
    }
    Some(bytes)
}

/// The number of the first new line of a hunk, from what follows its
/// `@@ `: `-a[,b] +c[,d] @@`.
fn first_new_line(header: &str) -> u64 {
    header
        .split(' ')
        .find_map(|part| part.strip_prefix('+'))
        .and_then(|range| range.split(',').next())
        .and_then(|start| start.parse().ok())
        .unwrap_or(0)
}

/// What the program prints on stdout and exits with for `found`: each as
/// `<file>:<line>: <kind>` and a last line saying what to do, exit 1;
/// nothing and exit 0 when nothing was found.
pub fn outcome(found: &[Finding]) -> (String, i32) {
    if found.is_empty() {
        return (String::new(), 0);
    }
    let mut text: String = found
        .iter()
        .map(|finding| {
            format!(
                "{}:{}: {}\n",
                finding.file,
                finding.line,
                finding.kind.as_str()
            )
        })
        .collect();
    text.push_str(&format!(
        "builtin:secrets: {} added line(s) look like a credential; remove the value from the change (keep it outside the repository), or turn builtin:secrets off or narrow its paths in dagq.toml if it is none\n",
        found.len()
    ));
    (text, 1)
}

#[cfg(test)]
mod tests;
