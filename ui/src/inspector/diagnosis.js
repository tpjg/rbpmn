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
      // The number is what a repair must name, so it belongs in the line an
      // operator reads. Only a payload that carries the read has one.
      headline: data.incident
        ? `Incident ${data.incident.incident} at ${incidentToken.elementId}`
        : `Incident at ${incidentToken.elementId}`,
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

/// The open incident as text: the number a repair names, where it failed and
/// re-enters, what else the freeze stopped, and per disposition what it takes
/// or why it would be refused (docs/design/incident-scope.md, D10). Label and
/// value pairs, for the caller to render.
///
/// Every verdict here is the engine's own — the core answers it from the
/// functions the repair command asks, so this reports rather than re-derives.
/// It says what a repair *would* do; it never offers to do one (D13).
///
/// @returns {[string, string][] | null} null when nothing is frozen.
export function describeRepair(data) {
  const incident = data.incident;
  if (!incident) return null;
  const lines = [
    ['incident', String(incident.incident)],
    ['failed at', incident.element],
  ];
  if (incident.resume !== incident.element) {
    lines.push(['a repair resumes at', incident.resume]);
  }
  if (incident.halted) {
    lines.push([
      'also stopped',
      `${incident.halted} token(s), resumed by the same repair`,
    ]);
  }
  for (const option of incident.options ?? []) {
    const takes =
      option.takes === 'nothing' ? 'takes nothing else' : `takes a ${option.takes}`;
    lines.push([
      option.disposition,
      option.refused ? `refused — ${option.refused.reason}` : `would land, ${takes}`,
    ]);
    // A Divert's code decides which boundary takes the error, and a boundary
    // further out costs the scope in between — so the read names both, and
    // the operator picks with the price in front of them.
    for (const landing of option.codes ?? []) {
      const code = landing.code ?? null;
      const cost = landing.tearsDown
        ? `caught at ${landing.caughtAt}, tearing down ${landing.tearsDown}`
        : `caught at ${landing.caughtAt}`;
      lines.push([
        code === null ? 'with no code' : `with code ${code}`,
        code === null ? `${cost} — a catch-all, so any code lands` : cost,
      ]);
    }
  }
  return lines;
}
