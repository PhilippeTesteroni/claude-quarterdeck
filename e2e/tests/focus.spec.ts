import { expect, test } from '@playwright/test';
import { gotoPopup, row } from '../helpers/popup';

// SPEC v1.1 R-15.4: click-to-focus terminal. A row click focuses the terminal
// window (the former no-op is gone); the context menu gains "Focus terminal" as
// its first item; a focus failure surfaces an inline "Couldn't find the terminal
// window" notice (R-15.4b). No real terminal exists in mock/browser mode, so the
// mock records the `focus_terminal` calls for assertion.
type Hooks = {
  focusTerminalCallCount(): number;
  lastFocusTerminalId(): string | null;
};

const focusCount = (page: import('@playwright/test').Page): Promise<number> =>
  page.evaluate(() => (window as unknown as { __qdMock: Hooks }).__qdMock.focusTerminalCallCount());
const lastFocusId = (page: import('@playwright/test').Page): Promise<string | null> =>
  page.evaluate(() => (window as unknown as { __qdMock: Hooks }).__qdMock.lastFocusTerminalId());

test.describe('click-to-focus terminal (R-15.4)', () => {
  test('a row click invokes focus_terminal for that session', async ({ page }) => {
    await gotoPopup(page, 'default');
    expect(await focusCount(page)).toBe(0);

    await row(page, 'dream-book-web').click();

    await expect.poll(() => focusCount(page)).toBe(1);
    expect(await lastFocusId(page)).toBe('s2');
  });

  test('the context menu has "Focus terminal" first and it focuses the row', async ({ page }) => {
    await gotoPopup(page, 'default');
    await row(page, 'quarterdeck').click({ button: 'right' });

    const items = page.locator('.qd-ctx-menu .qd-ctx-item');
    await expect(items.first()).toHaveText('Focus terminal');

    await items.first().click();
    await expect.poll(() => focusCount(page)).toBeGreaterThanOrEqual(1);
    expect(await lastFocusId(page)).toBe('s1');
  });

  test('a focus failure shows the inline "Couldn\'t find the terminal window" notice (R-15.4b)', async ({ page }) => {
    await gotoPopup(page, 'focus-fail');
    await row(page, 'quarterdeck').click();
    await expect(page.locator('.qd-focus-notice')).toHaveText("Couldn't find the terminal window");
  });
});
