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

MODEL = REPO / "crates/rbpmn-model/tests/fixtures/accept/28-demo-order.bpmn"
DECISION = REPO / "crates/rbpmn-dmn/tests/fixtures/accept/09-demo-triage.dmn"
# Read rather than written inline, because the editor opens on this same
# deployment and reads the same file. The topic names below are also the ones
# registered at `/v1/topics` further down: a second copy that drifted would
# leave the editor reporting `unresolved-topic` against this very server.
BINDINGS = json.loads(
    (REPO / "crates/rbpmn-model/tests/fixtures/accept/28-demo-order.bindings.json").read_text()
)


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
    try:
        with urllib.request.urlopen(request) as response:
            raw = response.read()
            return json.loads(raw) if raw else None
    except urllib.error.HTTPError as e:
        # The body is the whole point of a 4xx here: rbpmn answers a refused
        # deploy with its diagnostics, rule id and element included. Letting
        # urllib raise a bare "HTTP Error 400: Bad Request" throws that away
        # and makes a wiring mistake look like a broken script.
        detail = e.read().decode(errors="replace").strip()
        raise SystemExit(f"{method} {path} -> {e.code} {e.reason}\n{detail}") from None


def wait_port(port, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=1):
                return
        except OSError:
            time.sleep(0.2)
    raise RuntimeError(f"port {port} never came up")


def port_in_use(port):
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=1):
            return True
    except OSError:
        return False


def require_port_free(port):
    """Refuse to run against somebody else's server.

    Both this and the e2e recreate the demo schema before starting, so a
    leftover server still bound to the port answers every call with a 500
    against a database it no longer matches. `wait_port` cannot tell the
    difference — it sees an open socket and proceeds — and the resulting
    failure looks like a bug in the code under test rather than a stale
    process.
    """
    if not port_in_use(port):
        return
    raise SystemExit(
        f"port {port} is already in use — something is still running there.\n"
        f"That is almost always a leftover rbpmn-server from an interrupted run:\n"
        f"    kill $(lsof -ti :{port})"
    )


def start_server(env, quiet=True):
    """Build, then exec the binary directly — never through `cargo run`.

    `cargo run` makes cargo the child process and the server its grandchild,
    so terminating the handle kills cargo and orphans the server, which keeps
    the port. Every "port already in use" in this repo's history traces back
    to that.
    """
    subprocess.run(["cargo", "build", "-q", "-p", "rbpmn-server"], cwd=REPO, check=True)
    return subprocess.Popen(
        [str(REPO / "target/debug/rbpmn-server")],
        cwd=REPO,
        env=env,
        stdout=subprocess.DEVNULL if quiet else None,
    )


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


def available_item(instance, element):
    """The open work item at `element`, or a clear failure."""
    inspection = api(f"/v1/instances/{instance}/inspect", method="GET")
    for item in inspection["workItems"]:
        if item["state"] == "available" and item["elementId"] == element:
            return item["id"]
    raise SystemExit(
        f"expected an open work item at {element!r}, got "
        + repr([(w["elementId"], w["state"]) for w in inspection["workItems"]])
    )


def build_stuck_instance():
    """Walk one order through the whole model and leave it on an incident.

    The point is that the inspector has something to show. By the time this
    returns, the instance's history holds the document it started with, a
    worker's merge patch, a decision evaluated against that patch, the branch
    the decision sent it down, and an incident — which is the full vocabulary
    of the engine in one trace.
    """
    # Declared from the manifest rather than listed again: the environment half
    # of the wiring has to cover exactly the topics the deployment resolves to,
    # and deriving it is the only way that stays true when one of them is
    # renamed.
    for topic in sorted(set(BINDINGS["topics"].values())):
        api("/v1/topics", {"name": topic})

    # One deployment: process, wiring and decision together, versioned as a
    # unit. The decision binding says which invocable `triage` calls and where
    # its answer lands — in code, never in the XML.
    api(
        "/v1/definitions",
        {
            "bpmn": MODEL.read_text(),
            "decisions": [DECISION.read_text()],
            "bindings": BINDINGS,
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
                # Top-level and scalar, because that is what an index can be:
                # it becomes a real index on one JSONB field, so a dotted path
                # is refused at deploy.
                "channel": "web",
            },
        },
    )["instanceId"]

    # The risk worker answers with a merge patch — a *delta*, which is what a
    # worker sends and why `variables-patched` records the patch rather than
    # the result. The decision then reads `risk.score` from it.
    api(
        f"/v1/work-items/{available_item(instance, 'score')}/complete",
        {"patch": {"risk": {"score": 82, "model": "fraud-v3"}}},
    )

    # 82 >= 70, so the table's first rule matches and `triage.band` is
    # "review" — which is the branch the gateway takes, so an item is waiting
    # at the user task rather than at the charge. Completing it walks on.
    api(f"/v1/work-items/{available_item(instance, 'review')}/complete", {})

    # An error code the boundary does NOT catch (it listens for
    # PAYMENT_FAILED), so exhausting the budget freezes the instance instead
    # of taking the recovery path — the case someone actually gets paged for.
    item = available_item(instance, "charge")
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
    require_port_free(7420)
    require_port_free(PROXY_PORT)
    if psql(f"select 1 from pg_database where datname = '{DB}'").stdout.count("1 row") == 0:
        psql(f"create database {DB}")
    # A demo should start from nothing every time, or the second run inspects
    # the first run's leftovers.
    psql("drop schema public cascade; create schema public", db=DB)

    env = {
        **os.environ,
        "RBPMN_DATABASE_URL": f"postgres://{os.environ.get('USER')}@localhost:5432/{DB}",
        "RBPMN_API_TOKEN": TOKEN,
        "RBPMN_TOPICS": "risk-scoring,payments",
    }
    print("building and starting rbpmn-server ...")
    server = start_server(env)
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

   One order walked the whole model. Its history holds, in order:
   the variables it started with, a worker's merge patch (the risk
   score), a DMN decision table evaluated against that patch, the
   branch the decision chose, and the incident it ended on.

   It is frozen because 'Charge the card' kept answering 502 and
   raised GATEWAY_TIMEOUT, which the error boundary (PAYMENT_FAILED)
   does not catch — the case someone gets paged for.

   Worth clicking, in the inspector:
     * 'Triage the order' — the decision, its answer, and the
       variables pane showing where the answer landed
     * the gateway's two branches: 'review' was taken because the
       risk score was 82
     * 'Charge the card' — every retry, with the handler's message

   The editor opens on this same deployment — the model, its
   manifest and the decision together, which is what makes it a
   deployment rather than a diagram. Press "Check against server":
   both topics below are declared here, so the wiring comes back
   covered. Open triage.dmn to see the table this instance ran, and
   note that both its inputs are *declared* — a FEEL expression can
   only read names the decision says it requires.

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
