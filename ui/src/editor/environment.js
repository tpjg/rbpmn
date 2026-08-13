// L3 — the only tier that needs a server, and the only one that is not the
// engine's own code.
//
// Deliberately a *set subtraction the browser performs*, not a validation
// call: the covered topic names come to the page, the model never goes to the
// server. That is what lets a confidential process be checked against a
// production environment without being uploaded to it.
//
// Kept apart from validate.js so it carries no WASM import: these are plain
// functions over plain data, and they are tested as such.

/// `covered` is `null` when no environment has been loaded. That is a
/// *different state* from "not covered" and the two must never render the
/// same way — without a server the honest answer is "unknown", never
/// "missing".
export function environmentGaps(verdict, covered) {
  if (!covered) return [];
  return Object.entries(verdict.topics ?? {})
    .filter(([, topic]) => !covered.includes(topic))
    .map(([element, topic]) => ({
      rule: 'unresolved-topic',
      element,
      severity: 'error',
      message:
        `topic '${topic}' has no registered handler and no declared ` +
        `external-worker topic on the server you checked against`,
    }));
}

/// Per-element wiring state for the pane: three states, and "unknown" is one
/// of them. Severities are never downgraded to express uncertainty — rule ids
/// and their severities are stable public API asserted by the fixture corpus,
/// so uncertainty lives here instead of in a diagnostic.
export function wiringState(topic, covered) {
  if (!covered) return 'unknown';
  return covered.includes(topic) ? 'covered' : 'missing';
}
