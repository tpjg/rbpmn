// Diagram colours for the viewer's theme.
//
// bpmn-js paints element strokes, fills, label text *and* the arrowhead
// markers from options given at construction (`bpmnRenderer`), as SVG
// attributes rather than CSS. A stylesheet cannot reach them: that is why a
// dark canvas showed black labels on near-black, and why overriding
// `.djs-visual` with `!important` only ever fixes half of it — the markers in
// `<defs>` keep the old colour and the arrows go missing.
//
// So the colours are chosen here, once, and handed to the constructor. The
// cost is that they cannot change afterwards, which is what `onThemeChange`
// is for.

const DARK = '(prefers-color-scheme: dark)';

export function prefersDark() {
  return window.matchMedia(DARK).matches;
}

/// The two palettes, named rather than inlined because one surface must
/// deliberately *not* follow the viewer's theme: an exported diagram. A
/// document is not a screen — `#c9cfda` strokes are near-invisible on white
/// paper — so `svg-export.js` asks for [`LIGHT_DIAGRAM`] by name.

/// The `bpmnRenderer` options for the viewer's current theme. Kept in step
/// with the `--ink` / `--panel` tokens in the stylesheets by hand; there is
/// no way to read a CSS variable into an SVG attribute at construction time.
export const DARK_DIAGRAM = {
  defaultFillColor: '#1d2026',
  defaultStrokeColor: '#c9cfda',
  defaultLabelColor: '#e7e9ee',
};

export const LIGHT_DIAGRAM = {
  defaultFillColor: '#ffffff',
  defaultStrokeColor: '#16181d',
  defaultLabelColor: '#16181d',
};

export function rendererColors() {
  return prefersDark() ? DARK_DIAGRAM : LIGHT_DIAGRAM;
}

/// Runs `handler` when the viewer's theme flips — a laptop switching to dark
/// at sunset, mid-session, with the document already open. Without it the
/// diagram keeps yesterday's colours and becomes unreadable in place.
export function onThemeChange(handler) {
  window.matchMedia(DARK).addEventListener('change', handler);
}
