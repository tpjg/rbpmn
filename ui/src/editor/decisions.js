// The decision half of a deployment: dmn-js, and the working set of DMN
// artifacts the editor is holding.
//
// dmn-js is four editors behind one façade — a DRD canvas, a decision table, a
// literal expression and a boxed expression — and it picks the right one for
// whatever the user drills into. That is why it is embedded whole rather than
// as the decision-table widget alone: a decision model is a graph of decisions
// before it is any one table, and an editor that could only open tables would
// be the sort of half-tool this project keeps refusing to ship.

import DmnModeler from 'dmn-js/lib/Modeler';
import 'dmn-js/dist/assets/diagram-js.css';
import 'dmn-js/dist/assets/dmn-js-shared.css';
import 'dmn-js/dist/assets/dmn-js-drd.css';
import 'dmn-js/dist/assets/dmn-js-decision-table.css';
import 'dmn-js/dist/assets/dmn-js-decision-table-controls.css';
import 'dmn-js/dist/assets/dmn-js-literal-expression.css';
import 'dmn-js/dist/assets/dmn-js-boxed-expression.css';
import 'dmn-js/dist/assets/dmn-js-boxed-expression-controls.css';
import 'dmn-js/dist/assets/dmn-font/css/dmn.css';

import { decisionFileName } from './bundle.js';
import definitionsHeader from './dmn-definitions-header.js';

/// A new decision model, ready to edit. DMN 1.3, which is what dmn-js writes
/// and what dsntk reads.
///
/// Three things here are not decoration, and the first two were bug reports.
///
/// **`<variable>` on the decision.** Without it, drilling into the decision's
/// logic — the `{}` overlay — throws `Cannot read properties of undefined
/// (reading 'name')` inside dmn-js and leaves a blank canvas. The literal
/// expression editor renders the decision's output variable in its footer and
/// does not check whether there is one. A starter that crashes the first time
/// you open it is worse than no starter.
///
/// **`<inputData>` and the `<informationRequirement>` wiring it.** A FEEL
/// expression can only see names the decision *declares* it requires. Without
/// this, `order.amount` is not an error and not a warning — it is `null`, so
/// every comparison against it is null, and the decision quietly returns its
/// else branch forever. Nothing in rbpmn catches that: `feel-parses` checks
/// that expressions parse, not that the names in them resolve. So the starter
/// declares one, and the expression uses it, because the shape is the lesson.
///
/// The layout is deliberate too: the decision sits to the *right* of its
/// input rather than directly above it, so that after fit-viewport its
/// top-left corner — where dmn-js anchors the `{}` drill-down overlay — is
/// clear of the definitions header, which is pinned to the canvas's top-left
/// and would otherwise swallow the click.
///
/// **An expression that does something.** `1` evaluates, which made it a fine
/// smoke test and a useless example — it demonstrated neither inputs nor path
/// access. This one answers from its input, and the try-it pane's default
/// document is written to match it, so New decision → Evaluate returns a real
/// answer with nothing else typed.
///
/// It reads `order.total` rather than the `order.amount` it once did, because
/// that one default document also has to fit the `Triage` table the editor
/// opens on. Two decisions reachable without opening a file, one document
/// between them: the alternative is a default that quietly answers null for
/// whichever of the two you happen to be looking at.
///
/// This is a *second* decision added to a deployment, not the one the editor
/// boots with — see `EXAMPLE` in `main.js`. Hence a literal expression: the
/// example already shows a decision table, and the two views are the halves of
/// dmn-js worth knowing about.
export function starterDecision(name) {
  const id = name.replace(/[^A-Za-z0-9_-]+/g, '_');
  return `<?xml version="1.0" encoding="UTF-8"?>
<definitions xmlns="https://www.omg.org/spec/DMN/20191111/MODEL/"
             xmlns:dmndi="https://www.omg.org/spec/DMN/20191111/DMNDI/"
             xmlns:dc="http://www.omg.org/spec/DMN/20180521/DC/"
             id="_${id}" name="${name}" namespace="https://rbpmn.local/${id}">
  <inputData id="${id}_input" name="order">
    <variable id="${id}_input_var" name="order" typeRef="Any" />
  </inputData>
  <decision id="${id}_decision" name="${name}">
    <variable id="${id}_decision_var" name="${name}" typeRef="Any" />
    <informationRequirement id="${id}_requires">
      <requiredInput href="#${id}_input" />
    </informationRequirement>
    <literalExpression id="${id}_expression">
      <text>if order.total &gt; 100 then "large" else "small"</text>
    </literalExpression>
  </decision>
  <dmndi:DMNDI>
    <dmndi:DMNDiagram id="${id}_diagram">
      <dmndi:DMNShape id="${id}_shape" dmnElementRef="${id}_decision">
        <dc:Bounds height="80" width="180" x="300" y="80" />
      </dmndi:DMNShape>
      <dmndi:DMNShape id="${id}_input_shape" dmnElementRef="${id}_input">
        <dc:Bounds height="45" width="125" x="180" y="250" />
      </dmndi:DMNShape>
      <dmndi:DMNEdge id="${id}_edge" dmnElementRef="${id}_requires">
        <di:waypoint x="242" y="250" xmlns:di="http://www.omg.org/spec/DMN/20180521/DI/" />
        <di:waypoint x="390" y="160" xmlns:di="http://www.omg.org/spec/DMN/20180521/DI/" />
      </dmndi:DMNEdge>
    </dmndi:DMNDiagram>
  </dmndi:DMNDI>
</definitions>
`;
}

