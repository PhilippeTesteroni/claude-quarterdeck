import { expect, test } from '@playwright/test';
import { gotoAsk, gotoPopup } from '../helpers/popup';

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

  // SPEC R-8: `push_state()` broadcasts to the ask window on ANY session's state
  // change, not just this ask's. A partially-typed free-text answer (and focus)
  // must survive such an unrelated re-render — it's the only interactive surface
  // the ask channel provides, and silent loss is undiscoverable.
  test('an unrelated state change preserves an in-progress free-text answer (R-8)', async ({ page }) => {
    await gotoAsk(page, 'default');

    // a1 is primary (options + free text); a2 is queued behind it ("1 more waiting").
    const input = page.locator('.qd-ask-freeform input');
    await input.click();
    await input.fill('Segment proportionally by count');
    await expect(input).toBeFocused();
    await expect(page.locator('#qd-ask-badge')).toHaveText('1 more waiting');

    // Drive a deck://state push from an UNRELATED ask (dismiss the queued a2
    // headlessly) — the same global broadcast a sibling session's change causes.
    // a1 stays primary, so its free-text field is rebuilt from scratch.
    await page.evaluate(() =>
      (window as unknown as { __qdMock: { answerAsk: (id: string, a: string, k: string) => void } }).__qdMock.answerAsk(
        'a2',
        '',
        'dismissed',
      ),
    );

    // The badge clears — proof the re-render actually ran.
    await expect(page.locator('#qd-ask-badge')).toBeHidden();
    // ...and the typed answer + focus survived it.
    await expect(input).toHaveValue('Segment proportionally by count');
    await expect(input).toBeFocused();
  });

  // SPEC R-8.7: an ask recovered from disk after a restart can never be answered
  // (its MCP connection is gone). It renders as expired with only a Dismiss
  // action — "never answered into the void": no options, no free-text field,
  // and no live countdown.
  test('orphaned ask renders expired with only Dismiss (R-8.7)', async ({ page }) => {
    await gotoAsk(page, 'ask-orphaned');

    await expect(page.locator('.qd-ask-question')).toContainText('Migrate the settings schema now');
    // Expired marker instead of a live countdown.
    await expect(page.locator('.qd-ask-countdown')).toHaveText('expired');
    await expect(page.locator('.qd-ask-empty')).toContainText('can no longer be answered');
    // No answer surfaces: no option buttons, no free-text input.
    await expect(page.locator('.qd-ask-option')).toHaveCount(0);
    await expect(page.locator('.qd-ask-freeform')).toHaveCount(0);
    // The only action is Dismiss.
    const actionButtons = page.locator('.qd-ask-actions button');
    await expect(actionButtons).toHaveCount(1);
    await expect(actionButtons).toHaveText('Dismiss');

    // Dismissing clears it.
    await actionButtons.click();
    await expect(page.locator('.qd-ask-empty')).toHaveText('No pending questions.');
  });

  // The same orphaned ask, mirrored as a row in the main popup (R-8.3 "also
  // mirrored as rows-with-input in the main popup", R-8.7 expired rendering).
  test('orphaned ask is mirrored as an expired row in the popup (R-8.7)', async ({ page }) => {
    await gotoPopup(page, 'ask-orphaned');

    const expiredRow = page.locator('.qd-ask-row-expired');
    await expect(expiredRow).toHaveCount(1);
    await expect(expiredRow).toContainText('expired');
    await expect(expiredRow.locator('.qd-ask-row-expired-note')).toContainText(
      'Expired while Quarterdeck was closed.',
    );
    // No answer input in an orphaned mirror row; Dismiss is the only action.
    await expect(expiredRow.locator('.qd-ask-row-input')).toHaveCount(0);
    await expect(expiredRow.locator('.qd-ask-row-dismiss')).toHaveText('Dismiss');
  });

  // SPEC R-18.2: the ask window's own title-bar area is draggable, the same
  // mechanism as the popup header (R-14.1).
  test('the ask window header is a drag region (R-18.2)', async ({ page }) => {
    await gotoAsk(page, 'default');
    await expect(page.locator('.qd-ask .qd-header')).toHaveAttribute('data-tauri-drag-region', 'deep');
  });

  // SPEC R-18.1: the X (top-right) hides the window WITHOUT dismissing any
  // pending ask — distinct from per-ask "Dismiss", which resolves it. There's
  // no second real window to observe hiding in mock/browser mode, so this
  // asserts the actual required behavior: the ask list is untouched.
  test('close-X hides without dismissing pending asks (R-18.1)', async ({ page }) => {
    await gotoAsk(page, 'default');
    await expect(page.locator('.qd-ask-question')).toContainText('Which approach for the watch line segments');
    await expect(page.locator('#qd-ask-badge')).toHaveText('1 more waiting');

    await page.locator('#qd-ask-close').click();

    // Unlike Dismiss, the primary ask (and the queued one behind it) is
    // completely unaffected.
    await expect(page.locator('.qd-ask-question')).toContainText('Which approach for the watch line segments');
    await expect(page.locator('#qd-ask-badge')).toHaveText('1 more waiting');
    await expect(page.locator('.qd-ask-empty')).toHaveCount(0);

    // The window-hide itself fired (SPEC: "closes (hides) the WINDOW").
    const hideCalls = await page.evaluate(
      () => (window as unknown as { __qdMock: { hideCurrentWindowCallCount(): number } }).__qdMock.hideCurrentWindowCallCount(),
    );
    expect(hideCalls).toBe(1);
  });

  // SPEC R-18.1: Esc is ALWAYS the same as the X — it never silently
  // dismisses, whether one ask is pending or several.
  test('Esc hides without dismissing, with one or several pending (R-18.1)', async ({ page }) => {
    await gotoAsk(page, 'default');
    await expect(page.locator('#qd-ask-badge')).toHaveText('1 more waiting');

    await page.keyboard.press('Escape');
    await expect(page.locator('.qd-ask-question')).toContainText('Which approach for the watch line segments');
    await expect(page.locator('#qd-ask-badge')).toHaveText('1 more waiting');

    // Down to exactly one pending ask: Esc still hides, never dismisses.
    await page.getByRole('button', { name: 'Dismiss' }).click();
    await expect(page.locator('#qd-ask-badge')).toBeHidden();
    await expect(page.locator('.qd-ask-question')).toContainText('Should the empty state link');

    await page.keyboard.press('Escape');
    await expect(page.locator('.qd-ask-question')).toContainText('Should the empty state link');
    await expect(page.locator('.qd-ask-empty')).toHaveCount(0);

    const hideCalls = await page.evaluate(
      () => (window as unknown as { __qdMock: { hideCurrentWindowCallCount(): number } }).__qdMock.hideCurrentWindowCallCount(),
    );
    expect(hideCalls).toBe(2);
  });

  // SPEC R-18.1 "(or via popup mirror click)": clicking a mirrored ask row in
  // the popup re-surfaces the ask window.
  test('clicking a popup mirror row re-surfaces the ask window (R-18.1)', async ({ page }) => {
    await gotoPopup(page, 'default');

    // Click the row's question text (not a button/input) for the first ask.
    await page
      .locator('.qd-ask-row', { hasText: 'CSS grid columns or flex-basis percentages' })
      .locator('.qd-ask-row-question')
      .click();

    const showCalls = await page.evaluate(
      () => (window as unknown as { __qdMock: { showAskWindowCallCount(): number } }).__qdMock.showAskWindowCallCount(),
    );
    expect(showCalls).toBe(1);

    // Clicking an option button must NOT also trigger the reopen (it answers
    // instead) — the row click handler ignores interactive descendants.
    await page
      .locator('.qd-ask-row', { hasText: 'CSS grid columns or flex-basis percentages' })
      .getByRole('button', { name: 'CSS grid columns', exact: true })
      .click();
    const showCallsAfterAnswer = await page.evaluate(
      () => (window as unknown as { __qdMock: { showAskWindowCallCount(): number } }).__qdMock.showAskWindowCallCount(),
    );
    expect(showCallsAfterAnswer).toBe(1);
  });
});
