import NavigatedViewer from 'bpmn-js/lib/NavigatedViewer';
import 'bpmn-js/dist/assets/diagram-js.css';
import 'bpmn-js/dist/assets/bpmn-js.css';
import 'bpmn-js/dist/assets/bpmn-font/css/bpmn.css';
import './style.css';

import init, { lint } from './wasm/rbpmn_wasm.js';
import { FIXTURES } from './fixtures.generated.js';
import { annotate, focus } from './annotations.js';
import { ensureDi } from './layout.js';

const viewer = new NavigatedViewer({ container: document.getElementById('canvas') });
const editor = document.getElementById('editor');
const fixtureList = document.getElementById('fixture-list');
const diagnosticList = document.getElementById('diagnostic-list');
const verdict = document.getElementById('verdict');
const canvasNote = document.getElementById('canvas-note');

let activeFixture = null;
let importSeq = 0;

function note(text) {
  canvasNote.hidden = !text;
  canvasNote.textContent = text ?? '';
}

function runLint(xml) {
  const result = JSON.parse(lint(xml));
  renderDiagnostics(result);
  return result;
}

function renderDiagnostics(result) {
  diagnosticList.replaceChildren();
  if (result.parseError) {
    verdict.textContent = 'not BPMN';
    verdict.className = 'rejected';
    const li = document.createElement('li');
    li.className = 'severity-error';
    li.innerHTML = `<span class="rule">parse error</span><div class="message"></div>`;
    li.querySelector('.message').textContent = result.parseError;
    diagnosticList.append(li);
    return;
  }
  verdict.textContent = result.ok ? 'would deploy' : 'rejected';
  verdict.className = result.ok ? 'ok' : 'rejected';
  if (!result.diagnostics.length) {
    const li = document.createElement('li');
    li.className = 'none';
    li.textContent = 'no diagnostics — model is clean';
    diagnosticList.append(li);
    return;
  }
  for (const d of result.diagnostics) {
    const li = document.createElement('li');
    li.className = `severity-${d.severity}`;
    const head = document.createElement('div');
    const rule = document.createElement('span');
    rule.className = 'rule';
    rule.textContent = d.rule;
    const el = document.createElement('span');
    el.className = 'element';
    el.textContent = ` @ ${d.element}`;
    head.append(rule, el);
    const message = document.createElement('div');
    message.className = 'message';
    message.textContent = d.message;
    li.append(head, message);
    li.addEventListener('click', () => focus(viewer, d.element));
    diagnosticList.append(li);
  }
}

async function renderDiagram(xml, result) {
  const seq = ++importSeq;
  try {
    // Hand-typed XML without DI gets an automatic layout, so the playground
    // stays the fastest way to author new fixtures.
    const renderable = await ensureDi(xml);
    if (seq !== importSeq) return;
    const { warnings } = await viewer.importXML(renderable);
    if (seq !== importSeq) return;
    viewer.get('canvas').zoom('fit-viewport');
    note(
      warnings.length
        ? `imported with ${warnings.length} warning(s) — broken references are part of some reject fixtures`
        : null
    );
    if (result && !result.parseError) {
      const annotations = result.diagnostics.map((d) => ({
        elementId: d.element,
        kind: d.severity,
        payload: { title: `${d.rule}: ${d.message}` },
      }));
      const { missing } = annotate(viewer, annotations);
      if (missing.length) {
        note(
          `${missing.length} diagnostic(s) target elements not on this diagram ` +
            `(scope-level or unresolvable ids) — see the list`
        );
      }
    }
  } catch (e) {
    if (seq !== importSeq) return;
    note(`diagram cannot be rendered: ${e.message}`);
  }
}

function setXml(xml, { updateEditor = true } = {}) {
  if (updateEditor) editor.value = xml;
  const result = runLint(xml);
  renderDiagram(xml, result);
}

function buildFixtureList() {
  for (const group of ['accept', 'reject']) {
    const wrap = document.createElement('div');
    wrap.className = 'fixture-group';
    for (const name of Object.keys(FIXTURES).filter((n) => n.startsWith(group))) {
      const button = document.createElement('button');
      button.className = 'fixture-item';
      button.dataset.name = name;
      const dot = document.createElement('span');
      dot.className = `dot dot-${group}`;
      button.append(dot, name.slice(group.length + 1).replace(/\.bpmn$/, ''));
      button.addEventListener('click', () => selectFixture(name));
      wrap.append(button);
    }
    fixtureList.append(wrap);
  }
}

function selectFixture(name) {
  activeFixture = name;
  for (const el of fixtureList.querySelectorAll('.fixture-item')) {
    el.classList.toggle('active', el.dataset.name === name);
  }
  setXml(FIXTURES[name]);
}

let debounce;
editor.addEventListener('input', () => {
  clearTimeout(debounce);
  debounce = setTimeout(() => setXml(editor.value, { updateEditor: false }), 300);
});

await init();
buildFixtureList();
selectFixture(Object.keys(FIXTURES)[0]);
