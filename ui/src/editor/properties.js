// The element properties pane — the *editing* half of what a properties
// panel does, deliberately hand-written rather than `bpmn-js-properties-panel`.
//
// Two reasons. The stock panel's vendor provider packs write `camunda:` /
// `zeebe:` extension elements, which is exactly the XML pollution
// `no-foreign-implementation` exists to warn about; and a hand-written pane
// can only write the attributes it is given controls for, so XML purity is
// enforced by construction instead of by review. Everything below is
// standard BPMN, and the runtime wiring lives in the manifest pane next door.

import { el } from '../shared/dom.js';

/// Text row that commits on blur/Enter, so a half-typed value never reaches
/// the model (and never triggers a re-lint mid-word).
function textRow(label, value, commit, { placeholder, hint, multiline = false } = {}) {
  const row = el('label', 'prop');
  row.append(el('span', 'prop-label', label));
  const input = multiline ? el('textarea', 'prop-input') : el('input', 'prop-input');
  input.value = value ?? '';
  if (placeholder) input.placeholder = placeholder;
  if (multiline) input.rows = 2;
  const fire = () => {
    if ((value ?? '') !== input.value) commit(input.value);
  };
  input.addEventListener('blur', fire);
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !multiline) {
      e.preventDefault();
      input.blur();
    }
  });
  row.append(input);
  if (hint) row.append(el('span', 'prop-hint', hint));
  return row;
}

function selectRow(label, value, options, commit, { hint } = {}) {
  const row = el('label', 'prop');
  row.append(el('span', 'prop-label', label));
  const select = el('select', 'prop-input');
  for (const [optionValue, optionLabel] of options) {
    const option = el('option', null, optionLabel);
    option.value = optionValue;
    if (optionValue === value) option.selected = true;
    select.append(option);
  }
  select.addEventListener('change', () => commit(select.value));
  row.append(select);
  if (hint) row.append(el('span', 'prop-hint', hint));
  return row;
}

/// The interrupting question, read off a boundary event's business object.
///
/// `cancelActivity` is BPMN's double negative and its schema default is
/// *true*, so an absent attribute means interrupting and only an explicit
/// `false` may read as non-interrupting — the same reading `describeElement`
/// does on the inspector's side.
///
/// `fixed` says the answer is not the modeller's to give: an error boundary
/// always cancels the activity the error escaped from (BPMN 2.0.2, the error
/// row of the boundary event table — there is no non-interrupting form), so
/// the pane states the reason instead of offering a control.
///
/// Pure — no DOM, no modeler — so it is unit-tested under node.
/// @returns {{interrupting: boolean, fixed: boolean}|null} null if `bo` is
/// not a boundary event, in which case there is no row at all.
/// Which timer definitions the linter will execute for this element. A
/// repeating `timeCycle` only on a *non-interrupting* boundary: anywhere else
/// the first occurrence ends the wait and the repetitions could only be
/// dropped silently, which `no-unsupported-element` refuses — so the control
/// is not offered there rather than offered and then rejected. A file that
/// already carries one elsewhere still shows it, labelled as not executed.
export function timerKinds(bo, current) {
  const kinds = [
    ['timeDuration', 'duration (ISO-8601, e.g. P3D)'],
    ['timeDate', 'date (ISO-8601 with UTC offset)'],
  ];
  if (boundaryInterrupting(bo)?.interrupting === false) {
    kinds.push(['timeCycle', 'cycle (R/P7D, R3/P1D, or anchored R/2026-08-31T00:00:00+02:00/P7D)']);
  } else if (current === 'timeCycle') {
    kinds.push(['timeCycle', 'cycle (only executed on a non-interrupting boundary)']);
  }
  return kinds;
}

export function boundaryInterrupting(bo) {
  if (bo?.$type !== 'bpmn:BoundaryEvent') return null;
  const definitions = bo.eventDefinitions ?? [];
  return {
    interrupting: bo.cancelActivity !== false,
    fixed: definitions.some((d) => d.$type === 'bpmn:ErrorEventDefinition'),
  };
}

