// Pure SVG-document surgery, kept free of bpmn-js so `just ui-test` can load
// it under node. The half that needs a browser lives in `svg-export.js`.

/// Paint an opaque background behind an exported diagram.
///
/// `saveSVG` emits no background at all — on screen the page's own CSS
/// supplies it — so an exported file is transparent, and transparent means
/// "whatever is underneath": near-black strokes vanish into a dark document
/// viewer, and a slide deck with a coloured master shows the deck through the
/// diagram. An artifact meant for a document has to carry its own paper.
///
/// The rect takes its geometry from the **viewBox**, not `100%`. That is the
/// whole reason this is a function rather than a string append: bpmn-js emits
/// a viewBox whose origin is the diagram's bounding box, which is routinely
/// negative, and a `width="100%" height="100%"` rect resolves against the
/// viewport with `x`/`y` defaulting to 0 — so it would sit offset from the
/// content it is supposed to be behind, and clip a corner off every diagram
/// that starts left of or above the origin.
///
/// Returns the input unchanged if there is no viewBox to read, because a
/// mispainted background is worse than none.
export function withBackground(svg, color = '#ffffff') {
  const viewBox = /<svg[^>]*\sviewBox="\s*(-?[\d.]+)\s+(-?[\d.]+)\s+(-?[\d.]+)\s+(-?[\d.]+)\s*"/.exec(
    svg
  );
  if (!viewBox) return svg;
  const [, x, y, width, height] = viewBox;
  const rect = `<rect x="${x}" y="${y}" width="${width}" height="${height}" fill="${color}"/>`;
  // Immediately after the opening tag, so it is the first thing painted and
  // everything else lands on top of it.
  return svg.replace(/(<svg\b[^>]*>)/, `$1${rect}`);
}

/// `orders.bpmn` -> `orders.svg`. Same shape as `manifestNameFor`, so an
/// exported diagram sits next to its model with an obvious relationship.
///
/// Here rather than beside its siblings in `editor/files.js` for two reasons:
/// that module reaches for `Blob`, `document` and `FileReader`, so node cannot
/// load it and the rule would go untested; and the inspector will want this
/// name too, and `files.js` is the editor's.
export function svgNameFor(bpmnName) {
  return `${(bpmnName || 'process.bpmn').replace(/\.bpmn$/i, '')}.svg`;
}
