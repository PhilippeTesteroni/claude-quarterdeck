import { expect, test } from '@playwright/test';
import { gotoPopup, row } from '../helpers/popup';

// SPEC §23 (token usage & context health), UI half. The Rust engine owns the
// incremental transcript reader + aggregation (unit-tested in
// `crates/deck-core/src/usage.rs`); against the mocked IPC this proves the UI
// RENDERS whatever the shell computed: the `ctx {n}% · {spend}` second line
// (R-23.4), amber/red context-health coloring + the "nearly full" tooltip, the
// "≥" lower-bound spend (R-23.1), and the `showTokenStats` toggle hiding all of
// it (R-23.6). §37 retired the `⛭ N · {spend}` subagent chip for a plain glyph.
test.describe('token stats (§23)', () => {
  test('row shows the ctx% · spend second line (R-23.4)', async ({ page }) => {
    await gotoPopup(page, 'token-stats');
    const usage = row(page, 'quarterdeck').locator('.qd-row-usage');
    await expect(usage).toBeVisible();
    await expect(usage).toContainText('ctx 62%');
    await expect(usage).toContainText('1.4M');
    // 62% is below the amber threshold → plain muted, no warn/crit class.
    await expect(row(page, 'quarterdeck').locator('.qd-row-ctx')).not.toHaveClass(/warn|crit/);
  });

  test('multi-agent glyph carries no count or spend text (§37)', async ({ page }) => {
    await gotoPopup(page, 'token-stats');
    const badge = row(page, 'quarterdeck').locator('.qd-row-subagents');
    await expect(badge).toHaveText('⛭');
    // §37: the old `⛭ N · {spend}` chip is gone — no number/token text at all.
    await expect(badge).not.toContainText(/\d/);
  });

  test('context ≥90% is red with a nearly-full tooltip (R-23.4)', async ({ page }) => {
    await gotoPopup(page, 'token-stats');
    const near = row(page, 'dream-book-web');
    await expect(near.locator('.qd-row-ctx')).toHaveClass(/crit/);
    await expect(near.locator('.qd-row-ctx')).toContainText('ctx 93%');
    const tooltip = await near.getAttribute('title');
    expect(tooltip).toContain('context nearly full');
  });

  test('context ≥75% is amber and an approximate spend shows ≥ (R-23.1/R-23.4)', async ({ page }) => {
    await gotoPopup(page, 'token-stats');
    const amber = row(page, 'dating-coach');
    await expect(amber.locator('.qd-row-ctx')).toHaveClass(/warn/);
    await expect(amber.locator('.qd-row-ctx')).not.toHaveClass(/crit/);
    // spendApprox → "≥120k".
    await expect(amber.locator('.qd-row-usage')).toContainText('≥120k');
  });

  test('showTokenStats off hides every usage line (R-23.6)', async ({ page }) => {
    await gotoPopup(page, 'token-stats-off');
    // The row still renders (with its ⛭ glyph), but no usage second line.
    await expect(row(page, 'quarterdeck')).toBeVisible();
    await expect(page.locator('.qd-row-usage')).toHaveCount(0);
    // The multi-agent glyph is unaffected by the token toggle — still just the icon.
    await expect(row(page, 'quarterdeck').locator('.qd-row-subagents')).toHaveText('⛭');
  });

  test('toggling token stats off in settings hides the usage line live (R-23.6)', async ({ page }) => {
    await gotoPopup(page, 'token-stats');
    await expect(page.locator('.qd-row-usage').first()).toBeVisible();
    // Open settings, flip the "Show token usage on rows" switch off.
    await page.locator('#qd-gear').click();
    const toggle = page
      .locator('.qd-toggle-row', { has: page.getByText('Show token usage on rows') })
      .locator('.qd-toggle');
    await toggle.click();
    // Back out of settings; the usage lines are gone.
    await page.keyboard.press('Escape');
    await expect(page.locator('.qd-row-usage')).toHaveCount(0);
  });
});
