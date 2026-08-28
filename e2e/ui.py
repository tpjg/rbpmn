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
import re
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


def is_dark(css_color):
    """Relative luminance below the midpoint, for a #rgb/#rrggbb token.

    Deliberately crude — the question is "would this read as a white patch on a
    dark canvas", not colour science. Anything unparseable is *not* dark, so a
    token that stops being a hex string fails loudly rather than passing by
    accident.
    """
    text = (css_color or "").strip().lstrip("#")
    if len(text) == 3:
        text = "".join(c * 2 for c in text)
    if len(text) != 6:
        return False
    try:
        r, g, b = (int(text[i : i + 2], 16) for i in (0, 2, 4))
    except ValueError:
        return False
    return (0.2126 * r + 0.7152 * g + 0.0722 * b) < 128


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
    # ...and what it was configured with. The topic says which handler ran;
    # config says what it was told, and it is in the manifest and nowhere else.
    check("acquirer-a" in pane, "element pane shows the manifest config")
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

    check_example(page)

    # Back to a cleared desk before driving the editor. Everything below
    # authors a deployment from nothing — empty manifest, `Decision1`,
    # `Decision2` — and the example would shift every one of those.
    page.click("text=New")
    page.wait_for_function(
        "() => document.querySelector('.decisions').textContent.includes('No decisions')",
        timeout=20000,
    )
    # The skeleton New lands on is deployable too, and with an empty manifest —
    # a New document that opens already rejected teaches the wrong first
    # lesson, and now that the *example* is what boot asserts, nothing else
    # here would notice if the skeleton stopped passing.
    page.wait_for_function(
        "() => (document.querySelector('.verdict')?.textContent ?? '').startsWith('valid')",
        timeout=20000,
    )
    empty = page.locator("textarea.code-manifest").input_value().strip()
    check(empty == "{}", f"New lands on a deployable skeleton needing no manifest ({empty!r})")

    # Paste an existing model through the XML pane, then confirm the linter
    # reacts and the wiring pane offers the manifest binding.
    xml = page.locator("textarea.code-xml")
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

    manifest = page.locator("textarea.code-manifest").input_value()
    check('"rt": "order.id"' in manifest, f"the manifest JSON updated ({manifest!r})")

    # Config is the one binding whose value is an object, so it gets a box of
    # its own — and the box must refuse what deploy would refuse rather than
    # storing it and letting the verdict find it later.
    page.click('.djs-element[data-element-id="st"]')
    config_box = page.locator(".wiring textarea").first
    config_box.fill('"warning_first"')
    config_box.blur()
    page.wait_for_function(
        "() => document.querySelector('.wiring .inline-error-text')?.textContent"
        ".includes('JSON object')",
        timeout=15000,
    )
    check(True, "the config box refuses a non-object without touching the manifest")
    check(
        '"config"' not in page.locator("textarea.code-manifest").input_value(),
        "a refused config never reached the manifest",
    )

    config_box.fill('{"template": "warning_first"}')
    config_box.blur()
    page.wait_for_function(
        "() => document.querySelector('textarea.code-manifest').value"
        ".includes('warning_first')",
        timeout=15000,
    )
    check(True, "config typed in the wiring pane reaches the manifest")

    # A receive task has wiring of its own (the correlation) but produces no
    # work item, so there is nothing to deliver config on and the pane does
    # not offer it — `config-binds-task` would refuse it anyway.
    page.click('.djs-element[data-element-id="rt"]')
    check(
        page.locator(".wiring textarea").count() == 0,
        "config is offered only where a work item can carry it",
    )

    # Clearing the box removes the entry, which is what makes "no config" one
    # control rather than a checkbox and a box. It also has to happen before
    # the later checks paste a different model over this one: config has no
    # default, so a key left behind pointing at an element that model does not
    # contain is `config-binds-task`, and an L2 error stops the verdict before
    # it reaches the rules those checks are about.
    page.click('.djs-element[data-element-id="st"]')
    config_box = page.locator(".wiring textarea").first
    config_box.fill("")
    config_box.blur()
    page.wait_for_function(
        "() => !document.querySelector('textarea.code-manifest').value.includes('config')",
        timeout=15000,
    )
    check(True, "clearing the config box removes the entry")

    SHOTS.mkdir(parents=True, exist_ok=True)
    page.screenshot(path=str(SHOTS / "ui_editor.png"), full_page=False)
    check_condition_repair(page)
    check_boundary_interrupting(page)
    check_decisions(page)
    check_decision_working_set(page)
    # Asserted last: check_condition_repair drives the whole condition-editing
    # flow, and anything it throws — a JS exception in reveal(), a
    # CSP-blocked request — lands in this same list. Checking before it ran
    # meant those passed silently.
    check(not problems, f"no console errors or network requests: {problems}")
    page.close()


