// The model + manifest editor.
//
// A `.bpmn` file is not deployable on its own: rbpmn keeps every runtime
// binding out of the XML, so a deployment is a *pair* — the model and its
// manifest. No bpmn-io tool knows the second half exists, which is why this
// document does. It edits both together, validates them together with the
// same code deploy runs (compiled to WASM), and hands both back as local
// files. Nothing is uploaded and nothing is stored server-side.

import Modeler from 'bpmn-js/lib/Modeler';
import 'bpmn-js/dist/assets/diagram-js.css';
import 'bpmn-js/dist/assets/bpmn-js.css';
import 'bpmn-js/dist/assets/bpmn-font/css/bpmn.css';
import './editor.css';

import { annotate, focus } from '../shared/annotations.js';
import { ensureDi } from '../shared/layout.js';
import { el } from '../shared/dom.js';
import { renderProperties } from './properties.js';
import {
  emptyManifest,
  orphanedBindings,
  parseManifest,
  serializeManifest,
  setBinding,
} from './manifest.js';
import { checkModel, initValidator } from './validate.js';
import { environmentGaps, wiringState } from './environment.js';
import { download, manifestNameFor, openFile } from './files.js';

/// A *deployable* starting point, not the usual lone start event.
///
/// The linter is this project's front door and the editor's whole job is to
/// show its verdict — so opening a new document on a model that is already
/// rejected teaches the wrong first lesson. This one passes: one start, one
/// end, an activity between them.
const STARTER = `<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
                  xmlns:bpmndi="http://www.omg.org/spec/BPMN/20100524/DI"
                  xmlns:dc="http://www.omg.org/spec/DD/20100524/DC"
                  xmlns:di="http://www.omg.org/spec/DD/20100524/DI"
                  id="definitions" targetNamespace="https://rbpmn.dev/models">
  <bpmn:process id="process" isExecutable="true">
    <bpmn:startEvent id="start" name="Started">
      <bpmn:outgoing>f1</bpmn:outgoing>
    </bpmn:startEvent>
    <bpmn:userTask id="review" name="Review">
      <bpmn:incoming>f1</bpmn:incoming>
      <bpmn:outgoing>f2</bpmn:outgoing>
    </bpmn:userTask>
    <bpmn:endEvent id="end" name="Done">
      <bpmn:incoming>f2</bpmn:incoming>
    </bpmn:endEvent>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="review" />
    <bpmn:sequenceFlow id="f2" sourceRef="review" targetRef="end" />
  </bpmn:process>
  <bpmndi:BPMNDiagram id="diagram">
    <bpmndi:BPMNPlane id="plane" bpmnElement="process">
      <bpmndi:BPMNShape id="start_di" bpmnElement="start">
        <dc:Bounds x="152" y="102" width="36" height="36" />
      </bpmndi:BPMNShape>
      <bpmndi:BPMNShape id="review_di" bpmnElement="review">
        <dc:Bounds x="240" y="80" width="100" height="80" />
      </bpmndi:BPMNShape>
      <bpmndi:BPMNShape id="end_di" bpmnElement="end">
        <dc:Bounds x="392" y="102" width="36" height="36" />
      </bpmndi:BPMNShape>
      <bpmndi:BPMNEdge id="f1_di" bpmnElement="f1">
        <di:waypoint x="188" y="120" />
        <di:waypoint x="240" y="120" />
      </bpmndi:BPMNEdge>
      <bpmndi:BPMNEdge id="f2_di" bpmnElement="f2">
        <di:waypoint x="340" y="120" />
        <di:waypoint x="392" y="120" />
      </bpmndi:BPMNEdge>
    </bpmndi:BPMNPlane>
  </bpmndi:BPMNDiagram>
</bpmn:definitions>
`;

const state = {
  manifest: emptyManifest(),
  /// null means "no server consulted" — a different thing from "not covered",
  /// and the UI must never render them the same way.
  covered: null,
  fileName: 'process.bpmn',
  selection: null,
  lastVerdict: null,
};

let modeler;
const ui = {};

function button(label, onClick, className = 'btn') {
  const node = el('button', className, label);
  node.addEventListener('click', onClick);
  return node;
}

