// The validator: tiers L1 and L2, both of them the engine's own code.
//
//   L1  model-only lint              — rbpmn-model
//   L2  model + manifest + decisions  — rbpmn-core::check_deployable
//
// Compiled to wasm32 from the same crates deploy uses, so the editor never
// reimplements a rule and cannot reach a different verdict. L3 — are those
// topics covered by a running environment — lives in environment.js, because
// it is the one tier that needs a server and the only one that is not shared
// code.

import init, { check_deployable, evaluate_decision } from '../../wasm/rbpmn_wasm.js';
import { WASM_BASE64 } from '../generated/wasm-inline.js';

let ready = null;

function wasmBytes() {
  const binary = atob(WASM_BASE64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

/// Async, not `initSync`: Chrome refuses synchronous WebAssembly compilation
/// of buffers over 4KB on the main thread, and the linter is ~400KB.
export function initValidator() {
  ready ??= init({ module_or_path: wasmBytes() });
  return ready;
}

/// L1 + L2. Returns the parsed verdict from the WASM boundary.
///
/// `ok` there means "deploy would proceed to the environment link" — never
/// "deploy would succeed". Reporting it as success would skip
/// `unresolved-topic`, which is precisely the wiring gap the manifest exists
/// to make visible.
///
/// Decisions are the exception, and the reason this tier is worth having:
/// topics need a server to resolve, but a deployment's DMN artifacts travel
/// inside it, so `unresolved-decision` and `feel-deterministic` are answered
/// completely here. A confidential decision table is validated without
/// leaving the browser.
export function checkModel(xml, bindings, decisions = []) {
  return JSON.parse(
    check_deployable(xml, JSON.stringify(bindings), JSON.stringify(decisions))
  );
}

/// Run one decision against sample input, for the try-it pane.
///
/// The same evaluator the engine runs, so what the pane shows is what an
/// instance would get. A `null` outcome is an *answer*, not an error — dsntk
/// cannot distinguish a legal "no rule matched" from a broken evaluation, so
/// the pane shows the reason as explanation rather than as a verdict
/// (docs/dmn.md, "What P1 measured").
export function evaluateDecision(decisions, invocable, input) {
  return JSON.parse(
    evaluate_decision(
      JSON.stringify(decisions),
      invocable.namespace,
      invocable.model,
      invocable.name,
      input
    )
  );
}
