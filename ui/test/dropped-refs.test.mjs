// A mistyped errorRef must not come back from the editor as a catch-all. The
// hazard lives in bpmn-moddle, so these run the real thing rather than a
// stub — including the premise, so that if moddle ever stops dropping a
// dangling reference, a test says the guard has become unnecessary.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import * as mod from 'bpmn-moddle';
import { droppedErrorRefDiagnostic, droppedErrorRefs } from '../src/editor/dropped-refs.js';

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
