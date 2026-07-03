import { expect, test } from '@playwright/test';
import { gotoPopup } from '../helpers/popup';

// SPEC v1.1 §14 "Window behavior": R-14.1 draggable header, R-14.2 pin-on-top,
// R-14.3 true auto-height (160..560, no more 460 floor, shrinks back).
test.describe('popup window behavior (R-14)', () => {
  test('the header is a drag region while the gear and pin stay clickable (R-14.1)', async ({ page }) => {
    await gotoPopup(page, 'default');

    // The header row carries the drag-region marker (SPEC: "data-tauri-drag-region
    // ... interactive header controls (gear, pin) remain clickable").
    await expect(page.locator('.qd-header-row')).toHaveAttribute('data-tauri-drag-region', 'deep');

    // The gear and pin are real <button>s, not marked as drag regions
    // themselves — Tauri's drag script treats any clickable element without
    // its own marker as blocking the drag (see
    // tauri-2.11.5/src/window/scripts/drag.js), so both stay clickable.
    expect(await page.locator('#qd-gear').getAttribute('data-tauri-drag-region')).toBeNull();
    expect(await page.locator('#qd-pin').getAttribute('data-tauri-drag-region')).toBeNull();

    // Clicking the gear still opens settings (proves it isn't swallowed by a
    // drag-start on the ancestor header row).
    await page.locator('#qd-gear').click();
    await expect(page.locator('#qd-settings')).toHaveClass(/open/);
  });

  test('pin toggle flips visual state via the generic set_setting channel (R-14.2)', async ({ page }) => {
    await gotoPopup(page, 'default');

    const pin = page.locator('#qd-pin');
    await expect(pin).toHaveAttribute('aria-pressed', 'false');
    await expect(pin).not.toHaveClass(/pinned/);
    await expect(pin).toHaveAttribute('title', 'Pin on top');

    // Toggling sends `set_setting('popupPinned', true)` (same mechanism as
    // mcpEnabled, R-8.6); the Rust shell applies always-on-top + disables
    // hide-on-blur (unit-tested in `src-tauri/src/windows.rs`) and mirrors the
    // persisted flag back on the next snapshot — reflected here as the header
    // icon filling in with the clay accent.
    await pin.click();
    await expect(pin).toHaveAttribute('aria-pressed', 'true');
    await expect(pin).toHaveClass(/pinned/);
    await expect(pin).toHaveAttribute('title', 'Unpin');

    await pin.click();
    await expect(pin).toHaveAttribute('aria-pressed', 'false');
    await expect(pin).not.toHaveClass(/pinned/);
  });

  // SPEC R-14.3 regression: "50 rows → 0" must not get stuck at the grown
  // height. There's no real OS window to measure in mock/browser mode, but
  // the mock records every `resize_popup` content-height report the UI sends
  // (`popup.ts`'s `syncPopupHeight`), so this asserts the reported number
  // itself grows well past the old v1.0 460 floor with 50 rows on screen,
  // then shrinks back down once every row disappears — proving the UI keeps
  // re-measuring and reporting a smaller number instead of latching the peak.
  test('reported content height grows with 50 rows then shrinks back to ~empty (R-14.3)', async ({ page }) => {
    type Hooks = { lastResizeContentHeight(): number | null; removeAllSessions(): void };
    const lastResizeHeight = (): Promise<number | null> =>
      page.evaluate(() => (window as unknown as { __qdMock: Hooks }).__qdMock.lastResizeContentHeight());

    await gotoPopup(page, 'many-sessions');
    await expect(page.locator('.qd-row')).toHaveCount(50);
    await expect.poll(lastResizeHeight).toBeGreaterThan(900);

    // Clear the whole fleet in one shot (bulk `remove_row` equivalent).
    await page.evaluate(() => (window as unknown as { __qdMock: Hooks }).__qdMock.removeAllSessions());
    await expect(page.locator('.qd-row')).toHaveCount(0);

    await expect.poll(lastResizeHeight).toBeLessThan(300);
  });
});
