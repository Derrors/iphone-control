#!/usr/bin/env node
/**
 * Zero-dependency headless Chrome / Chromium CDP smoke test for the control page (`iphone-control viewer`).
 *
 * Uses Node 22+'s built-in WebSocket client to speak CDP directly to a local Chrome / Chromium binary
 * (no npm packages, no Playwright/Puppeteer). Starts an isolated temporary mock daemon Unix socket and
 * the built `iphone-control viewer` binary in a temporary directory — never touches the host daemon,
 * user settings (`~/.config/iphone-control`), or any real USB device.
 *
 * Usage:
 *   node tests/viewer/smoke.mjs --binary target/debug/iphone-control
 *   CHROME_BIN=/path/to/chrome node tests/viewer/smoke.mjs --binary target/debug/iphone-control
 */

import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const IS_CI = process.env.CI === 'true' || process.env.CI === '1' || process.env.GITHUB_ACTIONS === 'true';

// 16x32 solid-color baseline JPEG (exact 1:2 aspect ratio matching the mock display 320x640).
const SAMPLE_JPEG = Buffer.from(
  '/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDABALDA4MChAODQ4SERATGCgaGBYWGDEjJR0oOjM9PDkzODdASFxOQERXRTc4UG1RV19iZ2hnPk1xeXBkeFxlZ2P/' +
  '2wBDARESEhgVGC8aGi9jQjhCY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2NjY2P/' +
  'wAARCAAgABADASIAAhEBAxEB/8QAHwAAAQUBAQEBAQEAAAAAAAAAAAECAwQFBgcICQoL/' +
  '8QAtRAAAgEDAwIEAwUFBAQAAAF9AQIDAAQRBRIhMUEGE1FhByJxFDKBkaEII0KxwRVS0fAkM2JyggkKFhcYGRolJicoKSo0NTY3ODk6Q0RFRkdISUpTVFVWV1hZWmNkZWZnaGlqc3R1dnd4eXqDhIWGh4iJipKTlJWWl5iZmqKjpKWmp6ipqrKztLW2t7i5usLDxMXGx8jJytLT1NXW19jZ2uHi4+Tl5ufo6erx8vP09fb3+Pn6/' +
  '8QAHwEAAwEBAQEBAQEBAQAAAAAAAAECAwQFBgcICQoL/' +
  '8QAtREAAgECBAQDBAcFBAQAAQJ3AAECAxEEBSExBhJBUQdhcRMiMoEIFEKRobHBCSMzUvAVYnLRChYkNOEl8RcYGRomJygpKjU2Nzg5OkNERUZHSElKU1RVVldYWVpjZGVmZ2hpanN0dXZ3eHl6goOEhYaHiImKkpOUlZaXmJmaoqOkpaanqKmqsrO0tba3uLm6wsPExcbHyMnK0tPU1dbX2Nna4uPk5ebn6Onq8vP09fb3+Pn6/' +
  '9oADAMBAAIRAxEAPwDJooorrOUKKKKAP//Z',
  'base64',
);

const MOCK_DEVICE = {
  udid: 'SMOKE-DEVICE-0001',
  name: 'Smoke iPhone',
  product_type: 'iPhone16,2',
  product_version: '26.4',
};

const MOCK_APPS = [
  { name: 'Clock', bundle_id: 'com.apple.mobiletimer' },
  { name: 'Safari', bundle_id: 'com.apple.mobilesafari' },
  { name: 'Settings', bundle_id: 'com.apple.Preferences' },
];

const MOCK_ELEMENTS = [
  { type: 'Button', label: 'Safari', x: 40, y: 120, width: 60, height: 60 },
  { type: 'StaticText', label: 'Smoke Home', x: 110, y: 40, width: 100, height: 24 },
];

function parseArgs(argv) {
  let binary = process.env.ICTL_BINARY || '';
  let chrome = process.env.CHROME_BIN || '';
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === '--binary' && argv[i + 1]) {
      binary = argv[++i];
    } else if (arg.startsWith('--binary=')) {
      binary = arg.slice('--binary='.length);
    } else if (arg === '--chrome' && argv[i + 1]) {
      chrome = argv[++i];
    } else if (arg.startsWith('--chrome=')) {
      chrome = arg.slice('--chrome='.length);
    } else if (arg === '--help' || arg === '-h') {
      console.log('Usage: node tests/viewer/smoke.mjs [--binary <path>] [--chrome <path>]');
      process.exit(0);
    } else {
      throw new Error(`Unknown argument: ${arg}`);
    }
  }
  if (!binary) {
    for (const rel of ['target/debug/iphone-control', 'target/release/iphone-control']) {
      const p = path.join(ROOT, rel);
      if (isExecutable(p)) {
        binary = p;
        break;
      }
    }
  }
  if (!binary) {
    throw new Error('Could not find iphone-control binary; build it first or pass --binary <path>');
  }
  const resolvedBinary = path.resolve(binary);
  if (!isExecutable(resolvedBinary)) {
    throw new Error(`Binary is missing or not executable: ${resolvedBinary}`);
  }
  return { binary: resolvedBinary, explicitChrome: chrome };
}

