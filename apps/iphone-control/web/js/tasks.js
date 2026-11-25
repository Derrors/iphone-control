const STOP = Symbol('stop');
function snapshot() {
  const c = demoEl.cloneNode(true);
  c.removeAttribute('id'); c.removeAttribute('hidden'); c.setAttribute('aria-hidden', 'true');
  c.style.setProperty('--k', String(64 / DW()));
  return c.outerHTML;
}
function elPoint(sel) {
  const el = demoEl.querySelector(sel); if (!el) return { el: null, p: null };
  const R = demoEl.getBoundingClientRect(); if (!R.width) return { el, p: null };
  const r = el.getBoundingClientRect(), k = R.width / DW();
  return { el, p: { x: round1((r.left - R.left + r.width / 2) / k), y: round1((r.top - R.top + r.height / 2) / k) } };
}
function aiTap(sel, what) {
  const { el, p } = elPoint(sel);
  if (!el) return { error: true, text: `${tr('界面上没找到「')}${what}${tr('」。界面可能已经变了，需要重新截图判断。')}` };
  drawMark('tap', 'ai', p);
  const msg = demoAct(el.dataset.act);
  return { text: p ? `${tr('点了')} (${p.x}, ${p.y})${tr('：')}${msg}` : msg };
}
const shot = () => ({ text: `${tr('当前在「')}${screenName()}${tr('」')}`, thumb: true });
const labelsMatching = f => $$('[data-label]', demoEl).map(e => e.dataset.label).filter(l => l.includes(f));
function classify(prompt) {
  if (/系统版本|iOS 版本|关于本机|version|about/i.test(prompt)) return 'version';
  if (/闹钟|alarm/i.test(prompt)) return 'alarms';
  if (/秒表|计时|stopwatch|timer/i.test(prompt)) return 'stopwatch';
  if (/主屏幕|桌面|home/i.test(prompt)) return 'home';
  if (/发|信息|短信|消息|告诉|message|text|tell|send/i.test(prompt)) return 'message';
  if (/截图|看看|有什么|screen|look/i.test(prompt)) return 'look';
  return 'unknown';
}
function buildScript(task) {
  const look = { thought: tr('先看一下设备现在的界面。'), tool: 'iphone_screenshot', args: {}, exec: shot };
  const launch = (name, fn) => ({ thought: `${tr('打开「')}${name}${tr('」。')}`, tool: 'iphone_launch_app', args: { app: name }, exec: () => { fn(); return { text: `${tr('「')}${name}${tr('」已在前台')}` }; }, think: 900 });
  switch (classify(task.prompt)) {
    case 'version': return { steps: [
      look, launch(tr('设置'), () => { D.setPage = 'root'; demoOpen('settings'); }),
      { thought: tr('系统版本在「通用 › 关于本机」里，先点「通用」。'), tool: 'iphone_tap_element', args: { text: tr('通用') }, exec: () => aiTap('[data-act="set:general"]', tr('通用')) },
      { thought: tr('再点「关于本机」。'), tool: 'iphone_tap_element', args: { text: tr('关于本机') }, exec: () => aiTap('[data-act="set:about"]', tr('关于本机')) },
      { thought: tr('读出「iOS 版本」这一行。'), tool: 'iphone_list_elements', args: { filter: tr('iOS 版本') }, exec: () => { const l = labelsMatching(tr('iOS 版本')); return l.length ? { text: l[0], thumb: true } : { error: true, text: tr('没找到「iOS 版本」') }; } }
    ], summary: () => tr('这台设备的系统版本是 iOS 18.1。') };
    case 'alarms': return { steps: [
      look, launch(tr('时钟'), () => demoOpen('clock')),
      { thought: tr('底部有「闹钟」标签，点它。'), tool: 'iphone_tap_element', args: { text: tr('闹钟') }, exec: () => aiTap('[data-act="tab:alarm"]', tr('闹钟')) },
      { thought: tr('读一下列表里的闹钟。'), tool: 'iphone_list_elements', args: { filter: tr('闹钟') }, exec: () => { const n = labelsMatching(tr('闹钟 ')).length; return n ? { text: `${tr('找到')} ${n} ${tr('个闹钟')}`, thumb: true } : { error: true, text: tr('列表里没有闹钟') }; } }
    ], summary: () => `${tr('一共')} ${ALARMS.length} ${tr('个闹钟：')}${ALARMS.map(a => `${a.time} ${a.note}${tr('（')}${a.on ? tr('开') : tr('关')}${tr('）')}`).join(tr('，'))}${tr('。')}` };
    case 'stopwatch': return { steps: [
      look, launch(tr('时钟'), () => demoOpen('clock')),
      { thought: tr('点底部的「秒表」。'), tool: 'iphone_tap_element', args: { text: tr('秒表') }, exec: () => aiTap('[data-act="tab:stopwatch"]', tr('秒表')) },
      { thought: tr('点「启动」。'), tool: 'iphone_tap_element', args: { text: tr('启动') }, exec: () => D.sw.running ? { text: tr('秒表已经在计时') } : aiTap('[data-act="sw:toggle"]', tr('启动')) },
      { thought: tr('确认秒表在走。'), tool: 'iphone_screenshot', args: {}, exec: () => D.sw.running ? { text: `${tr('秒表正在计时：')}${fmtSw(swElapsed())}`, thumb: true } : { error: true, text: tr('秒表没有在计时'), thumb: true }, think: 1200 }
    ], summary: () => tr('秒表已经开始计时。') };
    case 'home': return { steps: [
      { thought: tr('按一下主屏幕键。'), tool: 'iphone_press_button', args: { button: 'home' }, exec: () => ({ text: demoKey('home') }) },
      { thought: tr('确认已经在主屏幕。'), tool: 'iphone_screenshot', args: {}, exec: () => D.screen === 'home' ? shot() : { error: true, text: `${tr('还停在「')}${screenName()}${tr('」')}`, thumb: true } }
    ], summary: () => tr('已回到主屏幕。') };
    case 'message': {
      const who = Object.keys(D.chats).find(n => task.prompt.includes(n)) || tr('妈妈');
      const m = task.prompt.match(/[：:](.+)$/) || task.prompt.match(/说(.+)$/) || task.prompt.match(/\bsay(?:ing)?\s+(.+)$/i);
      const text = (m ? m[1] : tr('我晚点到')).trim() || tr('我晚点到');
      return { steps: [
        look, launch(tr('信息'), () => demoOpen('messages')),
        { thought: `${tr('点开和「')}${who}${tr('」的对话。')}`, tool: 'iphone_tap_element', args: { text: who }, exec: () => aiTap(`[data-act="open-chat:${who}"]`, who) },
        { thought: tr('点一下底部的输入框。'), tool: 'iphone_tap', argsFrom: () => elPoint('[data-act="chat:focus"]').p || {}, exec: () => aiTap('[data-act="chat:focus"]', tr('输入框')) },
        { thought: tr('输入要发的内容。'), tool: 'iphone_type_text', args: { text }, exec: () => { const r = demoType(text, false); return r.ok ? { text: r.message } : { error: true, text: r.message }; } },
        { thought: tr('内容填好了。发消息需要你先确认。'), tool: 'iphone_tap_element', args: { text: tr('发送') }, confirm: { action: tr('发送消息'), detail: `${tr('发给「')}${who}${tr('」：')}${text}` }, rejectSummary: tr('你拒绝了发送。消息还留在输入框里，没有发出。'), exec: () => aiTap('[data-act="chat:send"]', tr('发送')) }
      ], summary: () => `${tr('已把「')}${text}${tr('」发给「')}${who}${tr('」。')}` };
    }
    case 'look': return { steps: [
      look,
      { thought: tr('读一下界面上带文字的元素。'), tool: 'iphone_list_elements', args: { limit: 150 }, exec: () => ({ text: `${tr('找到')} ${$$('[data-label]', demoEl).length} ${tr('个带文字的元素')}` }), think: 1100 }
    ], summary: () => { const l = [...new Set($$('[data-label]', demoEl).map(e => e.dataset.label))]; return `${tr('现在在「')}${screenName()}${tr('」，能看到：')}${l.slice(0, 8).join(tr('、'))}${l.length > 8 ? tr(' 等') : ''}${tr('。')}`; } };
    default: return { steps: [
      look,
      { thought: tr('演示模式只内置了几个示例：查系统版本、看闹钟、秒表、发信息、回主屏幕、看屏幕。这条在模拟画面上做不了。'), tool: null, args: null, exec: () => ({ error: true, text: tr('演示模式没有模拟这个任务') }), think: 900 }
    ], failSummary: tr('演示模式只能跑内置示例。连上服务后，这条指令会交给真正的 agent。') };
  }
}
async function gate(task) {
  while (task.status === 'paused' || task.status === 'takeover') { await sleep(150); if (task.stopped) throw STOP; }
  if (task.stopped) throw STOP;
}
async function runStep(task, spec) {
  await gate(task);
  if (task.recheck) { task.recheck = false; await runStep(task, { thought: tr('你刚才接管过设备，先重新看一下界面再继续。'), tool: 'iphone_screenshot', args: {}, exec: shot }); }
  const step = { n: task.steps.length + 1, thought: spec.thought, tool: spec.tool, args: spec.argsFrom ? spec.argsFrom() : spec.args, status: 'running' };
  task.steps.push(step); renderTurns(true);
  const t0 = performance.now();
  await sleep(spec.think ?? 700);
  await gate(task);
  if (spec.confirm) {
    step.status = 'waiting'; step.confirm = spec.confirm; task.status = 'waiting'; renderAll(); scrollConvo();
    const ok = await new Promise(res => { task.confirmResolve = res; });
    task.confirmResolve = null;
    if (task.stopped) throw STOP;
    if (!ok) { step.status = 'rejected'; step.result = tr('你拒绝了这一步，没有执行。'); finish(task, 'stopped', spec.rejectSummary); throw STOP; }
    step.status = 'running'; task.status = 'running'; renderAll(); await sleep(400);
  }
  const r = spec.exec() || {};
  step.ms = performance.now() - t0; step.status = r.error ? 'error' : 'ok'; step.result = r.text;
  renderTurns(true);
  if (r.error) throw new Error(r.text);
  await sleep(300);
}
async function runDemo(task) {
  const script = buildScript(task);
  try { for (const spec of script.steps) await runStep(task, spec); finish(task, 'success', script.summary()); }
  catch (e) { if (e === STOP) return; finish(task, 'failed', script.failSummary || `${tr('第')} ${task.steps.length} ${tr('步没走通：')}${e.message}${tr('。可以换个说法再试，或者接管后手动完成。')}`); }
}
function finish(task, status, summary) {
  task.status = status; task.endedAt = Date.now(); task.summary = summary; task.es?.close();
  S.expanded.delete(task.id);  // 完成后步骤自动收起，用户可以手动展开
  renderAll(); scrollConvo();
  show(status === 'success' ? tr('AI 任务已完成') : status === 'failed' ? tr('AI 任务没有完成') : tr('AI 任务已停止'), status === 'failed' ? 'err' : null);
}

