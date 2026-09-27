//! The language AI writes in for people (ADR-t616-2): a BCP 47 tag from
//! the repository's `dagq.toml` or the user's `config.toml`, and the one
//! instruction the runtime adds to every session's and job's prompt. The
//! runtime's own fixed strings stay English (ADR-t616-1).
use serde::Serialize;

/// Where the language in force came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LanguageSource {
    /// `[language] tag` of the repository's `dagq.toml`.
    Repository,
    /// `[language] tag` of the user's `config.toml`.
    User,
}

/// The language resolved for a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Language {
    pub tag: String,
    pub source: LanguageSource,
}

impl Language {
    /// The paragraph a prompt carries for this language.
    pub fn instruction(&self) -> String {
        instruction(&self.tag)
    }
}

/// Whether `tag` has the form of a BCP 47 language tag: subtags of 1 to 8
/// letters or digits joined by `-`, the first a language of 2 to 3 or 5 to
/// 8 letters. Whether it is registered is not checked.
pub fn check_tag(tag: &str) -> Result<(), String> {
    if tag.trim().is_empty() {
        return Err("the language tag is empty".to_owned());
    }
    let mut subtags = tag.split('-');
    let language = subtags.next().unwrap_or_default();
    let language_ok = language.chars().all(|c| c.is_ascii_alphabetic())
        && matches!(language.len(), 2..=3 | 5..=8);
    let rest_ok = subtags.all(|subtag| {
        (1..=8).contains(&subtag.len()) && subtag.chars().all(|c| c.is_ascii_alphanumeric())
    });
    if language_ok && rest_ok {
        Ok(())
    } else {
        Err(format!(
            "{tag:?} is not a BCP 47 language tag (for example \"ja\", \"en\" or \"pt-BR\")"
        ))
    }
}

/// The instruction for the language with BCP 47 tag `tag`.
pub fn instruction(tag: &str) -> String {
    format!(
        "Language: write everything you address to people in the language with BCP 47 tag `{tag}` — your replies in this conversation, ask questions and option descriptions, task and goal titles and descriptions, context, notes, findings, verdict reasons, and receipt summaries and follow_ups (the landing commit message is built from the task title and the receipt summary). Keep code, identifiers, CLI flags, ask option values and quoted runtime output as they are."
    )
}

/// `prompt` with the instruction of `language` added as its last
/// paragraph; unchanged without a language (ADR-t616-2 decision 3).
pub fn with_instruction(prompt: String, language: Option<&Language>) -> String {
    match language {
        Some(language) => format!("{}\n\n{}", prompt.trim_end(), language.instruction()),
        None => prompt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_must_have_the_form_of_bcp_47() {
        for tag in [
            "ja",
            "en",
            "pt-BR",
            "zh-Hant-TW",
            "es-419",
            "haw",
            "sgn-ase-x",
        ] {
            assert_eq!(check_tag(tag), Ok(()), "{tag}");
        }
        for tag in [
            "",
            " ",
            "j",
            "Japanese language",
            "日本語",
            "ja_JP",
            "ja-",
            "abcd",
            "ja-abcdefghi",
            "123",
        ] {
            assert!(check_tag(tag).is_err(), "{tag}");
        }
    }

    #[test]
    fn the_instruction_is_added_only_with_a_language() {
        assert_eq!(with_instruction("prompt\n".into(), None), "prompt\n");
        let ja = Language {
            tag: "ja".into(),
            source: LanguageSource::User,
        };
        let text = with_instruction("prompt\n".into(), Some(&ja));
        assert!(text.starts_with("prompt\n\nLanguage: "), "{text}");
        assert!(text.contains("BCP 47 tag `ja`"), "{text}");
        assert!(text.contains("receipt summaries"), "{text}");
        assert!(text.contains("ask option values"), "{text}");
    }
}