function isExecutable(filePath) {
  try {
    const st = fs.statSync(filePath);
    if (!st.isFile()) return false;
    fs.accessSync(filePath, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function findInPath(names) {
  const dirs = (process.env.PATH || '').split(path.delimiter).filter(Boolean);
  for (const name of names) {
    for (const dir of dirs) {
      const candidate = path.join(dir, name);
      if (isExecutable(candidate)) return candidate;
    }
  }
  return null;
}

function findPlaywrightChrome() {
  const home = process.env.HOME;
  if (!home) return null;
  const cacheDir = path.join(home, '.cache', 'ms-playwright');
  let entries;
  try {
    entries = fs.readdirSync(cacheDir);
  } catch {
    return null;
  }
  const sorted = entries.sort().reverse();
  for (const name of sorted) {
    if (!name.startsWith('chromium')) continue;
    const base = path.join(cacheDir, name);
    for (const rel of [
      'chrome-linux/chrome',
      'chrome-linux64/chrome',
      'chrome-headless-shell-linux64/chrome-headless-shell',
      'chrome-mac/Chromium.app/Contents/MacOS/Chromium',
      'chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing',
    ]) {
      const p = path.join(base, rel);
      if (isExecutable(p)) return p;
    }
  }
  return null;
}

function findChrome(explicitChrome) {
  if (explicitChrome) {
    const resolved = path.isAbsolute(explicitChrome)
      ? explicitChrome
      : (findInPath([explicitChrome]) || path.resolve(explicitChrome));
    if (!isExecutable(resolved)) {
      throw new Error(`Configured Chrome binary (--chrome / CHROME_BIN) is not executable: ${explicitChrome}`);
    }
    return resolved;
  }
  const onPath = findInPath([
    'google-chrome',
    'google-chrome-stable',
    'chromium',
    'chromium-browser',
    'chrome',
  ]);
  if (onPath) return onPath;

  const fixed = [
    '/usr/bin/google-chrome',
    '/usr/bin/google-chrome-stable',
    '/usr/bin/chromium',
    '/usr/bin/chromium-browser',
    '/snap/bin/chromium',
    '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
    '/Applications/Chromium.app/Contents/MacOS/Chromium',
  ];
  for (const candidate of fixed) {
    if (isExecutable(candidate)) return candidate;
  }
  return findPlaywrightChrome();
}

function cleanEnv(extra = {}) {
  const env = {};
  for (const [k, v] of Object.entries(process.env)) {
    if (v === undefined) continue;
    if (/^(http|https|all|no)_proxy$/i.test(k)) continue;
    if (/^(IPHONE_CONTROL|ICTL)_/.test(k)) continue;
    env[k] = v;
  }
  return { ...env, ...extra };
}

function redactUrl(url) {
  return String(url || '').replace(/([?&]token=)[^&#\s]+/gi, '$1<redacted>');
}

function sleep(ms) {
  return new Promise(resolve => setTimeout(resolve, ms));
}

async function killProcess(proc) {
  if (!proc || proc.exitCode !== null || proc.signalCode !== null) return;
  const exited = new Promise(resolve => proc.once('exit', resolve));
  try {
    proc.kill('SIGTERM');
  } catch {
    return;
  }
  const timer = setTimeout(() => {
    if (proc.exitCode === null && proc.signalCode === null) {
      try { proc.kill('SIGKILL'); } catch { /* already gone */ }
    }
  }, 3000);
  timer.unref();
  await exited;
  clearTimeout(timer);
}

/**
 * Minimal Unix-socket mock daemon speaking the JSON-lines protocol in `crates/ictl-protocol`.
 * Lets the viewer serve real 200 responses for `/api/*` and `/stream` without touching usbmuxd or devices.
 */
function startMockDaemon(socketPath) {
  const sockets = new Set();
  const timers = new Set();
  const pressedButtons = [];

  const snapshot = {
    version: null,
    pid: process.pid,
    maintenance: null,
    state: { state: 'ready' },
    device: MOCK_DEVICE,
    devices: [MOCK_DEVICE],
    display: { id: 'smoke-view-1', width: 320, height: 640, rotation: 0 },
    pinned: null,
    pinned_by_env: false,
    persisted: {
      team_id: null,
      wda_bundle: null,
      profile_expires: null,
      screen: { width: 320, height: 640 },
      screen_udid: MOCK_DEVICE.udid,
      screens: { [MOCK_DEVICE.udid]: { width: 320, height: 640 } },
      last_resign: null,
      selected_udid: null,
    },
    device_profile: 'NotChecked',
    native: false,
    auto_unlock: false,
    unlock_disabled: false,
    notify_configured: false,
    log_tail: [],
  };

  function writeJson(sock, obj) {
    if (!sock.destroyed) sock.write(JSON.stringify(obj) + '\n');
  }

  function handleMjpegStream(sock) {
    writeJson(sock, { kind: 'text', text: 'streaming' });
    const part = Buffer.concat([
      Buffer.from(`--ictl\r\nContent-Type: image/jpeg\r\nContent-Length: ${SAMPLE_JPEG.length}\r\n\r\n`),
      SAMPLE_JPEG,
      Buffer.from('\r\n'),
    ]);
    if (sock.destroyed) return;
    sock.write('HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=ictl\r\n\r\n');
    sock.write(part);
    const interval = setInterval(() => {
      if (sock.destroyed) {
        clearInterval(interval);
        timers.delete(interval);
        return;
      }
      sock.write(part);
    }, 2500);
    interval.unref();
    timers.add(interval);
    sock.once('close', () => {
      clearInterval(interval);
      timers.delete(interval);
    });
  }

  const server = net.createServer(sock => {
    sockets.add(sock);
    sock.on('error', () => { /* client closed */ });
    sock.once('close', () => sockets.delete(sock));

    let buf = '';
    let streaming = false;
    sock.on('data', chunk => {
      if (streaming) return;
      buf += chunk.toString('utf8');
      let nl;
      while ((nl = buf.indexOf('\n')) !== -1) {
        const line = buf.slice(0, nl).trim();
        buf = buf.slice(nl + 1);
        if (!line) continue;
        let req;
        try {
          req = JSON.parse(line);
        } catch {
          writeJson(sock, { kind: 'error', message: 'malformed json' });
          continue;
        }
        switch (req.op) {
          case 'daemon':
            writeJson(sock, { kind: 'text', text: JSON.stringify(snapshot) });
            break;
          case 'status':
            writeJson(sock, {
              kind: 'text',
              text: 'ok',
              data: {
                type: 'status',
                wda_ready: true,
                locked: false,
                possibly_locked: false,
                foreground: 'com.apple.springboard',
                width: 320,
                height: 640,
                native: false,
              },
            });
            break;
          case 'task_list':
            writeJson(sock, { kind: 'text', text: '[]' });
            break;
          case 'list_apps':
            writeJson(sock, { kind: 'text', text: 'ok', data: { type: 'apps', apps: MOCK_APPS } });
            break;
          case 'list_elements':
            writeJson(sock, {
              kind: 'text',
              text: 'ok',
              data: { type: 'elements', elements: MOCK_ELEMENTS, matched_count: MOCK_ELEMENTS.length, truncated: false },
            });
            break;
          case 'alert_check':
            writeJson(sock, { kind: 'text', text: 'ok', data: { type: 'alert', text: null } });
            break;
          case 'press_button':
            pressedButtons.push(req.button);
            writeJson(sock, { kind: 'text', text: `pressed ${req.button}` });
            break;
          case 'mjpeg_stream':
            streaming = true;
            handleMjpegStream(sock);
            return;
          default:
            writeJson(sock, { kind: 'error', message: `unsupported mock op: ${req.op}` });
        }
      }
    });
  });

  return new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(socketPath, () => {
      resolve({
        pressedButtons,
        async close() {
          for (const t of timers) clearInterval(t);
          timers.clear();
          for (const s of sockets) s.destroy();
          sockets.clear();
          await new Promise(done => server.close(() => done()));
        },
      });
    });
  });
}

async function startViewer(binary, dataDir, socketPath, tmpDir) {
  const home = path.join(tmpDir, 'home');
  const configDir = path.join(tmpDir, 'config');
  const xdgData = path.join(tmpDir, 'xdg-data');
  const runDir = path.join(tmpDir, 'run');
  for (const d of [home, configDir, xdgData, runDir]) {
    fs.mkdirSync(d, { recursive: true, mode: 0o700 });
  }
  const env = cleanEnv({
    HOME: home,
    XDG_CONFIG_HOME: configDir,
    XDG_DATA_HOME: xdgData,
    XDG_RUNTIME_DIR: runDir,
    IPHONE_CONTROL_DATA_DIR: dataDir,
    IPHONE_CONTROL_SOCKET: socketPath,
    IPHONE_CONTROL_LANG: 'zh',
    IPHONE_CONTROL_VIEWER_CODEC: 'mjpeg',
  });

  const proc = spawn(
    binary,
    ['--data-dir', dataDir, '--socket', socketPath, 'viewer', '--port', '0', '--tls', 'off'],
    { env, stdio: ['ignore', 'pipe', 'pipe'] },
  );

  let stdout = '';
  let stderr = '';
  const port = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new Error(`Timed out waiting for viewer to listen.\nstdout:\n${stdout}\nstderr:\n${stderr}`));
    }, 15000);

    const onExit = (code, signal) => {
      clearTimeout(timer);
      reject(new Error(`Viewer exited early (code=${code}, signal=${signal}).\nstdout:\n${stdout}\nstderr:\n${stderr}`));
    };
    proc.once('exit', onExit);
    proc.once('error', err => {
      clearTimeout(timer);
      reject(err);
    });

    proc.stderr.on('data', chunk => {
      stderr += chunk.toString('utf8');
      if (stderr.length > 32768) stderr = stderr.slice(-32768);
    });
    proc.stdout.on('data', chunk => {
      stdout += chunk.toString('utf8');
      if (stdout.length > 32768) stdout = stdout.slice(-32768);
      const m = stdout.match(/http:\/\/127\.0\.0\.1:(\d+)\//);
      if (m) {
        clearTimeout(timer);
        proc.off('exit', onExit);
        resolve(Number(m[1]));
      }
    });
  });

  const tokensPath = path.join(dataDir, 'viewer-tokens.json');
  const tokens = JSON.parse(fs.readFileSync(tokensPath, 'utf8'));
  const owner = tokens.find(t => t.name === 'owner');
  assert.ok(owner && typeof owner.token === 'string' && owner.token.length >= 16, 'owner token should be generated');
  return { proc, port, token: owner.token, getLogs: () => ({ stdout, stderr }) };
}

