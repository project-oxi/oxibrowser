#!/usr/bin/env node
// SPA stability acceptance: N reload cycles of react.html (load -> #count=0 ->
// native click #inc -> #count=1) in one context, then two concurrent isolated
// contexts typing distinct values into storage.html and each retaining only its
// own after reload (real keyboard input; no evaluate for input). pageerror /
// requestfailed / HTTP>=400 / crash / timeout fail. PASS = all cycles + both
// contexts pass. Only self-created resources are closed/killed, incl. signals.
// Timings/RSS are sampled observations on shared hardware, not perf claims.

import { createHash } from 'node:crypto';
import { execFile, spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const FIXTURES = path.join(HERE, 'fixtures');
const VENDOR = {
  '/vendor/react.js': ['react', 'umd/react.production.min.js'],
  '/vendor/react-dom.js': ['react-dom', 'umd/react-dom.production.min.js'],
};
const MIME = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8' };
const NAV_MS = 15_000;
const WAIT_MS = 10_000;
const STEP_BUDGET_MS = 30_000; // hard cap: one cycle / one storage run
const READY_TIMEOUT_MS = 30_000;
const RSS_SAMPLE_MS = 2_000;

const sha256 = (buf) => createHash('sha256').update(buf).digest('hex');
let cleanup = async () => {};
const ownedEngines = new Set();

function usage() {
  return [
    'Usage: node stability.mjs (--endpoint URL | --binary PATH | --chrome PATH) [--cycles N] [--out DIR]',
    '  --endpoint  CDP endpoint of a running engine (connectOverCDP).',
    '  --binary    engine binary, spawned isolated: `--no-audit serve --allow-private-ips --port <free>`.',
    '  --chrome    reference Chrome executable, launched isolated (temp profile, headless).',
    '  --cycles    reload cycles per target (default 100, positive integer).',
    '  --out       output dir for stability.json + screenshots (default /tmp/oxi-spa-stability).',
    'At least one target flag is required; multiple targets run sequentially.',
  ].join('\n');
}

function fail(msg) {
  console.error('error: ' + msg);
  process.exit(2);
}

function parseArgs(argv) {
  const args = { cycles: 100, out: '/tmp/oxi-spa-stability' };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--help' || a === '-h') { console.log(usage()); process.exit(0); }
    else if (a === '--endpoint') args.endpoint = argv[++i];
    else if (a === '--binary') args.binary = argv[++i];
    else if (a === '--chrome') args.chrome = argv[++i];
    else if (a === '--cycles') args.cycles = Number.parseInt(argv[++i], 10);
    else if (a === '--out') args.out = argv[++i];
    else fail('unknown argument ' + JSON.stringify(a) + '\n\n' + usage());
  }
  for (const [flag, val] of [['--endpoint', args.endpoint], ['--binary', args.binary], ['--chrome', args.chrome]]) {
    if (argv.includes(flag) && !val) fail(flag + ' requires a value');
  }
  if (!args.endpoint && !args.binary && !args.chrome) fail('need --endpoint, --binary, or --chrome\n\n' + usage());
  if (!Number.isInteger(args.cycles) || args.cycles < 1) fail('--cycles must be a positive integer');
  if (args.endpoint && !/^(https?|wss?):\/\//.test(args.endpoint)) fail('--endpoint must be an http(s) or ws(s) URL');
  return args;
}

// ------------------------------------------------------------- local server (dup of runner.mjs subset)

async function startServer() {
  const server = createServer(async (req, res) => {
    try {
      const p = decodeURIComponent(new URL(req.url, 'http://127.0.0.1').pathname);
      if (p === '/favicon.ico') return res.writeHead(204).end();
      let file;
      if (VENDOR[p]) file = path.join(HERE, 'node_modules', ...VENDOR[p]);
      else {
        const target = path.normalize(path.join(FIXTURES, p));
        if (target !== FIXTURES && !target.startsWith(FIXTURES + path.sep)) return res.writeHead(403).end('forbidden');
        file = target;
      }
      const body = await readFile(file); // exact same bytes as the runner serves
      res.writeHead(200, { 'content-type': MIME[path.extname(file)] || 'application/octet-stream', 'cache-control': 'no-store' });
      res.end(body);
    } catch {
      res.writeHead(404).end('not found');
    }
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return { server, baseUrl: `http://127.0.0.1:${server.address().port}` };
}

async function assets() {
  const vendor = {};
  for (const route of Object.keys(VENDOR)) {
    const file = path.join(HERE, 'node_modules', ...VENDOR[route]);
    vendor[route] = { file, sha256: sha256(await readFile(file)) };
  }
  const playwright = JSON.parse(await readFile(path.join(HERE, 'node_modules', 'playwright', 'package.json'), 'utf8')).version;
  const fixtures = {};
  for (const file of ['react.html', 'storage.html']) fixtures[file] = sha256(await readFile(path.join(FIXTURES, file)));
  return { vendor, fixtures, playwright };
}

// ------------------------------------------------------------- helpers

const withTimeout = (p, ms, what) => {
  let timer;
  p.catch(() => {}); // swallow late rejection if the watchdog fires first
  return Promise.race([
    p,
    new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(what + ` exceeded ${ms}ms`)), ms); }),
  ]).finally(() => clearTimeout(timer));
};

