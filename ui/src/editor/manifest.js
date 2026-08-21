// The bindings manifest: the half of a deployment that is deliberately not in
// the XML.
//
// Everything here operates on a plain object with the shape rbpmn-core's
// `Bindings` serializes to — `{ topics, correlations, indexes, decisions }` —
// because
// that object is the artifact: it is written next to the .bpmn in the user's
// repository and travels with it to `deploy`. Empty groups are pruned on the
// way out so a manifest with nothing in it is `{}` rather than three empty
// containers.

/// The scopes a declared index can carry. `definition` (the default) indexes
/// one definition's instances; `shared` indexes the field across every
/// definition that declares it — which asserts the field means the same thing
/// in all of them, a contract nothing can check for you.
export const INDEX_SCOPES = ['definition', 'shared'];

/// Both manifest spellings in, one shape internally.
export function normalizeIndex(entry) {
  return typeof entry === 'string'
    ? { field: entry, scope: 'definition' }
    : { field: entry.field, scope: entry.scope ?? 'definition' };
}

/// The field box's compact syntax: `order_no:shared`, plain `channel` for the
/// default. One line of text has to carry the scope somehow, and a suffix
/// round-trips without a second widget.
export function parseIndexField(text) {
  const [field, scope] = text.split(':');
  if (scope !== undefined && !INDEX_SCOPES.includes(scope)) {
    throw new Error(`unknown index scope "${scope}" — expected ${INDEX_SCOPES.join(' or ')}`);
  }
  return { field: field.trim(), scope: scope ?? 'definition' };
}

export function formatIndexField(entry) {
  const { field, scope } = normalizeIndex(entry);
  return scope === 'definition' ? field : `${field}:${scope}`;
}

export function emptyManifest() {
  return { topics: {}, correlations: {}, indexes: [], decisions: {} };
}

/// The manifest groups that map an element id to a plain string.
const STRING_GROUPS = ['topics', 'correlations'];

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
  for (const group of STRING_GROUPS) {
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
  // `decisions` is the odd one out: a business-rule task binds *two* things,
  // which decision it invokes and where the answer lands, so its entries are
  // objects rather than strings.
  if (raw.decisions !== undefined) {
    const value = raw.decisions;
    if (value === null || typeof value !== 'object' || Array.isArray(value)) {
      throw new Error('"decisions" must be an object of elementId -> { decision, result }');
    }
    for (const [key, entry] of Object.entries(value)) {
      if (entry === null || typeof entry !== 'object' || Array.isArray(entry)) {
        throw new Error(`"decisions.${key}" must be { decision, result }`);
      }
      for (const field of ['decision', 'result']) {
        if (typeof entry[field] !== 'string' || !entry[field]) {
          throw new Error(`"decisions.${key}.${field}" must be a non-empty string`);
        }
      }
      const extra = Object.keys(entry).filter((k) => !['decision', 'result'].includes(k));
      if (extra.length) {
        throw new Error(`unknown key(s) in "decisions.${key}": ${extra.join(', ')}`);
      }
    }
  }
  if (raw.indexes !== undefined) {
    if (!Array.isArray(raw.indexes)) {
      throw new Error('"indexes" must be an array');
    }
    for (const entry of raw.indexes) {
      // Two spellings, one meaning: a bare string is the definition-scoped
      // default, an object names its scope. Refused rather than repaired,
      // exactly as the engine refuses it — an editor that silently accepted a
      // manifest deploy rejects would be lying about what it validated.
      if (typeof entry === 'string') continue;
      if (!entry || typeof entry !== 'object' || Array.isArray(entry)) {
        throw new Error('"indexes" entries must be a field name or {field, scope}');
      }
      if (typeof entry.field !== 'string' || !entry.field) {
        throw new Error('"indexes" entries must carry a non-empty "field"');
      }
      if (entry.scope !== undefined && !INDEX_SCOPES.includes(entry.scope)) {
        throw new Error(
          `unknown index scope "${entry.scope}" — expected ${INDEX_SCOPES.join(' or ')}`
        );
      }
      const extra = Object.keys(entry).filter((k) => !['field', 'scope'].includes(k));
      if (extra.length) {
        throw new Error(`unknown key(s) in an "indexes" entry: ${extra.join(', ')}`);
      }
    }
  }
  const unknown = Object.keys(raw).filter(
    (k) => !['topics', 'correlations', 'indexes', 'decisions'].includes(k)
  );
  if (unknown.length) {
    throw new Error(`unknown manifest key(s): ${unknown.join(', ')}`);
  }
  return {
    topics: { ...(raw.topics ?? {}) },
    correlations: { ...(raw.correlations ?? {}) },
    indexes: (raw.indexes ?? []).map(normalizeIndex),
    decisions: Object.fromEntries(
      Object.entries(raw.decisions ?? {}).map(([k, v]) => [
        k,
        { decision: v.decision, result: v.result },
      ])
    ),
  };
}

/// The on-disk form: stable key order, no empty groups, trailing newline.
export function serializeManifest(manifest) {
  const out = {};
  const topics = sortedEntries(manifest.topics);
  const correlations = sortedEntries(manifest.correlations);
  // Sorted by field and written back in the narrowest spelling that carries
  // the meaning: a definition-scoped entry is the bare string it has always
  // been, so an existing manifest round-trips byte for byte. rbpmn's own
  // `Ord` sorts by field first for the same reason.
  const indexes = [...(manifest.indexes ?? [])]
    .map(normalizeIndex)
    .sort((a, b) => (a.field < b.field ? -1 : a.field > b.field ? 1 : 0))
    .map((i) => (i.scope === 'shared' ? { field: i.field, scope: 'shared' } : i.field));
  const decisions = Object.entries(manifest.decisions ?? {})
    .filter(([, v]) => v && v.decision && v.result)
    .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
  if (topics.length) out.topics = Object.fromEntries(topics);
  if (correlations.length) out.correlations = Object.fromEntries(correlations);
  if (indexes.length) out.indexes = indexes;
  if (decisions.length) out.decisions = Object.fromEntries(decisions);
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

/// Set one half of a decision binding. Clearing either half removes the whole
/// entry: a decision with no result path, or a result path with no decision,
/// is not a binding deploy would accept, and half a binding in the file is
/// worse than none.
export function setDecisionBinding(manifest, elementId, field, value) {
  const next = { ...manifest, decisions: { ...manifest.decisions } };
  const current = next.decisions[elementId] ?? { decision: '', result: '' };
  const updated = { ...current, [field]: (value ?? '').trim() };
  if (!updated.decision || !updated.result) {
    // Keep the partial entry in memory so typing one field does not erase the
    // other, but never let a half-binding reach the serialized manifest.
    next.decisions[elementId] = updated;
    if (!updated.decision && !updated.result) delete next.decisions[elementId];
  } else {
    next.decisions[elementId] = updated;
  }
  return next;
}

/// Bindings pointing at elements the model no longer contains. Deploy does
/// not reject these — an unused entry binds nothing — but they are almost
/// always a rename that lost its other half, so the editor says so.
export function orphanedBindings(manifest, elementIds) {
  const present = new Set(elementIds);
  const out = [];
  for (const group of [...STRING_GROUPS, 'decisions']) {
    for (const elementId of Object.keys(manifest[group] ?? {})) {
      if (!present.has(elementId)) out.push({ group, elementId });
    }
  }
  return out;
}
