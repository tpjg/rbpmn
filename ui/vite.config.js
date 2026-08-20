// One entry per invocation: IIFE output cannot carry multiple entries, and
// IIFE is what we want — the Rust side inlines the result into a single
// <script>, so there must be no imports, no chunks and no module graph left
// at runtime.
import { defineConfig } from 'vite';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const entry = process.env.RBPMN_UI_ENTRY;

if (!entry) {
  throw new Error('set RBPMN_UI_ENTRY (inspector|editor) — see scripts/build.mjs');
}

export default defineConfig({
  // dmn-js's decision table is built on Inferno, which reads
  // `process.env.NODE_ENV` *unguarded* at module scope. Vite substitutes that
  // for app builds but not for library builds, so without this the document
  // throws `process is not defined` on load and renders nothing at all — the
  // editor's own e2e caught it, which is the entire reason that check exists.
  //
  // Only the one expression is defined. The other `process` reads in the
  // bundle (semver, a parser's debug logging) are all behind
  // `typeof process`, which is safe on an undeclared name.
  define: {
    'process.env.NODE_ENV': JSON.stringify('production'),
  },
  build: {
    outDir: resolve(here, 'dist', entry),
    emptyOutDir: true,
    // No modulepreload polyfill, no chunk splitting: a single file is the
    // whole point (see the document model in bpmn-engine-design.md).
    cssCodeSplit: false,
    modulePreload: false,
    lib: {
      entry: resolve(here, 'src', entry, 'main.js'),
      name: `rbpmn_${entry}`,
      formats: ['iife'],
      fileName: () => `${entry}.js`,
    },
    rollupOptions: {
      output: { assetFileNames: `${entry}.[ext]` },
    },
  },
});