async function startChrome(chromePath, tmpDir) {
  const profileDir = path.join(tmpDir, 'chrome-profile');
  const chromeHome = path.join(tmpDir, 'chrome-home');
  fs.mkdirSync(profileDir, { recursive: true });
  fs.mkdirSync(chromeHome, { recursive: true });

  const env = cleanEnv({ HOME: chromeHome });
  const args = [
    '--headless=new',
    '--no-sandbox',
    '--disable-gpu',
    '--disable-dev-shm-usage',
    '--disable-extensions',
    '--disable-background-networking',
    '--no-first-run',
    '--no-default-browser-check',
    '--no-proxy-server',
    '--window-size=1440,900',
    `--user-data-dir=${profileDir}`,
    '--remote-debugging-port=0',
    'about:blank',
  ];

  const proc = spawn(chromePath, args, { env, stdio: ['ignore', 'pipe', 'pipe'] });
  let stderr = '';
  const wsUrl = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      reject(new Error(`Timed out waiting for Chrome DevTools endpoint.\nstderr:\n${stderr}`));
    }, 15000);

    const onExit = (code, signal) => {
      clearTimeout(timer);
      reject(new Error(`Chrome exited early (code=${code}, signal=${signal}).\nstderr:\n${stderr}`));
    };
    proc.once('exit', onExit);
    proc.once('error', err => {
      clearTimeout(timer);
      reject(err);
    });

    proc.stderr.on('data', chunk => {
      stderr += chunk.toString('utf8');
      if (stderr.length > 32768) stderr = stderr.slice(-32768);
      const m = stderr.match(/DevTools listening on (ws:\/\/\S+)/);
      if (m) {
        clearTimeout(timer);
        proc.off('exit', onExit);
        resolve(m[1]);
      }
    });
  });

  return { proc, wsUrl, getStderr: () => stderr };
}

