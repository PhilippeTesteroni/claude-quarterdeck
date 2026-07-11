import { expect, test } from '@playwright/test';
import { gotoPopup, row, watchlineSegment } from '../helpers/popup';

// SPEC §43: a session whose parent turn ended (Stop → hook idle) while
// background subagents/workflows are still running renders BLUE (`waiting`),
// between yellow working and green idle. The engine (Rust) owns the status
// derivation and is unit-tested in `crates/deck-core`; against the mocked IPC
// this proves the UI RENDERS the blue status the shell computed — the row dot,
// the watch-line segment, the lamp worst-of, and the footer copy.
test.describe('waiting-workflow scenario (§43)', () => {
  test('a waiting row shows the blue status dot and keeps its ⛭ subagent badge', async ({ page }) => {
    await gotoPopup(page, 'waiting-workflow');

    const waitingRow = row(page, 'quarterdeck');
    await expect(waitingRow.locator('.qd-row-dot')).toHaveAttribute('data-status', 'waiting');
    // The blue dot uses the §43 --status-blue token (theme-agnostic: #58a6ff
    // dark / #0969da light) — assert it resolves to that token, not green/yellow.
    const [dotColor, blueToken, greenToken] = await waitingRow
      .locator('.qd-row-dot')
      .evaluate((el) => {
        const cs = getComputedStyle(el);
        const probe = (name: string) => {
          const s = document.createElement('span');
          s.style.color = `var(${name})`;
          document.body.append(s);
          const c = getComputedStyle(s).color;
          s.remove();
          return c;
        };
        return [cs.backgroundColor, probe('--status-blue'), probe('--status-green')];
      });
    expect(dotColor).toBe(blueToken);
    expect(dotColor).not.toBe(greenToken);

    // The multi-agent indicator survives the parent Stop (the §43 bug fix): the
    // badge is still shown while background subagents run.
    await expect(waitingRow.locator('.qd-row-subagents')).toBeVisible();

    // The plain idle row alongside is green, not blue.
    await expect(row(page, 'dream-book-web').locator('.qd-row-dot')).toHaveAttribute(
      'data-status',
      'idle',
    );
  });

  test('the watch line carries a blue waiting segment and the footer names it', async ({ page }) => {
    await gotoPopup(page, 'waiting-workflow');

    // A non-zero blue segment sits in the watch line.
    const seg = watchlineSegment(page, 'waiting');
    await expect(seg).toHaveCount(1);
    const basis = await seg.evaluate((el) => (el as HTMLElement).style.flexBasis);
    expect(parseFloat(basis)).toBeGreaterThan(0);

    // R-7.3 footer copy lists the waiting group between working and idle.
    await expect(page.locator('#qd-footer')).toContainText('1 waiting');
  });

  test('§41: a waiting agent gets a blue lamp-pie wedge', async ({ page }) => {
    // Pin + collapse the waiting-workflow fleet (s1 waiting, s2 idle) → the pie
    // carries a blue `waiting` wedge alongside the green `idle` one.
    await page.emulateMedia({ colorScheme: 'dark' });
    await gotoPopup(page, 'waiting-workflow');
    await page.locator('#qd-pin').click();
    await page.locator('#qd-collapse').click();

    const wedges = page.locator('#qd-lamp-pie .qd-lamp-wedge');
    await expect(wedges).toHaveCount(2);
    const waiting = page.locator('#qd-lamp-pie .qd-lamp-wedge[data-status="waiting"]');
    await expect(waiting).toHaveCount(1);
    // §43 blue token (#58a6ff dark default in the Playwright color scheme).
    const fill = await waiting.evaluate((el) => getComputedStyle(el).fill);
    expect(fill).toBe('rgb(88, 166, 255)');
    await expect(page.locator('#qd-lamp')).toHaveAttribute('title', /waiting/);
  });
});
