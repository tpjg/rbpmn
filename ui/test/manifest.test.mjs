// The manifest is the artifact the editor exists to author: it is written
// next to the .bpmn in a repository and travels with it to deploy. So its
// parsing is strict (a shape rbpmn would reject must not look accepted here)
// and its serialization is stable (a diff should show what changed, not the
// order a map happened to iterate in).

import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  binding,
  emptyManifest,
  formatConfig,
  formatIndexField,
  orphanedBindings,
  parseIndexField,
  parseManifest,
  serializeManifest,
  setBinding,
  setConfigBinding,
  setDecisionBinding,
} from '../src/editor/manifest.js';

test('an empty document is an empty manifest, not an error', () => {
  assert.deepEqual(parseManifest(''), emptyManifest());
  assert.deepEqual(parseManifest('   \n '), emptyManifest());
});

test('missing groups are filled in, present ones preserved', () => {
  const parsed = parseManifest('{"topics":{"st":"payments"}}');
  assert.deepEqual(parsed, {
    topics: { st: 'payments' },
    correlations: {},
    indexes: [],
    decisions: {},
    config: {},
  });
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
  assert.throws(() => parseManifest('{"config":[]}'), /"config" must be an object/);
  assert.throws(() => parseManifest('{"config":{"st":"warning"}}'), /"config\.st"/);
  assert.throws(() => parseManifest('{"config":{"st":[1]}}'), /"config\.st"/);
  assert.throws(() => parseManifest('{"topic":{}}'), /unknown manifest key\(s\): topic/);
});

test('serialization is stable and drops empty groups', () => {
  const manifest = { topics: { b: 'two', a: 'one' }, correlations: {}, indexes: [] };
  assert.equal(serializeManifest(manifest), '{\n  "topics": {\n    "a": "one",\n    "b": "two"\n  }\n}\n');
  assert.equal(serializeManifest(emptyManifest()), '{}\n');
});

