import { defineConfig, devices } from '@playwright/test';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

// T8: Playwright UI suite (SPEC §11 "UI tests (Playwright against Vite dev
// server, Tauri IPC mocked)"). The repo root's `ui:dev` script starts the
// same Vite dev server T4 uses for manual scenario browsing
// (`ui/src/tauri-mock.ts` picks the mock IPC backend automatically whenever
// the page isn't actually hosted inside a Tauri webview, see
// `ui/src/ipc-client.ts`), so no app code is touched to make it testable.
const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, '..');

export default defineConfig({
  testDir: './tests',
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  // A single worker keeps this deterministic on the dev machine (the popup's
  // 1s duration ticker and countdown timers are real wall-clock timers; more
  // parallelism just adds flake risk for no speed win in a ~20-test suite).
  workers: 1,
  reporter: [['list'], ['html', { open: 'never', outputFolder: 'report' }]],
  use: {
    baseURL: 'http://localhost:1420',
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
  webServer: {
    command: 'npm run ui:dev',
    cwd: repoRoot,
    url: 'http://localhost:1420/popup.html',
    reuseExistingServer: !process.env.CI,
    timeout: 30_000,
    stdout: 'pipe',
    stderr: 'pipe',
  },
});
