//! Whether the inbox `up` recorded was opened with the guardrail that
//! refuses raw `cmux` (ADR-t1228-2 decision 4): the one judgment `status`
//! and `doctor` show as `inbox_guardrail`. `up` writes `inbox_opened` when
//! it opens the inbox's workspace, with the workspace and whether the
//! settings went with it. An inbox `up` reuses keeps whatever it was
//! opened with, and one opened before the record existed has none, so it
//! counts as without the guardrail. A `claude` typed again in the
//! workspace is not seen (the guardrail's limit).

use serde_json::{Value, json};

/// What a person does to get an inbox with the guardrail: the procedure is
/// in the `dagq-recover` skill's `reference/up-down.md`.
pub const REOPEN: &str = "write the inbox's handoff, then close the inbox (a person, in a terminal without DAGQ_ROLE) and open it again with `dagq up`; see the dagq-recover skill's reference/up-down.md";

/// The guardrail of the inbox recorded as `recorded` (its workspace), by
/// the payload of the newest `inbox_opened`: `guardrail` is `null` when no
/// inbox is recorded, `true` when the newest open of that workspace had the
/// guardrail, and `false` otherwise, with why (`reason`) and `next`.
pub fn judge(recorded: Option<&str>, opened: Option<&Value>) -> Value {
    let Some(workspace) = recorded else {
        return json!({"workspace_id": null, "guardrail": null});
    };
    let same = opened.filter(|payload| payload["workspace_id"].as_str() == Some(workspace));
    match same {
        Some(payload) if payload["guardrail"] == true => json!({
            "workspace_id": workspace,
            "guardrail": true,
            "settings": payload["settings"],
        }),
        Some(_) => json!({
            "workspace_id": workspace,
            "guardrail": false,
            "reason": "opened_without_guardrail",
            "next": REOPEN,
        }),
        None => json!({
            "workspace_id": workspace,
            "guardrail": false,
            "reason": "no_record",
            "next": REOPEN,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inbox_opened_with_the_settings_has_the_guardrail() {
        let opened = json!({"workspace_id": "w1", "guardrail": true, "settings": "/q/claude-inbox-settings.json"});
        let view = judge(Some("w1"), Some(&opened));
        assert_eq!(view["guardrail"], true);
        assert_eq!(view["settings"], "/q/claude-inbox-settings.json");
        assert!(view.get("next").is_none());
    }

    #[test]
    fn an_inbox_without_a_record_of_its_open_has_none_and_says_to_reopen() {
        // Opened before the record existed, or the record is of another
        // workspace (an earlier inbox).
        for opened in [None, Some(json!({"workspace_id": "w0", "guardrail": true}))] {
            let view = judge(Some("w1"), opened.as_ref());
            assert_eq!(view["guardrail"], false);
            assert_eq!(view["reason"], "no_record");
            assert_eq!(view["next"], REOPEN);
        }
        assert!(REOPEN.contains("dagq up") && REOPEN.contains("up-down.md"));
    }

    #[test]
    fn an_inbox_opened_without_the_settings_has_none() {
        let opened = json!({"workspace_id": "w1", "guardrail": false, "settings": null});
        let view = judge(Some("w1"), Some(&opened));
        assert_eq!(view["guardrail"], false);
        assert_eq!(view["reason"], "opened_without_guardrail");
        assert_eq!(view["next"], REOPEN);
    }

    #[test]
    fn no_recorded_inbox_has_nothing_to_judge() {
        let view = judge(None, None);
        assert_eq!(view["guardrail"], Value::Null);
        assert!(view.get("next").is_none());
    }
}