def check_example(page):
    """The editor opens on a deployment, not on a diagram.

    Three artifacts, imported into the bundle at build time from the files
    `e2e/demo.py` deploys: the model, its manifest and its decision. Each is
    already covered on its own — the fixture corpus lints the model and
    validates the DMN — so what is asserted here is the only thing those
    cannot see, which is whether the three still agree *with each other*. The
    manifest binds a task in the model to a decision in the working set by
    name, and nothing but this checks that the name still resolves.

    That gap is the whole reason this document exists. An example shipping with
    it open would be a poor advertisement.
    """
    check(
        page.locator('.djs-element[data-element-id="triage"]').count() == 1,
        "the example's business-rule task is on the canvas",
    )
    names = page.eval_on_selector_all(
        ".decision-list .btn-link", "els => els.map(e => e.textContent.trim())"
    )
    check(names == ["triage.dmn"], f"the example bundles its decision ({names})")

    manifest = page.locator("textarea.code-manifest").input_value()
    check('"decision": "Triage"' in manifest, f"the manifest binds the task ({manifest!r})")
    check('"risk-scoring"' in manifest, f"the manifest carries the service topics ({manifest!r})")

    # The bound name resolving against the bundled artifact is decidable
    # offline — a deployment's decisions travel inside it — so this is a real
    # verdict with no server involved, and the assertion that fails first if
    # the manifest and the DMN ever drift apart.
    diagnostics = page.inner_text(".diagnostics")
    check(
        "unresolved-decision" not in diagnostics and "decision-has-binding" not in diagnostics,
        f"the example's decision binding resolves ({diagnostics!r})",
    )

    # And it evaluates: the try-it pane runs the engine's own evaluator, so
    # this is the table the demo instance ran, answering here.
    page.wait_for_function(
        "() => document.querySelector('.try-it select') !== null", timeout=15000
    )
    page.click("text=Evaluate")
    page.wait_for_function(
        "() => (document.querySelector('.try-output')?.textContent ?? '') !== ''",
        timeout=15000,
    )
    answer = page.inner_text(".try-output")
    # The real answer, not merely that something came back: at a risk score of
    # 82 the table's first rule matches. A decision that cannot see its
    # declared input answers null instead, and a null is a legal answer — so
    # nothing but an assertion on the value would report it.
    check(
        answer.strip() == '"review"',
        f"the bundled table evaluates against the default input ({answer!r})",
    )
    # fit-viewport anchors a model at the viewport's top-left, and the palette
    # floats over exactly that corner — the example's start event rendered
    # underneath it, on the first screen, before `fitClearOfPalette`. It is
    # geometry, so it is checkable rather than a screenshot someone has to
    # remember to look at.
    box = page.evaluate(
        "() => {"
        " const r = e => document.querySelector(e).getBoundingClientRect();"
        " const canvas = r('.djs-container'), palette = r('.djs-palette');"
        " const first = r('.djs-element[data-element-id=\"placed\"]');"
        " const last = r('.djs-element[data-element-id=\"abandoned\"]');"
        " return {clear: first.left >= palette.right,"
        "         inside: last.right <= canvas.right && first.top >= canvas.top}; }"
    )
    check(box["clear"], "the model is fitted clear of the palette")
    check(box["inside"], "making room for the palette did not push the model off-canvas")

    SHOTS.mkdir(parents=True, exist_ok=True)
    page.screenshot(path=str(SHOTS / "ui_editor_example.png"), full_page=False)


