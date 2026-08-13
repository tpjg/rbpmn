// "What went wrong" — the line the operator actually arrived for.
//
// The diagram answers *where*, which is rarely the question. Everything here
// is derived from rows the inspection already carries; nothing is fetched and
// nothing is guessed. Pure function, so it is unit-tested directly.

/// @returns {{severity: 'error'|'warn'|'ok'|'info', headline: string,
///            detail: string|null, elementId: string|null}}
export function diagnose(data) {
  const incidentToken = data.tokens.find((t) => t.waitKind === 'incident');
  if (incidentToken) {
    const failed = data.workItems.find(
      (w) => w.elementId === incidentToken.elementId && w.state === 'failed'
    );
    const parts = [];
    if (failed) {
      parts.push(`${failed.kind} work item on topic '${failed.topic}' failed`);
      if (failed.retries === 0) parts.push('retry budget exhausted');
      if (failed.lastFailure) parts.push(failed.lastFailure);
    } else {
      // An incident without a failed work item: a correlation or timer
      // problem froze the instance. The reason, when there is one, lives in
      // an event's `detail` — `display` is the stable golden-trace format and
      // cannot carry prose that may be reworded. Without this the headline
      // could only say *that* it froze.
      const reason = data.events
        .filter((e) => e.elementId === incidentToken.elementId && e.detail)
        .map((e) => e.detail)
        .pop();
      parts.push(
        reason ?? 'the instance froze here; no failed work item — check the trace'
      );
    }
    return {
      severity: 'error',
      headline: `Incident at ${incidentToken.elementId}`,
      detail: parts.join(' — '),
      elementId: incidentToken.elementId,
    };
  }

  if (data.status === 'completed') {
    return {
      severity: 'ok',
      headline: 'Completed',
      detail: `${data.events.length} event(s) recorded`,
      elementId: null,
    };
  }
  if (data.status === 'terminated') {
    return {
      severity: 'warn',
      headline: 'Terminated',
      detail: 'a terminate end event ended this instance',
      elementId: null,
    };
  }
  if (data.status === 'failed') {
    const failed = data.workItems.find((w) => w.state === 'failed');
    return {
      severity: 'error',
      headline: 'Failed',
      detail: failed?.lastFailure ?? 'no failure detail recorded',
      elementId: failed?.elementId ?? null,
    };
  }

  if (!data.tokens.length) {
    return {
      severity: 'warn',
      headline: 'Active with no tokens',
      detail: 'nothing is in flight — this instance cannot progress on its own',
      elementId: null,
    };
  }

  const waits = data.tokens.map((t) => describeWait(t, data));
  return {
    severity: 'info',
    headline: `Waiting — ${data.tokens.length} token(s)`,
    detail: waits.join('; '),
    elementId: data.tokens[0].elementId,
  };
}

function describeWait(token, data) {
  const where = token.elementId;
  const timer = data.timers.find((t) => t.elementId === where);
  if (timer) return `${where} until ${timer.dueAt} (${timer.dueSpec})`;
  const sub = data.subscriptions.find((s) => s.elementId === where);
  if (sub) return `${where} for message ${sub.messageName} (key ${sub.correlationKey})`;
  const item = data.workItems.find(
    (w) => w.elementId === where && (w.state === 'available' || w.state === 'locked')
  );
  if (item) return `${where} on ${item.kind} work item (${item.state}, topic ${item.topic})`;
  return `${where} (${token.waitKind})`;
}
