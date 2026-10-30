#!/usr/bin/env node
// Manual acceptance for the unchanged e84f8842-d6722ab retained release.
// Requires Node 22+ and an OxiBrowser CDP server built with the webgl feature.
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';

const [gameUrl, cdpUrl = 'ws://127.0.0.1:9222/ws', requestedOutput] = process.argv.slice(2);
if (!gameUrl || typeof WebSocket !== 'function') {
  console.error('Usage: node scripts/check-cheonggye.mjs <game-url> [cdp-ws-url] [output-dir] (Node 22+)');
  process.exit(2);
}
const output = requestedOutput ?? path.join(os.tmpdir(), `oxibrowser-cheonggye-${Date.now()}`);
await fs.mkdir(output, { recursive: true });
const report = { gameUrl, cdpUrl, startedAt: new Date().toISOString(), samples: [], events: [] };
const started = performance.now();
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const ws = new WebSocket(cdpUrl);
const pending = new Map();
let nextId = 0;

ws.addEventListener('message', ({ data }) => {
  const message = JSON.parse(data);
  if (message.id) {
    const request = pending.get(message.id);
    if (!request) return;
    clearTimeout(request.timer);
    pending.delete(message.id);
    if (message.error) request.reject(new Error(JSON.stringify(message.error)));
    else request.resolve(message.result);
  } else if (['Runtime.exceptionThrown', 'Runtime.consoleAPICalled'].includes(message.method)) {
    report.events.push(message);
  }
});
ws.addEventListener('close', () => {
  for (const request of pending.values()) {
    clearTimeout(request.timer);
    request.reject(new Error('CDP connection closed'));
  }
  pending.clear();
});
function send(method, params = {}) {
  return new Promise((resolve, reject) => {
    const id = ++nextId;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`${method} timed out`));
    }, 600_000);
    pending.set(id, { resolve, reject, timer });
    ws.send(JSON.stringify({ id, method, params }));
  });
}
function welcomeState(node) {
  const attributes = Object.fromEntries(Array.from({ length: (node.attributes?.length ?? 0) / 2 },
    (_, index) => node.attributes.slice(index * 2, index * 2 + 2)));
  if (attributes.id === 'welcome') return attributes;
  for (const child of node.children ?? []) {
    const state = welcomeState(child);
    if (state) return state;
  }
  return null;
}
async function evaluate(expression, awaitPromise = false) {
  const result = await send('Runtime.evaluate', { expression, awaitPromise, returnByValue: true });
  if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
  return JSON.parse(result.result.value);
}
async function screenshot(name) {
  const shot = await send('Page.captureScreenshot', { format: 'png' });
  await fs.writeFile(path.join(output, name), Buffer.from(shot.data, 'base64'));
}

