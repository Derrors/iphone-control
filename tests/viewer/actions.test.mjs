import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';

const html = readFileSync(new URL('../../apps/iphone-control/src/viewer.html', import.meta.url), 'utf8');

// Exercise the embedded page's handlers with their network and DOM boundaries replaced.
function handlers(names, context) {
  const functions = names.map(name => {
    const match = html.match(new RegExp(`^(?:async )?function ${name}\\([^]*?^}`, 'm'));
    assert.ok(match, name);
    return match[0];
  });
  return runInNewContext(functions.join('\n') + '\n({' + names.join(', ') + '})', context);
}

const noop = () => {};
const common = { performance, tr: String, show: noop, renderFeed: noop, refreshStatusSoon: noop, feedEpoch: 1 };

test('two rejected stale downs keep live touch available for the next gesture', async () => {
  for (const reply of [new Error('HTTP 409'), { ok: false }, { ok: false, retry_as_gesture: false }]) {
    const S = { liveTouch: true };
    const T = { q: [], busy: false, fails: 0, gesture: 0 };
    const context = {
      ...common, S, T, W: { on: false }, down: null, loadInfo: noop, fingerUp: noop, hideHoverDot: noop,
      async api() { if (reply instanceof Error) throw reply; return reply; },
    };
    const { touchPump } = handlers(['touchPump'], context);
    for (let i = 0; i < 2; i++) {
      context.down = { legacy: false, gesture: T.gesture + 1 };
      T.q.push({ phase: 'down', device: 'phone', view: 'old', epoch: 1, gesture: ++T.gesture });
      await touchPump();
      assert.equal(T.fails, 0);
      assert.equal(context.down, null, 'do not replay a rejected press');
    }
    assert.equal(S.liveTouch, true);
    context.api = async (url, body) => { assert.equal(body.view, 'new'); return { ok: true }; };
    T.q.push({ phase: 'down', device: 'phone', view: 'new', epoch: 1, gesture: ++T.gesture });
    await touchPump();
    assert.equal(S.liveTouch, true);
  }
});

test('two confirmed HID connection failures still enable whole-gesture fallback', async () => {
  const S = { liveTouch: true };
  const T = { q: [], busy: false, fails: 0, gesture: 0 };
  const context = { ...common, S, T, down: null, fingerUp: noop, api: async () => ({ ok: false, retry_as_gesture: true }) };
  const { touchPump } = handlers(['touchPump'], context);
  for (let i = 0; i < 2; i++) {
    context.down = { legacy: false, gesture: T.gesture + 1 };
    T.q.push({ phase: 'down', device: 'phone', view: 'same', epoch: 1, gesture: ++T.gesture });
    await touchPump();
    assert.equal(context.down.legacy, true);
  }
  assert.equal(S.liveTouch, false);
});

test('a late transport error drops only moves from the original gesture', async () => {
  let rejectFirst;
  const pending = new Promise((_, reject) => { rejectFirst = reject; });
  const T = { q: [], busy: false, fails: 0, gesture: 1 };
  const sent = [];
  const context = {
    ...common, T, S: { liveTouch: true }, down: { gesture: 1 },
    async api(_url, event) {
      sent.push([event.phase, event.view]);
      if (sent.length === 1) await pending;
      return { ok: true };
    },
  };
  const { touchPump } = handlers(['touchPump'], context);
  const first = { device: 'phone', epoch: 1, gesture: 1, view: 'first' };
  const next = { ...first, gesture: 2, view: 'next' };
  T.q.push({ ...first, phase: 'down' });
  const draining = touchPump();
  T.q.push({ ...first, phase: 'move' }, { ...first, phase: 'up' }, { ...next, phase: 'down' }, { ...next, phase: 'move' });
  T.gesture = 2;
  context.down = { gesture: 2, legacy: false };
  rejectFirst(new Error('connection lost'));
  await draining;
  assert.deepEqual(sent, [['down', 'first'], ['up', 'first'], ['down', 'next'], ['move', 'next']]);
  assert.equal(context.down.legacy, false);
  assert.equal(T.fails, 0);
});

