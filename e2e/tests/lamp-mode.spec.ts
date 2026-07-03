import { expect, test } from '@playwright/test';
import { gotoPopup } from '../helpers/popup';

// SPEC §25 (R-25.1..R-25.5): the pinned popup's compact traffic-light lamp
// mode — mode switching, aggregate color/badge, drag-vs-click discrimination,
// the unpin-from-lamp path, and the onboarding refresh.

type MockHooks = {
  lastPermDecision(): string | null;
  startDraggingCallCount(): number;
};

test.describe('lamp mode (R-25.1/R-25.2)', () => {
  test('the collapse button only shows while pinned, and collapses to the lamp', async ({ page }) => {
    await gotoPopup(page, 'default');

    const collapse = page.locator('#qd-collapse');
    await expect(collapse).toBeHidden();

    await page.locator('#qd-pin').click();
    await expect(collapse).toBeVisible();

    await collapse.click();
    await expect(page.locator('#app')).toHaveClass(/qd-app-lamp/);
    await expect(page.locator('#qd-lamp')).toBeVisible();
    await expect(page.locator('.qd-header')).toBeHidden();
    await expect(page.locator('#qd-content')).toBeHidden();
    await expect(page.locator('#qd-footer')).toBeHidden();
  });

  test('shows the worst-of aggregate color and the attention-count badge (R-25.1)', async ({ page }) => {
    // Fixture: 1 attention + 1 working session, pinned + already collapsed.
    await gotoPopup(page, 'lamp');

    await expect(page.locator('#app')).toHaveClass(/qd-app-lamp/);
    const dot = page.locator('#qd-lamp-dot');
    await expect(dot).toHaveAttribute('data-status', 'attention');

    const badge = page.locator('#qd-lamp-badge');
    await expect(badge).toBeVisible();
    await expect(badge).toHaveText('1');

    // R-25.3: hover tooltip carries the same counts line as the popup footer.
    await expect(page.locator('#qd-lamp')).toHaveAttribute('title', /needs you/);
  });

  test('badge is hidden when there are zero attention sessions', async ({ page }) => {
    // `token-stats` is all working/idle (no attention) — pin + collapse it.
    await gotoPopup(page, 'token-stats');
    await page.locator('#qd-pin').click();
    await page.locator('#qd-collapse').click();
    await expect(page.locator('#qd-lamp-dot')).toHaveAttribute('data-status', 'working');
    await expect(page.locator('#qd-lamp-badge')).toBeHidden();
  });

  test('a plain click expands back to the list in place; a drag calls startDragging instead (R-25.1/R-25.2)', async ({ page }) => {
    await gotoPopup(page, 'lamp');
    const lamp = page.locator('#qd-lamp');
    await expect(lamp).toBeVisible();

    // First: a genuine drag (movement past the threshold) must NOT expand the
    // lamp, and must route through the manual `startDragging()` discrimination
    // (SPEC R-25.1) rather than a click.
    const box = (await lamp.boundingBox())!;
    const cx = box.x + box.width / 2;
    const cy = box.y + box.height / 2;
    await page.mouse.move(cx, cy);
    await page.mouse.down();
    await page.mouse.move(cx + 30, cy + 30, { steps: 5 });
    await page.mouse.up();

    const dragCalls = await page.evaluate(
      () => (window as unknown as { __qdMock: MockHooks }).__qdMock.startDraggingCallCount(),
    );
    expect(dragCalls).toBeGreaterThan(0);
    await expect(page.locator('#app')).toHaveClass(/qd-app-lamp/, { timeout: 500 });

    // Then: a plain click (no movement) expands back to the list, in place.
    await lamp.click();
    await expect(page.locator('#app')).not.toHaveClass(/qd-app-lamp/);
    await expect(page.locator('.qd-header')).toBeVisible();
    await expect(page.locator('#qd-pin')).toHaveAttribute('aria-pressed', 'true');
  });

  test('unpin-from-lamp via the right-click menu reverts to list + unpinned (R-25.2)', async ({ page }) => {
    await gotoPopup(page, 'lamp');
    await expect(page.locator('#app')).toHaveClass(/qd-app-lamp/);

    await page.locator('#qd-lamp').click({ button: 'right' });
    await expect(page.locator('.qd-ctx-menu')).toBeVisible();
    await page.getByRole('button', { name: 'Unpin' }).click();

    await expect(page.locator('#app')).not.toHaveClass(/qd-app-lamp/);
    await expect(page.locator('.qd-header')).toBeVisible();
    await expect(page.locator('#qd-pin')).toHaveAttribute('aria-pressed', 'false');
    await expect(page.locator('#qd-collapse')).toBeHidden();
  });

  test('a pending ask does not auto-expand the lamp (R-25.3)', async ({ page }) => {
    // `lamp` fixture ships a pending ask on s1 alongside the pinned+collapsed
    // state; nothing in the client reacts to ask/perm arrival by switching
    // popupMode, so the lamp must still be showing on load.
    await gotoPopup(page, 'lamp');
    await expect(page.locator('#app')).toHaveClass(/qd-app-lamp/);
    await expect(page.locator('#qd-lamp')).toBeVisible();
  });

  test('reduced motion disables the working lamp pulse (R-25.5)', async ({ page }) => {
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await gotoPopup(page, 'token-stats');
    await page.locator('#qd-pin').click();
    await page.locator('#qd-collapse').click();

    const dot = page.locator('#qd-lamp-dot');
    await expect(dot).toHaveAttribute('data-status', 'working');
    // Chromium reports the computed value in whichever unit it normalizes to
    // (observed: seconds, e.g. "1e-06s"); parse the leading number rather than
    // matching the exact string — the global reduced-motion rule collapses it
    // to 0.001ms either way, so both "0.001ms" and "1e-06s" parse well under 1.
    const duration = await dot.evaluate((el) => getComputedStyle(el).animationDuration);
    expect(parseFloat(duration)).toBeLessThan(0.01);
  });
});

test.describe('onboarding refresh (R-25.4)', () => {
  test('carries the closing pin/lamp tip line and the takeover explanation', async ({ page }) => {
    await gotoPopup(page, 'onboarding');
    await expect(page.locator('.qd-onboarding-tip')).toContainText('shrink it to a traffic light');
    await expect(page.locator('.qd-onboarding-hint')).toContainText('Claude Code will ask permission here');
    // Consolidated consent line from R-16.4, still present under the refresh.
    await expect(page.locator('.qd-onboarding-takeover')).toContainText('Take over permission prompts');
  });

  test('the settings pane exposes takeoverPermissions and showTokenStats toggles', async ({ page }) => {
    await gotoPopup(page, 'default');
    await page.locator('#qd-gear').click();
    const toggle = (label: string) => page.locator('.qd-toggle-row', { hasText: label }).locator('.qd-toggle');
    await expect(toggle('Take over permission prompts')).toBeVisible();
    await expect(toggle('Show token usage on rows')).toBeVisible();
  });
});
