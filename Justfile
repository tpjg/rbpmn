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

# Bake DI into any fixture that lacks a BPMNDiagram section (idempotent).
fixtures-di:
    cd playground && npm install && node scripts/add-di.mjs
