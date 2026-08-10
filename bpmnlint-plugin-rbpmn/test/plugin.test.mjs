// End-to-end through bpmnlint's real pipeline: every corpus fixture is
// imported with bpmn-moddle, linted via the plugin's WASM-backed rules, and
// the reports are compared against the fixture's embedded expect-diagnostics
// — the same expectations the Rust test suite asserts.
import { readFileSync, readdirSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { BpmnModdle } from 'bpmn-moddle';
import plugin from '../index.js';

const require = createRequire(import.meta.url);
const Linter = require('bpmnlint/lib/linter.js');
const StaticResolver = require('bpmnlint/lib/resolver/static-resolver.js');

const here = dirname(fileURLToPath(import.meta.url));
const fixturesRoot = join(here, '..', '..', 'crates', 'rbpmn-model', 'tests', 'fixtures');
const moddle = new BpmnModdle();

const ruleConfigs = plugin.configs.recommended.rules;
const staticRules = {};
for (const qualified of Object.keys(ruleConfigs)) {
  const name = qualified.replace('rbpmn/', '');
  const mod = await import(`../rules/${name}.js`);
  staticRules[`rule:bpmnlint-plugin-rbpmn/${name}`] = mod.default;
}
const linter = new Linter({ resolver: new StaticResolver(staticRules) });
const config = { rules: ruleConfigs };

function parseExpectations(xml) {
  const start = xml.indexOf('expect-diagnostics:');
  const rest = xml.slice(start + 'expect-diagnostics:'.length);
  return rest
    .slice(0, rest.indexOf('-->'))
    .split('\n')
    .map((l) => l.trim())
    .filter(Boolean)
    .map((line) => {
      const [severity, rule, , element] = line.split(/\s+/);
      return `${severity} ${rule} @ ${element}`;
    })
    .sort();
}

// Known limitation of every moddle-based tool (bpmnlint included): bpmn-moddle
// silently repairs duplicate ids on import, so this fixture's defect never
// reaches the linter here. Deploy reads the raw XML and does reject it — the
// plugin is a preview, deploy is the authority.
const MODDLE_BLIND_SPOTS = new Set(['reject/duplicate-id.bpmn']);

let failures = 0;
let total = 0;

for (const dir of ['accept', 'reject']) {
  for (const file of readdirSync(join(fixturesRoot, dir))
    .filter((f) => f.endsWith('.bpmn'))
    .sort()) {
    const name = `${dir}/${file}`;
    if (MODDLE_BLIND_SPOTS.has(name)) continue;
    total += 1;
    const xml = readFileSync(join(fixturesRoot, dir, file), 'utf8');
    const expected = parseExpectations(xml);

    const { rootElement } = await moddle.fromXML(xml);
    const results = await linter.lint(rootElement, config);

    const actual = [];
    for (const [qualified, reports] of Object.entries(results)) {
      const rule = qualified.replace('rbpmn/', '');
      const severity = ruleConfigs[qualified] === 'warn' ? 'warn' : 'error';
      for (const report of reports) {
        actual.push(`${severity} ${rule} @ ${report.id}`);
      }
    }
    actual.sort();

    if (JSON.stringify(actual) !== JSON.stringify(expected)) {
      failures += 1;
      console.log(`MISMATCH ${name}`);
      console.log(`  expected: ${JSON.stringify(expected)}`);
      console.log(`  actual:   ${JSON.stringify(actual)}`);
    }
  }
}

console.log(`${total - failures}/${total} fixtures match through the bpmnlint pipeline`);
if (failures) process.exit(1);