test('a named element can be tapped without a rendered frame and targets its device', async () => {
  const row = { label: 'Settings', type: 'Button', x: 5, y: 10, width: 20, height: 20 };
  const S = { mode: 'live', udid: 'phone', display: null, elements: [row, row], elState: 'ok' };
  const list = {}, count = {};
  const requests = [], marks = [];
  let geometry = null;
  const { tapElement, renderElements } = handlers(['renderElements', 'tapElement', 'api', 'act'], {
    ...common, S, LANG: 'en', dims: () => geometry, guarded: () => false, highlight: noop, esc: String,
    drawMark: (kind, who, point) => marks.push([kind, who, point.x, point.y]), round1: n => n,
    $: id => ({ '#el-q': { value: '' }, '#elements': list, '#el-count': count })[id],
    fetch: async (url, opts) => {
      requests.push([url, opts.headers['X-Ictl-Device'], JSON.parse(opts.body)]);
      return { ok: true, json: async () => ({ ok: true }) };
    },
  });
  renderElements();
  assert.match(list.innerHTML, /data-action="tap-el"/);
  assert.doesNotMatch(list.innerHTML, / disabled/);
  await tapElement(1);
  assert.deepEqual(requests, [['/api/tap_element', 'phone', { text: 'Settings', index: 1 }]]);
  assert.deepEqual(marks, []);
  geometry = { W: 390, H: 844 };
  await tapElement(0);
  assert.deepEqual(marks, [['tap', 'user', 15, 20]]);
});

test('hovering keeps the HID session warm at most every 20 seconds', () => {
  const sent = [];
  let now = 100000;
  const context = {
    S: { liveTouch: true, mode: 'live' }, T: { warm: 0 }, dims: () => ({ W: 1, H: 1 }), guarded: () => false,
    touchSend: phase => sent.push(phase), Date: { now: () => now },
  };
  const { touchWarm } = handlers(['touchWarm'], context);
  touchWarm(); touchWarm();
  now += 19999; touchWarm();
  assert.deepEqual(sent, ['prepare'], 'one prepare per 20 s while the pointer moves');
  now += 1; touchWarm();
  assert.deepEqual(sent, ['prepare', 'prepare']);
  context.S.liveTouch = false; now += 20000; touchWarm();
  assert.equal(sent.length, 2, 'no warm-up without live touch');
});

test('WDA guidance follows the selected device independently of its video', () => {
  const notice = {}, message = {}, install = {};
  const elements = { '#wda-start-notice': notice, '#wda-start-message': message, '#wda-install': install };
  const S = { udid: 'PHONE', stream: 'ok', daemon: { state: 'WdaStarting', device: { udid: 'phone' }, signed_in: true,
    wda_start_notice: { kind: 'trust_developer', message: 'Trust the developer' } } };
  const { renderWdaNotice } = handlers(['renderWdaNotice'], { S, $: id => elements[id], tr: String });
  renderWdaNotice();
  assert.equal(notice.hidden, false);
  assert.equal(message.textContent, 'Trust the developer');
  assert.equal(install.hidden, true, 'a trust error should not offer to re-sign');
  S.daemon.wda_start_notice = { kind: 'launch_refused', message: 'Check developer trust and signing' };
  renderWdaNotice();
  assert.equal(install.hidden, false, 'signed-in users can re-sign after a refused launch');
  assert.equal(install.textContent, '重新签名并安装 WDA');
  S.daemon.signed_in = false;
  renderWdaNotice();
  assert.equal(install.hidden, true);
  S.daemon.signed_in = true;
  S.signing = true;
  renderWdaNotice();
  assert.equal(install.disabled, true);
  S.signing = false;
  S.daemon.wda_start_notice = { kind: 'install_wda', message: 'Install WDA' };
  renderWdaNotice();
  assert.equal(install.hidden, false);
  assert.equal(install.textContent, '签名并安装 WDA');
  assert.equal(install.disabled, false);
  S.udid = 'other';
  renderWdaNotice();
  assert.equal(notice.hidden, true);
  S.udid = 'phone';
  S.daemon = { state: 'WdaStarting', device: { udid: 'phone' } }; // older daemon or an ordinary start
  renderWdaNotice();
  assert.equal(notice.hidden, true);
  S.daemon = { state: 'Ready', device: { udid: 'phone' }, wda_start_notice: null };
  renderWdaNotice();
  assert.equal(notice.hidden, true);
});
