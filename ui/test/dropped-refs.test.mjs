// A mistyped errorRef must not come back from the editor as a catch-all. The
// hazard lives in bpmn-moddle, so these run the real thing rather than a
// stub — including the premise, so that if moddle ever stops dropping a
// dangling reference, a test says the guard has become unnecessary.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as mod from 'bpmn-moddle';
import {
  carryDroppedErrorRefs,
  droppedErrorRefDiagnostic,
  droppedErrorRefStatus,
  droppedErrorRefs,
  triageDroppedErrorRefs,
} from '../src/editor/dropped-refs.js';

const BpmnModdle = mod.default ?? mod.BpmnModdle;

function model(boundary) {
  return `<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL" id="d" targetNamespace="x">
  <bpmn:error id="err_declined" errorCode="CARD_DECLINED" />
  <bpmn:message id="m_paid" name="PAID" />
  <bpmn:process id="p">
    <bpmn:serviceTask id="st" />
    <bpmn:boundaryEvent id="be" attachedToRef="st">${boundary}</bpmn:boundaryEvent>
  </bpmn:process>
</bpmn:definitions>`;
}

const moddle = () => new BpmnModdle();

test('the premise: a round trip drops a dangling errorRef', async () => {
  const m = moddle();
  const { rootElement } = await m.fromXML(model('<bpmn:errorEventDefinition errorRef="err_missing" />'));
  const { xml } = await m.toXML(rootElement);
  assert.match(xml, /<bpmn:errorEventDefinition\s*\/>/);
});

test('a dangling errorRef is found, by the boundary that held it', async () => {
  const { warnings } = await moddle().fromXML(
    model('<bpmn:errorEventDefinition errorRef="err_missing" />')
  );
  assert.deepEqual(droppedErrorRefs(warnings), [{ boundaryId: 'be', ref: 'err_missing' }]);
});

test('a resolvable errorRef and a deliberate catch-all are not reported', async () => {
  for (const boundary of [
    '<bpmn:errorEventDefinition errorRef="err_declined" />',
    '<bpmn:errorEventDefinition />',
  ]) {
    const { warnings } = await moddle().fromXML(model(boundary));
    assert.deepEqual(droppedErrorRefs(warnings), [], boundary);
  }
});

// A dangling messageRef leaves a message boundary with no message, which
// `message-has-correlation` refuses on its own. Only an errorRef's absence
// means something.
test('other dangling references are left to the linter', async () => {
  const { warnings } = await moddle().fromXML(
    model('<bpmn:messageEventDefinition messageRef="m_missing" />')
  );
  assert.ok(warnings.length > 0, 'moddle still warns about it');
  assert.deepEqual(droppedErrorRefs(warnings), []);
});

test('the diagnostic is an error on the boundary and names the consequence', () => {
  const d = droppedErrorRefDiagnostic({ boundaryId: 'be', ref: 'err_missing' });
  assert.equal(d.severity, 'error');
  assert.equal(d.element, 'be');
  assert.match(d.message, /err_missing/);
  assert.match(d.message, /catch-all/);
});

// The editor re-imports its own serialization — every XML-box edit, every
// theme change — and moddle wrote that text, so the reference is already gone
// from it. This is the case the carry exists for, run through the real thing.
test('a re-import of the serialized model finds nothing, so the finding is carried', async () => {
  const m = moddle();
  const original = model('<bpmn:errorEventDefinition errorRef="err_missing" />');
  const first = droppedErrorRefs((await m.fromXML(original)).warnings);
  const { rootElement } = await m.fromXML(original);
  const { xml: serialized } = await m.toXML(rootElement);
  const again = droppedErrorRefs((await moddle().fromXML(serialized)).warnings);
  assert.deepEqual(again, [], 'the round trip already lost it');
  assert.deepEqual(carryDroppedErrorRefs(first, again), first);
});

test('a fresh finding for the same boundary replaces the carried one', () => {
  assert.deepEqual(
    carryDroppedErrorRefs(
      [{ boundaryId: 'be', ref: 'old' }],
      [
        { boundaryId: 'be', ref: 'new' },
        { boundaryId: 'b2', ref: 'x' },
      ]
    ),
    [
      { boundaryId: 'be', ref: 'new' },
      { boundaryId: 'b2', ref: 'x' },
    ]
  );
});

test('a code retires an entry; a missing boundary keeps it, unreported', () => {
  const entries = ['coded', 'codeless', 'missing'].map((boundaryId) => ({ boundaryId, ref: 'r' }));
  const { keep, report } = triageDroppedErrorRefs(entries, (id) => id);
  assert.deepEqual(
    keep.map((d) => d.boundaryId),
    ['codeless', 'missing']
  );
  assert.deepEqual(
    report.map((d) => d.boundaryId),
    ['codeless']
  );
});

/// The boundary as moddle builds it — the same shape the editor reads from
/// `elementRegistry`, rather than a hand-made stand-in for it.
async function boundaryOf(definition) {
  const { rootElement } = await moddle().fromXML(model(definition));
  const process = rootElement.rootElements.find((e) => e.$type === 'bpmn:Process');
  return process.flowElements.find((e) => e.id === 'be');
}

test('the status of a dropped errorRef follows what the boundary now is', async () => {
  assert.equal(
    droppedErrorRefStatus(
      await boundaryOf('<bpmn:errorEventDefinition errorRef="err_declined" />')
    ),
    'coded'
  );
  assert.equal(droppedErrorRefStatus(await boundaryOf('<bpmn:errorEventDefinition />')), 'codeless');
});

// Through the XML box a boundary can stop being an error boundary at all. A
// timer has no errorRef to answer with, and reading that as `codeless` kept
// the entry reporting an error about a code the element can no longer carry.
test('a boundary that is no longer an error boundary is missing, not codeless', async () => {
  assert.equal(
    droppedErrorRefStatus(
      await boundaryOf(
        '<bpmn:timerEventDefinition><bpmn:timeDuration>PT1H</bpmn:timeDuration></bpmn:timerEventDefinition>'
      )
    ),
    'missing'
  );
  assert.equal(droppedErrorRefStatus(await boundaryOf('')), 'missing', 'no definition at all');
  assert.equal(droppedErrorRefStatus(undefined), 'missing', 'the element is gone');
});
