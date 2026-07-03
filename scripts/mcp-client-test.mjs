#!/usr/bin/env node
/**
 * Quarterdeck MCP client smoke test (SPEC §8, T6 AC).
 *
 * Exercises the streamable-HTTP MCP server end to end:
 *   1. a request without a bearer token is rejected with 401
 *   2. `initialize` handshake
 *   3. `notifications/initialized` is acknowledged (202)
 *   4. `tools/list` advertises ask_user + notify_user + update_ask + cancel_ask
 *   5. a blocking `ask_user` round-trip returns {answer, kind, ask_id}
 *   6. `notify_user` returns {delivered, id}
 *   7. a blocking `ask_user` with a progressToken streams >=1
 *      `notifications/progress` before the final result (R-19.3)
 *   8. a dismissed `ask_user` resolves with kind:"dismissed" — not a transport
 *      timeout (R-19.4 regression)
 *
 * Connection details come from the environment when set
 * (QUARTERDECK_MCP_PORT + QUARTERDECK_MCP_TOKEN — used by the Rust test
 * harness), otherwise from <data>/mcp.json (so it can also be run by hand
 * against the live app):
 *
 *   node scripts/mcp-client-test.mjs
 *   QUARTERDECK_MCP_PORT=1234 QUARTERDECK_MCP_TOKEN=abc node scripts/mcp-client-test.mjs
 *
 * Prints one `OK ...` line per check and `ALL CHECKS PASSED` on success;
 * exits non-zero with `FAILED: ...` on the first failure.
 */

import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import process from 'node:process';

function resolveConfig() {
  const envPort = process.env.QUARTERDECK_MCP_PORT;
  const envToken = process.env.QUARTERDECK_MCP_TOKEN;
  if (envPort && envToken) {
    return { port: Number(envPort), token: envToken };
  }
  const dataDir =
    process.env.QUARTERDECK_DATA_DIR ||
    (process.platform === 'win32'
      ? join(process.env.APPDATA || '', 'quarterdeck')
      : process.platform === 'darwin'
        ? join(process.env.HOME || '', 'Library', 'Application Support', 'quarterdeck')
        : join(process.env.HOME || '', '.local', 'share', 'quarterdeck'));
  const cfg = JSON.parse(readFileSync(join(dataDir, 'mcp.json'), 'utf8'));
  return { port: cfg.port, token: cfg.token };
}

const { port, token } = resolveConfig();
const endpoint = `http://127.0.0.1:${port}/mcp`;
let nextId = 1;

async function rpc(method, params, { expectStatus } = {}) {
  const isNotification = method.startsWith('notifications/');
  const message = { jsonrpc: '2.0', method, params };
  if (!isNotification) message.id = nextId++;

  const res = await fetch(endpoint, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      accept: 'application/json, text/event-stream',
      authorization: `Bearer ${token}`,
    },
    body: JSON.stringify(message),
  });

  if (expectStatus !== undefined && res.status !== expectStatus) {
    throw new Error(`${method}: expected HTTP ${expectStatus}, got ${res.status}`);
  }
  if (isNotification) return null;

  const text = await res.text();
  if (res.status !== 200) throw new Error(`${method}: HTTP ${res.status}: ${text}`);
  const json = JSON.parse(text);
  if (json.error) throw new Error(`${method}: RPC error ${json.error.code}: ${json.error.message}`);
  return json.result;
}

function assert(cond, msg) {
  if (!cond) throw new Error(`Assertion failed: ${msg}`);
}

/**
 * Calls `ask_user` with a `progressToken` so the server answers over an SSE
 * stream (R-19.3). Reads the whole stream (it closes right after the final
 * result), parses each `data: {json}` frame, and returns the collected progress
 * notifications plus the final JSON-RPC result.
 */
async function askUserStreaming(args, progressToken) {
  const id = nextId++;
  const res = await fetch(endpoint, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      accept: 'application/json, text/event-stream',
      authorization: `Bearer ${token}`,
    },
    body: JSON.stringify({
      jsonrpc: '2.0',
      id,
      method: 'tools/call',
      params: { name: 'ask_user', arguments: args, _meta: { progressToken } },
    }),
  });
  const ct = res.headers.get('content-type') || '';
  assert(ct.includes('text/event-stream'), `progressToken request should stream SSE (got ${ct})`);
  const text = await res.text();
  const frames = text
    .split('\n\n')
    .map((f) => f.trim())
    .filter((f) => f.startsWith('data:'))
    .map((f) => JSON.parse(f.slice('data:'.length).trim()));
  const progress = frames.filter((f) => f.method === 'notifications/progress');
  const result = frames.filter((f) => f.id === id).pop();
  return { progress, result };
}

