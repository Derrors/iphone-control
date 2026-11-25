const CHECKS = [
  { name: tr('USB 连接'), fail: ['Absent'], hint: tr('用 USB 连上设备，确认数据线能传数据。') },
  { name: tr('信任这台电脑'), fail: ['NeedsTrust'], hint: tr('运行 <code>iphone-control setup --reconfigure</code> 选「手机」，在设备上点「信任」。') },
  { name: tr('开发者模式'), fail: ['DeveloperModeOff'], hint: tr('在设备「设置 › 隐私与安全性 › 开发者模式」里打开，重启后确认。') },
  { name: tr('WDA 已签名安装'), fail: ['NeedsSigning'], hint: tr('运行 <code>iphone-control sign</code>，用免费 Apple ID 签名并安装。'), action: 'sign' },
  { name: tr('WDA 运行中'), fail: ['WdaStarting', 'NeedsUnlock', 'NeedsAutomationPin'], hint: '' }
];
/* 原生控制（iOS 27 起不用 WDA）：不签名、不启动 WDA，只挂开发者镜像、连无障碍服务 */
const NATIVE_CHECKS = [...CHECKS.slice(0, 3),
  { name: tr('开发者镜像、无障碍服务'), fail: ['Preparing', 'NeedsUnlock'], hint: '' }
];
const STARTING_NO_HID = tr('正在挂开发者镜像、启动 WebDriverAgent，最长约 2 分钟。这台设备低于 iOS 27，没有 HID 后备，期间不能截图和操作。');
const STARTING_HINT = {
  WdaStarting: tr('正在挂开发者镜像、启动 WebDriverAgent，最长约 2 分钟。期间截图和点击会改走 HID 后备。'),
  NeedsUnlock: tr('设备重启后还没解锁过，需要在设备上手动解锁一次。'),
  NeedsAutomationPin: tr('设备重启后首次启用 UI 自动化，请在设备上输入锁屏密码。'),
  Preparing: tr('正在挂开发者镜像、连接无障碍服务，通常几秒到半分钟。')
};
let statusTimerId = 0, statusSequence = 0;
function refreshStatusSoon() { clearTimeout(statusTimerId); statusTimerId = setTimeout(loadDeviceState, 600); }
async function loadDeviceState() {
  if (S.mode === 'demo') {
    S.daemon = { state: 'Ready', device: demoDev, auto_unlock: true };
    S.dstatus = { wda_ready: true, locked: D.screen === 'lock', foreground: foreground(), width: DW(), height: DH(), route: 'wda' };
    S.devices = DEMO_DEVICES.map(d => ({ ...d, controlled: d === demoDev })); S.statusErr = ''; S.devicesErr = '';
  } else if (S.mode === 'live') {
    const udid = S.udid, sequence = ++statusSequence;
    // /api/daemon 也带着设备列表（和它读同一份快照）：一轮两个请求
    const [d, s] = await Promise.allSettled([api('/api/daemon'), api('/api/status')]);
    const l = d;
    // Video can reconnect while the same device is still waiting for trust or signing.
    if (udid !== S.udid || sequence !== statusSequence) return;
    S.daemon = d.status === 'fulfilled' ? d.value : null;
    // busy: the phone was busy with one of the user's actions, so the check gave way; what was shown stays, asked again soon
    if (s.status === 'fulfilled' && s.value.busy) refreshStatusSoon();
    else S.dstatus = s.status === 'fulfilled' ? s.value : null;
    S.statusErr = d.status === 'rejected' ? String(d.reason.message) : s.status === 'rejected' ? String(s.reason.message) : '';
    if (l.status === 'fulfilled') { S.devices = l.value.devices || []; S.devicesFixed = !!l.value.fixed; S.devicesErr = ''; } else { S.devices = []; S.devicesErr = String(l.reason.message); }
  }
  $('#pane-dev').dataset.noDevice = String(S.mode === 'live' && !S.udid);
  renderDevices(); renderChips(); renderFeed(); if (S.devTab === 'status') renderStatusPanel();
}
/* 新设备上还没装 WDA：用已登录的 Apple ID 签名并安装，大约 1–2 分钟 */
async function signWda() {
  if (S.signing) return;
  S.signing = true; renderFeed(); if (S.devTab === 'status') renderStatusPanel();
  show(tr('正在签名并安装 WebDriverAgent，大约 1–2 分钟，保持手机解锁…'));
  try { await act('/api/sign', {}); } finally { S.signing = false; await loadDeviceState(); }
}
function renderDevices() {
  if (S.mode === 'detecting') return;
  if (!S.devices.length) {
    deviceEl.innerHTML = `<option>${esc(S.udid ? tr('当前设备') : tr('没有设备'))}</option>`; deviceEl.disabled = true;
    deviceEl.title = S.devicesErr || ''; pickerSync.device(); return;
  }
  const cur = S.devices.find(d => d.controlled);
  const labels = S.devices.map(deviceOptionLabel);
  deviceEl.innerHTML = S.devices.map((d, index) => {
    const name = labels[index];
    const duplicate = labels.indexOf(name) !== labels.lastIndexOf(name);
    const label = duplicate ? name + ' · ' + d.udid : name;
    return `<option value="${esc(d.udid)}"${d.controlled ? ' selected' : ''}>${esc(label)}</option>`;
  }).join('')
    + `<option value="auto"${cur ? '' : ' selected'}>${tr('自动选择')}</option>`;
  deviceEl.disabled = guarded() || S.switching || S.devicesFixed;
  deviceEl.title = [deviceEl.selectedOptions[0]?.textContent, S.devicesFixed && tr('已用 IPHONE_UDID 固定设备，不能切换')].filter(Boolean).join(' · ');
  pickerSync.device();
}
deviceEl.addEventListener('change', () => selectDevice(deviceEl.value));
$('#nav-devices').addEventListener('click', ev => {
  const row = ev.target.closest('button[data-device]');
  if (row && !row.disabled && row.getAttribute('aria-current') !== 'true') selectDevice(row.dataset.device);
});
async function selectDevice(v) {
  if (guarded() || S.switching || S.devicesFixed) return;
  if (S.mode === 'demo') {
    const next = v === 'auto' ? demoDev : DEMO_DEVICES.find(d => d.udid === v);
    if (!next || next === demoDev) { show(v === 'auto' ? `${tr('已改为自动选择：继续控制')} ${demoDev.name}` : tr('已经在控制这台设备')); return; }
    demoDev = next; D.screen = 'home'; D.focused = false;
    applyDemoGeometry(); renderDemo(); await loadDeviceState();
    S.appsState = 'idle'; if (S.devTab === 'apps') loadApps();
    show(`${tr('已切换到')} ${next.name}${tr('（演示）')}`);
    return;
  }
  cancelGestures(); S.switching = true; deviceEl.disabled = true; pickerSync.device();
  try {
    const r = await act('/api/select', { device: v }, true);
    // /api/info 确认当前设备后才重新启用输入
    if (r.ok) { const u = S.udid; await loadInfo(); if (S.udid === u) show(r.message || tr('已经在控制这台设备')); }
  } finally { S.switching = false; renderFeed(); renderElements(); await loadDeviceState(); }
}
function renderChips() {
  const c = $('#chips'), d = S.daemon, s = S.dstatus;
  if (S.mode === 'detecting') { c.innerHTML = ''; return; }
  const chips = [];
  if (frame.hasAttribute('data-absent')) { c.replaceChildren(); return; }
  const ready = d?.state === 'Ready';
  chips.push(`<span class="chip"><span class="chip-dot ${ready ? 'ok' : d ? 'warn' : S.stream === 'error' ? 'off' : 'warn'}"></span><span>${ready ? tr('就绪') : d ? tr('未就绪') : S.stream === 'error' ? tr('画面断开') : tr('连接中')}</span></span>`);
  // 不知道锁没锁（WDA 没起来、又没有 HID）时不显示
  if (s && s.locked != null) {
    chips.push(`<span class="chip"><span class="k">${tr('屏幕')}</span><span>${s.locked ? tr('已锁定') : tr('未锁定')}</span></span>`);
  }
  // first: the most important to see, and the row may be cut off on the right
  if (S.readOnly) chips.unshift(`<span class="chip" title="${esc(S.login.name)}"><span class="chip-dot warn"></span><span>${tr('只读')}</span></span>`);
  c.innerHTML = chips.join('');
}
// Keep the native disclosures and focused device buttons intact between status polls.
function navigatorHTML(id, html) {
  const node = $(id);
  if (node.navMarkup !== html) { node.innerHTML = html; node.navMarkup = html; }
}
function renderStatusPanel() {
  const d = S.daemon, s = S.dstatus, dev = d?.device;
  const list = S.devices.length ? S.devices : dev?.udid ? [dev] : [];
  const selected = dev?.udid || S.udid;
  const entries = list.map(item => {
    const active = !!item.controlled || item.udid === selected;
    const title = item.name || item.model_name || item.model || item.udid;
    const model = item.model_name || item.model;
    const duplicate = list.some(other => other.udid !== item.udid && deviceOptionLabel(other) === deviceOptionLabel(item));
    const sub = [model && model !== title && model, osVersion(item), duplicate && item.udid].filter(Boolean).join(' · ');
    return `<li><button type="button" class="nav-device" data-device="${esc(item.udid)}" aria-current="${active}" ${S.devicesFixed || guarded() || S.switching ? 'disabled' : ''}>
      ${icon(item.family === 'ipad' ? 'tablet' : 'phone')}<span class="nav-device-copy"><span class="device-name">${esc(title)}</span>${sub ? `<span class="device-sub">${esc(sub)}</span>` : ''}</span></button></li>`;
  });
  const empty = S.mode === 'detecting' ? tr('正在查找…') : S.devicesErr ? tr('不可用') : tr('没有设备');
  navigatorHTML('#nav-devices', entries.join('') || `<li class="nav-device nav-device-empty">${icon('phone')}${esc(empty)}</li>`);

  const screen = dims();
  const fact = ([k, v], mono = false) => `<div class="fact"><dt class="fact-k">${esc(k)}</dt><dd class="fact-v${mono ? ' mono' : ''}">${esc(v)}</dd></div>`;
  const facts = [
    [tr('前台 App'), s?.foreground], [tr('屏幕'), s?.locked != null && (s.locked ? tr('已锁定') : tr('未锁定'))],
    [tr('操作通道'), s?.route === 'wda' ? 'WDA' : s?.route === 'hid' ? tr('HID 后备') : s?.route === 'native' ? tr('原生') : null],
    [tr('屏幕尺寸'), screen && `${screen.W} × ${screen.H} pt`], [tr('自动解锁'), d && (d.auto_unlock ? tr('已设置密码') : tr('未设置'))]
  ].filter(([,v]) => v);
  $('#nav-facts').hidden = !selected || !facts.length;
  navigatorHTML('#nav-facts', facts.map(row => fact(row)).join(''));
  const ids = [['ProductType', dev?.model], ['UDID', selected]].filter(([,v]) => v);
  $('#device-identifiers').hidden = !ids.length;
  navigatorHTML('#nav-ids', ids.map(row => fact(row, true)).join(''));

  const error = S.statusErr || S.devicesErr;
  const allChecks = d?.state === 'Preparing' || s?.route === 'native' ? NATIVE_CHECKS : CHECKS;
  const failAt = allChecks.findIndex(check => check.fail.includes(d?.state));
  const checks = d?.state === 'Ready' ? allChecks : allChecks.slice(0, failAt + 1);
  const rows = checks.map((c,i) => {
    const issue = i === failAt;
    const busy = issue && (d.state === 'WdaStarting' || d.state === 'Preparing');
    const hint = issue && (c.hint || (busy && s?.route === 'none' ? STARTING_NO_HID : STARTING_HINT[d.state]));
    const label = issue ? (busy ? tr('进行中') : tr('需要处理')) : tr('已完成');
    const sign = issue && c.action === 'sign' && d.signed_in;
    return `<li>${icon(issue ? (busy ? 'loader' : 'info') : 'check', issue ? (busy ? 'spin' : '') : 'check-ok')}
      <div><span class="check-name">${c.name}<span class="sr-only">${tr('：')}${label}</span></span>
      ${hint ? `<span class="check-hint">${hint}</span>` : ''}
      ${sign ? `<button type="button" class="btn btn-sm" data-action="sign"${S.signing ? ' disabled' : ''}>${esc(S.signing ? tr('正在签名安装…') : tr('签名并安装 WDA'))}</button>` : ''}
      ${issue && d.last_error ? `<span class="check-err">${esc(d.last_error)}</span>` : ''}</div></li>`;
  });
  navigatorHTML('#nav-checks', (rows.length ? `<ol class="nav-check-list">${rows.join('')}</ol>` : '') +
    (error || !d ? `<p class="check-hint">${esc(error || tr('还没有拿到守护进程状态。'))}</p>` : ''));
}

