// A reference to nothing cannot survive a round trip through bpmn-moddle:
// import reports it only as a warning, and export drops it. For most
// references the shape left behind is refused by the linter anyway — a
// message boundary with no message, a flow with no target. An errorRef is the
// exception: an error boundary with no errorRef is a *catch-all*, so a
// mistyped reference comes back meaning "catch every error", and nothing in
// the saved XML is left to say it was ever a typo. The linter cannot see what
// is no longer there, so the editor has to: found at import, and reported for
// as long as the boundary still has no code.
//
// Pure functions over moddle's import warnings, tested under node.

/// The errorRefs an import could not resolve, by the boundary that held them.
export function droppedErrorRefs(warnings) {
  return (warnings ?? [])
    .filter(
      (w) =>
        w.property === 'bpmn:errorRef' &&
        w.value &&
        String(w.message ?? '').startsWith('unresolved reference')
    )
    .map((w) => ({
      boundaryId: w.element?.$parent?.id ?? w.element?.id ?? '',
      ref: String(w.value),
    }));
}

export function droppedErrorRefDiagnostic({ boundaryId, ref }) {
  return {
    severity: 'error',
    rule: 'import',
    element: boundaryId,
    message:
      `the imported model's errorRef '${ref}' on '${boundaryId}' resolves to nothing, and ` +
      `the editor cannot keep a reference to nothing: it was dropped, which makes this ` +
      `boundary a catch-all that catches every error. Give it an error code, or remove it`,
  };
}