def check_decisions(page):
    """Authoring the decision half of a deployment, in the same window.

    The loop this closes: a business-rule task binds a decision by name, and
    without an editor that knows both halves the name is a guess. Everything
    here happens with no server — a deployment's DMN artifacts travel inside
    it, so the verdict on them is complete offline.
    """
    # Start from a model with a business-rule task, pasted through the XML
    # pane the way the earlier checks do.
    xml = page.locator("textarea.code-xml")
    xml.fill(
        (REPO / "crates/rbpmn-model/tests/fixtures/accept/25-business-rule-task.bpmn").read_text()
    )
    page.wait_for_function(
        "() => document.querySelectorAll('.djs-element[data-element-id=\"decide\"]').length > 0",
        timeout=15000,
    )

    # With nothing bundled, the task cannot deploy: it has no binding, and
    # there is no decision to bind it to.
    page.wait_for_function(
        "() => document.querySelector('.diagnostics').textContent"
        ".includes('decision-has-binding')",
        timeout=15000,
    )
    check(True, "a business-rule task with no binding is refused")

    # Author a decision. dmn-js is a second modeler on a second canvas; the
    # editor switches rather than showing both.
    page.click("text=New decision")
    # dmn-js opens on the DRD view: its own container, with a drill-down
    # overlay per decision leading into the table/expression editors.
    page.wait_for_selector(".dmn-js-parent .dmn-drd-container", timeout=20000)
    check(page.locator(".dmn-js-parent .djs-container").count() >= 1,
          "the decision canvas rendered")
    check(page.locator(".drill-down-overlay").count() >= 1,
          "the DRD offers drill-down into the decision's logic")
    page.screenshot(path=str(SHOTS / "ui_editor_decision.png"), full_page=False)

    # The Element pane reads a bpmn-js selection, so on this canvas it can only
    # be stale or empty — clicking a DMN shape leaves it saying whatever the
    # process canvas last said, with nothing to indicate that it is not
    # answering. It is hidden here rather than left inert.
    element_pane = page.locator("details.pane:has(.properties)")
    check(not element_pane.is_visible(), "the Element pane is gone on the decision canvas")
    check(
        page.locator("details.pane:has(.try-it)").is_visible(),
        "the panes that do apply to a decision are still there",
    )

    check_definitions_header(page)

    # Back to the process: the new decision is now bindable *by name*, which
    # is the thing no server was asked about.
    page.click("text=Process")
    check(element_pane.is_visible(), "the Element pane comes back with the process canvas")
    page.click('.djs-element[data-element-id="decide"]')
    page.wait_for_function(
        "() => document.querySelector('.wiring').textContent.includes('bundled decisions')",
        timeout=15000,
    )
    check(True, "the wiring pane offers the bundled decision")

    inputs = page.locator(".wiring input")
    inputs.nth(0).fill("Decision1")
    inputs.nth(0).blur()
    inputs.nth(1).fill("order.discount")
    inputs.nth(1).blur()
    page.wait_for_function(
        "() => !document.querySelector('.diagnostics').textContent"
        ".includes('decision-has-binding') && "
        "!document.querySelector('.diagnostics').textContent"
        ".includes('unresolved-decision')",
        timeout=15000,
    )
    check(True, "binding the decision clears both decision diagnostics")

    manifest = page.locator("textarea.code-manifest").input_value()
    check('"decision": "Decision1"' in manifest, f"the manifest carries the binding ({manifest!r})")

    # And the try-it pane runs the same evaluator the engine runs.
    page.wait_for_function(
        "() => document.querySelector('.try-it select') !== null", timeout=15000
    )
    page.click("text=Evaluate")
    page.wait_for_function(
        "() => (document.querySelector('.try-output')?.textContent ?? '') !== ''",
        timeout=15000,
    )
    answer = page.inner_text(".try-output")
    # A *literal expression* over `order.total` from its declared input, where
    # `check_example`'s is a decision table — the two halves of dmn-js, both
    # answering from the same default document, which is why they had to agree
    # on an input name. Asserting the real answer rather than a constant is the
    # point: a decision that cannot see its input answers `null`, which is
    # exactly the trap a starter with no `<inputData>` walked users into.
    check(
        answer.strip() == '"large"',
        f"the try-it pane evaluated the decision against its input ({answer!r})",
    )


