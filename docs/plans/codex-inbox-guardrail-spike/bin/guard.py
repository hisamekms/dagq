# PreToolUse hook: log stdin JSON; deny when the command runs a dagq command the
# inbox is denied (permission_deny(ActorRole::Inbox)) or sets/exports/env/unsets an identity variable.
import sys, json, re, os, time
raw = sys.stdin.read()
log = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "hooks.log")
try:
    ev = json.loads(raw)
except Exception:
    ev = {}
ti = ev.get("tool_input", {})
cmd = ti.get("command", "") if isinstance(ti, dict) else ""
if isinstance(cmd, list):
    cmd = " ".join(map(str, cmd))
DENIED = r"(request\s+decline|replan\s+propose)"
IDENT = r"(DAGQ_ROLE|DAGQ_ACTOR_ID|DAGQ_RUN_ID|DAGQ_TASK_ID)"
B = r"(^|[\s;&|()'\"`])"
rules = [
    B + r"dagq\s+" + DENIED + r"\b",
    B + IDENT + r"=",
    B + r"export\s+" + IDENT,
    B + r"env\s+(-u\s+)?" + IDENT,
    B + r"unset\s+" + IDENT,
]
hit = next((r for r in rules if re.search(r, cmd)), None)
decision = "deny" if hit else "allow"
with open(log, "a") as f:
    f.write(json.dumps({"t": time.strftime("%H:%M:%S"), "decision": decision,
                        "tool_name": ev.get("tool_name"), "tool_input": ti, "input": ev}, ensure_ascii=False) + "\n")
if hit:
    print(json.dumps({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny",
                      "permissionDecisionReason": "dagq inbox guardrail: this command is denied to the inbox role"}}))