class CdpClient {
  constructor(ws) {
    this.ws = ws;
    this.nextId = 1;
    this.pending = new Map();
    this.listeners = new Set();

    ws.addEventListener('message', ev => {
      const msg = JSON.parse(typeof ev.data === 'string' ? ev.data : Buffer.from(ev.data).toString('utf8'));
      if (msg.id !== undefined) {
        const entry = this.pending.get(msg.id);
        if (!entry) return;
        this.pending.delete(msg.id);
        clearTimeout(entry.timer);
        if (msg.error) {
          entry.reject(new Error(`CDP ${entry.method} failed: ${msg.error.message} (${msg.error.code})`));
        } else {
          entry.resolve(msg.result || {});
        }
      } else if (msg.method) {
        for (const fn of this.listeners) fn(msg);
      }
    });

    const failAll = err => {
      for (const entry of this.pending.values()) {
        clearTimeout(entry.timer);
        entry.reject(err);
      }
      this.pending.clear();
    };
    ws.addEventListener('error', () => failAll(new Error('CDP WebSocket error')));
    ws.addEventListener('close', () => failAll(new Error('CDP WebSocket closed')));
  }

  static connect(wsUrl) {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(wsUrl);
      const timer = setTimeout(() => {
        try { ws.close(); } catch { /* ignore */ }
        reject(new Error(`Timed out connecting to CDP WebSocket: ${wsUrl}`));
      }, 10000);
      ws.addEventListener('open', () => {
        clearTimeout(timer);
        resolve(new CdpClient(ws));
      }, { once: true });
      ws.addEventListener('error', () => {
        clearTimeout(timer);
        reject(new Error(`Failed to connect to CDP WebSocket: ${wsUrl}`));
      }, { once: true });
    });
  }

  send(method, params = {}, sessionId = undefined, timeoutMs = 15000) {
    return new Promise((resolve, reject) => {
      const id = this.nextId++;
      const payload = sessionId ? { id, method, params, sessionId } : { id, method, params };
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`CDP timeout after ${timeoutMs}ms: ${method}`));
      }, timeoutMs);
      this.pending.set(id, { resolve, reject, timer, method });
      this.ws.send(JSON.stringify(payload));
    });
  }

  onEvent(listener) {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  }

  close() {
    try { this.ws.close(); } catch { /* ignore */ }
  }
}

class PageSession {
  constructor(cdp, browserContextId, targetId, sessionId) {
    this.cdp = cdp;
    this.browserContextId = browserContextId;
    this.targetId = targetId;
    this.sessionId = sessionId;

    this.pageErrors = [];
    this.consoleErrors = [];
    this.cspViolations = [];
    this.httpResponses = [];
    this.loadingFailures = [];
    this.loadWaiters = [];

    this.off = cdp.onEvent(msg => {
      if (msg.sessionId !== this.sessionId) return;
      const p = msg.params || {};
      switch (msg.method) {
        case 'Page.loadEventFired': {
          const waiters = this.loadWaiters.splice(0);
          for (const resolve of waiters) resolve();
          break;
        }
        case 'Runtime.exceptionThrown': {
          const d = p.exceptionDetails || {};
          const text = d.exception?.description || d.text || 'uncaught exception';
          this.pageErrors.push(`${text}${d.url ? ` @ ${redactUrl(d.url)}:${d.lineNumber}` : ''}`);
          break;
        }
        case 'Runtime.consoleAPICalled': {
          if (p.type === 'error') {
            const text = (p.args || []).map(a => a.value ?? a.description ?? a.type).join(' ');
            this.consoleErrors.push(text);
          }
          break;
        }
        case 'Log.entryAdded': {
          const e = p.entry || {};
          const isCsp = e.source === 'security' || /Content Security Policy|Refused to /i.test(e.text || '');
          if (isCsp) {
            this.cspViolations.push(`${e.source}:${e.level}: ${e.text}`);
          } else if (e.level === 'error' && e.source !== 'network') {
            this.consoleErrors.push(`[log:${e.source}] ${e.text}`);
          }
          break;
        }
        case 'Network.responseReceived': {
          const r = p.response || {};
          const headers = {};
          for (const [k, v] of Object.entries(r.headers || {})) {
            headers[k.toLowerCase()] = v;
          }
          this.httpResponses.push({
            type: p.type,
            url: redactUrl(r.url),
            status: r.status,
            headers,
          });
          break;
        }
        case 'Network.loadingFailed': {
          if (!p.canceled) {
            this.loadingFailures.push({ type: p.type, errorText: p.errorText, blockedReason: p.blockedReason });
          }
          break;
        }
      }
    });
  }

  static async create(cdp) {
    const { browserContextId } = await cdp.send('Target.createBrowserContext');
    const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank', browserContextId });
    const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
    const page = new PageSession(cdp, browserContextId, targetId, sessionId);

    await page.send('Page.enable');
    await page.send('Runtime.enable');
    await page.send('Log.enable');
    await page.send('Network.enable');
    await page.send('Emulation.setDeviceMetricsOverride', {
      width: 1440,
      height: 900,
      deviceScaleFactor: 1,
      mobile: false,
    });
    await page.send('Page.addScriptToEvaluateOnNewDocument', {
      source: `(() => {
        window.__smokeCspViolations = [];
        document.addEventListener('securitypolicyviolation', ev => {
          window.__smokeCspViolations.push({
            directive: ev.effectiveDirective || ev.violatedDirective,
            blockedURI: ev.blockedURI,
          });
        });
      })();`,
    });
    return page;
  }

