import { expect, test } from '@playwright/test';
import { gotoPopup, row } from '../helpers/popup';

// SPEC §40: the deck is a control surface, not a document. Chrome (rows,
// header, lamp, settings labels) shows the default arrow and suppresses text
// selection; the text caret + selection return only on genuinely editable
// inputs (the §27 rename field, ask free-text).
const cursorOf = (locator: import('@playwright/test').Locator): Promise<string> =>
  locator.evaluate((el) => getComputedStyle(el).cursor);

const userSelectOf = (locator: import('@playwright/test').Locator): Promise<string> =>
  locator.evaluate((el) => getComputedStyle(el).userSelect);

test.describe('cursor: text-caret only on the editable row (§40)', () => {
  test('a row title shows the default arrow and is not selectable', async ({ page }) => {
    await gotoPopup(page, 'default');
    const title = row(page, 'dream-book-web').locator('.qd-row-title');
    await expect(title).toBeVisible();

    expect(await cursorOf(title)).toBe('default');
    expect(await userSelectOf(title)).toBe('none');
  });

  test('the header wordmark also shows the default arrow', async ({ page }) => {
    await gotoPopup(page, 'default');
    expect(await cursorOf(page.locator('.qd-wordmark'))).toBe('default');
  });

  test('the rename input shows the text caret and allows selection', async ({ page }) => {
    await gotoPopup(page, 'default');
    await row(page, 'dream-book-web').locator('.qd-row-title').dblclick();

    const input = page.locator('.qd-row-title-edit');
    await expect(input).toBeFocused();
    expect(await cursorOf(input)).toBe('text');
    expect(await userSelectOf(input)).toBe('text');
  });
});
