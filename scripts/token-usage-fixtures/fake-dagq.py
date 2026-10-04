#!/usr/bin/env python3
"""A fake dagq for scripts/token-usage-test.py: `locate` and paged `events`.

Reads the events from $FAKE_DAGQ_EVENTS, the queue dir from $FAKE_DAGQ_QUEUE
and appends each call's arguments to $FAKE_DAGQ_LOG.
"""

import argparse
import datetime as dt
import json
import os
import sys


def utc(text):
    return dt.datetime.fromisoformat(text.replace("Z", "+00:00"))


def main():
    with open(os.environ["FAKE_DAGQ_LOG"], "a") as log:
        log.write(json.dumps(sys.argv[1:]) + "\n")
    if sys.argv[1:2] == ["locate"]:
        socket = os.path.join(os.environ["FAKE_DAGQ_QUEUE"], "service", "queue.sock")
        print(json.dumps({"client_mode": True, "db": None, "socket": socket}))
        return
    ap = argparse.ArgumentParser()
    ap.add_argument("command", choices=["events"])
    ap.add_argument("--kind", action="append", required=True)
    ap.add_argument("--since", required=True)
    ap.add_argument("--until", required=True)
    ap.add_argument("--limit", type=int, default=100)
    ap.add_argument("--after", type=int, default=0)
    a = ap.parse_args()
    if a.kind != ["run_integrated"]:
        sys.exit(f"unexpected --kind {a.kind}")
    with open(os.environ["FAKE_DAGQ_EVENTS"]) as f:
        events = json.load(f)
    page = [
        dict(e, kind="run_integrated")
        for e in events
        if e["id"] > a.after and utc(a.since) <= utc(e["created_at"]) < utc(a.until)
    ][: a.limit]
    print(json.dumps({"cursor": page[-1]["id"] if page else a.after, "events": page}))


if __name__ == "__main__":
    main()
