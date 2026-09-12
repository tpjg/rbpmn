// The diagram marks are a pure function over the inspection payload, so they
// are tested here rather than through a browser.
//
// The property that matters: an arm on a non-active instance is drawn, and is
// drawn as inert. A frozen instance keeps its subscription and timer rows for
// repair and nothing will ever act on them, so a picture that draws them like
// live ones tells the operator the failure is one element wide.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { marksFor } from '../src/inspector/marks.js';

function inspection(overrides = {}) {
  return {
    status: 'active',
    tokens: [],
    workItems: [],
    timers: [],
    subscriptions: [],
    events: [],
    ...overrides,
  };
}

const frozen = () =>
  inspection({
    status: 'failed',
    tokens: [
      { elementId: 'send_letter', waitKind: 'incident', scopeNo: 0 },
      { elementId: 'await_payment', waitKind: 'message', scopeNo: 0 },
    ],
    workItems: [
      {
        elementId: 'send_letter',
        state: 'failed',
        kind: 'service',
        topic: 'letters',
        retries: 0,
        lastFailure: 'template not found',
      },
      { elementId: 'file_copy', state: 'available', kind: 'service', topic: 'archive' },
    ],
    timers: [{ elementId: 'late_fee_due', dueSpec: 'R/P7D', dueAt: '2026-09-01T00:00:00Z' }],
    subscriptions: [
      { elementId: 'await_payment', messageName: 'PAID', correlationKey: 'T-2026-0042' },
    ],
  });

const find = (marks, elementId, kind) =>
  marks.find((m) => m.elementId === elementId && m.kind === kind);

test('on an active instance every mark is live', () => {
  const marks = marksFor(
    inspection({
      tokens: [{ elementId: 'await_payment', waitKind: 'message', scopeNo: 0 }],
      timers: [{ elementId: 'sla', dueSpec: 'P3D', dueAt: '2026-09-12T00:00:00Z' }],
      subscriptions: [
        { elementId: 'await_payment', messageName: 'PAID', correlationKey: 'T-1' },
      ],
    })
  );
  assert.ok(marks.length > 0);
  assert.ok(marks.every((m) => m.inert === false));
  assert.ok(marks.every((m) => !m.payload.title.includes('inert')));
});

test('a frozen instance draws its arms as inert, and says why', () => {
  const marks = marksFor(frozen());

  const subscription = find(marks, 'await_payment', 'message');
  assert.equal(subscription.inert, true);
  assert.match(subscription.payload.title, /awaiting PAID \(key T-2026-0042\)/);
  assert.match(subscription.payload.title, /inert: the instance is frozen on an incident/);

  for (const [elementId, kind] of [
    ['await_payment', 'token'],
    ['late_fee_due', 'timer'],
    ['file_copy', 'work'],
  ]) {
    assert.equal(find(marks, elementId, kind).inert, true, `${elementId} should be inert`);
  }
});

test('the failure itself is never muted', () => {
  const marks = marksFor(frozen());
  const errors = marks.filter((m) => m.kind === 'error');

  assert.equal(errors.length, 2, 'the incident token and the failed work item');
  assert.ok(errors.every((m) => m.inert === false));
  assert.ok(errors.every((m) => !m.payload.title.includes('inert')));
  assert.ok(
    errors.some((m) => m.payload.title.includes('template not found')),
    'the failure detail survives'
  );
});

test('completed and terminated instances give their own reason', () => {
  const leftover = {
    tokens: [{ elementId: 'await_payment', waitKind: 'message', scopeNo: 0 }],
  };
  for (const [status, reason] of [
    ['completed', /has completed/],
    ['terminated', /was terminated/],
    ['created', /has not started/],
  ]) {
    const [mark] = marksFor(inspection({ status, ...leftover }));
    assert.equal(mark.inert, true, status);
    assert.match(mark.payload.title, reason, status);
  }
});

test('an unknown status is inert with the status named, never silently live', () => {
  const [mark] = marksFor(
    inspection({
      status: 'quarantined',
      tokens: [{ elementId: 'await_payment', waitKind: 'message', scopeNo: 0 }],
    })
  );
  assert.equal(mark.inert, true);
  assert.match(mark.payload.title, /inert: the instance is quarantined/);
});

test('a failure a boundary caught is drawn as handled, not as the reason', () => {
  const marks = marksFor(
    inspection({
      status: 'completed',
      workItems: [
        {
          elementId: 'notify',
          state: 'failed',
          kind: 'service',
          topic: 'notices',
          retries: 0,
          lastFailure: 'dependency unavailable',
        },
      ],
    })
  );
  assert.equal(marks.length, 1);
  assert.equal(marks[0].kind, 'handled');
  assert.equal(marks[0].inert, false);
  assert.match(marks[0].payload.title, /dependency unavailable — handled by a boundary/);
});

test('on a frozen instance, a failure caught elsewhere stays handled', () => {
  const data = frozen();
  data.workItems.push({
    elementId: 'earlier',
    state: 'failed',
    kind: 'service',
    topic: 'x',
    retries: 0,
    lastFailure: 'caught earlier',
  });
  const mark = marksFor(data).find((m) => m.elementId === 'earlier');
  assert.equal(mark.kind, 'handled');
  assert.equal(mark.inert, false);
});

// A freeze stops more than the token that failed (D7): a move in flight and a
// decision waiting for its answer are halted where they stood. They are not
// arms — nothing the world does will satisfy one — so they are their own kind.
test('the freeze draws what it halted as collateral, not as an ordinary arm', () => {
  const data = frozen();
  data.tokens.push(
    { elementId: 'notify', waitKind: 'halted', scopeNo: 0 },
    { elementId: 'price', waitKind: 'halted_decision', scopeNo: 0 }
  );
  const marks = marksFor(data);

  const inFlight = find(marks, 'notify', 'halted');
  const decision = find(marks, 'price', 'halted');
  assert.match(inFlight.payload.title, /halted in flight by the freeze/);
  assert.match(decision.payload.title, /a decision pending/);
  assert.equal(inFlight.inert, true, 'collateral on a frozen instance is inert');
  assert.match(inFlight.payload.title, /inert: the instance is frozen on an incident/);
  assert.equal(find(marks, 'notify', 'token'), undefined, 'not also a plain arm');
});

test('a failure an operator repaired is repaired, not handled by a boundary', () => {
  const marks = marksFor(
    inspection({
      workItems: [
        {
          elementId: 'charge',
          state: 'failed',
          kind: 'service',
          topic: 'payments',
          retries: 0,
          lastFailure: 'handler answered 502',
        },
      ],
      events: [
        {
          kind: 'incident-repaired',
          elementId: 'charge',
          display: 'incident-repaired charge retry',
          detail: 'the acquirer was down all morning',
        },
      ],
    })
  );
  assert.equal(marks.length, 1);
  assert.equal(marks[0].kind, 'repaired');
  assert.equal(marks[0].inert, false);
  assert.match(marks[0].payload.title, /handler answered 502 — repaired: the acquirer was down/);
});

test('a repair at one element does not repaint a failure caught at another', () => {
  const marks = marksFor(
    inspection({
      workItems: [
        {
          elementId: 'notify',
          state: 'failed',
          kind: 'service',
          topic: 'notices',
          retries: 0,
          lastFailure: 'dependency unavailable',
        },
      ],
      events: [
        {
          kind: 'incident-repaired',
          elementId: 'charge',
          display: 'incident-repaired charge retry',
          detail: 'unrelated',
        },
      ],
    })
  );
  assert.equal(marks[0].kind, 'handled');
});
