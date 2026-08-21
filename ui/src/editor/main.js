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
import '../shared/diagram-theme.css';
import './editor.css';

import { annotate, focus } from '../shared/annotations.js';
import { ensureDi } from '../shared/layout.js';
import { el } from '../shared/dom.js';
import { renderProperties } from './properties.js';
import {
  emptyManifest,
  orphanedBindings,
  parseManifest,
  formatIndexField,
  parseIndexField,
  serializeManifest,
  setBinding,
  setDecisionBinding,
} from './manifest.js';
import { checkModel, evaluateDecision, initValidator } from './validate.js';
import { environmentGaps, wiringState } from './environment.js';
import { download, manifestNameFor, openFile } from './files.js';
import { buildBundle, bundleNameFor, decisionFileName, parseBundle } from './bundle.js';
import { DecisionEditor, addDecision, starterDecision } from './decisions.js';
import { onThemeChange, rendererColors } from '../shared/theme.js';

// The example deployment, imported from the files that *are* it rather than
// copied into string literals here. All three are the same artifacts
// `e2e/demo.py` deploys, so the editor opens on a deployment that demonstrably
// runs — and `cargo test`'s fixture corpus keeps the model lint-clean and the
// decision valid without this document having to be built to find out.
//
// Vite inlines `?raw` at build time, so nothing is fetched: the editor stays
// one file that works from `file://`. `ui-test`'s node runner never loads this
// module (it imports bpmn-js), so the non-standard specifier costs it nothing.
import EXAMPLE_BPMN from '../../../crates/rbpmn-model/tests/fixtures/accept/28-demo-order.bpmn?raw';
import EXAMPLE_BINDINGS from '../../../crates/rbpmn-model/tests/fixtures/accept/28-demo-order.bindings.json?raw';
import EXAMPLE_DMN from '../../../crates/rbpmn-dmn/tests/fixtures/accept/09-demo-triage.dmn?raw';

/// What the editor opens on: a whole **deployment**, not a diagram.
///
/// A lone start event teaches nothing, and the previous starter — start, user
/// task, end — taught only that the linter is happy. Neither showed the thing
/// this document exists for, which is that a `.bpmn` is half a deployment: the
/// other half is a manifest and the DMN artifacts that travel with it, and no
/// bpmn-io tool has any of it.
///
/// So the first screen has all three, already consistent: a business-rule task
/// bound by name to a decision in the working set, service tasks bound to
/// topics, an index declared. Every pane in the editor has something in it,
/// deleting is easier than authoring, and "New" is one click away for someone
/// who wants the bare skeleton instead.
///
/// The manifest goes through `parseManifest` rather than being written as an
/// object literal, so the example is held to exactly the shape the editor
/// accepts from a user — a starting point the editor itself would reject is
/// not a starting point.
const EXAMPLE = {
  fileName: 'order.bpmn',
  bpmn: EXAMPLE_BPMN,
  bindings: EXAMPLE_BINDINGS,
  decisions: [{ name: 'triage.dmn', xml: EXAMPLE_DMN }],
};

