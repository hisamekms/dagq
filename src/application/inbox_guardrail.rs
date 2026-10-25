//! Whether the inbox was opened with its guardrail (ADR-t2159-1 decisions
//! 2 and 5): the one judgment `status` and `doctor` show as
//! `inbox_guardrail`, with how the newest inbox was launched (ADR-t1181-1
//! decision 5). `dagq inbox` writes `inbox_opened` before it starts the
//! inbox's agent, with whether the guardrail went with it: Claude's
//! settings, or Codex's PreToolUse hook, whose having run is recorded
//! apart (`inbox_guardrail_reached`, once a session). The newest open is
//! the inbox judged. An inbox opened by hand, or an agent typed again in
//! the inbox's terminal, is not seen (the guardrail's limit).

use serde_json::{Value, json};

use crate::domain::RunEvent;

/// What a person does to get an inbox with the guardrail: the procedure is
/// in the `dagq-recover` skill's `reference/up-down.md`.
pub const REOPEN: &str = "write the inbox's handoff, then end the inbox and open it again with `dagq inbox` in a terminal without DAGQ_ROLE; see the dagq-recover skill's reference/up-down.md";

pub use crate::domain::sessions::GUARDRAIL_HOOK;

/// The guardrail of the inbox by `opened`, the payload of the newest
/// `inbox_opened`, and `reached`, whether its guardrail's hook was
/// recorded running since (an `inbox_guardrail_reached` after it):
/// `guardrail` is `true` when it went with the launch, with
/// `guardrail_by` (`settings` with their path, or the hook with
/// `reached`: `false` until the hook is seen to run, which a hook Codex
/// did not trust never does); `false` with why (`reason`) and `next` when
/// none went; `null` with `reason: no_record` when no inbox was ever
/// recorded opened. Each but the last names the launch's `provider`,
/// `source` and requested `model` (null for the provider's default, and
/// for an open recorded before they were).
pub fn judge(opened: Option<&Value>, reached: bool) -> Value {
    let Some(payload) = opened else {
        return json!({"guardrail": null, "reason": "no_record"});
    };
    let mut view = json!({
        "guardrail": payload["guardrail"] == true,
        "provider": payload["provider"],
        "source": payload["source"],
        "model": payload["model"],
    });
    if payload["guardrail"] != true {
        view["reason"] = json!("opened_without_guardrail");
        view["next"] = json!(REOPEN);
        return view;
    }
    if payload["guardrail_by"] == GUARDRAIL_HOOK {
        view["guardrail_by"] = json!(GUARDRAIL_HOOK);
        view["reached"] = json!(reached);
    } else {
        view["guardrail_by"] = json!("settings");
        view["settings"] = payload["settings"].clone();
    }
    view
}

/// [`judge`] of the newest `inbox_opened` (`opened`) and the newest
/// `inbox_guardrail_reached` (`reached`): the guardrail's hook counts as
/// reached when that record came after the open. It is of any session, so
/// an earlier inbox still running whose first tool call comes after the
/// newest launch counts as that launch's.
pub fn judge_latest(opened: Option<&RunEvent>, reached: Option<&RunEvent>) -> Value {
    let since = match (opened, reached) {
        (Some(opened), Some(reached)) => reached.id > opened.id,
        _ => false,
    };
    judge(opened.map(|event| &event.payload), since)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: i64, kind: &str, payload: Value) -> RunEvent {
        RunEvent {
            id: crate::domain::EventId::new(id),
            task_id: None,
            goal_id: None,
            run_id: None,
            kind: kind.into(),
            payload,
            created_at: "1970-01-01T00:00:00.000Z".into(),
            actor: None,
        }
    }

    /// The hook is reached only by a record after the newest open: none,
    /// one before it (an earlier launch's) and one after it.
    #[test]
    fn the_guardrail_hook_is_reached_by_a_record_after_the_newest_open() {
        let opened = event(
            5,
            "inbox_opened",
            json!({"guardrail": true, "guardrail_by": "pre_tool_use", "provider": "codex"}),
        );
        let reached = |id| event(id, "inbox_guardrail_reached", json!({"session_id": "s"}));
        assert_eq!(judge_latest(Some(&opened), None)["reached"], false);
        assert_eq!(
            judge_latest(Some(&opened), Some(&reached(3)))["reached"],
            false
        );
        assert_eq!(
            judge_latest(Some(&opened), Some(&reached(7)))["reached"],
            true
        );
        assert_eq!(judge_latest(None, Some(&reached(7)))["reason"], "no_record");
    }

    #[test]
    fn an_inbox_opened_with_the_settings_has_the_guardrail() {
        let opened = json!({"guardrail": true, "settings": "/q/claude-inbox-settings.json", "provider": "claude"});
        let view = judge(Some(&opened), false);
        assert_eq!(view["guardrail"], true);
        assert_eq!(view["settings"], "/q/claude-inbox-settings.json");
        assert_eq!(view["provider"], "claude");
        assert_eq!(view["guardrail_by"], "settings");
        assert!(view.get("next").is_none());
    }

    /// A Codex inbox's guardrail is its hook: given with the launch, and
    /// reached once its run was recorded; the launch's provider, source
    /// and model are shown.
    #[test]
    fn a_codex_inbox_has_its_guardrail_hook_given_and_reached_apart() {
        let opened = json!({"guardrail": true, "guardrail_by": "pre_tool_use", "settings": null,
                            "provider": "codex", "source": "dagq.toml", "model": "gpt-6.1-sol"});
        let given = judge(Some(&opened), false);
        assert_eq!(given["guardrail"], true);
        assert_eq!(given["guardrail_by"], GUARDRAIL_HOOK);
        assert_eq!(given["reached"], false);
        assert_eq!(given["provider"], "codex");
        assert_eq!(given["source"], "dagq.toml");
        assert_eq!(given["model"], "gpt-6.1-sol");
        assert!(given.get("settings").is_none() && given.get("next").is_none());
        assert_eq!(judge(Some(&opened), true)["reached"], true);
    }

    #[test]
    fn an_inbox_opened_without_the_settings_has_none_and_says_to_reopen() {
        let opened = json!({"guardrail": false, "settings": null, "provider": "codex"});
        let view = judge(Some(&opened), false);
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
        assert_eq!(judge(Some(&opened), false)["guardrail"], true);
    }

    #[test]
    fn no_record_of_an_open_has_nothing_to_judge() {
        let view = judge(None, false);
        assert_eq!(view["guardrail"], Value::Null);
        assert_eq!(view["reason"], "no_record");
        assert!(view.get("next").is_none());
    }
}
