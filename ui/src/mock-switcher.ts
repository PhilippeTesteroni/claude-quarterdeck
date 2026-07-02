/**
 * Dev-only affordance, mounted only when `ipc-client.ts` picked the mock
 * backend: a tiny scenario picker so a human (or a screenshot script that
 * prefers query params instead) can jump between fixture states without
 * hand-editing the URL. Never mounted inside a real Tauri build.
 */

import { h } from './dom';

const SCENARIOS = ['default', 'empty', 'nohooks', 'onboarding', 'cyrillic', 'ask-unknown', 'error'];

export function installMockScenarioSwitcher(): void {
  const params = new URLSearchParams(location.search);
  const current = params.get('scenario') ?? 'default';

  const select = h(
    'select',
    {
      onchange: (ev: Event) => {
        const value = (ev.target as HTMLSelectElement).value;
        const next = new URLSearchParams(location.search);
        next.set('scenario', value);
        location.search = next.toString();
      },
    },
    SCENARIOS.map((name) => h('option', { value: name, selected: name === current }, [name])),
  );

  const wrap = h('div', { className: 'qd-mock-switcher' }, ['mock: ', select]);
  document.body.append(wrap);
}
