//! `[language]` (ADR-t616-2): the tag of the repository's `dagq.toml`
//! (the main checkout's working file) over the user's
//! `$XDG_CONFIG_HOME/dagq/config.toml`. Resolved on every use and stored
//! nowhere. The other readers of `dagq.toml` accept the table without
//! looking into it ([`super::run_env::parse_config`]), so a mistake here
//! stops only `up`'s preflight and shows in `doctor`.
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{
    env, fs,
    path::{Path, PathBuf},
};

use super::run_env::{CONFIG_FILE_NAME, parse_string, strip_comment};
use crate::domain::language::{Language, LanguageSource, check_tag};

/// The user's configuration file under `$XDG_CONFIG_HOME/dagq/`.
pub const USER_CONFIG_FILE_NAME: &str = "config.toml";
const LANGUAGE_TABLE: &str = "language";
const TAG: &str = "tag";

/// The user's `config.toml`: `$XDG_CONFIG_HOME/dagq/config.toml`, or
/// `~/.config/dagq/config.toml` when `XDG_CONFIG_HOME` is empty or unset.
pub fn user_config_file() -> Option<PathBuf> {
    user_config_file_from(env::var_os("XDG_CONFIG_HOME"), env::var_os("HOME"))
}

fn user_config_file_from(
    xdg_config_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    xdg_config_home
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| home.map(|home| Path::new(&home).join(".config")))
        .map(|dir| dir.join("dagq").join(USER_CONFIG_FILE_NAME))
}

/// `[language] tag` of a file's text. `only_language`: every other table
/// and any key outside a table is an error (the user's `config.toml`);
/// otherwise they are skipped (`dagq.toml`, whose other tables its own
/// reader checks).
fn parse_tag(text: &str, file: &str, only_language: bool) -> Result<Option<String>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut in_language = false;
    let mut seen_table = false;
    let mut tag: Option<String> = None;
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let name = strip_comment(header)
                .strip_suffix(']')
                .with_context(|| format!("{file}:{number}: unclosed table header"))?
                .trim();
            in_language = name == LANGUAGE_TABLE;
            if in_language {
                ensure!(
                    !seen_table,
                    "{file}:{number}: [{LANGUAGE_TABLE}] is defined twice"
                );
                seen_table = true;
            } else if only_language {
                bail!(
                    "{file}:{number}: unknown table [{name}]; only [{LANGUAGE_TABLE}] is supported"
                );
            }
            continue;
        }
        if !in_language {
            ensure!(
                !only_language,
                "{file}:{number}: a key outside [{LANGUAGE_TABLE}]"
            );
            continue;
        }
        let (key, rest) = line
            .split_once('=')
            .with_context(|| format!("{file}:{number}: expected KEY = value"))?;
        let key = key.trim();
        ensure!(
            key == TAG,
            "{file}:{number}: unknown key {key} in [{LANGUAGE_TABLE}]; the key is {TAG}"
        );
        ensure!(tag.is_none(), "{file}:{number}: {key} is defined twice");
        let value = parse_string(rest.trim())
            .with_context(|| format!("{file}:{number}: value of {key}"))?;
        check_tag(&value).map_err(|why| anyhow::anyhow!("{file}:{number}: {why}"))?;
        tag = Some(value);
    }
    Ok(tag)
}