/* ---------- AI 任务：真实接口 ---------- */
function upsertStep(task, d) {
  let s = d.id != null ? task.steps.find(x => x.id === d.id) : null;
  if (!s) { s = { n: task.steps.length + 1, status: 'running' }; task.steps.push(s); }
  Object.assign(s, d);
  return s;
}
async function runLive(task) {
  try {
    const v = await api('/api/agent/tasks', { prompt: task.prompt, agent: task.agent, apps: task.apps || [] });
    follow(task, v.id);
  } catch (e) {
    task.steps.push({ n: 1, tool: null, status: 'error', result: String(e.message || e) });
    finish(task, 'failed', tr('任务没能交给 AI。手动操作和设备面板不受影响。'));
  }
}
/* 任务在守护进程里：这个页面开的、页面刷新前开的、在别处开的（iphone-control task run）都一样跟着它的事件 */
function follow(task, id) {
    task.remoteId = id;
    const es = new EventSource(`/api/agent/tasks/${encodeURIComponent(id)}/events`);
    task.es = es;
    es.addEventListener('step', e => { const d = JSON.parse(e.data); upsertStep(task, d); if (d.gesture) { const g = d.gesture; drawMark(g.kind, 'ai', { x: g.x, y: g.y }, g.x2 != null ? { x: g.x2, y: g.y2 } : null); } renderTurns(true); });
    es.addEventListener('confirm', e => { const d = JSON.parse(e.data); upsertStep(task, { id: d.step, status: 'waiting', confirm: d }); task.status = 'waiting'; renderAll(); scrollConvo(); });
    es.addEventListener('state', e => { const d = JSON.parse(e.data); if (ACTIVE.includes(task.status)) { task.status = d.state; renderAll(); } });
    es.addEventListener('done', e => { const d = JSON.parse(e.data); finish(task, d.ok ? 'success' : d.stopped ? 'stopped' : 'failed', d.summary || ''); refreshStatusSoon(); });
    es.onerror = () => { if (ACTIVE.includes(task.status)) show(tr('AI 任务的事件流断开了，正在重连…'), 'warn'); };
}
/* 页面已经跟不上的任务（守护进程不记得了，或者没连上服务） */
function lostTask(t) {
  t.reattach = false; t.status = 'stopped'; t.endedAt = t.endedAt || Date.now();
  t.summary = tr('页面刷新时这个任务还在执行，刷新后页面不再跟踪它的进度。');
  t.steps.forEach(st => { if (st.status === 'running' || st.status === 'waiting') st.status = 'stopped'; });
}
/* 正在进行、但这个页面还没跟着的任务（页面刷新过，或者是在别处开的）：接上，事件会从头重放 */
let adopting = false;
async function adoptTask() {
  const pending = S.tasks.filter(t => t.reattach);
  if (S.mode !== 'live' || adopting || (!pending.length && activeTask())) return;
  adopting = true;
  try {
    const list = (await api('/api/agent/tasks')).tasks || [];
    pending.forEach(t => { if (list.some(x => x.id === t.remoteId)) { t.reattach = false; follow(t, t.remoteId); } else lostTask(t); });
    if (pending.length) renderAll();
    const t = list.find(x => ACTIVE.includes(x.state));
    if (!t || activeTask() || S.tasks.some(x => x.remoteId === t.id)) return;
    const task = { id: 't' + (++S.seq), prompt: t.title, status: t.state, steps: [], startedAt: t.started || Date.now(), source: 'live', agent: t.owner, apps: t.apps || [] };
    S.tasks.push(task); S.expanded.add(task.id);
    follow(task, t.id);
    renderAll(); scrollConvo();
    show(isExternal(task) ? tr('接上了一个正在进行的外部任务') : tr('接上了一个正在进行的 AI 任务'));
  } catch { /* 下一轮再看 */ } finally { adopting = false; }
}
async function liveControl(task, action, body) {
  if (!task.remoteId) return;
  try { await api(`/api/agent/tasks/${encodeURIComponent(task.remoteId)}/${action}`, body || {}); } catch (e) { show(`${tr('操作没有生效：')}${e.message || e}`, 'err'); }
}

