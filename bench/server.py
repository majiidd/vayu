#!/usr/bin/env python3
"""Range-capable HTTP server with controlled bandwidth and latency, for local benchmarks.

Usage:
  bench/server.py [--size MIB] [--per-conn RATE] [--global RATE] [--latency MS] [--no-ranges]
                  [--host HOST] [--port PORT]

RATE is bytes per second with an optional binary K or M suffix (2M = 2 MiB/s).

Every path serves the same deterministic payload with Content-Length, a strong ETag, and
`Accept-Ranges: bytes`; `Range: bytes=A-B` and `bytes=A-` are honored unless --no-ranges is given.
HTTP/1.1 keep-alive is supported.

  --per-conn  shapes each response, like a host that limits every connection;
  --global    is one budget shared by all responses, like a saturated access link;
  --latency   delays each response's headers, approximating the round trips before the first
              byte. It does not model TCP slow start or packet loss; use netem for those.

GET /__stats returns JSON with the request count, bytes sent, and the peak number of responses
in flight since the last GET /__reset. The server prints its base URL on the first line of
stdout.
"""

import argparse
import json
import random
import socket
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PIECE = 64 * 1024


def rate(text):
    text = text.strip().upper()
    scale = {"K": 1024, "M": 1024 * 1024}.get(text[-1:], 1)
    return int(float(text.rstrip("KM")) * scale)


class Budget:
    """A bandwidth budget: callers book bytes and learn how long to wait before sending."""

    def __init__(self, bytes_per_second):
        self.rate = float(bytes_per_second)
        self.lock = threading.Lock()
        self.start = time.monotonic()
        self.booked = 0

    def wait(self, n):
        with self.lock:
            now = time.monotonic()
            if now - self.start > self.booked / self.rate + 1:
                # Idle since the last booking: no catch-up burst.
                self.start, self.booked = now, 0
            self.booked += n
            due = self.booked / self.rate
        return max(0.0, due - (time.monotonic() - self.start))


class Stats:
    def __init__(self):
        self.lock = threading.Lock()
        self.reset()

    def reset(self):
        self.requests = 0
        self.sent = 0
        self.active = 0
        self.peak = 0

    def snapshot(self):
        with self.lock:
            return {
                "requests": self.requests,
                "bytes_sent": self.sent,
                "peak_connections": self.peak,
            }


def make_handler(args, payload, etag, shared, stats):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def do_GET(self):
            if self.path == "/__stats":
                return self.reply_json(stats.snapshot())
            if self.path == "/__reset":
                with stats.lock:
                    stats.reset()
                return self.reply_json({})
            with stats.lock:
                stats.requests += 1
            start, end = 0, len(payload)
            status = 200
            spec = self.headers.get("Range")
            if spec and not args.no_ranges and spec.startswith("bytes="):
                first, _, last = spec[6:].partition("-")
                try:
                    start = int(first)
                    end = min(int(last) + 1, len(payload)) if last else len(payload)
                except ValueError:
                    start, end = 0, len(payload)
                if start >= len(payload) or start >= end:
                    self.send_response(416)
                    self.send_header("Content-Range", f"bytes */{len(payload)}")
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                if_range = self.headers.get("If-Range")
                status = 206 if if_range in (None, etag) else 200
                if status == 200:
                    start, end = 0, len(payload)
            if args.latency:
                time.sleep(args.latency / 1000)
            self.send_response(status)
            self.send_header("Content-Length", str(end - start))
            self.send_header("ETag", etag)
            self.send_header("Accept-Ranges", "none" if args.no_ranges else "bytes")
            if status == 206:
                self.send_header("Content-Range", f"bytes {start}-{end - 1}/{len(payload)}")
            self.end_headers()
            self.send_body(start, end)

        def send_body(self, start, end):
            own = Budget(args.per_conn) if args.per_conn else None
            with stats.lock:
                stats.active += 1
                stats.peak = max(stats.peak, stats.active)
            try:
                at = start
                while at < end:
                    n = min(PIECE, end - at)
                    wait = 0.0
                    if own:
                        wait = own.wait(n)
                    if shared:
                        wait = max(wait, shared.wait(n))
                    if wait:
                        time.sleep(wait)
                    self.wfile.write(payload[at : at + n])
                    with stats.lock:
                        stats.sent += n
                    at += n
            except (BrokenPipeError, ConnectionResetError):
                self.close_connection = True
            finally:
                with stats.lock:
                    stats.active -= 1

        def reply_json(self, value):
            body = json.dumps(value).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--size", type=float, default=64, help="payload size in MiB")
    parser.add_argument("--per-conn", type=rate, default=0)
    parser.add_argument("--global", dest="shared", type=rate, default=0)
    parser.add_argument("--latency", type=float, default=0, help="milliseconds before headers")
    parser.add_argument("--no-ranges", action="store_true")
    parser.add_argument("--host", default="::1")
    parser.add_argument("--port", type=int, default=0)
    args = parser.parse_args()

    payload = random.Random(1).randbytes(int(args.size * 1024 * 1024))
    etag = f'"bench-{len(payload)}"'
    shared = Budget(args.shared) if args.shared else None
    stats = Stats()

    family = socket.AF_INET6 if ":" in args.host else socket.AF_INET

    class Server(ThreadingHTTPServer):
        address_family = family
        daemon_threads = True

    server = Server((args.host, args.port), make_handler(args, payload, etag, shared, stats))
    host = f"[{args.host}]" if family == socket.AF_INET6 else args.host
    print(f"http://{host}:{server.server_address[1]}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
