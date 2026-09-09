// The bindings manifest: the half of a deployment that is deliberately not in
// the XML.
//
// Everything here operates on a plain object with the shape rbpmn-core's
// `Bindings` serializes to — `{ topics, correlations, indexes, decisions,
// config, retries }` — because that object is the artifact: it is written
// next to the .bpmn in the user's repository and travels with it to `deploy`.
// Empty groups are pruned on the way out so a manifest with nothing in it is
// `{}` rather than six empty containers.

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
  return {
    topics: {},
    correlations: {},
    indexes: [],
    decisions: {},
    config: {},
    retries: { by_element: {}, by_topic: {} },
  };
}

/// The members of one retry policy, in the order rbpmn writes them back.
const RETRY_MEMBERS = ['attempts', 'backoff', 'multiplier'];

/// The two layers, spelled as rbpmn spells them — and held that way too.
/// Everything in a manifest object must be something `Bindings` deserializes,
/// because `checkModel` stringifies this object straight into the WASM
/// verdict; a camelCase convenience here would reach the engine as an unknown
/// field. (`indexes` is normalized in memory and gets away with it for
/// exactly this reason: `{field, scope}` is valid manifest syntax too.)
///
/// They are separate maps because they cannot share a key space: an unmapped
/// service task's topic *is* its element id.
const RETRY_LAYERS = ['by_element', 'by_topic'];

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
  // `config` is free JSON — rbpmn never looks inside one, and neither does
  // this. The one thing checked is the shape rbpmn checks: an entry is an
  // object, so a single value is spelled `{"template": "..."}` and there is
  // room for a second key later.
  if (raw.config !== undefined) {
    const value = raw.config;
    if (value === null || typeof value !== 'object' || Array.isArray(value)) {
      throw new Error('"config" must be an object of elementId -> object');
    }
    for (const [key, entry] of Object.entries(value)) {
      if (!isConfigEntry(entry)) throw new Error(`"config.${key}" must be a JSON object`);
    }
  }
  // `retries` is checked for shape, never for range: 1..=1000 attempts and a
  // 1..=10 multiplier are the engine's numbers, and duplicating them here is
  // how an editor starts disagreeing with the verdict it displays. What is
  // checked is what a *shape* mistake would otherwise turn into — a policy
  // silently dropped, or a layer name nobody notices is wrong.
  if (raw.retries !== undefined) {
    const value = raw.retries;
    if (value === null || typeof value !== 'object' || Array.isArray(value)) {
      throw new Error('"retries" must be an object of { by_element, by_topic }');
    }
    const extraLayers = Object.keys(value).filter((k) => !RETRY_LAYERS.includes(k));
    if (extraLayers.length) {
      throw new Error(
        `unknown key(s) in "retries": ${extraLayers.join(', ')} — the layers are by_element and by_topic`
      );
    }
    for (const wire of RETRY_LAYERS) {
      const layer = value[wire];
      if (layer === undefined) continue;
      if (layer === null || typeof layer !== 'object' || Array.isArray(layer)) {
        throw new Error(`"retries.${wire}" must be an object of key -> policy`);
      }
      for (const [key, policy] of Object.entries(layer)) {
        if (policy === null || typeof policy !== 'object' || Array.isArray(policy)) {
          throw new Error(`"retries.${wire}.${key}" must be { attempts?, backoff?, multiplier? }`);
        }
        const extra = Object.keys(policy).filter((k) => !RETRY_MEMBERS.includes(k));
        if (extra.length) {
          throw new Error(`unknown key(s) in "retries.${wire}.${key}": ${extra.join(', ')}`);
        }
        if (policy.attempts !== undefined && !Number.isInteger(policy.attempts)) {
          throw new Error(`"retries.${wire}.${key}.attempts" must be a whole number of handler calls`);
        }
        if (policy.backoff !== undefined && typeof policy.backoff !== 'string') {
          throw new Error(`"retries.${wire}.${key}.backoff" must be an ISO-8601 duration, like PT10M`);
        }
        if (policy.multiplier !== undefined && typeof policy.multiplier !== 'number') {
          throw new Error(`"retries.${wire}.${key}.multiplier" must be a number`);
        }
      }
    }
  }
  const unknown = Object.keys(raw).filter(
    (k) => !['topics', 'correlations', 'indexes', 'decisions', 'config', 'retries'].includes(k)
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
    config: { ...(raw.config ?? {}) },
    retries: Object.fromEntries(
      RETRY_LAYERS.map((layer) => [layer, { ...(raw.retries?.[layer] ?? {}) }])
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
  // Element order is sorted like every other group; the keys *inside* an
  // entry are written back exactly as they came in. They are the
  // application's, not rbpmn's, and reordering them would put noise in a diff
  // and change nothing about what deploys.
  const config = Object.entries(manifest.config ?? {}).sort(([a], [b]) =>
    a < b ? -1 : a > b ? 1 : 0
  );
  if (topics.length) out.topics = Object.fromEntries(topics);
  if (correlations.length) out.correlations = Object.fromEntries(correlations);
  if (indexes.length) out.indexes = indexes;
  if (decisions.length) out.decisions = Object.fromEntries(decisions);
  if (config.length) out.config = Object.fromEntries(config);
  // Sorted by key like every other group, and each policy written back in the
  // narrowest spelling that carries it: unset members are absent, an empty
  // layer does not appear, and an empty group does not either. That is not
  // tidiness — `deploy` hashes these bytes, so a group that always appeared
  // would allocate a new version of every definition that never changed.
  const retries = {};
  for (const layer of RETRY_LAYERS) {
    const entries = Object.entries(manifest.retries?.[layer] ?? {})
      .map(([key, policy]) => [key, narrowPolicy(policy)])
      .filter(([, policy]) => Object.keys(policy).length)
      .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0));
    if (entries.length) retries[layer] = Object.fromEntries(entries);
  }
  if (Object.keys(retries).length) out.retries = retries;
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
    put(next[group], elementId, value.trim());
  }
  return next;
}

