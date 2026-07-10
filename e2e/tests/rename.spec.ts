import { expect, test } from '@playwright/test';
import { gotoPopup, row } from '../helpers/popup';

// SPEC §27 (R-27.5): rename a session by double-clicking its name. Double-click
// the title → an inline `<input>` seeded with the current title → Enter/blur
// commits (persisting in the mock, which is the frontend's source of truth in
// browser mode) → Escape cancels → an empty submit clears the override back to
// the derived name. The open editor must survive a background `deck://state`
// rebuild (R-27.5, the `captureAskInputs` analog).
type Hooks = {
  answerAsk(askId: string, answer: string, kind: string): void;
};

const pushBackgroundState = (page: import('@playwright/test').Page): Promise<void> =>
  // Answering a queued, non-primary ask drives an unrelated snapshot re-push,
  // exercising the "editor survives renderContent rebuild" guard.
  page.evaluate(() => (window as unknown as { __qdMock: Hooks }).__qdMock.answerAsk('a2', '', 'dismissed'));

test.describe('rename by double-click (R-27.5)', () => {
  test('double-click opens an inline editor seeded with the current title', async ({ page }) => {
    await gotoPopup(page, 'default');
    const target = row(page, 'dream-book-web');
    await expect(target.locator('.qd-row-title')).toHaveText('Fix locale-native generator cron');

    await target.locator('.qd-row-title').dblclick();

    const input = page.locator('.qd-row-title-edit');
    await expect(input).toBeFocused();
    await expect(input).toHaveValue('Fix locale-native generator cron');
  });

  test('Enter commits the new name and it persists across a background state push', async ({ page }) => {
    await gotoPopup(page, 'default');
    const target = row(page, 'dream-book-web');
    await target.locator('.qd-row-title').dblclick();

    await page.locator('.qd-row-title-edit').fill('Release train');
    await page.locator('.qd-row-title-edit').press('Enter');

    // The editor closes and the row shows the renamed title.
    await expect(page.locator('.qd-row-title-edit')).toHaveCount(0);
    await expect(row(page, 'dream-book-web').locator('.qd-row-title')).toHaveText('Release train');

    // The rename persists in the mock across an unrelated snapshot re-push.
    await pushBackgroundState(page);
    await expect(row(page, 'dream-book-web').locator('.qd-row-title')).toHaveText('Release train');
  });

  test('an empty submit clears the override, restoring the derived name', async ({ page }) => {
    await gotoPopup(page, 'default');
    const target = row(page, 'dream-book-web');

    // First rename it…
    await target.locator('.qd-row-title').dblclick();
    await page.locator('.qd-row-title-edit').fill('Temporary name');
    await page.locator('.qd-row-title-edit').press('Enter');
    await expect(row(page, 'dream-book-web').locator('.qd-row-title')).toHaveText('Temporary name');

    // …then clear it with an empty submit → back to the original derived title.
    await row(page, 'dream-book-web').locator('.qd-row-title').dblclick();
    await page.locator('.qd-row-title-edit').fill('   ');
    await page.locator('.qd-row-title-edit').press('Enter');
    await expect(row(page, 'dream-book-web').locator('.qd-row-title')).toHaveText(
      'Fix locale-native generator cron',
    );
  });

  test('Escape cancels without renaming', async ({ page }) => {
    await gotoPopup(page, 'default');
    const target = row(page, 'dream-book-web');
    await target.locator('.qd-row-title').dblclick();

    await page.locator('.qd-row-title-edit').fill('Discard me');
    await page.locator('.qd-row-title-edit').press('Escape');

    await expect(page.locator('.qd-row-title-edit')).toHaveCount(0);
    await expect(row(page, 'dream-book-web').locator('.qd-row-title')).toHaveText(
      'Fix locale-native generator cron',
    );
  });

  test('the open editor survives a background state rebuild (R-27.5)', async ({ page }) => {
    await gotoPopup(page, 'default');
    const target = row(page, 'dream-book-web');
    await target.locator('.qd-row-title').dblclick();

    // Type but do NOT commit, then force an unrelated snapshot re-push.
    await page.locator('.qd-row-title-edit').fill('Half-typed nam');
    await pushBackgroundState(page);

    // The editor is still open and keeps the in-progress value + focus.
    const input = page.locator('.qd-row-title-edit');
    await expect(input).toHaveCount(1);
    await expect(input).toHaveValue('Half-typed nam');
    await expect(input).toBeFocused();
  });
});