/// Wraps one dmn-js instance and the artifact it is currently showing.
///
/// The editor holds *several* artifacts but only one modeler: importing is
/// cheap, and a modeler per artifact would multiply the listeners, the
/// keyboard bindings and the ways two canvases can disagree about which one
/// has focus.
export class DecisionEditor {
  constructor(container, { onChange, colors }) {
    this.container = container;
    this.onChange = onChange;
    /// Renderer colours for the DRD, chosen at construction exactly as
    /// bpmn-js's are and for the same reason: `DrdRenderer` writes `fill` and
    /// `stroke` as **inline styles** on each shape, which no stylesheet can
    /// reach. Its defaults are `white` and `hsl(225, 10%, 15%)`, so a dark
    /// canvas got a white box with dark text on it.
    this.colors = colors;
    this.modeler = null;
    /// The artifact **actually on the canvas** — never merely the one
    /// requested. `currentXml` reads the canvas, so anything that writes that
    /// XML back must write it to the artifact it came from; pointing this at a
    /// requested-but-not-imported artifact silently copies one decision's
    /// content over another's.
    this.current = null;
    this.detach = null;
  }

  /// dmn-js is constructed lazily: a user who never opens a decision should
  /// not pay for one, and construction attaches global key bindings.
  ensureModeler() {
    if (this.modeler) return this.modeler;
    this.modeler = new DmnModeler({
      container: this.container,
      drd: {
        drdRenderer: this.colors,
        // Last wins in didi, and `additionalModules` is merged last — so this
        // replaces the DRD's definitions header with one that can be typed in.
        // See the file for the two upstream bugs it fixes.
        additionalModules: [definitionsHeader],
      },
    });
    // `views.changed` fires on drill-down too, which is the moment the active
    // editor changes underneath us; `commandStack.changed` only fires for
    // edits within one view. So it is both a change to report *and* the point
    // at which the command-stack listener has to move to the new view — a
    // drill-down into a decision table used to leave the listener behind on
    // the DRD, so edits inside the table never scheduled a check.
    this.modeler.on('views.changed', () => {
      this.watchActiveView();
      this.onChange();
    });
    return this.modeler;
  }

  /// Listen to the active view's command stack, and **only** that one.
  ///
  /// Every view has its own stack, so the listener has to follow the active
  /// view rather than being attached once at construction. It also has to be
  /// detached first: this runs on every import and every drill-down, and
  /// without the detach each one added another listener, so after five
  /// switches a single keystroke ran five validations.
  watchActiveView() {
    this.detach?.();
    this.detach = null;
    const bus = this.modeler?.getActiveViewer()?.get('eventBus');
    if (!bus) return;
    const handler = () => this.onChange();
    bus.on('commandStack.changed', handler);
    this.detach = () => bus.off('commandStack.changed', handler);
  }

  async show(artifact) {
    if (!artifact) {
      this.current = null;
      return;
    }
    const modeler = this.ensureModeler();
    await modeler.importXML(artifact.xml);
    // Only once the import has actually succeeded. If it throws, the canvas
    // still holds the *previous* artifact, and `current` has to keep pointing
    // at that one — otherwise the next capture writes the old decision's XML
    // into the new decision, which is how opening one malformed `.dmn`
    // destroyed the file before it.
    this.current = artifact;
    this.watchActiveView();
  }

  /// Give up what is on screen — the artifact *and* the canvas showing it.
  ///
  /// Deliberately one method rather than a light "forget the artifact" and a
  /// heavy "tear the modeler down". The light version existed and was wrong:
  /// nulling `current` left dmn-js still rendering the discarded decision,
  /// fully editable, while `captureDecision` returned early because nothing
  /// was current — so every keystroke in that canvas went nowhere. An editor
  /// on screen looking correct with the wrong content in it is the failure
  /// this whole class of fix is about.
  ///
  /// `ensureModeler` rebuilds lazily on the next `show`, so the cost is one
  /// construction against an import that was going to happen anyway. It also
  /// gets the leak right: a dropped dmn-js keeps its global key bindings and
  /// its listeners, so a replaced editor competes for the same keystrokes.
  destroy() {
    this.detach?.();
    this.detach = null;
    this.current = null;
    this.modeler?.destroy();
    this.modeler = null;
  }

  /// The XML of whatever is on screen, or the artifact's stored XML when
  /// nothing has been imported yet.
  async currentXml() {
    if (!this.current) return null;
    if (!this.modeler) return this.current.xml;
    try {
      const { xml } = await this.modeler.saveXML({ format: true });
      return xml;
    } catch {
      // A model mid-edit can be unserializable. Falling back to the last
      // good XML keeps validation running on *something* rather than
      // blanking the pane.
      return this.current.xml;
    }
  }
}

/// Add an artifact to the working set, giving it a name that does not collide.
export function addDecision(decisions, xml, preferredName) {
  const name = uniqueName(decisions, preferredName ?? decisionFileName(xml, decisions.length));
  return [...decisions, { name, xml }];
}

function uniqueName(decisions, name) {
  const taken = new Set(decisions.map((d) => d.name));
  if (!taken.has(name)) return name;
  const base = name.replace(/\.dmn$/i, '');
  for (let i = 2; ; i += 1) {
    const candidate = `${base}-${i}.dmn`;
    if (!taken.has(candidate)) return candidate;
  }
}
