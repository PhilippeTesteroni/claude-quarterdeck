import { expect, test } from '@playwright/test';
import { gotoAsk } from '../helpers/popup';

// SPEC R-8.3: the dedicated always-on-top ask window — FIFO queue, option
// buttons (keyboard 1-9), free text, dismiss, countdown, "N more waiting"
// badge, and the R-8.2 "Unknown agent" unmatched-ask display.
test.describe('ask window', () => {
  test('renders the first pending ask, answers by keyboard digit, then the next by free text', async ({ page }) => {
    await gotoAsk(page, 'default');

    // Primary ask (a1): identity, question, options, countdown, "1 more waiting".
    await expect(page.locator('.qd-ask-identity-project')).toHaveText('quarterdeck');
    await expect(page.locator('.qd-ask-question')).toContainText('Which approach for the watch line segments');
    await expect(page.locator('.qd-ask-option')).toHaveCount(3);
    await expect(page.locator('.qd-ask-option-key').first()).toHaveText('1');
    await expect(page.locator('.qd-ask-countdown')).toContainText('Times out in');
    await expect(page.locator('#qd-ask-badge')).toHaveText('1 more waiting');

    // Keyboard shortcut "2" answers with the 2nd option (R-8.3 "keyboard 1-9").
    await page.keyboard.press('2');

    // a2 has no options/timeout: freeform + dismiss only, badge hidden.
    await expect(page.locator('.qd-ask-question')).toHaveText(
      'Should the empty state link straight to the docs, or just name the command?',
    );
    await expect(page.locator('.qd-ask-option')).toHaveCount(0);
    await expect(page.locator('.qd-ask-countdown')).toHaveCount(0);
    await expect(page.locator('#qd-ask-badge')).toBeHidden();

    await page.locator('.qd-ask-freeform input').fill('Just name the command');
    await page.getByRole('button', { name: 'Send answer' }).click();

    await expect(page.locator('.qd-ask-empty')).toHaveText('No pending questions.');
    await expect(page.locator('#qd-ask-badge')).toBeHidden();
  });

  test('option buttons are clickable and free-text Enter submits', async ({ page }) => {
    await gotoAsk(page, 'default');
    await page.getByRole('button', { name: 'Either, pick for me' }).click();
    await expect(page.locator('.qd-ask-question')).toHaveText(
      'Should the empty state link straight to the docs, or just name the command?',
    );

    const input = page.locator('.qd-ask-freeform input');
    await input.fill('Docs link');
    await input.press('Enter');
    await expect(page.locator('.qd-ask-empty')).toBeVisible();
  });

  test('dismiss clears the ask without an answer', async ({ page }) => {
    await gotoAsk(page, 'default');
    await page.getByRole('button', { name: 'Dismiss' }).click();
    // a2 (no options) is now primary.
    await expect(page.locator('.qd-ask-question')).toContainText('Should the empty state link');
    await page.getByRole('button', { name: 'Dismiss' }).click();
    await expect(page.locator('.qd-ask-empty')).toBeVisible();
  });

  test('unmatched asks show "Unknown agent (<context>)" (R-8.2)', async ({ page }) => {
    await gotoAsk(page, 'ask-unknown');
    await expect(page.locator('.qd-ask-identity-project')).toContainText('Unknown agent (');
    // `truncate(context, 42)` cuts the cwd short with an ellipsis (R-8.2) —
    // assert the surviving prefix rather than the full path.
    await expect(page.locator('.qd-ask-identity-project')).toContainText(
      'C:/Users/phily/projects/some-untracked-sc',
    );
    await expect(page.locator('.qd-ask-identity .qd-row-dot')).toHaveAttribute('data-status', 'dead');
  });

  test('no pending asks renders the empty state', async ({ page }) => {
    await gotoAsk(page, 'empty');
    await expect(page.locator('.qd-ask-empty')).toHaveText('No pending questions.');
    await expect(page.locator('#qd-ask-badge')).toBeHidden();
  });
});
