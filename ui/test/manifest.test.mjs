// The manifest is the artifact the editor exists to author: it is written
// next to the .bpmn in a repository and travels with it to deploy. So its
// parsing is strict (a shape rbpmn would reject must not look accepted here)
// and its serialization is stable (a diff should show what changed, not the
// order a map happened to iterate in).

import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  emptyManifest,
  orphanedBindings,
  parseManifest,
  serializeManifest,
  setBinding,
} from '../src/editor/manifest.js';

test('an empty document is an empty manifest, not an error', () => {
  assert.deepEqual(parseManifest(''), emptyManifest());
  assert.deepEqual(parseManifest('   \n '), emptyManifest());
});

test('missing groups are filled in, present ones preserved', () => {
  const parsed = parseManifest('{"topics":{"st":"payments"}}');
  assert.deepEqual(parsed, { topics: { st: 'payments' }, correlations: {}, indexes: [] });
});

// Quietly repairing a manifest here would produce the exact failure this
// editor exists to prevent: something that looks accepted and then fails at
// deploy.
test('shapes rbpmn would reject are refused rather than repaired', () => {
  assert.throws(() => parseManifest('[]'), /JSON object/);
  assert.throws(() => parseManifest('"a"'), /JSON object/);
  assert.throws(() => parseManifest('{"topics":[]}'), /topics/);
  assert.throws(() => parseManifest('{"topics":{"st":3}}'), /topics\.st/);
  assert.throws(() => parseManifest('{"correlations":{"c":null}}'), /correlations\.c/);
  assert.throws(() => parseManifest('{"indexes":"a"}'), /indexes/);
  assert.throws(() => parseManifest('{"indexes":[1]}'), /indexes/);
  assert.throws(() => parseManifest('{"topic":{}}'), /unknown manifest key\(s\): topic/);
});

test('serialization is stable and drops empty groups', () => {
  const manifest = { topics: { b: 'two', a: 'one' }, correlations: {}, indexes: [] };
  assert.equal(serializeManifest(manifest), '{\n  "topics": {\n    "a": "one",\n    "b": "two"\n  }\n}\n');
  assert.equal(serializeManifest(emptyManifest()), '{}\n');
});

test('serialize/parse round-trips', () => {
  const manifest = {
    topics: { st: 'payments' },
    correlations: { rt: 'order.id' },
    indexes: ['order.status'],
  };
  assert.deepEqual(parseManifest(serializeManifest(manifest)), manifest);
});

// An unmapped service task runs on a topic named after its element id, so
// "empty" has to mean the default rather than a topic called "".
test('clearing a binding removes it so the default applies again', () => {
  let manifest = setBinding(emptyManifest(), 'topics', 'st', 'payments');
  assert.equal(manifest.topics.st, 'payments');
  manifest = setBinding(manifest, 'topics', 'st', '   ');
  assert.equal('st' in manifest.topics, false);
  manifest = setBinding(manifest, 'topics', 'st', null);
  assert.equal('st' in manifest.topics, false);
});

test('setBinding does not mutate the manifest it was given', () => {
  const before = emptyManifest();
  const after = setBinding(before, 'topics', 'st', 'payments');
  assert.deepEqual(before.topics, {});
  assert.notEqual(before, after);
});

test('values are trimmed, because a topic with a stray space is a wiring gap', () => {
  const manifest = setBinding(emptyManifest(), 'topics', 'st', '  payments  ');
  assert.equal(manifest.topics.st, 'payments');
});

// Deploy accepts these — an entry binding nothing binds nothing — but they
// are nearly always a rename that lost its other half.
test('bindings pointing at absent elements are reported', () => {
  const manifest = {
    topics: { st: 'payments', gone: 'ghosts' },
    correlations: { alsoGone: 'order.id' },
    indexes: [],
  };
  assert.deepEqual(orphanedBindings(manifest, ['st', 'start']), [
    { group: 'topics', elementId: 'gone' },
    { group: 'correlations', elementId: 'alsoGone' },
  ]);
  assert.deepEqual(orphanedBindings(manifest, ['st', 'gone', 'alsoGone']), []);
});
