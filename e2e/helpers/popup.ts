import type { Locator, Page } from '@playwright/test';

/** Navigates to the popup window with a given mock scenario. `story=off`
 * freezes the mock's background timeline so assertions are deterministic
 * (see `ui/src/tauri-mock.ts` module doc). */
export async function gotoPopup(
  page: Page,
  scenario: string,
  extraParams: Record<string, string> = {},
): Promise<void> {
  const params = new URLSearchParams({ scenario, story: 'off', ...extraParams });
  await page.goto(`/popup.html?${params.toString()}`);
  // Wait for the mock's first snapshot to render (footer or empty state).
  await page.waitForSelector('#qd-content', { state: 'attached' });
}

/** Navigates to the ask window with a given mock scenario. */
export async function gotoAsk(
  page: Page,
  scenario: string,
  extraParams: Record<string, string> = {},
): Promise<void> {
  const params = new URLSearchParams({ scenario, story: 'off', ...extraParams });
  await page.goto(`/ask.html?${params.toString()}`);
  await page.waitForSelector('#qd-ask-content', { state: 'attached' });
}

export function row(page: Page, project: string): Locator {
  return page.locator('.qd-row', { has: page.locator('.qd-row-project', { hasText: project }) });
}

export function watchlineSegment(page: Page, status: string): Locator {
  return page.locator(`.qd-watchline-seg[data-status="${status}"]`);
}

/** Reads a `.qd-watchline-seg`'s `flex-basis` (set inline as a `%` string). */
export async function segmentBasis(locator: Locator): Promise<number> {
  const style = await locator.getAttribute('style');
  const match = style?.match(/flex-basis:\s*([\d.]+)%/);
  return match ? Number(match[1]) : 0;
}
