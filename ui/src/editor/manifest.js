// The bindings manifest: the half of a deployment that is deliberately not in
// the XML.
//
// Everything here operates on a plain object with the shape rbpmn-core's
// `Bindings` serializes to — `{ topics, correlations, indexes }` — because
// that object is the artifact: it is written next to the .bpmn in the user's
// repository and travels with it to `deploy`. Empty groups are pruned on the
// way out so a manifest with nothing in it is `{}` rather than three empty
// containers.

export function emptyManifest() {
  return { topics: {}, correlations: {}, indexes: [] };
}

/// Accepts anything `Bindings` would deserialize from, and normalizes the
/// missing groups. Throws on shapes rbpmn would reject, rather than quietly
/// repairing them — a manifest that looks accepted here and fails at deploy
/// is the exact failure this editor exists to prevent.
export function parseManifest(text) {
  const trimmed = text.trim();
  if (!trimmed) return emptyManifest();
  const raw = JSON.parse(trimmed);
  if (raw === null || typeof raw !== 'object' || Array.isArray(raw)) {
    throw new Error('a manifest is a JSON object');
  }
  for (const group of ['topics', 'correlations']) {
    const value = raw[group];
    if (value === undefined) continue;
    if (value === null || typeof value !== 'object' || Array.isArray(value)) {
      throw new Error(`"${group}" must be an object of elementId -> string`);
    }
    for (const [key, entry] of Object.entries(value)) {
      if (typeof entry !== 'string') {
        throw new Error(`"${group}.${key}" must be a string`);
      }
    }
  }
  if (raw.indexes !== undefined) {
    if (!Array.isArray(raw.indexes) || raw.indexes.some((i) => typeof i !== 'string')) {
      throw new Error('"indexes" must be an array of strings');
    }
  }
  const unknown = Object.keys(raw).filter(
    (k) => !['topics', 'correlations', 'indexes'].includes(k)
  );
  if (unknown.length) {
    throw new Error(`unknown manifest key(s): ${unknown.join(', ')}`);
  }
  return {
    topics: { ...(raw.topics ?? {}) },
    correlations: { ...(raw.correlations ?? {}) },
    indexes: [...(raw.indexes ?? [])],
  };
}

/// The on-disk form: stable key order, no empty groups, trailing newline.
export function serializeManifest(manifest) {
  const out = {};
  const topics = sortedEntries(manifest.topics);
  const correlations = sortedEntries(manifest.correlations);
  const indexes = [...(manifest.indexes ?? [])].sort();
  if (topics.length) out.topics = Object.fromEntries(topics);
  if (correlations.length) out.correlations = Object.fromEntries(correlations);
  if (indexes.length) out.indexes = indexes;
  return `${JSON.stringify(out, null, 2)}\n`;
}

function sortedEntries(group) {
  return Object.entries(group ?? {})
    .filter(([, value]) => value !== '' && value !== undefined && value !== null)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
}

/// Setting a binding to the empty string removes it. That is what makes the
/// "revert to default" affordance in the wiring pane a single control: an
/// unmapped service task runs on a topic named after its element id, so an
/// empty box means the default, not a topic called "".
export function setBinding(manifest, group, elementId, value) {
  const next = { ...manifest, [group]: { ...manifest[group] } };
  if (value === null || value === undefined || value.trim() === '') {
    delete next[group][elementId];
  } else {
    next[group][elementId] = value.trim();
  }
  return next;
}

/// Bindings pointing at elements the model no longer contains. Deploy does
/// not reject these — an unused entry binds nothing — but they are almost
/// always a rename that lost its other half, so the editor says so.
export function orphanedBindings(manifest, elementIds) {
  const present = new Set(elementIds);
  const out = [];
  for (const group of ['topics', 'correlations']) {
    for (const elementId of Object.keys(manifest[group] ?? {})) {
      if (!present.has(elementId)) out.push({ group, elementId });
    }
  }
  return out;
}
