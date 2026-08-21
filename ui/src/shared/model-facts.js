// Static model facts, read straight off the moddle business object that
// bpmn-js already holds for every element after importXML. No re-parse, no
// bpmn-moddle dependency of our own, and — importantly — no editing
// component: this is the read side of what a properties panel would show,
// restricted to the standard-namespace attributes rbpmn actually executes.
//
// Pure function over a plain object, so it is unit-tested without a browser.

/// @returns {Array<[string, string]>} label/value pairs, ordered for reading.
export function describeElement(bo) {
  if (!bo) return [];
  const out = [];
  const push = (label, value) => {
    if (value !== undefined && value !== null && value !== '') out.push([label, String(value)]);
  };

  const documentation = (bo.documentation ?? [])
    .map((d) => d.text)
    .filter(Boolean)
    .join('\n');
  push('documentation', documentation);

  switch (bo.$type) {
    case 'bpmn:SequenceFlow':
      push('source', bo.sourceRef?.id);
      push('target', bo.targetRef?.id);
      push('condition', bo.conditionExpression?.body);
      break;
    case 'bpmn:ExclusiveGateway':
      push('default flow', bo.default?.id);
      break;
    default:
      break;
  }

  if (bo.attachedToRef) {
    push('attached to', bo.attachedToRef.id);
    // BPMN's double negative: cancelActivity defaults to true. The editor's
    // `boundaryInterrupting` reads it the same way; a non-interrupting
    // boundary starts a side path beside the host.
    push('interrupting', bo.cancelActivity === false ? 'no' : 'yes');
  }

  for (const definition of bo.eventDefinitions ?? []) {
    switch (definition.$type) {
      case 'bpmn:TimerEventDefinition':
        push('timer duration', definition.timeDuration?.body);
        push('timer date', definition.timeDate?.body);
        push('timer cycle', definition.timeCycle?.body);
        break;
      case 'bpmn:MessageEventDefinition':
        push('message', definition.messageRef?.name ?? definition.messageRef?.id);
        break;
      case 'bpmn:ErrorEventDefinition':
        push('error code', definition.errorRef?.errorCode);
        push('error name', definition.errorRef?.name);
        break;
      default:
        push('event definition', definition.$type?.replace(/^bpmn:/, ''));
        break;
    }
  }

  // Receive task carries its message directly rather than via an event
  // definition.
  if (bo.$type === 'bpmn:ReceiveTask') {
    push('message', bo.messageRef?.name ?? bo.messageRef?.id);
  }

  return out;
}
