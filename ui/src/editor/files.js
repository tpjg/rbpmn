// Local files only. The editor never uploads a model and the server never
// stores one: definitions live in the application's git repository, next to
// the code that binds them, and reach the engine through `deploy` — which is
// code, not a UI action.

export function download(filename, text, mime) {
  const blob = new Blob([text], { type: `${mime};charset=utf-8` });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement('a');
  anchor.href = url;
  anchor.download = filename;
  anchor.click();
  // Revoked on the next turn: Safari needs the element to have been clicked
  // before the object URL goes away.
  setTimeout(() => URL.revokeObjectURL(url), 0);
}

export function openFile(accept) {
  return new Promise((resolve) => {
    const input = document.createElement('input');
    input.type = 'file';
    input.accept = accept;
    input.addEventListener('change', () => {
      const file = input.files?.[0];
      if (!file) {
        resolve(null);
        return;
      }
      const reader = new FileReader();
      reader.addEventListener('load', () =>
        resolve({ name: file.name, text: String(reader.result) })
      );
      reader.readAsText(file);
    });
    input.click();
  });
}

/// `orders.bpmn` -> `orders.bindings.json`, so the pair stays obviously a
/// pair once both are sitting in a repository.
export function manifestNameFor(bpmnName) {
  return `${(bpmnName || 'process.bpmn').replace(/\.bpmn$/i, '')}.bindings.json`;
}
