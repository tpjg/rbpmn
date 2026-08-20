// Gate 0b: the dsntk DMN stack, running in WebAssembly.
//
// Built with wasm-pack exactly as `rbpmn-wasm` already is, so this is the
// production path rather than a contrivance: parse a DMN document, build its
// evaluators, and evaluate a decision table — all inside the VM.
import { createRequire } from 'node:module';
import { readFileSync } from 'node:fs';
const require = createRequire(import.meta.url);
const wasm = require('./pkg/rbpmn_dmn_wasm_probe.js');

const size = readFileSync('./pkg/rbpmn_dmn_wasm_probe_bg.wasm').length;
console.log(`module size   : ${(size / 1024 / 1024).toFixed(2)} MiB (wasm-opt'd release)`);

let failures = 0;
const check = (label, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failures++;
  console.log(`  ${ok ? 'ok  ' : 'FAIL'} ${label.padEnd(38)} got ${JSON.stringify(got)}`);
};

console.log('\nparse + build evaluators (the deploy-time check):');
check('compile() finds the invocable', wasm.compile(), 1);

console.log('\nevaluate the decision table (< 100 -> 0, >= 100 -> Amount * 0.1):');
for (const amount of [50, 99, 100, 250, 1000]) {
  check(`evaluate(${amount})`, wasm.evaluate(amount) / 100, amount < 100 ? 0 : amount * 0.1);
}

console.log('\na broken document must report, not panic:');
// A well-formed DMN document whose FEEL does not parse. This is the P1
// premise: `ModelEvaluator::new` is where every expression in the model is
// parsed, so building the evaluator *is* the deploy-time FEEL check.
const badFeel = `<?xml version="1.0" encoding="UTF-8"?>
<definitions namespace="https://rbpmn.example" name="bad" id="_bad"
             xmlns="https://www.omg.org/spec/DMN/20191111/MODEL/">
  <decision name="Broken" id="_broken">
    <literalExpression><text>1 +</text></literalExpression>
  </decision>
</definitions>`;
const err = wasm.compile_error(badFeel);
check('bad FEEL is reported', err !== 'ok' && err.length > 0, true);
console.log(`       -> ${err.split('\n')[0].slice(0, 100)}`);
check('not XML at all is reported', wasm.compile_error('<not-dmn') !== 'ok', true);

console.log(failures === 0 ? '\nGate 0b: PASS' : `\nGate 0b: ${failures} FAILURES`);
process.exit(failures === 0 ? 0 : 1);
