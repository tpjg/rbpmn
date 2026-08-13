// The diagnosis line is the inspector's answer to the question an operator
// actually arrived with. It is a pure function over the inspection payload,
// so it is tested here rather than through a browser.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { diagnose } from '../src/inspector/diagnosis.js';

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

test('an incident names the element and the failure that caused it', () => {
  const result = diagnose(
    inspection({
      tokens: [{ elementId: 'charge', waitKind: 'incident', scopeNo: 0 }],
      workItems: [
        {
          elementId: 'charge',
          state: 'failed',
          topic: 'payments',
          kind: 'service',
          retries: 0,
          lastFailure: 'handler answered 502',
        },
      ],
    })
  );
  assert.equal(result.severity, 'error');
  assert.equal(result.elementId, 'charge');
  assert.match(result.headline, /Incident at charge/);
  assert.match(result.detail, /payments/);
  assert.match(result.detail, /retry budget exhausted/);
  assert.match(result.detail, /handler answered 502/);
});

// Correlation and timer problems freeze an instance without any failed work
// item. Inventing a cause there would be worse than admitting there isn't one.
test('an incident with no failed work item says so instead of guessing', () => {
  const result = diagnose(
    inspection({ tokens: [{ elementId: 'wait', waitKind: 'incident', scopeNo: 0 }] })
  );
  assert.equal(result.severity, 'error');
  assert.match(result.detail, /no failed work item/);
});

test('retries still left are not reported as an exhausted budget', () => {
  const result = diagnose(
    inspection({
      tokens: [{ elementId: 'charge', waitKind: 'incident', scopeNo: 0 }],
      workItems: [
        {
          elementId: 'charge',
          state: 'failed',
          topic: 'payments',
          kind: 'service',
          retries: 2,
          lastFailure: 'timeout',
        },
      ],
    })
  );
  assert.doesNotMatch(result.detail, /exhausted/);
});

test('a waiting instance describes what each token waits for', () => {
  const result = diagnose(
    inspection({
      tokens: [
        { elementId: 'approve', waitKind: 'work', scopeNo: 0 },
        { elementId: 'deadline', waitKind: 'timer', scopeNo: 0 },
        { elementId: 'paid', waitKind: 'message', scopeNo: 0 },
      ],
      workItems: [
        {
          elementId: 'approve',
          state: 'available',
          topic: 'reviews',
          kind: 'user',
          retries: 3,
          lastFailure: null,
        },
      ],
      timers: [{ elementId: 'deadline', dueSpec: 'P3D', dueAt: '2026-08-16T09:00:00Z' }],
      subscriptions: [{ elementId: 'paid', messageName: 'Paid', correlationKey: 'o-1' }],
    })
  );
  assert.equal(result.severity, 'info');
  assert.match(result.headline, /3 token/);
  assert.match(result.detail, /approve on user work item \(available, topic reviews\)/);
  assert.match(result.detail, /deadline until 2026-08-16T09:00:00Z \(P3D\)/);
  assert.match(result.detail, /paid for message Paid \(key o-1\)/);
});

test('terminal statuses read as themselves', () => {
  assert.equal(diagnose(inspection({ status: 'completed' })).severity, 'ok');
  assert.equal(diagnose(inspection({ status: 'terminated' })).severity, 'warn');
  const failed = diagnose(
    inspection({
      status: 'failed',
      workItems: [{ elementId: 'x', state: 'failed', lastFailure: 'boom', retries: 0 }],
    })
  );
  assert.equal(failed.severity, 'error');
  assert.equal(failed.elementId, 'x');
  assert.match(failed.detail, /boom/);
});

// Active with nothing in flight is not "fine"; it is a stuck instance, and
// the inspector exists to say that out loud.
test('active with no tokens is flagged, not passed over', () => {
  const result = diagnose(inspection());
  assert.equal(result.severity, 'warn');
  assert.match(result.headline, /no tokens/);
});

test('an incident outranks a terminal status', () => {
  const result = diagnose(
    inspection({
      status: 'failed',
      tokens: [{ elementId: 'charge', waitKind: 'incident', scopeNo: 0 }],
    })
  );
  assert.match(result.headline, /Incident/);
});

// `display` is the golden-trace format and therefore stable API, so a reason
// that may still be reworded lives in the event's `detail`. An incident with
// no failed work item — a timer that would not resolve, say — has its cause
// only there, and dropping it leaves the headline saying merely *that* the
// instance froze.
test('an incident with no work item takes its reason from an event detail', () => {
  const result = diagnose(
    inspection({
      tokens: [{ elementId: 't1', waitKind: 'incident', scopeNo: 0 }],
      events: [
        { kind: 'timer-resolve-failed', elementId: 't1', display: 'timer-resolve-failed t1',
          detail: "'due' is not an ISO-8601 duration and no variable of that name is set" },
      ],
    })
  );
  assert.equal(result.severity, 'error');
  assert.match(result.detail, /not an ISO-8601 duration/);
});

test('an event detail for a different element is not borrowed', () => {
  const result = diagnose(
    inspection({
      tokens: [{ elementId: 't1', waitKind: 'incident', scopeNo: 0 }],
      events: [
        { kind: 'timer-resolve-failed', elementId: 'elsewhere', display: 'x', detail: 'unrelated' },
      ],
    })
  );
  assert.match(result.detail, /no failed work item/);
});

// A failed work item already carries its own reason; the detail path is the
// fallback for incidents that have none, not a replacement.
test('a failed work item still explains itself', () => {
  const result = diagnose(
    inspection({
      tokens: [{ elementId: 'charge', waitKind: 'incident', scopeNo: 0 }],
      workItems: [{ elementId: 'charge', state: 'failed', topic: 'payments', kind: 'service',
                    retries: 0, lastFailure: 'handler answered 502' }],
      events: [{ kind: 'x', elementId: 'charge', display: 'x', detail: 'should not win' }],
    })
  );
  assert.match(result.detail, /handler answered 502/);
  assert.doesNotMatch(result.detail, /should not win/);
});
