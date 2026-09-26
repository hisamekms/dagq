//! Whether a repository is dagq's source (ADR-t614-1): its root
//! `Cargo.toml` has a `[package]` table whose `name` is `"dagq"`. The
//! features only dagq's own development needs (integrate's migration
//! renumbering, `install` without `--from`, `up --auto-update`) run only
//! there. Judged from the file as it is every time; never stored, and
//! nothing overrides it.

/// The package name the check looks for.
pub const PACKAGE: &str = "dagq";

/// Whether `manifest`, the text of a repository root's `Cargo.toml`
/// (`None` when it is missing or unreadable), makes the repository dagq's
/// source. Anything that is not a plain `name = "dagq"` in the
/// `[package]` table is not: no file, no `[package]` (a workspace only),
/// another name, or a value this reading does not understand.
pub fn is_source(manifest: Option<&str>) -> bool {
    let Some(text) = manifest else {
        return false;
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut in_package = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_package = without_comment(line) == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "name" {
            continue;
        }
        return string_value(without_comment(value.trim())) == Some(PACKAGE);
    }
    false
}

/// `line` without a trailing `#` comment, trimmed. Package names and table
/// headers have no `#`, so a quoted `#` needs no care here.
fn without_comment(line: &str) -> &str {
    line.split_once('#').map_or(line, |(text, _)| text).trim()
}

/// The text of a basic or literal TOML string without escapes.
fn string_value(value: &str) -> Option<&str> {
    ['"', '\''].into_iter().find_map(|quote| {
        value
            .strip_prefix(quote)?
            .strip_suffix(quote)
            .filter(|text| !text.contains(quote) && !text.contains('\\'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_package_named_dagq_is_the_source() {
        assert!(is_source(Some(
            "[package]\nname = \"dagq\"\nversion = \"0.2.0\"\n"
        )));
        assert!(is_source(Some(
            "\u{feff}# dagq\n[package] # the crate\nedition = \"2024\"\nname='dagq' # here\n\n[dependencies]\nname = \"other\"\n"
        )));
    }

    #[test]
    fn anything_else_is_not_the_source() {
        for manifest in [
            None,
            Some(""),
            Some("[package]\nname = \"myapp\"\n"),
            Some("[package]\nname = \"dagq-fork\"\n"),
            Some("[workspace]\nmembers = [\"dagq\"]\n"),
            Some("[dependencies]\nname = \"dagq\"\n"),
            Some("[package]\nversion = \"1\"\n[lib]\nname = \"dagq\"\n"),
            Some("[package]\nname = dagq\n"),
            Some("[package]\nname = \"da\\\"gq\"\n"),
            Some("[package.metadata]\nname = \"dagq\"\n"),
            Some("not toml at all"),
        ] {
            assert!(!is_source(manifest), "{manifest:?}");
        }
    }
}
