function renderAll() {
  keepFocus(() => { renderRunbar(); renderGuard(); renderManual(); renderTurns(); renderChips(); renderFeed(); renderAgentPick(); renderScopePick(); renderSysAlert(); });
  if (S.devTab === 'apps') renderApps();
  if (S.devTab === 'elements') renderElements();
}
function fallbackFocus() {
  const rc = $('#runbar button');
  if (rc && !$('#runbar').hidden) return rc;
  return promptEl.disabled ? $('#intro-title') : promptEl;
}
function keepFocus(fn) {
  const a = document.activeElement, key = a && a.dataset ? a.dataset.focus : null;
  fn();
  if (key && !a.isConnected) {
    const n = $(`[data-focus="${CSS.escape(key)}"]`);
    (n && !n.disabled && n.offsetParent !== null ? n : fallbackFocus()).focus({ preventScroll: true });
  }
}
setInterval(() => {
  const t = activeTask(); if (!t) return;
  const el = $(`[data-runtime="${t.id}"]`); if (el) el.textContent = mmss(Date.now() - t.startedAt);
  const m = $(`[data-elapsed="${t.id}"]`); if (m) m.textContent = `${t.steps.length} ${tr('步')} · ${mmss(Date.now() - t.startedAt)}`;
}, 1000);

/* ---------- 视图：≥1280 三栏；1024–1279 右栏抽屉；<1024 三个标签页 ---------- */
const mqDrawer = matchMedia('(min-width: 1024px) and (max-width: 1279.98px)');
const mqTabs = matchMedia('(max-width: 1023.98px)');
function syncInert() {
  $('#pane-dev').inert = (mqDrawer.matches && !S.devOpen) || (mqTabs.matches && S.view !== 'device') || (isThreeCol() && devHidden);
  $('#dev-toggle').setAttribute('aria-expanded', String(!$('#pane-dev').inert));
  $('#pane-agent').inert = mqTabs.matches && S.view !== 'agent';
  $('#pane-stage').inert = mqTabs.matches && S.view !== 'screen';
}
function setView(v) {
  S.view = v; shell.dataset.view = v;
  ['agent', 'screen', 'device'].forEach(k => $(`#vt-${k}`).setAttribute('aria-selected', String(k === v)));
  if (v === 'device') setDevTab(S.devTab);
  syncInert();
}
function setDevOpen(open) {
  S.devOpen = open; shell.dataset.devOpen = String(open); $('#dev-toggle').setAttribute('aria-expanded', String(open)); syncInert();
  if (open) { setDevTab(S.devTab); setTimeout(() => $(`#tab-${S.devTab}`).focus(), 320); } else $('#dev-toggle').focus();
}
mqDrawer.addEventListener('change', syncInert);
mqTabs.addEventListener('change', syncInert);