  send(method, params = {}, timeoutMs = 15000) {
    return this.cdp.send(method, params, this.sessionId, timeoutMs);
  }

  waitForLoad(timeoutMs = 15000) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        const idx = this.loadWaiters.indexOf(done);
        if (idx >= 0) this.loadWaiters.splice(idx, 1);
        reject(new Error(`Timed out waiting for Page.loadEventFired (${timeoutMs}ms)`));
      }, timeoutMs);
      const done = () => {
        clearTimeout(timer);
        resolve();
      };
      this.loadWaiters.push(done);
    });
  }

  async navigate(url) {
    const loaded = this.waitForLoad();
    const res = await this.send('Page.navigate', { url });
    if (res.errorText) {
      throw new Error(`Navigation failed for ${redactUrl(url)}: ${res.errorText}`);
    }
    await loaded;
  }

  async evalJs(expression) {
    const res = await this.send('Runtime.evaluate', {
      expression,
      awaitPromise: true,
      returnByValue: true,
    });
    if (res.exceptionDetails) {
      const msg = res.exceptionDetails.exception?.description || res.exceptionDetails.text || 'Runtime.evaluate error';
      throw new Error(`JS evaluation failed: ${msg}`);
    }
    return res.result?.value;
  }

  async waitFor(expression, description, timeoutMs = 10000) {
    const deadline = Date.now() + timeoutMs;
    let lastValue;
    while (Date.now() < deadline) {
      lastValue = await this.evalJs(expression);
      if (lastValue) return lastValue;
      await sleep(50);
    }
    throw new Error(`Timed out waiting for ${description} (last value: ${JSON.stringify(lastValue)})`);
  }

  async click(selector) {
    const rect = await this.waitFor(`(() => {
      const el = document.querySelector(${JSON.stringify(selector)});
      if (!el || el.disabled) return null;
      el.scrollIntoView({ block: 'center', inline: 'center' });
      const r = el.getBoundingClientRect();
      return r.width > 0 && r.height > 0 ? { x: r.left + r.width / 2, y: r.top + r.height / 2 } : null;
    })()`, `clickable element ${selector}`);
    // Wait one animation frame so popover/dialog positioning settles before dispatching pointer events.
    await this.evalJs('new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))');
    const finalRect = await this.evalJs(`(() => {
      const el = document.querySelector(${JSON.stringify(selector)});
      const r = el.getBoundingClientRect();
      return { x: r.left + r.width / 2, y: r.top + r.height / 2 };
    })()`);
    const { x, y } = finalRect || rect;
    await this.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y });
    await this.send('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'left', clickCount: 1 });
    await this.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'left', clickCount: 1 });
  }

  async fillInput(selector, value) {
    await this.click(selector);
    await this.evalJs(`(() => {
      const el = document.querySelector(${JSON.stringify(selector)});
      el.focus();
      el.value = '';
      el.dispatchEvent(new Event('input', { bubbles: true }));
    })()`);
    if (value) {
      await this.send('Input.insertText', { text: value });
    }
  }

  async getDomCspViolations() {
    try {
      return (await this.evalJs('window.__smokeCspViolations || []')) || [];
    } catch {
      return [];
    }
  }

  clearNetworkRecords() {
    this.httpResponses.length = 0;
    this.loadingFailures.length = 0;
  }

  async assertClean({ allowHttpError = false } = {}) {
    const domCsp = await this.getDomCspViolations();
    assert.deepEqual(this.pageErrors, [], `Unexpected pageerror(s): ${this.pageErrors.join(' | ')}`);
    assert.deepEqual(this.consoleErrors, [], `Unexpected console.error(s): ${this.consoleErrors.join(' | ')}`);
    assert.deepEqual(this.cspViolations, [], `Unexpected CSP log violation(s): ${this.cspViolations.join(' | ')}`);
    assert.deepEqual(domCsp, [], `Unexpected DOM securitypolicyviolation(s): ${JSON.stringify(domCsp)}`);
    assert.deepEqual(this.loadingFailures, [], `Unexpected loading failure(s): ${JSON.stringify(this.loadingFailures)}`);
    if (!allowHttpError) {
      const bad = this.httpResponses.filter(r => r.status >= 400);
      assert.deepEqual(bad, [], `Unexpected HTTP >= 400 responses: ${JSON.stringify(bad)}`);
    }
  }

  async close() {
    this.off();
    try { await this.cdp.send('Target.closeTarget', { targetId: this.targetId }); } catch { /* ignore */ }
    try { await this.cdp.send('Target.disposeBrowserContext', { browserContextId: this.browserContextId }); } catch { /* ignore */ }
  }
}