function buildLayout(root) {
  const layout = el('div', 'layout');

  const bar = el('header', 'toolbar');
  bar.append(el('span', 'brand', 'rbpmn editor'));
  bar.append(
    button('New', newDiagram),
    button('Open .bpmn', openBpmn),
    button('Open manifest', openManifest),
    button('Save .bpmn', saveBpmn, 'btn btn-primary'),
    button('Save manifest', saveManifest, 'btn btn-primary')
  );
  ui.verdict = el('span', 'verdict', 'checking…');
  bar.append(ui.verdict);

  const canvasWrap = el('div', 'canvas-wrap');
  ui.canvas = el('div', 'canvas');
  canvasWrap.append(ui.canvas);

  const side = el('aside', 'side');

  ui.diagnostics = el('ul', 'diagnostics');
  side.append(pane('Diagnostics', ui.diagnostics));

  ui.properties = el('div', 'properties');
  side.append(pane('Element', ui.properties));

  ui.wiring = el('div', 'wiring');
  side.append(pane('Wiring (manifest)', ui.wiring));

  ui.environment = el('div', 'environment');
  side.append(pane('Environment', ui.environment));

  ui.manifestText = el('textarea', 'code');
  ui.manifestText.spellcheck = false;
  ui.manifestError = el('div', 'inline-error');
  ui.manifestError.hidden = true;
  side.append(pane('Manifest JSON', ui.manifestText, ui.manifestError, { open: false }));

  ui.xmlText = el('textarea', 'code');
  ui.xmlText.spellcheck = false;
  side.append(pane('BPMN XML', ui.xmlText, null, { open: false }));

  layout.append(bar, canvasWrap, side);
  root.append(layout);
}

function pane(title, body, extra, { open = true } = {}) {
  const wrap = el('details', 'pane');
  wrap.open = open;
  wrap.append(el('summary', null, title));
  const inner = el('div', 'pane-body');
  inner.append(body);
  if (extra) inner.append(extra);
  wrap.append(inner);
  return wrap;
}

// ---------------------------------------------------------------- validation

let checkTimer;
function scheduleCheck() {
  clearTimeout(checkTimer);
  checkTimer = setTimeout(runCheck, 250);
}

async function runCheck({ syncXmlBox = true } = {}) {
  let xml;
  try {
    ({ xml } = await modeler.saveXML({ format: true }));
  } catch (e) {
    setVerdict('error', 'cannot serialize');
    renderDiagnostics([{ severity: 'error', rule: 'export', element: '', message: e.message }]);
    return;
  }
  if (syncXmlBox && document.activeElement !== ui.xmlText) ui.xmlText.value = xml;

  const verdict = checkModel(xml, state.manifest);
  state.lastVerdict = verdict;

  const diagnostics = [...(verdict.diagnostics ?? [])];
  if (verdict.parseError) {
    diagnostics.push({
      severity: 'error',
      rule: 'parse',
      element: '',
      message: verdict.parseError,
    });
  }
  if (verdict.bindingsError) {
    diagnostics.push({
      severity: 'error',
      rule: 'manifest',
      element: '',
      message: verdict.bindingsError,
    });
  }
  if (verdict.processCount !== null && verdict.processCount !== undefined) {
    diagnostics.push({
      severity: 'error',
      rule: 'one-process',
      element: '',
      message: `a deployment must contain exactly one process, found ${verdict.processCount}`,
    });
  }
  diagnostics.push(...environmentGaps(verdict, state.covered));
  for (const { group, elementId } of orphanedBindings(state.manifest, elementIds())) {
    diagnostics.push({
      severity: 'warn',
      rule: 'manifest',
      element: elementId,
      message: `the manifest binds ${group}.${elementId}, which is not an element in this model`,
    });
  }

  renderDiagnostics(diagnostics);
  const errors = diagnostics.filter((d) => d.severity === 'error');
  if (errors.length) {
    setVerdict('error', `${errors.length} error(s)`);
  } else if (!state.covered) {
    setVerdict('warn', 'valid — environment unchecked');
  } else {
    setVerdict('ok', 'would deploy');
  }

  annotate(
    modeler,
    diagnostics
      .filter((d) => d.element)
      .map((d) => ({
        elementId: d.element,
        kind: d.severity === 'error' ? 'error' : 'warn',
        payload: { title: `${d.rule}: ${d.message}` },
      }))
  );
  renderWiring();
}