/* ================================================================
 * 应用
 * ================================================================ */
async function loadApps() {
  S.appsState = 'loading'; renderApps();
  const sys = $('#app-sys').checked, target = S.udid;
  try {
    if (S.mode === 'demo') { await sleep(350); S.apps = sys ? ALL_APPS.map(a => ({ name: a.name, bundle_id: a.bundle })) : []; }
    else {
      const result = await api(`/api/apps?system=${sys ? 1 : 0}`);
      if (target !== S.udid || sys !== $('#app-sys').checked) return;
      S.apps = result.apps || [];
    }
    S.appsState = 'ok';
  } catch (e) { if (target !== S.udid || sys !== $('#app-sys').checked) return; S.appsState = 'error'; S.appsErr = String(e.message || e); }
  renderApps();
}
function renderApps() {
  const ul = $('#apps'), q = $('#app-q').value.trim().toLowerCase(), lock = guarded();
  const list = S.apps.filter(a => !q || a.name.toLowerCase().includes(q) || a.bundle_id.toLowerCase().includes(q));
  $('#apps-count').textContent = S.appsState === 'ok' ? `${list.length} ${tr('个')}${S.mode === 'demo' ? tr(' · 演示数据') : ''}` : '';
  if (S.appsState === 'loading') { ul.innerHTML = tr('<li class="sr-only">正在读取 App 列表…</li>') + '<li class="skel-row" aria-hidden="true"><span class="skel tile"></span><span class="skel-lines"><span class="skel l1"></span><span class="skel l2"></span></span></li>'.repeat(6); return; }
  if (S.appsState === 'error') { ul.innerHTML = `<li class="empty-note err">${esc(S.appsErr)}</li>`; return; }
  if (S.appsState !== 'ok') { ul.innerHTML = tr('<li class="empty-note">打开这一页时会读取已安装的 App。</li>'); return; }
  if (!list.length) { ul.innerHTML = `<li class="empty-note">${q ? `${tr('没有名称或 bundle id 含「')}${esc(q)}${tr('」的 App。')}` : $('#app-sys').checked ? tr('没有读到 App。') : tr('没有你自己安装的 App。勾上「包含系统 App」可以看到系统自带的。')}</li>`; return; }
  ul.innerHTML = list.map(a => {
    const meta = appByAny(a.bundle_id);
    const tile = meta ? `style="background:${meta.bg};color:${meta.fg}"` : 'style="background:var(--sunken);color:var(--text-2)"';
    return `<li class="app-row"><span class="app-tile" ${tile} aria-hidden="true">${esc(meta ? meta.glyph : a.name.slice(0, 1))}</span>
      <span class="app-text"><span class="app-name od-truncate">${esc(a.name)}</span><span class="app-id od-truncate">${esc(a.bundle_id)}</span></span>
      <span class="app-actions"><button type="button" class="btn btn-sm" data-action="launch" data-app="${esc(a.bundle_id)}" data-name="${esc(a.name)}" ${lock ? 'disabled' : ''}>${tr('启动')}</button>
      <button type="button" class="btn btn-ghost btn-icon btn-sm" data-action="terminate" data-app="${esc(a.bundle_id)}" data-name="${esc(a.name)}" aria-label="${tr('结束「')}${esc(a.name)}${tr('」')}" title="${tr('结束')}" ${lock ? 'disabled' : ''}>${icon('x')}</button></span></li>`;
  }).join('');
}
$('#app-q').addEventListener('input', renderApps);
$('#app-sys').addEventListener('change', loadApps);
async function launchApp(bundle, name) {
  if (guarded()) { nudgeGuard(); return; }
  if (S.mode === 'live') return act('/api/launch_app', { app: bundle });
  show(demoLaunch(appByAny(bundle) || { id: bundle, name }));
}
async function terminateApp(bundle, name) {
  if (guarded()) { nudgeGuard(); return; }
  if (S.mode === 'live') return act('/api/terminate_app', { app: bundle });
  if (foreground() === name) demoOpen('home');
  show(`${tr('已结束「')}${name}${tr('」')}`);
}
async function openUrl() {
  if (guarded()) { nudgeGuard(); return; }
  const inp = $('#url'), url = inp.value.trim();
  if (!url) { show(tr('先填要打开的链接'), 'err'); inp.focus(); return; }
  if (S.mode === 'live') { if ((await act('/api/open_url', { url })).ok) inp.value = ''; return; }
  if (/^https?:\/\//i.test(url)) { D.generic = 'Safari'; D.genericSub = `${tr('正在打开')} ${url}${tr('（演示画面不加载网页）')}`; demoOpen('generic'); show(`${tr('已用 Safari 打开')} ${url}`); inp.value = ''; }
  else show(tr('演示模式只模拟 http(s) 链接，URL scheme 要连上真机才能试'), 'err');
}
$('#url').addEventListener('keydown', ev => { if (ev.key === 'Enter' && !ev.isComposing) { ev.preventDefault(); openUrl(); } });

/* ================================================================
 * 元素
 * ================================================================ */
async function loadElements() {
  const epoch = feedEpoch;
  S.elState = 'loading'; renderElements();
  try {
    if (S.mode === 'demo') { await sleep(300); S.elements = demoElements(); }
    else {
      const result = await api('/api/elements?filter=');
      if (epoch !== feedEpoch) return;
      S.elements = result.elements || [];
    }
    S.elState = 'ok';
  } catch (e) { if (epoch !== feedEpoch) return; S.elState = 'error'; S.elErr = String(e.message || e); }
  renderElements();
}
function renderElements() {
  const ul = $('#elements'), q = $('#el-q').value.trim().toLowerCase(), lock = guarded() || (S.mode === 'live' && (!S.udid || S.switching));
  const list = S.elements.map((e, i) => ({ ...e, i })).filter(e => !q || e.label.toLowerCase().includes(q) || e.type.toLowerCase().includes(q));
  $('#el-count').textContent = S.elState === 'ok' ? `${list.length} ${tr('个')}${S.mode === 'demo' ? tr(' · 演示数据') : ''}` : '';
  if (S.elState === 'loading') { ul.innerHTML = tr('<li class="sr-only">正在读取元素…</li>') + '<li class="skel-row no-tile" aria-hidden="true"><span class="skel-lines"><span class="skel l1"></span><span class="skel l2"></span></span></li>'.repeat(6); return; }
  if (S.elState === 'error') { ul.innerHTML = `<li class="empty-note err">${esc(S.elErr)}</li>`; return; }
  if (S.elState === 'stale' || S.elState === 'idle') { ul.innerHTML = `<li class="empty-note">${S.elState === 'stale' ? tr('界面变了，') : ''}${tr('点「读取」列出当前界面上带文字的元素。鼠标移上去会在画面上框出位置。')}</li>`; return; }
  if (!list.length) { ul.innerHTML = `<li class="empty-note">${q ? `${tr('没有含「')}${esc(q)}${tr('」的元素。')}` : tr('当前界面没有带文字的元素。')}</li>`; return; }
  ul.innerHTML = list.map(e => `<li class="el-row" data-el="${e.i}">
      <span class="el-text"><span class="el-label od-truncate">${esc(e.label)}</span><span class="el-meta od-truncate">${esc(e.type)} · ${Math.round(e.x + e.width / 2)}, ${Math.round(e.y + e.height / 2)}</span></span>
      <span class="el-actions"><button type="button" class="btn btn-sm" data-action="tap-el" data-el="${e.i}" ${lock ? 'disabled' : ''}>${tr('点它')}</button></span></li>`).join('');
}
$('#el-q').addEventListener('input', renderElements);
const elList = $('#elements');
elList.addEventListener('pointerover', ev => { const row = ev.target.closest('[data-el]'); if (row) highlight(S.elements[+row.dataset.el]); });
elList.addEventListener('pointerleave', () => highlight(null));
elList.addEventListener('focusin', ev => { const row = ev.target.closest('[data-el]'); if (row) highlight(S.elements[+row.dataset.el]); });
elList.addEventListener('focusout', () => highlight(null));
async function tapElement(i) {
  if (guarded()) { nudgeGuard(); return; }
  const e = S.elements[i]; if (!e) return;
  const same = S.elements.slice(0, i).filter(x => x.label === e.label).length;
  highlight(null);
  if (dims()) drawMark('tap', 'user', { x: round1(e.x + e.width / 2), y: round1(e.y + e.height / 2) });
  if (S.mode === 'live') await act('/api/tap_element', { text: e.label, index: same });
  else {
    const el = $$('[data-label]', demoEl).filter(x => x.dataset.label === e.label)[same];
    show(`${tr('按名字点「')}${e.label}${tr('」：')}${el && el.dataset.act ? demoAct(el.dataset.act) : tr('它不是可点的控件，界面没有变化')}`);
  }
  S.elState = 'stale'; renderElements();
}
async function waitFor() {
  const inp = $('#wait-q'), text = inp.value.trim(), out = $('#wait-result'), btn = $('#wait-btn');
  if (!text) { inp.focus(); out.hidden = false; out.className = 'wait-result t-danger'; out.textContent = tr('先填要等的文字。'); return; }
  btn.disabled = true; btn.textContent = tr('等待中…'); out.hidden = false; out.className = 'wait-result t-muted'; out.textContent = `${tr('等「')}${text}${tr('」出现，最多 5 秒')}…`;
  let ok = false, msg = '';
  try {
    if (S.mode === 'live') { const v = await api('/api/wait_for', { text, timeout: 5 }); ok = !!v.ok; msg = v.message || ''; }
    else {
      const t0 = Date.now();
      while (Date.now() - t0 < 5000) { if (demoElements().some(e => e.label.includes(text) && e.type !== 'TextField')) { ok = true; break; } await sleep(200); }
      msg = ok ? `${tr('「')}${text}${tr('」已经出现（')}${((Date.now() - t0) / 1000).toFixed(1)} ${tr('秒）')}` : `5 ${tr('秒内没有出现「')}${text}${tr('」')}`;
    }
  } catch (e) { msg = String(e.message || e); }
  out.className = `wait-result ${ok ? 't-ok' : 't-danger'}`; out.textContent = msg;
  btn.disabled = false; btn.textContent = tr('等待 5 秒');
}
$('#wait-q').addEventListener('keydown', ev => { if (ev.key === 'Enter' && !ev.isComposing) { ev.preventDefault(); waitFor(); } });

function setDevTab(tab) {
  S.devTab = tab;
  ['status', 'apps', 'elements'].forEach(t => {
    $(`#tab-${t}`).setAttribute('aria-selected', String(t === tab));
    $(`#tp-${t}`).hidden = t !== tab;
  });
  if (tab === 'status') renderStatusPanel();
  if (tab === 'apps' && S.appsState === 'idle' && (S.mode === 'demo' || S.udid)) loadApps(); else if (tab === 'apps') renderApps();
  if (tab === 'elements') renderElements();
}

/* ================================================================
 * AI 任务：演示脚本
 * ================================================================ */
