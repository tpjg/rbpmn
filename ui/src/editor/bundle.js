// The deploy bundle: process, manifest and decision artifacts in one file.
//
// This is not a format the editor invented. It is exactly the body
// `POST /v1/definitions` takes and exactly what `rbpmn_engine::Bundle`
// deserializes, so what the editor exports is what deploy consumes — no
// converter in between to disagree with either end.
//
// The individual files remain the primary artifacts: a `.bpmn`, a
// `.bindings.json` and one `.dmn` per decision, living in the application's
// git repository where a diff means something. The bundle is for *handing the
// whole deployment over* — to a deploy script, a colleague, a ticket.

/// Build the bundle from the editor's working set.
export function buildBundle({ bpmn, manifest, decisions }) {
  const bundle = { bpmn };
  // Only include what is there: a bundle with `"bindings": {}` and
  // `"decisions": []` in it reads as though someone meant something by them.
  const bindings = JSON.parse(manifest);
  if (Object.keys(bindings).length) bundle.bindings = bindings;
  if (decisions.length) bundle.decisions = decisions.map((d) => d.xml);
  return `${JSON.stringify(bundle, null, 2)}\n`;
}

/// Read a bundle back into a working set.
///
/// Refuses rather than repairs, for the same reason `parseManifest` does: a
/// bundle that looks accepted here and fails at deploy is precisely what this
/// editor exists to prevent.
export function parseBundle(text) {
  const raw = JSON.parse(text);
  if (raw === null || typeof raw !== 'object' || Array.isArray(raw)) {
    throw new Error('a bundle is a JSON object');
  }
  if (typeof raw.bpmn !== 'string' || !raw.bpmn.trim()) {
    throw new Error('"bpmn" must be the process XML');
  }
  if (raw.decisions !== undefined) {
    if (!Array.isArray(raw.decisions) || raw.decisions.some((d) => typeof d !== 'string')) {
      throw new Error('"decisions" must be an array of DMN documents');
    }
  }
  if (raw.bindings !== undefined) {
    if (raw.bindings === null || typeof raw.bindings !== 'object' || Array.isArray(raw.bindings)) {
      throw new Error('"bindings" must be a manifest object');
    }
  }
  const unknown = Object.keys(raw).filter((k) => !['bpmn', 'bindings', 'decisions'].includes(k));
  if (unknown.length) {
    throw new Error(`unknown bundle key(s): ${unknown.join(', ')}`);
  }
  return {
    bpmn: raw.bpmn,
    manifest: JSON.stringify(raw.bindings ?? {}, null, 2),
    decisions: (raw.decisions ?? []).map((xml, i) => ({
      name: decisionFileName(xml, i),
      xml,
    })),
  };
}

/// A file name for a DMN document, taken from the model's own `name` so that
/// what lands in a repository is recognisable rather than `decision-2.dmn`.
export function decisionFileName(xml, index = 0) {
  const name = /<(?:\w+:)?definitions[^>]*\sname="([^"]+)"/.exec(xml)?.[1];
  const safe = (name ?? '').trim().replace(/[^A-Za-z0-9._-]+/g, '-').replace(/^-+|-+$/g, '');
  return `${safe || `decision-${index + 1}`}.dmn`;
}

/// `orders.bpmn` -> `orders.bundle.json`, so the whole deployment keeps the
/// name of the process it deploys.
export function bundleNameFor(bpmnName) {
  return `${(bpmnName || 'process.bpmn').replace(/\.bpmn$/i, '')}.bundle.json`;
}
