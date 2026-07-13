import { expect, test, type Page } from '@playwright/test';
import { gotoPopup } from '../helpers/popup';

// SPEC R-7.4 settings pane (notification toggles, autostart, hooks
// install/repair/uninstall, agent-questions toggle, data dir + version) and
// the R-4.1/R-4.2 install/uninstall round trip as seen from the UI.
test.describe('settings pane', () => {
  test('opens via the gear and toggles notification/autostart switches', async ({ page }) => {
    await gotoPopup(page, 'default');

    await expect(page.locator('#qd-settings')).not.toHaveClass(/open/);
    await page.locator('#qd-gear').click();
    await expect(page.locator('#qd-settings')).toHaveClass(/open/);
    await expect(page.locator('.qd-settings-title')).toHaveText('Settings');

    const toggle = (label: string) =>
      page.locator('.qd-toggle-row', { hasText: label }).locator('.qd-toggle');

    // Defaults: notifyIdle on, notifyAttention on, launchAtLogin off
    // (`ui/src/tauri-mock.ts` defaultSettings()). §47: the "still waiting"
    // reminder toggle is retired and no longer rendered.
    await expect(toggle('Notify when a session finishes')).toHaveAttribute('aria-checked', 'true');
    await expect(toggle('Notify when a session needs you')).toHaveAttribute('aria-checked', 'true');
    await expect(page.locator('.qd-toggle-row', { hasText: 'still waiting' })).toHaveCount(0);
    await expect(toggle('Launch Quarterdeck at login')).toHaveAttribute('aria-checked', 'false');

    await toggle('Launch Quarterdeck at login').click();
    await expect(toggle('Launch Quarterdeck at login')).toHaveAttribute('aria-checked', 'true');
    await toggle('Launch Quarterdeck at login').click();
    await expect(toggle('Launch Quarterdeck at login')).toHaveAttribute('aria-checked', 'false');

    // Back button closes the pane (R-7.4).
    await page.locator('.qd-back').click();
    await expect(page.locator('#qd-settings')).not.toHaveClass(/open/);
  });

  test('Escape closes the settings pane', async ({ page }) => {
    await gotoPopup(page, 'default');
    await page.locator('#qd-gear').click();
    await expect(page.locator('#qd-settings')).toHaveClass(/open/);
    await page.keyboard.press('Escape');
    await expect(page.locator('#qd-settings')).not.toHaveClass(/open/);
  });

  test('agent-questions toggle flips label and button copy (R-8.6)', async ({ page }) => {
    await gotoPopup(page, 'default');
    await page.locator('#qd-gear').click();

    // `default` scenario ships mcpEnabled: true.
    await expect(page.getByText('Agent questions are enabled')).toBeVisible();
    await page.getByRole('button', { name: 'Disable agent questions' }).click();
    await expect(page.getByText('Agent questions are disabled')).toBeVisible();
    await expect(page.getByRole('button', { name: 'Enable agent questions' })).toBeVisible();
  });

  test('repair hooks shows a busy state then settles (R-4.1)', async ({ page }) => {
    await gotoPopup(page, 'default');
    await page.locator('#qd-gear').click();

    await expect(page.getByText('Hooks are installed')).toBeVisible();
    const repair = page.getByRole('button', { name: 'Repair hooks' });
    await repair.click();
    await expect(page.getByRole('button', { name: 'Installing…' })).toBeDisabled();
    await expect(page.getByRole('button', { name: 'Repair hooks' })).toBeVisible();
  });

  test('uninstall then reinstall hooks round-trips the installed state (R-4.2)', async ({ page }) => {
    await gotoPopup(page, 'default');
    await page.locator('#qd-gear').click();
    // Scoped to the settings pane + `exact` throughout: "Install hooks" is a
    // case-insensitive substring of "Uninstall hooks", and the popup content
    // behind the slide-in still has its own (now-obscured) banner button.
    const pane = page.locator('#qd-settings');

    await pane.getByRole('button', { name: 'Uninstall hooks', exact: true }).click();
    await expect(pane.getByText('Hooks are not installed')).toBeVisible();
    await expect(page.locator('#qd-gear')).toHaveClass(/has-issue/);

    await pane.getByRole('button', { name: 'Install hooks', exact: true }).click();
    await expect(pane.getByText('Hooks are installed')).toBeVisible();
    await expect(page.locator('#qd-gear')).not.toHaveClass(/has-issue/);
  });

  test('shows the data directory and version in the About section', async ({ page }) => {
    await gotoPopup(page, 'default');
    await page.locator('#qd-gear').click();
    await expect(page.locator('.qd-settings-meta', { hasText: 'Data directory' })).toContainText('quarterdeck');
    await expect(page.locator('.qd-settings-meta', { hasText: 'Version' })).toContainText('0.1.0');
  });

  test('a failed install surfaces the exact error copy (R-7.6)', async ({ page }) => {
    await gotoPopup(page, 'error');
    // hooksInstalled is false and there are no sessions -> both the banner
    // and the empty state render (see popup.ts renderContent).
    await page.getByRole('button', { name: 'Install hooks' }).click();
    await expect(page.locator('.qd-banner-error')).toHaveText(
      'Could not read ~/.claude/settings.json: unexpected token at line 12. Fix the JSON and try again.',
    );
  });
});

