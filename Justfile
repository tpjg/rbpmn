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

# The playground-never-lies check (Rust vs WASM byte parity) + the bpmnlint
# plugin's corpus test. Run before releasing anything WASM-facing.
parity: wasm
    cd playground && npm install && npm run parity
    cd bpmnlint-plugin-rbpmn && npm install && npm test

# TLA+ model checking of the concurrency protocol (spec/README.md). Needs
# java; fetches tla2tools.jar into spec/ on first use (gitignored). Two of the
# four configs are EXPECTED to fail — they are the counterexamples that show
# the checks have teeth.
tla:
    #!/usr/bin/env bash
    set -euo pipefail
    cd spec
    jar=.tla2tools.jar
    if [ ! -f "$jar" ]; then
        echo "fetching tla2tools.jar ..."
        curl -fsSL -o "$jar" https://github.com/tlaplus/tlaplus/releases/latest/download/tla2tools.jar
    fi
    check() { # name, config, module, expectation(hold|fail)
        local out
        if out=$(java -cp "$jar" tlc2.TLC -config "$2" "$3" 2>&1); then
            if [ "$4" = "hold" ]; then
                echo "  ok       $1"
            else
                echo "  MISSING  $1 — expected a counterexample, TLC found none"; return 1
            fi
        else
            if [ "$4" = "fail" ]; then
                echo "  ok       $1 (counterexample found, as expected)"
            else
                echo "  BROKEN   $1"; echo "$out" | tail -40; return 1
            fi
        fi
    }
    check "lock order: shipped protocol"      LockOrder.cfg           LockOrder.tla hold
    check "lock order: rejected AB/BA sketch" LockOrderHistorical.cfg LockOrder.tla fail
    check "lease: safety"                     Lease.cfg               Lease.tla     hold
    check "lease: double belief is reachable" Lease_DoubleBelief.cfg  Lease.tla     fail

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
