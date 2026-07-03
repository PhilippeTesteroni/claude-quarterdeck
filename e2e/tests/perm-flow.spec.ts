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
    // tool_input shown verbatim (already sanitized/capped by the shell).
    await expect(page.locator('.qd-perm-input')).toContainText('rm -rf ./dist && npm run build');

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

  // SPEC R-25.4: the onboarding card carries the "Take over permission prompts"
  // consent line (default on), and the settings pane exposes the toggle.
  test('onboarding shows the takeover consent line', async ({ page }) => {
    await gotoPopup(page, 'onboarding');
    await expect(page.locator('.qd-onboarding-takeover')).toContainText('Take over permission prompts');
  });
});