// SPEC R-31.2 "fixed settings height": opening settings sizes the window to a
// stable `header + 5 rows + footer` height instead of latching whatever the
// list happened to be, animated (rAF tween driving `resize_popup`) and snapped
// under reduced motion. There's no real OS window in mock/browser mode, but the
// mock records every reported content height + the resize call count, so the
// pane height and snap-vs-tween behavior are both observable.
test.describe('settings fixed 5-row height (R-31.2)', () => {
  type Hooks = {
    lastResizeContentHeight(): number | null;
    resizePopupCallCount(): number;
    removeAllSessions(): void;
    keepFirstSessions(n: number): void;
  };
  const lastResize = (page: Page): Promise<number | null> =>
    page.evaluate(() => (window as unknown as { __qdMock: Hooks }).__qdMock.lastResizeContentHeight());
  const resizeCalls = (page: Page): Promise<number> =>
    page.evaluate(() => (window as unknown as { __qdMock: Hooks }).__qdMock.resizePopupCallCount());
  const keepSessions = (page: Page, n: number): Promise<void> =>
    page.evaluate((k) => {
      const m = (window as unknown as { __qdMock: Hooks }).__qdMock;
      if (k === 0) m.removeAllSessions();
      else m.keepFirstSessions(k);
    }, n);

  test('a fixed 5-row pane at 0/2/8 sessions, snapped in a single report under reduced motion', async ({ page }) => {
      await page.emulateMedia({ reducedMotion: 'reduce' });

      // --- 0 sessions (empty state, no row to measure) ---
      await gotoPopup(page, 'empty');
      await expect(page.locator('.qd-empty-title')).toBeVisible();
      const emptyAuto = await lastResize(page);
      const before0 = await resizeCalls(page);

      await page.locator('#qd-gear').click();
      await expect(page.locator('#qd-settings')).toHaveClass(/open/);
      const h0 = await lastResize(page);

      // Reduced motion => exactly one resize report (a snap, no tween frames).
      expect((await resizeCalls(page)) - before0).toBe(1);
      // The pane expands to 5 rows, well past the compact empty auto-height.
      expect(h0 ?? 0).toBeGreaterThan(emptyAuto ?? 0);

      // --- 2 sessions ---
      await gotoPopup(page, 'many-sessions');
      await keepSessions(page, 2);
      await expect(page.locator('.qd-row')).toHaveCount(2);
      await page.locator('#qd-gear').click();
      await expect(page.locator('#qd-settings')).toHaveClass(/open/);
      const h2 = await lastResize(page);

      // --- 8 sessions ---
      await gotoPopup(page, 'many-sessions');
      await keepSessions(page, 8);
      await expect(page.locator('.qd-row')).toHaveCount(8);
      await page.locator('#qd-gear').click();
      await expect(page.locator('#qd-settings')).toHaveClass(/open/);
      const h8 = await lastResize(page);

      // The fixed 5-row height is identical regardless of how many sessions sit
      // behind the overlay — the whole point of R-31.2.
      expect(h2).toBe(h0);
      expect(h8).toBe(h0);
  });

  test('animates the open/close resize across frames, then restores the list height', async ({ page }) => {
    // Default project = motion allowed (no reducedMotion), so the resize tweens.
    await gotoPopup(page, 'many-sessions');
    await keepSessions(page, 8);
    await expect(page.locator('.qd-row')).toHaveCount(8);
    const listAuto = await lastResize(page);

    const beforeOpen = await resizeCalls(page);
    await page.locator('#qd-gear').click();
    await expect(page.locator('#qd-settings')).toHaveClass(/open/);
    // The tween drives `resize_popup` once per animation frame => several reports.
    await expect.poll(async () => (await resizeCalls(page)) - beforeOpen).toBeGreaterThan(1);
    // It settles at the fixed 5-row height, shorter than the 8-row list.
    await expect.poll(() => lastResize(page)).toBeLessThan(listAuto ?? Number.POSITIVE_INFINITY);

    // Closing restores the list auto-height (content behind the overlay is unchanged).
    await page.locator('.qd-back').click();
    await expect(page.locator('#qd-settings')).not.toHaveClass(/open/);
    await expect.poll(() => lastResize(page)).toBe(listAuto);
  });
});

