/**
 * Facade over the real Tauri IPC and the mock backend (`tauri-mock.ts`).
 *
 * Selection is automatic: `isTauri()` (from `@tauri-apps/api/core`) is true
 * only when the page is actually hosted inside a Tauri webview, so a plain
 * `npm run ui:dev` + browser tab transparently gets the scripted mock state
 * (SPEC T4 AC: "runs against mocked IPC ... activated by env").
 */

import { invoke as tauriInvoke, isTauri } from '@tauri-apps/api/core';
import { listen as tauriListen } from '@tauri-apps/api/event';
import { getCurrentWindow } from '@tauri-apps/api/window';
import type { Commands, StateSnapshot } from './ipc-contract';
import { STATE_EVENT } from './ipc-contract';
import * as mock from './tauri-mock';

export const usingMock: boolean = !isTauri();

export async function invoke<K extends keyof Commands>(
  cmd: K,
  args: Parameters<Commands[K]>[0],
): Promise<Awaited<ReturnType<Commands[K]>>> {
  if (usingMock) {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    return mock.invoke(cmd, args) as any;
  }
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  return tauriInvoke(cmd, args as any) as any;
}

/** Subscribes to full-state snapshots; returns an unsubscribe function. */
export function onState(cb: (snapshot: StateSnapshot) => void): () => void {
  if (usingMock) {
    return mock.onState(cb);
  }
  let unlisten: (() => void) | undefined;
  let cancelled = false;
  const prime = (): void => {
    void tauriInvoke<StateSnapshot>('get_state').then(cb).catch(() => undefined);
  };
  tauriListen<StateSnapshot>(STATE_EVENT, (event) => cb(event.payload))
    .then((u) => {
      if (cancelled) {
        u();
        return;
      }
      unlisten = u;
      // Re-prime once the subscription is actually attached, to close the
      // startup race where the engine's first `deck://state` push (fired from a
      // background thread after spool replay + discovery) could land before the
      // listener exists — otherwise the popup would keep the empty default
      // snapshot and never show the onboarding card (R-10.2 / R-3.4).
      prime();
    })
    // A rejected `listen` (e.g. an ACL misconfiguration) must not become an
    // unhandled rejection; the primed `get_state` still renders a snapshot.
    .catch(() => undefined);
  // Prime immediately too, so the view isn't blank while `listen` registers.
  prime();
  return () => {
    cancelled = true;
    unlisten?.();
  };
}

/**
 * Hides the window this code is running in (SPEC R-18.1 ask-window close-X /
 * Esc: "closes (hides) the WINDOW without dismissing pending asks"). A pure
 * window operation, not application state, so it goes straight through the
 * Tauri window API rather than a command (mirrors how the popup's own
 * Esc-hide is wired in `src-tauri/src/windows.rs`). No-op in mock/browser
 * mode aside from recording the call for Playwright specs to assert against.
 */
export function hideCurrentWindow(): void {
  if (usingMock) {
    mock.hideCurrentWindowMock();
    return;
  }
  void getCurrentWindow().hide();
}
