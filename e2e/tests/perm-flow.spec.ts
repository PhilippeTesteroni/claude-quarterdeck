import { expect, test } from '@playwright/test';
import { gotoAsk, gotoPopup } from '../helpers/popup';

// SPEC §16 (R-16.2): pending permission requests render in the SAME always-on-top
// ask window, visually distinct (amber), with Allow / Deny / In terminal actions
// and A / D / Esc keyboard shortcuts, and are mirrored into the popup.
test.describe('permission requests', () => {
  test('the perm modal renders amber with tool name, input, and three actions', async ({ page }) => {
    await gotoAsk(page, 'perm');

    // Amber-accented perm container (distinct from the clay ask flow).
    await expect(page.locator('.qd-perm')).toHaveCount(1);
    await expect(page.locator('.qd-perm-tag')).toHaveText('requests permission');
    await expect(page.locator('.qd-ask-identity-project')).toHaveText('quarterdeck');
    await expect(page.locator('.qd-perm-tool')).toHaveText('Run Bash?');
    // SPEC §35.1: a Bash perm renders the `command` in a labelled mono box (the
    // structured render), NOT the raw JSON object.
    await expect(page.locator('.qd-perm-field-label').first()).toHaveText('Command');
    await expect(page.locator('.qd-perm-input')).toContainText('rm -rf ./dist && npm run build');
    await expect(page.locator('.qd-perm-input')).not.toContainText('timeout');

    // Allow / Deny / In terminal.
    await expect(page.locator('.qd-perm-allow')).toHaveText('Allow');
    await expect(page.locator('.qd-perm-deny')).toHaveText('Deny');
    await expect(page.locator('.qd-perm-defer')).toHaveText('In terminal');

    // A pending ask is queued behind the perm → "1 more waiting" (R-16.2).
    await expect(page.locator('#qd-ask-badge')).toHaveText('1 more waiting');
  });

  test('Allow routes the allow decision and the queued ask takes over', async ({ page }) => {
    await gotoAsk(page, 'perm');
    await page.locator('.qd-perm-allow').click();

    const decision = await page.evaluate(
      () => (window as unknown as { __qdMock: { lastPermDecision(): string } }).__qdMock.lastPermDecision(),
    );
    expect(decision).toBe('allow');

    // The queued ask (a1) is now primary; the perm is gone.
    await expect(page.locator('.qd-perm')).toHaveCount(0);
    await expect(page.locator('.qd-ask-question')).toContainText('Ship the migration in this PR');
    await expect(page.locator('#qd-ask-badge')).toBeHidden();
  });

  test('keyboard A / D / Esc map to Allow / Deny / In terminal (R-16.2)', async ({ page }) => {
    await gotoAsk(page, 'perm');
    await page.keyboard.press('d');
    let decision = await page.evaluate(
      () => (window as unknown as { __qdMock: { lastPermDecision(): string } }).__qdMock.lastPermDecision(),
    );
    expect(decision).toBe('deny');

    // Reload for a fresh perm and test Esc = In terminal (defer), NOT hide.
    await gotoAsk(page, 'perm');
    await page.keyboard.press('Escape');
    decision = await page.evaluate(
      () => (window as unknown as { __qdMock: { lastPermDecision(): string } }).__qdMock.lastPermDecision(),
    );
    expect(decision).toBe('defer');
    // The window did NOT hide via close (Esc answered the perm instead).
    const hideCalls = await page.evaluate(
      () => (window as unknown as { __qdMock: { hideCurrentWindowCallCount(): number } }).__qdMock.hideCurrentWindowCallCount(),
    );
    expect(hideCalls).toBe(0);
  });

  test('an already-queued ask keeps the primary slot when a perm arrives later (R-16.2 FIFO)', async ({ page }) => {
    // R-16.2: perms "FIFO-queue with asks" — one queue by arrival, NOT perms
    // first. Here the ask arrived before the perm, so the ask stays primary and
    // the newer perm queues behind it.
    await gotoAsk(page, 'perm-after-ask');

    // The ask (clay) is primary, not the perm.
    await expect(page.locator('.qd-perm')).toHaveCount(0);
    await expect(page.locator('.qd-ask-question')).toContainText('Ship the migration in this PR');
    // The later perm is the one waiting behind it.
    await expect(page.locator('#qd-ask-badge')).toHaveText('1 more waiting');
  });

  test('an unmatched perm shows "Unknown agent (<context>)" (R-8.2/R-16.2)', async ({ page }) => {
    await gotoAsk(page, 'perm-unknown');
    await expect(page.locator('.qd-ask-identity-project')).toContainText('Unknown agent (');
    await expect(page.locator('.qd-perm-tool')).toHaveText('Run Write?');
  });

  test('perm is mirrored as an amber row in the popup with Allow/Deny/In terminal', async ({ page }) => {
    await gotoPopup(page, 'perm');

    const permRow = page.locator('.qd-perm-row');
    await expect(permRow).toHaveCount(1);
    await expect(permRow).toContainText('requests permission');
    await expect(permRow.locator('.qd-perm-row-tool')).toHaveText('Run Bash?');
    await expect(permRow.getByRole('button', { name: 'Allow' })).toBeVisible();
    await expect(permRow.getByRole('button', { name: 'Deny' })).toBeVisible();
    await expect(permRow.getByRole('button', { name: 'In terminal' })).toBeVisible();

    // Clicking Deny in the mirror routes the decision.
    await permRow.getByRole('button', { name: 'Deny' }).click();
    const decision = await page.evaluate(
      () => (window as unknown as { __qdMock: { lastPermDecision(): string } }).__qdMock.lastPermDecision(),
    );
    expect(decision).toBe('deny');
  });

  // SPEC R-32.1: a perm past its ~90 s deadline (its hook has given up) renders
  // "expired" with Allow/Deny DISABLED — a stale decision can no longer reach the
  // hook. "In terminal" stays live, and the A/D shortcuts are inert.
  test('an expired perm disables Allow/Deny and flags "expired"', async ({ page }) => {
    await gotoAsk(page, 'perm-expired');

    await expect(page.locator('.qd-perm')).toHaveCount(1);
    await expect(page.locator('.qd-perm-tag')).toHaveText('expired');
    await expect(page.locator('.qd-perm-allow')).toBeDisabled();
    await expect(page.locator('.qd-perm-deny')).toBeDisabled();
    // "In terminal" is still available to clear it locally.
    await expect(page.locator('.qd-perm-defer')).toBeEnabled();

    // The A/D keyboard shortcuts are inert past the deadline (no decision routed).
    await page.keyboard.press('a');
    await page.keyboard.press('d');
    const decision = await page.evaluate(
      () => (window as unknown as { __qdMock: { lastPermDecision(): string | null } }).__qdMock.lastPermDecision(),
    );
    expect(decision).toBeNull();

    // Esc still maps to "In terminal" (defer), which remains enabled.
    await page.keyboard.press('Escape');
    const deferred = await page.evaluate(
      () => (window as unknown as { __qdMock: { lastPermDecision(): string | null } }).__qdMock.lastPermDecision(),
    );
    expect(deferred).toBe('defer');
  });

  // SPEC §35.1: an AskUserQuestion perm renders the question + options READ-ONLY
  // and offers "In terminal" + Deny (NO Allow, since the permission channel
  // can't carry the user's choice back), plus a one-line hint.
  test('an AskUserQuestion perm renders the question + options and shows In terminal + Deny (no Allow)', async ({ page }) => {
    await gotoAsk(page, 'perm-question');

    await expect(page.locator('.qd-perm')).toHaveCount(1);
    await expect(page.locator('.qd-perm-tool')).toHaveText('Claude is asking a question');
    // The read-only question block: header + question + numbered options.
    await expect(page.locator('.qd-ask-q-header')).toHaveText('Deployment');
    await expect(page.locator('.qd-perm-q-question')).toHaveText('Which environment should I deploy to?');
    const options = page.locator('.qd-perm-q-option');
    await expect(options).toHaveCount(2);
    await expect(options.nth(0)).toContainText('Staging');
    await expect(options.nth(1)).toContainText('Production');

    // No Allow; grey "In terminal" (defer) + Deny; plus the hint.
    await expect(page.locator('.qd-perm-allow')).toHaveCount(0);
    await expect(page.locator('.qd-perm-defer')).toHaveText('In terminal');
    await expect(page.locator('.qd-perm-deny')).toHaveText('Deny');
    await expect(page.locator('.qd-perm-hint')).toContainText('ask_user');

    // The A key is inert (no Allow); Esc routes defer ("In terminal").
    await page.keyboard.press('a');
    let decision = await page.evaluate(
      () => (window as unknown as { __qdMock: { lastPermDecision(): string | null } }).__qdMock.lastPermDecision(),
    );
    expect(decision).toBeNull();
    await page.keyboard.press('Escape');
    decision = await page.evaluate(
      () => (window as unknown as { __qdMock: { lastPermDecision(): string | null } }).__qdMock.lastPermDecision(),
    );
    expect(decision).toBe('defer');
  });

  // SPEC §35.1: an unrecognized tool renders its parsed input as key/value rows
  // (mono values), never a raw JSON blob.
  test('an unknown tool renders key/value rows (not raw JSON)', async ({ page }) => {
    await gotoAsk(page, 'perm-unknown-tool');

    await expect(page.locator('.qd-perm-tool')).toHaveText('Run WebFetch?');
    const rows = page.locator('.qd-perm-kv .qd-perm-field');
    await expect(rows).toHaveCount(2);
    await expect(rows.nth(0).locator('.qd-perm-field-label')).toHaveText('url');
    await expect(rows.nth(0).locator('.qd-perm-field-path')).toHaveText('https://example.com/api');
    await expect(rows.nth(1).locator('.qd-perm-field-label')).toHaveText('prompt');
    // A normal tool keeps its Allow/Deny/In terminal.
    await expect(page.locator('.qd-perm-allow')).toHaveText('Allow');
  });

  // SPEC §35.1: a truncated/unparseable input falls back to the raw `<pre>`.
  test('a truncated/unparseable input falls back to the raw <pre>', async ({ page }) => {
    await gotoAsk(page, 'perm-bad-json');

    // No structured body; the raw fragment shows in the `<pre>` fallback.
    await expect(page.locator('.qd-perm-body')).toHaveCount(0);
    await expect(page.locator('pre.qd-perm-input')).toContainText('{"command":"npm run build');
  });

  // SPEC R-25.4: the onboarding card carries the "Take over permission prompts"
  // consent line (default on), and the settings pane exposes the toggle.
  test('onboarding shows the takeover consent line', async ({ page }) => {
    await gotoPopup(page, 'onboarding');
    await expect(page.locator('.qd-onboarding-takeover')).toContainText('Take over permission prompts');
  });
});
