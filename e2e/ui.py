#!/usr/bin/env python3
"""Browser checks for the two rbpmn-ui documents, driven from file://.

Everything else about these documents is asserted in Rust: one executable
script, hashes pinned by the policy, hostile data escaped. None of that
notices if the page renders a blank canvas — so this opens the real files in
a real browser and checks that the diagram drew, the panes filled in, and the
editor's embedded linter actually ran.

`file://` on purpose: self-containment is the whole design, and a document
that needs a server to work has lost it. The CSP the documents carry is
enforced here exactly as a browser would enforce it, so a policy that forbids
something the page needs fails this and nothing else.

Requires python3 with playwright; run via `just e2e-ui` (or e2e/run.py, which
calls in here).
"""

import http.server
import os
import subprocess
import sys
import threading
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(Path(__file__).resolve().parent))
DIST = REPO / "ui" / "dist"
SHOTS = Path(os.environ.get("RBPMN_SCREENSHOT_DIR", REPO / "e2e" / "screenshots"))

failures: list[str] = []


def skip_served(reason):
    """Skip the served half locally; refuse to skip it where it is mandatory.

    Both of this half's skip paths print one line and let the run finish with
    "ui documents ok" — a test that silently downgrades itself to a weaker
    check and still passes. That is the shape this repo refuses elsewhere
    (`just tla` will not read a spec that stopped parsing as "fails as
    expected"), and it bit exactly as predicted: CI ran green for a build
    with the served half skipped, because the builder's user has no
    PostgreSQL role.

    The graceful skip is still right for a developer without Postgres — the
    `file://` half is worth running on its own. So the strictness is opt-in
    and CI opts in: `RBPMN_E2E_REQUIRE_SERVED=1`.
    """
    if os.environ.get("RBPMN_E2E_REQUIRE_SERVED") == "1":
        check(False, f"served stack: {reason} (RBPMN_E2E_REQUIRE_SERVED is set)")
    else:
        print(f"served stack: {reason} — skipping")


def check(condition, message):
    if not condition:
        failures.append(message)
        print(f"  FAIL {message}")
    else:
        print(f"  ok   {message}")


def collect_problems(page):
    """CSP violations surface as console errors, so they fail the run."""
    problems: list[str] = []
    page.on(
        "console",
        lambda m: problems.append(f"console.{m.type}: {m.text}")
        if m.type == "error"
        else None,
    )
    page.on("pageerror", lambda e: problems.append(f"pageerror: {e}"))
    # Nothing may leave the document. A request to anything but the file
    # itself means self-containment broke.
    page.on(
        "request",
        lambda r: problems.append(f"network request: {r.url}")
        if not r.url.startswith("file://") and not r.url.startswith("data:")
        else None,
    )
    return problems


def check_inspector(browser):
    print("inspector.html")
    page = browser.new_page()
    problems = collect_problems(page)
    page.goto((DIST / "inspector.html").as_uri())
    page.wait_for_selector(".djs-container", timeout=15000)

    # The diagram drew real shapes, not an empty canvas.
    shapes = page.locator(".djs-element").count()
    check(shapes > 0, f"diagram rendered {shapes} elements")

    # The sample instance is frozen at 'st'; that is the headline.
    diagnosis = page.inner_text(".diagnosis")
    check("Incident at st" in diagnosis, f"diagnosis names the incident ({diagnosis!r})")
    check("handler answered 502" in diagnosis, "diagnosis carries the failure detail")

    # Runtime markers landed on the diagram.
    badges = page.locator(".rbpmn-badge").count()
    check(badges > 0, f"{badges} runtime marker(s) on the diagram")

    # The element pane fuses model, wiring and runtime. Clicking the failed
    # service task must show the topic from the *manifest*.
    page.click('.djs-element[data-element-id="st"]')
    pane = page.inner_text(".element-pane")
    check("ServiceTask" in pane, "element pane shows the model type")
    check("payments" in pane, "element pane shows the manifest topic")
    check("handler answered 502" in pane, "element pane shows the last failure")

    # An element the token never reached still shows its wiring — the reason
    # the manifest travels with the inspection at all. `t_fix` is on the
    # recovery path that was never taken, so no work item ever carried its
    # topic and the manifest is the only source for it.
    page.click('.djs-element[data-element-id="t_fix"]')
    pane = page.inner_text(".element-pane")
    check("payment-recovery" in pane, "unreached element still shows its bound topic")

    # The error boundary's code is the reason this instance froze rather than
    # recovering, so the pane must show it.
    page.click('.djs-element[data-element-id="be"]')
    pane = page.inner_text(".element-pane")
    check("PAYMENT_FAILED" in pane, "the boundary's error code is visible")
    check("attached to" in pane and "st" in pane, "the boundary shows its host")

    # Variables render as a tree.
    side = page.inner_text(".side")
    check("o-4711" in side, "variables are shown")

    # The detail column scrolls rather than growing past the viewport. Same
    # grid `min-height: auto` trap as the editor's, and just as invisible
    # until an instance carries a long trace or a big variable document.
    page.evaluate("() => document.querySelectorAll('details').forEach(d => d.open = true)")
    fits = page.evaluate(
        "() => { const s = document.querySelector('.side');"
        " return s.clientHeight <= window.innerHeight + 1 && s.scrollHeight > s.clientHeight; }"
    )
    check(fits, "the detail column stays inside the viewport and scrolls")

    SHOTS.mkdir(parents=True, exist_ok=True)
    page.screenshot(path=str(SHOTS / "ui_inspector.png"), full_page=False)
    check(not problems, f"no console errors or network requests: {problems}")
    page.close()


