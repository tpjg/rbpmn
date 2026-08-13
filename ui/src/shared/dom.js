// Tiny DOM helpers. Everything here sets `textContent`, never `innerHTML`:
// these documents render business data (variable documents, element names,
// failure messages) that no one on this side of the boundary controls, and
// the Rust renderer's escaping guarantee ends where the DOM begins.

export function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = String(text);
  return node;
}

export function field(label, value, { title } = {}) {
  const row = el('div', 'field');
  row.append(el('span', 'field-label', label), el('span', 'field-value', value));
  if (title) row.title = title;
  return row;
}

export function clear(node) {
  node.replaceChildren();
}

/// A collapsible section with a heading and a body the caller fills.
export function section(title, { open = true } = {}) {
  const wrap = el('details', 'section');
  wrap.open = open;
  const summary = el('summary', null, title);
  const body = el('div', 'section-body');
  wrap.append(summary, body);
  return { wrap, body };
}

/// Renders arbitrary JSON as a readable tree. Values are always text nodes,
/// so a variable holding `<img onerror=...>` is displayed, never parsed.
export function jsonTree(value, key = null) {
  const wrap = el('div', 'json-node');
  const isObject = value !== null && typeof value === 'object';

  if (!isObject) {
    const line = el('div', 'json-leaf');
    if (key !== null) line.append(el('span', 'json-key', `${key}: `));
    const type = value === null ? 'null' : typeof value;
    line.append(el('span', `json-value json-${type}`, value === null ? 'null' : String(value)));
    wrap.append(line);
    return wrap;
  }

  const entries = Array.isArray(value)
    ? value.map((v, i) => [String(i), v])
    : Object.entries(value);
  const details = el('details', 'json-branch');
  details.open = true;
  const label = Array.isArray(value) ? `[${entries.length}]` : `{${entries.length}}`;
  details.append(el('summary', null, key === null ? label : `${key} ${label}`));
  const body = el('div', 'json-children');
  for (const [k, v] of entries) body.append(jsonTree(v, k));
  details.append(body);
  wrap.append(details);
  return wrap;
}
