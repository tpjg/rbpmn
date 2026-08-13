// The read side of a properties panel: static facts pulled off the moddle
// business object bpmn-js already holds. Plain objects here — the function
// never touches the DOM or the modeler.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { describeElement } from '../src/shared/model-facts.js';
import { environmentGaps, wiringState } from '../src/editor/environment.js';

test('no business object yields no facts', () => {
  assert.deepEqual(describeElement(null), []);
  assert.deepEqual(describeElement(undefined), []);
});

test('a sequence flow shows its endpoints and condition', () => {
  const facts = describeElement({
    $type: 'bpmn:SequenceFlow',
    sourceRef: { id: 'gw' },
    targetRef: { id: 'approve' },
    conditionExpression: { body: 'amount > 100' },
  });
  assert.deepEqual(facts, [
    ['source', 'gw'],
    ['target', 'approve'],
    ['condition', 'amount > 100'],
  ]);
});

test('timer definitions surface whichever form is set', () => {
  assert.deepEqual(
    describeElement({
      $type: 'bpmn:IntermediateCatchEvent',
      eventDefinitions: [
        { $type: 'bpmn:TimerEventDefinition', timeDuration: { body: 'P3D' } },
      ],
    }),
    [['timer duration', 'P3D']]
  );
  assert.deepEqual(
    describeElement({
      $type: 'bpmn:IntermediateCatchEvent',
      eventDefinitions: [
        { $type: 'bpmn:TimerEventDefinition', timeDate: { body: '2026-12-24T09:00:00Z' } },
      ],
    }),
    [['timer date', '2026-12-24T09:00:00Z']]
  );
});

// BPMN's double negative: cancelActivity defaults to true, and rbpmn v1 only
// executes the interrupting form — so the pane must not read a missing
// attribute as "non-interrupting".
test('a boundary event reports interrupting correctly from an absent attribute', () => {
  assert.deepEqual(
    describeElement({ $type: 'bpmn:BoundaryEvent', attachedToRef: { id: 'charge' } }),
    [
      ['attached to', 'charge'],
      ['interrupting', 'yes'],
    ]
  );
  assert.deepEqual(
    describeElement({
      $type: 'bpmn:BoundaryEvent',
      attachedToRef: { id: 'charge' },
      cancelActivity: false,
    }),
    [
      ['attached to', 'charge'],
      ['interrupting', 'no'],
    ]
  );
});

test('a receive task carries its message directly, not via an event definition', () => {
  assert.deepEqual(
    describeElement({ $type: 'bpmn:ReceiveTask', messageRef: { name: 'ShipmentConfirmed' } }),
    [['message', 'ShipmentConfirmed']]
  );
});

test('an error boundary shows the code that must match a handler failure', () => {
  assert.deepEqual(
    describeElement({
      $type: 'bpmn:BoundaryEvent',
      attachedToRef: { id: 'charge' },
      eventDefinitions: [
        {
          $type: 'bpmn:ErrorEventDefinition',
          errorRef: { errorCode: 'CARD_DECLINED', name: 'Declined' },
        },
      ],
    }),
    [
      ['attached to', 'charge'],
      ['interrupting', 'yes'],
      ['error code', 'CARD_DECLINED'],
      ['error name', 'Declined'],
    ]
  );
});

test('empty and absent values are skipped rather than shown blank', () => {
  assert.deepEqual(
    describeElement({ $type: 'bpmn:Task', documentation: [{ text: '' }], name: undefined }),
    []
  );
});

// ---------------------------------------------------------------------- L3

// "Unknown" and "missing" are different answers and the UI must never render
// them the same way: without a server consulted, the honest report is that
// nothing has been checked.
test('no environment loaded means no gaps are claimed', () => {
  const verdict = { topics: { st: 'payments' } };
  assert.deepEqual(environmentGaps(verdict, null), []);
  assert.equal(wiringState('payments', null), 'unknown');
});

test('a loaded environment separates covered from missing', () => {
  const verdict = { topics: { st: 'payments', other: 'shipping' } };
  const gaps = environmentGaps(verdict, ['payments']);
  assert.equal(gaps.length, 1);
  assert.equal(gaps[0].element, 'other');
  assert.equal(gaps[0].rule, 'unresolved-topic');
  assert.equal(gaps[0].severity, 'error');
  assert.equal(wiringState('payments', ['payments']), 'covered');
  assert.equal(wiringState('shipping', ['payments']), 'missing');
});

// An environment that covers nothing is still an answer, unlike not asking.
test('an empty covered set is a checked environment, not an unchecked one', () => {
  assert.equal(wiringState('payments', []), 'missing');
  assert.equal(environmentGaps({ topics: { st: 'payments' } }, []).length, 1);
});
