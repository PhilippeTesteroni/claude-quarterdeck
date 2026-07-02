// T0 placeholder ask window. The real ask UI (agent identity, question, option
// buttons with 1-9 keyboard shortcuts, free-text field, countdown, queue badge)
// is implemented in T4 (SPEC R-8.3). This stub only proves the window loads.

const content = document.getElementById('content');
if (content) {
  content.textContent = 'Quarterdeck';
}

export {};