test.describe('onboarding card (R-10.2)', () => {
  test('walks install hooks -> launch at login -> agent questions -> continue', async ({ page }) => {
    await gotoPopup(page, 'onboarding');

    await expect(page.locator('.qd-onboarding-title')).toHaveText('Welcome aboard');
    await expect(page.locator('.qd-onboarding-body')).toContainText('~/.claude/settings.json');

    const installStep = page.locator('.qd-onboarding-step', { hasText: 'Install hooks' });
    await installStep.getByRole('button', { name: 'Install hooks' }).click();
    await expect(installStep.getByRole('button', { name: 'Installing…' })).toBeVisible();
    await expect(installStep.getByRole('button', { name: 'Installed' })).toBeDisabled();

    const loginStep = page.locator('.qd-onboarding-step', { hasText: 'Launch Quarterdeck at login?' });
    await loginStep.getByRole('button', { name: 'Yes', exact: true }).click();
    await expect(loginStep.getByRole('button', { name: 'Yes', exact: true })).toHaveClass(/qd-btn-primary/);

    const mcpStep = page.locator('.qd-onboarding-step', { hasText: 'Let agents ask you questions' });
    await mcpStep.getByRole('button', { name: 'Enable agent questions' }).click();
    await expect(mcpStep.getByRole('button', { name: 'Enabled' })).toBeDisabled();

    await page.getByRole('button', { name: 'Continue' }).click();
    await expect(page.locator('.qd-onboarding')).toHaveCount(0);
    // No sessions in the `onboarding` fixture -> lands on the empty state.
    await expect(page.locator('.qd-empty-title')).toBeVisible();
  });

  test('does not stack the onboarding card above a populated session list (R-10.2)', async ({ page }) => {
    // onboardingDone is still false for this data dir, but hooks already work and
    // a session is flowing. The card must NOT render over the live list (that
    // contradicts its own "Install hooks so sessions show up here" copy); the
    // list wins.
    await gotoPopup(page, 'onboarding-with-sessions');
    await expect(page.locator('.qd-onboarding')).toHaveCount(0);
    await expect(page.locator('.qd-row-project')).toHaveText('quarterdeck');
    await expect(page.locator('#qd-footer')).not.toHaveCSS('display', 'none');
  });
});
