// The read-only instance inspector.
//
// No API, no credentials, no writes: everything this document shows was
// inlined into it by `rbpmn_ui::render_inspection`, which is why the
// embedding application's authorization check is the only gate that exists.
// There is deliberately no button that changes anything — repair, retry and
// migration are designed APIs, and a UI is never the reason one ships early.

import NavigatedViewer from 'bpmn-js/lib/NavigatedViewer';
import 'bpmn-js/dist/assets/diagram-js.css';
import 'bpmn-js/dist/assets/bpmn-js.css';
import 'bpmn-js/dist/assets/bpmn-font/css/bpmn.css';
import './inspector.css';

import { annotate, focus } from '../shared/annotations.js';
import { ensureDi } from '../shared/layout.js';
import { clear, el, field, jsonTree, section } from '../shared/dom.js';
import { describeElement } from '../shared/model-facts.js';
import { diagnose } from './diagnosis.js';
import { onThemeChange, rendererColors } from '../shared/theme.js';

/// One trace line. `display` is the golden-trace format and therefore stable
/// API, so a reason that may still be reworded lives in `detail` instead —
/// which means dropping `detail` makes an incident say only *that* it failed.
/// A `timer-resolve-failed` reads as "timer-resolve-failed t1" without it.
function traceLine(event) {
  const item = el('li', null, event.display);
  if (event.detail) item.append(el('div', 'trace-detail', event.detail));
  return item;
}

function readData() {
  const node = document.getElementById('rbpmn-data');
  if (!node) throw new Error('no inspection data in this document');
  return JSON.parse(node.textContent);
}

function build(root, data) {
  const layout = el('div', 'layout');

  const header = el('header', 'topbar');
  const title = el('div', 'title');
  title.append(
    el('h1', null, data.definitionKey),
    el('span', `status status-${data.status}`, data.status)
  );
  const idLine = el('div', 'instance-id', data.id);
  idLine.title = 'instance id';
  header.append(title, idLine);

  const diagnosisBox = el('div', 'diagnosis');
  const canvasWrap = el('div', 'canvas-wrap');
  const canvas = el('div', 'canvas');
  const canvasNote = el('div', 'canvas-note');
  canvasNote.hidden = true;
  canvasWrap.append(canvas, canvasNote);

  const side = el('aside', 'side');

  layout.append(header, diagnosisBox, canvasWrap, side);
  root.append(layout);
  return { canvas, canvasNote, diagnosisBox, side };
}

/// The line an operator actually arrived for: not "where is the token" but
/// "what went wrong". Everything it needs is already in the payload.
/// Returns the element the headline names, so the caller can open on it
/// without running the whole diagnosis a second time.
function renderDiagnosis(box, data, viewer) {
  clear(box);
  const { severity, headline, detail, elementId } = diagnose(data);
  box.className = `diagnosis diagnosis-${severity}`;
  const text = el('div', 'diagnosis-text');
  text.append(el('strong', null, headline));
  if (detail) text.append(el('span', 'diagnosis-detail', detail));
  box.append(text);
  if (elementId) {
    const link = el('button', 'link', `show ${elementId}`);
    link.addEventListener('click', () => focus(viewer, elementId));
    box.append(link);
  }
  return elementId;
}

/// Every annotation the diagram carries, derived from runtime rows only.
function annotationsFor(data) {
  const marks = [];
  for (const token of data.tokens) {
    marks.push({
      elementId: token.elementId,
      kind: token.waitKind === 'incident' ? 'error' : 'token',
      payload: { title: `token — ${token.waitKind}` },
    });
  }
  for (const item of data.workItems) {
    if (item.state === 'available' || item.state === 'locked') {
      marks.push({
        elementId: item.elementId,
        kind: 'work',
        payload: { title: `work item ${item.state} (${item.kind} / ${item.topic})` },
      });
    } else if (item.state === 'failed') {
      marks.push({
        elementId: item.elementId,
        kind: 'error',
        payload: { title: `work item failed: ${item.lastFailure ?? 'no detail recorded'}` },
      });
    }
  }
  for (const timer of data.timers) {
    marks.push({
      elementId: timer.elementId,
      kind: 'timer',
      payload: { title: `timer ${timer.dueSpec} — due ${timer.dueAt}` },
    });
  }
  for (const sub of data.subscriptions) {
    marks.push({
      elementId: sub.elementId,
      kind: 'message',
      payload: { title: `awaiting ${sub.messageName} (key ${sub.correlationKey})` },
    });
  }
  return marks;
}

