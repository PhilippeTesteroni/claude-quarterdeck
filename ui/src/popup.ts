// T0 placeholder popup. The real snapshot-driven UI (rows, watch line, settings
// pane, empty state, onboarding) is implemented in T4 against the mocked IPC in
// `tauri-mock.ts`. This stub only proves the window loads and renders.

const content = document.getElementById('content');
if (content) {
  content.textContent = 'Quarterdeck';
}

export {};