async function smoke_unauthenticated_login_page_and_wrong_token(page, baseUrl) {
  page.clearNetworkRecords();
  await page.navigate(`${baseUrl}/`);

  const docResp = page.httpResponses.find(r => r.type === 'Document');
  assert.ok(docResp, 'Expected Document response for /');
  assert.equal(docResp.status, 401, 'Unauthenticated GET / must return 401');
  assert.ok(docResp.headers['content-security-policy'], 'Login page must include Content-Security-Policy');
  assert.equal(
    docResp.headers['referrer-policy'],
    'same-origin',
    'Login page must use Referrer-Policy: same-origin so browsers send a real Origin on form POST',
  );

  const loginDom = await page.evalJs(`({
    hasForm: !!document.querySelector('form#form[action="/login"]'),
    hasTokenInput: !!document.querySelector('input#token[name="token"]'),
    hasSubmit: !!document.querySelector('button#submit[type="submit"]'),
    hasConsoleShell: !!document.querySelector('#shell'),
  })`);
  assert.deepEqual(loginDom, {
    hasForm: true,
    hasTokenInput: true,
    hasSubmit: true,
    hasConsoleShell: false,
  });
  await page.assertClean({ allowHttpError: true });

  // Submitting a wrong token returns 401 with an error box and zero JS/CSP errors.
  page.clearNetworkRecords();
  await page.fillInput('#token', 'wrong-token-value');
  const wrongLoaded = page.waitForLoad();
  await page.click('#submit');
  await wrongLoaded;

  const wrongResp = page.httpResponses.find(r => r.type === 'Document');
  assert.equal(wrongResp?.status, 401, 'Wrong token submission must return 401');
  const hasErrBanner = await page.evalJs('!!document.querySelector(".err")');
  assert.equal(hasErrBanner, true, 'Wrong token must render the .err message');
  await page.assertClean({ allowHttpError: true });
}

async function smoke_form_login_and_cookie_session(page, baseUrl, port, token) {
  page.clearNetworkRecords();
  await page.fillInput('#token', token);
  const loggedInLoad = page.waitForLoad();
  await page.click('#submit');
  await loggedInLoad;

  const docResp = page.httpResponses.find(r => r.type === 'Document');
  assert.ok(docResp, 'Expected Document response after login redirect');
  assert.equal(docResp.status, 200, 'Authenticated GET / must return 200');
  assert.ok(docResp.headers['content-security-policy'], 'Control page must include Content-Security-Policy');
  assert.equal(docResp.headers['referrer-policy'], 'no-referrer', 'Control page must keep Referrer-Policy: no-referrer');
  assert.equal(docResp.headers['content-encoding'], 'gzip', 'Control page should be served gzipped');

  const loc = await page.evalJs('({ pathname: location.pathname, search: location.search })');
  assert.deepEqual(loc, { pathname: '/', search: '' }, 'Post-login URL must be / with no query token');

  const { cookies } = await page.send('Network.getCookies', { urls: [`${baseUrl}/`] });
  const sessionCookie = cookies.find(c => c.name === `ictl_viewer_${port}`);
  assert.ok(sessionCookie, `Session cookie ictl_viewer_${port} must be set`);
  assert.equal(sessionCookie.httpOnly, true, 'Session cookie must be HttpOnly');
  assert.equal(sessionCookie.sameSite, 'Lax', 'Session cookie must be SameSite=Lax');
  assert.notEqual(sessionCookie.value, token, 'Session cookie value must be HMAC-derived, never the raw token');

  // Wait for live mode (/api/info, /api/daemon, /api/status) and /stream MJPEG frame using observable DOM only.
  await page.waitFor(
    'document.getElementById("demo-note").hidden === true && document.getElementById("scope-pick").hidden === false && !document.documentElement.hasAttribute("data-readonly")',
    'control page to enter authenticated live operate mode',
  );
  await page.waitFor(
    'document.getElementById("live")?.naturalWidth === 16 && document.getElementById("live")?.naturalHeight === 32 && document.getElementById("stage-msg").hidden === true && !document.getElementById("zoom-val").disabled',
    'live MJPEG picture frame to load',
  );
  await page.waitFor(
    'document.getElementById("device-name")?.textContent?.includes("Smoke iPhone") && document.querySelector("#nav-devices")?.textContent?.includes("Smoke iPhone")',
    'device picker and status panel to show Smoke iPhone',
  );

  // Confirm /api/info actually returned 200 over the network.
  assert.ok(
    page.httpResponses.some(r => r.url.includes('/api/info') && r.status === 200),
    'Expected 200 response for /api/info',
  );
  await page.assertClean();
}

