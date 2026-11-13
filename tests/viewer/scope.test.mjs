import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';

const html = readFileSync(new URL('../../apps/iphone-control/src/viewer.html', import.meta.url), 'utf8');

// The page's handlers with the network and DOM replaced (same approach as actions.test.mjs)
function handlers(names, context) {
  const fns = names.map(n => {
    const m = html.match(new RegExp(`^(?:async )?function ${n}\\([^]*?^}`, 'm'));
    assert.ok(m, n);
    return m[0];
  });
  return runInNewContext(fns.join('\n') + `\n({${names}})`, context);
}
// through JSON: values made inside the vm have that realm's prototypes
const plain = v => JSON.parse(JSON.stringify(v));
const esc = s => String(s ?? '').replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));
const APPS = [{ name: '时钟', bundle_id: 'com.apple.mobiletimer' }, { name: '设置', bundle_id: 'com.apple.Preferences' }, { name: '照片', bundle_id: 'com.apple.mobileslideshow' }];

function dom() {
  const els = {};
  const el = id => (els[id] ??= { id, hidden: false, disabled: false, textContent: '', innerHTML: '', title: '', value: '', attrs: new Set(),
    toggleAttribute(a, on) { on ? this.attrs.add(a) : this.attrs.delete(a); }, matches: () => false, hidePopover() {} });
  return { els, $: sel => el(sel.replace('#', '')) };
}

test('a task from the page carries its app scope to the viewer', async () => {
  const sent = [];
  const context = { tr: String, finish() {}, follow() {}, async api(path, body) { sent.push([path, body]); return { id: 'abc' }; } };
  const { runLive } = handlers(['runLive'], context);
  await runLive({ prompt: '把亮度调到一半', agent: 'claude', apps: ['com.apple.Preferences'], steps: [] });
  await runLive({ prompt: '看看时间', agent: 'claude', steps: [] });
  assert.deepEqual(plain(sent), [
    ['/api/agent/tasks', { prompt: '把亮度调到一半', agent: 'claude', apps: ['com.apple.Preferences'] }],
    ['/api/agent/tasks', { prompt: '看看时间', agent: 'claude', apps: [] }],
  ]);
});

test('running a task again keeps the scope it had, not the one picked now', () => {
  const started = [];
  const S = { agents: [], scope: ['com.apple.mobiletimer'], tasks: [
    { id: 't1', source: 'live', agent: 'claude', prompt: '改设置', apps: ['com.apple.Preferences'] },
    { id: 't2', source: 'live', agent: 'claude', prompt: '看看', apps: undefined },
  ] };
  const context = { S, tr: String, show() {}, BUILTIN_AGENTS: { claude: 'Claude Code', codex: 'Codex' }, activeTask: () => null,
    startTask: (p, apps) => { started.push([p, apps]); return true; } };
  const { rerunTask } = handlers(['isExternal', 'rerunTask'], context);
  rerunTask('t1'); rerunTask('t2');
  assert.deepEqual(plain(started), [['改设置', ['com.apple.Preferences']], ['看看', []]], 'an old record without a scope runs without one');
});

test('the picker lists picked apps first, searches, and toggles', () => {
  const { els, $ } = dom();
  const S = { scope: ['com.apple.Preferences', 'com.example.gone'], scopeApps: APPS, scopeErr: '', mode: 'live', udid: 'U' };
  const context = { S, $, tr: String, esc, appLabel: b => b, activeTask: () => null, scopeMenu: { matches: () => false }, scopeBtn: $('#scope-btn') };
  const { renderScopeList, toggleScope } = handlers(['scopeName', 'scopeText', 'renderScopePick', 'renderScopeList', 'toggleScope'], context);
  renderScopeList();
  const order = [...els['scope-list'].innerHTML.matchAll(/value="([^"]+)"/g)].map(m => m[1]);
  assert.deepEqual(order.slice(0, 2), ['com.example.gone', 'com.apple.Preferences'], 'picked first, even one the list lacks');
  assert.equal((els['scope-list'].innerHTML.match(/ checked/g) || []).length, 2);
  assert.equal(els['scope-count'].textContent, '已选 2');
  $('#scope-q').value = 'timer';
  renderScopeList();
  assert.deepEqual([...els['scope-list'].innerHTML.matchAll(/value="([^"]+)"/g)].map(m => m[1]), ['com.apple.mobiletimer']);
  toggleScope('com.apple.mobiletimer', true); toggleScope('com.apple.mobiletimer', true);
  toggleScope('com.example.gone', false);
  assert.deepEqual(plain(S.scope), ['com.apple.Preferences', 'com.apple.mobiletimer'], 'each once, removed when unticked');
});

test('the button says what the task is limited to, and waits while a task runs', () => {
  const { els, $ } = dom();
  const S = { scope: [], scopeApps: APPS, mode: 'live', udid: 'U' };
  let active = null;
  const context = { S, $, tr: String, esc, appLabel: b => b, activeTask: () => active, scopeMenu: { matches: () => false, hidePopover() {} }, scopeBtn: $('#scope-btn') };
  const { renderScopePick } = handlers(['scopeName', 'scopeText', 'renderScopePick'], context);
  renderScopePick();
  assert.equal(els['scope-name'].textContent, '所有 App');
  assert.equal(els['scope-btn'].attrs.has('data-scoped'), false);
  S.scope = ['com.apple.Preferences', 'com.apple.mobiletimer'];
  renderScopePick();
  assert.equal(els['scope-name'].textContent, '设置、时钟');
  assert.equal(els['scope-btn'].attrs.has('data-scoped'), true);
  assert.equal(els['scope-btn'].disabled, false);
  active = { status: 'running' };
  renderScopePick();
  assert.equal(els['scope-btn'].disabled, true, 'a running task keeps its scope');
  S.mode = 'demo';
  renderScopePick();
  assert.equal(els['scope-pick'].hidden, true, 'nothing enforces it in the demo');
});

test('the button is announced as the task scope, not just an app name', () => {
  const btn = html.match(/<button[^>]*id="scope-btn"[^>]*>/)[0];
  assert.match(btn, /aria-labelledby="scope-label scope-name"/);
  assert.match(html, /<span class="sr-only" id="scope-label">任务范围<\/span>/);
});