const percentile = (sorted, q) => {
  if (!sorted.length) return null;
  const i = (sorted.length - 1) * q;
  const lo = Math.floor(i);
  const hi = Math.ceil(i);
  return Math.round(sorted[lo] + (sorted[hi] - sorted[lo]) * (i - lo));
};

function watchPage(page, sink) {
  page.on('pageerror', (e) => sink.push(`pageerror: ${e?.message || e}`));
  page.on('crash', () => sink.push('page crashed'));
  page.on('requestfailed', (r) => sink.push(`requestfailed: ${r.url()} ${r.failure()?.errorText || ''}`.trim()));
  page.on('response', (r) => { if (r.status() >= 400) sink.push(`http ${r.status()}: ${r.url()}`); });
}

// Pinned playwright 1.48.2 exposes no locator assertion methods; poll instead.
async function waitText(page, sel, expected, timeoutMs = WAIT_MS) {
  const started = performance.now();
  let last = null;
  while (performance.now() - started < timeoutMs) {
    last = await page.locator(sel).textContent({ timeout: timeoutMs }).catch(() => null);
    if (last === expected) return;
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`wait ${sel}==${JSON.stringify(expected)}: last ${JSON.stringify(last)} after ${Math.round(performance.now() - started)}ms`);
}

function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.once('error', reject);
    srv.listen(0, '127.0.0.1', () => { const { port } = srv.address(); srv.close(() => resolve(port)); });
  });
}

async function spawnEngine(binary) {
  const port = await freePort();
  const child = spawn(binary, ['--no-audit', 'serve', '--allow-private-ips', '--host', '127.0.0.1', '--port', String(port)], { stdio: ['ignore', 'pipe', 'pipe'] });
  ownedEngines.add(child);
  child.once('error', (error) => { child.exitInfo = { code: null, signal: null, error: String(error) }; });
  let tail = '';
  const note = (chunk) => { tail = (tail + chunk).slice(-4000); };
  child.stdout.on('data', note);
  child.stderr.on('data', note);
  child.once('exit', (code, signal) => { child.exitInfo = { code, signal }; });
  const origin = `http://127.0.0.1:${port}`;
  const deadline = Date.now() + READY_TIMEOUT_MS;
  while (Date.now() < deadline) {
    if (child.exitInfo) throw new Error(`engine exited before ready (code=${child.exitInfo.code} signal=${child.exitInfo.signal}); output tail: ${tail.slice(-800)}`);
    try {
      const res = await fetch(`${origin}/json/version`, { signal: AbortSignal.timeout(1000) });
      if (res.ok) return { child, endpoint: origin };
    } catch { /* not ready yet */ }
    await new Promise((r) => setTimeout(r, 250));
  }
  child.kill('SIGKILL');
  throw new Error(`engine not ready on ${origin} after ${READY_TIMEOUT_MS}ms; output tail: ${tail.slice(-800)}`);
}

// Sampled (not peak) RSS of the spawned engine; best-effort, missing samples are absent.
function startRssSampler(pid, samples) {
  const t0 = Date.now();
  const timer = setInterval(() => {
    execFile('ps', ['-o', 'rss=', '-p', String(pid)], { timeout: 2000 }, (err, stdout) => {
      const rssKb = err ? NaN : Number.parseInt(stdout.trim(), 10);
      if (Number.isFinite(rssKb)) samples.push({ atMs: Date.now() - t0, rssKb });
    });
  }, RSS_SAMPLE_MS);
  return () => clearInterval(timer);
}

// ------------------------------------------------------------- phases

async function captureViewport(page, file) {
  const png = await page.screenshot({ path: file });
  const viewport = page.viewportSize();
  if (png.length < 24 || !png.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]))) {
    throw new Error('capture is not a PNG');
  }
  const size = { width: png.readUInt32BE(16), height: png.readUInt32BE(20) };
  if (!viewport || size.width !== viewport.width || size.height !== viewport.height) {
    throw new Error(`capture size ${JSON.stringify(size)} != viewport ${JSON.stringify(viewport)}`);
  }
}