/// What **New** gives you: a cleared desk that still passes.
///
/// Deliberately not the example. "New" means "I want to model my own thing",
/// and starting that by deleting twelve elements is worse than starting from
/// nothing. But it is not nothing either — the linter is this project's front
/// door and the editor's whole job is to show its verdict, so a new document
/// that opens already rejected teaches the wrong first lesson. One start, one
/// end, an activity between them, no manifest needed.
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
  /// The DMN artifacts this deployment carries: `[{ name, xml }]`. They
  /// travel *inside* the deployment, which is why the editor can validate
  /// them completely without a server.
  decisions: [],
  /// Which artifact the decision canvas is showing, by index.
  activeDecision: 0,
  /// 'process' or 'decisions' — one canvas, two editors. Side by side was
  /// rejected: a decision is edited *because of* a task in the process, not
  /// alongside it, and two live canvases double the ways focus can be lost.
  mode: 'process',
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
  // Guarded here rather than at each call site, because nearly every handler
  // in this editor is `async` and a promise nobody awaits turns any throw into
  // an unhandled rejection — invisible to the user, and to the e2e run just a
  // console error with no indication of which button threw.
  node.addEventListener('click', () => handle(onClick()));
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
    button('Save manifest', saveManifest, 'btn btn-primary'),
    button('Export bundle', saveBundle, 'btn btn-primary'),
    button('Open bundle', openBundle)
  );
  ui.verdict = el('span', 'verdict', 'checking…');
  bar.append(ui.verdict);

  ui.modeProcess = button('Process', () => setMode('process'), 'btn btn-mode');
  ui.modeDecisions = button(
    'Decisions',
    () => goToDecision(state.activeDecision),
    'btn btn-mode'
  );
  bar.append(ui.modeProcess, ui.modeDecisions);

  const canvasWrap = el('div', 'canvas-wrap');
  ui.canvas = el('div', 'canvas');
  // A second canvas rather than a shared one: dmn-js and bpmn-js both own
  // their container outright, and handing the same node back and forth is how
  // you end up with two renderers fighting over it.
  ui.dmnCanvas = el('div', 'canvas');
  ui.dmnCanvas.hidden = true;
  canvasWrap.append(ui.canvas, ui.dmnCanvas);

  const side = el('aside', 'side');

  ui.diagnostics = el('ul', 'diagnostics');
  const diagnosticsPane = pane('Diagnostics', ui.diagnostics);
  ui.diagnosticsTitle = diagnosticsPane.querySelector('summary');
  side.append(diagnosticsPane);

  ui.properties = el('div', 'properties');
  // Hidden outright in decisions mode rather than left empty — see `setMode`.
  ui.elementPane = pane('Element', ui.properties);
  side.append(ui.elementPane);

  ui.wiring = el('div', 'wiring');
  side.append(pane('Wiring (manifest)', ui.wiring));

  ui.decisions = el('div', 'decisions');
  side.append(pane('Decisions', ui.decisions));

  ui.tryIt = el('div', 'try-it');
  side.append(pane('Try a decision', ui.tryIt, null, { open: false }));

  ui.environment = el('div', 'environment');
  side.append(pane('Environment', ui.environment));

  ui.manifestText = el('textarea', 'code code-manifest');
  ui.manifestText.spellcheck = false;
  ui.manifestError = el('div', 'inline-error');
  ui.manifestError.hidden = true;
  side.append(pane('Manifest JSON', ui.manifestText, ui.manifestError, { open: false }));

  ui.xmlText = el('textarea', 'code code-xml');
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

/// The listeners a fresh Modeler needs. Extracted because a theme flip has to
/// rebuild the modeler — renderer colours are construction-time — and a
/// rebuilt one with no listeners is an editor whose panes never update again.
function wireModeler() {
  modeler.on('selection.changed', ({ newSelection }) => {
    state.selection = newSelection.length === 1 ? newSelection[0] : null;
    renderProperties(ui.properties, modeler, state.selection);
    renderWiring();
  });
  modeler.on('commandStack.changed', () => {
    renderProperties(ui.properties, modeler, state.selection);
    scheduleCheck();
  });
}

/// Construct the decision editor, replacing any previous one.
///
/// The counterpart to `wireModeler`, and it *destroys* rather than merely
/// dropping: a theme flip rebuilds both editors, and a dropped dmn-js keeps
/// its global key bindings and its listeners alive, so each flip would leave
/// another instance competing for the same keystrokes.
function mountDecisionEditor() {
  decisionEditor?.destroy();
  decisionEditor = new DecisionEditor(ui.dmnCanvas, {
    colors: rendererColors(),
    onChange: () => {
      // An edit in the decision canvas is an edit to the deployment: it can
      // rename an invocable a manifest binds, so the whole verdict is
      // recomputed rather than just the DMN half.
      captureDecisionQueued().then(scheduleCheck);
    },
  });
}

