import { expect, test } from '@playwright/test';
import { gotoPopup, row } from '../helpers/popup';

// SPEC §36 (working-time timer), UI half. The Rust engine owns the anchor
// semantics (unit-tested in `crates/deck-core/tests/engine_worktimer.rs`);
// against the mocked IPC this proves the UI RENDERS them: a working row shows a
// live, running work-time counter (anchored at the work start, not
// time-in-status), and an idle row that just finished a turn freezes it as
// "took <duration>" rather than a running idle timer.
test.describe('working-time timer (§36)', () => {
  test('a working row shows a running work-time counter', async ({ page }) => {
    await gotoPopup(page, 'default');
    const time = row(page, 'dream-book-web').locator('.qd-row-time');
    await expect(time).toBeVisible();
    // Live counter, not the frozen "took …".
    await expect(time).not.toContainText('took');
    await expect(time).toHaveText(/^\d+m \d{2}s$/);
    // The 1s ticker advances it (base derived from workStartedMs, ticked up
    // locally) — capture, wait past one tick, and assert it moved.
    const before = await time.textContent();
    await page.waitForTimeout(1500);
    const after = await time.textContent();
    expect(after).not.toBe(before);
  });

  test('an idle row that finished a turn shows "took <duration>"', async ({ page }) => {
    await gotoPopup(page, 'default');
    const time = row(page, 'shitty-apps-back').locator('.qd-row-time');
    await expect(time).toBeVisible();
    // lastWorkMs = 200_000 → "took 3m 20s", frozen (dimmer `took` class).
    await expect(time).toHaveText('took 3m 20s');
    await expect(time).toHaveClass(/took/);
    // Frozen: it does not tick up while idle.
    const before = await time.textContent();
    await page.waitForTimeout(1500);
    await expect(time).toHaveText(before ?? '');
  });
});
