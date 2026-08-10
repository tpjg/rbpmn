// The playground-never-lies check: lints every fixture through the WASM build
// and through native Rust (same serialization path), comparing byte-for-byte.
// Catches stale WASM artifacts and any target-specific divergence.
import { execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { loadFixtures } from './gen-fixtures.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, '..', '..');

const require = createRequire(import.meta.url);
const { lint } = require(join(repoRoot, 'bpmnlint-plugin-rbpmn', 'wasm', 'rbpmn_wasm.js'));

const rust = JSON.parse(
  execFileSync('cargo', ['run', '-q', '-p', 'rbpmn-wasm', '--example', 'dump-diagnostics'], {
    cwd: repoRoot,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
  })
);

const { fixtures } = loadFixtures();
let mismatches = 0;

if (Object.keys(rust).length !== Object.keys(fixtures).length) {
  console.log(
    `fixture count mismatch: rust=${Object.keys(rust).length} js=${Object.keys(fixtures).length}`
  );
  mismatches += 1;
}
for (const [name, xml] of Object.entries(fixtures)) {
  if (lint(xml) !== rust[name]) {
    mismatches += 1;
    console.log(`PARITY BROKEN: ${name} — WASM and native Rust disagree (stale build?)`);
  }
}

const total = Object.keys(fixtures).length;
console.log(`${total - mismatches}/${total} fixtures byte-identical between native Rust and WASM`);
if (mismatches) process.exit(1);