/// Writing a condition is the same operation from either pane — the flow's
/// own, or the gateway's list of branches. Kept in one place so the empty
/// case (clear the expression rather than store an empty one) cannot drift
/// between them.
function conditionWriter(modeler, flowElement) {
  const modeling = modeler.get('modeling');
  const moddle = modeler.get('moddle');
  return (body) => {
    if (!flowElement) return;
    const expression = body.trim()
      ? moddle.create('bpmn:FormalExpression', { body: body.trim() })
      : undefined;
    modeling.updateProperties(flowElement, { conditionExpression: expression });
  };
}

/// Renders the editable standard-BPMN properties of `element`.
export function renderProperties(container, modeler, element) {
  container.replaceChildren();
  if (!element) {
    container.append(el('p', 'empty', 'Select an element to edit it.'));
    return;
  }
  const modeling = modeler.get('modeling');
  const moddle = modeler.get('moddle');
  const bo = element.businessObject;
  const update = (properties) => modeling.updateProperties(element, properties);

  container.append(
    (() => {
      const head = el('div', 'element-head');
      head.append(
        el('span', 'element-type', bo.$type.replace(/^bpmn:/, '')),
        el('span', 'element-id', bo.id)
      );
      return head;
    })()
  );

  container.append(
    textRow('id', bo.id, (value) => {
      const id = value.trim();
      // An id is an XML NCName and rbpmn keys tokens by it; refusing here is
      // friendlier than letting bpmn-js throw mid-edit.
      if (!/^[A-Za-z_][\w.-]*$/.test(id)) {
        window.alert(`'${id}' is not a valid element id (letter or _, then letters/digits/._-)`);
        return;
      }
      update({ id });
    })
  );

  if ('name' in bo || bo.$type !== 'bpmn:SequenceFlow') {
    container.append(textRow('name', bo.name, (name) => update({ name: name || undefined })));
  }

  // --- documentation: an array of bpmn:Documentation, collapsed to one entry
  const documentation = (bo.documentation ?? []).map((d) => d.text).filter(Boolean).join('\n');
  container.append(
    textRow(
      'documentation',
      documentation,
      (text) => {
        const entries = text.trim()
          ? [moddle.create('bpmn:Documentation', { text })]
          : [];
        update({ documentation: entries });
      },
      { multiline: true }
    )
  );

  if (bo.$type === 'bpmn:SequenceFlow') {
    container.append(
      textRow('condition', bo.conditionExpression?.body, conditionWriter(modeler, element), {
        placeholder: 'amount > 100',
        hint: 'FEEL subset: name op literal, and/or, parentheses',
      })
    );
  }

  if (bo.$type === 'bpmn:ExclusiveGateway') {
    const outgoing = bo.outgoing ?? [];
    if (outgoing.length > 1) {
      container.append(
        selectRow(
          'default flow',
          bo.default?.id ?? '',
          [['', '(none)'], ...outgoing.map((f) => [f.id, f.name ? `${f.id} — ${f.name}` : f.id])],
          (id) => {
            const flow = outgoing.find((f) => f.id === id);
            update({ default: flow });
          },
          { hint: 'an exclusive split needs one, or conditions-feel-subset rejects it' }
        )
      );
      renderBranchConditions(container, modeler, bo, outgoing);
    }
  }

  if (bo.$type === 'bpmn:BoundaryEvent') {
    renderInterrupting(container, bo, update);
  }

  for (const definition of bo.eventDefinitions ?? []) {
    renderEventDefinition(container, modeler, element, definition);
  }

  if (bo.$type === 'bpmn:ReceiveTask') {
    renderMessageRef(container, modeler, element, bo, 'messageRef');
  }
}

