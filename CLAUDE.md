# rbpmn — working notes

Read `bpmn-engine-design.md` fully before changing semantics or the linter;
it records the reasoning behind every major decision (tokens-per-row, no
inclusive gateway, block structure, messages-only interaction, build order).

## Ground rules

- **Loudly reject, never silently reinterpret.** New capabilities land as
  linter rules first, with fixtures, then execution.
- **Rule IDs are stable public API.** Never rename one; add new ones. The four
  rules beyond the brief's list are documented in README.md.
- **Fixtures first.** Every phase starts with fixtures in
  `crates/rbpmn-model/tests/fixtures/{accept,reject}/`. Expected diagnostics
  are embedded in each `.bpmn` as an `expect-diagnostics:` comment; the runner
  is `tests/fixtures.rs`.
- `rbpmn-model` stays dependency-light (no IO/async/DB) — it must compile to
  WASM for the phase 0-B playground and the bpmnlint plugin.
- The engine advances tokens inside the caller's DB transaction; wait states
  are the transaction boundaries. Keep the pure `step` core IO-free (phase 1).

## Commands

- `cargo test` — everything including the fixture corpus.
- `just lint` — clippy `-D warnings` + fmt check (keep it at zero warnings).
- `just serve` — run the HTTP server with a throwaway token.
- `just playground` — linter playground (builds WASM, needs node + wasm-pack).
- `just parity` — MUST stay green: byte-parity of native Rust vs WASM lint
  output over the corpus, plus the bpmnlint plugin's pipeline test.
- `just fixtures-di` — fixtures carry baked-in BPMN DI so they render
  everywhere; new fixtures without a `bpmndi:BPMNDiagram` section get theirs
  from this (idempotent; two reject fixtures have hand-written DI — see the
  comments in them).

## Conventions

- **XML purity is a principle (Timo's explicit call — resist eroding it).**
  BPMN files are 100% standard-namespace: no rbpmn extension attributes, no
  vendor attributes, ever. It is always tempting to "just add a hint in the
  XML" — don't. Wiring lives in code at registration time (`map_topic`,
  `map_correlation`, `declare_topic`, `declare_index`) and is validated at
  deploy like a compiler validates types: fail early, never "seems to run".
- Conditions are a **strict FEEL subset** (identical syntax and semantics, so
  they stay valid when dsntk lands post-v1). Correlation keys use FEEL
  qualified names too (`order.id`), registered — not in the XML.
- Server config is env-only (`RBPMN_BIND`, `RBPMN_API_TOKEN[_FILE]`,
  `RBPMN_ALLOW_NON_LOOPBACK`); secrets never come from CLI args.
  Security posture: `docs/http-security.md`.
