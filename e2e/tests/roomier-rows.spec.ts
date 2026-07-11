import { expect, test } from '@playwright/test';
import { gotoPopup, row } from '../helpers/popup';

// SPEC §42 R-42 (roomier two-line agent rows). Each agent row lays out over two
// lines with breathing room: line 1 = status dot + name + right-aligned §36
// working-time; line 2 = project/branch + §23 `ctx% · spend` + the §37
// multi-agent glyph. Dense-but-calm (Mission Control). The §31 fixed 5-row
// settings height must still sit under the 160..560 window band after the rows
// grew (it's a stable constant pane, so it never drifts near the cap).
test.describe('roomier two-line rows (§42)', () => {
  test('a row renders line 1 (dot + name + time) over line 2 (project + usage)', async ({ page }) => {
    await gotoPopup(page, 'default');
    const r = row(page, 'dream-book-web');

    // Line 1: the status dot, the session name, and the working-time all live
    // on the primary line.
    const line1 = r.locator('.qd-row-line1');
    await expect(line1.locator('.qd-row-dot')).toBeVisible();
    await expect(line1.locator('.qd-row-title')).toHaveText('Fix locale-native generator cron');
    await expect(line1.locator('.qd-row-time')).toBeVisible();

    // Line 2: project + branch, then the usage/glyph grouped hard-right.
    const line2 = r.locator('.qd-row-line2');
    await expect(line2.locator('.qd-row-project')).toHaveText('dream-book-web');
    await expect(line2.locator('.qd-row-branch')).toHaveText('main');

    // The name/time are NOT in the detail line; the project is NOT in line 1.
    await expect(line1.locator('.qd-row-project')).toHaveCount(0);
    await expect(line2.locator('.qd-row-title')).toHaveCount(0);
    await expect(line2.locator('.qd-row-time')).toHaveCount(0);
  });

  test('the row is a two-line vertical stack with roomier padding', async ({ page }) => {
    await gotoPopup(page, 'default');
    const r = row(page, 'dream-book-web');

    // The row stacks its two lines vertically (was a single flex line).
    await expect(r).toHaveCSS('flex-direction', 'column');
    // Roomier vertical padding than the old 8px single-line row.
    const padTop = await r.evaluate((el) => parseFloat(getComputedStyle(el).paddingTop));
    expect(padTop).toBeGreaterThanOrEqual(10);

    // The name reads larger than the subordinate detail line's project.
    const nameSize = await r
      .locator('.qd-row-title')
      .evaluate((el) => parseFloat(getComputedStyle(el).fontSize));
    const projSize = await r
      .locator('.qd-row-project')
      .evaluate((el) => parseFloat(getComputedStyle(el).fontSize));
    expect(nameSize).toBeGreaterThan(projSize);
    expect(nameSize).toBeGreaterThanOrEqual(13);
  });

  test('the §37 multi-agent glyph rides line 2 with the usage, no counts', async ({ page }) => {
    await gotoPopup(page, 'token-stats');
    const r = row(page, 'quarterdeck');
    const line2 = r.locator('.qd-row-line2');
    // Both the ctx%·spend usage and the multi-agent glyph sit on the detail line.
    await expect(line2.locator('.qd-row-usage')).toContainText('ctx');
    const glyph = line2.locator('.qd-row-subagents');
    await expect(glyph).toHaveText('⛭');
    await expect(glyph).not.toContainText(/\d/);
  });

  test('the frozen §36 "took" time rides line 1 of an idle row', async ({ page }) => {
    await gotoPopup(page, 'default');
    // shitty-apps-back finished a 3m 20s turn → "took 3m 20s" on line 1.
    const r = row(page, 'shitty-apps-back');
    const time = r.locator('.qd-row-line1 .qd-row-time');
    await expect(time).toContainText('took');
  });

  test('two-line layout holds in light, dark, and reduced motion', async ({ page }) => {
    for (const scheme of ['dark', 'light'] as const) {
      await page.emulateMedia({ colorScheme: scheme });
      await gotoPopup(page, 'default');
      const r = row(page, 'dream-book-web');
      await expect(r).toHaveCSS('flex-direction', 'column');
      await expect(r.locator('.qd-row-line1 .qd-row-title')).toBeVisible();
      await expect(r.locator('.qd-row-line2 .qd-row-project')).toBeVisible();
    }

    await page.emulateMedia({ colorScheme: 'dark', reducedMotion: 'reduce' });
    await gotoPopup(page, 'default');
    const r = row(page, 'dream-book-web');
    await expect(r.locator('.qd-row-line1')).toBeVisible();
    await expect(r.locator('.qd-row-line2')).toBeVisible();
  });

  test('§31 fixed 5-row settings height still sits under the 560 cap', async ({ page }) => {
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await gotoPopup(page, 'default');
    await page.locator('#qd-gear').click();
    await expect(page.locator('#qd-settings')).toHaveClass(/open/);
    const settingsH = await page.evaluate(
      () =>
        (
          window as unknown as { __qdMock: { lastResizeContentHeight(): number | null } }
        ).__qdMock.lastResizeContentHeight(),
    );
    expect(settingsH).not.toBeNull();
    expect(settingsH ?? 0).toBeGreaterThan(0);
    expect(settingsH ?? 0).toBeLessThanOrEqual(560);
  });
});
