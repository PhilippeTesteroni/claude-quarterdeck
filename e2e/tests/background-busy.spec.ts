import { expect, test } from '@playwright/test';
import { gotoPopup, row } from '../helpers/popup';

// SPEC §21 (background work shows as working + subagent badge) and §22 (honest
// time-in-status for pre-existing sessions), UI half. The engine (Rust) owns the
// busy-override/badge/seeding logic and is unit-tested in `crates/deck-core`;
// against the mocked IPC this proves the UI RENDERS whatever the shell computed:
// the `⛭ N` badge (R-21.2), a yellow working row for a background-busy session
// (R-21.1), the estimated `~` time marker (R-22.4), and the session-age tooltip
// (R-22.3).
test.describe('background-busy scenario (§21/§22)', () => {
  test('busy-overridden row shows working with a ⛭ multi-agent glyph (R-21.1/§37)', async ({ page }) => {
    await gotoPopup(page, 'background-busy');

    const busyRow = row(page, 'quarterdeck');
    // The row displays working (the shell resolved the busy-override).
    await expect(busyRow.locator('.qd-row-dot')).toHaveAttribute('data-status', 'working');

    // §37: a plain multi-agent glyph is present — just the icon, no count.
    const badge = busyRow.locator('.qd-row-subagents');
    await expect(badge).toBeVisible();
    await expect(badge).toHaveText('⛭');
    // The glyph carries no number/spend text.
    await expect(badge).not.toContainText(/\d/);
  });

  test('a non-busy row shows no subagent badge (R-21.2 N=0 hides it)', async ({ page }) => {
    await gotoPopup(page, 'background-busy');
    // The idle/estimated row carries no subagents → no badge.
    await expect(row(page, 'dream-book-web').locator('.qd-row-subagents')).toHaveCount(0);
  });

  test('estimated (pre-existing) row renders its time with a ~ marker (R-22.4)', async ({ page }) => {
    await gotoPopup(page, 'background-busy');

    const estimatedTime = row(page, 'dream-book-web').locator('.qd-row-time');
    await expect(estimatedTime).toHaveClass(/estimated/);
    await expect(estimatedTime).toHaveText(/^~\d/); // e.g. "~12m 40s"

    // The exact (hook-tracked) busy row is NOT estimated — no `~`, no class.
    const exactTime = row(page, 'quarterdeck').locator('.qd-row-time');
    await expect(exactTime).not.toHaveClass(/estimated/);
    await expect(exactTime).not.toHaveText(/^~/);
  });

  test('row tooltip carries the session age alongside the cwd (R-22.3)', async ({ page }) => {
    await gotoPopup(page, 'background-busy');
    // title = "<cwd>\nsession 2h 14m" (R-22.3): both parts present.
    const title = await row(page, 'quarterdeck').getAttribute('title');
    expect(title).toContain('C:/Users/phily/projects/quarterdeck');
    expect(title).toMatch(/session 2h 14m/);
  });
});