try {
  await new Promise((resolve, reject) => {
    ws.addEventListener('open', resolve, { once: true });
    ws.addEventListener('error', reject, { once: true });
  });
  await send('Runtime.enable');
  await send('Page.enable');
  await send('Page.addScriptToEvaluateOnNewDocument', { source: `
    globalThis.__oxiAcceptanceKeys = [];
    for (const type of ['keydown', 'keyup']) window.addEventListener(type, event => {
      __oxiAcceptanceKeys.push({type: event.type, key: event.key, code: event.code});
    });
  ` });
  report.navigation = await send('Page.navigate', { url: gameUrl });
  if (report.navigation.errorText) throw new Error(report.navigation.errorText);

  // Native DOM polling avoids evaluating app work under foreground eval limits.
  for (let sample = 0; sample < 120; sample++) {
    const document = await send('DOM.getDocument', { depth: -1 });
    const welcome = welcomeState(document.root);
    report.samples.push({ elapsedMs: performance.now() - started, welcome });
    if (welcome && Object.hasOwn(welcome, 'hidden')) {
      report.ready = true;
      break;
    }
    if (welcome?.['aria-label'] === 'Unable to load') throw new Error('Game readiness failed');
    await sleep(10_000);
  }
  if (!report.ready) throw new Error('Game did not reach readiness within the polling window');

  report.beforeIdle = await evaluate(`import('/assets/engineStore-Bn2tApal.js').then(({t: store}) => {
    const scene = store.Instances.flatMap(engine => engine.scenes ?? [])
      .reduce((largest, next) => !largest || next.meshes.length > largest.meshes.length ? next : largest, null);
    globalThis.__oxiAcceptanceFrames = [];
    globalThis.__oxiAcceptanceObserver = scene.onAfterRenderObservable.add(() => {
      __oxiAcceptanceFrames.push({at: Date.now(), frameId: scene.getFrameId()});
      if (__oxiAcceptanceFrames.length > 1000) __oxiAcceptanceFrames.shift();
    });
    globalThis.__oxiAcceptanceScene = scene;
    globalThis.__oxiAcceptanceState = () => {
      return { meshes: scene?.meshes.length, version: scene?.getEngine().webGLVersion,
        frameId: scene?.getFrameId(), at: performance.now(),
        player: scene?.transformNodes.find(node => node.name === 'player')?.position.asArray(),
        loops: scene?.getEngine()._activeRenderLoops.length, keys: __oxiAcceptanceKeys,
        frames: __oxiAcceptanceFrames };
    };
    return JSON.stringify(__oxiAcceptanceState());
  })`, true);
  await screenshot('before.png');
  await sleep(5_000);
  report.before = await evaluate('JSON.stringify(__oxiAcceptanceState())');
  const key = { key: 'w', code: 'KeyW', windowsVirtualKeyCode: 87, nativeVirtualKeyCode: 87 };
  await send('Input.dispatchKeyEvent', { type: 'keyDown', ...key });
  report.holdStartedAt = Date.now();
  try {
    await sleep(5_000);
  } finally {
    report.holdEndedAt = Date.now();
    await send('Input.dispatchKeyEvent', { type: 'keyUp', ...key });
  }
  await sleep(500);
  report.after = await evaluate('JSON.stringify(__oxiAcceptanceState())');
  await send('Runtime.evaluate', { expression:
    '__oxiAcceptanceScene.onAfterRenderObservable.remove(__oxiAcceptanceObserver)' });
  await screenshot('after.png');
  const before = report.before, after = report.after;
  if (!report.beforeIdle.player || !before.player || !after.player) throw new Error('Player transform was not found');
  const horizontalDistance = (from, to) => Math.hypot(to[0] - from[0], to[2] - from[2]);
  report.idleDistance = horizontalDistance(report.beforeIdle.player, before.player);
  report.distance = horizontalDistance(before.player, after.player);
  // Local CDP and Node share the host clock. Only rendered frames strictly
  // inside the host's command-free hold count as autonomous progression.
  report.holdFrames = after.frames.filter(frame =>
    frame.at > report.holdStartedAt + 100 && frame.at < report.holdEndedAt - 100);
  const distinctFrames = new Set(report.holdFrames.map(frame => frame.frameId)).size;
  report.pass = before.version === 2 && before.meshes > 0 && after.loops > 0
    && distinctFrames >= 3
    && report.holdFrames.at(-1).at - report.holdFrames[0].at >= 1000
    && report.distance > Math.max(0.05, report.idleDistance * 2 + 0.05)
    && after.keys.some(event => event.type === 'keydown' && event.code === 'KeyW')
    && after.keys.some(event => event.type === 'keyup' && event.code === 'KeyW');
  if (!report.pass) throw new Error('Scene, frame progression or W movement acceptance failed');
} catch (error) {
  report.error = String(error);
  process.exitCode = 1;
} finally {
  report.finishedAt = new Date().toISOString();
  report.elapsedMs = performance.now() - started;
  await fs.writeFile(path.join(output, 'report.json'), JSON.stringify(report, null, 2));
  ws.close();
  console.log(JSON.stringify({ pass: report.pass ?? false, output, error: report.error }));
}
