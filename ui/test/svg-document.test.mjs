// The SVG export's pure half. The half that needs a browser — the detached
// viewer in svg-export.js — is driven by e2e/ui.py, because bpmn-js cannot
// render under node and a mocked renderer would test the mock.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { svgNameFor, withBackground } from '../src/shared/svg-document.js';

const svg = (viewBox) =>
  `<?xml version="1.0" encoding="utf-8"?>\n<svg xmlns="http://www.w3.org/2000/svg" ` +
  `width="100" height="50" viewBox="${viewBox}" version="1.1"><g class="djs-group"/></svg>`;

test('the background takes its geometry from the viewBox, not the viewport', () => {
  // The case that makes this a function rather than a string append: bpmn-js
  // emits the diagram's bounding box as the origin, and it is routinely
  // negative. A `100%` rect would sit at 0,0 and clip the top-left corner off.
  const out = withBackground(svg('-120 -40 800 600'));
  assert.match(out, /<rect x="-120" y="-40" width="800" height="600" fill="#ffffff"\/>/);
});

test('the background is painted first, so everything lands on top of it', () => {
  const out = withBackground(svg('0 0 10 10'));
  assert.ok(out.indexOf('<rect') < out.indexOf('<g class="djs-group"'), out);
  assert.match(out, /<svg[^>]*><rect/, 'immediately after the opening tag');
});

test('a diagram with no viewBox is returned untouched', () => {
  // A mispainted background is worse than none: it would cover the diagram.
  const bare = '<svg xmlns="http://www.w3.org/2000/svg"><g/></svg>';
  assert.equal(withBackground(bare), bare);
});

test('the background colour is overridable but defaults to paper white', () => {
  assert.match(withBackground(svg('0 0 1 1')), /fill="#ffffff"/);
  assert.match(withBackground(svg('0 0 1 1'), '#ff0000'), /fill="#ff0000"/);
});

test('whitespace and decimals in a viewBox are tolerated', () => {
  // bpmn-js writes floats, and padding arithmetic produces them.
  const out = withBackground(svg('-12.5 -7.25 300.5 200.75'));
  assert.match(out, /<rect x="-12.5" y="-7.25" width="300.5" height="200.75"/);
});

test('an exported diagram is named after its model', () => {
  assert.equal(svgNameFor('orders.bpmn'), 'orders.svg');
  assert.equal(svgNameFor('Orders.BPMN'), 'Orders.svg');
  assert.equal(svgNameFor(''), 'process.svg');
  assert.equal(svgNameFor(undefined), 'process.svg');
});
