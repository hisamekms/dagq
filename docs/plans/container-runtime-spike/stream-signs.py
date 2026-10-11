"""Spike (a): print, from a `claude -p --output-format stream-json --verbose`
output file, the signs the runtime reads a login that ran out or a usage
limit from (docs/plans/container-runtime-spike.md lists the code that reads
them). It only extracts; it does not classify, so a person compares the
lines with that code. Standard library only; run it with `python3 -I`.

    python3 -I stream-signs.py STREAM_JSON_FILE
"""

import json
import sys


def first_line(text):
    return (text or "").strip().splitlines()[0][:300] if (text or "").strip() else ""


def main():
    path = sys.argv[1]
    counts = {"lines": 0, "not_json": 0, "assistant": 0}
    with open(path, encoding="utf-8", errors="replace") as f:
        for line in f:
            counts["lines"] += 1
            try:
                event = json.loads(line)
            except ValueError:
                counts["not_json"] += 1
                continue
            kind, sub = event.get("type"), event.get("subtype")
            if kind == "system" and sub == "init":
                print("init", json.dumps({"model": event.get("model"), "apiKeySource": event.get("apiKeySource")}))
            elif kind == "system" and sub == "api_retry":
                print("api_retry", json.dumps({k: event.get(k) for k in ("attempt", "error_status", "error")}))
            elif kind == "rate_limit_event":
                print("rate_limit_event", json.dumps(event.get("rate_limit_info"), ensure_ascii=False))
            elif kind == "assistant":
                counts["assistant"] += 1
                if event.get("error"):
                    texts = [c.get("text") for c in event.get("message", {}).get("content", []) if c.get("text")]
                    print("assistant_error", json.dumps({"error": event["error"], "text": first_line(texts[0] if texts else "")}, ensure_ascii=False))
            elif kind == "result":
                print("result", json.dumps({
                    "subtype": event.get("subtype"),
                    "is_error": event.get("is_error"),
                    "api_error_status": event.get("api_error_status"),
                    "num_turns": event.get("num_turns"),
                    "duration_ms": event.get("duration_ms"),
                    "result": first_line(event.get("result")),
                }, ensure_ascii=False))
    print("counts", json.dumps(counts))


if __name__ == "__main__":
    main()