/* ---------- 发送、控制、确认 ---------- */
function clearPromptError() { promptErr.hidden = true; promptErr.textContent = ''; promptEl.removeAttribute('aria-invalid'); $('#composer').classList.remove('invalid'); }
function syncSend() {
  const empty = !promptEl.value.trim();
  $('#send').disabled = promptEl.disabled || empty || (S.mode === 'live' && !S.udid);
}
function autosize() { promptEl.style.height = 'auto'; promptEl.style.height = Math.min(promptEl.scrollHeight, 176) + 'px'; syncSend(); }
try {
  const draft = sessionStorage.getItem('ictl.languageDraft');
  if (draft !== null) {
    sessionStorage.removeItem('ictl.languageDraft');
    promptEl.value = draft;
    autosize();
  }
} catch { /* No draft when session storage is unavailable. */ }
function sendPrompt() {
  if (activeTask() || (S.mode === 'live' && !S.udid)) return;
  const text = promptEl.value.trim();
  if (!text) {
    promptErr.textContent = tr('先写下要 AI 做的事，例如「打开设置，查看系统版本」。'); promptErr.hidden = false;
    promptEl.setAttribute('aria-invalid', 'true'); $('#composer').classList.add('invalid'); promptEl.focus(); return;
  }
  if (S.mode === 'live' && !S.agent) {
    promptErr.textContent = S.agents.length ? tr('这台机器上没有可用的 AI。安装并登录 Claude Code 或 Codex，再重新运行 iphone-control viewer。') : tr('还在读取可用的 AI，稍等一下再试。');
    promptErr.hidden = false; return;
  }
  clearPromptError();
  if (startTask(text)) { promptEl.value = ''; autosize(); }
}
function startTask(text, apps = S.scope) {
  if (activeTask()) return false;
  if (S.readOnly) { show(tr('只读登录：只能看，不能操作'), 'warn'); return false; }
  if (S.mode === 'detecting') { show(tr('还在连接服务，稍等一下再开始'), 'warn'); return false; }
  S.pendingDelete = null;
  const task = { id: 't' + (++S.seq), prompt: text, status: 'running', steps: [], startedAt: Date.now(), source: S.mode, agent: S.agent, apps: S.mode === 'live' ? [...apps] : [] };
  S.tasks.push(task); S.expanded.add(task.id);
  renderAll(); scrollConvo();
  ($('[data-focus="rc-pause"]') || $('#intro-title')).focus({ preventScroll: true });
  show(tr('任务已交给 AI，画面的手动操作已锁定'));
  (S.mode === 'demo' ? runDemo : runLive)(task);
  return true;
}
function rerunTask(id) {
  const t = S.tasks.find(x => x.id === id);
  if (!t || activeTask() || isExternal(t)) return;
  // 重新执行沿用原来的范围
  if (startTask(t.prompt, t.apps || [])) show(`${tr('重新执行：')}${t.prompt}`);
}
let delTimer = 0;
function askDelete(id) {
  S.pendingDelete = id; renderTurns();
  $(`[data-focus="dely-${CSS.escape(id)}"]`)?.focus({ preventScroll: true });
  clearTimeout(delTimer);
  delTimer = setTimeout(() => { if (S.pendingDelete === id) { S.pendingDelete = null; renderTurns(); } }, 5000);
}
function cancelDelete(id) {
  clearTimeout(delTimer); S.pendingDelete = null; renderTurns();
  $(`[data-focus="del-${CSS.escape(id)}"]`)?.focus({ preventScroll: true });
}
function confirmDelete(id) {
  clearTimeout(delTimer);
  const i = S.tasks.findIndex(x => x.id === id);
  if (i < 0 || ACTIVE.includes(S.tasks[i].status)) return;
  S.tasks.splice(i, 1); S.expanded.delete(id); S.pendingDelete = null;
  renderAll();
  show(tr('已删除这条记录'));
  // 焦点落到相邻记录的删除按钮上；没有记录了就回到输入框
  const near = S.tasks[i] || S.tasks[i - 1];
  ((near && $(`[data-focus="del-${CSS.escape(near.id)}"]`)) || (promptEl.disabled ? $('#intro-title') : promptEl)).focus({ preventScroll: true });
}
function control(action) {
  const t = activeTask(); if (!t) return;
  if (t.source === 'live') liveControl(t, action);
  if (action === 'pause') { t.status = 'paused'; show(tr('AI 已暂停')); }
  if (action === 'resume') { t.status = 'running'; show(tr('AI 继续执行')); }
  if (action === 'takeover') { t.status = 'takeover'; show(tr('你已接管：现在可以直接在画面上操作。')); }
  if (action === 'handback') { t.status = 'running'; t.recheck = true; show(tr('已交还 AI，它会先重新看一遍屏幕')); }
  renderAll();
}
function answerConfirm(approve) {
  const t = activeTask(); if (!t || t.status !== 'waiting') return;
  if (t.source === 'live') {
    const s = t.steps.find(x => x.status === 'waiting');
    liveControl(t, 'confirm', { step: s?.id, approve });
    if (s) s.status = approve ? 'running' : 'rejected';
    t.status = 'running'; renderAll();
  } else t.confirmResolve?.(approve);
  show(approve ? tr('已批准，AI 继续执行') : tr('已拒绝这一步'));
}
function stopTask() {
  const t = activeTask(); if (!t) return;
  t.stopped = true;
  t.steps.forEach(s => { if (s.status === 'running' || s.status === 'waiting') s.status = 'stopped'; });
  t.confirmResolve?.(false);
  if (t.source === 'live') liveControl(t, 'stop');
  finish(t, 'stopped', `${tr('在第')} ${t.steps.length} ${tr('步停下了。已经做完的操作不会撤销。')}`);
}
function gotoConfirm() {
  setView('agent');
  const t = activeTask(); if (t) { S.expanded.add(t.id); renderTurns(); }
  const btn = $('[data-action="approve"]');
  if (btn) { btn.closest('.confirm').scrollIntoView({ block: 'center' }); btn.focus({ preventScroll: true }); }
}

