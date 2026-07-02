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
  tauriListen<StateSnapshot>(STATE_EVENT, (event) => cb(event.payload)).then((u) => {
    if (cancelled) {
      u();
    } else {
      unlisten = u;
    }
  });
  // Prime the view immediately rather than waiting for the next push.
  void tauriInvoke<StateSnapshot>('get_state').then(cb).catch(() => undefined);
  return () => {
    cancelled = true;
    unlisten?.();
  };
}