/// Rebuild the modeler for the new theme, keeping the work.
///
/// The model survives (re-imported), the manifest survives (it lives in
/// `state`); the undo history does not, which is the price of a diagram you
/// can still read. This fires when the OS flips at sunset, not on a keystroke.
async function remountForTheme() {
  let xml;
  try {
    ({ xml } = await modeler.saveXML({ format: true }));
  } catch {
    return;
  }
  // The decision half needs the same treatment as the process half, and did
  // not have it: `mountDecisionEditor` destroys the old dmn-js, so anything on
  // that canvas not already written back by an `onChange` capture went with
  // it — silently, and into a pane left blank in process mode.
  await captureDecisionQueued();
  modeler.destroy();
  modeler = new Modeler({ container: ui.canvas, bpmnRenderer: rendererColors() });
  wireModeler();

  mountDecisionEditor();
  setMode('process');
  renderDecisions();
  await importXml(xml);
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

  const verdict = checkModel(xml, state.manifest, await decisionXmls());
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
  renderTryIt();
}

function setVerdict(kind, text) {
  ui.verdict.className = `verdict verdict-${kind}`;
  ui.verdict.textContent = text;
}

/// Show an element *and* select it, so the panes below follow.
///
/// `focus` only marks and scrolls (it is shared with the read-only
/// inspector, which has nothing to select into). In an editor that is half
/// the job: clicking a `conditions-feel-subset` diagnostic has to put the
/// offending sequence flow in the Element pane, or the diagnostic tells you
/// what is wrong and then gives you no way to fix it.
function reveal(elementId) {
  const element = modeler.get('elementRegistry').get(elementId);
  if (!element) return;
  focus(modeler, elementId);
  modeler.get('selection').select(element);
}