/// Set an element's config from the text of the editor's box. Empty removes
/// the entry, which is what makes "no config" a single control rather than a
/// checkbox and a box.
///
/// Throws on anything rbpmn would reject — bad JSON, or a value that is not
/// an object — rather than storing it and letting the verdict find it. The
/// caller shows the message beside the box; the manifest never holds a shape
/// that could not deploy.
export function setConfigBinding(manifest, elementId, text) {
  const next = { ...manifest, config: { ...manifest.config } };
  if (!text || !text.trim()) {
    delete next.config[elementId];
    return next;
  }
  let value;
  try {
    value = JSON.parse(text);
  } catch (e) {
    throw new Error(`not valid JSON: ${e.message}`);
  }
  // One predicate with `parseManifest`, framed for whoever is reading: there
  // the manifest is a file and the key locates the problem, here it is a box
  // and an example is more use than a path.
  if (!isConfigEntry(value)) {
    throw new Error('a config entry is a JSON object, like {"template": "warning_first"}');
  }
  put(next.config, elementId, value);
  return next;
}

/// The shape rbpmn accepts for one config entry (`config-binds-task`): an
/// object, and nothing else. What is *inside* is the application's and is
/// never inspected, at any depth.
function isConfigEntry(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}

/// The box's text for an element: pretty-printed so it is editable, and empty
/// when nothing is configured.
export function formatConfig(manifest, elementId) {
  if (!has(manifest.config, elementId)) return '';
  return JSON.stringify(manifest.config[elementId], null, 2);
}

/// Element ids are NCNames, and `__proto__` is one. Writing it with `=` calls
/// the Object.prototype setter instead of creating a key: nothing is stored,
/// nothing serializes, and the next read gets the object back off the
/// prototype chain — so the box looks saved while the manifest is empty.
/// Reading it with `[]` on a manifest that has no such key is the mirror
/// image, and hands out `Object.prototype`.
function put(group, key, value) {
  Object.defineProperty(group, key, {
    value,
    enumerable: true,
    writable: true,
    configurable: true,
  });
}

function has(group, key) {
  return group != null && Object.hasOwn(group, key);
}

/// Read one entry of a manifest group, own keys only — the read half of what
/// [`put`] guards on the write side.
export function binding(manifest, group, elementId) {
  return has(manifest[group], elementId) ? manifest[group][elementId] : undefined;
}

/// Set one half of a decision binding. Clearing either half removes the whole
/// entry: a decision with no result path, or a result path with no decision,
/// is not a binding deploy would accept, and half a binding in the file is
/// worse than none.
export function setDecisionBinding(manifest, elementId, field, value) {
  const next = { ...manifest, decisions: { ...manifest.decisions } };
  const current = has(next.decisions, elementId)
    ? next.decisions[elementId]
    : { decision: '', result: '' };
  const updated = { ...current, [field]: (value ?? '').trim() };
  if (!updated.decision || !updated.result) {
    // Keep the partial entry in memory so typing one field does not erase the
    // other, but never let a half-binding reach the serialized manifest.
    put(next.decisions, elementId, updated);
    if (!updated.decision && !updated.result) delete next.decisions[elementId];
  } else {
    put(next.decisions, elementId, updated);
  }
  return next;
}

/// Bindings pointing at elements the model no longer contains. Deploy does
/// not reject these — an unused entry binds nothing — but they are almost
/// always a rename that lost its other half, so the editor says so.
///
/// `config` is deliberately not among the groups checked here. Deploy *does*
/// reject a stale config key (`config-binds-task`, because config has no
/// default to fall back on), so the verdict already carries it — with the
/// element highlighted and the rule id a reader can look up. Adding it here
/// would report the same defect twice, once as a rule and once as an editor
/// hunch.
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


/// One policy with its unset members dropped, in rbpmn's own member order.
function narrowPolicy(policy) {
  const out = {};
  for (const member of RETRY_MEMBERS) {
    const value = policy?.[member];
    if (value !== undefined && value !== null && value !== '') out[member] = value;
  }
  return out;
}
