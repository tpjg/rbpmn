// Export a diagram as a standalone SVG, for printing and for pasting into a
// document someone will discuss with stakeholders.
//
// WHY A SECOND, DETACHED VIEWER rather than exporting the canvas in front of
// you. bpmn-js paints strokes, fills, labels and the arrowhead markers in
// `<defs>` from options given at *construction*, as SVG attributes (see
// `theme.js`) — so a dark-mode canvas exports a dark-mode diagram, which is
// what you least want on white paper, and there is no way to restyle it
// afterwards.
//
// The tempting fix is to flip the live canvas: change the colours, `saveSVG`,
// change back. It works, and the editor already owns the machinery
// (`remountForTheme`), but read what that function's own comment costs:
// re-constructing the modeler discards the undo history. That is a fair price
// once, when the OS flips at sunset; it is not a price to pay silently every
// time someone clicks Export. It would also reset zoom, scroll and selection,
// and — worst — leave the canvas in the wrong theme if `saveSVG` threw
// halfway.
//
// Rendering into a detached container costs an import of XML the editor
// already has in hand, and costs the visible canvas nothing at all.

import Viewer from 'bpmn-js/lib/Viewer';
import { LIGHT_DIAGRAM } from './theme.js';
import { withBackground } from './svg-document.js';

/// Render `xml` to a standalone light-themed SVG string.
///
/// Deliberately a plain `Viewer`, not a `Modeler`: an export needs rendering
/// and nothing else, and the palette, context pads and drag handles a Modeler
/// builds would be work thrown away — and markup to keep out of the file.
export async function diagramToSvg(xml) {
  const host = document.createElement('div');
  // Off-screen, but **laid out**. Not `display: none`: `saveSVG` measures the
  // content with `getBBox()`, and an unrendered SVG reports zeros — the export
  // would come back with a 0x0 viewBox and look empty everywhere it is opened.
  host.style.cssText =
    'position:absolute;left:-10000px;top:0;width:1200px;height:900px;pointer-events:none;';
  host.setAttribute('aria-hidden', 'true');
  document.body.append(host);

  const viewer = new Viewer({ container: host, bpmnRenderer: LIGHT_DIAGRAM });
  try {
    await viewer.importXML(xml);
    const { svg } = await viewer.saveSVG();
    return withBackground(svg);
  } finally {
    // Both, on every path: a thrown import must not leak a viewer or leave a
    // stray container in the document.
    viewer.destroy();
    host.remove();
  }
}