def check_editor(browser):
    print("editor.html")
    page = browser.new_page()
    problems = collect_problems(page)
    page.goto((DIST / "editor.html").as_uri())
    page.wait_for_selector(".djs-container", timeout=15000)
    # The starter diagram plus a validator that had to compile ~400KB of WASM.
    page.wait_for_function(
        "() => document.querySelector('.verdict') && "
        "!document.querySelector('.verdict').textContent.includes('checking')",
        timeout=20000,
    )

    verdict = page.inner_text(".verdict")
    check(verdict != "", f"the embedded linter ran (verdict: {verdict!r})")
    # No server was consulted, so the honest verdict is "unchecked", never
    # "would deploy".
    check("environment unchecked" in verdict, "an unchecked environment is said out loud")

    check(page.locator(".djs-element").count() > 0, "starter diagram rendered")
    check(page.locator(".djs-palette").count() == 1, "the modelling palette is present")

    # The manifest and XML panes start collapsed; open them the way a user
    # would before driving them.
    page.evaluate("() => document.querySelectorAll('details.pane').forEach(d => d.open = true)")

    # Paste an existing model through the XML pane, then confirm the linter
    # reacts and the wiring pane offers the manifest binding.
    xml = page.locator("textarea.code").nth(1)
    xml.fill(
        (REPO / "crates/rbpmn-model/tests/fixtures/accept/07-task-kinds.bpmn").read_text()
    )
    page.wait_for_function(
        "() => document.querySelectorAll('.djs-element[data-element-id=\"st\"]').length > 0",
        timeout=15000,
    )
    check(True, "a pasted model imports and lays out")

    # 07-task-kinds has a receive task, which has no correlation binding yet:
    # exactly the manifest gap only L2 can see.
    page.wait_for_function(
        "() => document.querySelector('.diagnostics').textContent"
        ".includes('message-has-correlation')",
        timeout=15000,
    )
    check(True, "L2 reports the missing correlation binding")

    # Bind it in the wiring pane and watch the diagnostic clear — the loop the
    # editor exists to close.
    page.click('.djs-element[data-element-id="rt"]')
    wiring = page.locator(".wiring input").first
    wiring.fill("order.id")
    wiring.blur()
    page.wait_for_function(
        "() => !document.querySelector('.diagnostics').textContent"
        ".includes('message-has-correlation')",
        timeout=15000,
    )
    check(True, "binding the correlation in the wiring pane clears the diagnostic")

    manifest = page.locator("textarea.code").first.input_value()
    check('"rt": "order.id"' in manifest, f"the manifest JSON updated ({manifest!r})")

    SHOTS.mkdir(parents=True, exist_ok=True)
    page.screenshot(path=str(SHOTS / "ui_editor.png"), full_page=False)
    check_condition_repair(page)
    # Asserted last: check_condition_repair drives the whole condition-editing
    # flow, and anything it throws — a JS exception in reveal(), a
    # CSP-blocked request — lands in this same list. Checking before it ran
    # meant those passed silently.
    check(not problems, f"no console errors or network requests: {problems}")
    page.close()