async function smoke_viewer_core_dom_interactions(page, mockDaemon) {
  // 1. Device panel collapse and reopen (3-column wide desktop viewport 1440x900).
  assert.equal(await page.evalJs('document.getElementById("shell").dataset.devHidden'), 'false');
  await page.click('#dev-close');
  await page.waitFor(
    'document.getElementById("shell").dataset.devHidden === "true" && document.getElementById("pane-dev").inert === true && document.getElementById("dev-toggle").getAttribute("aria-expanded") === "false"',
    'device panel to hide',
  );
  await page.click('#dev-toggle');
  await page.waitFor(
    'document.getElementById("shell").dataset.devHidden === "false" && document.getElementById("pane-dev").inert === false && document.getElementById("dev-toggle").getAttribute("aria-expanded") === "true"',
    'device panel to reopen',
  );

  // 2. Device panel tabs: Apps (with live /api/apps + search filter), Elements, back to Status.
  await page.click('#tab-apps');
  await page.waitFor(
    '(() => { const t = document.getElementById("apps").textContent; return document.getElementById("tab-apps").getAttribute("aria-selected") === "true" && !document.getElementById("tp-apps").hidden && t.includes("Safari") && t.includes("Settings") && t.includes("Clock"); })()',
    'Apps tab to render mock installed apps',
  );
  const appsText = await page.evalJs('document.getElementById("apps").textContent');
  assert.ok(appsText.includes('Safari') && appsText.includes('Settings') && appsText.includes('Clock'), `Unexpected apps list: ${appsText}`);

  await page.fillInput('#app-q', 'safari');
  await page.waitFor(
    '(() => { const t = document.getElementById("apps").textContent; return t.includes("Safari") && !t.includes("Clock"); })()',
    'app search filter to narrow to Safari',
  );
  await page.fillInput('#app-q', '');

  await page.click('#tab-elements');
  await page.waitFor(
    'document.getElementById("tab-elements").getAttribute("aria-selected") === "true" && !document.getElementById("tp-elements").hidden',
    'Elements tab to open',
  );
  await page.click('#el-btn');
  await page.waitFor(
    'document.getElementById("elements").textContent.includes("Smoke Home")',
    'Elements tab to render mock elements',
  );
  await page.click('#tab-status');
  await page.waitFor('!document.getElementById("tp-status").hidden', 'Status tab to become active');

  // 3. Zoom controls: 100%, zoom-in (110%), zoom-out (100%), zoom-fit.
  assert.equal(await page.evalJs('document.getElementById("zoom-fit").getAttribute("aria-pressed")'), 'true');
  await page.click('#zoom-val');
  await page.waitFor(
    'document.getElementById("frame").dataset.zoom === "fixed" && document.getElementById("zoom-val").textContent === "100%" && document.getElementById("zoom-fit").getAttribute("aria-pressed") === "false"',
    'zoom-actual to switch to 100%',
  );
  await page.click('#zoom-in');
  await page.waitFor('document.getElementById("zoom-val").textContent === "110%"', 'zoom-in to step to 110%');
  await page.click('#zoom-out');
  await page.waitFor('document.getElementById("zoom-val").textContent === "100%"', 'zoom-out to return to 100%');
  await page.click('#zoom-fit');
  await page.waitFor(
    'document.getElementById("zoom-fit").getAttribute("aria-pressed") === "true" && !("zoom" in document.getElementById("frame").dataset)',
    'zoom-fit to restore fit mode',
  );

  // 4. App scope popover (#scope-btn / #scope-menu): open, pick Safari, clear, close.
  await page.click('#scope-btn');
  await page.waitFor(
    'document.getElementById("scope-menu").matches(":popover-open") && document.getElementById("scope-btn").getAttribute("aria-expanded") === "true"',
    'scope popover to open',
  );
  await page.waitFor(
    '!!document.querySelector("#scope-list input[value=\\"com.apple.mobilesafari\\"]")',
    'scope popover to load app checkboxes',
  );
  await page.click('#scope-list input[value="com.apple.mobilesafari"]');
  await page.waitFor(
    'document.getElementById("scope-btn").hasAttribute("data-scoped") && document.getElementById("scope-name").textContent.includes("Safari")',
    'scope selection to update trigger button',
  );
  await page.click('#scope-clear');
  await page.waitFor(
    '!document.getElementById("scope-btn").hasAttribute("data-scoped")',
    'scope clear button to reset scope',
  );
  await page.click('#scope-done');
  await page.waitFor(
    '!document.getElementById("scope-menu").matches(":popover-open")',
    'scope popover to close',
  );

  // 5. Hardware button action (`POST /api/button` via fetch under `Referrer-Policy: no-referrer` sends real Origin).
  const beforeButtons = mockDaemon.pressedButtons.length;
  await page.click('#dock button[data-key="home"]');
  const buttonDeadline = Date.now() + 5000;
  while (mockDaemon.pressedButtons.length <= beforeButtons && Date.now() < buttonDeadline) {
    await sleep(30);
  }
  assert.equal(
    mockDaemon.pressedButtons.at(-1),
    'home',
    'Clicking Home in the dock must reach the daemon via authenticated POST /api/button',
  );

  // 6. About & updates popover (#about-updates / #updates-dialog).
  await page.click('#about-updates');
  await page.waitFor(
    'document.getElementById("updates-dialog").matches(":popover-open") && document.querySelectorAll("#update-facts dt").length > 0',
    'updates dialog to open and populate version facts',
  );
  await page.click('#update-close');
  await page.waitFor(
    '!document.getElementById("updates-dialog").matches(":popover-open")',
    'updates dialog to close',
  );

  // 7. Settings modal (#appearance): theme -> dark, text-size -> large, low-bandwidth -> on, language -> en (reload).
  const zhTitle = await page.evalJs('document.title');
  assert.equal(await page.evalJs('document.documentElement.lang'), 'zh-CN');

  await page.click('#appearance-btn');
  await page.waitFor('document.getElementById("appearance").open === true', 'settings dialog to open');

  await page.click('#theme');
  await page.waitFor('document.getElementById("theme-options").matches(":popover-open")', 'theme picker popover to open');
  await page.click('#theme-options [data-value="dark"]');
  await page.waitFor('document.documentElement.dataset.scheme === "dark"', 'theme to switch to dark');

  await page.click('#text-size');
  await page.waitFor('document.getElementById("text-size-options").matches(":popover-open")', 'text-size picker popover to open');
  await page.click('#text-size-options [data-value="large"]');
  await page.waitFor('document.documentElement.dataset.textSize === "large"', 'text size to switch to large');

  await page.click('#low-bandwidth');
  await page.waitFor(
    'document.getElementById("low-bandwidth").checked === true && localStorage.getItem("ictl.lowBandwidth") === "1"',
    'low-bandwidth toggle to turn on',
  );

  // Language switch persists `ictl.lang` and triggers a full `location.reload()`.
  await page.click('#language');
  await page.waitFor('document.getElementById("language-options").matches(":popover-open")', 'language picker popover to open');
  const reloaded = page.waitForLoad();
  await page.click('#language-options [data-value="en"]');
  await reloaded;

  await page.waitFor(
    'document.documentElement.lang === "en" && document.getElementById("demo-note").hidden === true && document.getElementById("live")?.naturalWidth === 16 && document.getElementById("stage-msg").hidden === true',
    'page to reload in English and reconnect live stream',
  );
  const enState = await page.evalJs(`({
    lang: document.documentElement.lang,
    title: document.title,
    scheme: document.documentElement.dataset.scheme,
    textSize: document.documentElement.dataset.textSize,
    lowBandwidth: document.getElementById("low-bandwidth").checked,
    settingsBtn: document.getElementById("appearance-btn").textContent.trim(),
  })`);
  assert.equal(enState.lang, 'en');
  assert.notEqual(enState.title, zhTitle, 'English title should differ from Chinese title');
  assert.equal(enState.scheme, 'dark', 'Dark theme must persist across language reload');
  assert.equal(enState.textSize, 'large', 'Large text size must persist across language reload');
  assert.equal(enState.lowBandwidth, true, 'Low-bandwidth toggle must persist across language reload');
  assert.equal(enState.settingsBtn, 'Settings', 'Settings button label should be translated to English');

  await page.assertClean();
}

