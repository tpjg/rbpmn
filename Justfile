default: test

test:
    cargo test

lint:
    cargo clippy --all-targets -- -D warnings
    cargo fmt --check

fmt:
    cargo fmt

# Run the HTTP server with a fresh throwaway token (printed for curl use).
serve:
    #!/usr/bin/env bash
    set -euo pipefail
    psql -h localhost postgres -tc "select 1 from pg_database where datname = 'rbpmn_dev'" | grep -q 1 || psql -h localhost postgres -c "create database rbpmn_dev"
    export RBPMN_DATABASE_URL="postgres://$USER@localhost:5432/rbpmn_dev"
    export RBPMN_API_TOKEN=$(openssl rand -hex 32)
    echo "token: $RBPMN_API_TOKEN"
    cargo run -p rbpmn-server

# Build the WASM linter: web target (playground) + node target (bpmnlint plugin).
wasm:
    wasm-pack build crates/rbpmn-wasm --target web --out-dir ../../playground/src/wasm --no-typescript
    wasm-pack build crates/rbpmn-wasm --target nodejs --out-dir ../../bpmnlint-plugin-rbpmn/wasm --no-typescript

# Linter playground: fixture browser + live lint at http://localhost:5173.
playground: wasm
    cd playground && npm install && npm run dev

# Build the two UI documents into crates/rbpmn-ui/assets/. Build output, so
# gitignored: run this once after cloning, and again after touching ui/ or the
# linter crates the editor embeds. rbpmn-ui's build.rs says so if you forget.
ui:
    cd ui && npm install && npm run build

# The UI's own unit tests: the pure modules (diagnosis, manifest parsing,
# model facts, the L3 subtraction), no browser and no build artifacts needed.
ui-test:
    cd ui && npm install && npm test

# Write both documents to disk so they can be opened directly — the editor is
# usable with no server at all, and this is how you check that.
ui-dist:
    cargo run -p rbpmn-ui --example write-documents

# Browser checks for the two documents, driven from file://. The Rust tests
# pin their structure and escaping; none of that notices a blank canvas, so
# this opens the real files and drives them. Also the only place the CSP is
# enforced for real. Needs python3 + playwright.
e2e-ui: ui-dist
    python3 e2e/ui.py

# The playground-never-lies check (Rust vs WASM byte parity) + the bpmnlint
# plugin's corpus test. Run before releasing anything WASM-facing.
parity: wasm
    cd playground && npm install && npm run parity
    cd bpmnlint-plugin-rbpmn && npm install && npm test

# TLA+ model checking of the concurrency protocol (spec/README.md). Needs
# java; fetches tla2tools.jar into spec/ on first use (gitignored). The
# version is pinned and the download is checksum-verified before java ever
# runs it: an unpinned `releases/latest` would silently change what executes
# on a developer's machine and would make past hold/fail verdicts
# irreproducible. To move to a new TLA+ release, bump both constants — the
# recipe refuses to run on a mismatch rather than trusting the download.
# Six of the eleven configs are EXPECTED to fail — they are the
# counterexamples that show the checks have teeth, and two of them
# reproduce bugs that were real (the AB/BA timer-claim sketch, and the
# phase-6 scope teardown that left a timer row behind).
tla:
    #!/usr/bin/env bash
    set -euo pipefail
    cd spec
    version=v1.7.4
    sha=936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88
    jar=.tla2tools-$version.jar
    verify() { # jar -> 0 when it matches the pinned checksum
        local got
        got=$(shasum -a 256 "$1" | cut -d' ' -f1)
        [ "$got" = "$sha" ] || { echo "  checksum mismatch for $1"; echo "    expected $sha"; echo "    got      $got"; return 1; }
    }
    if [ ! -f "$jar" ]; then
        echo "fetching tla2tools.jar $version ..."
        curl -fsSL -o "$jar.part" \
            "https://github.com/tlaplus/tlaplus/releases/download/$version/tla2tools.jar"
        # Verify BEFORE the file is usable: never leave an unverified jar
        # where the next run would find it and skip the check.
        verify "$jar.part" || { rm -f "$jar.part"; exit 1; }
        mv "$jar.part" "$jar"
    fi
    verify "$jar" || exit 1
    # name, config, module, hold|fail, expected-error regex (fail only), [TLC flags]
    #
    # A non-zero exit is NOT enough to call a counterexample: TLC also exits
    # non-zero when a spec or config fails to parse (151 on a semantic error).
    # Treating that as "fails as expected" is how an expected-fail config rots
    # silently while `just tla` stays green — so the failure must match the
    # specific violation it is there to demonstrate.
    check() {
        local out
        if out=$(java -cp "$jar" tlc2.TLC ${6:-} -config "$2" "$3" 2>&1); then
            if [ "$4" = "hold" ]; then
                echo "  ok       $1"
            else
                echo "  MISSING  $1 — expected a counterexample, TLC found none"; return 1
            fi
        elif [ "$4" = "fail" ]; then
            if grep -qE "$5" <<<"$out"; then
                echo "  ok       $1 (counterexample found, as expected)"
            else
                echo "  WRONG    $1 — TLC failed, but not with /$5/:"
                echo "$out" | grep -E "^Error" | head -5
                return 1
            fi
        else
            echo "  BROKEN   $1"; echo "$out" | tail -40; return 1
        fi
    }
    check "lock order: shipped protocol"      LockOrder.cfg           LockOrder.tla hold ""
    check "lock order: rejected AB/BA sketch" LockOrderHistorical.cfg LockOrder.tla fail "Error: Deadlock reached"
    check "lock order: all five per-instance rows"  LockOrderAllRows.cfg           LockOrder.tla hold ""
    check "lock order: five rows, wrong order"      LockOrderAllRowsHistorical.cfg LockOrder.tla fail "Error: Deadlock reached"
    check "lease: safety"                     Lease.cfg               Lease.tla     hold ""
    check "lease: double belief is reachable" Lease_DoubleBelief.cfg  Lease.tla     fail "Invariant DoubleBeliefIsReachable is violated"
    # -deadlock: a terminal state is legitimate here (everything torn down,
    # nothing armed). Deadlock freedom is a property under test only for
    # LockOrder, where the flag is deliberately absent.
    check "timer claim vs scope teardown"     TimerTeardown.cfg       TimerTeardown.tla hold "" -deadlock
    check "teardown leaving a timer behind"   TimerTeardown_Buggy.cfg TimerTeardown.tla fail "Invariant NeverFiredADanglingTimer is violated" -deadlock
    check "retention: floor and the archive gap" Retention.cfg              Retention.tla hold "" -deadlock
    check "retention: floor from the plan"       Retention_FloorFromPlan.cfg Retention.tla fail "Invariant FloorIsSomethingDeleted is violated" -deadlock
    check "retention: no DUE re-check"           Retention_NoRecheck.cfg     Retention.tla fail "Invariant OnlyDueRecordsDeleted is violated" -deadlock

# Differential the FEEL subset against dsntk (the DMN-TCK-verified reference):
# every condition we accept must evaluate identically there. Outside the
# workspace and not part of `test` — dsntk pulls ~170 crates and a C library.
feel-parity:
    cd feel-parity && cargo test

# Bake DI into any fixture that lacks a BPMNDiagram section (idempotent).
fixtures-di:
    cd playground && npm install && node scripts/add-di.mjs

# Browser end-to-end tests with automatic screenshots (e2e/screenshots/,
# gitignored): every fixture rendered, plus the full inspection stack when a
# local Postgres is reachable. Needs python3 + playwright
# (pip install playwright && playwright install chromium).
e2e:
    cd playground && npm install
    python3 e2e/run.py
