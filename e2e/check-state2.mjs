#!/usr/bin/env node
// Connects to the running quarterdeck's popup webview over CDP and dumps
// get_state() + row DOM. Reusable checkpoint script across chaos rounds.
import { chromium } from '@playwright/test';

const cdpPort = 9788;

async function main() {
  const browser = await chromium.connectOverCDP(`http://127.0.0.1:${cdpPort}`);
  try {
    let popupPage;
    for (const ctx of browser.contexts()) {
      for (const p of ctx.pages()) {
        if (p.url().includes('popup.html')) popupPage = p;
      }
    }
    if (!popupPage) { console.log('NO POPUP PAGE FOUND'); process.exit(1); }
    const snap = await popupPage.evaluate(() => window.__TAURI__.core.invoke('get_state'));
    console.log('=== get_state() snapshot ===');
    console.log(JSON.stringify(snap, null, 2).slice(0, 8000));
    const rowStatuses = await popupPage.evaluate(() =>
      Array.from(document.querySelectorAll('.qd-row')).map((r) => ({
        project: r.querySelector('.qd-row-project')?.textContent,
        title: r.querySelector('.qd-row-title')?.textContent,
        status: r.querySelector('.qd-row-dot')?.getAttribute('data-status'),
      })));
    console.log('=== DOM rows ===');
    console.log(JSON.stringify(rowStatuses, null, 2));
  } finally {
    await browser.close().catch(() => {});
  }
}
main().catch((e) => { console.error('ERROR', e); process.exit(1); });