async function reloadCycles(browser, baseUrl, cycles, shotsDir, label) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await ctx.newPage();
  const errors = [];
  watchPage(page, errors);
  const url = `${baseUrl}/react.html`;
  const per = [];
  let failure = null;
  for (let i = 1; i <= cycles && !failure; i++) {
    const t0 = performance.now();
    const seen = errors.length;
    try {
      await withTimeout((async () => {
        await (i === 1 ? page.goto(url, { timeout: NAV_MS }) : page.reload({ timeout: NAV_MS }));
        await waitText(page, '#count', 'count=0');
        await page.locator('#inc').click({ timeout: WAIT_MS });
        await waitText(page, '#count', 'count=1');
        if (errors.length > seen) throw new Error(`cycle ${i}: ${errors.slice(seen).join(' | ')}`);
      })(), STEP_BUDGET_MS, `cycle ${i}`);
      per.push({ cycle: i, ms: Math.round(performance.now() - t0) });
    } catch (err) {
      failure = { cycle: i, error: String(err?.message || err) };
      await page.screenshot({ path: path.join(shotsDir, `${label}-cycle-${i}-fail.png`) }).catch(() => {});
    }
  }
  try {
    await captureViewport(page, path.join(shotsDir, `${label}-cycle-end.png`));
  } catch (err) {
    errors.push(`final screenshot: ${err?.message || err}`);
    failure ||= { cycle: per.length, error: errors[errors.length - 1] };
  }
  await ctx.close();
  const ok = per.map((c) => c.ms).sort((a, b) => a - b);
  return {
    requested: cycles,
    passed: per.length,
    notRun: failure ? Math.max(0, cycles - per.length - 1) : 0,
    perCycle: per,
    p50Ms: percentile(ok, 0.5), // success-only samples
    p95Ms: percentile(ok, 0.95),
    errors,
    failure,
  };
}

async function storageCheck(browser, baseUrl, value, shotsDir, label) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const page = await ctx.newPage();
  const errors = [];
  watchPage(page, errors);
  const rec = { label, value, mirrorAfterReload: null, retainedInput: null, pass: false, error: null };
  try {
    await withTimeout((async () => {
      await page.goto(`${baseUrl}/storage.html`, { timeout: NAV_MS });
      await page.locator('#persist').click({ timeout: WAIT_MS }); // real focus, no evaluate
      await page.keyboard.type(value, { delay: 5 }); // real keystrokes drive the input listener
      await waitText(page, '#mirror', value);
      await page.reload({ timeout: NAV_MS });
      await waitText(page, '#mirror', value);
      rec.mirrorAfterReload = await page.locator('#mirror').textContent();
      rec.retainedInput = await page.locator('#persist').inputValue(); // observation only
      if (rec.retainedInput !== value) throw new Error(`retained input ${JSON.stringify(rec.retainedInput)} != ${JSON.stringify(value)}`);
      if (errors.length) throw new Error(errors.join(' | '));
      await captureViewport(page, path.join(shotsDir, `${label}-storage-end.png`));
      rec.pass = true;
    })(), STEP_BUDGET_MS, `${label} storage run`);
  } catch (err) {
    rec.error = String(err?.message || err);
    await page.screenshot({ path: path.join(shotsDir, `${label}-storage-fail.png`) }).catch(() => {});
  }
  await ctx.close();
  return rec;
}

async function runTarget(target, args, baseUrl, shotsDir) {
  const rec = {
    kind: target.kind,
    label: target.label,
    launched: target.launched,
    browserVersion: null,
    binarySha256: target.binarySha256 || null,
    cycles: null,
    storageIsolation: null,
    rssSamplesKb: target.rss ? [] : null,
    errors: [],
  };
  try {
    rec.browserVersion = await target.browser.version();
    rec.cycles = await reloadCycles(target.browser, baseUrl, args.cycles, shotsDir, target.kind);
    console.log(`[${target.kind}] cycles ${rec.cycles.passed}/${rec.cycles.requested} passed` + (rec.cycles.failure ? ` :: ${rec.cycles.failure.error}` : ''));
    const stamp = `${Date.now().toString(36)}`;
    const contexts = await Promise.all([
      storageCheck(target.browser, baseUrl, `ctx-a-${stamp}`, shotsDir, 'ctx-a'),
      storageCheck(target.browser, baseUrl, `ctx-b-${stamp}`, shotsDir, 'ctx-b'),
    ]);
    rec.storageIsolation = { contexts, pass: contexts.every((c) => c.pass) && contexts[0].retainedInput !== contexts[1].retainedInput };
    console.log(`[${target.kind}] storage isolation ${rec.storageIsolation.pass ? 'PASS' : 'FAIL'}`);
    const cyclesOk = rec.cycles.passed === rec.cycles.requested && !rec.cycles.failure && rec.cycles.errors.length === 0;
    rec.verdict = cyclesOk && rec.storageIsolation.pass ? 'PASS' : 'FAIL';
  } catch (err) {
    rec.verdict = 'FAIL';
    rec.errors.push(String(err?.stack || err));
  } finally {
    if (target.rss) rec.rssSamplesKb = target.rss.samples;
    await target.teardown().catch(() => {});
  }
  return rec;
}