/// Every branch condition, editable from the gateway.
///
/// A condition lives on the sequence flow, so strictly it is editable by
/// selecting the flow. In practice a split with a broken grammar produces one
/// `conditions-feel-subset` error per branch, and fixing them one selection
/// at a time — on edges that are fiddly to click — is miserable. The gateway
/// is where you are already looking, so the whole split is editable here.
function renderBranchConditions(container, modeler, gateway, outgoing) {
  const registry = modeler.get('elementRegistry');

  container.append(el('div', 'prop-group', 'branch conditions'));
  for (const flow of outgoing) {
    const label = flow.name ? `${flow.id} — ${flow.name}` : flow.id;
    const target = flow.targetRef?.id ? ` → ${flow.targetRef.id}` : '';

    // The default branch is the one taken when nothing matched, so it
    // must not carry a condition of its own. Showing a disabled note
    // rather than an input keeps the editor from inviting a model the
    // linter will reject.
    if (gateway.default?.id === flow.id) {
      const row = el('div', 'prop');
      row.append(el('span', 'prop-label', `${label}${target}`));
      row.append(el('span', 'prop-hint', 'default branch — taken when no condition matches'));
      container.append(row);
      continue;
    }

    container.append(
      textRow(
        `${label}${target}`,
        flow.conditionExpression?.body,
        conditionWriter(modeler, registry.get(flow.id)),
        { placeholder: 'amount > 100' }
      )
    );
  }
}

/// Interrupting or not — the standard `cancelActivity` attribute, nothing
/// vendor-specific, and the one control that changes what bpmn-js draws
/// (the dashed double circle) as well as what the XML says.
function renderInterrupting(container, bo, update) {
  const state = boundaryInterrupting(bo);
  if (!state) return;

  if (state.fixed) {
    // A note rather than a disabled select, the same shape the default
    // branch uses above: showing a control for an answer BPMN has already
    // given would invite an edit that cannot mean anything.
    const row = el('div', 'prop');
    row.append(el('span', 'prop-label', 'interrupting'));
    row.append(
      el(
        'span',
        'prop-hint',
        state.interrupting
          ? 'always yes — an error cancels the activity it escaped from'
          : 'this file says cancelActivity="false", which an error boundary cannot be:'
            + ' an error always cancels the activity it escaped from'
      )
    );
    container.append(row);
    return;
  }

  container.append(
    selectRow(
      'interrupting',
      state.interrupting ? 'yes' : 'no',
      [
        ['yes', 'yes (cancels the activity)'],
        ['no', 'no (the activity continues; a new token starts here)'],
      ],
      // `true` is the attribute's schema default, so bpmn-moddle writes
      // nothing for it: an interrupting boundary stays free of the attribute
      // exactly as every other modeller spells it, and only the
      // non-interrupting case reaches the XML.
      (choice) => update({ cancelActivity: choice === 'yes' }),
      {
        hint: 'boundary-side-path: a non-interrupting path is a side path — it runs beside'
          + ' the activity and must end at its own end event',
      }
    )
  );
}

