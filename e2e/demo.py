#!/usr/bin/env python3
"""A live demo of the two UI documents, against a real engine. `just demo`.

Brings up Postgres, rbpmn-server and a real process instance, drives that
instance into an **incident** — a service task whose handler kept answering
502 until the retry budget ran out, with an error boundary that does not match
the code it raised — and then prints two links and waits.

The proxy in front is the point, not scaffolding. rbpmn's UI routes sit behind
the same bearer as everything else, and a browser cannot send that header on a
top-level navigation. That is deliberate: the documented posture is that an
*application* authenticates its user and reverse-proxies to rbpmn. This script
is the smallest honest version of that application — it authenticates nobody,
which is exactly why it binds to loopback and says so.

Requires python3 (stdlib only) and a local Postgres.
"""

import http.server
import json
import os
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
TOKEN = "demo-token-0123456789abcdef-0123456789abcdef"
SERVER = "http://127.0.0.1:7420"
PROXY_PORT = int(os.environ.get("RBPMN_DEMO_PORT", "8099"))
DB = "rbpmn_demo"

MODEL = REPO / "crates/rbpmn-model/tests/fixtures/accept/10-error-boundary.bpmn"


def psql(sql, db="postgres"):
    return subprocess.run(
        ["psql", "-q", "-h", "localhost", db, "-c", sql],
        capture_output=True,
        text=True,
    )


def api(path, body=None, method="POST"):
    request = urllib.request.Request(
        SERVER + path,
        data=json.dumps(body).encode() if body is not None else None,
        method=method,
        headers={
            "Authorization": f"Bearer {TOKEN}",
            "Content-Type": "application/json",
        },
    )
    with urllib.request.urlopen(request) as response:
        raw = response.read()
        return json.loads(raw) if raw else None


def wait_port(port, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=1):
                return
        except OSError:
            time.sleep(0.2)
    raise RuntimeError(f"port {port} never came up")


class AuthInjectingProxy(http.server.BaseHTTPRequestHandler):
    """Forwards to rbpmn-server, adding the bearer the browser cannot send.

    This is the one job the embedding application has that rbpmn deliberately
    will not do for it. A real one would authenticate the user first and
    decide whether they may see this instance at all.
    """

    protocol_version = "HTTP/1.1"

    def do_GET(self):  # noqa: N802 - BaseHTTPRequestHandler's naming
        self.forward("GET")

    def do_POST(self):  # noqa: N802
        length = int(self.headers.get("content-length") or 0)
        self.forward("POST", self.rfile.read(length) if length else None)

    def forward(self, method, body=None):
        request = urllib.request.Request(
            SERVER + self.path,
            data=body,
            method=method,
            headers={
                "Authorization": f"Bearer {TOKEN}",
                "Accept": self.headers.get("accept", "*/*"),
                "Content-Type": self.headers.get("content-type", "application/json"),
            },
        )
        try:
            with urllib.request.urlopen(request) as response:
                payload = response.read()
                status, headers = response.status, response.headers
        except urllib.error.HTTPError as e:
            payload = e.read()
            status, headers = e.code, e.headers
        except urllib.error.URLError as e:
            payload = f"cannot reach rbpmn-server: {e}".encode()
            status, headers = 502, {}

        self.send_response(status)
        for header in ("content-type", "cache-control", "x-content-type-options"):
            value = headers.get(header) if headers else None
            if value:
                self.send_header(header, value)
        # A real application would also set frame-ancestors here, and serve
        # the document inside <iframe sandbox="allow-scripts"> — see
        # docs/http-security.md.
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *_args):
        pass


def build_stuck_instance():
    """Deploy, start, and fail until the instance freezes on an incident."""
    api("/v1/topics", {"name": "payments"})
    api(
        "/v1/definitions",
        {
            "bpmn": MODEL.read_text(),
            "bindings": {"topics": {"st": "payments"}},
        },
    )
    instance = api(
        "/v1/instances",
        {
            "definitionKey": "p",
            "businessKey": "order-4711",
            "variables": {
                "order": {
                    "id": "o-4711",
                    "total": 129.95,
                    "currency": "EUR",
                    "lines": [
                        {"sku": "RB-100", "qty": 2, "price": 49.95},
                        {"sku": "RB-205", "qty": 1, "price": 30.05},
                    ],
                },
                "customer": {"id": "c-88", "tier": "gold", "email": "ada@example.com"},
                "payment": {"method": "card", "last4": "4242", "attempts": 0},
            },
        },
    )["instanceId"]

    inspection = api(f"/v1/instances/{instance}/inspect", method="GET")
    item = next(w for w in inspection["workItems"] if w["state"] == "available")["id"]

    # An error code the boundary does NOT catch (it listens for
    # PAYMENT_FAILED), so exhausting the budget freezes the instance instead
    # of taking the recovery path — the case someone actually gets paged for.
    for attempt in range(1, 12):
        outcome = api(
            f"/v1/work-items/{item}/fail",
            {
                "errorCode": "GATEWAY_TIMEOUT",
                "errorMessage": f"handler answered 502 (Bad Gateway), attempt {attempt}",
            },
        )["outcome"]
        if outcome != "retrying":
            print(f"  work item {outcome} after {attempt} attempt(s)")
            break
    return instance


def main():
    if psql("select 1").returncode != 0:
        sys.exit("no local Postgres on localhost — start one and retry")
    if psql(f"select 1 from pg_database where datname = '{DB}'").stdout.count("1 row") == 0:
        psql(f"create database {DB}")
    # A demo should start from nothing every time, or the second run inspects
    # the first run's leftovers.
    psql("drop schema public cascade; create schema public", db=DB)

    env = {
        **os.environ,
        "RBPMN_DATABASE_URL": f"postgres://{os.environ.get('USER')}@localhost:5432/{DB}",
        "RBPMN_API_TOKEN": TOKEN,
        "RBPMN_TOPICS": "payments",
    }
    print("building and starting rbpmn-server ...")
    server = subprocess.Popen(
        ["cargo", "run", "-q", "-p", "rbpmn-server"],
        cwd=REPO,
        env=env,
        stdout=subprocess.DEVNULL,
    )
    try:
        wait_port(7420)
        print("creating a stuck instance ...")
        instance = build_stuck_instance()

        proxy = http.server.ThreadingHTTPServer(("127.0.0.1", PROXY_PORT), AuthInjectingProxy)
        threading.Thread(target=proxy.serve_forever, daemon=True).start()

        base = f"http://localhost:{PROXY_PORT}"
        print(
            f"""
  ────────────────────────────────────────────────────────────────
   inspector   {base}/ui/inspect/{instance}
   editor      {base}/ui/editor

   The instance is frozen on an incident: 'Charge card' kept
   answering 502 and raised GATEWAY_TIMEOUT, which the error
   boundary (PAYMENT_FAILED) does not catch.

   In the editor, press "Check against server" — the topic
   'payments' is declared, so the wiring shows as covered.

   Loopback only, and the proxy in front authenticates nobody:
   it just adds the bearer a browser cannot send. That is the
   application's job in a real deployment.

   Ctrl-C to stop (the {DB} database is left for poking at).
  ────────────────────────────────────────────────────────────────
"""
        )
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        print("\nstopping")
    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()


if __name__ == "__main__":
    main()
