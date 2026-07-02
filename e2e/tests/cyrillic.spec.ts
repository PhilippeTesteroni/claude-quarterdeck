import { expect, test } from '@playwright/test';
import { gotoAsk, gotoPopup, row } from '../helpers/popup';

// SPEC R-5.3: "Cyrillic/Unicode paths MUST work end-to-end". Exercises the
// popup rows, cwd tooltip, branch chip, ask mirror row, and the dedicated ask
// window with real Cyrillic (and, for good measure, CJK) text end to end.
test.describe('Cyrillic / Unicode', () => {
  test('renders Cyrillic and CJK project/title/branch without mangling', async ({ page }) => {
    await gotoPopup(page, 'cyrillic');

    const first = row(page, 'сон-книга');
    await expect(first).toBeVisible();
    await expect(first.locator('.qd-row-dot')).toHaveAttribute('data-status', 'attention');
    await expect(first.locator('.qd-row-title')).toHaveText('Исправить генератор снов — юникод и кириллица ✓');
    await expect(first.locator('.qd-row-branch')).toHaveText('фикс/юникод');
    // R-7.2 hover tooltip is the raw cwd (title attribute), incl. the emoji.
    await expect(first).toHaveAttribute('title', 'C:/Users/phily/projects/сон-книга 📖');

    const second = row(page, '知识库');
    await expect(second).toBeVisible();
    await expect(second.locator('.qd-row-title')).toHaveText('修复本地化生成器');
    await expect(second.locator('.qd-row-dot')).toHaveAttribute('data-status', 'working');

    // Ask mirror row: Cyrillic question + options, answerable normally.
    await expect(page.locator('.qd-ask-row-question')).toHaveText(
      'Использовать московский часовой пояс для крон-джобы?',
    );
    await page.getByRole('button', { name: 'Да', exact: true }).click();
    await expect(page.locator('.qd-ask-row')).toHaveCount(0);
    await expect(first.locator('.qd-row-dot')).toHaveAttribute('data-status', 'idle');
  });

  test('the dedicated ask window renders and answers a Cyrillic question', async ({ page }) => {
    await gotoAsk(page, 'cyrillic');
    await expect(page.locator('.qd-ask-identity-project')).toHaveText('сон-книга');
    await expect(page.locator('.qd-ask-question')).toHaveText(
      'Использовать московский часовой пояс для крон-джобы?',
    );
    await expect(page.locator('.qd-ask-option-text')).toHaveText(['Да', 'Нет, UTC']);

    // Keyboard shortcut works identically for non-Latin option labels.
    await page.keyboard.press('1');
    await expect(page.locator('.qd-ask-empty')).toBeVisible();
  });
});