// ------------------------------------------------------------- main

async function main() {
  const args = parseArgs(process.argv.slice(2));
  let assetInfo;
  try {
    assetInfo = await assets();
  } catch {
    fail('vendor bundles missing — run `npm ci --prefix tests/spa` first');
  }
  const { server, baseUrl } = await startServer();
  const outDir = path.resolve(args.out);
  await mkdir(path.join(outDir, 'screenshots'), { recursive: true });
  const targets = [];
  let cleaned = false;
  cleanup = async () => {
    if (cleaned) return;
    cleaned = true;
    for (const t of targets) await t.teardown().catch(() => {});
    // Also cover a server whose CDP connection failed before target registration.
    for (const child of ownedEngines) {
      if (child.exitCode !== null || child.signalCode !== null) continue;
      const exited = new Promise((resolve) => child.once('exit', resolve));
      child.kill('SIGTERM');
      await Promise.race([exited, new Promise((resolve) => setTimeout(resolve, 3000))]);
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
    }
    server.closeIdleConnections?.();
    server.closeAllConnections?.();
    await new Promise((resolve) => server.close(resolve));
  };
  const report = {
    startedAt: new Date().toISOString(),
    argv: process.argv.slice(2),
    baseUrl,
    node: process.version,
    host: { platform: os.platform(), release: os.release(), arch: os.arch(), cpus: os.cpus().length },
    playwright: assetInfo.playwright,
    assets: assetInfo,
    cyclesRequested: args.cycles,
    output: outDir,
    note: 'timings and RSS are sampled observations on shared hardware, not performance claims',
    targets: [],
  };
  try {
    if (args.chrome) {
      const browser = await chromium.launch({ executablePath: args.chrome, headless: true, args: ['--no-first-run', '--no-default-browser-check'] });
      targets.push({ kind: 'chrome', label: `chrome:${args.chrome}`, browser, launched: true, teardown: () => browser.close() });
    }
    if (args.binary) {
      const binarySha256 = sha256(await readFile(args.binary));
      const { child, endpoint } = await spawnEngine(args.binary);
      const rssSamples = [];
      const stopRss = startRssSampler(child.pid, rssSamples);
      const browser = await chromium.connectOverCDP(endpoint, { timeout: 20_000 });
      targets.push({
        kind: 'binary',
        label: `binary:${args.binary}`,
        browser,
        launched: true,
        binarySha256,
        rss: { samples: rssSamples },
        teardown: async () => {
          await browser.close().catch(() => {});
          stopRss();
          if (child.exitInfo) return;
          const died = new Promise((resolve) => child.once('exit', () => resolve()));
          if (!child.exitInfo) child.kill('SIGTERM');
          await Promise.race([died, new Promise((r) => setTimeout(r, 3000))]);
          if (!child.exitInfo) { child.kill('SIGKILL'); await Promise.race([died, new Promise((r) => setTimeout(r, 2000))]); }
        },
      });
    }
    if (args.endpoint) {
      const browser = await chromium.connectOverCDP(args.endpoint, { timeout: 20_000 });
      targets.push({ kind: 'endpoint', label: `endpoint:${args.endpoint}`, browser, launched: false, teardown: () => browser.close() });
    }
    let exitCode = 0;
    for (const target of targets) {
      const rec = await runTarget(target, args, baseUrl, path.join(outDir, 'screenshots'));
      report.targets.push(rec);
      console.log(`[${target.kind}] verdict ${rec.verdict}`);
      if (rec.verdict !== 'PASS') exitCode = 1;
    }
    report.verdict = report.targets.length > 0 && report.targets.every((t) => t.verdict === 'PASS') ? 'PASS' : 'FAIL';
    await cleanup();
    await writeFile(path.join(outDir, 'stability.json'), JSON.stringify(report, null, 2));
    console.log(`report: ${path.join(outDir, 'stability.json')}`);
    process.exit(exitCode);
  } catch (err) {
    report.verdict = 'FAIL';
    report.fatal = String(err?.stack || err);
    await cleanup().catch(() => {});
    await writeFile(path.join(outDir, 'stability.json'), JSON.stringify(report, null, 2)).catch(() => {});
    console.error('stability fatal:', report.fatal);
    process.exit(1);
  }
}

process.once('SIGINT', async () => { await cleanup().catch(() => {}); process.exit(130); });
process.once('SIGTERM', async () => { await cleanup().catch(() => {}); process.exit(143); });
main().catch((err) => { console.error('stability fatal:', (err && err.stack) || err); process.exit(2); });