/* ================================================================
 * 渲染
 * ================================================================ */
/* 一步的说明：agent 自己说的话；没说时按工具和参数写一句看得懂的话 */
/* bundle id → App 名：没读过就读一次全部 App（含系统的），读完重画执行记录 */
const appNames = { map: null, loading: false };
function appLabel(app) {
  const a = S.apps.find(x => x.bundle_id === app);
  if (a) return a.name;
  if (appNames.map) return appNames.map[app] || app;
  if (!appNames.loading && S.mode === 'live') {
    const target = S.udid; appNames.loading = true;
    api('/api/apps?system=1').then(v => {
      if (target !== S.udid) return;
      appNames.map = Object.fromEntries((v.apps || []).map(x => [x.bundle_id, x.name]));
      renderTurns();
    }).catch(() => { if (target === S.udid) appNames.map = {}; });
  }
  return app;
}
function describeStep(s) {
  // 中文「」紧挨着前面的字，英文用 “” 并空一格
  const a = s.args || {}, sp = LANG === 'en' ? ' ' : '', q = v => sp + (LANG === 'en' ? `“${String(v ?? '')}”` : `「${String(v ?? '')}」`);
  switch (s.tool) {
    case 'iphone_screenshot': return tr('看一下屏幕');
    case 'iphone_list_elements': return tr('读取屏幕上的内容');
    case 'iphone_status': return tr('查看手机状态');
    case 'iphone_tap': return tr('点击屏幕');
    case 'iphone_tap_element': return tr('点击') + q(a.text);
    case 'iphone_long_press': return tr('长按屏幕');
    case 'iphone_swipe': return tr('滑动屏幕');
    case 'iphone_type_text': return tr('输入') + q(a.text);
    case 'iphone_press_button': return ({ home: tr('按主屏幕键'), lock: tr('锁屏'), volume_up: tr('音量加'), volume_down: tr('音量减') })[a.button] || tr('按键');
    case 'iphone_launch_app': return tr('打开 App') + q(appLabel(a.app));
    case 'iphone_terminate_app': return tr('关闭') + q(appLabel(a.app));
    case 'iphone_open_url': return tr('打开链接') + ' ' + String(a.url || '');
    case 'iphone_wait_for': return tr('等待') + q(a.text) + sp + tr('出现');
    case 'iphone_list_apps': return tr('查看已安装的 App');
    case 'iphone_list_devices': return tr('查看已连接的设备');
    case 'iphone_batch': return tr('连续执行几步操作');
    case 'iphone_confirm': return tr('请你确认') + q(a.action);
    default: return tr('操作手机');
  }
}
/* 执行记录里每一步只有一句话：做了什么；没成功时注明一句（具体原因是给 agent 看的，它的下一步和总结会说明）。截图、工具名、参数和原始结果都不显示 */
function stepHtml(s) {
  const st = STEP_STATUS[s.status] || STEP_STATUS.running;
  const text = s.thought || describeStep(s);
  const confirm = s.status === 'waiting' && s.confirm ? `<div class="confirm" role="group" aria-labelledby="cf-${s.n}">
      <p class="confirm-title" id="cf-${s.n}">${icon('alert')}<span>${tr('需要你确认：')}${esc(s.confirm.action)}</span></p>
      <p class="confirm-detail">${esc(s.confirm.detail)}</p>
      <div class="od-cluster"><button type="button" class="btn btn-primary" data-action="approve" data-focus="approve">${icon('check')}<span>${tr('批准并执行')}</span></button><button type="button" class="btn" data-action="reject" data-focus="reject">${tr('拒绝')}</button></div></div>` : '';
  return `<li class="step"><div class="step-rail"><span class="step-dot tone-${st.tone}" title="${st.label}">${icon(st.icon, st.icon === 'loader' ? 'spin' : '')}<span class="sr-only">${st.label}</span></span><span class="step-line"></span></div>
    <div class="step-body"><p class="step-thought">${esc(text)}</p>${s.status === 'error' ? `<p class="step-result err">${tr('这一步没成功')}</p>` : ''}${confirm}</div></li>`;
}
/* 谁在执行：本机的 Claude Code / Codex，或者在别处开任务的程序（iphone-control task run 的 --owner）。
 * 外部任务不是这个页面交给 AI 的，所以不能「重新执行」，提示里也不说是 AI */
