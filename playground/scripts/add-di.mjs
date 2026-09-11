// Bakes BPMN DI (diagram interchange) into fixtures that lack it, so every
// fixture renders in bpmn-js and any standard modeler. Re-runnable: files
// that already carry a BPMNDiagram are left untouched (hand-tuned DI wins —
// two reject fixtures carry hand-written DI because automated layout cannot
// or must not process them; see the comments in those files).
// The expect-diagnostics header comment is preserved verbatim — it is the
// corpus's source of truth and the Rust runner asserts against it.
import { readFileSync, writeFileSync, readdirSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { BpmnModdle } from 'bpmn-moddle';
import { ensureDi } from '../src/layout.js';

// Layout round-trips the model through bpmn-moddle, which reports a dangling
// reference only as an import warning and then drops it on export. For an
// errorRef that is not cosmetic: an error boundary with no errorRef is a
// catch-all, so the round trip would turn a mistyped reference into "catch
// everything" and rewrite what the fixture asserts. Anything moddle warns
// about is therefore refused here, and gets its DI written by hand.
const moddle = new BpmnModdle();

const here = dirname(fileURLToPath(import.meta.url));
const fixturesRoot = join(here, '..', '..', 'crates', 'rbpmn-model', 'tests', 'fixtures');

let ok = 0;
const failed = [];

for (const dir of ['accept', 'reject']) {
  const dirPath = join(fixturesRoot, dir);
  for (const file of readdirSync(dirPath).filter((f) => f.endsWith('.bpmn')).sort()) {
    const path = join(dirPath, file);
    const original = readFileSync(path, 'utf8');
    if (original.includes('bpmndi:BPMNDiagram')) continue;

    const defsStart = original.indexOf('<bpmn:definitions');
    if (defsStart < 0) {
      failed.push([`${dir}/${file}`, 'no bpmn:definitions element']);
      continue;
    }
    const header = original.slice(0, defsStart);

    try {
      const { warnings } = await moddle.fromXML(original);
      if (warnings.length) {
        failed.push([
          `${dir}/${file}`,
          `bpmn-moddle would rewrite it (${warnings.map((w) => w.message).join('; ')})`,
        ]);
        continue;
      }
      const laidOut = await ensureDi(original);
      const body = laidOut.replace(/^<\?xml[^>]*\?>\s*/, '');
      writeFileSync(path, header + body + (body.endsWith('\n') ? '' : '\n'));
      ok += 1;
    } catch (e) {
      failed.push([`${dir}/${file}`, e.message]);
    }
  }
}

console.log(`DI baked into ${ok} fixture(s)`);
for (const [file, reason] of failed) {
  console.log(`FAILED ${file}: ${reason} — write DI by hand (see dangling-flow.bpmn)`);
}
process.exitCode = failed.length ? 1 : 0;
