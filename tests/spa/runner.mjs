#!/usr/bin/env node
// Deterministic SPA acceptance runner for OxiBrowser vs reference Chrome.
//
// Runs one fixed fixture suite against:
//   --endpoint http://host:port  an already-running engine, connected via CDP (connectOverCDP)
//   --chrome /path/to/Chrome     an isolated reference Chrome launched by this runner (temp profile, headless)
//
// Contract enforced by this runner:
// - User input goes exclusively through Playwright input APIs (locator.click, keyboard.type,
//   keyboard.press). page.evaluate is used only to OBSERVE DOM/CSS state, never to click or type.
// - Every case has a 30s hard timeout; timeouts are failures, never hangs.
// - Uncaptured page errors, console errors, and failed requests fail the case.
// - Steps run sequentially; the first failing step aborts the case with a recorded error.
// - Each case records status, errors, per-step log, timing, and an end-of-case screenshot.
// - Any failed case => non-zero exit code.
//
// All fixtures and framework bundles are served from a local ephemeral HTTP server
// (no runtime CDN). Bundle hashes are recorded into report.json on every run.

import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import process from 'node:process';
import { chromium } from 'playwright';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const FIXTURES = path.join(HERE, 'fixtures');
const VENDOR = {
  '/vendor/react.js': ['react', 'umd/react.production.min.js'],
  '/vendor/react-dom.js': ['react-dom', 'umd/react-dom.production.min.js'],
  '/vendor/vue.js': ['vue', 'dist/vue.global.prod.js'],
};
const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
};
const API_DELAY_MS = 150; // makes the fetch case observably async
const CASE_TIMEOUT_MS = 30_000;
const WAIT_TIMEOUT_MS = 5_000;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
let cleanup = async () => {};

function usage() {
  return [
    'Usage: node runner.mjs [--endpoint http://127.0.0.1:PORT] [--chrome /path/to/Chrome] [--runs N] [--out DIR]',
    '',
    '  --endpoint  CDP endpoint of an already-running engine (Playwright connectOverCDP).',
    '  --chrome    reference Chrome executable; launched isolated (temp profile, headless).',
    '  --runs      consecutive full-suite runs per target (default 10).',
    '  --out       output directory for report.json/report.md/screenshots (default tests/spa/results).',
    '',
    'At least one of --endpoint / --chrome is required; both may be given (same fixture suite).',
  ].join('\n');
}

function fail(msg) {
  console.error('error: ' + msg);
  process.exit(2);
}

function parseArgs(argv) {
  const args = { runs: 10, out: path.join(HERE, 'results') };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--help' || a === '-h') {
      console.log(usage());
      process.exit(0);
    } else if (a === '--endpoint') args.endpoint = argv[++i];
    else if (a === '--chrome') args.chrome = argv[++i];
    else if (a === '--runs') args.runs = Number.parseInt(argv[++i], 10);
    else if (a === '--out') args.out = argv[++i];
    else fail('unknown argument ' + JSON.stringify(a) + '\n\n' + usage());
  }
  if (argv.includes('--endpoint') && !args.endpoint) fail('--endpoint requires a value');
  if (argv.includes('--chrome') && !args.chrome) fail('--chrome requires a value');
  if (!args.endpoint && !args.chrome) fail('need --endpoint and/or --chrome\n\n' + usage());
  if (!Number.isInteger(args.runs) || args.runs < 1) fail('--runs must be a positive integer');
  if (args.endpoint && !/^(https?|wss?):\/\//.test(args.endpoint)) fail('--endpoint must be an http(s) or ws(s) URL');
  return args;
}

// ---------------------------------------------------------------- local server

