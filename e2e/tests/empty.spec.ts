import { expect, test } from '@playwright/test';
import { gotoPopup, segmentBasis, watchlineSegment } from '../helpers/popup';

// SPEC R-7.5 empty state + R-2.6 "zero sessions -> neutral/gray icon" (mirrored
// client-side by the watch line's "none" segment).
test.describe('empty state', () => {
  test('shows the empty-state copy and hides the footer when hooks are installed', async ({ page }) => {
    await gotoPopup(page, 'empty');

    await expect(page.locator('.qd-empty-title')).toContainText('No Claude Code sessions yet');
    await expect(page.locator('.qd-empty-title code')).toHaveText('claude');
    await expect(page.locator('.qd-empty-health')).toHaveText('Hooks installed — waiting for a session.');

    // R-7.3: footer is hidden entirely when there is nothing to count.
    await expect(page.locator('#qd-footer')).toHaveCSS('display', 'none');

    // R-2.6 mirrored client-side: the "none" watch-line segment takes 100%,
    // every status segment is 0%.
    await expect(watchlineSegment(page, 'none')).toHaveCSS('flex-basis', '100%');
    for (const status of ['attention', 'working', 'idle', 'dead']) {
      expect(await segmentBasis(watchlineSegment(page, status))).toBe(0);
    }

    // No hooks issue -> the gear has no red dot.
    await expect(page.locator('#qd-gear')).not.toHaveClass(/has-issue/);
  });

  test('shows the "install hooks" empty-state message when hooks are missing', async ({ page }) => {
    // `nohooks` still has 2 sessions, so this exercises the *other* empty
    // path: the persistent hooks banner (R-7.5), not the empty state. Kept
    // here because it's the direct counterpart of the health line above.
    await gotoPopup(page, 'nohooks');
    await expect(page.locator('.qd-banner-text')).toHaveText(
      'Hooks not installed — sessions won’t be detected.',
    );
    await expect(page.locator('#qd-gear')).toHaveClass(/has-issue/);
  });
});