function renderEventDefinition(container, modeler, element, definition) {
  const modeling = modeler.get('modeling');
  const moddle = modeler.get('moddle');
  const updateDefinition = (properties) =>
    modeling.updateModdleProperties(element, definition, properties);

  switch (definition.$type) {
    case 'bpmn:TimerEventDefinition': {
      // Duration, date — and a cycle only where the linter executes one
      // (`timerKinds`): offering a control for something the linter refuses
      // would be teaching the wrong thing.
      const kind = definition.timeDate
        ? 'timeDate'
        : definition.timeCycle
          ? 'timeCycle'
          : 'timeDuration';
      const current = definition[kind]?.body ?? '';
      container.append(
        selectRow('timer kind', kind, timerKinds(element.businessObject, kind), (next) => {
          const body = current;
          const expression = (selected) =>
            next === selected && body ? moddle.create('bpmn:FormalExpression', { body }) : undefined;
          updateDefinition({
            timeDuration: expression('timeDuration'),
            timeDate: expression('timeDate'),
            timeCycle: expression('timeCycle'),
          });
        })
      );
      const labels = { timeDate: 'due date', timeDuration: 'duration', timeCycle: 'cycle' };
      const placeholders = { timeDate: '2026-12-24T09:00:00Z', timeDuration: 'P3D', timeCycle: 'R/P7D' };
      const hints = {
        timeDate: 'timer-iso8601: dates need an explicit UTC offset',
        timeDuration: 'timer-iso8601: an ISO-8601 duration, or a variable name read when the timer is armed',
        timeCycle:
          'timer-iso8601: R[n]/P… or R[n]/<datetime>/P…, fixed-length period (weeks, days, hours, minutes, seconds); the anchor fixes the phase, each occurrence steps from the previous due',
      };
      container.append(
        textRow(
          labels[kind],
          current,
          (body) => {
            const expression = body.trim()
              ? moddle.create('bpmn:FormalExpression', { body: body.trim() })
              : undefined;
            updateDefinition({ [kind]: expression });
          },
          { placeholder: placeholders[kind], hint: hints[kind] }
        )
      );
      break;
    }
    case 'bpmn:MessageEventDefinition':
      renderMessageRef(container, modeler, element, definition, 'messageRef');
      break;
    case 'bpmn:ErrorEventDefinition':
      renderErrorRef(container, modeler, element, definition);
      break;
    default:
      break;
  }
}

/// A message element must reference a *named* bpmn:message
/// (`message-has-correlation`). The reference lives in the XML; the
/// correlation key that makes it routable does not — that is the manifest's
/// job, in the wiring pane.
function renderMessageRef(container, modeler, element, owner, property) {
  const modeling = modeler.get('modeling');
  const moddle = modeler.get('moddle');
  const definitions = modeler.getDefinitions();
  const messages = (definitions.rootElements ?? []).filter((r) => r.$type === 'bpmn:Message');
  const current = owner[property];

  container.append(
    textRow(
      'message name',
      current?.name ?? '',
      (name) => {
        const trimmed = name.trim();
        if (!trimmed) {
          modeling.updateModdleProperties(element, owner, { [property]: undefined });
          return;
        }
        const existing = messages.find((m) => m.name === trimmed);
        if (existing) {
          modeling.updateModdleProperties(element, owner, { [property]: existing });
          return;
        }
        if (current) {
          modeling.updateModdleProperties(element, current, { name: trimmed });
          return;
        }
        const message = moddle.create('bpmn:Message', {
          id: uniqueId(definitions, 'Message'),
          name: trimmed,
        });
        message.$parent = definitions;
        definitions.get('rootElements').push(message);
        modeling.updateModdleProperties(element, owner, { [property]: message });
      },
      { placeholder: 'PaymentReceived', hint: 'creates or reuses a bpmn:message declaration' }
    )
  );
}

function renderErrorRef(container, modeler, element, definition) {
  const modeling = modeler.get('modeling');
  const moddle = modeler.get('moddle');
  const definitions = modeler.getDefinitions();
  const current = definition.errorRef;

  container.append(
    textRow(
      'error code',
      current?.errorCode ?? '',
      (code) => {
        const trimmed = code.trim();
        if (!trimmed) {
          modeling.updateModdleProperties(element, definition, { errorRef: undefined });
          return;
        }
        if (current) {
          modeling.updateModdleProperties(element, current, { errorCode: trimmed });
          return;
        }
        const error = moddle.create('bpmn:Error', {
          id: uniqueId(definitions, 'Error'),
          name: trimmed,
          errorCode: trimmed,
        });
        error.$parent = definitions;
        definitions.get('rootElements').push(error);
        modeling.updateModdleProperties(element, definition, { errorRef: error });
      },
      { placeholder: 'CARD_DECLINED', hint: 'matched against the failure code a handler reports' }
    )
  );
}

function uniqueId(definitions, prefix) {
  const taken = new Set((definitions.rootElements ?? []).map((r) => r.id));
  let n = 1;
  while (taken.has(`${prefix}_${n}`)) n += 1;
  return `${prefix}_${n}`;
}