fn read(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn file_tag(path: &Path, only_language: bool) -> Result<Option<String>> {
    let Some(text) = read(path)? else {
        return Ok(None);
    };
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    parse_tag(&text, &name, only_language).with_context(|| format!("parse {}", path.display()))
}

/// The language in force: the `tag` of the `dagq.toml` in `checkout`, else
/// the `tag` of `user_config`, else none (no instruction). An error names
/// the file and the mistake; `up`'s preflight stops on it.
pub fn resolve_language(
    checkout: Option<&Path>,
    user_config: Option<&Path>,
) -> Result<Option<Language>> {
    // Both files are read, so a mistake in either shows even when the
    // other one decides.
    let repository = checkout
        .map(|checkout| file_tag(&checkout.join(CONFIG_FILE_NAME), false))
        .transpose()?
        .flatten();
    let user = user_config
        .map(|path| file_tag(path, true))
        .transpose()?
        .flatten();
    Ok(repository
        .map(|tag| Language {
            tag,
            source: LanguageSource::Repository,
        })
        .or_else(|| {
            user.map(|tag| Language {
                tag,
                source: LanguageSource::User,
            })
        }))
}

/// [`resolve_language`] for building a prompt: a mistake adds no
/// instruction and is a warning, never stopping a run or a planner
/// (ADR-t616-2 decision 6).
pub fn language_for_prompt(
    checkout: Option<&Path>,
    user_config: Option<&Path>,
) -> Option<Language> {
    resolve_language(checkout, user_config).unwrap_or_else(|error| {
        tracing::warn!(error = %format_args!("{error:#}"), "[language] not read: {error:#}; the prompt carries no language instruction");
        None
    })
}

/// The `language` field of `doctor` and `status --role`: the tag in force,
/// where it came from (`repository`, `user` or `unset`), the user's file
/// read, the instruction the prompts carry, and the mistake if any.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LanguageReport {
    pub tag: Option<String>,
    pub source: &'static str,
    pub user_config: Option<PathBuf>,
    pub instruction: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// [`resolve_language`] as a [`LanguageReport`].
pub fn language_report(checkout: Option<&Path>, user_config: Option<&Path>) -> LanguageReport {
    let (language, error) = match resolve_language(checkout, user_config) {
        Ok(language) => (language, None),
        Err(error) => (None, Some(format!("{error:#}"))),
    };
    LanguageReport {
        tag: language.as_ref().map(|language| language.tag.clone()),
        source: match language.as_ref().map(|language| language.source) {
            Some(LanguageSource::Repository) => "repository",
            Some(LanguageSource::User) => "user",
            None => "unset",
        },
        user_config: user_config.map(Path::to_path_buf),
        instruction: language.as_ref().map(Language::instruction),
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_config_is_under_the_xdg_config_home() {
        assert_eq!(
            user_config_file_from(Some("/x".into()), Some("/home/u".into())),
            Some(PathBuf::from("/x/dagq/config.toml"))
        );
        assert_eq!(
            user_config_file_from(Some("".into()), Some("/home/u".into())),
            Some(PathBuf::from("/home/u/.config/dagq/config.toml"))
        );
        assert_eq!(user_config_file_from(None, None), None);
    }

    #[test]
    fn parses_the_language_table() {
        let dagq =
            "[run.env]\nA = 'x'\n[language] # the one\ntag = \"ja\" # Japanese\n[stall]\nx = 1\n";
        assert_eq!(
            parse_tag(dagq, "dagq.toml", false).unwrap(),
            Some("ja".into())
        );
        assert_eq!(
            parse_tag("[run.env]\nA = 'x'\n", "dagq.toml", false).unwrap(),
            None
        );
        assert_eq!(parse_tag("[language]\n", "dagq.toml", false).unwrap(), None);
        for (text, only, error) in [
            (
                "[language]\nname = 'ja'",
                false,
                "unknown key name in [language]",
            ),
            ("[language]\ntag = 1", false, "expected a quoted string"),
            ("[language]\ntag = ''", false, "empty"),
            (
                "[language]\ntag = 'ja_JP'",
                false,
                "not a BCP 47 language tag",
            ),
            ("[language]\ntag = 'ja'\ntag = 'en'", false, "defined twice"),
            (
                "[language]\n[language]",
                false,
                "[language] is defined twice",
            ),
            ("[language]\ntag 'ja'", false, "expected KEY = value"),
            ("[language\ntag = 'ja'", false, "unclosed table header"),
            ("[kpi]\nx = 1", true, "unknown table [kpi]"),
            ("tag = 'ja'", true, "a key outside [language]"),
        ] {
            let message = format!("{:#}", parse_tag(text, "f", only).unwrap_err());
            assert!(message.contains(error), "{text}: {message}");
        }
    }

    #[test]
    fn the_repository_tag_is_over_the_users() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("repo");
        fs::create_dir(&checkout).unwrap();
        let user = dir.path().join("config.toml");
        let resolve = || resolve_language(Some(&checkout), Some(&user)).unwrap();
        // Nothing set: no language, nothing to add.
        assert_eq!(resolve(), None);
        let report = language_report(Some(&checkout), Some(&user));
        assert_eq!((report.tag, report.source), (None, "unset"));
        assert_eq!(report.instruction, None);
        // The user's.
        fs::write(&user, "[language]\ntag = 'en'\n").unwrap();
        assert_eq!(
            resolve(),
            Some(Language {
                tag: "en".into(),
                source: LanguageSource::User
            })
        );
        // A table without a tag falls through to the user's.
        fs::write(checkout.join(CONFIG_FILE_NAME), "[language]\n").unwrap();
        assert_eq!(resolve().unwrap().source, LanguageSource::User);
        // The repository's over the user's.
        fs::write(checkout.join(CONFIG_FILE_NAME), "[language]\ntag = 'ja'\n").unwrap();
        let report = language_report(Some(&checkout), Some(&user));
        assert_eq!(report.tag.as_deref(), Some("ja"));
        assert_eq!(report.source, "repository");
        assert_eq!(report.user_config.as_deref(), Some(user.as_path()));
        assert!(report.instruction.unwrap().contains("`ja`"));
        assert_eq!(
            resolve_language(Some(&checkout), None)
                .unwrap()
                .unwrap()
                .tag,
            "ja"
        );
        // A mistake in either file is an error, and no instruction for a prompt.
        fs::write(&user, "[language]\ntag = 'nope nope'\n").unwrap();
        let error = format!(
            "{:#}",
            resolve_language(Some(&checkout), Some(&user)).unwrap_err()
        );
        assert!(error.contains("config.toml:2"), "{error}");
        assert_eq!(language_for_prompt(Some(&checkout), Some(&user)), None);
        let report = language_report(Some(&checkout), Some(&user));
        assert_eq!(report.source, "unset");
        assert!(report.error.unwrap().contains("BCP 47"));
    }
}