DEBOUNCE_MS = 300
"""dmn-js's definitions header debounces its commit by this much."""


def check_definitions_header(page):
    """Typing into the DRD's definitions header — name and id, top left.

    Two upstream dmn-js bugs, both fixed in `ui/src/editor/dmn-definitions-
    header.js`, and both needing a *pause* to show up. That is why they were
    reported by a human and not by this file: every other check here types at
    full speed, under the 300 ms debounce, where the header behaves perfectly.

    So these deliberately type slowly. It costs a few seconds and it is the
    only reason a dmn-js upgrade that reintroduces either one would be caught
    before someone tries to rename a decision.
    """
    name = page.locator(".dmn-js-parent .dmn-definitions-name")
    canvas = page.locator(".dmn-js-parent .dmn-drd-container")

    def retype(locator, text, delay_ms):
        locator.click()
        page.keyboard.press("ControlOrMeta+A")
        page.keyboard.press("Delete")
        page.wait_for_timeout(DEBOUNCE_MS + 200)
        for char in text:
            page.keyboard.type(char)
            page.wait_for_timeout(delay_ms)

    # 1. Slower than the debounce, so a commit lands between each keystroke.
    # `update()` rewrote the field's text node every time, which collapses the
    # caret to offset 0 — so the next character went to the front and "Slow"
    # came out "wolS".
    retype(name, "Slow", DEBOUNCE_MS + 150)
    typed = name.inner_text()
    check(typed == "Slow", f"typing slowly into the definitions name reads forward ({typed!r})")

    # 2. ...and it reached the model, not just the screen. Blurring makes the
    # header reconcile itself *from* the business object, so a field that still
    # says "Slow" after a blur is one whose edit was actually committed. No
    # export needed: the component's own redraw is the assertion.
    canvas.click(position={"x": 40, "y": 40})
    page.wait_for_timeout(DEBOUNCE_MS + 400)
    kept = name.inner_text()
    check(kept == "Slow", f"the slow-typed name survives a blur, so the model has it ({kept!r})")

    # 3. Faster than the debounce, then click away inside it. `blur` used to
    # reconcile the field from the *stale* model and the pending commit then
    # read back the value it had just reverted — the edit vanished silently.
    name.click()
    page.keyboard.press("ControlOrMeta+A")
    page.keyboard.press("Delete")
    page.wait_for_timeout(DEBOUNCE_MS + 200)
    page.keyboard.type("Kept", delay=10)
    canvas.click(position={"x": 40, "y": 40})
    page.wait_for_timeout(DEBOUNCE_MS + 400)
    survived = name.inner_text()
    check(survived == "Kept", f"an edit finished inside the debounce is not lost ({survived!r})")

    # The id field shares the same code path and must not have been touched by
    # any of it — `update()` writes both fields on every commit.
    element_id = page.inner_text(".dmn-js-parent .dmn-definitions-id")
    check(
        element_id == "_Decision1",
        f"editing the name left the definitions id alone ({element_id!r})",
    )


