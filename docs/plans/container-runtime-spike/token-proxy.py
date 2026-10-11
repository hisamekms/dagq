"""Spike (a): a small forwarder on 127.0.0.1 that swaps the Authorization of
each request for the long-lived token and sends it to the upstream, streaming
the response back as it comes (docs/plans/container-runtime-spike.md).

Standard library only; run it with `python3 -I`. The token is pasted by a
person at a prompt that does not echo it and is held only in this process's
memory: it is not taken from argv, the environment or a file, and it is never
printed, logged or written anywhere. `--invalid` uses a fixed value that is
not a token instead of asking. The log holds, per request, the method, path,
status, sizes, timings, the rate-limit headers and the first bytes of an
error body, with any occurrence of the token replaced. On Ctrl-C (or SIGTERM) it reads
every file under `--scan DIR` and prints how many hold the token (counts
only), then forgets it.

    python3 -I token-proxy.py --log FILE [--invalid] [--scan DIR] [--port 8787]
        [--scheme bearer|x-api-key] [--add-beta VALUE]
"""

import argparse
import getpass
import http.client
import http.server
import json
import os
import signal
import sys
import threading
import time
import urllib.parse

# Headers a proxy does not forward (RFC 9110 hop-by-hop), plus the ones it
# sets itself.
HOP = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
}
REPLACED = {"host", "authorization", "x-api-key", "content-length", "accept-encoding"}
ERROR_BODY_BYTES = 4096


INVALID = "sk-ant-oat01-spike-invalid"


def read_token():
    # Without a terminal getpass would echo the token.
    if not (sys.stdin.isatty() and os.isatty(2)):
        sys.exit("run it in a terminal; it asks for the token there")
    token = getpass.getpass("paste the token (not echoed), then Enter: ").strip()
    if not token:
        sys.exit("empty; not starting")
    return token


def scan(root, token):
    """How many files under `root` hold the token; names only, never the value."""
    needle = token.encode()
    held = []
    for dirpath, _, names in os.walk(root):
        for name in names:
            path = os.path.join(dirpath, name)
            try:
                with open(path, "rb") as f:
                    if needle in f.read():
                        held.append(path)
            except OSError:
                pass
    return held


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--invalid", action="store_true", help="send a fixed value that is not a token")
    p.add_argument("--scan", default=None, help="dir whose files are checked for the token on exit")
    p.add_argument("--log", required=True)
    p.add_argument("--port", type=int, default=8787)
    p.add_argument("--upstream", default="https://api.anthropic.com")
    p.add_argument("--scheme", choices=["bearer", "x-api-key"], default="bearer")
    p.add_argument("--add-beta", default=None, help="value appended to anthropic-beta")
    args = p.parse_args()

    token = INVALID if args.invalid else read_token()
    upstream = urllib.parse.urlsplit(args.upstream)
    lock = threading.Lock()
    log = open(args.log, "a", encoding="utf-8")

    def redact(text):
        return text.replace(token, "<redacted>")

    def write(entry):
        with lock:
            log.write(redact(json.dumps(entry, ensure_ascii=False)) + "\n")
            log.flush()

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def forward(self):
            started = time.monotonic()
            entry = {
                "at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                "method": self.command,
                "path": urllib.parse.urlsplit(self.path).path,
                "had_authorization": "authorization" in (k.lower() for k in self.headers),
                "had_x_api_key": "x-api-key" in (k.lower() for k in self.headers),
            }
            length = int(self.headers.get("content-length") or 0)
            body = self.rfile.read(length) if length else None
            headers = {k: v for k, v in self.headers.items() if k.lower() not in HOP | REPLACED}
            headers["accept-encoding"] = "identity"
            if args.scheme == "bearer":
                headers["authorization"] = f"Bearer {token}"
            else:
                headers["x-api-key"] = token
            if args.add_beta:
                beta = [b for b in [self.headers.get("anthropic-beta"), args.add_beta] if b]
                headers = {k: v for k, v in headers.items() if k.lower() != "anthropic-beta"}
                headers["anthropic-beta"] = ",".join(beta)
            # http is only for trying the forwarder against a local fake.
            connection = http.client.HTTPConnection if upstream.scheme == "http" else http.client.HTTPSConnection
            conn = connection(upstream.hostname, upstream.port, timeout=900)
            try:
                conn.request(self.command, self.path, body=body, headers=headers)
                resp = conn.getresponse()
            except (OSError, http.client.HTTPException) as e:
                conn.close()
                entry.update(status=502, error=f"upstream: {type(e).__name__}: {e}", secs=round(time.monotonic() - started, 3))
                write(entry)
                self.send_error(502)
                return
            entry["status"] = resp.status
            entry["content_type"] = resp.getheader("content-type")
            entry["request_id"] = resp.getheader("request-id")
            entry["ratelimit"] = {
                k.lower(): v
                for k, v in resp.getheaders()
                if k.lower().startswith("anthropic-ratelimit") or k.lower() == "retry-after"
            }
            sent, gap, last, tail, stops, error_body = 0, 0.0, None, b"", 0, b""
            complete = False
            # Every request leaves its line in the log, however it ends.
            try:
                self.send_response(resp.status)
                for k, v in resp.getheaders():
                    if k.lower() not in HOP and k.lower() != "content-length":
                        self.send_header(k, v)
                if self.command == "HEAD" or resp.status in (204, 304):
                    self.end_headers()
                    complete = True
                    return
                self.send_header("transfer-encoding", "chunked")
                self.end_headers()
                while True:
                    try:
                        chunk = resp.read1(65536)
                    except (OSError, http.client.HTTPException) as e:
                        # A cut chunked body raises IncompleteRead, which is
                        # not an OSError; a timeout is one.
                        entry["upstream_broke"] = f"{type(e).__name__}: {e}"
                        break
                    now = time.monotonic()
                    if last is None:
                        entry["first_byte_secs"] = round(now - started, 3)
                    else:
                        gap = max(gap, now - last)
                    last = now
                    if not chunk:
                        complete = True
                        break
                    seen = tail + chunk
                    stops += seen.count(b"event: message_stop")
                    tail = seen[-(len(b"event: message_stop") - 1):]
                    if resp.status >= 400 and len(error_body) < ERROR_BODY_BYTES:
                        error_body += chunk[: ERROR_BODY_BYTES - len(error_body)]
                    self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                    self.wfile.flush()
                    sent += len(chunk)
                if complete:
                    self.wfile.write(b"0\r\n\r\n")
                    self.wfile.flush()
            except OSError as e:
                entry["client_closed"] = f"{type(e).__name__}: {e}"
            finally:
                conn.close()
                if not complete:
                    # Without the last chunk the client sees the response cut
                    # when the connection closes, instead of waiting on it.
                    self.close_connection = True
                entry.update(
                    bytes=sent,
                    secs=round(time.monotonic() - started, 3),
                    max_gap_secs=round(gap, 3),
                    upstream_complete=complete and "upstream_broke" not in entry,
                    message_stop=stops,
                )
                if error_body:
                    entry["error_body"] = error_body.decode("utf-8", "replace")
                write(entry)

        do_HEAD = do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = forward

    def stop(*_):
        raise KeyboardInterrupt

    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"listening on http://127.0.0.1:{args.port} -> {args.upstream} ({args.scheme})", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    log.close()
    if args.scan:
        held = scan(args.scan, token)
        print(f"files under {args.scan} holding the token: {len(held)}", flush=True)
        for path in held:
            print(f"  {path}", flush=True)


if __name__ == "__main__":
    main()
