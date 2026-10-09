//! Whether the inbox was opened with its guardrail settings (ADR-t2159-1
//! decisions 2 and 5): the one judgment `status` and `doctor` show as
//! `inbox_guardrail`. `dagq inbox` writes `inbox_opened` before it starts
//! the inbox's agent, with whether the settings went with it; the newest
//! one is the inbox judged. An inbox opened by hand, or a `claude` typed
//! again in the inbox's terminal, is not seen (the guardrail's limit).

use serde_json::{Value, json};

/// What a person does to get an inbox with the guardrail: the procedure is
/// in the `dagq-recover` skill's `reference/up-down.md`.
pub const REOPEN: &str = "write the inbox's handoff, then end the inbox and open it again with `dagq inbox` in a terminal without DAGQ_ROLE; see the dagq-recover skill's reference/up-down.md";

/// The guardrail of the inbox by `opened`, the payload of the newest
/// `inbox_opened`: `guardrail` is `true` when it had the settings, `false`
/// with why (`reason`) and `next` when it had none, and `null` with
/// `reason: no_record` when no inbox was ever recorded opened.
pub fn judge(opened: Option<&Value>) -> Value {
    match opened {
        Some(payload) if payload["guardrail"] == true => json!({
            "guardrail": true,
            "settings": payload["settings"],
            "provider": payload["provider"],
        }),
        Some(payload) => json!({
            "guardrail": false,
            "provider": payload["provider"],
            "reason": "opened_without_guardrail",
            "next": REOPEN,
        }),
        None => json!({"guardrail": null, "reason": "no_record"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inbox_opened_with_the_settings_has_the_guardrail() {
        let opened = json!({"guardrail": true, "settings": "/q/claude-inbox-settings.json", "provider": "claude"});
        let view = judge(Some(&opened));
        assert_eq!(view["guardrail"], true);
        assert_eq!(view["settings"], "/q/claude-inbox-settings.json");
        assert_eq!(view["provider"], "claude");
        assert!(view.get("next").is_none());
    }

    #[test]
    fn an_inbox_opened_without_the_settings_has_none_and_says_to_reopen() {
        let opened = json!({"guardrail": false, "settings": null, "provider": "codex"});
        let view = judge(Some(&opened));
        assert_eq!(view["guardrail"], false);
        assert_eq!(view["reason"], "opened_without_guardrail");
        assert_eq!(view["next"], REOPEN);
        assert!(REOPEN.contains("dagq inbox") && REOPEN.contains("without DAGQ_ROLE"));
    }

    #[test]
    fn the_newest_open_is_judged_whatever_workspace_an_earlier_up_recorded() {
        // An inbox an earlier `up` opened recorded its workspace; the
        // record is judged the same.
        let opened = json!({"workspace_id": "w1", "guardrail": true, "settings": "/q/s.json"});
        assert_eq!(judge(Some(&opened))["guardrail"], true);
    }

    #[test]
    fn no_record_of_an_open_has_nothing_to_judge() {
        let view = judge(None);
        assert_eq!(view["guardrail"], Value::Null);
        assert_eq!(view["reason"], "no_record");
        assert!(view.get("next").is_none());
    }
}
