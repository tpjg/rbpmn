// The marks the diagram carries, derived from runtime rows only.
//
// Pure function over the inspection payload, so it is unit-tested directly,
// like `diagnose`.
//
// Instance status is part of that derivation rather than decoration on top of
// it. A frozen instance keeps its timers and subscriptions — they are the
// repair target — and every gate that would act on one is closed by `status`
// alone: `CLAIMABLE` requires an active instance, so does the scheduler's
// due-timer query, and `correlate` resolves subscriptions only on one. A row
// on a non-active instance is therefore *present* and *inert*, and drawing it
// exactly like a live one makes the picture claim a blast radius one element
// wide when it is the whole instance.
//
// A freeze stops more than the token that failed: a move in flight and a
// decision waiting for its answer are halted where they stood, and a repair
// resumes them with the cause (D7). They are drawn as their own kind, not as
// ordinary arms — an arm is something the world can still satisfy, and these
// are waiting on an operator.
//
// What is never inert: the marks that record a failure. An incident token,
// and the failed work item it is parked at, are not arms waiting on
// something — they are the reason nothing else will happen, and muting them
// would hide the one thing an operator came for. A failed item with no
// incident token at its element was *caught*: a boundary took it, and it is
// drawn as handled — history, not a reason. (Matched by element, because the
// inspection carries no token on a work item: a task that failed twice in a
// loop, once caught and once not, draws both as the incident.) A failure an
// operator repaired is neither: no boundary took it, a person did, and the
// reason they gave is on the mark.

/// Why the arms on this instance cannot fire, or null while they can.
function inertReason(status) {
  switch (status) {
    case 'active':
      return null;
    case 'failed':
      return 'the instance is frozen on an incident';
    case 'completed':
      return 'the instance has completed';
    case 'terminated':
      return 'the instance was terminated';
    case 'created':
      return 'the instance has not started';
    default:
      return `the instance is ${status}`;
  }
}

export function marksFor(data) {
  const inert = inertReason(data.status);
  const incidentAt = new Set(
    data.tokens.filter((t) => t.waitKind === 'incident').map((t) => t.elementId)
  );
  /// Element -> the reason the operator gave, when a repair landed there.
  /// `detail` carries it because `display` is the golden-trace format and
  /// cannot hold prose that may be reworded.
  const repairedAt = new Map(
    data.events
      .filter((e) => e.kind === 'incident-repaired' && e.elementId)
      .map((e) => [e.elementId, e.detail])
  );
  const marks = [];

  /// An arm or a live wait: true now, and only while the instance is active.
  const arm = (elementId, kind, title) => {
    marks.push({
      elementId,
      kind,
      inert: inert !== null,
      payload: { title: inert ? `${title} — inert: ${inert}` : title },
    });
  };

  /// The failure record itself: true regardless of status, never muted.
  const failure = (elementId, title) => {
    marks.push({ elementId, kind: 'error', inert: false, payload: { title } });
  };

  for (const token of data.tokens) {
    if (token.waitKind === 'incident') {
      failure(token.elementId, 'token — incident');
    } else if (token.waitKind === 'halted') {
      arm(token.elementId, 'halted', 'token — halted in flight by the freeze');
    } else if (token.waitKind === 'halted_decision') {
      arm(token.elementId, 'halted', 'token — halted by the freeze, a decision pending');
    } else {
      arm(token.elementId, 'token', `token — ${token.waitKind}`);
    }
  }
  for (const item of data.workItems) {
    if (item.state === 'available' || item.state === 'locked') {
      arm(
        item.elementId,
        'work',
        `work item ${item.state} (${item.kind} / ${item.topic})`
      );
    } else if (item.state === 'failed') {
      const detail = item.lastFailure ?? 'no detail recorded';
      if (incidentAt.has(item.elementId)) {
        failure(item.elementId, `work item failed: ${detail}`);
      } else if (repairedAt.has(item.elementId)) {
        const reason = repairedAt.get(item.elementId);
        marks.push({
          elementId: item.elementId,
          kind: 'repaired',
          inert: false,
          payload: {
            title:
              `work item failed: ${detail} — repaired` + (reason ? `: ${reason}` : ''),
          },
        });
      } else {
        marks.push({
          elementId: item.elementId,
          kind: 'handled',
          inert: false,
          payload: { title: `work item failed: ${detail} — handled by a boundary` },
        });
      }
    }
  }
  for (const timer of data.timers) {
    arm(timer.elementId, 'timer', `timer ${timer.dueSpec} — due ${timer.dueAt}`);
  }
  for (const sub of data.subscriptions) {
    arm(
      sub.elementId,
      'message',
      `awaiting ${sub.messageName} (key ${sub.correlationKey})`
    );
  }
  return marks;
}