/* ---------- 事件委托 ---------- */
document.addEventListener('click', ev => {
  const b = ev.target.closest('[data-action]');
  if (!b || b.disabled) return;
  const a = b.dataset.action;
  switch (a) {
    case 'send': sendPrompt(); break;
    case 'pause': case 'resume': case 'takeover': case 'handback': control(a); break;
    case 'approve': answerConfirm(true); break;
    case 'reject': answerConfirm(false); break;
    case 'stop':
      $('#stop-desc').textContent = isExternal(activeTask()) ? tr('发起这个任务的程序会被停止。已经在设备上做完的操作不会撤销。') : tr('AI 会立刻停下。已经在设备上做完的操作不会撤销。');
      stopDialog.showModal(); requestAnimationFrame(() => $('#stop-cancel').focus()); break;
    case 'clear': if (activeTask() || !S.tasks.length) return; clearDialog.showModal(); requestAnimationFrame(() => $('#clear-cancel').focus()); break;
    case 'goto-confirm': gotoConfirm(); break;
    case 'sign': signWda(); break;
    case 'rerun': rerunTask(b.dataset.id); break;
    case 'sys-alert-tap': tapSysAlert(+b.dataset.i); break;
    case 'del-ask': askDelete(b.dataset.id); break;
    case 'del-no': cancelDelete(b.dataset.id); break;
    case 'del-yes': confirmDelete(b.dataset.id); break;
    case 'zoom-in': zoomStep(1); break;
    case 'zoom-out': zoomStep(-1); break;
    case 'zoom-actual': setZoom(1); break;
    case 'zoom-fit': setZoom('fit'); break;
    case 'toggle-steps': { const id = b.dataset.id; if (S.expanded.has(id)) S.expanded.delete(id); else S.expanded.add(id); renderTurns(); break; }
    case 'view': setView(b.dataset.view); break;
    case 'dev-open': if (isThreeCol()) setDevHidden(false, true); else setDevOpen(true); break;
    case 'dev-close': if (isThreeCol()) setDevHidden(true, true); else setDevOpen(false); break;
    case 'dev-tab': setDevTab(b.dataset.tab); break;
    case 'load-apps': loadApps(); break;
    case 'launch': launchApp(b.dataset.app, b.dataset.name); break;
    case 'terminate': terminateApp(b.dataset.app, b.dataset.name); break;
    case 'open-url': openUrl(); break;
    case 'load-elements': loadElements(); break;
    case 'tap-el': tapElement(+b.dataset.el); break;
    case 'wait-for': waitFor(); break;
    case 'reconnect':
      if (S.mode === 'live') { connect(); loadInfo(); loadDeviceState(); show(tr('正在重新连接画面…')); }
      else if (S.mode === 'demo') { renderDemo(); loadDeviceState(); show(tr('已刷新演示画面')); }
      break;
  }
});
promptEl.addEventListener('keydown', ev => { if (ev.key === 'Enter' && !ev.shiftKey && !ev.isComposing && ev.keyCode !== 229) { ev.preventDefault(); sendPrompt(); } });
promptEl.addEventListener('input', () => { autosize(); if (!promptErr.hidden && promptEl.value.trim()) clearPromptError(); });
document.addEventListener('keydown', ev => { if (ev.key === 'Escape' && mqDrawer.matches && S.devOpen && !document.querySelector('dialog[open]')) setDevOpen(false); });
// 设备面板的标签页支持左右方向键切换
$('#pane-dev .seg').addEventListener('keydown', ev => {
  if (ev.key !== 'ArrowLeft' && ev.key !== 'ArrowRight') return;
  const tabs = ['status', 'apps', 'elements'], i = tabs.indexOf(S.devTab), n = tabs[(i + (ev.key === 'ArrowRight' ? 1 : 2)) % 3];
  setDevTab(n); $(`#tab-${n}`).focus();
});

function closeStop(confirmed) {
  stopDialog.close();
  if (confirmed) stopTask();
  const back = $('[data-focus="rc-stop"]');
  (back || fallbackFocus()).focus();
}
$('#stop-cancel').addEventListener('click', () => closeStop(false));
$('#stop-confirm').addEventListener('click', () => closeStop(true));
stopDialog.addEventListener('cancel', ev => { ev.preventDefault(); closeStop(false); });

// 清空操作记录：只清本页记录，任务执行中不可用
const clearDialog = $('#clear-dialog');
function closeClear(confirmed) {
  clearDialog.close();
  if (confirmed && !activeTask()) {
    S.tasks.forEach(t => t.es?.close());
    S.tasks = []; S.expanded.clear();
    renderAll();
    $('#agent-scroll').scrollTop = 0;
    show(tr('已清空操作记录'));
    promptEl.focus();
    return;
  }
  ($('#clear-btn').hidden ? promptEl : $('#clear-btn')).focus();
}
$('#clear-cancel').addEventListener('click', () => closeClear(false));
$('#clear-confirm').addEventListener('click', () => closeClear(true));
clearDialog.addEventListener('cancel', ev => { ev.preventDefault(); closeClear(false); });