def check_decision_working_set(page):
    """The working set — where an editor loses work without saying anything.

    None of this is visible from the pure-module tests: it lives in the
    interaction between one dmn-js instance, an array of artifacts, and an
    index into that array. Every check here failed before the fix, and failed
    *silently* — the editor stayed on screen looking correct with the wrong
    content in it.

    The observable throughout is the try-it pane's invocable list, because it
    is derived from the compiled DMN rather than from the name beside it in
    the list: a decision whose XML was overwritten keeps its file name and
    changes its invocable.
    """
    page.click("text=New decision")
    page.wait_for_function(
        "() => document.querySelectorAll('.decision-list li').length === 2",
        timeout=20000,
    )
    # Wait for the *verdict* to catch up, not just the list. The invocable
    # list below is the observable, and it lags a render behind — asserting on
    # it before the second decision registered would read the previous verdict
    # and pass whatever happened next.
    page.wait_for_function(
        "() => document.querySelectorAll('.try-it select option').length === 2",
        timeout=20000,
    )
    before = page.eval_on_selector_all(
        ".try-it select option", "els => els.map(e => e.textContent)"
    )
    check(before == ["Decision1", "Decision2"],
          f"both decisions are bundled before the removal ({before})")

    # Show the first, then remove it *while it is on screen*. On the way out
    # the editor captures the canvas back into the working set — and it used
    # to capture by index, after the array had already been filtered, so the
    # removed decision's XML landed on whichever artifact inherited its index.
    page.click(".decision-list li:nth-child(1) .btn-link")
    page.wait_for_function(
        "() => document.querySelector('.decision-list li:nth-child(1)')"
        ".classList.contains('on')",
        timeout=20000,
    )
    page.click(".decision-list li:nth-child(1) button:text-is('Remove')")
    page.wait_for_function(
        "() => document.querySelectorAll('.decision-list li').length === 1",
        timeout=20000,
    )
    page.wait_for_function(
        "() => document.querySelectorAll('.try-it select option').length === 1",
        timeout=20000,
    )
    names = page.eval_on_selector_all(
        ".try-it select option", "els => els.map(e => e.textContent)"
    )
    check(
        names == ["Decision2"],
        f"removing the decision on screen leaves the other one intact ({names})",
    )

    # "New" empties the manifest, so it means a new *deployment* — and the
    # decisions are part of one. They used to survive it and travel into the
    # next bundle.
    page.click("text=New")
    page.wait_for_function(
        "() => document.querySelector('.decisions').textContent"
        ".includes('No decisions')",
        timeout=20000,
    )
    check(True, "a new diagram starts with no decisions carried over")

    # ...and the canvas goes with them. Forgetting the artifact without
    # clearing the canvas left dmn-js still rendering the discarded decision,
    # fully editable, while every keystroke in it went nowhere — nothing was
    # "current", so nothing was captured.
    page.click("text=Decisions")
    page.wait_for_timeout(500)
    leftover = page.locator(".dmn-js-parent .djs-element").count()
    check(leftover == 0,
          f"the discarded decision is off the canvas too ({leftover} elements)")
    page.click("text=Process")

    # A bundle whose `bindings` is an object but not a manifest passes the
    # bundle shape check and fails the manifest parse. That threw past the
    # guard, after the file name had already been replaced: an unhandled
    # rejection, no diagnostic, and an editor left half-loaded.
    bad = REPO / "e2e" / "screenshots" / "bad.bundle.json"
    bad.parent.mkdir(parents=True, exist_ok=True)
    bad.write_text('{"bpmn": "<definitions/>", "bindings": {"topics": 5}}')
    with page.expect_file_chooser() as fc:
        page.click("text=Open bundle")
    fc.value.set_files(str(bad))
    page.wait_for_function(
        "() => document.querySelector('.diagnostics').textContent.includes('bundle')",
        timeout=20000,
    )
    check(True, "a bundle with a malformed manifest is refused, not half-loaded")
    # ...and the editor still works afterwards, which is the half an unhandled
    # rejection takes away.
    page.click("text=New decision")
    page.wait_for_function(
        "() => document.querySelectorAll('.decision-list li').length === 1",
        timeout=20000,
    )
    check(True, "the editor is still usable after a rejected bundle")

    # A decision that will not parse must not be validated as the *previous*
    # one. `currentXml` reads the canvas, and a failed import leaves the canvas
    # on the artifact before it — so substituting the live XML by index put
    # good bytes in the broken file's place and the verdict came back clean,
    # for a bundle the server refuses. The verdict split, from the side the
    # editor exists to prevent.
    broken = REPO / "e2e" / "screenshots" / "broken.dmn"
    broken.write_text("<definitions this is not xml at all")
    with page.expect_file_chooser() as fc:
        page.click("text=Open .dmn")
    fc.value.set_files(str(broken))
    page.wait_for_function(
        "() => document.querySelectorAll('.decision-list li').length === 2",
        timeout=20000,
    )
    # Give the debounced re-check its turn — the bug showed up *after* it ran,
    # not before, which is what made it survive the first look.
    page.wait_for_timeout(1500)
    verdict = page.inner_text(".verdict")
    diagnostics = page.inner_text(".diagnostics")
    check(
        "valid" not in verdict.lower() or "dmn" in diagnostics.lower(),
        f"an unparseable decision is not reported as deployable "
        f"(verdict={verdict!r}, diagnostics={diagnostics!r})",
    )
    broken.unlink()
    bad.unlink()