/// The element pane: static model facts, the wiring the manifest supplies,
/// the runtime rows sitting on this element, and its slice of the trace.
function renderElement(container, data, viewer, elementId) {
  clear(container);
  if (!elementId) {
    container.append(el('p', 'empty', 'Select an element to inspect it.'));
    return;
  }
  const registry = viewer.get('elementRegistry');
  const element = registry.get(elementId);
  const bo = element?.businessObject;

  const head = el('div', 'element-head');
  head.append(
    el('span', 'element-type', bo ? bo.$type.replace(/^bpmn:/, '') : 'unknown'),
    el('span', 'element-id', elementId)
  );
  if (bo?.name) head.append(el('div', 'element-name', bo.name));
  container.append(head);

  // --- static model facts, straight off the moddle object bpmn-js already
  // holds. No re-parse, no extra dependency.
  const facts = describeElement(bo);
  if (facts.length) {
    const { wrap, body } = section('Model');
    for (const [label, value] of facts) body.append(field(label, value));
    container.append(wrap);
  }

  // --- wiring. The manifest is deliberately absent from the XML, so this is
  // the only place a reader can recover it — and for elements the token has
  // not reached, the only place at all.
  const wiring = wiringFor(data, bo, elementId);
  if (wiring.length) {
    const { wrap, body } = section('Wiring');
    for (const [label, value, note] of wiring) body.append(field(label, value, { title: note }));
    container.append(wrap);
  }

  // --- runtime state at this element
  const runtime = [];
  for (const t of data.tokens.filter((t) => t.elementId === elementId)) {
    runtime.push(['token', `${t.waitKind} (scope ${t.scopeNo})`]);
  }
  for (const w of data.workItems.filter((w) => w.elementId === elementId)) {
    runtime.push([`work item (${w.kind})`, `${w.state} · topic ${w.topic} · retries ${w.retries}`]);
    if (w.lastFailure) runtime.push(['last failure', w.lastFailure]);
  }
  for (const t of data.timers.filter((t) => t.elementId === elementId)) {
    runtime.push(['timer', `${t.dueSpec} — due ${t.dueAt}`]);
  }
  for (const s of data.subscriptions.filter((s) => s.elementId === elementId)) {
    runtime.push(['subscription', `${s.messageName} — key ${s.correlationKey}`]);
  }
  const { wrap: runtimeWrap, body: runtimeBody } = section('Runtime');
  if (runtime.length) {
    for (const [label, value] of runtime) runtimeBody.append(field(label, value));
  } else {
    runtimeBody.append(el('p', 'empty', 'nothing parked here'));
  }
  container.append(runtimeWrap);

  // --- this element's slice of the trace
  const slice = data.events.filter((e) => e.elementId === elementId);
  const { wrap: traceWrap, body: traceBody } = section(`Trace (${slice.length})`, {
    open: slice.length > 0,
  });
  if (slice.length) {
    const list = el('ol', 'trace');
    for (const event of slice) list.append(traceLine(event));
    traceBody.append(list);
  } else {
    traceBody.append(el('p', 'empty', 'no events for this element'));
  }
  container.append(traceWrap);
}

/// What the manifest says about this element, including the default nobody
/// wrote down: an unmapped service task runs on a topic named after itself.
function wiringFor(data, bo, elementId) {
  const out = [];
  const bindings = data.bindings ?? {};
  const type = bo?.$type ?? '';
  const topic = bindings.topics?.[elementId];
  if (type === 'bpmn:ServiceTask') {
    out.push(
      topic
        ? ['topic', topic, 'bound in the deployment manifest']
        : ['topic', `${elementId} (default)`, 'no mapping in the manifest: the topic is the element id']
    );
  } else if (topic) {
    out.push(['topic', topic, 'bound in the deployment manifest']);
  }
  const correlation = bindings.correlations?.[elementId];
  if (correlation) out.push(['correlation key', correlation, 'FEEL path into the variables']);
  return out;
}

async function main() {
  const data = readData();
  const root = document.getElementById('rbpmn-root');
  const { canvas, canvasNote, diagnosisBox, side } = build(root, data);

  const viewer = new NavigatedViewer({ container: canvas, bpmnRenderer: rendererColors() });

  // Renderer colours are fixed at construction, so a theme flip needs a fresh
  // render. Reloading is the honest way to get one here: this document is
  // read-only and carries its own data, so there is nothing to lose — no
  // edits, no undo stack, no unsaved anything.
  onThemeChange(() => location.reload());

  const elementPane = el('div', 'element-pane');
  const paneSection = el('section', 'pane');
  paneSection.append(el('h2', null, 'Element'), elementPane);
  side.append(paneSection);

  const varsSection = el('section', 'pane');
  const { wrap: varsWrap, body: varsBody } = section('Variables');
  varsBody.append(jsonTree(data.variables));
  varsSection.append(el('h2', null, 'Instance'), varsWrap);

  const { wrap: bindWrap, body: bindBody } = section('Deployed manifest', { open: false });
  bindBody.append(jsonTree(data.bindings ?? {}));
  varsSection.append(bindWrap);

  const { wrap: traceWrap, body: traceBody } = section(`Trace (${data.events.length})`, {
    open: false,
  });
  const traceList = el('ol', 'trace');
  for (const event of data.events) {
    const item = traceLine(event);
    if (event.elementId) {
      item.classList.add('clickable');
      item.addEventListener('click', () => {
        focus(viewer, event.elementId);
        renderElement(elementPane, data, viewer, event.elementId);
      });
    }
    traceList.append(item);
  }
  traceBody.append(traceList);
  varsSection.append(traceWrap);
  side.append(varsSection);

  try {
    const renderable = await ensureDi(data.bpmnXml);
    const { warnings } = await viewer.importXML(renderable);
    viewer.get('canvas').zoom('fit-viewport');
    if (warnings.length) {
      canvasNote.hidden = false;
      canvasNote.textContent = `diagram imported with ${warnings.length} warning(s)`;
    }
    const { missing } = annotate(viewer, annotationsFor(data));
    if (missing.length) {
      canvasNote.hidden = false;
      canvasNote.textContent =
        `${missing.length} marker(s) target elements not on this diagram — ` +
        `they are listed in the panels`;
    }
    viewer.get('eventBus').on('element.click', ({ element }) => {
      const id = element.id === viewer.get('canvas').getRootElement().id ? null : element.id;
      renderElement(elementPane, data, viewer, id);
    });
  } catch (e) {
    canvasNote.hidden = false;
    canvasNote.textContent = `diagram cannot be rendered: ${e.message}`;
  }

  // Open on the problem. Whoever followed a link here was sent because
  // something is wrong with this instance, so making them hunt for the
  // element the headline already names is a wasted click. Falls back to the
  // empty pane when there is nothing to point at.
  const elementId = renderDiagnosis(diagnosisBox, data, viewer);
  renderElement(elementPane, data, viewer, elementId);
  if (elementId) focus(viewer, elementId);
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', main);
} else {
  main();
}
