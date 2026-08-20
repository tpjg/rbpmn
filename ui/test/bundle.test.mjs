// The bundle is the deploy body. What matters here is that it round-trips and
// that it refuses what deploy would refuse — an editor that exports something
// the server rejects is worse than one that never exported at all.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  buildBundle,
  bundleNameFor,
  decisionFileName,
  parseBundle,
} from '../src/editor/bundle.js';

const BPMN = '<definitions id="d"><process id="p"/></definitions>';
const DMN = '<definitions xmlns="https://www.omg.org/spec/DMN/20191111/MODEL/" name="pricing"/>';

test('a bundle round-trips through build and parse', () => {
  const bundle = buildBundle({
    bpmn: BPMN,
    manifest: '{"topics":{"st":"payments"}}',
    decisions: [{ name: 'pricing.dmn', xml: DMN }],
  });
  const back = parseBundle(bundle);
  assert.equal(back.bpmn, BPMN);
  assert.deepEqual(JSON.parse(back.manifest), { topics: { st: 'payments' } });
  assert.equal(back.decisions.length, 1);
  assert.equal(back.decisions[0].xml, DMN);
});

// A bundle carrying `"bindings": {}` reads as though somebody meant something
// by it. Absent is the honest spelling of nothing.
test('empty parts are left out rather than written empty', () => {
  const bundle = JSON.parse(buildBundle({ bpmn: BPMN, manifest: '{}', decisions: [] }));
  assert.deepEqual(Object.keys(bundle), ['bpmn']);
});

test('shapes deploy would reject are refused rather than repaired', () => {
  for (const bad of [
    '[]',
    '"a"',
    '{}',
    '{"bpmn":""}',
    '{"bpmn":"x","decisions":"one"}',
    '{"bpmn":"x","decisions":[1]}',
    '{"bpmn":"x","bindings":[]}',
    '{"bpmn":"x","extra":1}',
  ]) {
    assert.throws(() => parseBundle(bad), undefined, bad);
  }
});

// What lands in a repository should be recognisable, not `decision-2.dmn`.
test('a decision file is named after the model', () => {
  assert.equal(decisionFileName(DMN), 'pricing.dmn');
  assert.equal(decisionFileName('<definitions dmn:name="x"/>', 3), 'decision-4.dmn');
  assert.equal(
    decisionFileName('<definitions name="Loan / Approval!"/>'),
    'Loan-Approval.dmn'
  );
  // A name made entirely of separators must not produce a dotfile.
  assert.equal(decisionFileName('<definitions name="///"/>', 0), 'decision-1.dmn');
});

test('the bundle keeps the name of the process it deploys', () => {
  assert.equal(bundleNameFor('orders.bpmn'), 'orders.bundle.json');
  assert.equal(bundleNameFor(''), 'process.bundle.json');
});
