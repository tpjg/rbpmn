// The playground-never-lies check: runs every fixture through the WASM build
// and through native Rust (same serialization path), comparing byte-for-byte.
// Catches stale WASM artifacts and any target-specific divergence.
//
// Both exports are covered. `lint` is what bpmnlint and the playground ask;
// `check_deployable` additionally drives the compile stage the editor relies
// on, so a divergence there cannot hide behind a clean lint.
import { execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadFixtures } from './gen-fixtures.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, '..', '..');

const require = createRequire(import.meta.url);
const wasm = require(join(repoRoot, 'bpmnlint-plugin-rbpmn', 'wasm', 'rbpmn_wasm.js'));

const rust = JSON.parse(
  execFileSync('cargo', ['run', '-q', '-p', 'rbpmn-wasm', '--example', 'dump-diagnostics'], {
    cwd: repoRoot,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
  })
);

const { fixtures } = loadFixtures();
let mismatches = 0;

// Each export is `(xml) -> string`, paired with the native dump to compare to.
const exports = [
  { name: 'lint', run: (xml) => wasm.lint(xml), native: rust.lint },
  { name: 'check_deployable', run: (xml) => wasm.check_deployable(xml, '{}'), native: rust.check },
];

for (const { name, run, native } of exports) {
  if (typeof run !== 'function' || run.length === undefined) {
    console.log(`PARITY BROKEN: WASM build exports no ${name} (stale build?)`);
    mismatches += 1;
    continue;
  }
  if (Object.keys(native).length !== Object.keys(fixtures).length) {
    console.log(
      `fixture count mismatch for ${name}: ` +
        `rust=${Object.keys(native).length} js=${Object.keys(fixtures).length}`
    );
    mismatches += 1;
  }
  for (const [fixture, xml] of Object.entries(fixtures)) {
    if (run(xml) !== native[fixture]) {
      mismatches += 1;
      console.log(
        `PARITY BROKEN: ${fixture} via ${name} — WASM and native Rust disagree (stale build?)`
      );
    }
  }
}

const total = Object.keys(fixtures).length * exports.length;
console.log(`${total - mismatches}/${total} checks byte-identical between native Rust and WASM`);
if (mismatches) process.exit(1);