function setVerdict(kind, text) {
  ui.verdict.className = `verdict verdict-${kind}`;
  ui.verdict.textContent = text;
}

function renderDiagnostics(diagnostics) {
  ui.diagnostics.replaceChildren();
  if (!diagnostics.length) {
    ui.diagnostics.append(el('li', 'empty', 'no diagnostics — the model and manifest agree'));
    return;
  }
  for (const d of diagnostics) {
    const item = el('li', `diagnostic severity-${d.severity}`);
    const head = el('div', 'diagnostic-head');
    head.append(el('span', 'rule', d.rule));
    if (d.element) head.append(el('span', 'element', d.element));
    item.append(head, el('div', 'message', d.message));
    if (d.element) {
      item.classList.add('clickable');
      item.addEventListener('click', () => focus(modeler, d.element));
    }
    ui.diagnostics.append(item);
  }
}

// ------------------------------------------------------------------- wiring

function elementIds() {
  return modeler
    .get('elementRegistry')
    .getAll()
    .map((e) => e.id);
}

function renderWiring() {
  ui.wiring.replaceChildren();
  const element = state.selection;
  if (!element) {
    ui.wiring.append(el('p', 'empty', 'Select an element to bind it.'));
    renderIndexes();
    return;
  }
  const bo = element.businessObject;
  const id = bo.id;
  const type = bo.$type;

  const wantsTopic = type === 'bpmn:ServiceTask';
  const wantsCorrelation =
    type === 'bpmn:ReceiveTask' ||
    (bo.eventDefinitions ?? []).some((d) => d.$type === 'bpmn:MessageEventDefinition');

  if (!wantsTopic && !wantsCorrelation) {
    ui.wiring.append(el('p', 'empty', 'this element needs no manifest wiring'));
    renderIndexes();
    return;
  }

  if (wantsTopic) {
    const bound = state.manifest.topics[id];
    const effective = bound || id;
    const row = bindingRow(
      'topic',
      bound ?? '',
      id,
      (value) => {
        state.manifest = setBinding(state.manifest, 'topics', id, value);
        syncManifestBox();
        scheduleCheck();
      },
      'empty means the default: the topic is the element id'
    );
    const status = wiringState(effective, state.covered);
    row.append(
      el(
        'span',
        `wiring-state wiring-${status}`,
        status === 'unknown'
          ? 'unknown to this editor — no server checked'
          : status === 'covered'
            ? `covered by the server as '${effective}'`
            : `no handler or declared topic named '${effective}'`
      )
    );
    ui.wiring.append(row);
  }

  if (wantsCorrelation) {
    ui.wiring.append(
      bindingRow(
        'correlation key',
        state.manifest.correlations[id] ?? '',
        'order.id',
        (value) => {
          state.manifest = setBinding(state.manifest, 'correlations', id, value);
          syncManifestBox();
          scheduleCheck();
        },
        'a FEEL qualified name into the instance variables; there is no default'
      )
    );
  }
  renderIndexes();
}

function bindingRow(label, value, placeholder, commit, hint) {
  const row = el('div', 'prop');
  row.append(el('span', 'prop-label', label));
  const input = el('input', 'prop-input');
  input.value = value;
  input.placeholder = placeholder;
  const fire = () => {
    if (value !== input.value) commit(input.value);
  };
  input.addEventListener('blur', fire);
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      input.blur();
    }
  });
  row.append(input);
  if (hint) row.append(el('span', 'prop-hint', hint));
  return row;
}

function renderIndexes() {
  const row = el('div', 'prop');
  row.append(el('span', 'prop-label', 'indexes'));
  const input = el('input', 'prop-input');
  input.value = (state.manifest.indexes ?? []).join(', ');
  input.placeholder = 'order.status, customer.tier';
  const commit = () => {
    const fields = input.value
      .split(',')
      .map((f) => f.trim())
      .filter(Boolean);
    state.manifest = { ...state.manifest, indexes: fields };
    syncManifestBox();
    scheduleCheck();
  };
  input.addEventListener('blur', commit);
  row.append(input);
  row.append(
    el('span', 'prop-hint', 'optional: variables fields the application filters tasks by')
  );
  ui.wiring.append(row);
}

function syncManifestBox() {
  if (document.activeElement !== ui.manifestText) {
    ui.manifestText.value = serializeManifest(state.manifest);
  }
}

