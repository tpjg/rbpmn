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
