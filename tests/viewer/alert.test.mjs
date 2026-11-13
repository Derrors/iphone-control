import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';

const html = readFileSync(new URL('../../apps/iphone-control/src/viewer.html', import.meta.url), 'utf8');

// The page's system-alert handlers, with the network and DOM replaced (same approach as actions.test.mjs)
function page({ device = true, task = null, alert = null } = {}) {
  const el = { hidden: true, innerHTML: '' };
  const calls = [];
  const context = {
    S: { mode: 'live', display: { id: 'view-1' } }, V: { device }, document: { hidden: false },
    SA: { busy: false, key: '', quietUntil: 0 }, Date, tr: String, icon: () => '', setTimeout: () => {},
    esc: s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c])),
    $: () => el, guarded: () => !!task, dims: () => ({ W: 430, H: 932 }),
    async api(path) { calls.push(path); return { alert }; },
    async act(path, body) { calls.push([path, body]); return { ok: true }; },
  };
  const names = ['pollAlert', 'renderSysAlert', 'tapSysAlert'];
  const fns = names.map(n => html.match(new RegExp(`^(?:async )?function ${n}\\([^]*?^}`, 'm'))[0]);
  return { el, calls, context, ...runInNewContext(fns.join('\n') + `\n({${names}})`, context) };
}

const prompt = {
  text: '允许“日历”使用你的位置？\n你的位置会用于出发时间提醒。', frame: { x: 80, y: 306, width: 270, height: 320 },
  buttons: [{ label: '精确位置：打开', x: 140, y: 405, width: 110 }, { label: '允许一次', x: 215, y: 520, width: 270 }, { label: '不允许', x: 215, y: 608, width: 270 }],
};

test('a system alert shows over the picture, its buttons tap where they are', async () => {
  const p = page({ alert: prompt });
  await p.pollAlert();
  assert.deepEqual(p.calls, ['/api/alert']);
  assert.equal(p.el.hidden, false);
  assert.match(p.el.innerHTML, /允许“日历”使用你的位置？/);
  assert.match(p.el.innerHTML, /不允许/);
  assert.doesNotMatch(p.el.innerHTML, /disabled/);
  // drawn where the real alert is, like it: title, message, the narrow toggle as a chip, the actions as full rows
  assert.match(p.el.innerHTML, /left:18\.60%;width:62\.79%;top:50\.00%/);
  assert.match(p.el.innerHTML, /class="sa-title">允许“日历”使用你的位置？</);
  assert.match(p.el.innerHTML, /class="sa-msg">你的位置会用于出发时间提醒。</);
  assert.match(p.el.innerHTML, /class="sa-chip"[^>]*>精确位置：打开</);
  assert.match(p.el.innerHTML, /class="sa-btn"[^>]*>不允许</);
  await p.tapSysAlert(2);
  // through JSON: the body was made inside the vm, with that realm's Object
  assert.deepEqual(JSON.parse(JSON.stringify(p.calls[1])), ['/api/tap', { x: 215, y: 608, view: 'view-1' }], 'by position: 不允许, not the first match for 允许');
  assert.equal(p.el.hidden, true, 'gone until the next look confirms');
});

test('only asked while the page shows the phone\'s own video', async () => {
  const p = page({ device: false, alert: prompt });
  await p.pollAlert();
  assert.deepEqual(p.calls, [], 'WDA\'s picture shows alerts itself');
  assert.equal(p.el.hidden, true);
});

test('during an AI task the buttons wait for a takeover', async () => {
  const p = page({ task: { status: 'running' }, alert: prompt });
  await p.pollAlert();
  assert.match(p.el.innerHTML, /disabled/);
  assert.match(p.el.innerHTML, /先接管/);
  await p.tapSysAlert(0);
  assert.deepEqual(p.calls, ['/api/alert'], 'no tap');
});

test('right after the user touched the phone, it doesn\'t ask (a touch would queue behind it)', async () => {
  const p = page({ alert: prompt });
  p.context.SA.quietUntil = Date.now() + 3000;
  await p.pollAlert();
  assert.deepEqual(p.calls, []);
});

test('no alert: nothing shown, and alert text can\'t inject markup', async () => {
  const none = page();
  await none.pollAlert();
  assert.equal(none.el.hidden, true);
  const evil = page({ alert: { text: '<img src=x onerror=alert(1)>', buttons: [{ label: '<b>x</b>', x: 1, y: 2 }] } });
  await evil.pollAlert();
  assert.doesNotMatch(evil.el.innerHTML, /<img|<b>/);
});