/* ---------- 启动 ---------- */
async function detect() {
  try {
    const ctrl = new AbortController(); const timer = setTimeout(() => ctrl.abort(), 1500);
    const r = await fetch('/api/info', { signal: ctrl.signal, headers: { 'X-Ictl-Lang': LANG } }); clearTimeout(timer);
    if (loggedOut(r)) return;
    if (!r.ok) throw new Error('no service');
    const v = await r.json();
    if (!v || typeof v !== 'object' || !('width' in v || 'warning' in v)) throw new Error('no service');
    S.mode = 'live'; applyInfo(v);
  } catch {
    if (document.documentElement.hasAttribute('data-viewer')) {
      S.mode = 'live';
    } else {
      S.mode = 'demo'; S.stream = 'ok';
      stageMsg.hidden = true; live.hidden = true; cv.hidden = true; demoEl.hidden = false; $('#demo-note').hidden = false;
      applyDemoGeometry(); renderDemo();
      show(tr('演示模式：没有连上 iphone-control 服务，画面是本地模拟的。'));
    }
  }
  if (S.mode === 'live') {
    connect(); loadAgents();
    setInterval(refreshFrame, 250);
    setTimeout(pollInfo, 2000);
    alertLoop();
    setInterval(loadDeviceState, 10000);
    adoptTask(); setInterval(adoptTask, 3000);
  } else {
    S.tasks.filter(t => t.reattach).forEach(lostTask);
  }
  await loadDeviceState();
  renderAll(); setDevTab(S.devTab);
}
/* ---------- 执行任务的 AI：本机装了哪些就列哪些，选择记在浏览器里 ---------- */
const AGENT_KEY = 'ictl.agent';
const agentSelect = $('#agent-value');
async function loadAgents() {
  try { S.agents = (await api('/api/agent/backends')).backends || []; } catch { S.agents = []; }
  const ok = S.agents.filter(a => a.available);
  let saved = null;
  try { saved = localStorage.getItem(AGENT_KEY); } catch { /* 预览环境可能禁用存储 */ }
  S.agent = (ok.find(a => a.id === saved) || ok[0] || {}).id || null;
  agentSelect.innerHTML = S.agents.map(a => `<option value="${esc(a.id)}"${a.available ? '' : ' disabled'}>${esc(a.name)}${a.available ? '' : tr('（未安装）')}</option>`).join('');
  if (!S.agent) agentSelect.add(new Option(tr('没有可用的 AI'), ''), 0);
  renderAgentPick();
}
agentSelect.addEventListener('change', () => {
  S.agent = agentSelect.value;
  try { localStorage.setItem(AGENT_KEY, S.agent); } catch { /* The choice still applies for this session. */ }
});
function renderAgentPick() {
  const wrap = $('#agent-pick');
  wrap.hidden = S.mode !== 'live';
  agentSelect.value = S.agent || '';
  agentSelect.title = S.agent ? tr('执行任务的本地 AI') : tr('这台机器的 PATH 里没有 claude 或 codex');
  agentSelect.disabled = !S.agent || !!activeTask();
  pickerSync.agent();
}

/* ---------- 右侧设备面板隐藏 / 显示（宽屏三栏时），记在浏览器里 ---------- */
let devHidden = false;
try { devHidden = localStorage.getItem('ictl.devHidden') === '1'; } catch { /* 预览环境可能禁用存储 */ }
shell.dataset.devHidden = String(devHidden);
function setDevHidden(hidden, moveFocus) {
  devHidden = hidden; shell.dataset.devHidden = String(hidden);
  try { localStorage.setItem('ictl.devHidden', hidden ? '1' : '0'); } catch { /* 只在本次会话生效 */ }
  $('#dev-toggle').setAttribute('aria-expanded', String(!hidden));
  syncInert(); fitPanes();
  if (!hidden) setDevTab(S.devTab);
  if (moveFocus) (hidden ? $('#dev-toggle') : $(`#tab-${S.devTab}`)).focus();
  show(hidden ? tr('已隐藏设备面板，点右上角「设备面板」可以再打开') : tr('已显示设备面板'));
}

