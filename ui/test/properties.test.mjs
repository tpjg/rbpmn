// The one reading in the properties pane that is not the attribute's face
// value: `cancelActivity` is a double negative with a schema default of
// *true*, so "absent" and "no" are opposite answers and confusing them writes
// the wrong shape into a model. Pure function, so it is checked here rather
// than only in a browser.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { boundaryInterrupting, timerKinds } from '../src/editor/properties.js';

test('only a boundary event gets the row at all', () => {
  assert.equal(boundaryInterrupting(null), null);
  assert.equal(boundaryInterrupting(undefined), null);
  assert.equal(boundaryInterrupting({ $type: 'bpmn:IntermediateCatchEvent' }), null);
  assert.equal(boundaryInterrupting({ $type: 'bpmn:UserTask' }), null);
});

// The trap: a boundary event written by any modeller carries no
// `cancelActivity` at all, and reading that as false would offer to "fix" an
// interrupting boundary into a non-interrupting one behind the modeller's back.
test('an absent cancelActivity reads as interrupting', () => {
  assert.deepEqual(boundaryInterrupting({ $type: 'bpmn:BoundaryEvent' }), {
    interrupting: true,
    fixed: false,
  });
  assert.deepEqual(
    boundaryInterrupting({ $type: 'bpmn:BoundaryEvent', cancelActivity: true }),
    { interrupting: true, fixed: false }
  );
  assert.deepEqual(
    boundaryInterrupting({ $type: 'bpmn:BoundaryEvent', cancelActivity: false }),
    { interrupting: false, fixed: false }
  );
});

test("a message or timer boundary is the modeller's to answer", () => {
  for (const kind of ['bpmn:MessageEventDefinition', 'bpmn:TimerEventDefinition']) {
    assert.deepEqual(
      boundaryInterrupting({
        $type: 'bpmn:BoundaryEvent',
        eventDefinitions: [{ $type: kind }],
      }),
      { interrupting: true, fixed: false },
      kind
    );
  }
});

// An error always cancels the activity it escaped from, so there is nothing to
// choose — and a file that says otherwise is reported as such rather than
// displayed as "yes".
test('an error boundary is interrupting by definition', () => {
  assert.deepEqual(
    boundaryInterrupting({
      $type: 'bpmn:BoundaryEvent',
      eventDefinitions: [{ $type: 'bpmn:ErrorEventDefinition' }],
    }),
    { interrupting: true, fixed: true }
  );
  assert.deepEqual(
    boundaryInterrupting({
      $type: 'bpmn:BoundaryEvent',
      cancelActivity: false,
      eventDefinitions: [{ $type: 'bpmn:ErrorEventDefinition' }],
    }),
    { interrupting: false, fixed: true }
  );
});

// A repeating timer is executed on a non-interrupting boundary and nowhere
// else, so the control follows the linter: offered there, absent elsewhere —
// unless the file already carries one, which is shown rather than hidden.
test('timeCycle is offered only on a non-interrupting boundary', () => {
  const ids = (bo, current) => timerKinds(bo, current).map(([id]) => id);
  assert.deepEqual(ids({ $type: 'bpmn:IntermediateCatchEvent' }), ['timeDuration', 'timeDate']);
  assert.deepEqual(ids({ $type: 'bpmn:BoundaryEvent' }), ['timeDuration', 'timeDate']);
  assert.deepEqual(ids({ $type: 'bpmn:BoundaryEvent', cancelActivity: true }), ['timeDuration', 'timeDate']);
  assert.deepEqual(ids({ $type: 'bpmn:BoundaryEvent', cancelActivity: false }), [
    'timeDuration',
    'timeDate',
    'timeCycle',
  ]);
  // Already in the file, on an element where it is not executed: shown, and
  // labelled as such, rather than silently re-read as a duration.
  const shown = timerKinds({ $type: 'bpmn:IntermediateCatchEvent' }, 'timeCycle');
  assert.equal(shown.at(-1)[0], 'timeCycle');
  assert.match(shown.at(-1)[1], /only executed on a non-interrupting boundary/);
});