def check_condition_repair(page):
    """The loop a modeler with a broken split actually walks.

    A gateway whose branches use full FEEL produces one
    `conditions-feel-subset` error per branch. Reading them is useless unless
    clicking one puts the offending flow in the Element pane — `focus` alone
    marks and scrolls without selecting, which left the diagnostics pointing
    at something the panes never showed.
    """
    print("editor.html — repairing a broken split")
    xml = page.locator("textarea.code").nth(1)
    xml.fill(
        (REPO / "crates/rbpmn-model/tests/fixtures/reject/condition-full-feel.bpmn").read_text()
    )
    page.wait_for_function(
        "() => (document.querySelectorAll('.diagnostic .rule').length "
        "&& [...document.querySelectorAll('.diagnostic .rule')]"
        ".filter(r => r.textContent === 'conditions-feel-subset').length >= 3)",
        timeout=15000,
    )
    check(True, "three broken branches report three condition errors")

    # Long diagnostic lists must not bury the pane you fix them in.
    overflows = page.evaluate(
        "() => { const d = document.querySelector('.diagnostics');"
        " return getComputedStyle(d).overflowY; }"
    )
    check(overflows in ("auto", "scroll"), f"the diagnostics list scrolls on its own ({overflows})")

    # The side column itself must scroll rather than growing past the viewport
    # (a grid item defaults to min-height:auto, which silently defeats
    # overflow-y).
    fits = page.evaluate(
        "() => { const s = document.querySelector('.side');"
        " return s.clientHeight <= window.innerHeight + 1; }"
    )
    check(fits, "the side column stays inside the viewport instead of growing")

    # Click the first condition diagnostic: the flow must land in the pane,
    # with an editable condition.
    page.locator(".diagnostic", has_text="conditions-feel-subset").first.click()
    pane = page.inner_text(".properties")
    check("SequenceFlow" in pane, f"clicking a diagnostic selects the flow ({pane[:60]!r})")
    check("condition" in pane, "the selected flow exposes its condition")

    # And the gateway offers every branch at once, which is how you fix a
    # split without hunting for edges.
    page.click('.djs-element[data-element-id="xs"]')
    pane = page.inner_text(".properties")
    check("branch conditions" in pane.lower(), "the gateway lists its branch conditions")
    check("default branch" in pane, "the default branch is shown as such, with no input")
    for flow in ("f_fn", "f_arith", "f_range", "f_def"):
        check(flow in pane, f"the gateway offers branch {flow}")

    # Branch conditions make this pane tall; it must scroll in place rather
    # than pushing the wiring and manifest panes out of reach.
    overflows = page.evaluate(
        "() => getComputedStyle(document.querySelector('.properties')).overflowY"
    )
    check(overflows in ("auto", "scroll"), f"the element pane scrolls on its own ({overflows})")

    # Repair one branch from the gateway and watch that error clear.
    before = page.locator(".diagnostic", has_text="conditions-feel-subset").count()
    boxes = page.locator(".properties .prop", has_text="f_fn").locator("input")
    boxes.first.fill("amount > 100")
    boxes.first.blur()
    page.wait_for_function(
        f"() => [...document.querySelectorAll('.diagnostic .rule')]"
        f".filter(r => r.textContent === 'conditions-feel-subset').length < {before}",
        timeout=15000,
    )
    check(True, "fixing a branch from the gateway clears its diagnostic")
    page.screenshot(path=str(SHOTS / "ui_editor_conditions.png"), full_page=False)


def check_dark_mode(browser):
    """Legible on a dark desktop.

    bpmn-js paints strokes, label text and arrowhead markers from options
    given at construction, as SVG attributes — CSS cannot reach them. So a
    dark canvas kept black labels and black arrows and the diagram became
    unreadable in place, which is exactly what happens when a laptop switches
    theme at sunset with the document open.
    """
    print("dark mode")
    for name in ("inspector", "editor"):
        page = browser.new_page(color_scheme="dark")
        problems = collect_problems(page)
        page.goto((DIST / f"{name}.html").as_uri())
        page.wait_for_selector(".djs-container", timeout=20000)
        page.wait_for_timeout(1500)

        # The stroke a shape was actually painted with, not what CSS wishes.
        stroke = page.evaluate(
            "() => { const v = document.querySelector('.djs-visual > :not(text)');"
            " return v && (v.getAttribute('stroke') || getComputedStyle(v).stroke); }"
        )
        check(
            stroke is not None and stroke.lower() not in ("#000", "#000000", "black", "rgb(0, 0, 0)"),
            f"{name}: shapes are not painted black on dark ({stroke})",
        )
        label = page.evaluate(
            "() => { const t = document.querySelector('.djs-label text, .djs-visual text');"
            " return t && (t.getAttribute('fill') || getComputedStyle(t).fill); }"
        )
        check(
            label is None or label.lower() not in ("#000", "#000000", "black", "rgb(0, 0, 0)"),
            f"{name}: labels are not painted black on dark ({label})",
        )
        SHOTS.mkdir(parents=True, exist_ok=True)
        page.screenshot(path=str(SHOTS / f"dark_{name}.png"))
        check(not problems, f"{name}: no console errors in dark mode: {problems}")
        page.close()


