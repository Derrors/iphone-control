const SA = { busy: false, key: '', quietUntil: 0 };
function alertQuiet() { SA.quietUntil = Date.now() + 3000; }
async function alertLoop() {
  const t0 = performance.now();
  await pollAlert();
  const slow = performance.now() - t0 > 300;
  setTimeout(alertLoop, S.sysAlert ? 1500 : slow ? 6000 : 2000);
}
async function pollAlert() {
  if (SA.busy || Date.now() < SA.quietUntil) return;
  if (S.mode !== 'live' || document.hidden || !V.device) {
    if (S.sysAlert) { S.sysAlert = null; renderSysAlert(); }
    return;
  }
  SA.busy = true;
  // busy：手机正在执行操作，这次没查（让操作先走），保留上次的结果
  try { const v = await api('/api/alert'); if (!v.busy) S.sysAlert = v.alert || null; } catch { /* 下一轮再问 */ } finally { SA.busy = false; }
  renderSysAlert();
}
function renderSysAlert() {
  const el = $('#sys-alert'), a = S.sysAlert, locked = guarded();
  const f = a && a.frame, d = f ? dims() : null;
  const key = a ? JSON.stringify([a, locked, d]) : '';
  if (key === SA.key) return;
  SA.key = key;
  el.hidden = !a;
  if (!a) { el.innerHTML = ''; return; }
  // 真实弹窗的位置（点 → 画面的百分比），竖直方向对齐它的中心；画面尺寸还不知道时放在中间
  const pct = v => `${(v * 100).toFixed(2)}%`;
  const pos = d ? `left:${pct(f.x / d.W)};width:${pct(f.width / d.W)};top:${pct((f.y + f.height / 2) / d.H)}` : 'left:18%;width:64%;top:50%';
  // 整行宽的是弹窗的操作按钮；窄的（「精确位置：打开」这类开关）做成小标签
  const wide = b => !f || !b.width || b.width >= f.width * 0.6;
  const btn = (b, i, cls) => `<button type="button" class="${cls}" data-action="sys-alert-tap" data-i="${i}"${locked ? ' disabled' : ''}>${esc(b.label)}</button>`;
  const chips = a.buttons.map((b, i) => wide(b) ? '' : btn(b, i, 'sa-chip')).join('');
  const acts = a.buttons.map((b, i) => wide(b) ? btn(b, i, 'sa-btn') : '').join('');
  const [title, ...rest] = String(a.text).split('\n');
  el.innerHTML = `<div class="sa-box" style="${pos}"><div class="sa-card" role="alertdialog" aria-label="${esc(title)}">
      <p class="sa-title">${esc(title)}</p>${rest.length ? `<p class="sa-msg">${esc(rest.join('\n'))}</p>` : ''}
      ${chips ? `<div class="sa-chips">${chips}</div>` : ''}${acts ? `<div class="sa-btns">${acts}</div>` : ''}
    </div><p class="sa-note">${icon('alert')}<span>${locked ? tr('AI 任务进行中：要自己点，先接管。') : tr('实时画面里看不到这个系统弹窗，点这里就是点手机上的按钮')}</span></p></div>`;
}
async function tapSysAlert(i) {
  const b = S.sysAlert && S.sysAlert.buttons[i];
  if (!b || !S.display || guarded()) return;
  await act('/api/tap', { x: b.x, y: b.y, view: S.display.id });
  S.sysAlert = null; renderSysAlert();
  setTimeout(pollAlert, 800);
}
/* ---------- 任务的 App 范围：开任务时可以限定只在哪些 App 里操作（守护进程强制，见 docs/task-control.md 5.5）。
 * 选择在本次页面里一直保留，按钮上始终显示；刷新后不保留，免得忘了而让任务被悄悄限制。演示模式里没有 ---------- */
