import { expect, test } from '@playwright/test';
import { gotoAsk, gotoPopup } from '../helpers/popup';

// SPEC §29 (R-29.4): the ask window renders a multi-question / multi-select
// `ask_user` form — radio blocks (single-select), checkbox blocks (multiSelect),
// optional per-question free text, one Submit that returns a `{answers:[...]}`
// document under kind:"form". R-29.5: the popup mirror shows a compact
// "N questions — Answer in window" summary instead of inline options.
test.describe('multi-question ask form (§29)', () => {
  test('renders header + radio/checkbox blocks and a Submit', async ({ page }) => {
    await gotoAsk(page, 'ask-form');

    // Two question blocks, each with its header + question.
    await expect(page.locator('.qd-ask-form-q')).toHaveCount(2);
    await expect(page.locator('.qd-ask-q-header').nth(0)).toHaveText('Environment');
    await expect(page.locator('.qd-ask-q-header').nth(1)).toHaveText('Flags');
    await expect(page.locator('.qd-ask-form-question').nth(0)).toHaveText('Which environment?');

    // Q1 = radio (single-select), Q2 = checkbox (multiSelect).
    await expect(page.getByRole('radio')).toHaveCount(2);
    await expect(page.getByRole('checkbox')).toHaveCount(3);

    // Submit + the queued single-question ask behind it.
    await expect(page.getByRole('button', { name: 'Submit' })).toBeVisible();
    await expect(page.locator('#qd-ask-badge')).toHaveText('1 more waiting');
  });

  test('radio replaces the prior choice; checkbox accumulates', async ({ page }) => {
    await gotoAsk(page, 'ask-form');

    const prod = page.getByRole('radio', { name: 'prod' });
    const staging = page.getByRole('radio', { name: 'staging' });
    await prod.click();
    await expect(prod).toHaveClass(/selected/);
    // Selecting another radio deselects the first (exactly one).
    await staging.click();
    await expect(staging).toHaveClass(/selected/);
    await expect(prod).not.toHaveClass(/selected/);

    // Checkboxes accumulate.
    const fast = page.getByRole('checkbox', { name: '--fast' });
    const safe = page.getByRole('checkbox', { name: '--safe' });
    await fast.click();
    await safe.click();
    await expect(fast).toHaveClass(/selected/);
    await expect(safe).toHaveClass(/selected/);
  });

  test('Submit validates a required single-select, then sends kind:"form" with the answers doc', async ({ page }) => {
    await gotoAsk(page, 'ask-form');

    // Submitting with the required radio unanswered surfaces the error and does
    // NOT send anything.
    await expect(page.locator('.qd-ask-form-error')).toBeHidden();
    await page.getByRole('button', { name: 'Submit' }).click();
    await expect(page.locator('.qd-ask-form-error')).toBeVisible();
    const notYet = await page.evaluate(
      () => (window as unknown as { __qdMock: { lastAnswerAskFull(): unknown } }).__qdMock.lastAnswerAskFull(),
    );
    expect(notYet).toBeNull();

    // Answer the radio + two checkboxes + a free-text on Q2, then Submit.
    await page.getByRole('radio', { name: 'staging' }).click();
    await page.getByRole('checkbox', { name: '--fast' }).click();
    await page.getByRole('checkbox', { name: '--safe' }).click();
    await page.locator('.qd-ask-form-q').nth(1).locator('.qd-ask-form-text').fill('and -j8');
    await page.getByRole('button', { name: 'Submit' }).click();

    const full = await page.evaluate(
      () =>
        (window as unknown as { __qdMock: { lastAnswerAskFull(): { askId: string; answer: string; kind: string } | null } }).__qdMock.lastAnswerAskFull(),
    );
    expect(full?.askId).toBe('a1');
    expect(full?.kind).toBe('form');
    const parsed = JSON.parse(full!.answer) as {
      answers: { header?: string; question: string; selected: string[]; text?: string }[];
    };
    expect(parsed.answers).toHaveLength(2);
    expect(parsed.answers[0]).toEqual({
      header: 'Environment',
      question: 'Which environment?',
      selected: ['staging'],
    });
    expect(parsed.answers[1]).toEqual({
      header: 'Flags',
      question: 'Extra flags?',
      selected: ['--fast', '--safe'],
      text: 'and -j8',
    });
  });

  test('an unrelated state push preserves in-progress selections (R-29.4)', async ({ page }) => {
    await gotoAsk(page, 'ask-form');
    await page.getByRole('radio', { name: 'staging' }).click();
    await page.getByRole('checkbox', { name: '--fast' }).click();

    // Drive a deck://state re-push from an unrelated ask (dismiss the queued a2).
    await page.evaluate(() =>
      (window as unknown as { __qdMock: { answerAsk: (id: string, a: string, k: string) => void } }).__qdMock.answerAsk(
        'a2',
        '',
        'dismissed',
      ),
    );
    await expect(page.locator('#qd-ask-badge')).toBeHidden();

    // Selections survived the rebuild.
    await expect(page.getByRole('radio', { name: 'staging' })).toHaveClass(/selected/);
    await expect(page.getByRole('checkbox', { name: '--fast' })).toHaveClass(/selected/);
  });

  // SPEC §46 dual-answer: the multi-question form also carries the secondary
  // "In terminal" escape; clicking it resolves the ask with kind:"terminal"
  // without validating/submitting the form.
  test('form "In terminal" resolves the ask with kind:"terminal" (§46)', async ({ page }) => {
    await gotoAsk(page, 'ask-form');
    await expect(page.getByRole('button', { name: 'In terminal' })).toBeVisible();

    // No form validation runs — it hands off to the terminal even with the
    // required radio unanswered.
    await page.getByRole('button', { name: 'In terminal' }).click();
    const last = await page.evaluate(
      () => (window as unknown as { __qdMock: { lastAnswerAsk(): { askId: string; kind: string } | null } }).__qdMock.lastAnswerAsk(),
    );
    expect(last).toEqual({ askId: 'a1', kind: 'terminal' });
    await expect(page.locator('.qd-ask-form-error')).toHaveCount(0);
  });

  test('popup mirror shows "N questions — Answer in window", no inline input (R-29.5)', async ({ page }) => {
    await gotoPopup(page, 'ask-form');

    const formRow = page.locator('.qd-ask-row-form');
    await expect(formRow).toHaveCount(1);
    await expect(formRow).toContainText('2 questions');
    // A form mirror row has no inline free-text answer field.
    await expect(formRow.locator('.qd-ask-row-input')).toHaveCount(0);

    // Clicking "Answer in window" re-surfaces the ask window.
    await formRow.getByRole('button', { name: 'Answer in window' }).click();
    const showCalls = await page.evaluate(
      () => (window as unknown as { __qdMock: { showAskWindowCallCount(): number } }).__qdMock.showAskWindowCallCount(),
    );
    expect(showCalls).toBe(1);
  });
});