/* Local appearance affects workspace text only. Device pixels have their own scale. */
const appearanceKey = 'ictl.appearance.v1';
const systemDark = matchMedia('(prefers-color-scheme: dark)');
const textSize = $('#text-size-value'), theme = $('#theme-value');
function applyAppearance() {
  document.documentElement.dataset.textSize = textSize.value;
  document.documentElement.dataset.scheme = theme.value === 'system' ? (systemDark.matches ? 'dark' : 'light') : theme.value;
}
function readAppearance() {
  let saved;
  try { saved = JSON.parse(localStorage.getItem(appearanceKey)); } catch {}
  textSize.value = ['large','xlarge'].includes(saved?.textSize) ? saved.textSize : 'standard';
  theme.value = ['dark','light'].includes(saved?.theme) ? saved.theme : 'system';
  applyAppearance();
  pickerSync.theme(); pickerSync['text-size']();
}
readAppearance();
systemDark.addEventListener('change', applyAppearance);
window.addEventListener('storage', event => { if (event.key === appearanceKey || event.key === null) readAppearance(); });
$('#appearance').addEventListener('change', () => {
  applyAppearance();
  try { localStorage.setItem(appearanceKey, JSON.stringify({textSize:textSize.value, theme:theme.value})); } catch {}
});
$('#appearance-btn').addEventListener('click', () => $('#appearance').showModal());
/* ---------- 弱网模式：画面码率约减半（手机自己的 HEVC：这台手机的所有页面一起；H.264：只这个页面），记在浏览器里 ---------- */
const LOW_BANDWIDTH_KEY = 'ictl.lowBandwidth';
function lowBandwidthOn() { try { return localStorage.getItem(LOW_BANDWIDTH_KEY) === '1'; } catch { return false; } }
$('#low-bandwidth').checked = lowBandwidthOn();
$('#low-bandwidth').addEventListener('change', ev => {
  ev.stopPropagation();
  try { localStorage.setItem(LOW_BANDWIDTH_KEY, ev.target.checked ? '1' : '0'); } catch { /* 预览环境可能禁用存储 */ }
  if (S.mode === 'live') { connect(); show(tr(ev.target.checked ? '已开启弱网模式，正在重新连接画面…' : '已关闭弱网模式，正在重新连接画面…')); }
  renderFeed();
});
window.addEventListener('storage', event => { if (event.key === LOW_BANDWIDTH_KEY) $('#low-bandwidth').checked = lowBandwidthOn(); });
/* ---------- 两侧栏宽度：拖拽、方向键调整，双击恢复默认，记在浏览器里 ---------- */
const MIN_STAGE = 420;   // 中间设备画面至少保留的宽度
const isThreeCol = () => matchMedia('(min-width: 1280px)').matches;
function makeResizer({ handle, pane, cssVar, def, min, key, edge, maxFn }) {
  const r = { w: def };
  try { const v = parseInt(localStorage.getItem(key), 10); if (v) r.w = v; } catch { /* 预览环境可能禁用存储 */ }
  r.set = (w, save) => {
    const max = Math.round(maxFn());
    r.w = Math.round(Math.min(max, Math.max(min, w)));
    shell.style.setProperty(cssVar, r.w + 'px');
    handle.setAttribute('aria-valuemin', min);
    handle.setAttribute('aria-valuemax', max);
    handle.setAttribute('aria-valuenow', r.w);
    handle.setAttribute('aria-valuetext', `${r.w} ${tr('像素')}`);
    if (save) { try { localStorage.setItem(key, String(r.w)); } catch { /* 只在本次会话生效 */ } }
  };
  handle.addEventListener('pointerdown', ev => {
    if (ev.button !== 0) return;
    ev.preventDefault();
    handle.setPointerCapture(ev.pointerId);
    const rect = pane.getBoundingClientRect();
    handle.classList.add('dragging'); document.body.classList.add('resizing');
    // 手柄在右边缘（左栏）：往右拖变宽；手柄在左边缘（右栏）：往左拖变宽
    const move = e => r.set(edge === 'right' ? e.clientX - rect.left : rect.right - e.clientX, false);
    const up = () => {
      handle.removeEventListener('pointermove', move);
      handle.removeEventListener('pointerup', up);
      handle.removeEventListener('pointercancel', up);
      handle.classList.remove('dragging'); document.body.classList.remove('resizing');
      r.set(r.w, true);
    };
    handle.addEventListener('pointermove', move);
    handle.addEventListener('pointerup', up);
    handle.addEventListener('pointercancel', up);
  });
  handle.addEventListener('dblclick', () => r.set(def, true));
  handle.addEventListener('keydown', ev => {
    const step = (ev.shiftKey ? 64 : 16) * (edge === 'right' ? 1 : -1);
    const next = { ArrowLeft: r.w - step, ArrowRight: r.w + step, Home: min, End: maxFn() }[ev.key];
    if (next === undefined) return;
    ev.preventDefault(); r.set(next, true);
  });
  return r;
}
const agentRS = makeResizer({
  handle: $('#resizer'), pane: $('#pane-agent'), cssVar: '--w-agent', def: 460, min: 360, key: 'ictl.agentWidth', edge: 'right',
  maxFn: () => Math.max(360, Math.min(760, window.innerWidth - (isThreeCol() && !devHidden ? devRS.w : 0) - MIN_STAGE))
});
const devRS = makeResizer({
  handle: $('#resizer-dev'), pane: $('#pane-dev'), cssVar: '--w-dev', def: 344, min: 300, key: 'ictl.devWidth', edge: 'left',
  // 三栏时给左栏和设备画面留够位置；1024–1279px 的抽屉最宽占视口 90%
  maxFn: () => isThreeCol() ? Math.max(300, Math.min(600, window.innerWidth - agentRS.w - MIN_STAGE)) : Math.max(300, Math.min(600, window.innerWidth * 0.9))
});
function fitPanes() { devRS.set(devRS.w, false); agentRS.set(agentRS.w, false); }
window.addEventListener('resize', fitPanes);
fitPanes();

