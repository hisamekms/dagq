"""Read the run events of [from, to) once, 6 hours at a time (a queue service
answer over 16 MiB is cut), into one JSON array on stdout.
Usage: python3 fetch_events.py FROM TO > events.json"""
import datetime as D, json, subprocess, sys

def z(x):
    return x.strftime('%Y-%m-%dT%H:%M:%S.') + f'{x.microsecond // 1000:03d}Z'

f = D.datetime.fromisoformat(sys.argv[1].replace('Z', '+00:00'))
t = D.datetime.fromisoformat(sys.argv[2].replace('Z', '+00:00'))
out, s = [], f
while s < t:
    u = min(s + D.timedelta(hours=6), t)
    r = json.loads(subprocess.run(
        ['dagq', 'events', '--all', '--full', '--since', z(s), '--until', z(u), '--limit', '200000'],
        capture_output=True, text=True, check=True).stdout)
    out += r['events']
    s = u
json.dump(out, sys.stdout)
