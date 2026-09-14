// The diagnosis line is the inspector's answer to the question an operator
// actually arrived with. It is a pure function over the inspection payload,
// so it is tested here rather than through a browser.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { describeRepair, diagnose } from '../src/inspector/diagnosis.js';

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

// What a repair would do is a read the engine answers (D10); the inspector
// reports it and never offers to act on it (D13).
const openIncident = {
  incident: 3,
  element: 'charge',
  resume: 'charge',
  halted: 2,
  options: [
    { disposition: 'retry', takes: 'patch', refused: null },
    { disposition: 'abandon-instance', takes: 'nothing', refused: null },
    {
      disposition: 'divert',
      takes: 'code',
      refused: { cause: 'nothing-catches', reason: 'no boundary catches an error raised here' },
      codes: [],
    },
  ],
};

/// The same incident in a model that does catch something: one code caught on
/// the failing activity itself, one caught further out at the cost of the
/// subprocess in between, and the codeless divert a catch-all takes.
const divertible = {
  ...openIncident,
  options: [
    {
      disposition: 'divert',
      takes: 'code',
      refused: null,
      codes: [
        { code: null, caughtAt: 'any_error', tearsDown: null },
        { code: 'PAYMENT_FAILED', caughtAt: 'be', tearsDown: null },
        { code: 'ESCALATE', caughtAt: 'sub_be', tearsDown: 'sub' },
      ],
    },
  ],
};

test('the headline names the number a repair has to give', () => {
  const frozen = inspection({
    status: 'failed',
    tokens: [{ elementId: 'charge', waitKind: 'incident', scopeNo: 0 }],
  });
  assert.match(diagnose(frozen).headline, /^Incident at charge$/);
  assert.match(
    diagnose({ ...frozen, incident: openIncident }).headline,
    /^Incident 3 at charge$/
  );
});

test('the open incident is described disposition by disposition', () => {
  const lines = describeRepair(inspection({ status: 'failed', incident: openIncident }));
  const value = (label) => lines.find(([l]) => l === label)?.[1];

  assert.equal(value('incident'), '3');
  assert.equal(value('failed at'), 'charge');
  assert.equal(value('a repair resumes at'), undefined, 'not repeated when it is the element');
  assert.match(value('also stopped'), /2 token\(s\)/);
  assert.match(value('retry'), /would land, takes a patch/);
  assert.match(value('abandon-instance'), /takes nothing else/);
  assert.match(value('divert'), /refused — no boundary catches/);
});

test('a repair that re-enters elsewhere says where', () => {
  const lines = describeRepair(
    inspection({
      status: 'failed',
      incident: { ...openIncident, element: 'be_timeout', resume: 'review', halted: 0 },
    })
  );
  const value = (label) => lines.find(([l]) => l === label)?.[1];
  assert.equal(value('a repair resumes at'), 'review');
  assert.equal(value('also stopped'), undefined, 'no collateral, nothing to say');
});

test('a divert names its codes, where each lands, and what it costs', () => {
  const lines = describeRepair(inspection({ status: 'failed', incident: divertible }));
  const value = (label) => lines.find(([l]) => l === label)?.[1];

  assert.match(value('with code PAYMENT_FAILED'), /^caught at be$/);
  assert.match(value('with code ESCALATE'), /caught at sub_be, tearing down sub/);
  assert.match(value('with no code'), /caught at any_error — a catch-all, so any code lands/);
});

test('a disposition that takes no code lists none', () => {
  const lines = describeRepair(inspection({ status: 'failed', incident: openIncident }));
  assert.equal(
    lines.filter(([label]) => label.startsWith('with ')).length,
    0,
    'nothing catches here, so there is no code to name'
  );
});

// The engine could not read the incident; the reason takes its place rather
// than the section vanishing as if nothing were frozen.
test('an unreadable incident is described by why', () => {
  const reason = "definition no longer compiles: token references unknown element 'nowhere'";
  assert.deepEqual(
    describeRepair(inspection({ status: 'failed', incident: null, incidentUnreadable: reason })),
    [['unreadable', reason]]
  );
});

test('nothing frozen, nothing to describe', () => {
  assert.equal(describeRepair(inspection()), null);
  assert.equal(describeRepair(inspection({ status: 'completed' })), null);
});
