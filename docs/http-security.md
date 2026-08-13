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

That last exclusion survives the arrival of the two UI documents, which is
worth stating plainly because it looks like it should not. The documents are
**documents**, not API clients: the inspector carries its data inlined and
issues no requests at all, and the editor's one optional call is same-origin
by construction. Nothing about them asks this API to become browser-friendly.
What they do add is a *rendering* surface for business data, and that has its
own section below.

## Controls (implemented)

**Authentication** — every `/v1` route requires `Authorization: Bearer <token>`.
The surface now includes the full engine API (deploy, start, complete/fail,
topic declaration, instance inspection) — all privileged; deploy remains the
most privileged of all (deploy = code). The playground's inspection view
reaches the server through its own dev proxy, so the server still never emits
CORS headers.

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
- **Audit**: the append-only `rbpmn_event` table is the audit log; request ids from
  the HTTP layer land in event payloads for end-to-end correlation.

## Embedding the UI documents

`rbpmn-ui` renders two self-contained HTML documents: a read-only instance
**inspector** and the model+manifest **editor**. Both inline everything they
need — one stylesheet, one script, and for the inspector one JSON data block —
so they work from `file://` and never fetch a subresource.

The inspector inlines the instance's data rather than calling an API. That is
a security decision, not a packaging one: there is no second request to
protect, so **"can this person reach this path" is the entire access
decision** — which is the only way "the application handles authorization"
can be literally true.

### What rbpmn guarantees

- **Escaping.** Business data — variable documents, work-item failure
  messages, element names — is escaped for the HTML `<script>` data block by
  replacing `<`, `>`, `&`, U+2028 and U+2029 with their JSON `\uXXXX` forms.
  Those characters only ever occur inside JSON string literals, so the value
  round-trips exactly while `</script`, `<!--` and `<![CDATA[` become
  unrepresentable. Everything else is rendered through the DOM with
  `textContent`, never `innerHTML`. Tested against a hostile-payload corpus in
  `crates/rbpmn-ui/tests/documents.rs`.
  - Escaping is **not** sanitization, and rbpmn does no sanitization: the
    inspector shows the whole variable document by design. Deciding *who may
    see it* is the application's call — see redaction below.
- **A policy the document carries.** Each document emits its own
  `<meta http-equiv="Content-Security-Policy">`:

  ```
  default-src 'none'; script-src 'sha256-…'; style-src 'unsafe-inline';
  img-src data:; font-src data:; connect-src 'none'; base-uri 'none';
  form-action 'none'
  ```

  `connect-src 'none'` is the one worth reading twice: the document **cannot
  phone home** — no fetch, no XHR, no WebSocket, no beacon. Every other fetch
  directive is `'none'` or `data:`, so `style-src 'unsafe-inline'` (which
  diagram-js needs) is not an exfiltration channel either: CSS `url()` loads
  resolve under `img-src`/`font-src` and have nowhere to go. Script execution,
  the part that would matter, stays hash-pinned.
  - Only the **editor** carries `'wasm-unsafe-eval'`, because only the editor
    compiles the linter. The inspector renders an already-deployed model and
    ships no validator.
- **Response headers**, when served through `rbpmn-ui`'s routers:
  `Cache-Control: no-store, max-age=0`, `X-Content-Type-Options: nosniff`.
- **No credentials in the browser.** Neither document reads, stores or sends a
  token. The editor's optional environment call is a plain same-origin GET
  whose URL it derives from its own location, so it never needs to know what
  prefix it was mounted under.

### What the application must do

1. **Authenticate and authorize the viewer.** rbpmn does not, and the routers
   deliberately carry no auth of their own. On the standalone server they sit
   behind the same bearer as `/v1`, which a browser cannot send on a top-level
   navigation — that is intentional, not an oversight.
2. **Frame it in a sandbox.** Serve the inspector inside

   ```html
   <iframe sandbox="allow-scripts" src="/bpmn-inspector/instance/<id>">
   ```

   `allow-scripts` **without** `allow-same-origin`. The two together are the
   footgun — a page granted both can remove its own sandbox. Alone,
   `allow-scripts` gives the document an opaque origin, so business data
   rendered inside it cannot reach the application's cookies or storage.
3. **Set `frame-ancestors` by header.** A meta CSP cannot express it, and only
   the application knows which of its origins may frame the page.
4. **Never proxy `/v1` to a browser audience.** Deploy is code. Expose the UI
   paths and nothing else.
5. **Treat a saved document as a data extract.** The flip side of an artifact
   you can attach to a support ticket is an uncontrolled copy of business
   data. `no-store` covers the cache; it does not cover a downloaded file.

### Redaction

There is no redaction feature and there will not be one. The library boundary
is a *value*, not an endpoint — `render_inspection(&InstanceInspection)` — so
an application that must not show variables to tier-1 support edits the struct
before rendering:

```rust
let mut inspection = engine.inspect_instance(id).await?;
if !viewer.may_see_business_data() {
    inspection.variables = serde_json::json!({ "redacted": true });
}
let html = rbpmn_ui::render_inspection(&inspection);
```

That door is open for free and costs this crate nothing to keep open, which is
the entire argument for it being the only redaction on offer.

## Operator checklist

1. Generate tokens: `openssl rand -hex 32`; distribute via secret manager.
2. Keep the default loopback bind unless a TLS proxy is in place; then set
   `RBPMN_ALLOW_NON_LOOPBACK=true` and firewall the port to the proxy.
3. Rotate by appending a new token, migrating clients, removing the old one.
4. Point log shipping at stdout; `Authorization` is already redacted.
5. If the UI documents are exposed to humans, re-read "Embedding the UI
   documents" above: the sandboxed iframe and `frame-ancestors` are the
   application's job, and nothing in rbpmn will notice if they are missing.