test('serialize/parse round-trips', () => {
  // Index entries are normalized to {field, scope} in memory and written back
  // in the narrowest spelling that carries the meaning, so the on-disk form of
  // a definition-scoped entry is still the bare string it has always been.
  const manifest = {
    topics: { st: 'payments' },
    correlations: { rt: 'order.id' },
    indexes: [{ field: 'order_no', scope: 'shared' }, { field: 'status', scope: 'definition' }],
    decisions: { brt: { decision: 'Discount', result: 'order.discount' } },
    config: { st: { template: 'warning_first', copies: 2 } },
  };
  assert.deepEqual(parseManifest(serializeManifest(manifest)), manifest);
  // Sorted by field, and the definition-scoped entry stays a bare string.
  assert.match(serializeManifest(manifest), /"scope": "shared"\n\s+\},\n\s+"status"/);
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

// A business-rule task binds two things — which decision, and where the answer
// lands — so its manifest entries are objects rather than strings.
test('decision bindings round-trip and are validated as a pair', () => {
  const manifest = parseManifest(
    '{"decisions":{"brt":{"decision":"Discount","result":"order.discount"}}}'
  );
  assert.deepEqual(manifest.decisions.brt, {
    decision: 'Discount',
    result: 'order.discount',
  });

  for (const bad of [
    '{"decisions":{"brt":"Discount"}}',
    '{"decisions":{"brt":{"decision":"Discount"}}}',
    '{"decisions":{"brt":{"decision":"Discount","result":""}}}',
    '{"decisions":{"brt":{"decision":"D","result":"r","extra":1}}}',
    '{"decisions":[]}',
  ]) {
    assert.throws(() => parseManifest(bad), undefined, bad);
  }
});

// Half a binding is not a binding deploy would accept, and half of one in the
// file is worse than none — but typing one field must not erase the other.
test('a half-written decision binding never reaches the file', () => {
  let manifest = setDecisionBinding(emptyManifest(), 'brt', 'decision', 'Discount');
  assert.equal(manifest.decisions.brt.decision, 'Discount');
  assert.equal(serializeManifest(manifest), '{}\n');

  manifest = setDecisionBinding(manifest, 'brt', 'result', 'order.discount');
  assert.match(serializeManifest(manifest), /"decisions"/);

  // Clearing either half takes the whole entry out of the file again.
  const cleared = setDecisionBinding(manifest, 'brt', 'result', '');
  assert.equal(serializeManifest(cleared), '{}\n');
});

test('a decision binding on a vanished element is reported as orphaned', () => {
  const manifest = setDecisionBinding(
    setDecisionBinding(emptyManifest(), 'gone', 'decision', 'D'),
    'gone',
    'result',
    'a.b'
  );
  assert.deepEqual(orphanedBindings(manifest, ['still-here']), [
    { group: 'decisions', elementId: 'gone' },
  ]);
});

test('index declarations carry a scope, in either spelling', () => {
  const parsed = parseManifest(
    '{"indexes":["channel",{"field":"order_no","scope":"shared"}]}'
  );
  assert.deepEqual(parsed.indexes, [
    { field: 'channel', scope: 'definition' },
    { field: 'order_no', scope: 'shared' },
  ]);
  // The default is written back as the bare string it has always been, so an
  // existing manifest round-trips byte for byte; only `shared` widens.
  assert.equal(
    serializeManifest(parsed),
    '{\n  "indexes": [\n    "channel",\n    {\n      "field": "order_no",\n      "scope": "shared"\n    }\n  ]\n}\n'
  );
  assert.deepEqual(parseManifest(serializeManifest(parsed)), parsed);
  // Spelling the default the long way is the same wiring.
  assert.deepEqual(
    parseManifest('{"indexes":[{"field":"channel","scope":"definition"}]}').indexes,
    [{ field: 'channel', scope: 'definition' }]
  );
});

test('index shapes rbpmn would reject are refused rather than repaired', () => {
  assert.throws(() => parseManifest('{"indexes":"a"}'), /indexes/);
  assert.throws(() => parseManifest('{"indexes":[1]}'), /indexes/);
  assert.throws(() => parseManifest('{"indexes":[{}]}'), /field/);
  assert.throws(() => parseManifest('{"indexes":[{"field":"f","scoop":"x"}]}'), /scoop/);
  assert.throws(
    () => parseManifest('{"indexes":[{"field":"f","scope":"sharded"}]}'),
    /definition or shared/
  );
});

test('the field box round-trips a scope through one line of text', () => {
  assert.deepEqual(parseIndexField('order_no:shared'), {
    field: 'order_no',
    scope: 'shared',
  });
  assert.deepEqual(parseIndexField('channel'), { field: 'channel', scope: 'definition' });
  assert.equal(formatIndexField({ field: 'channel', scope: 'definition' }), 'channel');
  assert.equal(formatIndexField({ field: 'order_no', scope: 'shared' }), 'order_no:shared');
  assert.equal(formatIndexField('channel'), 'channel');
  assert.throws(() => parseIndexField('f:sharded'), /definition or shared/);
});

// Config is free JSON: rbpmn never looks inside one, so neither does this.
// The only shape checked is the one `config-binds-task` checks.
test('a config entry is stored verbatim, at any depth', () => {
  const value = { template: 'warning_first', copies: 2, cc: ['a', { b: null }] };
  const manifest = setConfigBinding(emptyManifest(), 'st', JSON.stringify(value));
  assert.deepEqual(manifest.config.st, value);
  assert.deepEqual(parseManifest(serializeManifest(manifest)).config.st, value);
});

test('an empty box removes the entry, so "no config" is one control', () => {
  let manifest = setConfigBinding(emptyManifest(), 'st', '{"template":"a"}');
  assert.equal('st' in manifest.config, true);
  manifest = setConfigBinding(manifest, 'st', '   \n ');
  assert.equal('st' in manifest.config, false);
  assert.equal(formatConfig(manifest, 'st'), '');
});

// The box must not be able to write a shape deploy would refuse: a failed
// commit shows the reason and leaves the manifest alone.
test('the config box refuses what deploy would refuse', () => {
  assert.throws(() => setConfigBinding(emptyManifest(), 'st', '{oops'), /not valid JSON/);
  assert.throws(() => setConfigBinding(emptyManifest(), 'st', '"warning_first"'), /JSON object/);
  assert.throws(() => setConfigBinding(emptyManifest(), 'st', '[1, 2]'), /JSON object/);
  assert.throws(() => setConfigBinding(emptyManifest(), 'st', 'null'), /JSON object/);
});

test('setConfigBinding does not mutate the manifest it was given', () => {
  const before = emptyManifest();
  const after = setConfigBinding(before, 'st', '{"template":"a"}');
  assert.deepEqual(before.config, {});
  assert.notEqual(before, after);
});

// Deploy rejects a stale config key (`config-binds-task`), so the verdict
// already names it with the element highlighted. Reporting it here too would
// say the same thing twice, once as a rule and once as a hunch.
test('config is not in the orphan warning, because the rule covers it', () => {
  const manifest = { ...emptyManifest(), config: { gone: { template: 'a' } } };
  assert.deepEqual(orphanedBindings(manifest, ['st']), []);
});

// `__proto__` is a valid NCName, so bpmn-js will hand it out as an element id.
// Writing it with `=` calls the Object.prototype setter: nothing is stored,
// nothing serializes, and the read comes back off the prototype chain — so the
// box looks saved while the manifest is empty.
test('an element called __proto__ is a key, not a prototype write', () => {
  const manifest = setConfigBinding(emptyManifest(), '__proto__', '{"template":"a"}');
  assert.deepEqual(Object.keys(manifest.config), ['__proto__']);
  assert.equal(formatConfig(manifest, '__proto__'), '{\n  "template": "a"\n}');
  assert.equal(serializeManifest(manifest).includes('"template": "a"'), true);
  assert.deepEqual(parseManifest(serializeManifest(manifest)).config.__proto__, { template: 'a' });
});

test('and an unconfigured __proto__ reads as unconfigured, not as a prototype', () => {
  assert.equal(formatConfig(emptyManifest(), '__proto__'), '');
  assert.equal(binding(emptyManifest(), 'topics', '__proto__'), undefined);
  const topics = setBinding(emptyManifest(), 'topics', '__proto__', 'payments');
  assert.equal(binding(topics, 'topics', '__proto__'), 'payments');
});
