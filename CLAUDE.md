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

## Conventions

- rbpmn extension namespace `https://rbpmn.dev/schema/1.0`: `rbpmn:topic` on
  service tasks, `rbpmn:correlationKey` (JSON pointer) on message events and
  receive tasks.
- Server config is env-only (`RBPMN_BIND`, `RBPMN_API_TOKEN[_FILE]`,
  `RBPMN_ALLOW_NON_LOOPBACK`); secrets never come from CLI args.
  Security posture: `docs/http-security.md`.
