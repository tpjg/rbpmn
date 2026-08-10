// One WASM-backed check behind every rule: the moddle tree bpmnlint hands us
// is serialized back to XML (synchronously — bpmnlint rule checks are sync,
// so bpmn-moddle's async toXML is bypassed in favor of moddle-xml's Writer)
// and linted by the exact deploy-time linter compiled from rbpmn-model.
// Never a JS reimplementation of a rule: a second implementation would drift.
//
// The full lint runs once per document (WeakMap-cached on the Definitions
// node); each wrapped rule filters for its own diagnostics.
import { createRequire } from 'node:module';
import { Writer } from 'moddle-xml';

const require = createRequire(import.meta.url);
const { lint } = require('../wasm/rbpmn_wasm.js');

const cache = new WeakMap();

function diagnosticsFor(definitions) {
  let diagnostics = cache.get(definitions);
  if (!diagnostics) {
    const xml = new Writer({ format: true, preamble: true }).toXML(definitions);
    diagnostics = JSON.parse(lint(xml)).diagnostics ?? [];
    cache.set(definitions, diagnostics);
  }
  return diagnostics;
}

export function wrap(ruleId) {
  return function rule() {
    return {
      check(node, reporter) {
        if (node.$type !== 'bpmn:Definitions') return;
        for (const d of diagnosticsFor(node)) {
          if (d.rule === ruleId) {
            reporter.report(d.element, d.message);
          }
        }
      },
    };
  };
}
