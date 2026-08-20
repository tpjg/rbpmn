// The DRD's definitions header — the model's name and id, editable in place at
// the top left of the decision canvas — made safe to type in.
//
// Two upstream bugs live in dmn-js 17's `DefinitionPropertiesView.update`,
// which is four lines:
//
//     this.nameElement.textContent = businessObject.name;
//     this.idElement.textContent = businessObject.id;
//
// Assigning `textContent` **replaces the text node**, and the browser then has
// nowhere to put the caret, so it collapses the selection to offset 0. And
// `DrdUpdater` calls `update()` on every `element.updateProperties` — including
// the one the header's own debounced `input` handler just issued, 300 ms after
// you stopped typing.
//
// 1. **Reverse typing.** Type slower than the debounce and every keystroke
//    lands at the front: "ABCDE" comes out "EDCBA". Type faster and it is
//    invisible, which is why it survived a headless browser check that typed at
//    full speed — the bug needs a *pause*, and only a human types with pauses.
//
// 2. **A finished edit thrown away.** Type faster than the debounce and click
//    away, and the header reverts. `blur` runs `update()` before the pending
//    commit fires, writing the *old* model value over what you typed; the
//    debounce then reads that reverted DOM and commits nothing. The name you
//    typed is gone, with no error and no diagnostic.
//
// Both are fixed here rather than reported and waited on, because this document
// ships dmn-js to people who did not choose it. Neither fix assumes the bug:
// the reconcile is a no-op once upstream stops clobbering the caret, and the
// flush is a no-op once a pending edit is committed before `blur`. So an
// upgrade that fixes either one silently makes our half redundant instead of
// wrong. `e2e/ui.py` types with a real pause and asserts both, which is the
// part that will notice if an upgrade breaks them differently.
//
// The override is a didi registration rather than a patched prototype: dmn-js
// merges `additionalModules` last, so re-registering `definitionPropertiesView`
// replaces it for the DRD view alone, and a rename upstream fails at the import
// instead of quietly leaving a dead monkey-patch behind.

import DefinitionPropertiesView from 'dmn-js-drd/lib/features/definition-properties/DefinitionPropertiesView';

/// Selector to business-object property, matching the header's two fields.
const FIELDS = [
  ['.dmn-definitions-name', 'name'],
  ['.dmn-definitions-id', 'id'],
];

function DefinitionsHeader(eventBus, canvas, translate, injector) {
  DefinitionPropertiesView.call(this, eventBus, canvas, translate);
  this._injector = injector;

  // The **capture** phase, deliberately. `blur` does not bubble, but capture
  // still walks the ancestors first, so this runs before the header's own blur
  // handler — which is the only moment at which the DOM still holds the edit
  // and the model does not.
  //
  // Registered from `definitionIdView.create` because that is when the header
  // markup exists; the base constructor only schedules its creation for
  // `diagram.init`, which has not fired yet.
  eventBus.on('definitionIdView.create', (event) => {
    event.html.addEventListener('blur', (e) => this.flushPendingEdit(e.target), true);
  });
}

DefinitionsHeader.prototype = Object.create(DefinitionPropertiesView.prototype);
DefinitionsHeader.prototype.constructor = DefinitionsHeader;
DefinitionsHeader.$inject = ['eventBus', 'canvas', 'translate', 'injector'];

/// Write the model into the header, leaving a field being typed in alone.
DefinitionsHeader.prototype.update = function () {
  const businessObject = this._canvas.getRootElement().businessObject;
  reconcile(this.nameElement, businessObject.name);
  reconcile(this.idElement, businessObject.id);
};

function reconcile(node, value) {
  // `?? ''` where upstream assigns straight through: a definitions element with
  // no name renders the literal text "undefined" there, which is then what the
  // first keystroke edits.
  const text = value ?? '';
  // A focused field *is* the newer copy — whatever the model holds was read out
  // of this very node a moment ago — so there is never anything to write and
  // always a caret to lose.
  if (node.ownerDocument.activeElement === node) return;
  if (node.textContent === text) return;
  node.textContent = text;
}

/// Commit an edit the debounce has not delivered yet, before `blur` reconciles
/// the field and destroys it.
///
/// Routed through `definitionPropertiesEdit` rather than `modeling` so an id
/// keeps its validation and its error message; resolved lazily because that
/// component injects *this* one, and asking for it up front is a cycle.
DefinitionsHeader.prototype.flushPendingEdit = function (node) {
  const field = FIELDS.find(([selector]) => node?.matches?.(selector));
  if (!field) return;
  const [, property] = field;
  const businessObject = this._canvas.getRootElement().businessObject;
  const value = (node.textContent ?? '').trim();
  // Only a real pending edit. Without this, every click away from the header
  // would push an undo entry that changes nothing.
  if (value === (businessObject[property] ?? '')) return;
  this._injector.get('definitionPropertiesEdit', false)?.update(property, value);
};

export default {
  definitionPropertiesView: ['type', DefinitionsHeader],
};