/* ---------- 操作记录保存在浏览器里，刷新后还在 ---------- */
const TASKS_KEY = 'ictl.tasks', MAX_TASKS = 50;
function serializeTasks(withThumbs) {
  return JSON.stringify(S.tasks.slice(-MAX_TASKS).map(t => ({
    id: t.id, prompt: t.prompt, status: t.status, source: t.source, agent: t.agent, apps: t.apps, startedAt: t.startedAt, endedAt: t.endedAt, summary: t.summary, remoteId: t.remoteId,
    steps: t.steps.map(st => ({ id: st.id, n: st.n, thought: st.thought, tool: st.tool, args: st.args, status: st.status, ms: st.ms, result: st.result, confirm: st.confirm }))
  })));
}
let saveTimer = 0;
function saveTasks() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => {
    try { localStorage.setItem(TASKS_KEY, serializeTasks(true)); }
    catch {
      // 空间不够时去掉截图缩略图再存一次；存储被禁用时只保留在本次页面里
      try { localStorage.setItem(TASKS_KEY, serializeTasks(false)); } catch { /* 忽略 */ }
    }
  }, 300);
}
function loadTasks() {
  let list;
  try { list = JSON.parse(localStorage.getItem(TASKS_KEY) || '[]'); } catch { return; }
  if (!Array.isArray(list)) return;
  list = list.filter(t => t && t.id && Array.isArray(t.steps));
  list.forEach(t => {
    // 刷新前还在执行的任务：任务在守护进程里，连上后接着跟（adoptTask）；演示任务没有可接的，如实标成已停止
    if (ACTIVE.includes(t.status)) {
      if (t.remoteId) t.reattach = true; else lostTask(t);
    }
  });
  S.tasks = list;
  S.seq = list.reduce((m, t) => Math.max(m, parseInt(String(t.id).replace(/\D/g, ''), 10) || 0), 0);
}
loadTasks();

