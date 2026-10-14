use super::*;

/// Credential-like values, put together here so that no line of this
/// source is one, each with the kind it is.
fn fakes() -> Vec<(Kind, String)> {
    let dashes = "-".repeat(5);
    let body36 = "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8";
    vec![
        (
            Kind::PrivateKey,
            format!("{dashes}BEGIN OPENSSH PRIVATE KEY{dashes}"),
        ),
        (
            Kind::PrivateKey,
            format!("x = \"{dashes}BEGIN PGP PRIVATE KEY BLOCK{dashes}\""),
        ),
        (
            Kind::AwsAccessKeyId,
            format!("id = {}{}", "AKIA", "Q7ZK2M4N6P8R0T3V"),
        ),
        (Kind::GithubToken, format!("{}_{body36}", "ghp")),
        (
            Kind::GithubToken,
            format!("{}_{}", "github_pat", "11ABCDEFG0abcdefghij_klmnop"),
        ),
        (
            Kind::GitlabToken,
            format!("{}-{}", "glpat", "abcdEFGH1234ijkl-_mn"),
        ),
        (
            Kind::SlackToken,
            format!("{}-{}", "xoxb", "1234567890-abcdef"),
        ),
        (
            Kind::StripeKey,
            format!("{}_{}", "sk_live", "4eC39HqLyjWDarjtT1zdp7dc"),
        ),
        (
            Kind::GoogleApiKey,
            format!("{}{}", "AIza", "SyA1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q"),
        ),
        (
            Kind::AnthropicApiKey,
            format!("{}-{}", "sk-ant", "api03-abcdefghij0123456789"),
        ),
        (
            Kind::OpenaiApiKey,
            format!("{}-{}", "sk-proj", "abcdefghij0123456789XY"),
        ),
        (Kind::NpmToken, format!("{}_{body36}", "npm")),
        (
            Kind::HighEntropyAssignment,
            format!("api_key = \"{}{}\"", "Zq8vR3kP0wL5", "mX2nB7tY4cH9"),
        ),
    ]
}

fn entropic() -> String {
    format!("{}{}", "Zq8vR3kP0wL5", "mX2nB7tY4cH9")
}

/// Each kind is found on a line that has it.
#[test]
fn each_kind_is_found_on_a_line_that_has_it() {
    for (kind, line) in fakes() {
        assert_eq!(line_kind(&line), Some(kind), "{line}");
    }
}

/// What only resembles a credential is none: a header that is no private
/// key's, a token prefix with too short a body, a name that only starts
/// like one (author, tokenizer), and a literal too short, without a digit,
/// of too little entropy or not assigned.
#[test]
fn what_only_resembles_a_credential_is_none() {
    let dashes = "-".repeat(5);
    let value = entropic();
    for line in [
        format!("{dashes}BEGIN PUBLIC KEY{dashes}"),
        format!("{dashes}BEGIN private key{dashes}"),
        format!("{}{}", "AKIA", "SHORT1"),
        format!("{}_{}", "ghp", "short"),
        format!("author = \"{value}\""),
        format!("tokenizer = \"{value}\""),
        "token = \"Zq8vR3kP0wL5\"".to_owned(),
        "password = \"abcdefghijklmnopqrstuvwxyz\"".to_owned(),
        "secret = \"a1a1a1a1a1a1a1a1a1a1a1a1\"".to_owned(),
        format!("let token = read({value});"),
        "token: String".to_owned(),
        String::new(),
    ] {
        assert_eq!(line_kind(&line), None, "{line}");
    }
}

/// Every name on a line is tried: a first one assigned nothing like a
/// credential does not hide a later one, and a suffixed name, a quoted
/// name and each operator count.
#[test]
fn every_name_on_a_line_and_each_form_of_assignment_counts() {
    let value = entropic();
    for line in [
        format!("{{\"token_type\": \"bearer\", \"access_key\": \"{value}\"}}"),
        format!("DB_PASSWORD={value}"),
        format!("auth_token := '{value}'"),
        format!("secret => `{value}`"),
        format!("credential_json:\t{value}"),
    ] {
        assert_eq!(
            line_kind(&line),
            Some(Kind::HighEntropyAssignment),
            "{line}"
        );
    }
}

/// A diff as [`diff_args`] makes git print it, of `files` (`(header,
/// hunks)`), each hunk a header and lines.
fn diff(files: &[(&str, &str)]) -> String {
    files
        .iter()
        .map(|(name, body)| {
            format!("diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ {name}\n{body}")
        })
        .collect()
}