// -------------------------------------------------------------- environment

function environmentUrl() {
  // Relative to this document, so the page never needs to know the prefix an
  // application mounted it under.
  return new URL('api/environment', document.baseURI).href;
}

async function loadEnvironment() {
  ui.environment.replaceChildren(el('p', 'empty', 'loading…'));
  try {
    const response = await fetch(environmentUrl(), { headers: { accept: 'application/json' } });
    if (!response.ok) {
      renderEnvironment(null, `server answered ${response.status}`);
      return;
    }
    const body = await response.json();
    state.covered = Array.isArray(body.topics) ? body.topics : [];
    renderEnvironment(state.covered, null);
  } catch (e) {
    renderEnvironment(null, `cannot reach the server: ${e.message}`);
  }
  scheduleCheck();
}

function renderEnvironment(covered, error) {
  ui.environment.replaceChildren();
  const load = button('Check against server', loadEnvironment);
  ui.environment.append(load);
  if (error) {
    ui.environment.append(el('p', 'inline-error-text', error));
    return;
  }
  if (!covered) {
    ui.environment.append(
      el(
        'p',
        'empty',
        'No server checked. Topics show as unknown — that is not the same as missing.'
      )
    );
    return;
  }
  ui.environment.append(
    el('p', 'note', `${covered.length} topic(s) covered by the server:`),
    (() => {
      const list = el('ul', 'topic-list');
      for (const topic of covered) list.append(el('li', null, topic));
      return list;
    })()
  );
}

// -------------------------------------------------------------------- files

async function newDiagram() {
  state.fileName = 'process.bpmn';
  state.manifest = emptyManifest();
  syncManifestBox();
  await importXml(STARTER);
}

async function openBpmn() {
  const file = await openFile('.bpmn,.xml');
  if (!file) return;
  state.fileName = file.name;
  await importXml(file.text);
}

async function openManifest() {
  const file = await openFile('.json');
  if (!file) return;
  try {
    state.manifest = parseManifest(file.text);
    ui.manifestError.hidden = true;
  } catch (e) {
    ui.manifestError.hidden = false;
    ui.manifestError.textContent = e.message;
    return;
  }
  syncManifestBox();
  scheduleCheck();
}

async function saveBpmn() {
  const { xml } = await modeler.saveXML({ format: true });
  download(state.fileName, xml, 'application/xml');
}

function saveManifest() {
  download(manifestNameFor(state.fileName), serializeManifest(state.manifest), 'application/json');
}

async function importXml(xml) {
  try {
    // Hand-written models carry no diagram; laying one out beats rendering an
    // empty canvas.
    const renderable = await ensureDi(xml);
    await modeler.importXML(renderable);
    modeler.get('canvas').zoom('fit-viewport');
  } catch (e) {
    setVerdict('error', 'cannot import');
    renderDiagnostics([
      { severity: 'error', rule: 'import', element: '', message: e.message },
    ]);
    return;
  }
  state.selection = null;
  renderProperties(ui.properties, modeler, null);
  await runCheck();
}

// --------------------------------------------------------------------- boot

async function main() {
  const root = document.getElementById('rbpmn-root');
  buildLayout(root);
  renderEnvironment(null, null);

  modeler = new Modeler({ container: ui.canvas });

  modeler.on('selection.changed', ({ newSelection }) => {
    state.selection = newSelection.length === 1 ? newSelection[0] : null;
    renderProperties(ui.properties, modeler, state.selection);
    renderWiring();
  });
  modeler.on('commandStack.changed', () => {
    renderProperties(ui.properties, modeler, state.selection);
    scheduleCheck();
  });

  ui.manifestText.addEventListener('input', () => {
    try {
      state.manifest = parseManifest(ui.manifestText.value);
      ui.manifestError.hidden = true;
    } catch (e) {
      ui.manifestError.hidden = false;
      ui.manifestError.textContent = e.message;
      return;
    }
    renderWiring();
    scheduleCheck();
  });

  let xmlTimer;
  ui.xmlText.addEventListener('input', () => {
    clearTimeout(xmlTimer);
    xmlTimer = setTimeout(() => importXml(ui.xmlText.value), 400);
  });

  await initValidator();
  syncManifestBox();
  await importXml(STARTER);
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', main);
} else {
  main();
}
