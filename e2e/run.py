#!/usr/bin/env python3
"""End-to-end browser tests for the rbpmn playground, with screenshots.

Drives the built playground headlessly, screenshots every fixture in the
corpus, and — when a local Postgres is reachable — runs the full inspection
stack (rbpmn-server + deploy + start + token-overlay view) and screenshots
that too. Screenshots land in e2e/screenshots/ (gitignored); the script
exits non-zero on any assertion failure.

Requirements: node/npm, python3 with playwright (`pip install playwright`
then `playwright install chromium`), and optionally a local Postgres.
Run via `just e2e`.
"""

import json
import os
import socket
import subprocess
import sys
import time
import urllib.request
import uuid
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SHOTS = Path(os.environ.get("RBPMN_SCREENSHOT_DIR", REPO / "e2e" / "screenshots"))
PLAYGROUND = REPO / "playground"
TOKEN = "e2e-token-0123456789abcdef-0123456789abcdef"
PREVIEW = "http://localhost:4173"
SERVER = "http://127.0.0.1:7420"

failures: list[str] = []


def check(condition, message):
    if not condition:
        failures.append(message)
        print(f"  FAIL {message}")


def wait_port(port, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        for host in ("127.0.0.1", "::1"):
            try:
                with socket.create_connection((host, port), timeout=1):
                    return
            except OSError:
                pass
        time.sleep(0.2)
    raise RuntimeError(f"port {port} never came up")


def api(path, body=None, method="POST"):
    req = urllib.request.Request(
        SERVER + path,
        data=json.dumps(body).encode() if body is not None else None,
        method=method,
        headers={"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req) as resp:
        data = resp.read()
        return json.loads(data) if data else None


def postgres_reachable():
    try:
        with socket.create_connection(("127.0.0.1", 5432), timeout=1):
            return True
    except OSError:
        return False


def screenshot_fixtures(page):
    page.goto(PREVIEW)
    page.wait_for_load_state("networkidle")
    page.wait_for_selector(".fixture-item")
    items = page.locator(".fixture-item")
    count = items.count()
    print(f"fixtures listed: {count}")
    check(count >= 40, f"expected the full corpus, got {count}")

    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    for i in range(count):
        button = items.nth(i)
        name = button.get_attribute("data-name")
        button.click()
        page.wait_for_timeout(400)
        safe = name.replace("/", "_").replace(".bpmn", "")
        page.screenshot(path=str(SHOTS / f"fixture_{safe}.png"))
        # Reject fixtures must visibly show what kills them.
        if name.startswith("reject"):
            verdict = page.inner_text("#verdict")
            check(verdict == "rejected", f"{name}: verdict '{verdict}'")
    check(not errors, f"console errors during fixture sweep: {errors[:3]}")


def screenshot_inspection(page, server_proc_env):
    xml = (
        REPO / "crates/rbpmn-model/tests/fixtures/accept/03-parallel-gateway.bpmn"
    ).read_text()
    deployed = api("/v1/definitions", {"bpmn": xml})
    print(f"deployed {deployed['key']} v{deployed['version']}")
    instance = api("/v1/instances", {"definitionKey": "p", "variables": {"orderId": 7}})[
        "instanceId"
    ]

    # The deep link: token entered once, instance via URL hash.
    page.goto(PREVIEW)
    page.wait_for_load_state("networkidle")
    page.fill("#inspect-token", TOKEN)
    page.fill("#inspect-id", instance)
    page.click("#inspect-load")
    page.wait_for_timeout(1500)
    page.screenshot(path=str(SHOTS / "inspection_active.png"))

    check(page.locator(".rbpmn-badge-token").count() == 2, "expected 2 token badges")
    check(page.locator(".rbpmn-mark-work").count() == 2, "expected 2 work-item marks")
    check("active" in page.inner_text("#verdict"), "instance should be active")

    # Complete one branch, reload through the deep link (auto-load).
    inspection = api(f"/v1/instances/{instance}/inspect", method="GET")
    item = next(w for w in inspection["workItems"] if w["state"] in ("available", "locked"))
    api(f"/v1/work-items/{item['id']}/complete", {"patch": {"packed": True}})

    page.goto(f"{PREVIEW}/#instance={instance}")
    page.reload()
    page.wait_for_timeout(1500)
    page.screenshot(path=str(SHOTS / "inspection_half_done.png"))
    check(
        page.locator(".rbpmn-badge-token").count() == 2,
        "one branch done: task token + join token expected",
    )


def load_and_shot(page, instance, name, expect_token_badges=None):
    """Reload the inspection for `instance` and capture one flow frame."""
    page.fill("#inspect-id", instance)
    page.click("#inspect-load")
    page.wait_for_timeout(1200)
    page.screenshot(path=str(SHOTS / f"{name}.png"))
    if expect_token_badges is not None:
        got = page.locator(".rbpmn-badge-token").count()
        check(got == expect_token_badges, f"{name}: {got} token badge(s), expected {expect_token_badges}")


def open_item(instance, element=None):
    inspection = api(f"/v1/instances/{instance}/inspect", method="GET")
    for w in inspection["workItems"]:
        if w["state"] in ("available", "locked") and (element is None or w["elementId"] == element):
            return w["id"]
    raise AssertionError(f"no open work item at {element} in {instance}")


def screenshot_token_flow(page):
    """Flip-book sequences: watch tokens move through a whole lifecycle."""
    page.goto(PREVIEW)
    page.wait_for_load_state("networkidle")
    page.fill("#inspect-token", TOKEN)

    # Flow A — parallel block with an XOR inside (05): the fast lane.
    xml = (
        REPO / "crates/rbpmn-model/tests/fixtures/accept/05-xor-inside-parallel.bpmn"
    ).read_text()
    api("/v1/definitions", {"bpmn": xml})
    instance = api(
        "/v1/instances", {"definitionKey": "p", "variables": {"fast": True}}
    )["instanceId"]
    load_and_shot(page, instance, "flow_a_1_two_branches", expect_token_badges=2)
    api(f"/v1/work-items/{open_item(instance, 'tf')}/complete", {})
    load_and_shot(page, instance, "flow_a_2_fast_lane_done_token_at_join", expect_token_badges=2)
    api(f"/v1/work-items/{open_item(instance, 'tb')}/complete", {})
    load_and_shot(page, instance, "flow_a_3_completed", expect_token_badges=0)
    check(
        "completed" in page.inner_text("#inspect-note"),
        "flow A should end completed",
    )

    # Flow B — error boundary (10): three failures walk into the boundary path.
    xml = (
        REPO / "crates/rbpmn-model/tests/fixtures/accept/10-error-boundary.bpmn"
    ).read_text()
    api("/v1/topics", {"name": "st"})
    api("/v1/definitions", {"bpmn": xml})
    instance = api("/v1/instances", {"definitionKey": "p"})["instanceId"]
    load_and_shot(page, instance, "flow_b_1_service_task_pending", expect_token_badges=1)
    for _ in range(3):
        api(
            f"/v1/work-items/{open_item(instance, 'st')}/fail",
            {"errorCode": "PAYMENT_FAILED"},
        )
    load_and_shot(page, instance, "flow_b_2_boundary_taken_repair_task", expect_token_badges=1)
    inspection = api(f"/v1/instances/{instance}/inspect", method="GET")
    check(
        inspection["tokens"][0]["elementId"] == "t_fix",
        "token should sit at the repair task after the boundary",
    )
    api(f"/v1/work-items/{open_item(instance, 't_fix')}/complete", {})
    load_and_shot(page, instance, "flow_b_3_completed_via_error_path", expect_token_badges=0)


def main():
    SHOTS.mkdir(parents=True, exist_ok=True)
    for old in SHOTS.glob("*.png"):
        old.unlink()

    build = subprocess.run(["npm", "run", "build"], cwd=PLAYGROUND, capture_output=True, text=True)
    if build.returncode != 0:
        print(build.stdout)
        print(build.stderr)
        sys.exit("playground build failed")
    procs = []
    db_name = None
    try:
        procs.append(
            subprocess.Popen(
                ["npm", "run", "preview"],
                cwd=PLAYGROUND,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        )
        wait_port(4173)

        with_stack = postgres_reachable()
        if with_stack:
            db_name = f"rbpmn_e2e_{uuid.uuid4().hex[:8]}"
            subprocess.run(
                ["psql", "-q", "-h", "localhost", "postgres", "-c", f"create database {db_name}"],
                check=True,
            )
            subprocess.run(
                ["cargo", "build", "-q", "-p", "rbpmn-server"], cwd=REPO, check=True
            )
            env = os.environ | {
                "RBPMN_DATABASE_URL": f"postgres://{os.environ.get('USER', 'postgres')}@localhost:5432/{db_name}",
                "RBPMN_API_TOKEN": TOKEN,
            }
            server_log = open(SHOTS / "server.log", "w")
            server = subprocess.Popen(
                [REPO / "target/debug/rbpmn-server"],
                env=env,
                stdout=server_log,
                stderr=server_log,
            )
            procs.append(server)
            try:
                wait_port(7420)
            except RuntimeError:
                server_log.flush()
                print((SHOTS / "server.log").read_text())
                raise
            if server.poll() is not None:
                print((SHOTS / "server.log").read_text())
                sys.exit("rbpmn-server exited during startup")
        else:
            print("no local Postgres — skipping the inspection stack (fixture sweep only)")

        from playwright.sync_api import sync_playwright

        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True)
            page = browser.new_page(viewport={"width": 1440, "height": 900})
            screenshot_fixtures(page)
            if with_stack:
                screenshot_inspection(page, None)
                screenshot_token_flow(page)
            browser.close()
    finally:
        for proc in procs:
            proc.terminate()
        for proc in procs:
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        if db_name:
            subprocess.run(
                ["psql", "-q", "-h", "localhost", "postgres", "-c",
                 f"drop database {db_name} (force)"],
                check=False,
            )

    shots = sorted(SHOTS.glob("*.png"))
    print(f"{len(shots)} screenshot(s) in {SHOTS.relative_to(REPO)}/")
    if failures:
        print(f"\n{len(failures)} FAILURE(S):")
        for f in failures:
            print(f"  {f}")
        sys.exit(1)
    print("E2E OK")


if __name__ == "__main__":
    main()
