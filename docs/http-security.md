# rbpmn-server — HTTP security

The server is a thin wrapper around the engine library for deployments that
don't embed Rust. It is a **control plane for business state**: deploying a
definition decides what code-adjacent behavior runs (which topics execute,
where the future HttpPostHandler posts), and runtime endpoints mutate process
instances and can read variable documents. Treat every endpoint as privileged.

## Threat model

In scope: an attacker who can reach the port (credential guessing, sniffing on
non-TLS paths, oversized/slow requests, secrets leaking into logs), and
injection through deployed models (SQL via generated per-definition indexes,
SSRF via handler URLs).

Out of scope by design: multi-tenant isolation (one deployment = one trust
domain), DoS beyond basic limits (a reverse proxy owns rate limiting), and
browser-facing concerns — this is a service API; no CORS headers are ever
emitted, so browsers cannot call it cross-origin.

## Controls (implemented)

**Authentication** — every `/v1` route requires `Authorization: Bearer <token>`.

- Tokens come from `RBPMN_API_TOKEN` (comma-separated, enabling rotation:
  accept old+new, flip clients, drop old) or `RBPMN_API_TOKEN_FILE` (one per
  line, `#` comments) — never from CLI args, which leak via `ps`.
- Minimum 32 characters enforced at startup (`openssl rand -hex 32`).
- Stored and compared as SHA-256 hashes with a constant-time, non-short-
  circuiting membership test (`subtle`): no timing oracle on token bytes or on
  which of several tokens matched.
- Uniform `401` + `WWW-Authenticate: Bearer` with no detail about why.

**Transport** — the server speaks plain HTTP and binds `127.0.0.1:7420` by
default. A non-loopback bind is **refused at startup** unless
`RBPMN_ALLOW_NON_LOOPBACK=true`, the operator's explicit statement that a
TLS-terminating reverse proxy (nginx/caddy/envoy — which can also add mTLS or
OIDC) sits in front. Direct TLS via rustls is a possible later feature; it is
deliberately not the default posture.

**Request hygiene**

- Body limit 5 MiB (deploy payloads are BPMN XML; nothing legitimate is bigger).
- 30 s request timeout (408 on expiry).
- `Authorization` is marked sensitive so tracing/logging never records it.
- `x-request-id` is generated (UUID) and propagated for audit correlation.
- Application-level errors return generic JSON (`{"error": "..."}`) — no stack
  traces, no internal paths. Framework-enforced limits (413 body too large,
  408 timeout, 400 invalid UTF-8) return plain-text or empty bodies: clients
  must switch on status code, never on body shape. Lint diagnostics are data,
  not error leakage: they describe the caller's own document.
- Unauthenticated `/healthz` returns a static status only — no version, no
  build info. Everything under `/v1` — including unknown paths — answers 401
  before 404, so anonymous callers cannot probe which routes exist.

## Controls (committed for later phases)

- **SQL injection via definitions**: per-definition partial indexes embed the
  definition key as a **literal** in generated DDL/queries (a planner
  requirement, see the design brief). Therefore definition keys must match
  `[a-z][a-z0-9-]{0,63}` at deploy time — validated before any SQL is
  generated. Everything else is bound parameters (sqlx).
- **SSRF**: `HttpPostHandler` targets come from operator configuration at
  engine build time, never from request data or model content.
- **Scoped tokens**: deploy (code-adjacent) vs runtime (start/correlate/
  complete) privileges as distinct token sets, once those endpoints exist in
  phase 2/3. The `Tokens` type is already a set to make this a small step.
- **Idempotency**: completing an already-completed work item is a distinct
  no-op result in the engine contract, so retried HTTP calls cannot
  double-advance state.
- **Audit**: the append-only `event` table is the audit log; request ids from
  the HTTP layer land in event payloads for end-to-end correlation.

## Operator checklist

1. Generate tokens: `openssl rand -hex 32`; distribute via secret manager.
2. Keep the default loopback bind unless a TLS proxy is in place; then set
   `RBPMN_ALLOW_NON_LOOPBACK=true` and firewall the port to the proxy.
3. Rotate by appending a new token, migrating clients, removing the old one.
4. Point log shipping at stdout; `Authorization` is already redacted.