/// Only added lines are read: the same values on removed lines, and on
/// lines already there (which a `--unified=0` diff does not show, or a
/// pure rename shows without a hunk), are not found; an added line is,
/// numbered in the new file across hunks and after removed lines.
#[test]
fn only_added_lines_are_found_and_numbered_in_the_new_file() {
    let fakes = fakes();
    let (aws, key) = (&fakes[2].1, &fakes[0].1);
    let text = format!(
        "diff --git a/kept.txt b/kept.txt\nindex 1..2 100644\n--- a/kept.txt\n+++ b/kept.txt\n\
         @@ -3,2 +2,0 @@\n-{aws}\n-{key}\n\
         @@ -9 +7,3 @@\n-old\n+plain\n+{aws}\n+++ {key}\n\
         \\ No newline at end of file\n\
         diff --git a/gone.txt b/gone.txt\ndeleted file mode 100644\nindex 1..0\n--- a/gone.txt\n+++ /dev/null\n\
         @@ -1,2 +0,0 @@\n-{aws}\n-{key}\n\
         diff --git a/old.txt b/moved.txt\nsimilarity index 100%\nrename from old.txt\nrename to moved.txt\n"
    );
    let found = scan(&text, &[]);
    assert_eq!(
        found,
        [
            Finding {
                file: "kept.txt".into(),
                line: 8,
                kind: Kind::AwsAccessKeyId
            },
            Finding {
                file: "kept.txt".into(),
                line: 9,
                kind: Kind::PrivateKey
            },
        ]
    );
    let removed_only = text
        .replace(&format!("+{aws}\n"), "+plain\n")
        .replace(&format!("+++ {key}\n"), "+plain\n");
    assert_eq!(scan(&removed_only, &[]), []);
}

/// A name git quotes (a tab, a backslash, a quote, a byte in octal) is
/// decoded, so the paths a narrowed `paths` selected match it; a name with
/// a space ends with the tab git adds; `only` keeps those files only.
#[test]
fn quoted_names_are_decoded_and_only_the_given_paths_are_read() {
    let key = &fakes()[0].1;
    let hunk = format!("@@ -0,0 +1 @@\n+{key}\n");
    let text = diff(&[
        ("\"b/src/a\\tb.txt\"", &hunk),
        ("\"b/src/back\\\\slash\\\"q.txt\"", &hunk),
        ("\"b/src/\\303\\274.txt\"", &hunk),
        ("b/src/with space.txt\t", &hunk),
        ("b/docs/out.txt", &hunk),
    ]);
    let names = [
        "src/a\tb.txt",
        "src/back\\slash\"q.txt",
        "src/ü.txt",
        "src/with space.txt",
    ]
    .map(str::to_owned);
    let found: Vec<String> = scan(&text, &names).into_iter().map(|f| f.file).collect();
    assert_eq!(found, names);
    assert_eq!(scan(&text, &[]).len(), 5);
    assert!(scan(&text, &["docs/other.txt".to_owned()]).is_empty());
    assert_eq!(unquote("\"unterminated"), None);
    assert_eq!(unquote("\"bad\\"), None);
}

/// A name that is no UTF-8 (`\377`), or a quoted form that does not
/// decode (an octal cut short, no closing quote), is read whatever paths
/// are given, never skipped: an added key in it is found under its lossy
/// name or the header as written.
#[test]
fn a_name_that_does_not_decode_exactly_is_always_read() {
    let key = &fakes()[0].1;
    let hunk = format!("@@ -0,0 +1 @@\n+{key}\n");
    let text = diff(&[
        ("\"b/leak\\377.env\"", &hunk),
        ("\"b/short\\37\"", &hunk),
        ("\"b/unterminated.env", &hunk),
    ]);
    let narrowed = ["src/other.txt".to_owned()];
    for only in [&[][..], &narrowed[..]] {
        let found: Vec<(String, Kind)> = scan(&text, only)
            .into_iter()
            .map(|f| (f.file, f.kind))
            .collect();
        assert_eq!(
            found,
            [
                ("leak\u{FFFD}.env".to_owned(), Kind::PrivateKey),
                ("\"b/short\\37\"".to_owned(), Kind::PrivateKey),
                ("\"b/unterminated.env".to_owned(), Kind::PrivateKey),
            ],
            "{only:?}"
        );
    }
}

/// The diff git is asked for sets every option configuration could change,
/// and a commit that reads as an option is refused.
#[test]
fn the_diff_sets_its_options_and_refuses_an_option_for_a_commit() {
    let args = diff_args("base", "head").unwrap();
    for option in [
        "--no-ext-diff",
        "--no-textconv",
        "--text",
        "--find-renames",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--no-relative",
        "--unified=0",
    ] {
        assert!(args.iter().any(|a| a == option), "{option}");
    }
    assert!(args.ends_with(&["--end-of-options", "base", "head", "--"].map(str::to_owned)));
    for bad in ["-p", "--output=/tmp/x", ""] {
        assert!(diff_args(bad, "head").is_err(), "{bad}");
        assert!(diff_args("base", bad).is_err(), "{bad}");
    }
}

/// The output names the file, the line and the kind, never the value, and
/// the exit says whether anything was found.
#[test]
fn the_output_names_file_line_and_kind_and_never_the_value() {
    let (kind, value) = fakes().swap_remove(3);
    let found = scan(
        &diff(&[("b/a.env", &format!("@@ -0,0 +1,2 @@\n+ok\n+{value}\n"))]),
        &[],
    );
    let (text, code) = outcome(&found);
    assert_eq!(code, 1);
    assert!(
        text.starts_with(&format!("a.env:2: {}\n", kind.as_str())),
        "{text}"
    );
    assert!(!text.contains(&value), "{text}");
    assert_eq!(outcome(&[]), (String::new(), 0));
}