async function smoke_token_link_login_strips_query_and_loads_console(cdp, baseUrl, port, token) {
  const page = await PageSession.create(cdp);
  try {
    await page.navigate(`${baseUrl}/?token=${encodeURIComponent(token)}`);
    const loc = await page.evalJs('({ pathname: location.pathname, search: location.search })');
    assert.deepEqual(loc, { pathname: '/', search: '' }, 'Token query login must redirect to clean /');

    const { cookies } = await page.send('Network.getCookies', { urls: [`${baseUrl}/`] });
    const sessionCookie = cookies.find(c => c.name === `ictl_viewer_${port}`);
    assert.ok(sessionCookie && sessionCookie.httpOnly, 'Token query login must set HttpOnly session cookie');

    await page.waitFor(
      'document.getElementById("demo-note").hidden === true && document.getElementById("live")?.naturalWidth === 16 && document.getElementById("stage-msg").hidden === true',
      'token-link login session to load live console',
    );
    await page.assertClean();
  } finally {
    await page.close();
  }
}

async function main() {
  if (typeof globalThis.WebSocket !== 'function') {
    console.error(`ERROR: Node ${process.version} lacks built-in global WebSocket (Node 22+ is required).`);
    process.exit(1);
  }

  let binary;
  let chromePath;
  try {
    const parsed = parseArgs(process.argv.slice(2));
    binary = parsed.binary;
    chromePath = findChrome(parsed.explicitChrome);
  } catch (err) {
    console.error(`ERROR: ${err.message}`);
    process.exit(1);
  }
  if (!chromePath) {
    const msg = 'No Chrome or Chromium executable found. Install google-chrome/chromium or pass --chrome / CHROME_BIN.';
    if (IS_CI) {
      console.error(`ERROR (CI): ${msg}`);
      process.exit(1);
    }
    console.log(`SKIP: ${msg}`);
    process.exit(0);
  }

  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'ictl-smoke-'));
  const dataDir = path.join(tmpDir, 'data');
  const socketPath = path.join(tmpDir, 'ctl.sock');
  fs.mkdirSync(dataDir, { recursive: true });

  let mockDaemon = null;
  let viewer = null;
  let chrome = null;
  let cdp = null;
  let page = null;

  const cleanup = async () => {
    if (page) { await page.close(); page = null; }
    if (cdp) { cdp.close(); cdp = null; }
    if (chrome) { await killProcess(chrome.proc); chrome = null; }
    if (viewer) { await killProcess(viewer.proc); viewer = null; }
    if (mockDaemon) { await mockDaemon.close(); mockDaemon = null; }
    fs.rmSync(tmpDir, { recursive: true, force: true });
  };

  const onSignal = sig => {
    cleanup().finally(() => process.exit(128 + (sig === 'SIGINT' ? 2 : 15)));
  };
  process.once('SIGINT', onSignal);
  process.once('SIGTERM', onSignal);

  const startedAt = Date.now();
  try {
    mockDaemon = await startMockDaemon(socketPath);
    viewer = await startViewer(binary, dataDir, socketPath, tmpDir);
    chrome = await startChrome(chromePath, tmpDir);
    cdp = await CdpClient.connect(chrome.wsUrl);

    const baseUrl = `http://127.0.0.1:${viewer.port}`;
    page = await PageSession.create(cdp);

    const steps = [
      ['unauthenticated login page & wrong token', () => smoke_unauthenticated_login_page_and_wrong_token(page, baseUrl)],
      ['form login, HttpOnly cookie & live console', () => smoke_form_login_and_cookie_session(page, baseUrl, viewer.port, viewer.token)],
      ['core DOM interactions (panel, tabs, zoom, scope, dock POST, updates, appearance & i18n reload)', () => smoke_viewer_core_dom_interactions(page, mockDaemon)],
      ['token-link login strips query & loads console', () => smoke_token_link_login_strips_query_and_loads_console(cdp, baseUrl, viewer.port, viewer.token)],
    ];

    for (const [label, fn] of steps) {
      const t0 = Date.now();
      await fn();
      console.log(`ok - ${label} (${Date.now() - t0}ms)`);
    }
    console.log(`PASS: all ${steps.length} viewer headless Chrome smoke checks passed in ${Date.now() - startedAt}ms (${path.basename(chromePath)})`);
  } catch (err) {
    console.error('FAIL:', err && err.stack ? err.stack : err);
    if (viewer) {
      const logs = viewer.getLogs();
      if (logs.stderr.trim()) console.error('--- viewer stderr ---\n' + logs.stderr.trim());
    }
    if (chrome) {
      const errOut = chrome.getStderr();
      if (errOut.trim()) console.error('--- chrome stderr ---\n' + errOut.trim().slice(-2000));
    }
    process.exitCode = 1;
  } finally {
    process.off('SIGINT', onSignal);
    process.off('SIGTERM', onSignal);
    await cleanup();
  }
}

await main();
