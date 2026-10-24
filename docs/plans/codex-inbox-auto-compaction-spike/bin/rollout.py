# usage: python3 -I rollout.py <rollout.jsonl> ; print record kinds that mark the session id, model and compaction (no message bodies)
import json, sys
for l in open(sys.argv[1]):
    o = json.loads(l); t = o.get("type"); p = o.get("payload") or {}
    pt = p.get("type") if isinstance(p, dict) else None
    ts = o.get("timestamp")
    if t == "session_meta":
        print(ts, t, "id=%s originator=%s cli_version=%s source=%s model_provider=%s" % (p.get("id"), p.get("originator"), p.get("cli_version"), p.get("source"), p.get("model_provider")))
    elif t == "turn_context":
        print(ts, t, "model=%s effort=%s turn_id=%s" % (p.get("model"), p.get("effort"), p.get("turn_id")))
    elif t == "compacted":
        u = p.get("latest_token_usage_record") or {}
        print(ts, t, "keys=%s trigger=%s replacement_history=%s usage.session_id=%s usage.thread_id=%s usage.turn_id=%s" % (sorted(p.keys()), p.get("trigger"), len(p.get("replacement_history") or []), u.get("session_id"), u.get("thread_id"), u.get("turn_id")))
    elif pt and ("compact" in pt or pt in ("task_started", "task_complete")):
        print(ts, t, pt, json.dumps({k: v for k, v in p.items() if k not in ("message", "last_agent_message", "summary")})[:300])
    elif pt == "token_count":
        i = p.get("info") or {}
        print(ts, t, pt, "total=%s ctx_window=%s" % ((i.get("total_token_usage") or {}).get("total_tokens"), i.get("model_context_window")))
