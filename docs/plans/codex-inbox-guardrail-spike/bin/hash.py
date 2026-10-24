# usage: python3 -I hash.py <event_label> <command> <timeout>  -> sha256:... (codex 0.160.0 hook_hash for a command hook without matcher)
import sys, json, hashlib
ev, cmd, t = sys.argv[1], sys.argv[2], int(sys.argv[3])
ident = {"event_name": ev, "hooks": [{"type": "command", "command": cmd, "timeout": t, "async": False}]}
s = json.dumps(ident, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
print("sha256:" + hashlib.sha256(s.encode()).hexdigest())