/* ---------- 画面缩放：适应窗口，或按实际尺寸的 50%–200%（100% = 一个点占一个像素），记在浏览器里 ---------- */
const ZOOM = { min: 0.5, max: 2, steps: [0.5, 0.67, 0.75, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2], key: 'ictl.zoom' };
const stageWrap = $('#stage-wrap');
let zoom = 'fit';
try { const v = localStorage.getItem(ZOOM.key); if (v && v !== 'fit' && +v >= ZOOM.min && +v <= ZOOM.max) zoom = +v; } catch { /* 预览环境可能禁用存储 */ }
const devPointsW = () => (dims() || {}).W || 0;                 // 当前方向下设备的宽度（点）
const fitScale = () => { const w = devPointsW(); return w ? stage.clientWidth / w : 1; };
const currentScale = () => zoom === 'fit' ? fitScale() : zoom;
function applyZoom() {
  const w = devPointsW();
  if (zoom === 'fit' || !w) { delete frame.dataset.zoom; frame.style.removeProperty('--fixed-w'); }
  else {
    const pad = parseFloat(getComputedStyle(frame).getPropertyValue('--pad')) || 11;
    frame.dataset.zoom = 'fixed'; frame.style.setProperty('--fixed-w', `${Math.round(w * zoom + pad * 2)}px`);
  }
  updateZoomUI();
}
function updateZoomUI() {
  const known = !!devPointsW(), sc = currentScale();
  $('#zoom-val').textContent = known ? `${Math.round(sc * 100)}%` : '—';
  $('#zoom-val').disabled = !known;
  $('#zoom-fit').setAttribute('aria-pressed', String(zoom === 'fit'));
  $('#zoom-out').disabled = !known || sc <= ZOOM.min + 0.001;
  $('#zoom-in').disabled = !known || sc >= ZOOM.max - 0.001;
}
function setZoom(z, anchor) {
  if (z !== 'fit' && !devPointsW()) return;
  // 缩放前记下视野中心（或滚轮所在点）在内容里的相对位置，缩放后保持在原处
  const r = stageWrap.getBoundingClientRect();
  const ax = anchor ? anchor.x - r.left : stageWrap.clientWidth / 2, ay = anchor ? anchor.y - r.top : stageWrap.clientHeight / 2;
  const fx = (stageWrap.scrollLeft + ax) / stageWrap.scrollWidth, fy = (stageWrap.scrollTop + ay) / stageWrap.scrollHeight;
  zoom = z === 'fit' ? 'fit' : Math.round(Math.min(ZOOM.max, Math.max(ZOOM.min, z)) * 100) / 100;
  try { localStorage.setItem(ZOOM.key, String(zoom)); } catch { /* 只在本次会话生效 */ }
  applyZoom();
  stageWrap.scrollLeft = fx * stageWrap.scrollWidth - ax;
  stageWrap.scrollTop = fy * stageWrap.scrollHeight - ay;
}
function zoomStep(dir) {
  const sc = currentScale();
  const next = dir > 0 ? ZOOM.steps.find(v => v > sc + 0.005) : [...ZOOM.steps].reverse().find(v => v < sc - 0.005);
  setZoom(next ?? (dir > 0 ? ZOOM.max : ZOOM.min));
}
/* 画面只用上方的加减号缩放。触控板双指捏合在浏览器里是按住 Ctrl 的滚轮（Safari 另有 gesture 事件），拖动、滑动时很容易误触，
 * 所以在画面区域拦下来：不缩放画面，也不交给浏览器缩放整页（整页缩放照样可以用 Ctrl/⌘ 加减号） */
function blockZoomWheel(ev) {
  if (ev.ctrlKey || ev.metaKey) ev.preventDefault();
}
stageWrap.addEventListener('wheel', blockZoomWheel, { passive: false });
for (const t of ['gesturestart', 'gesturechange', 'gestureend']) stageWrap.addEventListener(t, ev => ev.preventDefault());
// 适应模式下窗口或栏宽变化时，比例读数跟着更新
new ResizeObserver(updateZoomUI).observe(stage);

hydrateIcons();
renderAll();
renderStatusPanel();
syncInert();
detect();