def check_condition_repair(page):
    """The loop a modeler with a broken split actually walks.

    A gateway whose branches use full FEEL produces one
    `conditions-feel-subset` error per branch. Reading them is useless unless
    clicking one puts the offending flow in the Element pane — `focus` alone
    marks and scrolls without selecting, which left the diagnostics pointing
    at something the panes never showed.
    """
    print("editor.html — repairing a broken split")
    xml = page.locator("textarea.code-xml")
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


SHOWS_INTERRUPTING = (
    "() => document.querySelector('.properties').textContent.includes('interrupting')"
)
"""The Element pane has rendered a boundary event's interrupting row."""


def prop_select(page, label):
    """The <select> in the Element pane row whose label is exactly `label`."""
    return page.locator(
        ".properties label.prop",
        has=page.locator(f"span.prop-label:text-is('{label}')"),
    ).locator("select")


def check_boundary_interrupting(page):
    """Interrupting or not — the standard `cancelActivity` attribute.

    Three things only a browser can answer, and all three are places this
    could go wrong silently. `cancelActivity` is a double negative whose
    schema default is *true*, so "interrupting" is spelled by the attribute
    being **absent**: a pane that wrote `cancelActivity="true"` would still
    read back correctly and still be wrong, because every other tool spells
    it by omission. bpmn-js redraws the dashed double circle from the same
    attribute, which is the only part of this a modeller actually looks at.
    And an error boundary has no choice to offer — an error always cancels
    the activity it escaped from — so the row must state that rather than
    offer a control whose two answers do not both exist.
    """
    print("editor.html — the interrupting toggle")
    xml = page.locator("textarea.code-xml")
    xml.fill(
        (REPO / "crates/rbpmn-model/tests/fixtures/accept/29-message-boundary.bpmn").read_text()
    )
    page.wait_for_function(
        "() => document.querySelectorAll("
        "'.djs-element[data-element-id=\"paid_during_contest\"]').length > 0",
        timeout=15000,
    )
    page.click('.djs-element[data-element-id="paid_during_contest"]')
    page.wait_for_function(SHOWS_INTERRUPTING, timeout=15000)

    toggle = prop_select(page, "interrupting")
    check(toggle.count() == 1, "a boundary event offers the interrupting row")
    check(
        toggle.input_value() == "yes",
        f"an absent cancelActivity reads as interrupting ({toggle.input_value()!r})",
    )
    dashed = (
        "() => document.querySelector('.djs-element"
        "[data-element-id=\"paid_during_contest\"] .djs-visual')"
        ".outerHTML.includes('stroke-dasharray')"
    )
    check(not page.evaluate(dashed), "and is drawn as the solid double circle")

    toggle.select_option("no")
    page.wait_for_function(
        "() => document.querySelector('textarea.code-xml').value"
        ".includes('cancelActivity=\"false\"')",
        timeout=15000,
    )
    check(True, 'choosing no writes the standard cancelActivity="false"')
    page.wait_for_function(dashed, timeout=15000)
    check(True, "bpmn-js redraws the boundary as the dashed double circle")
    page.screenshot(path=str(SHOTS / "ui_editor_boundary.png"), full_page=False)

    # Back again. The attribute must *go*, not become `cancelActivity="true"`:
    # bpmn-moddle omits a value equal to the schema default, and this is what
    # asserts that it still does.
    toggle.select_option("yes")
    page.wait_for_function(
        "() => !document.querySelector('textarea.code-xml').value.includes('cancelActivity')",
        timeout=15000,
    )
    check(True, "choosing yes removes the attribute rather than writing true")
    check(not page.evaluate(dashed), "and the solid double circle comes back")

    # An error boundary: the answer is BPMN's, so the pane says so instead of
    # offering a control.
    xml.fill(
        (REPO / "crates/rbpmn-model/tests/fixtures/accept/10-error-boundary.bpmn").read_text()
    )
    page.wait_for_function(
        "() => document.querySelectorAll('.djs-element[data-element-id=\"be\"]').length > 0",
        timeout=15000,
    )
    page.click('.djs-element[data-element-id="be"]')
    page.wait_for_function(SHOWS_INTERRUPTING, timeout=15000)
    pane = page.inner_text(".properties")
    check(prop_select(page, "interrupting").count() == 0, "an error boundary offers no toggle")
    check("always yes" in pane, f"and says why it cannot be anything else ({pane!r})")


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

        # The libraries' own chrome, which `shared/theme.js` cannot reach — it
        # only sets renderer options. These are the surfaces a night-time bug
        # report named: a white palette, an unreadable context pad, and the
        # loud one, a near-white `--shape-drop-allowed-fill-color` painted
        # across the whole canvas while dragging a pool.
        #
        # Read from the *computed* value rather than asserting a stylesheet
        # exists, because the failure mode was not a missing rule: the rule was
        # there and correct, and lost on source order to a second copy of
        # diagram-js.css that dmn-js ships.
        chrome = page.evaluate(
            """(keys) => {
              const el = document.querySelector('.djs-parent');
              if (!el) return null;
              const cs = getComputedStyle(el);
              return Object.fromEntries(keys.map(k => [k, cs.getPropertyValue(k).trim()]));
            }""",
            [
                "--shape-drop-allowed-fill-color",
                "--palette-background-color",
                "--context-pad-entry-background-color",
                "--popup-background-color",
            ],
        )
        check(chrome is not None, f"{name}: the diagram root exposes its colour tokens")
        for token, value in (chrome or {}).items():
            check(
                is_dark(value),
                f"{name}: {token} is dark on a dark canvas ({value})",
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


def check_svg_export(browser):
    """The Export SVG button, driven in DARK mode on purpose.

    bpmn-js bakes stroke and fill as SVG *attributes* at construction, so the
    canvas in front of you exports the theme it was built with — and a
    dark-mode export is near-invisible on white paper, which is the one thing
    the button exists for. `svg-export.js` renders through a second, detached
    viewer with the light palette rather than restyling the live canvas, and
    this is the test that the choice actually holds end to end: right colours
    out, and the canvas you were working on untouched.
    """
    print("svg export")
    page = browser.new_page(color_scheme="dark")
    problems = collect_problems(page)
    page.goto((DIST / "editor.html").as_uri())
    page.wait_for_selector(".djs-container", timeout=20000)
    page.wait_for_timeout(1500)

    # The canvas under test really is dark, or nothing below proves anything.
    # Read the *computed* stroke: bpmn-js writes its palette into an inline
    # `style`, not into `stroke`/`fill` attributes, so getAttribute is null.
    live = page.evaluate(
        "() => { const v = document.querySelector('.djs-visual > :not(text)');"
        " return v && getComputedStyle(v).stroke; }"
    )
    check(
        live is not None and live.replace(" ", "") == "rgb(201,207,218)",
        f"the canvas being exported from is dark ({live})",
    )
    # A marker on the live container: if the export had restyled the canvas by
    # re-constructing the modeler, this node would be replaced and the marker
    # would be gone — which is `remountForTheme`'s documented cost, the undo
    # history, and the reason the export does not take that route.
    page.evaluate(
        "() => { document.querySelector('.canvas .djs-container').dataset.probe = 'kept'; }"
    )

    with page.expect_download() as pending:
        page.click("text=Export SVG")
    downloaded = pending.value
    check(
        downloaded.suggested_filename.endswith(".svg"),
        f"downloaded as an svg ({downloaded.suggested_filename})",
    )
    svg = Path(downloaded.path()).read_text(encoding="utf-8")

    check(svg.startswith("<?xml"), "a standalone svg document, not a fragment")
    check("<svg" in svg and "</svg>" in svg, "with a closed root element")
    check("data-element-id" in svg, "carrying the model's elements")
    check('fill="#ffffff"' in svg, "and its own white paper")
    # The whole point: the viewer's dark palette must not have travelled.
    #
    # Compared with the spaces stripped, and in `rgb()` rather than hex,
    # because that is how bpmn-js actually writes a palette — into an inline
    # `style` attribute. Asserting on the hex strings looked fine and proved
    # nothing: they never appear in the file at all, so the check passed
    # whatever the export contained.
    compact = re.sub(r"\s+", "", svg)
    for label, rgb in (
        ("fill", "rgb(29,32,38)"),
        ("stroke", "rgb(201,207,218)"),
        ("label", "rgb(231,233,238)"),
    ):
        check(rgb not in compact, f"no dark-palette {label} reached the export ({rgb})")
    check("rgb(22,24,29)" in compact, "painted with the light palette instead")
    check("rgb(255,255,255)" in compact, "on light fills")
    # Overlays live outside the SVG layer, so diagnostics stay out for free.
    check("rbpmn-badge" not in svg, "diagnostic badges stayed out of the export")

    kept = page.evaluate(
        "() => document.querySelector('.canvas .djs-container')?.dataset.probe"
    )
    check(kept == "kept", "the live canvas was not re-constructed to do it")
    after = page.evaluate(
        "() => { const v = document.querySelector('.djs-visual > :not(text)');"
        " return v && getComputedStyle(v).stroke; }"
    )
    check(after == live, f"and it is still the theme the user chose ({after})")
    check(not problems, f"no console errors while exporting: {problems}")
    page.close()


def check_print_layout(browser):
    """The print stylesheets, under emulated print media.

    The complaint they answer is concrete: browser print takes the whole page,
    and the editor's side column — diagnostics, wiring, the XML pane — was
    getting more of the paper than the model it describes. Asserted rather
    than eyeballed because a print rule is invisible in normal use: it can rot
    for months and nobody notices until they print.
    """
    print("print layout")
    for name in ("editor", "inspector"):
        page = browser.new_page()
        problems = collect_problems(page)
        page.goto((DIST / f"{name}.html").as_uri())
        page.wait_for_selector(".djs-container", timeout=20000)
        page.wait_for_timeout(1000)

        on_screen = page.evaluate(
            "() => getComputedStyle(document.querySelector('.side')).display"
        )
        check(on_screen != "none", f"{name}: the side column is there on screen")

        page.emulate_media(media="print")
        hidden = page.evaluate(
            "() => getComputedStyle(document.querySelector('.side')).display"
        )
        check(hidden == "none", f"{name}: the side column gives up the paper ({hidden})")

        # The canvas has to become a real block: it is `position: absolute;
        # inset: 0` inside a relative wrapper on screen, which prints as
        # nothing once the grid it depended on is gone.
        canvas = page.evaluate(
            """() => {
              const c = document.querySelector('.canvas');
              const cs = getComputedStyle(c);
              return { position: cs.position, height: c.getBoundingClientRect().height };
            }"""
        )
        check(
            canvas["position"] == "static" and canvas["height"] > 200,
            f"{name}: the diagram gets the page ({canvas})",
        )

        if name == "editor":
            toolbar = page.evaluate(
                "() => getComputedStyle(document.querySelector('.toolbar')).display"
            )
            check(toolbar == "none", f"editor: the toolbar is not printed ({toolbar})")
        else:
            # The inspector keeps what identifies the printout and the sentence
            # someone reads first; only the panes go.
            kept = page.evaluate(
                """() => ['.topbar', '.diagnosis'].map(
                     s => { const e = document.querySelector(s);
                            return e ? getComputedStyle(e).display : 'missing'; })"""
            )
            check(
                all(d not in ("none", "missing") for d in kept),
                f"inspector: heading and diagnosis survive the print rules ({kept})",
            )

        page.emulate_media(media="screen")
        check(not problems, f"{name}: no console errors: {problems}")
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
        # `charge` is the demo model's payment task — see
        # `accept/28-demo-order.bpmn`. This half shares `e2e/demo.py`'s stack
        # deliberately, so the model the demo shows off is the model this
        # asserts on, and a change to one cannot quietly diverge from the other.
        check(
            "Incident at charge" in diagnosis,
            f"a real frozen instance diagnoses itself ({diagnosis[:60]!r})",
        )
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
        check_svg_export(browser)
        check_print_layout(browser)
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