function renderDiagnostics(diagnostics) {
  ui.diagnostics.replaceChildren();
  const errors = diagnostics.filter((d) => d.severity === 'error').length;
  const warnings = diagnostics.length - errors;
  ui.diagnosticsTitle.textContent = diagnostics.length
    ? `Diagnostics — ${errors} error${errors === 1 ? '' : 's'}` +
      (warnings ? `, ${warnings} warning${warnings === 1 ? '' : 's'}` : '')
    : 'Diagnostics';
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
      item.addEventListener('click', () => reveal(d.element));
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
    // Named for the canvas in front of you. "Select an element" on the
    // decision canvas invites a click that does nothing, because the selection
    // this reads is bpmn-js's.
    ui.wiring.append(
      el(
        'p',
        'empty',
        state.mode === 'decisions'
          ? 'Bindings attach to process elements — pick one under Process.'
          : 'Select an element to bind it.'
      )
    );
    renderIndexes();
    return;
  }
  const bo = element.businessObject;
  const id = bo.id;
  const type = bo.$type;

  const wantsTopic = type === 'bpmn:ServiceTask';
  const wantsDecision = type === 'bpmn:BusinessRuleTask';
  const wantsCorrelation =
    type === 'bpmn:ReceiveTask' ||
    (bo.eventDefinitions ?? []).some((d) => d.$type === 'bpmn:MessageEventDefinition');

  if (!wantsTopic && !wantsCorrelation && !wantsDecision) {
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
      'empty means the default: the topic is the element id',
      state.covered ?? []
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
            : `no handler or declared topic named '${effective}' — the server covers: ` +
              `${state.covered.join(', ') || 'nothing'}`
      )
    );
    ui.wiring.append(row);
  }

  if (wantsDecision) {
    const bound = state.manifest.decisions[id] ?? { decision: '', result: '' };
    // The invocables the *bundle* exposes. Unlike topics this needs no
    // server: the artifacts are part of the deployment, so the editor knows
    // the whole answer.
    const invocables = (state.lastVerdict?.invocables ?? []).map((i) => i.name);
    ui.wiring.append(
      bindingRow(
        'decision',
        bound.decision,
        invocables[0] ?? 'Discount',
        (value) => {
          state.manifest = setDecisionBinding(state.manifest, id, 'decision', value);
          syncManifestBox();
          scheduleCheck();
        },
        invocables.length
          ? `bundled decisions: ${invocables.join(', ')}`
          : 'no DMN artifacts in this deployment yet — add one under Decisions',
        invocables
      )
    );
    ui.wiring.append(
      bindingRow(
        'result path',
        bound.result,
        'order.discount',
        (value) => {
          state.manifest = setDecisionBinding(state.manifest, id, 'result', value);
          syncManifestBox();
          scheduleCheck();
        },
        'where the answer lands in the variable document; a FEEL qualified name'
      )
    );
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

function bindingRow(label, value, placeholder, commit, hint, options) {
  const row = el('div', 'prop');
  row.append(el('span', 'prop-label', label));
  const input = el('input', 'prop-input');
  input.value = value;
  input.placeholder = placeholder;
  // The covered set is the list of names that can possibly work, and the
  // editor has it. Offering it turns "invent a topic and wonder why the
  // error stayed" into a pick from what exists.
  if (options?.length) {
    const listId = 'rbpmn-covered-topics';
    let list = document.getElementById(listId);
    if (!list) {
      list = el('datalist');
      list.id = listId;
      document.body.append(list);
    }
    list.replaceChildren(
      ...options.map((option) => {
        const node = el('option');
        node.value = option;
        return node;
      })
    );
    input.setAttribute('list', listId);
  }
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
  input.value = (state.manifest.indexes ?? []).map(formatIndexField).join(', ');
  // Top-level names only, and the placeholder used to show two dotted paths
  // that deploy refuses: an index is a single segment (`parse_qname` with
  // `path.len() == 1`), because it becomes a real database index on one JSONB
  // field. A placeholder is an example, and an example deploy rejects is a
  // bug report waiting to happen.
  //
  // `field:shared` is the compact spelling of the cross-definition scope, so a
  // scope set in the JSON survives a round trip through this box rather than
  // being silently flattened back to the default.
  input.placeholder = 'status, tier, order_no:shared';
  const commit = () => {
    let fields;
    try {
      fields = input.value
        .split(',')
        .map((f) => f.trim())
        .filter(Boolean)
        .map(parseIndexField);
    } catch (e) {
      // An unknown scope is refused, never defaulted — the same answer deploy
      // gives. The text stays in the box so the typo is visible and editable.
      ui.manifestError.hidden = false;
      ui.manifestError.textContent = e.message;
      return;
    }
    ui.manifestError.hidden = true;
    state.manifest = { ...state.manifest, indexes: fields };
    syncManifestBox();
    scheduleCheck();
  };
  input.addEventListener('blur', commit);
  row.append(input);
  row.append(
    el('span', 'prop-hint', 'optional: top-level variables fields the application filters tasks by')
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
  // application mounted it under — but relative to it *as a directory*.
  // Plain `new URL('api/…', baseURI)` resolves against the parent when the
  // path has no trailing slash, so a document at /ui/editor asked for
  // /ui/api/… and got a 404. Both spellings of the mount serve the document,
  // so both have to resolve the same way.
  //
  // Built from `document.baseURI`, never `location.origin`: on file:// the
  // origin is the string "null", and `new URL(path, "null")` throws — which
  // reached the user as "cannot reach the server: Invalid URL" rather than
  // the truth, which is that a document opened from disk has no server.
  const base = new URL(document.baseURI);
  base.search = '';
  base.hash = '';
  if (!base.pathname.endsWith('/')) base.pathname += '/';
  return new URL('api/environment', base).href;
}

async function loadEnvironment() {
  ui.environment.replaceChildren(el('p', 'empty', 'loading…'));
  try {
    const response = await fetch(environmentUrl(), { headers: { accept: 'application/json' } });
    if (!response.ok) {
      renderEnvironment(null, `server answered ${response.status}`);
      return;
    }
    // A 200 that is not JSON means this document was served from a path whose
    // API sibling does not exist, and something upstream answered with a page
    // instead. Say that, rather than letting JSON.parse fail on markup.
    const contentType = response.headers.get('content-type') ?? '';
    if (!contentType.includes('json')) {
      renderEnvironment(
        null,
        `expected JSON at ${environmentUrl()} but got ${contentType || 'no content type'} — ` +
          'is the environment API mounted beside this document?'
      );
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

/// Install `EXAMPLE` as the working deployment. Boot only — there is no button
/// for it, because "get the example back" is a page reload and an affordance
/// that silently replaces unsaved work is not worth the toolbar space.
///
/// Same order as `newDiagram`, and for the same reason: the manifest and the
/// decisions have to be in place *before* `importXml`, because importing runs
/// the check, and a check that ran against the old working set would report
/// this deployment as unbound for one render.
async function loadExample() {
  state.fileName = EXAMPLE.fileName;
  state.manifest = parseManifest(EXAMPLE.bindings);
  state.decisions = EXAMPLE.decisions.map((d) => ({ ...d }));
  state.activeDecision = 0;
  setMode('process');
  renderDecisions();
  syncManifestBox();
  await importXml(EXAMPLE.bpmn);
}

async function newDiagram() {
  state.fileName = 'process.bpmn';
  state.manifest = emptyManifest();
  // "New" means a new *deployment*, which is why the manifest is emptied — and
  // the decisions are part of that deployment, so they go with it. Leaving
  // them behind put the previous process's decisions into the next bundle.
  //
  // `openBpmn` deliberately does the opposite and keeps both: opening a `.bpmn`
  // replaces the process within the deployment you are holding, which is what
  // makes re-opening an edited process keep its bindings. Stale bindings and
  // stale decisions are then a *validation* answer, not a silent reset.
  //
  // Note this lands on `STARTER`, not on the example the editor booted with —
  // see the two constants at the top.
  decisionEditor?.destroy();
  state.decisions = [];
  state.activeDecision = 0;
  setMode('process');
  renderDecisions();
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

/// The whole deployment in one file: exactly the body `POST /v1/definitions`
/// takes, so what is exported here is what deploy consumes. The individual
/// files stay the primary artifacts — they are what a git diff can show — and
/// this is for handing the deployment over.
async function saveBundle() {
  await captureDecisionQueued();
  const { xml } = await modeler.saveXML({ format: true });
  const bundle = buildBundle({
    bpmn: xml,
    manifest: serializeManifest(state.manifest),
    decisions: state.decisions,
  });
  download(bundleNameFor(state.fileName), bundle, 'application/json');
}

async function openBundle() {
  const file = await openFile('.json,application/json');
  if (!file) return;
  let parsed;
  let manifest;
  try {
    parsed = parseBundle(file.text);
    // Inside the guard, not after it. `parseBundle` checks that `bindings` is
    // an *object*; `parseManifest` is what checks it is a manifest, and it
    // throws — so `{"bpmn": "...", "bindings": {"topics": 5}}` passed the first
    // and blew up in the second, after `state.fileName` had already moved.
    // An unhandled rejection is not a diagnostic, and it left the editor
    // half-loaded with nothing on screen to say so.
    manifest = parseManifest(parsed.manifest);
  } catch (e) {
    setVerdict('error', 'bundle rejected');
    renderDiagnostics([
      { severity: 'error', rule: 'bundle', element: file.name, message: e.message },
    ]);
    return;
  }
  state.fileName = file.name.replace(/\.bundle\.json$/i, '.bpmn');
  state.manifest = manifest;
  decisionEditor?.destroy();
  state.decisions = parsed.decisions.map((d, i) => ({
    name: d.name || decisionFileName(d.xml, i),
    xml: d.xml,
  }));
  state.activeDecision = 0;
  syncManifestBox();
  setMode('process');
  renderDecisions();
  await importXml(parsed.bpmn);
}

/// Fit the model to the canvas with the palette kept off it.
///
/// `zoom('fit-viewport')` anchors the model's top-left corner at the
/// viewport's, and the palette floats over exactly that corner — so the first
/// element of every model opened here rendered underneath it. The example
/// deployment is what made it loud, its start event disappearing behind the
/// palette on the first screen, but it was never specific to that model.
///
/// The gutter is measured rather than assumed: the palette lays itself out in
/// two columns when the canvas is short and one when it is tall, so either
/// constant would be wrong at one of the two.
///
/// `fitted` is the ceiling on the result, and it is load-bearing rather than
/// defensive: fit-viewport deliberately refuses to zoom *in* past 100%, so a
/// three-element model sits at 1.0 with room to spare. Recomputing a scale
/// from the reduced box would blow it up to fill the space instead — clearing
/// the palette must move a model, never magnify one.
function fitClearOfPalette() {
  const canvas = modeler.get('canvas');
  canvas.zoom('fit-viewport');
  const palette = ui.canvas.querySelector('.djs-palette');
  const { inner, outer, scale: fitted } = canvas.viewbox();
  if (!palette || !inner.width || !inner.height) return;
  const left = palette.getBoundingClientRect().right - ui.canvas.getBoundingClientRect().left + 16;
  const top = 16;
  const scale = Math.min(
    fitted,
    (outer.width - left - 24) / inner.width,
    (outer.height - top - 24) / inner.height
  );
  // A hidden canvas measures zero, which is every dimension here at once. It
  // happens — "Open .bpmn" is in the toolbar and the toolbar is reachable from
  // the decisions pane — and leaving fit-viewport's own result alone is the
  // right answer, not a viewbox computed from a negative scale.
  if (!(scale > 0)) return;
  canvas.viewbox({
    x: inner.x - left / scale,
    y: inner.y - top / scale,
    width: outer.width / scale,
    height: outer.height / scale,
  });
}

async function importXml(xml) {
  try {
    // Hand-written models carry no diagram; laying one out beats rendering an
    // empty canvas.
    const renderable = await ensureDi(xml);
    await modeler.importXML(renderable);
    fitClearOfPalette();
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


// ---------------------------------------------------------------- decisions
//
// The artifacts a deployment carries. They are edited in the same window as
// the process because that is where the question comes up — a business-rule
// task is bound to a decision, and switching apps to answer "what does this
// decide?" is how a manifest ends up pointing at nothing.

let decisionEditor;

function setMode(mode) {
  state.mode = mode;
  ui.canvas.hidden = mode !== 'process';
  ui.dmnCanvas.hidden = mode !== 'decisions';
  ui.modeProcess.classList.toggle('btn-mode-on', mode === 'process');
  ui.modeDecisions.classList.toggle('btn-mode-on', mode === 'decisions');

  // The Element pane reads a *bpmn-js* selection, so on the decision canvas it
  // is inert: clicking a DMN shape leaves it showing whatever process element
  // was last selected, or nothing, with no way to tell which. Gone is clearer
  // than stale — a pane that never answers is worse than one that is not
  // there. The rest of the column is not process-specific and stays.
  ui.elementPane.hidden = mode !== 'process';
  // The wiring pane *does* stay: a decision is usually being edited because of
  // a business-rule task, and keeping that task's bindings in view is the
  // point. Only its no-selection line is mode-specific, so re-render it.
  renderWiring();
}

/// Switch to the decisions pane *and* show one — the only way in.
///
/// `setMode` used to start a `showDecision` of its own, unawaited, so every
/// caller that then awaited its own `showDecision` had two of them in flight
/// against one dmn-js: two captures and two imports interleaving, which could
/// leave the canvas showing one artifact while `current` named another.
async function goToDecision(index) {
  setMode('decisions');
  await showDecision(index);
}

/// One decision transition at a time.
///
/// Every path here imports into a single dmn-js instance and writes the
/// previous artifact back on the way out, so two overlapping transitions can
/// interleave a capture with an import and cross-write two artifacts. Clicks
/// are asynchronous and arrive whenever they like; serialising is cheaper than
/// proving no pair of callers can ever race.
let decisionQueue = Promise.resolve();
function serializeDecisionWork(work) {
  // `then(work, work)` so a failed transition does not skip the next one, and
  // the queue itself is the *settled* promise: it must survive a failure —
  // a decision that will not open is reported, not a reason to wedge every
  // later switch.
  const next = decisionQueue.then(work, work);
  decisionQueue = next.then(
    () => {},
    () => {}
  );
  return next;
}

/// Report anything a handler throws, instead of losing it.
///
/// Every failure this editor knows how to describe goes through the diagnostics
/// pane; one that escapes a click handler went nowhere at all.
function handle(result) {
  Promise.resolve(result).catch((e) => {
    setVerdict('error', 'editor error');
    renderDiagnostics([
      { severity: 'error', rule: 'editor', element: '', message: String(e?.message ?? e) },
    ]);
  });
}

/// `captureDecision`, queued.
///
/// Serialising `showDecision` alone was not enough: a capture reads
/// `decisionEditor.current` synchronously and *then* awaits `saveXML`, which
/// reads whatever definitions the canvas holds by the time it runs. Click a
/// decision and hit Remove before its import resolves and those two disagree —
/// the new artifact's XML written over the old one's, silently. Every path
/// that captures outside a transition goes through here.
function captureDecisionQueued() {
  return serializeDecisionWork(captureDecision);
}

/// The current XML of every artifact — the *live* one for whichever is on
/// screen, so validation sees edits rather than the last import.
///
/// Substituted by **identity**, not by index. `currentXml` reads the canvas,
/// and the canvas holds `decisionEditor.current`, which is not
/// `state.decisions[state.activeDecision]` whenever an import failed: opening
/// a malformed `.dmn` moves the index and leaves the canvas on the previous
/// artifact, and keying by index then validated the previous decision's XML in
/// the broken one's place. The verdict came back clean and `saveBundle` wrote
/// the bytes the server refuses — the exact verdict split this editor exists
/// to prevent.
async function decisionXmls() {
  const shown = decisionEditor?.current;
  if (!shown) return state.decisions.map((d) => d.xml);
  const live = await decisionEditor.currentXml();
  return state.decisions.map((d) => (live && d === shown ? live : d.xml));
}

/// Pull the on-screen artifact's XML back into the working set, so switching
/// away from a decision does not lose what was typed into it.
///
/// It writes to `decisionEditor.current` — the artifact the canvas actually
/// holds — and deliberately **not** to `state.decisions[state.activeDecision]`.
/// The two come apart exactly when it matters: removing the decision on screen
/// filters the array first, so that index then names a *different* artifact,
/// and the capture copied the removed decision's XML over its neighbour.
async function captureDecision() {
  const artifact = decisionEditor?.current;
  if (!artifact) return;
  const live = await decisionEditor.currentXml();
  if (live) artifact.xml = live;
}

function showDecision(index) {
  return serializeDecisionWork(() => showDecisionNow(index));
}

/// The body, for callers already holding the queue. Never call it from outside
/// one — that is what `showDecision` is for.
async function showDecisionNow(index) {
  await captureDecision();
  state.activeDecision = index;
  const artifact = state.decisions[index];
  renderDecisions();
  if (!artifact) {
    await decisionEditor?.show(null);
    return;
  }
  try {
    await decisionEditor.show(artifact);
  } catch (e) {
    setVerdict('error', 'decision will not open');
    renderDiagnostics([
      { severity: 'error', rule: 'dmn-validates', element: artifact.name, message: e.message },
    ]);
  }
}

function renderDecisions() {
  ui.decisions.replaceChildren();
  const list = el('ul', 'decision-list');
  state.decisions.forEach((artifact, i) => {
    const item = el('li', i === state.activeDecision ? 'decision on' : 'decision');
    const open = button(artifact.name, () => goToDecision(i), 'btn btn-link');
    item.append(open);
    item.append(
      button('Save', () => saveDecision(i), 'btn btn-small'),
      button('Remove', () => removeDecision(i), 'btn btn-small')
    );
    list.append(item);
  });
  if (!state.decisions.length) {
    ui.decisions.append(
      el('p', 'empty', 'No decisions. A business-rule task needs one to bind to.')
    );
  } else {
    ui.decisions.append(list);
  }
  const actions = el('div', 'row');
  actions.append(
    button('New decision', newDecision, 'btn btn-small'),
    button('Open .dmn', openDecision, 'btn btn-small')
  );
  ui.decisions.append(actions);
  renderTryIt();
}

async function newDecision() {
  const name = `Decision${state.decisions.length + 1}`;
  state.decisions = addDecision(state.decisions, starterDecision(name), `${name}.dmn`);
  await goToDecision(state.decisions.length - 1);
  scheduleCheck();
}

async function openDecision() {
  const file = await openFile('.dmn,application/xml,text/xml');
  if (!file) return;
  state.decisions = addDecision(state.decisions, file.text, file.name);
  await goToDecision(state.decisions.length - 1);
  scheduleCheck();
}

async function saveDecision(index) {
  await captureDecisionQueued();
  const artifact = state.decisions[index];
  if (artifact) download(artifact.name, artifact.xml, 'application/xml');
}

async function removeDecision(index) {
  // The **whole** removal is queued, not just its capture: it rewrites
  // `state.decisions` and `state.activeDecision`, and a transition still in
  // flight resolves its own index against that array.
  await serializeDecisionWork(async () => {
    // Capture first, while the indices still mean what the canvas means, and
    // give up the canvas before filtering: the artifact on screen may be the
    // one being removed, and a capture after the filter wrote its XML into
    // whatever artifact inherited its index.
    await captureDecision();
    const removed = state.decisions[index];
    if (decisionEditor?.current === removed) decisionEditor.destroy();
    state.decisions = state.decisions.filter((_, i) => i !== index);
    state.activeDecision = Math.max(0, Math.min(state.activeDecision, state.decisions.length - 1));
    if (!state.decisions.length) {
      setMode('process');
      renderDecisions();
    } else {
      await showDecisionNow(state.activeDecision);
    }
  });
  scheduleCheck();
}

// ------------------------------------------------------------------ try it
//
// The same evaluator the engine runs, against sample input the user types.
// Not a debugger and not a test runner: one decision, one input, one answer —
// enough to know whether a table says what its author meant.

function renderTryIt() {
  ui.tryIt.replaceChildren();
  const invocables = state.lastVerdict?.invocables ?? [];
  if (!invocables.length) {
    ui.tryIt.append(el('p', 'empty', 'No bundled decision to run.'));
    return;
  }
  const select = el('select', 'input');
  invocables.forEach((i, index) => {
    const option = el('option', null, i.name);
    option.value = String(index);
    select.append(option);
  });
  const input = el('textarea', 'code code-small code-try-input');
  input.spellcheck = false;
  // Fits *both* decisions the editor can produce without a file being opened:
  // the example's `Triage` table (`order.total` and `risk.score`) and
  // `starterDecision`'s literal expression (`order.total`). So Evaluate answers
  // on the first click either way — a default that does not fit the decision in
  // front of you answers null, and null is exactly the trap this pane is for.
  //
  // Keeping one document is what forced the two to share an input name; the
  // alternative was a default that silently fits only whichever decision the
  // list happens to be showing.
  input.value =
    ui.tryItInput?.value ?? '{\n  "order": { "total": 250 },\n  "risk": { "score": 82 }\n}';
  ui.tryItInput = input;
  const output = el('pre', 'try-output', '');
  const run = button(
    'Evaluate',
    async () => {
      const invocable = invocables[Number(select.value)];
      try {
        const result = evaluateDecision(await decisionXmls(), invocable, input.value);
        output.className = `try-output try-${result.outcome}`;
        output.textContent = describeOutcome(result);
      } catch (e) {
        output.className = 'try-output try-error';
        output.textContent = String(e.message ?? e);
      }
    },
    'btn btn-small'
  );
  ui.tryIt.append(select, input, run, output);
}

/// A null is an answer, not a failure — dsntk cannot tell a legal "no rule
/// matched" from a broken evaluation, so the reason is shown as explanation
/// and never as a verdict (docs/dmn.md, "What P1 measured").
function describeOutcome(result) {
  switch (result.outcome) {
    case 'value':
      return JSON.stringify(result.value, null, 2);
    case 'null':
      return result.reason
        ? `null\n\n${result.reason}\n\n(a null is an answer: an incomplete table and a bad input look the same here)`
        : 'null';
    case 'unrepresentable':
      return `no usable answer: ${result.reason}`;
    default:
      return result.reason ?? 'no answer';
  }
}

// --------------------------------------------------------------------- boot

async function main() {
  const root = document.getElementById('rbpmn-root');
  buildLayout(root);
  renderEnvironment(null, null);

  modeler = new Modeler({ container: ui.canvas, bpmnRenderer: rendererColors() });
  wireModeler();

  mountDecisionEditor();
  setMode('process');
  renderDecisions();


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

  onThemeChange(remountForTheme);

  await initValidator();
  await loadExample();
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', main);
} else {
  main();
}