def check_served(browser):
    """The half `file://` cannot reach: a real server behind a real proxy.

    The editor's environment call is same-origin, so it simply does not happen
    from a file, and its CSP is never exercised there. That gap shipped a
    `connect-src 'none'` editor whose own button was blocked by its own
    policy, and a URL that resolved a path segment short — neither of which
    any other test could see. Hence this.
    """
    import demo

    if demo.psql("select 1").returncode != 0:
        skip_served("no local Postgres")
        return

    # Skipped rather than failed when the ports are taken: `just demo` runs
    # on exactly these, and a developer with the demo open in another terminal
    # should not have the test suite abort on them. demo.py keeps the hard
    # guard, because there it is the demo's own correctness at stake.
    for port in (7420, demo.PROXY_PORT):
        if demo.port_in_use(port):
            skip_served(f"port {port} is busy (is `just demo` running?)")
            return

    print("served stack (real engine, auth-injecting proxy)")
    if demo.psql(f"select 1 from pg_database where datname = '{demo.DB}'").stdout.count("1 row") == 0:
        demo.psql(f"create database {demo.DB}")
    demo.psql("drop schema public cascade; create schema public", db=demo.DB)

    env = {
        **os.environ,
        "RBPMN_DATABASE_URL": f"postgres://{os.environ.get('USER')}@localhost:5432/{demo.DB}",
        "RBPMN_API_TOKEN": demo.TOKEN,
        "RBPMN_TOPICS": "payments",
    }
    server = demo.start_server(env)
    proxy = None
    try:
        demo.wait_port(7420)
        instance = demo.build_stuck_instance()
        proxy = http.server.ThreadingHTTPServer(
            ("127.0.0.1", demo.PROXY_PORT), demo.AuthInjectingProxy
        )
        threading.Thread(target=proxy.serve_forever, daemon=True).start()
        base = f"http://localhost:{demo.PROXY_PORT}"

        page = browser.new_page()
        problems: list[str] = []
        page.on("pageerror", lambda e: problems.append(f"pageerror: {e}"))
        page.on(
            "console",
            lambda m: problems.append(m.text) if m.type == "error" else None,
        )

        page.goto(f"{base}/ui/inspect/{instance}")
        page.wait_for_selector(".djs-container", timeout=20000)
        diagnosis = page.inner_text(".diagnosis")
        check("Incident at st" in diagnosis, f"a real frozen instance diagnoses itself ({diagnosis[:60]!r})")
        # Opens on the problem rather than an empty pane.
        pane = page.inner_text(".element-pane")
        check("ServiceTask" in pane and "payments" in pane, "the element pane opens on the incident")

        # The editor's one call, under its real CSP, through a real mount
        # prefix. Both spellings, because both serve the document.
        for suffix in ("", "/"):
            page.goto(f"{base}/ui/editor{suffix}")
            page.wait_for_selector(".djs-container", timeout=20000)
            page.click("text=Check against server")
            page.wait_for_function(
                "() => document.querySelector('.environment').textContent.includes('covered by the server')",
                timeout=15000,
            )
            environment = page.inner_text(".environment")
            check("payments" in environment, f"/ui/editor{suffix} reached its API and listed topics")

        check(not problems, f"no console errors on the served pages: {problems}")
        page.close()
    finally:
        if proxy:
            # shutdown() only stops serve_forever; without server_close() the
            # listening socket stays bound for the life of the process and the
            # next run trips the port guard.
            proxy.shutdown()
            proxy.server_close()
        server.terminate()
        try:
            server.wait(timeout=10)
        except subprocess.TimeoutExpired:
            server.kill()


def main():
    from playwright.sync_api import sync_playwright

    for name in ("editor.html", "inspector.html"):
        if not (DIST / name).exists():
            print(f"{DIST / name} missing — run `just ui-dist` first")
            return 1

    with sync_playwright() as p:
        browser = p.chromium.launch()
        check_inspector(browser)
        check_editor(browser)
        check_dark_mode(browser)
        check_served(browser)
        browser.close()

    print()
    if failures:
        print(f"{len(failures)} failure(s)")
        return 1
    print("ui documents ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
