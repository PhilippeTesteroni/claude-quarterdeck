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

  test('§41: renders one pie wedge per agent, each filled by that agent status, plus the attention badge (R-25.1)', async ({ page }) => {
    // Fixture: 2 sessions (s1 attention, s2 working), pinned + already collapsed.
    await gotoPopup(page, 'lamp');

    await expect(page.locator('#app')).toHaveClass(/qd-app-lamp/);
    const wedges = page.locator('#qd-lamp-pie .qd-lamp-wedge');
    await expect(wedges).toHaveCount(2);
    // Wedges follow the engine-sorted session order (attention before working).
    await expect(wedges.nth(0)).toHaveAttribute('data-status', 'attention');
    await expect(wedges.nth(1)).toHaveAttribute('data-status', 'working');
    // No neutral ring while agents exist.
    await expect(page.locator('#qd-lamp-pie .qd-lamp-ring')).toHaveCount(0);

    const badge = page.locator('#qd-lamp-badge');
    await expect(badge).toBeVisible();
    await expect(badge).toHaveText('1');

    // R-25.3: hover tooltip carries the same counts line as the popup footer.
    await expect(page.locator('#qd-lamp')).toHaveAttribute('title', /needs you/);
  });

  test('§41: per-status fills resolve to the status tokens in both dark and light', async ({ page }) => {
    // token-stats = 3 sessions (working, idle, working), no attention.
    for (const [scheme, working, idle] of [
      ['dark', 'rgb(210, 153, 34)', 'rgb(63, 185, 80)'], // #d29922 / #3fb950
      ['light', 'rgb(154, 103, 0)', 'rgb(26, 127, 55)'], // #9a6700 / #1a7f37
    ] as const) {
      await page.emulateMedia({ colorScheme: scheme });
      await gotoPopup(page, 'token-stats');
      await page.locator('#qd-pin').click();
      await page.locator('#qd-collapse').click();

      const wedges = page.locator('#qd-lamp-pie .qd-lamp-wedge');
      await expect(wedges).toHaveCount(3);
      // Two working sessions + one idle. Select wedges by status rather than
      // array index: the engine sort (status priority, then most-recent-active)
      // interleaves the two working rows around the idle one, so the fixed
      // positions aren't working/idle/working.
      const workingWedge = page.locator('#qd-lamp-pie .qd-lamp-wedge[data-status="working"]').first();
      const idleWedge = page.locator('#qd-lamp-pie .qd-lamp-wedge[data-status="idle"]');
      await expect(idleWedge).toHaveCount(1);
      const fillWorking = await workingWedge.evaluate((el) => getComputedStyle(el).fill);
      const fillIdle = await idleWedge.evaluate((el) => getComputedStyle(el).fill);
      expect(fillWorking).toBe(working);
      expect(fillIdle).toBe(idle);

      // No attention session here → badge hidden.
      await expect(page.locator('#qd-lamp-badge')).toBeHidden();
    }
  });

  test('§41: zero agents render a neutral ring, no wedges (R-25.1)', async ({ page }) => {
    // `empty` has no sessions — pin + collapse into the lamp.
    await gotoPopup(page, 'empty');
    await page.locator('#qd-pin').click();
    await page.locator('#qd-collapse').click();

    await expect(page.locator('#app')).toHaveClass(/qd-app-lamp/);
    await expect(page.locator('#qd-lamp-pie .qd-lamp-ring')).toHaveCount(1);
    await expect(page.locator('#qd-lamp-pie .qd-lamp-wedge')).toHaveCount(0);
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

  test('reduced motion disables the working lamp-wedge pulse (R-25.5)', async ({ page }) => {
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await gotoPopup(page, 'token-stats');
    await page.locator('#qd-pin').click();
    await page.locator('#qd-collapse').click();

    const wedge = page.locator('#qd-lamp-pie .qd-lamp-wedge[data-status="working"]').first();
    await expect(wedge).toHaveCount(1);
    // Chromium reports the computed value in whichever unit it normalizes to
    // (observed: seconds, e.g. "1e-06s"); parse the leading number rather than
    // matching the exact string — the global reduced-motion rule collapses it
    // to 0.001ms either way, so both "0.001ms" and "1e-06s" parse well under 1.
    const duration = await wedge.evaluate((el) => getComputedStyle(el).animationDuration);
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