async function main() {
  // 1. Auth: a request with no bearer token must be rejected.
  {
    const res = await fetch(endpoint, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 999, method: 'initialize', params: {} }),
    });
    assert(res.status === 401, `unauthenticated request should be 401 (got ${res.status})`);
    console.log('OK  401 without bearer token');
  }

  // 2. initialize.
  const init = await rpc('initialize', {
    protocolVersion: '2025-06-18',
    capabilities: {},
    clientInfo: { name: 'quarterdeck-mcp-client-test', version: '1.0.0' },
  });
  assert(typeof init.protocolVersion === 'string', 'initialize returns protocolVersion');
  assert(init.serverInfo && init.serverInfo.name === 'quarterdeck', 'serverInfo.name is quarterdeck');
  console.log(
    `OK  initialize (protocol ${init.protocolVersion}, server ${init.serverInfo.name} ${init.serverInfo.version})`,
  );

  // 3. initialized notification.
  await rpc('notifications/initialized', {}, { expectStatus: 202 });
  console.log('OK  notifications/initialized (202)');

  // 4. tools/list.
  const list = await rpc('tools/list', {});
  const names = (list.tools || []).map((t) => t.name);
  assert(names.includes('ask_user'), 'tools/list includes ask_user');
  assert(names.includes('notify_user'), 'tools/list includes notify_user');
  assert(names.includes('update_ask'), 'tools/list includes update_ask (R-19.5)');
  assert(names.includes('cancel_ask'), 'tools/list includes cancel_ask (R-19.5)');
  console.log(`OK  tools/list -> [${names.join(', ')}]`);

  // 5. ask_user round-trip (blocks until answered / timeout / dismissed).
  const options = ['Yes', 'No'];
  const call = await rpc('tools/call', {
    name: 'ask_user',
    arguments: {
      question: 'Proceed with deploy?',
      options,
      detail: 'Build 41 passed CI; the nightly is still 6 hours out.',
      context: process.cwd(),
      timeout_seconds: 30,
    },
  });
  assert(Array.isArray(call.content) && call.content.length > 0, 'ask_user returns content');
  const structured = call.structuredContent || JSON.parse(call.content[0].text);
  assert(typeof structured.answer === 'string', 'answer is a string');
  assert(
    ['option', 'text', 'timeout', 'dismissed', 'cancelled'].includes(structured.kind),
    `kind is one of option|text|timeout|dismissed|cancelled (got ${structured.kind})`,
  );
  // R-19.5: the result carries an ask_id for update_ask/cancel_ask.
  assert(typeof structured.ask_id === 'string' && structured.ask_id.length > 0, 'result carries ask_id');
  if (structured.kind === 'option') {
    assert(options.includes(structured.answer), 'option answer is one of the offered options');
  }
  console.log(
    `OK  ask_user round-trip -> {answer: ${JSON.stringify(structured.answer)}, kind: ${structured.kind}, ask_id: ${structured.ask_id}}`,
  );

  // 6. notify_user (fire-and-forget) returns {delivered, id} (R-19.6).
  const notif = await rpc('tools/call', {
    name: 'notify_user',
    arguments: { message: 'mcp-client-test says hi', context: process.cwd() },
  });
  assert(notif.isError !== true, 'notify_user is not an error');
  const notifStructured = notif.structuredContent || JSON.parse(notif.content[0].text);
  assert(notifStructured.delivered === true, 'notify_user returns delivered:true');
  assert(typeof notifStructured.id === 'string' && notifStructured.id.length > 0, 'notify_user returns an id');
  console.log(`OK  notify_user -> {delivered: true, id: ${notifStructured.id}}`);

  // 7. Keepalive: a blocking ask_user with a progressToken streams >=1
  // notifications/progress before the final result (R-19.3). The harness delays
  // its answer for a "PROGRESS"-tagged question so a keepalive tick fires.
  {
    const progressToken = 'client-tok-1';
    const { progress, result } = await askUserStreaming(
      { question: 'PROGRESS: keep me alive then answer', context: process.cwd() },
      progressToken,
    );
    assert(progress.length >= 1, `expected >=1 progress notification, got ${progress.length}`);
    assert(
      progress[0].params && progress[0].params.progressToken === progressToken,
      'progress notification echoes the progressToken',
    );
    assert(result && result.result && !result.result.isError, 'final result arrives after progress');
    const s = result.result.structuredContent || JSON.parse(result.result.content[0].text);
    assert(['text', 'option'].includes(s.kind), `final kind is an answer (got ${s.kind})`);
    console.log(`OK  keepalive -> ${progress.length} progress notification(s), then ${s.kind}`);
  }

  // 8. Dismiss regression: a dismissed ask_user resolves with kind:"dismissed",
  // NOT a transport timeout (R-19.4). The harness dismisses "DISMISS"-tagged
  // questions. Guard with a wall-clock timeout so a regression surfaces as a
  // clear failure rather than a hang.
  {
    const dismissed = await Promise.race([
      rpc('tools/call', {
        name: 'ask_user',
        arguments: { question: 'DISMISS: this decision is moot', context: process.cwd() },
      }),
      new Promise((_, reject) => setTimeout(() => reject(new Error('ask_user hung after dismiss (R-19.4 regression)')), 10_000)),
    ]);
    const s = dismissed.structuredContent || JSON.parse(dismissed.content[0].text);
    assert(s.kind === 'dismissed', `dismiss resolves as kind:"dismissed" (got ${s.kind})`);
    console.log('OK  dismiss round-trip -> kind:"dismissed"');
  }

  console.log('ALL CHECKS PASSED');
}

main().catch((err) => {
  console.error(`FAILED: ${err.message}`);
  process.exit(1);
});
