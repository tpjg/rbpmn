// Generic diagram annotation layer: decorates a bpmn-js canvas from a list of
// `{ elementId, kind, payload }` records. Diagnostics are the first consumer;
// live token state (phase 2's instance inspection mode) is designed to be the
// second — same component, no rework. `kind` is an open set: it becomes a CSS
// suffix (`rbpmn-mark-<kind>`) and a badge class, nothing here enumerates it.

const OVERLAY_TYPE = 'rbpmn-annotation';

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

  const missing = [];
  for (const [elementId, items] of byElement) {
    const element = elementRegistry.get(elementId);
    if (!element || element === canvas.getRootElement()) {
      missing.push(...items);
      continue;
    }
    for (const { kind } of items) {
      canvas.addMarker(elementId, `rbpmn-mark-${kind}`);
    }
    const kinds = [...new Set(items.map((a) => a.kind))];
    const badge = document.createElement('div');
    badge.className = `rbpmn-badge rbpmn-badge-${kinds[0]}`;
    badge.textContent = String(items.length);
    badge.title = items.map((a) => a.payload?.title ?? a.kind).join('\n');
    overlays.add(elementId, OVERLAY_TYPE, {
      position: { top: -10, right: 10 },
      html: badge,
    });
  }
  return { missing };
}

export function clear(viewer) {
  viewer.get('overlays').remove({ type: OVERLAY_TYPE });
  const canvas = viewer.get('canvas');
  for (const element of viewer.get('elementRegistry').getAll()) {
    for (const marker of ['error', 'warn', 'selected']) {
      canvas.removeMarker(element, `rbpmn-mark-${marker}`);
    }
  }
}

export function focus(viewer, elementId) {
  const elementRegistry = viewer.get('elementRegistry');
  const canvas = viewer.get('canvas');
  const element = elementRegistry.get(elementId);
  if (!element) return false;

  for (const other of elementRegistry.getAll()) {
    canvas.removeMarker(other, 'rbpmn-mark-selected');
  }

  // The element may live on a subprocess drill-down plane.
  const root = canvas.findRoot(element) ?? canvas.getRootElement();
  if (root !== canvas.getRootElement()) {
    canvas.setRootElement(root);
  }
  canvas.addMarker(element, 'rbpmn-mark-selected');
  canvas.scrollToElement(element);
  return true;
}
