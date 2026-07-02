import { expect, test } from '@playwright/test';
import { gotoPopup } from '../helpers/popup';

// SPEC §7 tokens: adaptive light/dark via `prefers-color-scheme`, and
// `prefers-reduced-motion` turning off the pulse/watch-line animations
// (styles.css: dark is the `:root` default, light overrides under the media
// query; reduced-motion clamps every animation/transition to ~0).
test.describe('theme', () => {
  test('dark is the default token set', async ({ page }) => {
    await page.emulateMedia({ colorScheme: 'dark' });
    await gotoPopup(page, 'default');
    const bg = await page.evaluate(() => getComputedStyle(document.body).backgroundColor);
    expect(bg).toBe('rgb(13, 17, 23)'); // #0d1117
    const wordmark = await page.evaluate(() => getComputedStyle(document.querySelector('.qd-wordmark')!).color);
    expect(wordmark).toBe('rgb(217, 119, 87)'); // #d97757 accent, same in both themes
  });

  test('light overrides the surface tokens', async ({ page }) => {
    await page.emulateMedia({ colorScheme: 'light' });
    await gotoPopup(page, 'default');
    const bg = await page.evaluate(() => getComputedStyle(document.body).backgroundColor);
    expect(bg).toBe('rgb(255, 255, 255)'); // #ffffff
    const text = await page.evaluate(() => getComputedStyle(document.body).color);
    expect(text).toBe('rgb(31, 35, 40)'); // #1f2328
  });

  test('reduced motion collapses the working-dot pulse and watch-line transition', async ({ page }) => {
    await page.emulateMedia({ colorScheme: 'dark', reducedMotion: 'reduce' });
    await gotoPopup(page, 'default');

    const workingDot = page.locator('.qd-row-dot[data-status="working"]').first();
    await expect(workingDot).toBeVisible();
    const animationDuration = await workingDot.evaluate((el) => getComputedStyle(el).animationDuration);
    // styles.css forces `0.001ms !important` under reduced-motion; parse
    // whatever unit the engine reports it back in rather than assume "ms".
    expect(parseFloat(animationDuration)).toBeLessThan(0.01);

    const seg = page.locator('.qd-watchline-seg').first();
    const transitionDuration = await seg.evaluate((el) => getComputedStyle(el).transitionDuration);
    expect(parseFloat(transitionDuration)).toBeLessThan(0.01);
  });

  test('no reduced motion: the working dot keeps its 1.6s pulse', async ({ page }) => {
    await page.emulateMedia({ colorScheme: 'dark', reducedMotion: 'no-preference' });
    await gotoPopup(page, 'default');
    const workingDot = page.locator('.qd-row-dot[data-status="working"]').first();
    const animationDuration = await workingDot.evaluate((el) => getComputedStyle(el).animationDuration);
    expect(animationDuration).toBe('1.6s');
  });
});