async function startServer() {
  const server = createServer(async (req, res) => {
    try {
      const u = new URL(req.url, 'http://127.0.0.1');
      let p = decodeURIComponent(u.pathname);
      if (p.endsWith('/')) p += 'index.html';
      let file = null;
      let delay = 0;
      if (p === '/favicon.ico') {
        res.writeHead(204).end();
        return;
      }
      if (VENDOR[p]) {
        file = path.join(HERE, 'node_modules', VENDOR[p][0], VENDOR[p][1]);
      } else if (p === '/api/data.json') {
        file = path.join(FIXTURES, 'api', 'data.json');
        delay = API_DELAY_MS;
      } else {
        const target = path.normalize(path.join(FIXTURES, p));
        if (target !== FIXTURES && !target.startsWith(FIXTURES + path.sep)) {
          res.writeHead(403).end('forbidden');
          return;
        }
        file = target;
      }
      const body = await readFile(file);
      if (delay) await sleep(delay);
      res.writeHead(200, {
        'content-type': MIME[path.extname(file)] || 'application/octet-stream',
        'cache-control': 'no-store',
      });
      res.end(body);
    } catch {
      res.writeHead(404).end('not found');
    }
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return { server, baseUrl: `http://127.0.0.1:${server.address().port}` };
}

async function vendorInfo() {
  const info = {};
  for (const [route, [pkg, rel]] of Object.entries(VENDOR)) {
    const file = path.join(HERE, 'node_modules', pkg, rel);
    const buf = await readFile(file);
    info[route] = { file: `node_modules/${pkg}/${rel}`, sha256: createHash('sha256').update(buf).digest('hex') };
  }
  let playwright = 'unknown';
  try {
    playwright = JSON.parse(await readFile(path.join(HERE, 'node_modules', 'playwright', 'package.json'), 'utf8')).version;
  } catch {}
  const fixtures = {};
  for (const file of new Set([...CASES.map((c) => c.file), 'child.html', 'css/dynamic.css', 'api/data.json'])) {
    fixtures[file] = createHash('sha256').update(await readFile(path.join(FIXTURES, file))).digest('hex');
  }
  return { vendor: info, fixtures, playwright };
}

// ---------------------------------------------------------------- case helpers

function makeEnv(page, run, record) {
  const env = {
    page,
    run,
    log: (s) => record.steps.push(s),
    eval: (fn, arg) => page.evaluate(fn, arg),
    click: async (sel) => {
      record.steps.push(`click ${sel}`);
      await page.locator(sel).click({ timeout: WAIT_TIMEOUT_MS });
    },
    type: async (sel, text) => {
      record.steps.push(`click ${sel}`);
      await page.locator(sel).click({ timeout: WAIT_TIMEOUT_MS });
      record.steps.push(`type ${sel} ${JSON.stringify(text)}`);
      await page.keyboard.type(text, { delay: 10 });
    },
    typeText: async (text) => {
      record.steps.push(`type ${JSON.stringify(text)}`);
      await page.keyboard.type(text, { delay: 10 });
    },
    press: async (key) => {
      record.steps.push(`press ${key}`);
      await page.keyboard.press(key);
    },
    reload: async () => {
      record.steps.push('reload');
      await page.reload({ waitUntil: 'load', timeout: 20_000 });
    },
    goBack: async () => {
      record.steps.push('goBack');
      await page.goBack({ waitUntil: 'load', timeout: 10_000 });
    },
    goForward: async () => {
      record.steps.push('goForward');
      await page.goForward({ waitUntil: 'load', timeout: 10_000 });
    },
    eq: (actual, expected, label) => {
      if (actual !== expected) {
        throw new Error(`${label}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
      }
      record.steps.push(`eq ${label} ok`);
    },
    text: (sel) =>
      page.evaluate((s) => {
        const el = document.querySelector(s);
        return el ? el.textContent : null;
      }, sel),
    waitFor: async (fn, label = '') => {
      const started = performance.now();
      let lastErr = null;
      while (performance.now() - started < WAIT_TIMEOUT_MS) {
        try {
          if (await fn()) {
            record.steps.push(`waitFor ${label} ok`);
            return;
          }
        } catch (err) {
          lastErr = err;
        }
        await sleep(50);
      }
      throw new Error(`waitFor ${label} timed out after ${WAIT_TIMEOUT_MS}ms` + (lastErr ? ` (last error: ${lastErr.message})` : ''));
    },
    waitForEq: async (sel, expected) => {
      let last;
      await env.waitFor(async () => {
        last = await env.text(sel);
        return last === expected;
      }, `${sel}==${JSON.stringify(expected)}`);
    },
  };
  return env;
}

const rectOfMarker = () => {
  const r = document.getElementById('marker').getBoundingClientRect();
  return { x: r.x, y: r.y, w: r.width, h: r.height };
};

// ---------------------------------------------------------------- case suite

const CASES = [
  {
    name: 'vanilla-form',
    file: 'vanilla.html',
    steps: async (e) => {
      await e.click('#submit');
      await e.waitForEq('#validation-status', 'invalid');
      await e.eq(await e.text('#status'), 'idle', 'invalid form was not submitted');
      await e.click('#name');
      await e.eq(await e.eval(() => document.activeElement && document.activeElement.id), 'name', 'focus after click #name');
      await e.typeText('oxi user');
      await e.press('Tab');
      await e.eq(await e.eval(() => document.activeElement && document.activeElement.id), 'agree', 'focus after Tab');
      await e.click('#agree');
      await e.eq(await e.eval(() => document.getElementById('agree').checked), true, 'checkbox checked');
      await e.click('#submit');
      await e.waitForEq('#status', 'submitted:oxi user');
      await e.waitForEq('#out li:last-child', 'oxi user|checked');
    },
  },
  {
    name: 'react-counter',
    file: 'react.html',
    steps: async (e) => {
      await e.waitForEq('#count', 'count=0');
      await e.click('#inc');
      await e.click('#inc');
      await e.click('#inc');
      await e.waitForEq('#count', 'count=3');
    },
  },
  {
    name: 'react-controlled-input',
    file: 'react.html',
    steps: async (e) => {
      await e.waitForEq('#count', 'count=0');
      await e.click('#ctl');
      await e.typeText('hello oxi');
      await e.waitForEq('#mirror', 'hello oxi');
      await e.eq(await e.eval(() => document.getElementById('ctl').value), 'hello oxi', 'controlled input.value');
    },
  },
  {
    name: 'react-list',
    file: 'react.html',
    steps: async (e) => {
      await e.waitForEq('#count', 'count=0');
      await e.type('#item', 'alpha');
      await e.click('#add');
      await e.type('#item', 'beta');
      await e.click('#add');
      await e.waitForEq('#list li:nth-child(1)', 'alpha');
      await e.waitForEq('#list li:nth-child(2)', 'beta');
      await e.eq(await e.eval(() => document.querySelectorAll('#list .li').length), 2, 'list length');
      await e.waitFor(async () => (await e.eval(() => document.getElementById('item').value)) === '', 'item input reset');
    },
  },
  {
    name: 'react-conditional',
    file: 'react.html',
    steps: async (e) => {
      await e.waitForEq('#count', 'count=0');
      await e.eq(await e.eval(() => document.getElementById('cond')), null, 'cond initially absent');
      await e.click('#toggle');
      await e.waitForEq('#cond', 'visible');
      await e.click('#toggle');
      await e.waitFor(async () => (await e.eval(() => document.getElementById('cond'))) === null, 'cond removed');
    },
  },
  {
    name: 'vue-counter',
    file: 'vue.html',
    steps: async (e) => {
      await e.waitForEq('#count', 'count=0');
      await e.click('#inc');
      await e.click('#inc');
      await e.click('#inc');
      await e.waitForEq('#count', 'count=3');
    },
  },
  {
    name: 'vue-controlled-input',
    file: 'vue.html',
    steps: async (e) => {
      await e.waitForEq('#count', 'count=0');
      await e.click('#ctl');
      await e.typeText('hello vue');
      await e.waitForEq('#mirror', 'hello vue');
      await e.eq(await e.eval(() => document.getElementById('ctl').value), 'hello vue', 'v-model input.value');
    },
  },
  {
    name: 'vue-list',
    file: 'vue.html',
    steps: async (e) => {
      await e.waitForEq('#count', 'count=0');
      await e.type('#item', 'alpha');
      await e.click('#add');
      await e.type('#item', 'beta');
      await e.click('#add');
      await e.waitForEq('#list li:nth-child(1)', 'alpha');
      await e.waitForEq('#list li:nth-child(2)', 'beta');
      await e.eq(await e.eval(() => document.querySelectorAll('#list .li').length), 2, 'list length');
      await e.waitFor(async () => (await e.eval(() => document.getElementById('item').value)) === '', 'item input reset');
    },
  },
  {
    name: 'vue-conditional',
    file: 'vue.html',
    steps: async (e) => {
      await e.waitForEq('#count', 'count=0');
      await e.eq(await e.eval(() => document.getElementById('cond')), null, 'cond initially absent');
      await e.click('#toggle');
      await e.waitForEq('#cond', 'visible');
      await e.click('#toggle');
      await e.waitFor(async () => (await e.eval(() => document.getElementById('cond'))) === null, 'cond removed');
    },
  },
  {
    name: 'async-fetch',
    file: 'async.html',
    steps: async (e) => {
      await e.waitForEq('#fetch-status', 'loaded:3:rev1');
      await e.eq(await e.text('#fetch-out li:nth-child(2)'), 'beta', 'fetched item text');
      await e.eq(await e.eval(() => document.querySelectorAll('#fetch-out .fitem').length), 3, 'fetched item count');
    },
  },
  {
    name: 'async-router',
    file: 'async.html',
    steps: async (e) => {
      await e.waitForEq('#fetch-status', 'loaded:3:rev1');
      await e.waitForEq('#view', 'view:#/home');
      await e.click('#nav-a');
      await e.waitForEq('#view', 'view:#/a');
      await e.eq(await e.eval(() => location.hash), '#/a', 'hash after click A');
      await e.click('#nav-b');
      await e.waitForEq('#view', 'view:#/b');
      await e.eq(await e.eval(() => location.hash), '#/b', 'hash after click B');
      await e.goBack();
      await e.waitForEq('#view', 'view:#/a');
      await e.eq(await e.eval(() => location.hash), '#/a', 'hash after goBack');
      await e.goForward();
      await e.waitForEq('#view', 'view:#/b');
      await e.eq(await e.eval(() => location.hash), '#/b', 'hash after goForward');
    },
  },
  {
    name: 'async-stylesheet',
    file: 'async.html',
    steps: async (e) => {
      await e.waitForEq('#fetch-status', 'loaded:3:rev1');
      await e.eq(
        await e.eval(() => getComputedStyle(document.getElementById('box')).color),
        'rgb(0, 0, 0)',
        'color before dynamic css'
      );
      await e.click('#loadcss');
      await e.waitForEq('#css-status', 'css:rgb(1, 2, 3)');
      await e.eq(
        await e.eval(() => getComputedStyle(document.getElementById('box')).color),
        'rgb(1, 2, 3)',
        'color after dynamic css'
      );
      await e.eq(await e.eval(() => !!document.getElementById('dyn')), true, 'dynamic link element present');
    },
  },
  {
    name: 'storage-reload',
    file: 'storage.html',
    steps: async (e) => {
      await e.eval(() => localStorage.removeItem('oxi-spa-persist'));
      await e.reload();
      await e.eq(await e.eval(() => document.getElementById('persist').value), '', 'value cleared');
      await e.click('#persist');
      const token = `keep-${e.run}`;
      await e.typeText(token);
      await e.waitForEq('#mirror', token);
      await e.eq(await e.eval(() => localStorage.getItem('oxi-spa-persist')), token, 'localStorage after input');
      await e.reload();
      await e.eq(await e.eval(() => document.getElementById('persist').value), token, 'value restored after reload');
      await e.waitForEq('#mirror', token);
      await e.eval(() => localStorage.removeItem('oxi-spa-persist'));
    },
  },
  {
    name: 'frame-nested',
    file: 'frames.html',
    steps: async (e) => {
      const page = e.page;
      let child = null;
      await e.waitFor(async () => {
        child = page.frames().find((f) => f.url().includes('child.html'));
        return !!child;
      }, 'child frame attached');
      await child.locator('#inc').click({ timeout: WAIT_TIMEOUT_MS });
      await child.locator('#inc').click({ timeout: WAIT_TIMEOUT_MS });
      e.log('click child #inc x2');
      await e.waitFor(
        async () => (await child.evaluate(() => document.getElementById('ccount').textContent)) === 'clicks:2',
        'child counter'
      );
      await child.locator('#cinput').click({ timeout: WAIT_TIMEOUT_MS });
      await e.typeText('hi');
      await e.waitFor(
        async () => (await child.evaluate(() => document.getElementById('cmirror').textContent)) === 'hi',
        'child input mirror'
      );
      await e.click('#read');
      await e.waitForEq('#parent-read', 'clicks:2');
    },
  },
  {
    name: 'geometry-rect',
    file: 'frames.html',
    steps: async (e) => {
      let before = null;
      let after = null;
      await e.waitFor(async () => {
        before = await e.eval(rectOfMarker);
        return before.w === 50 && before.h === 20;
      }, 'initial marker rect');
      await e.eq(Math.round(before.x), 10, 'marker x before move');
      await e.click('#move');
      await e.waitForEq('#moved-status', 'moved');
      await e.waitFor(async () => {
        after = await e.eval(rectOfMarker);
        return after.x - before.x > 100;
      }, 'marker rect after move');
      await e.eq(Math.round(after.x - before.x), 150, 'marker dx');
      await e.eq(
        await e.eval((pt) => {
          const el = document.elementFromPoint(pt.x, pt.y);
          return el ? el.id : null;
        }, { x: after.x + 5, y: after.y + 5 }),
        'marker',
        'elementFromPoint hits marker'
      );
      await e.eq(
        await e.eval(() => getComputedStyle(document.getElementById('marker')).width),
        '50px',
        'computed width'
      );
    },
  },
];

// ---------------------------------------------------------------- execution

async function runCase(ctx, def, run, baseUrl, shotsDir, shotPrefix) {
  const page = await ctx.newPage();
  const record = {
    name: def.name,
    file: def.file,
    url: `${baseUrl}/${def.file}`,
    run,
    status: 'pass',
    errors: [],
    steps: [],
    issues: { pageErrors: [], consoleErrors: [], requestFailed: [], httpErrors: [] },
    screenshot: null,
    ms: null,
  };
  const issues = record.issues;
  page.on('pageerror', (err) => issues.pageErrors.push(String((err && err.stack) || err)));
  page.on('console', (msg) => {
    if (msg.type() === 'error') issues.consoleErrors.push(msg.text());
  });
  page.on('crash', () => issues.pageErrors.push('page crashed'));
  page.on('response', (response) => {
    if (response.status() >= 400) issues.httpErrors.push(`${response.status()} ${response.url()}`);
  });
  page.on('requestfailed', (req) => {
    issues.requestFailed.push(`${req.url()} :: ${req.failure() ? req.failure().errorText : 'unknown'}`);
  });

  const started = performance.now();
  let timer = null;
  let timedOut = false;
  try {
    await Promise.race([
      (async () => {
        await page.goto(record.url, { waitUntil: 'load', timeout: 20_000 });
        await def.steps(makeEnv(page, run, record));
      })(),
      new Promise((_, reject) => {
        timer = setTimeout(() => {
          timedOut = true;
          reject(new Error(`case timeout after ${CASE_TIMEOUT_MS}ms`));
        }, CASE_TIMEOUT_MS);
      }),
    ]);
  } catch (err) {
    record.status = timedOut ? 'timeout' : 'fail';
    record.errors.push(String((err && err.message) || err));
  } finally {
    clearTimeout(timer);
  }

  if (!page.isClosed()) {
    const shot = path.join(shotsDir, `${shotPrefix}--${def.name}-run${run}.png`);
    try {
      const capture = await page.screenshot({ path: shot });
      record.screenshot = shot;
      if (capture.length < 24 || !capture.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]))) {
        throw new Error('capture is not a PNG');
      }
      record.screenshotSize = { width: capture.readUInt32BE(16), height: capture.readUInt32BE(20) };
      const viewport = page.viewportSize();
      if (!viewport || record.screenshotSize.width !== viewport.width || record.screenshotSize.height !== viewport.height) {
        throw new Error(`capture size ${JSON.stringify(record.screenshotSize)} != viewport ${JSON.stringify(viewport)}`);
      }
    } catch (err) {
      record.errors.push(`screenshot failed: ${(err && err.message) || err}`);
      record.status = 'fail';
    }
  } else {
    record.errors.push('page closed before screenshot');
    record.status = 'fail';
  }

  if (record.status === 'pass') {
    if (issues.pageErrors.length) {
      record.status = 'fail';
      record.errors.push('page errors: ' + issues.pageErrors.join(' | '));
    }
    if (issues.consoleErrors.length) {
      record.status = 'fail';
      record.errors.push('console errors: ' + issues.consoleErrors.join(' | '));
    }
    if (issues.httpErrors.length) {
      record.status = 'fail';
      record.errors.push('HTTP failures: ' + issues.httpErrors.join(' | '));
    }
    if (issues.requestFailed.length) {
      record.status = 'fail';
      record.errors.push('failed requests: ' + issues.requestFailed.join(' | '));
    }
  }

  record.ms = Math.round(performance.now() - started);
  await page.close().catch(() => {});
  return record;
}

async function runTarget(target, args, baseUrl, outDir) {
  const shotsDir = path.join(outDir, 'screenshots');
  await mkdir(shotsDir, { recursive: true });
  const browserVersion = target.browser.version();
  const report = {
    kind: target.kind,
    label: target.kind === 'chrome' ? `chrome:${args.chrome}` : `endpoint:${args.endpoint}`,
    launched: target.launched,
    browserVersion,
    runsRequested: args.runs,
    runs: [],
  };
  for (let run = 1; run <= args.runs; run++) {
    const ctx = await target.browser.newContext({ viewport: { width: 1280, height: 800 } });
    const cases = [];
    for (const def of CASES) {
      const rec = await runCase(ctx, def, run, baseUrl, shotsDir, target.kind);
      cases.push(rec);
      console.log(`[${target.kind}] run ${run}/${args.runs} ${def.name} ${rec.status.toUpperCase()} ${rec.ms}ms` +
        (rec.status === 'pass' ? '' : ' :: ' + rec.errors[0]));
    }
    await ctx.close();
    report.runs.push({ run, cases });
  }

  const perCase = CASES.map((def) => {
    const recs = report.runs.flatMap((r) => r.cases.filter((c) => c.name === def.name));
    const ok = recs.filter((c) => c.status === 'pass');
    const bad = recs.filter((c) => c.status !== 'pass');
    return {
      name: def.name,
      pass: ok.length,
      fail: bad.length,
      avgMs: ok.length ? Math.round(ok.reduce((a, c) => a + c.ms, 0) / ok.length) : null,
      firstError: bad.length ? bad[0].errors[0] : null,
    };
  });
  const failed = perCase.filter((c) => c.fail > 0).length;
  report.summary = {
    cases: CASES.length,
    runs: args.runs,
    caseRuns: CASES.length * args.runs,
    caseRunsPassed: perCase.reduce((a, c) => a + c.pass, 0),
    caseRunsFailed: perCase.reduce((a, c) => a + c.fail, 0),
    casesFullyPassing: CASES.length - failed,
    casesWithFailures: failed,
    verdict: failed === 0 ? 'PASS' : 'FAIL',
  };
  report.perCase = perCase;
  return report;
}

function renderMarkdown(report) {
  const lines = [];
  lines.push('# SPA acceptance results');
  lines.push('');
  lines.push(`- startedAt: ${report.startedAt}`);
  lines.push(`- baseUrl: ${report.baseUrl}`);
  lines.push(`- node: ${report.node} | playwright: ${report.assets.playwright}`);
  lines.push(`- caseTimeoutMs: ${report.caseTimeoutMs} | runs: ${report.argv.join(' ') || '(defaults)'}`);
  lines.push('');
  for (const route of Object.keys(report.assets.vendor)) {
    const v = report.assets.vendor[route];
    lines.push(`- ${route} <- ${v.file} (sha256 ${v.sha256})`);
  }
  for (const t of report.targets) {
    lines.push('');
    lines.push(`## ${t.label}`);
    lines.push('');
    lines.push(`browser: ${t.browserVersion} (${t.launched ? 'launched isolated' : 'connectOverCDP'})`);
    lines.push('');
    lines.push('| case | pass/runs | avg ms | first error |');
    lines.push('| --- | --- | --- | --- |');
    for (const c of t.perCase) {
      lines.push(`| ${c.name} | ${c.pass}/${c.pass + c.fail} | ${c.avgMs} | ${c.firstError ? c.firstError.replace(/\|/g, '\\|') : ''} |`);
    }
    lines.push('');
    lines.push(`**verdict: ${t.summary.verdict}** — ${t.summary.caseRunsPassed}/${t.summary.caseRunsPassed + t.summary.caseRunsFailed} case-runs passed, ${t.summary.casesWithFailures}/${t.summary.cases} cases with failures`);
  }
  lines.push('');
  return lines.join('\n');
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  let info;
  try {
    info = await vendorInfo();
  } catch {
    fail('vendor bundles missing — run `npm ci --prefix tests/spa` first');
  }
  const { server, baseUrl } = await startServer();
  const outDir = path.resolve(args.out);
  await mkdir(outDir, { recursive: true });

  const report = {
    startedAt: new Date().toISOString(),
    baseUrl,
    argv: process.argv.slice(2),
    node: process.version,
    caseTimeoutMs: CASE_TIMEOUT_MS,
    assets: info,
    targets: [],
  };

  const browsers = [];
  let cleaned = false;
  cleanup = async () => {
    if (cleaned) return;
    cleaned = true;
    for (const target of browsers) await target.browser.close().catch(() => {});
    server.closeIdleConnections?.();
    server.closeAllConnections?.();
    await new Promise((resolve) => server.close(resolve));
  };
  let exitCode = 0;
  try {
    if (args.chrome) {
      browsers.push({
        kind: 'chrome',
        browser: await chromium.launch({
          executablePath: args.chrome,
          headless: true,
          args: ['--no-first-run', '--no-default-browser-check'],
        }),
        launched: true,
      });
    }
    if (args.endpoint) {
      browsers.push({
        kind: 'endpoint',
        browser: await chromium.connectOverCDP(args.endpoint, { timeout: 20_000 }),
        launched: false,
      });
    }
    for (const target of browsers) {
      const t = await runTarget(target, args, baseUrl, outDir);
      report.targets.push(t);
      if (t.summary.verdict !== 'PASS') exitCode = 1;
      console.log(
        `[${target.kind}] verdict ${t.summary.verdict}: ${t.summary.caseRunsPassed}/${t.summary.caseRunsPassed + t.summary.caseRunsFailed} case-runs passed`
      );
    }
  } finally {
    await cleanup();
    await writeFile(path.join(outDir, 'report.json'), JSON.stringify(report, null, 2));
    await writeFile(path.join(outDir, 'report.md'), renderMarkdown(report));
  }

  console.log(`report: ${path.join(outDir, 'report.md')}`);
  process.exit(exitCode);
}

process.once('SIGINT', async () => { await cleanup(); process.exit(130); });
process.once('SIGTERM', async () => { await cleanup(); process.exit(143); });
main().catch((err) => {
  console.error('runner fatal:', (err && err.stack) || err);
  process.exit(2);
});
