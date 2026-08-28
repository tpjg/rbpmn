// The playground-never-lies check: runs every fixture through the WASM build
// and through native Rust (same serialization path), comparing byte-for-byte.
// Catches stale WASM artifacts and any target-specific divergence.
//
// All three surfaces are covered. `lint` is what bpmnlint and the playground
// ask; `check_deployable` additionally drives the compile stage the editor
// relies on, so a divergence there cannot hide behind a clean lint; and the
// DMN corpus goes through `check_deployable` too, because decisions run dsntk
// — including a decimal implementation this project substituted — and are
// therefore the part with the most plausible reason to diverge between
// targets.
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadFixtures } from './gen-fixtures.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, '..', '..');

const require = createRequire(import.meta.url);
const wasm = require(join(repoRoot, 'bpmnlint-plugin-rbpmn', 'wasm', 'rbpmn_wasm.js'));

const rust = JSON.parse(
  // `--features dmn` matches what `just wasm` builds — named on both sides
  // rather than left to the default, so they cannot drift apart. Without it the two
  // sides would be different builds and the DMN corpus would compare a
  // refusal against a verdict — which the check catches, loudly, per fixture.
  execFileSync('cargo', ['run', '-q', '-p', 'rbpmn-wasm', '--features', 'dmn', '--example', 'dump-diagnostics'], {
    cwd: repoRoot,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
  })
);

const { fixtures, bindings } = loadFixtures();
let mismatches = 0;
let total = 0;

// The same minimal host process the native dump pairs every DMN artifact with.
const HOST_PROCESS = `<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL" id="defs">
  <bpmn:process id="p" isExecutable="true">
    <bpmn:startEvent id="start"><bpmn:outgoing>f1</bpmn:outgoing></bpmn:startEvent>
    <bpmn:endEvent id="end"><bpmn:incoming>f1</bpmn:incoming></bpmn:endEvent>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="end" />
  </bpmn:process>
</bpmn:definitions>`;

// Each surface pairs a JS-side call with the native dump to compare it to.
// `inputs` is what to feed it: the BPMN corpus, or the DMN corpus that the
// native dump listed (so the two sides cannot disagree about which fixtures
// exist).
const surfaces = [
  { name: 'lint', inputs: fixtures, run: (xml) => wasm.lint(xml), native: rust.lint },
  {
    name: 'check_deployable',
    inputs: fixtures,
    run: (xml, name) => wasm.check_deployable(xml, bindings[name], '[]'),
    native: rust.check,
  },
  {
    name: 'check_deployable(dmn)',
    inputs: readDmnFixtures(),
    run: (dmn) => wasm.check_deployable(HOST_PROCESS, '{}', JSON.stringify([dmn])),
    native: rust.decisions ?? {},
  },
];

function readDmnFixtures() {
  const root = join(repoRoot, 'crates', 'rbpmn-dmn', 'tests', 'fixtures');
  const out = {};
  for (const dir of ['accept', 'reject']) {
    const path = join(root, dir);
    if (!existsSync(path)) continue;
    for (const file of readdirSync(path).sort()) {
      if (!file.endsWith('.dmn')) continue;
      out[`${dir}/${file}`] = readFileSync(join(path, file), 'utf8');
    }
  }
  return out;
}

for (const { name, inputs, run, native } of surfaces) {
  if (typeof run !== 'function') {
    console.log(`PARITY BROKEN: WASM build exports no ${name} (stale build?)`);
    mismatches += 1;
    continue;
  }
  // A surface with no fixtures is not a passing surface — it is a surface
  // nobody checked. The DMN corpus in particular would silently vanish if the
  // native dump were built without the `dmn` feature.
  if (Object.keys(inputs).length === 0) {
    console.log(`PARITY BROKEN: ${name} has no fixtures to compare`);
    mismatches += 1;
    continue;
  }
  if (Object.keys(native).length !== Object.keys(inputs).length) {
    console.log(
      `fixture count mismatch for ${name}: ` +
        `rust=${Object.keys(native).length} js=${Object.keys(inputs).length}`
    );
    mismatches += 1;
  }
  for (const [fixture, source] of Object.entries(inputs)) {
    total += 1;
    if (run(source, fixture) !== native[fixture]) {
      mismatches += 1;
      console.log(
        `PARITY BROKEN: ${fixture} via ${name} — WASM and native Rust disagree (stale build?)`
      );
    }
  }
}

console.log(`${total - mismatches}/${total} checks byte-identical between native Rust and WASM`);
if (mismatches) process.exit(1);
