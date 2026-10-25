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
/// recorded running since (an `inbox_guardrail_reached` after it). It is
/// read by the launch's `provider`: a Codex inbox has the guardrail when
/// its PreToolUse hook ([`GUARDRAIL_HOOK`]) is among the `hooks` given,
/// and any other (Claude, and an open recorded before the provider was,
/// which only Claude opened) when its `settings` went with it (an open
/// that recorded only `guardrail` is read by it).
///
/// `guardrail` is `true` when it went with the launch, with
/// `guardrail_by` how: `settings` with their path, or the hook with
/// `reached` apart, `false` until the hook is seen to run. A hook given is
/// not a hook run (Codex trusts each hook on its own and runs no hook it
/// did not trust), so `reached: false` says only that no run of it was
/// recorded, and the launch's record cannot tell whether it will run.
/// `guardrail` is `false` with why (`reason`) and `next` when none went,
/// and `null` with `reason: no_record` when no inbox was ever recorded
/// opened. Each but the last names the launch's `provider`, `source` and
/// requested `model` (null for the provider's default, and for an open
/// recorded before they were).
pub fn judge(opened: Option<&Value>, reached: bool) -> Value {
    let Some(payload) = opened else {
        return json!({"guardrail": null, "reason": "no_record"});
    };
    let by = guardrail_by(payload);
    let mut view = json!({
        "guardrail": by.is_some(),
        "provider": payload["provider"],
        "source": payload["source"],
        "model": payload["model"],
    });
    match by {
        None => {
            view["reason"] = json!("opened_without_guardrail");
            view["next"] = json!(REOPEN);
        }
        Some(GUARDRAIL_HOOK) => {
            view["guardrail_by"] = json!(GUARDRAIL_HOOK);
            view["reached"] = json!(reached);
        }
        Some(_) => {
            view["guardrail_by"] = json!(SETTINGS);
            view["settings"] = payload["settings"].clone();
        }
    }
    view
}

/// How the guardrail went with the launch `payload` records, if it did:
/// see [`judge`].
fn guardrail_by(payload: &Value) -> Option<&'static str> {
    if payload["provider"] == "codex" {
        let hooked = payload["hooks"]
            .as_array()
            .is_some_and(|hooks| hooks.iter().any(|hook| hook["hook"] == GUARDRAIL_HOOK));
        return hooked.then_some(GUARDRAIL_HOOK);
    }
    let settings = if payload["provider"].is_null() {
        !payload["settings"].is_null() || payload["guardrail"] == true
    } else {
        !payload["settings"].is_null()
    };
    settings.then_some(SETTINGS)
}

/// `guardrail_by` of an inbox whose guardrail is Claude's settings.
const SETTINGS: &str = "settings";

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
            json!({"guardrail": true, "guardrail_by": "pre_tool_use", "provider": "codex",
                   "hooks": [{"hook": "pre_tool_use"}]}),
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

    /// A Codex inbox's guardrail is its hook among those given: given with
    /// the launch, and reached once its run was recorded, so no record of
    /// a run leaves it given and not reached; the launch's provider,
    /// source and model are shown.
    #[test]
    fn a_codex_inbox_has_its_guardrail_hook_given_and_reached_apart() {
        let hooks = json!([
            {"hook": "session_start", "key": "/c/config.toml:session_start:0:0", "trusted_hash": "sha256:a"},
            {"hook": "pre_tool_use", "key": "/c/config.toml:pre_tool_use:0:0", "trusted_hash": "sha256:b"},
        ]);
        let opened = json!({"guardrail": true, "guardrail_by": "pre_tool_use", "settings": null,
                            "provider": "codex", "source": "dagq.toml", "model": "gpt-6.1-sol",
                            "hooks": hooks});
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

    /// A Codex inbox is judged by the hooks given, not by the settings or
    /// the recorded `guardrail`: without its PreToolUse hook among them
    /// (none given, other hooks only, or an open recorded before Codex's
    /// hooks were) it has none.
    #[test]
    fn a_codex_inbox_without_its_guardrail_hook_given_has_none() {
        for hooks in [
            Value::Null,
            json!([]),
            json!([{"hook": "session_start", "key": "k", "trusted_hash": "h"}]),
        ] {
            let opened = json!({"guardrail": true, "guardrail_by": "pre_tool_use",
                                "settings": "/q/s.json", "provider": "codex", "hooks": hooks});
            let view = judge(Some(&opened), true);
            assert_eq!(view["guardrail"], false, "{hooks}");
            assert_eq!(view["reason"], "opened_without_guardrail");
            assert_eq!(view["next"], REOPEN);
            assert!(view.get("guardrail_by").is_none() && view.get("reached").is_none());
        }
    }

    /// A Claude inbox is judged by its settings, whatever else it records,
    /// and an open recorded before the provider was by its settings or
    /// its `guardrail`.
    #[test]
    fn a_claude_inbox_and_an_open_without_a_provider_are_judged_by_the_settings() {
        let claude = |settings: Value| {
            judge(
                Some(
                    &json!({"guardrail": true, "settings": settings, "provider": "claude",
                             "hooks": [{"hook": "pre_tool_use"}]}),
                ),
                true,
            )
        };
        assert_eq!(claude(json!("/q/s.json"))["guardrail_by"], "settings");
        assert_eq!(claude(Value::Null)["reason"], "opened_without_guardrail");
        let before = |payload: Value| judge(Some(&payload), false);
        let guarded = before(json!({"guardrail": true}));
        assert_eq!(guarded["guardrail"], true);
        assert_eq!(guarded["guardrail_by"], "settings");
        assert_eq!(guarded["provider"], Value::Null);
        assert_eq!(
            before(json!({"guardrail": false, "settings": null}))["reason"],
            "opened_without_guardrail"
        );
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
