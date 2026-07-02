import { expect, test } from '@playwright/test';
import { gotoPopup, row, segmentBasis, watchlineSegment } from '../helpers/popup';

// SPEC §2/§7: "3-session lifecycle" — working -> attention -> recovery ->
// idle -> end.
//
// R-3.4 makes the frontend deliberately dumb: every hook-driven status
// transition (working/attention/dead, R-2.2 attention->working recovery,
// R-2.5 dead pruning) is engine (Rust) logic, exhaustively unit-tested in
// `crates/deck-core` (T1's own AC). A UI test against the *mocked* IPC layer
// cannot script those Rust transitions — it can only prove the UI renders
// whatever StateSnapshot it's handed, and that the UI-originated commands the
// mock DOES implement (`answer_ask`, `remove_row`) round-trip correctly.
//
// So this test walks the one lifecycle the mock genuinely drives end-to-end:
// the `default` fixture opens with s1 `attention` (2 pending asks) plus two
// `working` sessions (s2, s3) and one `idle` session (s4) already on screen;
// answering s1's last pending ask is the client-visible equivalent of R-2.4
// "recovery" (attention clears, status recomputes — here to `idle`, since the
// fixture never set a `working` pre-ask status); right-click -> "Remove row"
// is the client-visible equivalent of R-2.5 "SessionEnd -> row removed".
// Every one of {working, attention, recovery, idle, end} is exercised.
test('3-session lifecycle: working -> attention -> recovery -> idle -> end', async ({ page }) => {
  await gotoPopup(page, 'default');

  // --- initial fleet: attention, 2x working, idle, dead -------------------
  await expect(row(page, 'quarterdeck').locator('.qd-row-dot')).toHaveAttribute('data-status', 'attention');
  await expect(row(page, 'dream-book-web').locator('.qd-row-dot')).toHaveAttribute('data-status', 'working');
  await expect(row(page, 'dating-coach').locator('.qd-row-dot')).toHaveAttribute('data-status', 'working');
  await expect(row(page, 'shitty-apps-back').locator('.qd-row-dot')).toHaveAttribute('data-status', 'idle');
  await expect(row(page, 'legacy-tool').locator('.qd-row-dot')).toHaveAttribute('data-status', 'dead');

  // R-7.3 sort order: attention -> working -> idle -> dead.
  const projectsInOrder = await page.locator('.qd-row-project').allTextContents();
  expect(projectsInOrder).toEqual(['quarterdeck', 'dream-book-web', 'dating-coach', 'shitty-apps-back', 'legacy-tool']);

  await expect(page.locator('#qd-footer')).toHaveText('1 needs you · 2 working · 1 idle · 1 dead');
  expect(await segmentBasis(watchlineSegment(page, 'attention'))).toBe(20);
  expect(await segmentBasis(watchlineSegment(page, 'working'))).toBe(40);
  expect(await segmentBasis(watchlineSegment(page, 'idle'))).toBe(20);
  expect(await segmentBasis(watchlineSegment(page, 'dead'))).toBe(20);

  // R-8.3 "also mirrored as rows-with-input in the main popup": 2 pending
  // asks for s1/quarterdeck.
  await expect(page.locator('.qd-ask-row')).toHaveCount(2);

  // --- recovery: clear both pending asks for s1 ----------------------------
  await page
    .locator('.qd-ask-row', { hasText: 'CSS grid columns or flex-basis percentages' })
    .getByRole('button', { name: 'CSS grid columns', exact: true })
    .click();
  await expect(page.locator('.qd-ask-row')).toHaveCount(1);
  // Still attention: one ask is still pending for the session (R-2.4).
  await expect(row(page, 'quarterdeck').locator('.qd-row-dot')).toHaveAttribute('data-status', 'attention');

  const lastAskInput = page.locator('.qd-ask-row').getByPlaceholder('Type an answer…');
  await lastAskInput.fill('Link straight to the docs');
  await lastAskInput.press('Enter');
  await expect(page.locator('.qd-ask-row')).toHaveCount(0);

  // Recovery landed on idle (R-2.4 recompute; no `working` pre-ask status was
  // recorded in this fixture).
  await expect(row(page, 'quarterdeck').locator('.qd-row-dot')).toHaveAttribute('data-status', 'idle');
  await expect(page.locator('#qd-footer')).toHaveText('2 working · 2 idle · 1 dead');
  expect(await segmentBasis(watchlineSegment(page, 'attention'))).toBe(0);
  expect(await segmentBasis(watchlineSegment(page, 'idle'))).toBe(40);

  // --- end: right-click -> Remove row --------------------------------------
  await row(page, 'quarterdeck').click({ button: 'right' });
  await page.getByRole('button', { name: 'Remove row' }).click();

  await expect(row(page, 'quarterdeck')).toHaveCount(0);
  await expect(page.locator('.qd-row')).toHaveCount(4);
  await expect(page.locator('#qd-footer')).toHaveText('2 working · 1 idle · 1 dead');
});

test('right-click context menu offers copy session id and remove row', async ({ page }) => {
  await gotoPopup(page, 'default');
  await row(page, 'dream-book-web').click({ button: 'right' });
  await expect(page.locator('.qd-ctx-menu')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Copy session id' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Remove row' })).toBeVisible();

  // Clicking elsewhere dismisses the menu without side effects.
  await page.locator('.qd-header').click();
  await expect(page.locator('.qd-ctx-menu')).toHaveCount(0);
  await expect(row(page, 'dream-book-web')).toHaveCount(1);
});
