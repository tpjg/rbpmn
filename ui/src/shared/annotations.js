// Generic diagram annotation layer: decorates a bpmn-js canvas from a list of
// `{ elementId, kind, payload }` records. Diagnostics were its first consumer
// (phase 0-B), live token state its second (phase 2) — `kind` stays an open
// set: it becomes a CSS suffix (`rbpmn-mark-<kind>`) and a badge class, and
// nothing here enumerates it.
//
// Unlike the playground's original, this tracks what it applied. The
// playground got away with a hardcoded removal list because every load
// re-imported the XML onto a fresh canvas; a long-lived editor canvas
// re-annotated on every keystroke does not.

const OVERLAY_TYPE = 'rbpmn-annotation';

/// viewer -> [[elementId, kind], ...] applied by the last annotate() call.
const applied = new WeakMap();

export function annotate(viewer, annotations) {
  clear(viewer);
  const overlays = viewer.get('overlays');
  const canvas = viewer.get('canvas');
  const elementRegistry = viewer.get('elementRegistry');

  const byElement = new Map();
  for (const a of annotations) {
    if (!byElement.has(a.elementId)) byElement.set(a.elementId, []);
    byElement.get(a.elementId).push(a);
  }

  const marks = [];
  const missing = [];
  for (const [elementId, items] of byElement) {
    const element = elementRegistry.get(elementId);
    if (!element || element === canvas.getRootElement()) {
      missing.push(...items);
      continue;
    }
    for (const { kind } of items) {
      canvas.addMarker(elementId, `rbpmn-mark-${kind}`);
      marks.push([elementId, kind]);
    }
    const kinds = [...new Set(items.map((a) => a.kind))];
    const badge = document.createElement('div');
    badge.className = `rbpmn-badge rbpmn-badge-${kinds[0]}`;
    badge.textContent = String(items.length);
    // textContent, not innerHTML: payload titles carry model and runtime
    // strings (element names, handler failure messages).
    badge.title = items.map((a) => a.payload?.title ?? a.kind).join('\n');
    overlays.add(elementId, OVERLAY_TYPE, {
      position: { top: -10, right: 10 },
      html: badge,
    });
  }
  applied.set(viewer, marks);
  return { missing };
}

export function clear(viewer) {
  viewer.get('overlays').remove({ type: OVERLAY_TYPE });
  const canvas = viewer.get('canvas');
  const elementRegistry = viewer.get('elementRegistry');
  for (const [elementId, kind] of applied.get(viewer) ?? []) {
    if (elementRegistry.get(elementId)) canvas.removeMarker(elementId, `rbpmn-mark-${kind}`);
  }
  applied.set(viewer, []);
}

export function focus(viewer, elementId) {
  const elementRegistry = viewer.get('elementRegistry');
  const canvas = viewer.get('canvas');
  const element = elementRegistry.get(elementId);
  if (!element) return false;

  for (const other of elementRegistry.getAll()) {
    canvas.removeMarker(other, 'rbpmn-mark-selected');
  }

  // The element may live on a subprocess drill-down plane (phase 6 makes
  // that the normal case, not the exception).
  const root = canvas.findRoot(element) ?? canvas.getRootElement();
  if (root !== canvas.getRootElement()) {
    canvas.setRootElement(root);
  }
  canvas.addMarker(element, 'rbpmn-mark-selected');
  canvas.scrollToElement(element);
  return true;
}
