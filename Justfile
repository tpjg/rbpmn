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