const BUILTIN_AGENTS = { claude: 'Claude Code', codex: 'Codex' };
function isExternal(t) {
  return !!t && t.source === 'live' && !!t.agent && !(t.agent in BUILTIN_AGENTS) && !S.agents.some(a => a.id === t.agent);
}
function agentLabel(t) {
  if (t.source !== 'live' || !t.agent) return `AI ${tr('助手')}`;
  const name = (S.agents.find(a => a.id === t.agent) || {}).name || BUILTIN_AGENTS[t.agent] || t.agent;
  return isExternal(t) ? `${name} · ${tr('外部任务')}` : name;
}
function turnHtml(t) {
  // the id is the page's own (`t<n>`), but read back from localStorage: escaped like everything else put in HTML
  const id = esc(t.id);
  const st = TASK_STATUS[t.status], active = ACTIVE.includes(t.status), open = S.expanded.has(t.id) || active;
  const busy = !!activeTask();
  const actions = active ? '' : S.pendingDelete === t.id
    ? `<button type="button" class="mini confirm-del" data-action="del-yes" data-id="${id}" data-focus="dely-${id}">${icon('trash')}<span>${tr('确认删除')}</span></button><button type="button" class="mini" data-action="del-no" data-id="${id}" data-focus="deln-${id}">${tr('取消')}</button>`
    : `${isExternal(t) ? '' : `<button type="button" class="mini" data-action="rerun" data-id="${id}" data-focus="rerun-${id}" ${busy ? 'disabled' : ''}>${icon('refresh')}<span>${tr('重新执行')}</span></button>`}<button type="button" class="mini danger" data-action="del-ask" data-id="${id}" data-focus="del-${id}" aria-label="${tr('删除这条记录')}" title="${tr('删除这条记录')}">${icon('trash')}<span>${tr('删除')}</span></button>`;
  const answer = !active && t.summary ? `<div class="answer tone-${st.tone}">${icon(st.icon)}<div class="answer-text"><span class="answer-title">${st.label}</span><div class="answer-body">${mdLite(t.summary)}</div></div></div>` : '';
  const toggle = !active && t.steps.length ? `<button type="button" class="toggle-steps" data-action="toggle-steps" data-id="${id}" data-focus="ts-${id}" aria-expanded="${open}" aria-controls="steps-${id}"><span>${open ? tr('收起步骤') : `${tr('查看')} ${t.steps.length} ${tr('个步骤')}`}</span>${icon('chevron')}</button>` : '';
  return `<li class="turn">
    <div class="msg-user">
      ${isExternal(t) ? `<div class="bubble">${esc(t.prompt)}</div>` : `<button type="button" class="bubble" data-action="rerun" data-id="${id}" data-focus="bubble-${id}" title="${busy ? tr('当前任务结束后可以重新执行') : tr('点击重新执行这条指令')}" aria-label="${tr('重新执行：')}${esc(t.prompt)}" ${busy ? 'disabled' : ''}>${esc(t.prompt)}</button>`}
      <div class="msg-meta"><span class="msg-time">${hhmm(new Date(t.startedAt))}</span>${t.apps?.length ? `<span class="msg-scope" title="${esc(tr('任务只在这些 App 里操作：'))}${esc(scopeText(t.apps))}">${icon('grid')}<span class="od-truncate">${esc(tr('只在') + ' ' + scopeText(t.apps))}</span></span>` : ''}${actions}</div>
    </div>
    <div class="msg-agent">
      <div class="agent-line"><span class="avatar">${icon('bot')}</span><span class="agent-name">${esc(agentLabel(t))}</span><span class="agent-meta" data-elapsed="${id}">${t.steps.length} ${tr('步')} · ${mmss((t.endedAt || Date.now()) - t.startedAt)}</span>${badge(st)}</div>
      ${answer}${toggle}
      ${t.steps.length ? `<ol class="steps" id="steps-${id}" aria-label="${tr('执行步骤')}"${open ? '' : ' hidden'}>${t.steps.map(stepHtml).join('')}</ol>` : ''}
    </div></li>`;
}
function renderTurns(follow) {
  const sc = $('#agent-scroll'), near = sc.scrollHeight - sc.scrollTop - sc.clientHeight < 120;
  keepFocus(() => { turnsEl.innerHTML = S.tasks.map(turnHtml).join(''); });
  saveTasks();
  $('#intro').hidden = S.tasks.length > 0;
  if (follow && near) sc.scrollTop = sc.scrollHeight;
}
function scrollConvo() { const sc = $('#agent-scroll'); sc.scrollTop = sc.scrollHeight; }
function renderRunbar() {
  const t = activeTask(), bar = $('#runbar');
  const btn = (a, l, i, cls = '') => `<button type="button" class="btn btn-sm ${cls}" data-action="${a}" data-focus="rc-${a}">${icon(i)}<span>${l}</span></button>`;
  if (!t) { bar.hidden = true; bar.innerHTML = ''; }
  else {
    const st = TASK_STATUS[t.status], b = [];
    if (t.status === 'running') b.push(btn('pause', tr('暂停'), 'pause'), btn('takeover', tr('接管'), 'cursor'));
    if (t.status === 'paused') b.push(btn('resume', tr('继续'), 'play', 'btn-primary'), btn('takeover', tr('接管'), 'cursor'));
    if (t.status === 'takeover') b.push(btn('handback', tr('交还 AI'), 'play', 'btn-primary'));
    if (t.status === 'waiting') b.push(btn('goto-confirm', tr('去确认'), 'arrow', 'btn-primary'));
    b.push(btn('stop', tr('停止'), 'stop', 'btn-danger'));
    bar.innerHTML = `<span class="runbar-state t-${st.tone}">${icon(st.icon, st.icon === 'loader' ? 'spin' : '')}<span>${st.label}</span><span class="runbar-time" data-runtime="${esc(t.id)}">${mmss(Date.now() - t.startedAt)}</span></span>${b.join('')}`;
    bar.hidden = false;
  }
  const busy = !!t;
  promptEl.disabled = busy; syncSend();
  promptEl.placeholder = busy ? tr('当前任务结束后可以发新任务') : tr('想让设备做什么？');
  const clear = $('#clear-btn');
  clear.hidden = !S.tasks.length || busy;
  $('#agent-badge').innerHTML = badge(t ? TASK_STATUS[t.status] : { label: S.mode === 'demo' ? tr('演示模式') : tr('空闲'), tone: S.mode === 'demo' ? 'warn' : 'muted', icon: S.mode === 'demo' ? 'info' : 'dot' });
}
function renderGuard() {
  const t = activeTask(), g = $('#guard');
  // 提示条和按键工具条占同一个位置、同样高度：AI 操作时换成提示条，设备画面尺寸保持不变
  const dock = $('#dock'), handback = $('#dock-handback');
  handback.hidden = !(t && t.status === 'takeover');
  if (!t || t.status === 'takeover') {
    g.hidden = true; g.innerHTML = ''; dock.classList.remove('is-covered'); dock.inert = false;
    frame.dataset.ctl = t ? 'takeover' : 'user';
    return;
  }
  dock.classList.add('is-covered'); dock.inert = true;
  const ext = isExternal(t);
  const cfg = {
    running: { tone: 'accent', ic: 'loader', title: ext ? tr('外部任务正在操作这台设备') : tr('AI 正在操作这台设备'), sub: ext ? tr('手动操作已锁定，避免和它同时点击。') : tr('手动操作已锁定，避免和 AI 同时点击。'), btns: [['takeover', tr('接管'), 'cursor']] },
    paused: { tone: 'warn', ic: 'pause', title: ext ? tr('外部任务已暂停') : tr('AI 已暂停'), sub: ext ? tr('画面仍为这个任务保留，要自己操作先接管。') : tr('画面仍为 AI 保留，要自己操作先接管。'), btns: [['resume', tr('继续'), 'play'], ['takeover', tr('接管'), 'cursor']] },
    waiting: { tone: 'warn', ic: 'alert', title: ext ? tr('外部任务在等你确认一步操作') : tr('AI 在等你确认一步操作'), sub: tr('确认前画面保持锁定。'), btns: [['goto-confirm', tr('去确认'), 'arrow']] },
    takeover: { tone: 'user', ic: 'cursor', title: tr('你在接管'), sub: tr('AI 已暂停。交还后它会先重新看一遍屏幕。'), btns: [['handback', tr('交还 AI'), 'play']] }
  }[t.status];
  g.className = `guard glass tone-${cfg.tone}`;
  g.innerHTML = `<span class="t-${cfg.tone}">${icon(cfg.ic, cfg.ic === 'loader' ? 'spin' : '')}</span><div class="guard-text"><span class="guard-title od-truncate">${cfg.title}</span><span class="guard-sub od-truncate">${cfg.sub}</span></div>
    <div class="od-row od-fixed">${cfg.btns.map(([a, l, i]) => `<button type="button" class="btn btn-sm" data-action="${a}" data-focus="guard-${a}">${icon(i)}<span>${l}</span></button>`).join('')}</div>`;
  g.hidden = false;
  frame.dataset.ctl = 'ai';
}
function renderManual() {
  const lock = guarded() || S.mode === 'detecting';
  const deviceUnavailable = lock || (S.mode === 'live' && !S.udid);
  $$('[data-key]').forEach(b => { b.disabled = deviceUnavailable; });
  typeEl.disabled = deviceUnavailable; $('#type-btn').disabled = deviceUnavailable; $('#type-submit').disabled = deviceUnavailable;
  $('#url').disabled = deviceUnavailable; $('#url-btn').disabled = deviceUnavailable;
  deviceEl.disabled = lock || S.switching || !S.devices.length || !!S.devicesFixed;
  pickerSync.device();
  $$('#nav-devices button').forEach(row => { row.disabled = lock || S.switching || !!S.devicesFixed; });
  const t = activeTask();
  $('#vt-agent-label').textContent = t ? `AI · ${{ running: tr('执行中'), waiting: tr('待确认'), paused: tr('已暂停'), takeover: tr('接管中') }[t.status]}` : tr('AI 助手');
}
/* 系统弹窗（权限请求之类）：手机自己的视频里可能看不到（定位授权弹窗只剩变暗的屏幕），画面用它时问守护进程有没有，有就在
 * 画面上方显示弹窗的文字和按钮。点按钮是按坐标点手机上的那个按钮：按名字点会把「允许」点到「不允许」上。
 * 问一次要拿设备锁：在 App 里约 0.03 秒，前台是 SpringBoard（主屏幕、锁屏、或者弹窗正开着）时要问 WDA 弹窗文字，约 0.6 秒
 * （iPhone 15 Pro Max，iOS 27.0.1）。所以查得慢又没有弹窗时放慢到 6 秒一次，刚触摸或操作过的 3 秒内不问，免得按下排在它后面 */
