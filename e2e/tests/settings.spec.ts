import { expect, test } from '@playwright/test';
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

    // Defaults: notifyIdle on, notifyAttention on, notifyReminder off,
    // launchAtLogin off (`ui/src/tauri-mock.ts` defaultSettings()).
    await expect(toggle('Notify when a session finishes')).toHaveAttribute('aria-checked', 'true');
    await expect(toggle('Notify when a session needs you')).toHaveAttribute('aria-checked', 'true');
    await expect(toggle('Remind me if a session is still waiting')).toHaveAttribute('aria-checked', 'false');
    await expect(toggle('Launch Quarterdeck at login')).toHaveAttribute('aria-checked', 'false');

    await toggle('Remind me if a session is still waiting').click();
    await expect(toggle('Remind me if a session is still waiting')).toHaveAttribute('aria-checked', 'true');

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
});
