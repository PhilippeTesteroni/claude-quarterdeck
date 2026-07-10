import { expect, test, type Page } from '@playwright/test';
import { gotoAsk } from '../helpers/popup';

// SPEC §35.2: the always-on-top ask window auto-sizes to its content — it grows
// for a large §29 form / long perm input and shrinks back for a short one — via
// the `resize_ask` report the frontend sends after each render. The shell clamps
// it to 140..=640 + `set_size`s the real window (Rust-side, unit-tested); there's
// no OS window in mock/browser mode, so the mock records the last reported
// content height and the specs assert grow/shrink against it.
test.describe('ask window auto-size (§35.2)', () => {
  type Hooks = {
    lastAskResizeHeight(): number | null;
    answerAsk(id: string, answer: string, kind: string): void;
  };
  const reported = (page: Page): Promise<number | null> =>
    page.evaluate(() => (window as unknown as { __qdMock: Hooks }).__qdMock.lastAskResizeHeight());

  test('a large form reports a taller height than the short ask it collapses to', async ({ page }) => {
    await gotoAsk(page, 'ask-form');
    // The multi-question form is primary (tall content).
    await expect(page.locator('.qd-ask-form')).toHaveCount(1);
    const tall = await reported(page);
    expect(tall).not.toBeNull();
    expect(tall ?? 0).toBeGreaterThan(200);

    // Answer the big form (a1) → the short single-question ask (a2) takes over,
    // and the reported height shrinks right back down (never sticks tall).
    await page.evaluate(() =>
      (window as unknown as { __qdMock: Hooks }).__qdMock.answerAsk('a1', '', 'dismissed'),
    );
    await expect(page.locator('.qd-ask-form')).toHaveCount(0);
    await expect(page.locator('.qd-ask-question')).toContainText('Tag the release after merge?');
    const short = await reported(page);
    expect(short ?? 0).toBeLessThan(tall ?? 0);
  });

  test('the content area scrolls once the window hits the cap', async ({ page }) => {
    await gotoAsk(page, 'ask-form');
    // At the 640 cap the window stops growing and `.qd-ask-content` scrolls.
    const overflowY = await page
      .locator('.qd-ask-content')
      .evaluate((el) => getComputedStyle(el).overflowY);
    expect(overflowY).toBe('auto');
  });

  test('an empty ask window reports a compact height', async ({ page }) => {
    await gotoAsk(page, 'empty');
    await expect(page.locator('.qd-ask-empty')).toHaveText('No pending questions.');
    const compact = await reported(page);
    expect(compact).not.toBeNull();
    // Short content sits below the mid-band; the Rust clamp floors it at 140.
    expect(compact ?? 0).toBeLessThan(200);
  });
});
